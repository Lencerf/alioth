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

use std::fs::{File, OpenOptions};
use std::os::unix::fs::FileExt;
use std::path::PathBuf;
use std::sync::mpsc::Receiver;
use std::sync::Arc;
use std::thread::JoinHandle;

use bitflags::bitflags;
use mio::event::Event;
use mio::Registry;
use serde::Deserialize;
use serde_aco::Help;
use snafu::ResultExt;
use zerocopy::{AsBytes, FromBytes, FromZeroes};

use crate::hv::IoeventFd;
use crate::mem::mapped::{Ram, RamBus};
use crate::virtio::dev::{DevParam, Virtio, WakeEvent};
use crate::virtio::queue::handlers::handle_desc;
use crate::virtio::queue::{Descriptor, Queue, VirtQueue};
use crate::virtio::worker::mio::{Mio, VirtioMio};
use crate::virtio::worker::Waker;
use crate::virtio::{error, DeviceId, IrqSender, Result, FEATURE_BUILT_IN};
use crate::{c_enum, impl_mmio_for_zerocopy};

c_enum! {
    #[derive(FromBytes, FromZeroes)]
    pub struct RequestType(u32);
    {
        IN = 0;
        OUT = 1;
        FLUSH = 4;
        GET_ID = 8;
        GET_LIFETIME = 10;
        DISCARD = 11;
        WRITE_ZEROES = 13;
        SECURE_ERASE = 14;
    }
}

c_enum! {
    #[derive(FromBytes, FromZeroes)]
    pub struct Status(u8);
    {
        OK = 0;
        IOERR = 1;
        UNSUPP = 2;
    }
}

#[repr(C)]
#[derive(Debug, FromZeroes, FromBytes)]
pub struct Request {
    type_: RequestType,
    reserved: u32,
    sector: u64,
}

pub const VIRTIO_BLK_ID_SIZE: usize = 20;

const SECTOR_SIZE: usize = 1 << 9;

bitflags! {
    #[derive(Default, Debug, Clone, Copy, PartialEq, Eq, Hash)]
    pub struct BlockFeature: u64 {
        const SIZE_MAX = 1 << 1;
        const SEG_MAX = 1 << 2;
        const GEOMETRY = 1 << 4;
        const RO = 1 << 5;
        const BLK_SIZE = 1 << 6;
        const FLUSH = 1 << 9;
        const TOPOLOGY = 1 << 10;
        const CONFIG_WCE = 1 << 11;
        const MQ = 1 << 12;
        const DISCARD = 1 << 13;
        const WRITE_ZEROS = 1 << 14;
        const LIFETIME = 1 << 15;
        const SECURE_ERASE = 1 << 16;
    }
}

#[derive(Debug, Default, FromBytes, FromZeroes, AsBytes)]
#[repr(C)]
pub struct BlockConfig {
    capacity: u64,
    size_max: u32,
    seg_max: u32,

    // geometry
    cylinders: u16,
    heads: u8,
    sectors: u8,

    blk_size: u32,

    // topology
    physical_block_exp: u8,
    alignment_offset: u8,
    min_io_size: u16,
    opt_io_size: u32,

    writeback: u8,
    unused0: u8,
    num_queues: u16,
    max_discard_sectors: u32,
    max_discard_seg: u32,
    discard_sector_alignment: u32,
    max_write_zeroes_sectors: u32,
    max_write_zeroes_seg: u32,
    write_zeroes_may_unmap: u8,
    _unused1: [u8; 3],
    max_secure_erase_sectors: u32,
    max_secure_erase_seg: u32,
    secure_erase_sector_alignment: u32,
}
impl_mmio_for_zerocopy!(BlockConfig);

#[derive(Debug, Clone, Deserialize, Help)]
pub struct BlockParam {
    /// Path to a raw-formatted disk image.
    pub path: PathBuf,
    /// Set the device as readonly. [default: false]
    #[serde(default)]
    pub readonly: bool,
}

impl DevParam for BlockParam {
    type Device = Block;

    fn build(self, name: impl Into<Arc<str>>) -> Result<Block> {
        Block::new(self, name)
    }
}

#[derive(Debug)]
pub struct Block {
    name: Arc<str>,
    config: Arc<BlockConfig>,
    disk: File,
    feature: BlockFeature,
}

impl Block {
    pub fn new(param: BlockParam, name: impl Into<Arc<str>>) -> Result<Self> {
        let access_disk = error::AccessFile {
            path: param.path.as_path(),
        };
        let disk = OpenOptions::new()
            .read(true)
            .write(!param.readonly)
            .open(&param.path)
            .context(access_disk)?;
        let len = disk.metadata().context(access_disk)?.len();
        let config = BlockConfig {
            capacity: len / SECTOR_SIZE as u64,
            num_queues: 1,
            ..Default::default()
        };
        let config = Arc::new(config);
        let mut feature = BlockFeature::FLUSH;
        if param.readonly {
            feature |= BlockFeature::RO;
        }
        Ok(Block {
            name: name.into(),
            disk,
            config,
            feature,
        })
    }

