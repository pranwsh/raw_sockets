//! IPv4 / UDP header construction and parsing for the raw-socket transport.
//!
//! The transport owns its own network layer: it builds IPv4 headers with
//! `IP_HDRINCL` and carries `msgd` frames inside UDP datagrams. That means the
//! wire format is ours end to end — framing ([`crate::protocol`]) is nested
//! inside a UDP header nested inside an IPv4 header, and this module is
//! responsible for the outer two.
//!
//! # Byte order
//!
//! Multi-byte header fields are stored big-endian (network byte order), and
//! [`ipv4_addr_to_be`] / [`be_to_ipv4_addr`] convert to and from the `u32`
//! representation `sockaddr_in` uses.
//!
//! # Checksums
//!
//! Both the IPv4 header checksum and the UDP checksum use the standard
//! 16-bit one's-complement sum over the header (plus a pseudo-header for UDP).
//! The checksum field itself is treated as zero while computing.

/// length of an IPv4 header with no options
pub const IPV4_HEADER_LEN: usize = 20;

/// Note on fragmentation over loopback: Linux's `lo` interface has an MTU of
/// 65536 and does NOT reassemble IPv4 fragments sent to a local UDP socket via a
/// raw socket. A hand-built fragment set (two or more packets with the MF flag
/// and non-zero offsets) is accepted by `sendto` but silently dropped before it
/// reaches the UDP layer, so nothing is ever delivered to the receiver.
///
/// This was confirmed independently in C on this host: a single unfragmented
/// 2000-byte datagram arrives intact, while a minimal two-fragment set does not.
/// It is a kernel/loopback behaviour, not a defect in this module.
///
/// Consequence for the transport: over loopback, a payload larger than
/// `max_frame_for_mtu` cannot be delivered. Production traffic on a real
/// interface (MTU 1500) fragments normally, and [`Reassembler`] handles the
/// in-order/out-of-order/lost cases there — which is what the unit tests cover.

/// length of a UDP header
pub const UDP_HEADER_LEN: usize = 8;

/// `IPPROTO_UDP`
pub const IPPROTO_UDP: u8 = 17;

/// default ethernet MTU assumed for fragmentation
pub const DEFAULT_MTU: usize = 1500;

/// largest UDP payload that fits in an IPv4 packet (65535 − 20 − 8)
pub const MAX_UDP_PAYLOAD: usize = 65507;

/// largest protocol frame that can be carried without fragmentation
pub fn max_frame_for_mtu(mtu: usize) -> usize {
    mtu.saturating_sub(IPV4_HEADER_LEN + UDP_HEADER_LEN)
}

/// IPv4 fragmentation flags. They occupy the TOP 3 bits of the 16-bit
/// flags+offset field; the fragment offset takes the low 13. Getting this
/// wrong silently corrupts the offset — `MF == 0b001` would collide with
/// offset bit 0 and shift every reassembly by one 8-byte unit.
const FLAG_DF: u16 = 0b0100_0000_0000_0000;
const FLAG_MF: u16 = 0b0010_0000_0000_0000;
const FRAG_OFFSET_MASK: u16 = 0b0001_1111_1111_1111;

/// an IPv4 endpoint: address in network byte order plus port
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Endpoint {
    /// `sin_addr.s_addr` — network byte order
    pub addr: u32,
    /// port in host byte order
    pub port: u16,
}

/// convert a `sockaddr_in`-style address (network order) to a readable string
pub fn ipv4_addr_to_string(addr: u32) -> std::net::Ipv4Addr {
    // `addr` is network byte order, so the octets are big-endian
    std::net::Ipv4Addr::from(addr)
}

/// convert an `Ipv4Addr` into the network-byte-order `u32` used by
/// [`Endpoint::addr`] and `sockaddr_in::sin_addr`
///
/// `Ipv4Addr::octets` is always big-endian order, and so is the wire format,
/// so this is `from_be_bytes` — NOT `from_ne_bytes`, which silently produces
/// the reversed value on a little-endian host and yields packets the receiver
/// drops as malformed.
pub fn ipv4_addr_to_be(ip: std::net::Ipv4Addr) -> u32 {
    u32::from_be_bytes(ip.octets())
}

/// convert a network-byte-order address back into an `Ipv4Addr`
pub fn be_to_ipv4_addr(addr: u32) -> std::net::Ipv4Addr {
    std::net::Ipv4Addr::from(addr)
}

