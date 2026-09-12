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

//! A [`Notifier`] backed by a Mach port.
//!
//! macOS has no `eventfd(2)`. A Mach port whose queue limit is one behaves the
//! same way when used purely as a wakeup: a send either enqueues the single
//! pending notification or fails with `MACH_SEND_TIMED_OUT`, which only means
//! that a notification is already pending. Both outcomes are success, so the
//! sender never blocks and notifications coalesce.
//!
//! Unlike an `EVFILT_USER` notifier, the port exists independently of any
//! kqueue. [`Notifier::notify`] therefore works before the notifier has been
//! registered, matching the way an `eventfd` counter latches on Linux, and a
//! send right can later be handed to another process.

use std::io::{Error, Result};
use std::os::fd::AsRawFd;
use std::ptr::null;

use mio::event::Source;
use mio::{Interest, Registry, Token};

use crate::ffi;
use crate::sys::mach::{
    MACH_PORT_NULL, MachMsgHeader, MachMsgOption, MachMsgRecvBuf, MachMsgType, MachPort,
    MachPortFlavor, MachPortLimits, MachPortRight, MachRet, mach_msg, mach_port_allocate,
    mach_port_deallocate, mach_port_insert_right, mach_port_mod_refs, mach_port_set_attributes,
    mach_task_self,
};

#[derive(Debug)]
pub struct Notifier {
    /// Holds both the receive right and a send right under the same name.
    port: MachPort,
}

impl Notifier {
    pub fn new() -> Result<Self> {
        let task = mach_task_self();
        let mut port = MACH_PORT_NULL;
        unsafe { mach_port_allocate(task, MachPortRight::RECEIVE, &mut port) }.check()?;

        // Constructed before anything else can fail so that `Drop` owns the
        // port from here on.
        let notifier = Notifier { port };

        unsafe { mach_port_insert_right(task, port, port, MachMsgType::MAKE_SEND) }.check()?;

        // Allow a single undelivered notification. Further sends then coalesce
        // into it rather than queueing up a backlog.
        let limits = MachPortLimits { qlimit: 1 };
        unsafe {
            mach_port_set_attributes(
                task,
                port,
                MachPortFlavor::LIMITS_INFO,
                (&raw const limits).cast(),
                MachPortLimits::COUNT,
            )
        }
        .check()?;

        Ok(notifier)
    }

    pub fn notify(&self) -> Result<()> {
        let mut msg = MachMsgHeader {
            bits: MachMsgType::COPY_SEND.raw(),
            size: size_of::<MachMsgHeader>() as u32,
            remote_port: self.port,
            ..Default::default()
        };
        let ret = unsafe {
            mach_msg(
                &mut msg,
                MachMsgOption::SEND_MSG | MachMsgOption::SEND_TIMEOUT,
                msg.size,
                0,
                MACH_PORT_NULL,
                0,
                MACH_PORT_NULL,
            )
        };
        match ret {
            // A full queue means a notification is already pending, so there is
            // nothing to add. This is the coalescing an eventfd counter gives.
            MachRet::SUCCESS | MachRet::SEND_TIMED_OUT => Ok(()),
            ret => ret.check(),
        }
    }

    /// Consumes the pending notification.
    ///
    /// Must be called once the notifier's event has been reported, and before
    /// acting on what it signalled. Leaving the message queued keeps the port
    /// full, so every later [`Notifier::notify`] coalesces into it and no
    /// further kqueue event is delivered.
    pub fn clear(&self) -> Result<()> {
        let mut buf = MachMsgRecvBuf::new();
        let size = buf.size();
        let ret = unsafe {
            mach_msg(
                buf.as_mut_ptr(),
                MachMsgOption::RCV_MSG | MachMsgOption::RCV_TIMEOUT,
                0,
                size,
                self.port,
                0,
                MACH_PORT_NULL,
            )
        };
        match ret {
            MachRet::SUCCESS | MachRet::RCV_TIMED_OUT => Ok(()),
            ret => ret.check(),
        }
    }

    fn update_kqueue(&self, registry: &Registry, flags: u16, token: Token) -> Result<()> {
        let change = libc::kevent {
            ident: self.port as _,
            filter: libc::EVFILT_MACHPORT,
            flags: flags | libc::EV_RECEIPT,
            fflags: 0,
            data: 0,
            udata: token.0 as _,
        };
        // EV_RECEIPT makes the kernel report the outcome of the change itself
        // as a single EV_ERROR event, with data set to 0 on success.
        let mut event = change;
        ffi!(unsafe {
            libc::kevent(
                registry.as_raw_fd(),
                &change,
                1,
                &mut event,
                1,
                null::<libc::timespec>(),
            )
        })?;
        if event.flags & libc::EV_ERROR != 0 && event.data != 0 {
            return Err(Error::from_raw_os_error(event.data as i32));
        }
        Ok(())
    }
}

impl Source for Notifier {
    fn register(&mut self, registry: &Registry, token: Token, _: Interest) -> Result<()> {
        self.update_kqueue(registry, libc::EV_ADD | libc::EV_CLEAR, token)
    }

    fn reregister(&mut self, registry: &Registry, token: Token, _: Interest) -> Result<()> {
        self.update_kqueue(registry, libc::EV_ADD | libc::EV_CLEAR, token)
    }

    fn deregister(&mut self, registry: &Registry) -> Result<()> {
        self.update_kqueue(registry, libc::EV_DELETE, Token(0))
    }
}

impl Drop for Notifier {
    fn drop(&mut self) {
        let task = mach_task_self();
        // Release the send right created in `new`, then the receive right.
        let ret = unsafe { mach_port_deallocate(task, self.port) };
        if let Err(e) = ret.check() {
            log::error!("cannot deallocate mach port {:#x}: {e:?}", self.port);
        }
        let ret = unsafe { mach_port_mod_refs(task, self.port, MachPortRight::RECEIVE, -1) };
        if let Err(e) = ret.check() {
            log::error!("cannot release mach port {:#x}: {e:?}", self.port);
        }
    }
}
