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

#[cfg(target_arch = "aarch64")]
#[path = "cpu_arm64.rs"]
mod aarch64;
#[cfg(target_arch = "x86_64")]
#[path = "cpu_amd64/cpu_amd64.rs"]
mod x86_64;

use std::sync::Arc;
use std::thread::JoinHandle;

use flume::Sender;
use parking_lot::{Condvar, Mutex, MutexGuard, RwLock};
use snafu::{ResultExt, Snafu};

use crate::board::Board;
use crate::errors::{DebugTrace, trace_error};
use crate::hv::{Vcpu, Vm, VmEntry, VmExit};
#[cfg(target_arch = "x86_64")]
use crate::loader::xen;
use crate::loader::{Executable, InitState, linux};

#[trace_error]
#[derive(Snafu, DebugTrace)]
#[snafu(module, context(suffix(false)))]
pub enum Error {
    #[snafu(display("Hypervisor internal error"), context(false))]
    HvError { source: Box<crate::hv::Error> },
    #[snafu(display("Failed to configure guest memory"), context(false))]
    Memory { source: Box<crate::mem::Error> },
    #[snafu(display("Failed to setup board"), context(false))]
    Board { source: Box<crate::board::Error> },
    #[snafu(display("Failed to reset PCI devices"))]
    ResetPci { source: Box<crate::pci::Error> },
    #[snafu(display("Firmware error"), context(false))]
    Firmware { source: Box<crate::firmware::Error> },
    #[snafu(display("Unknown firmware metadata"))]
    UnknownFirmwareMetadata,
    #[snafu(display("Missing payload"))]
    MissingPayload,
    #[snafu(display("Failed to load payload"), context(false))]
    Loader { source: Box<crate::loader::Error> },
    #[snafu(display("Failed to notify the VMM thread"))]
    NotifyVmm,
    #[snafu(display("Another VCPU thread has signaled failure"))]
    PeerFailure,
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    /// The vCPU threads wait for the VM to boot.
    Stopped,
    /// The guest runs.
    Running,
    /// The guest is paused.
    Paused,
    /// The VM is being reset, and boots again afterwards.
    RebootPending,
    /// The VM is being reset, and stops afterwards.
    ShutdownPending,
    /// The vCPU threads exit.
    Terminated,
}

pub(crate) struct MpSync {
    pub(crate) state: State,
    fatal: bool,
    count: u16,
    // Incremented every time all vCPUs arrive at the barrier.
    generation: u32,
    // The number of vCPUs that are parked for a pause.
    parked: u16,
}

pub struct VcpuHandle {
    pub thread: JoinHandle<Result<()>>,
}

pub struct Context<V: Vm> {
    pub(crate) board: Board<V>,
    pub(crate) vcpus: RwLock<Vec<VcpuHandle>>,

    pub(crate) sync: Mutex<MpSync>,
    pub(crate) cond: Condvar,
}

impl<V: Vm> Context<V> {
    pub fn new(board: Board<V>) -> Self {
        Self {
            board,
            vcpus: RwLock::new(Vec::new()),
            sync: Mutex::new(MpSync {
                state: State::Stopped,
                fatal: false,
                count: 0,
                generation: 0,
                parked: 0,
            }),
            cond: Condvar::new(),
        }
    }
}

struct VcpuThread<V: Vm> {
    ctx: Arc<Context<V>>,
    index: u16,
    event_tx: Sender<u16>,
    vcpu: <V as Vm>::Vcpu,
}

/// Blocks until `count` vCPUs arrive, or a vCPU fails. The last vCPU to
/// arrive calls `complete` before it releases the others.
fn barrier<F>(sync: &Mutex<MpSync>, cond: &Condvar, count: u16, complete: F) -> Result<()>
where
    F: FnOnce(&mut MpSync),
{
    let mut sync = sync.lock();
    if sync.fatal {
        return error::PeerFailure.fail();
    }

    sync.count += 1;
    if sync.count == count {
        sync.count = 0;
        sync.generation = sync.generation.wrapping_add(1);
        complete(&mut sync);
        cond.notify_all();
    } else {
        // The condition variable also signals state changes.
        let generation = sync.generation;
        cond.wait_while(&mut sync, |s| s.generation == generation && !s.fatal);
    }

    if sync.fatal {
        return error::PeerFailure.fail();
    }

    Ok(())
}

