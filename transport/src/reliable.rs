//! Application-level reliability on top of an unreliable datagram transport.
//!
//! Raw IP gives us a packet stream with no delivery guarantee, no ordering and
//! no flow control. Everything TCP used to provide is rebuilt here, explicitly:
//!
//! * **sequencing** — every outbound datagram carries a monotonic sequence
//!   number in a fixed 12-byte [`ENVELOPE_HEADER_LEN`] header;
//! * **acknowledgement** — the receiver piggybacks a cumulative ack plus a 16-bit
//!   bitmap onto its own traffic, and sends a standalone ack when idle;
//! * **retransmission** — unacknowledged datagrams are resent after
//!   [`RETRANSMIT_TIMEOUT_MS`], up to [`MAX_ATTEMPTS`] tries;
//! * **reordering** — out-of-order arrivals are buffered and delivered in
//!   sequence order;
//! * **deduplication** — a retransmitted datagram that arrives after its original
//!   is dropped rather than delivered twice.
//!
//! # Why at the application layer
//!
//! [`crate::protocol`] is strictly request–response in application order, and
//! `Delivered(seq)` already gives the application an end-to-end ordering
//! guarantee. Rebuilding TCP's byte-stream semantics on top of a packet
//! transport would mean re-deriving ordering from the wrong layer; doing it here
//! keeps the guarantees aligned with what the protocol actually needs.
//!
//! # Scope
//!
//! This module is pure state and logic — no sockets — so it is exercised by unit
//! tests on any host, privileged or not. [`crate::sys`] and [`crate::reactor`]
//! do the actual I/O.

/// length of the fixed reliability header in front of every datagram payload
///
/// Layout: `[magic:1][version:1][flags:1][reserved:1][seq:4][ack_seq:4][bitmap:2]`
pub const ENVELOPE_HEADER_LEN: usize = 14;

/// envelope magic byte
pub const ENVELOPE_MAGIC: u8 = 0xE1;

/// envelope version
pub const ENVELOPE_VERSION: u8 = 1;

/// resend an unacknowledged datagram after this long
pub const RETRANSMIT_TIMEOUT_MS: u64 = 120;

/// give up on a datagram after this many send attempts
pub const MAX_ATTEMPTS: u32 = 6;

/// send a standalone ack when this long has passed with nothing to piggyback on
pub const ACK_INTERVAL_MS: u64 = 40;

/// width of the ack bitmap, in sequence numbers
pub const ACK_BITMAP_BITS: u32 = 16;

/// envelope flag: sender wants an ack
pub const FLAG_ACK_REQ: u8 = 0b0000_0001;

/// envelope flag: this datagram is itself an ack
pub const FLAG_ACK: u8 = 0b0000_0010;

/// number of sequence numbers covered by the ack bitmap
pub const ACK_WINDOW: u32 = ACK_BITMAP_BITS;

/// modular sequence comparison, valid across a window smaller than 2^31
///
/// Sequence numbers are `u32` and wrap; comparing them with `<` would break at
/// the wrap point and silently drop a large number of datagrams. These helpers
/// do the subtraction in wrapping arithmetic and interpret the difference as
/// signed, which is correct for any window under half the sequence space — far
/// more than the 16-entry ack bitmap plus the retransmit window.
fn seq_diff(a: u32, b: u32) -> i32 {
    a.wrapping_sub(b) as i32
}

/// `a < b` in modular sequence space
pub fn seq_lt(a: u32, b: u32) -> bool {
    seq_diff(a, b) < 0
}

/// `a <= b` in modular sequence space
pub fn seq_le(a: u32, b: u32) -> bool {
    seq_diff(a, b) <= 0
}

/// a decoded reliability envelope
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Envelope {
    pub flags: u8,
    pub seq: u32,
    /// highest sequence number received contiguously by the peer
    pub ack_seq: u32,
    /// bitmap of sequences received *after* `ack_seq`
    pub ack_bitmap: u16,
    /// the carried datagram payload (a protocol frame)
    pub payload: Vec<u8>,
}

