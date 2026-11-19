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

use std::os::fd::BorrowedFd;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use assert_matches::assert_matches;
use flume::{Receiver, Sender};
use mio::event::Event;
use mio::{Events, Interest, Poll, Registry, Token};
use parking_lot::Mutex;
use rstest::rstest;

use crate::mem::mapped::{ArcMemPages, RamBus};
use crate::mem::{MemRegion, MemRegionType, Memory};
use crate::sync::notifier::Notifier;
use crate::virtio::dev::entropy::{EntropyConfig, EntropyFeature};
use crate::virtio::dev::{
    ActiveBackend, BackendEvent, Context, KickWorker, Register, StartParam, Virtio, WakeEvent,
    WorkerCtl, WorkerParam, WorkerReport, WorkerState,
};
use crate::virtio::queue::packed::PackedQueue;
use crate::virtio::queue::split::SplitQueue;
use crate::virtio::queue::tests::{GuestQueue, VirtQueueGuest};
use crate::virtio::queue::{Queue, QueueReg, Status, VirtQueue};
use crate::virtio::tests::{DATA_ADDR, MEM_SIZE, fixture_queues, fixture_ram_bus};
use crate::virtio::worker::mio::{ActiveMio, Mio, VirtioMio};
use crate::virtio::{DeviceId, Error, IrqSender, Result, VirtioFeature, error};

const TIMEOUT: Duration = Duration::from_secs(5);

#[test]
fn test_virtio_register_get_driver_feature() {
    let reg = Register::default();
    assert_eq!(reg.get_driver_feature(), 0);

    reg.driver_feature[0].store(0x1234_5678, Ordering::Release);
    reg.driver_feature[1].store(0x9abc_def0, Ordering::Release);
    reg.driver_feature[2].store(0xfeed_cafe, Ordering::Release);
    reg.driver_feature[3].store(0x0123_4567, Ordering::Release);

    let expected = (0x1234_5678u128)
        | ((0x9abc_def0u128) << 32)
        | ((0xfeed_cafeu128) << 64)
        | ((0x0123_4567u128) << 96);
    assert_eq!(reg.get_driver_feature(), expected);
}

/// A device that completes the chains of its queue right away, and
/// records what the worker does to it.
///
/// It borrows the config and feature types of the entropy device, which
/// has neither.
#[derive(Debug, Default)]
struct FakeDev {
    /// The queue that something else serves, like vhost in the kernel.
    offloaded: Option<u16>,
    /// Makes activation fail, like a backend rejecting the driver setup.
    fail_activate: Arc<AtomicBool>,
    /// Counts successful activations.
    activations: Arc<AtomicU32>,
    /// Counts the chains that the device completed.
    chains: Arc<AtomicU32>,
    /// Counts the calls to [`VirtioMio::reset`].
    resets: Arc<AtomicU32>,
    /// Makes [`Virtio::pause`] fail, like a backend that cannot be stopped.
    fail_pause: Arc<AtomicBool>,
    /// Counts the successful calls to [`Virtio::pause`].
    pauses: Arc<AtomicU32>,
    /// Counts the calls to [`Virtio::resume`].
    resumes: Arc<AtomicU32>,
    /// The numbers of queues and notifiers of the last [`Virtio::resume`].
    resumed_with: Arc<Mutex<Option<(usize, usize)>>>,
}

impl Virtio for FakeDev {
    type Config = EntropyConfig;
    type Feature = EntropyFeature;

    fn name(&self) -> &str {
        "fake"
    }

    fn id(&self) -> DeviceId {
        DeviceId::ENTROPY
    }

    fn num_queues(&self) -> u16 {
        1
    }

    fn notifier_offloaded(&self, q_index: u16) -> Result<bool> {
        Ok(self.offloaded == Some(q_index))
    }

    fn config(&self) -> Arc<EntropyConfig> {
        Arc::new(EntropyConfig)
    }

    fn feature(&self) -> u128 {
        0
    }

    fn pause(&mut self) -> Result<()> {
        if self.fail_pause.load(Ordering::Acquire) {
            return error::PauseUnsupported.fail();
        }
        self.pauses.fetch_add(1, Ordering::AcqRel);
        Ok(())
    }

    fn resume<Q, S>(
        &mut self,
        queues: &[Option<Queue<Q>>],
        notifiers: &[Notifier],
        _: &S,
    ) -> Result<()>
    where
        Q: VirtQueue,
        S: IrqSender,
    {
        self.resumes.fetch_add(1, Ordering::AcqRel);
        *self.resumed_with.lock() = Some((queues.len(), notifiers.len()));
        Ok(())
    }

    fn spawn_worker<S>(self, param: WorkerParam<S>) -> Result<(JoinHandle<()>, Arc<Notifier>)>
    where
        S: IrqSender,
    {
        Mio::spawn_worker(self, param)
    }
}

impl VirtioMio for FakeDev {
    fn activate<Q, S>(
        &mut self,
        _feature: u128,
        _active_mio: &mut ActiveMio<'_, Q, S>,
    ) -> Result<()>
    where
        Q: VirtQueue,
        S: IrqSender,
    {
        if self.fail_activate.load(Ordering::Acquire) {
            return error::InvalidBuffer.fail();
        }
        self.activations.fetch_add(1, Ordering::AcqRel);
        Ok(())
    }

