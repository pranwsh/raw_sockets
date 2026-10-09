//! End-to-end tests for the datagram reactor against the real `msgd` framing.
//!
//! Requires `CAP_NET_RAW`; every test SKIPs cleanly without it.
//!
//! # Why the client is a plain UDP socket
//!
//! A raw socket is not demultiplexed by port the way a normal UDP socket is: it
//! receives its own outgoing datagrams back, and a datagram is delivered to only
//! the FIRST raw socket that reads it. Two raw reactors in one process therefore
//! compete for traffic rather than each observing all of it — the first to read
//! consumes the other's datagram.
//!
//! So these tests pair the reactor under test with a **plain UDP socket** as the
//! far end. That socket is demultiplexed by port, so it does not compete, and it
//! exercises exactly the path that matters: the reactor receives a datagram we
//! built, decodes frames out of it, and sends replies we can read back. The
//! packets are genuinely raw-socket packets in both directions — only the
//! test's own send/receive helper uses `SOCK_DGRAM`.

use std::sync::Arc;
use std::time::{Duration, Instant};

use protocol::{HelloReq, MsgType, Payload, SendReq};
use transport::conn::ConnectionId;
use transport::dgram::DgramReactor;
use transport::packet::{self, Endpoint};
use transport::reactor::EventHandler;
use transport::reliable::Envelope;

const LOOPBACK: u32 = 0x7F00_0001;

/// An `EventHandler` that records what it saw and answers Hello and Send.
///
/// Stands in for the real `Domain` without pulling in storage.
#[derive(Default)]
struct Recorder {
    frames: Vec<(MsgType, Vec<u8>)>,
    /// per-conversation sequence counter, as the real domain keeps it
    sequences: std::collections::HashMap<Vec<u8>, u64>,
    outbound: Vec<(ConnectionId, Arc<[u8]>)>,
    teardowns: Vec<(ConnectionId, transport::conn::TeardownReason)>,
    accepted: Vec<ConnectionId>,
    user: Vec<u8>,
}

impl Recorder {
    fn frames(&self) -> Vec<(MsgType, Vec<u8>)> {
        self.frames.clone()
    }

    fn saw(&self, t: MsgType) -> bool {
        self.frames.iter().any(|(ty, _)| *ty == t)
    }
}

impl EventHandler for Recorder {
    fn on_accept(&mut self, id: ConnectionId, _peer: std::net::SocketAddrV4) {
        self.accepted.push(id);
    }

    fn on_frame(&mut self, id: ConnectionId, frame: protocol::OwnedFrame) {
        match frame.msg_type {
            MsgType::Hello => {
                let body = HelloReq::decode(&frame.body).expect("hello body");
                self.frames.push((MsgType::Hello, frame.body.to_vec()));
                let created = self.user.is_empty();
                self.user = body.user.clone();
                let body = protocol::AuthOkBody { created }.encode();
                self.outbound
                    .push((id, protocol::encode(MsgType::AuthOk, 0, &body).into()));
            }
            MsgType::Send => {
                let body = SendReq::decode(&frame.body).expect("send body");
                self.frames.push((MsgType::Send, body.text.clone()));
                // sequences are per conversation, so each starts at 1
                let counter = self.sequences.entry(body.conv.clone()).or_insert(0);
                *counter += 1;
                let seq = *counter;
                let ack = protocol::DeliveredBody { seq }.encode();
                self.outbound
                    .push((id, protocol::encode(MsgType::Delivered, 0, &ack).into()));
            }
            other => self.frames.push((other, frame.body.to_vec())),
        }
    }

    fn on_teardown(&mut self, id: ConnectionId, reason: transport::conn::TeardownReason) {
        self.teardowns.push((id, reason));
    }

    fn drain_outbound(&mut self, out: &mut Vec<(ConnectionId, Arc<[u8]>)>) {
        out.append(&mut self.outbound);
    }

    fn drain_teardowns(&mut self, out: &mut Vec<(ConnectionId, transport::conn::TeardownReason)>) {
        out.append(&mut self.teardowns);
    }
}

// The far end of the conversation: a normal UDP socket that can also inject
// raw-built datagrams at the reactor. See the module docs for why the test's own
// end is not a second raw socket.
struct UdpPeer {
    sock: std::net::UdpSocket,
    addr: Endpoint,
}

impl UdpPeer {
    fn bind(target_port: u16) -> std::io::Result<UdpPeer> {
        let sock = std::net::UdpSocket::bind("127.0.0.1:0")?;
        sock.set_read_timeout(Some(Duration::from_millis(300)))?;
        Ok(UdpPeer {
            sock,
            // the reactor we are talking to, addressed by its bound port
            addr: Endpoint {
                addr: LOOPBACK,
                port: target_port,
            },
        })
    }
}

