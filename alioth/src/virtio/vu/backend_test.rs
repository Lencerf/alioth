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
use std::io::{ErrorKind, Read};
use std::os::fd::AsFd;
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use flume::{Receiver, Sender};
use mio::{Events, Interest, Poll, Token};
use rstest::rstest;

use crate::mem;
use crate::mem::emulated::{Action, Mmio};
use crate::mem::mapped::RamBus;
use crate::sync::notifier::Notifier;
use crate::virtio::dev::{VirtioDevice, WakeEvent, WorkerCtl, WorkerReport};
use crate::virtio::queue::packed::PackedQueue;
use crate::virtio::queue::split::SplitQueue;
use crate::virtio::queue::{QueueReg, VirtQueue};
use crate::virtio::vu::backend::{VuBackend, VuIrqSender};
use crate::virtio::vu::bindings::{MemoryRegion, MemorySingleRegion, VirtqAddr, VirtqState};
use crate::virtio::vu::conn::VuSession;
use crate::virtio::{DevStatus, DeviceId, IrqSender, VirtioFeature};

const TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug)]
struct TestDevConfig;

impl Mmio for TestDevConfig {
    fn size(&self) -> u64 {
        0
    }

    fn read(&self, _offset: u64, _size: u8) -> mem::Result<u64> {
        Ok(0)
    }

    fn write(&self, _offset: u64, _size: u8, _val: u64) -> mem::Result<Action> {
        Ok(Action::None)
    }
}

type TestWakeReceiver = Receiver<WakeEvent<VuIrqSender>>;

/// Serves a device with one queue, whose worker is `worker`, on a backend
/// thread. Returns the frontend end of the connection and the thread.
///
/// The thread joins the worker after the frontend disconnects.
fn spawn_backend<W>(worker: W) -> (VuSession, JoinHandle<()>)
where
    W: FnOnce(TestWakeReceiver, Sender<WorkerReport>) + Send + 'static,
{
    let notifier = Arc::new(Notifier::new().unwrap());
    spawn_backend_on(Arc::default(), notifier, worker)
}

/// Like [`spawn_backend`], with the guest memory of the backend and the
/// notifier of the worker.
fn spawn_backend_on<W>(
    memory: Arc<RamBus>,
    notifier: Arc<Notifier>,
    worker: W,
) -> (VuSession, JoinHandle<()>)
where
    W: FnOnce(TestWakeReceiver, Sender<WorkerReport>) + Send + 'static,
{
    let queue_regs = Arc::new([QueueReg::default()]);
    spawn_backend_with(queue_regs, memory, notifier, worker)
}

/// Like [`spawn_backend_on`], with the queue registers of the device, which
/// the backend shares with the worker.
fn spawn_backend_with<W>(
    queue_regs: Arc<[QueueReg]>,
    memory: Arc<RamBus>,
    notifier: Arc<Notifier>,
    worker: W,
) -> (VuSession, JoinHandle<()>)
where
    W: FnOnce(TestWakeReceiver, Sender<WorkerReport>) + Send + 'static,
{
    let (event_tx, event_rx) = flume::unbounded();
    let (report_tx, report_rx) = flume::unbounded();
    let dev = VirtioDevice {
        name: "vu-backend".into(),
        id: DeviceId::NET,
        device_config: Arc::new(TestDevConfig),
        device_feature: 0,
        queue_regs,
        shared_mem_regions: None,
        ctl: Arc::new(WorkerCtl::new(
            "vu-backend".into(),
            event_tx,
            notifier,
            report_rx,
        )),
        worker_handle: None,
    };
    let worker = thread::spawn(move || worker(event_rx, report_tx));

    let (frontend_conn, backend_conn) = UnixStream::pair().unwrap();
    let mut backend = VuBackend::new(backend_conn, dev, memory).unwrap();
    let backend_thread = thread::spawn(move || {
        backend.run().unwrap();
        // Dropping the backend disconnects the worker.
        drop(backend);
        worker.join().unwrap();
    });
    (VuSession::from(frontend_conn), backend_thread)
}

