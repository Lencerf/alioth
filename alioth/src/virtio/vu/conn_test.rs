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

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use assert_matches::assert_matches;
use zerocopy::{FromZeros, IntoBytes};

use crate::virtio::vu::bindings::{
    DeviceConfig, MAX_CONFIG_SIZE, Message, MessageFlag, VhostUserMmap, VirtqState, VuBackMsg,
    VuFrontMsg,
};
use crate::virtio::vu::conn::{VuChannel, VuSession};
use crate::virtio::vu::{Error, error};

/// Reads a request with its payload from `conn`, and returns its header.
fn read_request(mut conn: &UnixStream) -> Message {
    let mut msg = Message::new_zeroed();
    conn.read_exact(msg.as_mut_bytes()).unwrap();
    let mut payload = vec![0u8; msg.size as usize];
    conn.read_exact(&mut payload).unwrap();
    msg
}

#[test]
fn test_vu_session_reply_in_parts() {
    let (client_conn, mut server_conn) = UnixStream::pair().unwrap();
    let client = VuSession::from(client_conn);

    // The reply arrives in two parts, like the replies of libvhost-user.
    let server_thread = thread::spawn(move || {
        let req = read_request(&server_conn);
        let reply = Message {
            request: req.request,
            flag: MessageFlag::receiver(),
            size: 8,
        };
        server_conn.write_all(reply.as_bytes()).unwrap();
        thread::sleep(Duration::from_millis(50));
        server_conn.write_all(0xdead_beef_u64.as_bytes()).unwrap();
        server_conn
    });

    assert_matches!(client.get_features(), Ok(0xdead_beef));
    drop(server_thread.join().unwrap());
}

#[test]
fn test_vu_session_reply_without_payload() {
    let (client_conn, mut server_conn) = UnixStream::pair().unwrap();
    let client = VuSession::from(client_conn);

    // A back-end reports that it cannot read the config with an empty reply.
    let server_thread = thread::spawn(move || {
        let req = read_request(&server_conn);
        assert_eq!(req.request, VuFrontMsg::GET_CONFIG.raw());
        let reply = Message {
            request: req.request,
            flag: MessageFlag::receiver(),
            size: 0,
        };
        server_conn.write_all(reply.as_bytes()).unwrap();
        server_conn
    });

    // The session does not wait for a payload that never arrives.
    let cfg = DeviceConfig {
        offset: 0,
        size: 8,
        flags: 0,
    };
    let mut out = 0u64;
    assert_matches!(
        client.get_config(&cfg, out.as_mut_bytes()),
        Err(Error::PayloadSize {
            want: 20,
            got: 0,
            ..
        })
    );
    drop(server_thread.join().unwrap());
}

#[test]
fn test_vu_session_concurrent_requests() {
    let (client_conn, server_conn) = UnixStream::pair().unwrap();
    let client = Arc::new(VuSession::from(client_conn));
    let server = VuSession::from(server_conn);

    let server_thread = thread::spawn(move || {
        let mut fds = [const { None }; 8];
        while let Ok(msg) = server.recv_msg(&mut fds) {
            match VuFrontMsg::from(msg.request) {
                VuFrontMsg::GET_FEATURES => {
                    server
                        .reply(VuFrontMsg::GET_FEATURES, &0xdead_beef_u64, &[])
                        .unwrap();
                }
                VuFrontMsg::GET_CONFIG => {
                    let mut buf = [0u8; MAX_CONFIG_SIZE];
                    let cfg = server.recv_config(&mut buf).unwrap();
                    let val = 0x0102_0304_0506_0708_u64;
                    server
                        .reply_config(&cfg, &val.as_bytes()[..cfg.size as usize])
                        .unwrap();
                }
                VuFrontMsg::SET_STATUS => {
                    let _status: u64 = server.recv_payload().unwrap();
                    server.reply(VuFrontMsg::SET_STATUS, &0u64, &[]).unwrap();
                }
                req => panic!("unexpected request: {req:?}"),
            }
        }
    });

    let mut handles = Vec::new();
    for i in 0..4 {
        let session = client.clone();
        handles.push(thread::spawn(move || {
            for _ in 0..50 {
                if i % 2 == 0 {
                    assert_eq!(session.get_features().unwrap(), 0xdead_beef);
                    session.set_status(&0).unwrap();
                } else {
                    let req = DeviceConfig {
                        offset: 0,
                        size: 8,
                        flags: 0,
                    };
                    let mut out = 0u64;
                    session.get_config(&req, out.as_mut_bytes()).unwrap();
                    assert_eq!(out, 0x0102_0304_0506_0708);
                }
            }
        }));
    }

    for h in handles {
        h.join().unwrap();
    }
    drop(client);
    server_thread.join().unwrap();
}

