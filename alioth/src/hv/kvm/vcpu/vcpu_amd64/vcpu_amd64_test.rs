// Copyright 2025 Google LLC
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

use std::mem::{size_of, size_of_val};
use std::os::unix::thread::JoinHandleExt;
use std::ptr::null_mut;
use std::thread;
use std::time::Duration;

use assert_matches::assert_matches;
use flume::RecvTimeoutError;
use libc::{MAP_ANONYMOUS, MAP_FAILED, MAP_SHARED, PROT_EXEC, PROT_READ, PROT_WRITE, mmap};

use crate::arch::cpuid::CpuidIn;
use crate::arch::intr::DeliveryMode;
use crate::arch::msr::{ApicBase, Efer, Msr};
use crate::arch::paging::Entry;
use crate::arch::reg::{Cr0, Cr4, Reg, SegAccess};
use crate::ffi;
use crate::hv::{
    DtReg, DtRegVal, Error, Hypervisor, Kvm, MemMapOption, SReg, SegReg, SegRegVal, Vcpu, Vm,
    VmEntry, VmExit,
};
use crate::sys::kvm::{
    KvmMpState, KvmNestedState, KvmNestedStateFlag, KvmNestedStateFormat, MSRS_PER_CALL,
    kvm_get_debugregs, kvm_get_mp_state, kvm_get_nested_state, kvm_get_sregs, kvm_get_vcpu_events,
    kvm_get_xcrs, kvm_set_debugregs, kvm_set_mp_state, kvm_set_nested_state, kvm_set_sregs,
    kvm_set_vcpu_events, kvm_set_xcrs,
};

use super::power_on_nested_state;

#[test]
#[cfg_attr(not(feature = "test-hv"), ignore)]
fn test_vcpu_regs() {
    use crate::hv::VmSpec;
    use crate::hv::kvm::KvmSpec;

    let kvm = Kvm::new(KvmSpec::default()).unwrap();
    let spec = VmSpec { coco: None };
    let vm = kvm.create_vm(&spec).unwrap();
    let mut vcpu = vm.create_vcpu(0, 0).unwrap();
    let regs = [
        (Reg::Rax, 0xa93f90f6ce9c8040),
        (Reg::Rbx, 0xacbfb3f1f6f9cc1a),
        (Reg::Rcx, 0x885e7996751c1cd5),
        (Reg::Rdx, 0xd0fdf85b84d0cc9c),
        (Reg::Rsi, 0x3cc1f46972391c30),
        (Reg::Rdi, 0xf67783992ddc4484),
        (Reg::Rsp, 0x6363e7f07d68f992),
        (Reg::Rbp, 0x7aeb086e85756325),
        (Reg::R8, 0x72a90eeeb1f73300),
        (Reg::R9, 0x8893ba64a98de27e),
        (Reg::R10, 0x543f074b89fd6531),
        (Reg::R11, 0x5330fea600e3a98c),
        (Reg::R12, 0x5d2af23af80a0c15),
        (Reg::R13, 0x596ad2d66a74a573),
        (Reg::R14, 0x9d97437934678adb),
        (Reg::R15, 0x7ae7b06eebe1f4fc),
        (Reg::Rip, 0xdb424549231b8d3e),
        (Reg::Rflags, 1 << 1),
    ];
    vcpu.set_regs(&regs).unwrap();
    for (reg, val) in regs {
        assert_eq!(vcpu.get_reg(reg).unwrap(), val);
    }

    let sregs = [
        (SReg::Cr0, (1 << 0) | (1 << 5) | (1 << 31)),
        (SReg::Cr2, 0xffff88ac93e00000),
        (SReg::Cr3, 0x1362d001),
        (SReg::Cr4, 1 << 5),
        (SReg::Cr8, 0x0),
    ];
    let seg_regs = [
        (
            SegReg::Cs,
            SegRegVal {
                selector: 0x10,
                base: 0,
                limit: 0xffff_ffff,
                access: SegAccess(0xa09b),
            },
        ),
        (
            SegReg::Ds,
            SegRegVal {
                selector: 0x18,
                base: 0,
                limit: 0xffff_ffff,
                access: SegAccess(0xc093),
            },
        ),
        (
            SegReg::Es,
            SegRegVal {
                selector: 0x18,
                base: 0,
                limit: 0xffff_ffff,
                access: SegAccess(0xc093),
            },
        ),
        (
            SegReg::Fs,
            SegRegVal {
                selector: 0x18,
                base: 0,
                limit: 0xffff_ffff,
                access: SegAccess(0xc093),
            },
        ),
        (
            SegReg::Gs,
            SegRegVal {
                selector: 0x18,
                base: 0,
                limit: 0xffff_ffff,
                access: SegAccess(0xc093),
            },
        ),
        (
            SegReg::Ss,
            SegRegVal {
                selector: 0x18,
                base: 0,
                limit: 0xffff_ffff,
                access: SegAccess(0xc093),
            },
        ),
        (
            SegReg::Tr,
            SegRegVal {
                selector: 0x20,
                base: 0,
                limit: 0xf_ffff,
                access: SegAccess(0x8b),
            },
        ),
        (
            SegReg::Ldtr,
            SegRegVal {
                selector: 0x28,
                base: 0,
                limit: 0xf_ffff,
                access: SegAccess(0x82),
            },
        ),
    ];

    let dt_regs = [
        (
            DtReg::Gdtr,
            DtRegVal {
                base: 0xfffffe2a4aeeb000,
                limit: 0x7f,
            },
        ),
        (
            DtReg::Idtr,
            DtRegVal {
                base: 0xfffffe0000000000,
                limit: 0xfff,
            },
        ),
    ];
    vcpu.set_sregs(&sregs, &seg_regs, &dt_regs).unwrap();

    for (sreg, val) in sregs {
        assert_eq!(vcpu.get_sreg(sreg).unwrap(), val);
    }
    for (seg_reg, val) in seg_regs {
        assert_eq!(vcpu.get_seg_reg(seg_reg).unwrap(), val)
    }
    for (dt_reg, val) in dt_regs {
        assert_eq!(vcpu.get_dt_reg(dt_reg).unwrap(), val)
    }
}

