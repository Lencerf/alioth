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

//! Virtio-pgalloc device backed by the host kernel vhost-pgalloc module.
//!
//! The datapath (requestq/eventq) is fully offloaded to the kernel vhost
//! driver; this device only emulates the transport: config space, feature
//! negotiation and queue wiring (kicks via ioeventfd, interrupts via irqfd).

use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::thread::JoinHandle;

use flume::Receiver;
use libc::{EFD_CLOEXEC, EFD_NONBLOCK, eventfd};
use mio::event::Event;
use mio::unix::SourceFd;
use mio::{Interest, Registry, Token};
use serde::Deserialize;
use serde_aco::Help;
use zerocopy::{FromBytes, Immutable, IntoBytes};

use crate::hv::IoeventFd;
use crate::mem::LayoutUpdated;
use crate::mem::mapped::RamBus;
use crate::sync::notifier::Notifier;
use crate::sys::vhost::{VHOST_FILE_UNBIND, VirtqAddr, VirtqFile, VirtqState};
use crate::virtio::dev::{DevSpec, DeviceId, Virtio, WakeEvent};
use crate::virtio::queue::{QueueReg, VirtQueue};
use crate::virtio::vhost::{UpdateVhostMem, VhostDev, error};
use crate::virtio::worker::mio::{ActiveMio, Mio, VirtioMio};
use crate::virtio::{IrqSender, Result, VirtioFeature};
use crate::{bitflags, ffi, impl_mmio_for_zerocopy};

/// Default management unit size: 2 MiB page blocks.
pub const PGALLOC_DEFAULT_PAGEBLOCK_SIZE: u64 = 1 << 21;

#[repr(C, align(8))]
#[derive(Debug, Clone, Default, FromBytes, IntoBytes, Immutable)]
pub struct PgallocConfig {
    pub pageblock_size: u64,
    pub addr: u64,
    pub region_size: u64,
    pub node_id: u16,
    pub padding: [u8; 6],
}

impl_mmio_for_zerocopy!(PgallocConfig);

bitflags! {
    pub struct PgallocFeature(u128) {
        PAGEBLOCK_SIZE = 1 << 0;
        ACPI_PXM = 1 << 1;
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Help)]
pub struct PgallocSpec {
    /// Path to the host device file. [default: /dev/vhost-pgalloc]
    pub dev: Option<Box<Path>>,
    /// Size of the management unit in bytes. [default: 2 MiB]
    pub pageblock_size: Option<u64>,
    /// Guest physical address of the managed region start. [default: 0]
    pub addr: Option<u64>,
    /// Size of the managed region in bytes. [default: guest RAM size]
    pub region_size: Option<u64>,
    /// NUMA node id of the managed region. [default: 0]
    pub node_id: Option<u16>,
}

impl DevSpec for PgallocSpec {
    type Device = VhostPgalloc;

    fn build(self, name: impl Into<Arc<str>>) -> Result<Self::Device> {
        VhostPgalloc::new(self, name)
    }

    fn needs_mem_shared_fd(&self) -> bool {
        // The host backend will mark guest-freed pages discardable in the
        // shmem file backing the guest RAM, which requires a shared memfd
        // backend.
        true
    }
}

#[derive(Debug)]
pub struct VhostPgalloc {
    name: Arc<str>,
    vhost_dev: Arc<VhostDev>,
    config: Arc<PgallocConfig>,
    features: u64,
    error_fds: [Option<OwnedFd>; 2],
}

impl VhostPgalloc {
    pub fn new(spec: PgallocSpec, name: impl Into<Arc<str>>) -> Result<VhostPgalloc> {
        let name = name.into();
        let vhost_dev = match spec.dev {
            Some(dev) => VhostDev::new(dev),
            None => VhostDev::new("/dev/vhost-pgalloc"),
        }?;
        vhost_dev.set_owner()?;
        if let Ok(backend_feature) = vhost_dev.get_backend_features() {
            log::debug!("{name}: vhost-pgalloc backend feature: {backend_feature:x?}");
            vhost_dev.set_backend_features(&backend_feature)?;
        }
        let dev_feat = vhost_dev.get_features()? as u128;
        let known_feat = VirtioFeature::from_bits_truncate(dev_feat).bits()
            | PgallocFeature::from_bits_truncate(dev_feat).bits();
        if !VirtioFeature::from_bits_retain(known_feat).contains(VirtioFeature::VERSION_1) {
            return error::VhostMissingDeviceFeature {
                feature: VirtioFeature::VERSION_1.bits(),
            }
            .fail()?;
        }
        let node_id = spec.node_id.unwrap_or(0);
        let config = PgallocConfig {
            pageblock_size: spec
                .pageblock_size
                .unwrap_or(PGALLOC_DEFAULT_PAGEBLOCK_SIZE),
            addr: spec.addr.unwrap_or(0),
            region_size: spec.region_size.unwrap_or(0),
            node_id,
            padding: [0; 6],
        };
        if config.region_size == 0 {
            log::warn!("{name}: region_size is 0; leave it unset to default to the guest RAM size");
        }
        Ok(VhostPgalloc {
            name,
            vhost_dev: Arc::new(vhost_dev),
            config: Arc::new(config),
            features: known_feat as u64,
            error_fds: [None, None],
        })
    }
}

impl Virtio for VhostPgalloc {
    type Config = PgallocConfig;
    type Feature = PgallocFeature;

    fn id(&self) -> DeviceId {
        DeviceId::PGALLOC
    }

