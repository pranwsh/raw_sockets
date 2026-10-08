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

/// total wire size of a frame whose body is `body_len` bytes
pub const fn frame_len(body_len: usize) -> usize { HEADER_LEN + body_len + TRAILER_LEN }
pub const MAX_BODY_LEN: usize = 16 * 1024 * 1024;

pub const FLAG_COMPRESSED: u16 = 1 << 15;
pub const FLAG_ACK_REQ: u16 = 1 << 14;
pub const FLAG_MASK: u16 = 0xE000;
pub const TYPE_MASK: u16 = 0x1FFF;

// message types

/// all message types share one frame envelope
///
/// The single source of truth for wire ids: the [`msg_types!`] registry below
/// generates the enum, the numeric mapping (`from_u16`/`as_u16`) and the full
/// variant list (`ALL`) from one table, so they can never desync. Adding a
/// message type = add one line to the [`msg_types!`] invocation.
macro_rules! msg_types {
    ($( $name:ident = $num:literal ),+ $(,)?) => {
        #[repr(u16)]
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        pub enum MsgType {
            $( $name = $num, )*
        }

        impl MsgType {
            /// every declared message type, in declaration order
            pub const ALL: &'static [MsgType] = &[ $( Self::$name, )* ];

            pub const fn from_u16(v: u16) -> Option<Self> {
                $( if v == $num { return Some(Self::$name); } )*
                None
            }

            pub const fn as_u16(self) -> u16 {
                match self {
                    $( Self::$name => $num, )*
                }
            }
        }
    };
}

msg_types![
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
];

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
    let tf = msg_type.as_u16() & TYPE_MASK | (flags & FLAG_MASK);
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
    let tf = msg_type.as_u16() & TYPE_MASK | (flags & FLAG_MASK);
    buf[2..4].copy_from_slice(&tf.to_le_bytes());
    buf[4..8].copy_from_slice(&(body_len as u32).to_le_bytes());
    let crc = crc32c(&buf[1..HEADER_LEN + body_len]);
    buf[HEADER_LEN + body_len..total].copy_from_slice(&crc.to_le_bytes());
    total
}

// wire body formats — typed codecs
//
// Each message's body layout lives in exactly one [`Payload`] implementation.
// The layouts are documented in PROTOCOL.md; these impls are the authoritative
// in-code definition shared by the client, server and tests.

/// a message body codec: serializes/deserializes one message type's body
pub trait Payload: Sized {
    fn encode(&self) -> Vec<u8>;
    fn decode(body: &[u8]) -> Option<Self>;
}

/// empty body (Ping, Pong, ListConvs, Goodbye)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Empty;

impl Payload for Empty {
    fn encode(&self) -> Vec<u8> {
        Vec::new()
    }
    fn decode(body: &[u8]) -> Option<Self> {
        if body.is_empty() { Some(Empty) } else { None }
    }
}

/// opaque pass-through body (Presence)
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Opaque(pub Vec<u8>);

impl Payload for Opaque {
    fn encode(&self) -> Vec<u8> {
        self.0.clone()
    }
    fn decode(body: &[u8]) -> Option<Self> {
        Some(Opaque(body.to_vec()))
    }
}

/// `Hello` body: `<user>\n<password>`
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HelloReq {
    pub user: Vec<u8>,
    pub password: Vec<u8>,
}

impl Payload for HelloReq {
    fn encode(&self) -> Vec<u8> {
        newline_join(&[&self.user, &self.password])
    }
    fn decode(body: &[u8]) -> Option<Self> {
        let (user, password) = split_at_newline(body)?;
        Some(HelloReq { user: user.to_vec(), password: password.to_vec() })
    }
}

/// `CreateConv` body: comma-separated member ids (empty segments ignored)
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreateConvReq {
    pub members: Vec<Vec<u8>>,
}

impl Payload for CreateConvReq {
    fn encode(&self) -> Vec<u8> {
        let mut body = Vec::new();
        for (i, m) in self.members.iter().enumerate() {
            if i > 0 { body.push(b','); }
            body.extend_from_slice(m);
        }
        body
    }
    fn decode(body: &[u8]) -> Option<Self> {
        let members: Vec<Vec<u8>> =
            body.split(|&b| b == b',').filter(|m| !m.is_empty()).map(|m| m.to_vec()).collect();
        Some(CreateConvReq { members })
    }
}

/// client→server `Send` body: `<conv_id>\n<text>`
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SendReq {
    pub conv: Vec<u8>,
    pub text: Vec<u8>,
}

impl Payload for SendReq {
    fn encode(&self) -> Vec<u8> {
        newline_join(&[&self.conv, &self.text])
    }
    fn decode(body: &[u8]) -> Option<Self> {
        let (conv, text) = split_at_newline(body)?;
        Some(SendReq { conv: conv.to_vec(), text: text.to_vec() })
    }
}

