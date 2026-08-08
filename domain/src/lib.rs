//! domain logic: accounts, conversations, inboxes

#![forbid(unsafe_code)]

mod sha256;

use protocol::{self, MsgType, OwnedFrame};
use storage::{Store, StoreResult};
use transport::{ConnectionId, EventHandler, TeardownReason};
use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::net::SocketAddrV4;
use std::sync::mpsc;
use std::time::{Duration, Instant};

pub const USER_ID_MAX_LEN: usize = 256;
pub const PASSWORD_MAX_LEN: usize = 256;
pub const SALT_LEN: usize = 16;
pub const HASH_LEN: usize = 32;
pub const CRED_LEN: usize = SALT_LEN + HASH_LEN;

// session

#[derive(Debug, Clone)]
struct Session {
    user_id: Vec<u8>,
    authenticated: bool,
}

// inbox helpers

fn inbox_prefix_user(user_id: &[u8]) -> Vec<u8> {
    let mut key = Vec::with_capacity(user_id.len() + 1);
    key.extend_from_slice(user_id);
    key.push(b'/');
    key
}

// pending async storage operations

/// what kind of storage operation is in flight, plus the context needed to process the result on the next tick
enum PendingKind {
    HelloLookup { user_id: Vec<u8>, password: Vec<u8> },
    AccountCreate { user_id: Vec<u8> },
    SendConvLookup { session: Session, conv_id: Vec<u8>, msg_body: Vec<u8> },
    SendSeqNext { session: Session, conv_id: Vec<u8>, msg_body: Vec<u8>, members: Vec<Vec<u8>> },
    CreateConvStore { conv_id: Vec<u8>, members_raw: Vec<u8> },
    InboxFetch,
}

struct PendingOp {
    conn_id: ConnectionId,
    kind: PendingKind,
    rx: mpsc::Receiver<StoreResult>,
}

// domain

pub struct Domain {
    store: Store,
    sessions: HashMap<ConnectionId, Session>,
    user_connections: HashMap<Vec<u8>, Vec<ConnectionId>>,
    user_conversations: HashMap<Vec<u8>, HashSet<Vec<u8>>>,
    /// in-memory conversation membership, kept in sync with the store on
    /// create/invite. Lets a Send skip the async conversation lookup store hop
    /// entirely (falling back to the store when a conv isn't cached yet, e.g.
    /// right after a server restart).
    conversations: HashMap<Vec<u8>, Vec<Vec<u8>>>,
    /// frames queued for the reactor to send
    outbound: Vec<(ConnectionId, Box<[u8]>)>,
    /// teardown requests queued for the reactor
    teardowns: Vec<(ConnectionId, TeardownReason)>,
    /// in-flight async storage operations
    pending_ops: Vec<PendingOp>,
}

impl Domain {
    pub fn new(store: Store) -> Self {
        Self {
            store,
            sessions: HashMap::new(),
            user_connections: HashMap::new(),
            user_conversations: HashMap::new(),
            conversations: HashMap::new(),
            outbound: Vec::new(),
            teardowns: Vec::new(),
            pending_ops: Vec::new(),
        }
    }

    fn enqueue(&mut self, id: ConnectionId, msg_type: MsgType, body: &[u8]) {
        let frame = protocol::encode(msg_type, 0, body);
        self.outbound.push((id, frame));
    }

    // auth

