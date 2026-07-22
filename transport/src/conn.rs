//! Per-connection state machine, write buffer, backpressure policy, teardown.
//!
//! A [`Connection`] owns:
//! - A raw socket fd (`O_NONBLOCK`, managed by the reactor).
//! - A growable read buffer into which `recv()` bytes are appended; the
//!   protocol decoder consumes complete frames from the front.
//! - A bounded ring-style write buffer (two `Vec<u8>` segments so we can
//!   drain the front without memmoving the tail — a poor-man's
//!   io-uring-style buffer chain, but simpler).
//! - A deadline for idle disconnect.
//! - A [`ConnectionId`] assigned by the reactor at creation time.
//!
//! **Backpressure policy** (the single, explicit policy for slow clients):
//!
//! | Threshold          | Action                                                       |
//! |-------------------|--------------------------------------------------------------|
//! | < `SOFT_LIMIT`    | Read and write as normal.                                    |
//! | SOFT .. HARD      | Pause reads from this connection (don't register EPOLLIN).   |
//! | ≥ `HARD_CAP`      | Drop the newest frame and queue a `SlowConsumer` error frame,|
//! |                   | then schedule teardown with `Reason::SlowConsumer`. The      |
//! |                   | connection is dead — the write buffer is flushed best-effort  |
//! |                   | before the fd closes.                                        |
//!
//! The objective: no single slow reader or attacker can exhaust heap by
//! forcing endless buffering. Caps are per-connection; one slow peer affects
//! only itself, not the shard's memory budget. A future improvement would be
//! a global memory pool with per-connection quotas enforced at the reactor
//! level, but the per-connection cap is the correctness floor.

#![forbid(unsafe_code)]

use crate::sys;
use protocol::{self, Decode, DecodeError, OwnedFrame};
use std::io;
use std::os::unix::io::RawFd;
use std::time::Duration;

// ---------------------------------------------------------------------------
// Constants — these should be tuned by benchmark; values below are
// conservative starting points.
// ---------------------------------------------------------------------------

/// When buffered bytes for this connection's outbound exceed this, stop
/// reading from the socket (let the peer drain before we accept more).
pub const WRITE_BUFFER_SOFT_LIMIT: usize = 256 * 1024;

/// Absolute cap: when outbound exceeds this, the connection is torn down
/// with `Reason::SlowClient`. Must be ≥ WRITE_BUFFER_SOFT_LIMIT.
pub const WRITE_BUFFER_HARD_CAP: usize = 1 * 1024 * 1024;

/// Maximum size of the read buffer before we disconnect the client (anti-OOM).
pub const READ_BUFFER_HARD_CAP: usize = 1 * 1024 * 1024;

/// Default idle timeout.
pub const DEFAULT_IDLE_TIMEOUT: Duration = Duration::from_secs(60);

/// After a teardown is initiated, how long we keep the fd around to flush
/// remaining outbound data before closing.
pub const DRAIN_TIMEOUT_MS: u64 = 3_000;

// ---------------------------------------------------------------------------
// ConnectionId
// ---------------------------------------------------------------------------

/// Opaque u64 handle. The reactor allocates these from a monotonically
/// increasing counter; the value is stored in epoll's `epoll_event.u64`
/// so completed I/O is routed back to the correct connection without a
/// hash lookup.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ConnectionId(pub u64);

impl From<ConnectionId> for u64 {
    fn from(id: ConnectionId) -> Self {
        id.0
    }
}

// ---------------------------------------------------------------------------
// TeardownReason
// ---------------------------------------------------------------------------

/// Why a connection was torn down. Every branch records one of these.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TeardownReason {
    /// Peer sent a clean `Goodbye` frame.
    ClientGoodbye,
    /// Idle timeout — no frames received within the deadline.
    IdleTimeout,
    /// Unparseable frame — protocol mismatch or corruption.
    ProtocolViolation,
    /// Peer reset (RST) or closed without Goodbye (EOF).
    PeerClosed,
    /// Outbound buffer hit `HARD_CAP`.
    SlowClient,
    /// Server is shutting down this shard.
    Shutdown,
}

// ---------------------------------------------------------------------------
// WriteBuffer
// ---------------------------------------------------------------------------

