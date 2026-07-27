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

use std::io::{Read, Write};
use std::mem::size_of;
use std::sync::Arc;

use assert_matches::assert_matches;
use libc::{PROT_READ, PROT_WRITE};
use zerocopy::{FromBytes, Immutable, IntoBytes};

use super::{ArcMemPages, RamBus};

#[derive(Debug, IntoBytes, FromBytes, Immutable, PartialEq, Eq)]
#[repr(C)]
struct MyStruct {
    data: [u32; 8],
}

const PAGE_SIZE: u64 = 1 << 12;

#[test]
fn test_ram_bus_read() {
    let bus = RamBus::new();
    let prot = PROT_READ | PROT_WRITE;
    let mem1 = ArcMemPages::from_anonymous(PAGE_SIZE as usize, Some(prot), None).unwrap();
    let mem2 = ArcMemPages::from_anonymous(PAGE_SIZE as usize, Some(prot), None).unwrap();

    let add_to_bus = |gpa: u64, mem: ArcMemPages| {
        let mut guard = bus.ram.write();
        let mut ram = (**guard).clone();
        ram.add(gpa, mem).unwrap();
        *guard = Arc::new(ram);
    };

    if mem1.addr > mem2.addr {
        add_to_bus(0x0, mem1);
        add_to_bus(PAGE_SIZE, mem2);
    } else {
        add_to_bus(0x0, mem2);
        add_to_bus(PAGE_SIZE, mem1);
    }

    let data = MyStruct {
        data: [1, 2, 3, 4, 5, 6, 7, 8],
    };
    let data_size = size_of::<MyStruct>() as u64;
    for gpa in (PAGE_SIZE - data_size)..=PAGE_SIZE {
        let ram = bus.ram.read().clone();
        ram.write_t(gpa, &data).unwrap();
        let r: MyStruct = ram.read_t(gpa).unwrap();
        assert_eq!(r, data)
    }
    let memory_end = PAGE_SIZE * 2;
    for gpa in (memory_end - data_size - 10)..=(memory_end - data_size) {
        let ram = bus.ram.read().clone();
        ram.write_t(gpa, &data).unwrap();
        let r: MyStruct = ram.read_t(gpa).unwrap();
        assert_eq!(r, data)
    }
    for gpa in (memory_end - data_size + 1)..memory_end {
        let ram = bus.ram.read().clone();
        assert_matches!(ram.write_t(gpa, &data), Err(_));
        assert_matches!(ram.read_t::<MyStruct>(gpa), Err(_));
    }

    let data: Vec<u8> = (0..64).collect();
    for gpa in (PAGE_SIZE - 64)..=PAGE_SIZE {
        let ram = bus.ram.read().clone();
        ram.write_range(gpa, 64, &*data).unwrap();
        let mut buf = Vec::new();
        ram.read_range(gpa, 64, &mut buf).unwrap();
        assert_eq!(data, buf)
    }

    let guest_iov = [(0, 16), (PAGE_SIZE - 16, 32), (2 * PAGE_SIZE - 16, 16)];
    let ram = bus.ram.read().clone();
    let write_ret = ram.write_vectored(&guest_iov, |iov| {
        assert_eq!(iov.len(), 4);
        (&*data).read_vectored(iov)
    });
    assert_matches!(write_ret, Ok(Ok(64)));
    let mut buf_read = Vec::new();
    let read_ret = ram.read_vectored(&guest_iov, |iov| {
        assert_eq!(iov.len(), 4);
        buf_read.write_vectored(iov)
    });
    assert_matches!(read_ret, Ok(Ok(64)));

    let bufs = ram.translate_iov(&guest_iov).unwrap();
    println!("{bufs:?}");

    let mut guard = bus.ram.write();
    let mut ram_to_remove = (**guard).clone();
    ram_to_remove.remove(0x0).unwrap();
    *guard = Arc::new(ram_to_remove);
}
