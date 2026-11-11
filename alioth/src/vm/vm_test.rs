// Copyright 2026 Google LLC
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

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use std::{fs, thread};

use assert_matches::assert_matches;
use tempfile::TempDir;

use crate::arch::x86_64::msr::MiscEnable;
use crate::board::{BoardSpec, CpuSpec, CpuTopology};
use crate::cpu::State;
use crate::device::console::{ConsoleSpec, LogConsoleSpec};
use crate::hv::{Kvm, KvmSpec};
use crate::loader::PayloadSpec;
use crate::mem::MemSpec;
use crate::vm::config::VmSpec;
use crate::vm::{Error, Machine};

/// Real-mode code at the reset vector that powers off the VM.
const POWER_OFF: [u8; 9] = [
    0xba, 0x00, 0x06, // mov dx, 0x600 (PORT_ACPI_SLEEP_CONTROL)
    0xb0, 0x34, // mov al, 0x34 (SLP_EN, SLP_TYPx = 5)
    0xee, // out dx, al
    0xf4, // hlt
    0xeb, 0xfd, // jmp -3
];

/// Real-mode code at the reset vector that halts forever.
pub(crate) const HALT: [u8; 3] = [
    0xf4, // hlt
    0xeb, 0xfd, // jmp -3
];

pub(crate) fn fixture_spec(firmware: &Path) -> VmSpec {
    VmSpec {
        board: BoardSpec {
            cpu: CpuSpec {
                count: 2,
                topology: CpuTopology {
                    smt: false,
                    cores: 2,
                    sockets: 1,
                    thread_contiguous: false,
                },
            },
            mem: MemSpec {
                size: 16 << 20,
                ..Default::default()
            },
            ..Default::default()
        },
        payload: PayloadSpec {
            firmware: Some(firmware.into()),
            ..Default::default()
        },
        console: ConsoleSpec::Log(LogConsoleSpec {
            path: Path::new("/dev/null").into(),
            append: false,
        }),
        ..Default::default()
    }
}

fn create_vm(kvm: &Kvm, firmware: &Path) -> Machine<Kvm> {
    Machine::create(kvm, fixture_spec(firmware)).unwrap()
}

/// Writes a 4 KiB firmware that runs `code` at the reset vector.
pub(crate) fn write_firmware(temp_dir: &TempDir, code: &[u8]) -> PathBuf {
    let mut firmware = vec![0xf4; 4096];
    firmware[4096 - 16..][..code.len()].copy_from_slice(code);
    let path = temp_dir.path().join("firmware.bin");
    fs::write(&path, firmware).unwrap();
    path
}

fn fixture_vm(kvm: &Kvm, temp_dir: &TempDir, code: &[u8]) -> Machine<Kvm> {
    create_vm(kvm, &write_firmware(temp_dir, code))
}

fn wait_state(vm: &Machine<Kvm>, state: State) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while vm.state() != state {
        assert!(Instant::now() < deadline, "stuck in {:?}", vm.state());
        thread::sleep(Duration::from_millis(10));
    }
}

#[test]
#[cfg_attr(not(feature = "test-hv"), ignore)]
fn test_guest_power_off() {
    let kvm = Kvm::new(KvmSpec::default()).unwrap();
    let temp_dir = TempDir::new().unwrap();
    let vm = fixture_vm(&kvm, &temp_dir, &POWER_OFF);
    assert_eq!(vm.state(), State::Stopped);

    // A VM that the guest powered off boots again.
    for _ in 0..2 {
        vm.boot().unwrap();
        vm.wait_stopped();
        assert_eq!(vm.state(), State::Stopped);
    }

    vm.terminate().unwrap();
    assert_eq!(vm.state(), State::Terminated);
}

#[test]
#[cfg_attr(not(feature = "test-hv"), ignore)]
fn test_host_requests() {
    let kvm = Kvm::new(KvmSpec::default()).unwrap();
    let temp_dir = TempDir::new().unwrap();
    let vm = fixture_vm(&kvm, &temp_dir, &HALT);

    assert_matches!(
        vm.shutdown(),
        Err(Error::UnexpectedState {
            state: State::Stopped,
            ..
        })
    );
    vm.boot().unwrap();
    assert_matches!(
        vm.boot(),
        Err(Error::UnexpectedState {
            state: State::Running,
            ..
        })
    );

    vm.reboot().unwrap();
    wait_state(&vm, State::Running);

    vm.pause().unwrap();
    vm.shutdown().unwrap();
    vm.wait_stopped();
    assert_eq!(vm.state(), State::Stopped);

    vm.terminate().unwrap();
}

