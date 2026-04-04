// Copyright 2026 Google LLC
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

#[cfg(target_arch = "aarch64")]
#[path = "cpu_aarch64.rs"]
mod aarch64;
#[cfg(target_arch = "x86_64")]
#[path = "cpu_x86_64/cpu_x86_64.rs"]
mod x86_64;

#[cfg(target_os = "linux")]
use std::collections::HashMap;
use std::collections::HashSet;
#[cfg(target_os = "linux")]
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU16, Ordering};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use flume::{Receiver, Sender};
use libc::SCHED_BATCH;
use parking_lot::{Condvar, Mutex, MutexGuard, RwLock, RwLockReadGuard, RwLockWriteGuard};
use snafu::{ResultExt, Snafu};

#[cfg(target_arch = "aarch64")]
use crate::arch::layout::{PL011_START, PL031_START};
#[cfg(target_arch = "x86_64")]
use crate::arch::layout::{PORT_CMOS_REG, PORT_COM1, PORT_FW_CFG_SELECTOR, PORT_FWDBG};
use crate::board::{Board, BoardConfig};
use crate::device::clock::SystemClock;
#[cfg(target_arch = "x86_64")]
use crate::device::cmos::Cmos;
use crate::device::console::StdioConsole;
#[cfg(target_arch = "x86_64")]
use crate::device::fw_cfg::{FwCfg, FwCfgItemParam};
#[cfg(target_arch = "x86_64")]
use crate::device::fw_dbg::FwDbg;
#[cfg(target_arch = "aarch64")]
use crate::device::pl011::Pl011;
#[cfg(target_arch = "aarch64")]
use crate::device::pl031::Pl031;
#[cfg(target_arch = "x86_64")]
use crate::device::serial::Serial;
use crate::errors::{DebugTrace, trace_error};
use crate::hv::{Hypervisor, IoeventFdRegistry, Vcpu, Vm, VmEntry, VmExit};
#[cfg(target_arch = "x86_64")]
use crate::loader::xen;
use crate::loader::{Executable, InitState, Payload, linux};
use crate::pci::pvpanic::PvPanic;
use crate::pci::{Bdf, Pci};
#[cfg(target_os = "linux")]
use crate::sys::vfio::VfioIommu;
#[cfg(target_os = "linux")]
use crate::vfio::cdev::Cdev;
#[cfg(target_os = "linux")]
use crate::vfio::container::{Container, UpdateContainerMapping};
#[cfg(target_os = "linux")]
use crate::vfio::group::{DevFd, Group};
#[cfg(target_os = "linux")]
use crate::vfio::iommu::{Ioas, Iommu, UpdateIommuIoas};
#[cfg(target_os = "linux")]
use crate::vfio::pci::VfioPciDev;
#[cfg(target_os = "linux")]
use crate::vfio::{CdevParam, ContainerParam, GroupParam, IoasParam};
use crate::virtio::dev::{DevParam, Virtio, VirtioDevice};
use crate::virtio::pci::VirtioPciDevice;

#[trace_error]
#[derive(Snafu, DebugTrace)]
#[snafu(module, context(suffix(false)))]
pub enum Error {
    #[snafu(display("Hypervisor internal error"), context(false))]
    HvError { source: Box<crate::hv::Error> },
    #[snafu(display("Failed to create VCPU-{index} thread"))]
    VcpuThread { index: u16, error: std::io::Error },
    #[snafu(display("Failed to create a console"))]
    CreateConsole { error: Box<crate::device::Error> },
    #[snafu(display("Failed to create fw_cfg device"))]
    FwCfg { error: std::io::Error },
    #[snafu(display("Failed to create a VirtIO device"), context(false))]
    CreateVirtio { source: Box<crate::virtio::Error> },
    #[snafu(display("Guest memory is not backed by sharable file descriptors"))]
    MemNotSharedFd,
    #[cfg(target_os = "linux")]
    #[snafu(display("Failed to create a VFIO device"), context(false))]
    CreateVfio { source: Box<crate::vfio::Error> },
    #[snafu(display("Failed to configure guest memory"), context(false))]
    Memory { source: Box<crate::mem::Error> },
    #[snafu(display("Failed to setup board"), context(false))]
    Board { source: Box<crate::board::Error> },
    #[cfg(target_os = "linux")]
    #[snafu(display("{name:?} already exists"))]
    AlreadyExists { name: Box<str> },
    #[cfg(target_os = "linux")]
    #[snafu(display("{name:?} does not exist"))]
    NotExist { name: Box<str> },
    #[snafu(display("Failed to reset PCI devices"))]
    ResetPci { source: Box<crate::pci::Error> },
    #[snafu(display("Firmware error"), context(false))]
    Firmware { source: Box<crate::firmware::Error> },
    #[snafu(display("Unknown firmware metadata"))]
    UnknownFirmwareMetadata,
    #[snafu(display("Missing payload"))]
    MissingPayload,
    #[snafu(display("Failed to load payload"), context(false))]
    Loader { source: Box<crate::loader::Error> },
    #[snafu(display("Failed to notify the VMM thread"))]
    NotifyVmm,
    #[snafu(display("Another VCPU thread has signaled failure"))]
    PeerFailure,
    #[snafu(display("Unexpected state: {state:?}, want {want:?}"))]
    UnexpectedState { state: State, want: State },
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Paused,
    Running,
    Shutdown,
    RebootPending,
}

