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

//! Tokio-based async backend for virtio devices.
//!
//! This is an alternative to `Mio` that uses a single-threaded tokio runtime
//! instead of a raw epoll/kqueue event loop. It's particularly useful for
//! devices that manage many concurrent connections (e.g. vsock).
//!
//! ## Architecture
//!
//! ```text
//! ┌─────────────────┐     tokio::sync::Notify      ┌──────────────────────┐
//! │  VCPU thread     │ ──────────────────────────▶  │  Device thread       │
//! │  (non-tokio)     │                              │  (tokio runtime)     │
//! │                  │ ◀──────────────────────────  │                      │
//! │  event_tx.send() │        flume channel         │  block_on(main_loop) │
//! └─────────────────┘                              └──────────────────────┘
//! ```
//!
//! Key design decisions:
//! - **Single-threaded runtime**: each virtio device gets its own
//!   `tokio::runtime::Runtime` on a dedicated OS thread, matching the existing
//!   1:1 thread-per-device model.
//! - **Guest memory is NOT 'static**: virtqueue descriptors borrow `&'m Ram`,
//!   so we cannot spawn them into tokio tasks. Queue processing happens
//!   synchronously in the main event loop. Only socket I/O is async.
//! - **`flume::recv_async()` for cross-thread wakeup**: the VCPU thread
//!   sends via `event_tx.send()` and flume's internal waker mechanism
//!   wakes the tokio task. No extra fd needed.

use std::sync::Arc;
use std::thread::JoinHandle;

use flume::Receiver;
use tokio::runtime::{Builder, Runtime};

use crate::hv::IoeventFd;
use crate::mem::mapped::{Ram, RamBus};
use crate::sync::notifier::Notifier;
use crate::virtio::dev::{Backend, Context, StartParam, Virtio, WakeEvent, Worker, WorkerState};
use crate::virtio::queue::{Queue, QueueReg, VirtQueue};
use std::os::fd::AsRawFd;
use tokio::io::unix::AsyncFd;
use snafu::ResultExt;
use crate::virtio::{IrqSender, Result, error};

// ---------------------------------------------------------------------------
// VirtioTokio trait — what devices must implement
// ---------------------------------------------------------------------------

/// Trait for virtio devices that want to use the Tokio async backend.
///
/// Compare with `VirtioMio`. The key differences:
/// - `handle_queue` and `handle_event` are `async fn`, so devices can `.await`
///   socket I/O directly.
/// - `ActiveTokio` provides a `tokio::runtime::Handle` for spawning background
///   tasks (e.g. per-connection handlers).
#[allow(async_fn_in_trait)]
pub trait VirtioTokio: Virtio {
    /// Called once when the device is activated (guest writes DRIVER_OK).
    fn activate<'m, Q, S, E>(
        &mut self,
        feature: u128,
        active: &mut ActiveTokio<'_, '_, 'm, Q, S, E>,
    ) -> Result<()>
    where
        Q: VirtQueue<'m>,
        S: IrqSender,
        E: IoeventFd;

    /// Handle a queue notification (guest kicked a virtqueue).
    async fn handle_queue<'m, Q, S, E>(
        &mut self,
        index: u16,
        active: &mut ActiveTokio<'_, '_, 'm, Q, S, E>,
    ) -> Result<()>
    where
        Q: VirtQueue<'m>,
        S: IrqSender,
        E: IoeventFd;

    /// Handle a device-specific I/O event (e.g. tap fd readable).
    ///
    /// This is called when an fd registered via `register_io` becomes ready.
    /// For simple devices like virtio-net, this replaces the mio event callback.
    async fn handle_io<'m, Q, S, E>(
        &mut self,
        token: u64,
        active: &mut ActiveTokio<'_, '_, 'm, Q, S, E>,
    ) -> Result<()>
    where
        Q: VirtQueue<'m>,
        S: IrqSender,
        E: IoeventFd;

    /// Called on device reset. Deregister any registered I/O sources.
    fn reset(&mut self);
}

// ---------------------------------------------------------------------------
// ActiveTokio — the "active backend" passed to device methods
// ---------------------------------------------------------------------------

pub struct ActiveTokio<'a, 'r, 'm, Q, S, E>
where
    Q: VirtQueue<'m>,
{
    /// All virtqueues, indexed by queue number.
    pub queues: &'a mut [Option<Queue<'r, 'm, Q>>],
    /// Sender for injecting interrupts into the guest.
    pub irq_sender: &'a S,
    /// Ioeventfds for queue notifications. Index = queue index.
    pub ioeventfds: &'a [E],
    /// Handle to the tokio runtime, for spawning background tasks.
    pub rt_handle: &'a tokio::runtime::Handle,
    /// Guest physical memory view.
    pub mem: &'m Ram,
    /// Sender for device-specific wakeups.
    pub wakeup_tx: tokio::sync::mpsc::Sender<u16>,
}

// ---------------------------------------------------------------------------
// Tokio backend: implements the Backend trait, owns the tokio Runtime
// ---------------------------------------------------------------------------

#[allow(dead_code)]
const TOKEN_QUEUE: u64 = 1 << 62;

