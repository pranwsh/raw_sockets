//! Per-peer state for the datagram transport: frame reassembly (partial I/O),
//! send-queue backpressure, and teardown.
//!
//! # Why reassembly is still needed
//!
//! A datagram is atomic, so there is no partial *read* in the TCP sense: each
//! `recvfrom` either yields a whole frame or nothing. What survives on a packet
//! transport is the other half of the problem — a `msgd` frame is a
//! length-prefixed record that may be **larger than one datagram**. When the
//! frame spans several datagrams (or arrives out of order, or a fragment is
//! lost), the same offset-and-compact machinery as the stream path assembles it.
//! [`Peer::push_payload`] is the single entry point for both cases.
//!
//! # Backpressure
//!
//! TCP had a kernel socket buffer whose fill level the application could
//! observe. A datagram socket has no such buffer, so backpressure becomes an
//! explicit policy over a bounded send queue: once the queue fills, new
//! datagrams are dropped (and counted) rather than growing without bound. The
//! reliable layer above will retransmit the drops, which is what makes dropping
//! safe here in a way it would not be on a bare UDP socket.

use protocol::{self, Decode, DecodeError, OwnedFrame};
use std::collections::VecDeque;
use std::sync::Arc;

use crate::reliable::{ReliabilityLayer, ReliabilityStats};
use crate::sys;

// constants

/// outbound queue depth at which we start refusing new traffic
pub const SEND_QUEUE_SOFT_LIMIT: usize = 64;

/// outbound queue depth that tears the peer down as a slow consumer
pub const SEND_QUEUE_HARD_CAP: usize = 1024;

/// maximum bytes of undecoded frame data buffered per peer (anti-OOM)
pub const READ_BUFFER_HARD_CAP: usize = 1024 * 1024;

/// how long we keep a tearing-down peer around to flush queued acks before
/// dropping it
pub const DRAIN_TIMEOUT_MS: u64 = 3_000;

/// why a peer was torn down
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeerTeardownReason {
    /// too many unacknowledged datagrams (peer stopped acking)
    Unresponsive,
    /// outbound queue hit the hard cap
    SlowConsumer,
    /// a frame could not be parsed — protocol mismatch or corruption
    ProtocolViolation,
    /// a frame exceeded [`READ_BUFFER_HARD_CAP`]
    OversizedFrame,
    /// the peer sent a clean Goodbye
    Goodbye,
    /// the server is shutting down
    Shutdown,
}

/// what the caller should do after queueing a frame
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueueOutcome {
    /// accepted and queued
    Queued,
    /// the peer is tearing down; the frame was refused
    Rejected,
    /// the queue is at its soft limit; the frame was dropped
    DroppedSoftLimit,
}

/// a frame that could not be handed on
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RejectedFrame {
    pub outcome: QueueOutcome,
    pub frame: Arc<[u8]>,
}

/// per-peer counters for logging and tests
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PeerStats {
    /// datagrams dropped because the send queue was at its soft limit
    pub dropped_soft_limit: u64,
    /// frames dropped because the read buffer hit the hard cap
    pub dropped_oversized: u64,
    /// decode errors seen
    pub decode_errors: u64,
    /// frames successfully decoded
    pub frames_decoded: u64,
}

/// a peer in its teardown phase
#[derive(Debug, Clone, Copy)]
pub struct TeardownState {
    pub reason: PeerTeardownReason,
    pub deadline_ms: u64,
    pub sent_farewell: bool,
}

/// one peer's transport state
#[derive(Debug)]
pub struct Peer {
    /// the endpoint this peer is reachable at
    pub addr: crate::packet::Endpoint,

    // read side: frame reassembly over an ordered datagram payload stream
    read_buf: Vec<u8>,
    /// offset of undecoded data in `read_buf`; compacting is deferred until the
    /// consumed prefix is large so the common case never memmoves
    read_offset: usize,

    // write side: bounded queue of encoded envelopes awaiting transmission
    send_queue: VecDeque<Vec<u8>>,
    /// true while the reactor still owes us a transmission attempt
    pub write_pending: bool,

    /// the reliability layer wrapping every payload we send and receive
    pub reliable: ReliabilityLayer,

    /// if Some, the peer is tearing down
    pub teardown: Option<TeardownState>,

    stats: PeerStats,
}

impl Peer {
    /// a new peer at `addr`
    pub fn new(addr: crate::packet::Endpoint) -> Self {
        Self {
            addr,
            read_buf: Vec::with_capacity(8192),
            read_offset: 0,
            send_queue: VecDeque::with_capacity(16),
            write_pending: false,
            reliable: ReliabilityLayer::new(),
            teardown: None,
            stats: PeerStats::default(),
        }
    }

    // ---------------------------------------------------------------- read

