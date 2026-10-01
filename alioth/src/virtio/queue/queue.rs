// Copyright 2024 Google LLC
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

pub mod packed;
pub mod split;

use std::collections::HashMap;
use std::fmt::Debug;
use std::io::{ErrorKind, IoSlice, IoSliceMut, Read, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU64, Ordering, fence};

use crate::bitflags;
use crate::mem::mapped::{Ram, RamBus};
use crate::virtio::{IrqSender, Result, error};

pub const QUEUE_SIZE_MAX: u16 = 256;

bitflags! {
    pub struct DescFlag(u16) {
        NEXT = 1 << 0;
        WRITE = 1 << 1;
        INDIRECT = 1 << 2;
        AVAIL = 1 << 7;
        USED = 1 << 15;
    }
}

#[derive(Debug, Default)]
pub struct QueueReg {
    pub size: AtomicU16,
    pub desc: AtomicU64,
    pub driver: AtomicU64,
    pub device: AtomicU64,
    pub enabled: AtomicBool,
    pub reset_counter: AtomicU16,
}

impl QueueReg {
    pub fn reset(&self) {
        // Disable the queue before clearing its ring addresses, and bump the
        // reset counter last. A reader that checks `enabled` and the counter
        // after loading the addresses then never accepts a partially cleared
        // queue, nor a mix of the addresses of two boots, see
        // [`RingAddrs::load`].
        self.enabled.store(false, Ordering::Release);
        self.size.store(QUEUE_SIZE_MAX, Ordering::Release);
        self.desc.store(0, Ordering::Release);
        self.driver.store(0, Ordering::Release);
        self.device.store(0, Ordering::Release);
        self.reset_counter.fetch_add(1, Ordering::Release);
    }
}

/// The ring addresses of a queue, read from its [`QueueReg`] once when the
/// device is activated.
///
/// Per the virtio spec, the addresses do not change until the device is
/// reset, but the registers can be cleared by a reset at any time. Queues
/// keep this snapshot to re-derive ring pointers when they move to a new
/// memory layout, and its `reset_counter` to detect a pending reset: it
/// identifies the boot that set up the rings, so comparing it with the
/// register is enough even if a later boot enables the queue again at the
/// same addresses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RingAddrs {
    pub size: u16,
    pub desc: u64,
    pub driver: u64,
    pub device: u64,
    pub reset_counter: u16,
}

impl RingAddrs {
    /// Reads the ring addresses, or returns `None` if the queue is disabled
    /// or a reset raced with the read.
    pub fn load(reg: &QueueReg) -> Option<Self> {
        let reset_counter = reg.reset_counter.load(Ordering::Acquire);
        if !reg.enabled.load(Ordering::Acquire) {
            return None;
        }
        let addrs = RingAddrs {
            size: reg.size.load(Ordering::Acquire),
            desc: reg.desc.load(Ordering::Acquire),
            driver: reg.driver.load(Ordering::Acquire),
            device: reg.device.load(Ordering::Acquire),
            reset_counter,
        };
        // A concurrent reset disables the queue first and bumps the counter
        // last, see [`QueueReg::reset`]. If it is still in progress, the
        // queue reads as disabled. If it completed, and the next boot has
        // already set up and enabled the queue again, the counter changed.
        // Either way, no mix of the registers of two boots is returned.
        if !reg.enabled.load(Ordering::Acquire)
            || reg.reset_counter.load(Ordering::Acquire) != reset_counter
        {
            return None;
        }
        Some(addrs)
    }
}

#[derive(Debug)]
pub struct DescChain<'m> {
    id: u16,
    delta: u16,
    pub readable: Vec<IoSlice<'m>>,
    pub writable: Vec<IoSliceMut<'m>>,
    ram: &'m Ram,
}

impl DescChain<'_> {
    pub fn id(&self) -> u16 {
        self.id
    }

    /// The memory snapshot the buffers were translated with.
    ///
    /// Devices that access guest memory beyond the buffers, e.g. at
    /// addresses found inside them, should use this snapshot. The accessor
    /// borrows the chain, so the snapshot cannot outlive the device callback.
    pub fn ram(&self) -> &Ram {
        self.ram
    }

    /// Erases the lifetime of the guest memory the chain points to.
    ///
    /// # Safety
    ///
    /// The caller must keep the memory referenced by the chain alive, e.g.
    /// by holding the [`Ram`] snapshot it was translated from, until the
    /// returned chain is dropped.
    ///
    /// The static chain must not escape to device code either. Device
    /// callbacks take `impl FnMut(&mut DescChain)`, which is higher-ranked
    /// over the chain lifetime, so they cannot keep the buffers beyond the
    /// call. Do not change them to take `DescChain<'static>`.
    unsafe fn into_static(self) -> DescChain<'static> {
        unsafe { std::mem::transmute::<DescChain<'_>, DescChain<'static>>(self) }
    }
}