impl Envelope {
    /// encode as `[12-byte header][payload]`
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(ENVELOPE_HEADER_LEN + self.payload.len());
        out.push(ENVELOPE_MAGIC);
        out.push(ENVELOPE_VERSION);
        out.push(self.flags);
        out.push(0); // reserved
        out.extend_from_slice(&self.seq.to_le_bytes());
        out.extend_from_slice(&self.ack_seq.to_le_bytes());
        out.extend_from_slice(&self.ack_bitmap.to_le_bytes());
        out.extend_from_slice(&self.payload);
        out
    }

    /// decode an envelope, returning `None` for anything malformed
    pub fn decode(buf: &[u8]) -> Option<Envelope> {
        if buf.len() < ENVELOPE_HEADER_LEN {
            return None;
        }
        if buf[0] != ENVELOPE_MAGIC || buf[1] != ENVELOPE_VERSION {
            return None;
        }
        let flags = buf[2];
        let seq = u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]);
        let ack_seq = u32::from_le_bytes([buf[8], buf[9], buf[10], buf[11]]);
        let ack_bitmap = u16::from_le_bytes([buf[12], buf[13]]);
        Some(Envelope {
            flags,
            seq,
            ack_seq,
            ack_bitmap,
            payload: buf[ENVELOPE_HEADER_LEN..].to_vec(),
        })
    }
}

/// build the ack state a peer should be told about
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AckState {
    /// highest sequence number received contiguously. Only meaningful once
    /// [`AckState::started`] is set — before the first datagram there is no
    /// watermark, and sequence 0 must not be mistaken for one.
    pub watermark: u32,
    /// bitmap of the [`ACK_WINDOW`] sequence numbers after `watermark`
    pub bitmap: u16,
    /// false until the first datagram is recorded
    pub started: bool,
}

impl AckState {
    /// record that `seq` arrived, promoting the watermark where possible
    ///
    /// `watermark` is the highest sequence number received *contiguously*; the
    /// first sequence number never received is therefore `watermark + 1`.
    pub fn record(&mut self, seq: u32) {
        if !self.started {
            self.started = true;
            self.watermark = seq;
            self.bitmap = 0;
            return;
        }
        let ahead = seq_diff(seq, self.watermark);
        if ahead <= 0 {
            return; // already covered
        }
        if ahead as u32 > ACK_WINDOW {
            // jumped beyond the bitmap: the gap is unrecoverable, so the
            // watermark moves anyway and the missing range is reported lost.
            self.watermark = seq;
            self.bitmap = 0;
            return;
        }
        self.bitmap |= 1u16 << (ahead - 1);
        // promote while the lowest bit is set. Promotion stops at the first
        // hole, so the watermark only ever names a CONTIGUOUS run — which is
        // what lets the reorder buffer detect the gap in front of it.
        while self.bitmap & 1 != 0 {
            self.bitmap >>= 1;
            self.watermark = self.watermark.wrapping_add(1);
        }
    }

    /// true when `seq` has already been received
    ///
    /// The watermark is the highest *contiguous* sequence number received, so it
    /// itself has been received and counts as a duplicate. Everything at or
    /// below it is also covered.
    pub fn contains(&self, seq: u32) -> bool {
        if !self.started {
            return false; // nothing received yet, so nothing is a duplicate
        }
        let ahead = seq_diff(seq, self.watermark);
        if ahead <= 0 {
            return true; // at or below the watermark
        }
        if ahead as u32 > ACK_WINDOW {
            return false;
        }
        // bit (ahead - 1) covers the sequence numbers just past the watermark
        self.bitmap & (1u16 << (ahead - 1)) != 0
    }
}

/// one datagram awaiting acknowledgement
#[derive(Debug, Clone)]
struct OutEntry {
    payload: Vec<u8>,
    last_sent_ms: u64,
    attempts: u32,
}

