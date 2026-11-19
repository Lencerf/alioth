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
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use flume::{Receiver, Sender};
use io_uring::cqueue::Entry as Cqe;
use io_uring::opcode;
use io_uring::types::Fd;

use crate::ffi;
use crate::mem::mapped::{ArcMemPages, RamBus};
use crate::sync::notifier::Notifier;
use crate::virtio::dev::entropy::{EntropyConfig, EntropyFeature};
use crate::virtio::dev::{ActiveBackend, StartParam, Virtio, WakeEvent, WorkerCtl, WorkerParam};
use crate::virtio::queue::split::SplitQueue;
use crate::virtio::queue::tests::GuestQueue;
use crate::virtio::queue::{self, DescChain, Queue, QueueReg, VirtQueue};
use crate::virtio::tests::{self, DATA_ADDR, FakeIrqSender, fixture_queues, fixture_ram_bus};
use crate::virtio::worker::io_uring::{
    ActiveIoUring, BufferAction, IoUring, RING_SIZE, VirtioIoUring,
};
use crate::virtio::{DeviceId, FEATURE_BUILT_IN, IrqSender, Result, VirtioFeature};

const TIMEOUT: Duration = Duration::from_secs(5);

/// A device that reads guest buffers from a pipe with io_uring.
#[derive(Debug)]
struct PipeReader {
    reader: OwnedFd,
    config: Arc<EntropyConfig>,
    submitted: Sender<u16>,
    completed: Arc<AtomicUsize>,
    resets: Arc<AtomicUsize>,
    /// The number of queues that the calls to [`Virtio::resume`] got.
    resumed_queues: Arc<AtomicUsize>,
}

impl Virtio for PipeReader {
    type Config = EntropyConfig;
    type Feature = EntropyFeature;

    fn id(&self) -> DeviceId {
        DeviceId::ENTROPY
    }

    fn name(&self) -> &str {
        "pipe-reader"
    }

    fn pause(&mut self) -> Result<()> {
        Ok(())
    }

    fn resume<Q, S>(&mut self, queues: &[Option<Queue<Q>>], _: &[Notifier], _: &S) -> Result<()>
    where
        Q: VirtQueue,
        S: IrqSender,
    {
        self.resumed_queues
            .fetch_add(queues.len(), Ordering::AcqRel);
        Ok(())
    }

    fn spawn_worker<S>(self, param: WorkerParam<S>) -> Result<(JoinHandle<()>, Arc<Notifier>)>
    where
        S: IrqSender,
    {
        IoUring::spawn_worker(self, param)
    }

    fn num_queues(&self) -> u16 {
        1
    }

    fn config(&self) -> Arc<EntropyConfig> {
        self.config.clone()
    }

    fn feature(&self) -> u128 {
        FEATURE_BUILT_IN
    }
}

impl VirtioIoUring for PipeReader {
    fn activate<Q, S>(&mut self, _feature: u128, _ring: &mut ActiveIoUring<'_, Q, S>) -> Result<()>
    where
        Q: VirtQueue,
        S: IrqSender,
    {
        Ok(())
    }

    fn handle_desc(&mut self, _q_index: u16, chain: &mut DescChain) -> Result<BufferAction> {
        let writable = &chain.writable;
        let entry = opcode::Readv::new(
            Fd(self.reader.as_raw_fd()),
            writable.as_ptr() as *const _,
            writable.len() as _,
        )
        .build();
        self.submitted.send(chain.id()).unwrap();
        Ok(BufferAction::Sqe(entry))
    }

    fn complete_desc(&mut self, _q_index: u16, _chain: &mut DescChain, cqe: &Cqe) -> Result<u32> {
        self.completed.fetch_add(1, Ordering::Relaxed);
        Ok(cqe.result().max(0) as u32)
    }

    fn reset(&mut self) {
        self.resets.fetch_add(1, Ordering::AcqRel);
    }
}

fn pipe() -> (OwnedFd, File) {
    let mut fds = [0; 2];
    ffi!(unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) }).unwrap();
    let reader = unsafe { OwnedFd::from_raw_fd(fds[0]) };
    let writer = unsafe { File::from_raw_fd(fds[1]) };
    (reader, writer)
}

