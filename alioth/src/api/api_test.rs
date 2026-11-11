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

use std::sync::Arc;
use std::thread;
use std::time::Duration;

use assert_matches::assert_matches;
use parking_lot::{Condvar, Mutex};
use rstest::rstest;

use crate::api::{Code, Error, Instance, ListVmsRequest, Result, Vm, VmManager, VmState};
use crate::board::BoardSpec;
use crate::cpu::State;
use crate::device::console::{ConsoleSpec, PtyConsoleSpec};
use crate::mem::MemSpec;
use crate::vm::config::VmSpec;

#[derive(Debug)]
struct FakeState {
    state: Mutex<State>,
    cond: Condvar,
}

impl FakeState {
    fn new() -> Self {
        FakeState {
            state: Mutex::new(State::Stopped),
            cond: Condvar::new(),
        }
    }

    /// Changes the state as the vCPUs do, e.g. when the guest powers off.
    fn set(&self, state: State) {
        *self.state.lock() = state;
        self.cond.notify_all();
    }

    fn transit(&self, want: &[State], next: State) -> Result<()> {
        let mut state = self.state.lock();
        if !want.contains(&state) {
            let msg = format!("the VM is {}", VmState::from(*state));
            return Err(Error::new(Code::FailedPrecondition, msg));
        }
        *state = next;
        self.cond.notify_all();
        Ok(())
    }
}

/// Keeps the states of the fake VMs for tests to inspect.
#[derive(Debug, Default)]
struct FakeHv {
    states: Mutex<Vec<Arc<FakeState>>>,
}

/// A fake VM. At most `MAX` of them can exist, unless `MAX` is 0.
#[derive(Debug)]
struct FakeVm<const MAX: usize = 0> {
    state: Arc<FakeState>,
}

impl<const MAX: usize> Instance for FakeVm<MAX> {
    type Hypervisor = FakeHv;

    const MAX_VMS: Option<usize> = if MAX == 0 { None } else { Some(MAX) };

    fn create(hypervisor: &FakeHv, spec: VmSpec) -> Result<Self> {
        if spec.board.mem.size == 0 {
            return Err(Error::new(Code::InvalidArgument, "no memory"));
        }
        let state = Arc::new(FakeState::new());
        hypervisor.states.lock().push(state.clone());
        Ok(FakeVm { state })
    }

    fn state(&self) -> State {
        *self.state.state.lock()
    }

    fn boot(&self) -> Result<()> {
        self.state.transit(&[State::Stopped], State::Running)
    }

    fn shutdown(&self) -> Result<()> {
        let want = &[
            State::Running,
            State::Paused,
            State::RebootPending,
            State::ShutdownPending,
        ];
        self.state.transit(want, State::ShutdownPending)?;
        // The vCPUs reset the VM in the background.
        let state = self.state.clone();
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(10));
            state.set(State::Stopped);
        });
        Ok(())
    }

    fn reboot(&self) -> Result<()> {
        let want = &[State::Running, State::Paused, State::RebootPending];
        self.state.transit(want, State::Running)
    }

    fn wait_stopped(&self) {
        let mut state = self.state.state.lock();
        self.state.cond.wait_while(&mut state, |s| {
            !matches!(s, State::Stopped | State::Terminated)
        });
    }

    fn terminate(&self) -> Result<()> {
        let want = &[State::Stopped, State::Terminated];
        self.state.transit(want, State::Terminated)
    }
}

fn fixture_manager() -> VmManager<FakeVm> {
    VmManager::new(FakeHv::default())
}

fn fixture_spec(console: ConsoleSpec) -> VmSpec {
    VmSpec {
        board: BoardSpec {
            mem: MemSpec {
                size: 1 << 30,
                ..Default::default()
            },
            ..Default::default()
        },
        console,
        ..Default::default()
    }
}

fn pty() -> ConsoleSpec {
    ConsoleSpec::Pty(PtyConsoleSpec::default())
}

fn names(vms: &[Vm]) -> Vec<&str> {
    vms.iter().map(|vm| vm.name.as_str()).collect()
}

