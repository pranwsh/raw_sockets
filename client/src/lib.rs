//! channel-based client for the messaging service
//!
//! A [`Client`] connects to a messaging server and exposes two channels:
//! - **in**: high-level [`Action`]s (authenticate, create a conversation, send a message, ping, goodbye)
//! - **out**: high-level [`Event`]s (auth results, incoming messages, deliveries, errors, disconnects)
//!
//! A frontend (like the term_render TUI) only ever speaks these two types; all
//! wire framing lives behind the `protocol` crate. Because the interface is
//! just channels, other programs can plug in later without touching the TUI.

#![forbid(unsafe_code)]

use protocol::{self, Decode, MsgType, OwnedFrame};
use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

/// a high-level operation the caller wants the connected server to perform
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// authenticate (the account is created if it doesn't exist yet)
    Hello { user: String, password: String },
    /// create a conversation with the given members; the authenticated user is prepended automatically
    CreateConv { members: Vec<String> },
    /// list the conversations the authenticated user is a member of
    ListConvs,
    /// send a message into a conversation
    Send { conv: Vec<u8>, text: String },
    /// send a keepalive ping
    Ping,
    /// send a clean goodbye and close the connection
    Goodbye,
}

/// a high-level notification produced by the client
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// connection established (the socket is up)
    Connected,
    /// authentication succeeded; `created` is true when a new account was made
    AuthOk { created: bool },
    /// authentication was rejected
    AuthFail { reason: String },
    /// the server created a conversation and returned its id
    ConvCreated { id: Vec<u8> },
    /// the server's response to [`Action::ListConvs`]
    Convs { ids: Vec<Vec<u8>> },
    /// an inbound message (delivered to this connection)
    Message { conv: Vec<u8>, from: String, seq: u64, text: String },
    /// our [`Action::Send`] was accepted with a sequence number
    Delivered { seq: u64 },
    /// reply to [`Action::Ping`]
    Pong,
    /// the server reported an error
    Error { msg: String },
    /// the connection was closed
    Disconnected { reason: String },
}

/// a connected client: send [`Action`]s in, receive [`Event`]s out
#[derive(Debug)]
pub struct Client {
    action_tx: mpsc::Sender<Action>,
    event_rx: mpsc::Receiver<Event>,
    _join: thread::JoinHandle<()>,
}

impl Client {
    /// open a connection to the messaging server at `host:port` and spawn the
    /// background thread that owns the socket. Returns once the TCP connection
    /// is established.
    pub fn connect(host: &str, port: u16) -> io::Result<Client> {
        let addr = format!("{}:{}", host, port);
        let stream = TcpStream::connect(&addr)?;
        // The background loop does a blocking read and can only drain the
        // action channel (and notice disconnects) when that read returns. A
        // short timeout keeps actions flowing promptly: with 100ms, a queued
        // Send could sit in the channel for ~100ms before being written to the
        // socket, adding noticeable end-to-end latency. 10ms keeps pickup near
        // real-time at negligible CPU cost.
        stream.set_read_timeout(Some(Duration::from_millis(10)))?;

        let (action_tx, action_rx) = mpsc::channel();
        let (event_tx, event_rx) = mpsc::channel();
        let _ = event_tx.send(Event::Connected);

        let _join = thread::spawn(move || run_loop(stream, action_rx, event_tx));

        Ok(Client { action_tx, event_rx, _join })
    }

    /// queue an action to be sent; returns false if the connection is gone
    pub fn send(&self, action: Action) -> bool {
        self.action_tx.send(action).is_ok()
    }

    /// the event channel; poll it with `try_iter`/`recv` to consume server events
    pub fn events(&self) -> &mpsc::Receiver<Event> {
        &self.event_rx
    }
}

// background loop — the only place that touches the socket