    fn handle_queue<Q, S>(&mut self, index: u16, active_mio: &mut ActiveMio<'_, Q, S>) -> Result<()>
    where
        Q: VirtQueue,
        S: IrqSender,
    {
        let Some(Some(queue)) = active_mio.queues.get_mut(index as usize) else {
            return Ok(());
        };
        queue.handle_desc(index, active_mio.irq_sender, |_| {
            self.chains.fetch_add(1, Ordering::AcqRel);
            Ok(Status::Done { len: 0 })
        })
    }

    fn handle_event<Q, S>(
        &mut self,
        _event: &Event,
        _active_mio: &mut ActiveMio<'_, Q, S>,
    ) -> Result<()>
    where
        Q: VirtQueue,
        S: IrqSender,
    {
        Ok(())
    }

    fn reset(&mut self, _registry: &Registry) {
        self.resets.fetch_add(1, Ordering::AcqRel);
    }
}

/// Counts configuration interrupts.
#[derive(Debug, Default)]
struct TestIrqSender {
    config_irqs: AtomicU32,
}

impl IrqSender for TestIrqSender {
    fn queue_irq(&self, _idx: u16) {}

    fn config_irq(&self) {
        self.config_irqs.fetch_add(1, Ordering::AcqRel);
    }

    fn queue_irqfd<F, T>(&self, _idx: u16, _f: F) -> Result<T>
    where
        F: FnOnce(BorrowedFd) -> Result<T>,
    {
        unimplemented!()
    }

    fn config_irqfd<F, T>(&self, _f: F) -> Result<T>
    where
        F: FnOnce(BorrowedFd) -> Result<T>,
    {
        unimplemented!()
    }
}

struct TestWorker {
    ctl: Arc<WorkerCtl<TestIrqSender>>,
    handle: JoinHandle<()>,
    /// The features that the driver accepts when it starts the device.
    feature: u128,
    irq_sender: Arc<TestIrqSender>,
    ram_bus: Arc<RamBus>,
    queue_regs: Arc<[QueueReg]>,
    fail_activate: Arc<AtomicBool>,
    activations: Arc<AtomicU32>,
    chains: Arc<AtomicU32>,
    resets: Arc<AtomicU32>,
    fail_pause: Arc<AtomicBool>,
    pauses: Arc<AtomicU32>,
    resumes: Arc<AtomicU32>,
}

impl TestWorker {
    fn spawn() -> Self {
        Self::spawn_on(Arc::new(fixture_ram_bus()))
    }

    /// Spawns a worker whose queue is in the fixture memory of `ram_bus`.
    fn spawn_on(ram_bus: Arc<RamBus>) -> Self {
        Self::spawn_dev(FakeDev::default(), ram_bus)
    }

    fn spawn_dev(dev: FakeDev, ram_bus: Arc<RamBus>) -> Self {
        let fail_activate = dev.fail_activate.clone();
        let activations = dev.activations.clone();
        let chains = dev.chains.clone();
        let resets = dev.resets.clone();
        let fail_pause = dev.fail_pause.clone();
        let pauses = dev.pauses.clone();
        let resumes = dev.resumes.clone();
        let (event_tx, event_rx) = flume::unbounded();
        let (report_tx, report_rx) = flume::unbounded();
        let queue_regs = Arc::<[QueueReg]>::from(fixture_queues(1));
        let param = WorkerParam {
            event_rx,
            report_tx,
            memory: ram_bus.clone(),
            queue_regs: queue_regs.clone(),
        };
        let (handle, notifier) = dev.spawn_worker(param).unwrap();
        TestWorker {
            ctl: Arc::new(WorkerCtl::new("fake".into(), event_tx, notifier, report_rx)),
            handle,
            feature: VirtioFeature::VERSION_1.bits(),
            irq_sender: Arc::default(),
            ram_bus,
            queue_regs,
            fail_activate,
            activations,
            chains,
            resets,
            fail_pause,
            pauses,
            resumes,
        }
    }

    fn start(&self) {
        self.start_param(None, None)
    }

    /// Starts the device with the queue notifiers of a transport.
    fn start_with(&self, notifiers: Option<Arc<[Notifier]>>) {
        self.start_param(notifiers, None)
    }

    /// Starts the device at the vring base `base` of its queue.
    fn start_at(&self, base: u32) {
        self.start_param(None, Some(Arc::new([base])))
    }

    fn start_param(&self, notifiers: Option<Arc<[Notifier]>>, bases: Option<Arc<[u32]>>) {
        let param = StartParam {
            feature: self.feature,
            irq_sender: self.irq_sender.clone(),
            notifiers,
            bases,
        };
        assert!(self.ctl.wake(WakeEvent::Start { param }));
    }

    /// Returns where the worker recorded that the device stands in the
    /// rings of its queue.
    fn base(&self) -> u32 {
        self.queue_regs[0].base.load(Ordering::Acquire)
    }