/// A descriptor chain the device has not finished yet.
///
/// It pins the snapshot the chain was translated from, so the buffers stay
/// valid even if the queue moves to a newer snapshot in the meantime, e.g.
/// while an asynchronous I/O request is in flight.
struct DeferredChain {
    chain: DescChain<'static>,
    // Must be declared after `chain` so it is dropped last.
    _ram: Arc<Ram>,
}

pub trait VirtQueue: Sized {
    type Index: Clone + Copy;
    const INIT_INDEX: Self::Index;
    /// Creates the queue from its ring addresses.
    ///
    /// The queue may cache host pointers into `ram`. The caller must keep
    /// `ram` alive for as long as the queue is in use.
    fn new(addrs: &RingAddrs, ram: &Ram, event_idx: bool) -> Result<Self>;
    fn desc_avail(&self, index: Self::Index) -> bool;
    fn get_avail<'m>(&self, index: Self::Index, ram: &'m Ram) -> Result<Option<DescChain<'m>>>;
    fn set_used(&self, index: Self::Index, id: u16, len: u32);
    fn enable_notification(&self, enabled: bool);
    fn interrupt_enabled(&self, index: Self::Index, delta: u16) -> bool;
    fn index_add(&self, index: Self::Index, delta: u16) -> Self::Index;
}

#[derive(Debug)]
pub enum Status {
    Done { len: u32 },
    Deferred,
    Break,
}

/// The device side of a virtqueue.
///
/// A `Queue` owns the [`Ram`] snapshot its ring pointers were derived from,
/// so it no longer borrows guest memory. The ring state (avail and used
/// indices, deferred chains) lives in the `Queue` rather than in the
/// snapshot, which allows [`Queue::sync_ram`] to move the queue to a newer
/// snapshot without disturbing the driver.
pub struct Queue<'r, Q>
where
    Q: VirtQueue,
{
    q: Q,
    avail: Q::Index,
    used: Q::Index,
    reg: &'r QueueReg,
    addrs: RingAddrs,
    event_idx: bool,
    deferred: HashMap<u16, DeferredChain>,
    ram_bus: Arc<RamBus>,
    /// The bus generation the queue last tried to move to. It differs from
    /// `ram.generation()` only if that attempt failed.
    bus_generation: u64,
    // Must be declared after `q` so it is dropped last.
    ram: Arc<Ram>,
}

