//! integration tests that spawn a server instance and speak the binary protocol over real TCP sockets

use std::io::{Read, Write};
use std::net::{TcpStream, SocketAddr};
use std::path::PathBuf;
use std::process::{Command, Child};
use std::time::{Duration, Instant};

// reuse the workspace's protocol crate for frame encoding/decoding
// this is the actual protocol logic — the test verifies that the server speaks it correctly
use protocol::{self, ConvEntry, ConvsRespBody, HelloReq, MsgType, OwnedFrame, Payload, SendReq, Decode};

// helpers

const START_TIMEOUT: Duration = Duration::from_secs(8);

fn server_bin() -> PathBuf {
    let profile = std::env::var("PROFILE").unwrap_or_else(|_| "debug".to_string());
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../target")
        .join(profile)
        .join("msgd")
}

/// wraps a child process and kills it on drop
struct ServerGuard {
    child: Option<Child>,
}

impl ServerGuard {
    fn new(child: Child) -> Self {
        Self { child: Some(child) }
    }
}

impl Drop for ServerGuard {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// spawn a msgd instance on a random port
fn spawn_server() -> (ServerGuard, SocketAddr) {
    // Retry across a few ports: between picking a port and the server binding
    // it, anything else on the box (including a sibling test running in
    // parallel) can take it. `msgd` sets SO_REUSEPORT, so a lost race does
    // not fail loudly -- two servers end up silently sharing the port and its
    // redb file, and the test then talks to whichever one the kernel picks.
    // Detecting that requires retrying, so give it a few attempts.
    let mut last_err = String::new();
    for _ in 0..8 {
        let port = portpicker();
        let data_path = format!("/tmp/msgd_test_{}", port);
        let bind = format!("127.0.0.1:{port}");
        let addr: SocketAddr = bind.parse().unwrap();

        let child = match Command::new(server_bin())
            .arg("--bind")
            .arg(&bind)
            .arg("--data")
            .arg(&data_path)
            .spawn()
        {
            Ok(c) => c,
            Err(e) => {
                last_err = format!("spawn failed: {e}");
                continue;
            }
        };

        let guard = ServerGuard::new(child);
        if wait_until_accepting(addr) {
            return (guard, addr);
        }
        // the server exited or never came up on this port: drop the guard so
        // the fd is released, then try another port
        last_err = format!("server never accepted on port {port}");
        drop(guard);
        let _ = std::fs::remove_file(&data_path);
    }
    panic!("failed to start msgd after several attempts: {last_err}");
}

/// Wait until the server is actually accepting connections on `addr`.
fn wait_until_accepting(addr: SocketAddr) -> bool {
    let deadline = Instant::now() + START_TIMEOUT;
    while Instant::now() < deadline {
        if std::net::TcpStream::connect(addr).is_ok() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    false
}

/// Pick a free TCP port that no other test in this process has taken.
///
/// The listener is closed before returning, so a race with an unrelated
/// process is still possible; `spawn_server` retries to cover it. Tracking the
/// ports we already handed out removes the collision between tests running in
/// parallel in this same binary, which is the common case.
fn portpicker() -> u16 {
    static TAKEN: std::sync::LazyLock<std::sync::Mutex<std::collections::HashSet<u16>>> =
        std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashSet::new()));

    for _ in 0..64 {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        if TAKEN.lock().unwrap().insert(port) {
            return port;
        }
    }
    panic!("could not find a free port");
}

/// a thin client that sends/receives frames over a TCP stream
struct TestClient {
    stream: TcpStream,
    buf: Vec<u8>,
    read_offset: usize,
}

