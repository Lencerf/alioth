// Copyright 2025 Google LLC
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     https://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::collections::HashMap;
use std::fmt::Debug;
use std::fs;
use std::io::{BufRead, BufReader, BufWriter, ErrorKind, IoSlice, IoSliceMut, Read, Write};
use std::mem::size_of_val;
use std::num::Wrapping;
use std::os::fd::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicU32, Ordering};
use std::thread::JoinHandle;

use flume::Receiver;
use mio::event::Event;
use mio::unix::SourceFd;
use mio::{Interest, Registry, Token};
use serde::Deserialize;
use serde_aco::Help;
use zerocopy::{FromBytes, IntoBytes};

use tokio::io::{
    AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader as TokioBufReader,
    BufWriter as TokioBufWriter,
};
use tokio::net::UnixStream as TokioUnixStream;

use crate::ffi;
use crate::hv::IoeventFd;
use crate::mem::mapped::RamBus;
use crate::sync::notifier::Notifier;
use crate::virtio::dev::vsock::{
    ShutdownFlag, VSOCK_CID_HOST, VsockConfig, VsockFeature, VsockHeader, VsockOp, VsockType,
    VsockVirtq,
};
use crate::virtio::dev::{DevSpec, Virtio, WakeEvent};
use crate::virtio::queue::{DescChain, Queue, QueueReg, Status, VirtQueue};
use crate::virtio::worker::WorkerApi;
use crate::virtio::worker::mio::{ActiveMio, Mio, VirtioMio};
use crate::virtio::worker::tokio::{ActiveTokio, TokioBackend, VirtioTokio};
use crate::virtio::{DeviceId, FEATURE_BUILT_IN, IrqSender, Result, error};

const HEADER_SIZE: usize = size_of::<VsockHeader>();
const SOCKET_TYPE: VsockType = VsockType::STREAM;

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Help)]
pub struct UdsVsockSpec {
    /// Vsock context id.
    pub cid: u32,
    /// Host-side Unix domain socket path.
    pub path: Box<Path>,
    /// System API for asynchronous IO.
    #[serde(default)]
    pub api: WorkerApi,
}

impl DevSpec for UdsVsockSpec {
    type Device = UdsVsock;

    fn build(self, name: impl Into<Arc<str>>) -> Result<UdsVsock> {
        UdsVsock::new(self, name)
    }
}

#[derive(Debug)]
pub struct UdsVsock {
    name: Arc<str>,
    config: Arc<VsockConfig>,
    path: Box<Path>,
    listener: UnixListener,
    worker_api: WorkerApi,
    // mio state
    connections: HashMap<(u32, u32), Connection>,
    ports: HashMap<Token, (u32, u32)>,
    sockets: HashMap<Token, UnixStream>,
    host_ports: HashMap<u32, u32>,
    next_port: u32,
    // tokio state (unused when worker_api == Mio)
    shared: Arc<VsockShared>,
    wakeup_tx: Option<tokio::sync::mpsc::Sender<u16>>,
    rx_data_rx: flume::Receiver<VsockHeaderAndData>,
    rx_data_tx: flume::Sender<VsockHeaderAndData>,
    pending_rx: Option<VsockHeaderAndData>,
}

// ---------------------------------------------------------------------------
// Tokio-mode shared state
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct ConnHandle {
    tx_data_tx: flume::Sender<Vec<u8>>,
    tx_cnt: Arc<AtomicU32>,
}

#[derive(Debug)]
struct VsockHeaderAndData {
    hdr: VsockHeader,
    data: Vec<u8>,
}

#[derive(Debug)]
struct VsockShared {
    connections: Mutex<HashMap<(u32, u32), ConnHandle>>,
    host_ports: Mutex<HashMap<u32, u32>>,
    next_port: Mutex<u32>,
}

impl VsockShared {
    fn allocate_host_port(&self) -> Option<u32> {
        let mut next = self.next_port.lock().unwrap();
        let ports = self.host_ports.lock().unwrap();
        let mut count: u64 = 0;
        while ports.contains_key(&*next) && count < u32::MAX as u64 {
            *next = next.wrapping_add(1);
            count += 1;
        }
        if count == u32::MAX as u64 {
            None
        } else {
            let port = *next;
            *next = next.wrapping_add(1);
            Some(port)
        }
    }
}

fn get_buf_size(stream: &UnixStream) -> Result<usize> {
    let mut buf_size = 0i32;
    let mut arg_size = size_of_val(&buf_size) as libc::socklen_t;
    ffi!(unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_SNDBUF,
            &mut buf_size as *mut _ as _,
            &mut arg_size,
        )
    })?;
    Ok(buf_size as usize)
}

impl UdsVsock {
    fn allocate_port(&mut self) -> Option<u32> {
        let mut count: u64 = 0;
        while self.host_ports.contains_key(&self.next_port) && count < u32::MAX as u64 {
            self.next_port = self.next_port.wrapping_add(1);
            count += 1;
        }
        if count == u32::MAX as u64 {
            None
        } else {
            Some(self.next_port)
        }
    }

    fn create_socket(&mut self, registry: &Registry) -> Result<()> {
        let (stream, _) = self.listener.accept()?;
        stream.set_nonblocking(true)?;
        let token = Token(stream.as_raw_fd() as usize);
        registry.register(
            &mut SourceFd(&stream.as_raw_fd()),
            token,
            Interest::READABLE,
        )?;
        self.sockets.insert(token, stream);
        Ok(())
    }

