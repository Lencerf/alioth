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

#[cfg(target_arch = "aarch64")]
#[path = "vm_aarch64.rs"]
mod aarch64;
#[cfg(target_arch = "x86_64")]
#[path = "vm_x86_64/vm_x86_64.rs"]
mod x86_64;

#[cfg(target_os = "linux")]
use std::collections::HashMap;
#[cfg(target_os = "linux")]
use std::path::Path;
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use flume::{Receiver, Sender};
use parking_lot::{Condvar, Mutex, RwLock};
use snafu::{ResultExt, Snafu};

#[cfg(target_arch = "aarch64")]
use crate::arch::layout::{PL011_START, PL031_START};
#[cfg(target_arch = "x86_64")]
use crate::arch::layout::{PORT_CMOS_REG, PORT_COM1, PORT_FW_CFG_SELECTOR, PORT_FWDBG};
use crate::board::{Board, BoardConfig};
use crate::device::clock::SystemClock;
#[cfg(target_arch = "x86_64")]
use crate::device::cmos::Cmos;
use crate::device::console::StdioConsole;
#[cfg(target_arch = "x86_64")]
use crate::device::fw_cfg::{FwCfg, FwCfgItemParam};
#[cfg(target_arch = "x86_64")]
use crate::device::fw_dbg::FwDbg;
#[cfg(target_arch = "aarch64")]
use crate::device::pl011::Pl011;
#[cfg(target_arch = "aarch64")]
use crate::device::pl031::Pl031;
#[cfg(target_arch = "x86_64")]
use crate::device::serial::Serial;
use crate::errors::{DebugTrace, trace_error};
use crate::hv::{Hypervisor, IoeventFdRegistry, Vcpu, Vm, VmEntry, VmExit};
#[cfg(target_arch = "x86_64")]
use crate::loader::xen;
use crate::loader::{Executable, InitState, Payload, linux};
use crate::pci::pvpanic::PvPanic;
use crate::pci::{Bdf, Pci};
#[cfg(target_os = "linux")]
use crate::sys::vfio::VfioIommu;
#[cfg(target_os = "linux")]
use crate::vfio::cdev::Cdev;
#[cfg(target_os = "linux")]
use crate::vfio::container::{Container, UpdateContainerMapping};
#[cfg(target_os = "linux")]
use crate::vfio::group::{DevFd, Group};
#[cfg(target_os = "linux")]
use crate::vfio::iommu::{Ioas, Iommu, UpdateIommuIoas};
#[cfg(target_os = "linux")]
use crate::vfio::pci::VfioPciDev;
#[cfg(target_os = "linux")]
use crate::vfio::{CdevParam, ContainerParam, GroupParam, IoasParam};
use crate::virtio::dev::{DevParam, Virtio, VirtioDevice};
use crate::virtio::pci::VirtioPciDevice;