fn driver_ok() -> u64 {
    let status = DevStatus::ACK | DevStatus::DRIVER | DevStatus::FEATURES_OK | DevStatus::DRIVER_OK;
    status.bits() as u64
}

/// Starts ring `index`. The backend takes the kick fd when it starts the
/// worker.
fn start_ring(frontend: &VuSession, index: u64) {
    let kick = Notifier::new().unwrap();
    frontend.set_virtq_kick(&index, kick.as_fd()).unwrap();
}

/// Starts a device with one queue.
fn start(frontend: &VuSession) {
    start_ring(frontend, 0);
    frontend.set_status(&driver_ok()).unwrap();
}

/// Returns how often `eventfd` was signalled since the last call.
fn take_signals(eventfd: &Notifier) -> u64 {
    let mut eventfd = File::from(eventfd.as_fd().try_clone_to_owned().unwrap());
    let mut count = [0u8; 8];
    match eventfd.read(&mut count) {
        Ok(_) => u64::from_ne_bytes(count),
        Err(e) if e.kind() == ErrorKind::WouldBlock => 0,
        Err(e) => panic!("cannot read eventfd: {e:?}"),
    }
}

#[test]
fn test_vu_backend_stops_worker_before_reply() {
    // A fake worker that takes a while to stop.
    let starts = Arc::new(AtomicU32::new(0));
    let stopped = Arc::new(AtomicBool::new(false));
    let (worker_starts, worker_stopped) = (starts.clone(), stopped.clone());
    let (frontend, backend_thread) = spawn_backend(move |event_rx, report_tx| {
        for event in event_rx.iter() {
            match event {
                WakeEvent::Start { .. } => {
                    worker_starts.fetch_add(1, Ordering::AcqRel);
                }
                WakeEvent::Reset { seq } => {
                    thread::sleep(Duration::from_millis(50));
                    worker_stopped.store(true, Ordering::Release);
                    report_tx.send(WorkerReport::ResetDone { seq }).unwrap();
                }
                WakeEvent::Pause { seq } => {
                    thread::sleep(Duration::from_millis(50));
                    worker_stopped.store(true, Ordering::Release);
                    report_tx.send(WorkerReport::Paused { seq }).unwrap();
                }
                event => panic!("unexpected event: {event:?}"),
            }
        }
    });

    // The reply to SET_STATUS(0) means that the worker has stopped.
    start(&frontend);
    frontend.set_status(&0).unwrap();
    assert!(stopped.swap(false, Ordering::AcqRel));

    // So does the reply to GET_VRING_BASE, for which the worker pauses.
    start(&frontend);
    let virtq_base = VirtqState { index: 0, val: 0 };
    assert_eq!(frontend.get_virtq_base(&virtq_base).unwrap().val, 0);
    assert!(stopped.swap(false, Ordering::AcqRel));

    // A paused worker still has to reset the device.
    frontend.set_status(&0).unwrap();
    assert!(stopped.swap(false, Ordering::AcqRel));

    // The worker has already stopped, so there is nothing left to stop.
    frontend.set_status(&0).unwrap();
    assert_eq!(frontend.get_virtq_base(&virtq_base).unwrap().val, 0);
    assert!(!stopped.load(Ordering::Acquire));

    drop(frontend);
    backend_thread.join().unwrap();
    assert_eq!(starts.load(Ordering::Acquire), 2);
}