    fn handle_conn_request<'m, Q, S>(
        &mut self,
        token: Token,
        socket: UnixStream,
        rx_q: &mut Queue<'_, 'm, Q>,
        irq_sender: &S,
    ) -> Result<()>
    where
        Q: VirtQueue<'m>,
        S: IrqSender,
    {
        let mut msg = String::new();
        let writer = socket.try_clone()?;
        let mut reader = BufReader::new(socket);
        let buf_size = get_buf_size(&writer)?;
        reader.read_line(&mut msg)?;
        let port_str = msg.trim_start_matches("CONNECT ").trim_end();
        let Ok(port) = port_str.parse::<u32>() else {
            log::error!("{}: failed to parse port {port_str}", self.name);
            return Ok(());
        };
        let Some(host_port) = self.allocate_port() else {
            log::error!("{}: failed to allocate port", self.name);
            return Ok(());
        };
        let hdr = VsockHeader {
            src_cid: VSOCK_CID_HOST,
            dst_cid: self.config.guest_cid,
            src_port: host_port,
            dst_port: port,
            type_: SOCKET_TYPE,
            op: VsockOp::REQUEST,
            fwd_cnt: Wrapping(0),
            buf_alloc: buf_size as u32,
            ..Default::default()
        };
        self.respond(&hdr, irq_sender, rx_q)?;
        let conn = Connection {
            state: ConnState::Requested,
            reader,
            writer: BufWriter::new(writer),
            buf_alloc: buf_size as u32,
        };
        self.connections.insert((host_port, port), conn);
        let count = self.host_ports.entry(host_port).or_default();
        *count += 1;
        log::trace!(
            "{}: host:{host_port}: count incremented to {count}",
            self.name
        );
        self.ports.insert(token, (host_port, port));
        log::trace!("{}: host:{host_port} -> vm:{port}: requested", self.name);
        Ok(())
    }

    fn respond_rst<'m, Q, S>(
        &self,
        hdr: &VsockHeader,
        irq_sender: &S,
        rx_q: &mut Queue<'_, 'm, Q>,
    ) -> Result<()>
    where
        Q: VirtQueue<'m>,
        S: IrqSender,
    {
        let resp = VsockHeader {
            src_cid: VSOCK_CID_HOST,
            dst_cid: self.config.guest_cid,
            src_port: hdr.dst_port,
            dst_port: hdr.src_port,
            type_: hdr.type_,
            op: VsockOp::RST,
            ..Default::default()
        };
        self.respond(&resp, irq_sender, rx_q)
    }

    fn respond<'m, Q, S>(
        &self,
        hdr: &VsockHeader,
        irq_sender: &S,
        rx_q: &mut Queue<'_, 'm, Q>,
    ) -> Result<()>
    where
        Q: VirtQueue<'m>,
        S: IrqSender,
    {
        let mut hdr_buf = hdr.as_bytes();
        rx_q.handle_desc(VsockVirtq::RX.raw(), irq_sender, |desc| {
            if hdr_buf.is_empty() {
                return Ok(Status::Break);
            }
            let c = hdr_buf.read_vectored(&mut desc.writable)? as u32;
            Ok(Status::Done { len: c })
        })?;
        if !hdr_buf.is_empty() {
            log::error!(
                "{}: queue RX: no enough writable buffers for {:?}",
                self.name,
                hdr.op
            );
            return error::InvalidBuffer.fail();
        }
        Ok(())
    }

    fn handle_tx_response<'m, Q, S>(
        &mut self,
        hdr: &VsockHeader,
        rx_q: &mut Queue<'_, 'm, Q>,
        irq_sender: &S,
    ) -> Result<()>
    where
        Q: VirtQueue<'m>,
        S: IrqSender,
    {
        let host_port = hdr.dst_port;
        let guest_port = hdr.src_port;
        let Some(conn) = self.connections.get_mut(&(host_port, guest_port)) else {
            log::warn!(
                "{}: vm:{guest_port} -> host:{host_port}: unknown connection",
                self.name
            );
            return Ok(());
        };
        if conn.state != ConnState::Requested {
            log::error!(
                "{}: vm:{guest_port} -> host:{host_port}: found {:?}, expect {:?}",
                self.name,
                conn.state,
                ConnState::Requested
            );
            return Ok(());
        };
        writeln!(conn.writer, "OK {host_port}")?;
        conn.writer.flush()?;
        conn.state = ConnState::Established {
            fwd_cnt: Wrapping(0),
        };
        log::trace!(
            "{}: host:{host_port} -> vm:{guest_port}: established",
            self.name
        );
        self.transfer_rx_data(host_port, guest_port, rx_q, irq_sender)
    }

    fn remove_conn(&mut self, host_port: u32, guest_port: u32, registry: &Registry) -> Result<()> {
        let Some(conn) = self.connections.remove(&(host_port, guest_port)) else {
            log::warn!(
                "{}: vm:{guest_port} -> host:{host_port}: unknown connection",
                self.name
            );
            return Ok(());
        };
        let reader = conn.reader.into_inner();
        let token = Token(reader.as_raw_fd() as usize);
        self.ports.remove(&token);
        if let Some(count) = self.host_ports.get_mut(&host_port) {
            if *count == 1 {
                self.host_ports.remove(&host_port);
                log::trace!("{}: host:{host_port}: free port", self.name);
            } else {
                *count -= 1;
                log::trace!(
                    "{}: host:{host_port}: count decremented to {count}",
                    self.name
                );
            }
        } else {
            log::error!(
                "{}: vm:{guest_port} -> host:{host_port}: unknown host port",
                self.name
            );
        }
        registry.deregister(&mut SourceFd(&reader.as_raw_fd()))?;
        Ok(())
    }

    fn handle_tx_rst(&mut self, hdr: &VsockHeader, registry: &Registry) -> Result<()> {
        let host_port = hdr.dst_port;
        let guest_port = hdr.src_port;
        self.remove_conn(host_port, guest_port, registry)?;
        log::trace!("{}: vm:{guest_port} -> host:{host_port}: reset", self.name);
        Ok(())
    }

    fn handle_tx_shutdown<'m, Q, S>(
        &mut self,
        hdr: &VsockHeader,
        registry: &Registry,
        irq_sender: &S,
        rx_q: &mut Queue<'_, 'm, Q>,
    ) -> Result<()>
    where
        Q: VirtQueue<'m>,
        S: IrqSender,
    {
        let host_port = hdr.dst_port;
        let guest_port = hdr.src_port;
        let Some(conn) = self.connections.get_mut(&(host_port, guest_port)) else {
            log::warn!(
                "{}: vm:{guest_port} -> host:{host_port}: unknown connection",
                self.name
            );
            return Ok(());
        };
        let mut flags = if let ConnState::Shutdown { flags } = conn.state {
            flags
        } else {
            ShutdownFlag::empty()
        };
        flags |= ShutdownFlag::from_bits_truncate(hdr.flags);
        if flags != ShutdownFlag::all() {
            conn.state = ConnState::Shutdown { flags };
            log::trace!(
                "{}: vm:{guest_port} -> host:{host_port}: {flags:?}",
                self.name
            );
        } else {
            if let Err(e) = self.respond_rst(hdr, irq_sender, rx_q) {
                log::error!("{}: failed to respond to shutdown: {e:?}", self.name);
            }
            self.remove_conn(host_port, guest_port, registry)?;
            log::trace!(
                "{}: vm:{guest_port} -> host:{host_port}: shutdown",
                self.name
            );
        }
        Ok(())
    }

    fn handle_tx_request<'m, Q, S>(
        &mut self,
        hdr: &VsockHeader,
        registry: &Registry,
        irq_sender: &S,
        rx_q: &mut Queue<'_, 'm, Q>,
    ) -> Result<()>
    where
        Q: VirtQueue<'m>,
        S: IrqSender,
    {
        let host_port = hdr.dst_port;
        let guest_port = hdr.src_port;
        let port_socket = format!("{}_{host_port}", self.path.to_string_lossy());
        let reader = match UnixStream::connect(&port_socket) {
            Ok(reader) => reader,
            Err(e) => {
                log::error!("{}: failed to connect to {port_socket}: {e:?}", self.name);
                return self.respond_rst(hdr, irq_sender, rx_q);
            }
        };
        reader.set_nonblocking(true)?;
        let writer = reader.try_clone()?;
        let token = Token(reader.as_raw_fd() as usize);
        registry.register(
            &mut SourceFd(&reader.as_raw_fd()),
            token,
            Interest::READABLE,
        )?;
        let buf_size = get_buf_size(&writer)?;
        let conn = Connection {
            reader: BufReader::new(reader),
            writer: BufWriter::new(writer),
            buf_alloc: buf_size as u32,
            state: ConnState::Established {
                fwd_cnt: Wrapping(0),
            },
        };
        let resp = VsockHeader {
            src_cid: VSOCK_CID_HOST,
            dst_cid: self.config.guest_cid,
            src_port: host_port,
            dst_port: guest_port,
            type_: hdr.type_,
            op: VsockOp::RESPONSE,
            fwd_cnt: Wrapping(0),
            buf_alloc: buf_size as u32,
            ..Default::default()
        };
        self.respond(&resp, irq_sender, rx_q)?;
        self.connections.insert((host_port, guest_port), conn);
        let count = self.host_ports.entry(host_port).or_default();
        *count += 1;
        log::trace!(
            "{}: host:{host_port}: count incremented to {count}",
            self.name
        );
        self.ports.insert(token, (host_port, guest_port));
        log::trace!(
            "{}: vm:{guest_port} -> host:{host_port}: established",
            self.name
        );
        Ok(())
    }

    fn handle_tx_desc<'m, Q, S>(
        &mut self,
        desc: &mut DescChain,
        registry: &Registry,
        irq_sender: &S,
        rx_q: &mut Queue<'_, 'm, Q>,
    ) -> Result<()>
    where
        Q: VirtQueue<'m>,
        S: IrqSender,
    {
        let name = &*self.name;
        let [buf, readable @ ..] = desc.readable.as_slice() else {
            return error::InvalidBuffer.fail();
        };
        let Some((header, body)) = buf.split_first_chunk::<HEADER_SIZE>() else {
            return error::InvalidBuffer.fail();
        };
        let Ok(hdr) = VsockHeader::ref_from_bytes(header) else {
            return error::InvalidBuffer.fail();
        };
        if hdr.src_cid != self.config.guest_cid || hdr.dst_cid != VSOCK_CID_HOST {
            log::warn!(
                "{name}: invalid CID pair: {} -> {}",
                hdr.src_cid,
                hdr.dst_cid
            );
        }
        log::trace!(
            "{name}: vm:{} -> host:{}: {:?}",
            hdr.src_port,
            hdr.dst_port,
            hdr.op
        );
        match hdr.op {
            VsockOp::REQUEST => self.handle_tx_request(hdr, registry, irq_sender, rx_q),
            VsockOp::RESPONSE => self.handle_tx_response(hdr, rx_q, irq_sender),
            VsockOp::RST => self.handle_tx_rst(hdr, registry),
            VsockOp::RW => self.transfer_tx_data(hdr, body, readable),
            VsockOp::CREDIT_UPDATE => {
                log::info!(
                    "{name}: CREDIT_UPDATE: fwd_cnt: {}, buf_alloc: {}",
                    hdr.fwd_cnt,
                    hdr.buf_alloc
                );
                Ok(())
            }
            VsockOp::SHUTDOWN => self.handle_tx_shutdown(hdr, registry, irq_sender, rx_q),
            _ => {
                log::error!("{name}: unsupported operation: {:?}", hdr.op);
                Ok(())
            }
        }
    }

    fn handle_tx<'m, Q, S, E>(
        &mut self,
        active_mio: &mut ActiveMio<'_, '_, 'm, Q, S, E>,
    ) -> Result<()>
    where
        Q: VirtQueue<'m>,
        S: IrqSender,
        E: IoeventFd,
    {
        let [Some(rx_q), Some(tx_q), ..] = active_mio.queues else {
            let tx_index = VsockVirtq::TX.raw();
            return error::InvalidQueueIndex { index: tx_index }.fail();
        };
        let irq_sender = active_mio.irq_sender;
        let registry = active_mio.poll.registry();
        tx_q.handle_desc(VsockVirtq::TX.raw(), irq_sender, |desc| {
            self.handle_tx_desc(desc, registry, irq_sender, rx_q)?;
            Ok(Status::Done { len: 0 })
        })
    }

    fn transfer_rx_data<'m, Q, S>(
        &mut self,
        host_port: u32,
        guest_port: u32,
        rx_q: &mut Queue<'_, 'm, Q>,
        irq_sender: &S,
    ) -> Result<()>
    where
        Q: VirtQueue<'m>,
        S: IrqSender,
    {
        fn copy_to_rx(
            hdr: &mut VsockHeader,
            conn: &mut BufReader<UnixStream>,
            buffers: &mut [IoSliceMut],
        ) -> Result<usize> {
            let mut nskip = 0;
            let mut nread = 0;
            for buf in buffers.iter_mut() {
                let r = if HEADER_SIZE > nskip {
                    let Some((_, data)) = buf.split_at_mut_checked(HEADER_SIZE - nskip) else {
                        nskip += buf.len();
                        continue;
                    };
                    nskip = HEADER_SIZE;
                    if data.is_empty() {
                        continue;
                    }
                    conn.read(data)
                } else {
                    conn.read(buf)
                };
                let n = match r {
                    Ok(0) => break,
                    Ok(n) => n,
                    Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                    Err(e) => Err(e)?,
                };
                nread += n;
            }
            if nskip != HEADER_SIZE {
                return error::InvalidBuffer.fail();
            }
            hdr.len = nread as u32;
            let mut hdr_buf = hdr.as_bytes();
            let _ = hdr_buf.read_vectored(buffers);
            Ok(nread)
        }

        let rx_idx = VsockVirtq::RX.raw();
        let Some(conn) = self.connections.get_mut(&(host_port, guest_port)) else {
            log::warn!(
                "{}: vm:{guest_port} -> host:{host_port}: unknown connection",
                self.name
            );
            return Ok(());
        };
        let ConnState::Established { fwd_cnt } = conn.state else {
            log::error!("{}: unexpected state {:?}", self.name, conn.state);
            return Ok(());
        };
        let mut hdr = VsockHeader {
            src_cid: VSOCK_CID_HOST,
            dst_cid: self.config.guest_cid,
            src_port: host_port,
            dst_port: guest_port,
            type_: SOCKET_TYPE,
            op: VsockOp::RW,
            fwd_cnt,
            buf_alloc: conn.buf_alloc,
            ..Default::default()
        };
        rx_q.handle_desc(rx_idx, irq_sender, |desc| {
            let nread = copy_to_rx(&mut hdr, &mut conn.reader, &mut desc.writable)? as u32;
            if nread == 0 {
                return Ok(Status::Break);
            }
            log::trace!(
                "{}: host:{host_port} -> vm:{guest_port}: transfered {nread} bytes",
                self.name
            );
            Ok(Status::Done {
                len: nread + HEADER_SIZE as u32,
            })
        })?;
        Ok(())
    }

    fn transfer_tx_data(
        &mut self,
        hdr: &VsockHeader,
        body: &[u8],
        buffers: &[IoSlice],
    ) -> Result<()> {
        fn copy_to_conn(
            buf: &[u8],
            conn: &mut BufWriter<UnixStream>,
            remain: &mut usize,
        ) -> Result<()> {
            if let Some(b) = buf.get(..*remain) {
                conn.write_all(b)?;
                *remain = 0;
            } else {
                conn.write_all(buf)?;
                *remain -= buf.len();
            }
            Ok(())
        }

        let host_port = hdr.dst_port;
        let guest_port = hdr.src_port;
        let Some(conn) = self.connections.get_mut(&(host_port, guest_port)) else {
            log::warn!(
                "{}: vm:{guest_port} -> host:{host_port}: unknown connection",
                self.name
            );
            return Ok(());
        };
        let ConnState::Established { fwd_cnt } = &mut conn.state else {
            log::warn!("{}: invalid connection state {:?}", self.name, conn.state);
            return Ok(());
        };
        let mut remain = hdr.len as usize;
        if !body.is_empty() {
            copy_to_conn(body, &mut conn.writer, &mut remain)?;
        }
        for buf in buffers {
            if remain == 0 {
                break;
            }
            copy_to_conn(buf, &mut conn.writer, &mut remain)?;
        }
        if remain != 0 {
            log::error!("{}: missing {remain} bytes", self.name);
            return error::InvalidBuffer.fail();
        }
        *fwd_cnt += hdr.len;
        log::trace!(
            "{}: vm:{guest_port} -> host:{host_port}: transferred {} bytes",
            self.name,
            hdr.len
        );
        conn.writer.flush()?;
        Ok(())
    }
}