#[trace_error]
#[derive(Snafu, DebugTrace)]
#[snafu(module, context(suffix(false)))]
pub enum Error {
    #[snafu(display("Hypervisor internal error"), context(false))]
    HvError { source: Box<crate::hv::Error> },
    #[snafu(display("Failed to create VCPU-{index} thread"))]
    VcpuThread { index: u16, error: std::io::Error },
    #[snafu(display("Failed to create a console"))]
    CreateConsole { error: Box<crate::device::Error> },
    #[snafu(display("Failed to create fw_cfg device"))]
    FwCfg { error: std::io::Error },
    #[snafu(display("Failed to create a VirtIO device"), context(false))]
    CreateVirtio { source: Box<crate::virtio::Error> },
    #[snafu(display("Guest memory is not backed by sharable file descriptors"))]
    MemNotSharedFd,
    #[cfg(target_os = "linux")]
    #[snafu(display("Failed to create a VFIO device"), context(false))]
    CreateVfio { source: Box<crate::vfio::Error> },
    #[snafu(display("Failed to configure guest memory"), context(false))]
    Memory { source: Box<crate::mem::Error> },
    #[snafu(display("Failed to setup board"), context(false))]
    Board { source: Box<crate::board::Error> },
    #[cfg(target_os = "linux")]
    #[snafu(display("{name:?} already exists"))]
    AlreadyExists { name: Box<str> },
    #[cfg(target_os = "linux")]
    #[snafu(display("{name:?} does not exist"))]
    NotExist { name: Box<str> },
    #[snafu(display("Failed to reset PCI devices"))]
    ResetPci { source: Box<crate::pci::Error> },
    #[snafu(display("Firmware error"), context(false))]
    Firmware { source: Box<crate::firmware::Error> },
    #[snafu(display("Unknown firmware metadata"))]
    UnknownFirmwareMetadata,
    #[snafu(display("Missing payload"))]
    MissingPayload,
    #[snafu(display("Failed to load payload"), context(false))]
    Loader { source: Box<crate::loader::Error> },
    #[snafu(display("Failed to notify the VMM thread"))]
    NotifyVmm,
    #[snafu(display("Another VCPU thread has signaled failure"))]
    PeerFailure,
    #[snafu(display("Unexpected state: {state:?}, want {want:?}"))]
    UnexpectedState { state: State, want: State },
}

type Result<T, E = Error> = std::result::Result<T, E>;

pub struct Machine<H>
where
    H: Hypervisor,
{
    board: Arc<Board<H::Vm>>,
    vcpus: Arc<RwLock<Vec<VcpuHandle>>>,
    sync: Arc<MpSync>,
    event_rx: Receiver<u16>,
    _event_tx: Sender<u16>,
    #[cfg(target_os = "linux")]
    iommu: Mutex<Option<Arc<Iommu>>>,
    #[cfg(target_os = "linux")]
    pub vfio_ioases: Mutex<HashMap<Box<str>, Arc<Ioas>>>,
    #[cfg(target_os = "linux")]
    pub vfio_containers: Mutex<HashMap<Box<str>, Arc<Container>>>,
}

pub type VirtioPciDev<H> = VirtioPciDevice<
    <<H as Hypervisor>::Vm as Vm>::MsiSender,
    <<<H as Hypervisor>::Vm as Vm>::IoeventFdRegistry as IoeventFdRegistry>::IoeventFd,
>;

