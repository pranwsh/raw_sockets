//! Datagram (raw IP) event loop.
//!
//! Structurally the same shape as [`crate::reactor`] — a single-threaded,
//! edge-triggered epoll loop that pulls bytes in, hands complete frames to an
//! [`EventHandler`], and drains the handler's outbound queue back out — but the
//! socket model is inverted:
//!
//! | | stream reactor | this reactor |
//! |---|---|---|
//! | sockets | one listener + one fd per connection | **one** raw socket |
//! | peers | established by `accept()` | keyed by the source address of each datagram |
//! | write readiness | `EPOLLOUT` per connection | none; `sendto` is retried each tick |
//! | buffering | per-connection read/write buffers | per-peer send queue ([`Peer`]) |
//! | reliability | TCP's | [`crate::reliable`], at the application layer |
//!
//! Because there is no `accept()`, a peer springs into existence the first time
//! it sends us something. That is inherent to a datagram transport, and it does
//! mean an idle peer is indistinguishable from an absent one — hence
//! [`DgramReactor::disconnect`], so the application can retire one explicitly.
//!
//! # Timing
//!
//! The loop returns immediately whenever a datagram arrives and otherwise waits
//! at most [`TICK_TIMEOUT_MS`]. Retransmission is driven off that same tick, so
//! this value bounds how late a retransmit can be.

use std::collections::HashMap;
use std::io;
use std::sync::Arc;

use crate::packet::{self, Endpoint, Reassembler, DEFAULT_MTU};
use crate::peer::{Peer, PeerTeardownReason, QueueOutcome};
use crate::conn::ConnectionId;
use crate::reactor::EventHandler;
use crate::sys;

/// how long `epoll_wait` may sleep when there is nothing to do
///
/// Also the granularity of the retransmit sweep, so it bounds how late a
/// retransmission can be.
pub const TICK_TIMEOUT_MS: libc::c_int = 10;

/// buffer for one received datagram, including IP and UDP headers
///
/// 65,536 is the largest an IPv4 datagram can be, so nothing is ever truncated.
const RECV_BUFFER_LEN: usize = 65_536;

/// epoll data pointer for the raw socket itself
const SOCKET_SENTINEL: u64 = 0;

/// epoll data pointer for the wake eventfd
const WAKE_SENTINEL: u64 = u64::MAX;

/// the datagram reactor
pub struct DgramReactor<H: EventHandler> {
    handler: H,
    socket: i32,
    epoll_fd: i32,
    wake_fd: Option<i32>,

    /// peers by address — the datagram equivalent of a connection table
    /// label for debug output, so a two-reactor test can tell them apart
    pub tag: &'static str,

    /// the address we were asked to bind, used as the source of every outgoing
    /// datagram
    ///
    /// Recorded at construction because `getsockname` cannot supply it: on a raw
    /// socket it reports `0.0.0.0` and the protocol number in place of the port.
    /// Building a packet with `src = 0.0.0.0` yields one the receiver discards
    /// without an error, so the bind address has to be remembered, not queried.
    bound_addr: Endpoint,

    peers: HashMap<Endpoint, Peer>,
    /// handler-facing handle -> address
    ids: HashMap<u64, Endpoint>,
    /// address -> handler-facing handle
    addrs: HashMap<Endpoint, u64>,
    next_id: u64,

    /// partial datagrams awaiting their remaining fragments
    ///
    /// Held across ticks on purpose: fragments of one datagram routinely arrive
    /// in separate `recvfrom` calls, so this cannot be per-tick scratch.
    reassembler: Reassembler,

    events: Vec<libc::epoll_event>,
    recv_buf: Vec<u8>,

    read_frames: Vec<(ConnectionId, protocol::OwnedFrame)>,
    outbound_buf: Vec<(ConnectionId, Arc<[u8]>)>,
    teardown_buf: Vec<(ConnectionId, crate::conn::TeardownReason)>,
    overflow: Vec<u64>,

    shutdown: bool,
}

impl<H: EventHandler> DgramReactor<H> {
    /// label this reactor in debug output, distinguishing two in one process
    pub fn with_tag(mut self, tag: &'static str) -> Self {
        self.tag = tag;
        self
    }

