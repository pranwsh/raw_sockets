//! `ConvCreated` — server → client: reply to `CreateConv`

use chat_model::{ConvInfo, Event};
use protocol::MsgType;

pub const TYPE: MsgType = MsgType::ConvCreated;

pub fn decode(body: &[u8]) -> Option<Event> {
    // body is the raw 8-byte conversation id; the creating client already
    // knows the member list, so only the id needs to travel back
    if body.len() < 8 {
        return None;
    }
    Some(Event::ConvCreated(ConvInfo { id: body.to_vec(), members: Vec::new() }))
}
