//! `Error` — server → client: application error (connection stays open)

use chat_model::Event;
use protocol::MsgType;

pub const TYPE: MsgType = MsgType::Error;

pub fn decode(body: &[u8]) -> Option<Event> {
    // body is a UTF-8 error string
    Some(Event::Error { msg: String::from_utf8_lossy(body).into_owned() })
}