#[test]
fn io_uring_drain_test() {
    let ram_bus = Arc::new(fixture_ram_bus());
    let regs: Arc<[QueueReg]> = Arc::from(fixture_queues(1));

    let (reader, mut writer) = pipe();
    let observer = unsafe { File::from_raw_fd(libc::dup(reader.as_raw_fd())) };
    let (submitted_tx, submitted_rx) = flume::unbounded();
    let completed = Arc::new(AtomicUsize::new(0));
    let resets = Arc::new(AtomicUsize::new(0));
    let dev = PipeReader {
        reader,
        config: Arc::new(EntropyConfig),
        submitted: submitted_tx,
        completed: completed.clone(),
        resets: resets.clone(),
        resumed_queues: Arc::default(),
    };

    let (tx, rx) = flume::unbounded();
    let (handle, notifier) = dev
        .spawn_worker(WorkerParam {
            event_rx: rx,
            report_tx: flume::unbounded().0,
            memory: ram_bus.clone(),
            queue_regs: regs.clone(),
        })
        .unwrap();
    let (irq_tx, irq_rx) = flume::unbounded();
    let start_param = StartParam {
        feature: VirtioFeature::VERSION_1.bits(),
        irq_sender: Arc::new(FakeIrqSender { q_tx: irq_tx }),
        notifiers: Option::<Arc<[Notifier]>>::None,
        bases: None,
    };
    tx.send(WakeEvent::Start { param: start_param }).unwrap();
    notifier.notify().unwrap();

    // The pipe is empty, so the read stays in flight.
    {
        let ram = ram_bus.load();
        let mut guest_q = GuestQueue::new(
            SplitQueue::new(&regs[0], &ram, false).unwrap().unwrap(),
            &regs[0],
        );
        let id = guest_q.add_desc(&[], &[(DATA_ADDR, 4 << 10)]);
        tx.send(WakeEvent::Notify { q_index: 0 }).unwrap();
        notifier.notify().unwrap();
        assert_eq!(submitted_rx.recv_timeout(Duration::from_secs(1)), Ok(id));
    }

    // Shutting down cancels and reaps the read, instead of hanging or
    // leaving it to the kernel after the worker releases guest memory.
    tx.send(WakeEvent::Shutdown).unwrap();
    notifier.notify().unwrap();
    handle.join().unwrap();
    assert_eq!(completed.load(Ordering::Relaxed), 0);
    assert_eq!(resets.load(Ordering::Acquire), 1);
    assert!(irq_rx.try_recv().is_err());

    // Nobody reads from the pipe any more, so the data stays there and
    // never reaches guest memory.
    let s = b"written after shutdown";
    let sentinel = [0xa5u8; 22];
    ram_bus.write(DATA_ADDR, &sentinel).unwrap();
    writer.write_all(s).unwrap();
    drop(writer);
    let mut buf = Vec::new();
    let mut observer = observer;
    observer.read_to_end(&mut buf).unwrap();
    assert_eq!(buf, s);
    let mut guest_buf = [0u8; 22];
    ram_bus.read(DATA_ADDR, &mut guest_buf).unwrap();
    assert_eq!(guest_buf, sentinel);
}

#[test]
fn io_uring_drain_cancels_pending_reads() {
    let ram_bus = Arc::new(fixture_ram_bus());
    let ram = ram_bus.load();
    let regs = fixture_queues(1);
    let mut guest_q = GuestQueue::new(
        SplitQueue::new(&regs[0], &ram, false).unwrap().unwrap(),
        &regs[0],
    );
    guest_q.add_desc(&[], &[(DATA_ADDR, 4 << 10)]);

    let (reader, mut writer) = pipe();
    let mut observer = unsafe { File::from_raw_fd(libc::dup(reader.as_raw_fd())) };
    let (submitted_tx, submitted_rx) = flume::unbounded();
    let completed = Arc::new(AtomicUsize::new(0));
    let mut dev = PipeReader {
        reader,
        config: Arc::new(EntropyConfig),
        submitted: submitted_tx,
        completed: completed.clone(),
        resets: Arc::default(),
        resumed_queues: Arc::default(),
    };

    let (irq_tx, irq_rx) = flume::unbounded();
    let irq_sender = FakeIrqSender { q_tx: irq_tx };
    let mut queues = [Queue::<SplitQueue>::new(&regs[0], ram_bus.clone(), false).unwrap()];
    let mut active = ActiveIoUring {
        ring: io_uring::IoUring::new(RING_SIZE as u32).unwrap(),
        queues: &mut queues,
        irq_sender: &irq_sender,
        notifiers: Arc::new([]),
        shared_count: RING_SIZE - 1,
        submit_counts: Box::new([0]),
        cancelling: Vec::new(),
        held: Vec::new(),
    };
    active.submit_buffers(&mut dev, 0).unwrap();
    active.ring.submit().unwrap();
    assert!(submitted_rx.try_recv().is_ok());
    assert_eq!(active.submit_counts[0], 1);

    active.drain().unwrap();
    assert_eq!(active.submit_counts[0], 0);

    // The ring is still alive, but the read is gone: data written now
    // stays in the pipe.
    let s = b"written after drain";
    writer.write_all(s).unwrap();
    drop(writer);
    drop(dev);
    let mut buf = Vec::new();
    observer.read_to_end(&mut buf).unwrap();
    assert_eq!(buf, s);
    assert_eq!(completed.load(Ordering::Relaxed), 0);
    assert!(irq_rx.try_recv().is_err());
    drop(active);
}