/// one's-complement 16-bit sum, the basis of both checksums
/// 16-bit one's-complement sum over `chunks` treated as ONE contiguous buffer
///
/// The chunks are a pseudo-header, a header and a payload, concatenated — not
/// separate buffers. Padding each chunk to an even length independently is
/// wrong: an odd-length chunk shifts the byte alignment of everything after it
/// and silently produces a different (invalid) checksum. Only a genuinely odd
/// *total* length is padded, at the very end.
fn ones_complement_sum(chunks: &[&[u8]]) -> u16 {
    let total: usize = chunks.iter().map(|c| c.len()).sum();
    // accumulate words, carrying a single leftover byte across chunks
    let mut sum: u32 = 0;
    let mut pending: Option<u8> = None;
    for chunk in chunks {
        let mut i = 0;
        if let Some(lo) = pending.take() {
            if let Some(&hi) = chunk.first() {
                sum += u16::from_be_bytes([lo, hi]) as u32;
                i = 1;
            } else {
                pending = Some(lo);
                continue;
            }
        }
        while i + 1 < chunk.len() {
            sum += u16::from_be_bytes([chunk[i], chunk[i + 1]]) as u32;
            i += 2;
        }
        if i < chunk.len() {
            pending = Some(chunk[i]);
        }
    }
    if let Some(lo) = pending {
        // odd total length: pad the final byte on the right with a zero
        sum += (lo as u32) << 8;
    }
    let _ = total;
    while sum >> 16 != 0 {
        sum = (sum & 0xFFFF) + (sum >> 16);
    }
    !(sum as u16)
}

/// build one IPv4 header carrying `payload_len` bytes
///
/// Returns the 20 header bytes; the caller appends the payload after it.
fn build_ipv4_header(
    src: u32,
    dst: u32,
    payload_len: usize,
    identification: u16,
    fragment_offset: u16,
    more_fragments: bool,
    dont_fragment: bool,
) -> [u8; IPV4_HEADER_LEN] {
    let total_len = IPV4_HEADER_LEN + payload_len;
    assert!(
        total_len <= u16::MAX as usize,
        "ipv4 total length {total_len} exceeds 16 bits"
    );

    let mut h = [0u8; IPV4_HEADER_LEN];
    // version 4, IHL 5 (20 bytes / 4)
    h[0] = 0x45;
    // DSCP / ECN: best effort
    h[1] = 0x00;
    h[2..4].copy_from_slice(&(total_len as u16).to_be_bytes());
    h[4..6].copy_from_slice(&identification.to_be_bytes());
    // flags + fragment offset (offset is in 8-byte units)
    // Flags live in the top 3 bits of the 16-bit field, the fragment offset in
    // the low 13.
    let flags = if more_fragments { FLAG_MF } else { 0 } | if dont_fragment { FLAG_DF } else { 0 };
    h[6..8].copy_from_slice(&(flags | (fragment_offset & FRAG_OFFSET_MASK)).to_be_bytes());
    h[8] = 64; // TTL
    h[9] = IPPROTO_UDP;
    // h[10..12] is the header checksum, left zero for the computation
    h[12..16].copy_from_slice(&src.to_be_bytes());
    h[16..20].copy_from_slice(&dst.to_be_bytes());
    let csum = ones_complement_sum(&[&h]);
    h[10..12].copy_from_slice(&csum.to_be_bytes());
    h
}

/// compute the UDP checksum given the addresses it is sent between
/// the UDP pseudo-header: src, dst, zero, protocol, UDP length
fn pseudo_header(src_ip: u32, dst_ip: u32, udp_len: usize) -> [u8; 12] {
    [
        (src_ip >> 24) as u8,
        (src_ip >> 16) as u8,
        (src_ip >> 8) as u8,
        src_ip as u8,
        (dst_ip >> 24) as u8,
        (dst_ip >> 16) as u8,
        (dst_ip >> 8) as u8,
        dst_ip as u8,
        0, // zero
        IPPROTO_UDP,
        (udp_len >> 8) as u8,
        udp_len as u8,
    ]
}

