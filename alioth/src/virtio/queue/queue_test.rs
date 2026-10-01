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

use std::collections::HashMap;
use std::io::{ErrorKind, IoSlice, IoSliceMut, Read, Write};
use std::ptr::eq as ptr_eq;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use assert_matches::assert_matches;
use flume::TryRecvError;
use rstest::rstest;

use crate::mem::mapped::{ArcMemPages, Ram};
use crate::virtio::Error;
use crate::virtio::queue::packed::PackedQueue;
use crate::virtio::queue::split::SplitQueue;
use crate::virtio::queue::{
    DescChain, Queue, QueueReg, Status, VirtQueue, copy_from_reader, copy_to_writer, sync_queues,
};
use crate::virtio::tests::{DATA_ADDR, FakeIrqSender, MEM_SIZE, fixture_queues, fixture_ram_bus};

#[derive(Debug)]
pub struct UsedDesc {
    pub id: u16,
    pub delta: u16,
    pub len: u32,
}

pub trait VirtQueueGuest: VirtQueue {
    fn add_desc(
        &mut self,
        index: Self::Index,
        ids: &[u16],
        readable: &[(u64, u32)],
        writable: &[(u64, u32)],
    ) -> u16;

    fn get_used(&mut self, index: Self::Index, chains: &HashMap<u16, Vec<u16>>)
    -> Option<UsedDesc>;

    fn notification_enabled(&self) -> bool;

    fn enable_interrupt(&mut self, enabled: bool, index: Self::Index);
}

pub struct GuestQueue<Q>
where
    Q: VirtQueueGuest,
{
    q: Q,
    avail: Q::Index,
    used: Q::Index,
    ids: Vec<bool>,
    chains: HashMap<u16, Vec<u16>>,
    next_id: u16,
}

impl<Q> GuestQueue<Q>
where
    Q: VirtQueueGuest,
{
    pub fn new(q: Q, reg: &QueueReg) -> Self {
        let size = reg.size.load(Ordering::Acquire);
        Self {
            q,
            avail: Q::INIT_INDEX,
            used: Q::INIT_INDEX,
            ids: vec![false; size as usize],
            chains: HashMap::new(),
            next_id: 0,
        }
    }
}

impl<Q> GuestQueue<Q>
where
    Q: VirtQueueGuest,
{
    pub fn add_desc(&mut self, readable: &[(u64, u32)], writable: &[(u64, u32)]) -> u16 {
        let mut ids = vec![];
        let total = readable.len() + writable.len();
        for _ in 0..self.ids.len() {
            if !self.ids[self.next_id as usize] {
                ids.push(self.next_id);
                self.ids[self.next_id as usize] = true;
            }
            self.next_id = self.next_id.wrapping_add(1) % self.ids.len() as u16;
            if ids.len() == total {
                break;
            }
        }
        assert_eq!(ids.len(), total);
        let delta = self.q.add_desc(self.avail, &ids, readable, writable);
        self.avail = self.q.index_add(self.avail, delta);
        let head_id = ids[0];
        self.chains.insert(head_id, ids);
        head_id
    }

    pub fn get_used(&mut self) -> Option<UsedDesc> {
        let used = self.q.get_used(self.used, &self.chains)?;
        let ids = self.chains.remove(&used.id).unwrap();
        for id in ids {
            self.ids[id as usize] = false;
        }
        self.used = self.q.index_add(self.used, used.delta);
        Some(used)
    }

    pub fn notification_enabled(&self) -> bool {
        self.q.notification_enabled()
    }

    pub fn enable_interrupt(&mut self, enabled: bool) {
        self.q.enable_interrupt(enabled, self.used);
    }
}

#[derive(Debug)]
enum ReaderData<'a> {
    Buf(&'a [u8]),
    Err(ErrorKind),
}

#[derive(Debug)]
struct Reader<'a> {
    data: &'a [ReaderData<'a>],
    index: usize,
    pos: usize,
}

impl<'a> Read for Reader<'a> {
    fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
        unreachable!()
    }

    fn read_vectored(&mut self, bufs: &mut [IoSliceMut<'_>]) -> std::io::Result<usize> {
        let mut count = 0;
        let mut buf_iter = bufs.iter_mut();
        let Some(s) = buf_iter.next() else {
            return Ok(0);
        };
        let mut buf = s.as_mut();
        while let Some(data) = self.data.get(self.index) {
            match data {
                ReaderData::Buf(data) => {
                    let c = buf.write(&data[self.pos..]).unwrap();
                    self.pos += c;
                    if self.pos == data.len() {
                        self.index += 1;
                        self.pos = 0;
                    }
                    count += c;
                    if buf.is_empty() {
                        let Some(s) = buf_iter.next() else {
                            break;
                        };
                        buf = s.as_mut();
                    }
                }
                ReaderData::Err(kind) => {
                    if count > 0 {
                        break;
                    }
                    self.index += 1;
                    return Err((*kind).into());
                }
            }
        }
        Ok(count)
    }
}