#[test]
fn io_uring_request_in_flight_across_layout_change() {
    const EXTRA_GPA: u64 = 4 << 20;

    let ram_bus = Arc::new(fixture_ram_bus());
    let regs = fixture_queues(1);
    let mut guest_q = GuestQueue::new(
        SplitQueue::new(&regs[0], &ram_bus.load(), false)
            .unwrap()
            .unwrap(),
        &regs[0],
    );
    let pages = ArcMemPages::from_anonymous(1 << 20, None, None).unwrap();
    ram_bus.update(|ram| ram.add(EXTRA_GPA, pages)).unwrap();
    let ram = ram_bus.load();
    let id = guest_q.add_desc(&[], &[(EXTRA_GPA, 4 << 10)]);

    let (reader, mut writer) = pipe();
    let (submitted_tx, submitted_rx) = flume::unbounded();
    let mut dev = PipeReader {
        reader,
        config: Arc::new(EntropyConfig),
        submitted: submitted_tx,
        completed: Arc::default(),
        resets: Arc::default(),
        resumed_queues: Arc::default(),
    };
    let (irq_tx, irq_rx) = flume::unbounded();
    let irq_sender = FakeIrqSender { q_tx: irq_tx };
    let mut queues = [Queue::<SplitQueue>::new(&regs[0], ram_bus.clone(), false).unwrap()];
    let mut active = ActiveIoUring {
        ring: io_uring::IoUring::new(RING_SIZE as u32).unwrap(),
        queues: &mut queues,
        irq_sender: &irq_sender,
        notifiers: Arc::new([]),
        shared_count: RING_SIZE - 1,
        submit_counts: Box::new([0]),
        cancelling: Vec::new(),
        held: Vec::new(),
    };
    active.submit_buffers(&mut dev, 0).unwrap();
    active.ring.submit().unwrap();
    assert_eq!(submitted_rx.try_recv(), Ok(id));

    // The memory of the buffer is unplugged while the read is in flight,
    // and the queue moves to the new layout. The chain keeps the old one.
    let old_ram = Arc::downgrade(&ram);
    drop(ram);
    ram_bus.update(|ram| ram.remove(EXTRA_GPA)).unwrap();
    let queues = ActiveBackend::<PipeReader>::queues(&mut active);
    assert!(queue::sync_queues(queues).unwrap());
    let queue = active.queues[0].as_ref().unwrap();
    assert_eq!(queue.ram().generation(), ram_bus.generation());
    assert!(old_ram.upgrade().is_some());

    // The kernel still writes into the buffer, and completing the chain
    // releases the old layout.
    let s = b"written after unplug";
    writer.write_all(s).unwrap();
    active.ring.submit_and_wait(1).unwrap();
    let cqe = active.ring.completion().next().unwrap();
    active.handle_event(&mut dev, &cqe).unwrap();
    assert_eq!(irq_rx.try_recv(), Ok(0));
    let used = guest_q.get_used().unwrap();
    assert_eq!((used.id, used.len), (id, s.len() as u32));
    assert!(old_ram.upgrade().is_none());
}

