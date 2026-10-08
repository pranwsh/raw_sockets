//! `Goodbye` — bidirectional: clean teardown

use chat_model::{Action, Event};
use protocol::{self, Empty, MsgType, Payload};

pub const TYPE: MsgType = MsgType::Goodbye;

pub fn encode(action: &Action, _authenticated: &mut Option<Vec<u8>>) -> Option<(MsgType, Vec<u8>)> {
    match action {
        Action::Goodbye => Some((MsgType::Goodbye, Empty.encode())),
        _ => None,
    }
}

pub fn decode(body: &[u8]) -> Option<Event> {
    Empty::decode(body).map(|_| Event::Disconnected { reason: "goodbye".into() })
}