//! Latency probe: measures end-to-end send-to-deliver time between two
//! clients. Run with `cargo test -p msgclient --test latency -- --nocapture`.
//! Asserts the round-trip stays well under the previous ~2s worst case.

mod common;

use common::{connect_with_retry, start_server, wait_for_poll};
use msgclient::{Action, Event};
use std::time::Duration;

#[test]
fn send_to_deliver_latency() {
    let srv = start_server();
    let alice = connect_with_retry(srv.port);
    let bob = connect_with_retry(srv.port);
    alice.send(Action::Hello { user: "alice".into(), password: "pw".into() });
    bob.send(Action::Hello { user: "bob".into(), password: "pw".into() });
    wait_for_poll(&alice, |e| matches!(e, Event::AuthOk { .. }), Duration::from_millis(1));
    wait_for_poll(&bob, |e| matches!(e, Event::AuthOk { .. }), Duration::from_millis(1));

    alice.send(Action::CreateConv { members: vec!["bob".into()] });
    let conv = match wait_for_poll(&alice, |e| matches!(e, Event::ConvCreated { .. }), Duration::from_millis(1)) {
        Event::ConvCreated { id } => id,
        other => panic!("expected ConvCreated, got {other:?}"),
    };

    // drain any pre-existing events (conv list etc.) so the measurement is clean
    let _ = alice.events().try_iter().count();
    let _ = bob.events().try_iter().count();

    const N: usize = 20;
    let mut max_us: u128 = 0;
    let mut sum_us: u128 = 0;
    for i in 0..N {
        let text = format!("msg {i}");
        let t0 = std::time::Instant::now();
        alice.send(Action::Send { conv: conv.clone(), text: text.clone() });
        // wait until bob sees this exact message
        wait_for_poll(&bob, |e| matches!(e, Event::Message { text: t, .. } if *t == text), Duration::from_millis(1));
        let us = t0.elapsed().as_micros();
        sum_us += us;
        if us > max_us { max_us = us; }
        // alice should also ack
        wait_for_poll(&alice, |e| matches!(e, Event::Delivered { .. }), Duration::from_millis(1));
        let _ = alice.events().try_iter().count();
        let _ = bob.events().try_iter().count();
    }
    let avg_ms = (sum_us / N as u128) as f64 / 1000.0;
    let max_ms = max_us as f64 / 1000.0;
    eprintln!("send→deliver: avg={avg_ms:.2}ms max={max_ms:.2}ms over {N} sends");
    // assert well clear of the old ~2s worst case
    assert!(max_ms < 500.0, "max latency {max_ms:.2}ms too high (was ~2000ms before the fix)");
}
