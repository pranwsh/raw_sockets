//! End-to-end test: spins up the real msgd server binary and drives it
//! through the channel-based client API.

mod common;

use common::{connect_with_retry, start_server, wait_for};
use msgclient::{Action, Event};
use std::time::{Duration, Instant};

fn assert_auth_ok(client: &msgclient::Client) {
    let ev = wait_for(client, |e| matches!(e, Event::AuthOk { .. } | Event::AuthFail { .. }));
    assert!(matches!(ev, Event::AuthOk { .. }), "expected AuthOk, got {ev:?}");
}

#[test]
fn two_clients_exchange_messages() {
    let srv = start_server();

    let alice = connect_with_retry(srv.port);
    let bob = connect_with_retry(srv.port);

    alice.send(Action::Hello { user: "alice".into(), password: "pw".into() });
    bob.send(Action::Hello { user: "bob".into(), password: "pw".into() });
    assert_auth_ok(&alice);
    assert_auth_ok(&bob);

    // alice creates a conversation with bob (the client auto-prepends "alice")
    alice.send(Action::CreateConv { members: vec!["bob".into()] });
    let conv = match wait_for(&alice, |e| matches!(e, Event::ConvCreated(..))) {
        Event::ConvCreated(info) => info.id,
        other => panic!("expected ConvCreated, got {other:?}"),
    };

    // alice lists conversations — the new one must be present
    alice.send(Action::ListConvs);
    match wait_for(&alice, |e| matches!(e, Event::Convs { .. })) {
        Event::Convs { convs } => {
            let ids: Vec<Vec<u8>> = convs.into_iter().map(|c| c.id).collect();
            assert!(ids.contains(&conv), "conv missing from list");
        }
        other => panic!("expected Convs, got {other:?}"),
    }

    // alice sends a message; bob must receive it
    alice.send(Action::Send { conv: conv.clone(), text: "hello bob".into() });
    let recv = match wait_for(&bob, |e| matches!(e, Event::Message { .. })) {
        Event::Message { conv: c, from, seq, text } => (c, from, seq, text),
        other => panic!("expected Message, got {other:?}"),
    };
    assert_eq!(recv.0, conv, "wrong conversation");
    assert_eq!(recv.1, "alice", "wrong sender");
    assert_eq!(recv.3, "hello bob", "wrong text");
    assert!(recv.2 >= 1, "bad seq");

    // alice should get a Delivered for her own send
    assert!(matches!(
        wait_for(&alice, |e| matches!(e, Event::Delivered { .. })),
        Event::Delivered { .. }
    ));

    // bob replies; alice receives it
    bob.send(Action::Send { conv, text: "hi alice".into() });
    match wait_for(&alice, |e| matches!(e, Event::Message { .. })) {
        Event::Message { from, text, .. } => {
            assert_eq!(from, "bob");
            assert_eq!(text, "hi alice");
        }
        other => panic!("expected Message, got {other:?}"),
    }
}

#[test]
fn double_hello_does_not_duplicate_delivery() {
    let srv = start_server();

    let alice = connect_with_retry(srv.port);
    let bob = connect_with_retry(srv.port);

    // deliberately authenticate twice on the same connection (the old TUI
    // adapter sent Hello both at connect time and from the frontend). The
    // server must not register the connection twice, or live delivery would
    // target the same socket twice and duplicate every received message.
    alice.send(Action::Hello { user: "alice".into(), password: "pw".into() });
    alice.send(Action::Hello { user: "alice".into(), password: "pw".into() });
    bob.send(Action::Hello { user: "bob".into(), password: "pw".into() });
    assert_auth_ok(&alice);
    assert_auth_ok(&alice);
    assert_auth_ok(&bob);

    alice.send(Action::CreateConv { members: vec!["bob".into()] });
    let conv = match wait_for(&alice, |e| matches!(e, Event::ConvCreated(..))) {
        Event::ConvCreated(info) => info.id,
        other => panic!("expected ConvCreated, got {other:?}"),
    };

    // one send must produce exactly one Message on bob
    alice.send(Action::Send { conv, text: "once only".into() });
    let first = wait_for(&bob, |e| matches!(e, Event::Message { .. }));
    match first {
        Event::Message { text, .. } => assert_eq!(text, "once only"),
        other => panic!("expected Message, got {other:?}"),
    }
    // grace period: a duplicate delivery would show up as a second Message
    let deadline = Instant::now() + Duration::from_millis(500);
    let mut dups = Vec::new();
    while Instant::now() < deadline {
        for ev in bob.events().try_iter() {
            if matches!(ev, Event::Message { .. }) {
                dups.push(ev);
            }
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(dups.is_empty(), "duplicate delivery: {dups:?}");
}
