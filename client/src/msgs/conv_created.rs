//! `ConvCreated` — server → client: reply to `CreateConv`

use chat_model::Event;
use protocol::MsgType;

pub const TYPE: MsgType = MsgType::ConvCreated;

pub fn decode(body: &[u8]) -> Option<Event> {
    // body is the raw 8-byte conversation id
    Some(Event::ConvCreated { id: body.to_vec() })
}