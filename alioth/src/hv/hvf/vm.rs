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

use std::collections::HashMap;
use std::os::fd::{AsFd, BorrowedFd};
use std::os::raw::c_void;
use std::ptr::null_mut;
use std::thread::JoinHandle;

use parking_lot::Mutex;
use snafu::ResultExt;

use crate::hv::hvf::bindings::{
    GicDistributorReg, HvMemoryFlag, hv_gic_config_create, hv_gic_config_set_distributor_base,
    hv_gic_config_set_msi_interrupt_range, hv_gic_config_set_msi_region_base,
    hv_gic_config_set_redistributor_base, hv_gic_create, hv_gic_get_distributor_base_alignment,
    hv_gic_get_distributor_reg, hv_gic_get_distributor_size, hv_gic_get_msi_region_base_alignment,
    hv_gic_get_msi_region_size, hv_gic_get_redistributor_base_alignment,
    hv_gic_get_redistributor_region_size, hv_gic_get_redistributor_size,
    hv_gic_get_spi_interrupt_range, hv_gic_send_msi, hv_gic_set_distributor_reg, hv_gic_set_spi,
    hv_vcpu_create, hv_vm_destroy, hv_vm_map, hv_vm_unmap,
};
use crate::hv::hvf::check_ret;
use crate::hv::hvf::vcpu::HvfVcpu;
use crate::hv::{
    GicV2, GicV3, IoeventFd, IoeventFdRegistry, IrqFd, IrqSender, MemMapOption, MsiSender, Result,
    Vm, VmExit, VmMemory, error,
};
use crate::hvffi;

#[derive(Debug)]
pub struct HvfMemory {}

impl VmMemory for HvfMemory {
    fn deregister_encrypted_range(&self, _range: &[u8]) -> Result<()> {
        unimplemented!()
    }

    fn mem_map(&self, gpa: u64, size: u64, hva: usize, option: MemMapOption) -> Result<()> {
        if option.log_dirty {
            return error::Capability { cap: "log dirty" }.fail();
        }
        let mut flags = HvMemoryFlag::empty();
        if option.read {
            flags |= HvMemoryFlag::READ;
        }
        if option.write {
            flags |= HvMemoryFlag::WRITE;
        }
        if option.exec {
            flags |= HvMemoryFlag::EXEC;
        }
        let ret = unsafe { hv_vm_map(hva as *const u8, gpa, size as usize, flags) };
        check_ret(ret).context(error::GuestMap { hva, gpa, size })?;
        Ok(())
    }

    fn register_encrypted_range(&self, _range: &[u8]) -> Result<()> {
        unimplemented!()
    }

    fn unmap(&self, gpa: u64, size: u64) -> Result<()> {
        let ret = unsafe { hv_vm_unmap(gpa, size as usize) };
        check_ret(ret).context(error::GuestUnmap { gpa, size })?;
        Ok(())
    }

    fn mark_private_memory(&self, _gpa: u64, _size: u64, _private: bool) -> Result<()> {
        unimplemented!()
    }

    fn reset(&self) -> Result<()> {
        log::error!("HvfMemory reset");
        Ok(())
    }
}

#[derive(Debug)]
pub struct HvfIrqSender {
    intid: u8,
}

impl IrqSender for HvfIrqSender {
    fn send(&self) -> Result<()> {
        hvffi!(unsafe { hv_gic_set_spi(self.intid as u32, true) }).context(error::SendInterrupt)
    }
}

#[derive(Debug)]
pub struct HvfIrqFd {}
impl AsFd for HvfIrqFd {
    fn as_fd(&self) -> BorrowedFd<'_> {
        unimplemented!()
    }
}
impl IrqFd for HvfIrqFd {
    fn get_addr_hi(&self) -> u32 {
        unimplemented!()
    }

    fn get_addr_lo(&self) -> u32 {
        unimplemented!()
    }

    fn get_data(&self) -> u32 {
        unimplemented!()
    }

    fn get_masked(&self) -> bool {
        unimplemented!()
    }

    fn set_addr_hi(&self, _val: u32) -> Result<()> {
        unimplemented!()
    }

    fn set_addr_lo(&self, _val: u32) -> Result<()> {
        unimplemented!()
    }

    fn set_data(&self, _val: u32) -> Result<()> {
        unimplemented!()
    }

    fn set_masked(&self, _val: bool) -> Result<bool> {
        unimplemented!()
    }
}

#[derive(Debug)]
pub struct HvfMsiSender;

impl MsiSender for HvfMsiSender {
    type IrqFd = HvfIrqFd;

