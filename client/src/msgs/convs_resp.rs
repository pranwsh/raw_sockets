//! `ConvsResp` — server → client: reply to `ListConvs`

use chat_model::Event;
use protocol::{self, ConvsRespBody, MsgType, Payload};

pub const TYPE: MsgType = MsgType::ConvsResp;

pub fn decode(body: &[u8]) -> Option<Event> {
    ConvsRespBody::decode(body).map(|p| Event::Convs { ids: p.ids })
}