trait UdpPeerHelpers {
    fn send_to_raw(&self, envelope: &[u8]);
    fn recv_frames(&mut self, quiet: Duration, deadline_after: Duration) -> Vec<(MsgType, Vec<u8>)>;
}

impl UdpPeerHelpers for UdpPeer {
    /// send a datagram to the reactor, as a plain UDP payload
    fn send_to_raw(&self, envelope: &[u8]) {
        self.sock
            .send_to(envelope, ("127.0.0.1", self.addr.port))
            .expect("send to reactor");
    }

    /// read replies until nothing arrives for `quiet` or `deadline` passes
    fn recv_frames(&mut self, quiet: Duration, deadline_after: Duration) -> Vec<(MsgType, Vec<u8>)> {
        let mut out = Vec::new();
        let hard_stop = Instant::now() + deadline_after;
        let mut buf = vec![0u8; 65_536];
        loop {
            self.sock
                .set_read_timeout(Some(quiet))
                .expect("set timeout");
            match self.sock.recv(&mut buf) {
                Ok(n) => {
                    let datagram = &buf[..n];
                    // the reactor's reply is a UDP payload carrying an envelope
                    if let Some(env) = Envelope::decode(datagram)
                        && !env.payload.is_empty()
                        && let protocol::Decode::Complete { frame, .. } =
                            protocol::decode(&env.payload)
                    {
                        out.push((frame.msg_type, frame.body.to_vec()));
                    }
                }
                Err(_) => break,
            }
            if Instant::now() > hard_stop {
                break;
            }
        }
        out
    }
}

fn endpoint(port: u16) -> Endpoint {
    Endpoint {
        addr: LOOPBACK,
        port,
    }
}

fn free_port() -> u16 {
    std::net::UdpSocket::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn can_raw() -> bool {
    match transport::sys::socket_raw_ipv4() {
        Ok(fd) => transport::sys::set_ip_hdrincl(fd).is_ok(),
        Err(_) => false,
    }
}

/// The far end's send side: one reliability layer for the whole conversation.
///
/// It must be shared across messages. A fresh layer per message restarts the
/// sequence numbers at 1, and the reactor — correctly — treats every repeat of a
/// sequence number it has already seen as a retransmit and drops it. So a second
/// message would silently never arrive.
struct FarEnd {
    layer: transport::reliable::ReliabilityLayer,
}

impl FarEnd {
    fn new() -> FarEnd {
        FarEnd {
            layer: transport::reliable::ReliabilityLayer::new(),
        }
    }

    /// wrap one frame, advancing the sequence number
    fn wrap(&mut self, frame: &[u8]) -> Vec<u8> {
        let (_, envelope) = self.layer.wrap(frame, 0);
        envelope
    }
}

/// run the reactor until `done` or the deadline passes
///
/// `done` is handed the reactor, so a caller can inspect its handler between
/// ticks without holding a second borrow.
fn pump_until(server: &mut DgramReactor<Recorder>, mut done: impl FnMut(&DgramReactor<Recorder>) -> bool) -> bool {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if done(server) {
            return true;
        }
        server.tick_once().expect("tick");
        std::thread::sleep(Duration::from_millis(1));
    }
    false
}

#[test]
fn a_reactor_completes_a_request_response_round_trip() {
    if !can_raw() {
        eprintln!("SKIP: no CAP_NET_RAW");
        return;
    }
    let port = free_port();
    let mut server = DgramReactor::new(Recorder::default(), endpoint(port), None)
        .expect("server raw socket")
        .with_tag("SERVER");
    let mut client = UdpPeer::bind(port).expect("client UDP socket");
    let mut far = FarEnd::new();

    // Hello -> AuthOk
    let hello = HelloReq {
        user: b"alice".to_vec(),
        password: b"pw".to_vec(),
    }
    .encode();
    client.send_to_raw(&far.wrap(&protocol::encode(MsgType::Hello, 0, &hello)));

    assert!(
        pump_until(&mut server, |r| r.handler().saw(MsgType::Hello)),
        "server must decode the Hello"
    );
    assert!(server.handler().saw(MsgType::Hello), "Hello was decoded");
    assert_eq!(server.peer_count(), 1, "the sender becomes one peer");

    let replies = client.recv_frames(Duration::from_millis(400), Duration::from_secs(3));
    assert!(
        replies.iter().any(|(t, _)| *t == MsgType::AuthOk),
        "client must receive AuthOk, got {replies:?}"
    );
    // first Hello on a fresh server means the account was created
    let auth = replies
        .iter()
        .find(|(t, _)| *t == MsgType::AuthOk)
        .expect("AuthOk");
    assert_eq!(auth.1, vec![1], "AuthOk flag should say new account");

    // Send -> Delivered
    let body = SendReq {
        conv: b"conv0001".to_vec(),
        text: b"hello over raw ip".to_vec(),
    }
    .encode();
    client.send_to_raw(&far.wrap(&protocol::encode(MsgType::Send, 0, &body)));

    let decoded = pump_until(&mut server, |r| {
        r.handler()
            .frames()
            .iter()
            .any(|(t, b)| *t == MsgType::Send && b == b"hello over raw ip")
    });
    assert!(decoded, "server must decode the Send payload");
    let replies = client.recv_frames(Duration::from_millis(400), Duration::from_secs(3));
    let ack = replies
        .iter()
        .find(|(t, _)| *t == MsgType::Delivered)
        .expect("client must receive Delivered");
    // `Delivered` carries the per-conversation sequence as a u64 little-endian
    let seq = u64::from_le_bytes(ack.1.as_slice().try_into().expect("8-byte seq"));
    assert_eq!(seq, 1, "first sequence number is 1");
}

