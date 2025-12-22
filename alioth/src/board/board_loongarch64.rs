use std::marker::PhantomData;
use std::path::Path;

use crate::board::{Board, BoardConfig, Result, VcpuGuard};
use crate::hv::{Hypervisor, Vcpu, Vm};
use crate::loader::{InitState, Payload};

pub struct ArchBoard<V>
where
    V: Vm,
{
    v: PhantomData<V>,
}

impl<V: Vm> ArchBoard<V> {
    pub fn new<H>(_hv: &H, vm: &V, config: &BoardConfig) -> Result<Self>
    where
        H: Hypervisor<Vm = V>,
    {
        unimplemented!()
    }
}

impl<V> Board<V>
where
    V: Vm,
{
    pub fn encode_cpu_identity(&self, index: u16) -> u64 {
        unimplemented!()
    }

    pub fn setup_firmware(&self, _: &Path, _: &Payload) -> Result<InitState> {
        unimplemented!()
    }

    pub fn init_vcpu(&self, index: u16, vcpu: &mut V::Vcpu) -> Result<()> {
        unimplemented!()
    }

    pub fn reset_vcpu(&self, index: u16, vcpu: &mut V::Vcpu) -> Result<()> {
        unimplemented!()
    }

    pub fn create_ram(&self) -> Result<()> {
        unimplemented!()
    }

    pub fn coco_init(&self, _id: u16) -> Result<()> {
        Ok(())
    }

    pub fn coco_finalize(&self, _id: u16, _vcpus: &VcpuGuard) -> Result<()> {
        Ok(())
    }

    pub fn init_boot_vcpu(&self, vcpu: &mut V::Vcpu, init_state: &InitState) -> Result<()> {
        vcpu.set_regs(&init_state.regs)?;
        vcpu.set_sregs(&init_state.sregs)?;
        Ok(())
    }

    pub fn create_firmware_data(&self, init_state: &InitState) -> Result<()> {
        unimplemented!()
    }

    pub fn init_ap(&self, _id: u16, _vcpu: &mut V::Vcpu, _vcpus: &VcpuGuard) -> Result<()> {
        Ok(())
    }

    pub fn arch_init(&self) -> Result<()> {
        unimplemented!()
    }
}
