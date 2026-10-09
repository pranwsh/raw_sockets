//! non-blocking transport layer

// SAFETY: sys contains raw libc syscall wrappers with documented invariants

pub mod conn;
pub mod packet;
pub mod reactor;
pub mod sys;

pub use conn::{ConnectionId, TeardownReason};
pub use reactor::{EventHandler, Reactor};