#[test]
#[cfg_attr(not(feature = "test-hv"), ignore)]
fn test_boot_after_host_shutdown() {
    let kvm = Kvm::new(KvmSpec::default()).unwrap();
    let temp_dir = TempDir::new().unwrap();
    let vm = fixture_vm(&kvm, &temp_dir, &HALT);

    vm.boot().unwrap();
    // Lets the BSP halt with interrupts disabled.
    thread::sleep(Duration::from_millis(100));
    vm.shutdown().unwrap();
    vm.wait_stopped();

    // The next boot runs the new firmware from the reset vector.
    write_firmware(&temp_dir, &POWER_OFF);
    vm.boot().unwrap();
    wait_state(&vm, State::Stopped);

    vm.terminate().unwrap();
}

/// Real-mode code at the reset vector that stores the low half of
/// IA32_MISC_ENABLE at `MISC_ENABLE_COPY`.
const READ_MISC_ENABLE: [u8; 15] = [
    0x66, 0xb9, 0xa0, 0x01, 0x00, 0x00, // mov ecx, 0x1a0 (IA32_MISC_ENABLE)
    0x0f, 0x32, // rdmsr
    0x66, 0xa3, 0x00, 0x10, // mov [0x1000], eax
    0xf4, // hlt
    0xeb, 0xfd, // jmp -3
];

const MISC_ENABLE_COPY: u64 = 0x1000;

/// Waits until the guest has stored IA32_MISC_ENABLE, and checks that fast
/// strings are enabled. It clears the copy, so that the next boot has to
/// store it again.
fn assert_fast_strings(vm: &Machine<Kvm>) {
    let deadline = Instant::now() + Duration::from_secs(10);
    let misc_enable = loop {
        let ram = vm.ctx.board.memory.ram_bus();
        // The guest memory exists once the BSP has started to boot.
        let value = ram.read_t::<u32>(MISC_ENABLE_COPY).unwrap_or(0);
        if value != 0 {
            ram.write_t(MISC_ENABLE_COPY, &0u32).unwrap();
            break MiscEnable::from_bits_retain(value as u64);
        }
        assert!(Instant::now() < deadline, "IA32_MISC_ENABLE is not stored");
        thread::sleep(Duration::from_millis(1));
    };
    assert!(
        misc_enable.contains(MiscEnable::FAST_STRINGS),
        "{misc_enable:?}"
    );
}

#[test]
#[cfg_attr(not(feature = "test-hv"), ignore)]
fn test_fast_strings_on_every_boot() {
    let kvm = Kvm::new(KvmSpec::default()).unwrap();
    let temp_dir = TempDir::new().unwrap();
    let vm = fixture_vm(&kvm, &temp_dir, &READ_MISC_ENABLE);

    vm.boot().unwrap();
    assert_fast_strings(&vm);

    // A reset writes back the MSRs that the vCPUs started with, which have
    // fast strings disabled. So they are enabled again for the next boot,
    // after a reboot ...
    vm.reboot().unwrap();
    wait_state(&vm, State::Running);
    assert_fast_strings(&vm);

    // ... and after a shutdown.
    vm.shutdown().unwrap();
    vm.wait_stopped();
    vm.boot().unwrap();
    assert_fast_strings(&vm);

    vm.shutdown().unwrap();
    vm.wait_stopped();
    vm.terminate().unwrap();
}

#[test]
#[cfg_attr(not(feature = "test-hv"), ignore)]
fn test_reset_while_booting() {
    let kvm = Kvm::new(KvmSpec::default()).unwrap();
    let temp_dir = TempDir::new().unwrap();
    let vm = fixture_vm(&kvm, &temp_dir, &HALT);

    // Requests that arrive before all vCPUs boot.
    for _ in 0..20 {
        vm.boot().unwrap();
        vm.reboot().unwrap();
        vm.shutdown().unwrap();
        vm.wait_stopped();
        assert_eq!(vm.state(), State::Stopped);
    }

    vm.terminate().unwrap();
}

#[test]
#[cfg_attr(not(feature = "test-hv"), ignore)]
fn test_drop_running_vm() {
    let kvm = Kvm::new(KvmSpec::default()).unwrap();
    let temp_dir = TempDir::new().unwrap();
    let vm = fixture_vm(&kvm, &temp_dir, &HALT);
    vm.boot().unwrap();
    wait_state(&vm, State::Running);

    // Powers off the VM and ends the vCPU threads.
    drop(vm);
}

#[test]
#[cfg_attr(not(feature = "test-hv"), ignore)]
fn test_vcpu_failure() {
    let kvm = Kvm::new(KvmSpec::default()).unwrap();
    let temp_dir = TempDir::new().unwrap();
    let vm = create_vm(&kvm, &temp_dir.path().join("missing.bin"));

    vm.boot().unwrap();
    vm.wait_stopped();
    assert_eq!(vm.state(), State::Terminated);
    assert_matches!(vm.terminate(), Err(Error::VcpuExit { index: 0, .. }));
}