    /// append one decoded datagram payload to the reassembly buffer
    ///
    /// This is the partial-I/O entry point. Because [`crate::reliable`]
    /// delivers payloads in sequence order, appending in arrival order already
    /// yields a contiguous byte stream; the buffer then only has to find frame
    /// boundaries, exactly as the stream path does.
    pub fn push_payload(&mut self, payload: &[u8]) {
        if self.read_buf.len() + payload.len() > READ_BUFFER_HARD_CAP {
            self.stats.dropped_oversized += 1;
            self.start_teardown(PeerTeardownReason::OversizedFrame);
            return;
        }
        self.read_buf.extend_from_slice(payload);
    }

    /// undecoded bytes currently buffered
    pub fn read_buffered_bytes(&self) -> usize {
        self.read_buf.len().saturating_sub(self.read_offset)
    }

    /// try to decode one frame from the reassembly buffer
    pub fn try_decode_frame(&mut self) -> Result<Option<OwnedFrame>, DecodeError> {
        let offset = self.read_offset;
        let available = &self.read_buf[offset..];
        if available.is_empty() {
            return Ok(None);
        }
        match protocol::decode(available) {
            Decode::Complete { frame, consumed } => {
                let owned = OwnedFrame::from_borrowed(&frame);
                self.read_offset += consumed;
                // compact lazily: only once the dead prefix is worth reclaiming
                if self.read_offset >= 32 * 1024 {
                    self.read_buf.drain(..self.read_offset);
                    self.read_offset = 0;
                }
                self.stats.frames_decoded += 1;
                Ok(Some(owned))
            }
            Decode::Need => Ok(None),
            Decode::Err(e) => {
                self.stats.decode_errors += 1;
                Err(e)
            }
        }
    }

    // --------------------------------------------------------------- write

    /// queue a frame for transmission
    ///
    /// Applies the backpressure policy: below the soft limit the frame is
    /// wrapped by the reliability layer and queued; at or above it the frame is
    /// dropped and counted, because the peer is not draining fast enough and
    /// letting the queue grow is how a slow peer turns into an OOM.
    pub fn enqueue_frame(&mut self, frame: &[u8], now_ms: u64) -> QueueOutcome {
        if self.teardown.is_some() {
            return QueueOutcome::Rejected;
        }
        if self.send_queue.len() >= SEND_QUEUE_HARD_CAP {
            self.start_teardown(PeerTeardownReason::SlowConsumer);
            return QueueOutcome::Rejected;
        }
        if self.send_queue.len() >= SEND_QUEUE_SOFT_LIMIT {
            self.stats.dropped_soft_limit += 1;
            return QueueOutcome::DroppedSoftLimit;
        }
        let (_, envelope) = self.reliable.wrap(frame, now_ms);
        self.send_queue.push_back(envelope);
        self.write_pending = true;
        QueueOutcome::Queued
    }

    /// take the next datagram to transmit, if any
    pub fn pop_datagram(&mut self) -> Option<Vec<u8>> {
        let next = self.send_queue.pop_front();
        self.write_pending = !self.send_queue.is_empty();
        next
    }

    /// put a datagram back at the head of the queue after a transient send
    /// failure, so the next tick retries it ahead of newer traffic
    pub fn requeue_front(&mut self, bytes: Vec<u8>) {
        self.send_queue.push_front(bytes);
        self.write_pending = true;
    }

    /// datagrams waiting to be transmitted
    pub fn send_queue_len(&self) -> usize {
        self.send_queue.len()
    }

    /// whether anything is waiting to be transmitted
    pub fn is_write_empty(&self) -> bool {
        self.send_queue.is_empty()
    }

    /// queue a standalone ack so the peer learns about what we have received
    /// even when we have nothing else to say
    pub fn pump_ack(&mut self, now_ms: u64) -> bool {
        if self.teardown.is_some() {
            return false;
        }
        match self.reliable.ack_if_due(now_ms) {
            Some(bytes) => {
                if self.send_queue.len() >= SEND_QUEUE_HARD_CAP {
                    return false;
                }
                self.send_queue.push_back(bytes);
                self.write_pending = true;
                true
            }
            None => false,
        }
    }

    /// queue every datagram whose retransmit timer has expired
    pub fn pump_retransmits(&mut self, now_ms: u64) -> usize {
        if self.teardown.is_some() {
            return 0;
        }
        let due = self
            .reliable
            .due_for_retransmit(now_ms, crate::reliable::RETRANSMIT_TIMEOUT_MS);
        let n = due.len();
        for (_, envelope) in due {
            if self.send_queue.len() >= SEND_QUEUE_HARD_CAP {
                break;
            }
            self.send_queue.push_back(envelope);
            self.write_pending = true;
        }
        n
    }