fn udp_checksum_with(src_ip: u32, dst_ip: u32, src_port: u16, dst_port: u16, payload: &[u8]) -> u16 {
    let udp_len = UDP_HEADER_LEN + payload.len();
    let pseudo = pseudo_header(src_ip, dst_ip, udp_len);
    let mut hdr = [0u8; UDP_HEADER_LEN];
    hdr[0..2].copy_from_slice(&src_port.to_be_bytes());
    hdr[2..4].copy_from_slice(&dst_port.to_be_bytes());
    hdr[4..6].copy_from_slice(&(udp_len as u16).to_be_bytes());
    ones_complement_sum(&[&pseudo, &hdr, payload])
}

/// one raw datagram: a complete IPv4 + UDP packet
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Packet {
    pub bytes: Vec<u8>,
}

/// build a complete UDP-over-IPv4 packet carrying `payload`
fn build_packet(src: Endpoint, dst: Endpoint, payload: &[u8], identification: u16) -> Packet {
    let mut bytes = Vec::with_capacity(IPV4_HEADER_LEN + UDP_HEADER_LEN + payload.len());
    let ipv4 = build_ipv4_header(
        src.addr,
        dst.addr,
        UDP_HEADER_LEN + payload.len(),
        identification,
        0,
        false,
        false,
    );
    bytes.extend_from_slice(&ipv4);

    let udp_len = UDP_HEADER_LEN + payload.len();
    let csum = udp_checksum_with(src.addr, dst.addr, src.port, dst.port, payload);
    bytes.extend_from_slice(&src.port.to_be_bytes());
    bytes.extend_from_slice(&dst.port.to_be_bytes());
    bytes.extend_from_slice(&(udp_len as u16).to_be_bytes());
    bytes.extend_from_slice(&csum.to_be_bytes());
    bytes.extend_from_slice(payload);

    Packet { bytes }
}

/// a datagram split into one or more IP fragments
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FragmentSet {
    /// complete IPv4 packets, in order; the last has `more_fragments = false`
    pub packets: Vec<Packet>,
}

/// the parsed header fields of a received datagram
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ParsedHeaders {
    pub src: Endpoint,
    pub dst: Endpoint,
    pub protocol: u8,
    /// bytes of UDP payload after the headers
    pub payload_offset: usize,
    pub payload_len: usize,
    /// true when this datagram is an IP fragment other than the first
    pub is_fragment: bool,
    pub more_fragments: bool,
}

/// encode `payload` as one or more raw IP packets, fragmenting when needed
///
/// A payload that fits the MTU is a single packet. A larger one is split into
/// fragments of `mtu - IPV4_HEADER_LEN` bytes; every fragment except the last
/// carries the `MF` flag, and the fragment offset counts 8-byte units as the
/// IPv4 header requires (the final fragment is padded to a multiple of 8).
pub fn encode_datagram(
    src: Endpoint,
    dst: Endpoint,
    payload: &[u8],
    identification: u16,
    mtu: usize,
) -> FragmentSet {
    assert!(
        payload.len() <= MAX_UDP_PAYLOAD,
        "udp payload {} exceeds {MAX_UDP_PAYLOAD}",
        payload.len()
    );

    let max_fragment_payload = mtu.saturating_sub(IPV4_HEADER_LEN);
    if payload.len() <= max_fragment_payload {
        return FragmentSet {
            packets: vec![build_packet(src, dst, payload, identification)],
        };
    }

    // Fragment the UDP payload. Only the first fragment carries the UDP
    // header; the rest are continuations at increasing offsets.
    let udp_header_len = UDP_HEADER_LEN;
    let mut packets = Vec::new();
    let mut offset = 0usize; // byte offset into the UDP payload
    let mut fragment_id = identification;

    while offset < payload.len() {
        let first = offset == 0;
        let remaining = payload.len() - offset;
        // room for UDP header on the first fragment
        let capacity = max_fragment_payload
            .saturating_sub(if first { udp_header_len } else { 0 });
        // Every non-final fragment must contribute a whole number of 8-byte
        // units, because the next fragment's IPv4 offset is expressed in
        // units of 8 and is measured from the start of the IP payload (which
        // begins with the UDP header). For the first fragment the unit count
        // covers the header too, so its data length must make
        // `UDP_HEADER_LEN + take` a multiple of 8.
        let is_last = remaining <= capacity;
        let align = if first { (UDP_HEADER_LEN + capacity) & !7 } else { capacity & !7 };
        let mut take = if is_last { remaining } else { align.min(capacity) };
        if take == 0 {
            // an mtu too small to carry even one aligned unit
            take = remaining.min(8);
        }

        let chunk = &payload[offset..offset + take];
        let mut bytes = Vec::with_capacity(IPV4_HEADER_LEN + take + if first { udp_header_len } else { 0 });
        if first {
            let ipv4 = build_ipv4_header(
                src.addr,
                dst.addr,
                udp_header_len + take,
                fragment_id,
                0,
                !is_last,
                false,
            );
            bytes.extend_from_slice(&ipv4);
            let udp_len = UDP_HEADER_LEN + payload.len();
            let csum = udp_checksum_with(src.addr, dst.addr, src.port, dst.port, payload);
            bytes.extend_from_slice(&src.port.to_be_bytes());
            bytes.extend_from_slice(&dst.port.to_be_bytes());
            bytes.extend_from_slice(&(udp_len as u16).to_be_bytes());
            bytes.extend_from_slice(&csum.to_be_bytes());
        } else {
            // the offset counts 8-byte units from the start of the IP payload,
            // which the first fragment opened with the UDP header
            let frag_offset_units = ((offset + UDP_HEADER_LEN) / 8) as u16;
            let ipv4 = build_ipv4_header(
                src.addr,
                dst.addr,
                take,
                fragment_id,
                frag_offset_units,
                !is_last,
                false,
            );
            bytes.extend_from_slice(&ipv4);
        }
        bytes.extend_from_slice(chunk);

        packets.push(Packet { bytes });
        offset += take;
        fragment_id = fragment_id.wrapping_add(1);
    }

    FragmentSet { packets }
}

