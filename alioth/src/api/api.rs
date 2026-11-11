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

//! A manager of VMs. Its resources, methods, and errors follow the AIPs
//! (<https://google.aip.dev>).

use std::cmp;
use std::collections::BTreeMap;
use std::error::Error as StdError;
use std::fmt::{self, Display, Formatter};
use std::ops::Bound;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

use crate::cpu::State;
use crate::device::console::ConsoleSpec;
use crate::hv::Hypervisor;
use crate::vm::config::VmSpec;
use crate::vm::{self, Machine};

/// Canonical error codes, see AIP-193.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Code {
    InvalidArgument,
    FailedPrecondition,
    NotFound,
    AlreadyExists,
    Unimplemented,
    Internal,
}

impl Code {
    /// Returns the HTTP status code that AIP-193 maps the code to.
    pub fn http_status(self) -> u16 {
        match self {
            Code::InvalidArgument | Code::FailedPrecondition => 400,
            Code::NotFound => 404,
            Code::AlreadyExists => 409,
            Code::Unimplemented => 501,
            Code::Internal => 500,
        }
    }
}

/// An error, in the shape of the `error` object of AIP-193.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Error {
    /// The HTTP status code.
    pub code: u16,
    pub message: String,
    pub status: Code,
}

impl Error {
    pub fn new(status: Code, message: impl Into<String>) -> Self {
        Error {
            code: status.http_status(),
            message: message.into(),
            status,
        }
    }
}

impl Display for Error {
    fn fmt(&self, f: &mut Formatter) -> fmt::Result {
        write!(f, "{:?}: {}", self.status, self.message)
    }
}

impl StdError for Error {}

pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Formats `error` and its sources on one line.
fn chain(error: &dyn StdError) -> String {
    let mut message = error.to_string();
    let mut source = error.source();
    while let Some(error) = source {
        message.push_str(": ");
        message.push_str(&error.to_string());
        source = error.source();
    }
    message
}

impl From<vm::Error> for Error {
    fn from(error: vm::Error) -> Self {
        match error {
            vm::Error::UnexpectedState { state, .. } => {
                let state = VmState::from(state);
                Error::new(Code::FailedPrecondition, format!("the VM is {state}"))
            }
            error => Error::new(Code::Internal, chain(&error)),
        }
    }
}

/// The state of a VM, see AIP-216.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum VmState {
    /// The VM is not running. It can boot.
    Stopped,
    /// The VM is being powered off.
    Stopping,
    /// The VM is running, or rebooting.
    Running,
    /// The VM is paused.
    Paused,
    /// A VMM-side error stopped the VM. It can only be deleted.
    Failed,
}

impl From<State> for VmState {
    fn from(state: State) -> Self {
        match state {
            State::Stopped => VmState::Stopped,
            State::ShutdownPending => VmState::Stopping,
            State::Running | State::RebootPending => VmState::Running,
            State::Paused => VmState::Paused,
            State::Terminated => VmState::Failed,
        }
    }
}

impl Display for VmState {
    fn fmt(&self, f: &mut Formatter) -> fmt::Result {
        let s = match self {
            VmState::Stopped => "STOPPED",
            VmState::Stopping => "STOPPING",
            VmState::Running => "RUNNING",
            VmState::Paused => "PAUSED",
            VmState::Failed => "FAILED",
        };
        f.write_str(s)
    }
}

/// A VM resource.
#[derive(Debug, Serialize)]
pub struct Vm {
    /// The resource name, e.g. `vms/vm0`.
    pub name: String,
    pub state: VmState,
    #[serde(flatten)]
    pub spec: VmSpec,
}

/// The query of a Create request.
#[derive(Debug, Default, Deserialize)]
pub struct CreateVmRequest {
    /// The ID of the VM. If omitted, the first free one of `vm0`, `vm1`, ...
    pub vm_id: Option<String>,
}

/// The query of a List request, see AIP-158.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct ListVmsRequest {
    pub page_size: u32,
    pub page_token: String,
}

#[derive(Debug, Serialize)]
pub struct ListVmsResponse {
    pub vms: Vec<Vm>,
    pub next_page_token: String,
}

const DEFAULT_PAGE_SIZE: u32 = 50;
const MAX_PAGE_SIZE: u32 = 1000;

/// A VM that a [`VmManager`] manages.
pub trait Instance: Sized + Send + Sync + 'static {
    /// What creates the VMs.
    type Hypervisor: Send + Sync + 'static;

    /// The maximum number of VMs that the Hypervisor can support at the same
    /// time.
    const MAX_VMS: Option<usize> = None;

    fn create(hypervisor: &Self::Hypervisor, spec: VmSpec) -> Result<Self>;
    fn state(&self) -> State;
    fn boot(&self) -> Result<()>;
    fn shutdown(&self) -> Result<()>;
    fn reboot(&self) -> Result<()>;
    /// Blocks until the VM stops, or fails.
    fn wait_stopped(&self);
    /// Ends a stopped or failed VM.
    fn terminate(&self) -> Result<()>;
}

