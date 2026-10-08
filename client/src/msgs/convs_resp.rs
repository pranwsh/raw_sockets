//! `ConvsResp` — server → client: reply to `ListConvs`

use chat_model::{ConvInfo, Event};
use protocol::{ConvsRespBody, MsgType, Payload};

pub const TYPE: MsgType = MsgType::ConvsResp;

pub fn decode(body: &[u8]) -> Option<Event> {
    ConvsRespBody::decode(body).map(|p| Event::Convs {
        convs: p
            .convs
            .into_iter()
            .map(|c| ConvInfo {
                id: c.id,
                members: c.members.iter().map(|m| String::from_utf8_lossy(m).into_owned()).collect(),
            })
            .collect(),
    })
}