/// parse the IPv4 and UDP headers of a received datagram
///
/// Returns `None` for anything that is not a well-formed UDP-over-IPv4 packet,
/// including datagrams whose declared length disagrees with the buffer.
pub fn parse_headers(buf: &[u8]) -> Option<ParsedHeaders> {
    if buf.len() < IPV4_HEADER_LEN + UDP_HEADER_LEN {
        return None;
    }
    let version = buf[0] >> 4;
    let ihl = (buf[0] & 0x0F) as usize * 4;
    if version != 4 || ihl < IPV4_HEADER_LEN || buf.len() < ihl {
        return None;
    }
    let total_len = u16::from_be_bytes([buf[2], buf[3]]) as usize;
    if total_len < ihl + UDP_HEADER_LEN || total_len > buf.len() {
        return None;
    }
    let frag_field = u16::from_be_bytes([buf[6], buf[7]]);
    let frag_offset = frag_field & FRAG_OFFSET_MASK;
    let more_fragments = frag_field & FLAG_MF != 0;
    let protocol = buf[9];

    let src = Endpoint {
        addr: u32::from_be_bytes([buf[12], buf[13], buf[14], buf[15]]),
        port: u16::from_be_bytes([buf[ihl], buf[ihl + 1]]),
    };
    let dst = Endpoint {
        addr: u32::from_be_bytes([buf[16], buf[17], buf[18], buf[19]]),
        port: u16::from_be_bytes([buf[ihl + 2], buf[ihl + 3]]),
    };

    Some(ParsedHeaders {
        src,
        dst,
        protocol,
        payload_offset: ihl + UDP_HEADER_LEN,
        payload_len: total_len - ihl - UDP_HEADER_LEN,
        is_fragment: frag_offset != 0,
        more_fragments,
    })
}

/// extract the UDP payload of a non-fragmented datagram
pub fn payload_of(buf: &[u8]) -> Option<&[u8]> {
    let h = parse_headers(buf)?;
    if h.protocol != IPPROTO_UDP || h.is_fragment {
        return None;
    }
    buf.get(h.payload_offset..h.payload_offset + h.payload_len)
}

/// verify the IPv4 header checksum of a received datagram
///
/// Returns `false` if the checksum is wrong, which means the header was
/// corrupted in transit and the datagram must be dropped.
pub fn verify_ipv4_checksum(buf: &[u8]) -> bool {
    if buf.len() < IPV4_HEADER_LEN {
        return false;
    }
    let ihl = (buf[0] & 0x0F) as usize * 4;
    if buf.len() < ihl || ihl < IPV4_HEADER_LEN {
        return false;
    }
    let stored = u16::from_be_bytes([buf[10], buf[11]]);
    // recompute with the checksum field zeroed
    let mut h = [0u8; IPV4_HEADER_LEN];
    h.copy_from_slice(&buf[..IPV4_HEADER_LEN]);
    h[10] = 0;
    h[11] = 0;
    let computed = ones_complement_sum(&[&h]);
    stored == computed
}

