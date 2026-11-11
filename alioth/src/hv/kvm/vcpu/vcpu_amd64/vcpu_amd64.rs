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

mod tdx;

use std::arch::x86_64::CpuidResult;
use std::collections::HashMap;
use std::iter::zip;
use std::os::fd::{FromRawFd, OwnedFd};

use libc::EINVAL;
use snafu::ResultExt;
use zerocopy::{transmute_mut, transmute_ref};

use crate::arch::cpuid::CpuidIn;
use crate::arch::intr::DeliveryMode;
use crate::arch::msr::{ApicBase, Efer, Msr};
use crate::arch::reg::{
    Cr0, Cr3, Cr4, DtReg, DtRegVal, Reg, Rflags, SReg, SegAccess, SegReg, SegRegVal,
};
use crate::hv::kvm::kvm_error;
use crate::hv::kvm::vcpu::KvmVcpu;
use crate::hv::kvm::vm::KvmVm;
use crate::hv::{Error, Result, error};
use crate::sys::kvm::{
    KVM_MAX_CPUID_ENTRIES, KvmCpuid2, KvmCpuid2Flag, KvmCpuidEntry2, KvmDebugregs, KvmDtable,
    KvmLapicState, KvmMpState, KvmMsrEntry, KvmMsrs, KvmNestedState, KvmNestedStateFlag,
    KvmNestedStateFormat, KvmNestedStateHdr, KvmRegs, KvmSegment, KvmSregs, KvmVcpuEventValid,
    KvmVcpuEvents, KvmVmxNestedStateHdr, KvmXcr, KvmXcrs, KvmXsave, MSRS_PER_CALL, kvm_create_vcpu,
    kvm_get_lapic, kvm_get_msrs, kvm_get_regs, kvm_get_sregs, kvm_get_xsave, kvm_kvmclock_ctrl,
    kvm_set_cpuid2, kvm_set_debugregs, kvm_set_lapic, kvm_set_mp_state, kvm_set_msrs,
    kvm_set_nested_state, kvm_set_regs, kvm_set_sregs, kvm_set_vcpu_events, kvm_set_xcrs,
    kvm_set_xsave,
};

/// Default Family/Model/Stepping (P6) placed in EDX at power-on before guest
/// CPUID is configured.
const DEFAULT_CPUID_1_EAX: u32 = 0x600;

#[derive(Debug)]
pub struct VcpuArch {
    pub io_index: usize,
    apic_id: u32,
    cpuid_1_eax: u32,
}

impl VcpuArch {
    pub fn new(id: u64) -> Self {
        Self {
            io_index: 0,
            apic_id: id as u32,
            cpuid_1_eax: DEFAULT_CPUID_1_EAX,
        }
    }
}

macro_rules! set_kvm_sreg {
    ($kvm_sregs:ident, $sreg:ident, $val:expr) => {
        match $sreg {
            SReg::Cr0 => $kvm_sregs.cr0 = Cr0::from_bits_retain($val),
            SReg::Cr2 => $kvm_sregs.cr2 = $val,
            SReg::Cr3 => $kvm_sregs.cr3 = Cr3::from_bits_retain($val),
            SReg::Cr4 => $kvm_sregs.cr4 = Cr4::from_bits_retain($val),
            SReg::Cr8 => $kvm_sregs.cr8 = $val,
        }
    };
}

macro_rules! set_kvm_dt_reg {
    ($kvm_sregs:ident, $dt_reg:ident, $val:expr) => {
        let target = match $dt_reg {
            DtReg::Idtr => &mut $kvm_sregs.idt,
            DtReg::Gdtr => &mut $kvm_sregs.gdt,
        };
        target.limit = $val.limit;
        target.base = $val.base;
    };
}

macro_rules! set_kvm_seg_reg {
    ($kvm_sregs:ident, $seg_reg:ident, $val:expr) => {
        let target = match $seg_reg {
            SegReg::Cs => &mut $kvm_sregs.cs,
            SegReg::Ds => &mut $kvm_sregs.ds,
            SegReg::Es => &mut $kvm_sregs.es,
            SegReg::Fs => &mut $kvm_sregs.fs,
            SegReg::Gs => &mut $kvm_sregs.gs,
            SegReg::Ss => &mut $kvm_sregs.ss,
            SegReg::Tr => &mut $kvm_sregs.tr,
            SegReg::Ldtr => &mut $kvm_sregs.ldt,
        };
        target.selector = $val.selector;
        target.base = $val.base;
        target.limit = $val.limit;
        target.type_ = $val.access.seg_type() as u8;
        target.s = $val.access.s_code_data() as u8;
        target.dpl = $val.access.priv_level() as u8;
        target.present = $val.access.present() as u8;
        target.avl = $val.access.available() as u8;
        target.db = $val.access.db_size_32() as u8;
        target.g = $val.access.granularity() as u8;
        target.l = $val.access.l_64bit() as u8;
        target.unusable = $val.access.unusable() as u8;
    };
}

