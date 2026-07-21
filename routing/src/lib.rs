//! Inter-node routing — lightweight TCP-based pub/sub between instances.
//!
//! This crate provides the mesh communication layer that enables multi-node
//! scale-out. Each server instance:
//!
//! 1. Listens on a routing port for incoming peer connections.
//! 2. Maintains outgoing connections to configured peer nodes.
//! 3. Exchanges [`RouteAnnounce`] frames so every node knows which users are
//!    on which peer.
//! 4. Forwards [`RouteDeliver`] frames to the correct peer when a message
//!    targets a user not connected to the local node.
//!
//! **Architecture note**: routing uses the same binary framing protocol as
//! client connections, so all message validation and decoding code is reused.
//! The routing connection is just another socket managed by a reactor — no
//! special I/O path.
//!
//! The first implementation is single-node only (routing is a no-op). The
//! design and interfaces are wired so that multi-node support can be added
//! without modifying the transport or protocol crate.

#![forbid(unsafe_code)]

use protocol::{self, MsgType, OwnedFrame};
use std::collections::{HashMap, HashSet};

/// Routing table: maps user_id → set of node IDs.
type RoutingTable = HashMap<Vec<u8>, HashSet<Vec<u8>>>;

/// Tracks which users are local to this node.
type LocalUsers = HashSet<Vec<u8>>;

/// The routing state for one server instance.
pub struct Router {
    /// Our node ID (e.g. hostname or config value).
    node_id: Vec<u8>,
    /// Known peer node IDs and their routing addresses.
    peers: Vec<PeerInfo>,
    /// Routing table: user_id → set of node IDs that have this user.
    table: RoutingTable,
    /// Users connected to this local node.
    local: LocalUsers,
}

#[derive(Debug, Clone)]
pub struct PeerInfo {
    pub node_id: Vec<u8>,
    pub addr: String, // "host:port"
}

impl Router {
    pub fn new(node_id: &[u8]) -> Self {
        Self {
            node_id: node_id.to_vec(),
            peers: Vec::new(),
            table: HashMap::new(),
            local: HashSet::new(),
        }
    }

    /// Configure peer nodes. Called at startup before the event loop runs.
    pub fn set_peers(&mut self, peers: Vec<PeerInfo>) {
        self.peers = peers;
    }

    /// Register a user as connected to the local node.
    pub fn register_local(&mut self, user_id: &[u8]) {
        self.local.insert(user_id.to_vec());
        self.table
            .entry(user_id.to_vec())
            .or_default()
            .insert(self.node_id.clone());
    }

    /// Unregister a user (disconnected).
    pub fn unregister_local(&mut self, user_id: &[u8]) {
        self.local.remove(user_id);
        if let Some(nodes) = self.table.get_mut(user_id) {
            nodes.remove(&self.node_id);
            if nodes.is_empty() {
                self.table.remove(user_id);
            }
        }
    }

    /// Check if a user is connected to this node.
    pub fn is_local(&self, user_id: &[u8]) -> bool {
        self.local.contains(user_id)
    }

    /// Find which peers have the given user (excluding local node).
    pub fn find_peers_for_user(&self, user_id: &[u8]) -> Vec<Vec<u8>> {
        self.table
            .get(user_id)
            .map(|nodes| {
                nodes
                    .iter()
                    .filter(|n| *n != &self.node_id)
                    .cloned()
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Process an incoming routing frame (RouteAnnounce / RouteDeliver).
    /// Returns the decoded frame if it's a regular message to be dispatched
    /// locally, or None if it was a routing-internal message.
    pub fn handle_routing_frame(&mut self, frame: OwnedFrame) -> Option<OwnedFrame> {
        match frame.msg_type {
            MsgType::RouteAnnounce => {
                // Body: comma-separated user IDs that the peer has.
                let peer_id = &frame.body; // In a real impl, the peer ID comes from the connection.
                for user_id in frame.body.split(|&b| b == b',') {
                    if !user_id.is_empty() {
                        self.table
                            .entry(user_id.to_vec())
                            .or_default()
                            .insert(peer_id.to_vec());
                    }
                }
                None
            }
            MsgType::RouteDeliver => {
                // Body format: user_id \n message_data
                if let Some(sep) = frame.body.iter().position(|&b| b == b'\n') {
                    let target_user = &frame.body[..sep];
                    let msg_data = &frame.body[sep + 1..];
                    if self.local.contains(target_user) {
                        // Deliver locally — return a Send frame for dispatch.
                        let send_frame =
                            protocol::encode(MsgType::Send, 0, msg_data);
                        let decoded = protocol::decode(&send_frame);
                        if let protocol::Decode::Complete { frame: f, .. } = decoded {
                            return Some(OwnedFrame::from_borrowed(&f));
                        }
                    }
                }
                None
            }
            // For non-routing frames, pass through for local dispatch.
            _ => Some(frame),
        }
    }

    /// Build a RouteAnnounce frame with our local users.
    pub fn build_announce(&self) -> Box<[u8]> {
        let mut body = Vec::new();
        for user_id in &self.local {
            if !body.is_empty() {
                body.push(b',');
            }
            body.extend_from_slice(user_id);
        }
        protocol::encode(MsgType::RouteAnnounce, 0, &body)
    }
}