    /// create a reactor bound to `local`, with an optional eventfd for waking
    ///
    /// Requires `CAP_NET_RAW`; without it this fails with `EPERM`.
    pub fn new(handler: H, local: Endpoint, wake_fd: Option<i32>) -> io::Result<Self> {
        let socket = sys::socket_raw_ipv4()?;
        if let Err(e) = sys::set_ip_hdrincl(socket) {
            sys::close(socket);
            return Err(e);
        }
        // We compute the IP and UDP checksums ourselves, so never let the
        // kernel try to fill them in.
        let _ = set_checksum_offload(socket, false);

        let addr = sys::sockaddr_v4(local.addr, local.port);
        if let Err(e) = sys::bind_raw_v4(socket, &addr) {
            sys::close(socket);
            return Err(e);
        }

        let epoll_fd = sys::epoll_create()?;
        if let Err(e) = sys::epoll_add(
            epoll_fd,
            socket,
            (libc::EPOLLIN | libc::EPOLLET) as u32,
            SOCKET_SENTINEL,
        ) {
            sys::close(epoll_fd);
            sys::close(socket);
            return Err(e);
        }
        if let Some(wfd) = wake_fd
            && let Err(e) = sys::epoll_add(
                epoll_fd,
                wfd,
                (libc::EPOLLIN | libc::EPOLLET) as u32,
                WAKE_SENTINEL,
            )
        {
            sys::close(epoll_fd);
            sys::close(socket);
            return Err(e);
        }

        Ok(Self {
            handler,
            socket,
            tag: "?",
            bound_addr: local,
            epoll_fd,
            wake_fd,
            peers: HashMap::new(),
            ids: HashMap::new(),
            addrs: HashMap::new(),
            next_id: 1,
            reassembler: Reassembler::default(),
            events: vec![
                libc::epoll_event { events: 0, u64: 0 };
                sys::EPOLL_MAX_EVENTS
            ],
            recv_buf: vec![0u8; RECV_BUFFER_LEN],
            read_frames: Vec::new(),
            outbound_buf: Vec::new(),
            teardown_buf: Vec::new(),
            overflow: Vec::new(),
            shutdown: false,
        })
    }

    /// the address this reactor sends from
    ///
    /// This is the address and port it was constructed with, which is also what
    /// it advertises as the source of outgoing datagrams.
    pub fn local_addr(&self) -> Endpoint {
        self.bound_addr
    }

    /// how many peers are currently tracked
    pub fn peer_count(&self) -> usize {
        self.peers.len()
    }

    /// block, looping until shutdown
    pub fn run(&mut self) -> io::Result<()> {
        while !self.shutdown {
            self.tick()?;
        }
        Ok(())
    }

    /// stop the loop
    pub fn shutdown(&mut self) {
        self.shutdown = true;
    }

    /// run a single iteration of the loop
    ///
    /// Public so a test or an embedding application can drive the reactor one
    /// step at a time instead of only through [`Self::run`].
    pub fn tick_once(&mut self) -> io::Result<()> {
        self.tick()
    }

    /// access the handler, e.g. to inspect state or queue outbound frames
    pub fn handler(&self) -> &H {
        &self.handler
    }

    pub fn handler_mut(&mut self) -> &mut H {
        &mut self.handler
    }

    /// send a frame to `dst` through this reactor's socket
    ///
    /// The frame is wrapped in a reliability envelope for the same peer the
    /// inbound path expects — sending the bare frame would arrive as an
    /// undecodable datagram, because the receiver always runs it through
    /// [`crate::reliable`] first.
    pub fn send_frame(&mut self, dst: Endpoint, frame: &[u8]) -> io::Result<()> {
        let now = sys::now_ms();
        let (_, envelope) = {
            let peer = self.peers.entry(dst).or_insert_with(|| Peer::new(dst));
            peer.reliable.wrap(frame, now)
        };
        self.send_datagram(dst, &envelope)
    }

    /// send an already-encoded payload to `dst`, with no reliability wrapping
    fn send_datagram(&mut self, dst: Endpoint, payload: &[u8]) -> io::Result<()> {
        // `bind()` on a raw socket does not actually bind a port — `getsockname`
        // reports 255 — so the source port has to be carried explicitly. The
        // source ADDRESS is still whatever we bound to.
        let local = self.bound_addr;
        let encoded = packet::encode_datagram(local, dst, payload, next_identification(), DEFAULT_MTU);
        let target = sys::sockaddr_v4(dst.addr, dst.port);
        for p in &encoded.packets {
            sys::send_packet(self.socket, &p.bytes, &target)?;
        }
        Ok(())
    }