impl Drop for UdsVsock {
    fn drop(&mut self) {
        let Ok(addr) = self.listener.local_addr() else {
            return;
        };
        let Some(path) = addr.as_pathname() else {
            return;
        };
        if let Err(e) = fs::remove_file(path) {
            log::error!("{}: error removing {path:?}: {e:?}", self.name);
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum ConnState {
    Requested,
    Established { fwd_cnt: Wrapping<u32> },
    Shutdown { flags: ShutdownFlag },
}

#[derive(Debug)]
pub struct Connection {
    state: ConnState,
    reader: BufReader<UnixStream>,
    writer: BufWriter<UnixStream>,
    buf_alloc: u32,
}

impl UdsVsock {
    fn new(spec: UdsVsockSpec, name: impl Into<Arc<str>>) -> Result<Self> {
        let name = name.into();
        let listener = UnixListener::bind(&spec.path)?;
        listener.set_nonblocking(true)?;
        let (rx_data_tx, rx_data_rx) = flume::unbounded();
        let vsock = UdsVsock {
            name,
            path: spec.path,
            config: Arc::new(VsockConfig {
                guest_cid: spec.cid,
                ..Default::default()
            }),
            listener,
            worker_api: spec.api,
            connections: HashMap::new(),
            sockets: HashMap::new(),
            ports: HashMap::new(),
            host_ports: HashMap::new(),
            next_port: 1024,
            shared: Arc::new(VsockShared {
                connections: Mutex::new(HashMap::new()),
                host_ports: Mutex::new(HashMap::new()),
                next_port: Mutex::new(1024),
            }),
            wakeup_tx: None,
            rx_data_rx,
            rx_data_tx,
            pending_rx: None,
        };
        Ok(vsock)
    }
}

impl Virtio for UdsVsock {
    type Config = VsockConfig;
    type Feature = VsockFeature;

    fn id(&self) -> DeviceId {
        DeviceId::SOCKET
    }

    fn name(&self) -> &str {
        &self.name
    }

    fn num_queues(&self) -> u16 {
        3
    }

    fn config(&self) -> Arc<VsockConfig> {
        self.config.clone()
    }

    fn feature(&self) -> u128 {
        VsockFeature::STREAM.bits() | FEATURE_BUILT_IN
    }

    fn spawn_worker<S, E>(
        self,
        event_rx: Receiver<WakeEvent<S, E>>,
        memory: Arc<RamBus>,
        queue_regs: Arc<[QueueReg]>,
    ) -> Result<(JoinHandle<()>, Option<Arc<Notifier>>)>
    where
        S: IrqSender,
        E: IoeventFd,
    {
        match self.worker_api {
            WorkerApi::Mio => Mio::spawn_worker(self, event_rx, memory, queue_regs),
            WorkerApi::Tokio => TokioBackend::spawn_worker(self, event_rx, memory, queue_regs),
            #[cfg(target_os = "linux")]
            WorkerApi::IoUring => unimplemented!("vsock: io_uring not supported"),
        }
    }
}

impl VirtioMio for UdsVsock {
    fn activate<'m, Q, S, E>(
        &mut self,
        _feature: u128,
        active_mio: &mut ActiveMio<'_, '_, 'm, Q, S, E>,
    ) -> Result<()>
    where
        Q: VirtQueue<'m>,
        S: IrqSender,
        E: IoeventFd,
    {
        active_mio.poll.registry().register(
            &mut SourceFd(&self.listener.as_raw_fd()),
            Token(self.listener.as_raw_fd() as usize),
            Interest::READABLE,
        )?;
        Ok(())
    }

    fn handle_event<'m, Q, S, E>(
        &mut self,
        event: &Event,
        active_mio: &mut ActiveMio<'_, '_, 'm, Q, S, E>,
    ) -> Result<()>
    where
        Q: VirtQueue<'m>,
        S: IrqSender,
        E: IoeventFd,
    {
        let token = event.token();
        let registry = active_mio.poll.registry();
        let irq_sender = active_mio.irq_sender;
        let rx_index = VsockVirtq::RX.raw();
        let Some(Some(rx_q)) = active_mio.queues.get_mut(rx_index as usize) else {
            return error::InvalidQueueIndex { index: rx_index }.fail();
        };
        if token.0 == self.listener.as_raw_fd() as usize {
            self.create_socket(registry)
        } else if let Some(socket) = self.sockets.remove(&token) {
            self.handle_conn_request(token, socket, rx_q, irq_sender)
        } else if let Some(port_pair) = self.ports.get(&token) {
            let (host_port, guest_port) = port_pair.to_owned();
            self.transfer_rx_data(host_port, guest_port, rx_q, irq_sender)
        } else {
            log::error!("{}: invalid token: {token:#x?}", self.name);
            Ok(())
        }
    }

    fn handle_queue<'m, Q, S, E>(
        &mut self,
        index: u16,
        active_mio: &mut ActiveMio<'_, '_, 'm, Q, S, E>,
    ) -> Result<()>
    where
        Q: VirtQueue<'m>,
        S: IrqSender,
        E: IoeventFd,
    {
        let index = VsockVirtq::from(index);
        let name = &self.name;
        match index {
            VsockVirtq::TX => self.handle_tx(active_mio)?,
            VsockVirtq::RX => log::debug!("{name}: queue RX buffer available"),
            VsockVirtq::EVENT => log::debug!("{name}: queue EVENT buffer available"),
            _ => log::error!("{name}: unknown queue index {index:?}"),
        }
        Ok(())
    }

