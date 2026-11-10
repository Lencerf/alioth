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

#[cfg(target_os = "linux")]
use std::collections::HashMap;
use std::io::ErrorKind;
#[cfg(target_os = "linux")]
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use std::{mem, thread};

#[cfg(target_os = "linux")]
use parking_lot::Mutex;
use snafu::{ResultExt, Snafu};

#[cfg(target_arch = "aarch64")]
use crate::arch::layout::{PL011_START, PL031_START};
#[cfg(target_arch = "x86_64")]
use crate::arch::layout::{PORT_CMOS_REG, PORT_COM1, PORT_FW_CFG_SELECTOR, PORT_FWDBG};
use crate::board::{Board, BoardSpec};
use crate::cpu::{Context, State, VcpuHandle, stop_vcpus, vcpu_thread};
use crate::device::MmioDev;
use crate::device::clock::SystemClock;
#[cfg(target_arch = "x86_64")]
use crate::device::cmos::Cmos;
use crate::device::console::{ConsoleSpec, LogConsole, PtyConsole, StdioConsole};
#[cfg(target_arch = "x86_64")]
use crate::device::fw_cfg::{FwCfg, FwCfgItemSpec};
#[cfg(target_arch = "x86_64")]
use crate::device::fw_dbg::FwDbg;
#[cfg(target_arch = "aarch64")]
use crate::device::pl011::Pl011;
#[cfg(target_arch = "aarch64")]
use crate::device::pl031::Pl031;
#[cfg(target_arch = "x86_64")]
use crate::device::serial::Serial;
use crate::errors::{DebugTrace, trace_error};
use crate::hv::{self, Hypervisor, Vm};
use crate::loader::PayloadSpec;
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
use crate::vfio::user::conn::VfioUserSession;
#[cfg(target_os = "linux")]
use crate::vfio::user::device::{UpdateVfioUserMapping, VfioUserDevice};
#[cfg(target_os = "linux")]
use crate::vfio::{VfioCdevSpec, VfioContainerSpec, VfioGroupSpec, VfioIoasSpec, VfioUserSpec};
#[cfg(target_os = "linux")]
use std::os::unix::net::UnixStream;

#[cfg(target_os = "linux")]
use crate::virtio::DeviceId;
use crate::virtio::dev::{DevSpec, Virtio, VirtioDevice};
use crate::virtio::pci::VirtioPciDevice;
#[cfg(target_os = "linux")]
use crate::virtio::vu::frontend::VuFrontendSpec;
use crate::vm::config::{BlkSpec, FsSpec, NetSpec, VmSpec, VsockSpec};

pub mod config;

