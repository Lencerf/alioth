use std::os::fd::{FromRawFd, OwnedFd};

use snafu::ResultExt;

use crate::arch::reg::{Reg, SReg};
use crate::hv::kvm::vcpu::KvmVcpu;
use crate::hv::kvm::vm::KvmVm;
use crate::hv::{Result, error};
use crate::sys::kvm::{KvmOneReg, kvm_create_vcpu, kvm_get_one_reg, kvm_set_one_reg};

pub struct VcpuArch {}

impl VcpuArch {
    pub fn new(_: u64) -> Self {
        VcpuArch {}
    }
}

const fn encode_reg(reg: Reg) -> u64 {
    reg.raw() as u64 | (0x9 << 60)
}

fn encode_system_reg(reg: SReg) -> u64 {
    reg.raw() as u64 | (0x9 << 60) | (0x1 << 16)
}

impl KvmVcpu {
    pub fn create_vcpu(vm: &KvmVm, index: u16, _: u64) -> Result<OwnedFd> {
        let fd = unsafe { kvm_create_vcpu(&vm.vm.fd, index as u32) }.context(error::CreateVcpu)?;
        Ok(unsafe { OwnedFd::from_raw_fd(fd) })
    }

    pub fn kvm_vcpu_init(&mut self, is_bsp: bool) -> Result<()> {
        unimplemented!()
    }

    fn get_one_reg(&self, reg: u64) -> Result<u64> {
        let mut val = 0;
        let one_reg = KvmOneReg {
            id: reg,
            addr: &mut val as *mut _ as _,
        };
        unsafe { kvm_get_one_reg(&self.fd, &one_reg) }.context(error::VcpuReg)?;
        Ok(val)
    }

    fn set_one_reg(&self, reg: u64, val: u64) -> Result<()> {
        let one_reg = KvmOneReg {
            id: reg,
            addr: &val as *const _ as _,
        };
        unsafe { kvm_set_one_reg(&self.fd, &one_reg) }.context(error::VcpuReg)?;
        Ok(())
    }

    pub fn kvm_set_regs(&self, vals: &[(Reg, u64)]) -> Result<()> {
        for (reg, val) in vals {
            self.set_one_reg(encode_reg(*reg), *val)?;
        }
        Ok(())
    }

    pub fn kvm_get_reg(&self, reg: Reg) -> Result<u64> {
        self.get_one_reg(encode_reg(reg))
    }

    pub fn kvm_set_sregs(&self, vals: &[(SReg, u64)]) -> Result<()> {
        for (reg, val) in vals {
            self.set_one_reg(encode_system_reg(*reg), *val)?;
        }
        Ok(())
    }

    pub fn kvm_get_sreg(&self, reg: SReg) -> Result<u64> {
        self.get_one_reg(encode_system_reg(reg))
    }
}
