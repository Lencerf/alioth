use std::os::fd::OwnedFd;

use crate::hv::kvm::vm::KvmVm;
use crate::hv::{Kvm, Result, VmConfig};
use crate::sys::kvm::KvmVmType;

pub fn translate_msi_addr(addr_lo: u32, addr_hi: u32) -> (u32, u32) {
    (addr_lo, addr_hi)
}

#[derive(Debug)]
pub struct VmArch;

impl VmArch {
    pub fn new(_kvm: &Kvm, _config: &VmConfig) -> Result<Self> {
        Ok(VmArch)
    }
}

impl KvmVm {
    pub fn determine_vm_type(_config: &VmConfig) -> KvmVmType {
        0
    }

    pub fn create_guest_memfd(_config: &VmConfig, _fd: &OwnedFd) -> Result<Option<OwnedFd>> {
        Ok(None)
    }

    pub fn init(&self, _config: &VmConfig) -> Result<()> {
        Ok(())
    }
}