#[test]
fn test_vu_backend_needs_reset_after_worker_failure() {
    // A fake worker that fails as soon as it starts.
    let (frontend, backend_thread) = spawn_backend(move |event_rx, report_tx| {
        for event in event_rx.iter() {
            let report = match event {
                WakeEvent::Start { .. } => WorkerReport::Failed,
                WakeEvent::Reset { seq } => WorkerReport::ResetDone { seq },
                WakeEvent::Pause { seq } => WorkerReport::Paused { seq },
                event => panic!("unexpected event: {event:?}"),
            };
            report_tx.send(report).unwrap();
        }
    });

    start(&frontend);
    let needs_reset = driver_ok() | DevStatus::NEEDS_RESET.bits() as u64;
    let deadline = Instant::now() + Duration::from_secs(5);
    while frontend.get_status().unwrap() != needs_reset {
        assert!(Instant::now() < deadline, "no DEVICE_NEEDS_RESET");
        thread::sleep(Duration::from_millis(1));
    }

    // Resetting the device brings it back into service.
    frontend.set_status(&0).unwrap();
    assert_eq!(frontend.get_status().unwrap(), 0);

    // The frontend also learns about a failure after it has stopped the
    // ring.
    start(&frontend);
    stop_ring(&frontend, 0);
    assert_eq!(frontend.get_status().unwrap(), needs_reset);
    frontend.set_status(&0).unwrap();
    assert_eq!(frontend.get_status().unwrap(), 0);

    drop(frontend);
    backend_thread.join().unwrap();
}

#[test]
fn test_vu_backend_changes_memory_while_running() {
    let memory = Arc::new(RamBus::new());
    let mut poll = Poll::new().unwrap();
    let mut notifier = Notifier::new().unwrap();
    let registry = poll.registry();
    registry
        .register(&mut notifier, Token(0), Interest::READABLE)
        .unwrap();
    // A fake worker that does nothing but complete resets and pauses.
    let worker = |event_rx: TestWakeReceiver, report_tx: Sender<WorkerReport>| {
        for event in event_rx.iter() {
            match event {
                WakeEvent::Start { .. } => {}
                WakeEvent::Reset { seq } => {
                    report_tx.send(WorkerReport::ResetDone { seq }).unwrap()
                }
                WakeEvent::Pause { seq } => report_tx.send(WorkerReport::Paused { seq }).unwrap(),
                event => panic!("unexpected event: {event:?}"),
            }
        }
    };
    let (frontend, backend_thread) = spawn_backend_on(memory.clone(), Arc::new(notifier), worker);
    start(&frontend);

    let gpa = 0x2_0000;
    let file = tempfile::tempfile().unwrap();
    file.set_len(0x1000).unwrap();
    let region = MemorySingleRegion {
        _padding: 0,
        region: MemoryRegion {
            gpa,
            size: 0x1000,
            hva: 0x5_0000,
            mmap_offset: 0,
        },
    };
    // The backend changes the layout while the worker runs, and kicks the
    // worker after each change.
    let mut events = Events::with_capacity(1);
    frontend.add_mem_region(&region, file.as_fd()).unwrap();
    assert!(memory.load().translate(gpa).is_ok());
    poll.poll(&mut events, Some(Duration::from_secs(5)))
        .unwrap();
    assert!(!events.is_empty());

    frontend.remove_mem_region(&region).unwrap();
    assert!(memory.load().translate(gpa).is_err());
    poll.poll(&mut events, Some(Duration::from_secs(5)))
        .unwrap();
    assert!(!events.is_empty());

    // The backend also kicks a paused worker, which then moves to the
    // latest layout once it resumes. The request to pause woke the worker
    // up as well.
    stop_ring(&frontend, 0);
    poll.poll(&mut events, Some(Duration::from_secs(5)))
        .unwrap();
    assert!(!events.is_empty());
    frontend.add_mem_region(&region, file.as_fd()).unwrap();
    poll.poll(&mut events, Some(Duration::from_secs(5)))
        .unwrap();
    assert!(!events.is_empty());

    drop(frontend);
    backend_thread.join().unwrap();
}

