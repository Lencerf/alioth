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

#[cfg(target_os = "linux")]
use std::os::fd::{AsFd, AsRawFd};
use std::sync::Arc;
use std::thread::JoinHandle;

use mio::event::Event;
#[cfg(target_os = "linux")]
use mio::unix::SourceFd;
use mio::{Events, Interest, Poll, Registry, Token};
use snafu::ResultExt;

use crate::sync::notifier::Notifier;
use crate::virtio::dev::{
    ActiveBackend, Backend, BackendEvent, Context, StartParam, Virtio, Worker, WorkerParam,
    WorkerState,
};
use crate::virtio::queue::{Queue, VirtQueue};
use crate::virtio::{IrqSender, Result, error};

pub trait VirtioMio: Virtio {
    fn activate<Q, S>(&mut self, feature: u128, active_mio: &mut ActiveMio<'_, Q, S>) -> Result<()>
    where
        Q: VirtQueue,
        S: IrqSender;

    fn handle_queue<Q, S>(
        &mut self,
        index: u16,
        active_mio: &mut ActiveMio<'_, Q, S>,
    ) -> Result<()>
    where
        Q: VirtQueue,
        S: IrqSender;

    fn handle_event<Q, S>(
        &mut self,
        event: &Event,
        active_mio: &mut ActiveMio<'_, Q, S>,
    ) -> Result<()>
    where
        Q: VirtQueue,
        S: IrqSender;

    fn reset(&mut self, registry: &Registry);
}

impl BackendEvent for Event {
    fn token(&self) -> u64 {
        self.token().0 as u64
    }
}

const TOKEN_QUEUE: u64 = 1 << 62;

pub struct Mio {
    poll: Poll,
}

impl Mio {
    pub fn spawn_worker<D, S>(
        dev: D,
        param: WorkerParam<S>,
    ) -> Result<(JoinHandle<()>, Arc<Notifier>)>
    where
        D: VirtioMio,
        S: IrqSender,
    {
        let poll = Poll::new().context(error::CreatePoll)?;
        let m = Mio { poll };
        Worker::spawn(dev, m, param)
    }
}

impl<D> Backend<D> for Mio
where
    D: VirtioMio,
{
    fn register_notifier(&mut self, token: u64) -> Result<Arc<Notifier>> {
        let mut notifier = Notifier::new()?;
        let registry = self.poll.registry();
        registry.register(&mut notifier, Token(token as usize), Interest::READABLE)?;
        Ok(Arc::new(notifier))
    }

    fn reset(&self, dev: &mut D) -> Result<()> {
        dev.reset(self.poll.registry());
        Ok(())
    }

    fn event_loop<S, Q>(
        &mut self,
        context: &mut Context<D, S>,
        queues: &mut [Option<Queue<Q>>],
        param: &StartParam<S>,
    ) -> Result<()>
    where
        S: IrqSender,
        Q: VirtQueue,
    {
        let mut active_mio = ActiveMio {
            queues,
            irq_sender: &*param.irq_sender,
            notifiers: param.notifiers.clone().unwrap_or_default(),
            poll: &mut self.poll,
        };
        let ret = active_mio.run(context, param.feature);
        // The poll outlives the event loop. The next loop cannot wait for a
        // notifier that is still in the poll, so take the notifiers out
        // even if the loop failed.
        ret.and(active_mio.deregister_notifiers(&context.dev))
    }
}

pub struct ActiveMio<'a, Q, S>
where
    Q: VirtQueue,
{
    pub queues: &'a mut [Option<Queue<Q>>],
    pub irq_sender: &'a S,
    /// The notifiers of the queues. The event loop waits on those of the
    /// queues that the worker serves itself.
    pub notifiers: Arc<[Notifier]>,
    pub poll: &'a mut Poll,
}

impl<Q, S> ActiveMio<'_, Q, S>
where
    Q: VirtQueue,
    S: IrqSender,
{
    /// Activates the device of `context`, and handles its events until the
    /// device is reset or the worker shuts down.
    fn run<D>(&mut self, context: &mut Context<D, S>, feature: u128) -> Result<()>
    where
        D: VirtioMio,
    {
        context.dev.activate(feature, self)?;
        // The driver may have notified the queues since it set DRIVER_OK.
        context.dev.notify_offloaded(&self.notifiers)?;
        self.register_notifiers(&context.dev)?;
        let mut events = Events::with_capacity(128);
        loop {
            self.poll
                .poll(&mut events, None)
                .context(error::PollEvents)?;
            for event in events.iter() {
                context.handle_event(event, self)?;
                if context.state != WorkerState::Running {
                    return Ok(());
                }
            }
        }
    }

    /// Waits for the notifications of the queues that the worker serves
    /// itself.
    #[cfg(target_os = "linux")]
    fn register_notifiers<D>(&self, dev: &D) -> Result<()>
    where
        D: Virtio,
    {
        for (index, notifier) in self.notifiers.iter().enumerate() {
            if dev.notifier_offloaded(index as u16)? {
                continue;
            }
            let token = index as u64 | TOKEN_QUEUE;
            let registry = self.poll.registry();
            registry
                .register(
                    &mut SourceFd(&notifier.as_fd().as_raw_fd()),
                    Token(token as usize),
                    Interest::READABLE,
                )
                .context(error::EventSource)?;
        }
        Ok(())
    }

    /// Stops waiting for the notifications of the queues. It goes on after
    /// a failure, and returns the first one.
    #[cfg(target_os = "linux")]
    fn deregister_notifiers<D>(&self, dev: &D) -> Result<()>
    where
        D: Virtio,
    {
        let mut ret = Ok(());
        for (index, notifier) in self.notifiers.iter().enumerate() {
            if matches!(dev.notifier_offloaded(index as u16), Ok(true)) {
                continue;
            }
            let registry = self.poll.registry();
            let deregistered = registry.deregister(&mut SourceFd(&notifier.as_fd().as_raw_fd()));
            ret = ret.and(deregistered.context(error::EventSource));
        }
        ret
    }

    /// Only KVM offers ioeventfds, see [`crate::hv::NotifierRegistry`].
    #[cfg(not(target_os = "linux"))]
    fn register_notifiers<D>(&self, _: &D) -> Result<()> {
        Ok(())
    }

    #[cfg(not(target_os = "linux"))]
    fn deregister_notifiers<D>(&self, _: &D) -> Result<()> {
        Ok(())
    }
}

impl<D, Q, S> ActiveBackend<D> for ActiveMio<'_, Q, S>
where
    D: VirtioMio,
    Q: VirtQueue,
    S: IrqSender,
{
    type Event = Event;
    type Q = Q;

    fn queues(&mut self) -> &mut [Option<Queue<Q>>] {
        self.queues
    }

    fn handle_event(&mut self, dev: &mut D, event: &Self::Event) -> Result<()> {
        let token = event.token().0 as u64;
        if token & TOKEN_QUEUE == TOKEN_QUEUE {
            dev.handle_queue(token as u16, self)
        } else {
            dev.handle_event(event, self)
        }
    }

    fn handle_queue(&mut self, dev: &mut D, index: u16) -> Result<()> {
        dev.handle_queue(index, self)
    }

    fn pause(&mut self, _dev: &mut D) -> Result<bool> {
        // A device completes its requests while it handles an event, so
        // none is in flight between two events.
        Ok(true)
    }

    fn resume(&mut self, dev: &mut D) -> Result<()> {
        // Whatever serves the offloaded queues was stopped while the
        // device was paused, like before the device was activated.
        dev.notify_offloaded(&self.notifiers)
    }

    fn resume_dev(&mut self, dev: &mut D) -> Result<()> {
        dev.resume(self.queues, &self.notifiers, self.irq_sender)
    }

    fn set_notifiers(&mut self, dev: &D, notifiers: Arc<[Notifier]>) -> Result<()> {
        self.deregister_notifiers(dev)?;
        self.notifiers = notifiers;
        self.register_notifiers(dev)
    }
}
