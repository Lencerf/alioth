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

use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use assert_matches::assert_matches;
use parking_lot::{Condvar, Mutex};

use crate::cpu::{Error, MpSync, State, barrier, park, request_reset, wait_parked};

fn fixture_sync() -> Arc<(Mutex<MpSync>, Condvar)> {
    let sync = MpSync {
        state: State::Running,
        fatal: false,
        count: 0,
        generation: 0,
        parked: 0,
    };
    Arc::new((Mutex::new(sync), Condvar::new()))
}

/// Spawns `n` threads waiting at the barrier for `count` vCPUs, and returns
/// after all of them wait.
fn spawn_waiters(
    sync: &Arc<(Mutex<MpSync>, Condvar)>,
    n: u16,
    count: u16,
) -> Vec<thread::JoinHandle<Result<(), Error>>> {
    let handles = (0..n)
        .map(|_| {
            let sync = sync.clone();
            // Only the last vCPU to arrive completes the barrier.
            thread::spawn(move || barrier(&sync.0, &sync.1, count, |_| unreachable!()))
        })
        .collect();
    while sync.0.lock().count < n {
        thread::yield_now();
    }
    handles
}

#[test]
fn test_barrier() {
    let sync = fixture_sync();

    // The barrier is reusable.
    for _ in 0..2 {
        let waiters = spawn_waiters(&sync, 2, 3);

        // A notification for a state change does not release the waiters.
        sync.1.notify_all();
        thread::sleep(Duration::from_millis(100));
        assert!(waiters.iter().all(|w| !w.is_finished()));

        // The last vCPU releases them.
        barrier(&sync.0, &sync.1, 3, |_| {}).unwrap();
        for waiter in waiters {
            waiter.join().unwrap().unwrap();
        }
    }
}

#[test]
fn test_barrier_complete() {
    let sync = fixture_sync();
    let waiters = spawn_waiters(&sync, 2, 3);

    barrier(&sync.0, &sync.1, 3, |s| s.state = State::Stopped).unwrap();
    for waiter in waiters {
        waiter.join().unwrap().unwrap();
    }
    assert_eq!(sync.0.lock().state, State::Stopped);
}

#[test]
fn test_barrier_fatal() {
    let sync = fixture_sync();
    let waiters = spawn_waiters(&sync, 2, 3);

    sync.0.lock().fatal = true;
    sync.1.notify_all();

    for waiter in waiters {
        assert_matches!(waiter.join().unwrap(), Err(Error::PeerFailure { .. }));
    }
    assert_matches!(
        barrier(&sync.0, &sync.1, 3, |_| {}),
        Err(Error::PeerFailure { .. })
    );
}

/// Joins `handle`, and panics if the thread does not finish in time.
fn join<T>(handle: thread::JoinHandle<T>) -> T {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !handle.is_finished() {
        assert!(Instant::now() < deadline, "the thread is stuck");
        thread::sleep(Duration::from_millis(1));
    }
    handle.join().unwrap()
}

/// Spawns `n` of `count` vCPU threads, and returns after all of them are
/// parked. The threads return the state that released them.
fn spawn_parked(
    sync: &Arc<(Mutex<MpSync>, Condvar)>,
    n: u16,
    count: u16,
) -> Vec<thread::JoinHandle<State>> {
    let parked = sync.0.lock().parked;
    let handles = (0..n)
        .map(|_| {
            let sync = sync.clone();
            thread::spawn(move || {
                let mut guard = sync.0.lock();
                park(&mut guard, &sync.1, count);
                guard.state
            })
        })
        .collect();
    while sync.0.lock().parked < parked + n {
        thread::yield_now();
    }
    handles
}

/// Spawns a thread that waits for `count` vCPUs to park, and returns the
/// state that it finds afterwards.
fn spawn_wait_parked(
    sync: &Arc<(Mutex<MpSync>, Condvar)>,
    count: u16,
) -> thread::JoinHandle<State> {
    let sync = sync.clone();
    thread::spawn(move || {
        let mut guard = sync.0.lock();
        wait_parked(&mut guard, &sync.1, count);
        guard.state
    })
}

#[test]
fn test_park() {
    let sync = fixture_sync();
    sync.0.lock().state = State::Paused;

    // The wait ends once the last vCPU is parked.
    let waiter = spawn_wait_parked(&sync, 2);
    let mut vcpus = spawn_parked(&sync, 1, 2);
    thread::sleep(Duration::from_millis(100));
    assert!(!waiter.is_finished());
    vcpus.extend(spawn_parked(&sync, 1, 2));
    assert_eq!(join(waiter), State::Paused);

    // The vCPUs stay parked if the VM is paused again before they wake up.
    let mut guard = sync.0.lock();
    guard.state = State::Running;
    sync.1.notify_all();
    guard.state = State::Paused;
    drop(guard);
    thread::sleep(Duration::from_millis(100));
    assert!(vcpus.iter().all(|v| !v.is_finished()));
    assert_eq!(sync.0.lock().parked, 2);

    sync.0.lock().state = State::Running;
    sync.1.notify_all();
    for vcpu in vcpus {
        assert_eq!(join(vcpu), State::Running);
    }
    assert_eq!(sync.0.lock().parked, 0);
}

#[test]
fn test_wait_parked_interrupted() {
    let sync = fixture_sync();
    sync.0.lock().state = State::Paused;

    // One of the two vCPUs never parks.
    let vcpus = spawn_parked(&sync, 1, 2);
    let waiter = spawn_wait_parked(&sync, 2);
    thread::sleep(Duration::from_millis(100));
    assert!(!waiter.is_finished());

    // A state change ends the wait.
    sync.0.lock().state = State::RebootPending;
    sync.1.notify_all();
    assert_eq!(join(waiter), State::RebootPending);
    for vcpu in vcpus {
        assert_eq!(join(vcpu), State::RebootPending);
    }
}

#[test]
fn test_request_reset() {
    let sync = fixture_sync();

    let mut guard = sync.0.lock();
    assert!(request_reset(&mut guard, &sync.1, State::RebootPending));
    assert_eq!(guard.state, State::RebootPending);
    // The first request wins.
    assert!(!request_reset(&mut guard, &sync.1, State::ShutdownPending));
    assert_eq!(guard.state, State::RebootPending);
}

#[test]
fn test_request_reset_while_pausing() {
    let sync = fixture_sync();
    sync.0.lock().state = State::Paused;

    // One vCPU is parked, and the VMM waits for the other one, which
    // requests a shutdown instead.
    let vcpus = spawn_parked(&sync, 1, 2);
    let waiter = spawn_wait_parked(&sync, 2);
    thread::sleep(Duration::from_millis(100));
    assert!(request_reset(
        &mut sync.0.lock(),
        &sync.1,
        State::ShutdownPending
    ));

    assert_eq!(join(waiter), State::ShutdownPending);
    for vcpu in vcpus {
        assert_eq!(join(vcpu), State::ShutdownPending);
    }
}