macro_rules! get_kvm_sreg {
    ($kvm_sregs:ident, $sreg:ident) => {
        match $sreg {
            SReg::Cr0 => $kvm_sregs.cr0.bits(),
            SReg::Cr2 => $kvm_sregs.cr2,
            SReg::Cr3 => $kvm_sregs.cr3.bits(),
            SReg::Cr4 => $kvm_sregs.cr4.bits(),
            SReg::Cr8 => $kvm_sregs.cr8,
        }
    };
}

macro_rules! fix_kvm_efer {
    ($kvm_sregs:ident) => {
        if $kvm_sregs.cr0.contains(Cr0::PG) && $kvm_sregs.cr4.contains(Cr4::PAE) {
            $kvm_sregs.efer |= Efer::LME | Efer::LMA;
        } else {
            $kvm_sregs.efer &= !(Efer::LME | Efer::LMA);
        }
    };
}

macro_rules! get_kvm_dt_reg {
    ($kvm_sregs:ident, $dt_reg:ident) => {{
        let target = match $dt_reg {
            DtReg::Idtr => $kvm_sregs.idt,
            DtReg::Gdtr => $kvm_sregs.gdt,
        };
        DtRegVal {
            limit: target.limit,
            base: target.base,
        }
    }};
}

macro_rules! get_kvm_seg_reg {
    ($kvm_sregs:ident, $seg_reg:ident) => {{
        let kvm_segment = match $seg_reg {
            SegReg::Cs => $kvm_sregs.cs,
            SegReg::Ds => $kvm_sregs.ds,
            SegReg::Es => $kvm_sregs.es,
            SegReg::Fs => $kvm_sregs.fs,
            SegReg::Gs => $kvm_sregs.gs,
            SegReg::Ss => $kvm_sregs.ss,
            SegReg::Tr => $kvm_sregs.tr,
            SegReg::Ldtr => $kvm_sregs.ldt,
        };
        let access = (kvm_segment.unusable as u32) << 16
            | (kvm_segment.g as u32) << 15
            | (kvm_segment.db as u32) << 14
            | (kvm_segment.l as u32) << 13
            | (kvm_segment.avl as u32) << 12
            | (kvm_segment.present as u32) << 7
            | (kvm_segment.dpl as u32) << 5
            | (kvm_segment.s as u32) << 4
            | (kvm_segment.type_ as u32);
        SegRegVal {
            selector: kvm_segment.selector,
            base: kvm_segment.base,
            limit: kvm_segment.limit,
            access: SegAccess(access),
        }
    }};
}

impl KvmVcpu {
    pub fn create_vcpu(vm: &KvmVm, _: u16, identity: u64) -> Result<OwnedFd> {
        let apic_id = identity as u32;
        let fd = unsafe { kvm_create_vcpu(&vm.vm.fd, apic_id) }.context(error::CreateVcpu)?;
        Ok(unsafe { OwnedFd::from_raw_fd(fd) })
    }

    pub fn init(&self) -> Result<()> {
        self.vm.save_power_on_msrs(false, self)
    }

    /// Tells the guest that the VMM stopped this vCPU for a while, so that
    /// its soft lockup watchdog does not take the gap for a lockup.
    pub fn kvmclock_ctrl(&mut self) -> Result<()> {
        let ret = unsafe { kvm_kvmclock_ctrl(&self.fd) };
        // The guest has not enabled kvmclock on this vCPU, e.g. it still
        // runs the firmware, so there is nothing to tell.
        if matches!(&ret, Err(e) if e.raw_os_error() == Some(EINVAL)) {
            return Ok(());
        }
        ret.context(kvm_error::KvmClockCtrl)?;
        Ok(())
    }

    fn get_kvm_regs(&self) -> Result<KvmRegs> {
        let kvm_regs = unsafe { kvm_get_regs(&self.fd) }.context(error::VcpuReg)?;
        Ok(kvm_regs)
    }

