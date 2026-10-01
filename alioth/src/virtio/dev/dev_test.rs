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

use std::sync::Arc;
use std::sync::atomic::Ordering;

use bitflags::bitflags;

use super::*;
use crate::mem::emulated::{Action, Mmio};
use crate::virtio::tests::fixture_ram_bus;
use crate::virtio::{IrqSender, Result};

#[test]
fn test_virtio_register_get_driver_feature() {
    let reg = Register::default();
    assert_eq!(reg.get_driver_feature(), 0);

    reg.driver_feature[0].store(0x1234_5678, Ordering::Release);
    reg.driver_feature[1].store(0x9abc_def0, Ordering::Release);
    reg.driver_feature[2].store(0xfeed_cafe, Ordering::Release);
    reg.driver_feature[3].store(0x0123_4567, Ordering::Release);

    let expected = (0x1234_5678u128)
        | ((0x9abc_def0u128) << 32)
        | ((0xfeed_cafeu128) << 64)
        | ((0x0123_4567u128) << 96);
    assert_eq!(reg.get_driver_feature(), expected);
}

#[derive(Debug)]
struct EmptyConfig;

impl Mmio for EmptyConfig {
    fn size(&self) -> u64 {
        0
    }

    fn read(&self, _: u64, _: u8) -> crate::mem::Result<u64> {
        Ok(0)
    }

    fn write(&self, _: u64, _: u8, _: u64) -> crate::mem::Result<Action> {
        Ok(Action::None)
    }
}

bitflags! {
    #[derive(Debug)]
    struct DummyFeature: u128 {}
}

#[derive(Debug)]
struct DummyDev;

impl Virtio for DummyDev {
    type Config = EmptyConfig;
    type Feature = DummyFeature;

    fn name(&self) -> &str {
        "dummy"
    }

    fn id(&self) -> DeviceId {
        DeviceId::ENTROPY
    }

    fn num_queues(&self) -> u16 {
        0
    }

    fn config(&self) -> Arc<EmptyConfig> {
        Arc::new(EmptyConfig)
    }

    fn feature(&self) -> u128 {
        0
    }

    fn spawn_worker<S: IrqSender>(
        self,
        _: WorkerParam<S>,
    ) -> Result<(std::thread::JoinHandle<()>, Arc<Notifier>)> {
        unimplemented!()
    }
}

#[derive(Debug)]
struct DummyIrqSender;

impl IrqSender for DummyIrqSender {
    fn config_irq(&self) {}

    fn queue_irq(&self, _: u16) {}

    fn config_irqfd<F, T>(&self, _: F) -> Result<T>
    where
        F: FnOnce(std::os::fd::BorrowedFd) -> Result<T>,
    {
        unimplemented!()
    }

    fn queue_irqfd<F, T>(&self, _: u16, _: F) -> Result<T>
    where
        F: FnOnce(std::os::fd::BorrowedFd) -> Result<T>,
    {
        unimplemented!()
    }
}

/// An event that is not the worker notifier, so that handling it does not
/// touch the wake event channel.
struct DummyEvent;

impl BackendEvent for DummyEvent {
    fn token(&self) -> u64 {
        0
    }
}

/// An active backend that only counts how often its queues are asked to
/// move to the latest guest memory layout.
#[derive(Debug, Default)]
struct SyncCounter {
    syncs: usize,
}

impl ActiveBackend<DummyDev> for SyncCounter {
    type Event = DummyEvent;

    fn sync_ram(&mut self) {
        self.syncs += 1;
    }

    fn handle_event(&mut self, _: &mut DummyDev, _: &Self::Event) -> Result<()> {
        Ok(())
    }

    fn handle_queue(&mut self, _: &mut DummyDev, _: u16) -> Result<()> {
        Ok(())
    }
}

#[test]
fn test_handle_event_syncs_ram_once_per_layout() {
    let (tx, rx) = flume::unbounded();
    let ram_bus = Arc::new(fixture_ram_bus());
    let mut context = Context {
        dev: DummyDev,
        param: WorkerParam {
            event_rx: rx,
            memory: ram_bus.clone(),
            queue_regs: Arc::new([]),
        },
        state: WorkerState::Running,
        synced_generation: None,
    };
    let mut backend = SyncCounter::default();

    // The first event of an event loop syncs; later ones do not while the
    // layout stays the same.
    context.handle_event(&DummyEvent, &mut backend).unwrap();
    context.handle_event(&DummyEvent, &mut backend).unwrap();
    assert_eq!(backend.syncs, 1);

    // A layout change is picked up by the next event, and only by it.
    ram_bus.update(|ram| ram.remove(0)).unwrap();
    context.handle_event(&DummyEvent, &mut backend).unwrap();
    context.handle_event(&DummyEvent, &mut backend).unwrap();
    assert_eq!(backend.syncs, 2);

    // A new event loop starts with a sync even if the layout is unchanged.
    tx.send(WakeEvent::Start {
        param: StartParam {
            feature: 0,
            irq_sender: Arc::new(DummyIrqSender),
            notifiers: None,
        },
    })
    .unwrap();
    assert!(context.wait_start().is_some());
    context.handle_event(&DummyEvent, &mut backend).unwrap();
    assert_eq!(backend.syncs, 3);
}
