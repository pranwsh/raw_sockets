//! non-blocking transport layer

// SAFETY: sys contains raw libc syscall wrappers with documented invariants

pub mod conn;
pub mod reactor;
pub mod sys;

pub use conn::{Connection, ConnectionId, TeardownReason, WriteOutcome};
pub use reactor::{EventHandler, Reactor};
