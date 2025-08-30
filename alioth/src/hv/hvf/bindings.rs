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

use std::os::raw::c_void;

use bitflags::bitflags;

use crate::arch::reg::{EsrEl2, SReg};
use crate::c_enum;

c_enum! {
    pub struct HvReg(u32);
    {
        X0 = 0;
        X1 = 1;
        X2 = 2;
        X3 = 3;
        X4 = 4;
        X5 = 5;
        X6 = 6;
        X7 = 7;
        X8 = 8;
        X9 = 9;
        X10 = 10;
        X11 = 11;
        X12 = 12;
        X13 = 13;
        X14 = 14;
        X15 = 15;
        X16 = 16;
        X17 = 17;
        X18 = 18;
        X19 = 19;
        X20 = 20;
        X21 = 21;
        X22 = 22;
        X23 = 23;
        X24 = 24;
        X25 = 25;
        X26 = 26;
        X27 = 27;
        X28 = 28;
        X29 = 29;
        X30 = 30;
        PC= 31;
        FPCR = 32;
        FPSR = 33;
        CPSR = 34;
    }
}

c_enum! {
    #[derive(Default)]
    pub struct HvExitReason(u32);
    {
        CANCEL = 0;
        EXCEPTION = 1;
        VTIMER_ACTIVATED = 2;
        UNKNOWN = 3;
    }
}

#[repr(C)]
#[derive(Debug, Clone, Default)]
pub struct HvVcpuExitException {
    pub syndrome: EsrEl2,
    pub virtual_address: u64,
    pub physical_address: u64,
}

#[repr(C)]
#[derive(Debug, Clone, Default)]
pub struct HvVcpuExit {
    pub reason: HvExitReason,
    pub exception: HvVcpuExitException,
}

bitflags! {
    #[derive(Debug, Clone, Copy, Default)]
    #[repr(transparent)]
    pub struct HvMemoryFlag: u64 {
        const READ = 1 << 0;
        const WRITE = 1 << 1;
        const EXEC = 1 << 2;
    }
}

#[link(name = "Hypervisor", kind = "framework")]
unsafe extern "C" {
    pub fn hv_vm_create(config: *mut i32) -> i32;
    pub fn hv_vm_destroy() -> i32;
    pub fn hv_vcpu_create(vcpu: &mut u64, exit: &mut *mut HvVcpuExit, config: *mut c_void) -> i32;
    pub fn hv_vcpu_destroy(vcpu: u64) -> i32;
    pub fn hv_vcpu_get_reg(vcpu: u64, reg: HvReg, value: &mut u64) -> i32;
    pub fn hv_vcpu_set_reg(vcpu: u64, reg: HvReg, value: u64) -> i32;
    pub fn hv_vcpu_set_sys_reg(vcpu: u64, reg: SReg, val: u64) -> i32;
    pub fn hv_vcpu_get_sys_reg(vcpu: u64, reg: SReg, val: &mut u64) -> i32;
    pub fn hv_vcpu_run(vcpu: u64) -> i32;
    pub fn hv_vm_map(addr: *const u8, ipa: u64, size: usize, flags: HvMemoryFlag) -> i32;
    pub fn hv_vm_unmap(ipa: u64, size: usize) -> i32;
    pub fn hv_gic_config_create() -> usize;
    pub fn hv_gic_create(gic_config: usize) -> i32;
    pub fn hv_gic_config_set_distributor_base(config: usize, distributor_base_address: u64) -> i32;
    pub fn hv_gic_config_set_redistributor_base(
        config: usize,
        redistributor_base_address: u64,
    ) -> i32;
    pub fn hv_gic_get_redistributor_base(vcpu: u64, redistributor_base_address: &mut u64) -> i32;
    pub fn hv_gic_get_redistributor_region_size(redistributor_region_size: &mut usize) -> i32;
    pub fn hv_gic_get_redistributor_size(redistributor_size: &mut usize) -> i32;
    pub fn hv_gic_get_distributor_size(distributor_size: &mut usize) -> i32;
    pub fn hv_gic_set_spi(intid: u32, level: bool) -> i32;
    pub fn hv_gic_get_spi_interrupt_range(
        spi_intid_base: &mut u32,
        spi_intid_count: &mut u32,
    ) -> i32;

}