    fn reset(&mut self, registry: &Registry) {
        for (_, conn) in self.connections.drain() {
            let reader = conn.reader.into_inner();
            if let Err(err) = registry.deregister(&mut SourceFd(&reader.as_raw_fd())) {
                log::error!("{}: failed to deregister socket: {err}", self.name);
            }
        }
        for (_, socket) in self.sockets.drain() {
            if let Err(err) = registry.deregister(&mut SourceFd(&socket.as_raw_fd())) {
                log::error!("{}: failed to deregister socket: {err}", self.name);
            }
        }
        if let Err(err) = registry.deregister(&mut SourceFd(&self.listener.as_raw_fd())) {
            log::error!("{}: failed to deregister listener: {err}", self.name);
        }
        self.host_ports.clear();
        self.next_port = 1024;
    }
}

// ---------------------------------------------------------------------------
// VirtioTokio — tokio async backend
// ---------------------------------------------------------------------------

const HEADER_SIZE_TOKIO: usize = size_of::<VsockHeader>();

impl VirtioTokio for UdsVsock {
    fn activate<'m, Q, S, E>(
        &mut self,
        _feature: u128,
        active: &mut ActiveTokio<'_, '_, 'm, Q, S, E>,
    ) -> Result<()>
    where
        Q: VirtQueue<'m>,
        S: IrqSender,
        E: IoeventFd,
    {
        self.wakeup_tx = Some(active.wakeup_tx.clone());
        let rt_handle = active.rt_handle.clone();
        self.spawn_listener_tokio(rt_handle);
        Ok(())
    }

    async fn handle_queue<'m, Q, S, E>(
        &mut self,
        index: u16,
        active: &mut ActiveTokio<'_, '_, 'm, Q, S, E>,
    ) -> Result<()>
    where
        Q: VirtQueue<'m>,
        S: IrqSender,
        E: IoeventFd,
    {
        let vq = VsockVirtq::from(index);
        match vq {
            VsockVirtq::TX => {
                let [Some(_rx_q), Some(tx_q), ..] = active.queues else {
                    return error::InvalidQueueIndex {
                        index: VsockVirtq::TX.raw(),
                    }
                    .fail();
                };
                let irq_sender = active.irq_sender;
                tx_q.handle_desc(VsockVirtq::TX.raw(), irq_sender, |desc| {
                    self.handle_tx_desc_tokio(desc)?;
                    Ok(Status::Done { len: 0 })
                })?;
            }
            VsockVirtq::RX => {
                let rx_data_rx = &self.rx_data_rx;
                let [Some(rx_q), ..] = active.queues else {
                    return error::InvalidQueueIndex {
                        index: VsockVirtq::RX.raw(),
                    }
                    .fail();
                };
                let irq_sender = active.irq_sender;
                loop {
                    if self.pending_rx.is_none() {
                        if let Ok(item) = rx_data_rx.try_recv() {
                            self.pending_rx = Some(item);
                        }
                    }
                    if let Some(item) = self.pending_rx.take() {
                        let (success, item) = Self::push_rx_tokio(rx_q, irq_sender, item)?;
                        if success {
                            // continue
                        } else {
                            self.pending_rx = Some(item);
                            break;
                        }
                    } else {
                        break;
                    }
                }
            }
            VsockVirtq::EVENT => {
                log::debug!("{}: EVENT queue buffer available", self.name);
            }
            _ => {
                log::error!("{}: unknown queue {index}", self.name);
            }
        }
        Ok(())
    }

    async fn handle_io<'m, Q, S, E>(
        &mut self,
        _token: u64,
        _active: &mut ActiveTokio<'_, '_, 'm, Q, S, E>,
    ) -> Result<()>
    where
        Q: VirtQueue<'m>,
        S: IrqSender,
        E: IoeventFd,
    {
        Ok(())
    }

    fn reset(&mut self) {
        self.shared.connections.lock().unwrap().clear();
        self.shared.host_ports.lock().unwrap().clear();
        *self.shared.next_port.lock().unwrap() = 1024;
    }
}

