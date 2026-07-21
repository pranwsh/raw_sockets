//! Domain logic: accounts, conversations, inboxes.
//!
//! This crate is **I/O-free** — it receives decoded [`OwnedFrame`] values and
//! produces encoded frames to send back. It reads/writes through the
//! [`storage::Store`] handle (which offloads to a background thread).
//!
//! The [`Domain`] struct implements [`transport::EventHandler`] and is the
//! centerpiece the server binary wires into each shard's reactor.

#![forbid(unsafe_code)]

use protocol::{self, MsgType, OwnedFrame};
use storage::Store;
use transport::{ConnectionId, EventHandler, TeardownReason};
use std::collections::HashMap;
use std::net::SocketAddrV4;

pub const TOKEN_LEN: usize = 32;
const SERVER_SECRET: &[u8] = b"change-me-in-production";

// ---------------------------------------------------------------------------
// Session
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct Session {
    pub user_id: Vec<u8>,
    pub authenticated: bool,
}

// ---------------------------------------------------------------------------
// Inbox helpers
// ---------------------------------------------------------------------------

fn inbox_prefix_user(user_id: &[u8]) -> Vec<u8> {
    let mut key = Vec::with_capacity(user_id.len() + 1);
    key.extend_from_slice(user_id);
    key.push(b'/');
    key
}

// ---------------------------------------------------------------------------
// Domain
// ---------------------------------------------------------------------------

pub struct Domain {
    store: Store,
    sessions: HashMap<ConnectionId, Session>,
    user_connections: HashMap<Vec<u8>, Vec<ConnectionId>>,
    /// Frames queued for the reactor to send.
    pub outbound: Vec<(ConnectionId, Box<[u8]>)>,
    /// Teardown requests queued for the reactor.
    pub teardowns: Vec<(ConnectionId, TeardownReason)>,
}

impl Domain {
    pub fn new(store: Store) -> Self {
        Self {
            store,
            sessions: HashMap::new(),
            user_connections: HashMap::new(),
            outbound: Vec::new(),
            teardowns: Vec::new(),
        }
    }

    fn enqueue(&mut self, id: ConnectionId, msg_type: MsgType, body: &[u8]) {
        let frame = protocol::encode(msg_type, 0, body);
        self.outbound.push((id, frame));
    }

    // -------------------------------------------------------------------
    // Auth
    // -------------------------------------------------------------------

    fn derive_token(user_id: &[u8]) -> [u8; TOKEN_LEN] {
        let mut token = [0u8; TOKEN_LEN];
        let mut combined = Vec::with_capacity(user_id.len() + 1 + SERVER_SECRET.len());
        combined.extend_from_slice(user_id);
        combined.push(b':');
        combined.extend_from_slice(SERVER_SECRET);
        for (i, &b) in combined.iter().enumerate() {
            token[i % TOKEN_LEN] ^= b.wrapping_add((i as u8).wrapping_mul(0x9E));
        }
        let len = combined.len() as u32;
        token[0] ^= (len >> 24) as u8;
        token[1] ^= (len >> 16) as u8;
        token[2] ^= (len >> 8) as u8;
        token[3] ^= len as u8;
        token
    }

    fn handle_hello(&mut self, id: ConnectionId, user_id: &[u8]) {
        if user_id.is_empty() || user_id.len() > 256 {
            self.enqueue(id, MsgType::AuthFail, b"invalid_user_id");
            return;
        }
        match self.store.get_account(user_id) {
            Ok(Some(_)) => {
                let challenge_body = [b"token_required\n".as_slice(), user_id].concat();
                self.enqueue(id, MsgType::AuthChallenge, &challenge_body);
            }
            Ok(None) => {
                let token = Self::derive_token(user_id);
                if let Err(e) = self.store.put_account(user_id, &token) {
                    self.enqueue(id, MsgType::Error, format!("storage: {e}").as_bytes());
                    return;
                }
                self.enqueue(id, MsgType::AuthOk, &token);
                self.establish_session(id, user_id.to_vec());
            }
            Err(e) => {
                self.enqueue(id, MsgType::Error, format!("storage: {e}").as_bytes());
            }
        }
    }