    fn create_irqfd(&self) -> Result<Self::IrqFd> {
        Err(std::io::ErrorKind::Unsupported.into()).context(error::IrqFd)
    }

    fn send(&self, addr: u64, data: u32) -> Result<()> {
        hvffi!(unsafe { hv_gic_send_msi(addr, data) }).context(error::SendInterrupt)
    }
}

#[derive(Debug)]
pub struct HvfIoeventFd {}

impl IoeventFd for HvfIoeventFd {}

impl AsFd for HvfIoeventFd {
    fn as_fd(&self) -> BorrowedFd<'_> {
        unimplemented!()
    }
}

#[derive(Debug)]
pub struct HvfIoeventFdRegistry;

impl IoeventFdRegistry for HvfIoeventFdRegistry {
    type IoeventFd = HvfIoeventFd;

    fn create(&self) -> Result<Self::IoeventFd> {
        Err(std::io::ErrorKind::Unsupported.into()).context(error::IoeventFd)
    }

    fn deregister(&self, _fd: &Self::IoeventFd) -> Result<()> {
        Err(std::io::ErrorKind::Unsupported.into()).context(error::IoeventFd)
    }

    fn register(
        &self,
        _fd: &Self::IoeventFd,
        _gpa: u64,
        _len: u8,
        _data: Option<u64>,
    ) -> Result<()> {
        Err(std::io::ErrorKind::Unsupported.into()).context(error::IoeventFd)
    }
}

#[derive(Debug)]
pub struct HvfGicV2 {}

impl GicV2 for HvfGicV2 {
    fn init(&self) -> Result<()> {
        unimplemented!()
    }

    fn get_dist_reg(&self, _cpu_index: u32, _offset: u16) -> Result<u32> {
        unimplemented!()
    }

    fn set_dist_reg(&self, _cpu_index: u32, _offset: u16, _val: u32) -> Result<()> {
        unimplemented!()
    }

    fn get_cpu_reg(&self, _cpu_index: u32, _offset: u16) -> Result<u32> {
        unimplemented!()
    }

    fn set_cpu_reg(&self, _cpu_index: u32, _offset: u16, _val: u32) -> Result<()> {
        unimplemented!()
    }

    fn get_num_irqs(&self) -> Result<u32> {
        unimplemented!()
    }

    fn set_num_irqs(&self, _val: u32) -> Result<()> {
        unimplemented!()
    }
}

#[derive(Debug)]
pub struct HvfGicV3 {
    config: usize,
}

impl GicV3 for HvfGicV3 {
    fn init(&self) -> Result<()> {
        log::error!("HvfGicV3::init");
        Ok(())
    }
}

#[derive(Debug)]
pub struct HvfVm {
    pub(super) vcpus: Mutex<HashMap<u32, u64>>,
}

impl Drop for HvfVm {
    fn drop(&mut self) {
        let ret = unsafe { hv_vm_destroy() };
        if let Err(e) = check_ret(ret) {
            log::error!("hv_vm_destroy: {e:?}");
        }
    }
}

impl Vm for HvfVm {
    type GicV2 = HvfGicV2;
    type GicV3 = HvfGicV3;
    type IoeventFdRegistry = HvfIoeventFdRegistry;
    type IrqSender = HvfIrqSender;
    type Memory = HvfMemory;
    type MsiSender = HvfMsiSender;
    type Vcpu = HvfVcpu;

    fn create_ioeventfd_registry(&self) -> Result<Self::IoeventFdRegistry> {
        Ok(HvfIoeventFdRegistry)
    }

    fn create_msi_sender(&self, _devid: u32) -> Result<Self::MsiSender> {
        Ok(HvfMsiSender)
    }

    fn create_vcpu(&self, id: u32) -> Result<Self::Vcpu> {
        let mut exit = null_mut();
        let mut vcpu_id = 0;
        let ret = unsafe { hv_vcpu_create(&mut vcpu_id, &mut exit, null_mut()) };
        check_ret(ret).context(error::CreateVcpu)?;
        self.vcpus.lock().insert(id, vcpu_id);
        Ok(HvfVcpu {
            exit,
            vcpu_id,
            vmexit: VmExit::Shutdown,
            advance_pc: false,
            exit_reg: None,
        })
    }

    fn create_vm_memory(&mut self) -> Result<Self::Memory> {
        Ok(HvfMemory {})
    }

    fn stop_vcpu<T>(_id: u32, _handle: &JoinHandle<T>) -> Result<()> {
        Err(std::io::ErrorKind::Unsupported.into()).context(error::StopVcpu)
    }