impl<H> Instance for Machine<H>
where
    H: Hypervisor + Send + Sync + 'static,
{
    type Hypervisor = H;

    const MAX_VMS: Option<usize> = H::MAX_VMS;

    fn create(hypervisor: &H, spec: VmSpec) -> Result<Self> {
        Machine::create(hypervisor, spec).map_err(|e| Error::new(Code::InvalidArgument, chain(&e)))
    }

    fn state(&self) -> State {
        Machine::state(self)
    }

    fn boot(&self) -> Result<()> {
        Ok(Machine::boot(self)?)
    }

    fn shutdown(&self) -> Result<()> {
        Ok(Machine::shutdown(self)?)
    }

    fn reboot(&self) -> Result<()> {
        Ok(Machine::reboot(self)?)
    }

    fn wait_stopped(&self) {
        Machine::wait_stopped(self)
    }

    fn terminate(&self) -> Result<()> {
        Ok(Machine::terminate(self)?)
    }
}

struct Entry<V> {
    vm: V,
    spec: VmSpec,
    deleting: AtomicBool,
}

impl<V: Instance> Entry<V> {
    fn resource(&self, id: &str) -> Vm {
        let state = if self.deleting.load(Ordering::Acquire) {
            VmState::Stopping
        } else {
            VmState::from(self.vm.state())
        };
        Vm {
            name: format!("vms/{id}"),
            state,
            spec: self.spec.clone(),
        }
    }
}

enum Slot<V> {
    /// The VM is being created.
    Creating {
        stdio: bool,
    },
    Ready(Arc<Entry<V>>),
}

impl<V> Slot<V> {
    fn uses_stdio(&self) -> bool {
        match self {
            Slot::Creating { stdio } => *stdio,
            Slot::Ready(entry) => matches!(entry.spec.console, ConsoleSpec::Stdio),
        }
    }
}

/// Checks a VM ID against AIP-122: lowercase letters, digits, and hyphens,
/// starting with a letter and not ending with a hyphen, at most 63 chars.
fn check_id(id: &str) -> Result<()> {
    let valid = id.len() <= 63
        && id.starts_with(|c: char| c.is_ascii_lowercase())
        && !id.ends_with('-')
        && id
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
    if valid {
        Ok(())
    } else {
        Err(Error::new(
            Code::InvalidArgument,
            format!("invalid VM ID {id:?}"),
        ))
    }
}

fn not_found(id: &str) -> Error {
    Error::new(Code::NotFound, format!("vms/{id} not found"))
}

/// Adds the resource name of VM `id` to `error`.
fn named(id: &str) -> impl FnOnce(Error) -> Error {
    move |e| Error {
        message: format!("vms/{id}: {}", e.message),
        ..e
    }
}

/// Manages VMs.
pub struct VmManager<V: Instance> {
    hypervisor: V::Hypervisor,
    vms: Mutex<BTreeMap<String, Slot<V>>>,
}

impl<V: Instance> VmManager<V> {
    pub fn new(hypervisor: V::Hypervisor) -> Self {
        VmManager {
            hypervisor,
            vms: Mutex::new(BTreeMap::new()),
        }
    }

    /// Creates a stopped VM.
    pub fn create(&self, vm_id: Option<&str>, spec: VmSpec) -> Result<Vm> {
        if spec.payload.executable.is_none() && spec.payload.firmware.is_none() {
            let msg = "the payload has neither an executable nor a firmware";
            return Err(Error::new(Code::InvalidArgument, msg));
        }
        let stdio = matches!(spec.console, ConsoleSpec::Stdio);
        let id = {
            let mut vms = self.vms.lock();
            if let Some(max) = V::MAX_VMS
                && vms.len() >= max
            {
                let msg = format!("cannot create more than {max} VMs");
                return Err(Error::new(Code::FailedPrecondition, msg));
            }
            if stdio && vms.values().any(Slot::uses_stdio) {
                let msg = "another VM uses the stdio console";
                return Err(Error::new(Code::FailedPrecondition, msg));
            }
            let id = match vm_id {
                Some(id) => {
                    check_id(id)?;
                    if vms.contains_key(id) {
                        let msg = format!("vms/{id} already exists");
                        return Err(Error::new(Code::AlreadyExists, msg));
                    }
                    id.to_owned()
                }
                None => (0..)
                    .map(|i| format!("vm{i}"))
                    .find(|id| !vms.contains_key(id))
                    .unwrap(),
            };
            vms.insert(id.clone(), Slot::Creating { stdio });
            id
        };

        let ret = V::create(&self.hypervisor, spec.clone());
        let mut vms = self.vms.lock();
        match ret {
            Ok(vm) => {
                let entry = Arc::new(Entry {
                    vm,
                    spec,
                    deleting: AtomicBool::new(false),
                });
                let resource = entry.resource(&id);
                vms.insert(id, Slot::Ready(entry));
                Ok(resource)
            }
            Err(e) => {
                vms.remove(&id);
                Err(e)
            }
        }
    }