impl TestClient {
    /// connect with retry so a just-spawned server doesn't flake the test
    fn connect(addr: SocketAddr) -> std::io::Result<Self> {
        let deadline = Instant::now() + START_TIMEOUT;
        let stream = loop {
            match TcpStream::connect(addr) {
                Ok(s) => break s,
                Err(_e) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(50)),
                Err(e) => return Err(e),
            }
        };
        stream.set_read_timeout(Some(Duration::from_secs(5)))?;
        stream.set_write_timeout(Some(Duration::from_secs(5)))?;
        Ok(Self { stream, buf: Vec::with_capacity(65536), read_offset: 0 })
    }

    /// send a raw frame
    fn send_frame(&mut self, msg_type: MsgType, flags: u16, body: &[u8]) {
        let frame = protocol::encode(msg_type, flags, body);
        self.stream.write_all(&frame).unwrap();
    }

    /// send a Hello(user_id \n password) frame
    fn hello(&mut self, user_id: &[u8], password: &[u8]) {
        let body = HelloReq { user: user_id.to_vec(), password: password.to_vec() }.encode();
        self.send_frame(MsgType::Hello, 0, &body);
    }

    /// read and decode exactly one frame from the stream
    fn recv_frame(&mut self) -> OwnedFrame {
        loop {
            // extract owned frame data and consumed count in a scope that releases the borrow on self.buf before we modify it
            let consumed;
            let owned = {
                let buf_slice = &self.buf[self.read_offset..];
                match protocol::decode(buf_slice) {
                    Decode::Complete { frame, consumed: c } => {
                        consumed = c;
                        OwnedFrame::from_borrowed(&frame)
                    }
                    Decode::Need => {
                        let mut tmp = [0u8; 8192];
                        let n = self.stream.read(&mut tmp).unwrap();
                        if n == 0 {
                            panic!("connection closed by server");
                        }
                        self.buf.extend_from_slice(&tmp[..n]);
                        continue;
                    }
                    Decode::Err(e) => {
                        panic!("decode error: {e:?}");
                    }
                }
            };
            self.read_offset += consumed;
            if self.read_offset > 4096 {
                self.buf.drain(..self.read_offset);
                self.read_offset = 0;
            }
            return owned;
        }
    }
}

impl Drop for TestClient {
    fn drop(&mut self) {
        let frame = protocol::encode(MsgType::Goodbye, 0, &[]);
        let _ = self.stream.write_all(&frame);
        let _ = self.stream.flush();
    }
}

// tests

#[test]
fn test_auth_handshake_new_account() {
    let (_server, addr) = spawn_server();
    let mut client = TestClient::connect(addr).unwrap();

    // send hello with the user-chosen password
    client.hello(b"alice", b"hunter2");

    // expect AuthOk with a 1-byte "new account" flag
    let resp = client.recv_frame();
    assert_eq!(resp.msg_type, MsgType::AuthOk, "expected AuthOk, got {:?}", resp.msg_type);
    assert_eq!(resp.body.first(), Some(&1), "new account should be flagged with 1");
}

#[test]
fn test_auth_handshake_reconnect() {
    let (_server, addr) = spawn_server();
    let mut client = TestClient::connect(addr).unwrap();

    // register alice with a password
    client.hello(b"alice", b"hunter2");
    let resp = client.recv_frame();
    assert_eq!(resp.msg_type, MsgType::AuthOk);
    assert_eq!(resp.body.first(), Some(&1));

    // disconnect and reconnect with the same password
    drop(client);
    let mut client2 = TestClient::connect(addr).unwrap();

    client2.hello(b"alice", b"hunter2");
    let ok = client2.recv_frame();
    assert_eq!(ok.msg_type, MsgType::AuthOk);
    assert_eq!(ok.body.first(), Some(&0), "existing account should be flagged with 0");
}

#[test]
fn test_auth_wrong_password() {
    let (_server, addr) = spawn_server();
    let mut client = TestClient::connect(addr).unwrap();

    client.hello(b"alice", b"hunter2");
    let resp = client.recv_frame();
    assert_eq!(resp.msg_type, MsgType::AuthOk);

    drop(client);
    let mut client2 = TestClient::connect(addr).unwrap();

    // wrong password must be rejected
    client2.hello(b"alice", b"wrong-pass");
    let fail = client2.recv_frame();
    assert_eq!(fail.msg_type, MsgType::AuthFail, "expected AuthFail, got {:?}", fail.msg_type);
}