impl<H> Machine<H>
where
    H: Hypervisor,
{
    pub fn new(hv: &H, config: BoardConfig) -> Result<Self> {
        let board = Arc::new(Board::new(hv, config)?);

        let (event_tx, event_rx) = flume::unbounded();

        let sync = Arc::new(MpSync::new());
        let vcpus = Arc::new(RwLock::new(Vec::new()));
        let mut handles = vcpus.write();
        for index in 0..board.config.cpu.count {
            let event_tx = event_tx.clone();
            let board = board.clone();
            let sync = sync.clone();
            let vcpus = vcpus.clone();
            let handle = thread::Builder::new()
                .name(format!("vcpu_{index}"))
                .spawn(move || vcpu_thread(index, board, vcpus, event_tx, sync))
                .context(error::VcpuThread { index })?;
            if event_rx.recv_timeout(Duration::from_secs(2)).is_err() {
                let err = std::io::ErrorKind::TimedOut.into();
                Err(err).context(error::VcpuThread { index })?;
            }
            log::trace!("VCPU-{index}: created");
            handles.push(handle);
        }
        drop(handles);

        board.arch_init()?;

        let vm = Machine {
            board,
            vcpus,
            sync,
            event_rx,
            _event_tx: event_tx,
            #[cfg(target_os = "linux")]
            iommu: Mutex::new(None),
            #[cfg(target_os = "linux")]
            vfio_ioases: Mutex::new(HashMap::new()),
            #[cfg(target_os = "linux")]
            vfio_containers: Mutex::new(HashMap::new()),
        };

        Ok(vm)
    }

    #[cfg(target_arch = "x86_64")]
    pub fn add_com1(&self) -> Result<(), Error> {
        let io_apic = self.board.arch.io_apic.clone();
        let console = StdioConsole::new().context(error::CreateConsole)?;
        let com1 = Serial::new(PORT_COM1, io_apic, 4, console).context(error::CreateConsole)?;
        self.board.io_devs.write().push((PORT_COM1, Arc::new(com1)));
        Ok(())
    }

    #[cfg(target_arch = "x86_64")]
    pub fn add_cmos(&self) -> Result<(), Error> {
        let mut io_devs = self.board.io_devs.write();
        io_devs.push((PORT_CMOS_REG, Arc::new(Cmos::new(SystemClock))));
        Ok(())
    }

    #[cfg(target_arch = "x86_64")]
    pub fn add_fw_dbg(&self) -> Result<(), Error> {
        let mut io_devs = self.board.io_devs.write();
        io_devs.push((PORT_FWDBG, Arc::new(FwDbg::new())));
        Ok(())
    }

    #[cfg(target_arch = "aarch64")]
    pub fn add_pl011(&self) -> Result<(), Error> {
        let irq_line = self.board.vm.create_irq_sender(1)?;
        let console = StdioConsole::new().context(error::CreateConsole)?;
        let pl011_dev = Pl011::new(PL011_START, irq_line, console).context(error::CreateConsole)?;
        let mut mmio_devs = self.board.mmio_devs.write();
        mmio_devs.push((PL011_START, Arc::new(pl011_dev)));
        Ok(())
    }

    #[cfg(target_arch = "aarch64")]
    pub fn add_pl031(&self) {
        let pl031_dev = Pl031::new(PL031_START, SystemClock);
        let mut mmio_devs = self.board.mmio_devs.write();
        mmio_devs.push((PL031_START, Arc::new(pl031_dev)));
    }

    pub fn add_pci_dev(&self, bdf: Option<Bdf>, dev: Arc<dyn Pci>) -> Result<(), Error> {
        let bdf = if let Some(bdf) = bdf {
            bdf
        } else {
            self.board.pci_bus.reserve(None).unwrap()
        };
        dev.config().get_header().set_bdf(bdf);
        log::info!("{bdf}: device: {}", dev.name());
        self.board.pci_bus.add(bdf, dev);
        Ok(())
    }

    pub fn add_pvpanic(&self) -> Result<(), Error> {
        let dev = PvPanic::new();
        let pci_dev = Arc::new(dev);
        self.add_pci_dev(None, pci_dev)
    }

    #[cfg(target_arch = "x86_64")]
    pub fn add_fw_cfg(
        &self,
        params: impl Iterator<Item = FwCfgItemParam>,
    ) -> Result<Arc<Mutex<FwCfg>>, Error> {
        let items = params
            .map(|p| p.build())
            .collect::<Result<Vec<_>, _>>()
            .context(error::FwCfg)?;
        let fw_cfg = Arc::new(Mutex::new(
            FwCfg::new(self.board.memory.ram_bus(), items).context(error::FwCfg)?,
        ));
        let mut io_devs = self.board.io_devs.write();
        io_devs.push((PORT_FW_CFG_SELECTOR, fw_cfg.clone()));
        *self.board.fw_cfg.lock() = Some(fw_cfg.clone());
        Ok(fw_cfg)
    }

    pub fn add_virtio_dev<D, P>(
        &self,
        name: impl Into<Arc<str>>,
        param: P,
    ) -> Result<Arc<VirtioPciDev<H>>, Error>
    where
        P: DevParam<Device = D>,
        D: Virtio,
    {
        if param.needs_mem_shared_fd() && !self.board.config.mem.has_shared_fd() {
            return error::MemNotSharedFd.fail();
        }
        let name = name.into();
        let bdf = self.board.pci_bus.reserve(None).unwrap();
        let dev = param.build(name.clone())?;
        if let Some(callback) = dev.mem_update_callback() {
            self.board.memory.register_update_callback(callback)?;
        }
        if let Some(callback) = dev.mem_change_callback() {
            self.board.memory.register_change_callback(callback)?;
        }
        let registry = self.board.vm.create_ioeventfd_registry()?;
        let virtio_dev = VirtioDevice::new(
            name.clone(),
            dev,
            self.board.memory.ram_bus(),
            self.board.config.coco.is_some(),
        )?;
        let msi_sender = self.board.vm.create_msi_sender(
            #[cfg(target_arch = "aarch64")]
            u32::from(bdf.0),
        )?;
        let dev = VirtioPciDevice::new(virtio_dev, msi_sender, registry)?;
        let dev = Arc::new(dev);
        self.add_pci_dev(Some(bdf), dev.clone())?;
        Ok(dev)
    }

    pub fn add_payload(&self, payload: Payload) {
        *self.board.payload.write() = Some(payload)
    }

    pub fn boot(&self) -> Result<()> {
        self.resume()
    }

    pub fn resume(&self) -> Result<()> {
        let mut state = self.sync.state.lock();
        if !matches!(state.state, State::Paused) {
            return error::UnexpectedState {
                state: state.state,
                want: State::Paused,
            }
            .fail();
        }
        state.state = State::Running;
        self.sync.cond.notify_all();
        Ok(())
    }

    pub fn pause(&self) -> Result<()> {
        let vcpus = self.vcpus.read();
        let mut state = self.sync.state.lock();
        if !matches!(state.state, State::Running) {
            return error::UnexpectedState {
                state: state.state,
                want: State::Running,
            }
            .fail();
        }
        state.state = State::Paused;
        stop_vcpus(&self.board, None, &vcpus)?;
        Ok(())
    }

    pub fn wait(&self) -> Result<()> {
        self.event_rx.recv().unwrap();
        let vcpus = self.vcpus.read();
        for _ in 1..vcpus.len() {
            self.event_rx.recv().unwrap();
        }
        drop(vcpus);
        let mut vcpus = self.vcpus.write();
        let mut ret = Ok(());
        for (index, handle) in vcpus.drain(..).enumerate() {
            let Ok(r) = handle.join() else {
                log::error!("Cannot join VCPU-{index}");
                continue;
            };
            if ret.is_ok() {
                ret = r;
            }
        }
        ret
    }
}