    fn create_gic_v2(
        &self,
        _distributor_base: u64,
        _cpu_interface_base: u64,
    ) -> Result<Self::GicV2> {
        Err(std::io::ErrorKind::Unsupported.into()).context(error::CreateDevice)
    }

    fn create_irq_sender(&self, pin: u8) -> Result<Self::IrqSender> {
        log::error!("create_irq_sender: pin={pin}");
        Ok(HvfIrqSender { intid: pin + 32 })
    }

    fn create_gic_v3(
        &self,
        distributor_base: u64,
        redistributor_base: u64,
        redistributor_count: u32,
        its_base: Option<u64>,
    ) -> Result<Self::GicV3> {
        let mut redistributor_region_size = 0;
        hvffi!(unsafe { hv_gic_get_redistributor_region_size(&mut redistributor_region_size) })
            .context(error::CreateDevice)?;
        let mut redistributor_size = 0;
        hvffi!(unsafe { hv_gic_get_redistributor_size(&mut redistributor_size) })
            .context(error::CreateDevice)?;
        let mut distributor_size = 0;
        hvffi!(unsafe { hv_gic_get_distributor_size(&mut distributor_size) })
            .context(error::CreateDevice)?;
        log::info!(
            "create_gic_v3: distributor_size={distributor_size:x}, redistributor_size={redistributor_size:x}, redistributor_region_size={redistributor_region_size:x}"
        );

        let mut msi_region_size = 0;
        hvffi!(unsafe { hv_gic_get_msi_region_size(&mut msi_region_size) })
            .context(error::CreateDevice)?;
        let mut msi_region_base_align = 0;
        hvffi!(unsafe { hv_gic_get_msi_region_base_alignment(&mut msi_region_base_align) })
            .context(error::CreateDevice)?;
        let mut redistributor_base_align = 0;
        hvffi!(unsafe { hv_gic_get_redistributor_base_alignment(&mut redistributor_base_align) })
            .context(error::CreateDevice)?;
        let mut distributor_base_align = 0;
        hvffi!(unsafe { hv_gic_get_distributor_base_alignment(&mut distributor_base_align) })
            .context(error::CreateDevice)?;
        log::info!(
            "msi_region_size={msi_region_size}, msi_region_base_align={msi_region_base_align}, redistributor_base_align={redistributor_base_align}, distributor_base_align={distributor_base_align}"
        );

        let mut spi_intid_base = 0;
        let mut spi_intid_count = 0;
        hvffi!(unsafe {
            hv_gic_get_spi_interrupt_range(&mut spi_intid_base, &mut spi_intid_count)
        })
        .context(error::CreateDevice)?;
        log::info!(
            "create_gic_v3: spi_intid_base={spi_intid_base}, spi_intid_count={spi_intid_count}"
        );

        let config = unsafe { hv_gic_config_create() };
        hvffi!(unsafe { hv_gic_config_set_distributor_base(config, distributor_base) })
            .context(error::CreateDevice)?;
        hvffi!(unsafe { hv_gic_config_set_redistributor_base(config, redistributor_base) })
            .context(error::CreateDevice)?;
        if let Some(its_base) = its_base {
            log::info!("creating its at {its_base:x}");
            hvffi!(unsafe { hv_gic_config_set_msi_region_base(config, its_base) })
                .context(error::CreateDevice)?;
            hvffi!(unsafe { hv_gic_config_set_msi_interrupt_range(config, 64, 955) })
                .context(error::CreateDevice)?;
        }

        let ret = unsafe { hv_gic_create(config) };
        check_ret(ret).context(error::CreateDevice)?;
        log::error!(
            "create_gic_v3: distributor_base={distributor_base:x}, redistributor_base={redistributor_base:x}, redistributor_count={redistributor_count}"
        );

        let mut typer_value = 0;
        hvffi!(unsafe { hv_gic_get_distributor_reg(GicDistributorReg::TYPER, &mut typer_value) })
            .context(error::CreateDevice)?;
        log::info!(
            "typer value = {typer_value:x}, has bit 17 = {}",
            typer_value & (1 << 17) != 0
        );
        hvffi!(unsafe {
            hv_gic_set_distributor_reg(GicDistributorReg::TYPER, typer_value | (1 << 17))
        })
        .context(error::CreateDevice)?;
        let mut typer_value = 0;
        hvffi!(unsafe { hv_gic_get_distributor_reg(GicDistributorReg::TYPER, &mut typer_value) })
            .context(error::CreateDevice)?;
        log::info!(
            "typer value = {typer_value:x}, has bit 17 = {}",
            typer_value & (1 << 17) != 0
        );
        Ok(HvfGicV3 { config })
    }
}
