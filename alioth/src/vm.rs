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

use std::fmt::Debug;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::thread;

use parking_lot::{Condvar, Mutex, RwLock};
use thiserror::Error;

#[cfg(target_arch = "aarch64")]
use crate::arch::layout::PL011_START;
use crate::board::{self, ArchBoard, Board, BoardConfig, STATE_CREATED, STATE_RUNNING};
use crate::device::fw_cfg::{FwCfg, FwCfgItemParam, PORT_SELECTOR};
#[cfg(target_arch = "aarch64")]
use crate::device::pl011::Pl011;
use crate::device::pvpanic::PvPanic;
#[cfg(target_arch = "x86_64")]
use crate::device::serial::Serial;
use crate::hv::{self, Hypervisor, IoeventFdRegistry, Vm, VmConfig};
use crate::loader::{self, Payload};
use crate::mem::Memory;
#[cfg(target_arch = "aarch64")]
use crate::mem::{AddrOpt, MemRegion, MemRegionType};
use crate::pci::bus::PciBus;
use crate::pci::PciDevice;
use crate::virtio::dev::{DevParam, Virtio, VirtioDevice};
use crate::virtio::pci::VirtioPciDevice;
use crate::{mem, pci, virtio};

#[derive(Debug, Error)]
pub enum Error {
    #[error("hypervisor: {0}")]
    Hv(#[from] hv::Error),

    #[error("memory: {0}")]
    Memory(#[from] mem::Error),

    #[error("host io: {0}")]
    HostIo(#[from] std::io::Error),

    #[error("loader: {0}")]
    Loader(#[from] loader::Error),

    #[error("board: {0}")]
    Board(#[from] board::Error),

    #[error("PCI bus: {0}")]
    PciBus(#[from] pci::Error),

    #[error("Virtio: {0}")]
    Virtio(#[from] virtio::Error),

    #[error("ACPI bytes exceed EBDA area")]
    AcpiTooLong,

    #[error("cannot handle {0:#x?}")]
    VmExit(String),
}

type Result<T, E = Error> = std::result::Result<T, E>;

pub struct Machine<H>
where
    H: Hypervisor,
{
    board: Arc<Board<H::Vm>>,
    event_rx: Receiver<u32>,
    _event_tx: Sender<u32>,
}

impl<H> Machine<H>
where
    H: Hypervisor + 'static,
{
    pub fn new(hv: H, config: BoardConfig) -> Result<Self, Error> {
        let vm_config = VmConfig {
            coco: config.coco.clone(),
        };
        let mut vm = hv.create_vm(&vm_config)?;
        let vm_memory = vm.create_vm_memory()?;
        let memory = Memory::new(vm_memory);
        let arch = ArchBoard::new(&hv, &vm, &config)?;

        let board = Arc::new(Board {
            vm,
            memory,
            arch,
            config,
            state: AtomicU8::new(STATE_CREATED),
            payload: RwLock::new(None),
            vcpus: Arc::new(RwLock::new(Vec::new())),
            mp_sync: Arc::new((Mutex::new(0), Condvar::new())),
            io_devs: RwLock::new(Vec::new()),
            #[cfg(target_arch = "aarch64")]
            mmio_devs: RwLock::new(Vec::new()),
            pci_bus: PciBus::new(),
            pci_devs: RwLock::new(Vec::new()),
            fw_cfg: Mutex::new(None),
        });

        let (event_tx, event_rx) = mpsc::channel();

        let mut vcpus = board.vcpus.write();
        for vcpu_id in 0..board.config.num_cpu {
            let (boot_tx, boot_rx) = mpsc::channel();
            let event_tx = event_tx.clone();
            let board = board.clone();
            let handle = thread::Builder::new()
                .name(format!("vcpu_{}", vcpu_id))
                .spawn(move || board.run_vcpu(vcpu_id, event_tx, boot_rx))?;
            event_rx.recv().unwrap();
            vcpus.push((handle, boot_tx));
        }
        drop(vcpus);

        board.arch_init()?;

        let machine = Machine {
            board,
            event_rx,
            _event_tx: event_tx,
        };

        Ok(machine)
    }

    #[cfg(target_arch = "x86_64")]
    pub fn add_com1(&self) -> Result<(), Error> {
        let irq_sender = self.board.vm.create_irq_sender(4)?;
        let com1 = Serial::new(0x3f8, irq_sender)?;
        self.board.io_devs.write().push((0x3f8, Arc::new(com1)));
        Ok(())
    }

    #[cfg(target_arch = "aarch64")]
    pub fn add_pl011(&self) -> Result<(), Error> {
        let irq_line = self.board.vm.create_irq_sender(1)?;
        let pl011_dev = Pl011::new(PL011_START, irq_line)?;
        self.board.mmio_devs.write().push((
            AddrOpt::Fixed(PL011_START),
            Arc::new(MemRegion::with_emulated(
                Arc::new(pl011_dev),
                MemRegionType::Hidden,
            )),
        ));
        Ok(())
    }

    pub fn add_pci_dev(&mut self, dev: PciDevice) -> Result<(), Error> {
        let config = dev.dev.config();
        let bdf = self.board.pci_bus.add(None, config.clone())?;
        let header = config.get_header();
        header.set_bdf(bdf);
        log::info!("{} located at {bdf}", dev.name);
        self.board.pci_devs.write().push(dev);
        Ok(())
    }

    pub fn add_pvpanic(&mut self) -> Result<(), Error> {
        let dev = PvPanic::new();
        let pci_dev = PciDevice::new("pvpanic".to_owned().into(), Arc::new(dev));
        self.add_pci_dev(pci_dev)
    }

    pub fn add_fw_cfg(
        &mut self,
        params: impl Iterator<Item = FwCfgItemParam>,
    ) -> Result<Arc<Mutex<FwCfg>>, Error> {
        let items = params.map(|p| p.build()).collect::<Result<Vec<_>, _>>()?;
        let fw_cfg = Arc::new(Mutex::new(FwCfg::new(self.board.memory.ram_bus(), items)?));
        let mut io_devs = self.board.io_devs.write();
        io_devs.push((PORT_SELECTOR, fw_cfg.clone()));
        *self.board.fw_cfg.lock() = Some(fw_cfg.clone());
        Ok(fw_cfg)
    }

    pub fn add_virtio_dev<D, P>(
        &mut self,
        name: String,
        param: P,
    ) -> Result<
        Arc<
            VirtioPciDevice<
                D,
                <<H as Hypervisor>::Vm as Vm>::MsiSender,
                <<<H as Hypervisor>::Vm as Vm>::IoeventFdRegistry as IoeventFdRegistry>::IoeventFd,
            >,
        >,
        Error,
    >
    where
        P: DevParam<Device = D>,
        D: Virtio,
    {
        let name = Arc::new(name);
        let dev = param.build(name.clone())?;
        let registry = self.board.vm.create_ioeventfd_registry()?;
        let virtio_dev = VirtioDevice::new(
            name.clone(),
            dev,
            self.board.memory.ram_bus(),
            &registry,
            self.board.config.coco.is_some(),
        )?;
        let msi_sender = self.board.vm.create_msi_sender()?;
        let dev = VirtioPciDevice::new(virtio_dev, msi_sender, registry)?;
        let dev = Arc::new(dev);
        let pci_dev = PciDevice::new(name.clone(), dev.clone());
        self.add_pci_dev(pci_dev)?;
        Ok(dev)
    }

    pub fn add_payload(&mut self, payload: Payload) {
        *self.board.payload.write() = Some(payload)
    }

    pub fn boot(&mut self) -> Result<(), Error> {
        let vcpus = self.board.vcpus.read();
        self.board.state.store(STATE_RUNNING, Ordering::Release);
        for (_, boot_tx) in vcpus.iter() {
            boot_tx.send(()).unwrap();
        }
        Ok(())
    }

    pub fn wait(&mut self) -> Vec<Result<()>> {
        self.event_rx.recv().unwrap();
        let vcpus = self.board.vcpus.read();
        for _ in 1..vcpus.len() {
            self.event_rx.recv().unwrap();
        }
        drop(vcpus);
        let mut vcpus = self.board.vcpus.write();
        vcpus
            .drain(..)
            .enumerate()
            .map(|(id, (handle, _))| match handle.join() {
                Err(e) => {
                    log::error!("cannot join vcpu {}: {:?}", id, e);
                    Ok(())
                }
                Ok(r) => r.map_err(Error::Board),
            })
            .collect()
    }
}