/// What a [`RingWorker`] did.
#[derive(Debug, PartialEq, Eq)]
enum RingEvent {
    /// The worker started the device at these vring bases.
    Start(Option<Arc<[u32]>>),
    /// The worker reset the device.
    Reset,
    /// The worker was asked to pause the device.
    Pause,
    /// The worker was asked to resume the device.
    Resume,
}

/// A fake worker that tells where it stands in queue 0. Whenever it serves
/// the queue, it takes [`RingWorker::CHAINS`] chains from it right away.
struct RingWorker {
    queue_regs: Arc<[QueueReg]>,
    event_tx: Sender<RingEvent>,
    /// Whether the device can pause.
    can_pause: bool,
}

impl RingWorker {
    const CHAINS: u32 = 5;

    fn run(self, event_rx: TestWakeReceiver, report_tx: Sender<WorkerReport>) {
        let reg = &self.queue_regs[0];
        let mut base = 0;
        let mut paused = false;
        for event in event_rx.iter() {
            let event = match event {
                WakeEvent::Start { param } => {
                    let start = param.bases.as_ref().map_or(0, |bases| bases[0]);
                    base = start + Self::CHAINS;
                    RingEvent::Start(param.bases)
                }
                WakeEvent::Reset { seq } => {
                    // Like a real worker, tell where the device stopped.
                    reg.base.store(base, Ordering::Release);
                    paused = false;
                    report_tx.send(WorkerReport::ResetDone { seq }).unwrap();
                    RingEvent::Reset
                }
                WakeEvent::Pause { seq } if self.can_pause => {
                    reg.base.store(base, Ordering::Release);
                    paused = true;
                    report_tx.send(WorkerReport::Paused { seq }).unwrap();
                    RingEvent::Pause
                }
                WakeEvent::Pause { seq } => {
                    let report = WorkerReport::PauseFailed { seq };
                    report_tx.send(report).unwrap();
                    RingEvent::Pause
                }
                // A worker that has not paused keeps serving the queue.
                WakeEvent::Resume { .. } => {
                    if paused {
                        base += Self::CHAINS;
                    }
                    paused = false;
                    RingEvent::Resume
                }
                event => panic!("unexpected event: {event:?}"),
            };
            self.event_tx.send(event).unwrap();
        }
    }
}

/// Serves a device with `num_queues` queues, whose worker is a
/// [`RingWorker`]. Returns what [`spawn_backend`] returns, and what the
/// worker does.
fn spawn_ring_backend(num_queues: usize) -> (VuSession, JoinHandle<()>, Receiver<RingEvent>) {
    spawn_ring_backend_with(num_queues, true)
}

/// Like [`spawn_ring_backend`], with a device that can pause or not.
fn spawn_ring_backend_with(
    num_queues: usize,
    can_pause: bool,
) -> (VuSession, JoinHandle<()>, Receiver<RingEvent>) {
    let queue_regs = (0..num_queues).map(|_| QueueReg::default());
    let queue_regs = queue_regs.collect::<Arc<[_]>>();
    let (event_tx, event_rx) = flume::unbounded();
    let worker = RingWorker {
        queue_regs: queue_regs.clone(),
        event_tx,
        can_pause,
    };
    let notifier = Arc::new(Notifier::new().unwrap());
    let (frontend, backend_thread) = spawn_backend_with(
        queue_regs,
        Arc::default(),
        notifier,
        |event_rx, report_tx| worker.run(event_rx, report_tx),
    );
    (frontend, backend_thread, event_rx)
}