    /// Makes `count` chains available in `guest_q`, the queue of the
    /// device as the driver sees it, and notifies the device.
    fn add_chains<Q>(&self, guest_q: &mut GuestQueue<Q>, count: usize) -> Vec<u16>
    where
        Q: VirtQueueGuest,
    {
        let ids = (0..count).map(|_| guest_q.add_desc(&[(DATA_ADDR, 8)], &[]));
        let ids = ids.collect();
        assert!(self.ctl.wake(WakeEvent::Notify { q_index: 0 }));
        ids
    }

    fn reset(&self) {
        let seq = self.ctl.request_reset();
        assert_matches!(self.ctl.wait_reset(seq, TIMEOUT), Ok(()));
        assert!(self.ctl.poll_reset(seq));
    }

    fn resets(&self) -> u32 {
        self.resets.load(Ordering::Acquire)
    }

    fn pause(&self) {
        let seq = self.ctl.request_pause();
        assert_matches!(self.ctl.wait_pause(seq, TIMEOUT), Ok(()));
    }

    fn pauses(&self) -> u32 {
        self.pauses.load(Ordering::Acquire)
    }

    /// Resumes the device, and waits until it has resumed `count` times.
    fn resume(&self, count: u32) {
        assert!(self.ctl.wake(WakeEvent::Resume));
        let deadline = Instant::now() + TIMEOUT;
        while self.resumes.load(Ordering::Acquire) < count {
            assert!(Instant::now() < deadline, "device not resumed");
            thread::sleep(Duration::from_millis(1));
        }
    }

    fn config_irqs(&self) -> u32 {
        self.irq_sender.config_irqs.load(Ordering::Acquire)
    }

    /// Waits until the device has been activated `count` times.
    fn wait_activations(&self, count: u32) {
        let deadline = Instant::now() + TIMEOUT;
        while self.activations.load(Ordering::Acquire) < count {
            assert!(Instant::now() < deadline, "device not activated");
            thread::sleep(Duration::from_millis(1));
        }
    }

    /// Waits until the worker reports a failure.
    fn wait_failed(&self) {
        let deadline = Instant::now() + TIMEOUT;
        while !self.ctl.failed() {
            assert!(Instant::now() < deadline, "no failure reported");
            thread::sleep(Duration::from_millis(1));
        }
    }

