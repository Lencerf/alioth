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

use std::ffi::CString;
use std::fs::OpenOptions;
use std::io::Write;
use std::os::fd::{AsFd, BorrowedFd};
use std::os::unix::fs::OpenOptionsExt;
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU64};

use flexi_logger::Logger;
use mio::Poll;
use rstest::{fixture, rstest};
use tempdir::TempDir;
use zerocopy::IntoBytes;

use crate::ffi;
use crate::hv::IoeventFd;
use crate::virtio::dev::entropy::{EntropyConfig, EntropyFeature, EntropyParam};
use crate::virtio::dev::{DevParam, Virtio};
use crate::virtio::queue::split::{AvailHeader, Desc, DescFlag, UsedElem, UsedHeader};
use crate::virtio::worker::mio::{ActiveMio, VirtioMio};
use crate::virtio::{DeviceId, FEATURE_BUILT_IN, IrqSender, Result};
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

#[derive(Debug, Default)]
struct FakeIrqSender;

impl IrqSender for FakeIrqSender {
    fn queue_irq(&self, idx: u16) {
        log::trace!("queue irq {idx}");
    }

    fn config_irq(&self) {
        unimplemented!()
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

#[derive(Debug, Default)]
struct FakeIoeventFd;

impl AsFd for FakeIoeventFd {
    fn as_fd(&self) -> BorrowedFd<'_> {
        unreachable!()
    }
}

impl IoeventFd for FakeIoeventFd {}

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
    Queue {
        size: AtomicU16::new(QUEUE_SIZE),
        desc: AtomicU64::new(DESC_ADDR),
        driver: AtomicU64::new(AVAIL_ADDR),
        device: AtomicU64::new(USED_ADDR),
        enabled: AtomicBool::new(true),
    }
}

#[rstest]
fn entropy_test(fixutre_ram_bus: RamBus, fixture_queue: Queue) {
    let _ = Logger::try_with_env().unwrap().start().unwrap();
    let ram = fixutre_ram_bus.lock_layout();

    let buf0_addr = DATA_ADDR;
    let buf1_addr = buf0_addr + (4 << 10);
    let buf2_addr = buf1_addr + (4 << 10);
    let desc_0 = Desc {
        addr: buf0_addr,
        len: 4 << 10,
        flag: DescFlag::NEXT.bits(),
        next: 1,
    };
    let desc_1 = Desc {
        addr: buf1_addr,
        len: 4 << 10,
        flag: DescFlag::WRITE.bits(),
        next: 0,
    };
    let desc_2 = Desc {
        addr: buf2_addr,
        len: 4 << 10,
        flag: DescFlag::WRITE.bits(),
        next: 0,
    };
    ram.write(DESC_ADDR, desc_0.as_bytes()).unwrap();
    ram.write(DESC_ADDR + size_of::<Desc>() as u64, desc_1.as_bytes())
        .unwrap();
    ram.write(DESC_ADDR + size_of::<Desc>() as u64 * 2, desc_2.as_bytes())
        .unwrap();

    let temp_dir = TempDir::new("entropy_test").unwrap();
    let pipe_path = temp_dir.path().join("urandom.sock");
    let pipe_path_c = CString::new(pipe_path.as_os_str().as_encoded_bytes()).unwrap();
    ffi!(unsafe { libc::mkfifo(pipe_path_c.as_ptr(), 0o600) }).unwrap();

    let param = EntropyParam {
        source: Some(pipe_path.clone()),
    };
    let mut dev = param.build("entropy").unwrap();

    assert_eq!(dev.id(), DeviceId::Entropy);
    assert_eq!(dev.name(), "entropy");
    assert_eq!(dev.num_queues(), 1);
    assert_eq!(*dev.config(), EntropyConfig);
    assert_eq!(dev.feature(), FEATURE_BUILT_IN);

    let q = SplitQueue::new(&fixture_queue, &*ram, 0).unwrap().unwrap();
    log::info!("used index = {}", q.used_index);
    let mut poll = Poll::new().unwrap();
    let irq_sender = FakeIrqSender::default();
    let q_ioeventfd = FakeIoeventFd::default();
    let mut mio = ActiveMio {
        queues: &mut [Some(q)],
        irq_sender: &irq_sender,
        ioeventfds: &[q_ioeventfd],
        poll: &mut poll,
        mem: &*ram,
    };
    dev.activate(EntropyFeature::all().bits(), &mut mio)
        .unwrap();

    ram.write(
        AVAIL_ADDR + size_of::<AvailHeader>() as u64,
        [0u16, 2u16].as_bytes(),
    )
    .unwrap();

    let set_avail = |idx| {
        let avail_header = AvailHeader { flags: 0, idx };
        ram.write(AVAIL_ADDR, avail_header.as_bytes()).unwrap();
    };
    let get_used_idx = || -> u16 {
        let hdr: UsedHeader = ram.read(USED_ADDR).unwrap();
        hdr.idx
    };
    let get_used = |idx: usize| -> UsedElem {
        let elem: UsedElem = ram
            .read(USED_ADDR + (size_of::<UsedHeader>() + size_of::<UsedElem>() * idx) as u64)
            .unwrap();
        elem
    };

    let mut writer = OpenOptions::new()
        .write(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(&pipe_path)
        .unwrap();

    set_avail(1);
    dev.handle_queue(0, &mut mio).unwrap();

    assert_eq!(mio.queues[0].as_ref().unwrap().avail_index(), 1);

    let s0 = b"Hello, World!";
    writer.write_all(s0).unwrap();
    writer.flush().unwrap();
    log::info!("write: {s0:?}");
    dev.handle_queue(0, &mut mio).unwrap();

    assert_eq!(mio.queues[0].as_ref().unwrap().avail_index(), 1);
    assert_eq!(get_used_idx(), 1);
    assert_eq!(
        get_used(0),
        UsedElem {
            id: 0,
            len: s0.len() as u32
        }
    );

    let s1 = b"Goodbye, World!";
    writer.write_all(s1).unwrap();
    writer.flush().unwrap();
    log::info!("write: {s1:?}");
    set_avail(2);
    dev.handle_queue(0, &mut mio).unwrap();

    assert_eq!(mio.queues[0].as_ref().unwrap().avail_index(), 2);
    assert_eq!(get_used_idx(), 2);
    assert_eq!(
        get_used(1),
        UsedElem {
            id: 2,
            len: s1.len() as u32
        }
    );
}
