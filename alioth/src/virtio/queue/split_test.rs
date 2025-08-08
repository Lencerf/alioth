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

use std::ptr::eq as ptr_eq;
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU64, Ordering};

use assert_matches::assert_matches;
use rstest::{fixture, rstest};
use zerocopy::IntoBytes;

use crate::virtio::VirtioFeature;
use crate::virtio::queue::VirtQueue;
use crate::virtio::queue::split::{AvailHeader, Desc, DescFlag};
use crate::{
    mem::mapped::{ArcMemPages, RamBus},
    virtio::queue::{QUEUE_SIZE_MAX, Queue, split::SplitQueue},
};

const MEM_SIZE: usize = 2 << 20;
const QUEUE_SIZE: u16 = QUEUE_SIZE_MAX;
const DESC_ADDR: u64 = 0x1000;
const AVAIL_ADDR: u64 = 0x2000;
const USED_ADDR: u64 = 0x3000;
const DATA_ADDR: u64 = 0x4000;

#[fixture]
fn fixutre_ram_bus() -> RamBus {
    println!("fixutre_ram_bus done");
    let host_pages = ArcMemPages::from_anonymous(MEM_SIZE, None, None).unwrap();
    let ram_bus = RamBus::new();
    ram_bus.add(0, host_pages).unwrap();
    ram_bus
}

#[fixture]
fn fixture_queue() -> Queue {
    println!("fixture_queue done");
    Queue {
        size: AtomicU16::new(QUEUE_SIZE),
        desc: AtomicU64::new(DESC_ADDR),
        driver: AtomicU64::new(AVAIL_ADDR),
        device: AtomicU64::new(USED_ADDR),
        enabled: AtomicBool::new(true),
    }
}

#[rstest]
fn disabled_queue(fixutre_ram_bus: RamBus, fixture_queue: Queue) {
    let ram = fixutre_ram_bus.lock_layout();
    fixture_queue.enabled.store(false, Ordering::Relaxed);
    let split_queue = SplitQueue::new(&fixture_queue, &*ram, 0);
    assert_matches!(split_queue, Ok(None));
}

#[rstest]
fn enabled_queue(fixutre_ram_bus: RamBus, fixture_queue: Queue) {
    let ram = fixutre_ram_bus.lock_layout();
    let split_queue = SplitQueue::new(&fixture_queue, &*ram, 0).unwrap().unwrap();
    assert!(ptr_eq(split_queue.reg(), &fixture_queue));
    assert_eq!(split_queue.size(), QUEUE_SIZE);

    let str1 = "Hello, World!";
    let str2 = "Goodbye, World!";
    let str1_addr = DATA_ADDR;
    let str2_addr = str1_addr + str1.len() as u64;
    ram.write(str1_addr, str1.as_bytes()).unwrap();
    ram.write(str2_addr, str2.as_bytes()).unwrap();
    let desc_1 = Desc {
        addr: str1_addr,
        len: str1.len() as u32,
        flag: DescFlag::NEXT.bits(),
        next: 1,
    };
    let desc_2 = Desc {
        addr: str2_addr,
        len: str2.len() as u32,
        flag: 0,
        next: 0,
    };
    ram.write(DESC_ADDR, desc_1.as_bytes()).unwrap();
    ram.write(DESC_ADDR + size_of::<Desc>() as u64, desc_2.as_bytes())
        .unwrap();
    let avail_header = AvailHeader { flags: 0, idx: 1 };
    ram.write(AVAIL_ADDR, avail_header.as_bytes()).unwrap();
    ram.write(
        AVAIL_ADDR + size_of::<AvailHeader>() as u64,
        0u16.as_bytes(),
    )
    .unwrap();

    assert_eq!(split_queue.avail_index(), 1);
    assert_eq!(split_queue.read_avail(0), 0);
    assert_eq!(*split_queue.get_desc(0).unwrap(), desc_1);
    assert_eq!(*split_queue.get_desc(1).unwrap(), desc_2);
    let desc = split_queue.next_desc().unwrap().unwrap();
    assert_eq!(desc.id, 0);
    assert_eq!(&*desc.readable[0], str1.as_bytes());
    assert_eq!(&*desc.readable[1], str2.as_bytes());
    assert_eq!(desc.writable.len(), 0);
}

#[rstest]
fn event_idx_enabled(fixutre_ram_bus: RamBus, fixture_queue: Queue) {
    let ram = fixutre_ram_bus.lock_layout();
    let q = SplitQueue::new(&fixture_queue, &*ram, VirtioFeature::EVENT_IDX.bits())
        .unwrap()
        .unwrap();
    unsafe { *q.used_event.unwrap() = 1 };
    assert_eq!(q.used_event(), Some(1));

    assert_eq!(q.set_avail_event(12), Some(()));
    assert_eq!(unsafe { *q.avail_event.unwrap() }, 12);
}
