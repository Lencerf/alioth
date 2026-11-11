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

mod sev;
mod tdx;

use std::arch::x86_64::__cpuid;
use std::ops::RangeInclusive;
use std::os::fd::{FromRawFd, OwnedFd};
use std::path::Path;

use parking_lot::{MappedRwLockReadGuard, RwLock, RwLockReadGuard};
use snafu::ResultExt;
use zerocopy::IntoBytes;

use crate::arch::intr::{MsiAddrHi, MsiAddrLo};
use crate::arch::ioapic::NUM_PINS;
use crate::arch::msr::Msr;
use crate::hv::kvm::vcpu::KvmVcpu;
use crate::hv::kvm::vm::VmInner;
use crate::hv::kvm::x86_64::sev::SevFd;
use crate::hv::kvm::{KvmVm, kvm_error};
use crate::hv::{CocoSpec, Kvm, Result, VmSpec, error};
use crate::sys::kvm::{
    KvmCap, KvmCreateGuestMemfd, KvmMsrEntry, KvmNestedStateFormat, KvmVmType, KvmX2apicApiFlag,
    kvm_create_guest_memfd, kvm_set_identity_map_addr, kvm_set_tss_addr,
};

/// Hyper-V synthetic MSRs.
const HYPERV_MSRS: RangeInclusive<u32> = 0x4000_0000..=0x4000_01ff;

/// Whether `reset()` restores the MSR.
fn is_power_on_msr(index: &u32) -> bool {
    // Writing back the saved TSC would rewind it, and could leave the vCPUs
    // out of sync. The TSC keeps counting instead.
    if *index == Msr::TSC.raw() {
        return false;
    }
    // alioth exposes no Hyper-V interface, so the guest cannot change Hyper-V
    // MSRs, and accessing them would make KVM allocate Hyper-V state.
    !HYPERV_MSRS.contains(index)
}

fn detect_nested_format(kvm: &Kvm) -> Option<KvmNestedStateFormat> {
    kvm.check_extension(KvmCap::NESTED_STATE).ok()?;
    let leaf0 = __cpuid(0);
    let vendor = [leaf0.ebx, leaf0.edx, leaf0.ecx];
    match vendor.as_bytes() {
        b"GenuineIntel" => Some(KvmNestedStateFormat::VMX),
        b"AuthenticAMD" | b"HygonGenuine" => Some(KvmNestedStateFormat::SVM),
        _ => None,
    }
}

#[derive(Debug)]
enum PowerOnMsrs {
    Coco,
    Unset,
    Default(Box<[KvmMsrEntry]>),
    Configured(Box<[KvmMsrEntry]>),
}

impl PowerOnMsrs {
    fn needs_save(&self, configured: bool) -> bool {
        match self {
            Self::Unset => true,
            Self::Default(_) => configured,
            Self::Coco | Self::Configured(_) => false,
        }
    }
}

pub fn translate_msi_addr(addr_lo: u32, addr_hi: u32) -> (u32, u32) {
    let mut addr_lo = MsiAddrLo(addr_lo);
    let mut addr_hi = MsiAddrHi(addr_hi);
    if addr_lo.virt_dest_id_hi() == 0 || addr_lo.remappable() || addr_hi.dest_id_hi() != 0 {
        return (addr_lo.0, addr_hi.0);
    }

    addr_hi.set_dest_id_hi(addr_lo.virt_dest_id_hi() as u32);
    addr_lo.set_virt_dest_id_hi(0);
    (addr_lo.0, addr_hi.0)
}

#[derive(Debug)]
pub struct VmArch {
    pub sev_fd: Option<SevFd>,
    nested_format: Option<KvmNestedStateFormat>,
    has_xcrs: bool,
    power_on_msr_indices: Box<[u32]>,
    power_on_msrs: RwLock<PowerOnMsrs>,
}

impl VmArch {
    pub fn new(kvm: &Kvm, spec: &VmSpec) -> Result<Self> {
        if let Some(coco) = &spec.coco {
            let sev_fd = match coco {
                CocoSpec::AmdSev { .. } | CocoSpec::AmdSnp { .. } => {
                    let default_dev = Path::new("/dev/sev");
                    let dev_sev = kvm.spec.dev_sev.as_deref().unwrap_or(default_dev);
                    Some(SevFd::new(dev_sev)?)
                }
                CocoSpec::IntelTdx { .. } => None,
            };
            return Ok(VmArch {
                sev_fd,
                nested_format: None,
                has_xcrs: false,
                power_on_msr_indices: Box::new([]),
                power_on_msrs: RwLock::new(PowerOnMsrs::Coco),
            });
        }
        let power_on_msr_indices = kvm
            .get_msr_index_list()?
            .into_iter()
            .filter(is_power_on_msr)
            .collect();
        Ok(VmArch {
            sev_fd: None,
            nested_format: detect_nested_format(kvm),
            has_xcrs: kvm.check_extension(KvmCap::XCRS).is_ok(),
            power_on_msr_indices,
            power_on_msrs: RwLock::new(PowerOnMsrs::Unset),
        })
    }
}