/// A two-segment write buffer. Writing appends to the active segment; the
/// I/O dispatch drains from `front()` and advances a cursor — no memmove
/// on partial writes. When the cursor reaches the end of `buf_a`, we swap
/// in `buf_b` (if any) and reset.
#[derive(Debug)]
struct WriteBuffer {
    buf_a: Vec<u8>,
    buf_b: Vec<u8>,
    /// Cursor into `buf_a` (bytes already written to the kernel).
    cursor: usize,
}

impl WriteBuffer {
    fn new() -> Self {
        // Pre-allocate to avoid first-write latency spike.
        Self { buf_a: Vec::with_capacity(4096), buf_b: Vec::with_capacity(4096), cursor: 0 }
    }

    fn len(&self) -> usize {
        (self.buf_a.len() - self.cursor) + self.buf_b.len()
    }

    fn is_empty(&self) -> bool {
        self.cursor >= self.buf_a.len() && self.buf_b.is_empty()
    }

    /// Push a new frame into the write buffer. If the total exceeds
    /// `HARD_CAP`, returns `Err` and does NOT push — the caller decides
    /// teardown, not the buffer.
    fn push(&mut self, bytes: &[u8]) -> Result<(), ()> {
        if self.len() + bytes.len() > WRITE_BUFFER_HARD_CAP {
            return Err(());
        }
        // Try to extend `buf_a` if the cursor hasn't advanced far; otherwise
        // push onto `buf_b` to avoid overwriting unsent bytes.
        if self.cursor == 0 && self.buf_b.is_empty() {
            self.buf_a.extend_from_slice(bytes);
        } else {
            self.buf_b.extend_from_slice(bytes);
        }
        Ok(())
    }

    /// Returns the slice of bytes still to be written (the I/O source).
    fn front(&self) -> &[u8] {
        if self.cursor < self.buf_a.len() {
            &self.buf_a[self.cursor..]
        } else if !self.buf_b.is_empty() {
            &self.buf_b[self.cursor.saturating_sub(self.buf_a.len())..]
        } else {
            &[]
        }
    }