#[test]
#[cfg_attr(not(feature = "test-hv"), ignore)]
fn test_kvmclock_ctrl_without_kvmclock() {
    use crate::hv::VmSpec;
    use crate::hv::kvm::KvmSpec;

    let kvm = Kvm::new(KvmSpec::default()).unwrap();
    let vm = kvm.create_vm(&VmSpec { coco: None }).unwrap();
    let mut vcpu = vm.create_vcpu(0, 0).unwrap();

    // A guest that has not enabled kvmclock has nothing to be told.
    assert_matches!(vcpu.kvmclock_ctrl(), Ok(()));
}

#[test]
#[cfg_attr(not(feature = "test-hv"), ignore)]
fn test_kick_before_kvm_run() {
    use crate::hv::VmSpec;
    use crate::hv::kvm::KvmSpec;

    let kvm = Kvm::new(KvmSpec::default()).unwrap();
    let vm = kvm.create_vm(&VmSpec { coco: None }).unwrap();

    let prot = PROT_WRITE | PROT_EXEC | PROT_READ;
    let flag = MAP_ANONYMOUS | MAP_SHARED;
    let user_mem = ffi!(
        unsafe { mmap(null_mut(), 0x1000, prot, flag, -1, 0) },
        MAP_FAILED
    )
    .unwrap();
    let mmap_option = MemMapOption {
        read: true,
        write: true,
        exec: true,
        ..Default::default()
    };
    vm.map(0, 0x1000, user_mem as usize, mmap_option).unwrap();
    // out 0x80, al
    unsafe { (user_mem as *mut [u8; 2]).write([0xe6, 0x80]) };

    let mut vcpu = vm.create_vcpu(0, 0).unwrap();
    let cs = SegRegVal {
        selector: 0,
        base: 0,
        limit: 0xffff,
        access: SegAccess(0x9b),
    };
    vcpu.set_sregs(&[], &[(SegReg::Cs, cs)], &[]).unwrap();
    vcpu.set_regs(&[(Reg::Rip, 0), (Reg::Rflags, 0x2)]).unwrap();

    // A kick before KVM_RUN makes KVM_RUN return immediately.
    assert_eq!(
        unsafe { libc::pthread_kill(libc::pthread_self(), libc::SIGRTMIN()) },
        0
    );
    assert_matches!(vcpu.run(VmEntry::None), Ok(VmExit::Interrupted));

    // The next KVM_RUN runs the guest.
    assert_matches!(
        vcpu.run(VmEntry::None),
        Ok(VmExit::Io {
            port: 0x80,
            size: 1,
            ..
        })
    );
}

