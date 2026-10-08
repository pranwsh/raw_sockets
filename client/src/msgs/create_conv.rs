//! `CreateConv` — client → server: create a conversation (≥ 2 members)

use chat_model::Action;
use protocol::{self, CreateConvReq, MsgType, Payload};

pub fn encode(action: &Action, authenticated: &mut Option<Vec<u8>>) -> Option<(MsgType, Vec<u8>)> {
    match action {
        Action::CreateConv { members } => {
            let mut members = members.clone();
            // the authenticated user is prepended automatically
            if let Some(me) = authenticated.as_ref() {
                let me = String::from_utf8_lossy(me).into_owned();
                if !members.contains(&me) {
                    members.insert(0, me);
                }
            }
            let body = CreateConvReq {
                members: members.into_iter().map(String::into_bytes).collect(),
            }.encode();
            Some((MsgType::CreateConv, body))
        }
        _ => None,
    }
}