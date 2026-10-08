//! `AuthOk` — server → client: authentication succeeded

use chat_model::Event;
use protocol::{self, AuthOkBody, MsgType, Payload};

pub const TYPE: MsgType = MsgType::AuthOk;

pub fn decode(body: &[u8]) -> Option<Event> {
    AuthOkBody::decode(body).map(|p| Event::AuthOk { created: p.created })
}