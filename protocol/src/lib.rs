//! Versioned binary framing protocol for the messaging service.
//!
//! Wire format (little-endian unless noted):
//!
//! ```text
//!  +--------+--------+--------+--------+--------+--------+--------+--------+
//!  | magic  | ver    | type/flags lo | type/flags hi | body len (u32 LE)  |
//!  +--------+--------+--------+--------+--------+--------+--------+--------+
//!  |                          body (len bytes)                              |
//!  +------------------------------------------------------------------------+
//!  |                       crc32c (u32 LE)                                  |
//!  +------------------------------------------------------------------------+
//! ```
//!
//! - `magic`    = `0x4D` ('M') — fast reject of unrelated traffic.
//! - `ver`      = protocol version; mismatch returns a typed decode error and
//!                lets the caller negotiate/select. Currently `1`.
//! - `type/flags` is a u16: high nibble is flags (compress/ack/req/etc), low
//!                12 bits are message type. Keeps the header tight at 8 bytes.
//! - `body len`  = u32 LE, capped at `MAX_BODY_LEN` to bound buffering memory.
//! - `crc32c`    = Castagnoli CRC32 over (everything from `ver` through end
//!                of body) — detects wire corruption before dispatch.
//!
//! The crate is I/O-free: `Decoder` consumes `&[u8]` slices produced by the
//! transport layer and `Frame::encode` produces owned `Bytes`-like output.
//! The transport crate is responsible for feeding partial socket reads in to
//! the decoder; this crate never touches a socket.

#![forbid(unsafe_code)]

#[cfg(test)]
mod tests;

use core::fmt;

// ----------------------------------------------------------------------------
// Constants
// ----------------------------------------------------------------------------

pub const MAGIC: u8 = 0x4D;
pub const VERSION: u8 = 1;
pub const HEADER_LEN: usize = 8;
pub const TRAILER_LEN: usize = 4; // crc32c
pub const FRAME_OVERHEAD: usize = HEADER_LEN + TRAILER_LEN;
pub const MAX_BODY_LEN: usize = 16 * 1024 * 1024;

pub const FLAG_COMPRESSED: u16 = 1 << 15;
pub const FLAG_ACK_REQ: u16 = 1 << 14;
pub const FLAG_PRIORITY: u16 = 1 << 13;
pub const FLAG_MASK: u16 = 0xE000;
pub const TYPE_MASK: u16 = 0x1FFF;

// ----------------------------------------------------------------------------
// Message types
// ----------------------------------------------------------------------------

/// All message types share one frame envelope. Adding a type is a
/// backward-compatible change; receivers must ignore unknown types rather than
/// tear the connection down (gated by protocol version).
#[repr(u16)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MsgType {
    // Handshake / auth
    Hello = 1,
    AuthChallenge = 2,
    AuthResponse = 3,
    AuthOk = 4,
    AuthFail = 5,
    Goodbye = 6,

    // Account / presence
    Presence = 10,
    Typing = 11,

    // Conversations
    CreateConv = 20,
    ConvCreated = 21,
    ConvInvite = 22,
    ConvJoin = 23,
    ConvLeave = 24,
    ConvMemberEvent = 25,

    // Messaging
    Send = 30,
    Delivered = 31,
    Read = 32,
    HistoryReq = 33,
    HistoryResp = 34,
    InboxFetch = 35,
    InboxResp = 36,

    // Conversation listing
    ListConvs = 37,
    ConvsResp = 38,

    // Routing (inter-node) — uses the same frame on a cluster-internal socket.
    RouteAnnounce = 40,
    RouteDeliver = 41,
    NodeHello = 42,

    // Control
    Ping = 90,
    Pong = 91,
    Error = 99,
}

impl MsgType {
    pub fn from_u16(v: u16) -> Option<Self> {
        Some(match v {
            1 => Self::Hello,
            2 => Self::AuthChallenge,
            3 => Self::AuthResponse,
            4 => Self::AuthOk,
            5 => Self::AuthFail,
            6 => Self::Goodbye,
            10 => Self::Presence,
            11 => Self::Typing,
            20 => Self::CreateConv,
            21 => Self::ConvCreated,
            22 => Self::ConvInvite,
            23 => Self::ConvJoin,
            24 => Self::ConvLeave,
            25 => Self::ConvMemberEvent,
            30 => Self::Send,
            31 => Self::Delivered,
            32 => Self::Read,
            33 => Self::HistoryReq,
            34 => Self::HistoryResp,
            35 => Self::InboxFetch,
            36 => Self::InboxResp,
            37 => Self::ListConvs,
            38 => Self::ConvsResp,
            40 => Self::RouteAnnounce,
            41 => Self::RouteDeliver,
            42 => Self::NodeHello,
            90 => Self::Ping,
            91 => Self::Pong,
            99 => Self::Error,
            _ => return None,
        })
    }
}

