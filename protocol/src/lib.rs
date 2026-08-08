//! versioned binary framing protocol for the messaging service

#![forbid(unsafe_code)]

#[cfg(test)]
mod tests;

use core::fmt;

// constants

pub const MAGIC: u8 = 0x4D;
pub const VERSION: u8 = 1;
pub const HEADER_LEN: usize = 8;
pub const TRAILER_LEN: usize = 4; // crc32c
pub const FRAME_OVERHEAD: usize = HEADER_LEN + TRAILER_LEN;
pub const MAX_BODY_LEN: usize = 16 * 1024 * 1024;

pub const FLAG_COMPRESSED: u16 = 1 << 15;
pub const FLAG_ACK_REQ: u16 = 1 << 14;
pub const FLAG_MASK: u16 = 0xE000;
pub const TYPE_MASK: u16 = 0x1FFF;

// message types

/// all message types share one frame envelope
#[repr(u16)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MsgType {
    // handshake / auth
    Hello = 1,
    AuthOk = 4,
    AuthFail = 5,
    Goodbye = 6,

    // account / presence
    Presence = 10,

    // conversations
    CreateConv = 20,
    ConvCreated = 21,

    // messaging
    Send = 30,
    Delivered = 31,

    // conversation listing
    ListConvs = 37,
    ConvsResp = 38,

    // control
    Ping = 90,
    Pong = 91,
    Error = 99,
}

impl MsgType {
    pub fn from_u16(v: u16) -> Option<Self> {
        Some(match v {
            1 => Self::Hello,
            4 => Self::AuthOk,
            5 => Self::AuthFail,
            6 => Self::Goodbye,
            10 => Self::Presence,
            20 => Self::CreateConv,
            21 => Self::ConvCreated,
            30 => Self::Send,
            31 => Self::Delivered,
            37 => Self::ListConvs,
            38 => Self::ConvsResp,
            90 => Self::Ping,
            91 => Self::Pong,
            99 => Self::Error,
            _ => return None,
        })
    }
}

// frame

/// a decoded frame
#[derive(Clone, PartialEq, Eq)]
pub struct Frame<'a> {
    pub version: u8,
    pub flags: u16,
    pub msg_type: MsgType,
    pub body: &'a [u8],
}

impl<'a> Frame<'a> {
    pub fn is_compressed(&self) -> bool {
        self.flags & FLAG_COMPRESSED != 0
    }

    pub fn ack_requested(&self) -> bool {
        self.flags & FLAG_ACK_REQ != 0
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

// owned frame (for dispatch after the buffer is reused)

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

// encoder

/// encode a frame into dst caller guarantees dst.len() >= frame.wire_len()
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

/// convenience: produce an owned encoded buffer
pub fn encode(msg_type: MsgType, flags: u16, body: &[u8]) -> Box<[u8]> {
    let total = HEADER_LEN + body.len() + TRAILER_LEN;
    let mut buf = vec![0u8; total].into_boxed_slice();
    encode_into(&mut buf, msg_type, flags, body);
    buf
}

/// seal a buffer where the body has already been written at buf[HEADER_LEN..HEADER_LEN+body_len] writes the header and CRC in place, avoiding a redundant body copy
pub fn seal(buf: &mut [u8], msg_type: MsgType, flags: u16, body_len: usize) -> usize {
    let total = HEADER_LEN + body_len + TRAILER_LEN;
    debug_assert!(buf.len() >= total);
    buf[0] = MAGIC;
    buf[1] = VERSION;
    let tf = (msg_type as u16) & TYPE_MASK | (flags & FLAG_MASK);
    buf[2..4].copy_from_slice(&tf.to_le_bytes());
    buf[4..8].copy_from_slice(&(body_len as u32).to_le_bytes());
    let crc = crc32c(&buf[1..HEADER_LEN + body_len]);
    buf[HEADER_LEN + body_len..total].copy_from_slice(&crc.to_le_bytes());
    total
}

// wire body formats — the application-level encodings shared by the client
// and server so the layouts live in exactly one place

/// body of a `Hello`: `<user>\n<password>`
pub fn hello_body(user: &[u8], password: &[u8]) -> Vec<u8> {
    let mut body = Vec::with_capacity(user.len() + 1 + password.len());
    body.extend_from_slice(user);
    body.push(b'\n');
    body.extend_from_slice(password);
    body
}

/// split a `Hello` body into (user, password)
pub fn split_hello(body: &[u8]) -> Option<(&[u8], &[u8])> {
    split_at_newline(body)
}

/// body of a client→server `Send`: `<conv_id>\n<text>`
pub fn send_body(conv: &[u8], text: &[u8]) -> Vec<u8> {
    let mut body = Vec::with_capacity(conv.len() + 1 + text.len());
    body.extend_from_slice(conv);
    body.push(b'\n');
    body.extend_from_slice(text);
    body
}

/// split a client→server `Send` body into (conv_id, text)
pub fn split_send(body: &[u8]) -> Option<(&[u8], &[u8])> {
    split_at_newline(body)
}

/// body of a server→client delivery `Send`:
/// `<conv_id>\n<seq:8le>\n<sender>\n<text>`
pub fn delivery_body(conv: &[u8], seq: u64, sender: &[u8], text: &[u8]) -> Vec<u8> {
    let mut body = Vec::with_capacity(conv.len() + 1 + 8 + 1 + sender.len() + 1 + text.len());
    body.extend_from_slice(conv);
    body.push(b'\n');
    body.extend_from_slice(&write_u64_le(seq));
    body.push(b'\n');
    body.extend_from_slice(sender);
    body.push(b'\n');
    body.extend_from_slice(text);
    body
}

/// split a delivery body into (conv_id, seq, sender, text)
pub type Delivery<'a> = (&'a [u8], u64, &'a [u8], &'a [u8]);
pub fn split_delivery(body: &[u8]) -> Option<Delivery<'_>> {
    let (conv, rest) = split_at_newline(body)?;
    let seq = read_u64_le(rest)?;
    // skip the 8 seq bytes plus the separating `\n` to reach `<sender>\n<text>`
    let (sender, text) = split_at_newline(&rest[9..])?;
    Some((conv, seq, sender, text))
}