#[test]
fn test_power_on_nested_state() {
    let vmx = power_on_nested_state(KvmNestedStateFormat::VMX);
    assert_eq!(vmx.flags, KvmNestedStateFlag::empty());
    assert_eq!(vmx.format, KvmNestedStateFormat::VMX);
    assert_eq!(vmx.size as usize, size_of::<KvmNestedState>());
    assert_eq!(unsafe { vmx.hdr.vmx.vmxon_pa }, u64::MAX);
    assert_eq!(unsafe { vmx.hdr.vmx.vmcs12_pa }, u64::MAX);
    assert_eq!(unsafe { vmx.hdr.vmx.smm_flags }, 0);
    assert_eq!(unsafe { vmx.hdr.vmx.flags }, 0);

    let svm = power_on_nested_state(KvmNestedStateFormat::SVM);
    assert_eq!(svm.flags, KvmNestedStateFlag::GIF_SET);
    assert_eq!(svm.format, KvmNestedStateFormat::SVM);
    assert_eq!(svm.size as usize, size_of::<KvmNestedState>());
    assert_eq!(unsafe { svm.hdr.svm.vmcb_pa }, 0);
    assert_eq!(unsafe { svm.hdr.svm.gpat }, 0);
}

#[test]
#[cfg_attr(not(feature = "test-hv"), ignore)]
fn test_reset_halted_vcpu() {
    use crate::hv::VmSpec;
    use crate::hv::kvm::KvmSpec;

    let kvm = Kvm::new(KvmSpec::default()).unwrap();
    let vm = kvm.create_vm(&VmSpec { coco: None }).unwrap();

    let prot = PROT_WRITE | PROT_EXEC | PROT_READ;
    let flag = MAP_ANONYMOUS | MAP_SHARED;
    let user_mem = ffi!(
        unsafe { mmap(null_mut(), 0x1000, prot, flag, -1, 0) },
        MAP_FAILED
    )
    .unwrap();
    let mmap_option = MemMapOption {
        read: true,
        write: true,
        exec: true,
        ..Default::default()
    };
    vm.map(0, 0x1000, user_mem as usize, mmap_option).unwrap();
    // hlt; out 0x80, al
    unsafe { (user_mem as *mut [u8; 3]).write([0xf4, 0xe6, 0x80]) };

    let (done_tx, done_rx) = flume::bounded(1);
    let vcpu_thread = thread::spawn(move || {
        let mut vcpu = vm.create_vcpu(0, 0).unwrap();
        let cs = SegRegVal {
            selector: 0,
            base: 0,
            limit: 0xffff,
            access: SegAccess(0x9b),
        };
        vcpu.set_sregs(&[], &[(SegReg::Cs, cs)], &[]).unwrap();
        // With interrupts disabled, only a kick stops `hlt`.
        vcpu.set_regs(&[(Reg::Rip, 0), (Reg::Rflags, 0x2)]).unwrap();

        assert_matches!(vcpu.run(VmEntry::None), Ok(VmExit::Interrupted));
        let mp_state = unsafe { kvm_get_mp_state(&vcpu.fd) }.unwrap();
        assert_eq!(mp_state, KvmMpState::HALTED);

        vcpu.reset(true).unwrap();
        let mp_state = unsafe { kvm_get_mp_state(&vcpu.fd) }.unwrap();
        assert_eq!(mp_state, KvmMpState::RUNNABLE);
        assert_eq!(vcpu.get_reg(Reg::Rip).unwrap(), 0xfff0);

        vcpu.set_sregs(&[], &[(SegReg::Cs, cs)], &[]).unwrap();
        vcpu.set_regs(&[(Reg::Rip, 1)]).unwrap();
        let exit = vcpu.run(VmEntry::None);
        done_tx.send(()).unwrap();
        assert_matches!(
            exit,
            Ok(VmExit::Io {
                port: 0x80,
                size: 1,
                ..
            })
        );
    });

    let kick = || unsafe { libc::pthread_kill(vcpu_thread.as_pthread_t() as _, libc::SIGRTMIN()) };
    thread::sleep(Duration::from_millis(100));
    assert_eq!(kick(), 0);
    // A vCPU that stays halted after the reset exits only on another kick.
    if done_rx.recv_timeout(Duration::from_secs(1)) == Err(RecvTimeoutError::Timeout) {
        assert_eq!(kick(), 0);
    }
    vcpu_thread.join().unwrap();
}

