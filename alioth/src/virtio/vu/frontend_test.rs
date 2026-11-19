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

use std::fs::File;
use std::io::Write;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd};
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::thread;
use std::time::Duration;

use assert_matches::assert_matches;
use mio::unix::SourceFd;
use mio::{Events, Interest, Poll, Token};
use rstest::rstest;
use zerocopy::IntoBytes;

use crate::sync::notifier::Notifier;
use crate::virtio::dev::Virtio;
use crate::virtio::queue::packed::PackedQueue;
use crate::virtio::queue::split::SplitQueue;
use crate::virtio::queue::{Queue, VirtQueue};
use crate::virtio::tests::{fixture_queues, fixture_ram_bus};
use crate::virtio::vu::Error as VuError;
use crate::virtio::vu::bindings::{
    VhostUserMmap, VirtqAddr, VirtqState, VuBackMsg, VuFeature, VuFrontMsg,
};
use crate::virtio::vu::conn::{VuChannel, VuSession};
use crate::virtio::vu::frontend::VuFrontend;
use crate::virtio::worker::mio::{ActiveMio, VirtioMio};
use crate::virtio::{DeviceId, Error, IrqSender, Result, VirtioFeature};

#[derive(Debug)]
struct TestIrqSender {
    irqfd: Notifier,
}

impl Default for TestIrqSender {
    fn default() -> Self {
        Self {
            irqfd: Notifier::new().unwrap(),
        }
    }
}

impl IrqSender for TestIrqSender {
    fn queue_irq(&self, _idx: u16) {}

    fn config_irq(&self) {}

    fn queue_irqfd<F, T>(&self, _idx: u16, f: F) -> Result<T>
    where
        F: FnOnce(BorrowedFd) -> Result<T>,
    {
        f(self.irqfd.as_fd())
    }

    fn config_irqfd<F, T>(&self, f: F) -> Result<T>
    where
        F: FnOnce(BorrowedFd) -> Result<T>,
    {
        f(self.irqfd.as_fd())
    }
}

#[test]
fn test_vu_frontend_reset_stops_rings() {
    let (frontend_conn, backend_conn) = UnixStream::pair().unwrap();
    let backend = VuSession::from(backend_conn);
    let backend_thread = thread::spawn(move || {
        let mut fds = [const { None }; 8];
        let mut requests = vec![];
        while let Ok(msg) = backend.recv_msg(&mut fds) {
            let req = VuFrontMsg::from(msg.request);
            match req {
                VuFrontMsg::GET_VIRTQ_BASE => {
                    let mut virtq_base: VirtqState = backend.recv_payload().unwrap();
                    requests.push((req, virtq_base.index as u64));
                    virtq_base.val = 0;
                    backend.reply(req, &virtq_base, &[]).unwrap();
                }
                VuFrontMsg::SET_VIRTQ_ENABLE => {
                    let virtq_enable: VirtqState = backend.recv_payload().unwrap();
                    assert_eq!(virtq_enable.val, 0);
                    requests.push((req, virtq_enable.index as u64));
                    backend.reply(req, &0u64, &[]).unwrap();
                }
                VuFrontMsg::SET_STATUS => {
                    let status: u64 = backend.recv_payload().unwrap();
                    requests.push((req, status));
                    backend.reply(req, &0u64, &[]).unwrap();
                }
                req => panic!("unexpected request: {req:?}"),
            }
        }
        requests
    });

    let mut frontend = VuFrontend {
        name: "vu-frontend".into(),
        session: Arc::new(VuSession::from(frontend_conn)),
        channel: None,
        id: DeviceId::NET,
        vu_feature: VuFeature::STATUS,
        device_feature: 0,
        num_queues: 2,
        err_fds: Vec::new(),
    };
    let poll = Poll::new().unwrap();
    frontend.reset(poll.registry());
    drop(frontend);

    // Every ring is stopped before the device is reset.
    let requests = backend_thread.join().unwrap();
    assert_eq!(
        requests,
        [
            (VuFrontMsg::GET_VIRTQ_BASE, 0),
            (VuFrontMsg::SET_VIRTQ_ENABLE, 0),
            (VuFrontMsg::GET_VIRTQ_BASE, 1),
            (VuFrontMsg::SET_VIRTQ_ENABLE, 1),
            (VuFrontMsg::SET_STATUS, 0),
        ]
    );
}