#[cfg(target_os = "linux")]
impl<H> Machine<H>
where
    H: Hypervisor,
{
    const DEFAULT_NAME: &str = "default";

    pub fn add_vfio_ioas(&self, param: IoasParam) -> Result<Arc<Ioas>, Error> {
        let mut ioases = self.vfio_ioases.lock();
        if ioases.contains_key(&param.name) {
            return error::AlreadyExists { name: param.name }.fail();
        }
        let maybe_iommu = &mut *self.iommu.lock();
        let iommu = if let Some(iommu) = maybe_iommu {
            iommu.clone()
        } else {
            let iommu_path = if let Some(dev_iommu) = &param.dev_iommu {
                dev_iommu
            } else {
                Path::new("/dev/iommu")
            };
            let iommu = Arc::new(Iommu::new(iommu_path)?);
            maybe_iommu.replace(iommu.clone());
            iommu
        };
        let ioas = Arc::new(Ioas::alloc_on(iommu)?);
        let update = Box::new(UpdateIommuIoas { ioas: ioas.clone() });
        self.board.memory.register_change_callback(update)?;
        ioases.insert(param.name, ioas.clone());
        Ok(ioas)
    }

    fn get_ioas(&self, name: Option<&str>) -> Result<Arc<Ioas>> {
        let ioas_name = name.unwrap_or(Self::DEFAULT_NAME);
        if let Some(ioas) = self.vfio_ioases.lock().get(ioas_name) {
            return Ok(ioas.clone());
        };
        if name.is_none() {
            self.add_vfio_ioas(IoasParam {
                name: Self::DEFAULT_NAME.into(),
                dev_iommu: None,
            })
        } else {
            error::NotExist { name: ioas_name }.fail()
        }
    }

    pub fn add_vfio_cdev(&self, name: Arc<str>, param: CdevParam) -> Result<(), Error> {
        let ioas = self.get_ioas(param.ioas.as_deref())?;

        let mut cdev = Cdev::new(&param.path)?;
        cdev.attach_iommu_ioas(ioas.clone())?;

        let bdf = self.board.pci_bus.reserve(None).unwrap();
        let msi_sender = self.board.vm.create_msi_sender(
            #[cfg(target_arch = "aarch64")]
            u32::from(bdf.0),
        )?;
        let dev = VfioPciDev::new(name.clone(), cdev, msi_sender)?;
        self.add_pci_dev(Some(bdf), Arc::new(dev))?;
        Ok(())
    }

    pub fn add_vfio_container(&self, param: ContainerParam) -> Result<Arc<Container>, Error> {
        let mut containers = self.vfio_containers.lock();
        if containers.contains_key(&param.name) {
            return error::AlreadyExists { name: param.name }.fail();
        }
        let vfio_path = if let Some(dev_vfio) = &param.dev_vfio {
            dev_vfio
        } else {
            Path::new("/dev/vfio/vfio")
        };
        let container = Arc::new(Container::new(vfio_path)?);
        let update = Box::new(UpdateContainerMapping {
            container: container.clone(),
        });
        self.board.memory.register_change_callback(update)?;
        containers.insert(param.name, container.clone());
        Ok(container)
    }

    fn get_container(&self, name: Option<&str>) -> Result<Arc<Container>> {
        let container_name = name.unwrap_or(Self::DEFAULT_NAME);
        if let Some(container) = self.vfio_containers.lock().get(container_name) {
            return Ok(container.clone());
        }
        if name.is_none() {
            self.add_vfio_container(ContainerParam {
                name: Self::DEFAULT_NAME.into(),
                dev_vfio: None,
            })
        } else {
            error::NotExist {
                name: container_name,
            }
            .fail()
        }
    }

    pub fn add_vfio_devs_in_group(&self, name: &str, param: GroupParam) -> Result<()> {
        let container = self.get_container(param.container.as_deref())?;
        let mut group = Group::new(&param.path)?;
        group.attach(container, VfioIommu::TYPE1_V2)?;

        let group = Arc::new(group);
        for device in param.devices {
            let devfd = DevFd::new(group.clone(), &device)?;
            let name = format!("{name}-{device}");
            self.add_vfio_devfd(name.into(), devfd)?;
        }

        Ok(())
    }

    fn add_vfio_devfd(&self, name: Arc<str>, devfd: DevFd) -> Result<()> {
        let bdf = self.board.pci_bus.reserve(None).unwrap();
        let msi_sender = self.board.vm.create_msi_sender(
            #[cfg(target_arch = "aarch64")]
            u32::from(bdf.0),
        )?;
        let dev = VfioPciDev::new(name.clone(), devfd, msi_sender)?;
        self.add_pci_dev(Some(bdf), Arc::new(dev))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Paused,
    Running,
    Shutdown,
    RebootPending,
}

struct MpState {
    state: State,
    fatal: bool,
    count: u16,
}

struct MpSync {
    state: Mutex<MpState>,
    cond: Condvar,
}

impl MpSync {
    pub fn new() -> Self {
        Self {
            state: Mutex::new(MpState {
                state: State::Paused,
                fatal: false,
                count: 0,
            }),
            cond: Condvar::new(),
        }
    }
}

struct Context<V: Vm> {
    board: Arc<Board<V>>,
    vcpus: Arc<RwLock<Vec<VcpuHandle>>>,
    sync: Arc<MpSync>,
    index: u16,
    event_tx: Sender<u16>,
    vcpu: <V as Vm>::Vcpu,
}

type VcpuHandle = JoinHandle<Result<()>>;

fn notify_vmm(event_tx: &Sender<u16>, index: u16) -> Result<()> {
    if event_tx.send(index).is_err() {
        error::NotifyVmm.fail()
    } else {
        Ok(())
    }
}

impl<V: Vm> Context<V> {
    pub fn new(
        index: u16,
        board: Arc<Board<V>>,
        vcpus: Arc<RwLock<Vec<VcpuHandle>>>,
        sync: Arc<MpSync>,
        event_tx: Sender<u16>,
    ) -> Result<Self> {
        let identity = board.encode_cpu_identity(index);
        let vcpu = board.vm.create_vcpu(index, identity)?;

        Ok(Self {
            board,
            vcpus,
            sync,
            index,
            vcpu,
            event_tx,
        })
    }

    fn notify_vmm(&self) -> Result<()> {
        notify_vmm(&self.event_tx, self.index)
    }

    fn sync_vcpus(&self, vcpus: &[VcpuHandle]) -> Result<()> {
        let mut state = self.sync.state.lock();
        if state.fatal {
            return error::PeerFailure.fail();
        }

        state.count += 1;
        if state.count == vcpus.len() as u16 {
            state.count = 0;
            self.sync.cond.notify_all();
        } else {
            self.sync.cond.wait(&mut state)
        }

        if state.fatal {
            return error::PeerFailure.fail();
        }

        Ok(())
    }

    fn load_payload(&self) -> Result<InitState> {
        let payload = self.board.payload.read();
        let Some(payload) = payload.as_ref() else {
            return error::MissingPayload.fail();
        };

        if let Some(fw) = payload.firmware.as_ref() {
            return self.setup_firmware(fw, payload);
        }

        let Some(exec) = &payload.executable else {
            return error::MissingPayload.fail();
        };
        let mem_regions = self.board.memory.mem_region_entries();
        let init_state = match exec {
            Executable::Linux(image) => linux::load(
                &self.board.memory.ram_bus(),
                &mem_regions,
                image.as_ref(),
                payload.cmdline.as_deref(),
                payload.initramfs.as_deref(),
            ),
            #[cfg(target_arch = "x86_64")]
            Executable::Pvh(image) => xen::load(
                &self.board.memory.ram_bus(),
                &mem_regions,
                image.as_ref(),
                payload.cmdline.as_deref(),
                payload.initramfs.as_deref(),
            ),
        }?;
        Ok(init_state)
    }

    fn boot_init_sync(&mut self) -> Result<()> {
        let vcpus = self.vcpus.clone();
        let vcpus = vcpus.read();
        if self.index == 0 {
            self.board.init_devices()?;
            let init_state = self.load_payload()?;
            self.init_boot_vcpu(&init_state)?;
            self.board.create_firmware_data(&init_state)?;
        }
        self.init_ap(&vcpus)?;
        self.coco_finalize(&vcpus)?;
        self.sync_vcpus(&vcpus)
    }

    fn vcpu_loop(&mut self) -> Result<State> {
        let mut vm_entry = VmEntry::None;
        loop {
            let vm_exit = self.vcpu.run(vm_entry)?;
            vm_entry = match vm_exit {
                #[cfg(target_arch = "x86_64")]
                VmExit::Io { port, write, size } => {
                    self.board.memory.handle_io(port, write, size)?
                }
                VmExit::Mmio { addr, write, size } => {
                    self.board.memory.handle_mmio(addr, write, size)?
                }
                VmExit::Shutdown => break Ok(State::Shutdown),
                VmExit::Reboot => break Ok(State::RebootPending),
                VmExit::Paused => break Ok(State::Paused),
                VmExit::Interrupted => {
                    let state = self.sync.state.lock();
                    match state.state {
                        State::Shutdown => VmEntry::Shutdown,
                        State::RebootPending => VmEntry::Reboot,
                        State::Paused => VmEntry::Pause,
                        State::Running => VmEntry::None,
                    }
                }
                VmExit::ConvertMemory { gpa, size, private } => {
                    self.board.memory.mark_private_memory(gpa, size, private)?;
                    VmEntry::None
                }
            };
        }
    }

    fn run(&mut self) -> Result<()> {
        self.init_vcpu()?;

        'reboot: loop {
            let mut mp_state = self.sync.state.lock();
            loop {
                match mp_state.state {
                    State::Paused => self.sync.cond.wait(&mut mp_state),
                    State::Running => break,
                    State::Shutdown => break 'reboot Ok(()),
                    State::RebootPending => mp_state.state = State::Running,
                }
            }
            drop(mp_state);

            self.boot_init_sync()?;

            let request = 'pause: loop {
                let request = self.vcpu_loop();

                let vcpus = self.vcpus.read();
                let mut state = self.sync.state.lock();
                if state.state == State::Running {
                    state.state = match request {
                        Ok(State::RebootPending) => State::RebootPending,
                        Ok(State::Paused) => State::Paused,
                        _ => State::Shutdown,
                    };
                    log::trace!("VCPU-{}: change state to {:?}", self.index, state.state);
                    stop_vcpus(&self.board, Some(self.index), &vcpus)?;
                }
                loop {
                    match state.state {
                        State::Running => break,
                        State::Paused => self.sync.cond.wait(&mut state),
                        State::RebootPending | State::Shutdown => break 'pause request,
                    }
                }
            };

            if self.index == 0 {
                let board = &self.board;
                board.pci_bus.segment.reset().context(error::ResetPci)?;
                board.memory.reset()?;
            }
            self.reset_vcpu()?;

            request?;

            let vcpus = self.vcpus.read();
            self.sync_vcpus(&vcpus)?;
        }
    }
}

