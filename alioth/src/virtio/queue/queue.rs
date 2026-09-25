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
use crate::mem::mapped::Ram;
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
}

#[derive(Debug)]
pub struct DescChain<'m> {
    id: u16,
    delta: u16,
    pub readable: Vec<IoSlice<'m>>,
    pub writable: Vec<IoSliceMut<'m>>,
}

impl DescChain<'_> {
    pub fn id(&self) -> u16 {
        self.id
    }

    /// Erases the lifetime of the guest memory the chain points to.
    ///
    /// # Safety
    ///
    /// The caller must keep the memory referenced by the chain alive, e.g.
    /// by holding the [`Ram`] snapshot it was translated from, until the
    /// returned chain is dropped.
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
    /// Creates the queue from the registers set by the driver.
    ///
    /// The queue may cache host pointers into `ram`. The caller must keep
    /// `ram` alive for as long as the queue is in use.
    fn new(reg: &QueueReg, ram: &Ram, event_idx: bool) -> Result<Option<Self>>;
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
/// snapshot, which allows [`Queue::update_ram`] to move the queue to a newer
/// snapshot without disturbing the driver.
pub struct Queue<'r, Q>
where
    Q: VirtQueue,
{
    q: Q,
    avail: Q::Index,
    used: Q::Index,
    reg: &'r QueueReg,
    event_idx: bool,
    deferred: HashMap<u16, DeferredChain>,
    // Must be declared after `q` so it is dropped last.
    ram: Arc<Ram>,
}

impl<'r, Q> Queue<'r, Q>
where
    Q: VirtQueue,
{
    pub fn new(reg: &'r QueueReg, ram: Arc<Ram>, event_idx: bool) -> Result<Option<Self>> {
        let Some(q) = Q::new(reg, &ram, event_idx)? else {
            return Ok(None);
        };
        Ok(Some(Self {
            q,
            avail: Q::INIT_INDEX,
            used: Q::INIT_INDEX,
            reg,
            event_idx,
            deferred: HashMap::new(),
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

    /// Moves the queue to a newer memory snapshot.
    ///
    /// The ring pointers are re-derived from `ram`, while the avail and used
    /// indices and the deferred chains are kept. Deferred chains keep
    /// pinning the snapshot they were translated from until they complete.
    pub fn update_ram(&mut self, ram: Arc<Ram>) -> Result<()> {
        if Arc::ptr_eq(&self.ram, &ram) {
            return Ok(());
        }
        let Some(q) = Q::new(self.reg, &ram, self.event_idx)? else {
            return error::QueueDisabled.fail();
        };
        self.q = q;
        self.ram = ram;
        Ok(())
    }

    /// Number of chains taken from the ring but not returned yet.
    pub fn num_deferred(&self) -> usize {
        self.deferred.len()
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
        let Some(DeferredChain { mut chain, _ram }) = self.deferred.remove(&id) else {
            return error::InvalidDescriptor { id }.fail();
        };
        let len = op(&mut chain)?;
        let delta = chain.delta;
        self.push_used(chain, len);
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
        let ram = self.ram.clone();
        'out: loop {
            if !self.q.desc_avail(self.avail) {
                break;
            }
            self.q.enable_notification(false);
            while let Some(mut chain) = self.q.get_avail(self.avail, &ram)? {
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
                        send_irq = send_irq || self.q.interrupt_enabled(self.used, delta);
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
            fence(Ordering::SeqCst);
            irq_sender.queue_irq(q_index);
        }
        ret
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
