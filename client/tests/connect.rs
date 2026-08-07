//! End-to-end test: spins up the real msgd server binary and drives it
//! through the channel-based client API.

use msgclient::{Action, Event, Client};
use std::net::TcpListener;
use std::path::PathBuf;
use std::process::{Child, Command};
use std::time::{Duration, Instant};

const TIMEOUT: Duration = Duration::from_secs(8);

fn server_bin() -> PathBuf {
    let profile = std::env::var("PROFILE").unwrap_or_else(|_| "debug".to_string());
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../target")
        .join(profile)
        .join("msgd")
}

fn free_port() -> u16 {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    l.local_addr().unwrap().port()
}

struct Server {
    child: Child,
    port: u16,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn start_server() -> Server {
    let port = free_port();
    let data = std::env::temp_dir().join(format!("msgclient_it_{}_{}.redb", std::process::id(), port));
    let _ = std::fs::remove_file(&data);
    let child = Command::new(server_bin())
        .args([
            "--bind",
            &format!("127.0.0.1:{port}"),
            "--data",
            data.to_str().unwrap(),
        ])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn msgd server");
    Server { child, port }
}

fn connect_with_retry(port: u16) -> Client {
    let deadline = Instant::now() + TIMEOUT;
    loop {
        match Client::connect("127.0.0.1", port) {
            Ok(c) => return c,
            Err(_) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => panic!("connect failed: {e}"),
        }
    }
}

/// drain events until one matches `pred`, returning it (non-matching events are dropped)
fn wait_for(client: &Client, pred: impl Fn(&Event) -> bool) -> Event {
    let deadline = Instant::now() + TIMEOUT;
    loop {
        if let Some(ev) = client.events().try_iter().find(|ev| pred(ev)) {
            return ev;
        }
        if Instant::now() >= deadline {
            panic!("timed out waiting for event");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn assert_auth_ok(client: &Client) {
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
    let conv = match wait_for(&alice, |e| matches!(e, Event::ConvCreated { .. })) {
        Event::ConvCreated { id } => id,
        other => panic!("expected ConvCreated, got {other:?}"),
    };

    // alice lists conversations — the new one must be present
    alice.send(Action::ListConvs);
    match wait_for(&alice, |e| matches!(e, Event::Convs { .. })) {
        Event::Convs { ids } => assert!(ids.contains(&conv), "conv missing from list"),
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
    let conv = match wait_for(&alice, |e| matches!(e, Event::ConvCreated { .. })) {
        Event::ConvCreated { id } => id,
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
