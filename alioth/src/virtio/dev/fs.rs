use std::mem::{size_of, size_of_val};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use bitflags::{bitflags, Flags};
use serde::Deserialize;
use zerocopy::{AsBytes, FromBytes, FromZeroes};

use crate::hv::IoeventFd;
use crate::impl_mmio_for_zerocopy;
use crate::virtio::queue::Queue;
use crate::virtio::vhost_user::{
    DeviceConfig, MemoryOneRegion, MemoryRegion, VhostFeature, VhostUserDev, VirtqAddr, VirtqState,
};
use crate::virtio::{DeviceId, Error, Result, VirtioFeature};

use super::{DevParam, Virtio};

#[repr(C, align(4))]
#[derive(Debug, FromBytes, FromZeroes, AsBytes)]
pub struct FsConfig {
    tag: [u8; 36],
    num_request_queues: u32,
    notify_buf_size: u32,
}

impl_mmio_for_zerocopy!(FsConfig);

bitflags! {
    #[derive(Default, Debug, Clone, Copy, PartialEq, Eq, Hash)]
    pub struct FsFeature: u64 {
        const NOTIFICATION = 1 << 0;
    }
}

#[derive(Debug)]
pub struct Fs {
    name: Arc<String>,
    vhost_dev: VhostUserDev,
    config: Arc<FsConfig>,
    feature: u64,
    num_queues: u16,
}

impl Fs {
    pub fn new(param: FsParam, name: Arc<String>) -> Result<Self> {
        let vhost_dev = VhostUserDev::new(param.vhost_sock)?;
        let dev_feat = vhost_dev.get_features()?;
        let virtio_feat = VirtioFeature::from_bits_retain(dev_feat);
        if virtio_feat.contains(VirtioFeature::VHOST_PROTOCOL_FEATURES) {
            let prot_feat = vhost_dev.get_protocol_features()?;
            log::info!(
                "vhost feat: {:?}",
                VhostFeature::from_bits_retain(prot_feat)
            );
            let know_feat = VhostFeature::MQ
                | VhostFeature::REPLY_ACK
                | VhostFeature::CONFIG
                | VhostFeature::CONFIGURE_MEM_SLOTS;
            vhost_dev.set_protocol_features(&know_feat.bits())?;
        } else {
            return Err(Error::CannotGetVhostDevConfig);
        }
        let num_queues = vhost_dev.get_queue_num()? as u16;
        let mut cfg = DeviceConfig::new_zeroed();
        cfg.size = size_of_val(&cfg.region) as _;
        let dev_config = vhost_dev.get_config(&cfg)?;
        let config = FsConfig::read_from_prefix(&dev_config.region).unwrap();
        log::info!("fs config: {:#x?}", config);
        vhost_dev.set_owner()?;
        Ok(Fs {
            num_queues,
            name,
            vhost_dev,
            config: Arc::new(config),
            feature: dev_feat,
        })
    }
}

#[derive(Deserialize)]
pub struct FsParam {
    pub vhost_sock: PathBuf,
}

impl DevParam for FsParam {
    type Device = Fs;

    fn build(self, name: Arc<String>) -> Result<Self::Device> {
        Fs::new(self, name)
    }
}

impl Virtio for Fs {
    type Config = FsConfig;
    fn config(&self) -> Arc<Self::Config> {
        self.config.clone()
    }
    fn device_id() -> DeviceId {
        DeviceId::FileSystem
    }
    fn feature(&self) -> u64 {
        self.feature
    }

    fn activate(
        &mut self,
        _registry: &mio::Registry,
        feature: u64,
        memory: &crate::mem::mapped::RamBus,
        irq_sender: &impl crate::virtio::IrqSender,
        queues: &[Queue],
    ) -> Result<()> {
        log::info!(
            "{}: virtio feat: {:?}, fs feat{:?}",
            self.name,
            VirtioFeature::from_bits_retain(feature),
            FsFeature::from_bits_retain(feature)
        );
        self.vhost_dev
            .set_features(&(feature | VirtioFeature::VHOST_PROTOCOL_FEATURES.bits()))?;
        let mem = memory.lock_layout();
        for (gpa, slot) in mem.iter() {
            let region = MemoryOneRegion {
                _padding: 0,
                region: MemoryRegion {
                    guest_phys_addr: gpa as _,
                    memory_size: slot.pages.size() as _,
                    userspace_addr: slot.pages.addr() as _,
                    mmap_offset: 0,
                },
            };
            self.vhost_dev
                .add_mem_region(&region, slot.pages.fd().as_raw_fd())
                .unwrap();
            log::info!("region: {region:x?}");
        }
        for (index, queue) in queues.iter().enumerate() {
            let fd = irq_sender.queue_irqfd(index as _)?;
            self.vhost_dev.set_vring_call(&(index as u64), fd).unwrap();
            let vring_num = VirtqState {
                index: index as _,
                num: queue.size.load(Ordering::Acquire) as _,
            };
            self.vhost_dev.set_vring_num(&vring_num).unwrap();
            log::info!("set_vring_num: {vring_num:x?}");
            let vring_base = VirtqState {
                index: index as _,
                num: 0,
            };
            self.vhost_dev.set_vring_base(&vring_base).unwrap();
            log::info!("set_vring_base: {vring_base:x?}");
            let vring_addr = VirtqAddr {
                index: index as _,
                flags: 0,
                desc_user_addr: mem.translate(queue.desc.load(Ordering::Acquire) as _)? as _,
                used_user_addr: mem.translate(queue.device.load(Ordering::Acquire) as _)? as _,
                avail_user_addr: mem.translate(queue.driver.load(Ordering::Acquire) as _)? as _,
                log_guest_addr: 0,
            };
            self.vhost_dev.set_vring_addr(&vring_addr).unwrap();
            log::info!("queue: {:x?}", queue);
            log::info!("vring_addr: {vring_addr:x?}");
            let vring_enable = VirtqState {
                index: index as _,
                num: 1,
            };
            self.vhost_dev.set_vring_enable(&vring_enable).unwrap();
            log::info!("vring_enable: {vring_enable:x?}");
        }
        Ok(())
        // self.vhost_dev.set_features(payload)
    }

    fn handle_event(
        &mut self,
        event: &mio::event::Event,
        queues: &[impl crate::virtio::queue::VirtQueue],
        irq_sender: &impl crate::virtio::IrqSender,
        registry: &mio::Registry,
    ) -> Result<()> {
        // queues[0].
        unimplemented!()
    }

    fn handle_queue(
        &mut self,
        index: u16,
        queues: &[impl crate::virtio::queue::VirtQueue],
        irq_sender: &impl crate::virtio::IrqSender,
        registry: &mio::Registry,
    ) -> Result<()> {
        unimplemented!()
    }

    fn num_queues(&self) -> u16 {
        self.num_queues
    }

    fn reset(&mut self, registry: &mio::Registry) {
        unimplemented!()
    }

    fn offload_ioeventfd<E>(&self, q_index: u16, fd: &E) -> Result<bool>
    where
        E: IoeventFd,
    {
        if q_index < self.num_queues {
            self.vhost_dev
                .set_vring_kick(&(q_index as u64), fd.as_fd().as_raw_fd())?;
            Ok(true)
        } else {
            Ok(false)
        }
    }
}

#[cfg(test)]
mod test {
    use crate::virtio::dev::fs::FsFeature;
    use crate::virtio::VirtioFeature;

    #[test]
    fn test_feature() {
        let f = 0x170000000;
        println!("{:?}", VirtioFeature::from_bits_retain(f));
        println!("{:?}", FsFeature::from_bits_retain(f));
    }
}
