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

use std::sync::Arc;
use std::time::Duration;

use assert_matches::assert_matches;
use mio::{Events, Interest, Poll, Token};

use super::Notifier;

#[test]
fn test_notifier() {
    let mut n1 = Notifier::new().unwrap();
    let mut n2 = Notifier::new().unwrap();

    let mut poll = Poll::new().unwrap();
    poll.registry()
        .register(&mut n1, Token(1), Interest::READABLE)
        .unwrap();
    poll.registry()
        .register(&mut n2, Token(2), Interest::READABLE)
        .unwrap();

    n1.notify().unwrap();
    n2.notify().unwrap();

    let mut events = Events::with_capacity(8);
    poll.poll(&mut events, None).unwrap();

    let tokens: Vec<_> = events.iter().map(|e| e.token()).collect();
    assert_matches!(&tokens[..], [Token(1), Token(2)]);

    n1.clear().unwrap();
    n2.clear().unwrap();

    poll.registry().deregister(&mut n1).unwrap();
    poll.registry().deregister(&mut n2).unwrap();
}

/// A notification raised before the notifier joins a poll must not be lost.
#[test]
fn test_notify_before_register() {
    let mut n = Notifier::new().unwrap();
    n.notify().unwrap();

    let mut poll = Poll::new().unwrap();
    poll.registry()
        .register(&mut n, Token(7), Interest::READABLE)
        .unwrap();

    let mut events = Events::with_capacity(8);
    poll.poll(&mut events, Some(Duration::from_secs(5))).unwrap();

    let tokens: Vec<_> = events.iter().map(|e| e.token()).collect();
    assert_matches!(&tokens[..], [Token(7)]);
}

/// Repeated notifications coalesce, and the notifier re-arms once cleared.
#[test]
fn test_notify_coalesce_and_rearm() {
    let mut n = Notifier::new().unwrap();
    let mut poll = Poll::new().unwrap();
    poll.registry()
        .register(&mut n, Token(3), Interest::READABLE)
        .unwrap();

    let mut events = Events::with_capacity(8);
    for _ in 0..4 {
        // Far more notifications than wakeups; none of these may block.
        for _ in 0..32 {
            n.notify().unwrap();
        }
        poll.poll(&mut events, Some(Duration::from_secs(5))).unwrap();
        let tokens: Vec<_> = events.iter().map(|e| e.token()).collect();
        assert_matches!(&tokens[..], [Token(3)]);
        n.clear().unwrap();
    }
}

#[test]
fn test_notify_from_another_thread() {
    let mut n = Notifier::new().unwrap();
    let mut poll = Poll::new().unwrap();
    poll.registry()
        .register(&mut n, Token(5), Interest::READABLE)
        .unwrap();

    let n = Arc::new(n);
    let sender = n.clone();
    let handle = std::thread::spawn(move || sender.notify().unwrap());

    let mut events = Events::with_capacity(8);
    poll.poll(&mut events, Some(Duration::from_secs(5))).unwrap();
    let tokens: Vec<_> = events.iter().map(|e| e.token()).collect();
    assert_matches!(&tokens[..], [Token(5)]);

    handle.join().unwrap();
    n.clear().unwrap();
}