#[test]
fn test_copy_from_reader() {
    let ram_bus = Arc::new(fixture_ram_bus());
    let queues = fixture_queues(1);
    let ram = ram_bus.load();
    let reg = &queues[0];
    let mut host_q = Queue::<SplitQueue>::new(reg, ram_bus.clone(), false)
        .unwrap()
        .unwrap();
    let mut guest_q = GuestQueue::new(SplitQueue::new(reg, &ram, false).unwrap().unwrap(), reg);
    assert!(ptr_eq(host_q.reg(), reg));

    let (irq_tx, irq_rx) = flume::unbounded();
    let irq_sender = FakeIrqSender { q_tx: irq_tx };

    let str_0 = "Hello, World!";
    let str_1 = "Goodbye, World!";
    let str_2 = "Bose-Einstein condensate";
    let addr_0 = DATA_ADDR;
    let addr_1 = addr_0 + str_0.len() as u64;
    let addr_2 = addr_1 + str_1.len() as u64;
    let addr_3 = addr_2 + str_2.len() as u64;

    let mut reader = Reader {
        data: &[
            ReaderData::Buf(str_0.as_bytes()),
            ReaderData::Buf(str_1.as_bytes()),
            ReaderData::Err(ErrorKind::WouldBlock),
            ReaderData::Buf(str_2.as_bytes()),
            ReaderData::Err(ErrorKind::Interrupted),
        ],
        pos: 0,
        index: 0,
    };

    // no writable descriptors
    host_q
        .handle_desc(0, &irq_sender, copy_from_reader(&mut reader))
        .unwrap();
    assert_eq!(irq_rx.try_recv(), Err(TryRecvError::Empty));

    // empty writable descripter
    guest_q.add_desc(&[], &[(addr_0, 0)]);
    host_q
        .handle_desc(0, &irq_sender, copy_from_reader(&mut reader))
        .unwrap();
    assert_eq!(irq_rx.try_recv(), Ok(0));

    guest_q.add_desc(
        &[],
        &[(addr_0, str_0.len() as u32), (addr_1, str_1.len() as u32)],
    );
    host_q
        .handle_desc(0, &irq_sender, copy_from_reader(&mut reader))
        .unwrap();
    assert_eq!(irq_rx.try_recv(), Ok(0));

    // no writable descriptors
    host_q
        .handle_desc(0, &irq_sender, copy_from_reader(&mut reader))
        .unwrap();
    assert_eq!(irq_rx.try_recv(), Err(TryRecvError::Empty));

    guest_q.add_desc(&[], &[(addr_2, str_2.len() as u32)]);
    // will hit ErrorKind::WouldBlock
    host_q
        .handle_desc(0, &irq_sender, copy_from_reader(&mut reader))
        .unwrap();
    assert!(!guest_q.notification_enabled());
    assert_eq!(irq_rx.try_recv(), Err(TryRecvError::Empty));

    host_q
        .handle_desc(0, &irq_sender, copy_from_reader(&mut reader))
        .unwrap();
    assert!(guest_q.notification_enabled());
    assert_eq!(irq_rx.try_recv(), Ok(0));

    guest_q.add_desc(&[], &[(addr_3, 12)]);

    // will hit ErrorKind::Interrupted
    assert_matches!(
        host_q.handle_desc(0, &irq_sender, copy_from_reader(&mut reader)),
        Err(Error::System { error, .. }) if error.kind() == ErrorKind::Interrupted
    );
    assert!(guest_q.notification_enabled());

    host_q
        .handle_desc(0, &irq_sender, copy_from_reader(&mut reader))
        .unwrap();
    assert_eq!(irq_rx.try_recv(), Err(TryRecvError::Empty));

    for (s, addr) in [(str_0, addr_0), (str_1, addr_1), (str_2, addr_2)] {
        let mut buf = vec![0u8; s.len()];
        ram.read(addr, &mut buf).unwrap();
        assert_eq!(String::from_utf8_lossy(buf.as_slice()), s);
    }
}

#[derive(Debug)]
enum WriterData<'a> {
    Buf(&'a mut [u8]),
    Err(ErrorKind),
}

#[derive(Debug)]
struct Writer<'a> {
    data: &'a mut [WriterData<'a>],
    index: usize,
    pos: usize,
}

impl<'a> Write for Writer<'a> {
    fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
        unreachable!()
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }

    fn write_vectored(&mut self, bufs: &[IoSlice<'_>]) -> std::io::Result<usize> {
        let mut count = 0;
        let mut buf_iter = bufs.iter();
        let Some(s) = buf_iter.next() else {
            return Ok(0);
        };
        let mut buf = s.as_ref();
        while let Some(data) = self.data.get_mut(self.index) {
            match data {
                WriterData::Buf(data) => {
                    let c = buf.read(&mut data[self.pos..]).unwrap();
                    self.pos += c;
                    if self.pos == data.len() {
                        self.index += 1;
                        self.pos = 0;
                    }
                    count += c;
                    if buf.is_empty() {
                        let Some(s) = buf_iter.next() else {
                            break;
                        };
                        buf = s.as_ref();
                    }
                }
                WriterData::Err(kind) => {
                    if count > 0 {
                        break;
                    }
                    self.index += 1;
                    return Err((*kind).into());
                }
            }
        }
        Ok(count)
    }
}

