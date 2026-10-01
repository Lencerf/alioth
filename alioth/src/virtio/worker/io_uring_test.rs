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

use std::io::{PipeReader, PipeWriter, Read, Write};
use std::os::fd::AsRawFd;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

use flume::{Receiver, Sender};
use io_uring::cqueue::Entry as Cqe;
use io_uring::opcode;
use io_uring::types::Fd;

use crate::mem::mapped::RamBus;
use crate::sync::notifier::Notifier;
use crate::virtio::dev::entropy::{EntropyConfig, EntropyFeature};
use crate::virtio::dev::{ActiveBackend, StartParam, Virtio, WakeEvent, WorkerParam};
use crate::virtio::queue::split::SplitQueue;
use crate::virtio::queue::tests::GuestQueue;
use crate::virtio::queue::{DescChain, Queue, QueueReg, VirtQueue};
use crate::virtio::tests::{DATA_ADDR, FakeIrqSender, fixture_queues, fixture_ram_bus};
use crate::virtio::worker::io_uring::{
    ActiveIoUring, BufferAction, IoUring, RING_SIZE, VirtioIoUring,
};
use crate::virtio::{DeviceId, FEATURE_BUILT_IN, IrqSender, Result, VirtioFeature};

/// A device that reads guest buffers from a pipe with io_uring.
#[derive(Debug)]
struct PipeDev {
    reader: PipeReader,
    config: Arc<EntropyConfig>,
    /// Reports the id of every chain submitted to the ring.
    submitted: Sender<u16>,
    /// Counts the chains whose read completed.
    completed: Arc<AtomicUsize>,
}

impl Virtio for PipeDev {
    type Config = EntropyConfig;
    type Feature = EntropyFeature;

    fn id(&self) -> DeviceId {
        DeviceId::ENTROPY
    }