// ----------------------------------------------------------------------------
// Frame
// ----------------------------------------------------------------------------

/// A decoded frame. `body` is borrowed from the decoder buffer to avoid copies
/// on the hot path; the caller can `to_owned()` when it needs to outlive the
/// buffer (e.g. when dispatching off the event loop shard).
#[derive(Clone, PartialEq, Eq)]
pub struct Frame<'a> {
    pub version: u8,
    pub flags: u16,
    pub msg_type: MsgType,
    pub body: &'a [u8],
}

impl<'a> Frame<'a> {
    pub fn new(msg_type: MsgType, body: &'a [u8]) -> Self {
        Self { version: VERSION, flags: 0, msg_type, body }
    }

    pub fn with_flags(mut self, flags: u16) -> Self {
        self.flags = flags & FLAG_MASK;
        self
    }

    pub fn is_compressed(&self) -> bool {
        self.flags & FLAG_COMPRESSED != 0
    }

    pub fn ack_requested(&self) -> bool {
        self.flags & FLAG_ACK_REQ != 0
    }

    /// Total on-the-wire length including header and CRC trailer.
    pub fn wire_len(&self) -> usize {
        HEADER_LEN + self.body.len() + TRAILER_LEN
    }
}

impl fmt::Debug for Frame<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Frame")
            .field("version", &self.version)
            .field("flags", &self.flags)
            .field("msg_type", &self.msg_type)
            .field("body_len", &self.body.len())
            .finish()
    }
}

// ----------------------------------------------------------------------------
// Owned frame (for dispatch after the buffer is reused)
// ----------------------------------------------------------------------------

#[derive(Clone, PartialEq, Eq)]
pub struct OwnedFrame {
    pub version: u8,
    pub flags: u16,
    pub msg_type: MsgType,
    pub body: Box<[u8]>,
}

impl OwnedFrame {
    pub fn from_borrowed(f: &Frame<'_>) -> Self {
        Self {
            version: f.version,
            flags: f.flags,
            msg_type: f.msg_type,
            body: f.body.into(),
        }
    }
}

// ----------------------------------------------------------------------------
// Encoder
// ----------------------------------------------------------------------------

/// Encode a frame into `dst`. Caller guarantees `dst.len() >= frame.wire_len()`.
/// Returns the number of bytes written.
pub fn encode_into(dst: &mut [u8], msg_type: MsgType, flags: u16, body: &[u8]) -> usize {
    let total = HEADER_LEN + body.len() + TRAILER_LEN;
    debug_assert!(dst.len() >= total, "encode_into: dst too small");
    debug_assert!(body.len() <= MAX_BODY_LEN, "body too large");

    dst[0] = MAGIC;
    dst[1] = VERSION;
    let tf = (msg_type as u16) & TYPE_MASK | (flags & FLAG_MASK);
    dst[2..4].copy_from_slice(&tf.to_le_bytes());
    dst[4..8].copy_from_slice(&(body.len() as u32).to_le_bytes());
    dst[HEADER_LEN..HEADER_LEN + body.len()].copy_from_slice(body);

    let crc = crc32c(&dst[1..HEADER_LEN + body.len()]);
    dst[HEADER_LEN + body.len()..total].copy_from_slice(&crc.to_le_bytes());
    total
}

/// Convenience: produce an owned encoded buffer.
pub fn encode(msg_type: MsgType, flags: u16, body: &[u8]) -> Box<[u8]> {
    let total = HEADER_LEN + body.len() + TRAILER_LEN;
    let mut buf = vec![0u8; total].into_boxed_slice();
    encode_into(&mut buf, msg_type, flags, body);
    buf
}

// ----------------------------------------------------------------------------
// Decoder
// ----------------------------------------------------------------------------

