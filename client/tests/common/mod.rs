//! shared helpers for the `msgclient` end-to-end integration tests
//!
//! Each test binary compiles this module independently, so helpers a given
//! binary doesn't use would otherwise trigger dead-code warnings.
#![allow(dead_code)]

use msgclient::{Client, Event};
use std::net::TcpListener;
use std::path::PathBuf;
use std::process::{Child, Command};
use std::time::{Duration, Instant};

pub const TIMEOUT: Duration = Duration::from_secs(8);

/// path to the msgd server binary in the current profile's target dir
pub fn server_bin() -> PathBuf {
    let profile = std::env::var("PROFILE").unwrap_or_else(|_| "debug".to_string());
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../target")
        .join(profile)
        .join("msgd")
}

pub fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

/// a running msgd server, killed on drop
pub struct Server {
    child: Child,
    pub port: u16,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Start a server, retrying across ports until one actually comes up.
///
/// Picking a port and binding it are separate steps, so between them another
/// test binary (these run in parallel across the workspace) can take the same
/// port. `msgd` sets `SO_REUSEPORT`, so a lost race does not fail loudly --
/// two servers silently share the port and its redb file, and the client then
/// talks to whichever one the kernel picks. The only reliable signal is
/// connecting, so retry on a fresh port until that succeeds.
pub fn start_server() -> Server {
    let mut last = String::new();
    for _ in 0..8 {
        let port = free_port();
        let data = std::env::temp_dir()
            .join(format!("msgclient_it_{}_{}.redb", std::process::id(), port));
        let _ = std::fs::remove_file(&data);
        let child = Command::new(server_bin())
            .args(["--bind", &format!("127.0.0.1:{port}"), "--data", data.to_str().unwrap()])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn msgd server");

        let srv = Server { child, port };
        if wait_until_accepting(port) {
            return srv;
        }
        last = format!("server never accepted on port {port}");
        // drop `srv` so the process and its fd are released before retrying
        drop(srv);
        let _ = std::fs::remove_file(&data);
    }
    panic!("failed to start msgd after several attempts: {last}");
}

/// wait until the server accepts a TCP connection on `port`
fn wait_until_accepting(port: u16) -> bool {
    let deadline = Instant::now() + TIMEOUT;
    while Instant::now() < deadline {
        if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    false
}

pub fn connect_with_retry(port: u16) -> Client {
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
pub fn wait_for(client: &Client, pred: impl Fn(&Event) -> bool) -> Event {
    wait_for_poll(client, pred, Duration::from_millis(20))
}

/// like [`wait_for`], but polls the event channel every `poll` instead of a fixed 20ms
pub fn wait_for_poll(client: &Client, pred: impl Fn(&Event) -> bool, poll: Duration) -> Event {
    let deadline = Instant::now() + TIMEOUT;
    loop {
        if let Some(ev) = client.events().try_iter().find(|ev| pred(ev)) {
            return ev;
        }
        if Instant::now() >= deadline {
            panic!("timed out waiting for event");
        }
        std::thread::sleep(poll);
    }
}
