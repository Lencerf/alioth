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
use std::os::unix::fs::OpenOptionsExt;
use std::sync::Arc;
use std::time::Duration;

use assert_matches::assert_matches;
use flume::TryRecvError;
use tempfile::TempDir;

use crate::ffi;
use crate::mem::emulated::{Action, Mmio};
use crate::mem::mapped::ArcMemPages;
use crate::virtio::dev::entropy::{EntropyConfig, EntropySpec};
use crate::virtio::dev::{DevSpec, StartParam, Virtio, WakeEvent};
use crate::virtio::queue::QueueReg;
use crate::virtio::queue::split::SplitQueue;
use crate::virtio::queue::tests::GuestQueue;
use crate::virtio::tests::{
    DATA_ADDR, FakeIoeventFd, FakeIrqSender, fixture_queues, fixture_ram_bus,
};
use crate::virtio::{DeviceId, FEATURE_BUILT_IN, VirtioFeature};

#[test]
fn entry_config_test() {
    let config = EntropyConfig;

    assert_eq!(config.size(), 0);
    assert_matches!(config.read(0, 1), Ok(0));
    assert_matches!(config.write(0, 1, 0), Ok(Action::None));
}

#[test]
fn entropy_test() {
    let ram_bus = Arc::new(fixture_ram_bus());
    let ram = ram_bus.ram.read();
    let regs: Arc<[QueueReg]> = Arc::from(fixture_queues(1));

    let mut guest_q = GuestQueue::new(
        SplitQueue::new(&regs[0], &ram, false).unwrap().unwrap(),
        &regs[0],
    );

    let buf0_addr = DATA_ADDR;
    let buf1_addr = buf0_addr + (4 << 10);
    let s0 = "Hello, World!";
    let s1 = "Goodbye, World!";

    let temp_dir = TempDir::new().unwrap();
    let pipe_path = temp_dir.path().join("urandom");
    let pipe_path_c = CString::new(pipe_path.as_os_str().as_encoded_bytes()).unwrap();
    ffi!(unsafe { libc::mkfifo(pipe_path_c.as_ptr(), 0o600) }).unwrap();

    let param = EntropySpec {
        source: Some(pipe_path.clone().into()),
    };
    let dev = param.build("entropy").unwrap();

    assert_matches!(dev.id(), DeviceId::ENTROPY);
    assert_eq!(dev.name(), "entropy");
    assert_eq!(dev.num_queues(), 1);
    assert_matches!(*dev.config(), EntropyConfig);
    assert_eq!(dev.feature(), FEATURE_BUILT_IN);

    let (tx, rx) = flume::unbounded();
    let (handle, notifier) = dev.spawn_worker(rx, ram_bus.clone(), regs).unwrap();
    let (irq_tx, irq_rx) = flume::unbounded();
    let irq_sender = Arc::new(FakeIrqSender { q_tx: irq_tx });
    let start_param = StartParam {
        feature: VirtioFeature::VERSION_1.bits(),
        irq_sender,
        ioeventfds: Option::<Arc<[FakeIoeventFd]>>::None,
    };
    tx.send(WakeEvent::Start { param: start_param }).unwrap();
    notifier.notify().unwrap();

    let mut writer = OpenOptions::new()
        .write(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(&pipe_path)
        .unwrap();

    let id0 = guest_q.add_desc(&[], &[(buf0_addr, 4 << 10)]);
    tx.send(WakeEvent::Notify { q_index: 0 }).unwrap();
    notifier.notify().unwrap();
    assert_eq!(irq_rx.try_recv(), Err(TryRecvError::Empty));

    writer.write_all(s0.as_bytes()).unwrap();
    writer.flush().unwrap();
    tx.send(WakeEvent::Notify { q_index: 0 }).unwrap();
    notifier.notify().unwrap();
    assert_eq!(irq_rx.recv_timeout(Duration::from_secs(1)).unwrap(), 0);
    let used0 = guest_q.get_used().unwrap();
    assert_eq!(used0.id, id0);
    assert_eq!(used0.len, s0.len() as u32);

    writer.write_all(s1.as_bytes()).unwrap();
    writer.flush().unwrap();
    let id1 = guest_q.add_desc(&[], &[(buf1_addr, 4 << 10)]);
    tx.send(WakeEvent::Notify { q_index: 0 }).unwrap();
    notifier.notify().unwrap();
    assert_eq!(irq_rx.recv_timeout(Duration::from_secs(1)).unwrap(), 0);

    let used1 = guest_q.get_used().unwrap();
    assert_eq!(used1.id, id1);
    assert_eq!(used1.len, s1.len() as u32);

    tx.send(WakeEvent::Shutdown).unwrap();
    notifier.notify().unwrap();
    handle.join().unwrap();

    for (s, addr) in [(s0, buf0_addr), (s1, buf1_addr)] {
        let mut buf = vec![0u8; s.len()];
        ram.read(addr, &mut buf).unwrap();
        assert_eq!(String::from_utf8_lossy(buf.as_slice()), s);
    }
}

#[test]
fn entropy_memory_update_test() {
    let ram_bus = Arc::new(fixture_ram_bus());
    let ram = ram_bus.ram.read().clone();
    let regs: Arc<[QueueReg]> = Arc::from(fixture_queues(1));

    let mut guest_q = GuestQueue::new(
        SplitQueue::new(&regs[0], &ram, false).unwrap().unwrap(),
        &regs[0],
    );

    let buf0_addr = DATA_ADDR;
    let new_gpa = 0x200_000;
    let new_buf_addr = new_gpa + 0x1000;
    let s0 = "Initial message";
    let s1 = "Test memory update with new slot!";

    let temp_dir = TempDir::new().unwrap();
    let pipe_path = temp_dir.path().join("urandom");
    let pipe_path_c = CString::new(pipe_path.as_os_str().as_encoded_bytes()).unwrap();
    ffi!(unsafe { libc::mkfifo(pipe_path_c.as_ptr(), 0o600) }).unwrap();

    let param = EntropySpec {
        source: Some(pipe_path.clone().into()),
    };
    let dev = param.build("entropy_mem_update").unwrap();

    let (tx, rx) = flume::unbounded();
    let (handle, notifier) = dev.spawn_worker(rx, ram_bus.clone(), regs).unwrap();
    let (irq_tx, irq_rx) = flume::unbounded();
    let irq_sender = Arc::new(FakeIrqSender { q_tx: irq_tx });
    let start_param = StartParam {
        feature: VirtioFeature::VERSION_1.bits(),
        irq_sender,
        ioeventfds: Option::<Arc<[FakeIoeventFd]>>::None,
    };
    tx.send(WakeEvent::Start { param: start_param }).unwrap();
    notifier.notify().unwrap();

    let mut writer = OpenOptions::new()
        .write(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(&pipe_path)
        .unwrap();

    // 1. Prime worker on initial memory (DATA_ADDR)
    writer.write_all(s0.as_bytes()).unwrap();
    writer.flush().unwrap();
    let id0 = guest_q.add_desc(&[], &[(buf0_addr, 4 << 10)]);
    tx.send(WakeEvent::Notify { q_index: 0 }).unwrap();
    notifier.notify().unwrap();
    assert_eq!(irq_rx.recv_timeout(Duration::from_secs(1)).unwrap(), 0);
    let used0 = guest_q.get_used().unwrap();
    assert_eq!(used0.id, id0);
    assert_eq!(used0.len, s0.len() as u32);

    // 2. Dynamically add new memory pages to RamBus
    let new_pages = ArcMemPages::from_anonymous(0x100_000, None, None).unwrap();
    {
        let mut guard = ram_bus.ram.write();
        let mut new_ram = (**guard).clone();
        new_ram.add(new_gpa, new_pages).unwrap();
        *guard = Arc::new(new_ram);
    }

    // 3. Trigger MemoryUpdate event to notify worker of the new RamBus layout
    tx.send(WakeEvent::MemoryUpdate).unwrap();
    notifier.notify().unwrap();
    std::thread::sleep(Duration::from_millis(100));

    // 4. Submit descriptor id1 pointing to new_buf_addr AFTER MemoryUpdate
    writer.write_all(s1.as_bytes()).unwrap();
    writer.flush().unwrap();
    let id1 = guest_q.add_desc(&[], &[(new_buf_addr, 4 << 10)]);
    tx.send(WakeEvent::Notify { q_index: 0 }).unwrap();
    notifier.notify().unwrap();

    assert_eq!(irq_rx.recv_timeout(Duration::from_secs(1)).unwrap(), 0);
    let used1 = guest_q.get_used().unwrap();
    assert_eq!(used1.id, id1);
    assert_eq!(used1.len, s1.len() as u32);

    // Verify worker wrote data into the newly added memory slot
    let updated_ram = ram_bus.ram.read();
    let mut buf = vec![0u8; s1.len()];
    updated_ram.read(new_buf_addr, &mut buf).unwrap();
    assert_eq!(String::from_utf8_lossy(&buf), s1);

    tx.send(WakeEvent::Shutdown).unwrap();
    notifier.notify().unwrap();
    handle.join().unwrap();
}

#[test]
fn entropy_memory_update_negative_test() {
    let ram_bus = Arc::new(fixture_ram_bus());
    let ram = ram_bus.ram.read().clone();
    let regs: Arc<[QueueReg]> = Arc::from(fixture_queues(1));

    let mut guest_q = GuestQueue::new(
        SplitQueue::new(&regs[0], &ram, false).unwrap().unwrap(),
        &regs[0],
    );

    let buf0_addr = DATA_ADDR;
    let new_gpa = 0x200_000;
    let new_buf_addr = new_gpa + 0x1000;
    let s0 = "Initial message";
    let s1 = "This should fail because MemoryUpdate was not sent!";

    let temp_dir = TempDir::new().unwrap();
    let pipe_path = temp_dir.path().join("urandom");
    let pipe_path_c = CString::new(pipe_path.as_os_str().as_encoded_bytes()).unwrap();
    ffi!(unsafe { libc::mkfifo(pipe_path_c.as_ptr(), 0o600) }).unwrap();

    let param = EntropySpec {
        source: Some(pipe_path.clone().into()),
    };
    let dev = param.build("entropy_mem_update_neg").unwrap();

    let (tx, rx) = flume::unbounded();
    let (handle, notifier) = dev.spawn_worker(rx, ram_bus.clone(), regs).unwrap();
    let (irq_tx, irq_rx) = flume::unbounded();
    let irq_sender = Arc::new(FakeIrqSender { q_tx: irq_tx });
    let start_param = StartParam {
        feature: VirtioFeature::VERSION_1.bits(),
        irq_sender,
        ioeventfds: Option::<Arc<[FakeIoeventFd]>>::None,
    };
    tx.send(WakeEvent::Start { param: start_param }).unwrap();
    notifier.notify().unwrap();

    let mut writer = OpenOptions::new()
        .write(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(&pipe_path)
        .unwrap();

    // 1. Prime worker on initial memory (DATA_ADDR) to ensure worker thread is active with old Ram snapshot
    writer.write_all(s0.as_bytes()).unwrap();
    writer.flush().unwrap();
    let id0 = guest_q.add_desc(&[], &[(buf0_addr, 4 << 10)]);
    tx.send(WakeEvent::Notify { q_index: 0 }).unwrap();
    notifier.notify().unwrap();
    assert_eq!(irq_rx.recv_timeout(Duration::from_secs(1)).unwrap(), 0);
    let used0 = guest_q.get_used().unwrap();
    assert_eq!(used0.id, id0);
    assert_eq!(used0.len, s0.len() as u32);

    // 2. Add new memory slot to RamBus, BUT DO NOT send WakeEvent::MemoryUpdate to the worker!
    let new_pages = ArcMemPages::from_anonymous(0x100_000, None, None).unwrap();
    {
        let mut guard = ram_bus.ram.write();
        let mut new_ram = (**guard).clone();
        new_ram.add(new_gpa, new_pages).unwrap();
        *guard = Arc::new(new_ram);
    }

    // 3. Descriptor points to new_buf_addr in the newly added memory slot
    writer.write_all(s1.as_bytes()).unwrap();
    writer.flush().unwrap();
    let _id1 = guest_q.add_desc(&[], &[(new_buf_addr, 4 << 10)]);
    tx.send(WakeEvent::Notify { q_index: 0 }).unwrap();
    notifier.notify().unwrap();

    // Without WakeEvent::MemoryUpdate, the worker holds old Ram snapshot, fails to translate new_buf_addr,
    // logs an error ("0x201000 is not mapped"), and terminates worker thread.
    assert_matches!(
        irq_rx.recv_timeout(Duration::from_millis(200)),
        Err(flume::RecvTimeoutError::Disconnected)
    );

    // Joining the worker thread confirms it terminated due to the memory translation error.
    handle.join().unwrap();
}


