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

use std::fmt::{Display, Formatter};
use std::io::ErrorKind;

use crate::{bitflags, consts};

pub type MachPort = libc::mach_port_t;

pub const MACH_PORT_NULL: MachPort = 0;

consts! {
    /// Return codes shared by `kern_return_t` and `mach_msg_return_t`.
    ///
    /// The two spaces do not overlap: kernel errors are small integers, while
    /// message errors carry the `0x1000_0000` system code.
    pub struct MachRet(i32) {
        SUCCESS = 0;

        INVALID_ADDRESS = 1;
        PROTECTION_FAILURE = 2;
        NO_SPACE = 3;
        INVALID_ARGUMENT = 4;
        FAILURE = 5;
        RESOURCE_SHORTAGE = 6;
        NOT_RECEIVER = 7;
        NO_ACCESS = 8;
        NAME_EXISTS = 13;
        ABORTED = 14;
        INVALID_NAME = 15;
        INVALID_TASK = 16;
        INVALID_RIGHT = 17;
        INVALID_VALUE = 18;
        UREFS_OVERFLOW = 19;
        INVALID_CAPABILITY = 20;
        RIGHT_EXISTS = 21;

        SEND_IN_PROGRESS = 0x1000_0001;
        SEND_INVALID_DATA = 0x1000_0002;
        SEND_INVALID_DEST = 0x1000_0003;
        SEND_TIMED_OUT = 0x1000_0004;
        SEND_MSG_TOO_SMALL = 0x1000_0008;
        RCV_TIMED_OUT = 0x1000_4003;
    }
}

impl Display for MachRet {
    fn fmt(&self, f: &mut Formatter) -> std::fmt::Result {
        write!(f, "{} ({:#010x})", self.name(), self.raw() as u32)
    }
}

impl std::error::Error for MachRet {}

impl MachRet {
    /// Maps a Mach return code onto [`std::io::Error`], mirroring `check_ret`
    /// in the Hypervisor.framework bindings.
    pub fn check(self) -> std::io::Result<()> {
        if self == MachRet::SUCCESS {
            return Ok(());
        }
        let kind = match self {
            MachRet::PROTECTION_FAILURE | MachRet::NO_ACCESS => ErrorKind::PermissionDenied,
            MachRet::INVALID_ARGUMENT | MachRet::INVALID_VALUE => ErrorKind::InvalidInput,
            MachRet::INVALID_NAME | MachRet::INVALID_RIGHT => ErrorKind::NotFound,
            MachRet::NAME_EXISTS | MachRet::RIGHT_EXISTS => ErrorKind::AlreadyExists,
            MachRet::NO_SPACE | MachRet::RESOURCE_SHORTAGE => ErrorKind::OutOfMemory,
            MachRet::SEND_INVALID_DEST => ErrorKind::BrokenPipe,
            MachRet::SEND_TIMED_OUT | MachRet::RCV_TIMED_OUT => ErrorKind::WouldBlock,
            _ => ErrorKind::Other,
        };
        Err(std::io::Error::new(kind, self))
    }
}

consts! {
    /// `mach_port_right_t`
    pub struct MachPortRight(u32) {
        SEND = 0;
        RECEIVE = 1;
        SEND_ONCE = 2;
        PORT_SET = 3;
        DEAD_NAME = 4;
    }
}

consts! {
    /// `mach_msg_type_name_t`, the disposition of a transferred right.
    pub struct MachMsgType(u32) {
        MOVE_RECEIVE = 16;
        MOVE_SEND = 17;
        MOVE_SEND_ONCE = 18;
        COPY_SEND = 19;
        MAKE_SEND = 20;
        MAKE_SEND_ONCE = 21;
    }
}

consts! {
    /// `mach_port_flavor_t`
    pub struct MachPortFlavor(i32) {
        LIMITS_INFO = 1;
        RECEIVE_STATUS = 2;
    }
}

bitflags! {
    /// `mach_msg_option_t`
    pub struct MachMsgOption(i32) {
        SEND_MSG = 1 << 0;
        RCV_MSG = 1 << 1;
        RCV_LARGE = 1 << 2;
        SEND_TIMEOUT = 1 << 4;
        RCV_TIMEOUT = 1 << 8;
    }
}

/// `mach_msg_header_t`
#[repr(C)]
#[derive(Debug, Clone, Default)]
pub struct MachMsgHeader {
    pub bits: u32,
    pub size: u32,
    pub remote_port: MachPort,
    pub local_port: MachPort,
    pub voucher_port: MachPort,
    pub id: i32,
}

/// `mach_port_limits_t`
#[repr(C)]
#[derive(Debug, Clone, Default)]
pub struct MachPortLimits {
    pub qlimit: u32,
}

impl MachPortLimits {
    /// `MACH_PORT_LIMITS_INFO_COUNT`, in units of `natural_t`.
    pub const COUNT: u32 = 1;
}

/// A receive buffer large enough for a bare header plus the largest trailer
/// the kernel may append.
#[repr(C, align(8))]
#[derive(Debug)]
pub struct MachMsgRecvBuf([u8; Self::SIZE as usize]);

impl MachMsgRecvBuf {
    const SIZE: u32 = 128;

    pub const fn new() -> Self {
        MachMsgRecvBuf([0; Self::SIZE as usize])
    }

    pub const fn size(&self) -> u32 {
        Self::SIZE
    }

    pub const fn as_mut_ptr(&mut self) -> *mut MachMsgHeader {
        self.0.as_mut_ptr().cast()
    }
}

impl Default for MachMsgRecvBuf {
    fn default() -> Self {
        Self::new()
    }
}

unsafe extern "C" {
    /// The task's own port name, published by libSystem during process start
    /// up. `mach_task_self()` is a macro over this global in C.
    static mach_task_self_: MachPort;

    pub fn mach_port_allocate(task: MachPort, right: MachPortRight, name: &mut MachPort)
    -> MachRet;

    pub fn mach_port_insert_right(
        task: MachPort,
        name: MachPort,
        poly: MachPort,
        poly_type: MachMsgType,
    ) -> MachRet;

    pub fn mach_port_set_attributes(
        task: MachPort,
        name: MachPort,
        flavor: MachPortFlavor,
        info: *const u32,
        count: u32,
    ) -> MachRet;

    pub fn mach_port_deallocate(task: MachPort, name: MachPort) -> MachRet;

    pub fn mach_port_mod_refs(
        task: MachPort,
        name: MachPort,
        right: MachPortRight,
        delta: i32,
    ) -> MachRet;

    pub fn mach_msg(
        msg: *mut MachMsgHeader,
        option: MachMsgOption,
        send_size: u32,
        rcv_size: u32,
        rcv_name: MachPort,
        timeout: u32,
        notify: MachPort,
    ) -> MachRet;
}

/// Returns the port naming the calling task, the first argument to every
/// `mach_port_*` routine.
pub fn mach_task_self() -> MachPort {
    // Written once by libSystem before `main` runs and read-only thereafter.
    unsafe { mach_task_self_ }
}

#[cfg(test)]
#[path = "mach_test.rs"]
mod tests;