    fn handle_req_queue(&self, desc: &mut Descriptor) -> Result<Option<usize>> {
        let disk = &self.disk;
        let Some(buf0) = desc.readable.first() else {
            return error::InvalidBuffer.fail();
        };
        let Some(request) = Request::read_from(buf0) else {
            return error::InvalidBuffer.fail();
        };
        let offset = request.sector * SECTOR_SIZE as u64;
        let w_len = match request.type_ {
            RequestType::IN => {
                let Some(buf1) = desc.writable.first_mut() else {
                    return error::InvalidBuffer.fail();
                };
                let l = buf1.len();
                let status = match disk.read_exact_at(buf1, offset) {
                    Ok(()) => Status::OK,
                    Err(e) => {
                        log::error!("{}: read {l} bytes from offset {offset:#x}: {e}", self.name);
                        Status::IOERR
                    }
                };
                let Some(buf2) = desc.writable.get_mut(1) else {
                    return error::InvalidBuffer.fail();
                };
                let Some(status_byte) = buf2.first_mut() else {
                    return error::InvalidBuffer.fail();
                };
                *status_byte = status.into();
                l + 1
            }
            RequestType::OUT => {
                let Some(buf1) = desc.readable.get(1) else {
                    return error::InvalidBuffer.fail();
                };
                let l = buf1.len();
                let status = if self.feature.contains(BlockFeature::RO) {
                    Status::IOERR
                } else {
                    match disk.write_all_at(buf1, offset) {
                        Ok(()) => Status::OK,
                        Err(e) => {
                            log::error!(
                                "{}: write {l} bytes to offset {offset:#x}: {e}",
                                self.name
                            );
                            Status::IOERR
                        }
                    }
                };
                let Some(buf2) = desc.writable.first_mut() else {
                    return error::InvalidBuffer.fail();
                };
                let Some(status_byte) = buf2.first_mut() else {
                    return error::InvalidBuffer.fail();
                };
                *status_byte = status.into();
                1
            }
            RequestType::FLUSH => {
                // TODO flush the file
                let Some(w_buf) = desc.writable.last_mut() else {
                    return error::InvalidBuffer.fail();
                };
                let Some(status_byte) = w_buf.get_mut(0) else {
                    return error::InvalidBuffer.fail();
                };
                *status_byte = Status::OK.into();
                1
            }
            RequestType::GET_ID => {
                let Some(buf1) = desc.writable.first_mut() else {
                    return error::InvalidBuffer.fail();
                };
                let len = std::cmp::min(self.name.len(), buf1.len());
                buf1[0..len].copy_from_slice(&self.name.as_bytes()[0..len]);
                let Some(buf2) = desc.writable.get_mut(1) else {
                    return error::InvalidBuffer.fail();
                };
                let Some(status_byte) = buf2.first_mut() else {
                    return error::InvalidBuffer.fail();
                };
                *status_byte = Status::OK.into();
                1 + len
            }
            _ => {
                log::error!("unimplemented op: {:#x?}", request.type_);
                let Some(w_buf) = desc.writable.last_mut() else {
                    return error::InvalidBuffer.fail();
                };
                let Some(w_byte) = w_buf.get_mut(0) else {
                    return error::InvalidBuffer.fail();
                };
                *w_byte = Status::UNSUPP.into();
                1
            }
        };
        Ok(Some(w_len))
    }
}

impl Virtio for Block {
    const DEVICE_ID: DeviceId = DeviceId::Block;

    type Config = BlockConfig;
    type Feature = BlockFeature;

    fn name(&self) -> &str {
        &self.name
    }

    fn num_queues(&self) -> u16 {
        self.config.num_queues
    }

    fn config(&self) -> Arc<BlockConfig> {
        self.config.clone()
    }

    fn feature(&self) -> u64 {
        self.feature.bits() | FEATURE_BUILT_IN
    }

    fn spawn_worker<S, E>(
        self,
        event_rx: Receiver<WakeEvent<S>>,
        memory: Arc<RamBus>,
        queue_regs: Arc<[Queue]>,
        fds: Arc<[(E, bool)]>,
    ) -> Result<(JoinHandle<()>, Arc<Waker>)>
    where
        S: IrqSender,
        E: IoeventFd,
    {
        Mio::spawn_worker(self, event_rx, memory, queue_regs, fds)
    }
}

impl VirtioMio for Block {
    fn reset(&mut self, _registry: &Registry) {}

    fn activate(
        &mut self,
        _registry: &Registry,
        _feature: u64,
        _memory: &Ram,
        _irq_sender: &impl IrqSender,
        _queues: &[Queue],
    ) -> Result<()> {
        Ok(())
    }

    fn handle_event<'m, Q>(
        &mut self,
        _event: &Event,
        _queues: &mut [Option<Q>],
        _irq_sender: &impl IrqSender,
        _registry: &Registry,
    ) -> Result<()>
    where
        Q: VirtQueue<'m>,
    {
        Ok(())
    }

    fn handle_queue<'m, Q>(
        &mut self,
        index: u16,
        queues: &mut [Option<Q>],
        irq_sender: &impl IrqSender,
        _registry: &Registry,
    ) -> Result<()>
    where
        Q: VirtQueue<'m>,
    {
        let Some(Some(queue)) = queues.get_mut(index as usize) else {
            log::error!("{}: invalid queue index {index}", self.name);
            return Ok(());
        };
        handle_desc(&self.name, index, queue, irq_sender, |desc| {
            self.handle_req_queue(desc)
        })
    }
}