/// 8-byte little-endian encoding of a sequence number
pub fn write_u64_le(v: u64) -> [u8; 8] {
    v.to_le_bytes()
}

/// decode an 8-byte little-endian u64 from the front of `b`
pub fn read_u64_le(b: &[u8]) -> Option<u64> {
    Some(u64::from_le_bytes(b.get(..8)?.try_into().ok()?))
}

fn split_at_newline(body: &[u8]) -> Option<(&[u8], &[u8])> {
    let sep = body.iter().position(|&b| b == b'\n')?;
    Some((&body[..sep], &body[sep + 1..]))
}

// decoder

/// result of a non-consuming decode attempt against an append-only buffer
#[derive(Debug, PartialEq, Eq)]
pub enum Decode<'a> {
    /// a complete frame was produced
    Complete { frame: Frame<'a>, consumed: usize },
    /// not enough bytes yet
    Need,
    /// unrecoverable: the caller must tear the connection down via the single shared teardown path (transport crate), not ad-hoc per error
    Err(DecodeError),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecodeError {
    /// first byte isn't MAGIC stream is misaligned / not our protocol
    BadMagic,
    /// protocol version we don't speak
    UnsupportedVersion(u8),
    /// unknown message type
    UnknownMsgType(u16),
    /// declared body length exceeds MAX_BODY_LEN — almost certainly an attack or a corrupt length field
    BodyTooLarge { declared: usize, max: usize },
    /// CRC mismatch — corrupt frame on the wire
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

/// decode one frame from the front of buf does not mutate buf; caller advances by consumed only on Complete
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

// crc32c (castagnoli) — table-based implementation

const CRC32C_POLY: u32 = 0x82F63B78;

const CRC32C_TABLE: [u32; 256] = {
    let mut table = [0u32; 256];
    let mut i = 0usize;
    while i < 256 {
        let mut crc = i as u32;
        let mut _j = 0;
        while _j < 8 {
            crc = if crc & 1 != 0 { (crc >> 1) ^ CRC32C_POLY } else { crc >> 1 };
            _j += 1;
        }
        table[i] = crc;
        i += 1;
    }
    table
};

pub fn crc32c(bytes: &[u8]) -> u32 {
    let mut crc: u32 = 0xFFFF_FFFF;
    for &b in bytes {
        crc = CRC32C_TABLE[((crc ^ b as u32) & 0xFF) as usize] ^ (crc >> 8);
    }
    !crc
}