#[test]
fn test_vu_frontend_reset_answers_backend() {
    let (frontend_conn, backend_conn) = UnixStream::pair().unwrap();
    let (front_conn, back_conn) = UnixStream::pair().unwrap();
    let backend = VuSession::from(backend_conn);
    let back = VuChannel { conn: back_conn };
    // A frontend that does not answer fails the test, instead of blocking
    // it.
    let timeout = Duration::from_secs(5);
    back.conn.set_read_timeout(Some(timeout)).unwrap();
    let backend_thread = thread::spawn(move || {
        let mut answers = vec![];
        while let Ok(msg) = backend.recv_msg(&mut []) {
            let req = VuFrontMsg::from(msg.request);
            let state: VirtqState = backend.recv_payload().unwrap();
            match req {
                VuFrontMsg::GET_VIRTQ_BASE => {
                    // The back-end does not stop ring 1 before the frontend
                    // has answered two requests on the back-end channel.
                    if state.index == 1 {
                        let unmap = VhostUserMmap {
                            shm_offset: 0x2000,
                            len: 0x1000,
                            ..Default::default()
                        };
                        answers.push(back.shmem_unmap(&unmap));
                        answers.push(back.shmem_unmap(&unmap));
                    }
                    backend.reply(req, &state, &[]).unwrap();
                }
                VuFrontMsg::SET_VIRTQ_ENABLE => backend.reply(req, &0u64, &[]).unwrap(),
                req => panic!("unexpected request: {req:?}"),
            }
        }
        answers
    });

    let mut frontend = VuFrontend {
        name: "vu-frontend".into(),
        session: Arc::new(VuSession::from(frontend_conn)),
        channel: Some(VuChannel { conn: front_conn }),
        id: DeviceId::FILE_SYSTEM,
        vu_feature: VuFeature::BACKEND_REQ,
        device_feature: 0,
        num_queues: 2,
        err_fds: Vec::new(),
    };
    // The worker watches the back-end channel of an active device.
    let poll = Poll::new().unwrap();
    let channel_fd = frontend.channel().unwrap().conn.as_raw_fd();
    let registry = poll.registry();
    registry
        .register(&mut SourceFd(&channel_fd), Token(2), Interest::READABLE)
        .unwrap();

    let mut handled = vec![];
    frontend.reset_with(registry, |name, channel, msg, fd| {
        let unmap: VhostUserMmap = channel.recv_payload()?;
        handled.push((name.to_owned(), msg.request, unmap.shm_offset, fd.is_some()));
        channel.reply(VuBackMsg::from(msg.request), &0u64, &[])
    });
    drop(frontend);

    let answers = backend_thread.join().unwrap();
    assert_matches!(answers[..], [Ok(()), Ok(())]);
    let unmap = VuBackMsg::SHMEM_UNMAP.raw();
    let request = ("vu-frontend".to_owned(), unmap, 0x2000, false);
    assert_eq!(handled, [request.clone(), request]);
}

/// What a transport hands to a device with two queues.
struct Rings<Q>
where
    Q: VirtQueue,
{
    queues: Box<[Option<Queue<Q>>]>,
    irq_sender: TestIrqSender,
    notifiers: [Notifier; 2],
    poll: Poll,
}