#[test]
fn io_uring_reset_test() {
    let ram_bus = Arc::new(tests::fixture_ram_bus());
    let regs: Arc<[QueueReg]> = Arc::from(tests::fixture_queues(1));

    let (reader, _writer) = pipe();
    let (submitted_tx, submitted_rx) = flume::unbounded();
    let completed = Arc::new(AtomicUsize::new(0));
    let resets = Arc::new(AtomicUsize::new(0));
    let dev = PipeReader {
        reader,
        config: Arc::new(EntropyConfig),
        submitted: submitted_tx,
        completed: completed.clone(),
        resets: resets.clone(),
        resumed_queues: Arc::default(),
    };

    let (event_tx, event_rx) = flume::unbounded();
    let (report_tx, report_rx) = flume::unbounded();
    let (handle, notifier) = dev
        .spawn_worker(WorkerParam {
            event_rx,
            report_tx,
            memory: ram_bus.clone(),
            queue_regs: regs.clone(),
        })
        .unwrap();
    let ctl = WorkerCtl::new("pipe-reader".into(), event_tx, notifier, report_rx);

    // Resetting before start does not invoke VirtioIoUring::reset.
    let seq = ctl.request_reset();
    ctl.wait_reset(seq, Duration::from_secs(5)).unwrap();
    assert_eq!(resets.load(Ordering::Acquire), 0);

    let (irq_tx, _irq_rx) = flume::unbounded();
    let irq_sender = Arc::new(FakeIrqSender { q_tx: irq_tx });
    let start = || StartParam {
        feature: VirtioFeature::VERSION_1.bits(),
        irq_sender: irq_sender.clone(),
        notifiers: None,
        bases: None,
    };
    assert!(ctl.wake(WakeEvent::Start { param: start() }));

    // Submit an in-flight read and then reset the device.
    {
        let ram = ram_bus.load();
        let mut guest_q = GuestQueue::new(
            SplitQueue::new(&regs[0], &ram, false).unwrap().unwrap(),
            &regs[0],
        );
        let id = guest_q.add_desc(&[], &[(DATA_ADDR, 4 << 10)]);
        assert!(ctl.wake(WakeEvent::Notify { q_index: 0 }));
        assert_eq!(submitted_rx.recv_timeout(Duration::from_secs(1)), Ok(id));
    }

    let seq = ctl.request_reset();
    ctl.wait_reset(seq, Duration::from_secs(5)).unwrap();
    assert_eq!(completed.load(Ordering::Relaxed), 0);
    assert_eq!(resets.load(Ordering::Acquire), 1);

    // Re-activate and reset a second time.
    assert!(ctl.wake(WakeEvent::Start { param: start() }));
    let seq = ctl.request_reset();
    ctl.wait_reset(seq, Duration::from_secs(5)).unwrap();
    assert_eq!(resets.load(Ordering::Acquire), 2);

    assert!(ctl.wake(WakeEvent::Shutdown));
    handle.join().unwrap();
}

/// A pipe reader, and its queue with one chain available.
struct PipeFixture {
    ram_bus: Arc<RamBus>,
    dev: PipeReader,
    writer: File,
    submitted: Receiver<u16>,
    completed: Arc<AtomicUsize>,
    irq_sender: FakeIrqSender,
    irq_rx: Receiver<u16>,
    queues: [Option<Queue<SplitQueue>>; 1],
    /// The ID of the chain.
    id: u16,
}

fn fixture_pipe_reader() -> PipeFixture {
    let ram_bus = Arc::new(fixture_ram_bus());
    let ram = ram_bus.load();
    let regs = fixture_queues(1);
    let mut guest_q = GuestQueue::new(
        SplitQueue::new(&regs[0], &ram, false).unwrap().unwrap(),
        &regs[0],
    );
    let id = guest_q.add_desc(&[], &[(DATA_ADDR, 4 << 10)]);

    let (reader, writer) = pipe();
    let (submitted_tx, submitted) = flume::unbounded();
    let completed = Arc::new(AtomicUsize::new(0));
    let dev = PipeReader {
        reader,
        config: Arc::new(EntropyConfig),
        submitted: submitted_tx,
        completed: completed.clone(),
        resets: Arc::default(),
        resumed_queues: Arc::default(),
    };
    let (irq_tx, irq_rx) = flume::unbounded();
    let queues = [Queue::<SplitQueue>::new(&regs[0], ram_bus.clone(), false).unwrap()];
    PipeFixture {
        ram_bus,
        dev,
        writer,
        submitted,
        completed,
        irq_sender: FakeIrqSender { q_tx: irq_tx },
        irq_rx,
        queues,
        id,
    }
}