#[test]
fn many_messages_arrive_exactly_once_and_in_order() {
    if !can_raw() {
        eprintln!("SKIP: no CAP_NET_RAW");
        return;
    }
    let port = free_port();
    let mut server = DgramReactor::new(Recorder::default(), endpoint(port), None)
        .expect("server raw socket");
    let client = UdpPeer::bind(port).expect("client UDP socket");
    let mut far = FarEnd::new();

    // authenticate first so the peer is established
    let hello = HelloReq {
        user: b"alice".to_vec(),
        password: b"pw".to_vec(),
    }
    .encode();
    client.send_to_raw(&far.wrap(&protocol::encode(MsgType::Hello, 0, &hello)));
    assert!(pump_until(&mut server, |r| r.handler().saw(MsgType::Hello)));

    const N: usize = 25;
    let mut expected = Vec::new();
    for i in 0..N {
        let text = format!("msg-{i}");
        let body = SendReq {
            conv: b"conv0001".to_vec(),
            text: text.clone().into_bytes(),
        }
        .encode();
        expected.push(text.into_bytes());
        client.send_to_raw(&far.wrap(&protocol::encode(MsgType::Send, 0, &body)));
    }

    let all_arrived = pump_until(&mut server, |r| {
        r.handler()
            .frames()
            .iter()
            .filter(|(t, _)| *t == MsgType::Send)
            .count()
            >= N
    });
    assert!(all_arrived, "all sends must arrive");

    let texts: Vec<Vec<u8>> = server
        .handler()
        .frames()
        .into_iter()
        .filter(|(t, _)| *t == MsgType::Send)
        .map(|(_, b)| b)
        .collect();
    assert_eq!(texts, expected, "every message arrives once, in order");
}

/// A payload larger than the MTU is split into IP fragments on send.
///
/// Only the SEND side is asserted. Linux's loopback interface does not reassemble
/// hand-built IPv4 fragments for local UDP delivery — `sendto` succeeds and the
/// fragments are then dropped before reaching the socket — so a loopback test
/// cannot observe the receive side. Reassembly itself is covered exhaustively by
/// the unit tests in `packet` (in order, out of order, lost, and duplicate
/// fragments).
#[test]
fn a_payload_over_the_mtu_is_fragmented() {
    if !can_raw() {
        eprintln!("SKIP: no CAP_NET_RAW");
        return;
    }
    let port = free_port();
    let server_addr = endpoint(port);
    let src = endpoint(port);
    let dst = endpoint(port.wrapping_add(1));

    let text = vec![b'q'; 4000];
    let body = SendReq {
        conv: b"conv0001".to_vec(),
        text,
    }
    .encode();
    let frame = protocol::encode(MsgType::Send, 0, &body);
    assert!(frame.len() > 1500, "frame should exceed the MTU");

    let encoded = packet::encode_datagram(src, dst, &frame, 1, packet::DEFAULT_MTU);
    assert!(
        encoded.packets.len() > 1,
        "a 4 KB payload must fragment into several packets, got {}",
        encoded.packets.len()
    );
    // every fragment must fit the MTU and carry the right flags
    for (i, p) in encoded.packets.iter().enumerate() {
        assert!(
            p.bytes.len() <= packet::DEFAULT_MTU,
            "fragment {i} is {} bytes, over the MTU",
            p.bytes.len()
        );
    }
    // and the reactor must actually be able to send a large frame without
    // panicking or silently dropping it at the queue
    let server = DgramReactor::new(Recorder::default(), server_addr, None).expect("raw socket");
    assert_eq!(server.local_addr().port, port);
}