/// Enables queue 0, with its rings in guest memory that the backend maps.
fn enable_queue(frontend: &VuSession) {
    let (hva, size) = (0x5_0000, 0x3000);
    let file = tempfile::tempfile().unwrap();
    file.set_len(size).unwrap();
    let region = MemorySingleRegion {
        _padding: 0,
        region: MemoryRegion {
            gpa: 0x2_0000,
            size,
            hva,
            mmap_offset: 0,
        },
    };
    frontend.add_mem_region(&region, file.as_fd()).unwrap();
    let virtq_num = VirtqState { index: 0, val: 16 };
    frontend.set_virtq_num(&virtq_num).unwrap();
    let virtq_addr = VirtqAddr {
        index: 0,
        flags: 0,
        desc_hva: hva,
        used_hva: hva + 0x1000,
        avail_hva: hva + 0x2000,
        log_guest_addr: 0,
    };
    frontend.set_virtq_addr(&virtq_addr).unwrap();
    let virtq_enable = VirtqState { index: 0, val: 1 };
    frontend.set_virtq_enable(&virtq_enable).unwrap();
}

/// Stops ring `index`, and returns where the backend stopped it.
fn stop_ring(frontend: &VuSession, index: u32) -> u32 {
    let virtq_base = VirtqState { index, val: 0 };
    frontend.get_virtq_base(&virtq_base).unwrap().val
}

fn set_base(frontend: &VuSession, index: u32, base: u32) {
    let virtq_base = VirtqState { index, val: base };
    frontend.set_virtq_base(&virtq_base).unwrap();
}

fn start_bases(bases: &[u32]) -> RingEvent {
    RingEvent::Start(Some(Arc::from(bases)))
}

#[test]
fn test_vu_backend_keeps_ring_position() {
    let (frontend, backend_thread, events) = spawn_ring_backend(1);
    enable_queue(&frontend);

    // The worker starts at the base that the frontend sets.
    set_base(&frontend, 0, 7);
    start(&frontend);
    assert_eq!(events.recv_timeout(TIMEOUT), Ok(start_bases(&[7])));

    // The frontend learns where the worker stopped the ring, and the ring
    // stays there.
    let mut stopped_at = 7 + RingWorker::CHAINS;
    for _ in 0..2 {
        assert_eq!(stop_ring(&frontend, 0), stopped_at);
    }
    assert_eq!(events.recv_timeout(TIMEOUT), Ok(RingEvent::Pause));

    // The worker continues there, even if the frontend does not set the
    // base again.
    start(&frontend);
    assert_eq!(events.recv_timeout(TIMEOUT), Ok(RingEvent::Resume));
    stopped_at += RingWorker::CHAINS;
    assert_eq!(stop_ring(&frontend, 0), stopped_at);
    assert_eq!(events.recv_timeout(TIMEOUT), Ok(RingEvent::Pause));

    // So it does if the frontend sets the base that it got.
    set_base(&frontend, 0, stopped_at);
    start(&frontend);
    assert_eq!(events.recv_timeout(TIMEOUT), Ok(RingEvent::Resume));
    stopped_at += RingWorker::CHAINS;
    assert_eq!(stop_ring(&frontend, 0), stopped_at);
    assert_eq!(events.recv_timeout(TIMEOUT), Ok(RingEvent::Pause));

    // A base that the frontend sets replaces the one of the worker. A
    // paused worker cannot resume elsewhere, so it starts the device again.
    set_base(&frontend, 0, 3);
    assert_eq!(events.recv_timeout(TIMEOUT), Ok(RingEvent::Reset));
    assert_eq!(stop_ring(&frontend, 0), 3);
    start(&frontend);
    assert_eq!(events.recv_timeout(TIMEOUT), Ok(start_bases(&[3])));

    drop(frontend);
    backend_thread.join().unwrap();
    assert_eq!(events.try_iter().count(), 0);
}

