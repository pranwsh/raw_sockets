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
pub use link::Transport;

// per-message wire↔model bridge modules; the generated `encode_action` and
// `decode_event` live in `msgs`
mod link;
mod msgs;
use link::{Closer, LinkReader, LinkWriter};
use msgs::{decode_event, encode_action};

use protocol::{self, OwnedFrame};
use std::io;
use std::sync::mpsc;
use std::thread;

/// a connected client: send [`Action`]s in, receive [`Event`]s out
///
/// The socket is shared by two threads. A blocking reader thread owns the read
/// half and parks in the kernel until data arrives, so an idle client costs no
/// CPU. A second thread drains [`Action`]s and writes them as soon as they are
/// queued, so outbound latency does not depend on inbound traffic.
pub struct Client {
    action_tx: mpsc::Sender<Action>,
    event_rx: mpsc::Receiver<Event>,
    /// kept only so `Drop` can unblock the parked reader
    closer: Option<Box<dyn Closer>>,
}

impl std::fmt::Debug for Client {
    /// deliberately hand-written: the link is a trait object, and `Event` is a
    /// large enum whose derived output is noisy in a log
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Client")
            .field("connected", &self.closer.is_some())
            .finish_non_exhaustive()
    }
}

impl Client {
    /// open a TCP connection to the messaging server at `host:port`
    pub fn connect(host: &str, port: u16) -> io::Result<Client> {
        Self::connect_with(Transport::Tcp, host, port)
    }

    /// open a connection using `transport`
    ///
    /// For [`Transport::RawIp`] the `host` must be a local interface address:
    /// raw IP is not routed, and the client needs `CAP_NET_RAW` (run as root, or
    /// grant it with `setcap cap_net_raw+ep`). A raw-IP client also picks its own
    /// ephemeral source port.
    pub fn connect_with(transport: Transport, host: &str, port: u16) -> io::Result<Client> {
        let parts = match link::open(transport, host, port) {
            Ok(p) => p,
            Err(e) if transport == Transport::RawIp
                && e.kind() == io::ErrorKind::PermissionDenied =>
            {
                return Err(io::Error::new(
                    e.kind(),
                    format!(
                        "raw-ip needs CAP_NET_RAW: {e}. Run as root, or grant it with \
                         `setcap cap_net_raw+ep` on the binary, or use TCP."
                    ),
                ));
            }
            Err(e) => return Err(e),
        };

        let (action_tx, action_rx) = mpsc::channel();
        let (event_tx, event_rx) = mpsc::channel();
        let _ = event_tx.send(Event::Connected);

        // The reader and writer run on separate threads and share no lock: the
        // reader parks waiting for inbound data, and a shared mutex would keep it
        // from ever sending. `closer` lets `Drop` unblock the reader.
        let mut reader = parts.reader;
        let mut writer = parts.writer;
        thread::spawn(move || read_loop(&mut *reader, event_tx));
        thread::spawn(move || write_loop(&mut *writer, action_rx));

        Ok(Client {
            action_tx,
            event_rx,
            closer: Some(parts.closer),
        })
    }

    /// queue an action to be sent; returns false if the connection is gone
    pub fn send(&self, action: Action) -> bool {
        self.action_tx.send(action).is_ok()
    }

    /// the event channel; poll it with `try_iter`/`recv` to consume server events
    pub fn events(&self) -> &mpsc::Receiver<Event> {
        &self.event_rx
    }

    /// drain every pending [`Event`] into `out`, reusing its allocation.
    ///
    /// The frontend calls this once per drawn frame, so letting the caller own
    /// the buffer avoids a fresh `Vec` allocation on every frame. Returns the
    /// number of events drained.
    pub fn drain_events(&self, out: &mut Vec<Event>) -> usize {
        let mut n = 0;
        while let Ok(ev) = self.event_rx.try_recv() {
            out.push(ev);
            n += 1;
        }
        n
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        // Closing the link makes the reader's parked read return immediately
        // instead of holding the thread until the peer disconnects.
        if let Some(c) = self.closer.take() {
            c.close();
        }
    }
}

// reader thread — the only place that decodes inbound frames

