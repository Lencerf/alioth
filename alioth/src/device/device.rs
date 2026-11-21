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

use std::time::Instant;

use alioth_macros::trace_error;
use snafu::Snafu;

use crate::errors::DebugTrace;
use crate::mem::emulated::Mmio;

pub mod clock;
pub mod cmos;
pub mod console;
#[cfg(target_arch = "x86_64")]
#[path = "fw_cfg/fw_cfg.rs"]
pub mod fw_cfg;
pub mod fw_dbg;
pub mod ioapic;
pub mod net;
pub mod pl011;
pub mod pl031;
pub mod serial;

#[trace_error]
#[derive(Snafu, DebugTrace)]
#[snafu(module, visibility(pub(crate)), context(suffix(false)))]
pub enum Error {
    #[snafu(display("Error from OS"), context(false))]
    System { error: std::io::Error },
    #[snafu(display("Device is not pausable"))]
    NotPausable,
    #[snafu(display("Failed to pause device"))]
    Pause {
        source: Box<dyn DebugTrace + Send + Sync + 'static>,
    },
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

/// A device that stops while the VM is paused.
///
/// The vCPUs do not run while a device is paused, so a device only has to
/// stop what it does on its own, e.g. in a worker thread. A device that
/// acts only when a vCPU accesses it has nothing to do.
///
/// A pause takes two steps, so that a VM can ask all of its devices to
/// stop before it waits for any of them.
pub trait Pause {
    /// Asks the device to stop, without waiting for it.
    ///
    /// From then on the device stops, and stays stopped, until it resumes.
    fn request_pause(&self) -> Result<()>;

    /// Waits until `deadline` for the device to stop, as
    /// [`request_pause`](Self::request_pause) asked it to. Once this
    /// returns `Ok`, the device does not access guest memory, send
    /// interrupts, or change its state until it resumes.
    ///
    /// The provided method is for a device that has stopped when
    /// `request_pause()` returns.
    fn wait_pause(&self, _deadline: Instant) -> Result<()> {
        Ok(())
    }

    /// Lets the device run again, whether it has stopped or not.
    fn resume(&self) -> Result<()>;
}

pub trait MmioDev: Mmio + Pause {}