pub(crate) struct MpSync {
    pub(crate) state: State,
    fatal: bool,
    count: u16,
}

pub enum VcpuCmd {
    Snapshot,
}

pub enum VcpuMessage {
    Created,
    Finished,
    Snapshot,
}

pub struct VcpuResp {
    pub index: u16,
    pub msg: VcpuMessage,
}

pub struct VcpuHandle {
    pub thread: JoinHandle<Result<()>>,
    pub cmd_tx: Sender<VcpuCmd>,
}

pub struct Context<V: Vm> {
    pub board: Board<V>,
    pub vcpus: RwLock<Vec<VcpuHandle>>,

    pub(crate) sync: RwLock<MpSync>,
    // cond: Condvar,
}

impl<V: Vm> Context<V> {
    pub fn new(board: Board<V>) -> Self {
        Self {
            board,
            vcpus: RwLock::new(Vec::new()),
            sync: RwLock::new(MpSync {
                state: State::Paused,
                fatal: false,
                count: 0,
            }),
            // cond: Condvar::new(),
        }
    }
}

struct VcpuThread<V: Vm> {
    ctx: Arc<Context<V>>,
    index: u16,
    event_tx: Sender<VcpuResp>,
    cmd_rx: Receiver<VcpuCmd>,
    vcpu: <V as Vm>::Vcpu,
}

fn notify_vmm(event_tx: &Sender<VcpuResp>, index: u16, msg: VcpuMessage) -> Result<()> {
    if event_tx.send(VcpuResp { index, msg }).is_err() {
        error::NotifyVmm.fail()
    } else {
        Ok(())
    }
}

impl<V: Vm> VcpuThread<V> {
    pub fn new(
        index: u16,
        ctx: Arc<Context<V>>,
        event_tx: Sender<VcpuResp>,
        cmd_rx: Receiver<VcpuCmd>,
    ) -> Result<Self> {
        let identity = ctx.board.encode_cpu_identity(index);
        let vcpu = ctx.board.vm.create_vcpu(index, identity)?;

        Ok(Self {
            ctx,
            index,
            event_tx,
            cmd_rx,
            vcpu,
        })
    }

    fn notify_vmm(&self, msg: VcpuMessage) -> Result<()> {
        notify_vmm(&self.event_tx, self.index, msg)
    }