/// statistics useful for tests and logging
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ReliabilityStats {
    pub sent: u64,
    pub retransmits: u64,
    pub acked: u64,
    pub dropped_unacked: u64,
    pub delivered: u64,
    pub dedup_dropped: u64,
    pub buffered_out_of_order: u64,
    pub out_of_window: u64,
}

/// the reliability state for one peer
#[derive(Debug)]
pub struct ReliabilityLayer {
    // send side
    next_seq: u32,
    outbox: std::collections::BTreeMap<u32, OutEntry>,
    // receive side
    ack: AckState,
    /// lowest sequence number received but not yet delivered to the handler
    next_rx: u32,
    /// received but not yet contiguous, waiting for the gap to fill
    pending_rx: std::collections::BTreeMap<u32, Vec<u8>>,
    last_ack_ms: u64,
    stats: ReliabilityStats,
}

impl Default for ReliabilityLayer {
    fn default() -> Self {
        Self::new()
    }
}

impl ReliabilityLayer {
    /// a fresh layer
    ///
    /// Sequence numbers start at 0. [`AckState::watermark`] is initialised to 0
    /// meaning "nothing received yet, next expected is 1", and the sender's first
    /// datagram therefore carries sequence 1 — see [`Self::next_seq`].
    pub fn new() -> Self {
        Self {
            next_seq: 1,
            outbox: std::collections::BTreeMap::new(),
            ack: AckState::default(),
            next_rx: 1,
            pending_rx: std::collections::BTreeMap::new(),
            last_ack_ms: 0,
            stats: ReliabilityStats::default(),
        }
    }

    /// wrap `payload` for sending, returning its sequence number and the bytes
    /// to put in the datagram
    ///
    /// The returned buffer is the encoded envelope; the caller fragments it for
    /// transmission and keeps the sequence number for retransmission.
    pub fn wrap(&mut self, payload: &[u8], now_ms: u64) -> (u32, Vec<u8>) {
        let seq = self.next_seq;
        self.next_seq = self.next_seq.wrapping_add(1);
        let env = Envelope {
            flags: FLAG_ACK_REQ,
            seq,
            ack_seq: self.ack.watermark,
            ack_bitmap: self.ack.bitmap,
            payload: payload.to_vec(),
        };
        self.outbox.insert(
            seq,
            OutEntry {
                payload: payload.to_vec(),
                last_sent_ms: now_ms,
                attempts: 1,
            },
        );
        self.stats.sent += 1;
        self.last_ack_ms = now_ms;
        (seq, env.encode())
    }

    /// build a standalone ack, or `None` when it is not yet due
    ///
    /// Acking is cheap enough to do per datagram, but an idle peer still needs
    /// to tell the sender it is alive, so this allows one out-of-band ack every
    /// [`ACK_INTERVAL_MS`].
    pub fn ack_if_due(&self, now_ms: u64) -> Option<Vec<u8>> {
        if now_ms.saturating_sub(self.last_ack_ms) < ACK_INTERVAL_MS {
            return None;
        }
        if !self.ack.started {
            return None; // nothing received, so there is nothing to confirm
        }
        let env = Envelope {
            flags: FLAG_ACK,
            seq: 0,
            ack_seq: self.ack.watermark,
            ack_bitmap: self.ack.bitmap,
            payload: Vec::new(),
        };
        Some(env.encode())
    }