#[test]
fn test_copy_to_writer() {
    let ram_bus = Arc::new(fixture_ram_bus());
    let queues = fixture_queues(1);
    let ram = ram_bus.load();
    let reg = &queues[0];
    let mut host_q = Queue::<SplitQueue>::new(reg, ram_bus.clone(), false)
        .unwrap()
        .unwrap();
    let mut guest_q = GuestQueue::new(SplitQueue::new(reg, &ram, false).unwrap().unwrap(), reg);
    let (irq_tx, irq_rx) = flume::unbounded();
    let irq_sender = FakeIrqSender { q_tx: irq_tx };

    let str_0 = "Hello, World!";
    let str_1 = "Goodbye, World!";
    let str_2 = "Bose-Einstein condensate";
    let addr_0 = DATA_ADDR;
    let addr_1 = addr_0 + str_0.len() as u64;
    let addr_2 = addr_1 + str_1.len() as u64;
    let addr_3 = addr_2 + str_2.len() as u64;
    for (s, addr) in [(str_0, addr_0), (str_1, addr_1), (str_2, addr_2)] {
        ram.write(addr, s.as_bytes()).unwrap();
    }

    let mut buf_0 = vec![0u8; str_0.len()];
    let mut buf_1 = vec![0u8; str_1.len()];
    let mut buf_2 = vec![0u8; str_2.len()];
    let mut writer = Writer {
        data: &mut [
            WriterData::Buf(buf_0.as_mut_slice()),
            WriterData::Buf(buf_1.as_mut_slice()),
            WriterData::Err(ErrorKind::WouldBlock),
            WriterData::Buf(buf_2.as_mut_slice()),
            WriterData::Err(ErrorKind::Interrupted),
        ],
        pos: 0,
        index: 0,
    };

    // no readable descriptors
    host_q
        .handle_desc(0, &irq_sender, copy_to_writer(&mut writer))
        .unwrap();
    assert_eq!(irq_rx.try_recv(), Err(TryRecvError::Empty));

    // empty readble descripter
    guest_q.add_desc(&[(addr_0, 0)], &[]);
    host_q
        .handle_desc(0, &irq_sender, copy_to_writer(&mut writer))
        .unwrap();
    assert_eq!(irq_rx.try_recv(), Ok(0));

    guest_q.add_desc(
        &[(addr_0, str_0.len() as u32), (addr_1, str_1.len() as u32)],
        &[],
    );
    host_q
        .handle_desc(0, &irq_sender, copy_to_writer(&mut writer))
        .unwrap();
    assert_eq!(irq_rx.try_recv(), Ok(0));

    // no readable descriptors
    host_q
        .handle_desc(0, &irq_sender, copy_to_writer(&mut writer))
        .unwrap();
    assert_eq!(irq_rx.try_recv(), Err(TryRecvError::Empty));

    guest_q.add_desc(&[(addr_2, str_2.len() as u32)], &[]);
    // will hit ErrorKind::WouldBlock
    host_q
        .handle_desc(0, &irq_sender, copy_to_writer(&mut writer))
        .unwrap();
    assert!(!guest_q.notification_enabled());
    assert_eq!(irq_rx.try_recv(), Err(TryRecvError::Empty));

    host_q
        .handle_desc(0, &irq_sender, copy_to_writer(&mut writer))
        .unwrap();
    assert!(guest_q.notification_enabled());
    assert_eq!(irq_rx.try_recv(), Ok(0));

    guest_q.add_desc(&[(addr_3, 12)], &[]);

    // will hit ErrorKind::Interrupted
    assert_matches!(
        host_q.handle_desc(0, &irq_sender, copy_to_writer(&mut writer)),
        Err(Error::System { error, .. }) if error.kind() == ErrorKind::Interrupted
    );
    assert!(guest_q.notification_enabled());

    host_q
        .handle_desc(0, &irq_sender, copy_to_writer(&mut writer))
        .unwrap();
    assert_eq!(irq_rx.try_recv(), Err(TryRecvError::Empty));

    for (buf, s) in [(buf_0, str_0), (buf_1, str_1), (buf_2, str_2)] {
        assert_eq!(String::from_utf8_lossy(buf.as_slice()), s)
    }
}

#[test]
fn test_written_bytes() {
    let str_0 = "Hello, World!";
    let str_1 = "Goodbye, World!";

    let ram = Ram::default();
    let mut buf = vec![0u8; str_0.len()];
    let mut chain = DescChain {
        id: 0,
        delta: 1,
        readable: vec![],
        writable: vec![IoSliceMut::new(buf.as_mut_slice())],
        ram: &ram,
    };
    let reader = str_0.as_bytes();
    assert_matches!(
        copy_from_reader(reader)(&mut chain),
        Ok(Status::Done { len: 13 })
    );
    assert_eq!(buf.as_slice(), str_0.as_bytes());

    let mut buf = vec![];
    let mut chain = DescChain {
        id: 1,
        delta: 1,
        readable: vec![IoSlice::new(str_1.as_bytes())],
        writable: vec![],
        ram: &ram,
    };
    assert_matches!(
        copy_to_writer(&mut buf)(&mut chain),
        Ok(Status::Done { len: 0 })
    );
    assert_eq!(buf.as_slice(), str_1.as_bytes());
}