fn run_loop(
    mut stream: TcpStream,
    action_rx: mpsc::Receiver<Action>,
    event_tx: mpsc::Sender<Event>,
) {
    let mut authenticated: Option<Vec<u8>> = None;
    let mut buf = Vec::with_capacity(65536);
    let mut offset = 0usize;

    loop {
        // drain any queued actions first
        let mut write_failed = false;
        for action in action_rx.try_iter() {
            if let Some((msg_type, body)) = encode_action(action, &mut authenticated) {
                let frame = protocol::encode(msg_type, 0, &body);
                if stream.write_all(&frame).is_err() {
                    write_failed = true;
                    break;
                }
            }
        }
        if write_failed {
            break;
        }

        // read a chunk (blocks up to the read timeout)
        let mut tmp = [0u8; 8192];
        match stream.read(&mut tmp) {
            Ok(0) => break, // clean EOF — server closed
            Ok(n) => {
                buf.extend_from_slice(&tmp[..n]);
                loop {
                    let slice = &buf[offset..];
                    match protocol::decode(slice) {
                        Decode::Complete { frame, consumed } => {
                            offset += consumed;
                            let owned = OwnedFrame::from_borrowed(&frame);
                            if event_tx.send(decode_event(owned)).is_err() {
                                return; // receiver dropped; nothing left to do
                            }
                        }
                        Decode::Need => break,
                        // misaligned garbage: skip one byte and resync
                        Decode::Err(_) => {
                            offset += 1;
                        }
                    }
                }
                if offset > 4096 {
                    buf.drain(..offset);
                    offset = 0;
                }
            }
            Err(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) => {
                // nothing pending on the wire; loop to check for new actions
            }
            Err(_) => break,
        }
    }

    let _ = event_tx.send(Event::Disconnected { reason: "connection closed".into() });
}

// Action → wire frame

fn encode_action(action: Action, authenticated: &mut Option<Vec<u8>>) -> Option<(MsgType, Box<[u8]>)> {
    match action {
        Action::Hello { user, password } => {
            *authenticated = Some(user.clone().into_bytes());
            let mut body = Vec::with_capacity(user.len() + 1 + password.len());
            body.extend_from_slice(user.as_bytes());
            body.push(b'\n');
            body.extend_from_slice(password.as_bytes());
            Some((MsgType::Hello, body.into_boxed_slice()))
        }
        Action::CreateConv { mut members } => {
            if let Some(me) = authenticated.as_ref() {
                let me = String::from_utf8_lossy(me);
                if !members.iter().any(|m| *m == me) {
                    members.insert(0, me.into_owned());
                }
            }
            let body = members.join(",").into_bytes();
            Some((MsgType::CreateConv, body.into_boxed_slice()))
        }
        Action::ListConvs => Some((MsgType::ListConvs, Vec::new().into_boxed_slice())),
        Action::Send { conv, text } => {
            let mut body = Vec::with_capacity(conv.len() + 1 + text.len());
            body.extend_from_slice(&conv);
            body.push(b'\n');
            body.extend_from_slice(text.as_bytes());
            Some((MsgType::Send, body.into_boxed_slice()))
        }
        Action::Ping => Some((MsgType::Ping, Vec::new().into_boxed_slice())),
        Action::Goodbye => Some((MsgType::Goodbye, Vec::new().into_boxed_slice())),
    }
}

// wire frame → Event