// --- Tokio-mode helpers on UdsVsock ---

impl UdsVsock {
    fn spawn_listener_tokio(&self, rt_handle: tokio::runtime::Handle) {
        let std_listener = self.listener.try_clone().expect("listener try_clone");
        std_listener.set_nonblocking(true).ok();
        let listener = tokio::net::UnixListener::from_std(std_listener)
            .expect("convert to tokio UnixListener");

        let shared = self.shared.clone();
        let name = self.name.clone();
        let config = self.config.clone();
        let rx_data_tx = self.rx_data_tx.clone();
        let wakeup_tx = self.wakeup_tx.clone().unwrap();

        rt_handle.spawn(async move {
            loop {
                log::info!("{name}: uds looping");
                let (stream, _) = match listener.accept().await {
                    Ok(c) => c,
                    Err(e) => {
                        log::error!("{name}: accept error: {e}");
                        break;
                    }
                };
                log::info!("{name}: get connection");

                // Read handshake: "CONNECT <port>"
                let (reader, writer) = stream.into_split();
                let mut reader = TokioBufReader::new(reader);
                let mut line = String::new();
                if reader.read_line(&mut line).await.is_err() {
                    continue;
                }
                let port_str = line.trim_start_matches("CONNECT ").trim_end();
                let Ok(guest_port) = port_str.parse::<u32>() else {
                    log::error!("{name}: bad port {port_str}");
                    continue;
                };

                let Some(host_port) = shared.allocate_host_port() else {
                    log::error!("{name}: no free host ports");
                    continue;
                };

                let stream = reader.into_inner().reunite(writer).expect("reunite");
                Self::spawn_connection_tokio(
                    name.clone(),
                    config.clone(),
                    shared.clone(),
                    host_port,
                    guest_port,
                    stream,
                    rx_data_tx.clone(),
                    wakeup_tx.clone(),
                );

                let _ = rx_data_tx.send(VsockHeaderAndData {
                    hdr: VsockHeader {
                        src_cid: VSOCK_CID_HOST,
                        dst_cid: config.guest_cid,
                        src_port: host_port,
                        dst_port: guest_port,
                        type_: VsockType::STREAM,
                        op: VsockOp::REQUEST,
                        buf_alloc: 65536,
                        ..Default::default()
                    },
                    data: Vec::new(),
                });
                let _ = wakeup_tx.try_send(VsockVirtq::RX.raw());
            }
        });
    }