    fn handle_auth_response(&mut self, id: ConnectionId, body: &[u8]) {
        let Some(sep_pos) = body.iter().position(|&b| b == b'\n') else {
            self.enqueue(id, MsgType::AuthFail, b"bad_auth_format");
            return;
        };
        let user_id = &body[..sep_pos];
        let token = &body[sep_pos + 1..];
        if user_id.is_empty() || token.len() != TOKEN_LEN {
            self.enqueue(id, MsgType::AuthFail, b"bad_auth_format");
            return;
        }
        let expected = Self::derive_token(user_id);
        if token == &expected[..] {
            self.enqueue(id, MsgType::AuthOk, token);
            self.establish_session(id, user_id.to_vec());
        } else {
            self.enqueue(id, MsgType::AuthFail, b"invalid_token");
        }
    }

    fn establish_session(&mut self, id: ConnectionId, user_id: Vec<u8>) {
        self.sessions.insert(id, Session { user_id: user_id.clone(), authenticated: true });
        self.user_connections.entry(user_id.clone()).or_default().push(id);
        self.deliver_inbox(id, &user_id);
    }

    fn authenticated(&self, id: ConnectionId) -> Option<&Session> {
        let s = self.sessions.get(&id)?;
        if s.authenticated { Some(s) } else { None }
    }

    // -------------------------------------------------------------------
    // Conversations
    // -------------------------------------------------------------------

    fn handle_create_conv(&mut self, id: ConnectionId, body: &[u8]) {
        if self.authenticated(id).is_none() {
            return;
        }
        let members: Vec<&[u8]> = body.split(|&b| b == b',').filter(|m| !m.is_empty()).collect();
        if members.len() < 2 {
            self.enqueue(id, MsgType::Error, b"need_at_least_2_members");
            return;
        }
        let conv_id = {
            let mut sorted: Vec<Vec<u8>> = members.iter().map(|m| m.to_vec()).collect();
            sorted.sort();
            let mut h: u64 = 0xC6A4A7935BD1E995;
            for m in &sorted {
                for &b in m {
                    h = h.wrapping_mul(0x517CC1B727220A95).wrapping_add(b as u64);
                }
                h ^= h >> 31;
            }
            h.to_le_bytes().to_vec()
        };
        if let Err(e) = self.store.put_conversation(&conv_id, body) {
            self.enqueue(id, MsgType::Error, format!("storage: {e}").as_bytes());
            return;
        }
        self.enqueue(id, MsgType::ConvCreated, &conv_id);
    }

    fn handle_conv_invite(&mut self, id: ConnectionId, body: &[u8]) {
        if self.authenticated(id).is_none() {
            return;
        }
        let sep = body.iter().position(|&b| b == b'\n').unwrap_or(body.len());
        let conv_id = &body[..sep];
        let invitee = if sep < body.len() { &body[sep + 1..] } else { return };
        let data = match self.store.get_conversation(conv_id) {
            Ok(Some(d)) => d,
            _ => { self.enqueue(id, MsgType::Error, b"conv_not_found"); return; }
        };
        let mut members = data;
        members.extend_from_slice(b",");
        members.extend_from_slice(invitee);
        if let Err(e) = self.store.put_conversation(conv_id, &members) {
            self.enqueue(id, MsgType::Error, format!("storage: {e}").as_bytes());
            return;
        }
        self.enqueue(id, MsgType::ConvMemberEvent, &members);
    }

    // -------------------------------------------------------------------
    // Messaging
    // -------------------------------------------------------------------

    fn handle_send(&mut self, id: ConnectionId, body: &[u8]) {
        let session = match self.authenticated(id) {
            Some(s) => s.clone(),
            None => return,
        };
        let Some(sep) = body.iter().position(|&b| b == b'\n') else {
            self.enqueue(id, MsgType::Error, b"bad_send_format");
            return;
        };
        let conv_id = &body[..sep];
        let msg_body = &body[sep + 1..];
        if msg_body.is_empty() {
            self.enqueue(id, MsgType::Error, b"empty_message");
            return;
        }
        let seq = match self.store.next_sequence(conv_id) {
            Ok(s) => s,
            Err(e) => { self.enqueue(id, MsgType::Error, format!("seq: {e}").as_bytes()); return; }
        };
        let msg_data = {
            let mut d = Vec::new();
            d.extend_from_slice(&session.user_id);
            d.push(b'\n');
            d.extend_from_slice(msg_body);
            d
        };
        if let Err(e) = self.store.put_inbox(&seq.to_le_bytes(), &msg_data) {
            self.enqueue(id, MsgType::Error, format!("store: {e}").as_bytes());
            return;
        }
        self.deliver_to_connection(id, &session.user_id, conv_id, seq, &msg_data);
        self.enqueue(id, MsgType::Delivered, &seq.to_le_bytes());
    }