impl<Q> Rings<Q>
where
    Q: VirtQueue,
{
    /// The driver enables queue 0, and queue 1 if `enable_1` is set.
    fn new(enable_1: bool) -> Self {
        let ram_bus = Arc::new(fixture_ram_bus());
        let queue_regs = fixture_queues(2);
        queue_regs[1].enabled.store(enable_1, Ordering::Release);
        let queues = queue_regs
            .iter()
            .map(|reg| Queue::<Q>::new(reg, ram_bus.clone(), false))
            .collect::<Result<_>>()
            .unwrap();
        Rings {
            queues,
            irq_sender: TestIrqSender::default(),
            notifiers: [Notifier::new().unwrap(), Notifier::new().unwrap()],
            poll: Poll::new().unwrap(),
        }
    }

    fn activate(&mut self, frontend: &mut VuFrontend, feature: u128) {
        let mut active_mio = ActiveMio {
            queues: &mut self.queues,
            irq_sender: &self.irq_sender,
            notifiers: &self.notifiers,
            poll: &mut self.poll,
        };
        frontend.activate(feature, &mut active_mio).unwrap();
    }
}

fn activate_with<Q>(frontend: &mut VuFrontend, feature: u128)
where
    Q: VirtQueue,
{
    Rings::<Q>::new(true).activate(frontend, feature);
}

/// The ring index of a recorded request that is not about a ring.
const NO_RING: u32 = u32::MAX;

/// Spawns a back-end that acknowledges the requests about rings, and
/// records them with the ring index and the value of each.
fn spawn_backend(conn: UnixStream) -> thread::JoinHandle<Vec<(VuFrontMsg, u32, u64)>> {
    let backend = VuSession::from(conn);
    thread::spawn(move || {
        let mut fds = [const { None }; 8];
        let mut requests = vec![];
        while let Ok(msg) = backend.recv_msg(&mut fds) {
            let req = VuFrontMsg::from(msg.request);
            match req {
                VuFrontMsg::SET_FEATURES | VuFrontMsg::SET_STATUS => {
                    let val: u64 = backend.recv_payload().unwrap();
                    requests.push((req, NO_RING, val));
                }
                VuFrontMsg::SET_VIRTQ_KICK
                | VuFrontMsg::SET_VIRTQ_CALL
                | VuFrontMsg::SET_VIRTQ_ERR => {
                    let index: u64 = backend.recv_payload().unwrap();
                    requests.push((req, index as u32, 0));
                }
                VuFrontMsg::SET_VIRTQ_NUM
                | VuFrontMsg::SET_VIRTQ_BASE
                | VuFrontMsg::SET_VIRTQ_ENABLE => {
                    let state: VirtqState = backend.recv_payload().unwrap();
                    requests.push((req, state.index, state.val as u64));
                }
                VuFrontMsg::SET_VIRTQ_ADDR => {
                    let addr: VirtqAddr = backend.recv_payload().unwrap();
                    requests.push((req, addr.index, 0));
                }
                req => panic!("unexpected request: {req:?}"),
            }
            backend.reply(req, &0u64, &[]).unwrap();
        }
        requests
    })
}

#[test]
fn test_vu_frontend_activate_starts_rings_last() {
    let (frontend_conn, backend_conn) = UnixStream::pair().unwrap();
    let backend_thread = spawn_backend(backend_conn);
    let mut frontend = VuFrontend {
        name: "vu-frontend".into(),
        session: Arc::new(VuSession::from(frontend_conn)),
        channel: None,
        id: DeviceId::NET,
        vu_feature: VuFeature::STATUS,
        device_feature: 0,
        num_queues: 2,
        err_fds: Vec::new(),
    };

    // The driver enabled queue 0 only.
    let mut rings = Rings::<SplitQueue>::new(false);
    let feature = VirtioFeature::VERSION_1.bits();
    rings.activate(&mut frontend, feature);
    drop(frontend);

    // SET_VRING_KICK starts a ring, so it is the last request for a ring.
    let size = rings.queues[0].as_ref().unwrap().addrs().size as u64;
    let set_feat = (feature | VirtioFeature::VHOST_PROTOCOL.bits()) as u64;
    let requests = backend_thread.join().unwrap();
    assert_eq!(
        requests,
        [
            (VuFrontMsg::SET_FEATURES, NO_RING, set_feat),
            (VuFrontMsg::SET_VIRTQ_ERR, 0, 0),
            (VuFrontMsg::SET_VIRTQ_CALL, 0, 0),
            (VuFrontMsg::SET_VIRTQ_NUM, 0, size),
            (VuFrontMsg::SET_VIRTQ_BASE, 0, 0),
            (VuFrontMsg::SET_VIRTQ_ADDR, 0, 0),
            (VuFrontMsg::SET_VIRTQ_ENABLE, 0, 1),
            (VuFrontMsg::SET_VIRTQ_KICK, 0, 0),
            (VuFrontMsg::SET_VIRTQ_KICK, 1, 0),
            (VuFrontMsg::SET_STATUS, NO_RING, 0xf),
        ]
    );
}

