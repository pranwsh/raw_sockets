//! per-message bridge modules: the wire↔model glue for each message type
//!
//! Each module knows exactly one wire message: its [`MsgType`] (via `TYPE` for
//! server→client messages; via its `encode` arm for client→server messages),
//! its body codec, and how it maps to/from the model [`Action`]/[`Event`].
//!
//! The two `messages!` lists below are the client-side inventory. Adding a
//! bridge for a new message type means:
//! 1. one module file in this directory (`TYPE` + `decode` for server→client,
//!    `encode` for client→server),
//! 2. one entry in the matching list below.
//!
//! [`MsgType`]: protocol::MsgType

use chat_model::{Action, Event};
use protocol::{MsgType, OwnedFrame};

pub mod auth_fail;
pub mod auth_ok;
pub mod conv_created;
pub mod convs_resp;
pub mod create_conv;
pub mod delivered;
pub mod delivery;
pub mod goodbye;
pub mod hello;
pub mod list_convs;
pub mod ping;
pub mod pong;
pub mod send;
pub mod server_error;

/// generate `encode_action` from the outbound module list and `decode_event`
/// from the inbound module list. The inbound dispatch is keyed on [`MsgType`];
/// each module's `decode` reports its own `TYPE`, so an accidental duplicate
/// wire id between two inbound modules is a compile error.
macro_rules! messages {
    (outbound: [$( $out:ident ),* $(,)?]; inbound: [$( $in:ident ),* $(,)?]) => {
        pub(crate) fn encode_action(
            action: &Action,
            authenticated: &mut Option<Vec<u8>>,
        ) -> Option<(MsgType, Box<[u8]>)> {
            $(
                if let Some((msg_type, body)) = $out::encode(action, authenticated) {
                    return Some((msg_type, body.into_boxed_slice()));
                }
            )*
            None
        }

        pub(crate) fn decode_event(f: OwnedFrame) -> Event {
            match f.msg_type {
                $(
                    $in::TYPE => match $in::decode(&f.body) {
                        Some(event) => event,
                        None => Event::Error { msg: format!("malformed {} frame", stringify!($in)) },
                    },
                )*
                other => Event::Error { msg: format!("unhandled message type: {other:?}") },
            }
        }
    };
}

messages![
    outbound: [hello, create_conv, list_convs, send, ping, goodbye];
    inbound: [auth_ok, auth_fail, conv_created, convs_resp, delivery, delivered, pong, server_error, goodbye]
];