// #[repr(C)]
// #[derive(Debug, Copy, Clone)]
// pub struct hv_vm_config_s {
//     _unused: [u8; 0],
// }
// pub type hv_vm_config_t = *mut hv_vm_config_s;
// pub type hv_ipa_t = u64;
// unsafe extern "C" {
//     #[doc = "@abstract Create a GIC configuration object\n@result hv_gic_config_t A new GIC configuration object. Release with os_release\nwhen no longer needed.\n@discussion\nCreate the GIC configuration after the virtual machine has been created."]
//     pub fn hv_gic_config_create() -> hv_gic_config_t;
// }
// unsafe extern "C" {
//     #[doc = "@abstract Set the GIC distributor region base address.\n@param config GIC configuration object.\n@param distributor_base_address Guest physical address for distributor.\n@result HV_SUCCESS on success, an error code otherwise.\n@discussion\nGuest physical address for distributor base aligned to byte value\nreturned by hv_gic_get_distributor_base_alignment."]
//     pub fn hv_gic_config_set_distributor_base(
//         config: hv_gic_config_t,
//         distributor_base_address: hv_ipa_t,
//     ) -> hv_return_t;
// }
// unsafe extern "C" {
//     #[doc = "@abstract Set the GIC redistributor region base address.\n@param config GIC configuration object.\n@param redistributor_base_address Guest physical address for redistributor.\n@result HV_SUCCESS on success, an error code otherwise.\n@discussion\nGuest physical address for redistributor base aligned to byte value\nreturned by hv_gic_get_redistributor_base_alignment. The redistributor\nregion will contain redistributors for all vCPUs supported by the\nvirtual machine."]
//     pub fn hv_gic_config_set_redistributor_base(
//         config: hv_gic_config_t,
//         redistributor_base_address: hv_ipa_t,
//     ) -> hv_return_t;
// }
// unsafe extern "C" {
//     #[doc = "@abstract Set the GIC MSI region base address.\n@param config GIC configuration object.\n@param msi_region_base_address Guest physical address for MSI region.\n@result HV_SUCCESS on success, an error code otherwise.\n@discussion\nGuest physical address for MSI region base aligned to byte value\nreturned by hv_gic_get_msi_region_base_alignment.\n\nFor MSI support, you also need to set the interrupt range with\nhv_gic_config_set_msi_interrupt_range()."]
//     pub fn hv_gic_config_set_msi_region_base(
//         config: hv_gic_config_t,
//         msi_region_base_address: hv_ipa_t,
//     ) -> hv_return_t;
// }
// unsafe extern "C" {
//     #[doc = "@abstract Sets the range of MSIs supported.\n@param config GIC configuration object.\n@param msi_intid_base Lowest MSI interrupt number.\n@param msi_intid_count Number of MSIs.\n@result HV_SUCCESS on success, an error code otherwise.\n@discussion\nConfigures the range of identifiers supported for MSIs. If it is outside of\nthe range given by hv_gic_get_spi_interrupt_range() an error will be\nreturned.\n\nFor MSI support, you also need to set the region base address with\nhv_gic_config_set_msi_region_base()."]
//     pub fn hv_gic_config_set_msi_interrupt_range(
//         config: hv_gic_config_t,
//         msi_intid_base: u32,
//         msi_intid_count: u32,
//     ) -> hv_return_t;
// }