#[rstest]
#[case(VirtioFeature::VERSION_1, 0)]
#[case(VirtioFeature::VERSION_1 | VirtioFeature::RING_PACKED, (1 << 15) | (1 << 31))]
fn test_vu_frontend_activate_vring_base(#[case] drv_feat: VirtioFeature, #[case] expected: u32) {
    let (frontend_conn, backend_conn) = UnixStream::pair().unwrap();
    let backend = VuSession::from(backend_conn);
    let backend_thread = thread::spawn(move || {
        let mut fds = [const { None }; 8];
        let mut set_feat = None;
        let mut bases = vec![];
        while let Ok(msg) = backend.recv_msg(&mut fds) {
            let req = VuFrontMsg::from(msg.request);
            match req {
                VuFrontMsg::SET_FEATURES => {
                    let feat: u64 = backend.recv_payload().unwrap();
                    set_feat = Some(feat);
                    backend.reply(req, &0u64, &[]).unwrap();
                }
                VuFrontMsg::SET_VIRTQ_KICK
                | VuFrontMsg::SET_VIRTQ_CALL
                | VuFrontMsg::SET_VIRTQ_ERR
                | VuFrontMsg::SET_STATUS => {
                    let _: u64 = backend.recv_payload().unwrap();
                    backend.reply(req, &0u64, &[]).unwrap();
                }
                VuFrontMsg::SET_VIRTQ_NUM | VuFrontMsg::SET_VIRTQ_ENABLE => {
                    let _: VirtqState = backend.recv_payload().unwrap();
                    backend.reply(req, &0u64, &[]).unwrap();
                }
                VuFrontMsg::SET_VIRTQ_BASE => {
                    let state: VirtqState = backend.recv_payload().unwrap();
                    bases.push((state.index, state.val));
                    backend.reply(req, &0u64, &[]).unwrap();
                }
                VuFrontMsg::SET_VIRTQ_ADDR => {
                    let _: VirtqAddr = backend.recv_payload().unwrap();
                    backend.reply(req, &0u64, &[]).unwrap();
                }
                req => panic!("unexpected request: {req:?}"),
            }
        }
        (set_feat, bases)
    });

    let device_feature = (drv_feat | VirtioFeature::VHOST_PROTOCOL).bits() as u64;
    let mut frontend = VuFrontend {
        name: "vu-frontend".into(),
        session: Arc::new(VuSession::from(frontend_conn)),
        channel: None,
        id: DeviceId::NET,
        vu_feature: VuFeature::STATUS,
        device_feature,
        num_queues: 2,
        err_fds: Vec::new(),
    };

    // The worker picks the queue type by the driver features.
    if drv_feat.contains(VirtioFeature::RING_PACKED) {
        activate_with::<PackedQueue>(&mut frontend, drv_feat.bits());
    } else {
        activate_with::<SplitQueue>(&mut frontend, drv_feat.bits());
    }
    drop(frontend);

    let (set_feat, bases) = backend_thread.join().unwrap();
    assert_eq!(set_feat, Some(device_feature));
    assert_eq!(bases, [(0, expected), (1, expected)]);
}