    fn shutdown(self) {
        assert!(self.ctl.wake(WakeEvent::Shutdown));
        self.handle.join().unwrap();
    }
}

#[test]
fn test_worker_reset_before_start() {
    let worker = TestWorker::spawn();
    worker.reset();
    // The device has not started, so there was nothing to clean up.
    assert_eq!(worker.resets(), 0);
    worker.shutdown();
}

#[test]
fn test_worker_reset_after_start() {
    let worker = TestWorker::spawn();
    worker.start();
    worker.reset();
    // The worker acknowledges a reset only after cleaning up.
    assert_eq!(worker.resets(), 1);

    worker.start();
    worker.reset();
    assert_eq!(worker.resets(), 2);
    assert!(!worker.ctl.failed());
    assert_eq!(worker.config_irqs(), 0);
    worker.shutdown();
}

#[test]
fn test_notify_offloaded() {
    // Something else serves queue 1. The worker serves queue 0 itself.
    let dev = FakeDev {
        offloaded: Some(1),
        ..Default::default()
    };
    let mut observer = Poll::new().unwrap();
    let mut notifiers = [Notifier::new().unwrap(), Notifier::new().unwrap()];
    for (index, notifier) in notifiers.iter_mut().enumerate() {
        let registry = observer.registry();
        registry
            .register(notifier, Token(index), Interest::READABLE)
            .unwrap();
    }

    dev.notify_offloaded(&notifiers).unwrap();
    let mut events = Events::with_capacity(4);
    observer.poll(&mut events, Some(Duration::ZERO)).unwrap();
    let tokens = events.iter().map(|e| e.token()).collect::<Vec<_>>();
    assert_eq!(tokens, [Token(1)]);
}

#[test]
fn test_worker_notifies_offloaded_queues_on_start() {
    // Something else serves queue 1. It may have dropped what the driver
    // notified before the worker activated the device.
    let dev = FakeDev {
        offloaded: Some(1),
        ..Default::default()
    };
    let worker = TestWorker::spawn_dev(dev, Arc::new(fixture_ram_bus()));
    let mut observer = Poll::new().unwrap();
    let mut notifiers = [Notifier::new().unwrap(), Notifier::new().unwrap()];
    let registry = observer.registry();
    registry
        .register(&mut notifiers[1], Token(1), Interest::READABLE)
        .unwrap();

    worker.start_with(Some(Arc::from(notifiers)));
    worker.wait_activations(1);
    let mut events = Events::with_capacity(4);
    observer.poll(&mut events, Some(TIMEOUT)).unwrap();
    let tokens = events.iter().map(|e| e.token()).collect::<Vec<_>>();
    assert_eq!(tokens, [Token(1)]);

    worker.shutdown();
}

#[test]
fn test_worker_survives_activation_failure() {
    let worker = TestWorker::spawn();
    worker.fail_activate.store(true, Ordering::Release);
    worker.start();
    // The worker cleans up the partially activated device before reporting
    // the failure.
    worker.wait_failed();
    assert_eq!(worker.resets(), 1);

    // The worker stays alive to complete the reset, which brings the device
    // back into service.
    worker.reset();
    assert!(!worker.ctl.failed());
    // The worker has notified the driver of the failure.
    assert_eq!(worker.config_irqs(), 1);

    worker.fail_activate.store(false, Ordering::Release);
    worker.start();
    worker.reset();
    assert_eq!(worker.resets(), 2);
    assert!(!worker.ctl.failed());
    worker.shutdown();
}

#[test]
fn test_worker_fails_without_rings() {
    let worker = TestWorker::spawn();
    worker.start();
    worker.wait_activations(1);

    // The driver takes away the memory of the rings while the device runs.
    // Before the next event, the worker tries to move the queue to the new
    // layout, and fails.
    let pages = worker.ram_bus.update(|ram| ram.remove(0)).unwrap();
    assert!(worker.ctl.wake(WakeEvent::Notify { q_index: 0 }));
    worker.wait_failed();
    assert_eq!(worker.resets(), 1);

    // A reset brings the device back into service.
    worker.reset();
    assert!(!worker.ctl.failed());
    assert_eq!(worker.config_irqs(), 1);

    worker.ram_bus.update(|ram| ram.add(0, pages)).unwrap();
    worker.start();
    worker.wait_activations(2);
    worker.reset();
    assert_eq!(worker.resets(), 2);
    assert!(!worker.ctl.failed());
    worker.shutdown();
}

#[test]
fn test_worker_kicked_after_mem_update() {
    let memory = Memory::new();
    let add_ram = |gpa, size| {
        let pages = ArcMemPages::from_anonymous(size, None, None).unwrap();
        let region = MemRegion::with_ram(pages, MemRegionType::Ram);
        memory.add_region(gpa, Arc::new(region)).unwrap();
    };
    add_ram(0, MEM_SIZE);
    let worker = TestWorker::spawn_on(memory.ram_bus());
    let kick = KickWorker {
        ctl: Arc::downgrade(&worker.ctl),
    };
    memory.register_update_callback(Box::new(kick)).unwrap();
    worker.start();
    worker.wait_activations(1);

    // The worker is idle, but drops the layout it started with once memory
    // is plugged.
    let old_ram = Arc::downgrade(&memory.ram_bus().load());
    add_ram(4 << 20, 1 << 20);
    let deadline = Instant::now() + TIMEOUT;
    while old_ram.strong_count() > 0 {
        assert!(Instant::now() < deadline, "old layout still in use");
        thread::sleep(Duration::from_millis(1));
    }

    // Memory updates do not fail once the worker is gone.
    worker.shutdown();
    assert_matches!(memory.remove_region(4 << 20), Ok(_));
}

/// An event that does not come from the worker notifier.
struct DeviceEvent;

impl BackendEvent for DeviceEvent {
    fn token(&self) -> u64 {
        0
    }
}

/// Counts how often the worker takes the queues to move them to the
/// latest guest memory layout.
#[derive(Debug, Default)]
struct SyncCounter {
    syncs: u32,
}

impl ActiveBackend<FakeDev> for SyncCounter {
    type Event = DeviceEvent;
    type Q = SplitQueue;

    fn queues(&mut self) -> &mut [Option<Queue<SplitQueue>>] {
        self.syncs += 1;
        &mut []
    }

    fn handle_event(&mut self, _: &mut FakeDev, _: &DeviceEvent) -> Result<()> {
        Ok(())
    }

    fn handle_queue(&mut self, _: &mut FakeDev, _: u16) -> Result<()> {
        Ok(())
    }

    fn pause(&mut self, _: &mut FakeDev) -> Result<bool> {
        Ok(true)
    }

    fn resume(&mut self, _: &mut FakeDev) -> Result<()> {
        Ok(())
    }