    /// queue a frame for `id` through the normal outbound path
    pub fn send_to_peer(&mut self, id: ConnectionId, frame: &[u8]) -> io::Result<()> {
        let addr = self
            .ids
            .get(&id.0)
            .copied()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotConnected, "unknown peer"))?;
        self.send_frame(addr, frame)
    }

    /// explicitly retire a peer, e.g. after a `Goodbye` frame
    pub fn disconnect(&mut self, id: ConnectionId) {
        if let Some(addr) = self.ids.get(&id.0).copied()
            && let Some(peer) = self.peers.get_mut(&addr)
        {
            peer.start_teardown(PeerTeardownReason::Goodbye);
        }
    }

    /// one iteration: wait, read every pending datagram, dispatch, flush
    fn tick(&mut self) -> io::Result<()> {
        if self.shutdown {
            return Ok(());
        }
        let n = sys::epoll_wait(self.epoll_fd, &mut self.events, TICK_TIMEOUT_MS)?;

        // Copy out the sentinel flags first: the loop below mutably borrows
        // `self`, so it cannot hold a borrow of `self.events` at the same time.
        let woke = self.events[..n].iter().any(|ev| ev.u64 == WAKE_SENTINEL);
        let has_socket = self.events[..n].iter().any(|ev| ev.u64 == SOCKET_SENTINEL);
        if has_socket {
            // edge-triggered: drain until EAGAIN, because one wake can carry a
            // burst and stopping after one datagram would stall the socket
            self.read_datagrams();
        }
        if woke
            && let Some(wfd) = self.wake_fd
        {
            sys::drain_eventfd(wfd);
        }

        self.handler.tick();
        self.dispatch_frames();
        self.drain_handler_queues();
        self.flush_outbound();
        self.sweep_retransmits();
        self.sweep_timeouts();
        Ok(())
    }

    /// read and process every datagram the socket has ready
    fn read_datagrams(&mut self) {
        loop {
            let mut from = sys::sockaddr_v4(0, 0);
            let n = match sys::recv_packet(self.socket, &mut self.recv_buf, &mut from) {
                Ok(n) => n,
                // EAGAIN: the socket is drained
                Err(_) => return,
            };
            // The buffer is the maximum an IPv4 datagram can be, so `n` cannot
            // exceed it; a short read means a truncated frame and is dropped
            // rather than fed to the decoder.
            if n < packet::IPV4_HEADER_LEN + packet::UDP_HEADER_LEN || n > self.recv_buf.len() {
                continue;
            }
            // Take the buffer so it is moved out of `self` before the call:
            // `process_datagram` takes `&mut self`, which cannot coexist with
            // an immutable borrow of `self.recv_buf`.
            let datagram = std::mem::take(&mut self.recv_buf);
            self.process_datagram(&datagram[..n]);
            self.recv_buf = datagram;
        }
    }

    /// handle one received datagram: reassemble it, then decode frames
    fn process_datagram(&mut self, datagram: &[u8]) {
        let Some(headers) = packet::parse_headers(datagram) else {
            return; // not IPv4/UDP, or internally inconsistent
        };
        if !packet::verify_ipv4_checksum(datagram) {
            return; // corrupt header
        }
        let identification = u16::from_be_bytes([datagram[4], datagram[5]]);

        // A raw socket does not get demultiplexed by port the way a normal UDP
        // socket is: EVERY datagram on the interface arrives here, including
        // traffic between other processes. Filtering by destination port is the
        // application's job, and without it two reactors on one host each see the
        // other's traffic and answer as if it were their own.
        if headers.dst.port != self.bound_addr.port {
            return;
        }

        // A raw socket also receives its OWN outgoing datagrams back: on loopback
        // the packet we just sent is delivered into our receive queue with the
        // real source port intact. Treating that as inbound would make every peer
        // look like it was talking to itself, and the reliability layer would then
        // dedup our own traffic as a retransmit.
        //
        // src.port == our own source port means we sent it; a genuine peer can
        // never share our port, because the port filter above already proved the
        // datagram was addressed to us.
        if headers.src.port == self.bound_addr.port {
            return;
        }

        // A fragmented datagram only becomes usable once every fragment has
        // arrived; the reassembler buffers the rest.
        let payload: Vec<u8> = if headers.is_fragment || headers.more_fragments {
            match self.reassembler.accept(headers, datagram, identification) {
                Some(p) => p,
                None => return,
            }
        } else {
            match packet::payload_of(datagram) {
                Some(p) => p.to_vec(),
                None => return,
            }
        };


        if !self.peers.contains_key(&headers.src) {
            self.peers.insert(headers.src, Peer::new(headers.src));
            let id = self.next_id;
            self.next_id += 1;
            self.ids.insert(id, headers.src);
            self.addrs.insert(headers.src, id);
            self.handler
                .on_accept(ConnectionId(id), addr_to_std(headers.src));
        }
        // `on_accept` runs application code, which is allowed to tear the peer
        // down again (a rejecting auth handler, say). Re-check rather than
        // indexing, so that is not a panic.
        let Some(&id) = self.addrs.get(&headers.src) else {
            return;
        };
        let now = sys::now_ms();

        // The reliability layer reorders, deduplicates and acks; payloads it
        // releases are appended to this peer's reassembly buffer.
        let Some(peer) = self.peers.get_mut(&headers.src) else {
            return;
        };
        // payloads land in the peer's reassembly buffer for frame decoding
        peer.on_datagram(&payload, now);

        loop {
            match peer.try_decode_frame() {
                Ok(Some(frame)) => self.read_frames.push((ConnectionId(id), frame)),
                Ok(None) => break,
                Err(_) => {
                    peer.start_teardown(PeerTeardownReason::ProtocolViolation);
                    self.read_frames.clear();
                    break;
                }
            }
        }
    }

    /// deliver decoded frames to the handler
    fn dispatch_frames(&mut self) {
        if self.read_frames.is_empty() {
            return;
        }
        let frames = std::mem::take(&mut self.read_frames);
        for (id, frame) in frames {
            self.handler.on_frame(id, frame);
        }
    }

    /// pull outbound frames and teardown requests out of the handler
    fn drain_handler_queues(&mut self) {
        self.handler.drain_outbound(&mut self.outbound_buf);
        let mut outbound = std::mem::take(&mut self.outbound_buf);
        self.overflow.clear();
        let now = sys::now_ms();
        for (target, frame) in outbound.drain(..) {
            let Some(addr) = self.ids.get(&target.0).copied() else {
                continue; // the peer is already gone
            };
            let Some(peer) = self.peers.get_mut(&addr) else {
                continue;
            };
            if peer.enqueue_frame(&frame, now) == QueueOutcome::Rejected {
                self.overflow.push(target.0);
            }
        }
        self.outbound_buf = outbound;

        self.handler.drain_teardowns(&mut self.teardown_buf);
        let teardowns = std::mem::take(&mut self.teardown_buf);
        for (id, reason) in &teardowns {
            if let Some(addr) = self.ids.get(&id.0).copied()
                && let Some(peer) = self.peers.get_mut(&addr)
            {
                peer.start_teardown(map_teardown(*reason));
            }
        }
        self.teardown_buf = teardowns;

        for id in std::mem::take(&mut self.overflow) {
            if let Some(addr) = self.ids.get(&id).copied()
                && let Some(peer) = self.peers.get_mut(&addr)
            {
                peer.start_teardown(PeerTeardownReason::SlowConsumer);
            }
        }
    }

    /// transmit queued datagrams for every peer that has any
    ///
    /// There is no `EPOLLOUT` to wait on: `sendto` either takes the whole packet
    /// or fails, and a full send buffer is retried on the next tick. So this is
    /// a plain drain, bounded per peer so one unreachable address cannot starve
    /// the others.
    fn flush_outbound(&mut self) {
        let addrs: Vec<Endpoint> = self.peers.keys().copied().collect();
        for addr in addrs {
            let target = sys::sockaddr_v4(addr.addr, addr.port);
            loop {
                let Some(peer) = self.peers.get_mut(&addr) else {
                    break;
                };
                let Some(datagram) = peer.pop_datagram() else {
                    break;
                };
                // The source address must be the one we are actually bound to:
                // the receiver recomputes the IP and UDP checksums over it, so a
                // wrong value means every outgoing datagram is silently dropped.
                let local = self.bound_addr;
                let encoded = packet::encode_datagram(
                    local,
                    addr,
                    &datagram,
                    next_identification(),
                    DEFAULT_MTU,
                );
                let mut retry = false;
                let mut fatal = false;
                for p in &encoded.packets {
                    if let Err(e) = sys::send_packet(self.socket, &p.bytes, &target) {
                        if e.kind() == io::ErrorKind::WouldBlock {
                            retry = true; // transient: keep it for the next tick
                        } else {
                            fatal = true;
                        }
                        break;
                    }
                }
                if retry || fatal {
                    if let Some(peer) = self.peers.get_mut(&addr) {
                        if retry {
                            peer.requeue_front(datagram);
                        } else {
                            peer.start_teardown(PeerTeardownReason::Unresponsive);
                        }
                    }
                    break;
                }
            }
        }
    }

    /// queue retransmissions whose timers have expired
    fn sweep_retransmits(&mut self) {
        let now = sys::now_ms();
        for peer in self.peers.values_mut() {
            if peer.teardown.is_none() {
                peer.pump_retransmits(now);
            }
        }
    }

    /// drop peers whose teardown has finished, and report it to the handler
    fn sweep_timeouts(&mut self) {
        let now = sys::now_ms();
        let done: Vec<(Endpoint, PeerTeardownReason)> = self
            .peers
            .iter()
            .filter(|(_, p)| p.teardown_complete(now))
            .map(|(a, p)| {
                (
                    *a,
                    p.teardown
                        .map(|t| t.reason)
                        .unwrap_or(PeerTeardownReason::Shutdown),
                )
            })
            .collect();

        for (addr, reason) in done {
            self.peers.remove(&addr);
            if let Some(id) = self.addrs.remove(&addr) {
                self.ids.remove(&id);
                self.handler
                    .on_teardown(ConnectionId(id), map_peer_teardown(reason));
            }
        }
    }
}