    fn set_kvm_regs(&self, kvm_regs: &KvmRegs) -> Result<()> {
        unsafe { kvm_set_regs(&self.fd, kvm_regs) }.context(error::VcpuReg)?;
        Ok(())
    }

    pub fn kvm_set_regs(&self, vals: &[(Reg, u64)]) -> Result<()> {
        let mut kvm_regs = self.get_kvm_regs()?;
        for (reg, val) in vals {
            match reg {
                Reg::Rax => kvm_regs.rax = *val,
                Reg::Rbx => kvm_regs.rbx = *val,
                Reg::Rcx => kvm_regs.rcx = *val,
                Reg::Rdx => kvm_regs.rdx = *val,
                Reg::Rsi => kvm_regs.rsi = *val,
                Reg::Rdi => kvm_regs.rdi = *val,
                Reg::Rsp => kvm_regs.rsp = *val,
                Reg::Rbp => kvm_regs.rbp = *val,
                Reg::R8 => kvm_regs.r8 = *val,
                Reg::R9 => kvm_regs.r9 = *val,
                Reg::R10 => kvm_regs.r10 = *val,
                Reg::R11 => kvm_regs.r11 = *val,
                Reg::R12 => kvm_regs.r12 = *val,
                Reg::R13 => kvm_regs.r13 = *val,
                Reg::R14 => kvm_regs.r14 = *val,
                Reg::R15 => kvm_regs.r15 = *val,
                Reg::Rip => kvm_regs.rip = *val,
                Reg::Rflags => kvm_regs.rflags = *val,
            }
        }
        self.set_kvm_regs(&kvm_regs)
    }

    pub fn kvm_get_reg(&self, reg: Reg) -> Result<u64> {
        let kvm_regs = self.get_kvm_regs()?;
        let val = match reg {
            Reg::Rax => kvm_regs.rax,
            Reg::Rbx => kvm_regs.rbx,
            Reg::Rcx => kvm_regs.rcx,
            Reg::Rdx => kvm_regs.rdx,
            Reg::Rsi => kvm_regs.rsi,
            Reg::Rdi => kvm_regs.rdi,
            Reg::Rsp => kvm_regs.rsp,
            Reg::Rbp => kvm_regs.rbp,
            Reg::R8 => kvm_regs.r8,
            Reg::R9 => kvm_regs.r9,
            Reg::R10 => kvm_regs.r10,
            Reg::R11 => kvm_regs.r11,
            Reg::R12 => kvm_regs.r12,
            Reg::R13 => kvm_regs.r13,
            Reg::R14 => kvm_regs.r14,
            Reg::R15 => kvm_regs.r15,
            Reg::Rip => kvm_regs.rip,
            Reg::Rflags => kvm_regs.rflags,
        };
        Ok(val)
    }

    pub fn kvm_set_sregs(
        &mut self,
        sregs: &[(SReg, u64)],
        seg_regs: &[(SegReg, SegRegVal)],
        dt_regs: &[(DtReg, DtRegVal)],
    ) -> Result<(), Error> {
        let mut kvm_sregs = unsafe { kvm_get_sregs(&self.fd) }.context(error::VcpuReg)?;
        for (reg, val) in sregs {
            set_kvm_sreg!(kvm_sregs, reg, *val)
        }
        for (reg, val) in dt_regs {
            set_kvm_dt_reg!(kvm_sregs, reg, val);
        }
        for (reg, val) in seg_regs {
            set_kvm_seg_reg!(kvm_sregs, reg, val);
        }
        fix_kvm_efer!(kvm_sregs);
        unsafe { kvm_set_sregs(&self.fd, &kvm_sregs) }.context(error::VcpuReg)?;
        Ok(())
    }

    pub fn kvm_get_dt_reg(&self, reg: DtReg) -> Result<DtRegVal> {
        let kvm_sregs = unsafe { kvm_get_sregs(&self.fd) }.context(error::VcpuReg)?;
        let val = get_kvm_dt_reg!(kvm_sregs, reg);
        Ok(val)
    }

    pub fn kvm_get_seg_reg(&self, reg: SegReg) -> Result<SegRegVal> {
        let kvm_sregs = unsafe { kvm_get_sregs(&self.fd) }.context(error::VcpuReg)?;
        let val = get_kvm_seg_reg!(kvm_sregs, reg);
        Ok(val)
    }

