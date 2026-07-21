//! Integration tests that spawn a server instance and speak the binary protocol
//! over real TCP sockets.
//!
//! Each test starts a server on a random port, connects a client, performs the
//! auth handshake, and verifies application-level semantics.

use std::io::{Read, Write};
use std::net::{TcpStream, SocketAddr};
use std::process::{Command, Child};
use std::thread;
use std::time::Duration;

// Reuse the workspace's protocol crate for frame encoding/decoding.
// This is the actual protocol logic — the test verifies that the server
// speaks it correctly.
use protocol::{self, MsgType, OwnedFrame, Decode};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Spawn a `msgd` instance on a random port. Returns the process handle and
/// the bound address.
fn spawn_server() -> (Child, SocketAddr) {
    let port = portpicker();
    let data_path = format!("/tmp/msgd_test_{}", port);
    let bind = format!("127.0.0.1:{}", port);

    let child = Command::new(concat!(env!("CARGO_MANIFEST_DIR"), "/../target/debug/msgd"))
        .arg("--bind")
        .arg(&bind)
        .arg("--data")
        .arg(&data_path)
        .spawn()
        .expect("failed to spawn msgd");

    // Give the server time to start.
    thread::sleep(Duration::from_millis(200));

    let addr: SocketAddr = bind.parse().unwrap();
    (child, addr)
}

fn portpicker() -> u16 {
    // Bind to port 0, get the assigned port, close.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.local_addr().unwrap().port()
}

/// A thin client that sends/receives frames over a TCP stream.
struct TestClient {
    stream: TcpStream,
    buf: Vec<u8>,
    read_offset: usize,
}

impl TestClient {
    fn connect(addr: SocketAddr) -> std::io::Result<Self> {
        let stream = TcpStream::connect(addr)?;
        stream.set_read_timeout(Some(Duration::from_secs(5)))?;
        stream.set_write_timeout(Some(Duration::from_secs(5)))?;
        Ok(Self { stream, buf: Vec::with_capacity(65536), read_offset: 0 })
    }

    /// Send a raw frame.
    fn send_frame(&mut self, msg_type: MsgType, flags: u16, body: &[u8]) {
        let frame = protocol::encode(msg_type, flags, body);
        self.stream.write_all(&frame).unwrap();
    }

    /// Send a `Hello(user_id)` frame.
    fn hello(&mut self, user_id: &[u8]) {
        self.send_frame(MsgType::Hello, 0, user_id);
    }

    /// Send an `AuthResponse(user_id \n token)` frame.
    fn auth_response(&mut self, user_id: &[u8], token: &[u8]) {
        let mut body = Vec::new();
        body.extend_from_slice(user_id);
        body.push(b'\n');
        body.extend_from_slice(token);
        self.send_frame(MsgType::AuthResponse, 0, &body);
    }

    /// Read and decode exactly one frame from the stream. Blocks until a
    /// complete frame is available.
    fn recv_frame(&mut self) -> OwnedFrame {
        loop {
            // Extract owned frame data and consumed count in a scope that
            // releases the borrow on self.buf before we modify it.
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

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn test_auth_handshake_new_account() {
    let (_server, addr) = spawn_server();
    let mut client = TestClient::connect(addr).unwrap();

    // Send Hello.
    client.hello(b"alice");

    // Expect AuthOk with a 32-byte token.
    let resp = client.recv_frame();
    assert_eq!(resp.msg_type, MsgType::AuthOk, "expected AuthOk, got {:?}", resp.msg_type);
    assert_eq!(resp.body.len(), 32, "token should be 32 bytes");
}

#[test]
fn test_auth_handshake_reconnect() {
    let (_server, addr) = spawn_server();
    let mut client = TestClient::connect(addr).unwrap();

    // Register alice.
    client.hello(b"alice");
    let resp = client.recv_frame();
    assert_eq!(resp.msg_type, MsgType::AuthOk);
    let token = resp.body.to_vec(); // copy before dropping client

    // Disconnect and reconnect.
    drop(client);
    let mut client2 = TestClient::connect(addr).unwrap();

    // Send Hello for existing account — expect AuthChallenge.
    client2.hello(b"alice");
    let challenge = client2.recv_frame();
    assert_eq!(challenge.msg_type, MsgType::AuthChallenge);

    // Respond with the token we saved earlier.
    client2.auth_response(b"alice", &token);
    let ok = client2.recv_frame();
    assert_eq!(ok.msg_type, MsgType::AuthOk);
}

#[test]
fn test_create_conv_and_send() {
    let (_server, addr) = spawn_server();
    let mut alice = TestClient::connect(addr).unwrap();

    // Register alice.
    alice.hello(b"alice");
    let resp = alice.recv_frame();
    assert_eq!(resp.msg_type, MsgType::AuthOk);

    // Create conversation: "alice,bob"
    alice.send_frame(MsgType::CreateConv, 0, b"alice,bob");
    let created = alice.recv_frame();
    assert_eq!(created.msg_type, MsgType::ConvCreated);
    let conv_id = created.body.to_vec();

    // Send a message to the conversation.
    let mut msg = Vec::new();
    msg.extend_from_slice(&conv_id);
    msg.push(b'\n');
    msg.extend_from_slice(b"Hello, Bob!");
    alice.send_frame(MsgType::Send, 0, &msg);

    // The send triggers: (1) a Send frame echoing the message back to the
    // sender (inbox delivery), then (2) a Delivered ack. Read until we see
    // Delivered.
    loop {
        let f = alice.recv_frame();
        if f.msg_type == MsgType::Delivered {
            break;
        }
    }
}

#[test]
fn test_ping_pong() {
    let (_server, addr) = spawn_server();
    let mut client = TestClient::connect(addr).unwrap();

    client.hello(b"pinger");
    let resp = client.recv_frame();
    assert_eq!(resp.msg_type, MsgType::AuthOk);

    client.send_frame(MsgType::Ping, 0, &[]);
    let pong = client.recv_frame();
    assert_eq!(pong.msg_type, MsgType::Pong);
}