    fn park(&self, sync: RwLockReadGuard<'_, MpSync>) -> RwLockReadGuard<'_, MpSync> {
        drop(sync);
        thread::park();
        self.ctx.sync.read()
    }

    fn unpark(&self, vcpus: &[VcpuHandle]) {
        for (index, vcpu) in vcpus.iter().enumerate() {
            if index == self.index as usize {
                continue;
            }
            vcpu.thread.thread().unpark();
        }
    }

    fn sync_vcpus(&self, vcpus: &[VcpuHandle]) -> Result<()> {
        let mut sync = self.ctx.sync.write();
        if sync.fatal {
            return error::PeerFailure.fail();
        }
        sync.count += 1;

        if sync.count == vcpus.len() as u16 {
            sync.count = 0;
            drop(sync);
            self.unpark(vcpus);
        } else {
            let mut sync = RwLockWriteGuard::downgrade(sync);
            while sync.count != 0 && !sync.fatal {
                sync = self.park(sync);
            }
            if sync.fatal {
                return error::PeerFailure.fail();
            }
        }

        // let count = self.ctx.sync.count.fetch_add(1, Ordering::AcqRel);

        // // while
        // if count == vcpus.len() as u16 - 1 {
        //     self.ctx.sync.count.store(0, Ordering::Release);
        //     self.ctx.cond.notify_all();
        //     log::info!("VCPU-{}: unblocking other VCPUs", self.index);
        // }

        Ok(())

        // let mut sync = self.ctx.sync.lock();
        // if sync.fatal {
        //     return error::PeerFailure.fail();
        // }

        // sync.count += 1;
        // while sync.count != vcpus.len() as u16 && sync.count != 0 {
        //     self.ctx.cond.wait(&mut sync)
        // }
        // if sync.count == vcpus.len() as u16 {
        //     sync.count = 0;
        //     self.ctx.cond.notify_all();
        //     log::info!("VCPU-{}: unblocking other VCPUs", self.index);
        // }

        // if sync.fatal {
        //     return error::PeerFailure.fail();
        // }

        // Ok(())
    }

    fn load_payload(&self) -> Result<InitState> {
        let payload = self.ctx.board.payload.read();
        let Some(payload) = payload.as_ref() else {
            return error::MissingPayload.fail();
        };

        if let Some(fw) = payload.firmware.as_ref() {
            return self.setup_firmware(fw, payload);
        }

        let Some(exec) = &payload.executable else {
            return error::MissingPayload.fail();
        };
        let mem_regions = self.ctx.board.memory.mem_region_entries();
        let init_state = match exec {
            Executable::Linux(image) => linux::load(
                &self.ctx.board.memory.ram_bus(),
                &mem_regions,
                image.as_ref(),
                payload.cmdline.as_deref(),
                payload.initramfs.as_deref(),
            ),
            #[cfg(target_arch = "x86_64")]
            Executable::Pvh(image) => xen::load(
                &self.ctx.board.memory.ram_bus(),
                &mem_regions,
                image.as_ref(),
                payload.cmdline.as_deref(),
                payload.initramfs.as_deref(),
            ),
        }?;
        Ok(init_state)
    }

    fn boot_init_sync(&mut self) -> Result<()> {
        let ctx = self.ctx.clone();
        let vcpus = ctx.vcpus.read();
        if self.index == 0 {
            self.ctx.board.init_devices()?;
            let init_state = self.load_payload()?;
            self.init_boot_vcpu(&init_state)?;
            self.ctx.board.create_firmware_data(&init_state)?;
        }
        self.init_ap(&vcpus)?;
        self.coco_finalize(&vcpus)?;
        self.sync_vcpus(&vcpus)
    }

    fn vcpu_loop(&mut self) -> Result<State> {
        let mut vm_entry = VmEntry::None;
        loop {
            let vm_exit = self.vcpu.run(vm_entry)?;
            let memory = &self.ctx.board.memory;
            vm_entry = match vm_exit {
                #[cfg(target_arch = "x86_64")]
                VmExit::Io { port, write, size } => memory.handle_io(port, write, size)?,
                VmExit::Mmio { addr, write, size } => memory.handle_mmio(addr, write, size)?,
                VmExit::Shutdown => break Ok(State::Shutdown),
                VmExit::Reboot => break Ok(State::RebootPending),
                VmExit::Paused => break Ok(State::Paused),
                VmExit::Interrupted => {
                    let state = self.ctx.sync.read();
                    match state.state {
                        State::Shutdown => VmEntry::Shutdown,
                        State::RebootPending => VmEntry::Reboot,
                        State::Paused => VmEntry::Pause,
                        State::Running => VmEntry::None,
                    }
                }
                VmExit::ConvertMemory { gpa, size, private } => {
                    memory.mark_private_memory(gpa, size, private)?;
                    VmEntry::None
                }
            };
        }
    }

    fn handle_cmds(&self) -> Result<()> {
        while let Ok(cmd) = self.cmd_rx.try_recv() {
            match cmd {
                VcpuCmd::Snapshot => {
                    log::info!("VCPU-{}: Snapshot command received", self.index);

                    // Handle snapshot command
                }
            }
        }
        Ok(())
    }

    fn run(&mut self) -> Result<()> {
        self.init_vcpu()?;

        'reboot: loop {
            let mut sync = self.ctx.sync.read();
            loop {
                match sync.state {
                    State::Running => break,
                    State::Paused => {
                        self.handle_cmds()?;
                        sync = self.park(sync);
                    }
                    State::RebootPending => continue 'reboot,
                    State::Shutdown => break 'reboot Ok(()),
                }
            }
            drop(sync);

            self.boot_init_sync()?;

            let request = 'pause: loop {
                let request = self.vcpu_loop();

                let vcpus = self.ctx.vcpus.read();
                let mut sync = self.ctx.sync.write();
                if sync.state == State::Running {
                    sync.state = match request {
                        Ok(State::RebootPending) => State::RebootPending,
                        Ok(State::Paused) => State::Paused,
                        _ => State::Shutdown,
                    };
                    log::trace!("VCPU-{}: change state to {:?}", self.index, sync.state);
                    stop_vcpus(&self.ctx.board, Some(self.index), &vcpus)?;
                }
                let mut sync = RwLockWriteGuard::downgrade(sync);
                loop {
                    match sync.state {
                        State::Running => break,
                        State::Paused => {
                            self.handle_cmds()?;
                            sync = self.park(sync);
                        }
                        State::RebootPending | State::Shutdown => break 'pause request,
                    }
                }
            };

            if self.index == 0 {
                let board = &self.ctx.board;
                board.pci_bus.segment.reset().context(error::ResetPci)?;
                board.memory.reset()?;
            }
            self.reset_vcpu()?;

            request?;

            let vcpus = self.ctx.vcpus.read();
            self.sync_vcpus(&vcpus)?;

            let mut sync = self.ctx.sync.write();
            match sync.state {
                State::RebootPending => sync.state = State::Running,
                State::Shutdown => break Ok(()),
                _ => {}
            }
        }
    }
}