    pub fn kvm_get_sreg(&self, reg: SReg) -> Result<u64> {
        let kvm_sregs = unsafe { kvm_get_sregs(&self.fd) }.context(error::VcpuReg)?;
        let val = get_kvm_sreg!(kvm_sregs, reg);
        Ok(val)
    }

    pub fn kvm_set_cpuids(&mut self, cpuids: &HashMap<CpuidIn, CpuidResult>) -> Result<(), Error> {
        if cpuids.len() > KVM_MAX_CPUID_ENTRIES {
            return kvm_error::CpuidTableTooLong.fail()?;
        }
        let mut kvm_cpuid2 = KvmCpuid2 {
            nent: cpuids.len() as u32,
            padding: 0,
            entries: [KvmCpuidEntry2::default(); KVM_MAX_CPUID_ENTRIES],
        };
        for ((in_, out), entry) in zip(cpuids, &mut kvm_cpuid2.entries) {
            entry.eax = out.eax;
            entry.ebx = out.ebx;
            entry.ecx = out.ecx;
            entry.edx = out.edx;
            entry.function = in_.func;
            if let Some(index) = in_.index {
                entry.index = index;
                entry.flags = KvmCpuid2Flag::SIGNIFCANT_INDEX;
            } else {
                entry.flags = KvmCpuid2Flag::empty();
            }
        }
        unsafe { kvm_set_cpuid2(&self.fd, &kvm_cpuid2) }.context(error::GuestCpuid)?;
        let leaf1 = CpuidIn {
            func: 0x1,
            index: None,
        };
        if let Some(out) = cpuids.get(&leaf1) {
            self.arch.cpuid_1_eax = out.eax;
        }
        self.vm.save_power_on_msrs(true, self)?;
        Ok(())
    }

    /// Reads the MSRs in `indices`.
    pub fn get_kvm_msrs(&self, indices: &[u32]) -> Result<Vec<KvmMsrEntry>> {
        let mut kvm_msrs = KvmMsrs {
            nmsrs: 0,
            _pad: 0,
            entries: [KvmMsrEntry::default(); MSRS_PER_CALL],
        };
        let mut msrs = Vec::with_capacity(indices.len());
        for chunk in indices.chunks(MSRS_PER_CALL) {
            kvm_msrs.nmsrs = chunk.len() as u32;
            for (entry, index) in zip(&mut kvm_msrs.entries, chunk) {
                entry.index = *index;
            }
            let n = unsafe { kvm_get_msrs(&self.fd, &mut kvm_msrs) }.context(error::GuestMsr)?;
            // KVM stops at the first MSR that it cannot read.
            if let Some(index) = chunk.get(n as usize) {
                return error::GetMsr { index: *index }.fail();
            }
            msrs.extend_from_slice(&kvm_msrs.entries[..chunk.len()]);
        }
        Ok(msrs)
    }

    fn set_kvm_msrs<T>(&self, msrs: &[T], mut encode: impl FnMut(&T) -> KvmMsrEntry) -> Result<()> {
        let mut kvm_msrs = KvmMsrs {
            nmsrs: 0,
            _pad: 0,
            entries: [KvmMsrEntry::default(); MSRS_PER_CALL],
        };
        for chunk in msrs.chunks(MSRS_PER_CALL) {
            kvm_msrs.nmsrs = chunk.len() as u32;
            for (entry, msr) in zip(&mut kvm_msrs.entries, chunk) {
                *entry = encode(msr);
            }
            let n = unsafe { kvm_set_msrs(&self.fd, &kvm_msrs) }.context(error::GuestMsr)?;
            // KVM stops at the first MSR that it cannot write.
            if let Some(msr) = kvm_msrs.entries[..chunk.len()].get(n as usize) {
                return error::SetMsr { index: msr.index }.fail();
            }
        }
        Ok(())
    }

    pub fn kvm_set_msrs(&mut self, msrs: &[(Msr, u64)]) -> Result<()> {
        self.set_kvm_msrs(msrs, |&(index, data)| KvmMsrEntry {
            index: index.raw(),
            data,
            ..Default::default()
        })
    }

    pub fn kvm_get_xsave(&self, xsave: &mut [u32; 1024]) -> Result<()> {
        let buf = transmute_mut!(xsave);
        unsafe { kvm_get_xsave(&self.fd, buf) }.context(error::GuestXsave)?;
        Ok(())
    }