#[test]
fn test_vu_frontend_feature_filtering() {
    let (frontend_conn, _backend_conn) = UnixStream::pair().unwrap();
    let dev_low = (1 << 0) | (1 << 5) | (1 << 23);
    let dev_high = (1 << 50) | (1 << 57) | (1 << 63);
    let supported_transport = (VirtioFeature::INDIRECT_DESC
        | VirtioFeature::EVENT_IDX
        | VirtioFeature::VERSION_1
        | VirtioFeature::ACCESS_PLATFORM
        | VirtioFeature::RING_PACKED)
        .bits() as u64;
    // VHOST_PROTOCOL (30) plus unsupported/reserved transport bits:
    // NOTIFY_ON_EMPTY (24), ANY_LAYOUT (27), IN_ORDER (35),
    // NOTIFICATION_DATA (38), RING_RESET (40), ADMIN_VQ (41), SUSPEND (43),
    // and reserved bit 49.
    let unsupported_transport = VirtioFeature::VHOST_PROTOCOL.bits() as u64
        | (1 << 24)
        | (1 << 27)
        | (1 << 35)
        | (1 << 38)
        | (1 << 40)
        | (1 << 41)
        | (1 << 43)
        | (1 << 49);

    let frontend = VuFrontend {
        name: "vu-frontend".into(),
        session: Arc::new(VuSession::from(frontend_conn)),
        channel: None,
        id: DeviceId::NET,
        vu_feature: VuFeature::empty(),
        device_feature: dev_low | dev_high | supported_transport | unsupported_transport,
        num_queues: 1,
        err_fds: Vec::new(),
    };

    let exposed = frontend.feature();
    assert_eq!(exposed, (dev_low | dev_high | supported_transport) as u128);
    assert_eq!(exposed & VirtioFeature::VHOST_PROTOCOL.bits(), 0);
    assert_eq!(exposed & (unsupported_transport as u128), 0);
}