/// Parks a vCPU, one of `count`, until the VM leaves the paused state.
fn park(sync: &mut MutexGuard<MpSync>, cond: &Condvar, count: u16) {
    sync.parked += 1;
    if sync.parked == count {
        // Wakes up the thread in `wait_parked`.
        cond.notify_all();
    }
    cond.wait_while(sync, |s| s.state == State::Paused);
    sync.parked -= 1;
}

/// Blocks until `count` vCPUs are parked, or the VM leaves the paused state.
pub(crate) fn wait_parked(sync: &mut MutexGuard<MpSync>, cond: &Condvar, count: u16) {
    cond.wait_while(sync, |s| s.state == State::Paused && s.parked < count);
}

/// Changes the state to `next` for the reset that a vCPU requests, unless
/// the VM is being reset already. Returns `true` if the state changed.
fn request_reset(sync: &mut MutexGuard<MpSync>, cond: &Condvar, next: State) -> bool {
    // The request overrides a pause that is in progress.
    if !matches!(sync.state, State::Running | State::Paused) {
        return false;
    }
    sync.state = next;
    // Wakes up the parked vCPUs and the thread in `wait_parked`.
    cond.notify_all();
    true
}

impl<V: Vm> VcpuThread<V> {
    pub fn new(index: u16, ctx: Arc<Context<V>>, event_tx: Sender<u16>) -> Result<Self> {
        let identity = ctx.board.encode_cpu_identity(index);
        let vcpu = ctx.board.vm.create_vcpu(index, identity)?;

        Ok(Self {
            ctx,
            index,
            event_tx,
            vcpu,
        })
    }

    fn notify_vmm(&self) -> Result<()> {
        if self.event_tx.send(self.index).is_err() {
            error::NotifyVmm.fail()
        } else {
            Ok(())
        }
    }

    fn sync_vcpus(&self, vcpus: &[VcpuHandle]) -> Result<()> {
        barrier(&self.ctx.sync, &self.ctx.cond, vcpus.len() as u16, |_| {})
    }

    /// Waits for all vCPUs to finish the reset, and completes the request
    /// that caused it.
    fn sync_reset(&self, vcpus: &[VcpuHandle]) -> Result<()> {
        barrier(&self.ctx.sync, &self.ctx.cond, vcpus.len() as u16, |s| {
            s.state = match s.state {
                State::RebootPending => State::Running,
                State::ShutdownPending => State::Stopped,
                state => state,
            }
        })
    }

    fn load_payload(&self) -> Result<InitState> {
        let payload = self.ctx.board.payload.read();
        let Some(payload) = payload.as_ref() else {
            return error::MissingPayload.fail();
        };

        if let Some(fw) = payload.firmware.as_ref() {
            return self.setup_firmware(fw, payload);
        }

        let Some(exec) = &payload.executable else {
            return error::MissingPayload.fail();
        };
        let mem_regions = self.ctx.board.memory.mem_region_entries();
        let init_state = match exec {
            Executable::Linux(image) => linux::load(
                &self.ctx.board.memory.ram_bus(),
                &mem_regions,
                image.as_ref(),
                payload.cmdline.as_deref(),
                payload.initramfs.as_deref(),
            ),
            #[cfg(target_arch = "x86_64")]
            Executable::Pvh(image) => xen::load(
                &self.ctx.board.memory.ram_bus(),
                &mem_regions,
                image.as_ref(),
                payload.cmdline.as_deref(),
                payload.initramfs.as_deref(),
            ),
        }?;
        Ok(init_state)
    }

    fn boot_init_sync(&mut self) -> Result<()> {
        let ctx = self.ctx.clone();
        let vcpus = ctx.vcpus.read();
        if self.index == 0 {
            self.ctx.board.init_devices()?;
            let init_state = self.load_payload()?;
            self.init_boot_vcpu(&init_state)?;
            self.ctx.board.create_firmware_data(&init_state)?;
        }
        self.init_ap(&vcpus)?;
        self.coco_finalize(&vcpus)?;
        self.sync_vcpus(&vcpus)
    }

