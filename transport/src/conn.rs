//! per-connection state machine, write buffer, backpressure policy, teardown

#![forbid(unsafe_code)]

use crate::sys;
use protocol::{self, Decode, DecodeError, OwnedFrame};
use std::io;
use std::os::unix::io::RawFd;
use std::time::Duration;

// constants — these should be tuned by benchmark; values below are conservative starting points

/// when buffered bytes for this connection's outbound exceed this, stop reading from the socket (let the peer drain before we accept more)
pub const WRITE_BUFFER_SOFT_LIMIT: usize = 256 * 1024;

/// absolute cap: when outbound exceeds this, the connection is torn down with Reason::SlowClient must be ≥ WRITE_BUFFER_SOFT_LIMIT
pub const WRITE_BUFFER_HARD_CAP: usize = 1 * 1024 * 1024;

/// maximum size of the read buffer before we disconnect the client (anti-OOM)
pub const READ_BUFFER_HARD_CAP: usize = 1 * 1024 * 1024;

/// default idle timeout
pub const DEFAULT_IDLE_TIMEOUT: Duration = Duration::from_secs(60);

/// after a teardown is initiated, how long we keep the fd around to flush remaining outbound data before closing
pub const DRAIN_TIMEOUT_MS: u64 = 3_000;

// ConnectionId

/// opaque u64 handle
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ConnectionId(pub u64);

impl From<ConnectionId> for u64 {
    fn from(id: ConnectionId) -> Self {
        id.0
    }
}

// TeardownReason

/// why a connection was torn down
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TeardownReason {
    /// peer sent a clean Goodbye frame
    ClientGoodbye,
    /// idle timeout — no frames received within the deadline
    IdleTimeout,
    /// unparseable frame — protocol mismatch or corruption
    ProtocolViolation,
    /// peer reset (RST) or closed without goodbye (EOF)
    PeerClosed,
    /// outbound buffer hit HARD_CAP
    SlowClient,
    /// server is shutting down this shard
    Shutdown,
}

// WriteBuffer

/// a two-segment write buffer
#[derive(Debug)]
struct WriteBuffer {
    buf_a: Vec<u8>,
    buf_b: Vec<u8>,
    /// cursor into buf_a (bytes already written to the kernel)
    cursor: usize,
}

impl WriteBuffer {
    fn new() -> Self {
        // pre-allocate to avoid first-write latency spike
        Self { buf_a: Vec::with_capacity(4096), buf_b: Vec::with_capacity(4096), cursor: 0 }
    }

    fn len(&self) -> usize {
        (self.buf_a.len() - self.cursor) + self.buf_b.len()
    }

    fn is_empty(&self) -> bool {
        self.cursor >= self.buf_a.len() && self.buf_b.is_empty()
    }

    /// push a new frame into the write buffer
    fn push(&mut self, bytes: &[u8]) -> Result<(), ()> {
        if self.len() + bytes.len() > WRITE_BUFFER_HARD_CAP {
            return Err(());
        }
        // try to extend buf_a if the cursor hasn't advanced far; otherwise push onto buf_b to avoid overwriting unsent bytes
        if self.cursor == 0 && self.buf_b.is_empty() {
            self.buf_a.extend_from_slice(bytes);
        } else {
            self.buf_b.extend_from_slice(bytes);
        }
        Ok(())
    }

    /// returns the slice of bytes still to be written (the i/o source)
    fn front(&self) -> &[u8] {
        if self.cursor < self.buf_a.len() {
            &self.buf_a[self.cursor..]
        } else if !self.buf_b.is_empty() {
            &self.buf_b[self.cursor.saturating_sub(self.buf_a.len())..]
        } else {
            &[]
        }
    }

    /// advance the write cursor by n bytes (after a successful send)
    fn advance(&mut self, n: usize) {
        let remaining_in_a = self.buf_a.len() - self.cursor;
        if n < remaining_in_a {
            self.cursor += n;
        } else {
            // finished buf_a; drain buf_b
            self.cursor = self.buf_a.len() + (n - remaining_in_a);
            if self.cursor >= self.buf_a.len() + self.buf_b.len() {
                // fully drained
                self.buf_a.clear();
                std::mem::swap(&mut self.buf_a, &mut self.buf_b);
                self.cursor = 0;
            }
        }
    }
}

// connection

/// per-connection state
#[derive(Debug)]
pub struct Connection {
    pub id: ConnectionId,
    pub fd: RawFd,
    /// peer address for logging / routing decisions
    pub peer: std::net::SocketAddrV4,
    /// monotonic-ms deadline for idle disconnect
    pub idle_deadline: u64,

    // read side
    read_buf: Vec<u8>,
    /// offset of unconsumed data in read_buf (to avoid memmove on partial decode, we delay compacting until the consumed portion is large)
    read_offset: usize,

    // write side
    write_buf: WriteBuffer,
    /// true when we've asked the reactor to poll for EPOLLOUT (because we have pending data)
    pub write_pending: bool,

    /// if Some, the connection is in its teardown phase
    pub teardown: Option<TeardownState>,

    /// whether we should keep reading from this socket (soft backpressure)
    pub read_paused: bool,
}

/// a connection passes through a single teardown phase — flush remaining writes, then close
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TeardownState {
    pub reason: TeardownReason,
    /// deadline after which we stop flushing and close
    pub flush_deadline: u64,
    /// whether we've already queued a Goodbye or Error frame
    pub sent_farewell: bool,
}

