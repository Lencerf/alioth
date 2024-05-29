use std::fs::File;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use bitflags::bitflags;
use libc::{eventfd, EFD_CLOEXEC, EFD_NONBLOCK};
use mio::unix::SourceFd;
use mio::{Interest, Registry, Token};
use serde::Deserialize;

use crate::ffi;
use crate::mem::mapped::RamBus;
use crate::virtio::vhost::bindings::{
    VhostMemory, VhostMemoryRegion, VhostVringAddr, VhostVringFile, VhostVringState,
};
use crate::virtio::vhost::ioctls::{
    vhost_get_backend_features, vhost_get_features, vhost_set_backend_features, vhost_set_features,
    vhost_set_mem_table, vhost_set_owner, vhost_set_vring_addr, vhost_set_vring_base,
    vhost_set_vring_call, vhost_set_vring_err, vhost_set_vring_kick, vhost_set_vring_num,
    vhost_vsock_set_guest_cid, vhost_vsock_set_running,
};
use crate::virtio::Result;

use crate::virtio::dev::vsock::VsockConfig;
use crate::virtio::dev::{DevParam, DeviceId, Virtio};

#[derive(Debug, Deserialize)]
pub struct VhostVsockParam {
    pub cid: u32,
    pub dev: Option<PathBuf>,
}

impl DevParam for VhostVsockParam {
    type Device = VhostVsock;
    fn build(self, name: Arc<String>) -> Result<Self::Device> {
        VhostVsock::new(self, name)
    }
}

#[derive(Debug)]
pub struct VhostVsock {
    name: Arc<String>,
    vhost_dev: File,
    config: VsockConfig,
    features: VhostVsockFeature,
    // rx_fd: [OwnedFd; 2],
    // tx_fd: [OwnedFd; 2],
}

bitflags! {
    #[derive(Default, Debug, Clone, Copy, PartialEq, Eq, Hash)]
    pub struct VhostVsockFeature: u64 {
        const SEQPACKET = 1 << 1;
        const VERSION_1 = 1 << 32;
    }
}

impl VhostVsock {
    pub fn new(param: VhostVsockParam, name: Arc<String>) -> Result<VhostVsock> {
        let vhost_dev = match param.dev {
            Some(dev) => File::open(dev),
            None => File::open("/dev/vhost-vsock"),
        }?;
        let vhost_vsock_features;
        unsafe {
            vhost_set_owner(&vhost_dev)?;
            vhost_vsock_set_guest_cid(&vhost_dev, &(param.cid as u64))?;
            let backend_feature = vhost_get_backend_features(&vhost_dev)?;
            vhost_set_backend_features(&vhost_dev, &backend_feature)?;
            vhost_vsock_features =
                VhostVsockFeature::from_bits_truncate(vhost_get_features(&vhost_dev)?);
            // vhost_set_vring_err(
            //     &vhost_dev,
            //     &VhostVringFile {
            //         index: 0,
            //         fd: rx_error_fd.as_raw_fd(),
            //     },
            // )?;
            // vhost_set_vring_err(
            //     &vhost_dev,
            //     &VhostVringFile {
            //         index: 1,
            //         fd: tx_error_fd.as_raw_fd(),
            //     },
            // )?;
        }

        Ok(VhostVsock {
            name,
            vhost_dev,
            config: VsockConfig {
                guest_cid: param.cid,
                ..Default::default()
            },
            features: vhost_vsock_features,
        })
    }
}

const TOKEN_RX_CALL: usize = 0;
const TOKEN_RX_ERR: usize = 1;
const TOKEN_TX_CALL: usize = 2;
const TOKEN_TX_ERR: usize = 3;

impl Virtio for VhostVsock {
    type Config = VsockConfig;

    fn device_id() -> DeviceId {
        DeviceId::Socket
    }

    fn num_queues(&self) -> u16 {
        3
    }

    fn config(&self) -> Arc<VsockConfig> {
        Arc::new(self.config)
    }

    fn feature(&self) -> u64 {
        self.features.bits()
    }