#[test]
#[cfg_attr(not(feature = "test-hv"), ignore)]
fn test_reset_power_on_state() {
    use crate::hv::VmSpec;
    use crate::hv::kvm::KvmSpec;

    // Intel SDM Vol.3, Table 11-1.
    const APIC_ID: usize = 0x020 >> 2;
    const LVT_TIMER: usize = 0x320 >> 2;
    const LVT_LINT0: usize = 0x350 >> 2;
    const INITIAL_COUNT: usize = 0x380 >> 2;
    // Intel SDM Vol.1, Table 10-2 and Section 13.4.2.
    const MXCSR: usize = 24 >> 2;
    const XSTATE_BV: usize = 512 >> 2;

    let kvm = Kvm::new(KvmSpec::default()).unwrap();
    let vm = kvm.create_vm(&VmSpec { coco: None }).unwrap();
    let cpuids = kvm.get_supported_cpuids(None).unwrap();
    let cpuid_1_eax = cpuids[&CpuidIn {
        func: 1,
        index: None,
    }]
        .eax;

    let mut bsp = vm.create_vcpu(0, 0).unwrap();
    let mut ap = vm.create_vcpu(1, 7).unwrap();
    bsp.set_cpuids(cpuids.clone()).unwrap();
    ap.set_cpuids(cpuids).unwrap();

    // Dirty the BSP.
    let mut bsp_lapic = [0; 256];
    bsp.get_lapic(&mut bsp_lapic).unwrap();
    bsp_lapic[LVT_LINT0] = 1 << 16;
    bsp.set_lapic(&bsp_lapic).unwrap();
    bsp.set_msrs(&[(Msr::LSTAR, 0xffff_ffff_8100_0000)])
        .unwrap();
    unsafe { kvm_set_mp_state(&bsp.fd, &KvmMpState::HALTED) }.unwrap();

    // Dirty every state group on the AP.
    if vm.vm.nested_format() == Some(KvmNestedStateFormat::SVM) {
        let cleared_gif = KvmNestedState {
            flags: KvmNestedStateFlag::empty(),
            format: KvmNestedStateFormat::SVM,
            size: size_of::<KvmNestedState>() as u32,
            ..Default::default()
        };
        unsafe { kvm_set_nested_state(&ap.fd, &cleared_gif) }.unwrap();
    }
    ap.set_regs(&[
        (Reg::Rip, 0x1000),
        (Reg::Rax, 0x1234),
        (Reg::Rdx, 0xdead_beef),
    ])
    .unwrap();
    let mut sregs = unsafe { kvm_get_sregs(&ap.fd) }.unwrap();
    sregs.cr0 = Cr0::PE | Cr0::ET;
    sregs.apic_base.set_bsp(true);
    unsafe { kvm_set_sregs(&ap.fd, &sregs) }.unwrap();
    let mut xsave = [0; 1024];
    ap.get_xsave(&mut xsave).unwrap();
    xsave[MXCSR] = 0x1f00;
    // MXCSR belongs to the SSE state.
    xsave[XSTATE_BV] |= 1 << 1;
    ap.set_xsave(&xsave).unwrap();
    let mut xcrs = unsafe { kvm_get_xcrs(&ap.fd) }.unwrap();
    xcrs.xcrs[0].value = 0x3;
    unsafe { kvm_set_xcrs(&ap.fd, &xcrs) }.unwrap();
    let mut lapic = [0; 256];
    ap.get_lapic(&mut lapic).unwrap();
    lapic[APIC_ID] = 0;
    lapic[LVT_TIMER] = 0xec;
    lapic[LVT_LINT0] = (DeliveryMode::EXTINT.raw() as u32) << 8;
    lapic[INITIAL_COUNT] = 0x1000_0000;
    ap.set_lapic(&lapic).unwrap();
    ap.set_msrs(&[(Msr::LSTAR, 0xffff_ffff_8100_0000)]).unwrap();
    let mut events = unsafe { kvm_get_vcpu_events(&ap.fd) }.unwrap();
    events.nmi.masked = 1;
    unsafe { kvm_set_vcpu_events(&ap.fd, &events) }.unwrap();
    let mut debugregs = unsafe { kvm_get_debugregs(&ap.fd) }.unwrap();
    debugregs.dr7 |= 1 << 0;
    unsafe { kvm_set_debugregs(&ap.fd, &debugregs) }.unwrap();
    unsafe { kvm_set_mp_state(&ap.fd, &KvmMpState::RUNNABLE) }.unwrap();

    bsp.reset(true).unwrap();
    ap.reset(false).unwrap();

    // BSP state.
    let bsp_sregs = unsafe { kvm_get_sregs(&bsp.fd) }.unwrap();
    let mut expected_bsp_apic = ApicBase(0);
    expected_bsp_apic.set_base(0xfee00);
    expected_bsp_apic.set_xapic(true);
    expected_bsp_apic.set_bsp(true);
    assert_eq!(bsp_sregs.apic_base, expected_bsp_apic);
    bsp.get_lapic(&mut bsp_lapic).unwrap();
    assert_eq!(bsp_lapic[APIC_ID], 0);
    assert_eq!(
        bsp_lapic[LVT_LINT0],
        (DeliveryMode::EXTINT.raw() as u32) << 8
    );
    let bsp_msrs = bsp.get_kvm_msrs(&[Msr::LSTAR.raw()]).unwrap();
    assert_eq!(bsp_msrs[0].data, 0);
    assert_eq!(
        unsafe { kvm_get_mp_state(&bsp.fd) }.unwrap(),
        KvmMpState::RUNNABLE
    );

    // AP state.
    if vm.vm.nested_format() == Some(KvmNestedStateFormat::SVM) {
        let mut nested = KvmNestedState {
            size: size_of::<KvmNestedState>() as u32,
            ..Default::default()
        };
        unsafe { kvm_get_nested_state(&ap.fd, &mut nested) }.unwrap();
        assert_eq!(nested.flags, KvmNestedStateFlag::GIF_SET);
    }
    assert_eq!(ap.get_reg(Reg::Rip).unwrap(), 0xfff0);
    assert_eq!(ap.get_reg(Reg::Rax).unwrap(), 0);
    assert_eq!(ap.get_reg(Reg::Rdx).unwrap(), cpuid_1_eax as u64);
    let cr0 = Cr0::CD | Cr0::NW | Cr0::ET;
    assert_eq!(ap.get_sreg(SReg::Cr0).unwrap(), cr0.bits());
    let ap_sregs = unsafe { kvm_get_sregs(&ap.fd) }.unwrap();
    let mut expected_ap_apic = expected_bsp_apic;
    expected_ap_apic.set_bsp(false);
    assert_eq!(ap_sregs.apic_base, expected_ap_apic);
    ap.get_xsave(&mut xsave).unwrap();
    assert_eq!(xsave[MXCSR], 0x1f80);
    let xcrs = unsafe { kvm_get_xcrs(&ap.fd) }.unwrap();
    assert_eq!(xcrs.xcrs[0].value, 0x1);
    ap.get_lapic(&mut lapic).unwrap();
    assert_eq!(lapic[APIC_ID], 7 << 24);
    assert_eq!(lapic[LVT_TIMER], 1 << 16);
    assert_eq!(lapic[LVT_LINT0], 1 << 16);
    assert_eq!(lapic[INITIAL_COUNT], 0);
    let msrs = ap.get_kvm_msrs(&[Msr::LSTAR.raw()]).unwrap();
    assert_eq!(msrs[0].data, 0);
    let events = unsafe { kvm_get_vcpu_events(&ap.fd) }.unwrap();
    assert_eq!(events.nmi.masked, 0);
    let debugregs = unsafe { kvm_get_debugregs(&ap.fd) }.unwrap();
    assert_eq!(debugregs.dr7, 0x400);
    let mp_state = unsafe { kvm_get_mp_state(&ap.fd) }.unwrap();
    assert_eq!(mp_state, KvmMpState::UNINITIALIZED);
}