#[test]
fn test_vu_frontend_queue_err() {
    let (frontend_conn, backend_conn) = UnixStream::pair().unwrap();
    let backend = VuSession::from(backend_conn);
    let (err_tx, err_rx) = flume::unbounded();
    let backend_thread = thread::spawn(move || {
        let mut fds = [const { None }; 8];
        while let Ok(msg) = backend.recv_msg(&mut fds) {
            let req = VuFrontMsg::from(msg.request);
            match req {
                VuFrontMsg::SET_VIRTQ_ERR => {
                    let index: u64 = backend.recv_payload().unwrap();
                    let fd = fds[0].take().unwrap();
                    err_tx.send((index, File::from(fd))).unwrap();
                    backend.reply(req, &0u64, &[]).unwrap();
                }
                VuFrontMsg::SET_FEATURES
                | VuFrontMsg::SET_VIRTQ_KICK
                | VuFrontMsg::SET_VIRTQ_CALL => {
                    let _: u64 = backend.recv_payload().unwrap();
                    backend.reply(req, &0u64, &[]).unwrap();
                }
                VuFrontMsg::SET_VIRTQ_NUM
                | VuFrontMsg::SET_VIRTQ_BASE
                | VuFrontMsg::SET_VIRTQ_ENABLE => {
                    let _: VirtqState = backend.recv_payload().unwrap();
                    backend.reply(req, &0u64, &[]).unwrap();
                }
                VuFrontMsg::SET_VIRTQ_ADDR => {
                    let _: VirtqAddr = backend.recv_payload().unwrap();
                    backend.reply(req, &0u64, &[]).unwrap();
                }
                VuFrontMsg::GET_VIRTQ_BASE => {
                    let virtq_base: VirtqState = backend.recv_payload().unwrap();
                    backend.reply(req, &virtq_base, &[]).unwrap();
                }
                req => panic!("unexpected request: {req:?}"),
            }
        }
    });

    let mut frontend = VuFrontend {
        name: "vu-frontend".into(),
        session: Arc::new(VuSession::from(frontend_conn)),
        channel: None,
        id: DeviceId::NET,
        vu_feature: VuFeature::empty(),
        device_feature: 0,
        num_queues: 2,
        err_fds: Vec::new(),
    };

    // The driver enabled only the second queue.
    let ram_bus = Arc::new(fixture_ram_bus());
    let queue_regs = fixture_queues(2);
    queue_regs[0].enabled.store(false, Ordering::Release);
    let mut queues: Box<_> = queue_regs
        .iter()
        .map(|reg| Queue::<SplitQueue>::new(reg, ram_bus.clone(), false))
        .collect::<Result<_>>()
        .unwrap();
    let irq_sender = TestIrqSender::default();
    let notifiers = [Notifier::new().unwrap(), Notifier::new().unwrap()];
    let mut poll = Poll::new().unwrap();
    let mut active_mio = ActiveMio {
        queues: &mut queues,
        irq_sender: &irq_sender,
        notifiers: &notifiers,
        poll: &mut poll,
    };
    let feature = VirtioFeature::VERSION_1.bits();

    // The backend gets an error fd for the enabled queue only.
    frontend.activate(feature, &mut active_mio).unwrap();
    let (index, mut err_fd) = err_rx.recv().unwrap();
    assert_eq!(index, 1);
    assert!(err_rx.is_empty());

    // An error signaled by the backend fails the device.
    err_fd.write_all(1u64.as_bytes()).unwrap();
    let mut events = Events::with_capacity(4);
    active_mio
        .poll
        .poll(&mut events, Some(Duration::from_secs(5)))
        .unwrap();
    let event = events.iter().next().unwrap();
    assert_matches!(
        frontend.handle_event(event, &mut active_mio),
        Err(Error::Vu { source, .. }) if matches!(*source, VuError::QueueErr { index: 1, .. })
    );

    // After a reset, the next activation starts with a new error fd, and
    // the frontend no longer watches the old one.
    frontend.reset(active_mio.poll.registry());
    frontend.activate(feature, &mut active_mio).unwrap();
    assert_matches!(err_rx.recv(), Ok((1, _)));
    err_fd.write_all(1u64.as_bytes()).unwrap();
    active_mio
        .poll
        .poll(&mut events, Some(Duration::ZERO))
        .unwrap();
    assert!(events.is_empty());

    drop(frontend);
    backend_thread.join().unwrap();
}

#[test]
fn test_vu_frontend_handle_queue() {
    let (frontend_conn, _backend_conn) = UnixStream::pair().unwrap();
    let mut frontend = VuFrontend {
        name: "vu-frontend".into(),
        session: Arc::new(VuSession::from(frontend_conn)),
        channel: None,
        id: DeviceId::NET,
        vu_feature: VuFeature::empty(),
        device_feature: 0,
        num_queues: 1,
        err_fds: Vec::new(),
    };

    let mut poll = Poll::new().unwrap();
    let mut kick = Notifier::new().unwrap();
    poll.registry()
        .register(&mut kick, Token(0), Interest::READABLE)
        .unwrap();
    let notifiers = [kick];
    let mut queues: [Option<Queue<SplitQueue>>; 0] = [];
    let irq_sender = TestIrqSender::default();
    let mut active_mio = ActiveMio {
        queues: &mut queues,
        irq_sender: &irq_sender,
        notifiers: &notifiers,
        poll: &mut poll,
    };

    // With a kick notifier available, handle_queue forwards the notification.
    assert_matches!(frontend.handle_queue(0, &mut active_mio), Ok(()));
    let mut events = Events::with_capacity(1);
    active_mio
        .poll
        .poll(&mut events, Some(Duration::from_secs(5)))
        .unwrap();
    assert!(!events.is_empty());

    // Without a kick notifier (out-of-range queue), handle_queue logs and does not panic.
    assert_matches!(frontend.handle_queue(1, &mut active_mio), Ok(()));
}