fn vcpu_thread_<V: Vm>(
    index: u16,
    ctx: Arc<Context<V>>,
    event_tx: Sender<VcpuResp>,
    cmd_rx: Receiver<VcpuCmd>,
) -> Result<()> {
    let mut thread = VcpuThread::new(index, ctx, event_tx, cmd_rx)?;
    thread.notify_vmm(VcpuMessage::Created)?;
    thread.run()
}

pub fn vcpu_thread<V: Vm>(
    index: u16,
    ctx: Arc<Context<V>>,
    event_tx: Sender<VcpuResp>,
    cmd_rx: Receiver<VcpuCmd>,
) -> Result<()> {
    let ret = vcpu_thread_(index, ctx.clone(), event_tx.clone(), cmd_rx);

    let _ = notify_vmm(&event_tx, index, VcpuMessage::Finished);

    if matches!(ret, Ok(_) | Err(Error::PeerFailure { .. })) {
        return Ok(());
    }

    log::warn!("VCPU-{index} reported error {ret:?}, unblocking other VCPUs...");
    let mut sync = ctx.sync.write();
    sync.fatal = true;
    drop(sync);
    unpark_vcpus(Some(index), &ctx.vcpus.read());
    ret
}

pub fn stop_vcpus<V: Vm>(
    board: &Board<V>,
    current: Option<u16>,
    vcpus: &[VcpuHandle],
) -> Result<()> {
    for (index, handle) in vcpus.iter().enumerate() {
        let index = index as u16;
        if let Some(current) = current {
            if current == index {
                continue;
            }
            log::info!("VCPU-{current}: stopping VCPU-{index}");
        } else {
            log::info!("Stopping VCPU-{index}");
        }
        let identity = board.encode_cpu_identity(index);
        board.vm.stop_vcpu(identity, &handle.thread)?;
    }
    Ok(())
}

pub fn unpark_vcpus(current: Option<u16>, vcpus: &[VcpuHandle]) {
    for (i, vcpu) in vcpus.iter().enumerate() {
        if Some(i as u16) == current {
            continue;
        }
        vcpu.thread.thread().unpark();
    }
}
