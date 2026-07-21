//! Non-blocking transport layer.
//!
//! Three concerns, kept separate:
//!
//! - [`conn`] — per-connection state machine: read_into-buffer + frame decoder,
//!   bounded write buffer, backpressure policy, idle timeout. Pure-ish: knows
//!   about non-blocking sockets but not about epoll. Unit-testable with a
//!   fake fd / in-memory buffer when wired against a small abstraction.
//! - [`reactor`] — epoll-based single-threaded event loop. One reactor per
//!   core shard (the server binary spins up N reactors and uses
//!   `SO_REUSEPORT` so multiple shards can accept from the same port without
//!   cross-thread handoff or a shared accept lock).
//! - [`server`] (server binary) wires reactors to the domain layer.
//!
//! Hard requirements this crate owns:
//!   - No blocking calls on the hot path: every socket is `O_NONBLOCK`,
//!     epoll is edge-triggered with explicit `EAGAIN`/`EWOULDBLOCK` handling.
//!   - Partial reads/writes: see [`conn::Connection`] — read appends to a
//!     growable buffer and consumes whole frames from the front; write drains
//!     from the front toward the back. Neither assumes a single syscall
//!     completes a request.
//!   - Backpressure: the outbound buffer per connection is **bounded** at
//!     [`conn::WRITE_BUFFER_HARD_CAP`]. When buffered bytes exceed the soft
//!     cap, we stop reading from that connection (write-side drains first).
//!     At the hard cap, new outbound frames are dropped and an `Error` frame
//!     of kind `SlowConsumer` is queued (best-effort) before the connection
//!     is reset — we don't let one slow reader stall others or balloon memory.
//!   - Teardown: one path — [`conn::Connection::teardown`] — driven by reason
//!     code. Every error branch (malformed frame, idle timeout, write
//!     exhaustion, peer RST) funnels through it; nothing duplicates cleanup.

// SAFETY: `sys` contains raw `libc` syscall wrappers with documented
// invariants. All other modules are `#![forbid(unsafe_code)]`.

pub mod conn;
pub mod reactor;
pub mod sys;

pub use conn::{Connection, ConnectionId, TeardownReason, WriteOutcome};
pub use reactor::{EventHandler, Reactor};