    /// datagrams to (re)send right now, as `(seq, encoded envelope)`
    ///
    /// The returned bytes are a complete envelope carrying the original payload
    /// under the original sequence number, plus this node's current ack state —
    /// a retransmit has to be a first-class datagram, not a bare payload.
    /// Entries past [`MAX_ATTEMPTS`] are dropped and counted in
    /// [`ReliabilityStats::dropped_unacked`].
    pub fn due_for_retransmit(&mut self, now_ms: u64, timeout_ms: u64) -> Vec<(u32, Vec<u8>)> {
        let mut out = Vec::new();
        let mut dead = Vec::new();
        for (seq, entry) in self.outbox.iter_mut() {
            if entry.attempts >= MAX_ATTEMPTS {
                dead.push(*seq);
                continue;
            }
            if now_ms.saturating_sub(entry.last_sent_ms) >= timeout_ms {
                entry.attempts += 1;
                entry.last_sent_ms = now_ms;
                let env = Envelope {
                    flags: FLAG_ACK_REQ,
                    seq: *seq,
                    ack_seq: self.ack.watermark,
                    ack_bitmap: self.ack.bitmap,
                    payload: entry.payload.clone(),
                };
                out.push((*seq, env.encode()));
            }
        }
        for seq in dead {
            self.outbox.remove(&seq);
            self.stats.dropped_unacked += 1;
        }
        if !out.is_empty() {
            self.stats.retransmits += out.len() as u64;
        }
        out
    }

    /// apply an ack received from the peer, retiring anything it covers
    pub fn on_ack(&mut self, env: &Envelope) {
        // everything at or below the cumulative ack is confirmed
        let covered: Vec<u32> = self
            .outbox
            .keys()
            .copied()
            .take_while(|s| seq_le(*s, env.ack_seq))
            .collect();
        for seq in covered {
            self.outbox.remove(&seq);
            self.stats.acked += 1;
        }
        // then anything named in the bitmap
        for bit in 0..ACK_WINDOW {
            if env.ack_bitmap & (1u16 << bit) == 0 {
                continue;
            }
            let seq = env.ack_seq.wrapping_add(bit + 1);
            if self.outbox.remove(&seq).is_some() {
                self.stats.acked += 1;
            }
        }
    }

    /// ingest an inbound envelope, returning every payload that has become
    /// deliverable as a result — usually zero or one, but a gap closing can
    /// release a whole run
    ///
    /// Out-of-order arrivals are buffered until the gap in front of them fills.
    /// Duplicates are dropped. Returning a `Vec` matters: yielding only the first
    /// payload of a run while discarding the rest would silently lose datagrams.
    pub fn on_envelope(&mut self, env: &Envelope, now_ms: u64) -> Vec<Vec<u8>> {
        // Piggybacked ack state counts even on a data datagram: it is how the
        // sender learns which of our datagrams we have already received.
        if env.flags & FLAG_ACK_REQ != 0 {
            self.on_ack(env);
        }

        // A pure ack, or any envelope with no payload, carries nothing to
        // deliver.
        if env.flags & FLAG_ACK != 0 || env.payload.is_empty() {
            return Vec::new();
        }

        // Anything BELOW the lowest undelivered sequence has already been handed
        // on. `next_rx` itself has not, so it must be admitted.
        if seq_diff(env.seq, self.next_rx) < 0 {
            self.stats.dedup_dropped += 1;
            return Vec::new();
        }

        // Too far ahead of the window to ever be delivered in order.
        if seq_diff(env.seq, self.next_rx) > ACK_WINDOW as i32 {
            self.stats.out_of_window += 1;
            return Vec::new();
        }

        self.last_ack_ms = now_ms;
        self.ack.record(env.seq);
        self.pending_rx.insert(env.seq, env.payload.clone());

        // Deliver the longest contiguous run that is now complete. `next_rx`
        // tracks the lowest sequence number still undelivered, which is the
        // right anchor here: the watermark can leap over sequences whose
        // payloads are still buffered.
        let mut ready = Vec::new();
        while let Some(p) = self.pending_rx.remove(&self.next_rx) {
            ready.push(p);
            self.next_rx = self.next_rx.wrapping_add(1);
        }

        if ready.is_empty() {
            // A gap in front of it is still outstanding; it stays buffered.
            self.stats.buffered_out_of_order += 1;
            return Vec::new();
        }

        self.stats.delivered += ready.len() as u64;
        ready
    }

    /// datagrams buffered for reordering
    pub fn buffered(&self) -> usize {
        self.pending_rx.len()
    }

    /// datagrams still awaiting acknowledgement
    pub fn unacked(&self) -> usize {
        self.outbox.len()
    }