    fn vcpu_loop(&mut self) -> Result<State> {
        let mut vm_entry = VmEntry::None;
        loop {
            let vm_exit = self.vcpu.run(vm_entry)?;
            let memory = &self.ctx.board.memory;
            vm_entry = match vm_exit {
                #[cfg(target_arch = "x86_64")]
                VmExit::Io { port, write, size } => memory.handle_io(port, write, size)?,
                VmExit::Mmio { addr, write, size } => memory.handle_mmio(addr, write, size)?,
                VmExit::Shutdown => break Ok(State::ShutdownPending),
                VmExit::Reboot => break Ok(State::RebootPending),
                VmExit::Paused => break Ok(State::Paused),
                VmExit::Interrupted => {
                    let state = self.ctx.sync.lock();
                    match state.state {
                        State::Running => VmEntry::None,
                        State::Paused => VmEntry::Pause,
                        State::RebootPending => VmEntry::Reboot,
                        State::ShutdownPending | State::Stopped | State::Terminated => {
                            VmEntry::Shutdown
                        }
                    }
                }
                VmExit::ConvertMemory { gpa, size, private } => {
                    memory.mark_private_memory(&*self.ctx.board.vm, gpa, size, private)?;
                    VmEntry::None
                }
            };
        }
    }

    /// Runs the guest until the VM is reset, and parks the vCPU while the
    /// VM is paused.
    ///
    /// Returns the request of this vCPU, `Ok(State::Paused)` if another vCPU
    /// or the VMM requested the reset.
    fn run_guest(&mut self) -> Result<State> {
        loop {
            let vcpus = self.ctx.vcpus.read();
            let mut sync = self.ctx.sync.lock();
            if sync.state == State::Paused {
                park(&mut sync, &self.ctx.cond, vcpus.len() as u16);
            }
            if sync.state != State::Running {
                break Ok(State::Paused);
            }
            drop(sync);
            drop(vcpus);

            let request = self.vcpu_loop();
            if matches!(request, Ok(State::Paused)) {
                // The VM may have resumed already.
                continue;
            }

            let next = match request {
                Ok(State::RebootPending) => State::RebootPending,
                _ => State::ShutdownPending,
            };
            let vcpus = self.ctx.vcpus.read();
            let mut sync = self.ctx.sync.lock();
            if request_reset(&mut sync, &self.ctx.cond, next) {
                log::trace!("VCPU-{}: change state to {next:?}", self.index);
                stop_vcpus(&self.ctx.board, Some(self.index), &vcpus)?;
            }
            break request;
        }
    }

    fn run(&mut self) -> Result<()> {
        self.init_vcpu()?;

        'reboot: loop {
            let mut sync = self.ctx.sync.lock();
            loop {
                match sync.state {
                    State::Stopped => self.ctx.cond.wait(&mut sync),
                    // A pause or a reset requested before this vCPU boots is
                    // handled after the boot, together with the other vCPUs.
                    State::Running
                    | State::Paused
                    | State::RebootPending
                    | State::ShutdownPending => break,
                    State::Terminated => break 'reboot Ok(()),
                }
            }
            drop(sync);

            self.boot_init_sync()?;

            let request = self.run_guest();

            if self.index == 0 {
                let board = &self.ctx.board;
                board.pci_bus.segment.reset().context(error::ResetPci)?;
                board.memory.reset()?;
            }
            self.reset_vcpu()?;

            request?;

            let vcpus = self.ctx.vcpus.read();
            self.sync_reset(&vcpus)?;
        }
    }
}

fn vcpu_thread_<V: Vm>(index: u16, ctx: Arc<Context<V>>, event_tx: Sender<u16>) -> Result<()> {
    let mut thread = VcpuThread::new(index, ctx, event_tx)?;
    thread.notify_vmm()?;
    thread.run()
}

pub fn vcpu_thread<V: Vm>(index: u16, ctx: Arc<Context<V>>, event_tx: Sender<u16>) -> Result<()> {
    let ret = vcpu_thread_(index, ctx.clone(), event_tx);

    if matches!(ret, Ok(_) | Err(Error::PeerFailure { .. })) {
        return Ok(());
    }

    log::warn!("VCPU-{index} reported error {ret:?}, unblocking other VCPUs...");
    let mut sync = ctx.sync.lock();
    sync.fatal = true;
    sync.state = State::Terminated;
    ctx.cond.notify_all();
    ret
}

pub fn stop_vcpus<V: Vm>(
    board: &Board<V>,
    current: Option<u16>,
    vcpus: &[VcpuHandle],
) -> Result<()> {
    for (index, handle) in vcpus.iter().enumerate() {
        let index = index as u16;
        if let Some(current) = current {
            if current == index {
                continue;
            }
            log::info!("VCPU-{current}: stopping VCPU-{index}");
        } else {
            log::info!("Stopping VCPU-{index}");
        }
        let identity = board.encode_cpu_identity(index);
        board.vm.stop_vcpu(identity, &handle.thread)?;
    }
    Ok(())
}

#[cfg(test)]
#[path = "cpu_test.rs"]
mod tests;