    fn activate(
        &mut self,
        _registry: &Registry,
        feature: u64,
        memory: &RamBus,
        irq_sender: &impl crate::virtio::IrqSender,
        queues: &[crate::virtio::queue::Queue],
    ) -> Result<()> {
        let feature = VhostVsockFeature::from_bits_truncate(feature);
        log::info!("{}: virtio feat: {:?}", self.name, feature);
        unsafe { vhost_set_features(&self.vhost_dev, &feature.bits()) }?;
        let mut mem_table = VhostMemory {
            nregions: 0,
            padding: 0,
            regions: [VhostMemoryRegion::default(); 32],
        };
        let locked_mem = memory.lock_layout();
        for (index, (gpa, user_mem)) in locked_mem.iter().enumerate() {
            mem_table.nregions += 1;
            mem_table.regions[index].guest_phys_addr = gpa as u64;
            mem_table.regions[index].userspace_addr = user_mem.pages.addr() as u64;
            mem_table.regions[index].memory_size = user_mem.pages.size() as u64;
        }
        log::info!("memory table: {:#x?}", mem_table);
        unsafe { vhost_set_mem_table(&self.vhost_dev, &mem_table) }?;
        for (index, queue) in queues.iter().enumerate() {
            if index != 0 && index != 1 {
                continue;
            }
            let fd = irq_sender.queue_irqfd(index as _)?;
            unsafe {
                vhost_set_vring_call(
                    &self.vhost_dev,
                    &VhostVringFile {
                        index: index as _,
                        fd,
                    },
                )?;
                vhost_set_vring_num(
                    &self.vhost_dev,
                    &VhostVringState {
                        index: index as _,
                        num: queue.size.load(Ordering::Acquire) as u32,
                    },
                )?;
                vhost_set_vring_base(
                    &self.vhost_dev,
                    &VhostVringState {
                        index: index as _,
                        num: 0,
                    },
                )?;
                vhost_set_vring_addr(
                    &self.vhost_dev,
                    &VhostVringAddr {
                        index: index as _,
                        flags: 0,
                        desc_user_addr: locked_mem
                            .translate(queue.desc.load(Ordering::Acquire) as usize)?
                            as _,
                        used_user_addr: locked_mem
                            .translate(queue.device.load(Ordering::Acquire) as usize)?
                            as _,
                        avail_user_addr: locked_mem
                            .translate(queue.driver.load(Ordering::Acquire) as usize)?
                            as _,
                        log_guest_addr: 0,
                    },
                )?;
            }
        }
        unsafe { vhost_vsock_set_running(&self.vhost_dev, &1) }?;
        Ok(())
    }
    // fn activate(
    //     &mut self,
    //     registry: &Registry,
    //     feature: u64,
    //     memory: &RamBus,
    //     queues: &[QueueRegister],
    // ) -> Result<()> {

    //     registry.register(
    //         &mut SourceFd(&self.rx_fd[1].as_raw_fd()),
    //         Token(TOKEN_RX_ERR),
    //         Interest::READABLE,
    //     )?;
    //     registry.register(
    //         &mut SourceFd(&self.tx_fd[1].as_raw_fd()),
    //         Token(TOKEN_TX_ERR),
    //         Interest::READABLE,
    //     )?;

    //     unsafe {
    //         vhost_set_vring_num(
    //             &self.vhost_dev,
    //             &VhostVringState {
    //                 index: 0,
    //                 num: queues[0].size.load(Ordering::Acquire) as u32,
    //             },
    //         )?;
    //         vhost_set_vring_num(
    //             &self.vhost_dev,
    //             &VhostVringState {
    //                 index: 1,
    //                 num: queues[1].size.load(Ordering::Acquire) as u32,
    //             },
    //         )?;
    //         vhost_set_vring_base(&self.vhost_dev, &VhostVringState { index: 0, num: 0 })?;
    //         vhost_set_vring_base(&self.vhost_dev, &VhostVringState { index: 1, num: 0 })?;
    //     }