#[test]
fn test_create() {
    let manager = fixture_manager();

    let vm = manager.create(None, fixture_spec(pty())).unwrap();
    assert_eq!(vm.name, "vms/vm0");
    assert_eq!(vm.state, VmState::Stopped);
    assert_eq!(vm.spec, fixture_spec(pty()));

    assert_eq!(
        manager.create(None, fixture_spec(pty())).unwrap().name,
        "vms/vm1"
    );
    let vm = manager.create(Some("web-1"), fixture_spec(pty())).unwrap();
    assert_eq!(vm.name, "vms/web-1");
    assert_matches!(
        manager.create(Some("web-1"), fixture_spec(pty())),
        Err(Error {
            code: 409,
            status: Code::AlreadyExists,
            ..
        })
    );

    // The first free ID is reused.
    manager.delete("vm0").unwrap();
    assert_eq!(
        manager.create(None, fixture_spec(pty())).unwrap().name,
        "vms/vm0"
    );
}

#[rstest]
#[case("")]
#[case("Vm")]
#[case("1vm")]
#[case("vm-")]
#[case("vm_0")]
#[case(&"v".repeat(64))]
fn test_create_invalid_id(#[case] id: &str) {
    let manager = fixture_manager();
    assert_matches!(
        manager.create(Some(id), fixture_spec(pty())),
        Err(Error {
            code: 400,
            status: Code::InvalidArgument,
            ..
        })
    );
}

#[test]
fn test_create_failure() {
    let manager = fixture_manager();
    let mut spec = fixture_spec(pty());
    spec.board.mem.size = 0;
    assert_matches!(
        manager.create(Some("vm0"), spec),
        Err(Error {
            status: Code::InvalidArgument,
            ..
        })
    );
    // The ID is free again.
    manager.create(Some("vm0"), fixture_spec(pty())).unwrap();
}

#[test]
fn test_create_stdio() {
    let manager = fixture_manager();
    manager
        .create(None, fixture_spec(ConsoleSpec::Stdio))
        .unwrap();
    assert_matches!(
        manager.create(None, fixture_spec(ConsoleSpec::Stdio)),
        Err(Error {
            status: Code::FailedPrecondition,
            ..
        })
    );
    manager.create(None, fixture_spec(pty())).unwrap();
}

#[test]
fn test_create_max_vms() {
    let manager = VmManager::<FakeVm<1>>::new(FakeHv::default());
    manager.create(None, fixture_spec(pty())).unwrap();
    assert_matches!(
        manager.create(None, fixture_spec(pty())),
        Err(Error {
            status: Code::FailedPrecondition,
            ..
        })
    );
    manager.delete("vm0").unwrap();
    manager.create(None, fixture_spec(pty())).unwrap();
}

#[test]
fn test_get() {
    let manager = fixture_manager();
    let vm = manager.create(None, fixture_spec(pty())).unwrap();
    let got = manager.get("vm0").unwrap();
    assert_eq!(got.name, vm.name);
    assert_eq!(got.spec, vm.spec);
    assert_matches!(
        manager.get("vm1"),
        Err(Error {
            code: 404,
            status: Code::NotFound,
            ..
        })
    );
}

#[test]
fn test_list() {
    let manager = fixture_manager();
    for _ in 0..5 {
        manager.create(None, fixture_spec(pty())).unwrap();
    }

    let mut request = ListVmsRequest {
        page_size: 2,
        page_token: String::new(),
    };
    let mut pages = vec![];
    loop {
        let response = manager.list(&request).unwrap();
        pages.push(names(&response.vms).join(","));
        if response.next_page_token.is_empty() {
            break;
        }
        request.page_token = response.next_page_token;
    }
    assert_eq!(pages, ["vms/vm0,vms/vm1", "vms/vm2,vms/vm3", "vms/vm4"]);

    let all = manager.list(&ListVmsRequest::default()).unwrap();
    assert_eq!(all.vms.len(), 5);
    assert_eq!(all.next_page_token, "");

    let request = ListVmsRequest {
        page_size: 0,
        page_token: "../vm0".to_owned(),
    };
    assert_matches!(
        manager.list(&request),
        Err(Error {
            status: Code::InvalidArgument,
            ..
        })
    );
}

#[test]
fn test_lifecycle() {
    let manager = fixture_manager();
    manager.create(None, fixture_spec(pty())).unwrap();

    assert_eq!(manager.boot("vm0").unwrap().state, VmState::Running);
    let err = manager.boot("vm0").unwrap_err();
    assert_eq!(err.status, Code::FailedPrecondition);
    assert_eq!(err.message, "vms/vm0: the VM is RUNNING");

    assert_eq!(manager.reboot("vm0").unwrap().state, VmState::Running);

    // Shutdown returns after the VM stops.
    assert_eq!(manager.shutdown("vm0").unwrap().state, VmState::Stopped);
    assert_matches!(
        manager.shutdown("vm0"),
        Err(Error {
            status: Code::FailedPrecondition,
            ..
        })
    );
    assert_matches!(
        manager.reboot("vm0"),
        Err(Error {
            status: Code::FailedPrecondition,
            ..
        })
    );

    assert_eq!(manager.boot("vm0").unwrap().state, VmState::Running);
    assert_matches!(
        manager.boot("vm1"),
        Err(Error {
            status: Code::NotFound,
            ..
        })
    );
}