fn check_handle_deferred<Q>(
    mut host_q: Queue<'_, Q>,
    mut guest_q: GuestQueue<Q>,
    ram: &crate::mem::mapped::Ram,
) where
    Q: VirtQueueGuest,
{
    let (irq_tx, irq_rx) = flume::unbounded();
    let irq_sender = FakeIrqSender { q_tx: irq_tx };

    let str_0 = "Hello, World!";
    let str_1 = "Goodbye, World!";
    let str_2 = "Bose-Einstein condensate";
    let addr_0 = DATA_ADDR;
    let addr_1 = addr_0 + str_0.len() as u64;
    let addr_2 = addr_1 + str_1.len() as u64;
    for (s, addr) in [(str_0, addr_0), (str_1, addr_1), (str_2, addr_2)] {
        ram.write(addr, s.as_bytes()).unwrap();
    }

    guest_q.enable_interrupt(true);
    guest_q.add_desc(
        &[(addr_0, str_0.len() as u32), (addr_1, str_1.len() as u32)],
        &[],
    );
    guest_q.add_desc(&[(addr_2, str_2.len() as u32)], &[]);

    let mut ids = vec![];
    host_q
        .handle_desc(0, &irq_sender, |chain| {
            ids.push(chain.id());
            Ok(Status::Break)
        })
        .unwrap();
    assert!(!guest_q.notification_enabled());
    assert_eq!(ids, [0]);

    ids.clear();
    host_q
        .handle_desc(0, &irq_sender, |chain| {
            ids.push(chain.id());
            Ok(Status::Deferred)
        })
        .unwrap();
    assert!(guest_q.notification_enabled());
    assert_eq!(irq_rx.try_recv(), Err(TryRecvError::Empty));
    assert_eq!(ids, [0, 2]);

    host_q
        .handle_deferred(0, 0, &irq_sender, |chain| {
            assert_eq!(chain.id, 0);
            assert_eq!(&*chain.readable[0], str_0.as_bytes());
            assert_eq!(&*chain.readable[1], str_1.as_bytes());
            assert_eq!(chain.writable.len(), 0);
            Ok(0)
        })
        .unwrap();
    assert_eq!(irq_rx.try_recv(), Ok(0));
    let used = guest_q.get_used().unwrap();
    assert_eq!(used.id, 0);

    assert_matches!(
        host_q.handle_deferred(1, 0, &irq_sender, |_| Ok(0)),
        Err(Error::InvalidDescriptor { id: 1, .. })
    );

    guest_q.enable_interrupt(false);
    host_q
        .handle_deferred(2, 0, &irq_sender, |chain| {
            assert_eq!(chain.id, 2);
            assert_eq!(&*chain.readable[0], str_2.as_bytes());
            assert_eq!(chain.writable.len(), 0);
            Ok(0)
        })
        .unwrap();
    assert_eq!(irq_rx.try_recv(), Err(TryRecvError::Empty));
    let used = guest_q.get_used().unwrap();
    assert_eq!(used.id, 2);
}

#[rstest]
#[case(false, false)]
#[case(false, true)]
#[case(true, false)]
#[case(true, true)]
fn test_handle_deferred(#[case] packed: bool, #[case] event_idx: bool) {
    let ram_bus = Arc::new(fixture_ram_bus());
    let queues = fixture_queues(1);
    let ram = ram_bus.load();
    let reg = &queues[0];
    if packed {
        let host_q = Queue::<PackedQueue>::new(reg, ram_bus.clone(), event_idx)
            .unwrap()
            .unwrap();
        let guest_q = GuestQueue::new(
            PackedQueue::new(reg, &ram, event_idx).unwrap().unwrap(),
            reg,
        );
        check_handle_deferred(host_q, guest_q, &ram);
    } else {
        let host_q = Queue::<SplitQueue>::new(reg, ram_bus.clone(), event_idx)
            .unwrap()
            .unwrap();
        let guest_q = GuestQueue::new(SplitQueue::new(reg, &ram, event_idx).unwrap().unwrap(), reg);
        check_handle_deferred(host_q, guest_q, &ram);
    }
}

