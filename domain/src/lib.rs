//! Domain logic: accounts, conversations, inboxes.
//!
//! This crate is **I/O-free** — it receives decoded [`OwnedFrame`] values and
//! produces encoded frames to send back. It reads/writes through the
//! [`storage::Store`] handle (which offloads to a background thread).
//!
//! All storage operations are **non-blocking**: `on_frame` queues async store
//! calls and returns immediately. The reactor calls [`EventHandler::tick`]
//! each iteration, which polls pending storage results and dispatches the
//! next step of each operation. This keeps the event loop free to service
//! other clients while a DB read/write is in flight.
//!
//! The [`Domain`] struct implements [`transport::EventHandler`] and is the
//! centerpiece the server binary wires into each shard's reactor.

#![forbid(unsafe_code)]

use protocol::{self, MsgType, OwnedFrame};
use storage::{Store, StoreResult};
use transport::{ConnectionId, EventHandler, TeardownReason};
use std::collections::{HashMap, HashSet};
use std::net::SocketAddrV4;
use std::sync::mpsc;

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
// Pending async storage operations
// ---------------------------------------------------------------------------

/// What kind of storage operation is in flight, plus the context needed to
/// process the result on the next tick.
enum PendingKind {
    HelloLookup { user_id: Vec<u8> },
    AuthLookup { user_id: Vec<u8>, token: Vec<u8> },
    AccountCreate { user_id: Vec<u8>, token: [u8; TOKEN_LEN] },
    SendConvLookup { session: Session, conv_id: Vec<u8>, msg_body: Vec<u8> },
    SendSeqNext { session: Session, conv_id: Vec<u8>, msg_body: Vec<u8>, members: Vec<Vec<u8>> },
    CreateConvStore { conv_id: Vec<u8>, members_raw: Vec<u8> },
    ConvInviteLookup { conv_id: Vec<u8>, invitee: Vec<u8> },
    ConvInviteStore { conv_id: Vec<u8>, invitee: Vec<u8>, updated_members: Vec<u8> },
    InboxFetch { user_id: Vec<u8>, prefix: Vec<u8>, send_resp: bool },
}

struct PendingOp {
    conn_id: ConnectionId,
    kind: PendingKind,
    rx: mpsc::Receiver<StoreResult>,
}

// ---------------------------------------------------------------------------
// Domain
// ---------------------------------------------------------------------------

pub struct Domain {
    store: Store,
    sessions: HashMap<ConnectionId, Session>,
    user_connections: HashMap<Vec<u8>, Vec<ConnectionId>>,
    user_conversations: HashMap<Vec<u8>, HashSet<Vec<u8>>>,
    /// Frames queued for the reactor to send.
    pub outbound: Vec<(ConnectionId, Box<[u8]>)>,
    /// Teardown requests queued for the reactor.
    pub teardowns: Vec<(ConnectionId, TeardownReason)>,
    /// In-flight async storage operations.
    pending_ops: Vec<PendingOp>,
}