/// Returns a ring with the read for the chain of `fixture` in flight. The
/// pipe is empty, so the read does not complete by itself.
fn fixture_read_in_flight<'a>(
    queues: &'a mut [Option<Queue<SplitQueue>>],
    irq_sender: &'a FakeIrqSender,
    dev: &mut PipeReader,
) -> ActiveIoUring<'a, SplitQueue, FakeIrqSender> {
    let mut active = ActiveIoUring {
        ring: io_uring::IoUring::new(RING_SIZE as u32).unwrap(),
        queues,
        irq_sender,
        notifiers: Arc::new([]),
        shared_count: RING_SIZE - 1,
        submit_counts: Box::new([0]),
        cancelling: Vec::new(),
        held: Vec::new(),
    };
    active.submit_buffers(dev, 0).unwrap();
    active.ring.submit().unwrap();
    assert_eq!(active.submit_counts[0], 1);
    active
}

/// Handles completions until the device has completed a chain.
fn wait_completed(
    active: &mut ActiveIoUring<'_, SplitQueue, FakeIrqSender>,
    dev: &mut PipeReader,
    completed: &AtomicUsize,
) {
    while completed.load(Ordering::Relaxed) == 0 {
        active.ring.submit_and_wait(1).unwrap();
        let cqe = active.ring.completion().next().unwrap();
        active.handle_event(dev, &cqe).unwrap();
    }
}

#[test]
fn io_uring_pause_resubmits_pending_reads() {
    let mut f = fixture_pipe_reader();
    let mut active = fixture_read_in_flight(&mut f.queues, &f.irq_sender, &mut f.dev);
    assert_eq!(f.submitted.try_recv(), Ok(f.id));

    // The pause cancels the read, and the chain stays with the queue.
    assert!(active.pause(&mut f.dev).unwrap());
    assert_eq!(active.submit_counts[0], 0);
    assert_eq!(active.held, [(0, f.id)]);
    assert_eq!(f.completed.load(Ordering::Relaxed), 0);
    assert!(f.irq_rx.try_recv().is_err());

    // Nothing reads the pipe while the device is paused.
    let s = b"written while paused";
    let sentinel = [0xa5u8; 20];
    f.ram_bus.write(DATA_ADDR, &sentinel).unwrap();
    f.writer.write_all(s).unwrap();
    thread::sleep(Duration::from_millis(50));
    let mut guest_buf = [0u8; 20];
    f.ram_bus.read(DATA_ADDR, &mut guest_buf).unwrap();
    assert_eq!(guest_buf, sentinel);

    // The resume submits the read again, which completes the chain.
    active.resume(&mut f.dev).unwrap();
    assert_eq!(f.submitted.try_recv(), Ok(f.id));
    assert!(active.held.is_empty());
    wait_completed(&mut active, &mut f.dev, &f.completed);
    f.ram_bus.read(DATA_ADDR, &mut guest_buf).unwrap();
    assert_eq!(&guest_buf, s);
    assert_eq!(f.irq_rx.try_recv(), Ok(0));
    assert_eq!(active.submit_counts[0], 0);
}

#[test]
fn io_uring_pause_interrupted_by_wake_event() {
    let mut f = fixture_pipe_reader();
    let mut active = fixture_read_in_flight(&mut f.queues, &f.irq_sender, &mut f.dev);

    // A completion that belongs neither to a chain nor to a queue comes
    // from the notifier of the worker.
    let nop = opcode::Nop::new().build().user_data(1 << 63);
    unsafe { active.ring.submission().push(&nop) }.unwrap();
    assert!(!active.pause(&mut f.dev).unwrap());

    // The worker looks at its wake events, and goes on with the pause.
    assert!(active.pause(&mut f.dev).unwrap());
    assert_eq!(active.submit_counts[0], 0);
    assert_eq!(active.held, [(0, f.id)]);
    assert!(active.cancelling.is_empty());
}