#[test]
fn test_sync_ram() {
    const EXTRA_GPA: u64 = 4 << 20;
    const EXTRA_SIZE: usize = 1 << 20;

    let ram_bus = Arc::new(fixture_ram_bus());
    let extra = ArcMemPages::from_anonymous(EXTRA_SIZE, None, None).unwrap();
    ram_bus.update(|ram| ram.add(EXTRA_GPA, extra)).unwrap();
    let queues = fixture_queues(1);
    let reg = &queues[0];
    let ram = ram_bus.load();
    let mut host_q = Queue::<SplitQueue>::new(reg, ram_bus.clone(), false)
        .unwrap()
        .unwrap();
    let mut guest_q = GuestQueue::new(SplitQueue::new(reg, &ram, false).unwrap().unwrap(), reg);
    let (irq_tx, irq_rx) = flume::unbounded();
    let irq_sender = FakeIrqSender { q_tx: irq_tx };

    // The first chain points into the extra slot and stays in flight.
    let str_0 = "in flight across a memory update";
    ram.write(EXTRA_GPA, str_0.as_bytes()).unwrap();
    let id_0 = guest_q.add_desc(&[(EXTRA_GPA, str_0.len() as u32)], &[]);
    host_q
        .handle_desc(0, &irq_sender, |_| Ok(Status::Deferred))
        .unwrap();
    assert_eq!(host_q.num_deferred(), 1);

    // Unplug the extra slot and move the queue to the new layout.
    ram_bus.update(|ram| ram.remove(EXTRA_GPA)).unwrap();
    let old_ram = Arc::downgrade(&ram);
    drop(ram);
    assert!(host_q.sync_ram().unwrap());
    let new_ram = ram_bus.load();
    assert!(Arc::ptr_eq(host_q.ram(), &new_ram));
    // Nothing to do until the next update.
    assert!(!host_q.sync_ram().unwrap());
    // The deferred chain still pins the old snapshot.
    assert!(old_ram.upgrade().is_some());

    // A new chain is picked up at the right ring index, not from 0.
    let str_1 = "after the update";
    new_ram.write(DATA_ADDR, str_1.as_bytes()).unwrap();
    let id_1 = guest_q.add_desc(&[(DATA_ADDR, str_1.len() as u32)], &[]);
    let mut ids = vec![];
    host_q
        .handle_desc(0, &irq_sender, |chain| {
            ids.push(chain.id());
            assert_eq!(&*chain.readable[0], str_1.as_bytes());
            Ok(Status::Done { len: 0 })
        })
        .unwrap();
    assert_eq!(ids, [id_1]);
    assert_eq!(irq_rx.try_recv(), Ok(0));
    assert_matches!(guest_q.get_used(), Some(UsedDesc { id, .. }) if id == id_1);

    // The buffer of the in-flight chain is still accessible.
    host_q
        .handle_deferred(id_0, 0, &irq_sender, |chain| {
            assert_eq!(&*chain.readable[0], str_0.as_bytes());
            Ok(0)
        })
        .unwrap();
    assert_matches!(guest_q.get_used(), Some(UsedDesc { id, .. }) if id == id_0);
    assert_eq!(host_q.num_deferred(), 0);
    assert!(old_ram.upgrade().is_none());
}

#[test]
fn test_sync_ram_stays_on_old_snapshot_on_reset() {
    let ram_bus = Arc::new(fixture_ram_bus());
    let queues = fixture_queues(1);
    let reg = &queues[0];
    let ram = ram_bus.load();
    let mut host_q = Queue::<SplitQueue>::new(reg, ram_bus.clone(), false)
        .unwrap()
        .unwrap();
    let mut guest_q = GuestQueue::new(SplitQueue::new(reg, &ram, false).unwrap().unwrap(), reg);
    let (irq_tx, irq_rx) = flume::unbounded();
    let irq_sender = FakeIrqSender { q_tx: irq_tx };

    // A concurrent reset clears the registers before the worker sees the
    // reset, while the memory layout changes.
    reg.reset();
    let extra = ArcMemPages::from_anonymous(1 << 20, None, None).unwrap();
    ram_bus.update(|ram| ram.add(4 << 20, extra)).unwrap();
    // A reset is pending: the queue refuses to move to the new layout.
    assert_matches!(host_q.sync_ram(), Ok(false));
    assert!(Arc::ptr_eq(host_q.ram(), &ram));

    // The queue still uses the snapshot and rings set up at activation.
    let s = "after the registers were cleared";
    ram.write(DATA_ADDR, s.as_bytes()).unwrap();
    let id = guest_q.add_desc(&[(DATA_ADDR, s.len() as u32)], &[]);
    host_q
        .handle_desc(0, &irq_sender, |chain| {
            assert_eq!(&*chain.readable[0], s.as_bytes());
            Ok(Status::Done { len: 0 })
        })
        .unwrap();
    assert_eq!(irq_rx.try_recv(), Ok(0));
    assert_matches!(guest_q.get_used(), Some(UsedDesc { id: used, .. }) if used == id);
}