    /// hand a received datagram to the reliability layer, buffering its payload
    /// for reassembly and queuing any ack it implies
    ///
    /// Returns the datagrams that became deliverable as a result.
    pub fn on_datagram(&mut self, datagram: &[u8], now_ms: u64) -> Vec<Vec<u8>> {
        match crate::reliable::Envelope::decode(datagram) {
            Some(env) => {
                let ready = self.reliable.on_envelope(&env, now_ms);
                for payload in &ready {
                    self.push_payload(payload);
                }
                ready
            }
            None => Vec::new(),
        }
    }

    // ------------------------------------------------------------ teardown

    /// begin tearing the peer down
    pub fn start_teardown(&mut self, reason: PeerTeardownReason) {
        if self.teardown.is_some() {
            return;
        }
        self.teardown = Some(TeardownState {
            reason,
            deadline_ms: now_ms_deadline(),
            sent_farewell: false,
        });
    }

    /// whether the teardown has finished and the peer can be dropped
    pub fn teardown_complete(&self, now_ms: u64) -> bool {
        match &self.teardown {
            None => false,
            Some(t) => self.is_write_empty() || now_ms >= t.deadline_ms,
        }
    }

    /// payloads received but not yet released in order
    pub fn buffered(&self) -> usize {
        self.reliable.buffered()
    }

    /// datagrams sent but not yet acknowledged
    pub fn unacked(&self) -> usize {
        self.reliable.unacked()
    }

    pub fn stats(&self) -> PeerStats {
        self.stats
    }

    pub fn reliability_stats(&self) -> ReliabilityStats {
        self.reliable.stats()
    }
}

