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

pub mod packed;
pub mod split;

use std::io::{ErrorKind, IoSlice, IoSliceMut, Read, Write};
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU64, Ordering, fence};

use bitflags::bitflags;

use crate::virtio::{IrqSender, Result};

pub const QUEUE_SIZE_MAX: u16 = 256;

bitflags! {
    #[derive(Default, Debug, Clone, Copy, PartialEq, Eq, Hash)]
    pub struct DescFlag: u16 {
        const NEXT = 1;
        const WRITE = 2;
        const INDIRECT = 4;
        const AVAIL = 1 << 7;
        const USED = 1 << 15;
    }
}

#[derive(Debug, Default)]
pub struct Queue {
    pub size: AtomicU16,
    pub desc: AtomicU64,
    pub driver: AtomicU64,
    pub device: AtomicU64,
    pub enabled: AtomicBool,
}

#[derive(Debug)]
pub struct Descriptor<'m> {
    id: u16,
    index: u16,
    count: u16,
    pub readable: Vec<IoSlice<'m>>,
    pub writable: Vec<IoSliceMut<'m>>,
}

impl Descriptor<'_> {
    pub fn count(&self) -> u16 {
        self.count
    }
}

pub trait VirtQueue<'m> {
    fn reg(&self) -> &Queue;
    fn size(&self) -> u16;
    fn next_desc(&self) -> Option<Result<Descriptor<'m>>>;
    fn desc_available(&self, index: u16) -> bool;
    fn get_descriptor(&self, index: u16) -> Result<Descriptor<'m>>;
    fn has_next_desc(&self) -> bool;
    fn push_used(&mut self, desc: Descriptor, len: usize);
    fn enable_notification(&self, enabled: bool);
    fn interrupt_enabled(&self, desc: &Descriptor) -> bool;

    fn handle_desc(
        &mut self,
        q_index: u16,
        irq_sender: &impl IrqSender,
        mut op: impl FnMut(&mut Descriptor) -> Result<Option<usize>>,
    ) -> Result<()> {
        let mut send_irq = false;
        let mut ret = Ok(());
        'out: loop {
            if !self.has_next_desc() {
                break;
            }
            self.enable_notification(false);
            while let Some(desc) = self.next_desc() {
                let mut desc = desc?;
                match op(&mut desc) {
                    Err(e) => {
                        ret = Err(e);
                        self.enable_notification(true);
                        break 'out;
                    }
                    Ok(None) => break 'out,
                    Ok(Some(len)) => {
                        send_irq = send_irq || self.interrupt_enabled(&desc);
                        self.push_used(desc, len);
                    }
                }
            }
            self.enable_notification(true);
            fence(Ordering::SeqCst);
        }
        if send_irq {
            fence(Ordering::SeqCst);
            irq_sender.queue_irq(q_index)
        }
        ret
    }

    fn copy_from_reader(
        &mut self,
        q_index: u16,
        irq_sender: &impl IrqSender,
        mut reader: impl Read,
    ) -> Result<()> {
        self.handle_desc(q_index, irq_sender, |desc| {
            let ret = reader.read_vectored(&mut desc.writable);
            log::info!("RX: {q_index}: {ret:?}");
            match ret {
                Ok(0) => {
                    let size: usize = desc.writable.iter().map(|s| s.len()).sum();
                    if size == 0 { Ok(Some(0)) } else { Ok(None) }
                }
                Ok(len) => Ok(Some(len)),
                Err(e) if e.kind() == ErrorKind::WouldBlock => Ok(None),
                Err(e) => Err(e)?,
            }
        })
    }

    fn copy_to_writer(
        &mut self,
        q_index: u16,
        irq_sender: &impl IrqSender,
        mut writer: impl Write,
    ) -> Result<()> {
        self.handle_desc(q_index, irq_sender, |desc| {
            let ret = writer.write_vectored(&desc.readable);
            log::info!("TX: {q_index}: {ret:?}");
            match ret {
                Ok(0) => {
                    let size: usize = desc.readable.iter().map(|s| s.len()).sum();
                    if size == 0 { Ok(Some(0)) } else { Ok(None) }
                }
                Ok(len) => Ok(Some(len)),
                Err(e) if e.kind() == ErrorKind::WouldBlock => Ok(None),
                Err(e) => Err(e)?,
            }
        })
    }
}
