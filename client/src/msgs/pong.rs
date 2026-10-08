//! `Pong` — server → client: reply to `Ping`

use chat_model::Event;
use protocol::{self, Empty, MsgType, Payload};

pub const TYPE: MsgType = MsgType::Pong;

pub fn decode(body: &[u8]) -> Option<Event> {
    Empty::decode(body).map(|_| Event::Pong)
}