    //     unsafe {
    //         for i in [0, 1] {
    //             vhost_set_vring_addr(
    //                 &self.vhost_dev,
    //                 &VhostVringAddr {
    //                     index: i,
    //                     flags: 0,
    //                     desc_user_addr: locked_mem
    //                         .translate(queues[i as usize].desc.load(Ordering::Acquire) as usize)?
    //                         as _,
    //                     used_user_addr: locked_mem
    //                         .translate(queues[i as usize].device.load(Ordering::Acquire) as usize)?
    //                         as _,
    //                     avail_user_addr: locked_mem
    //                         .translate(queues[i as usize].driver.load(Ordering::Acquire) as usize)?
    //                         as _,
    //                     log_guest_addr: 0,
    //                 },
    //             )?;
    //         }
    //     };
    //     unsafe { vhost_vsock_set_running(&self.vhost_dev, &1) }?;
    //     Ok(())
    // }

    fn reset(&mut self, _registry: &Registry) {
        // call reset owner?
    }

    // fn handle_event(
    //     &mut self,
    //     event: &mio::event::Event,
    //     _registry: &Registry,
    //     _queues: &[impl crate::virtio::queue::VirtQueue],
    // ) -> Result<Vec<u16>> {
    //     match event.token() {
    //         Token(TOKEN_RX_CALL) => {
    //             log::info!("rx call");
    //             Ok(vec![0])
    //         }
    //         Token(TOKEN_TX_CALL) => {
    //             log::info!("tx call");
    //             Ok(vec![1])
    //         }
    //         Token(TOKEN_RX_ERR) => panic!("rx queue error"),
    //         Token(TOKEN_TX_ERR) => panic!("tx queue error"),
    //         _ => unreachable!(),
    //     }
    // }

    fn handle_event(
        &mut self,
        event: &mio::event::Event,
        queues: &[impl crate::virtio::queue::VirtQueue],
        irq_sender: &impl crate::virtio::IrqSender,
        registry: &Registry,
    ) -> Result<()> {
        unimplemented!()
    }

    // fn handle_queue(
    //     &mut self,
    //     index: u16,
    //     _queues: &[impl crate::virtio::queue::VirtQueue],
    //     _registry: &Registry,
    // ) -> Result<Vec<u16>> {
    //     match index {
    //         0 | 1 => unreachable!("0 and 1 are offloaded"),
    //         2 => log::info!("event queue buffer avaialebl"),
    //         _ => unreachable!(),
    //     }
    //     Ok(vec![])
    // }

    fn handle_queue(
        &mut self,
        index: u16,
        queues: &[impl crate::virtio::queue::VirtQueue],
        irq_sender: &impl crate::virtio::IrqSender,
        registry: &Registry,
    ) -> Result<()> {
        match index {
            0 | 1 => unreachable!("0 and 1 are offloaded"),
            2 => log::info!("event queue buffer avaialebl"),
            _ => unreachable!(),
        }
        Ok(())
    }

    fn offload_ioeventfd<E>(&self, q_index: u16, fd: &E) -> Result<bool>
    where
        E: crate::hv::IoeventFd,
    {
        if q_index == 0 || q_index == 1 {
            unsafe {
                vhost_set_vring_kick(
                    &self.vhost_dev,
                    &VhostVringFile {
                        index: q_index as _,
                        fd: fd.as_fd().as_raw_fd(),
                    },
                )
            }?;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    // fn offload_notifier<N>(&self, qindex: u16, notifier: &mut N) -> Result<bool>
    // where
    //     N: Notifier,
    // {
    //     match qindex {
    //         0 | 1 => unsafe {
    //             vhost_set_vring_kick(
    //                 &self.vhost_dev,
    //                 &VhostVringFile {
    //                     index: qindex as u32,
    //                     fd: notifier.as_raw_fd(),
    //                 },
    //             )?;
    //             log::info!("{qindex} is offloaded to vhost");
    //             Ok(true)
    //         },
    //         _ => Ok(false),
    //     }
    // }
}

impl Drop for VhostVsock {
    fn drop(&mut self) {
        let ret = unsafe { vhost_vsock_set_running(&self.vhost_dev, &0) };
        if let Err(e) = ret {
            log::error!("vhostvosck: {}", e)
        }
    }
}