    fn deliver_to_connection(&mut self, id: ConnectionId, _user_id: &[u8], conv_id: &[u8], seq: u64, msg_data: &[u8]) {
        let mut resp = Vec::new();
        resp.extend_from_slice(conv_id);
        resp.push(b'\n');
        resp.extend_from_slice(&seq.to_le_bytes());
        resp.push(b'\n');
        resp.extend_from_slice(msg_data);
        let frame = protocol::encode(MsgType::Send, 0, &resp);
        self.outbound.push((id, frame));
    }

    fn deliver_inbox(&mut self, id: ConnectionId, user_id: &[u8]) {
        let prefix = inbox_prefix_user(user_id);
        let items = match self.store.get_inbox_range(&prefix, 100) {
            Ok(items) => items,
            Err(e) => {
                self.enqueue(id, MsgType::Error, format!("inbox: {e}").as_bytes());
                return;
            }
        };
        for (key, data) in &items {
            if key.starts_with(&prefix) && key.len() > prefix.len() {
                let rest = &key[prefix.len()..];
                let parts: Vec<&[u8]> = rest.split(|&b| b == b'/').collect();
                if parts.len() >= 2 {
                    let conv_id = parts[0];
                    let seq = if parts[1].len() >= 8 {
                        let mut arr = [0u8; 8];
                        arr.copy_from_slice(&parts[1][..8]);
                        u64::from_be_bytes(arr)
                    } else {
                        continue;
                    };
                    self.deliver_to_connection(id, user_id, conv_id, seq, data);
                    let _ = self.store.delete_inbox(&key);
                }
            }
        }
    }

    fn handle_inbox_fetch(&mut self, id: ConnectionId, _body: &[u8]) {
        self.enqueue(id, MsgType::InboxResp, &[]);
    }

    fn handle_goodbye(&mut self, id: ConnectionId) {
        if let Some(session) = self.sessions.remove(&id) {
            if let Some(conns) = self.user_connections.get_mut(&session.user_id) {
                conns.retain(|&c| c != id);
                if conns.is_empty() {
                    self.user_connections.remove(&session.user_id);
                }
            }
        }
        self.teardowns.push((id, TeardownReason::ClientGoodbye));
    }
}

// ---------------------------------------------------------------------------
// EventHandler implementation
// ---------------------------------------------------------------------------

impl EventHandler for Domain {
    fn on_accept(&mut self, id: ConnectionId, _peer: SocketAddrV4) {
        self.sessions.insert(id, Session { user_id: Vec::new(), authenticated: false });
    }

    fn on_frame(&mut self, id: ConnectionId, frame: OwnedFrame) {
        match frame.msg_type {
            MsgType::Hello => self.handle_hello(id, &frame.body),
            MsgType::AuthResponse => self.handle_auth_response(id, &frame.body),
            MsgType::Goodbye => self.handle_goodbye(id),
            MsgType::CreateConv => self.handle_create_conv(id, &frame.body),
            MsgType::ConvInvite => self.handle_conv_invite(id, &frame.body),
            MsgType::Send => self.handle_send(id, &frame.body),
            MsgType::InboxFetch => self.handle_inbox_fetch(id, &frame.body),
            MsgType::Ping => self.enqueue(id, MsgType::Pong, &[]),
            MsgType::Presence => self.enqueue(id, MsgType::Presence, &frame.body),
            _ => {}
        }
    }

    fn on_teardown(&mut self, id: ConnectionId, _reason: TeardownReason) {
        if let Some(session) = self.sessions.remove(&id) {
            if let Some(conns) = self.user_connections.get_mut(&session.user_id) {
                conns.retain(|&c| c != id);
                if conns.is_empty() {
                    self.user_connections.remove(&session.user_id);
                }
            }
        }
    }

    fn drain_outbound(&mut self) -> Vec<(ConnectionId, Box<[u8]>)> {
        std::mem::take(&mut self.outbound)
    }

    fn drain_teardowns(&mut self) -> Vec<(ConnectionId, TeardownReason)> {
        std::mem::take(&mut self.teardowns)
    }
}
