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

use std::io::Write;
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use zerocopy::IntoBytes;

use crate::mem::mapped::RamBus;
use crate::virtio::DevStatus;
use crate::virtio::dev::entropy::EntropySpec;
use crate::virtio::dev::{DevSpec, VirtioDevice};
use crate::virtio::vu::backend::VuBackend;
use crate::virtio::vu::bindings::{Message, MessageFlag, VuFrontMsg};

#[test]
fn test_vu_backend_reset_clears_queue_regs() {
    let (conn1, mut conn2) = UnixStream::pair().unwrap();
    let memory = Arc::new(RamBus::new());
    let entropy = EntropySpec::default().build("test-entropy").unwrap();
    let dev = VirtioDevice::new("test-entropy", entropy, memory.clone(), false).unwrap();
    let queue_regs = dev.queue_regs.clone();
    let mut backend = VuBackend::new(conn1, dev, memory).unwrap();

    queue_regs[0].enabled.store(true, Ordering::Release);
    queue_regs[0].desc.store(0x1000, Ordering::Release);
    assert_eq!(queue_regs[0].reset_counter.load(Ordering::Acquire), 0);

    let status: u64 = 0;
    conn2.write_all(status.as_bytes()).unwrap();
    let mut msg = Message {
        request: VuFrontMsg::SET_STATUS.into(),
        flag: MessageFlag::default(),
        size: 8,
    };
    let mut fds = [const { None }; 8];
    backend.handle_msg(&mut msg, &mut fds).unwrap();

    assert!(!queue_regs[0].enabled.load(Ordering::Acquire));
    assert_eq!(queue_regs[0].desc.load(Ordering::Acquire), 0);
    assert_eq!(queue_regs[0].reset_counter.load(Ordering::Acquire), 1);
}

#[test]
fn test_vu_backend_reset_from_driver_ok() {
    let (conn1, mut conn2) = UnixStream::pair().unwrap();
    let memory = Arc::new(RamBus::new());
    let entropy = EntropySpec::default().build("test-entropy").unwrap();
    let dev = VirtioDevice::new("test-entropy", entropy, memory.clone(), false).unwrap();
    let queue_regs = dev.queue_regs.clone();
    let mut backend = VuBackend::new(conn1, dev, memory).unwrap();
    backend.status = DevStatus::DRIVER_OK;

    queue_regs[0].enabled.store(true, Ordering::Release);
    queue_regs[0].desc.store(0x1000, Ordering::Release);

    let status: u64 = 0;
    conn2.write_all(status.as_bytes()).unwrap();
    let mut msg = Message {
        request: VuFrontMsg::SET_STATUS.into(),
        flag: MessageFlag::default(),
        size: 8,
    };
    let mut fds = [const { None }; 8];
    backend.handle_msg(&mut msg, &mut fds).unwrap();

    assert!(!queue_regs[0].enabled.load(Ordering::Acquire));
    assert_eq!(queue_regs[0].desc.load(Ordering::Acquire), 0);
    assert_eq!(queue_regs[0].reset_counter.load(Ordering::Acquire), 1);
    assert_eq!(backend.status, DevStatus::empty());
}