    pub fn kvm_set_xsave(&mut self, xsave: &[u32; 1024]) -> Result<()> {
        let buf = transmute_ref!(xsave);
        unsafe { kvm_set_xsave(&self.fd, buf) }.context(error::GuestXsave)?;
        Ok(())
    }

    pub fn kvm_get_lapic(&self, lapic: &mut [u32; 256]) -> Result<()> {
        let buf = transmute_mut!(lapic);
        unsafe { kvm_get_lapic(&self.fd, buf) }.context(error::GuestLapic)?;
        Ok(())
    }

    pub fn kvm_set_lapic(&mut self, lapic: &[u32; 256]) -> Result<()> {
        let buf = transmute_ref!(lapic);
        unsafe { kvm_set_lapic(&self.fd, buf) }.context(error::GuestLapic)?;
        Ok(())
    }

    /// Resets the vCPU to its x86 power-on state (Intel SDM Vol. 3A, Table
    /// 10-1), restoring MSRs from the VM's initial MSR snapshot. Confidential
    /// vCPUs do not expose their state to userspace and are left untouched.
    pub fn kvm_vcpu_reset(&self, is_bsp: bool) -> Result<()> {
        let Some(msrs) = self.vm.power_on_msrs() else {
            return Ok(());
        };
        // Leave nested virtualization first, so that the rest applies to L1.
        if let Some(format) = self.vm.nested_format() {
            let nested = power_on_nested_state(format);
            unsafe { kvm_set_nested_state(&self.fd, &nested) }.context(error::NestedState)?;
        }
        let sregs = power_on_sregs(is_bsp);
        unsafe { kvm_set_sregs(&self.fd, &sregs) }.context(error::VcpuReg)?;
        let regs = KvmRegs {
            rdx: self.arch.cpuid_1_eax as u64,
            rip: 0xfff0,
            rflags: Rflags::RESERVED_1.bits() as u64,
            ..Default::default()
        };
        unsafe { kvm_set_regs(&self.fd, &regs) }.context(error::VcpuReg)?;
        let xsave = power_on_xsave();
        unsafe { kvm_set_xsave(&self.fd, &xsave) }.context(error::GuestXsave)?;
        if self.vm.has_xcrs() {
            let mut xcrs = KvmXcrs {
                nr_xcrs: 1,
                ..Default::default()
            };
            xcrs.xcrs[0] = KvmXcr {
                xcr: 0,
                reserved: 0,
                value: 1,
            };
            unsafe { kvm_set_xcrs(&self.fd, &xcrs) }.context(error::GuestXsave)?;
        }
        // KVM interprets the LAPIC state by the APIC base in sregs.
        let lapic = power_on_lapic(self.arch.apic_id, is_bsp);
        unsafe { kvm_set_lapic(&self.fd, &lapic) }.context(error::GuestLapic)?;
        self.set_kvm_msrs(&msrs, |&entry| entry)?;
        let events = KvmVcpuEvents {
            flags: KvmVcpuEventValid::NMI_PENDING
                | KvmVcpuEventValid::SIPI_VECTOR
                | KvmVcpuEventValid::SHADOW
                | KvmVcpuEventValid::SMM,
            ..Default::default()
        };
        unsafe { kvm_set_vcpu_events(&self.fd, &events) }.context(error::VcpuEvents)?;
        let debugregs = KvmDebugregs {
            dr6: 0xffff_0ff0,
            dr7: 0x0000_0400,
            ..Default::default()
        };
        unsafe { kvm_set_debugregs(&self.fd, &debugregs) }.context(error::DebugRegs)?;
        let mp_state = if is_bsp {
            KvmMpState::RUNNABLE
        } else {
            KvmMpState::UNINITIALIZED
        };
        unsafe { kvm_set_mp_state(&self.fd, &mp_state) }.context(error::MpState)?;
        Ok(())
    }
}

