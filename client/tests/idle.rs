//! Regression test: an idle client must NOT be disconnected (the former 60s
//! idle timeout is no longer enforced). Holds a connection silent for 65s,
//! then verifies it can still send/receive.

mod common;

use common::{connect_with_retry, start_server, wait_for};
use msgclient::{Action, Event};
use std::time::Duration;

const IDLE: Duration = Duration::from_secs(65);

#[test]
fn idle_client_is_not_disconnected() {
    let srv = start_server();
    let alice = connect_with_retry(srv.port);
    let bob = connect_with_retry(srv.port);
    alice.send(Action::Hello { user: "alice".into(), password: "pw".into() });
    bob.send(Action::Hello { user: "bob".into(), password: "pw".into() });
    // complete auth for both
    wait_for(&alice, |e| matches!(e, Event::AuthOk { .. }));
    wait_for(&bob, |e| matches!(e, Event::AuthOk { .. }));

    // alice creates the conversation so both know the conv id
    alice.send(Action::CreateConv { members: vec!["bob".into()] });
    let conv = match wait_for(&alice, |e| matches!(e, Event::ConvCreated(..))) {
        Event::ConvCreated(info) => info.id,
        other => panic!("expected ConvCreated, got {other:?}"),
    };

    // --- the actual regression check: stay IDLE past the old 60s timeout ---
    std::thread::sleep(IDLE);

    // assert alice did NOT get a Disconnected during the idle window
    for ev in alice.events().try_iter() {
        assert!(
            !matches!(ev, Event::Disconnected { .. }),
            "alice was disconnected while idle: {ev:?}"
        );
    }

    // now prove the connection is still alive: alice sends, bob receives, bob replies
    alice.send(Action::Send { conv: conv.clone(), text: "post-idle".into() });
    match wait_for(&bob, |e| matches!(e, Event::Message { .. })) {
        Event::Message { from, text, .. } => {
            assert_eq!(from, "alice");
            assert_eq!(text, "post-idle");
        }
        other => panic!("expected Message, got {other:?}"),
    }
    // alice should be alive enough to receive a Delivered
    wait_for(&alice, |e| matches!(e, Event::Delivered { .. }));

    bob.send(Action::Send { conv, text: "still here".into() });
    match wait_for(&alice, |e| matches!(e, Event::Message { .. })) {
        Event::Message { from, text, .. } => {
            assert_eq!(from, "bob");
            assert_eq!(text, "still here");
        }
        other => panic!("expected Message, got {other:?}"),
    }
}
