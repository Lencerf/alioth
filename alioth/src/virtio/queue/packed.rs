// Copyright 2025 Google LLC
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

use std::sync::atomic::Ordering;

use bitfield::bitfield;
use zerocopy::{FromBytes, Immutable, IntoBytes};

use crate::{
    mem::mapped::Ram,
    virtio::{
        Result, VirtioFeature,
        queue::{DescFlag, Descriptor, QueueReg, VirtQueue},
    },
};

#[path = "packed_test.rs"]
#[cfg(test)]
mod tests;

#[repr(C, align(16))]
#[derive(Debug, Clone, Default, FromBytes, Immutable, IntoBytes)]
struct Desc {
    /// Buffer Address
    pub addr: u64,
    /// Buffer Length
    pub len: u32,
    /// Buffer ID
    pub id: u16,
    /// The flags depending on descriptor type
    pub flag: u16,
}

bitfield! {
    #[derive(Copy, Clone, Default, PartialEq, Eq, Hash)]
    pub struct DescEvent(u32);
    impl Debug;
    pub u16, offset, set_offset : 14, 0;
    pub wrap, set_warp: 15;
    pub disabled, set_disabled : 16;
    pub enable_desc, set_enable_desc : 17;
}

#[derive(Debug)]
pub struct PackedQueue<'q, 'm> {
    reg: &'q QueueReg,
    ram: &'m Ram,
    size: u16,
    wrap_counter: bool,
    used_index: u16,
    desc: *mut Desc,
    enable_event_idx: bool,
    notification: *mut DescEvent,
    interrupt: *mut DescEvent,
}

impl<'q, 'm> PackedQueue<'q, 'm> {
    pub fn new(
        reg: &'q QueueReg,
        ram: &'m Ram,
        feature: u64,
    ) -> Result<Option<PackedQueue<'q, 'm>>> {
        if !reg.enabled.load(Ordering::Acquire) {
            return Ok(None);
        }
        let size = reg.size.load(Ordering::Acquire);
        let feature = VirtioFeature::from_bits_retain(feature);
        let desc = reg.desc.load(Ordering::Acquire);
        let notification: *mut DescEvent = ram.get_ptr(reg.device.load(Ordering::Acquire))?;
        unsafe {
            (&mut *notification).set_disabled(false);
            (&mut *notification).set_enable_desc(false);
        }
        Ok(Some(PackedQueue {
            reg,
            ram,
            size,
            wrap_counter: true,
            used_index: 0,
            desc: ram.get_ptr(desc)?,
            enable_event_idx: feature.contains(VirtioFeature::EVENT_IDX),
            notification,
            interrupt: ram.get_ptr(reg.driver.load(Ordering::Acquire))?,
        }))
    }

    fn get_next_desc(&self) -> Result<Option<Descriptor<'m>>> {
        if !self.has_next_desc() {
            return Ok(None);
        }
        log::trace!("get_next_desc: start, avail_index={}", self.used_index);
        let mut readable = Vec::new();
        let mut writeable = Vec::new();
        let mut index = self.used_index;
        let mut count = 0;
        let id = loop {
            let desc = unsafe { &*self.desc.offset(index as isize) };
            let flag = DescFlag::from_bits_retain(desc.flag);
            if flag.contains(DescFlag::INDIRECT) {
                todo!()
            }
            if flag.contains(DescFlag::WRITE) {
                writeable.push((desc.addr, desc.len as u64));
            } else {
                readable.push((desc.addr, desc.len as u64));
            }
            count += 1;
            if !flag.contains(DescFlag::NEXT) {
                break desc.id;
            }
            index = (index + 1) % self.size;
        };
        log::trace!("get desc desc: id={id}, avail_span={count}");
        Ok(Some(Descriptor {
            id,
            index: self.used_index,
            count,
            readable: self.ram.translate_iov(&readable)?,
            writable: self.ram.translate_iov_mut(&writeable)?,
        }))
    }

    fn flag_is_avail(&self, flag: DescFlag) -> bool {
        flag.contains(DescFlag::AVAIL) == self.wrap_counter
            && flag.contains(DescFlag::USED) != self.wrap_counter
    }

    fn set_flag_used(&self, flag: &mut DescFlag) {
        if self.wrap_counter {
            flag.insert(DescFlag::USED);
            flag.insert(DescFlag::AVAIL);
        } else {
            flag.remove(DescFlag::USED);
            flag.remove(DescFlag::AVAIL);
        }
    }
}

impl<'m> VirtQueue for PackedQueue<'_, 'm> {
    fn reg(&self) -> &QueueReg {
        self.reg
    }

    fn size(&self) -> u16 {
        self.size
    }

    fn next_desc(&self) -> Option<Result<Descriptor<'m>>> {
        self.get_next_desc().transpose()
    }

    fn has_next_desc(&self) -> bool {
        let desc = unsafe { &*self.desc.offset(self.used_index as isize) };
        let flag = DescFlag::from_bits_retain(desc.flag);
        self.flag_is_avail(flag)
    }

    fn desc_available(&self, index: u16) -> bool {
        let index = index % self.size;
        self.flag_is_avail(DescFlag::from_bits_retain(
            unsafe { &*self.desc.offset(index as isize) }.flag,
        ))
    }

    fn get_descriptor(&self, index: u16) -> Result<Descriptor<'m>> {
        unimplemented!()
    }

    fn push_used(&mut self, desc: Descriptor, len: usize) {
        assert_eq!(desc.index, self.used_index);
        log::trace!(
            "push used: id={}, avail_span={}, avail_index={}",
            desc.id,
            desc.index,
            self.used_index
        );
        let first = unsafe { &mut *self.desc.offset(self.used_index as isize) };
        let mut flag = DescFlag::from_bits_retain(first.flag);
        self.set_flag_used(&mut flag);
        first.flag = flag.bits();
        first.id = desc.id;
        first.len = len as u32;
        self.used_index += desc.count;
        if self.used_index >= self.size {
            self.used_index -= self.size;
            self.wrap_counter = !self.wrap_counter;
        }
    }

    fn enable_notification(&self, enabled: bool) {
        unsafe {
            (&mut *self.notification).set_disabled(!enabled);
        }
    }

    fn interrupt_enabled(&self, desc: &Descriptor) -> bool {
        let interrupt = unsafe { &*self.interrupt };
        let r = if self.enable_event_idx && interrupt.enable_desc() {
            let base = self.used_index;
            let end = base + desc.count;
            let mut offset = interrupt.offset();
            if interrupt.wrap() != self.wrap_counter {
                offset += self.size;
            }
            base <= offset && offset < end
            // let target =
            //     interrupt.offset() + (((interrupt.wrap() == self.wrap_counter) as u16) << 15);
            // log::info!("interrupt_enabled: {interrupt:?}");
            // self.used_index == interrupt.offset() && self.wrap_counter == interrupt.wrap()
        } else {
            !interrupt.disabled()
        };
        log::error!("interrupt_enabled: {r}");
        r
    }
}
