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

#[cfg(test)]
#[path = "queue_test.rs"]
mod tests;

pub mod split;

use std::collections::HashMap;
use std::io::{ErrorKind, IoSlice, IoSliceMut, Read, Write};
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU64, Ordering, fence};

use crate::virtio::{IrqSender, Result, error};

pub const QUEUE_SIZE_MAX: u16 = 256;

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
    count: u16,
    pub readable: Vec<IoSlice<'m>>,
    pub writable: Vec<IoSliceMut<'m>>,
}

impl DescChain<'_> {
    pub fn count(&self) -> u16 {
        self.count
    }
}

pub trait VirtQueue<'m> {
    fn reg(&self) -> &QueueReg;
    fn desc_avail(&self, index: u16) -> bool;
    fn get_desc_chain(&self, index: u16) -> Result<Option<DescChain<'m>>>;
    fn push_used(&self, desc: DescChain, len: u32);
    fn enable_notification(&self, enabled: bool);
    fn interrupt_enabled(&self, desc: &DescChain) -> bool;
    fn wrapping_add(&self, index: u16, count: u16) -> u16;
}

pub enum Status {
    Done { len: u32 },
    Deferred,
    Break,
}

pub struct Queue<'m, Q> {
    q: Q,
    desc_index: u16,
    pending: HashMap<u16, DescChain<'m>>,
}

impl<'m, Q> Queue<'m, Q> {
    pub fn new(q: Q) -> Self {
        Self {
            q,
            desc_index: 0,
            pending: HashMap::new(),
        }
    }
}

impl<'m, Q> Queue<'m, Q>
where
    Q: VirtQueue<'m>,
{
    pub fn reg(&self) -> &QueueReg {
        self.q.reg()
    }

    pub fn handle_pending(
        &mut self,
        index: u16,
        q_index: u16,
        irq_sender: &impl IrqSender,
        mut op: impl FnMut(&mut DescChain) -> Result<u32>,
    ) -> Result<()> {
        let Some(mut desc) = self.pending.remove(&index) else {
            return error::InvalidDescriptor { id: index }.fail();
        };
        let need_irq = self.q.interrupt_enabled(&desc);
        let len = op(&mut desc)?;
        self.q.push_used(desc, len);
        if need_irq {
            irq_sender.queue_irq(q_index);
        }
        Ok(())
    }

    pub fn handle_desc(
        &mut self,
        q_index: u16,
        irq_sender: &impl IrqSender,
        mut op: impl FnMut(u16, &mut DescChain) -> Result<Status>,
    ) -> Result<()> {
        let mut need_irq = false;
        let mut ret = Ok(());
        'out: loop {
            if !self.q.desc_avail(self.desc_index) {
                break;
            }
            self.q.enable_notification(false);
            while let Some(mut desc) = self.q.get_desc_chain(self.desc_index)? {
                let desc_count = desc.count;
                match op(self.desc_index, &mut desc) {
                    Err(e) => {
                        ret = Err(e);
                        self.q.enable_notification(true);
                        break 'out;
                    }
                    Ok(Status::Break) => break 'out,
                    Ok(Status::Done { len }) => {
                        need_irq = need_irq || self.q.interrupt_enabled(&desc);
                        self.q.push_used(desc, len);
                    }
                    Ok(Status::Deferred) => {
                        self.pending.insert(self.desc_index, desc);
                    }
                }
                self.desc_index = self.q.wrapping_add(self.desc_index, desc_count);
            }
            self.q.enable_notification(true);
            fence(Ordering::SeqCst);
        }
        if need_irq {
            fence(Ordering::SeqCst);
            irq_sender.queue_irq(q_index);
        }
        ret
    }

    pub fn copy_from_reader(
        &mut self,
        q_index: u16,
        irq_sender: &impl IrqSender,
        mut reader: impl Read,
    ) -> Result<()> {
        self.handle_desc(q_index, irq_sender, |_, desc| {
            let ret = reader.read_vectored(&mut desc.writable);
            match ret {
                Ok(0) => {
                    let size: usize = desc.writable.iter().map(|s| s.len()).sum();
                    if size == 0 {
                        Ok(Status::Done { len: 0 })
                    } else {
                        Ok(Status::Break)
                    }
                }
                Ok(len) => Ok(Status::Done { len: len as u32 }),
                Err(e) if e.kind() == ErrorKind::WouldBlock => Ok(Status::Break),
                Err(e) => Err(e)?,
            }
        })
    }

    pub fn copy_to_writer(
        &mut self,
        q_index: u16,
        irq_sender: &impl IrqSender,
        mut writer: impl Write,
    ) -> Result<()> {
        self.handle_desc(q_index, irq_sender, |_, desc| {
            let ret = writer.write_vectored(&desc.readable);
            match ret {
                Ok(0) => {
                    let size: usize = desc.readable.iter().map(|s| s.len()).sum();
                    if size == 0 {
                        Ok(Status::Done { len: 0 })
                    } else {
                        Ok(Status::Break)
                    }
                }
                Ok(len) => Ok(Status::Done { len: len as u32 }),
                Err(e) if e.kind() == ErrorKind::WouldBlock => Ok(Status::Break),
                Err(e) => Err(e)?,
            }
        })
    }
}