    fn spawn_connection_tokio(
        name: Arc<str>,
        config: Arc<VsockConfig>,
        shared: Arc<VsockShared>,
        host_port: u32,
        guest_port: u32,
        stream: TokioUnixStream,
        rx_data_tx: flume::Sender<VsockHeaderAndData>,
        wakeup_tx: tokio::sync::mpsc::Sender<u16>,
    ) -> ConnHandle {
        let (tx_data_tx, tx_data_rx) = flume::unbounded::<Vec<u8>>();

        let tx_cnt = Arc::new(AtomicU32::new(0));
        let handle = ConnHandle {
            tx_data_tx,
            tx_cnt: tx_cnt.clone(),
        };

        shared
            .connections
            .lock()
            .unwrap()
            .insert((host_port, guest_port), handle.clone());
        *shared
            .host_ports
            .lock()
            .unwrap()
            .entry(host_port)
            .or_default() += 1;

        let (reader, writer) = stream.into_split();
        tokio::spawn({
            let tx_cnt = tx_cnt.clone();
            async move {
            let mut reader = TokioBufReader::new(reader);
            let mut writer = TokioBufWriter::new(writer);

            let read_task = {
                let rx_data_tx = rx_data_tx.clone();
                let wakeup_tx = wakeup_tx.clone();
                let name = name.clone();
                let tx_cnt = tx_cnt.clone();
                async move {
                    let mut buf = vec![0u8; 65536];
                    loop {
                        let n = match reader.read(&mut buf).await {
                            Ok(0) => {
                                log::trace!("{name}: host_read: EOF");
                                let fwd_cnt = Wrapping(tx_cnt.load(Ordering::Relaxed));
                                let _ = rx_data_tx.send(VsockHeaderAndData {
                                    hdr: VsockHeader {
                                        src_cid: VSOCK_CID_HOST,
                                        dst_cid: config.guest_cid,
                                        src_port: host_port,
                                        dst_port: guest_port,
                                        type_: VsockType::STREAM,
                                        op: VsockOp::RST,
                                        fwd_cnt,
                                        buf_alloc: 65536,
                                        ..Default::default()
                                    },
                                    data: Vec::new(),
                                });
                                let _ = wakeup_tx.try_send(VsockVirtq::RX.raw());
                                break;
                            }
                            Ok(n) => {
                                log::trace!("{name}: host_read: read {n} bytes");
                                n
                            }
                            Err(e) => {
                                log::error!("{name}: read error: {e}");
                                break;
                            }
                        };
                        let fwd_cnt = Wrapping(tx_cnt.load(Ordering::Relaxed));
                        let send_res = rx_data_tx.send(VsockHeaderAndData {
                            hdr: VsockHeader {
                                src_cid: VSOCK_CID_HOST,
                                dst_cid: config.guest_cid,
                                src_port: host_port,
                                dst_port: guest_port,
                                type_: VsockType::STREAM,
                                op: VsockOp::RW,
                                fwd_cnt,
                                buf_alloc: 65536,
                                len: n as u32,
                                ..Default::default()
                            },
                            data: buf[..n].to_vec(),
                        });
                        if send_res.is_ok() {
                            let _ = wakeup_tx.try_send(VsockVirtq::RX.raw());
                        }
                    }
                }
            };

            let write_task = {
                let name = name.clone();
                async move {
                    while let Ok(data) = tx_data_rx.recv_async().await {
                        if let Err(e) = writer.write_all(&data).await {
                            log::error!("{name}: write error: {e}");
                            break;
                        }
                        writer.flush().await.ok();
                    }
                }
            };

            tokio::select! {
                _ = read_task => {
                    log::debug!("{name}: host:{host_port} vm:{guest_port}: reader done");
                }
                _ = write_task => {
                    log::debug!("{name}: vm:{guest_port} host:{host_port}: writer done");
                }
            }

            shared
                .connections
                .lock()
                .unwrap()
                .remove(&(host_port, guest_port));
            if let Ok(mut ports) = shared.host_ports.lock()
                && let Some(c) = ports.get_mut(&host_port)
            {
                if *c == 1 {
                    ports.remove(&host_port);
                } else {
                    *c -= 1;
                }
            }
        }
        });

        handle
    }