pub struct TokioBackend {
    rt: Runtime,
}

impl TokioBackend {
    /// Spawn a device worker thread backed by a single-threaded tokio runtime.
    pub fn spawn_worker<D, S, E>(
        dev: D,
        event_rx: Receiver<WakeEvent<S, E>>,
        memory: Arc<RamBus>,
        queue_regs: Arc<[QueueReg]>,
    ) -> Result<(JoinHandle<()>, Option<Arc<Notifier>>)>
    where
        D: VirtioTokio,
        S: IrqSender,
        E: IoeventFd,
    {
        let backend = TokioBackend {
            rt: Builder::new_current_thread().enable_io().build()?,
        };

        Worker::spawn(dev, backend, event_rx, memory, queue_regs)
    }
}

impl<D> Backend<D> for TokioBackend
where
    D: VirtioTokio,
{
    fn register_notifier(&mut self, _token: u64) -> Result<Option<Arc<Notifier>>> {
        // Tokio doesn't need a Notifier. The event loop uses
        // event_rx.recv_async() which is woken by flume's internal
        // waker mechanism without any extra fd.
        Ok(None)
    }

    fn reset(&self, dev: &mut D) -> Result<()> {
        dev.reset();
        Ok(())
    }

    fn event_loop<'m, S, Q, E>(
        &mut self,
        memory: &'m Ram,
        context: &mut Context<D, S, E>,
        queues: &mut [Option<Queue<'_, 'm, Q>>],
        param: &StartParam<S, E>,
    ) -> Result<()>
    where
        S: IrqSender,
        Q: VirtQueue<'m>,
        E: IoeventFd,
    {
        let event_rx = context.event_rx.clone();
        let (wakeup_tx, mut wakeup_rx) = tokio::sync::mpsc::channel::<u16>(100);

        let mut active = ActiveTokio {
            queues,
            irq_sender: &*param.irq_sender,
            ioeventfds: param.ioeventfds.as_deref().unwrap_or(&[]),
            rt_handle: self.rt.handle(),
            mem: memory,
            wakeup_tx,
        };

        self.rt.block_on(async {
            let mut io_tasks = Vec::new();
            for (index, fd) in param.ioeventfds.as_deref().unwrap_or(&[]).iter().enumerate() {
                if context.dev.ioeventfd_offloaded(index as u16)? {
                    continue;
                }
                let raw_fd = fd.as_fd().as_raw_fd();
                unsafe {
                    let flags = libc::fcntl(raw_fd, libc::F_GETFL, 0);
                    libc::fcntl(raw_fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
                }
                let async_fd = AsyncFd::new(raw_fd).context(error::EventSource)?;
                let wakeup_tx = active.wakeup_tx.clone();
                let index = index as u16;
                let task = tokio::spawn(async move {
                    loop {
                        match async_fd.readable().await {
                            Ok(mut guard) => {
                                let mut buf = [0u8; 8];
                                let ret = unsafe {
                                    libc::read(raw_fd, buf.as_mut_ptr() as *mut libc::c_void, 8)
                                };
                                if ret < 0 {
                                    let err = std::io::Error::last_os_error();
                                    if err.kind() == std::io::ErrorKind::WouldBlock {
                                        guard.clear_ready();
                                        continue;
                                    }
                                    log::error!("Failed to read ioeventfd {index}: {err:?}");
                                    break;
                                }
                                guard.clear_ready();
                                if wakeup_tx.send(index).await.is_err() {
                                    break;
                                }
                            }
                            Err(e) => {
                                log::error!("AsyncFd readable error on ioeventfd {index}: {e:?}");
                                break;
                            }
                        }
                    }
                });
                io_tasks.push(task);
            }

            struct TasksGuard(Vec<tokio::task::JoinHandle<()>>);
            impl Drop for TasksGuard {
                fn drop(&mut self) {
                    for task in &self.0 {
                        task.abort();
                    }
                }
            }
            let _guard = TasksGuard(io_tasks);

            context.dev.activate(param.feature, &mut active)?;

            loop {
                tokio::select! {
                    event = event_rx.recv_async() => {
                        match event {
                            Ok(WakeEvent::Notify { q_index }) => {
                                context.dev.handle_queue(q_index, &mut active).await?;
                            }
                            Ok(WakeEvent::Shutdown) => {
                                context.state = WorkerState::Shutdown;
                                return Ok(());
                            }
                            Ok(WakeEvent::Reset) => {
                                context.state = WorkerState::Pending;
                                return Ok(());
                            }
                            Ok(WakeEvent::Start { .. }) => {
                                log::error!("{}: already started", context.dev.name());
                            }
                            #[cfg(target_os = "linux")]
                            Ok(WakeEvent::VuChannel { channel }) => {
                                context.dev.set_vu_channel(channel);
                            }
                            Err(_) => {
                                // Channel closed
                                context.state = WorkerState::Shutdown;
                                return Ok(());
                            }
                        }
                    }
                    Some(q_index) = wakeup_rx.recv() => {
                        log::trace!("{}: wakeup queue {}", context.dev.name(), q_index);
                        context.dev.handle_queue(q_index, &mut active).await?;
                    }
                }
            }
        })
    }
}