#[test]
fn io_uring_pause_called_off() {
    let mut f = fixture_pipe_reader();
    let mut active = fixture_read_in_flight(&mut f.queues, &f.irq_sender, &mut f.dev);
    assert_eq!(f.submitted.try_recv(), Ok(f.id));

    // The pause is called off after the read is cancelled, so the event
    // loop gets the result of the read.
    active.cancel_requests().unwrap();
    assert_eq!(active.cancelling, [(0, f.id)]);
    while active.submit_counts[0] == 1 && f.submitted.is_empty() {
        active.ring.submit_and_wait(1).unwrap();
        let cqe = active.ring.completion().next().unwrap();
        active.handle_event(&mut f.dev, &cqe).unwrap();
    }

    // It submits the read again instead of completing the chain.
    assert_eq!(f.submitted.try_recv(), Ok(f.id));
    assert_eq!(active.submit_counts[0], 1);
    assert!(active.cancelling.is_empty());
    assert!(active.held.is_empty());
    assert_eq!(f.completed.load(Ordering::Relaxed), 0);

    f.writer.write_all(b"data").unwrap();
    wait_completed(&mut active, &mut f.dev, &f.completed);
    assert_eq!(f.irq_rx.try_recv(), Ok(0));
}

#[test]
fn io_uring_worker_pause_resume() {
    let ram_bus = Arc::new(fixture_ram_bus());
    let regs: Arc<[QueueReg]> = Arc::from(fixture_queues(1));

    let (reader, mut writer) = pipe();
    let (submitted_tx, submitted_rx) = flume::unbounded();
    let completed = Arc::new(AtomicUsize::new(0));
    let dev = PipeReader {
        reader,
        config: Arc::new(EntropyConfig),
        submitted: submitted_tx,
        completed: completed.clone(),
        resets: Arc::default(),
        resumed_queues: Arc::default(),
    };

    let (tx, rx) = flume::unbounded();
    let (report_tx, report_rx) = flume::unbounded();
    let (handle, notifier) = dev
        .spawn_worker(WorkerParam {
            event_rx: rx,
            report_tx,
            memory: ram_bus.clone(),
            queue_regs: regs.clone(),
        })
        .unwrap();
    let ctl = WorkerCtl::new("pipe-reader".into(), tx, notifier, report_rx);
    let (irq_tx, irq_rx) = flume::unbounded();
    let param = StartParam {
        feature: VirtioFeature::VERSION_1.bits(),
        irq_sender: Arc::new(FakeIrqSender { q_tx: irq_tx }),
        notifiers: Option::<Arc<[Notifier]>>::None,
        bases: None,
    };
    assert!(ctl.wake(WakeEvent::Start { param }));

    // The pipe is empty, so the read stays in flight.
    let ram = ram_bus.load();
    let mut guest_q = GuestQueue::new(
        SplitQueue::new(&regs[0], &ram, false).unwrap().unwrap(),
        &regs[0],
    );
    let id = guest_q.add_desc(&[], &[(DATA_ADDR, 4 << 10)]);
    assert!(ctl.wake(WakeEvent::Notify { q_index: 0 }));
    assert_eq!(submitted_rx.recv_timeout(TIMEOUT), Ok(id));

    // The worker pauses, although the read never completes by itself.
    let seq = ctl.request_pause();
    ctl.wait_pause(seq, TIMEOUT).unwrap();

    // A paused worker leaves the pipe and the queue alone.
    let s = b"written while paused";
    writer.write_all(s).unwrap();
    assert!(irq_rx.recv_timeout(Duration::from_millis(50)).is_err());
    assert_eq!(completed.load(Ordering::Relaxed), 0);

    // It reads the pipe again once the device resumes.
    assert!(ctl.wake(WakeEvent::Resume));
    assert_eq!(submitted_rx.recv_timeout(TIMEOUT), Ok(id));
    assert_eq!(irq_rx.recv_timeout(TIMEOUT), Ok(0));
    let mut guest_buf = [0u8; 20];
    ram_bus.read(DATA_ADDR, &mut guest_buf).unwrap();
    assert_eq!(&guest_buf, s);

    assert!(ctl.wake(WakeEvent::Shutdown));
    handle.join().unwrap();
}

#[test]
fn io_uring_resume_dev() {
    let mut f = fixture_pipe_reader();
    let mut active = fixture_read_in_flight(&mut f.queues, &f.irq_sender, &mut f.dev);

    // The device resumes with the queue of the event loop.
    active.resume_dev(&mut f.dev).unwrap();
    assert_eq!(f.dev.resumed_queues.load(Ordering::Acquire), 1);
}