impl<H: EventHandler> Drop for DgramReactor<H> {
    fn drop(&mut self) {
        sys::close(self.epoll_fd);
        sys::close(self.socket);
        if let Some(wfd) = self.wake_fd {
            sys::close(wfd);
        }
    }
}

/// monotonic identification for outgoing packets
fn next_identification() -> u16 {
    use std::sync::atomic::{AtomicU16, Ordering};
    static COUNTER: AtomicU16 = AtomicU16::new(1);
    COUNTER.fetch_add(1, Ordering::Relaxed)
}

/// adapt an [`Endpoint`] to the `SocketAddrV4` the handler trait expects
fn addr_to_std(addr: Endpoint) -> std::net::SocketAddrV4 {
    std::net::SocketAddrV4::new(std::net::Ipv4Addr::from(addr.addr), addr.port)
}

/// map a handler teardown request onto a peer teardown reason
fn map_teardown(reason: crate::conn::TeardownReason) -> PeerTeardownReason {
    match reason {
        crate::conn::TeardownReason::SlowClient => PeerTeardownReason::SlowConsumer,
        crate::conn::TeardownReason::ProtocolViolation => {
            PeerTeardownReason::ProtocolViolation
        }
        crate::conn::TeardownReason::ClientGoodbye => PeerTeardownReason::Goodbye,
        // PeerClosed and Shutdown both mean "drop it"; there is no stream to
        // drain on a datagram transport.
        crate::conn::TeardownReason::PeerClosed | crate::conn::TeardownReason::Shutdown => {
            PeerTeardownReason::Shutdown
        }
    }
}

/// map a peer teardown reason back onto the handler's enum
fn map_peer_teardown(reason: PeerTeardownReason) -> crate::conn::TeardownReason {
    match reason {
        PeerTeardownReason::SlowConsumer => crate::conn::TeardownReason::SlowClient,
        PeerTeardownReason::ProtocolViolation | PeerTeardownReason::OversizedFrame => {
            crate::conn::TeardownReason::ProtocolViolation
        }
        PeerTeardownReason::Goodbye => crate::conn::TeardownReason::ClientGoodbye,
        PeerTeardownReason::Unresponsive => crate::conn::TeardownReason::PeerClosed,
        PeerTeardownReason::Shutdown => crate::conn::TeardownReason::Shutdown,
    }
}

/// enable or disable kernel checksum offload
fn set_checksum_offload(fd: i32, on: bool) -> io::Result<()> {
    let optval: libc::c_int = if on { 1 } else { 0 };
    let rc = unsafe {
        libc::setsockopt(
            fd,
            libc::IPPROTO_IP,
            libc::IP_CHECKSUM,
            &optval as *const _ as *const libc::c_void,
            std::mem::size_of_val(&optval) as libc::socklen_t,
        )
    };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