    /// 16 bytes of randomness from the OS CSPRNG
    fn gen_salt() -> [u8; SALT_LEN] {
        let mut salt = [0u8; SALT_LEN];
        if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
            let _ = f.read_exact(&mut salt);
        }
        salt
    }

    /// stored credential = salt || sha256(salt || password)
    fn make_cred(password: &[u8], salt: [u8; SALT_LEN]) -> Vec<u8> {
        let mut input = Vec::with_capacity(SALT_LEN + password.len());
        input.extend_from_slice(&salt);
        input.extend_from_slice(password);
        let mut cred = Vec::with_capacity(CRED_LEN);
        cred.extend_from_slice(&salt);
        cred.extend_from_slice(&sha256::sha256(&input));
        cred
    }

    fn verify_password(stored: &[u8], password: &[u8]) -> bool {
        if stored.len() != CRED_LEN {
            return false;
        }
        let salt: [u8; SALT_LEN] = stored[..SALT_LEN].try_into().unwrap();
        stored[..] == Self::make_cred(password, salt)[..]
    }

    fn handle_hello(&mut self, id: ConnectionId, body: &[u8]) {
        let Some((user_id, password)) = protocol::split_hello(body) else {
            self.enqueue(id, MsgType::AuthFail, b"bad_hello_format");
            return;
        };
        if user_id.is_empty() || user_id.len() > USER_ID_MAX_LEN {
            self.enqueue(id, MsgType::AuthFail, b"invalid_user_id");
            return;
        }
        if password.is_empty() || password.len() > PASSWORD_MAX_LEN {
            self.enqueue(id, MsgType::AuthFail, b"invalid_password");
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
            kind: PendingKind::HelloLookup { user_id: user_id.to_vec(), password: password.to_vec() },
            rx,
        });
    }

    fn establish_session(&mut self, id: ConnectionId, user_id: Vec<u8>) {
        // idempotent re-auth: a client could authenticate twice on the same
        // connection (the TUI adapter used to send Hello both at connect time
        // and on startup). A duplicate registration pushed this conn id into
        // `user_connections` twice, so live delivery targeted the same socket
        // twice — every received message was delivered duplicated — and the
        // redundant inbox fetch re-delivered queued offline messages. Guard
        // against re-establishing the same user on an already-authenticated
        // connection.
        let already_authed = matches!(
            self.sessions.get(&id),
            Some(s) if s.authenticated && s.user_id == user_id
        );
        if already_authed {
            return;
        }
        self.sessions.insert(id, Session { user_id: user_id.clone(), authenticated: true });
        let conns = self.user_connections.entry(user_id.clone()).or_default();
        if !conns.contains(&id) {
            conns.push(id);
        }
        self.start_inbox_fetch(id);
    }

    /// queue a fetch of this user's stored inbox; delivered automatically after auth
    fn start_inbox_fetch(&mut self, id: ConnectionId) {
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
        self.pending_ops.push(PendingOp { conn_id: id, kind: PendingKind::InboxFetch, rx });
    }

    fn authenticated(&self, id: ConnectionId) -> Option<&Session> {
        let s = self.sessions.get(&id)?;
        if s.authenticated { Some(s) } else { None }
    }

    // conversations

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

    // messaging

    fn handle_send(&mut self, id: ConnectionId, body: &[u8]) {
        let session = match self.authenticated(id) {
            Some(s) => s.clone(),
            None => return,
        };
        let Some((conv_id, text)) = protocol::split_send(body) else {
            self.enqueue(id, MsgType::Error, b"bad_send_format");
            return;
        };
        let conv_id = conv_id.to_vec();
        let msg_body = text.to_vec();
        if msg_body.is_empty() {
            self.enqueue(id, MsgType::Error, b"empty_message");
            return;
        }
        // Fast path: membership is cached in memory, so skip the async
        // conversation lookup and go straight to the sequence write.
        if let Some(members) = self.conversations.get(&conv_id) {
            if !Self::is_member(members, &session.user_id) {
                self.enqueue(id, MsgType::Error, b"not_a_member");
                return;
            }
            let rx = match self.store.next_sequence_async(&conv_id) {
                Ok(rx) => rx,
                Err(e) => {
                    self.enqueue(id, MsgType::Error, format!("seq: {e}").as_bytes());
                    return;
                }
            };
            self.pending_ops.push(PendingOp {
                conn_id: id,
                kind: PendingKind::SendSeqNext { session, conv_id, msg_body, members: members.clone() },
                rx,
            });
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

    /// deliver a pre-built delivery `Send` body to one connection
    fn deliver_to_connection(&mut self, id: ConnectionId, body: &[u8]) {
        let body_len = body.len();
        let total = protocol::HEADER_LEN + body_len + protocol::TRAILER_LEN;
        let mut buf = vec![0u8; total].into_boxed_slice();
        buf[protocol::HEADER_LEN..protocol::HEADER_LEN + body_len].copy_from_slice(body);
        protocol::seal(&mut buf, MsgType::Send, 0, body_len);
        self.outbound.push((id, buf));
    }

    fn is_member(members: &[Vec<u8>], user: &[u8]) -> bool {
        members.iter().any(|m| m.as_slice() == user)
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

    fn handle_goodbye(&mut self, id: ConnectionId) {
        self.remove_session(id);
        self.teardowns.push((id, TeardownReason::ClientGoodbye));
    }

    /// drop the session for `id` and unregister it from the user's connection set
    fn remove_session(&mut self, id: ConnectionId) {
        if let Some(session) = self.sessions.remove(&id)
            && let Some(conns) = self.user_connections.get_mut(&session.user_id)
        {
            conns.retain(|&c| c != id);
            if conns.is_empty() {
                self.user_connections.remove(&session.user_id);
            }
        }
    }

    // async result processing

    fn tick(&mut self) {
        // A Send runs through two sequential async store hops (conversation
        // lookup → next_sequence), each producing a result on a oneshot
        // channel. Draining those results only here, once per reactor tick,
        // would let each hop wait ~one epoll wake-up before being observed —
        // up to ~epoll_wait timeout per hop. To collapse that, after the first
        // drain pass we keep looping (non-blocking) for a short wall-clock
        // budget so both hops can resolve within a single reactor wake whenever
        // the store worker keeps up (the common case).
        const SPIN_BUDGET: Duration = Duration::from_millis(1);
        let deadline = Instant::now() + SPIN_BUDGET;
        loop {
            let mut progressed = false;
            let mut i = 0;
            while i < self.pending_ops.len() {
                match self.pending_ops[i].rx.try_recv() {
                    Ok(result) => {
                        let op = self.pending_ops.swap_remove(i);
                        self.process_pending_result(op.conn_id, op.kind, result);
                        progressed = true;
                    }
                    Err(mpsc::TryRecvError::Empty) => {
                        i += 1;
                    }
                    Err(mpsc::TryRecvError::Disconnected) => {
                        self.pending_ops.swap_remove(i);
                    }
                }
            }
            if !progressed || Instant::now() >= deadline {
                break;
            }
        }
    }

    fn process_pending_result(&mut self, conn_id: ConnectionId, kind: PendingKind, result: StoreResult) {
        match (kind, result) {
            // hello flow: existing account — verify the user-chosen password
            (PendingKind::HelloLookup { user_id, password }, StoreResult::Account(Ok(Some(stored)))) => {
                if Self::verify_password(&stored, &password) {
                    self.enqueue(conn_id, MsgType::AuthOk, &[0u8]);
                    self.establish_session(conn_id, user_id);
                } else {
                    self.enqueue(conn_id, MsgType::AuthFail, b"invalid_password");
                }
            }
            // hello flow: new account — create it with the user-chosen password
            (PendingKind::HelloLookup { user_id, password }, StoreResult::Account(Ok(None))) => {
                let cred = Self::make_cred(&password, Self::gen_salt());
                let rx = match self.store.put_account_async(&user_id, &cred) {
                    Ok(rx) => rx,
                    Err(e) => {
                        self.enqueue(conn_id, MsgType::Error, format!("storage: {e}").as_bytes());
                        return;
                    }
                };
                self.pending_ops.push(PendingOp {
                    conn_id,
                    kind: PendingKind::AccountCreate { user_id },
                    rx,
                });
            }
            (PendingKind::HelloLookup { .. }, StoreResult::Account(Err(e))) => {
                self.enqueue(conn_id, MsgType::Error, format!("storage: {e}").as_bytes());
            }

            // account create (after hello for new user)
            (PendingKind::AccountCreate { user_id }, StoreResult::Stored(Ok(()))) => {
                self.enqueue(conn_id, MsgType::AuthOk, &[1u8]);
                self.establish_session(conn_id, user_id);
            }
            (PendingKind::AccountCreate { .. }, StoreResult::Stored(Err(e))) => {
                self.enqueue(conn_id, MsgType::Error, format!("storage: {e}").as_bytes());
            }

            // send: conversation lookup
            (PendingKind::SendConvLookup { session, conv_id, msg_body }, StoreResult::Conversation(Ok(Some(data)))) => {
                let members: Vec<Vec<u8>> = data.split(|&b| b == b',').map(|m| m.to_vec()).collect();
                if !Self::is_member(&members, &session.user_id) {
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

            // send: sequence obtained → deliver + ack
            (PendingKind::SendSeqNext { session, conv_id, msg_body, members }, StoreResult::Sequence(Ok(seq))) => {
                let delivery = protocol::delivery_body(&conv_id, seq, &session.user_id, &msg_body);
                for member in &members {
                    if member != &session.user_id
                        && let Some(conns) = self.user_connections.get(member)
                    {
                        let targets: Vec<ConnectionId> = conns.clone();
                        for target_id in targets {
                            self.deliver_to_connection(target_id, &delivery);
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
                        inbox_key.extend_from_slice(&protocol::write_u64_le(seq));
                        let _ = self.store.put_inbox_async(&inbox_key, &delivery);
                    }
                }
                self.enqueue(conn_id, MsgType::Delivered, &protocol::write_u64_le(seq));
            }
            (PendingKind::SendSeqNext { .. }, StoreResult::Sequence(Err(e))) => {
                self.enqueue(conn_id, MsgType::Error, format!("seq: {e}").as_bytes());
            }

            // CreateConv store complete
            (PendingKind::CreateConvStore { conv_id, members_raw }, StoreResult::Stored(Ok(()))) => {
                self.enqueue(conn_id, MsgType::ConvCreated, &conv_id);
                let members: Vec<Vec<u8>> =
                    members_raw.split(|&b| b == b',').filter(|m| !m.is_empty()).map(|m| m.to_vec()).collect();
                self.conversations.insert(conv_id.clone(), members.clone());
                for member in members {
                    self.user_conversations.entry(member).or_default().insert(conv_id.clone());
                }
            }
            (PendingKind::CreateConvStore { .. }, StoreResult::Stored(Err(e))) => {
                self.enqueue(conn_id, MsgType::Error, format!("storage: {e}").as_bytes());
            }

            // inbox flush after auth: deliver each stored delivery body, then delete it
            (PendingKind::InboxFetch, StoreResult::InboxRange(Ok(items))) => {
                for (key, data) in &items {
                    self.deliver_to_connection(conn_id, data);
                    let _ = self.store.delete_inbox_async(key);
                }
            }
            (PendingKind::InboxFetch, StoreResult::InboxRange(Err(e))) => {
                self.enqueue(conn_id, MsgType::Error, format!("inbox: {e}").as_bytes());
            }

            _ => {}
        }
    }
}

// EventHandler implementation

impl EventHandler for Domain {
    fn on_accept(&mut self, id: ConnectionId, _peer: SocketAddrV4) {
        self.sessions.insert(id, Session { user_id: Vec::new(), authenticated: false });
    }

    fn on_frame(&mut self, id: ConnectionId, frame: OwnedFrame) {
        match frame.msg_type {
            MsgType::Hello => self.handle_hello(id, &frame.body),
            MsgType::Goodbye => self.handle_goodbye(id),
            MsgType::CreateConv => self.handle_create_conv(id, &frame.body),
            MsgType::Send => self.handle_send(id, &frame.body),
            MsgType::ListConvs => self.handle_list_convs(id),
            MsgType::Ping => self.enqueue(id, MsgType::Pong, &[]),
            MsgType::Presence => self.enqueue(id, MsgType::Presence, &frame.body),
            _ => {}
        }
    }

    fn on_teardown(&mut self, id: ConnectionId, _reason: TeardownReason) {
        self.remove_session(id);
    }

    fn tick(&mut self) {
        self.tick();
    }

    fn drain_outbound(&mut self, out: &mut Vec<(ConnectionId, Box<[u8]>)>) {
        out.append(&mut self.outbound);
    }

    fn drain_teardowns(&mut self, out: &mut Vec<(ConnectionId, TeardownReason)>) {
        out.append(&mut self.teardowns);
    }
}