fn read_loop(reader: &mut dyn LinkReader, event_tx: mpsc::Sender<Event>) {
    loop {
        match reader.read() {
            Ok(frames) => {
                if frames.is_empty() {
                    continue;
                }
                for (msg_type, body) in frames {
                    let owned = OwnedFrame {
                        version: protocol::VERSION,
                        flags: 0,
                        msg_type,
                        body: body.into_boxed_slice(),
                    };
                    if event_tx.send(decode_event(owned)).is_err() {
                        return; // receiver dropped; nothing left to do
                    }
                }
            }
            Err(_) => break,
        }
    }

    let _ = event_tx.send(Event::Disconnected {
        reason: "connection closed".into(),
    });
}

// writer thread — parks on the action channel, writes frames as they arrive

fn write_loop(writer: &mut dyn LinkWriter, action_rx: mpsc::Receiver<Action>) {
    let mut authenticated: Option<Vec<u8>> = None;
    // reused across writes so a steady send loop allocates no frame buffers
    let mut frame: Vec<u8> = Vec::with_capacity(1024);

    // `recv` blocks until an action is queued: no polling, no wakeups while idle
    while let Ok(action) = action_rx.recv() {
        let Some((msg_type, body)) = encode_action(&action, &mut authenticated) else {
            continue;
        };
        let total = protocol::frame_len(body.len());
        frame.clear();
        frame.resize(total, 0);
        // `seal` writes header+CRC in place, so the body is copied in exactly once
        frame[protocol::HEADER_LEN..protocol::HEADER_LEN + body.len()].copy_from_slice(&body);
        protocol::seal(&mut frame, msg_type, 0, body.len());

        if writer.write_frame(&frame).is_err() {
            break;
        }
    }
    // signal a clean end-of-stream so the server sees a prompt close rather
    // than waiting out its own keepalive
    writer.close();
}

// Action → wire frame conversions and wire frame → Event conversions are
// generated from the per-message bridge modules in `msgs` (see `msgs/mod.rs`).

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::Decode;
    use protocol::MsgType;
    use chat_model::ConvInfo;

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
            &Action::Hello {
                user: "alice".into(),
                password: "x".into(),
            },
            &mut auth,
        )
        .expect("hello encodes");
        assert_eq!(ty, MsgType::Hello);

        let (ty, body) = encode_action(
            &Action::CreateConv {
                members: vec!["bob".into()],
            },
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
        assert_eq!(
            event_from(MsgType::AuthOk, &[0]),
            Event::AuthOk { created: false }
        );
        assert_eq!(
            event_from(MsgType::AuthOk, &[1]),
            Event::AuthOk { created: true }
        );
        assert_eq!(
            event_from(MsgType::AuthFail, b"invalid_password"),
            Event::AuthFail {
                reason: "invalid_password".into()
            }
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
    fn convs_carry_member_names() {
        // each record is `<8-byte id><comma-separated members>\n`
        let mut body = Vec::new();
        body.extend_from_slice(&[1u8; 8]);
        body.extend_from_slice(b"alice,bob\n");
        body.extend_from_slice(&[2u8; 8]);
        body.extend_from_slice(b"carol,dave,erin\n");
        // a record without its terminator is malformed
        assert!(event_from(MsgType::ConvsResp, b"\x01\x01\x01\x01\x01\x01\x01\x01alice")
            != Event::Convs { convs: vec![] });
        assert_eq!(
            event_from(MsgType::ConvsResp, &body),
            Event::Convs {
                convs: vec![
                    ConvInfo { id: vec![1u8; 8], members: vec!["alice".into(), "bob".into()] },
                    ConvInfo {
                        id: vec![2u8; 8],
                        members: vec!["carol".into(), "dave".into(), "erin".into()],
                    },
                ],
            }
        );
    }

    #[test]
    fn empty_convs_body_is_empty_listing() {
        assert_eq!(event_from(MsgType::ConvsResp, b""), Event::Convs { convs: vec![] });
    }

    #[test]
    fn delivered_seq() {
        assert_eq!(
            event_from(MsgType::Delivered, &7u64.to_le_bytes()),
            Event::Delivered { seq: 7 }
        );
    }
}