    fn resume_dev(&mut self, _: &mut FakeDev) -> Result<()> {
        Ok(())
    }
}

#[test]
fn test_handle_event_syncs_ram_once_per_layout() {
    let (event_tx, event_rx) = flume::unbounded();
    let ram_bus = Arc::new(fixture_ram_bus());
    let mut context = Context {
        dev: FakeDev::default(),
        param: WorkerParam {
            event_rx,
            report_tx: flume::unbounded().0,
            memory: ram_bus.clone(),
            queue_regs: Arc::new([]),
        },
        state: WorkerState::Running,
        reset_seq: None,
        pause_seq: None,
        ram_generation: None,
    };
    let mut backend = SyncCounter::default();

    // The first event of an event loop syncs, later ones do not while the
    // layout stays the same.
    for _ in 0..2 {
        context.handle_event(&DeviceEvent, &mut backend).unwrap();
    }
    assert_eq!(backend.syncs, 1);

    // A layout change is picked up by the next event, and only by it.
    ram_bus.update(|ram| ram.remove(0)).unwrap();
    for _ in 0..2 {
        context.handle_event(&DeviceEvent, &mut backend).unwrap();
    }
    assert_eq!(backend.syncs, 2);

    // A new event loop starts with a sync, even if the layout is unchanged.
    let param = StartParam {
        feature: 0,
        irq_sender: Arc::new(TestIrqSender::default()),
        notifiers: None,
        bases: None,
    };
    event_tx.send(WakeEvent::Start { param }).unwrap();
    assert!(context.wait_start().is_some());
    context.handle_event(&DeviceEvent, &mut backend).unwrap();
    assert_eq!(backend.syncs, 3);
}

#[test]
fn test_worker_exit_completes_reset() {
    let worker = TestWorker::spawn();
    worker.start();
    let TestWorker { ctl, handle, .. } = worker;
    assert!(ctl.wake(WakeEvent::Shutdown));
    handle.join().unwrap();

    // An exited worker no longer uses the queues.
    let seq = ctl.request_reset();
    assert!(ctl.poll_reset(seq));
    assert_matches!(ctl.wait_reset(seq, TIMEOUT), Ok(()));
}

#[test]
fn test_wait_reset_timeout() {
    let (event_tx, _event_rx) = flume::unbounded();
    let (report_tx, report_rx) = flume::unbounded();
    let notifier = Arc::new(Notifier::new().unwrap());
    let ctl = WorkerCtl::<TestIrqSender>::new("fake".into(), event_tx, notifier, report_rx);

    let seq1 = ctl.request_reset();
    let seq2 = ctl.request_reset();
    assert!(!ctl.poll_reset(seq1));
    assert_matches!(
        ctl.wait_reset(seq1, Duration::from_millis(10)),
        Err(Error::ResetTimeout { timeout, .. }) if timeout == Duration::from_millis(10)
    );

    // Completing a reset also completes the earlier ones.
    report_tx
        .send(WorkerReport::ResetDone { seq: seq2 })
        .unwrap();
    assert!(ctl.poll_reset(seq1));
    assert_matches!(ctl.wait_reset(seq2, Duration::ZERO), Ok(()));
}

#[test]
fn test_reset_seq_wraps_around() {
    let (event_tx, event_rx) = flume::unbounded();
    let (report_tx, report_rx) = flume::unbounded();
    let notifier = Arc::new(Notifier::new().unwrap());
    let ctl = WorkerCtl::<TestIrqSender>::new("fake".into(), event_tx, notifier, report_rx);
    // Pretend that almost all sequence numbers have been used up.
    *ctl.reset_seq.lock() = u64::MAX - 1;
    ctl.reports.lock().reset_done = u64::MAX - 1;

    let seq1 = ctl.request_reset();
    let seq2 = ctl.request_reset();
    assert_eq!((seq1, seq2), (u64::MAX, 0));
    for seq in [seq1, seq2] {
        assert_matches!(event_rx.try_recv(), Ok(WakeEvent::Reset { seq: s }) if s == seq);
    }
    assert!(!ctl.poll_reset(seq1));

    report_tx
        .send(WorkerReport::ResetDone { seq: seq1 })
        .unwrap();
    assert!(ctl.poll_reset(seq1));
    assert!(!ctl.poll_reset(seq2));

    report_tx
        .send(WorkerReport::ResetDone { seq: seq2 })
        .unwrap();
    assert!(ctl.poll_reset(seq1));
    assert_matches!(ctl.wait_reset(seq2, Duration::ZERO), Ok(()));
}

#[test]
fn test_request_reset_in_order() {
    let (event_tx, event_rx) = flume::unbounded();
    let notifier = Arc::new(Notifier::new().unwrap());
    let report_rx = flume::unbounded().1;
    let ctl = WorkerCtl::<TestIrqSender>::new("fake".into(), event_tx, notifier, report_rx);
    let ctl = Arc::new(ctl);

    let threads: Vec<_> = (0..4)
        .map(|_| {
            let ctl = ctl.clone();
            thread::spawn(move || {
                for _ in 0..100 {
                    ctl.request_reset();
                }
            })
        })
        .collect();
    for t in threads {
        t.join().unwrap();
    }

    // The worker sees the resets in the order of their sequence numbers,
    // even when they are requested concurrently.
    let seqs: Vec<u64> = event_rx
        .try_iter()
        .map(|event| match event {
            WakeEvent::Reset { seq } => seq,
            event => panic!("unexpected event: {event:?}"),
        })
        .collect();
    assert_eq!(seqs, (1..=400).collect::<Vec<_>>());
}

#[test]
fn test_worker_failure_until_reset() {
    let (event_tx, _event_rx) = flume::unbounded();
    let (report_tx, report_rx) = flume::unbounded();
    let notifier = Arc::new(Notifier::new().unwrap());
    let ctl = WorkerCtl::<TestIrqSender>::new("fake".into(), event_tx, notifier, report_rx);
    assert!(!ctl.failed());

    // Reports arrive in order. A failure before a reset is gone with the
    // reset, while a failure after it is not.
    let seq = ctl.request_reset();
    for report in [
        WorkerReport::Failed,
        WorkerReport::ResetDone { seq },
        WorkerReport::Failed,
    ] {
        report_tx.send(report).unwrap();
    }
    assert!(ctl.failed());
    assert!(ctl.poll_reset(seq));

    let seq = ctl.request_reset();
    report_tx.send(WorkerReport::ResetDone { seq }).unwrap();
    assert!(!ctl.failed());
}

#[test]
fn test_worker_pause_before_start() {
    let worker = TestWorker::spawn();
    worker.pause();
    // The device has not started, so there was nothing to stop.
    assert_eq!(worker.pauses(), 0);

    // Neither is there anything to resume.
    assert!(worker.ctl.wake(WakeEvent::Resume));
    worker.start();
    worker.wait_activations(1);
    assert_eq!(worker.resumes.load(Ordering::Acquire), 0);
    worker.shutdown();
}

#[test]
fn test_worker_pause_resume() {
    let worker = TestWorker::spawn();
    worker.start();
    for count in 1..=2 {
        worker.pause();
        assert_eq!(worker.pauses(), count);
        // A paused worker answers another pause without stopping the
        // device again.
        worker.pause();
        assert_eq!(worker.pauses(), count);
        worker.resume(count);
    }
    assert!(!worker.ctl.failed());
    assert_eq!(worker.resets(), 0);
    worker.shutdown();
}

#[test]
fn test_paused_worker_resets() {
    let worker = TestWorker::spawn();
    worker.start();
    worker.pause();
    worker.reset();
    assert_eq!(worker.resets(), 1);

    // The reset ended the pause, so the device starts without a resume.
    worker.start();
    worker.wait_activations(2);
    assert_eq!(worker.resumes.load(Ordering::Acquire), 0);

    // A paused worker shuts down.
    worker.pause();
    assert_eq!(worker.pauses(), 2);
    worker.shutdown();
}

#[test]
fn test_worker_answers_pause_before_reset() {
    let worker = TestWorker::spawn();
    worker.start();
    // The worker may see both requests at once, or the reset after it
    // has paused.
    let pause_seq = worker.ctl.request_pause();
    let reset_seq = worker.ctl.request_reset();
    assert_matches!(worker.ctl.wait_reset(reset_seq, TIMEOUT), Ok(()));
    assert_matches!(worker.ctl.wait_pause(pause_seq, TIMEOUT), Ok(()));
    assert_eq!(worker.resets(), 1);
    worker.shutdown();
}

#[test]
fn test_worker_pause_failure() {
    let worker = TestWorker::spawn();
    worker.start();
    worker.fail_pause.store(true, Ordering::Release);
    let seq = worker.ctl.request_pause();
    assert_matches!(
        worker.ctl.wait_pause(seq, TIMEOUT),
        Err(Error::PauseFailed { .. })
    );
    // The device keeps running.
    assert!(!worker.ctl.failed());
    assert_eq!(worker.resets(), 0);

    worker.fail_pause.store(false, Ordering::Release);
    worker.pause();
    assert_eq!(worker.pauses(), 1);
    worker.shutdown();
}

#[test]
fn test_worker_exit_completes_pause() {
    let worker = TestWorker::spawn();
    worker.start();
    let TestWorker { ctl, handle, .. } = worker;
    assert!(ctl.wake(WakeEvent::Shutdown));
    handle.join().unwrap();

    // An exited worker no longer uses the queues.
    let seq = ctl.request_pause();
    assert_matches!(ctl.wait_pause(seq, TIMEOUT), Ok(()));
}

#[test]
fn test_wait_pause_timeout() {
    let (event_tx, _event_rx) = flume::unbounded();
    let (report_tx, report_rx) = flume::unbounded();
    let notifier = Arc::new(Notifier::new().unwrap());
    let ctl = WorkerCtl::<TestIrqSender>::new("fake".into(), event_tx, notifier, report_rx);

    let seq1 = ctl.request_pause();
    let seq2 = ctl.request_pause();
    assert_matches!(
        ctl.wait_pause(seq1, Duration::from_millis(10)),
        Err(Error::PauseTimeout { timeout, .. }) if timeout == Duration::from_millis(10)
    );

    // The answer to a pause also answers the earlier ones.
    report_tx.send(WorkerReport::Paused { seq: seq2 }).unwrap();
    assert_matches!(ctl.wait_pause(seq1, Duration::ZERO), Ok(()));
    assert_matches!(ctl.wait_pause(seq2, Duration::ZERO), Ok(()));

    let seq3 = ctl.request_pause();
    report_tx
        .send(WorkerReport::PauseFailed { seq: seq3 })
        .unwrap();
    assert_matches!(
        ctl.wait_pause(seq3, Duration::ZERO),
        Err(Error::PauseFailed { .. })
    );
}

/// A backend whose requests in flight do not end right away: its first
/// `pause()` is interrupted by `wake`, if any.
struct SlowBackend {
    event_tx: Sender<WakeEvent<TestIrqSender>>,
    wake: Option<WakeEvent<TestIrqSender>>,
    pauses: u32,
    resumes: u32,
    /// How often the device had resumed when the backend resumed last.
    dev_resumes: u32,
}

impl ActiveBackend<FakeDev> for SlowBackend {
    type Event = DeviceEvent;
    type Q = SplitQueue;

