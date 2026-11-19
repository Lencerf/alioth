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

use std::io::{ErrorKind, Read, Write};
use std::os::fd::{AsFd, AsRawFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::thread::JoinHandle;
use std::time::Duration;
use std::{slice, thread};

use assert_matches::assert_matches;
use libc::PROT_NONE;
use mio::unix::SourceFd;
use mio::{Interest, Poll, Token};
use rstest::rstest;
use tempfile::TempDir;
use zerocopy::IntoBytes;

use crate::mem::mapped::ArcMemPages;
use crate::sync::notifier::Notifier;
use crate::virtio::dev::Virtio;
use crate::virtio::dev::fs::vu::{VuFs, VuFsSpec, handle_backend_req};
use crate::virtio::queue::Queue;
use crate::virtio::queue::split::SplitQueue;
use crate::virtio::tests::FakeIrqSender;
use crate::virtio::vu::Error as VuError;
use crate::virtio::vu::bindings::{
    Message, MessageFlag, VhostUserMmap, VirtqState, VuBackMsg, VuFeature, VuFrontMsg,
};
use crate::virtio::vu::conn::{VuChannel, VuSession};
use crate::virtio::worker::mio::VirtioMio;
use crate::virtio::{Result, VirtioFeature};

const WINDOW_SIZE: u64 = 0x1000;

fn fixture_window() -> ArcMemPages {
    ArcMemPages::from_anonymous(WINDOW_SIZE as usize, Some(PROT_NONE), None).unwrap()
}

/// Returns the channel of the frontend and that of the backend.
fn fixture_channels() -> (VuChannel, VuChannel) {
    let (front, back) = UnixStream::pair().unwrap();
    (VuChannel { conn: front }, VuChannel { conn: back })
}

/// Receives a request from the backend and handles it.
fn handle_req(front: &VuChannel, window: Option<&ArcMemPages>) -> Result<(), VuError> {
    let mut fd = None;
    let msg = front.recv_msg(slice::from_mut(&mut fd)).unwrap();
    handle_backend_req("test", front, window, &msg, fd)
}

#[rstest]
#[case(0, true)]
#[case(WINDOW_SIZE, false)]
fn shmem_map(#[case] offset: u64, #[case] valid: bool) {
    let window = fixture_window();
    let (front, back) = fixture_channels();
    let file = tempfile::tempfile().unwrap();
    file.set_len(WINDOW_SIZE).unwrap();

    let payload = VhostUserMmap {
        shm_offset: offset,
        len: WINDOW_SIZE,
        ..Default::default()
    };
    let backend = thread::spawn(move || back.shmem_map(&payload, file.as_fd()));
    // A failed request does not stop the frontend.
    assert_matches!(handle_req(&front, Some(&window)), Ok(()));
    let ret = backend.join().unwrap();
    if valid {
        assert_matches!(ret, Ok(()));
    } else {
        assert_matches!(ret, Err(VuError::RequestErr { ret: u64::MAX, .. }));
    }
}

#[test]
fn unsupported_req() {
    let window = fixture_window();
    let (front, back) = fixture_channels();

    let msg = Message {
        request: VuBackMsg::CONFIG_CHANGE_MSG.raw(),
        flag: MessageFlag::sender(),
        size: 0,
    };
    (&back.conn).write_all(msg.as_bytes()).unwrap();
    assert_matches!(
        handle_req(&front, Some(&window)),
        Err(VuError::UnsupportedReq { req: 2, .. })
    );
}

#[test]
fn no_dax_window() {
    let (front, back) = fixture_channels();

    let msg = Message {
        request: VuBackMsg::SHMEM_UNMAP.raw(),
        flag: MessageFlag::sender(),
        size: size_of::<VhostUserMmap>() as u32,
    };
    (&back.conn).write_all(msg.as_bytes()).unwrap();
    // A device without a DAX window expects no request.
    assert_matches!(
        handle_req(&front, None),
        Err(VuError::ProtocolFeature { .. })
    );
}

#[test]
fn no_reply() {
    let window = fixture_window();
    let (front, back) = fixture_channels();

    // The backend does not ask for a reply.
    let msg = Message {
        request: VuBackMsg::SHMEM_UNMAP.raw(),
        flag: MessageFlag(MessageFlag::VERSION_1),
        size: size_of::<VhostUserMmap>() as u32,
    };
    let payload = VhostUserMmap {
        len: WINDOW_SIZE,
        ..Default::default()
    };
    (&back.conn).write_all(msg.as_bytes()).unwrap();
    (&back.conn).write_all(payload.as_bytes()).unwrap();
    assert_matches!(handle_req(&front, Some(&window)), Ok(()));

    back.conn.set_nonblocking(true).unwrap();
    let mut buf = [0u8; 1];
    assert_matches!(
        (&back.conn).read(&mut buf),
        Err(e) if e.kind() == ErrorKind::WouldBlock
    );
}

/// What a scripted back-end got from the frontend.
#[derive(Debug, Default)]
struct Recorded {
    /// The requests about the rings, with the ring index of each.
    requests: Vec<(VuFrontMsg, u64)>,
    /// The answer to the mapping that the back-end asked for before it
    /// stopped ring 0.
    mapped: Option<Result<(), VuError>>,
}

/// Spawns a back-end with two rings that serves one frontend.
///
/// With a back-end channel, it stops ring 0 only after the frontend has
/// mapped a file into the DAX window, like a back-end with a request in
/// flight that needs the mapping.
fn spawn_backend(listener: UnixListener) -> JoinHandle<Recorded> {
    thread::spawn(move || {
        let (conn, _) = listener.accept().unwrap();
        let backend = VuSession::from(conn);
        let mut fds = [const { None }; 8];
        let mut channel = None;
        let mut recorded = Recorded::default();
        while let Ok(msg) = backend.recv_msg(&mut fds) {
            let req = VuFrontMsg::from(msg.request);
            let ret = match req {
                VuFrontMsg::GET_FEATURES => {
                    (VirtioFeature::VHOST_PROTOCOL | VirtioFeature::VERSION_1).bits() as u64
                }
                VuFrontMsg::GET_PROTOCOL_FEATURES => {
                    let feature = VuFeature::MQ
                        | VuFeature::REPLY_ACK
                        | VuFeature::CONFIGURE_MEM_SLOTS
                        | VuFeature::BACKEND_REQ
                        | VuFeature::BACKEND_SEND_FD
                        | VuFeature::SHMEM;
                    feature.bits()
                }
                VuFrontMsg::SET_PROTOCOL_FEATURES => {
                    let _: u64 = backend.recv_payload().unwrap();
                    0
                }
                VuFrontMsg::GET_QUEUE_NUM => 2,
                VuFrontMsg::SET_OWNER => 0,
                VuFrontMsg::SET_BACKEND_REQ_FD => {
                    let conn = UnixStream::from(fds[0].take().unwrap());
                    // A frontend that does not answer fails the test,
                    // instead of blocking it.
                    let timeout = Duration::from_secs(5);
                    conn.set_read_timeout(Some(timeout)).unwrap();
                    channel = Some(VuChannel { conn });
                    0
                }
                VuFrontMsg::SET_VIRTQ_ENABLE => {
                    let state: VirtqState = backend.recv_payload().unwrap();
                    recorded.requests.push((req, state.index as u64));
                    0
                }
                VuFrontMsg::SET_VIRTQ_KICK => {
                    let index: u64 = backend.recv_payload().unwrap();
                    recorded.requests.push((req, index));
                    0
                }
                VuFrontMsg::GET_VIRTQ_BASE => {
                    let state: VirtqState = backend.recv_payload().unwrap();
                    recorded.requests.push((req, state.index as u64));
                    if let Some(channel) = &channel
                        && state.index == 0
                    {
                        let file = tempfile::tempfile().unwrap();
                        file.set_len(WINDOW_SIZE).unwrap();
                        let payload = VhostUserMmap {
                            len: WINDOW_SIZE,
                            ..Default::default()
                        };
                        recorded.mapped = Some(channel.shmem_map(&payload, file.as_fd()));
                    }
                    backend.reply(req, &state, &[]).unwrap();
                    continue;
                }
                req => panic!("unexpected request: {req:?}"),
            };
            backend.reply(req, &ret, &[]).unwrap();
        }
        recorded
    })
}

/// Creates a device, and a back-end for it on a socket in `temp_dir`.
fn fixture_fs(temp_dir: &TempDir, dax_window: usize) -> (VuFs, JoinHandle<Recorded>) {
    let socket = temp_dir.path().join("vu.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let backend = spawn_backend(listener);
    let spec = VuFsSpec {
        socket: socket.into(),
        tag: Some("share".to_owned()),
        dax_window,
    };
    (VuFs::new(spec, "vu-fs").unwrap(), backend)
}

#[test]
fn reset_answers_backend() {
    let temp_dir = TempDir::new().unwrap();
    let (mut fs, backend) = fixture_fs(&temp_dir, WINDOW_SIZE as usize);

    // The worker watches the back-end channel of an active device.
    let poll = Poll::new().unwrap();
    let channel_fd = fs.frontend.channel().unwrap().conn.as_raw_fd();
    let registry = poll.registry();
    registry
        .register(&mut SourceFd(&channel_fd), Token(2), Interest::READABLE)
        .unwrap();
    fs.reset(registry);
    drop(fs);

    // The back-end got the mapping without which it does not stop ring 0.
    let recorded = backend.join().unwrap();
    assert_matches!(recorded.mapped, Some(Ok(())));
    assert_eq!(
        recorded.requests,
        [
            (VuFrontMsg::GET_VIRTQ_BASE, 0),
            (VuFrontMsg::SET_VIRTQ_ENABLE, 0),
            (VuFrontMsg::GET_VIRTQ_BASE, 1),
            (VuFrontMsg::SET_VIRTQ_ENABLE, 1),
        ]
    );
}

#[rstest]
#[case(0)]
#[case(WINDOW_SIZE as usize)]
fn pause_resume(#[case] dax_window: usize) {
    let temp_dir = TempDir::new().unwrap();
    let (mut fs, backend) = fixture_fs(&temp_dir, dax_window);

    let (q_tx, _q_rx) = flume::unbounded();
    let irq_sender = FakeIrqSender { q_tx };
    let notifiers = [Notifier::new().unwrap(), Notifier::new().unwrap()];
    // The driver enabled no queue.
    let queues: [Option<Queue<SplitQueue>>; 2] = [None, None];
    assert_matches!(fs.pause(), Ok(()));
    assert_matches!(fs.resume(&queues, &notifiers, &irq_sender), Ok(()));
    drop(fs);

    // The back-end stops the rings, and starts them again.
    let recorded = backend.join().unwrap();
    assert_eq!(
        recorded.requests,
        [
            (VuFrontMsg::GET_VIRTQ_BASE, 0),
            (VuFrontMsg::GET_VIRTQ_BASE, 1),
            (VuFrontMsg::SET_VIRTQ_KICK, 0),
            (VuFrontMsg::SET_VIRTQ_KICK, 1),
        ]
    );
    // With a DAX window, the back-end got the mapping without which it
    // does not stop ring 0.
    let mapped = recorded.mapped.map(|answer| answer.is_ok());
    assert_eq!(mapped, (dax_window > 0).then_some(true));
}