fn decode_event(f: OwnedFrame) -> Event {
    match f.msg_type {
        MsgType::AuthOk => {
            let created = f.body.first() == Some(&1);
            Event::AuthOk { created }
        }
        MsgType::AuthFail => {
            Event::AuthFail { reason: String::from_utf8_lossy(&f.body).into_owned() }
        }
        MsgType::ConvCreated => Event::ConvCreated { id: f.body.to_vec() },
        MsgType::ConvsResp => {
            let mut ids = Vec::new();
            for chunk in f.body.chunks_exact(8) {
                ids.push(chunk.to_vec());
            }
            Event::Convs { ids }
        }
        MsgType::Send => {
            // body: conv_id "\n" seq(8 LE) sender "\n" text
            let Some(sep) = f.body.iter().position(|&b| b == b'\n') else {
                return Event::Error { msg: "malformed inbound Send frame".into() };
            };
            let conv = f.body[..sep].to_vec();
            let after = &f.body[sep + 1..];
            let (seq, rest) = if after.len() >= 8 {
                let seq = u64::from_le_bytes(after[..8].try_into().unwrap());
                (seq, &after[8..])
            } else {
                return Event::Error { msg: "malformed inbound Send frame".into() };
            };
            let (from, text) = match rest.iter().position(|&b| b == b'\n') {
                Some(p) => (
                    String::from_utf8_lossy(&rest[..p]).into_owned(),
                    String::from_utf8_lossy(&rest[p + 1..]).into_owned(),
                ),
                None => (String::from_utf8_lossy(rest).into_owned(), String::new()),
            };
            Event::Message { conv, from, seq, text }
        }
        MsgType::Delivered => {
            if f.body.len() >= 8 {
                let seq = u64::from_le_bytes(f.body[..8].try_into().unwrap());
                Event::Delivered { seq }
            } else {
                Event::Error { msg: "malformed Delivered frame".into() }
            }
        }
        MsgType::Pong => Event::Pong,
        MsgType::Error => {
            Event::Error { msg: String::from_utf8_lossy(&f.body).into_owned() }
        }
        MsgType::Goodbye => Event::Disconnected { reason: "goodbye".into() },
        other => Event::Error { msg: format!("unhandled message type: {other:?}") },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(action: Action) -> OwnedFrame {
        let mut auth = None;
        let (ty, body) = encode_action(action, &mut auth).expect("encodes");
        let bytes = protocol::encode(ty, 0, &body);
        match protocol::decode(&bytes) {
            Decode::Complete { frame, .. } => OwnedFrame::from_borrowed(&frame),
            _ => panic!("decode failed"),
        }
    }

    #[test]
    fn hello_roundtrip() {
        let f = roundtrip(Action::Hello {
            user: "alice".into(),
            password: "hunter2".into(),
        });
        assert_eq!(f.msg_type, MsgType::Hello);
        assert_eq!(&*f.body, b"alice\nhunter2");
    }

    #[test]
    fn create_conv_prepends_user() {
        let mut auth = None;
        let (ty, _) = encode_action(
            Action::Hello { user: "alice".into(), password: "x".into() },
            &mut auth,
        )
        .expect("hello encodes");
        assert_eq!(ty, MsgType::Hello);

        let (ty, body) = encode_action(
            Action::CreateConv { members: vec!["bob".into()] },
            &mut auth,
        )
        .expect("create encodes");
        assert_eq!(ty, MsgType::CreateConv);
        // the authenticated user is prepended automatically
        assert_eq!(&*body, b"alice,bob");
    }

    #[test]
    fn send_roundtrip() {
        let conv = vec![0u8; 8];
        let f = roundtrip(Action::Send {
            conv: conv.clone(),
            text: "hello world".into(),
        });
        assert_eq!(f.msg_type, MsgType::Send);
        let mut want = conv;
        want.push(b'\n');
        want.extend_from_slice(b"hello world");
        assert_eq!(&*f.body, &want[..]);
    }

    fn event_from(msg_type: MsgType, body: &[u8]) -> Event {
        let bytes = protocol::encode(msg_type, 0, body);
        match protocol::decode(&bytes) {
            Decode::Complete { frame, .. } => decode_event(OwnedFrame::from_borrowed(&frame)),
            _ => panic!("decode failed"),
        }
    }

    #[test]
    fn auth_events() {
        assert_eq!(event_from(MsgType::AuthOk, &[0]), Event::AuthOk { created: false });
        assert_eq!(event_from(MsgType::AuthOk, &[1]), Event::AuthOk { created: true });
        assert_eq!(
            event_from(MsgType::AuthFail, b"invalid_password"),
            Event::AuthFail { reason: "invalid_password".into() }
        );
    }

    #[test]
    fn inbound_message_event() {
        let mut conv = vec![0xAA; 8];
        conv.push(b'\n');
        conv.extend_from_slice(&42u64.to_le_bytes());
        conv.extend_from_slice(b"carol\nhey!");
        let ev = event_from(MsgType::Send, &conv);
        assert_eq!(
            ev,
            Event::Message {
                conv: vec![0xAA; 8],
                from: "carol".into(),
                seq: 42,
                text: "hey!".into(),
            }
        );
    }

    #[test]
    fn convs_and_delivered() {
        let mut body = vec![0u8; 16];
        body[..8].copy_from_slice(&[1u8; 8]);
        body[8..].copy_from_slice(&[2u8; 8]);
        assert_eq!(
            event_from(MsgType::ConvsResp, &body),
            Event::Convs { ids: vec![vec![1u8; 8], vec![2u8; 8]] }
        );
        assert_eq!(
            event_from(MsgType::Delivered, &7u64.to_le_bytes()),
            Event::Delivered { seq: 7 }
        );
    }
}