fn power_on_nested_state(format: KvmNestedStateFormat) -> KvmNestedState {
    match format {
        KvmNestedStateFormat::VMX => KvmNestedState {
            flags: KvmNestedStateFlag::empty(),
            format,
            size: size_of::<KvmNestedState>() as u32,
            hdr: KvmNestedStateHdr {
                vmx: KvmVmxNestedStateHdr {
                    vmxon_pa: u64::MAX,
                    vmcs12_pa: u64::MAX,
                    ..Default::default()
                },
            },
        },
        KvmNestedStateFormat::SVM => KvmNestedState {
            flags: KvmNestedStateFlag::GIF_SET,
            format,
            size: size_of::<KvmNestedState>() as u32,
            hdr: KvmNestedStateHdr::default(),
        },
        _ => KvmNestedState {
            flags: KvmNestedStateFlag::empty(),
            format,
            size: size_of::<KvmNestedState>() as u32,
            hdr: KvmNestedStateHdr::default(),
        },
    }
}

fn power_on_sregs(is_bsp: bool) -> KvmSregs {
    let data_seg = KvmSegment {
        base: 0,
        limit: 0xffff,
        selector: 0,
        type_: 0x3,
        present: 1,
        s: 1,
        ..Default::default()
    };
    let mut apic_base = ApicBase(0);
    apic_base.set_base(0xfee00);
    apic_base.set_xapic(true);
    apic_base.set_bsp(is_bsp);
    KvmSregs {
        cs: KvmSegment {
            base: 0xffff_0000,
            limit: 0xffff,
            selector: 0xf000,
            type_: 0xb,
            present: 1,
            s: 1,
            ..Default::default()
        },
        ds: data_seg,
        es: data_seg,
        fs: data_seg,
        gs: data_seg,
        ss: data_seg,
        tr: KvmSegment {
            base: 0,
            limit: 0xffff,
            selector: 0,
            type_: 0xb,
            present: 1,
            s: 0,
            ..Default::default()
        },
        ldt: KvmSegment {
            base: 0,
            limit: 0xffff,
            selector: 0,
            type_: 0x2,
            present: 1,
            s: 0,
            ..Default::default()
        },
        gdt: KvmDtable {
            base: 0,
            limit: 0xffff,
            ..Default::default()
        },
        idt: KvmDtable {
            base: 0,
            limit: 0xffff,
            ..Default::default()
        },
        cr0: Cr0::ET | Cr0::NW | Cr0::CD,
        apic_base,
        ..Default::default()
    }
}

fn power_on_xsave() -> KvmXsave {
    // Intel SDM Vol. 1, Table 10-2 and Section 13.4.2: x87 FCW = 0x037f,
    // MXCSR = 0x1f80, and XSTATE_BV = 0 so KVM re-initializes all extended
    // state components.
    let mut xsave = KvmXsave { region: [0; 1024] };
    xsave.region[0] = 0x037f;
    xsave.region[24 >> 2] = 0x1f80;
    xsave
}

fn power_on_lapic(apic_id: u32, is_bsp: bool) -> KvmLapicState {
    // Intel SDM Vol. 3A, Table 11-1 and Section 11.4.7.1.
    const APIC_ID: usize = 0x020 >> 2;
    const APIC_DFR: usize = 0x0e0 >> 2;
    const APIC_SPIV: usize = 0x0f0 >> 2;
    const APIC_LVTT: usize = 0x320 >> 2;
    const APIC_LVTTHMR: usize = 0x330 >> 2;
    const APIC_LVTPC: usize = 0x340 >> 2;
    const APIC_LVT0: usize = 0x350 >> 2;
    const APIC_LVT1: usize = 0x360 >> 2;
    const APIC_LVTERR: usize = 0x370 >> 2;
    const APIC_LVT_MASKED: u32 = 1 << 16;

    let mut lapic = KvmLapicState { regs: [0; 0x100] };
    lapic.regs[APIC_ID] = apic_id << 24;
    lapic.regs[APIC_DFR] = u32::MAX;
    lapic.regs[APIC_SPIV] = 0xff;
    for lvt in [
        APIC_LVTT,
        APIC_LVTTHMR,
        APIC_LVTPC,
        APIC_LVT0,
        APIC_LVT1,
        APIC_LVTERR,
    ] {
        lapic.regs[lvt] = APIC_LVT_MASKED;
    }
    // KVM enables ExtINT on the BSP's LINT0 at reset
    // (`KVM_X86_QUIRK_LINT0_REENABLED`).
    if is_bsp {
        lapic.regs[APIC_LVT0] = (DeliveryMode::EXTINT.raw() as u32) << 8;
    }
    // KVM recomputes `APIC_LVR` (0x30) in `kvm_apic_set_state()`.
    lapic
}

#[cfg(test)]
#[path = "vcpu_amd64_test.rs"]
mod tests;