    /// Process one TX descriptor in tokio mode.
    fn handle_tx_desc_tokio(
        &mut self,
        desc: &mut DescChain,
    ) -> Result<()> {
        let [buf, readable @ ..] = desc.readable.as_slice() else {
            return error::InvalidBuffer.fail();
        };
        let Some((header, body)) = buf.split_first_chunk::<HEADER_SIZE_TOKIO>() else {
            return error::InvalidBuffer.fail();
        };
        let hdr = VsockHeader::ref_from_bytes(header).map_err(|_| error::InvalidBuffer.build())?;

        let host_port = hdr.dst_port;
        let guest_port = hdr.src_port;

        log::trace!(
            "{}: vm:{guest_port} host:{host_port}: {:?}",
            self.name,
            hdr.op
        );

        match hdr.op {
            VsockOp::REQUEST => self.handle_tx_request_tokio(hdr),
            VsockOp::RESPONSE => {
                log::trace!(
                    "{}: host:{host_port} vm:{guest_port}: established",
                    self.name
                );
                if let Some(conn) = self.shared.connections.lock().unwrap().get(&(host_port, guest_port)) {
                    let msg = format!("OK {host_port}\n").into_bytes();
                    conn.tx_data_tx.send(msg).ok();
                }
                Ok(())
            }
            VsockOp::RST => {
                self.shared
                    .connections
                    .lock()
                    .unwrap()
                    .remove(&(host_port, guest_port));
                if let Ok(mut p) = self.shared.host_ports.lock()
                    && let Some(c) = p.get_mut(&host_port)
                {
                    if *c == 1 {
                        p.remove(&host_port);
                    } else {
                        *c -= 1;
                    }
                }
                Ok(())
            }
            VsockOp::RW => self.handle_tx_data_tokio(hdr, body, readable),
            VsockOp::SHUTDOWN => {
                let flags = ShutdownFlag::from_bits_truncate(hdr.flags);
                if flags == ShutdownFlag::all() {
                    self.push_rst_tokio(hdr)?;
                    self.shared
                        .connections
                        .lock()
                        .unwrap()
                        .remove(&(host_port, guest_port));
                }
                Ok(())
            }
            VsockOp::CREDIT_UPDATE => Ok(()),
            _ => Ok(()),
        }
    }