impl Connection {
    /// create a new connection
    pub fn new(fd: RawFd, peer: std::net::SocketAddrV4, id: ConnectionId) -> Self {
        let deadline = sys::deadline_after(DEFAULT_IDLE_TIMEOUT);
        Self {
            id,
            fd,
            peer,
            idle_deadline: deadline,
            read_buf: Vec::with_capacity(8192),
            read_offset: 0,
            write_buf: WriteBuffer::new(),
            write_pending: false,
            teardown: None,
            read_paused: false,
        }
    }

    /// reset the idle timer (call after every successfully decoded frame)
    pub fn touch_idle(&mut self) {
        self.idle_deadline = sys::deadline_after(DEFAULT_IDLE_TIMEOUT);
    }

    // read path

    /// feed bytes from the kernel into the read buffer
    pub fn do_read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = sys::read(self.fd, buf)?;
        if n == 0 {
            return Err(io::Error::new(io::ErrorKind::ConnectionReset, "peer closed"));
        }
        self.read_buf.extend_from_slice(&buf[..n]);

        if self.read_offset > 32 * 1024 {
            self.read_buf.drain(..self.read_offset);
            self.read_offset = 0;
        }

        Ok(n)
    }

    /// total bytes buffered for reading (for anti-OOM checks)
    pub fn read_buffered_bytes(&self) -> usize {
        self.read_buf.len()
    }

    /// try to decode one frame from the read buffer
    pub fn try_decode_frame(&mut self) -> Result<Option<OwnedFrame>, DecodeError> {
        let offset = self.read_offset;
        let available = &self.read_buf[offset..];
        if available.is_empty() {
            return Ok(None);
        }
        match protocol::decode(available) {
            Decode::Complete { frame, consumed } => {
                let owned = OwnedFrame::from_borrowed(&frame);
                // frame and available borrows are dropped here
                self.read_offset += consumed;
                self.touch_idle();
                Ok(Some(owned))
            }
            Decode::Need => Ok(None),
            Decode::Err(e) => Err(e),
        }
    }

    // write path

    /// queue a frame for transmission
    pub fn enqueue_frame(&mut self, frame: &[u8]) -> WriteOutcome {
        if self.teardown.is_some() {
            return WriteOutcome::Rejected;
        }
        match self.write_buf.push(frame) {
            Ok(()) => WriteOutcome::Queued,
            Err(()) => WriteOutcome::Overflow,
        }
    }

    /// flush as many bytes as possible from the write buffer to the kernel
    pub fn do_write(&mut self) -> io::Result<usize> {
        let front = self.write_buf.front();
        if front.is_empty() {
            return Ok(0);
        }
        let n = sys::write(self.fd, front)?;
        self.write_buf.advance(n);

        // track whether we still need EPOLLOUT
        self.write_pending = !self.write_buf.is_empty();
        Ok(n)
    }

    /// returns true if the write buffer is empty
    pub fn is_write_empty(&self) -> bool {
        self.write_buf.is_empty()
    }

    /// current write-buffer occupancy in bytes
    pub fn write_buffered_bytes(&self) -> usize {
        self.write_buf.len()
    }

    // backpressure: soft limit check

    /// whether reading should be paused due to write-buffer pressure
    pub fn should_pause_reading(&self) -> bool {
        self.write_buf.len() >= WRITE_BUFFER_SOFT_LIMIT
    }

    // teardown — the single cleanup path

    /// initiate connection teardown
    pub fn start_teardown(&mut self, reason: TeardownReason) {
        if self.teardown.is_some() {
            return; // already tearing down
        }
        // shutdown reads immediately — we won't process any more input
        let _ = sys::shutdown(self.fd, libc::SHUT_RD);

        self.teardown = Some(TeardownState {
            reason,
            flush_deadline: sys::now_ms() + DRAIN_TIMEOUT_MS,
            sent_farewell: false,
        });
    }

    /// called by the reactor on each tick while teardown is active
    pub fn advance_teardown(&mut self) -> bool {
        // snapshot fields to avoid holding a borrow on self.teardown while calling other &mut self methods
        let (reason, flush_deadline, sent_farewell) = match &self.teardown {
            Some(s) => (s.reason, s.flush_deadline, s.sent_farewell),
            None => return true,
        };

        if !sent_farewell {
            let farewell = reason.farewell_frame();
            if self.enqueue_frame(&farewell) == WriteOutcome::Queued {
                self.write_pending = true;
            }
            self.teardown.as_mut().unwrap().sent_farewell = true;
        }

        if !self.write_buf.is_empty() {
            let _ = self.do_write();
        }

        if self.write_buf.is_empty() || sys::now_ms() >= flush_deadline {
            sys::close(self.fd);
            return true;
        }

        false
    }

    /// shutdown writes and close immediately — hard teardown, no drain
    pub fn close_immediately(&mut self) {
        let _ = sys::shutdown(self.fd, libc::SHUT_RDWR);
        sys::close(self.fd);
        self.teardown = Some(TeardownState {
            reason: TeardownReason::Shutdown,
            flush_deadline: 0,
            sent_farewell: true,
        });
    }
}

// TeardownReason → farewell frame

impl TeardownReason {
    fn farewell_frame(self) -> Box<[u8]> {
        let body: &[u8] = match self {
            TeardownReason::ClientGoodbye => b"goodbye",
            TeardownReason::IdleTimeout => b"idle_timeout",
            TeardownReason::ProtocolViolation => b"proto_error",
            TeardownReason::PeerClosed => b"peer_closed",
            TeardownReason::SlowClient => b"slow_client",
            TeardownReason::Shutdown => b"shutdown",
        };
        let msg_type = match self {
            TeardownReason::ClientGoodbye => protocol::MsgType::Goodbye,
            _ => protocol::MsgType::Error,
        };
        protocol::encode(msg_type, 0, body)
    }
}

// WriteOutcome

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteOutcome {
    Queued,
    Overflow,
    Rejected,
}
