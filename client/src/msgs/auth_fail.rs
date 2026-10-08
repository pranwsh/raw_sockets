//! `AuthFail` — server → client: authentication rejected

use chat_model::Event;
use protocol::MsgType;

pub const TYPE: MsgType = MsgType::AuthFail;

pub fn decode(body: &[u8]) -> Option<Event> {
    // body is a UTF-8 reason string
    Some(Event::AuthFail { reason: String::from_utf8_lossy(body).into_owned() })
}