    fn name(&self) -> &str {
        &self.name
    }

    fn num_queues(&self) -> u16 {
        2
    }

    fn config(&self) -> Arc<PgallocConfig> {
        self.config.clone()
    }

    fn feature(&self) -> u128 {
        self.features as u128
    }

    fn ioeventfd_offloaded(&self, q_index: u16) -> Result<bool> {
        // Both queues are handled by the kernel vhost driver.
        Ok(q_index < 2)
    }

    fn mem_update_callback(&self) -> Option<Box<dyn LayoutUpdated>> {
        Some(Box::new(UpdateVhostMem {
            dev: self.vhost_dev.clone(),
        }))
    }

    fn spawn_worker<S, E>(
        self,
        event_rx: Receiver<WakeEvent<S, E>>,
        memory: Arc<RamBus>,
        queue_regs: Arc<[QueueReg]>,
    ) -> Result<(JoinHandle<()>, Arc<Notifier>)>
    where
        S: IrqSender,
        E: IoeventFd,
    {
        Mio::spawn_worker(self, event_rx, memory, queue_regs)
    }
}

impl VirtioMio for VhostPgalloc {
    fn activate<'m, Q, S, E>(
        &mut self,
        feature: u128,
        active_mio: &mut ActiveMio<'_, '_, 'm, Q, S, E>,
    ) -> Result<()>
    where
        Q: VirtQueue<'m>,
        S: IrqSender,
        E: IoeventFd,
    {
        self.vhost_dev.set_features(&(feature as u64))?;
        for (index, fd) in active_mio.ioeventfds.iter().take(2).enumerate() {
            let kick = VirtqFile {
                index: index as u32,
                fd: fd.as_fd().as_raw_fd(),
            };
            self.vhost_dev.set_virtq_kick(&kick)?;
        }
        for (index, queue) in active_mio.queues.iter().take(2).enumerate() {
            let Some(queue) = queue else {
                continue;
            };
            let reg = queue.reg();
            let index = index as u32;
            active_mio.irq_sender.queue_irqfd(index as _, |fd| {
                self.vhost_dev.set_virtq_call(&VirtqFile {
                    index,
                    fd: fd.as_raw_fd(),
                })?;
                Ok(())
            })?;

            self.vhost_dev.set_virtq_num(&VirtqState {
                index,
                val: reg.size.load(Ordering::Acquire) as _,
            })?;
            self.vhost_dev
                .set_virtq_base(&VirtqState { index, val: 0 })?;
            let mem = active_mio.mem;
            let virtq_addr = VirtqAddr {
                index,
                flags: 0,
                desc_hva: mem.translate(reg.desc.load(Ordering::Acquire))? as _,
                used_hva: mem.translate(reg.device.load(Ordering::Acquire))? as _,
                avail_hva: mem.translate(reg.driver.load(Ordering::Acquire))? as _,
                log_guest_addr: 0,
            };
            self.vhost_dev.set_virtq_addr(&virtq_addr)?;
        }
        for (index, fd) in self.error_fds.iter_mut().enumerate() {
            let err_fd =
                unsafe { OwnedFd::from_raw_fd(ffi!(eventfd(0, EFD_CLOEXEC | EFD_NONBLOCK))?) };
            self.vhost_dev.set_virtq_err(&VirtqFile {
                index: index as u32,
                fd: err_fd.as_raw_fd(),
            })?;
            active_mio.poll.registry().register(
                &mut SourceFd(&err_fd.as_raw_fd()),
                Token(index as _),
                Interest::READABLE,
            )?;
            *fd = Some(err_fd);
        }
        self.vhost_dev.pgalloc_set_running(true)?;
        Ok(())
    }

    fn reset(&mut self, registry: &Registry) {
        self.vhost_dev.pgalloc_set_running(false).unwrap();
        for (index, error_fd) in self.error_fds.iter_mut().enumerate() {
            let Some(err_fd) = error_fd else {
                continue;
            };
            self.vhost_dev
                .set_virtq_err(&VirtqFile {
                    index: index as _,
                    fd: VHOST_FILE_UNBIND,
                })
                .unwrap();
            registry
                .deregister(&mut SourceFd(&err_fd.as_raw_fd()))
                .unwrap();
            *error_fd = None;
        }
    }

    fn handle_event<'a, 'm, Q, S, E>(
        &mut self,
        event: &Event,
        _active_mio: &mut ActiveMio<'_, '_, 'm, Q, S, E>,
    ) -> Result<()>
    where
        Q: VirtQueue<'m>,
        S: IrqSender,
        E: IoeventFd,
    {
        let q_index = event.token();
        error::VhostQueueErr {
            dev: "pgalloc",
            index: q_index.0 as u16,
        }
        .fail()?;
        Ok(())
    }

    fn handle_queue<'m, Q, S, E>(
        &mut self,
        index: u16,
        _active_mio: &mut ActiveMio<'_, '_, 'm, Q, S, E>,
    ) -> Result<()>
    where
        Q: VirtQueue<'m>,
        S: IrqSender,
        E: IoeventFd,
    {
        unreachable!(
            "{}: queue {index} is offloaded to the kernel vhost driver",
            self.name
        );
    }
}

impl Drop for VhostPgalloc {
    fn drop(&mut self) {
        let ret = self.vhost_dev.pgalloc_set_running(false);
        if let Err(e) = ret {
            log::error!("{}: {e}", self.name)
        }
    }
}
