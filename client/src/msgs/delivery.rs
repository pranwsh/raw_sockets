//! `Send` (server → client) — an inbound message delivery
//!
//! The server uses the same wire id (30) for deliveries as the client uses for
//! requests [`send`](super::send), but with a different body layout:
//! `<conv_id>\n<seq:8le>\n<sender>\n<text>`.

use chat_model::Event;
use protocol::{self, Delivery, MsgType, Payload};

pub const TYPE: MsgType = MsgType::Send;

pub fn decode(body: &[u8]) -> Option<Event> {
    Delivery::decode(body).map(|d| Event::Message {
        conv: d.conv,
        from: String::from_utf8_lossy(&d.sender).into_owned(),
        seq: d.seq,
        text: String::from_utf8_lossy(&d.text).into_owned(),
    })
}