impl<'r, Q> Queue<'r, Q>
where
    Q: VirtQueue,
{
    /// Creates the queue on the latest snapshot of `ram_bus`.
    pub fn new(reg: &'r QueueReg, ram_bus: Arc<RamBus>, event_idx: bool) -> Result<Option<Self>> {
        // The snapshot must be loaded before the registers are read. A VM
        // reset clears `enabled` and bumps the reset counter before the
        // memory update publishes the new generation with Release, so
        // loading the memory snapshot first ensures that any reset that
        // happened-before the memory layout is visible in `reg`.
        let ram = ram_bus.load();
        let Some(addrs) = RingAddrs::load(reg) else {
            return Ok(None);
        };
        let q = Q::new(&addrs, &ram, event_idx)?;
        Ok(Some(Self {
            q,
            avail: Q::INIT_INDEX,
            used: Q::INIT_INDEX,
            reg,
            addrs,
            event_idx,
            deferred: HashMap::new(),
            ram_bus,
            bus_generation: ram.generation(),
            ram,
        }))
    }

    pub fn reg(&self) -> &QueueReg {
        self.reg
    }

    /// The memory snapshot the queue currently works on.
    pub fn ram(&self) -> &Arc<Ram> {
        &self.ram
    }

    /// Moves the queue to the latest snapshot of its [`RamBus`], if that is
    /// newer than the current one. Returns `Ok(true)` if the queue moved.
    ///
    /// This is cheap when nothing changed: a single atomic load compares
    /// generations. Otherwise, before moving, it checks that no reset
    /// happened since the queue was created. If a reset is pending, the
    /// queue stays on its current snapshot so it never follows the bus into
    /// memory that may belong to a subsequent boot. Otherwise, ring pointers
    /// are re-derived from the new snapshot with the saved ring addresses.
    /// The avail and used indices and the deferred chains are kept. Deferred
    /// chains keep pinning the snapshot they were translated from until they
    /// complete.
    ///
    /// If no reset is pending but the rings are not accessible in the latest
    /// snapshot, e.g. the guest unplugged the memory they live in, it returns
    /// an error and the queue stays on its current snapshot. That is memory
    /// safe, since the snapshot keeps the pages alive. The same layout is not
    /// retried, so the error is reported once per layout change.
    ///
    /// A VM reset does not take that path: it clears the queue registers
    /// and bumps the reset counter before it removes guest memory, so the
    /// queue sees the pending reset and returns `Ok(false)` without trying
    /// the new layout, even if the new boot has already re-enabled the queue
    /// at the same addresses.
    pub fn sync_ram(&mut self) -> Result<bool> {
        // The generation must be loaded, with Acquire, before the reset
        // counter is read below. A VM reset bumps the counter before the
        // memory update publishes the new generation with Release, so a
        // queue that observes that generation is guaranteed to also observe
        // the reset, even if the new boot has already re-enabled the queue
        // with the same ring addresses.
        if self.ram_bus.generation() == self.bus_generation {
            return Ok(false);
        }
        let latest = self.ram_bus.load();
        self.bus_generation = latest.generation();
        // A reset is pending: never follow the bus into memory that may
        // already belong to the next boot. Stay on the pinned snapshot.
        if self.reset_pending() {
            return Ok(false);
        }
        if Arc::ptr_eq(&self.ram, &latest) {
            return Ok(false);
        }
        self.q = Q::new(&self.addrs, &latest, self.event_idx)?;
        self.ram = latest;
        Ok(true)
    }

    /// Whether the queue was reset since it was created.
    ///
    /// The reset counter is all that needs comparing: the transports do not
    /// let the driver change the registers of an enabled queue, so the
    /// registers only change through [`QueueReg::reset`], which bumps the
    /// counter. Unlike the ring addresses, the counter also tells a queue
    /// apart from its re-creation by the next boot at the same addresses.
    fn reset_pending(&self) -> bool {
        // A reset may race with this check, so the registers may read as
        // disabled or belong to a later boot. But a consistent snapshot of
        // the boot that created this queue must match the saved addresses.
        debug_assert!(
            RingAddrs::load(self.reg)
                .is_none_or(|a| a.reset_counter != self.addrs.reset_counter || a == self.addrs),
            "queue registers changed without a reset"
        );
        self.reg.reset_counter.load(Ordering::Acquire) != self.addrs.reset_counter
    }

    /// Whether a device reset is pending or the queue stayed on an old
    /// snapshot because its rings are not accessible in the latest layout.
    fn is_stale(&self) -> bool {
        self.reset_pending() || self.ram.generation() != self.bus_generation
    }

    /// Number of chains taken from the ring but not returned yet.
    pub fn num_deferred(&self) -> usize {
        self.deferred.len()
    }

    /// Forgets the chains taken from the ring but not returned yet, and
    /// leaks the memory snapshots they pin.
    ///
    /// This is the last resort for chains whose buffers may still be
    /// accessed, e.g. by the kernel, but cannot be waited for.
    pub fn leak_deferred(&mut self) {
        for (_, chain) in self.deferred.drain() {
            std::mem::forget(chain);
        }
    }

    pub fn desc_avail(&self) -> bool {
        self.q.desc_avail(self.avail)
    }

    fn push_used(&mut self, chain: DescChain, len: u32) {
        self.q.set_used(self.used, chain.id, len);
        self.used = self.q.index_add(self.used, chain.delta);
    }

    pub fn handle_deferred(
        &mut self,
        id: u16,
        q_index: u16,
        irq_sender: &impl IrqSender,
        mut op: impl FnMut(&mut DescChain) -> Result<u32>,
    ) -> Result<()> {
        // `_ram` must stay a named binding: it keeps the snapshot alive while
        // `chain` is in use. `..` or `_ram: _` would drop it right away.
        let Some(DeferredChain { mut chain, _ram }) = self.deferred.remove(&id) else {
            return error::InvalidDescriptor { id }.fail();
        };
        let len = op(&mut chain)?;
        let delta = chain.delta;
        self.push_used(chain, len);
        fence(Ordering::SeqCst);
        if self.q.interrupt_enabled(self.used, delta) {
            irq_sender.queue_irq(q_index);
        }
        Ok(())
    }

    pub fn handle_desc(
        &mut self,
        q_index: u16,
        irq_sender: &impl IrqSender,
        mut op: impl FnMut(&mut DescChain) -> Result<Status>,
    ) -> Result<()> {
        let mut send_irq = false;
        let mut ret = Ok(());
        let mut ram = self.ram.clone();
        'out: loop {
            if !self.q.desc_avail(self.avail) {
                break;
            }
            self.q.enable_notification(false);
            loop {
                let mut chain = match self.q.get_avail(self.avail, &ram) {
                    Ok(Some(chain)) => chain,
                    Ok(None) => break,
                    // The chain may point into memory plugged after the
                    // snapshot was taken, e.g. while the worker was handling
                    // an event that started before the memory was published.
                    // A stale snapshot may also still map memory the guest
                    // has just unplugged. That is memory safe, since the
                    // snapshot keeps the pages alive, and a driver must not
                    // unplug memory it has posted buffers in.
                    Err(e) => {
                        match self.sync_ram() {
                            Ok(true) => {
                                ram = self.ram.clone();
                                continue;
                            }
                            // A device reset is pending, or the rings are
                            // gone from the latest layout, so the queue
                            // cannot follow the driver. Stop here and wait
                            // for the device reset.
                            Err(e) => log::warn!("{:x?}: cannot catch up: {e:?}", self.addrs),
                            Ok(false) if self.is_stale() => {}
                            Ok(false) => ret = Err(e),
                        }
                        // Chains completed so far still need their
                        // interrupt.
                        self.q.enable_notification(true);
                        break 'out;
                    }
                };
                let delta = chain.delta;
                match op(&mut chain) {
                    Err(e) => {
                        ret = Err(e);
                        self.q.enable_notification(true);
                        break 'out;
                    }
                    Ok(Status::Break) => break 'out,
                    Ok(Status::Done { len }) => {
                        self.push_used(chain, len);
                        if !send_irq {
                            fence(Ordering::SeqCst);
                            send_irq = self.q.interrupt_enabled(self.used, delta);
                        }
                    }
                    Ok(Status::Deferred) => {
                        // SAFETY: the chain is stored together with the
                        // snapshot it points into.
                        let chain = unsafe { chain.into_static() };
                        let deferred = DeferredChain {
                            chain,
                            _ram: ram.clone(),
                        };
                        self.deferred.insert(deferred.chain.id, deferred);
                    }
                }
                self.avail = self.q.index_add(self.avail, delta);
            }
            self.q.enable_notification(true);
            fence(Ordering::SeqCst);
        }
        if send_irq {
            irq_sender.queue_irq(q_index);
        }
        ret
    }
}