#[test]
fn test_sync_ram_stays_on_old_snapshot_without_rings() {
    let ram_bus = Arc::new(fixture_ram_bus());
    let queues = fixture_queues(1);
    let reg = &queues[0];
    let ram = ram_bus.load();
    let mut host_q = Queue::<SplitQueue>::new(reg, ram_bus.clone(), false)
        .unwrap()
        .unwrap();
    let mut guest_q = GuestQueue::new(SplitQueue::new(reg, &ram, false).unwrap().unwrap(), reg);
    let (irq_tx, irq_rx) = flume::unbounded();
    let irq_sender = FakeIrqSender { q_tx: irq_tx };

    // A chain stays in flight, e.g. as an io_uring request.
    let id = guest_q.add_desc(&[], &[(DATA_ADDR, 16)]);
    host_q
        .handle_desc(0, &irq_sender, |_| Ok(Status::Deferred))
        .unwrap();

    // The memory of the rings goes away while the queue is still enabled,
    // e.g. the guest unplugs it. No reset is pending, so the queue tries the
    // new layout, but the rings cannot be rebuilt. The queue stays on its
    // old snapshot, which keeps the memory alive.
    ram_bus.update(|ram| ram.remove(0)).unwrap();
    assert_matches!(host_q.sync_ram(), Err(_));
    assert!(Arc::ptr_eq(host_q.ram(), &ram));
    // The failed layout is not retried.
    assert_matches!(host_q.sync_ram(), Ok(false));

    // The in-flight chain completes into the old snapshot.
    host_q
        .handle_deferred(id, 0, &irq_sender, |chain| {
            assert_eq!(chain.writable[0].len(), 16);
            Ok(16)
        })
        .unwrap();
    assert_eq!(irq_rx.try_recv(), Ok(0));
    assert_matches!(guest_q.get_used(), Some(UsedDesc { id: used, .. }) if used == id);

    // Once the memory comes back, the queue moves on.
    let pages = ArcMemPages::from_anonymous(MEM_SIZE, None, None).unwrap();
    ram_bus.update(|ram| ram.add(0, pages)).unwrap();
    assert_matches!(host_q.sync_ram(), Ok(true));
    assert!(Arc::ptr_eq(host_q.ram(), &ram_bus.load()));
}

#[test]
fn test_leak_deferred() {
    let ram_bus = Arc::new(fixture_ram_bus());
    let queues = fixture_queues(1);
    let reg = &queues[0];
    let ram = ram_bus.load();
    let mut host_q = Queue::<SplitQueue>::new(reg, ram_bus.clone(), false)
        .unwrap()
        .unwrap();
    let mut guest_q = GuestQueue::new(SplitQueue::new(reg, &ram, false).unwrap().unwrap(), reg);
    let (irq_tx, _irq_rx) = flume::unbounded();
    let irq_sender = FakeIrqSender { q_tx: irq_tx };

    guest_q.add_desc(&[], &[(DATA_ADDR, 16)]);
    host_q
        .handle_desc(0, &irq_sender, |_| Ok(Status::Deferred))
        .unwrap();
    assert_eq!(host_q.num_deferred(), 1);

    // The snapshot pinned by the chain outlives the queue and the bus.
    let snapshot = Arc::downgrade(&ram);
    drop(ram);
    host_q.leak_deferred();
    assert_eq!(host_q.num_deferred(), 0);
    drop(host_q);
    drop(ram_bus);
    assert!(snapshot.upgrade().is_some());
}

#[test]
fn test_queue_new_after_reset_and_layout_change() {
    let ram_bus = Arc::new(fixture_ram_bus());
    let queues = fixture_queues(1);
    let reg = &queues[0];

    // 1. Device reset clears registers and bumps the reset counter.
    reg.reset();

    // 2. Memory reset removes the old RAM where the initial rings were located.
    ram_bus.update(|ram| ram.remove(0)).unwrap();

    // 3. New boot creates fresh RAM at a different GPA where the old rings are not mapped.
    const NEW_GPA: u64 = 4 << 20;
    let fresh_pages = ArcMemPages::from_anonymous(MEM_SIZE, None, None).unwrap();
    ram_bus.update(|ram| ram.add(NEW_GPA, fresh_pages)).unwrap();

    // Queue::new observes enabled == false from the reset, returning Ok(None)
    // rather than attempting to dereference unmapped ring addresses in the new RAM.
    assert!(
        Queue::<SplitQueue>::new(reg, ram_bus.clone(), false)
            .unwrap()
            .is_none()
    );

    // 4. When Boot 2 enables the queue at valid addresses in the new RAM, Queue::new succeeds.
    reg.size.store(16, Ordering::Release);
    reg.desc.store(NEW_GPA, Ordering::Release);
    reg.driver.store(NEW_GPA + 0x1000, Ordering::Release);
    reg.device.store(NEW_GPA + 0x2000, Ordering::Release);
    reg.enabled.store(true, Ordering::Release);

    let host_q = Queue::<SplitQueue>::new(reg, ram_bus.clone(), false)
        .unwrap()
        .unwrap();
    assert_eq!(host_q.ram().generation(), ram_bus.generation());
    assert_eq!(host_q.bus_generation, ram_bus.generation());
}

