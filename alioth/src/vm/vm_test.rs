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
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::{Duration, Instant};
use std::{fs, hint, thread};

use assert_matches::assert_matches;
use parking_lot::Mutex;
use tempfile::TempDir;

use crate::arch::x86_64::msr::MiscEnable;
use crate::board::{BoardSpec, CpuSpec, CpuTopology};
use crate::cpu::State;
use crate::device::console::{ConsoleSpec, LogConsoleSpec};
use crate::device::{self, Pause};
use crate::hv::{Kvm, KvmSpec};
use crate::loader::PayloadSpec;
use crate::mem::MemSpec;
use crate::pci::config::PciConfig;
use crate::pci::pvpanic::PvPanic;
use crate::pci::{self, Pci};
use crate::vm::config::VmSpec;
use crate::vm::{self, Error, Machine};

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

/// Real-mode code at the reset vector that counts in memory forever.
const COUNT: [u8; 7] = [
    0x66, 0xff, 0x06, 0x00, 0x10, // inc dword ptr [0x1000]
    0xeb, 0xf9, // jmp -7
];

/// The address of the counter of `COUNT`.
const COUNTER: u64 = 0x1000;

pub(crate) fn fixture_spec(firmware: &Path) -> VmSpec {
    VmSpec {
        board: BoardSpec {
            cpu: CpuSpec {
                // More than two vCPUs, so that several of them wait for
                // the others at a barrier. With two vCPUs, the tests pass
                // even if a barrier lets a vCPU go on too early.
                count: 4,
                topology: CpuTopology {
                    smt: false,
                    cores: 4,
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

/// Returns the counter of `COUNT`, 0 if the VM has no memory yet.
fn read_counter(vm: &Machine<Kvm>) -> u32 {
    let ram = vm.ctx.board.memory.ram_bus();
    ram.read_t(COUNTER).unwrap_or(0)
}

/// Waits for the guest to count.
fn wait_counting(vm: &Machine<Kvm>) {
    let count = read_counter(vm);
    let deadline = Instant::now() + Duration::from_secs(10);
    while read_counter(vm) == count {
        assert!(Instant::now() < deadline, "stuck at {count}");
        thread::yield_now();
    }
}

/// Waits for `duration`, at a finer resolution than `thread::sleep()`.
fn spin(duration: Duration) {
    let start = Instant::now();
    while start.elapsed() < duration {
        hint::spin_loop();
    }
}

/// Asserts that the guest does not count.
fn assert_not_counting(vm: &Machine<Kvm>) {
    let count = read_counter(vm);
    thread::sleep(Duration::from_millis(1));
    assert_eq!(read_counter(vm), count);
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

#[test]
#[cfg_attr(not(feature = "test-hv"), ignore)]
fn test_pause_resume() {
    let kvm = Kvm::new(KvmSpec::default()).unwrap();
    let temp_dir = TempDir::new().unwrap();
    let vm = fixture_vm(&kvm, &temp_dir, &COUNT);

    assert_matches!(
        vm.pause(),
        Err(Error::UnexpectedState {
            state: State::Stopped,
            ..
        })
    );
    vm.boot().unwrap();
    assert_matches!(
        vm.resume(),
        Err(Error::UnexpectedState {
            state: State::Running,
            ..
        })
    );

    for _ in 0..100 {
        wait_counting(&vm);
        vm.pause().unwrap();
        assert_eq!(vm.state(), State::Paused);
        // No vCPU runs the guest once `pause()` returns.
        assert_not_counting(&vm);
        assert_matches!(
            vm.pause(),
            Err(Error::UnexpectedState {
                state: State::Paused,
                ..
            })
        );
        vm.resume().unwrap();
    }

    // A vCPU that is about to park does not pause the VM again after the
    // VM resumes.
    for _ in 0..1000 {
        vm.pause().unwrap();
        vm.resume().unwrap();
    }
    wait_counting(&vm);
    assert_eq!(vm.state(), State::Running);

    // A paused VM reboots, and runs afterwards.
    vm.pause().unwrap();
    vm.reboot().unwrap();
    wait_state(&vm, State::Running);
    wait_counting(&vm);

    vm.shutdown().unwrap();
    vm.wait_stopped();
    vm.terminate().unwrap();
}

#[test]
#[cfg_attr(not(feature = "test-hv"), ignore)]
fn test_pause_while_booting() {
    let kvm = Kvm::new(KvmSpec::default()).unwrap();
    let temp_dir = TempDir::new().unwrap();
    let vm = fixture_vm(&kvm, &temp_dir, &COUNT);

    for _ in 0..20 {
        // The pause returns after all vCPUs finish booting, which creates
        // the guest memory.
        vm.boot().unwrap();
        vm.pause().unwrap();
        let ram = vm.ctx.board.memory.ram_bus();
        assert_matches!(ram.read_t::<u32>(COUNTER), Ok(_));
        assert_not_counting(&vm);
        vm.resume().unwrap();
        wait_counting(&vm);

        // The same for a pause that follows a reboot.
        vm.reboot().unwrap();
        while let Err(e) = vm.pause() {
            assert_matches!(
                e,
                Error::UnexpectedState {
                    state: State::RebootPending,
                    ..
                }
            );
            thread::yield_now();
        }
        let ram = vm.ctx.board.memory.ram_bus();
        assert_matches!(ram.read_t::<u32>(COUNTER), Ok(_));
        assert_not_counting(&vm);
        vm.resume().unwrap();
        wait_counting(&vm);

        vm.shutdown().unwrap();
        vm.wait_stopped();
    }

    vm.terminate().unwrap();
}

#[test]
#[cfg_attr(not(feature = "test-hv"), ignore)]
fn test_guest_power_off_while_pausing() {
    let kvm = Kvm::new(KvmSpec::default()).unwrap();
    let temp_dir = TempDir::new().unwrap();
    let vm = fixture_vm(&kvm, &temp_dir, &POWER_OFF);

    // An upper bound of the time that the guest takes to power off the VM.
    let durations = (0..3).map(|_| {
        let start = Instant::now();
        vm.boot().unwrap();
        vm.wait_stopped();
        start.elapsed()
    });
    let duration = durations.min().unwrap();

    // The pause arrives before, while, and after the guest powers off the VM.
    for i in 0..200 {
        vm.boot().unwrap();
        spin(duration * i / 200);
        match vm.pause() {
            // The guest powers off the VM once it resumes.
            Ok(()) => vm.resume().unwrap(),
            // The request of the guest is not dropped, it ends the pause.
            Err(Error::PauseInterrupted {
                state: State::ShutdownPending | State::Stopped,
                ..
            }) => {}
            // The guest powered off the VM before the pause.
            Err(Error::UnexpectedState {
                state: State::ShutdownPending | State::Stopped,
                ..
            }) => {}
            Err(e) => panic!("{e:?}"),
        }
        wait_state(&vm, State::Stopped);
    }

    vm.terminate().unwrap();
}

#[test]
#[cfg_attr(not(feature = "test-hv"), ignore)]
fn test_vcpu_failure_while_pausing() {
    let kvm = Kvm::new(KvmSpec::default()).unwrap();
    let temp_dir = TempDir::new().unwrap();
    let vm = create_vm(&kvm, &temp_dir.path().join("missing.bin"));

    // The pause does not wait for a vCPU that fails to boot.
    vm.boot().unwrap();
    assert_matches!(
        vm.pause(),
        Err(Error::PauseInterrupted {
            state: State::Terminated,
            ..
        } | Error::UnexpectedState {
            state: State::Terminated,
            ..
        })
    );
    assert_matches!(vm.terminate(), Err(Error::VcpuExit { index: 0, .. }));
}

#[test]
#[cfg_attr(not(feature = "test-hv"), ignore)]
fn test_concurrent_pause_resume() {
    let kvm = Kvm::new(KvmSpec::default()).unwrap();
    let temp_dir = TempDir::new().unwrap();
    let vm = fixture_vm(&kvm, &temp_dir, &COUNT);
    vm.boot().unwrap();

    thread::scope(|s| {
        let pauser = s.spawn(|| {
            let mut count = 0;
            while count < 1000 {
                match vm.pause() {
                    Ok(()) => count += 1,
                    Err(Error::PauseInterrupted {
                        state: State::Running,
                        ..
                    }) => count += 1,
                    // The VM has not resumed yet.
                    Err(Error::UnexpectedState {
                        state: State::Paused,
                        ..
                    }) => thread::yield_now(),
                    Err(e) => panic!("{e:?}"),
                }
            }
        });
        // Resumes that overtake the pauses in progress.
        while !pauser.is_finished() {
            let _ = vm.resume();
        }
    });
    if vm.state() == State::Paused {
        vm.resume().unwrap();
    }
    wait_counting(&vm);
    assert_eq!(vm.state(), State::Running);

    // The VM still pauses.
    vm.pause().unwrap();
    assert_not_counting(&vm);
    vm.resume().unwrap();
    wait_counting(&vm);

    vm.shutdown().unwrap();
    vm.wait_stopped();
    vm.terminate().unwrap();
}

/// A PCI device that counts how often it pauses and resumes.
#[derive(Debug)]
struct TestPciDev {
    inner: PvPanic,
    fail_pause: AtomicBool,
    pauses: AtomicU32,
    resumes: AtomicU32,
}

impl TestPciDev {
    /// Returns how often the device paused and resumed.
    fn counts(&self) -> (u32, u32) {
        let pauses = self.pauses.load(Ordering::Acquire);
        (pauses, self.resumes.load(Ordering::Acquire))
    }
}

impl Pause for TestPciDev {
    fn request_pause(&self) -> device::Result<()> {
        if self.fail_pause.load(Ordering::Acquire) {
            return device::error::NotPausable.fail();
        }
        self.pauses.fetch_add(1, Ordering::AcqRel);
        Ok(())
    }

    fn resume(&self) -> device::Result<()> {
        self.resumes.fetch_add(1, Ordering::AcqRel);
        Ok(())
    }
}

impl Pci for TestPciDev {
    fn name(&self) -> &str {
        "test"
    }

    fn config(&self) -> &dyn PciConfig {
        self.inner.config()
    }

    fn reset(&self) -> pci::Result<()> {
        self.inner.reset()
    }
}

fn add_test_pci_dev(vm: &Machine<Kvm>) -> Arc<TestPciDev> {
    let dev = Arc::new(TestPciDev {
        inner: PvPanic::new(),
        fail_pause: AtomicBool::new(false),
        pauses: AtomicU32::new(0),
        resumes: AtomicU32::new(0),
    });
    vm.add_pci_dev(None, dev.clone()).unwrap();
    dev
}

#[test]
#[cfg_attr(not(feature = "test-hv"), ignore)]
fn test_pause_resume_devices() {
    let kvm = Kvm::new(KvmSpec::default()).unwrap();
    let temp_dir = TempDir::new().unwrap();
    let vm = fixture_vm(&kvm, &temp_dir, &COUNT);
    let dev = add_test_pci_dev(&vm);
    vm.boot().unwrap();
    wait_counting(&vm);

    vm.pause().unwrap();
    assert_eq!(dev.counts(), (1, 0));
    vm.resume().unwrap();
    assert_eq!(dev.counts(), (1, 1));
    wait_counting(&vm);

    // The devices of a paused VM resume before they are reset.
    vm.pause().unwrap();
    vm.reboot().unwrap();
    assert_eq!(dev.counts(), (2, 2));
    wait_state(&vm, State::Running);
    wait_counting(&vm);

    vm.pause().unwrap();
    vm.shutdown().unwrap();
    assert_eq!(dev.counts(), (3, 3));
    vm.wait_stopped();
    vm.terminate().unwrap();
}

#[test]
#[cfg_attr(not(feature = "test-hv"), ignore)]
fn test_pause_device_failure() {
    let kvm = Kvm::new(KvmSpec::default()).unwrap();
    let temp_dir = TempDir::new().unwrap();
    let vm = fixture_vm(&kvm, &temp_dir, &COUNT);
    let good = add_test_pci_dev(&vm);
    let bad = add_test_pci_dev(&vm);
    bad.fail_pause.store(true, Ordering::Release);
    vm.boot().unwrap();
    wait_counting(&vm);

    // The VM keeps running, and a device that paused before the failure
    // resumes.
    assert_matches!(vm.pause(), Err(Error::PauseDevice { name, .. }) if name == "test");
    assert_eq!(vm.state(), State::Running);
    wait_counting(&vm);
    let (pauses, resumes) = good.counts();
    assert_eq!(pauses, resumes);

    bad.fail_pause.store(false, Ordering::Release);
    vm.pause().unwrap();
    assert_not_counting(&vm);
    assert_eq!(good.counts(), (pauses + 1, resumes));
    vm.resume().unwrap();
    wait_counting(&vm);

    vm.shutdown().unwrap();
    vm.wait_stopped();
    vm.terminate().unwrap();
}

#[test]
#[cfg_attr(not(feature = "test-hv"), ignore)]
fn test_pause_passthrough_device() {
    let kvm = Kvm::new(KvmSpec::default()).unwrap();
    let temp_dir = TempDir::new().unwrap();
    let vm = fixture_vm(&kvm, &temp_dir, &COUNT);
    let dev = add_test_pci_dev(&vm);
    vm.passthrough.lock().push("vfio-0".into());
    vm.boot().unwrap();
    wait_counting(&vm);

    // The VM refuses the pause before it stops anything.
    assert_matches!(vm.pause(), Err(Error::Passthrough { name, .. }) if &*name == "vfio-0");
    assert_eq!(vm.state(), State::Running);
    assert_eq!(dev.counts(), (0, 0));
    wait_counting(&vm);

    vm.shutdown().unwrap();
    vm.wait_stopped();
    vm.terminate().unwrap();
}

/// A step of a pause, as a device sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step {
    Request,
    Wait,
    Resume,
}

/// What the devices that share a log saw of a pause.
#[derive(Debug, Default)]
struct PauseLog {
    /// The steps in order, each with the index of its device.
    steps: Vec<(usize, Step)>,
    /// The deadlines of the waits, in order.
    deadlines: Vec<Instant>,
}

/// A device that logs the steps of a pause, and fails in one of them.
#[derive(Debug)]
struct TestDev {
    index: usize,
    fail: Option<Step>,
    log: Arc<Mutex<PauseLog>>,
}

impl TestDev {
    fn step(&self, step: Step) -> device::Result<()> {
        self.log.lock().steps.push((self.index, step));
        if self.fail == Some(step) {
            return device::error::NotPausable.fail();
        }
        Ok(())
    }
}

impl Pause for TestDev {
    fn request_pause(&self) -> device::Result<()> {
        self.step(Step::Request)
    }

    fn wait_pause(&self, deadline: Instant) -> device::Result<()> {
        self.log.lock().deadlines.push(deadline);
        self.step(Step::Wait)
    }

    fn resume(&self) -> device::Result<()> {
        self.step(Step::Resume)
    }
}

/// Devices with their names, like the devices of a board.
type Devices = Vec<(String, Arc<dyn Pause>)>;

/// Creates a device for each item of `fails`, which fails in that step.
fn test_devices(fails: &[Option<Step>]) -> (Devices, Arc<Mutex<PauseLog>>) {
    let log = Arc::<Mutex<PauseLog>>::default();
    let devices = fails.iter().enumerate().map(|(index, fail)| {
        let dev = TestDev {
            index,
            fail: *fail,
            log: log.clone(),
        };
        (format!("dev-{index}"), Arc::new(dev) as Arc<dyn Pause>)
    });
    (devices.collect(), log)
}

#[test]
fn test_pause_devices() {
    let (devices, log) = test_devices(&[None, None]);
    let start = Instant::now();
    assert_matches!(vm::pause_devices(&devices), Ok(()));
    let log = log.lock();
    // Every device is asked to stop before the first wait.
    assert_eq!(
        log.steps,
        [
            (0, Step::Request),
            (1, Step::Request),
            (0, Step::Wait),
            (1, Step::Wait),
        ]
    );
    // The devices stop at the same time, so they share one deadline.
    let deadline = log.deadlines[0];
    assert_eq!(log.deadlines, [deadline; 2]);
    assert!(deadline >= start + vm::DEVICE_PAUSE_TIMEOUT);
    assert!(deadline <= Instant::now() + vm::DEVICE_PAUSE_TIMEOUT);
}

#[test]
fn test_pause_devices_request_failure() {
    let (devices, log) = test_devices(&[None, Some(Step::Request), None]);
    assert_matches!(
        vm::pause_devices(&devices),
        Err(Error::PauseDevice { name, .. }) if name == "dev-1"
    );
    // The device that was asked to stop resumes, and the last device is
    // left alone.
    assert_eq!(
        log.lock().steps,
        [(0, Step::Request), (1, Step::Request), (0, Step::Resume)]
    );
}

#[test]
fn test_pause_devices_wait_failure() {
    let (devices, log) = test_devices(&[None, Some(Step::Wait), None]);
    assert_matches!(
        vm::pause_devices(&devices),
        Err(Error::PauseDevice { name, .. }) if name == "dev-1"
    );
    // Every device was asked to stop, so every device resumes, also the
    // ones that have not stopped.
    assert_eq!(
        log.lock().steps,
        [
            (0, Step::Request),
            (1, Step::Request),
            (2, Step::Request),
            (0, Step::Wait),
            (1, Step::Wait),
            (2, Step::Resume),
            (1, Step::Resume),
            (0, Step::Resume),
        ]
    );
}