#[test]
#[cfg_attr(not(feature = "test-hv"), ignore)]
fn test_kvm_run() {
    use crate::hv::VmSpec;
    use crate::hv::kvm::KvmSpec;

    let kvm = Kvm::new(KvmSpec::default()).unwrap();
    let spec = VmSpec { coco: None };
    let vm = kvm.create_vm(&spec).unwrap();

    let prot = PROT_WRITE | PROT_EXEC | PROT_READ;
    let flag = MAP_ANONYMOUS | MAP_SHARED;
    let user_mem = ffi!(
        unsafe { mmap(null_mut(), 0x5000, prot, flag, -1, 0,) },
        MAP_FAILED
    )
    .unwrap();
    let mmap_option = MemMapOption {
        read: true,
        write: true,
        exec: true,
        ..Default::default()
    };
    vm.map(0, 0x5000, user_mem as usize, mmap_option).unwrap();

    // layout
    // 0x1000 - 0x1f00 code
    // 0x1f00 - 0x2000 GDT
    // 0x2000 - 0x3000 PML4
    // 0x3000 - 0x4000 PDPT
    // 0x4000 - 0x5000 PD

    #[rustfmt::skip]
    const CODE: [u8; 29] = [
        // mov dx, 0x3f8
        0x66, 0xba, 0xf8, 0x03,
        // in al, dx
        0xec,
        // add eax, 0x1
        0x83, 0xc0, 0x01,
        // out dx, al
        0xee,
        // mov rax, [0x5000]
        0x48, 0x8b, 0x04, 0x25, 0x00, 0x50, 0x00,
        0x00,
        // add rax, 0x11
        0x48, 0x83, 0xc0, 0x11,
        // mov [0x5004], rax
        0x48, 0x89, 0x04, 0x25, 0x04, 0x50, 0x00,
        0x00,
    ];
    unsafe { ((user_mem as usize + 0x1000) as *mut [u8; 29]).write(CODE) };

    let pml4e = (Entry::P | Entry::RW).bits() as u64 | 0x3000;
    unsafe { ((user_mem as usize + 0x2000) as *mut u64).write(pml4e) }
    let pdpte = (Entry::P | Entry::RW).bits() as u64 | 0x4000;
    unsafe { ((user_mem as usize + 0x3000) as *mut u64).write(pdpte) }
    let pde = (Entry::P | Entry::RW | Entry::PS).bits() as u64;
    unsafe { ((user_mem as usize + 0x4000) as *mut u64).write(pde) }

    let mut vcpu = vm.create_vcpu(0, 0).unwrap();
    let cs = SegRegVal {
        selector: 0x10,
        base: 0,
        limit: 0xffff_ffff,
        access: SegAccess(0xa09b),
    };
    let ds = SegRegVal {
        selector: 0x18,
        base: 0,
        limit: 0xffff_ffff,
        access: SegAccess(0xc093),
    };
    let tr = SegRegVal {
        selector: 0x20,
        base: 0,
        limit: 0,
        access: SegAccess(0x8b),
    };
    let ldtr = SegRegVal {
        selector: 0x28,
        base: 0,
        limit: 0,
        access: SegAccess(0x82),
    };
    let gdt = [
        0,
        0,
        cs.to_desc(),
        ds.to_desc(),
        tr.to_desc(),
        ldtr.to_desc(),
    ];
    assert!(size_of_val(&gdt) < 0x100);
    unsafe { ((user_mem as usize + 0x1f00) as *mut [u64; 6]).write(gdt) };
    let gdtr = DtRegVal {
        base: 0x1f00,
        limit: size_of_val(&gdt) as u16 - 1,
    };
    let idtr = DtRegVal { base: 0, limit: 0 };
    vcpu.set_sregs(
        &[
            (SReg::Cr0, (Cr0::NE | Cr0::PE | Cr0::PG).bits()),
            (SReg::Cr3, 0x2000),
            (SReg::Cr4, Cr4::PAE.bits()),
        ],
        &[
            (SegReg::Cs, cs),
            (SegReg::Ds, ds),
            (SegReg::Es, ds),
            (SegReg::Fs, ds),
            (SegReg::Gs, ds),
            (SegReg::Ss, ds),
            (SegReg::Tr, tr),
            (SegReg::Ldtr, ldtr),
        ],
        &[(DtReg::Gdtr, gdtr), (DtReg::Idtr, idtr)],
    )
    .unwrap();
    vcpu.set_msrs(&[(Msr::EFER, (Efer::LMA | Efer::LME).bits())])
        .unwrap();
    vcpu.set_regs(&[
        (Reg::Rip, 0x1000),
        (Reg::Rax, 0x2),
        (Reg::Rbx, 0x2),
        (Reg::Rdx, 0x3f8),
        (Reg::Rsi, 0x1000),
        (Reg::Rflags, 0x2),
    ])
    .unwrap();
    assert_matches!(
        vcpu.run(VmEntry::None),
        Ok(VmExit::Io {
            port: 0x3f8,
            write: None,
            size: 1
        })
    );
    assert_matches!(
        vcpu.run(VmEntry::Io { data: Some(0x10) }),
        Ok(VmExit::Io {
            port: 0x3f8,
            write: Some(0x11),
            size: 1
        })
    );
    assert_matches!(
        vcpu.run(VmEntry::None),
        Ok(VmExit::Mmio {
            addr: 0x5000,
            write: None,
            size: 8
        })
    );
    assert_matches!(
        vcpu.run(VmEntry::Mmio { data: 0x0000_ffff }),
        Ok(VmExit::Mmio {
            addr: 0x5004,
            write: Some(0x0001_0010),
            size: 8
        })
    );
}

