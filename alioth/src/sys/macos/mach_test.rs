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

use std::io::ErrorKind;

use super::{
    MACH_PORT_NULL, MachMsgHeader, MachMsgOption, MachMsgRecvBuf, MachMsgType, MachPort,
    MachPortFlavor, MachPortLimits, MachPortRight, MachRet, mach_msg, mach_port_allocate,
    mach_port_deallocate, mach_port_insert_right, mach_port_mod_refs, mach_port_set_attributes,
    mach_task_self,
};

/// Allocates a receive right with a send right under the same name, limited to
/// a single queued message.
fn alloc_port() -> MachPort {
    let task = mach_task_self();
    let mut port = MACH_PORT_NULL;
    unsafe { mach_port_allocate(task, MachPortRight::RECEIVE, &mut port) }
        .check()
        .unwrap();
    unsafe { mach_port_insert_right(task, port, port, MachMsgType::MAKE_SEND) }
        .check()
        .unwrap();
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
    .check()
    .unwrap();
    port
}

fn free_port(port: MachPort) {
    let task = mach_task_self();
    unsafe { mach_port_deallocate(task, port) }.check().unwrap();
    unsafe { mach_port_mod_refs(task, port, MachPortRight::RECEIVE, -1) }
        .check()
        .unwrap();
}

fn send(port: MachPort) -> MachRet {
    let mut msg = MachMsgHeader {
        bits: MachMsgType::COPY_SEND.raw(),
        size: size_of::<MachMsgHeader>() as u32,
        remote_port: port,
        ..Default::default()
    };
    unsafe {
        mach_msg(
            &mut msg,
            MachMsgOption::SEND_MSG | MachMsgOption::SEND_TIMEOUT,
            msg.size,
            0,
            MACH_PORT_NULL,
            0,
            MACH_PORT_NULL,
        )
    }
}

fn recv(port: MachPort) -> MachRet {
    let mut buf = MachMsgRecvBuf::new();
    let size = buf.size();
    unsafe {
        mach_msg(
            buf.as_mut_ptr(),
            MachMsgOption::RCV_MSG | MachMsgOption::RCV_TIMEOUT,
            0,
            size,
            port,
            0,
            MACH_PORT_NULL,
        )
    }
}

#[test]
fn test_mach_port_send_recv() {
    let port = alloc_port();
    assert_eq!(send(port), MachRet::SUCCESS);
    assert_eq!(recv(port), MachRet::SUCCESS);
    // Nothing left in the queue.
    assert_eq!(recv(port), MachRet::RCV_TIMED_OUT);
    free_port(port);
}

/// With `qlimit == 1` a second send finds the queue full and returns
/// immediately instead of blocking, which is what lets a notification coalesce.
#[test]
fn test_mach_port_coalesce() {
    let port = alloc_port();
    assert_eq!(send(port), MachRet::SUCCESS);
    for _ in 0..16 {
        assert_eq!(send(port), MachRet::SEND_TIMED_OUT);
    }
    assert_eq!(recv(port), MachRet::SUCCESS);
    assert_eq!(recv(port), MachRet::RCV_TIMED_OUT);
    // Draining re-opens the queue for one more notification.
    assert_eq!(send(port), MachRet::SUCCESS);
    assert_eq!(recv(port), MachRet::SUCCESS);
    free_port(port);
}

#[test]
fn test_mach_ret_check() {
    assert!(MachRet::SUCCESS.check().is_ok());

    let e = MachRet::SEND_TIMED_OUT.check().unwrap_err();
    assert_eq!(e.kind(), ErrorKind::WouldBlock);
    assert_eq!(e.to_string(), "SEND_TIMED_OUT (0x10000004)");

    let e = MachRet::INVALID_NAME.check().unwrap_err();
    assert_eq!(e.kind(), ErrorKind::NotFound);
}
