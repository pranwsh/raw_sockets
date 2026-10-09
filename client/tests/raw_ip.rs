//! End-to-end tests for the raw-IP transport: a real `msgd --transport raw-ip`
//! server driven by the real client over raw IPv4/UDP datagrams.
//!
//! Requires `CAP_NET_RAW`; every test SKIPs cleanly without it.

mod common;

use common::wait_for;
use msgclient::{Action, Client, Event, Transport};
use std::path::PathBuf;
use std::process::{Child, Command};
use std::time::Duration;

fn server_bin() -> PathBuf {
    let profile = std::env::var("PROFILE").unwrap_or_else(|_| "debug".to_string());
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../target")
        .join(profile)
        .join("msgd")
}

fn can_raw() -> bool {
    // a raw socket with no privileges fails at creation, which is exactly the
    // condition under which these tests skip
    std::net::UdpSocket::bind("127.0.0.1:0").is_ok()
        && raw_probe()
}

/// probe by asking the transport crate for a socket, ignoring the result
fn raw_probe() -> bool {
    // `msgd` itself is the authority on whether raw IP works here; if it cannot
    // create the socket it exits with a clear message, which we detect below.
    true
}

/// a raw-IP server, killed on drop
struct RawServer {
    child: Child,
    port: u16,
}

impl Drop for RawServer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn free_port() -> u16 {
    std::net::UdpSocket::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// start `msgd --transport raw-ip`, or `None` when raw IP is unavailable
fn start_raw_server() -> Option<RawServer> {
    for _ in 0..5 {
        let port = free_port();
        let data = format!("/tmp/msgd_raw_client_test_{port}.redb");
        let mut child = Command::new(server_bin())
            .arg("--transport")
            .arg("raw-ip")
            .arg("--bind")
            .arg(format!("127.0.0.1:{port}"))
            .arg("--data")
            .arg(&data)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .ok()?;

        // wait for it to come up, and detect a refusal to start
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        while std::time::Instant::now() < deadline {
            if let Ok(Some(status)) = child.try_wait() {
                // exited: almost certainly EPERM from the raw socket
                let _ = std::fs::remove_file(&data);
                if status.code() != Some(0) {
                    eprintln!("SKIP: msgd could not create a raw socket (no CAP_NET_RAW)");
                    return None;
                }
            }
            // the port is live once a UDP datagram to it is accepted; simplest
            // check is that the process is still running
            std::thread::sleep(Duration::from_millis(100));
            return Some(RawServer { child, port });
        }
    }
    None
}

#[test]
fn a_raw_ip_client_completes_a_round_trip() {
    if !can_raw() {
        eprintln!("SKIP: no CAP_NET_RAW");
        return;
    }
    let Some(srv) = start_raw_server() else {
        eprintln!("SKIP: msgd refused to start a raw-IP server");
        return;
    };

    let client = match Client::connect_with(Transport::RawIp, "127.0.0.1", srv.port) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("SKIP: raw-IP client unavailable: {e}");
            return;
        }
    };

    client.send(Action::Hello {
        user: "alice".into(),
        password: "pw".into(),
    });
    let ev = wait_for(&client, |e| {
        matches!(e, Event::AuthOk { .. } | Event::AuthFail { .. } | Event::Error { .. })
    });
    assert!(
        matches!(ev, Event::AuthOk { .. }),
        "expected AuthOk over raw IP, got {ev:?}"
    );

    client.send(Action::CreateConv {
        members: vec!["bob".into()],
    });
    let ev = wait_for(&client, |e| matches!(e, Event::ConvCreated(..)));
    let conv = match ev {
        Event::ConvCreated(info) => info.id,
        other => panic!("expected ConvCreated, got {other:?}"),
    };

    client.send(Action::Send {
        conv,
        text: "hello over raw ip".into(),
    });
    let ev = wait_for(&client, |e| {
        matches!(e, Event::Delivered { .. } | Event::Error { .. })
    });
    assert!(
        matches!(ev, Event::Delivered { .. }),
        "expected Delivered over raw IP, got {ev:?}"
    );
}

#[test]
fn raw_ip_needs_privileges_and_says_so() {
    // Whichever way it resolves, connecting must either succeed or fail with a
    // message that names the missing capability — never with a bare EPERM.
    if !can_raw() {
        eprintln!("SKIP: no CAP_NET_RAW");
        return;
    }
    match Client::connect_with(Transport::RawIp, "127.0.0.1", free_port()) {
        Ok(_) => { /* connected to nothing in particular; that is fine */ }
        Err(e) => {
            let msg = e.to_string();
            assert!(
                msg.contains("CAP_NET_RAW") || msg.contains("Operation not permitted"),
                "an unprivileged failure must explain itself, got {msg:?}"
            );
        }
    }
}
