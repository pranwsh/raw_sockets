//! `Delivered` — server → client: ack that a `Send` was persisted and queued

use chat_model::Event;
use protocol::{self, DeliveredBody, MsgType, Payload};

pub const TYPE: MsgType = MsgType::Delivered;

pub fn decode(body: &[u8]) -> Option<Event> {
    DeliveredBody::decode(body).map(|p| Event::Delivered { seq: p.seq })
}