#[test]
fn test_vu_session_get_virtq_base_serving() {
    let (client_conn, server_conn) = UnixStream::pair().unwrap();
    let (front_conn, back_conn) = UnixStream::pair().unwrap();
    let client = VuSession::from(client_conn);
    let front = VuChannel { conn: front_conn };
    let (closed_tx, closed_rx) = flume::bounded(1);
    // A frontend that does not answer fails the test, instead of blocking
    // it.
    let timeout = Duration::from_secs(5);

    // The back-end stops the ring once the frontend answered two requests.
    let server_thread = thread::spawn(move || {
        let server = VuSession::from(server_conn);
        let back = VuChannel { conn: back_conn };
        back.conn.set_read_timeout(Some(timeout)).unwrap();

        let msg = server.recv_msg(&mut []).unwrap();
        assert_eq!(msg.request, VuFrontMsg::GET_VIRTQ_BASE.raw());
        let mut state: VirtqState = server.recv_payload().unwrap();
        let unmap = VhostUserMmap {
            len: 0x1000,
            ..Default::default()
        };
        let answers = [back.shmem_unmap(&unmap), back.shmem_unmap(&unmap)];
        state.val = 0x1234;
        server
            .reply(VuFrontMsg::GET_VIRTQ_BASE, &state, &[])
            .unwrap();
        // A back-end may close the channel right after the reply.
        drop(back);
        closed_tx.send(()).unwrap();
        answers
    });

    let mut served = 0;
    let serve = || {
        let msg = front.recv_msg(&mut [])?;
        let _: VhostUserMmap = front.recv_payload()?;
        served += 1;
        front.reply(VuBackMsg::from(msg.request), &0u64, &[])?;
        if served == 2 {
            // When the session looks again, the reply is there, and the
            // channel is closed.
            closed_rx.recv_timeout(timeout).unwrap();
        }
        Ok(())
    };
    let state = VirtqState { index: 1, val: 0 };
    let base = client.get_virtq_base_serving(&state, &front, serve);
    assert_matches!(
        base,
        Ok(VirtqState {
            index: 1,
            val: 0x1234
        })
    );
    assert_eq!(served, 2);
    assert_matches!(server_thread.join().unwrap(), [Ok(()), Ok(())]);
}

#[test]
fn test_vu_session_get_virtq_base_serve_error() {
    let (client_conn, server_conn) = UnixStream::pair().unwrap();
    let (front_conn, mut back_conn) = UnixStream::pair().unwrap();
    let client = VuSession::from(client_conn);
    let front = VuChannel { conn: front_conn };
    let (failed_tx, failed_rx) = flume::bounded(1);
    let timeout = Duration::from_secs(5);

    // The back-end sends two requests that the frontend cannot answer, and
    // stops the ring after the frontend has failed.
    let server_thread = thread::spawn(move || {
        let server = VuSession::from(server_conn);
        let msg = server.recv_msg(&mut []).unwrap();
        assert_eq!(msg.request, VuFrontMsg::GET_VIRTQ_BASE.raw());
        let mut state: VirtqState = server.recv_payload().unwrap();
        let msg = Message {
            request: VuBackMsg::CONFIG_CHANGE_MSG.raw(),
            flag: MessageFlag::sender(),
            size: 0,
        };
        back_conn.write_all(msg.as_bytes()).unwrap();
        back_conn.write_all(msg.as_bytes()).unwrap();
        failed_rx.recv_timeout(timeout).unwrap();
        state.val = 0x1234;
        server
            .reply(VuFrontMsg::GET_VIRTQ_BASE, &state, &[])
            .unwrap();

        let msg = server.recv_msg(&mut []).unwrap();
        assert_eq!(msg.request, VuFrontMsg::GET_FEATURES.raw());
        server
            .reply(VuFrontMsg::GET_FEATURES, &0xdead_beef_u64, &[])
            .unwrap();
        back_conn
    });

    let mut served = 0;
    let serve = || {
        let msg = front.recv_msg(&mut [])?;
        served += 1;
        failed_tx.send(()).unwrap();
        error::UnsupportedReq { req: msg.request }.fail()
    };
    let state = VirtqState { index: 0, val: 0 };
    assert_matches!(
        client.get_virtq_base_serving(&state, &front, serve),
        Err(Error::UnsupportedReq { req: 2, .. })
    );
    // The session does not go on with a channel that it cannot answer.
    assert_eq!(served, 1);
    // The session took the reply, so the next request gets its own.
    assert_matches!(client.get_features(), Ok(0xdead_beef));
    drop(server_thread.join().unwrap());
}