fn vcpu_thread_<V: Vm>(
    index: u16,
    board: Arc<Board<V>>,
    vcpus: Arc<RwLock<Vec<VcpuHandle>>>,
    event_tx: Sender<u16>,
    sync: Arc<MpSync>,
) -> Result<()> {
    let mut ctx = Context::new(index, board, vcpus, sync, event_tx)?;
    ctx.notify_vmm()?;
    ctx.run()
}

fn vcpu_thread<V: Vm>(
    index: u16,
    board: Arc<Board<V>>,
    vcpus: Arc<RwLock<Vec<VcpuHandle>>>,
    event_tx: Sender<u16>,
    sync: Arc<MpSync>,
) -> Result<()> {
    let ret = vcpu_thread_(index, board, vcpus, event_tx.clone(), sync.clone());

    let _ = notify_vmm(&event_tx, index);

    if matches!(ret, Ok(_) | Err(Error::PeerFailure { .. })) {
        return Ok(());
    }

    log::warn!("VCPU-{index} reported error {ret:?}, unblocking other VCPUs...");
    let mut mp_state = sync.state.lock();
    mp_state.fatal = true;
    if mp_state.count > 0 {
        sync.cond.notify_all();
    }
    ret
}

fn stop_vcpus<V: Vm>(board: &Board<V>, current: Option<u16>, vcpus: &[VcpuHandle]) -> Result<()> {
    for (index, handle) in vcpus.iter().enumerate() {
        let index = index as u16;
        if let Some(current) = current {
            if current == index {
                continue;
            }
            log::info!("VCPU-{current}: stopping VCPU-{index}");
        } else {
            log::info!("Stopping VCPU-{index}");
        }
        let identity = board.encode_cpu_identity(index);
        board.vm.stop_vcpu(identity, handle)?;
    }
    Ok(())
}
