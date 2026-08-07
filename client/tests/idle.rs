//! Regression test: an idle client must NOT be disconnected after the former
//! 60s idle timeout (now disabled). Holds a connection silent for 65s, then
//! verifies it can still send/receive.

use msgclient::{Action, Event, Client};
use std::net::TcpListener;
use std::path::PathBuf;
use std::process::{Child, Command};
use std::time::{Duration, Instant};

const IDLE: Duration = Duration::from_secs(65);
const WAIT: Duration = Duration::from_secs(8);

fn server_bin() -> PathBuf {
    let profile = std::env::var("PROFILE").unwrap_or_else(|_| "debug".to_string());
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../target")
        .join(profile)
        .join("msgd")
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

struct Server { child: Child, port: u16 }
impl Drop for Server { fn drop(&mut self) { let _ = self.child.kill(); let _ = self.child.wait(); } }

fn start_server() -> Server {
    let port = free_port();
    let data = std::env::temp_dir().join(format!("msgclient_idle_{}.redb", std::process::id()));
    let _ = std::fs::remove_file(&data);
    let child = Command::new(server_bin())
        .args(["--bind", &format!("127.0.0.1:{port}"), "--data", data.to_str().unwrap()])
        .stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null())
        .spawn().expect("spawn msgd");
    Server { child, port }
}

fn connect_with_retry(port: u16) -> Client {
    let deadline = Instant::now() + WAIT;
    loop {
        match Client::connect("127.0.0.1", port) {
            Ok(c) => return c,
            Err(_) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(50)),
            Err(e) => panic!("connect failed: {e}"),
        }
    }
}

fn wait_for(client: &Client, pred: impl Fn(&Event) -> bool) -> Event {
    let deadline = Instant::now() + WAIT;
    loop {
        if let Some(ev) = client.events().try_iter().find(&pred) { return ev; }
        if Instant::now() >= deadline { panic!("timed out waiting for event"); }
        std::thread::sleep(Duration::from_millis(20));
    }
}

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
    let conv = match wait_for(&alice, |e| matches!(e, Event::ConvCreated { .. })) {
        Event::ConvCreated { id } => id,
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