#[test]
fn test_create_conv_and_send() {
    let (_server, addr) = spawn_server();
    let mut alice = TestClient::connect(addr).unwrap();

    // register alice
    alice.hello(b"alice", b"hunter2");
    let resp = alice.recv_frame();
    assert_eq!(resp.msg_type, MsgType::AuthOk);

    // create conversation: "alice,bob"
    alice.send_frame(MsgType::CreateConv, 0, b"alice,bob");
    let created = alice.recv_frame();
    assert_eq!(created.msg_type, MsgType::ConvCreated);
    // the reply carries the member list, not just the id, so the creating
    // client can label the conversation without another round trip
    let entry = ConvEntry::decode(&created.body).expect("ConvCreated carries a record");
    let mut created_members: Vec<String> =
        entry.members.iter().map(|m| String::from_utf8_lossy(m).into_owned()).collect();
    created_members.sort();
    assert_eq!(created_members, vec!["alice".to_string(), "bob".to_string()]);
    let conv_id = entry.id;

    // send a message to the conversation
    let msg = SendReq { conv: conv_id.clone(), text: b"Hello, Bob!".to_vec() }.encode();
    alice.send_frame(MsgType::Send, 0, &msg);

    // alice should get a Delivered ack for her send (other members are the
    // only ones who receive the message frame itself)
    loop {
        let f = alice.recv_frame();
        if f.msg_type == MsgType::Delivered {
            break;
        }
    }
}

#[test]
fn test_list_convs_returns_member_names() {
    let (_server, addr) = spawn_server();
    let mut alice = TestClient::connect(addr).unwrap();

    alice.hello(b"alice", b"hunter2");
    assert_eq!(alice.recv_frame().msg_type, MsgType::AuthOk);

    alice.send_frame(MsgType::CreateConv, 0, b"alice,bob");
    let created = alice.recv_frame();
    assert_eq!(created.msg_type, MsgType::ConvCreated);
    // the reply carries the member list, not just the id, so the creating
    // client can label the conversation without another round trip
    let entry = ConvEntry::decode(&created.body).expect("ConvCreated carries a record");
    let mut created_members: Vec<String> =
        entry.members.iter().map(|m| String::from_utf8_lossy(m).into_owned()).collect();
    created_members.sort();
    assert_eq!(created_members, vec!["alice".to_string(), "bob".to_string()]);
    let conv_id = entry.id;

    alice.send_frame(MsgType::ListConvs, 0, b"");
    let resp = alice.recv_frame();
    assert_eq!(resp.msg_type, MsgType::ConvsResp);

    // the listing must carry the member names, not just the opaque id, so a
    // frontend can render a readable label
    let listing = ConvsRespBody::decode(&resp.body).expect("decodes");
    assert_eq!(listing.convs.len(), 1, "expected exactly one conversation");
    assert_eq!(listing.convs[0].id, conv_id, "listing id must match the created id");
    let mut members: Vec<String> = listing.convs[0]
        .members
        .iter()
        .map(|m| String::from_utf8_lossy(m).into_owned())
        .collect();
    members.sort();
    assert_eq!(members, vec!["alice".to_string(), "bob".to_string()]);
}

#[test]
fn test_ping_pong() {
    let (_server, addr) = spawn_server();
    let mut client = TestClient::connect(addr).unwrap();

    client.hello(b"pinger", b"pong-pass");
    let resp = client.recv_frame();
    assert_eq!(resp.msg_type, MsgType::AuthOk);

    client.send_frame(MsgType::Ping, 0, &[]);
    let pong = client.recv_frame();
    assert_eq!(pong.msg_type, MsgType::Pong);
}