#[trace_error]
#[derive(Snafu, DebugTrace)]
#[snafu(module, context(suffix(false)))]
pub enum Error {
    #[snafu(display("Hypervisor internal error"), context(false))]
    HvError { source: Box<crate::hv::Error> },
    #[snafu(display("Failed to create VCPU-{index} thread"))]
    CreateVcpu { index: u16, error: std::io::Error },
    #[snafu(display("Failed to stop VCPUs"))]
    StopVcpus { source: Box<crate::cpu::Error> },
    #[snafu(display("VCPU-{index} thread exited unexpectedly"))]
    VcpuExit {
        index: u16,
        source: Box<crate::cpu::Error>,
    },
    #[snafu(display("Failed to create a console"))]
    CreateConsole { error: crate::device::Error },
    #[snafu(display("Failed to configure firmware"))]
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
    #[snafu(display("Unexpected state: {state:?}, want {want:?}"))]
    UnexpectedState {
        state: State,
        want: &'static [State],
    },
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

pub struct Machine<H>
where
    H: Hypervisor,
{
    ctx: Arc<Context<H::Vm>>,

    #[cfg(target_os = "linux")]
    iommu: Mutex<Option<Arc<Iommu>>>,
    #[cfg(target_os = "linux")]
    pub vfio_ioases: Mutex<HashMap<Box<str>, Arc<Ioas>>>,
    #[cfg(target_os = "linux")]
    pub vfio_containers: Mutex<HashMap<Box<str>, Arc<Container>>>,
}

pub type VirtioPciDev<H> = VirtioPciDevice<<<H as Hypervisor>::Vm as Vm>::MsiSender>;

impl<H> Machine<H>
where
    H: Hypervisor,
{
    pub fn new(hv: &H, spec: BoardSpec) -> Result<Self> {
        // On errors, dropping `vm` ends the vCPU threads created so far.
        let vm = Machine {
            ctx: Arc::new(Context::new(Board::new(hv, spec)?)),
            #[cfg(target_os = "linux")]
            iommu: Mutex::new(None),
            #[cfg(target_os = "linux")]
            vfio_ioases: Mutex::new(HashMap::new()),
            #[cfg(target_os = "linux")]
            vfio_containers: Mutex::new(HashMap::new()),
        };

        let (event_tx, event_rx) = flume::unbounded();
        for index in 0..vm.ctx.board.spec.cpu.count {
            let event_tx = event_tx.clone();
            let ctx = vm.ctx.clone();
            let handle = thread::Builder::new()
                .name(format!("vcpu_{index}"))
                .spawn(move || vcpu_thread(index, ctx, event_tx))
                .context(error::CreateVcpu { index })?;
            vm.ctx.vcpus.write().push(VcpuHandle { thread: handle });
            if event_rx.recv_timeout(Duration::from_secs(2)).is_err() {
                let err = std::io::ErrorKind::TimedOut.into();
                Err(err).context(error::CreateVcpu { index })?;
            }
        }

        vm.ctx.board.arch_init()?;

        Ok(vm)
    }

    /// Creates a VM from `spec`.
    pub fn create(hv: &H, spec: VmSpec) -> Result<Self> {
        let vm = Self::new(hv, spec.board)?;

        #[cfg(target_arch = "x86_64")]
        vm.add_com1(&spec.console)?;
        #[cfg(target_arch = "aarch64")]
        vm.add_pl011(&spec.console)?;
        #[cfg(target_arch = "aarch64")]
        vm.add_pl031();

        if spec.pvpanic {
            vm.add_pvpanic()?;
        }

        #[cfg(target_arch = "x86_64")]
        vm.add_cmos()?;
        #[cfg(target_arch = "x86_64")]
        if spec.payload.firmware.is_some() {
            vm.add_fw_dbg()?;
        }

        #[cfg(target_arch = "x86_64")]
        if spec.payload.firmware.is_some() || !spec.fw_cfg.is_empty() {
            vm.add_fw_cfg(spec.fw_cfg.into_iter())?;
        };

        if let Some(entropy_spec) = spec.entropy {
            vm.add_virtio_dev("virtio-entropy", entropy_spec)?;
        }

        for (index, net_spec) in spec.net.into_iter().enumerate() {
            match net_spec {
                #[cfg(target_os = "linux")]
                NetSpec::Tap(tap_spec) => {
                    vm.add_virtio_dev(format!("virtio-net-{index}"), tap_spec)
                }
                #[cfg(target_os = "linux")]
                NetSpec::Vu(sock) => {
                    let vu_spec = VuFrontendSpec {
                        id: DeviceId::NET,
                        socket: sock.socket,
                    };
                    vm.add_virtio_dev(format!("vu-net-{index}"), vu_spec)
                }
                #[cfg(target_os = "macos")]
                NetSpec::Vmnet(vmnet_spec) => {
                    vm.add_virtio_dev(format!("virtio-net-{index}"), vmnet_spec)
                }
            }?;
        }

        for (index, blk_spec) in spec.blk.into_iter().enumerate() {
            match blk_spec {
                BlkSpec::File(file_spec) => {
                    vm.add_virtio_dev(format!("virtio-blk-{index}"), file_spec)
                }
                #[cfg(target_os = "linux")]
                BlkSpec::Vu(s) => {
                    let vu_spec = VuFrontendSpec {
                        id: DeviceId::BLOCK,
                        socket: s.socket,
                    };
                    vm.add_virtio_dev(format!("vu-blk-{index}"), vu_spec)
                }
            }?;
        }

        for (index, fs_spec) in spec.fs.into_iter().enumerate() {
            match fs_spec {
                FsSpec::Dir(dir_spec) => vm.add_virtio_dev(format!("virtio-fs-{index}"), dir_spec),
                #[cfg(target_os = "linux")]
                FsSpec::Vu(vu_fs_spec) => vm.add_virtio_dev(format!("vu-fs-{index}"), vu_fs_spec),
            }?;
        }

        if let Some(vsock_spec) = spec.vsock {
            match vsock_spec {
                #[cfg(target_os = "linux")]
                VsockSpec::Vhost(vhost_spec) => vm.add_virtio_dev("vhost-vsock", vhost_spec),
                VsockSpec::Uds(uds_spec) => vm.add_virtio_dev("uds-vsock", uds_spec),
                #[cfg(target_os = "linux")]
                VsockSpec::Vu(s) => {
                    let vu_spec = VuFrontendSpec {
                        id: DeviceId::SOCKET,
                        socket: s.socket,
                    };
                    vm.add_virtio_dev("vu-vsock", vu_spec)
                }
            }?;
        }

        if let Some(balloon_spec) = spec.balloon {
            vm.add_virtio_dev("virtio-balloon", balloon_spec)?;
        }

        #[cfg(target_os = "linux")]
        for ioas_spec in spec.vfio_ioas.into_iter() {
            vm.add_vfio_ioas(ioas_spec)?;
        }
        #[cfg(target_os = "linux")]
        for (index, cdev_spec) in spec.vfio_cdev.into_iter().enumerate() {
            vm.add_vfio_cdev(format!("vfio-{index}").into(), cdev_spec)?;
        }
        #[cfg(target_os = "linux")]
        for (index, user_spec) in spec.vfio_user.into_iter().enumerate() {
            vm.add_vfio_user_dev(format!("vfio-user-{index}").into(), user_spec)?;
        }

        #[cfg(target_os = "linux")]
        for container_spec in spec.vfio_container.into_iter() {
            vm.add_vfio_container(container_spec)?;
        }
        #[cfg(target_os = "linux")]
        for (index, group_spec) in spec.vfio_group.into_iter().enumerate() {
            vm.add_vfio_devs_in_group(&index.to_string(), group_spec)?;
        }

        vm.add_payload(spec.payload);

        Ok(vm)
    }

    #[cfg(target_arch = "x86_64")]
    pub fn add_com1(&self, spec: &ConsoleSpec) -> Result<(), Error> {
        let io_apic = self.ctx.board.arch.io_apic.clone();
        let com1: Arc<dyn MmioDev> = match spec {
            ConsoleSpec::Stdio => {
                let console = StdioConsole::new().context(error::CreateConsole)?;
                Arc::new(Serial::new(PORT_COM1, io_apic, 4, console).context(error::CreateConsole)?)
            }
            ConsoleSpec::Log(spec) => {
                let console = LogConsole::new(spec).context(error::CreateConsole)?;
                Arc::new(Serial::new(PORT_COM1, io_apic, 4, console).context(error::CreateConsole)?)
            }
            ConsoleSpec::Pty(spec) => {
                let console = PtyConsole::new(spec).context(error::CreateConsole)?;
                Arc::new(Serial::new(PORT_COM1, io_apic, 4, console).context(error::CreateConsole)?)
            }
        };
        let mut io_devs = self.ctx.board.io_devs.write();
        io_devs.push((PORT_COM1, com1));
        Ok(())
    }

    #[cfg(target_arch = "x86_64")]
    pub fn add_cmos(&self) -> Result<(), Error> {
        let mut io_devs = self.ctx.board.io_devs.write();
        io_devs.push((PORT_CMOS_REG, Arc::new(Cmos::new(SystemClock))));
        Ok(())
    }

    #[cfg(target_arch = "x86_64")]
    pub fn add_fw_dbg(&self) -> Result<(), Error> {
        let mut io_devs = self.ctx.board.io_devs.write();
        io_devs.push((PORT_FWDBG, Arc::new(FwDbg::new())));
        Ok(())
    }

    #[cfg(target_arch = "aarch64")]
    pub fn add_pl011(&self, spec: &ConsoleSpec) -> Result<(), Error> {
        let irq_line = self.ctx.board.vm.create_irq_sender(1)?;
        let pl011_dev: Arc<dyn MmioDev> = match spec {
            ConsoleSpec::Stdio => {
                let console = StdioConsole::new().context(error::CreateConsole)?;
                Arc::new(Pl011::new(PL011_START, irq_line, console).context(error::CreateConsole)?)
            }
            ConsoleSpec::Log(spec) => {
                let console = LogConsole::new(spec).context(error::CreateConsole)?;
                Arc::new(Pl011::new(PL011_START, irq_line, console).context(error::CreateConsole)?)
            }
            ConsoleSpec::Pty(spec) => {
                let console = PtyConsole::new(spec).context(error::CreateConsole)?;
                Arc::new(Pl011::new(PL011_START, irq_line, console).context(error::CreateConsole)?)
            }
        };
        let mut mmio_devs = self.ctx.board.mmio_devs.write();
        mmio_devs.push((PL011_START, pl011_dev));
        Ok(())
    }

    #[cfg(target_arch = "aarch64")]
    pub fn add_pl031(&self) {
        let pl031_dev = Pl031::new(PL031_START, SystemClock);
        let mut mmio_devs = self.ctx.board.mmio_devs.write();
        mmio_devs.push((PL031_START, Arc::new(pl031_dev)));
    }

    pub fn add_pci_dev(&self, bdf: Option<Bdf>, dev: Arc<dyn Pci>) -> Result<(), Error> {
        let bdf = if let Some(bdf) = bdf {
            bdf
        } else {
            self.ctx.board.pci_bus.reserve(None).unwrap()
        };
        dev.config().get_header().set_bdf(bdf);
        log::info!("{bdf}: device: {}", dev.name());
        self.ctx.board.pci_bus.add(bdf, dev);
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
        specs: impl Iterator<Item = FwCfgItemSpec>,
    ) -> Result<Arc<Mutex<FwCfg>>, Error> {
        let items = specs
            .map(|spec| spec.build())
            .collect::<Result<Vec<_>, _>>()
            .context(error::FwCfg)?;
        let fw_cfg = Arc::new(Mutex::new(
            FwCfg::new(self.ctx.board.memory.ram_bus(), items).context(error::FwCfg)?,
        ));
        let mut io_devs = self.ctx.board.io_devs.write();
        io_devs.push((PORT_FW_CFG_SELECTOR, fw_cfg.clone()));
        *self.ctx.board.fw_cfg.lock() = Some(fw_cfg.clone());
        Ok(fw_cfg)
    }

    /// Creates a notifier registry, or returns `None` if the hypervisor has no
    /// notifier support and every access must exit to the VMM.
    fn create_notifier_registry(
        &self,
        name: &str,
    ) -> Result<Option<<H::Vm as Vm>::NotifierRegistry>, Error> {
        match self.ctx.board.vm.create_notifier_registry() {
            Ok(registry) => Ok(Some(registry)),
            Err(hv::Error::Notifier { error, .. }) if error.kind() == ErrorKind::Unsupported => {
                log::debug!("{name}: notifiers are not supported");
                Ok(None)
            }
            Err(e) => Err(e.into()),
        }
    }

    pub fn add_virtio_dev<D, P>(
        &self,
        name: impl Into<Arc<str>>,
        spec: P,
    ) -> Result<Arc<VirtioPciDev<H>>, Error>
    where
        P: DevSpec<Device = D>,
        D: Virtio,
    {
        if spec.needs_mem_shared_fd() && !self.ctx.board.spec.mem.has_shared_fd() {
            return error::MemNotSharedFd.fail();
        }
        let name = name.into();
        let bdf = self.ctx.board.pci_bus.reserve(None).unwrap();
        let dev = spec.build(name.clone())?;
        if let Some(callback) = dev.mem_update_callback() {
            self.ctx.board.memory.register_update_callback(callback)?;
        }
        if let Some(callback) = dev.mem_change_callback() {
            self.ctx.board.memory.register_change_callback(callback)?;
        }
        let registry = self.create_notifier_registry(&name)?;
        let virtio_dev = VirtioDevice::new(
            name.clone(),
            dev,
            self.ctx.board.memory.ram_bus(),
            self.ctx.board.spec.coco.is_some(),
        )?;
        let kick = virtio_dev.mem_update_kick();
        self.ctx.board.memory.register_update_callback(kick)?;
        let msi_sender = self.ctx.board.vm.create_msi_sender(
            #[cfg(target_arch = "aarch64")]
            u32::from(bdf.0),
        )?;
        let dev = VirtioPciDevice::new(virtio_dev, msi_sender, registry)?;
        let dev = Arc::new(dev);
        self.add_pci_dev(Some(bdf), dev.clone())?;
        Ok(dev)
    }

    pub fn add_payload(&self, payload: PayloadSpec) {
        *self.ctx.board.payload.write() = Some(payload)
    }
}

#[cfg(target_os = "linux")]
impl<H> Machine<H>
where
    H: Hypervisor,
{
    const DEFAULT_NAME: &str = "default";

    pub fn add_vfio_ioas(&self, spec: VfioIoasSpec) -> Result<Arc<Ioas>, Error> {
        let mut ioases = self.vfio_ioases.lock();
        if ioases.contains_key(&spec.name) {
            return error::AlreadyExists { name: spec.name }.fail();
        }
        let maybe_iommu = &mut *self.iommu.lock();
        let iommu = if let Some(iommu) = maybe_iommu {
            iommu.clone()
        } else {
            let iommu_path = if let Some(dev_iommu) = &spec.dev_iommu {
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
        self.ctx.board.memory.register_change_callback(update)?;
        ioases.insert(spec.name, ioas.clone());
        Ok(ioas)
    }

    fn get_ioas(&self, name: Option<&str>) -> Result<Arc<Ioas>> {
        let ioas_name = name.unwrap_or(Self::DEFAULT_NAME);
        if let Some(ioas) = self.vfio_ioases.lock().get(ioas_name) {
            return Ok(ioas.clone());
        };
        if name.is_none() {
            self.add_vfio_ioas(VfioIoasSpec {
                name: Self::DEFAULT_NAME.into(),
                dev_iommu: None,
            })
        } else {
            error::NotExist { name: ioas_name }.fail()
        }
    }

    pub fn add_vfio_cdev(&self, name: Arc<str>, spec: VfioCdevSpec) -> Result<(), Error> {
        let ioas = self.get_ioas(spec.ioas.as_deref())?;

        let mut cdev = Cdev::new(&spec.path)?;
        cdev.attach_iommu_ioas(ioas.clone())?;

        let bdf = self.ctx.board.pci_bus.reserve(None).unwrap();
        let msi_sender = self.ctx.board.vm.create_msi_sender(
            #[cfg(target_arch = "aarch64")]
            u32::from(bdf.0),
        )?;
        let none = None::<<H::Vm as Vm>::NotifierRegistry>;
        let dev = VfioPciDev::new(name.clone(), cdev, msi_sender, none)?;
        self.add_pci_dev(Some(bdf), Arc::new(dev))?;
        Ok(())
    }

    #[cfg(target_os = "linux")]
    pub fn add_vfio_user_dev(&self, name: Arc<str>, spec: VfioUserSpec) -> Result<(), Error> {
        let stream = UnixStream::connect(&spec.socket).map_err(crate::vfio::Error::from)?;

        let session = Arc::new(VfioUserSession::new(stream));
        session
            .negotiate_version()
            .map_err(crate::vfio::Error::from)?;

        let dev = VfioUserDevice::new(session.clone())?;

        let bdf = self.ctx.board.pci_bus.reserve(None).unwrap();
        let msi_sender = self.ctx.board.vm.create_msi_sender(
            #[cfg(target_arch = "aarch64")]
            u32::from(bdf.0),
        )?;
        let registry = self.create_notifier_registry(&name)?;
        let dev = VfioPciDev::new(name.clone(), dev, msi_sender, registry)?;
        self.add_pci_dev(Some(bdf), Arc::new(dev))?;

        let update = Box::new(UpdateVfioUserMapping::new(session.clone()));
        self.ctx.board.memory.register_change_callback(update)?;
        Ok(())
    }

    pub fn add_vfio_container(&self, spec: VfioContainerSpec) -> Result<Arc<Container>, Error> {
        let mut containers = self.vfio_containers.lock();
        if containers.contains_key(&spec.name) {
            return error::AlreadyExists { name: spec.name }.fail();
        }
        let vfio_path = if let Some(dev_vfio) = &spec.dev_vfio {
            dev_vfio
        } else {
            Path::new("/dev/vfio/vfio")
        };
        let container = Arc::new(Container::new(vfio_path)?);
        let update = Box::new(UpdateContainerMapping {
            container: container.clone(),
        });
        self.ctx.board.memory.register_change_callback(update)?;
        containers.insert(spec.name, container.clone());
        Ok(container)
    }

    fn get_container(&self, name: Option<&str>) -> Result<Arc<Container>> {
        let container_name = name.unwrap_or(Self::DEFAULT_NAME);
        if let Some(container) = self.vfio_containers.lock().get(container_name) {
            return Ok(container.clone());
        }
        if name.is_none() {
            self.add_vfio_container(VfioContainerSpec {
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

    pub fn add_vfio_devs_in_group(&self, name: &str, spec: VfioGroupSpec) -> Result<()> {
        let container = self.get_container(spec.container.as_deref())?;
        let mut group = Group::new(&spec.path)?;
        group.attach(container, VfioIommu::TYPE1_V2)?;

        let group = Arc::new(group);
        for device in spec.devices {
            let devfd = DevFd::new(group.clone(), &device)?;
            let name = format!("{name}-{device}");
            self.add_vfio_devfd(name.into(), devfd)?;
        }

        Ok(())
    }

    fn add_vfio_devfd(&self, name: Arc<str>, devfd: DevFd) -> Result<()> {
        let bdf = self.ctx.board.pci_bus.reserve(None).unwrap();
        let msi_sender = self.ctx.board.vm.create_msi_sender(
            #[cfg(target_arch = "aarch64")]
            u32::from(bdf.0),
        )?;
        let none = None::<<H::Vm as Vm>::NotifierRegistry>;
        let dev = VfioPciDev::new(name.clone(), devfd, msi_sender, none)?;
        self.add_pci_dev(Some(bdf), Arc::new(dev))
    }
}

impl<H> Machine<H>
where
    H: Hypervisor,
{
    /// Returns the state of the VM.
    pub fn state(&self) -> State {
        self.ctx.sync.lock().state
    }

    /// Boots a stopped VM.
    pub fn boot(&self) -> Result<()> {
        let mut sync = self.ctx.sync.lock();
        check_state(sync.state, &[State::Stopped])?;
        sync.state = State::Running;
        self.ctx.cond.notify_all();
        Ok(())
    }

    pub fn resume(&self) -> Result<()> {
        let mut sync = self.ctx.sync.lock();
        check_state(sync.state, &[State::Paused])?;
        sync.state = State::Running;
        self.ctx.cond.notify_all();
        Ok(())
    }

    pub fn pause(&self) -> Result<()> {
        let vcpus = self.ctx.vcpus.read();
        let mut sync = self.ctx.sync.lock();
        check_state(sync.state, &[State::Running])?;
        sync.state = State::Paused;
        stop_vcpus(&self.ctx.board, None, &vcpus).context(error::StopVcpus)?;
        Ok(())
    }

    /// Powers off the VM. The VM is reset and stops afterwards.
    pub fn shutdown(&self) -> Result<()> {
        let want = &[
            State::Running,
            State::Paused,
            State::RebootPending,
            State::ShutdownPending,
        ];
        self.request_reset(want, State::ShutdownPending)
    }

    /// Reboots the VM. The VM is reset and boots again afterwards.
    pub fn reboot(&self) -> Result<()> {
        let want = &[State::Running, State::Paused, State::RebootPending];
        self.request_reset(want, State::RebootPending)
    }

    fn request_reset(&self, want: &'static [State], next: State) -> Result<()> {
        let vcpus = self.ctx.vcpus.read();
        let mut sync = self.ctx.sync.lock();
        let state = sync.state;
        check_state(state, want)?;
        sync.state = next;
        if state == State::Running {
            stop_vcpus(&self.ctx.board, None, &vcpus).context(error::StopVcpus)?;
        } else {
            self.ctx.cond.notify_all();
        }
        Ok(())
    }

    /// Blocks until the VM stops, or its vCPU threads exit.
    pub fn wait_stopped(&self) {
        let mut sync = self.ctx.sync.lock();
        self.ctx.cond.wait_while(&mut sync, |s| {
            !matches!(s.state, State::Stopped | State::Terminated)
        });
    }

    /// Ends the vCPU threads of a stopped VM.
    ///
    /// Returns the first error that the vCPU threads reported.
    pub fn terminate(&self) -> Result<()> {
        let mut sync = self.ctx.sync.lock();
        check_state(sync.state, &[State::Stopped, State::Terminated])?;
        sync.state = State::Terminated;
        self.ctx.cond.notify_all();
        drop(sync);

        let vcpus = mem::take(&mut *self.ctx.vcpus.write());
        let mut ret = Ok(());
        for (index, handle) in vcpus.into_iter().enumerate() {
            let Ok(r) = handle.thread.join() else {
                log::error!("Cannot join VCPU-{index}");
                continue;
            };
            if ret.is_ok() {
                ret = r.context(error::VcpuExit {
                    index: index as u16,
                });
            }
        }
        ret
    }
}

impl<H> Drop for Machine<H>
where
    H: Hypervisor,
{
    fn drop(&mut self) {
        match self.shutdown() {
            Ok(()) => {}
            Err(Error::UnexpectedState {
                state: State::Stopped | State::Terminated,
                ..
            }) => {}
            Err(e) => log::error!("Failed to shut down the VM: {e:?}"),
        }
        self.wait_stopped();
        if let Err(e) = self.terminate() {
            log::error!("Failed to terminate the VM: {e:?}");
        }
    }
}

#[track_caller]
fn check_state(state: State, want: &'static [State]) -> Result<()> {
    if want.contains(&state) {
        Ok(())
    } else {
        error::UnexpectedState { state, want }.fail()
    }
}

#[cfg(all(test, target_os = "linux", target_arch = "x86_64"))]
#[path = "vm_test.rs"]
mod tests;