#[test]
fn test_vu_backend_pauses_worker_while_ring_stopped() {
    let (frontend, backend_thread, events) = spawn_ring_backend(2);
    enable_queue(&frontend);
    start_ring(&frontend, 0);
    start_ring(&frontend, 1);
    frontend.set_status(&driver_ok()).unwrap();
    assert_eq!(events.recv_timeout(TIMEOUT), Ok(start_bases(&[0, 0])));

    // One worker serves both rings, so it pauses with the first ring that
    // the frontend stops.
    let mut stopped_at = RingWorker::CHAINS;
    assert_eq!(stop_ring(&frontend, 1), 0);
    assert_eq!(events.recv_timeout(TIMEOUT), Ok(RingEvent::Pause));
    assert_eq!(stop_ring(&frontend, 0), stopped_at);

    // It stays paused until the frontend has started both rings again, at
    // the bases that it got.
    start_ring(&frontend, 0);
    assert_eq!(stop_ring(&frontend, 0), stopped_at);
    set_base(&frontend, 0, stopped_at);
    set_base(&frontend, 1, 0);
    start_ring(&frontend, 0);
    start_ring(&frontend, 1);
    assert_eq!(events.recv_timeout(TIMEOUT), Ok(RingEvent::Resume));

    // It also pauses while only one ring is stopped.
    stopped_at += RingWorker::CHAINS;
    assert_eq!(stop_ring(&frontend, 0), stopped_at);
    assert_eq!(events.recv_timeout(TIMEOUT), Ok(RingEvent::Pause));
    start_ring(&frontend, 0);
    assert_eq!(events.recv_timeout(TIMEOUT), Ok(RingEvent::Resume));

    drop(frontend);
    backend_thread.join().unwrap();
    assert_eq!(events.try_iter().count(), 0);
}

#[test]
fn test_vu_backend_starts_worker_with_last_ring() {
    let (frontend, backend_thread, events) = spawn_ring_backend(2);
    // The driver is ready before the frontend has started the rings.
    frontend.set_status(&driver_ok()).unwrap();
    start_ring(&frontend, 0);
    start_ring(&frontend, 1);
    assert_eq!(events.recv_timeout(TIMEOUT), Ok(start_bases(&[0, 0])));

    drop(frontend);
    backend_thread.join().unwrap();
    assert_eq!(events.try_iter().count(), 0);
}

#[test]
fn test_vu_backend_resets_device_that_cannot_pause() {
    let (frontend, backend_thread, events) = spawn_ring_backend_with(1, false);
    enable_queue(&frontend);
    start(&frontend);
    assert_eq!(events.recv_timeout(TIMEOUT), Ok(start_bases(&[0])));

    // The ring stops nevertheless: the backend calls off the pause, and
    // has the device reset.
    let stopped_at = RingWorker::CHAINS;
    assert_eq!(stop_ring(&frontend, 0), stopped_at);
    for event in [RingEvent::Pause, RingEvent::Resume, RingEvent::Reset] {
        assert_eq!(events.recv_timeout(TIMEOUT), Ok(event));
    }

    // The worker starts the device again where it stopped.
    start_ring(&frontend, 0);
    assert_eq!(events.recv_timeout(TIMEOUT), Ok(start_bases(&[stopped_at])));

    drop(frontend);
    backend_thread.join().unwrap();
    assert_eq!(events.try_iter().count(), 0);
}

#[test]
fn test_vu_backend_forgets_ring_position_on_reset() {
    let (frontend, backend_thread, events) = spawn_ring_backend(1);
    enable_queue(&frontend);
    set_base(&frontend, 0, 7);
    // The driver is on its way to DRIVER_OK, which is not a reset.
    let features_ok = DevStatus::ACK | DevStatus::DRIVER | DevStatus::FEATURES_OK;
    frontend.set_status(&(features_ok.bits() as u64)).unwrap();
    start(&frontend);
    assert_eq!(events.recv_timeout(TIMEOUT), Ok(start_bases(&[7])));

    // The driver resets the device, and sets up the rings again from their
    // beginning.
    frontend.set_status(&0).unwrap();
    assert_eq!(events.recv_timeout(TIMEOUT), Ok(RingEvent::Reset));
    assert_eq!(stop_ring(&frontend, 0), 0);
    start(&frontend);
    assert_eq!(events.recv_timeout(TIMEOUT), Ok(start_bases(&[0])));

    drop(frontend);
    backend_thread.join().unwrap();
}