    /// Advance the write cursor by `n` bytes (after a successful `send`).
    fn advance(&mut self, n: usize) {
        let remaining_in_a = self.buf_a.len() - self.cursor;
        if n < remaining_in_a {
            self.cursor += n;
        } else {
            // Finished `buf_a`; drain `buf_b`.
            self.cursor = self.buf_a.len() + (n - remaining_in_a);
            if self.cursor >= self.buf_a.len() + self.buf_b.len() {
                // Fully drained.
                self.buf_a.clear();
                std::mem::swap(&mut self.buf_a, &mut self.buf_b);
                self.cursor = 0;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Connection
// ---------------------------------------------------------------------------

/// Per-connection state.
#[derive(Debug)]
pub struct Connection {
    pub id: ConnectionId,
    pub fd: RawFd,
    /// Peer address for logging / routing decisions.
    pub peer: std::net::SocketAddrV4,
    /// Monotonic-ms deadline for idle disconnect.
    pub idle_deadline: u64,

    // Read side
    read_buf: Vec<u8>,
    /// Offset of unconsumed data in `read_buf` (to avoid memmove on partial
    /// decode, we delay compacting until the consumed portion is large).
    read_offset: usize,

    // Write side
    write_buf: WriteBuffer,
    /// True when we've asked the reactor to poll for EPOLLOUT (because
    /// we have pending data).
    pub write_pending: bool,

    /// If `Some`, the connection is in its teardown phase.
    pub teardown: Option<TeardownState>,

    /// Whether we should keep reading from this socket (soft backpressure).
    pub read_paused: bool,
}

/// A connection passes through a single teardown phase — flush remaining
/// writes, then close.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TeardownState {
    pub reason: TeardownReason,
    /// Deadline after which we stop flushing and close.
    pub flush_deadline: u64,
    /// Whether we've already queued a `Goodbye` or `Error` frame.
    pub sent_farewell: bool,
}

impl Connection {
    /// Create a new connection. The fd is expected to already be `O_NONBLOCK`
    /// (set by `accept` via `SOCK_NONBLOCK`).
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

    /// Reset the idle timer (call after every successfully decoded frame).
    pub fn touch_idle(&mut self) {
        self.idle_deadline = sys::deadline_after(DEFAULT_IDLE_TIMEOUT);
    }

    // -----------------------------------------------------------------------
    // Read path
    // -----------------------------------------------------------------------

    /// Feed bytes from the kernel into the read buffer. Returns the number of
    /// bytes read. The caller (reactor) checks for partial-read progress.
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

    /// Total bytes buffered for reading (for anti-OOM checks).
    pub fn read_buffered_bytes(&self) -> usize {
        self.read_buf.len()
    }

    /// Try to decode one frame from the read buffer.
    pub fn try_decode_frame(&mut self) -> Result<Option<OwnedFrame>, DecodeError> {
        let offset = self.read_offset;
        let available = &self.read_buf[offset..];
        if available.is_empty() {
            return Ok(None);
        }
        match protocol::decode(available) {
            Decode::Complete { frame, consumed } => {
                let owned = OwnedFrame::from_borrowed(&frame);
                // frame and available borrows are dropped here.
                self.read_offset += consumed;
                self.touch_idle();
                Ok(Some(owned))
            }
            Decode::Need => Ok(None),
            Decode::Err(e) => Err(e),
        }
    }

    // -----------------------------------------------------------------------
    // Write path
    // -----------------------------------------------------------------------

    /// Queue a frame for transmission. Returns `WriteOutcome`.
    pub fn enqueue_frame(&mut self, frame: &[u8]) -> WriteOutcome {
        if self.teardown.is_some() {
            return WriteOutcome::Rejected;
        }
        match self.write_buf.push(frame) {
            Ok(()) => WriteOutcome::Queued,
            Err(()) => WriteOutcome::Overflow,
        }
    }

    /// Flush as many bytes as possible from the write buffer to the kernel.
    /// Returns `Ok(bytes_written)` or the error. `WouldBlock` is handled
    /// internally — the reactor re-registers for EPOLLOUT.
    pub fn do_write(&mut self) -> io::Result<usize> {
        let front = self.write_buf.front();
        if front.is_empty() {
            return Ok(0);
        }
        let n = sys::write(self.fd, front)?;
        self.write_buf.advance(n);

        // Track whether we still need EPOLLOUT.
        self.write_pending = !self.write_buf.is_empty();
        Ok(n)
    }

    /// Returns `true` if the write buffer is empty.
    pub fn is_write_empty(&self) -> bool {
        self.write_buf.is_empty()
    }

    /// Current write-buffer occupancy in bytes.
    pub fn write_buffered_bytes(&self) -> usize {
        self.write_buf.len()
    }

    // -----------------------------------------------------------------------
    // Backpressure: soft limit check
    // -----------------------------------------------------------------------

    /// Whether reading should be paused due to write-buffer pressure.
    pub fn should_pause_reading(&self) -> bool {
        self.write_buf.len() >= WRITE_BUFFER_SOFT_LIMIT
    }

    // -----------------------------------------------------------------------
    // Teardown — the single cleanup path
    // -----------------------------------------------------------------------

    /// Initiate connection teardown. The reactor calls this from every error
    /// branch (malformed frame, idle timeout, peer RST, write exhaustion).
    ///
    /// This does **not** close the fd immediately. Instead it:
    /// 1. Records the reason.
    /// 2. Queues a farewell frame (best-effort).
    /// 3. Shuts down the read side (so the reactor stops polling for reads).
    ///    The write side stays open for a drain window.
    /// 4. Enters the teardown state; the reactor flushes remaining writes
    ///    and then closes the fd on the next tick (or after `DRAIN_TIMEOUT_MS`).
    pub fn start_teardown(&mut self, reason: TeardownReason) {
        if self.teardown.is_some() {
            return; // already tearing down
        }
        // Shutdown reads immediately — we won't process any more input.
        let _ = sys::shutdown(self.fd, libc::SHUT_RD);

        self.teardown = Some(TeardownState {
            reason,
            flush_deadline: sys::now_ms() + DRAIN_TIMEOUT_MS,
            sent_farewell: false,
        });
    }

    /// Called by the reactor on each tick while `teardown` is active.
    /// Returns `true` once the connection can be fully closed and removed.
    pub fn advance_teardown(&mut self) -> bool {
        // Snapshot fields to avoid holding a borrow on self.teardown while
        // calling other &mut self methods.
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

    /// Shutdown writes and close immediately — hard teardown, no drain.
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

// ---------------------------------------------------------------------------
// TeardownReason → farewell frame
// ---------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
// WriteOutcome
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteOutcome {
    Queued,
    Overflow,
    Rejected,
}
