//! Latency probe: measures end-to-end send-to-deliver time between two
//! clients. Run with `cargo test -p msgclient --test latency -- --nocapture`.
//! Asserts the round-trip stays well under the previous ~2s worst case.

use msgclient::{Action, Client, Event};
use std::net::TcpListener;
use std::path::PathBuf;
use std::process::{Child, Command};
use std::time::{Duration, Instant};

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
    let data = std::env::temp_dir().join(format!("msgclient_lat_{}.redb", std::process::id()));
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
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn send_to_deliver_latency() {
    let srv = start_server();
    let alice = connect_with_retry(srv.port);
    let bob = connect_with_retry(srv.port);
    alice.send(Action::Hello { user: "alice".into(), password: "pw".into() });
    bob.send(Action::Hello { user: "bob".into(), password: "pw".into() });
    wait_for(&alice, |e| matches!(e, Event::AuthOk { .. }));
    wait_for(&bob, |e| matches!(e, Event::AuthOk { .. }));

    alice.send(Action::CreateConv { members: vec!["bob".into()] });
    let conv = match wait_for(&alice, |e| matches!(e, Event::ConvCreated { .. })) {
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
        let t0 = Instant::now();
        alice.send(Action::Send { conv: conv.clone(), text: text.clone() });
        // wait until bob sees this exact message
        let ev = wait_for(&bob, |e| matches!(e, Event::Message { text: t, .. } if *t == text));
        let us = t0.elapsed().as_micros();
        sum_us += us;
        if us > max_us { max_us = us; }
        let _ = &ev;
        // alice should also ack
        let _ = wait_for(&alice, |e| matches!(e, Event::Delivered { .. }));
        let _ = alice.events().try_iter().count();
        let _ = bob.events().try_iter().count();
    }
    let avg_ms = (sum_us / N as u128) as f64 / 1000.0;
    let max_ms = max_us as f64 / 1000.0;
    eprintln!("send→deliver: avg={avg_ms:.2}ms max={max_ms:.2}ms over {N} sends");
    // assert well clear of the old ~2s worst case
    assert!(max_ms < 500.0, "max latency {max_ms:.2}ms too high (was ~2000ms before the fix)");
}