#[test]
fn test_vu_backend_keeps_base_of_disabled_queue() {
    let (frontend, backend_thread, events) = spawn_ring_backend(1);
    // The worker does not serve a disabled queue, so the ring stays at the
    // base that the frontend set.
    set_base(&frontend, 0, 7);
    start(&frontend);
    assert_eq!(events.recv_timeout(TIMEOUT), Ok(start_bases(&[7])));
    assert_eq!(stop_ring(&frontend, 0), 7);

    drop(frontend);
    backend_thread.join().unwrap();
}

#[rstest]
#[case(0, SplitQueue::INIT_VHOST_BASE)]
#[case(VirtioFeature::RING_PACKED.bits(), PackedQueue::INIT_VHOST_BASE)]
fn test_vu_backend_ring_starts_at_beginning(#[case] feature: u128, #[case] base: u32) {
    let (frontend, backend_thread, events) = spawn_ring_backend(1);
    frontend.set_features(&(feature as u64)).unwrap();

    // The frontend has not set a base.
    assert_eq!(stop_ring(&frontend, 0), base);
    start(&frontend);
    assert_eq!(events.recv_timeout(TIMEOUT), Ok(start_bases(&[base])));

    drop(frontend);
    backend_thread.join().unwrap();
}

#[test]
fn test_vu_backend_call_fd() {
    // A fake worker of a device with two queues, which hands out the
    // interrupt sender that it starts with.
    let (irq_tx, irq_rx) = flume::unbounded();
    let worker = move |event_rx: TestWakeReceiver, report_tx: Sender<WorkerReport>| {
        for event in event_rx.iter() {
            match event {
                WakeEvent::Start { param } => irq_tx.send(param.irq_sender).unwrap(),
                WakeEvent::Reset { seq } => {
                    report_tx.send(WorkerReport::ResetDone { seq }).unwrap()
                }
                event => panic!("unexpected event: {event:?}"),
            }
        }
    };
    let queue_regs = Arc::new([QueueReg::default(), QueueReg::default()]);
    let notifier = Arc::new(Notifier::new().unwrap());
    let (frontend, backend_thread) =
        spawn_backend_with(queue_regs, Arc::default(), notifier, worker);
    // Starts both rings, with a call fd for each.
    let start = || {
        for index in 0..2 {
            let kick = Notifier::new().unwrap();
            frontend.set_virtq_kick(&index, kick.as_fd()).unwrap();
        }
        frontend.set_status(&driver_ok()).unwrap();
    };

    let calls = [Notifier::new().unwrap(), Notifier::new().unwrap()];
    for (index, call) in calls.iter().enumerate() {
        frontend
            .set_virtq_call(&(index as u64), call.as_fd())
            .unwrap();
    }
    start();
    let irq_sender = irq_rx.recv_timeout(TIMEOUT).unwrap();
    irq_sender.queue_irq(1);
    assert_eq!(calls.each_ref().map(take_signals), [0, 1]);

    // The frontend replaces the call fd of ring 1 while the worker serves
    // the ring, e.g. when the driver masks the interrupt. The worker
    // signals the new one right away.
    let masked = Notifier::new().unwrap();
    frontend.set_virtq_call(&1, masked.as_fd()).unwrap();
    irq_sender.queue_irq(1);
    assert_eq!(take_signals(&masked), 1);
    irq_sender.queue_irq(0);
    assert_eq!(calls.each_ref().map(take_signals), [1, 0]);

    // The call fds stay when the worker starts again.
    frontend.set_status(&0).unwrap();
    start();
    let irq_sender = irq_rx.recv_timeout(TIMEOUT).unwrap();
    for index in 0..2 {
        irq_sender.queue_irq(index);
    }
    assert_eq!(take_signals(&masked), 1);
    assert_eq!(calls.each_ref().map(take_signals), [1, 0]);

    drop(frontend);
    backend_thread.join().unwrap();
}