    fn entry(&self, id: &str) -> Result<Arc<Entry<V>>> {
        match self.vms.lock().get(id) {
            Some(Slot::Ready(entry)) => Ok(entry.clone()),
            Some(Slot::Creating { .. }) | None => Err(not_found(id)),
        }
    }

    /// Returns the entry of VM `id`, unless the VM is being deleted.
    fn live_entry(&self, id: &str) -> Result<Arc<Entry<V>>> {
        let entry = self.entry(id)?;
        if entry.deleting.load(Ordering::Acquire) {
            let msg = format!("vms/{id} is being deleted");
            return Err(Error::new(Code::FailedPrecondition, msg));
        }
        Ok(entry)
    }

    pub fn get(&self, id: &str) -> Result<Vm> {
        Ok(self.entry(id)?.resource(id))
    }

    /// Lists VMs in the order of their IDs.
    pub fn list(&self, request: &ListVmsRequest) -> Result<ListVmsResponse> {
        let page_size = match request.page_size {
            0 => DEFAULT_PAGE_SIZE,
            size => cmp::min(size, MAX_PAGE_SIZE),
        };
        // A page token is the ID of the last VM of the previous page.
        let start = if request.page_token.is_empty() {
            Bound::Unbounded
        } else {
            check_id(&request.page_token)
                .map_err(|_| Error::new(Code::InvalidArgument, "invalid page token"))?;
            Bound::Excluded(request.page_token.as_str())
        };

        let vms = self.vms.lock();
        let mut entries =
            vms.range::<str, _>((start, Bound::Unbounded))
                .filter_map(|(id, slot)| match slot {
                    Slot::Ready(entry) => Some((id, entry)),
                    Slot::Creating { .. } => None,
                });
        let mut page = vec![];
        let mut last_id = None;
        for (id, entry) in entries.by_ref().take(page_size as usize) {
            page.push(entry.resource(id));
            last_id = Some(id);
        }
        let next_page_token = match (last_id, entries.next()) {
            (Some(id), Some(_)) => id.clone(),
            _ => String::new(),
        };
        Ok(ListVmsResponse {
            vms: page,
            next_page_token,
        })
    }

    /// Deletes VM `id`. A VM that is not stopped is powered off first.
    pub fn delete(&self, id: &str) -> Result<()> {
        let entry = self.live_entry(id)?;
        if entry.deleting.swap(true, Ordering::AcqRel) {
            let msg = format!("vms/{id} is being deleted");
            return Err(Error::new(Code::FailedPrecondition, msg));
        }
        match entry.vm.shutdown() {
            // The VM is already stopped, or failed.
            Ok(())
            | Err(Error {
                status: Code::FailedPrecondition,
                ..
            }) => {}
            Err(e) => {
                entry.deleting.store(false, Ordering::Release);
                return Err(named(id)(e));
            }
        }
        entry.vm.wait_stopped();
        if let Err(e) = entry.vm.terminate() {
            log::error!("vms/{id}: {e}");
        }
        self.vms.lock().remove(id);
        Ok(())
    }

    /// Deletes all VMs.
    pub fn delete_all(&self) {
        let vms = self.vms.lock();
        let ids: Vec<String> = vms
            .iter()
            .filter(|(_, slot)| matches!(slot, Slot::Ready(_)))
            .map(|(id, _)| id.clone())
            .collect();
        drop(vms);
        for id in ids {
            if let Err(e) = self.delete(&id) {
                log::error!("{e}");
            }
        }
    }

    /// Boots a stopped VM.
    pub fn boot(&self, id: &str) -> Result<Vm> {
        let entry = self.live_entry(id)?;
        entry.vm.boot().map_err(named(id))?;
        Ok(entry.resource(id))
    }

    /// Powers off VM `id`, and returns after it stops.
    pub fn shutdown(&self, id: &str) -> Result<Vm> {
        let entry = self.live_entry(id)?;
        entry.vm.shutdown().map_err(named(id))?;
        entry.vm.wait_stopped();
        Ok(entry.resource(id))
    }

    /// Reboots VM `id`.
    pub fn reboot(&self, id: &str) -> Result<Vm> {
        let entry = self.live_entry(id)?;
        entry.vm.reboot().map_err(named(id))?;
        Ok(entry.resource(id))
    }

    /// Blocks until VM `id` stops, fails, or is deleted.
    pub fn wait_stopped(&self, id: &str) -> Result<()> {
        self.entry(id)?.vm.wait_stopped();
        Ok(())
    }
}

#[cfg(test)]
#[path = "api_test.rs"]
mod tests;