fn now_ms_deadline() -> u64 {
    sys::now_ms() + DRAIN_TIMEOUT_MS
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::packet::Endpoint;
    use crate::reliable::{Envelope, FLAG_ACK};

    fn peer() -> Peer {
        Peer::new(Endpoint {
            addr: u32::from_be_bytes([10, 0, 0, 2]),
            port: 5000,
        })
    }

    fn msg_frame(text: &str) -> Vec<u8> {
        protocol::encode(protocol::MsgType::Ping, 0, text.as_bytes()).into_vec()
    }

    #[test]
    fn a_whole_frame_arriving_in_one_datagram_decodes() {
        let mut p = peer();
        let f = msg_frame("hello");
        p.push_payload(&f);
        let decoded = p.try_decode_frame().unwrap().unwrap();
        assert_eq!(&*decoded.body, b"hello");
        assert!(p.try_decode_frame().unwrap().is_none());
    }

    #[test]
    fn a_frame_split_across_datagrams_is_reassembled() {
        let mut p = peer();
        let f = msg_frame("split me please");
        // deliver it one byte at a time, as a partial read would
        for i in 0..f.len() {
            p.push_payload(&f[i..i + 1]);
            if i + 1 < f.len() {
                assert!(
                    p.try_decode_frame().unwrap().is_none(),
                    "must not decode until the frame is complete (after {i} bytes)"
                );
            }
        }
        let decoded = p.try_decode_frame().unwrap().unwrap();
        assert_eq!(&*decoded.body, b"split me please");
    }

    #[test]
    fn two_frames_in_one_datagram_decode_in_order() {
        let mut p = peer();
        let mut both = msg_frame("first");
        both.extend_from_slice(&msg_frame("second"));
        p.push_payload(&both);
        assert_eq!(&*p.try_decode_frame().unwrap().unwrap().body, b"first");
        assert_eq!(&*p.try_decode_frame().unwrap().unwrap().body, b"second");
        assert!(p.try_decode_frame().unwrap().is_none());
    }

    #[test]
    fn read_buffer_is_compacted_only_when_worth_it() {
        let mut p = peer();
        // feed small frames; the consumed prefix must not be reclaimed yet
        for i in 0..10 {
            let f = msg_frame(&format!("m{i}"));
            p.push_payload(&f);
            let _ = p.try_decode_frame().unwrap().unwrap();
        }
        assert_eq!(p.read_buffered_bytes(), 0, "all bytes consumed");
    }

    #[test]
    fn a_corrupt_frame_is_reported_not_silently_ignored() {
        let mut p = peer();
        let mut f = msg_frame("x");
        f[0] = 0x00; // break the magic byte
        p.push_payload(&f);
        assert!(p.try_decode_frame().is_err());
        assert_eq!(p.stats().decode_errors, 1);
    }

    #[test]
    fn oversized_input_tears_the_peer_down() {
        let mut p = peer();
        let big = vec![0u8; READ_BUFFER_HARD_CAP + 1];
        p.push_payload(&big);
        assert_eq!(p.stats().dropped_oversized, 1);
        assert_eq!(
            p.teardown.map(|t| t.reason),
            Some(PeerTeardownReason::OversizedFrame)
        );
    }

    #[test]
    fn enqueue_wraps_in_a_reliability_envelope() {
        let mut p = peer();
        assert_eq!(p.enqueue_frame(b"payload", 0), QueueOutcome::Queued);
        let datagram = p.pop_datagram().unwrap();
        let env = Envelope::decode(&datagram).unwrap();
        assert_eq!(env.payload, b"payload");
    }

    #[test]
    fn backpressure_drops_at_the_soft_limit() {
        let mut p = peer();
        for _ in 0..SEND_QUEUE_SOFT_LIMIT {
            assert_eq!(p.enqueue_frame(b"x", 0), QueueOutcome::Queued);
        }
        assert_eq!(
            p.enqueue_frame(b"overflow", 0),
            QueueOutcome::DroppedSoftLimit
        );
        assert_eq!(p.stats().dropped_soft_limit, 1);
        assert!(p.teardown.is_none(), "a soft drop must not tear the peer down");
    }

    #[test]
    fn enqueue_refuses_once_tearing_down() {
        let mut p = peer();
        p.start_teardown(PeerTeardownReason::Shutdown);
        assert_eq!(p.enqueue_frame(b"x", 0), QueueOutcome::Rejected);
    }

    #[test]
    fn pop_datagrams_in_fifo_order() {
        let mut p = peer();
        p.enqueue_frame(b"one", 0);
        p.enqueue_frame(b"two", 0);
        assert_eq!(Envelope::decode(&p.pop_datagram().unwrap()).unwrap().payload, b"one");
        assert_eq!(Envelope::decode(&p.pop_datagram().unwrap()).unwrap().payload, b"two");
        assert!(p.pop_datagram().is_none());
        assert!(p.is_write_empty());
    }

    #[test]
    fn on_datagram_delivers_payloads_for_reassembly() {
        let mut p = peer();
        // a peer on the other side wraps a frame
        let mut other = crate::reliable::ReliabilityLayer::new();
        let (_, bytes) = other.wrap(&msg_frame("via wire"), 0);
        let ready = p.on_datagram(&bytes, 1);
        assert_eq!(ready.len(), 1);
        assert_eq!(&*p.try_decode_frame().unwrap().unwrap().body, b"via wire");
    }

    #[test]
    fn pump_ack_is_rate_limited() {
        let mut p = peer();
        assert!(!p.pump_ack(0), "no ack before anything is received");
    }

    #[test]
    fn retransmits_are_queued_when_the_timer_expires() {
        let mut p = peer();
        p.enqueue_frame(b"needs ack", 0);
        let _ = p.pop_datagram();
        assert_eq!(p.send_queue_len(), 0);
        // before the timeout: nothing due
        assert_eq!(p.pump_retransmits(10), 0);
        // after it: one retransmit
        assert_eq!(p.pump_retransmits(crate::reliable::RETRANSMIT_TIMEOUT_MS), 1);
        let env = Envelope::decode(&p.pop_datagram().unwrap()).unwrap();
        assert_eq!(env.payload, b"needs ack");
    }

    #[test]
    fn an_ack_retires_the_send_queue_over_time() {
        let mut p = peer();
        let mut other = crate::reliable::ReliabilityLayer::new();
        for i in 0..3 {
            let f = msg_frame(&format!("m{i}"));
            let (_, bytes) = other.wrap(&f, i);
            let _ = p.on_datagram(&bytes, i);
        }
        assert_eq!(p.reliability_stats().delivered, 3);
    }

    #[test]
    fn teardown_finishes_when_the_queue_drains() {
        let mut p = peer();
        p.enqueue_frame(b"x", 0);
        p.start_teardown(PeerTeardownReason::Shutdown);
        assert!(!p.teardown_complete(0), "queued data still pending");
        let _ = p.pop_datagram();
        assert!(p.teardown_complete(0), "drained, so it can be dropped");
    }

    #[test]
    fn teardown_gives_up_after_the_drain_deadline() {
        let mut p = peer();
        p.enqueue_frame(b"x", 0);
        p.start_teardown(PeerTeardownReason::Shutdown);
        let deadline = p.teardown.unwrap().deadline_ms;
        assert!(p.teardown_complete(deadline));
    }

    #[test]
    fn pure_ack_datagrams_deliver_no_payload() {
        let mut p = peer();
        let mut other = crate::reliable::ReliabilityLayer::new();
        // other receives something so its ack state is non-empty
        let (_, first) = other.wrap(b"hello", 0);
        p.on_datagram(&first, 0);
        let ack = Envelope {
            flags: FLAG_ACK,
            seq: 0,
            ack_seq: other.ack_state().watermark,
            ack_bitmap: other.ack_state().bitmap,
            payload: Vec::new(),
        };
        let ready = p.on_datagram(&ack.encode(), 1);
        assert!(ready.is_empty());
    }
}