    pub fn stats(&self) -> ReliabilityStats {
        self.stats
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(seq: u32, payload: &[u8]) -> Envelope {
        Envelope {
            flags: FLAG_ACK_REQ,
            seq,
            ack_seq: 0,
            ack_bitmap: 0,
            payload: payload.to_vec(),
        }
    }

    #[test]
    fn envelope_round_trips() {
        let e = env(42, b"payload");
        let bytes = e.encode();
        assert_eq!(bytes.len(), ENVELOPE_HEADER_LEN + 7);
        let d = Envelope::decode(&bytes).unwrap();
        assert_eq!(d.seq, 42);
        assert_eq!(d.payload, b"payload");
    }

    #[test]
    fn envelope_rejects_garbage() {
        assert!(Envelope::decode(&[]).is_none());
        assert!(Envelope::decode(&[0u8; 4]).is_none());
        let mut bad = env(1, b"x").encode();
        bad[0] = 0x00;
        assert!(Envelope::decode(&bad).is_none());
        bad = env(1, b"x").encode();
        bad[1] = 0x99;
        assert!(Envelope::decode(&bad).is_none());
    }

    #[test]
    fn delivers_in_order() {
        let mut tx = ReliabilityLayer::new();
        let mut rx = ReliabilityLayer::new();
        for i in 1..=5u32 {
            let (_, bytes) = tx.wrap(format!("m{i}").as_bytes(), i as u64);
            let e = Envelope::decode(&bytes).unwrap();
            let got = rx.on_envelope(&e, i as u64);
            assert_eq!(got.len(), 1, "in-order delivery yields one payload");
            assert_eq!(got[0], format!("m{i}").as_bytes());
        }
    }

    #[test]
    fn buffers_out_of_order_and_drains_on_gap_fill() {
        let mut tx = ReliabilityLayer::new();
        let mut rx = ReliabilityLayer::new();
        let mut envs = Vec::new();
        for i in 1..=3u32 {
            let (_, b) = tx.wrap(format!("m{i}").as_bytes(), i as u64);
            envs.push(Envelope::decode(&b).unwrap());
        }
        // deliver 3, then 2: both wait behind the missing 1
        assert!(rx.on_envelope(&envs[2], 3).is_empty(), "3 must wait for 1");
        assert!(rx.on_envelope(&envs[1], 3).is_empty(), "2 must wait for 1");
        assert_eq!(rx.buffered(), 2);

        // 1 closes the gap, so the WHOLE run becomes deliverable at once. All
        // three must come back: yielding only the first would drop 2 and 3.
        let ready = rx.on_envelope(&envs[0], 3);
        assert_eq!(ready, vec![b"m1".to_vec(), b"m2".to_vec(), b"m3".to_vec()]);
        assert_eq!(rx.buffered(), 0);
    }

    #[test]
    fn duplicate_is_dropped() {
        let mut tx = ReliabilityLayer::new();
        let mut rx = ReliabilityLayer::new();
        let (_, b) = tx.wrap(b"once", 1);
        let e = Envelope::decode(&b).unwrap();
        assert_eq!(rx.on_envelope(&e, 1), vec![b"once".to_vec()]);
        assert!(rx.on_envelope(&e, 1).is_empty(), "duplicate must not deliver");
        assert_eq!(rx.stats().dedup_dropped, 1);
    }

    #[test]
    fn ack_retires_outbox_entries() {
        let mut tx = ReliabilityLayer::new();
        let mut rx = ReliabilityLayer::new();

        // tx sends four; rx receives them all. rx now knows all four arrived,
        // but tx cannot know that until rx's own traffic piggybacks an ack.
        let mut sent = Vec::new();
        for i in 1..=4u32 {
            let (_, b) = tx.wrap(format!("m{i}").as_bytes(), i as u64);
            sent.push(Envelope::decode(&b).unwrap());
        }
        assert_eq!(tx.unacked(), 4);
        for (i, e) in sent.iter().enumerate() {
            rx.on_envelope(e, i as u64);
        }

        // rx replies on its OWN layer; that datagram piggybacks the ack
        let (_, reply_bytes) = rx.wrap(b"reply", 10);
        let reply = Envelope::decode(&reply_bytes).unwrap();
        assert_eq!(reply.ack_seq, 4, "reply carries the cumulative ack");
        assert_eq!(reply.ack_bitmap, 0);

        // tx receives the reply and applies its piggybacked ack state
        tx.on_envelope(&reply, 10);
        assert_eq!(tx.unacked(), 0, "all four should be acknowledged");
        assert_eq!(tx.stats().acked, 4);
    }

    #[test]
    fn retransmits_after_timeout_and_stops_after_max_attempts() {
        let mut tx = ReliabilityLayer::new();
        let (_, b) = tx.wrap(b"data", 0);
        let e = Envelope::decode(&b).unwrap();

        // nothing due immediately
        assert!(tx.due_for_retransmit(0, RETRANSMIT_TIMEOUT_MS).is_empty());
        // due once the timeout elapses; the retransmit is a full envelope
        let due = tx.due_for_retransmit(RETRANSMIT_TIMEOUT_MS, RETRANSMIT_TIMEOUT_MS);
        assert_eq!(due.len(), 1);
        let env = Envelope::decode(&due[0].1).unwrap();
        assert_eq!(env.seq, due[0].0);
        assert_eq!(env.payload, b"data");
        assert_eq!(tx.stats().retransmits, 1);

        // keep retrying until the attempt budget is spent, then give up
        let mut t = RETRANSMIT_TIMEOUT_MS;
        for _ in 0..MAX_ATTEMPTS {
            t += RETRANSMIT_TIMEOUT_MS;
            tx.due_for_retransmit(t, RETRANSMIT_TIMEOUT_MS);
        }
        assert_eq!(tx.unacked(), 0, "entry must be dropped after MAX_ATTEMPTS");
        assert_eq!(tx.stats().dropped_unacked, 1);
        let _ = e;
    }

    #[test]
    fn a_lost_datagram_is_recovered_by_retransmission() {
        let mut tx = ReliabilityLayer::new();
        let mut rx = ReliabilityLayer::new();
        let (_, b1) = tx.wrap(b"first", 0);
        let e1 = Envelope::decode(&b1).unwrap();

        // the first datagram is lost in flight
        let due = tx.due_for_retransmit(RETRANSMIT_TIMEOUT_MS, RETRANSMIT_TIMEOUT_MS);
        assert_eq!(due.len(), 1);
        // ... and the resend lands
        assert_eq!(rx.on_envelope(&e1, 1), vec![b"first".to_vec()]);
        // the receiver's ack state now covers sequence 1
        assert!(rx.ack.contains(1));
        // and feeding that ack state back to the sender clears the outbox
        tx.on_ack(&Envelope {
            flags: FLAG_ACK,
            seq: 0,
            ack_seq: rx.ack.watermark,
            ack_bitmap: rx.ack.bitmap,
            payload: Vec::new(),
        });
        assert_eq!(tx.unacked(), 0, "ack should have retired the retransmit");
    }

    #[test]
    fn ack_bitmap_covers_gap() {
        let mut a = AckState::default();
        a.record(1);
        // receive 3 first: it lands in the bitmap one past the watermark
        a.record(3);
        assert_eq!(a.watermark, 1);
        assert!(a.contains(3));
        assert!(!a.contains(2));
        // filling 2 promotes both
        a.record(2);
        assert_eq!(a.watermark, 3);
        assert!(a.contains(3));
        assert!(!a.contains(4));
    }

    #[test]
    fn sequence_comparison_survives_wraparound() {
        // sequences near u32::MAX wrap to small numbers; comparisons must keep
        // working across that boundary for any window under 2^31
        let near_max = u32::MAX - 1;
        let wrapped = near_max.wrapping_add(2); // == 1
        assert!(seq_lt(near_max, wrapped));
        assert!(seq_le(near_max, wrapped));
        assert!(!seq_lt(wrapped, near_max));
        assert!(seq_diff(near_max, wrapped) == -2);
    }

    #[test]
    fn ack_state_handles_wraparound() {
        let mut a = AckState::default();
        a.record(u32::MAX);
        a.record(1); // wrapped past the end to 1
        assert!(a.contains(1));
        a.record(0);
        assert_eq!(a.watermark, 1);
    }

    #[test]
    fn standalone_ack_is_rate_limited() {
        let mut rx = ReliabilityLayer::new();
        let (_, b) = {
            let mut tx = ReliabilityLayer::new();
            tx.wrap(b"x", 0)
        };
        let e = Envelope::decode(&b).unwrap();
        rx.on_envelope(&e, 0);
        assert!(rx.ack_if_due(0).is_none(), "must not ack immediately");
        assert!(rx.ack_if_due(ACK_INTERVAL_MS).is_some());
    }

    #[test]
    fn pure_ack_delivers_nothing() {
        let mut rx = ReliabilityLayer::new();
        let e = Envelope {
            flags: FLAG_ACK,
            seq: 0,
            ack_seq: 0,
            ack_bitmap: 0,
            payload: Vec::new(),
        };
        assert!(rx.on_envelope(&e, 0).is_empty());
    }

    #[test]
    fn empty_payload_is_not_delivered() {
        let mut tx = ReliabilityLayer::new();
        let mut rx = ReliabilityLayer::new();
        let (_, b) = tx.wrap(b"", 0);
        let e = Envelope::decode(&b).unwrap();
        assert!(rx.on_envelope(&e, 0).is_empty());
    }

    /// Drive two reliability layers through a deliberately hostile link:
    /// datagrams are dropped and reordered, so retransmission, the reorder
    /// buffer and dedup all have to work together for every message to arrive
    /// exactly once and in order.
    #[test]
    fn survives_a_lossy_reordering_link() {
        let mut seed: u64 = 0x2545_F491_4F6C_DD1D;
        let mut rand = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };

        let mut tx = ReliabilityLayer::new();
        let mut rx = ReliabilityLayer::new();

        const MESSAGES: usize = 30;
        let mut expected: Vec<String> = Vec::new();
        // datagrams queued for delivery, each with the tick it becomes wire-ready
        let mut wire: Vec<Vec<u8>> = Vec::new();
        let mut received: Vec<String> = Vec::new();
        let mut now = 0u64;
        let mut ticks = 0;

        while received.len() < MESSAGES {
            now += 5;
            ticks += 1;
            assert!(ticks < 4_000, "link made no progress");

            // keep a few datagrams outstanding, like a real send window
            while expected.len() < MESSAGES && tx.unacked() < 4 {
                let text = format!("message-{}", expected.len());
                let (_, bytes) = tx.wrap(text.as_bytes(), now);
                expected.push(text);
                wire.push(bytes);
            }

            // acks flow back to the sender (this is what retires the outbox)
            if let Some(ack) = rx.ack_if_due(now) {
                tx.on_envelope(&Envelope::decode(&ack).unwrap(), now);
            }

            // retransmits join the wire
            for (_, bytes) in tx.due_for_retransmit(now, RETRANSMIT_TIMEOUT_MS) {
                wire.push(bytes);
            }

            // the link drops ~1 in 8 and reorders the rest
            wire.retain(|_| rand() % 8 != 0);
            for k in (1..wire.len()).rev() {
                let j = (rand() % (k as u64 + 1)) as usize;
                wire.swap(k, j);
            }

            for datagram in wire.drain(..) {
                let env = Envelope::decode(&datagram).expect("wire datagram decodes");
                for ready in rx.on_envelope(&env, now) {
                    received.push(String::from_utf8(ready).expect("utf8 payload"));
                }
            }
        }

        assert_eq!(
            received, expected,
            "every message must arrive exactly once and in order"
        );
    }
}