    fn name(&self) -> &str {
        "pipe-dev"
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

impl VirtioIoUring for PipeDev {
    fn activate<Q, S>(
        &mut self,
        _feature: u128,
        _ring: &mut ActiveIoUring<'_, '_, Q, S>,
    ) -> Result<()>
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
}

/// What a test writes to the pipe once the read should be gone.
const AFTER_CANCEL: &[u8] = b"written after cancel";

/// A guest with one queue, and the pipe its device reads from.
///
/// Nothing is written to the pipe while the device runs, so the read
/// submitted for a posted buffer stays in flight until something cancels
/// it. The fixture keeps a second handle to the read end of the pipe to
/// check, once the device is done, that the read is really gone.
struct Fixture {
    ram_bus: Arc<RamBus>,
    regs: Arc<[QueueReg]>,
    irq_sender: Arc<FakeIrqSender>,
    irq_rx: Receiver<u16>,
    submitted_rx: Receiver<u16>,
    completed: Arc<AtomicUsize>,
    observer: PipeReader,
}

impl Fixture {
    /// Returns the fixture, the device under test, and the write end of
    /// the pipe.
    fn new() -> (Self, PipeDev, PipeWriter) {
        let (reader, writer) = std::io::pipe().unwrap();
        let observer = reader.try_clone().unwrap();
        let (submitted_tx, submitted_rx) = flume::unbounded();
        let completed = Arc::new(AtomicUsize::new(0));
        let dev = PipeDev {
            reader,
            config: Arc::new(EntropyConfig),
            submitted: submitted_tx,
            completed: completed.clone(),
        };
        let (irq_tx, irq_rx) = flume::unbounded();
        let fixture = Fixture {
            ram_bus: Arc::new(fixture_ram_bus()),
            regs: Arc::from(fixture_queues(1)),
            irq_sender: Arc::new(FakeIrqSender { q_tx: irq_tx }),
            irq_rx,
            submitted_rx,
            completed,
            observer,
        };
        (fixture, dev, writer)
    }

    /// Posts one writable buffer at `DATA_ADDR` and returns its chain id.
    fn post_buffer(&self) -> u16 {
        let ram = self.ram_bus.load();
        let mut guest_q = GuestQueue::new(
            SplitQueue::new(&self.regs[0], &ram, false)
                .unwrap()
                .unwrap(),
            &self.regs[0],
        );
        guest_q.add_desc(&[], &[(DATA_ADDR, 4 << 10)])
    }

    /// Builds the queues of a worker's event loop.
    fn queues(&self) -> [Option<Queue<'_, SplitQueue>>; 1] {
        [Queue::new(&self.regs[0], self.ram_bus.clone(), false).unwrap()]
    }

    /// Builds the event loop state of a worker around `queues`, and
    /// submits the posted buffer to the ring.
    fn activate<'a, 'r>(
        &'a self,
        dev: &mut PipeDev,
        queues: &'a mut [Option<Queue<'r, SplitQueue>>],
    ) -> ActiveIoUring<'a, 'r, SplitQueue, FakeIrqSender> {
        let mut active = ActiveIoUring {
            ring: io_uring::IoUring::new(RING_SIZE as u32).unwrap(),
            queues,
            irq_sender: &*self.irq_sender,
            notifiers: &[],
            shared_count: RING_SIZE - 1,
            submit_counts: Box::new([0]),
        };
        active.submit_buffers(dev, 0).unwrap();
        active.ring.submit().unwrap();
        active
    }

    /// Checks that the read is gone: what is written to the pipe now stays
    /// there instead of landing in guest memory, no chain completed, and no
    /// interrupt was sent.
    fn assert_read_cancelled(&self, mut writer: PipeWriter) {
        writer.write_all(AFTER_CANCEL).unwrap();
        drop(writer);
        let mut buf = Vec::new();
        (&self.observer).read_to_end(&mut buf).unwrap();
        assert_eq!(buf, AFTER_CANCEL);
        assert_eq!(self.completed.load(Ordering::Relaxed), 0);
        assert!(self.irq_rx.try_recv().is_err());
    }
}

#[test]
fn io_uring_drain_test() {
    let (f, dev, writer) = Fixture::new();
    let (tx, rx) = flume::unbounded();
    let (handle, notifier) = dev
        .spawn_worker(WorkerParam {
            event_rx: rx,
            memory: f.ram_bus.clone(),
            queue_regs: f.regs.clone(),
        })
        .unwrap();
    let start_param = StartParam {
        feature: VirtioFeature::VERSION_1.bits(),
        irq_sender: f.irq_sender.clone(),
        notifiers: None,
    };
    tx.send(WakeEvent::Start { param: start_param }).unwrap();
    notifier.notify().unwrap();

    // The pipe is empty, so the read stays in flight.
    let id = f.post_buffer();
    tx.send(WakeEvent::Notify { q_index: 0 }).unwrap();
    notifier.notify().unwrap();
    assert_eq!(f.submitted_rx.recv_timeout(Duration::from_secs(1)), Ok(id));

    // Shutting down cancels and reaps the read, instead of hanging or
    // leaving it to the kernel after the worker releases guest memory.
    tx.send(WakeEvent::Shutdown).unwrap();
    notifier.notify().unwrap();
    handle.join().unwrap();

    // Nobody reads from the pipe any more, so the data stays there and
    // never reaches guest memory.
    let sentinel = [0xa5u8; AFTER_CANCEL.len()];
    f.ram_bus.write(DATA_ADDR, &sentinel).unwrap();
    f.assert_read_cancelled(writer);
    let mut guest_buf = [0u8; AFTER_CANCEL.len()];
    f.ram_bus.read(DATA_ADDR, &mut guest_buf).unwrap();
    assert_eq!(guest_buf, sentinel);
}

#[test]
fn io_uring_drain_cancels_pending_reads() {
    let (f, mut dev, writer) = Fixture::new();
    let id = f.post_buffer();
    let mut queues = f.queues();
    let mut active = f.activate(&mut dev, &mut queues);
    assert_eq!(f.submitted_rx.try_recv(), Ok(id));
    assert_eq!(active.submit_counts[0], 1);

    active.drain().unwrap();
    assert_eq!(active.submit_counts[0], 0);

    // The ring is still alive, but the read is gone: data written now
    // stays in the pipe.
    f.assert_read_cancelled(writer);
    drop(active);
}

#[test]
fn io_uring_drain_cancels_requests_of_stale_queue() {
    let (f, mut dev, writer) = Fixture::new();
    let id = f.post_buffer();
    let mut queues = f.queues();
    let mut active = f.activate(&mut dev, &mut queues);
    assert_eq!(f.submitted_rx.try_recv(), Ok(id));

    // All memory goes away while the read is in flight. The queue stays on
    // its old snapshot, so the read's buffer stays pinned.
    let old_ram = Arc::downgrade(&f.ram_bus.load());
    f.ram_bus.update(|ram| ram.remove(0)).unwrap();
    ActiveBackend::<PipeDev>::sync_ram(&mut active);
    assert!(active.queues[0].is_some());
    assert!(old_ram.upgrade().is_some());

    // Draining cancels the read, and dropping the queue releases the memory.
    active.drain().unwrap();
    assert_eq!(active.submit_counts[0], 0);
    drop(active);
    drop(queues);
    assert!(old_ram.upgrade().is_none());

    f.assert_read_cancelled(writer);
}