#[test]
fn test_handle_desc_catches_up_with_ram_bus() {
    const EXTRA_GPA: u64 = 4 << 20;
    const EXTRA_SIZE: usize = 1 << 20;

    let ram_bus = Arc::new(fixture_ram_bus());
    let queues = fixture_queues(1);
    let reg = &queues[0];
    let ram = ram_bus.load();
    let mut host_q = Queue::<SplitQueue>::new(reg, ram_bus.clone(), false)
        .unwrap()
        .unwrap();
    let mut guest_q = GuestQueue::new(SplitQueue::new(reg, &ram, false).unwrap().unwrap(), reg);
    let (irq_tx, irq_rx) = flume::unbounded();
    let irq_sender = FakeIrqSender { q_tx: irq_tx };

    // The driver posts a buffer in memory plugged after the queue took its
    // snapshot, e.g. while the worker is still handling an earlier event.
    let extra = ArcMemPages::from_anonymous(EXTRA_SIZE, None, None).unwrap();
    ram_bus.update(|ram| ram.add(EXTRA_GPA, extra)).unwrap();
    let s = "in hot-plugged memory";
    ram_bus.write(EXTRA_GPA, s.as_bytes()).unwrap();
    let id = guest_q.add_desc(&[(EXTRA_GPA, s.len() as u32)], &[]);
    assert!(Arc::ptr_eq(host_q.ram(), &ram));

    // The queue moves to the latest layout and retries.
    host_q
        .handle_desc(0, &irq_sender, |chain| {
            assert_eq!(&*chain.readable[0], s.as_bytes());
            assert_eq!(chain.ram().generation(), ram_bus.generation());
            Ok(Status::Done { len: 0 })
        })
        .unwrap();
    assert_eq!(host_q.ram().generation(), ram_bus.generation());
    assert_eq!(irq_rx.try_recv(), Ok(0));
    assert_matches!(guest_q.get_used(), Some(UsedDesc { id: used, .. }) if used == id);
}

#[test]
fn test_handle_desc_irq_before_invalid_chain() {
    let ram_bus = Arc::new(fixture_ram_bus());
    let queues = fixture_queues(1);
    let reg = &queues[0];
    let ram = ram_bus.load();
    let mut host_q = Queue::<SplitQueue>::new(reg, ram_bus.clone(), false)
        .unwrap()
        .unwrap();
    let mut guest_q = GuestQueue::new(SplitQueue::new(reg, &ram, false).unwrap().unwrap(), reg);
    let (irq_tx, irq_rx) = flume::unbounded();
    let irq_sender = FakeIrqSender { q_tx: irq_tx };

    // A valid chain followed by one pointing outside guest memory.
    let s = "valid";
    ram.write(DATA_ADDR, s.as_bytes()).unwrap();
    let id = guest_q.add_desc(&[(DATA_ADDR, s.len() as u32)], &[]);
    guest_q.add_desc(&[(1 << 40, 16)], &[]);

    assert_matches!(
        host_q.handle_desc(0, &irq_sender, |_| Ok(Status::Done { len: 0 })),
        Err(_)
    );
    // The driver is still told about the chain completed before the error.
    assert_eq!(irq_rx.try_recv(), Ok(0));
    assert_matches!(guest_q.get_used(), Some(UsedDesc { id: used, .. }) if used == id);
}

#[test]
fn test_handle_desc_on_stale_queue() {
    let ram_bus = Arc::new(fixture_ram_bus());
    let queues = fixture_queues(1);
    let reg = &queues[0];
    let ram = ram_bus.load();
    let mut host_qs = [Some(
        Queue::<SplitQueue>::new(reg, ram_bus.clone(), false)
            .unwrap()
            .unwrap(),
    )];
    let mut guest_q = GuestQueue::new(SplitQueue::new(reg, &ram, false).unwrap().unwrap(), reg);
    let (irq_tx, irq_rx) = flume::unbounded();
    let irq_sender = FakeIrqSender { q_tx: irq_tx };

    // All memory goes away while the queue is still enabled, e.g. the guest
    // unplugs it. No reset is pending, so the worker tries the new layout,
    // fails to rebuild the rings, and keeps the queue on its old snapshot.
    // The reboot case, where a reset is pending, is covered by
    // test_sync_ram_reboot_stays_on_old_snapshot.
    ram_bus.update(|ram| ram.remove(0)).unwrap();
    sync_queues(&mut host_qs);
    let host_q = host_qs[0].as_mut().unwrap();
    assert!(Arc::ptr_eq(host_q.ram(), &ram));

    // Chains in the old snapshot are still served.
    let s = "still mapped";
    ram.write(DATA_ADDR, s.as_bytes()).unwrap();
    let id = guest_q.add_desc(&[(DATA_ADDR, s.len() as u32)], &[]);
    host_q
        .handle_desc(0, &irq_sender, |chain| {
            assert_eq!(&*chain.readable[0], s.as_bytes());
            Ok(Status::Done { len: 0 })
        })
        .unwrap();
    assert_eq!(irq_rx.try_recv(), Ok(0));
    assert_matches!(guest_q.get_used(), Some(UsedDesc { id: used, .. }) if used == id);

    // A chain the queue cannot follow is not an error: a driver that took
    // the memory of its rings away cannot be served until it resets the
    // device.
    guest_q.add_desc(&[(1 << 40, 16)], &[]);
    host_q
        .handle_desc(0, &irq_sender, |_| Ok(Status::Done { len: 0 }))
        .unwrap();
    assert_eq!(irq_rx.try_recv(), Err(TryRecvError::Empty));
    assert_matches!(guest_q.get_used(), None);
}