impl Domain {
    pub fn new(store: Store) -> Self {
        Self {
            store,
            sessions: HashMap::new(),
            user_connections: HashMap::new(),
            user_conversations: HashMap::new(),
            outbound: Vec::new(),
            teardowns: Vec::new(),
            pending_ops: Vec::new(),
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

    fn handle_hello(&mut self, id: ConnectionId, body: &[u8]) {
        if body.is_empty() || body.len() > 256 {
            self.enqueue(id, MsgType::AuthFail, b"invalid_user_id");
            return;
        }
        let rx = match self.store.get_account_async(body) {
            Ok(rx) => rx,
            Err(e) => {
                self.enqueue(id, MsgType::Error, format!("storage: {e}").as_bytes());
                return;
            }
        };
        self.pending_ops.push(PendingOp {
            conn_id: id,
            kind: PendingKind::HelloLookup { user_id: body.to_vec() },
            rx,
        });
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
        let rx = match self.store.get_account_async(user_id) {
            Ok(rx) => rx,
            Err(e) => {
                self.enqueue(id, MsgType::Error, format!("storage: {e}").as_bytes());
                return;
            }
        };
        self.pending_ops.push(PendingOp {
            conn_id: id,
            kind: PendingKind::AuthLookup { user_id: user_id.to_vec(), token: token.to_vec() },
            rx,
        });
    }

    fn establish_session(&mut self, id: ConnectionId, user_id: Vec<u8>) {
        self.sessions.insert(id, Session { user_id: user_id.clone(), authenticated: true });
        self.user_connections.entry(user_id.clone()).or_default().push(id);
        let prefix = inbox_prefix_user(&user_id);
        let rx = match self.store.get_inbox_range_async(&prefix, 100) {
            Ok(rx) => rx,
            Err(e) => {
                self.enqueue(id, MsgType::Error, format!("inbox: {e}").as_bytes());
                return;
            }
        };
        self.pending_ops.push(PendingOp {
            conn_id: id,
            kind: PendingKind::InboxFetch { user_id, prefix, send_resp: false },
            rx,
        });
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
        let rx = match self.store.put_conversation_async(&conv_id, body) {
            Ok(rx) => rx,
            Err(e) => {
                self.enqueue(id, MsgType::Error, format!("storage: {e}").as_bytes());
                return;
            }
        };
        self.pending_ops.push(PendingOp {
            conn_id: id,
            kind: PendingKind::CreateConvStore { conv_id, members_raw: body.to_vec() },
            rx,
        });
    }

    fn handle_conv_invite(&mut self, id: ConnectionId, body: &[u8]) {
        if self.authenticated(id).is_none() {
            return;
        }
        let sep = body.iter().position(|&b| b == b'\n').unwrap_or(body.len());
        let conv_id = &body[..sep];
        let invitee = if sep < body.len() { &body[sep + 1..] } else { return };
        if invitee.is_empty() {
            return;
        }
        let rx = match self.store.get_conversation_async(conv_id) {
            Ok(rx) => rx,
            Err(e) => {
                self.enqueue(id, MsgType::Error, format!("storage: {e}").as_bytes());
                return;
            }
        };
        self.pending_ops.push(PendingOp {
            conn_id: id,
            kind: PendingKind::ConvInviteLookup { conv_id: conv_id.to_vec(), invitee: invitee.to_vec() },
            rx,
        });
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
        let conv_id = body[..sep].to_vec();
        let msg_body = body[sep + 1..].to_vec();
        if msg_body.is_empty() {
            self.enqueue(id, MsgType::Error, b"empty_message");
            return;
        }
        let rx = match self.store.get_conversation_async(&conv_id) {
            Ok(rx) => rx,
            Err(e) => {
                self.enqueue(id, MsgType::Error, format!("storage: {e}").as_bytes());
                return;
            }
        };
        self.pending_ops.push(PendingOp {
            conn_id: id,
            kind: PendingKind::SendConvLookup { session, conv_id, msg_body },
            rx,
        });
    }

    fn deliver_to_connection(&mut self, id: ConnectionId, _user_id: &[u8], conv_id: &[u8], seq: u64, msg_data: &[u8]) {
        let mut resp = Vec::new();
        resp.extend_from_slice(conv_id);
        resp.push(b'\n');
        resp.extend_from_slice(&seq.to_le_bytes());
        resp.extend_from_slice(msg_data);
        let frame = protocol::encode(MsgType::Send, 0, &resp);
        self.outbound.push((id, frame));
    }

    fn handle_list_convs(&mut self, id: ConnectionId) {
        let session = match self.authenticated(id) {
            Some(s) => s.clone(),
            None => return,
        };
        let conv_ids = self.user_conversations.get(&session.user_id).cloned().unwrap_or_default();
        let mut body = Vec::new();
        for conv_id in &conv_ids {
            body.extend_from_slice(conv_id);
        }
        self.enqueue(id, MsgType::ConvsResp, &body);
    }

    fn handle_inbox_fetch(&mut self, id: ConnectionId) {
        let session = match self.authenticated(id) {
            Some(s) => s.clone(),
            None => return,
        };
        let prefix = inbox_prefix_user(&session.user_id);
        let rx = match self.store.get_inbox_range_async(&prefix, 100) {
            Ok(rx) => rx,
            Err(e) => {
                self.enqueue(id, MsgType::Error, format!("inbox: {e}").as_bytes());
                return;
            }
        };
        self.pending_ops.push(PendingOp {
            conn_id: id,
            kind: PendingKind::InboxFetch { user_id: session.user_id, prefix, send_resp: true },
            rx,
        });
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

    // -------------------------------------------------------------------
    // Async result processing
    // -------------------------------------------------------------------

    fn tick(&mut self) {
        let mut i = 0;
        while i < self.pending_ops.len() {
            match self.pending_ops[i].rx.try_recv() {
                Ok(result) => {
                    let op = self.pending_ops.swap_remove(i);
                    self.process_pending_result(op.conn_id, op.kind, result);
                }
                Err(mpsc::TryRecvError::Empty) => {
                    i += 1;
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.pending_ops.swap_remove(i);
                }
            }
        }
    }

    fn process_pending_result(&mut self, conn_id: ConnectionId, kind: PendingKind, result: StoreResult) {
        match (kind, result) {
            // ---- Hello flow ----
            (PendingKind::HelloLookup { user_id }, StoreResult::Account(Ok(Some(_)))) => {
                let challenge_body = [b"token_required\n".as_slice(), &user_id].concat();
                self.enqueue(conn_id, MsgType::AuthChallenge, &challenge_body);
            }
            (PendingKind::HelloLookup { user_id }, StoreResult::Account(Ok(None))) => {
                let token = Self::derive_token(&user_id);
                let rx = match self.store.put_account_async(&user_id, &token) {
                    Ok(rx) => rx,
                    Err(e) => {
                        self.enqueue(conn_id, MsgType::Error, format!("storage: {e}").as_bytes());
                        return;
                    }
                };
                self.pending_ops.push(PendingOp {
                    conn_id,
                    kind: PendingKind::AccountCreate { user_id, token },
                    rx,
                });
            }
            (PendingKind::HelloLookup { .. }, StoreResult::Account(Err(e))) => {
                self.enqueue(conn_id, MsgType::Error, format!("storage: {e}").as_bytes());
            }

            // ---- Account create (after Hello for new user) ----
            (PendingKind::AccountCreate { user_id, token }, StoreResult::Stored(Ok(()))) => {
                self.enqueue(conn_id, MsgType::AuthOk, &token);
                self.establish_session(conn_id, user_id);
            }
            (PendingKind::AccountCreate { .. }, StoreResult::Stored(Err(e))) => {
                self.enqueue(conn_id, MsgType::Error, format!("storage: {e}").as_bytes());
            }

            // ---- Auth response flow ----
            (PendingKind::AuthLookup { user_id, token }, StoreResult::Account(Ok(Some(_)))) => {
                let expected = Self::derive_token(&user_id);
                if token.len() == TOKEN_LEN && &token[..] == &expected[..] {
                    self.enqueue(conn_id, MsgType::AuthOk, &token);
                    self.establish_session(conn_id, user_id);
                } else {
                    self.enqueue(conn_id, MsgType::AuthFail, b"invalid_token");
                }
            }
            (PendingKind::AuthLookup { .. }, StoreResult::Account(Ok(None))) => {
                self.enqueue(conn_id, MsgType::AuthFail, b"account_not_found");
            }
            (PendingKind::AuthLookup { .. }, StoreResult::Account(Err(e))) => {
                self.enqueue(conn_id, MsgType::Error, format!("storage: {e}").as_bytes());
            }

            // ---- Send: conversation lookup ----
            (PendingKind::SendConvLookup { session, conv_id, msg_body }, StoreResult::Conversation(Ok(Some(data)))) => {
                let members: Vec<Vec<u8>> = data.split(|&b| b == b',').map(|m| m.to_vec()).collect();
                if !members.iter().any(|m| m == &session.user_id) {
                    self.enqueue(conn_id, MsgType::Error, b"not_a_member");
                    return;
                }
                let rx = match self.store.next_sequence_async(&conv_id) {
                    Ok(rx) => rx,
                    Err(e) => {
                        self.enqueue(conn_id, MsgType::Error, format!("seq: {e}").as_bytes());
                        return;
                    }
                };
                self.pending_ops.push(PendingOp {
                    conn_id,
                    kind: PendingKind::SendSeqNext { session, conv_id, msg_body, members },
                    rx,
                });
            }
            (PendingKind::SendConvLookup { .. }, StoreResult::Conversation(Ok(None))) => {
                self.enqueue(conn_id, MsgType::Error, b"conv_not_found");
            }
            (PendingKind::SendConvLookup { .. }, StoreResult::Conversation(Err(e))) => {
                self.enqueue(conn_id, MsgType::Error, format!("storage: {e}").as_bytes());
            }

            // ---- Send: sequence obtained → deliver + ack ----
            (PendingKind::SendSeqNext { session, conv_id, msg_body, members }, StoreResult::Sequence(Ok(seq))) => {
                let msg_data = {
                    let mut d = Vec::new();
                    d.extend_from_slice(&session.user_id);
                    d.push(b'\n');
                    d.extend_from_slice(&msg_body);
                    d
                };
                for member in &members {
                    if member != &session.user_id {
                        if let Some(conns) = self.user_connections.get(member) {
                            let targets: Vec<ConnectionId> = conns.clone();
                            for target_id in targets {
                                self.deliver_to_connection(target_id, &session.user_id, &conv_id, seq, &msg_data);
                            }
                        }
                    }
                }
                for member in &members {
                    if !self.user_connections.contains_key(member) && member != &session.user_id {
                        let mut inbox_key = Vec::new();
                        inbox_key.extend_from_slice(member);
                        inbox_key.push(b'/');
                        inbox_key.extend_from_slice(&conv_id);
                        inbox_key.push(b'/');
                        inbox_key.extend_from_slice(&seq.to_be_bytes());
                        let _ = self.store.put_inbox_async(&inbox_key, &msg_data);
                    }
                }
                self.enqueue(conn_id, MsgType::Delivered, &seq.to_le_bytes());
            }
            (PendingKind::SendSeqNext { .. }, StoreResult::Sequence(Err(e))) => {
                self.enqueue(conn_id, MsgType::Error, format!("seq: {e}").as_bytes());
            }

            // ---- CreateConv store complete ----
            (PendingKind::CreateConvStore { conv_id, members_raw }, StoreResult::Stored(Ok(()))) => {
                self.enqueue(conn_id, MsgType::ConvCreated, &conv_id);
                for member in members_raw.split(|&b| b == b',') {
                    if !member.is_empty() {
                        self.user_conversations.entry(member.to_vec()).or_default().insert(conv_id.clone());
                    }
                }
            }
            (PendingKind::CreateConvStore { .. }, StoreResult::Stored(Err(e))) => {
                self.enqueue(conn_id, MsgType::Error, format!("storage: {e}").as_bytes());
            }

            // ---- ConvInvite: conversation lookup ----
            (PendingKind::ConvInviteLookup { conv_id, invitee }, StoreResult::Conversation(Ok(Some(data)))) => {
                let mut members = data;
                members.extend_from_slice(b",");
                members.extend_from_slice(&invitee);
                let rx = match self.store.put_conversation_async(&conv_id, &members) {
                    Ok(rx) => rx,
                    Err(e) => {
                        self.enqueue(conn_id, MsgType::Error, format!("storage: {e}").as_bytes());
                        return;
                    }
                };
                self.pending_ops.push(PendingOp {
                    conn_id,
                    kind: PendingKind::ConvInviteStore { conv_id, invitee, updated_members: members },
                    rx,
                });
            }
            (PendingKind::ConvInviteLookup { .. }, StoreResult::Conversation(Ok(None))) => {
                self.enqueue(conn_id, MsgType::Error, b"conv_not_found");
            }
            (PendingKind::ConvInviteLookup { .. }, StoreResult::Conversation(Err(e))) => {
                self.enqueue(conn_id, MsgType::Error, format!("storage: {e}").as_bytes());
            }

            // ---- ConvInvite: store complete → notify ----
            (PendingKind::ConvInviteStore { conv_id, invitee, updated_members }, StoreResult::Stored(Ok(()))) => {
                self.enqueue(conn_id, MsgType::ConvMemberEvent, &updated_members);
                if let Some(conns) = self.user_connections.get(&invitee) {
                    let targets: Vec<ConnectionId> = conns.clone();
                    for target_id in targets {
                        self.enqueue(target_id, MsgType::ConvMemberEvent, &updated_members);
                    }
                }
                self.user_conversations.entry(invitee).or_default().insert(conv_id);
            }
            (PendingKind::ConvInviteStore { .. }, StoreResult::Stored(Err(e))) => {
                self.enqueue(conn_id, MsgType::Error, format!("storage: {e}").as_bytes());
            }

            // ---- Inbox fetch (auth or explicit) ----
            (PendingKind::InboxFetch { user_id, prefix, send_resp }, StoreResult::InboxRange(Ok(items))) => {
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
                            self.deliver_to_connection(conn_id, &user_id, conv_id, seq, data);
                            let _ = self.store.delete_inbox_async(key);
                        }
                    }
                }
                if send_resp {
                    self.enqueue(conn_id, MsgType::InboxResp, &[]);
                }
            }
            (PendingKind::InboxFetch { .. }, StoreResult::InboxRange(Err(e))) => {
                self.enqueue(conn_id, MsgType::Error, format!("inbox: {e}").as_bytes());
            }

            _ => {}
        }
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
            MsgType::InboxFetch => self.handle_inbox_fetch(id),
            MsgType::ListConvs => self.handle_list_convs(id),
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

    fn tick(&mut self) {
        self.tick();
    }

    fn drain_outbound(&mut self) -> Vec<(ConnectionId, Box<[u8]>)> {
        std::mem::take(&mut self.outbound)
    }

    fn drain_teardowns(&mut self) -> Vec<(ConnectionId, TeardownReason)> {
        std::mem::take(&mut self.teardowns)
    }
}
