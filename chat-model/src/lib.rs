//! high-level chat model shared by the channel-based client and the TUI
//! frontend
//!
//! [`Action`] is what a caller asks a connected server to do; [`Event`] is a
//! notification the server produces. Both are pure data with no wire knowledge
//! — all framing lives behind the `protocol` crate. Because the client and the
//! TUI speak the same model, no adapter layer is needed between them.

#![forbid(unsafe_code)]

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

/// a high-level notification produced by the server
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