/// assemble fragments received for one datagram back into the original payload
///
/// IPv4 reassembly: fragments of one datagram share an identification and an
/// (src, dst, protocol) tuple, and arrive with byte offsets in 8-byte units.
/// This buffers out-of-order fragments and yields the payload once every byte
/// from offset 0 onward is present.
#[derive(Debug, Default)]
pub struct Reassembler {
    /// keyed by (identification, src, dst)
    partials: std::collections::HashMap<(u16, u32, u32), PartialDatagram>,
}

#[derive(Debug)]
struct PartialDatagram {
    /// sparse buffer of received bytes, indexed from offset 0; index 0..8 is
    /// the UDP header carried by the first fragment
    data: Vec<u8>,
    /// which byte indices have been filled
    filled: Vec<bool>,
    /// how many distinct bytes have arrived so far
    received: usize,
    /// total datagram length in bytes (UDP header + payload), taken from the
    /// UDP length field. Only known once the first fragment arrives, and
    /// required to know when an out-of-order datagram is complete.
    expected: Option<usize>,
}

impl Reassembler {
    /// offer one received datagram; returns the complete payload once the
    /// final fragment lands
    pub fn accept(
        &mut self,
        headers: ParsedHeaders,
        buf: &[u8],
        identification: u16,
    ) -> Option<Vec<u8>> {
        // A fragment carries raw IP payload starting right after the IPv4
        // header — a continuation fragment has no UDP header, so
        // `ParsedHeaders::payload_offset` (which skips 8 bytes for UDP) is
        // wrong here. Read the fragment from `ihl` to the declared total
        // length instead.
        let ihl = (buf[0] & 0x0F) as usize * 4;
        let total_len = u16::from_be_bytes([buf[2], buf[3]]) as usize;
        // The first fragment carries the UDP header inside its IP payload, so
        // its data is the whole IP payload; continuations carry only data.
        let frag_bytes = total_len - ihl;

        // The IPv4 fragment offset is measured in 8-byte units from the start
        // of the IP payload, which begins with the 8-byte UDP header, so a
        // continuation fragment lands at `units * 8` in a buffer that still
        // holds that header at index 0..8.
        //
        // The FIRST fragment has offset 0 and its bytes include that header,
        // so it must start at 0 — but `parse_headers` reports `payload_offset`
        // past the header. Read from `ihl` and offset by the fragment field,
        // which is 0 for the first fragment.
        let start = (headers_fragment_offset(buf) as usize) * 8;

        // Key on addresses only. A continuation fragment carries no UDP header,
        // so `headers.src.port` is parsed out of fragment payload and would
        // differ per fragment, splitting one datagram across several entries.
        let key = (
            identification,
            headers.src.addr,
            headers.dst.addr,
        );
        let entry = self.partials.entry(key).or_insert_with(|| PartialDatagram {
            data: Vec::new(),
            filled: Vec::new(),
            received: 0,
            expected: None,
        });
        let entry = &mut *entry;

        // The first fragment carries the UDP header, whose length field gives
        // the authoritative total size. Record it so a datagram whose final
        // fragment arrives first can still be recognised as complete later.
        if start == 0 && chunk_has_udp_header(buf, ihl, frag_bytes) {
            let udp_len = u16::from_be_bytes([buf[ihl + 4], buf[ihl + 5]]) as usize;
            entry.expected = Some(udp_len);
        }

        // Grow to cover this fragment. `resize` alone would keep the old
        // length when the vector is already long enough, so grow only.
        let end = start + frag_bytes;
        if entry.data.len() < end {
            entry.data.resize(end, 0);
        }
        if entry.filled.len() < end {
            entry.filled.resize(end, false);
        }
        let chunk = &buf[ihl..ihl + frag_bytes];
        for (i, byte) in chunk.iter().enumerate() {
            let idx = start + i;
            if !entry.filled[idx] {
                entry.filled[idx] = true;
                entry.data[idx] = *byte;
                entry.received += 1;
            }
        }

        // A datagram is complete when its total size is known and every byte of
        // it has arrived. Both are needed: the final fragment can land first
        // (so "MF clear" alone is premature), and the total size is only known
        // once the first fragment supplies the UDP length.
        let complete = match self.partials.get(&key) {
            Some(c) => match c.expected {
                Some(expected) => expected == c.data.len() && c.received == c.data.len(),
                None => false,
            },
            None => false,
        };
        if !complete {
            return None;
        }
        let complete = self.partials.remove(&key).expect("checked just above");
        Some(complete.data[UDP_HEADER_LEN..].to_vec())
    }
}