impl VmInner {
    pub fn nested_format(&self) -> Option<KvmNestedStateFormat> {
        self.arch.nested_format
    }

    pub fn has_xcrs(&self) -> bool {
        self.arch.has_xcrs
    }

    /// Returns the power-on MSRs of the VM, or `None` if the VM is confidential.
    pub fn power_on_msrs(&self) -> Option<MappedRwLockReadGuard<'_, [KvmMsrEntry]>> {
        RwLockReadGuard::try_map(self.arch.power_on_msrs.read(), |state| match state {
            PowerOnMsrs::Default(msrs) | PowerOnMsrs::Configured(msrs) => Some(&**msrs),
            PowerOnMsrs::Coco | PowerOnMsrs::Unset => None,
        })
        .ok()
    }

    /// Captures the power-on MSRs once, and refreshes them once guest CPUID is
    /// configured.
    pub fn save_power_on_msrs(&self, configured: bool, vcpu: &KvmVcpu) -> Result<()> {
        if !self.arch.power_on_msrs.read().needs_save(configured) {
            return Ok(());
        }
        let mut state = self.arch.power_on_msrs.write();
        if !state.needs_save(configured) {
            return Ok(());
        }
        let msrs = vcpu.get_kvm_msrs(&self.arch.power_on_msr_indices)?.into();
        *state = if configured {
            PowerOnMsrs::Configured(msrs)
        } else {
            PowerOnMsrs::Default(msrs)
        };
        Ok(())
    }
}

impl KvmVm {
    pub fn determine_vm_type(spec: &VmSpec) -> KvmVmType {
        let Some(coco) = &spec.coco else {
            return KvmVmType::DEFAULT;
        };
        match coco {
            CocoSpec::AmdSev { .. } => KvmVmType::DEFAULT,
            CocoSpec::AmdSnp { .. } => KvmVmType::SNP,
            CocoSpec::IntelTdx { .. } => KvmVmType::TDX,
        }
    }

    pub fn create_guest_memfd(spec: &VmSpec, fd: &OwnedFd) -> Result<Option<OwnedFd>> {
        let Some(coco) = &spec.coco else {
            return Ok(None);
        };
        if !matches!(coco, CocoSpec::AmdSnp { .. } | CocoSpec::IntelTdx { .. }) {
            return Ok(None);
        }
        let mut gmem = KvmCreateGuestMemfd {
            size: 1 << 48,
            ..Default::default()
        };
        let fd = unsafe { kvm_create_guest_memfd(fd, &mut gmem) }.context(kvm_error::GuestMemfd)?;
        Ok(Some(unsafe { OwnedFd::from_raw_fd(fd) }))
    }

    pub fn init(&self, spec: &VmSpec) -> Result<()> {
        let x2apic_caps =
            KvmX2apicApiFlag::USE_32BIT_IDS | KvmX2apicApiFlag::DISABLE_BROADCAST_QUIRK;
        if let Err(e) = self.vm.enable_cap(KvmCap::X2APIC_API, x2apic_caps.bits()) {
            log::error!("Failed to enable KVM_CAP_X2APIC_API: {e:?}");
        }
        self.vm.enable_cap(KvmCap::SPLIT_IRQCHIP, NUM_PINS as u64)?;
        // TODO should be in parameters
        unsafe { kvm_set_tss_addr(&self.vm.fd, 0xf000_0000) }.context(error::SetVmParam)?;
        unsafe { kvm_set_identity_map_addr(&self.vm.fd, &0xf000_3000) }
            .context(error::SetVmParam)?;

        if let Some(coco) = &spec.coco {
            match coco {
                CocoSpec::AmdSev { policy } => self.sev_init(*policy),
                CocoSpec::AmdSnp { .. } => self.snp_init(),
                CocoSpec::IntelTdx { .. } => self.tdx_init(),
            }?;
        }

        Ok(())
    }
}

#[cfg(test)]
#[path = "vm_amd64_test.rs"]
mod test;