#[test]
#[cfg_attr(not(feature = "test-hv"), ignore)]
fn test_kvm_set_msrs() {
    use crate::hv::VmSpec;
    use crate::hv::kvm::KvmSpec;

    let kvm = Kvm::new(KvmSpec::default()).unwrap();
    let vm = kvm.create_vm(&VmSpec { coco: None }).unwrap();
    let mut vcpu = vm.create_vcpu(0, 0).unwrap();

    let mut msrs = vec![(Msr::LSTAR, 0x1000); MSRS_PER_CALL + 1];
    vcpu.set_msrs(&msrs).unwrap();

    // Failing in the first chunk and in a later chunk both report the MSR.
    assert_matches!(
        vcpu.set_msrs(&[(Msr::LSTAR, 0x1000), (Msr::EFER, u64::MAX)]),
        Err(Error::SetMsr { index, .. }) if index == Msr::EFER.raw()
    );
    msrs.push((Msr::EFER, u64::MAX));
    assert_matches!(
        vcpu.set_msrs(&msrs),
        Err(Error::SetMsr { index, .. }) if index == Msr::EFER.raw()
    );
}

#[test]
#[cfg_attr(not(feature = "test-hv"), ignore)]
fn test_kvm_get_msrs() {
    use crate::hv::VmSpec;
    use crate::hv::kvm::KvmSpec;

    // KVM cannot read the x2APIC MSRs of a vCPU whose APIC is in xAPIC mode.
    const X2APIC_ID: u32 = 0x802;

    let kvm = Kvm::new(KvmSpec::default()).unwrap();
    let vm = kvm.create_vm(&VmSpec { coco: None }).unwrap();
    let mut vcpu = vm.create_vcpu(0, 0).unwrap();

    let lstar = (Msr::LSTAR.raw(), 0x1000);
    let cstar = (Msr::CSTAR.raw(), 0x2000);
    vcpu.set_msrs(&[(Msr::LSTAR, lstar.1), (Msr::CSTAR, cstar.1)])
        .unwrap();
    let read = |indices: &[u32]| {
        let msrs = vcpu.get_kvm_msrs(indices)?;
        let msrs: Vec<_> = msrs.iter().map(|msr| (msr.index, msr.data)).collect();
        Ok::<_, Error>(msrs)
    };

    assert_matches!(read(&[]), Ok(msrs) if msrs.is_empty());
    assert_matches!(read(&[cstar.0, lstar.0]), Ok(msrs) if msrs == [cstar, lstar]);

    let mut indices = vec![lstar.0; MSRS_PER_CALL + 1];
    indices[MSRS_PER_CALL] = cstar.0;
    let mut expected = vec![lstar; MSRS_PER_CALL + 1];
    expected[MSRS_PER_CALL] = cstar;
    assert_matches!(read(&indices), Ok(msrs) if msrs == expected);

    // Failing at the start of a chunk, in the first chunk and in a later
    // chunk all report the MSR.
    assert_matches!(
        read(&[X2APIC_ID, lstar.0]),
        Err(Error::GetMsr {
            index: X2APIC_ID,
            ..
        })
    );
    assert_matches!(
        read(&[lstar.0, X2APIC_ID, cstar.0]),
        Err(Error::GetMsr {
            index: X2APIC_ID,
            ..
        })
    );
    indices.push(X2APIC_ID);
    assert_matches!(
        read(&indices),
        Err(Error::GetMsr {
            index: X2APIC_ID,
            ..
        })
    );
}