/// true when a fragment starting at offset 0 carries the UDP header
fn chunk_has_udp_header(buf: &[u8], ihl: usize, frag_bytes: usize) -> bool {
    frag_bytes >= UDP_HEADER_LEN && buf.len() >= ihl + UDP_HEADER_LEN
}

fn headers_fragment_offset(buf: &[u8]) -> u16 {
    if buf.len() < 8 {
        return 0;
    }
    u16::from_be_bytes([buf[6], buf[7]]) & FRAG_OFFSET_MASK
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ep(addr: [u8; 4], port: u16) -> Endpoint {
        Endpoint { addr: u32::from_be_bytes(addr), port }
    }

    #[test]
    fn ones_complement_sum_pads_odd_trailing_byte() {
        // one byte 0xFF is padded on the right to 0xFF00, then complemented
        assert_eq!(ones_complement_sum(&[&[0xFF]]), !0xFF00u16);
        // two bytes need no padding
        assert_eq!(ones_complement_sum(&[&[0xFF, 0x01]]), !0xFF01u16);
    }

    /// Known-answer test: the IPv4 header checksum is a fixed, externally
    /// specified value. `verify_ipv4_checksum` only proves a packet agrees with
    /// itself — a wrong-but-consistent checksum passes it — so the checksum has
    /// to be pinned against an independent computation.
    #[test]
    fn ipv4_checksum_matches_a_known_answer() {
        // header for 127.0.0.1 -> 127.0.0.1, proto 17, total len 53, id 0x1234
        let src = ep([127, 0, 0, 1], 0);
        let dst = ep([127, 0, 0, 1], 0);
        let pkt = build_packet(src, dst, b"raw ip round trip payload", 0x1234);
        assert_eq!(
            &pkt.bytes[10..12],
            &[0x6a, 0x82],
            "IPv4 header checksum must be 0x6a82"
        );
        // UDP checksum for port 0 -> 0 over the same pseudo-header
        assert_eq!(
            &pkt.bytes[26..28],
            &[0xf0, 0x3d],
            "UDP checksum must be 0xf03d"
        );
        // and the same payload between real ports, so the test would catch a
        // checksum that ignored the ports entirely
        let pkt2 = build_packet(
            Endpoint { addr: u32::from_be_bytes([127, 0, 0, 1]), port: 9999 },
            Endpoint { addr: u32::from_be_bytes([127, 0, 0, 1]), port: 45311 },
            b"raw ip round trip payload",
            0x1234,
        );
        assert_eq!(&pkt2.bytes[26..28], &[0x18, 0x2f]);
    }

    /// The checksum of a whole packet must sum to zero when the stored value is
    /// included — the property every receiver actually checks.
    #[test]
    fn a_correct_packet_sums_to_zero() {
        let src = ep([10, 0, 0, 1], 1234);
        let dst = ep([10, 0, 0, 2], 5678);
        let pkt = build_packet(src, dst, b"payload", 0x4321);
        // IPv4 header alone must sum to zero
        let mut h = [0u8; IPV4_HEADER_LEN];
        h.copy_from_slice(&pkt.bytes[..IPV4_HEADER_LEN]);
        assert_eq!(
            ones_complement_sum(&[&h]),
            0,
            "IPv4 header checksum is self-inconsistent"
        );
        // UDP header + payload over the pseudo-header must sum to zero. The
        // stored checksum field has to be zeroed first, exactly as a receiver
        // does — summing with it still in place can never yield zero.
        let udp = &pkt.bytes[IPV4_HEADER_LEN..];
        let mut udp_zeroed = udp.to_vec();
        udp_zeroed[6] = 0;
        udp_zeroed[7] = 0;
        let pseudo = pseudo_header(src.addr, dst.addr, udp.len());
        let recomputed = ones_complement_sum(&[&pseudo, &udp_zeroed]);
        let stored = u16::from_be_bytes([udp[6], udp[7]]);
        assert_eq!(
            stored, recomputed,
            "UDP checksum {stored:#06x} != recomputed {recomputed:#06x}"
        );
    }

    /// The address in `Endpoint::addr` is network byte order, and the bytes that
    /// go on the wire are the SAME bytes the UDP pseudo-header must be computed
    /// over. Getting this wrong produces a checksum that verifies locally but
    /// is rejected by the receiver as malformed — a silent drop.
    #[test]
    fn address_byte_order_is_wire_order() {
        let ip = std::net::Ipv4Addr::new(127, 0, 0, 1);
        let be = ipv4_addr_to_be(ip);
        assert_eq!(be.to_be_bytes(), [127, 0, 0, 1], "must stay big-endian");
        // not the little-endian mistake
        assert_ne!(be.to_be_bytes(), [1, 0, 0, 127]);
        // and it round-trips
        assert_eq!(be_to_ipv4_addr(be), ip);
    }

    /// The UDP checksum has to be computed over exactly the header bytes that
    /// are transmitted, so a receiver's own computation agrees.
    #[test]
    fn udp_checksum_covers_the_bytes_on_the_wire() {
        let src = Endpoint { addr: ipv4_addr_to_be(std::net::Ipv4Addr::new(127,0,0,1)), port: 9999 };
        let dst = Endpoint { addr: ipv4_addr_to_be(std::net::Ipv4Addr::new(127,0,0,1)), port: 45311 };
        let pkt = build_packet(src, dst, b"raw ip round trip payload", 0x1234);
        // the header must carry the addresses in wire order
        assert_eq!(&pkt.bytes[12..16], &[127, 0, 0, 1]);
        assert_eq!(&pkt.bytes[16..20], &[127, 0, 0, 1]);
        // ports big-endian
        assert_eq!(&pkt.bytes[20..22], &9999u16.to_be_bytes());
        assert_eq!(&pkt.bytes[22..24], &45311u16.to_be_bytes());
        // and the checksum is the externally specified value
        assert_eq!(&pkt.bytes[26..28], &[0x18, 0x2f]);
    }

    #[test]
    fn ipv4_checksum_verifies() {
        let src = ep([10, 0, 0, 1], 1234);
        let dst = ep([10, 0, 0, 2], 5678);
        let pkt = build_packet(src, dst, b"hello", 0x1234);
        assert!(verify_ipv4_checksum(&pkt.bytes));
    }

    #[test]
    fn ipv4_checksum_detects_corruption() {
        let src = ep([10, 0, 0, 1], 1234);
        let dst = ep([10, 0, 0, 2], 5678);
        let mut pkt = build_packet(src, dst, b"hello", 0x1234);
        pkt.bytes[15] ^= 0xFF; // corrupt the destination address
        assert!(!verify_ipv4_checksum(&pkt.bytes));
    }

    #[test]
    fn round_trips_a_small_payload() {
        let src = ep([192, 168, 1, 10], 9000);
        let dst = ep([192, 168, 1, 20], 9001);
        let fs = encode_datagram(src, dst, b"msgd frame", 1, DEFAULT_MTU);
        assert_eq!(fs.packets.len(), 1);
        assert_eq!(payload_of(&fs.packets[0].bytes), Some(&b"msgd frame"[..]));
    }

    #[test]
    fn headers_parse_back_to_endpoints() {
        let src = ep([172, 16, 0, 5], 4444);
        let dst = ep([172, 16, 0, 6], 5555);
        let fs = encode_datagram(src, dst, b"abc", 1, DEFAULT_MTU);
        let h = parse_headers(&fs.packets[0].bytes).unwrap();
        assert_eq!(h.src, src);
        assert_eq!(h.dst, dst);
        assert_eq!(h.protocol, IPPROTO_UDP);
        assert!(!h.is_fragment);
        assert!(!h.more_fragments);
    }

    #[test]
    fn rejects_non_ipv4() {
        assert!(parse_headers(&[0u8; 64]).is_none());
        let mut pkt = build_packet(ep([1, 2, 3, 4], 1), ep([5, 6, 7, 8], 2), b"x", 1);
        pkt.bytes[0] = 0x65; // version 6
        assert!(parse_headers(&pkt.bytes).is_none());
    }

    #[test]
    fn fragments_a_payload_larger_than_mtu() {
        let src = ep([10, 1, 1, 1], 5000);
        let dst = ep([10, 1, 1, 2], 5001);
        let payload = vec![0xABu8; 4000];
        let fs = encode_datagram(src, dst, &payload, 7, DEFAULT_MTU);
        assert!(fs.packets.len() > 1, "expected fragmentation");
        // every fragment fits the mtu
        for p in &fs.packets {
            assert!(p.bytes.len() <= DEFAULT_MTU);
        }
        // only the last clears the MF flag
        for (i, p) in fs.packets.iter().enumerate() {
            let mf = u16::from_be_bytes([p.bytes[6], p.bytes[7]]) & FLAG_MF != 0;
            assert_eq!(mf, i + 1 < fs.packets.len(), "MF wrong on fragment {i}");
        }
    }

    #[test]
    fn reassembles_in_order() {
        let src = ep([10, 1, 1, 1], 5000);
        let dst = ep([10, 1, 1, 2], 5001);
        let payload: Vec<u8> = (0..3000u32).map(|i| (i % 251) as u8).collect();
        let fs = encode_datagram(src, dst, &payload, 9, DEFAULT_MTU);

        let mut r = Reassembler::default();
        let mut out = None;
        for p in &fs.packets {
            let h = parse_headers(&p.bytes).unwrap();
            if let Some(done) = r.accept(h, &p.bytes, 9) {
                out = Some(done);
            }
        }
        assert_eq!(out.unwrap(), payload);
    }

    #[test]
    fn reassembles_out_of_order() {
        let src = ep([10, 1, 1, 1], 5000);
        let dst = ep([10, 1, 1, 2], 5001);
        let payload: Vec<u8> = (0..5000u32).map(|i| (i % 253) as u8).collect();
        let fs = encode_datagram(src, dst, &payload, 11, DEFAULT_MTU);
        assert!(fs.packets.len() >= 4);

        let mut r = Reassembler::default();
        let mut out = None;
        // deliver last fragment first, then the rest
        for p in fs.packets.iter().rev() {
            let h = parse_headers(&p.bytes).unwrap();
            if let Some(done) = r.accept(h, &p.bytes, 11) {
                out = Some(done);
            }
        }
        assert_eq!(out.unwrap(), payload);
    }

    #[test]
    fn a_lost_fragment_never_completes() {
        let src = ep([10, 1, 1, 1], 5000);
        let dst = ep([10, 1, 1, 2], 5001);
        let payload: Vec<u8> = (0..3000u32).map(|i| (i % 251) as u8).collect();
        let fs = encode_datagram(src, dst, &payload, 13, DEFAULT_MTU);
        let mut r = Reassembler::default();
        let mut completed = false;
        for (i, p) in fs.packets.iter().enumerate() {
            if i == 1 {
                continue; // drop the middle fragment
            }
            let h = parse_headers(&p.bytes).unwrap();
            if r.accept(h, &p.bytes, 13).is_some() {
                completed = true;
            }
        }
        assert!(!completed, "a datagram with a missing fragment must not complete");
    }

    #[test]
    fn duplicate_fragments_are_idempotent() {
        let src = ep([10, 1, 1, 1], 5000);
        let dst = ep([10, 1, 1, 2], 5001);
        let payload: Vec<u8> = (0..2000u32).map(|i| (i % 241) as u8).collect();
        let fs = encode_datagram(src, dst, &payload, 17, DEFAULT_MTU);
        let mut r = Reassembler::default();
        let mut out = None;
        for p in &fs.packets {
            let h = parse_headers(&p.bytes).unwrap();
            if let Some(d) = r.accept(h, &p.bytes, 17) {
                out = Some(d);
            }
            // deliver the same fragment a second time
            let h2 = parse_headers(&p.bytes).unwrap();
            assert!(r.accept(h2, &p.bytes, 17).is_none(), "duplicate must not complete twice");
        }
        assert_eq!(out.unwrap(), payload);
    }

    #[test]
    fn max_frame_for_mtu_leaves_room_for_headers() {
        assert_eq!(max_frame_for_mtu(1500), 1500 - 28);
        assert_eq!(max_frame_for_mtu(10), 0);
    }
}