/// server→client delivery `Send` body: `<conv_id>\n<seq:8le>\n<sender>\n<text>`
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Delivery {
    pub conv: Vec<u8>,
    pub seq: u64,
    pub sender: Vec<u8>,
    pub text: Vec<u8>,
}

impl Payload for Delivery {
    fn encode(&self) -> Vec<u8> {
        let mut body = Vec::with_capacity(
            self.conv.len() + 1 + 8 + 1 + self.sender.len() + 1 + self.text.len(),
        );
        body.extend_from_slice(&self.conv);
        body.push(b'\n');
        body.extend_from_slice(&self.seq.to_le_bytes());
        body.push(b'\n');
        body.extend_from_slice(&self.sender);
        body.push(b'\n');
        body.extend_from_slice(&self.text);
        body
    }
    fn decode(body: &[u8]) -> Option<Self> {
        let (conv, rest) = split_at_newline(body)?;
        let seq = u64::from_le_bytes(rest.get(..8)?.try_into().ok()?);
        // skip the 8 seq bytes plus the separating `\n` to reach `<sender>\n<text>`
        let (sender, text) = split_at_newline(&rest[9..])?;
        Some(Delivery { conv: conv.to_vec(), seq, sender: sender.to_vec(), text: text.to_vec() })
    }
}

/// `AuthOk` body: 1 byte — 1 if the account was just created, 0 otherwise
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthOkBody {
    pub created: bool,
}

impl Payload for AuthOkBody {
    fn encode(&self) -> Vec<u8> {
        vec![if self.created { 1 } else { 0 }]
    }
    fn decode(body: &[u8]) -> Option<Self> {
        match body.first() {
            Some(&0) => Some(AuthOkBody { created: false }),
            Some(&1) => Some(AuthOkBody { created: true }),
            _ => None,
        }
    }
}

/// `ConvsResp` body: per conversation, its 8-byte id followed by a newline
/// separated member list
///
/// Members are carried so a frontend can label a conversation `alice, bob`
/// instead of an opaque id. The list is sorted by the server, so the label is
/// stable across reconnects.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConvsRespBody {
    pub convs: Vec<ConvEntry>,
}

/// one conversation in a [`ConvsRespBody`] listing
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConvEntry {
    pub id: Vec<u8>,
    pub members: Vec<Vec<u8>>,
}

impl ConvEntry {
    /// append this entry's wire form to `out`
    fn encode_into(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.id);
        out.extend_from_slice(&CreateConvReq { members: self.members.clone() }.encode());
        // terminate each entry: the id is fixed width, so a trailing separator
        // makes the record self-delimiting without a length prefix
        out.push(b'\n');
    }
}

impl Payload for ConvEntry {
    fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.id.len() + 32);
        self.encode_into(&mut out);
        out
    }
    fn decode(body: &[u8]) -> Option<Self> {
        // a standalone record (`ConvCreated`) carries its own terminator
        let body = body.strip_suffix(b"\n")?;
        ConvEntry::decode_record(body)
    }
}

impl ConvEntry {
    /// decode one record whose trailing separator has already been stripped
    fn decode_record(record: &[u8]) -> Option<Self> {
        // the id is fixed width, so the remainder of the record is the members
        let (id, members_raw) = record.split_at(8);
        let members: Vec<Vec<u8>> = members_raw
            .split(|&b| b == b',')
            .filter(|m| !m.is_empty())
            .map(|m| m.to_vec())
            .collect();
        Some(ConvEntry { id: id.to_vec(), members })
    }
}

impl Payload for ConvsRespBody {
    fn encode(&self) -> Vec<u8> {
        let mut body = Vec::with_capacity(self.convs.len() * 32);
        for c in &self.convs {
            c.encode_into(&mut body);
        }
        body
    }
    fn decode(body: &[u8]) -> Option<Self> {
        let mut convs = Vec::new();
        for entry in body.split(|&b| b == b'\n') {
            // the trailing separator yields one empty final chunk; skip it
            if entry.is_empty() {
                continue;
            }
            if entry.len() < 8 {
                return None;
            }
            convs.push(ConvEntry::decode_record(entry)?);
        }
        Some(ConvsRespBody { convs })
    }
}

/// `Delivered` body: 8-byte little-endian sequence number
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeliveredBody {
    pub seq: u64,
}

impl Payload for DeliveredBody {
    fn encode(&self) -> Vec<u8> {
        self.seq.to_le_bytes().to_vec()
    }
    fn decode(body: &[u8]) -> Option<Self> {
        let seq = u64::from_le_bytes(body.get(..8)?.try_into().ok()?);
        Some(DeliveredBody { seq })
    }
}

fn newline_join(parts: &[&[u8]]) -> Vec<u8> {
    let len = parts.iter().map(|p| p.len() + 1).sum::<usize>().saturating_sub(1);
    let mut body = Vec::with_capacity(len);
    for (i, p) in parts.iter().enumerate() {
        if i > 0 { body.push(b'\n'); }
        body.extend_from_slice(p);
    }
    body
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