// unsafe extern "C" {
//     #[doc = "@abstract Create a GIC v3 device for a VM configuration.\n@param gic_config GIC configuration object.\n@result HV_SUCCESS on success, an error code otherwise.\n@discussion\nThis function can be used to create an ARM Generic Interrupt Controller\n(GIC) v3 device. There must only be a single instance of this device per\nvirtual machine. The device supports a distributor, redistributors, msi and\nGIC CPU system registers. When EL2 is enabled, the device supports GIC\nhypervisor control registers which are used by the guest hypervisor for\ninjecting interrupts to its guest. hv_vcpu_{get/set}_interrupt functions\nare unsupported for injecting interrupts to a nested guest.\n\nThe hv_gic_create() API must only be called after a virtual machine has\nbeen created. It must also be done before vCPU's have been created so that\nGIC CPU system resources can be allocated. If either of these conditions\naren't met an error is returned.\n\nGIC v3 uses affinity based interrupt routing. vCPU's must set affinity\nvalues in their MPIDR_EL1 register. Once the virtual machine vcpus are\nrunning, its topology is considered final. Destroy vcpus only when you are\ntearing down the virtual machine.\n\nGIC MSI support is only provided if both an MSI region base address is\nconfigured and an MSI interrupt range is set."]
//     pub fn hv_gic_create(gic_config: hv_gic_config_t) -> hv_return_t;
// }
// unsafe extern "C" {
//     #[doc = "@abstract Trigger a Shared Peripheral Interrupt (SPI).\n@param intid Interrupt number of the SPI.\n@param level High or low level for an interrupt. Setting level also\ncauses an edge on the line for an edge triggered interrupt.\n@result HV_SUCCESS on success, an error code otherwise.\n@discussion\nLevel interrupts can be caused by setting a level value. If you want to\ncause an edge interrupt, call with a level of true. A level of false, for\nan edge interrupt will be ignored.\n\nAn interrupt identifier outside of hv_gic_get_spi_interrupt_range() or in\nthe MSI interrupt range will return a HV_BAD_ARGUMENT error code."]
//     pub fn hv_gic_set_spi(intid: u32, level: bool) -> hv_return_t;
// }
// unsafe extern "C" {
//     #[doc = "@abstract Send a Message Signaled Interrupt (MSI).\n@param address Guest physical address for message based SPI.\n@param intid Interrupt identifier for the message based SPI.\n@result HV_SUCCESS on success, an error code otherwise.\n@discussion\nUse the address of the HV_GIC_REG_GICM_SET_SPI_NSR register in the MSI frame."]
//     pub fn hv_gic_send_msi(address: hv_ipa_t, intid: u32) -> hv_return_t;
// }
// unsafe extern "C" {
//     #[doc = "@abstract Read a GIC distributor register.\n@param reg GIC distributor register enum.\n@param value Pointer to distributor register value (written on success).\n@result HV_SUCCESS on success, an error code otherwise.\n@discussion\nGIC distributor register enum values are equal to the device register\noffsets defined in the ARM GIC v3 specification. The client can use the\noffset alternatively, while looping through large register arrays."]
//     pub fn hv_gic_get_distributor_reg(
//         reg: hv_gic_distributor_reg_t,
//         value: *mut u64,
//     ) -> hv_return_t;
// }
// unsafe extern "C" {
//     #[doc = "@abstract Write a GIC distributor register.\n@param reg GIC distributor register enum.\n@param value GIC distributor register value to be written.\n@result HV_SUCCESS on success, an error code otherwise.\n@discussion\nGIC distributor register enum values are equal to the device register\noffsets defined in the ARM GIC v3 specification. The client can use the\noffset alternatively, while looping through large register arrays."]
//     pub fn hv_gic_set_distributor_reg(reg: hv_gic_distributor_reg_t, value: u64) -> hv_return_t;
// }
// unsafe extern "C" {
//     #[doc = "@abstract Gets the redistributor base guest physical address for the given vcpu.\n@param vcpu Handle for the vcpu.\n@param redistributor_base_address Pointer to the redistributor base guest physical address (written on success).\n@result HV_SUCCESS on success, an error code otherwise.\n@discussion\nMust be called after the affinity of the given vCPU has been set in its MPIDR_EL1 register."]
//     pub fn hv_gic_get_redistributor_base(
//         vcpu: hv_vcpu_t,
//         redistributor_base_address: *mut hv_ipa_t,
//     ) -> hv_return_t;
// }
// unsafe extern "C" {
//     #[doc = "@abstract Read a GIC redistributor register.\n@param vcpu Redistributor block for the vcpu.\n@param reg GIC redistributor register enum.\n@param value Pointer to redistributor register value (written on success).\n@result HV_SUCCESS on success, an error code otherwise.\n@discussion\nMust be called by the owning thread.\n\nGIC redistributor register enum values are equal to the device register\noffsets defined in the ARM GIC v3 specification. The client can use the\noffset alternatively, while looping through large register arrays."]
//     pub fn hv_gic_get_redistributor_reg(
//         vcpu: hv_vcpu_t,
//         reg: hv_gic_redistributor_reg_t,
//         value: *mut u64,
//     ) -> hv_return_t;
// }
// unsafe extern "C" {
//     #[doc = "@abstract Write a GIC redistributor register.\n@param vcpu Redistributor block for the vcpu.\n@param reg GIC redistributor register enum.\n@param value GIC redistributor register value to be written.\n@result HV_SUCCESS on success, an error code otherwise.\n@discussion\nMust be called by the owning thread.\n\nGIC redistributor register enum values are equal to the device register\noffsets defined in the ARM GIC v3 specification. The client can use the\noffset alternatively, while looping through large register arrays."]
//     pub fn hv_gic_set_redistributor_reg(
//         vcpu: hv_vcpu_t,
//         reg: hv_gic_redistributor_reg_t,
//         value: u64,
//     ) -> hv_return_t;
// }
// unsafe extern "C" {
//     #[doc = "@abstract Read a GIC ICC cpu system register.\n@param vcpu Handle for the vcpu.\n@param reg GIC ICC system register enum.\n@param value Pointer to ICC register value (written on success).\n@result HV_SUCCESS on success, an error code otherwise.\n@discussion\nMust be called by the owning thread."]
//     pub fn hv_gic_get_icc_reg(
//         vcpu: hv_vcpu_t,
//         reg: hv_gic_icc_reg_t,
//         value: *mut u64,
//     ) -> hv_return_t;
// }
// unsafe extern "C" {
//     #[doc = "@abstract Write a GIC ICC cpu system register.\n@param vcpu Handle for the vcpu.\n@param reg GIC ICC system register enum.\n@param value GIC ICC register value to be written.\n@result HV_SUCCESS on success, an error code otherwise.\n@discussion\nMust be called by the owning thread."]
//     pub fn hv_gic_set_icc_reg(vcpu: hv_vcpu_t, reg: hv_gic_icc_reg_t, value: u64) -> hv_return_t;
// }
// unsafe extern "C" {
//     #[doc = "@abstract Read a GIC ICH virtualization control system register.\n@param vcpu Handle for the vcpu.\n@param reg GIC ICH system register enum.\n@param value Pointer to ICH register value (written on success).\n@result HV_SUCCESS on success, an error code otherwise.\n@discussion\nICH registers are only available when EL2 is enabled, otherwise returns\nan error.\n\nMust be called by the owning thread."]
//     pub fn hv_gic_get_ich_reg(
//         vcpu: hv_vcpu_t,
//         reg: hv_gic_ich_reg_t,
//         value: *mut u64,
//     ) -> hv_return_t;
// }
// unsafe extern "C" {
//     #[doc = "@abstract Write a GIC ICH virtualization control system register.\n@param vcpu Handle for the vcpu.\n@param reg GIC ICH system register enum.\n@param value GIC ICH register value to be written.\n@result HV_SUCCESS on success, an error code otherwise.\n@discussion\nICH registers are only available when EL2 is enabled, otherwise returns\nan error.\n\nMust be called by the owning thread."]
//     pub fn hv_gic_set_ich_reg(vcpu: hv_vcpu_t, reg: hv_gic_ich_reg_t, value: u64) -> hv_return_t;
// }
// unsafe extern "C" {
//     #[doc = "@abstract Read a GIC ICV system register.\n@param vcpu Handle for the vcpu.\n@param reg GIC ICV system register enum.\n@param value Pointer to ICV register value (written on success).\n@result HV_SUCCESS on success, an error code otherwise.\n@discussion\nICV registers are only available when EL2 is enabled, otherwise returns\nan error.\n\nMust be called by the owning thread."]
//     pub fn hv_gic_get_icv_reg(
//         vcpu: hv_vcpu_t,
//         reg: hv_gic_icv_reg_t,
//         value: *mut u64,
//     ) -> hv_return_t;
// }
// unsafe extern "C" {
//     #[doc = "@abstract Write a GIC ICV system register.\n@param vcpu Handle for the vcpu.\n@param reg GIC ICV system register enum.\n@param value GIC ICV register value to be written.\n@result HV_SUCCESS on success, an error code otherwise.\n@discussion\nICV registers are only available when EL2 is enabled, otherwise returns\nan error.\n\nMust be called by the owning thread."]
//     pub fn hv_gic_set_icv_reg(vcpu: hv_vcpu_t, reg: hv_gic_icv_reg_t, value: u64) -> hv_return_t;
// }
// unsafe extern "C" {
//     #[doc = "@abstract Read a GIC distributor MSI register.\n@param reg GIC distributor MSI register enum.\n@param value Pointer to distributor MSI register value (written on success).\n@result HV_SUCCESS on success, an error code otherwise."]
//     pub fn hv_gic_get_msi_reg(reg: hv_gic_msi_reg_t, value: *mut u64) -> hv_return_t;
// }
// unsafe extern "C" {
//     #[doc = "@abstract Write a GIC distributor MSI register.\n@param reg GIC distributor MSI register enum.\n@param value GIC distributor MSI register value to be written.\n@result HV_SUCCESS on success, an error code otherwise."]
//     pub fn hv_gic_set_msi_reg(reg: hv_gic_msi_reg_t, value: u64) -> hv_return_t;
// }
// unsafe extern "C" {
//     #[doc = "@abstract Set state for GIC device to be restored.\n@param gic_state_data Pointer to the state buffer to set GIC with.\n@param gic_state_size Size of GIC state buffer.\n@result HV_SUCCESS on success, an error code otherwise.\n@discussion\nGIC state can only be restored after a GIC device and vcpus have been\ncreated and must be done before vcpu's are run. The rest of the virtual\nmachine including GIC CPU registers must also be restored compatibly with\nthe gic_state.\n\nIn some cases hv_gic_set_state() can fail if a software update has changed\nthe host in a way that would be incompatible with the previous format."]
//     pub fn hv_gic_set_state(
//         gic_state_data: *const ::std::os::raw::c_void,
//         gic_state_size: usize,
//     ) -> hv_return_t;
// }
// unsafe extern "C" {
//     #[doc = "@abstract Reset the GIC device.\n@result HV_SUCCESS on success, an error code otherwise.\n@discussion\nWhen the virtual machine is being reset, call this function to reset the\nGIC distributor, redistributor registers and the internal state of the\ndevice."]
//     pub fn hv_gic_reset() -> hv_return_t;
// }