    fn handle_tx_request_tokio(
        &mut self,
        hdr: &VsockHeader,
    ) -> Result<()> {
        let host_port = hdr.dst_port;
        let guest_port = hdr.src_port;

        let port_socket = format!("{}_{host_port}", self.path.to_string_lossy());
        let stream = match std::os::unix::net::UnixStream::connect(&port_socket) {
            Ok(s) => s,
            Err(e) => {
                log::error!("{}: connect {port_socket}: {e}", self.name);
                return self.push_rst_tokio(hdr);
            }
        };
        stream.set_nonblocking(true)?;
        let stream = TokioUnixStream::from_std(stream)?;

        // Respond to guest
        let resp = VsockHeader {
            src_cid: VSOCK_CID_HOST,
            dst_cid: self.config.guest_cid,
            src_port: host_port,
            dst_port: guest_port,
            type_: hdr.type_,
            op: VsockOp::RESPONSE,
            buf_alloc: 65536,
            ..Default::default()
        };
        self.rx_data_tx.send(VsockHeaderAndData {
            hdr: resp,
            data: Vec::new(),
        }).ok();
        self.wakeup_tx.as_ref().unwrap().try_send(VsockVirtq::RX.raw()).ok();

        Self::spawn_connection_tokio(
            self.name.clone(),
            self.config.clone(),
            self.shared.clone(),
            host_port,
            guest_port,
            stream,
            self.rx_data_tx.clone(),
            self.wakeup_tx.clone().unwrap(),
        );
        Ok(())
    }

    fn handle_tx_data_tokio(
        &mut self,
        hdr: &VsockHeader,
        body: &[u8],
        extra: &[std::io::IoSlice],
    ) -> Result<()> {
        let conn_handle = self
            .shared
            .connections
            .lock()
            .unwrap()
            .get(&(hdr.dst_port, hdr.src_port))
            .cloned();
        let Some(ch) = conn_handle else {
            return Ok(());
        };

        let total = hdr.len as usize;
        let mut data = Vec::with_capacity(total);
        let body_len = body.len().min(total);
        data.extend_from_slice(&body[..body_len]);
        let mut rem = total - body_len;
        for buf in extra {
            if rem == 0 {
                break;
            }
            let n = buf.len().min(rem);
            data.extend_from_slice(&buf[..n]);
            rem -= n;
        }
        if rem != 0 {
            return error::InvalidBuffer.fail();
        }
        ch.tx_data_tx.send(data).ok();
        ch.tx_cnt.fetch_add(total as u32, Ordering::Relaxed);
        Ok(())
    }

    fn push_rst_tokio(
        &self,
        hdr: &VsockHeader,
    ) -> Result<()> {
        let resp = VsockHeader {
            src_cid: VSOCK_CID_HOST,
            dst_cid: self.config.guest_cid,
            src_port: hdr.dst_port,
            dst_port: hdr.src_port,
            type_: hdr.type_,
            op: VsockOp::RST,
            ..Default::default()
        };
        self.rx_data_tx.send(VsockHeaderAndData {
            hdr: resp,
            data: Vec::new(),
        }).ok();
        self.wakeup_tx.as_ref().unwrap().try_send(VsockVirtq::RX.raw()).ok();
        Ok(())
    }

    fn push_rx_tokio<'m, Q, S>(
        rx_q: &mut Queue<'_, 'm, Q>,
        irq_sender: &S,
        item: VsockHeaderAndData,
    ) -> Result<(bool, VsockHeaderAndData)>
    where
        Q: VirtQueue<'m>,
        S: IrqSender,
    {
        log::trace!("push_rx_tokio: op={:?} len={}", item.hdr.op, item.data.len());
        let expected = (HEADER_SIZE_TOKIO + item.data.len()) as u32;
        let mut written_all = false;
        {
            let hdr_bytes = item.hdr.as_bytes();
            let data_bytes = &item.data;
            let mut written = false;
            rx_q.handle_desc(VsockVirtq::RX.raw(), irq_sender, |desc| {
                if written {
                    return Ok(Status::Break);
                }
                let mut writer = DescriptorWriter::new(&mut desc.writable);
                let n1 = writer.write(hdr_bytes);
                let n2 = writer.write(data_bytes);
                let write_len = (n1 + n2) as u32;
                if write_len == expected {
                    written_all = true;
                    written = true;
                    Ok(Status::Done { len: write_len })
                } else {
                    Ok(Status::Break)
                }
            })?;
        }
        Ok((written_all, item))
    }
}

struct DescriptorWriter<'a, 'm> {
    dsts: &'a mut [IoSliceMut<'m>],
    curr_idx: usize,
    curr_off: usize,
}

impl<'a, 'm> DescriptorWriter<'a, 'm> {
    fn new(dsts: &'a mut [IoSliceMut<'m>]) -> Self {
        DescriptorWriter {
            dsts,
            curr_idx: 0,
            curr_off: 0,
        }
    }

    fn write(&mut self, src: &[u8]) -> usize {
        let mut written = 0;
        while written < src.len() && self.curr_idx < self.dsts.len() {
            let dst = &mut self.dsts[self.curr_idx];
            let avail = dst.len() - self.curr_off;
            let rem = src.len() - written;
            let amt = avail.min(rem);

            dst[self.curr_off..self.curr_off + amt].copy_from_slice(&src[written..written + amt]);

            written += amt;
            self.curr_off += amt;
            if self.curr_off == dst.len() {
                self.curr_idx += 1;
                self.curr_off = 0;
            }
        }
        written
    }
}

#[cfg(test)]
#[path = "uds_vsock_test.rs"]
mod tests;
