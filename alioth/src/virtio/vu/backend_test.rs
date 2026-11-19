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
use crate::virtio::{DevStatus, DeviceId, VirtioFeature};

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

/// Starts the device. The backend takes the kick fd for each start.
fn start(frontend: &VuSession) {
    let kick = Notifier::new().unwrap();
    frontend.set_virtq_kick(&0, kick.as_fd()).unwrap();
    frontend.set_status(&driver_ok()).unwrap();
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
                event => panic!("unexpected event: {event:?}"),
            }
        }
    });

    // The reply to SET_STATUS(0) means that the worker has stopped.
    start(&frontend);
    frontend.set_status(&0).unwrap();
    assert!(stopped.swap(false, Ordering::AcqRel));

    // So does the reply to GET_VRING_BASE.
    start(&frontend);
    let virtq_base = VirtqState { index: 0, val: 0 };
    assert_eq!(frontend.get_virtq_base(&virtq_base).unwrap().val, 0);
    assert!(stopped.swap(false, Ordering::AcqRel));

    // The worker has already stopped, so there is nothing left to stop.
    frontend.set_status(&0).unwrap();
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
    // A fake worker that does nothing but complete resets.
    let worker = |event_rx: TestWakeReceiver, report_tx: Sender<WorkerReport>| {
        for event in event_rx.iter() {
            match event {
                WakeEvent::Start { .. } => {}
                WakeEvent::Reset { seq } => {
                    report_tx.send(WorkerReport::ResetDone { seq }).unwrap()
                }
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
}

/// A fake worker that tells where it stands in queue 0. Whenever it serves
/// the queue, it takes [`RingWorker::CHAINS`] chains from it right away.
struct RingWorker {
    queue_regs: Arc<[QueueReg]>,
    event_tx: Sender<RingEvent>,
}

impl RingWorker {
    const CHAINS: u32 = 5;

    fn run(self, event_rx: TestWakeReceiver, report_tx: Sender<WorkerReport>) {
        let reg = &self.queue_regs[0];
        let mut base = 0;
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
                    report_tx.send(WorkerReport::ResetDone { seq }).unwrap();
                    RingEvent::Reset
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
    let queue_regs = (0..num_queues).map(|_| QueueReg::default());
    let queue_regs = queue_regs.collect::<Arc<[_]>>();
    let (event_tx, event_rx) = flume::unbounded();
    let worker = RingWorker {
        queue_regs: queue_regs.clone(),
        event_tx,
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
    let stopped_at = 7 + RingWorker::CHAINS;
    for _ in 0..2 {
        assert_eq!(stop_ring(&frontend, 0), stopped_at);
    }
    assert_eq!(events.recv_timeout(TIMEOUT), Ok(RingEvent::Reset));

    // The worker continues there, even if the frontend does not set the
    // base again.
    start(&frontend);
    assert_eq!(events.recv_timeout(TIMEOUT), Ok(start_bases(&[stopped_at])));
    assert_eq!(stop_ring(&frontend, 0), stopped_at + RingWorker::CHAINS);
    assert_eq!(events.recv_timeout(TIMEOUT), Ok(RingEvent::Reset));

    // A base that the frontend sets replaces the one of the worker.
    set_base(&frontend, 0, 3);
    assert_eq!(stop_ring(&frontend, 0), 3);
    start(&frontend);
    assert_eq!(events.recv_timeout(TIMEOUT), Ok(start_bases(&[3])));

    drop(frontend);
    backend_thread.join().unwrap();
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
