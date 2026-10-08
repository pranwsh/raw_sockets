//! `Send` — client → server: send a message into a conversation

use chat_model::Action;
use protocol::{self, MsgType, Payload, SendReq};

pub fn encode(action: &Action, _authenticated: &mut Option<Vec<u8>>) -> Option<(MsgType, Vec<u8>)> {
    match action {
        Action::Send { conv, text } => {
            let body = SendReq { conv: conv.clone(), text: text.as_bytes().to_vec() }.encode();
            Some((MsgType::Send, body))
        }
        _ => None,
    }
}