#[test]
fn test_guest_power_off() {
    let manager = fixture_manager();
    manager.create(None, fixture_spec(pty())).unwrap();
    manager.boot("vm0").unwrap();

    manager.hypervisor.states.lock()[0].set(State::Stopped);
    manager.wait_stopped("vm0").unwrap();
    assert_eq!(manager.get("vm0").unwrap().state, VmState::Stopped);
}

#[test]
fn test_failed() {
    let manager = fixture_manager();
    manager.create(None, fixture_spec(pty())).unwrap();
    manager.boot("vm0").unwrap();

    manager.hypervisor.states.lock()[0].set(State::Terminated);
    assert_eq!(manager.get("vm0").unwrap().state, VmState::Failed);
    for result in [
        manager.boot("vm0"),
        manager.shutdown("vm0"),
        manager.reboot("vm0"),
    ] {
        assert_matches!(
            result,
            Err(Error {
                status: Code::FailedPrecondition,
                ..
            })
        );
    }
    manager.delete("vm0").unwrap();
    assert_matches!(
        manager.get("vm0"),
        Err(Error {
            status: Code::NotFound,
            ..
        })
    );
}

#[test]
fn test_delete() {
    let manager = fixture_manager();
    manager.create(None, fixture_spec(pty())).unwrap();
    manager.create(None, fixture_spec(pty())).unwrap();
    manager.boot("vm1").unwrap();

    // A running VM is powered off and terminated.
    manager.delete("vm1").unwrap();
    assert_eq!(
        *manager.hypervisor.states.lock()[1].state.lock(),
        State::Terminated
    );
    assert_matches!(
        manager.get("vm1"),
        Err(Error {
            status: Code::NotFound,
            ..
        })
    );
    assert_matches!(
        manager.delete("vm1"),
        Err(Error {
            status: Code::NotFound,
            ..
        })
    );

    manager.delete_all();
    assert_eq!(
        *manager.hypervisor.states.lock()[0].state.lock(),
        State::Terminated
    );
    assert!(
        manager
            .list(&ListVmsRequest::default())
            .unwrap()
            .vms
            .is_empty()
    );
}

#[rstest]
#[case(Code::InvalidArgument, 400)]
#[case(Code::FailedPrecondition, 400)]
#[case(Code::NotFound, 404)]
#[case(Code::AlreadyExists, 409)]
#[case(Code::Unimplemented, 501)]
#[case(Code::Internal, 500)]
fn test_http_status(#[case] code: Code, #[case] status: u16) {
    assert_eq!(code.http_status(), status);
    assert_eq!(Error::new(code, "").code, status);
}

#[rstest]
#[case(State::Stopped, VmState::Stopped)]
#[case(State::ShutdownPending, VmState::Stopping)]
#[case(State::Running, VmState::Running)]
#[case(State::RebootPending, VmState::Running)]
#[case(State::Paused, VmState::Paused)]
#[case(State::Terminated, VmState::Failed)]
fn test_vm_state(#[case] state: State, #[case] want: VmState) {
    assert_eq!(VmState::from(state), want);
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
#[test]
#[cfg_attr(not(feature = "test-hv"), ignore)]
fn test_kvm() {
    use tempfile::TempDir;

    use crate::hv::{Kvm, KvmSpec};
    use crate::vm::Machine;
    use crate::vm::tests::{HALT, fixture_spec, write_firmware};

    let temp_dir = TempDir::new().unwrap();
    let firmware = write_firmware(&temp_dir, &HALT);
    let manager = VmManager::<Machine<Kvm>>::new(Kvm::new(KvmSpec::default()).unwrap());

    manager.create(None, fixture_spec(&firmware)).unwrap();
    assert_eq!(manager.boot("vm0").unwrap().state, VmState::Running);
    assert_eq!(manager.reboot("vm0").unwrap().state, VmState::Running);
    assert_eq!(manager.shutdown("vm0").unwrap().state, VmState::Stopped);

    // A running VM is powered off before it is deleted.
    manager.boot("vm0").unwrap();
    manager.delete("vm0").unwrap();
    assert_matches!(
        manager.get("vm0"),
        Err(Error {
            status: Code::NotFound,
            ..
        })
    );
}