#[test]
fn test_sync_ram_reboot_stays_on_old_snapshot() {
    let ram_bus = Arc::new(fixture_ram_bus());
    let queues = fixture_queues(1);
    let reg = &queues[0];
    let ram = ram_bus.load();
    let mut host_qs = [Some(
        Queue::<SplitQueue>::new(reg, ram_bus.clone(), false)
            .unwrap()
            .unwrap(),
    )];
    let mut guest_q = GuestQueue::new(SplitQueue::new(reg, &ram, false).unwrap().unwrap(), reg);
    let (irq_tx, irq_rx) = flume::unbounded();
    let irq_sender = FakeIrqSender { q_tx: irq_tx };

    // Simulate a reboot:
    // 1. Device reset clears registers.
    reg.reset();

    // 2. Memory reset removes all RAM (generation G+1).
    ram_bus.update(|ram| ram.remove(0)).unwrap();

    // 3. New boot creates fresh RAM at the same GPA (generation G+2).
    let fresh_pages = ArcMemPages::from_anonymous(MEM_SIZE, None, None).unwrap();
    ram_bus.update(|ram| ram.add(0, fresh_pages)).unwrap();
    let new_ram = ram_bus.load();

    // 4. A queue event arrives before WakeEvent::Reset.
    sync_queues(&mut host_qs);
    let host_q = host_qs[0].as_mut().unwrap();

    // The queue must NOT follow into the fresh RAM; it must stay on the old snapshot.
    assert!(Arc::ptr_eq(host_q.ram(), &ram));
    assert!(!Arc::ptr_eq(host_q.ram(), &new_ram));
    assert!(host_q.is_stale());

    // In-flight chains in the old snapshot are still served.
    let s = "in old ram before reset";
    ram.write(DATA_ADDR, s.as_bytes()).unwrap();
    let id = guest_q.add_desc(&[(DATA_ADDR, s.len() as u32)], &[]);
    host_q
        .handle_desc(0, &irq_sender, |chain| {
            assert_eq!(&*chain.readable[0], s.as_bytes());
            Ok(Status::Done { len: 0 })
        })
        .unwrap();
    assert_eq!(irq_rx.try_recv(), Ok(0));
    assert_matches!(guest_q.get_used(), Some(UsedDesc { id: used, .. }) if used == id);

    // Any chain that cannot be translated in the old snapshot does not
    // error out or touch the new boot's RAM: the device reset will take
    // care of it.
    guest_q.add_desc(&[(1 << 40, 16)], &[]);
    host_q
        .handle_desc(0, &irq_sender, |_| Ok(Status::Done { len: 0 }))
        .unwrap();
    assert_eq!(irq_rx.try_recv(), Err(TryRecvError::Empty));
    assert_matches!(guest_q.get_used(), None);
}

#[test]
fn test_sync_ram_reboot_same_addrs_stays_on_old_snapshot() {
    let ram_bus = Arc::new(fixture_ram_bus());
    let queues = fixture_queues(1);
    let reg = &queues[0];
    let ram = ram_bus.load();
    let mut host_qs = [Some(
        Queue::<SplitQueue>::new(reg, ram_bus.clone(), false)
            .unwrap()
            .unwrap(),
    )];

    // Simulate a reboot where Boot 2 configures the queue at the exact same
    // GPA addresses:
    // 1. Device reset clears registers and bumps the reset counter.
    let old_desc = reg.desc.load(Ordering::Relaxed);
    let old_driver = reg.driver.load(Ordering::Relaxed);
    let old_device = reg.device.load(Ordering::Relaxed);
    let old_size = reg.size.load(Ordering::Relaxed);
    reg.reset();

    // 2. Memory reset removes all RAM (generation G+1).
    ram_bus.update(|ram| ram.remove(0)).unwrap();

    // 3. New boot creates fresh RAM at the same GPA (generation G+2).
    let fresh_pages = ArcMemPages::from_anonymous(MEM_SIZE, None, None).unwrap();
    ram_bus.update(|ram| ram.add(0, fresh_pages)).unwrap();
    let new_ram = ram_bus.load();

    // 4. Boot 2's driver configures the queue with the exact same ring addresses
    // and enables it, before the worker sees WakeEvent::Reset.
    reg.size.store(old_size, Ordering::Release);
    reg.desc.store(old_desc, Ordering::Release);
    reg.driver.store(old_driver, Ordering::Release);
    reg.device.store(old_device, Ordering::Release);
    reg.enabled.store(true, Ordering::Release);

    // 5. Worker syncs memory. The reset counter prevents moving into the fresh RAM.
    sync_queues(&mut host_qs);
    let host_q = host_qs[0].as_mut().unwrap();

    assert!(Arc::ptr_eq(host_q.ram(), &ram));
    assert!(!Arc::ptr_eq(host_q.ram(), &new_ram));
    assert!(host_q.is_stale());
    assert_matches!(host_q.sync_ram(), Ok(false));
}
