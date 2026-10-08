//! channel-based client for the messaging service
//!
//! A [`Client`] connects to a messaging server and exposes two channels:
//! - **in**: high-level [`Action`]s (authenticate, create a conversation, send a message, ping, goodbye)
//! - **out**: high-level [`Event`]s (auth results, incoming messages, deliveries, errors, disconnects)
//!
//! The [`Action`]/[`Event`] model lives in the `chat-model` crate (re-exported
//! here) so the term_render TUI and other frontends share it without any wire
//! knowledge. All framing lives behind the `protocol` crate.

#![forbid(unsafe_code)]

pub use chat_model::{Action, Event};

// per-message wire↔model bridge modules; the generated `encode_action` and
// `decode_event` live in `msgs`
mod msgs;
use msgs::{decode_event, encode_action};

use protocol::{self, Decode, OwnedFrame};
use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

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
        // Disable Nagle: message frames are small and lateness-sensitive, and
        // an unacknowledged in-flight write would otherwise hold up the next
        // frame for up to ~40ms (delayed-ACK).
        stream.set_nodelay(true)?;
        // The background loop does a blocking read and can only drain the
        // action channel (and notice disconnects) when that read returns. A
        // short timeout keeps actions flowing promptly: with 100ms, a queued
        // Send could sit in the channel for ~100ms before being written to the
        // socket, adding noticeable end-to-end latency. 1ms keeps pickup
        // bounded to ~1ms at negligible CPU cost.
        stream.set_read_timeout(Some(Duration::from_millis(1)))?;

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
            if let Some((msg_type, body)) = encode_action(&action, &mut authenticated) {
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

// Action → wire frame conversions and wire frame → Event conversions are
// generated from the per-message bridge modules in `msgs` (see `msgs/mod.rs`).

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::MsgType;

    fn roundtrip(action: Action) -> OwnedFrame {
        let mut auth = None;
        let (ty, body) = encode_action(&action, &mut auth).expect("encodes");
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
            &Action::Hello { user: "alice".into(), password: "x".into() },
            &mut auth,
        )
        .expect("hello encodes");
        assert_eq!(ty, MsgType::Hello);

        let (ty, body) = encode_action(
            &Action::CreateConv { members: vec!["bob".into()] },
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
        conv.push(b'\n');
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
