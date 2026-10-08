//! `ListConvs` — client → server: list the authenticated user's conversations

use chat_model::Action;
use protocol::{self, Empty, MsgType, Payload};

pub fn encode(action: &Action, _authenticated: &mut Option<Vec<u8>>) -> Option<(MsgType, Vec<u8>)> {
    match action {
        Action::ListConvs => Some((MsgType::ListConvs, Empty.encode())),
        _ => None,
    }
}