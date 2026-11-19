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

use assert_matches::assert_matches;
use flume::{Receiver, Sender};
use mio::{Events, Interest, Poll, Token};
use rstest::rstest;

use crate::mem;
use crate::mem::emulated::{Action, Mmio};
use crate::mem::mapped::{ArcMemPages, RamBus};
use crate::sync::notifier::Notifier;
use crate::virtio::dev::entropy::EntropySpec;
use crate::virtio::dev::{DevSpec, VirtioDevice, WakeEvent, WorkerCtl, WorkerReport};
use crate::virtio::queue::packed::PackedQueue;
use crate::virtio::queue::split::SplitQueue;
use crate::virtio::queue::tests::{GuestQueue, UsedDesc};
use crate::virtio::queue::{QueueReg, VirtQueue};
use crate::virtio::tests::{DATA_ADDR, MEM_SIZE, QUEUE_SIZE, fixture_queues};
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

/// Starts ring `index`, and returns its kick fd.
fn start_ring(frontend: &VuSession, index: u64) -> Notifier {
    let kick = Notifier::new().unwrap();
    frontend.set_virtq_kick(&index, kick.as_fd()).unwrap();
    kick
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

/// Waits until `eventfd` is signalled.
fn wait_signal(eventfd: &Notifier) {
    let deadline = Instant::now() + TIMEOUT;
    while take_signals(eventfd) == 0 {
        assert!(Instant::now() < deadline, "eventfd not signalled");
        thread::sleep(Duration::from_millis(1));
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

/// What the fake worker of [`spawn_kick_backend`] was asked to do.
#[derive(Debug)]
enum KickEvent {
    /// To start the device with these queue notifiers.
    Start(Option<Arc<[Notifier]>>),
    Pause,
    /// To resume the device with these queue notifiers.
    Resume(Option<Arc<[Notifier]>>),
    Reset,
}

/// Serves a device with `num_queues` queues, whose fake worker tells what
/// it is asked to do.
fn spawn_kick_backend(num_queues: usize) -> (VuSession, JoinHandle<()>, Receiver<KickEvent>) {
    let queue_regs = (0..num_queues).map(|_| QueueReg::default());
    let (event_tx, events) = flume::unbounded();
    let worker = move |event_rx: TestWakeReceiver, report_tx: Sender<WorkerReport>| {
        for event in event_rx.iter() {
            let event = match event {
                WakeEvent::Start { param } => KickEvent::Start(param.notifiers),
                WakeEvent::Pause { seq } => {
                    report_tx.send(WorkerReport::Paused { seq }).unwrap();
                    KickEvent::Pause
                }
                WakeEvent::Resume { notifiers } => KickEvent::Resume(notifiers),
                WakeEvent::Reset { seq } => {
                    report_tx.send(WorkerReport::ResetDone { seq }).unwrap();
                    KickEvent::Reset
                }
                event => panic!("unexpected event: {event:?}"),
            };
            event_tx.send(event).unwrap();
        }
    };
    let notifier = Arc::new(Notifier::new().unwrap());
    let (frontend, backend_thread) =
        spawn_backend_with(queue_regs.collect(), Arc::default(), notifier, worker);
    (frontend, backend_thread, events)
}

/// Returns how often each of `notifiers` was signalled since the last
/// call.
fn take_all_signals(notifiers: &[Notifier]) -> Vec<u64> {
    notifiers.iter().map(take_signals).collect()
}

#[test]
fn test_vu_backend_hands_kick_fds_to_worker() {
    let (frontend, backend_thread, events) = spawn_kick_backend(2);
    let kick_0 = start_ring(&frontend, 0);
    let kick_1 = start_ring(&frontend, 1);
    frontend.set_status(&driver_ok()).unwrap();

    // The worker starts with notifiers for the kick fds of the rings.
    let event = events.recv_timeout(TIMEOUT);
    let Ok(KickEvent::Start(Some(notifiers))) = event else {
        panic!("unexpected event: {event:?}");
    };
    kick_1.notify().unwrap();
    assert_eq!(take_all_signals(&notifiers), [0, 1]);

    // The frontend stops ring 0, and starts it again with another kick fd.
    // The worker resumes with a notifier for that one, and with a notifier
    // for the kick fd that ring 1 still has.
    stop_ring(&frontend, 0);
    assert_matches!(events.recv_timeout(TIMEOUT), Ok(KickEvent::Pause));
    let new_kick_0 = start_ring(&frontend, 0);
    let event = events.recv_timeout(TIMEOUT);
    let Ok(KickEvent::Resume(Some(notifiers))) = event else {
        panic!("unexpected event: {event:?}");
    };
    kick_0.notify().unwrap();
    assert_eq!(take_all_signals(&notifiers), [0, 0]);
    new_kick_0.notify().unwrap();
    kick_1.notify().unwrap();
    assert_eq!(take_all_signals(&notifiers), [1, 1]);

    // A ring that is started takes another kick fd as well. A worker takes
    // new notifiers only when it resumes, so it pauses first.
    let new_kick_1 = start_ring(&frontend, 1);
    assert_matches!(events.recv_timeout(TIMEOUT), Ok(KickEvent::Pause));
    let event = events.recv_timeout(TIMEOUT);
    let Ok(KickEvent::Resume(Some(notifiers))) = event else {
        panic!("unexpected event: {event:?}");
    };
    kick_1.notify().unwrap();
    assert_eq!(take_all_signals(&notifiers), [0, 0]);
    new_kick_0.notify().unwrap();
    new_kick_1.notify().unwrap();
    assert_eq!(take_all_signals(&notifiers), [1, 1]);

    // The rings keep their kick fds when the driver resets the device.
    frontend.set_status(&0).unwrap();
    assert_matches!(events.recv_timeout(TIMEOUT), Ok(KickEvent::Reset));
    frontend.set_status(&driver_ok()).unwrap();
    let event = events.recv_timeout(TIMEOUT);
    let Ok(KickEvent::Start(Some(notifiers))) = event else {
        panic!("unexpected event: {event:?}");
    };
    new_kick_1.notify().unwrap();
    assert_eq!(take_all_signals(&notifiers), [0, 1]);

    drop(frontend);
    backend_thread.join().unwrap();
    assert_eq!(events.try_iter().count(), 0);
}

/// Makes a buffer available to the device, and returns its ID.
fn add_buffer(guest_q: &mut GuestQueue<SplitQueue>) -> u16 {
    guest_q.add_desc(&[], &[(DATA_ADDR, 64)])
}

/// Checks that the device has filled buffer `id`, and no other one.
fn check_filled(guest_q: &mut GuestQueue<SplitQueue>, id: u16) {
    assert_matches!(guest_q.get_used(), Some(UsedDesc { id: i, len: 64, .. }) if i == id);
    assert_matches!(guest_q.get_used(), None);
}

#[test]
fn test_vu_backend_serves_ring_with_new_fds() {
    // An entropy device with its worker behind the backend.
    let memory = Arc::new(RamBus::new());
    let dev = EntropySpec { source: None }.build("entropy").unwrap();
    let dev = VirtioDevice::new("entropy", dev, memory.clone(), false).unwrap();
    let (frontend_conn, backend_conn) = UnixStream::pair().unwrap();
    let mut backend = VuBackend::new(backend_conn, dev, memory).unwrap();
    let backend_thread = thread::spawn(move || backend.run().unwrap());
    let frontend = VuSession::from(frontend_conn);

    // The guest memory, which the frontend shares with the backend, and
    // the queue of the driver in it.
    let file = tempfile::tempfile().unwrap();
    file.set_len(MEM_SIZE as u64).unwrap();
    let hva = 0x7f00_0000_0000;
    let region = MemorySingleRegion {
        _padding: 0,
        region: MemoryRegion {
            gpa: 0,
            size: MEM_SIZE as u64,
            hva,
            mmap_offset: 0,
        },
    };
    frontend.add_mem_region(&region, file.as_fd()).unwrap();
    let prot = libc::PROT_READ | libc::PROT_WRITE;
    let pages = ArcMemPages::from_file(file, 0, MEM_SIZE, prot).unwrap();
    let ram_bus = RamBus::new();
    ram_bus.update(|ram| ram.add(0, pages)).unwrap();
    let ram = ram_bus.load();
    let queue_regs = fixture_queues(1);
    let reg = &queue_regs[0];
    let guest_q = SplitQueue::new(reg, &ram, false).unwrap().unwrap();
    let mut guest_q = GuestQueue::new(guest_q, reg);

    let virtq_num = VirtqState {
        index: 0,
        val: QUEUE_SIZE as u32,
    };
    frontend.set_virtq_num(&virtq_num).unwrap();
    let virtq_addr = VirtqAddr {
        index: 0,
        flags: 0,
        desc_hva: hva + reg.desc.load(Ordering::Acquire),
        used_hva: hva + reg.device.load(Ordering::Acquire),
        avail_hva: hva + reg.driver.load(Ordering::Acquire),
        log_guest_addr: 0,
    };
    frontend.set_virtq_addr(&virtq_addr).unwrap();
    let virtq_enable = VirtqState { index: 0, val: 1 };
    frontend.set_virtq_enable(&virtq_enable).unwrap();
    let call = Notifier::new().unwrap();
    frontend.set_virtq_call(&0, call.as_fd()).unwrap();
    let kick = start_ring(&frontend, 0);
    frontend.set_status(&driver_ok()).unwrap();

    // The device fills a buffer of the driver, and signals the call fd.
    let id = add_buffer(&mut guest_q);
    kick.notify().unwrap();
    wait_signal(&call);
    check_filled(&mut guest_q, id);

    // The frontend stops the ring, and starts it again with other
    // eventfds. The driver made a buffer available meanwhile. The device
    // fills it without a notification, and signals the new call fd.
    assert_eq!(stop_ring(&frontend, 0), 1);
    let id = add_buffer(&mut guest_q);
    let new_call = Notifier::new().unwrap();
    frontend.set_virtq_call(&0, new_call.as_fd()).unwrap();
    let new_kick = start_ring(&frontend, 0);
    wait_signal(&new_call);
    check_filled(&mut guest_q, id);
    assert_eq!(take_signals(&call), 0);

    // The device waits on the new kick fd only.
    let id = add_buffer(&mut guest_q);
    kick.notify().unwrap();
    thread::sleep(Duration::from_millis(50));
    assert_matches!(guest_q.get_used(), None);
    new_kick.notify().unwrap();
    wait_signal(&new_call);
    check_filled(&mut guest_q, id);

    // The frontend replaces both fds of the ring without stopping it.
    let id = add_buffer(&mut guest_q);
    let newer_call = Notifier::new().unwrap();
    frontend.set_virtq_call(&0, newer_call.as_fd()).unwrap();
    let newer_kick = start_ring(&frontend, 0);
    wait_signal(&newer_call);
    check_filled(&mut guest_q, id);
    assert_eq!(take_signals(&new_call), 0);
    let id = add_buffer(&mut guest_q);
    new_kick.notify().unwrap();
    thread::sleep(Duration::from_millis(50));
    assert_matches!(guest_q.get_used(), None);
    newer_kick.notify().unwrap();
    wait_signal(&newer_call);
    check_filled(&mut guest_q, id);

    drop(frontend);
    backend_thread.join().unwrap();
}