/// Result of a non-consuming decode attempt against an append-only buffer.
#[derive(Debug, PartialEq, Eq)]
pub enum Decode<'a> {
    /// A complete frame was produced. Its bytes remain in the buffer until
    /// the caller advances past `consumed`; this avoids a copy on paths that
    /// can dispatch straight from the read buffer.
    Complete { frame: Frame<'a>, consumed: usize },
    /// Not enough bytes yet. Caller should read more and retry.
    Need,
    /// Unrecoverable: the caller must tear the connection down via the
    /// single shared teardown path (transport crate), not ad-hoc per error.
    Err(DecodeError),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecodeError {
    /// First byte isn't `MAGIC`. Stream is misaligned / not our protocol.
    BadMagic,
    /// Protocol version we don't speak. Caller may negotiate; default is reset.
    UnsupportedVersion(u8),
    /// Unknown message type. Per spec, ignore rather than reset for forward-compat —
    /// but the caller picks the policy; we surface it.
    UnknownMsgType(u16),
    /// Declared body length exceeds `MAX_BODY_LEN` — almost certainly an attack
    /// or a corrupt length field. Reset.
    BodyTooLarge { declared: usize, max: usize },
    /// CRC mismatch — corrupt frame on the wire. Reset, do not trust stream.
    CrcMismatch,
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BadMagic => write!(f, "bad magic byte"),
            Self::UnsupportedVersion(v) => write!(f, "unsupported protocol version {v}"),
            Self::UnknownMsgType(t) => write!(f, "unknown message type {t}"),
            Self::BodyTooLarge { declared, max } => {
                write!(f, "body length {declared} exceeds max {max}")
            }
            Self::CrcMismatch => write!(f, "crc32c mismatch"),
        }
    }
}

impl core::error::Error for DecodeError {}

/// Decode one frame from the front of `buf`. Does not mutate `buf`; caller
/// advances by `consumed` only on `Complete`.
///
/// This is the only entry point used by the transport's read path. It is
/// written to be branch-predictable on the happy path: header fits -> length
/// plausible -> full frame fits -> crc ok.
pub fn decode(buf: &[u8]) -> Decode<'_> {
    if buf.len() < HEADER_LEN {
        return Decode::Need;
    }
    if buf[0] != MAGIC {
        return Decode::Err(DecodeError::BadMagic);
    }
    let version = buf[1];
    if version != VERSION {
        return Decode::Err(DecodeError::UnsupportedVersion(version));
    }
    let tf = u16::from_le_bytes([buf[2], buf[3]]);
    let ty_raw = tf & TYPE_MASK;
    let Some(msg_type) = MsgType::from_u16(ty_raw) else {
        return Decode::Err(DecodeError::UnknownMsgType(ty_raw));
    };
    let body_len = u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]) as usize;
    if body_len > MAX_BODY_LEN {
        return Decode::Err(DecodeError::BodyTooLarge {
            declared: body_len,
            max: MAX_BODY_LEN,
        });
    }
    let total = HEADER_LEN + body_len + TRAILER_LEN;
    if buf.len() < total {
        return Decode::Need;
    }
    let body = &buf[HEADER_LEN..HEADER_LEN + body_len];
    let want_crc = u32::from_le_bytes([
        buf[HEADER_LEN + body_len],
        buf[HEADER_LEN + body_len + 1],
        buf[HEADER_LEN + body_len + 2],
        buf[HEADER_LEN + body_len + 3],
    ]);
    let got_crc = crc32c(&buf[1..HEADER_LEN + body_len]);
    if want_crc != got_crc {
        return Decode::Err(DecodeError::CrcMismatch);
    }
    Decode::Complete {
        frame: Frame { version, flags: tf & FLAG_MASK, msg_type, body },
        consumed: total,
    }
}

// ----------------------------------------------------------------------------
// crc32c (Castagnoli) — software implementation
// ----------------------------------------------------------------------------
// Software CRC32c with the Castagnoli polynomial 0x1EDC6F41 (reflected
// 0x82F63B78). We cannot pull a dependency onto the hot path; this stays
// in the protocol crate as pure Rust. A SIMD/table-based version is a
// later optimization — correctness first, profile-guided optimization later.

const CRC32C_POLY: u32 = 0x82F63B78;

pub fn crc32c(bytes: &[u8]) -> u32 {
    let mut crc: u32 = 0xFFFF_FFFF;
    for &b in bytes {
        crc ^= b as u32;
        for _ in 0..8 {
            crc = if crc & 1 != 0 { (crc >> 1) ^ CRC32C_POLY } else { crc >> 1 };
        }
    }
    !crc
}