    fn queues(&mut self) -> &mut [Option<Queue<SplitQueue>>] {
        &mut []
    }

    fn handle_event(&mut self, _: &mut FakeDev, _: &DeviceEvent) -> Result<()> {
        Ok(())
    }

    fn handle_queue(&mut self, _: &mut FakeDev, _: u16) -> Result<()> {
        Ok(())
    }

    fn pause(&mut self, _: &mut FakeDev) -> Result<bool> {
        self.pauses += 1;
        let Some(event) = self.wake.take() else {
            return Ok(true);
        };
        self.event_tx.send(event).unwrap();
        Ok(false)
    }

    fn resume(&mut self, dev: &mut FakeDev) -> Result<()> {
        self.resumes += 1;
        self.dev_resumes = dev.resumes.load(Ordering::Acquire);
        Ok(())
    }

    fn resume_dev(&mut self, dev: &mut FakeDev) -> Result<()> {
        dev.resume::<SplitQueue, TestIrqSender>(&[], &[], &TestIrqSender::default())
    }
}

type SlowWorker = (
    Context<FakeDev, TestIrqSender>,
    SlowBackend,
    Sender<WakeEvent<TestIrqSender>>,
    Receiver<WorkerReport>,
);

/// Returns a running worker context that has pause 1 pending, its
/// backend, and the other ends of its channels.
fn fixture_slow_worker(wake: Option<WakeEvent<TestIrqSender>>) -> SlowWorker {
    let (event_tx, event_rx) = flume::unbounded();
    let (report_tx, report_rx) = flume::unbounded();
    let context = Context {
        dev: FakeDev::default(),
        param: WorkerParam {
            event_rx,
            report_tx,
            memory: Arc::new(fixture_ram_bus()),
            queue_regs: Arc::new([]),
        },
        state: WorkerState::Running,
        reset_seq: None,
        pause_seq: None,
        ram_generation: None,
    };
    let backend = SlowBackend {
        event_tx: event_tx.clone(),
        wake,
        pauses: 0,
        resumes: 0,
        dev_resumes: 0,
    };
    event_tx.send(WakeEvent::Pause { seq: 1 }).unwrap();
    (context, backend, event_tx, report_rx)
}

#[test]
fn test_pause_called_off() {
    // The transport gives up while the requests in flight end.
    let (mut context, mut backend, _event_tx, report_rx) =
        fixture_slow_worker(Some(WakeEvent::Resume));
    context.handle_wake_events(&mut backend).unwrap();

    // The backend continues the requests that it stopped.
    assert_eq!((backend.pauses, backend.resumes), (1, 1));
    assert_eq!(context.dev.pauses.load(Ordering::Acquire), 0);
    assert_eq!(context.state, WorkerState::Running);
    assert!(report_rx.try_recv().is_err());
}

#[test]
fn test_pause_interrupted_by_reset() {
    let (mut context, mut backend, _event_tx, report_rx) =
        fixture_slow_worker(Some(WakeEvent::Reset { seq: 7 }));
    context.handle_wake_events(&mut backend).unwrap();

    // The event loop ends. The worker answers both requests afterwards.
    assert_eq!(context.state, WorkerState::Pending);
    assert_eq!(context.reset_seq, Some(7));
    assert_eq!(context.pause_seq, Some(1));
    assert_eq!((backend.pauses, backend.resumes), (1, 0));
    assert!(report_rx.try_recv().is_err());
}

#[test]
fn test_pause_interrupted_by_other_event() {
    let (mut context, mut backend, event_tx, report_rx) =
        fixture_slow_worker(Some(WakeEvent::Notify { q_index: 0 }));
    let resumer = thread::spawn(move || {
        let report = report_rx.recv_timeout(TIMEOUT);
        event_tx.send(WakeEvent::Resume).unwrap();
        report
    });
    context.handle_wake_events(&mut backend).unwrap();

    // The worker goes on with the pause, and waits for the resume.
    assert_eq!(resumer.join().unwrap(), Ok(WorkerReport::Paused { seq: 1 }));
    assert_eq!((backend.pauses, backend.resumes), (2, 1));
    assert_eq!(context.dev.pauses.load(Ordering::Acquire), 1);
    assert_eq!(context.dev.resumes.load(Ordering::Acquire), 1);
    assert_eq!(context.state, WorkerState::Running);
    // The device resumes first, so that it serves its queues again by the
    // time the backend notifies them.
    assert_eq!(backend.dev_resumes, 1);
}

#[test]
fn test_mio_resume_dev() {
    let mut dev = FakeDev::default();
    let notifiers = [Notifier::new().unwrap(), Notifier::new().unwrap()];
    let mut poll = Poll::new().unwrap();
    let mut queues: [Option<Queue<SplitQueue>>; 3] = [None, None, None];
    let irq_sender = TestIrqSender::default();
    let mut active_mio = ActiveMio {
        queues: &mut queues,
        irq_sender: &irq_sender,
        notifiers: &notifiers,
        poll: &mut poll,
    };

    // The device resumes with what the event loop works on.
    active_mio.resume_dev(&mut dev).unwrap();
    assert_eq!(dev.resumes.load(Ordering::Acquire), 1);
    assert_eq!(*dev.resumed_with.lock(), Some((3, 2)));
}

#[test]
fn test_mio_resume_notifies_offloaded_queues() {
    // Something else serves queue 1, and may have dropped a notification
    // while the device was paused.
    let mut dev = FakeDev {
        offloaded: Some(1),
        ..Default::default()
    };
    let mut observer = Poll::new().unwrap();
    let mut notifiers = [Notifier::new().unwrap(), Notifier::new().unwrap()];
    for (index, notifier) in notifiers.iter_mut().enumerate() {
        let registry = observer.registry();
        registry
            .register(notifier, Token(index), Interest::READABLE)
            .unwrap();
    }
    let mut poll = Poll::new().unwrap();
    let mut queues: [Option<Queue<SplitQueue>>; 0] = [];
    let irq_sender = TestIrqSender::default();
    let mut active_mio = ActiveMio {
        queues: &mut queues,
        irq_sender: &irq_sender,
        notifiers: &notifiers,
        poll: &mut poll,
    };

    ActiveBackend::resume(&mut active_mio, &mut dev).unwrap();

    // The worker itself serves queue 0, whose notifications stay pending
    // while it is paused.
    let mut events = Events::with_capacity(4);
    observer.poll(&mut events, Some(Duration::ZERO)).unwrap();
    let tokens = events.iter().map(|e| e.token()).collect::<Vec<_>>();
    assert_eq!(tokens, [Token(1)]);
}

#[test]
fn test_pause_fails_in_device() {
    let (mut context, mut backend, _event_tx, report_rx) = fixture_slow_worker(None);
    context.dev.fail_pause.store(true, Ordering::Release);
    context.handle_wake_events(&mut backend).unwrap();

    // The device keeps running, so the backend continues as well.
    assert_eq!(
        report_rx.try_recv(),
        Ok(WorkerReport::PauseFailed { seq: 1 })
    );
    assert_eq!((backend.pauses, backend.resumes), (1, 1));
    assert_eq!(context.state, WorkerState::Running);
}

/// Checks that `worker` records the vring bases `bases` of its queue, of
/// which `guest_q` is the driver side: after 3, 5 and 6 chains.
fn check_worker_records_bases<Q>(worker: TestWorker, mut guest_q: GuestQueue<Q>, bases: [u32; 3])
where
    Q: VirtQueueGuest,
{
    let mut ids = vec![];

    // The worker tells where the device stopped when it leaves the event
    // loop.
    worker.start();
    ids.extend(worker.add_chains(&mut guest_q, 3));
    worker.reset();
    assert_eq!(worker.base(), bases[0]);

    // The device continues there, instead of at the start of the rings.
    worker.start_at(bases[0]);
    ids.extend(worker.add_chains(&mut guest_q, 2));
    // The worker also tells where the device stands while it is paused.
    worker.pause();
    assert_eq!(worker.base(), bases[1]);
    worker.resume(1);
    ids.extend(worker.add_chains(&mut guest_q, 1));
    worker.reset();
    assert_eq!(worker.base(), bases[2]);

    // The device took every chain once, and the driver finds all of them
    // where it looks for the used ones.
    assert_eq!(worker.chains.load(Ordering::Acquire), 6);
    for id in ids {
        assert_eq!(guest_q.get_used().unwrap().id, id);
    }
    assert!(guest_q.get_used().is_none());
    worker.shutdown();
}

#[rstest]
#[case(false, [3, 5, 6])]
#[case(true, [0x8003_8003, 0x8005_8005, 0x8006_8006])]
fn test_worker_records_bases(#[case] packed: bool, #[case] bases: [u32; 3]) {
    let mut worker = TestWorker::spawn();
    let ram = worker.ram_bus.load();
    let queue_regs = worker.queue_regs.clone();
    let reg = &queue_regs[0];
    if packed {
        worker.feature |= VirtioFeature::RING_PACKED.bits();
        let guest_q = PackedQueue::new(reg, &ram, false).unwrap().unwrap();
        check_worker_records_bases(worker, GuestQueue::new(guest_q, reg), bases);
    } else {
        let guest_q = SplitQueue::new(reg, &ram, false).unwrap().unwrap();
        check_worker_records_bases(worker, GuestQueue::new(guest_q, reg), bases);
    }
}

#[test]
fn test_worker_fails_at_invalid_base() {
    let worker = TestWorker::spawn();
    // The base of a split queue is a 16-bit index.
    worker.start_at(0x1_0000);
    worker.wait_failed();
    assert_eq!(worker.activations.load(Ordering::Acquire), 0);

    // A reset brings the device back into service.
    worker.reset();
    assert!(!worker.ctl.failed());
    worker.start();
    worker.wait_activations(1);
    worker.shutdown();
}
