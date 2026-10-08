//! `Ping` — client → server: keepalive

use chat_model::Action;
use protocol::{self, Empty, MsgType, Payload};

pub fn encode(action: &Action, _authenticated: &mut Option<Vec<u8>>) -> Option<(MsgType, Vec<u8>)> {
    match action {
        Action::Ping => Some((MsgType::Ping, Empty.encode())),
        _ => None,
    }
}