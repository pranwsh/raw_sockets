//! inter-node routing — lightweight TCP-based pub/sub between instances

#![forbid(unsafe_code)]

use protocol::{self, MsgType, OwnedFrame};
use std::collections::{HashMap, HashSet};

/// routing table: maps user_id → set of node ids
type RoutingTable = HashMap<Vec<u8>, HashSet<Vec<u8>>>;

/// tracks which users are local to this node
type LocalUsers = HashSet<Vec<u8>>;

/// the routing state for one server instance
pub struct Router {
    /// our node ID (e.g hostname or config value)
    node_id: Vec<u8>,
    /// known peer node ids and their routing addresses
    peers: Vec<PeerInfo>,
    /// routing table: user_id → set of node ids that have this user
    table: RoutingTable,
    /// users connected to this local node
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

    /// configure peer nodes
    pub fn set_peers(&mut self, peers: Vec<PeerInfo>) {
        self.peers = peers;
    }

    /// register a user as connected to the local node
    pub fn register_local(&mut self, user_id: &[u8]) {
        self.local.insert(user_id.to_vec());
        self.table
            .entry(user_id.to_vec())
            .or_default()
            .insert(self.node_id.clone());
    }

    /// unregister a user (disconnected)
    pub fn unregister_local(&mut self, user_id: &[u8]) {
        self.local.remove(user_id);
        if let Some(nodes) = self.table.get_mut(user_id) {
            nodes.remove(&self.node_id);
            if nodes.is_empty() {
                self.table.remove(user_id);
            }
        }
    }

    /// check if a user is connected to this node
    pub fn is_local(&self, user_id: &[u8]) -> bool {
        self.local.contains(user_id)
    }

    /// find which peers have the given user (excluding local node)
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

    /// process an incoming routing frame (RouteAnnounce / RouteDeliver)
    pub fn handle_routing_frame(&mut self, frame: OwnedFrame) -> Option<OwnedFrame> {
        match frame.msg_type {
            MsgType::RouteAnnounce => {
                // body: comma-separated user ids that the peer has
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
                // body format: user_id \n message_data
                if let Some(sep) = frame.body.iter().position(|&b| b == b'\n') {
                    let target_user = &frame.body[..sep];
                    let msg_data = &frame.body[sep + 1..];
                    if self.local.contains(target_user) {
                        // deliver locally — return a send frame for dispatch
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
            // for non-routing frames, pass through for local dispatch
            _ => Some(frame),
        }
    }

    /// build a RouteAnnounce frame with our local users
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
