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
use std::net::{Shutdown, TcpStream};
use std::sync::mpsc;
use std::thread;

/// a connected client: send [`Action`]s in, receive [`Event`]s out
///
/// The socket is shared by two threads. A blocking reader thread owns the read
/// half and parks in the kernel until data arrives, so an idle client costs no
/// CPU. A second thread drains [`Action`]s and writes them as soon as they are
/// queued, so outbound latency does not depend on inbound traffic.
#[derive(Debug)]
pub struct Client {
    action_tx: mpsc::Sender<Action>,
    event_rx: mpsc::Receiver<Event>,
    /// a clone kept only so `Drop` can unblock the parked reader
    closer: Option<TcpStream>,
}

impl Client {
    /// open a connection to the messaging server at `host:port` and spawn the
    /// background threads that own the socket. Returns once the TCP connection
    /// is established.
    pub fn connect(host: &str, port: u16) -> io::Result<Client> {
        let addr = format!("{}:{}", host, port);
        let stream = TcpStream::connect(&addr)?;
        // Disable Nagle: message frames are small and lateness-sensitive, and
        // an unacknowledged in-flight write would otherwise hold up the next
        // frame for up to ~40ms (delayed-ACK).
        stream.set_nodelay(true)?;

        // `try_clone` dups the fd, so both halves refer to the same connection.
        // Only the reader sets a read timeout: it stays a *blocking* read with a
        // generous ceiling, so a quiet connection never turns into a busy loop
        // while a silent peer is still eventually noticed.
        let reader_stream = stream.try_clone()?;
        reader_stream.set_read_timeout(Some(READ_PARK_TIMEOUT))?;

        let (action_tx, action_rx) = mpsc::channel();
        let (event_tx, event_rx) = mpsc::channel();
        let _ = event_tx.send(Event::Connected);

        let closer = stream.try_clone()?;
        let writer_stream = stream;
        thread::spawn(move || read_loop(reader_stream, event_tx));
        thread::spawn(move || write_loop(writer_stream, action_rx));

        Ok(Client {
            action_tx,
            event_rx,
            closer: Some(closer),
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
        // Shutting down both halves makes the reader's parked `read` return
        // immediately instead of holding the thread until the peer disconnects.
        if let Some(s) = self.closer.take() {
            let _ = s.shutdown(Shutdown::Both);
        }
    }
}

// how long the reader may park in the kernel before re-checking liveness. This
// bounds how long a silent server takes to be noticed; it is not a latency
// path — a healthy connection is woken by the kernel the moment bytes arrive.
const READ_PARK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

// reader thread — the only place that decodes inbound frames

fn read_loop(mut stream: TcpStream, event_tx: mpsc::Sender<Event>) {
    // amortised: the buffer grows to the largest frame seen and is then reused
    let mut buf = Vec::with_capacity(READ_BUF_INIT);
    let mut scratch = vec![0u8; READ_CHUNK];
    let mut offset = 0usize;

    loop {
        // compact first so `buf[offset..]` is the unconsumed tail; `buf` grows
        // monotonically to the largest frame instead of being drained each round
        if offset > 0 {
            buf.copy_within(offset.., 0);
            buf.truncate(buf.len() - offset);
            offset = 0;
        }

        match stream.read(&mut scratch) {
            Ok(0) => break, // clean EOF — server closed
            Ok(n) => buf.extend_from_slice(&scratch[..n]),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) if is_park_timeout(&e) => {
                // liveness poll; nothing pending on the wire
                continue;
            }
            Err(_) => break,
        }

        loop {
            match protocol::decode(&buf[offset..]) {
                Decode::Complete { frame, consumed } => {
                    offset += consumed;
                    let owned = OwnedFrame::from_borrowed(&frame);
                    if event_tx.send(decode_event(owned)).is_err() {
                        return; // receiver dropped; nothing left to do
                    }
                }
                Decode::Need => break,
                // misaligned garbage: skip one byte and resync
                Decode::Err(_) => offset += 1,
            }
        }

        if offset == buf.len() {
            // everything consumed; reset to the empty tail
            offset = 0;
            buf.clear();
        }
    }

    let _ = event_tx.send(Event::Disconnected {
        reason: "connection closed".into(),
    });
}

// writer thread — parks on the action channel, writes frames as they arrive

fn write_loop(mut stream: TcpStream, action_rx: mpsc::Receiver<Action>) {
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
        if stream.write_all(&frame).is_err() {
            break;
        }
    }
    // signal a clean end-of-stream so the server sees a prompt close rather
    // than waiting out its own keepalive
    let _ = stream.shutdown(Shutdown::Write);
}

// buffer sizing

/// initial read-buffer size; grows to fit the largest frame seen
const READ_BUF_INIT: usize = 64 * 1024;
/// kernel read granularity
const READ_CHUNK: usize = 16 * 1024;

#[inline]
fn is_park_timeout(e: &io::Error) -> bool {
    // a blocking socket with SO_RCVTIMEO surfaces both, depending on platform
    matches!(
        e.kind(),
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
    )
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
    fn convs_and_delivered() {
        let mut body = vec![0u8; 16];
        body[..8].copy_from_slice(&[1u8; 8]);
        body[8..].copy_from_slice(&[2u8; 8]);
        assert_eq!(
            event_from(MsgType::ConvsResp, &body),
            Event::Convs {
                ids: vec![vec![1u8; 8], vec![2u8; 8]]
            }
        );
        assert_eq!(
            event_from(MsgType::Delivered, &7u64.to_le_bytes()),
            Event::Delivered { seq: 7 }
        );
    }
}
