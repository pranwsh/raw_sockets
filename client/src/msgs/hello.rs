//! `Hello` — client → server: authenticate (auto-creates the account)

use chat_model::Action;
use protocol::{self, HelloReq, MsgType, Payload};

pub fn encode(action: &Action, authenticated: &mut Option<Vec<u8>>) -> Option<(MsgType, Vec<u8>)> {
    match action {
        Action::Hello { user, password } => {
            *authenticated = Some(user.clone().into_bytes());
            let body = HelloReq { user: user.as_bytes().to_vec(), password: password.as_bytes().to_vec() }.encode();
            Some((MsgType::Hello, body))
        }
        _ => None,
    }
}
