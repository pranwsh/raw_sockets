//! integration tests that spawn a server instance and speak the binary protocol over real TCP sockets

use std::io::{Read, Write};
use std::net::{TcpStream, SocketAddr};
use std::process::{Command, Child};
use std::thread;
use std::time::Duration;

// reuse the workspace's protocol crate for frame encoding/decoding
// this is the actual protocol logic — the test verifies that the server speaks it correctly
use protocol::{self, MsgType, OwnedFrame, Decode};

// helpers

/// wraps a child process and kills it on drop
struct ServerGuard {
    child: Option<Child>,
}

impl ServerGuard {
    fn new(child: Child) -> Self {
        // give the server time to start
        thread::sleep(Duration::from_millis(200));
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

    let addr: SocketAddr = bind.parse().unwrap();
    (ServerGuard::new(child), addr)
}

fn portpicker() -> u16 {
    // bind to port 0, get the assigned port, close
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.local_addr().unwrap().port()
}

/// a thin client that sends/receives frames over a TCP stream
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

    /// send a raw frame
    fn send_frame(&mut self, msg_type: MsgType, flags: u16, body: &[u8]) {
        let frame = protocol::encode(msg_type, flags, body);
        self.stream.write_all(&frame).unwrap();
    }

    /// send a Hello(user_id \n password) frame
    fn hello(&mut self, user_id: &[u8], password: &[u8]) {
        let mut body = Vec::new();
        body.extend_from_slice(user_id);
        body.push(b'\n');
        body.extend_from_slice(password);
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
    let conv_id = created.body.to_vec();

    // send a message to the conversation
    let mut msg = Vec::new();
    msg.extend_from_slice(&conv_id);
    msg.push(b'\n');
    msg.extend_from_slice(b"Hello, Bob!");
    alice.send_frame(MsgType::Send, 0, &msg);

    // the send triggers: (1) a send frame echoing the message back to the sender (inbox delivery), then (2) a delivered ack
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

    client.hello(b"pinger", b"pong-pass");
    let resp = client.recv_frame();
    assert_eq!(resp.msg_type, MsgType::AuthOk);

    client.send_frame(MsgType::Ping, 0, &[]);
    let pong = client.recv_frame();
    assert_eq!(pong.msg_type, MsgType::Pong);
}
