//! `ConvCreated` — server → client: reply to `CreateConv`

use chat_model::{ConvInfo, Event};
use protocol::{ConvEntry, MsgType, Payload};

pub const TYPE: MsgType = MsgType::ConvCreated;

pub fn decode(body: &[u8]) -> Option<Event> {
    // body is one conversation record: the id plus the member list, so the
    // frontend can label it without a second round trip
    ConvEntry::decode(body).map(|e| Event::ConvCreated(ConvInfo {
        id: e.id,
        members: e
            .members
            .iter()
            .map(|m| String::from_utf8_lossy(m).into_owned())
            .collect(),
    }))
}