/// Moves all `queues` to the latest snapshot of their [`RamBus`].
///
/// Workers call this between two events, when they hold no reference to
/// guest memory other than through the queues and their deferred chains.
/// In RCU terms, this is their quiescent state. It is cheap when nothing
/// changed: one atomic load per queue.
///
/// A queue whose rings are not accessible in the latest snapshot, or for
/// which a device reset is pending, stays on its old one, see
/// [`Queue::sync_ram`]. On a VM reboot, for example, all guest memory is
/// removed and re-added before the worker sees the device reset that was
/// queued just before it; the queue refuses to follow the bus into the new
/// boot's memory and stays on its pinned snapshot until reset.
pub fn sync_queues<Q>(queues: &mut [Option<Queue<'_, Q>>])
where
    Q: VirtQueue,
{
    for (index, queue) in queues.iter_mut().enumerate() {
        let Some(queue) = queue else {
            continue;
        };
        let tried = queue.bus_generation;
        match queue.sync_ram() {
            Ok(false) if queue.bus_generation != tried && queue.is_stale() => log::debug!(
                "queue-{index}: device reset pending, staying on guest memory generation {}",
                queue.ram().generation()
            ),
            Ok(false) => {}
            Ok(true) => log::debug!(
                "queue-{index}: moved to guest memory generation {}",
                queue.ram().generation()
            ),
            Err(e) => log::warn!(
                "queue-{index}: rings not accessible in the latest layout, staying on generation {}: {e:?}",
                queue.ram().generation()
            ),
        }
    }
}

pub fn copy_from_reader(mut reader: impl Read) -> impl FnMut(&mut DescChain) -> Result<Status> {
    move |chain| {
        let ret = reader.read_vectored(&mut chain.writable);
        match ret {
            Ok(0) => {
                let size: usize = chain.writable.iter().map(|s| s.len()).sum();
                if size == 0 {
                    Ok(Status::Done { len: 0 })
                } else {
                    Ok(Status::Break)
                }
            }
            Ok(len) => Ok(Status::Done { len: len as u32 }),
            Err(e) if e.kind() == ErrorKind::WouldBlock => Ok(Status::Break),
            Err(e) => Err(e.into()),
        }
    }
}

pub fn copy_to_writer(mut writer: impl Write) -> impl FnMut(&mut DescChain) -> Result<Status> {
    move |chain| {
        let ret = writer.write_vectored(&chain.readable);
        match ret {
            Ok(0) => {
                let size: usize = chain.readable.iter().map(|s| s.len()).sum();
                if size == 0 {
                    Ok(Status::Done { len: 0 })
                } else {
                    Ok(Status::Break)
                }
            }
            Ok(_) => Ok(Status::Done { len: 0 }),
            Err(e) if e.kind() == ErrorKind::WouldBlock => Ok(Status::Break),
            Err(e) => Err(e.into()),
        }
    }
}

#[cfg(test)]
#[path = "queue_test.rs"]
pub(in crate::virtio) mod tests;
