//! Epoll-based, single-threaded event loop (one per core shard).
//!
//! Ownership model:
//!
//! ```text
//! Reactor (owns epoll fd + connection map)
//!   ├── Listener socket (SO_REUSEPORT, shared across shards)
//!   └── ConnectionMap: HashMap<ConnectionId, Connection>
//!         └── each Connection owns its RawFd + read/write buffers
//! ```
//!
//! The reactor does not know about accounts, conversations, or inboxes.
//! It dispatches decoded frames to an [`EventHandler`] trait that the
//! server binary implements, keeping the I/O layer testable in isolation
//! with a mock handler.
//!
//! **Edge-triggered epoll** is used so we must read/write until `EAGAIN`
//! on each event, which we do inside `handle_read` / `handle_write`.

#![forbid(unsafe_code)]

use crate::conn::{
    Connection, ConnectionId, TeardownReason, WriteOutcome,
    READ_BUFFER_HARD_CAP,
};
use crate::sys;
use protocol::OwnedFrame;
use std::collections::HashMap;
use std::io;
use std::net::SocketAddrV4;
use std::os::unix::io::RawFd;

// ---------------------------------------------------------------------------
// EventHandler trait — separates I/O from application logic
// ---------------------------------------------------------------------------

/// Implemented by the server binary (or test harness). All methods are called
/// from the reactor's thread/event loop — implementations must not block.
pub trait EventHandler {
    /// A new connection was accepted and registered.
    fn on_accept(&mut self, id: ConnectionId, peer: SocketAddrV4);

    /// A complete frame was decoded from the connection's read buffer.
    fn on_frame(&mut self, id: ConnectionId, frame: OwnedFrame);

    /// A connection has been fully closed and removed.
    fn on_teardown(&mut self, id: ConnectionId, reason: TeardownReason);

    /// Called once per reactor tick after all I/O events have been dispatched.
    /// Implementations should poll any pending async work (e.g. storage
    /// results) and queue outbound frames / teardowns as needed.
    fn tick(&mut self) {}

    /// Drain any outgoing frames queued by the handler.
    fn drain_outbound(&mut self) -> Vec<(ConnectionId, Box<[u8]>)> {
        Vec::new()
    }

    /// Drain any connection teardowns requested by the handler.
    fn drain_teardowns(&mut self) -> Vec<(ConnectionId, TeardownReason)> {
        Vec::new()
    }
}

// ---------------------------------------------------------------------------
// Reactor
// ---------------------------------------------------------------------------

pub struct Reactor<H: EventHandler> {
    /// index → next ConnectionId
    next_id: u64,

    epoll_fd: RawFd,
    /// Listener socket fd for accepting new connections (none for routing).
    listener_fd: Option<RawFd>,

    connections: HashMap<ConnectionId, Connection>,
    handler: H,

    /// Scratch buffer for reads (reused across events to reduce allocations).
    read_scratch: Vec<u8>,
    /// Reusable epoll event array.
    events: Vec<libc::epoll_event>,

    /// Whether the loop should exit on the next tick.
    pub shutdown: bool,

    // Stats / debug
    pub total_connections: u64,
}

impl<H: EventHandler> Reactor<H> {
    /// Create a new reactor. If `listener_fd` is `Some`, this reactor accepts
    /// connections on that socket (SO_REUSEPORT — multiple reactors on the
    /// same port is fine).
    pub fn new(handler: H, listener_fd: Option<RawFd>) -> io::Result<Self> {
        let epoll_fd = sys::epoll_create()?;

        if let Some(lfd) = listener_fd {
            // Add the listener in EPOLLIN | EPOLLET mode, with a sentinel
            // u64 = 0 (no connection ID can be 0).
            sys::epoll_add(epoll_fd, lfd, (libc::EPOLLIN | libc::EPOLLET) as u32, 0)?;
        }

        Ok(Self {
            next_id: 1,
            epoll_fd,
            listener_fd,
            connections: HashMap::new(),
            handler,
            read_scratch: vec![0u8; 65536],
            events: vec![libc::epoll_event { events: 0, u64: 0 }; 1024],
            shutdown: false,
            total_connections: 0,
        })
    }

    /// Shut down this reactor, closing all connections.
    pub fn shutdown(&mut self) {
        self.shutdown = true;
        let ids: Vec<ConnectionId> = self.connections.keys().copied().collect();
        for id in ids {
            if let Some(conn) = self.connections.get_mut(&id) {
                conn.start_teardown(TeardownReason::Shutdown);
                conn.close_immediately();
            }
        }
        self.connections.clear();
    }

    /// Return a mutable reference to the event handler (e.g. for tests that
    /// want to inspect state).
    pub fn handler(&mut self) -> &mut H {
        &mut self.handler
    }

    /// Run one iteration of the event loop. Returns `Ok(())` normally;
    /// returns `Err` on a non-recoverable epoll error.
    pub fn tick(&mut self) -> io::Result<()> {
        if self.shutdown {
            return Ok(());
        }

        // Compute max sleep: cap at idle check interval (1 second).
        let max_wait_ms: libc::c_int = 1000;
        let n = sys::epoll_wait(self.epoll_fd, &mut self.events, max_wait_ms)?;

        for i in 0..n {
            let ev = &self.events[i];
            let ptr = ev.u64;

            if ptr == 0 {
                // Listener event
                self.handle_accept()?;
            } else {
                let id = ConnectionId(ptr);
                let is_hup = (ev.events & libc::EPOLLHUP as u32) != 0
                    || (ev.events & libc::EPOLLERR as u32) != 0
                    || (ev.events & libc::EPOLLRDHUP as u32) != 0;
                let is_in = (ev.events & libc::EPOLLIN as u32) != 0;
                let is_out = (ev.events & libc::EPOLLOUT as u32) != 0;

                if is_hup {
                    self.handle_hup(id);
                } else {
                    if is_in {
                        self.handle_read(id);
                    }
                    if is_out {
                        self.handle_write(id);
                    }
                }
            }
        }

        // Let the handler poll async work (e.g. storage results) and queue
        // outbound frames.
        self.handler.tick();

        // Drain any frames/teardowns queued by tick() (or earlier by
        // on_frame() handlers that were not drained inside handle_read).
        self.drain_handler_queues();

        // Idle timeout sweep (only on ticks that didn't process events, to
        // keep the hot path fast; but we do it periodically by the 1s wait).
        self.sweep_timeouts();

        Ok(())
    }

    /// Blocking run — loops `tick()` until `shutdown` is set.
    pub fn run(&mut self) -> io::Result<()> {
        while !self.shutdown {
            self.tick()?;
        }
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Event handlers
    // -----------------------------------------------------------------------

    fn handle_accept(&mut self) -> io::Result<()> {
        let Some(lfd) = self.listener_fd else { return Ok(()) };
        // Edge-triggered: accept() until EAGAIN.
        loop {
            match sys::accept(lfd) {
                Ok((fd, addr)) => {
                    let id = ConnectionId(self.next_id);
                    self.next_id += 1;
                    let peer = SocketAddrV4::new(
                        std::net::Ipv4Addr::from(u32::from_be(addr.sin_addr.s_addr)),
                        u16::from_be(addr.sin_port),
                    );
                    let conn = Connection::new(fd, peer, id);
                    // Register in epoll (ET | IN for reads).
                    if let Err(e) = sys::epoll_add(
                        self.epoll_fd,
                        fd,
                        (libc::EPOLLIN | libc::EPOLLET | libc::EPOLLRDHUP) as u32,
                        id.into(),
                    ) {
                        sys::close(fd);
                        return Err(e);
                    }
                    self.handler.on_accept(id, peer);
                    self.connections.insert(id, conn);
                    self.total_connections += 1;
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(()),
                Err(e) => return Err(e),
            }
        }
    }

    fn handle_hup(&mut self, id: ConnectionId) {
        let Some(conn) = self.connections.get_mut(&id) else { return };
        if conn.teardown.is_none() {
            conn.start_teardown(TeardownReason::PeerClosed);
        }
    }

    fn handle_read(&mut self, id: ConnectionId) {
        let frames: Vec<OwnedFrame> = self.try_read_frames(id);
        for frame in frames {
            self.handler.on_frame(id, frame);
        }

        // Drain outbound frames, tracking connections for backpressure.
        let mut overflowed: Vec<ConnectionId> = Vec::new();
        let mut written_targets: Vec<ConnectionId> = Vec::new();
        for (target_id, frame) in self.handler.drain_outbound() {
            match self.send_to(target_id, &frame) {
                WriteOutcome::Overflow => overflowed.push(target_id),
                WriteOutcome::Queued => written_targets.push(target_id),
                _ => {}
            }
        }

        // Tear down connections whose write buffer exceeded HARD_CAP.
        for &target_id in &overflowed {
            if let Some(conn) = self.connections.get_mut(&target_id) {
                conn.start_teardown(TeardownReason::SlowClient);
            }
        }

        // Apply backpressure to target connections with high write buffer.
        for &target_id in &written_targets {
            if let Some(conn) = self.connections.get_mut(&target_id) {
                if conn.should_pause_reading() && conn.teardown.is_none() {
                    conn.read_paused = true;
                    let events =
                        (libc::EPOLLOUT | libc::EPOLLET | libc::EPOLLRDHUP) as u32;
                    let _ = sys::epoll_mod(self.epoll_fd, conn.fd, events, target_id.into());
                }
            }
        }

        // Also apply backpressure to the source connection (original behavior).
        if let Some(conn) = self.connections.get_mut(&id) {
            if conn.should_pause_reading() && conn.teardown.is_none() {
                conn.read_paused = true;
                let events = (libc::EPOLLOUT | libc::EPOLLET | libc::EPOLLRDHUP) as u32;
                let _ = sys::epoll_mod(self.epoll_fd, conn.fd, events, id.into());
            }
        }

        // Drain teardown requests from the handler.
        for (teardown_id, reason) in self.handler.drain_teardowns() {
            if let Some(conn) = self.connections.get_mut(&teardown_id) {
                conn.start_teardown(reason);
            }
        }
    }

    /// Read and decode any available frames. Returns empty vec on error/teardown.
    fn try_read_frames(&mut self, id: ConnectionId) -> Vec<OwnedFrame> {
        let conn = match self.connections.get_mut(&id) {
            Some(c) => c,
            None => return Vec::new(),
        };
        if conn.read_paused {
            return Vec::new();
        }
        loop {
            let scratch = &mut self.read_scratch[..];
            match conn.do_read(scratch) {
                Ok(0) => break,
                Ok(_) => {
                    if conn.read_buffered_bytes() > READ_BUFFER_HARD_CAP {
                        conn.start_teardown(TeardownReason::ProtocolViolation);
                        return Vec::new();
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(_) => {
                    conn.start_teardown(TeardownReason::PeerClosed);
                    return Vec::new();
                }
            }
        }
        let mut frames = Vec::with_capacity(4);
        loop {
            match conn.try_decode_frame() {
                Ok(Some(f)) => frames.push(f),
                Ok(None) => break,
                Err(_) => {
                    conn.start_teardown(TeardownReason::ProtocolViolation);
                    return Vec::new();
                }
            }
        }
        frames
    }

    fn handle_write(&mut self, id: ConnectionId) {
        let teardown_done: Option<TeardownReason> = {
            let conn = match self.connections.get_mut(&id) {
                Some(c) => c,
                None => return,
            };

            if conn.teardown.is_some() {
                let done = conn.advance_teardown();
                if done {
                    let reason = conn.teardown.map(|t| t.reason).unwrap_or(TeardownReason::Shutdown);
                    let _ = sys::epoll_del(self.epoll_fd, conn.fd);
                    self.connections.remove(&id);
                    Some(reason)
                } else {
                    None
                }
            } else {
                // Normal write flush.
                loop {
                    match conn.do_write() {
                        Ok(0) => break,
                        Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                        Err(_) => {
                            conn.start_teardown(TeardownReason::PeerClosed);
                            break;
                        }
                        Ok(_) => {
                            if conn.is_write_empty() {
                                conn.write_pending = false;
                                if conn.read_paused {
                                    conn.read_paused = false;
                                    let events = (libc::EPOLLIN | libc::EPOLLOUT | libc::EPOLLET | libc::EPOLLRDHUP) as u32;
                                    let _ = sys::epoll_mod(self.epoll_fd, conn.fd, events, id.into());
                                } else {
                                    let events = (libc::EPOLLIN | libc::EPOLLET | libc::EPOLLRDHUP) as u32;
                                    let _ = sys::epoll_mod(self.epoll_fd, conn.fd, events, id.into());
                                }
                                break;
                            }
                        }
                    }
                }
                None
            }
        };
        if let Some(reason) = teardown_done {
            self.handler.on_teardown(id, reason);
        }
    }

    /// Check idle deadlines; tear down stale connections.
    fn sweep_timeouts(&mut self) {
        let now = sys::now_ms();

        // 1. Mark idle connections for teardown.
        let mut expired: Vec<ConnectionId> = Vec::new();
        for (&id, conn) in &self.connections {
            if conn.teardown.is_none() && conn.idle_deadline <= now {
                expired.push(id);
            }
        }
        for &id in &expired {
            if let Some(conn) = self.connections.get_mut(&id) {
                conn.start_teardown(TeardownReason::IdleTimeout);
                let events =
                    (libc::EPOLLIN | libc::EPOLLOUT | libc::EPOLLET | libc::EPOLLRDHUP) as u32;
                let _ = sys::epoll_mod(self.epoll_fd, conn.fd, events, id.into());
            }
        }

        // 2. Close connections whose teardown is complete.
        let mut to_remove: Vec<(ConnectionId, TeardownReason)> = Vec::new();
        for (&id, conn) in &self.connections {
            if let Some(state) = &conn.teardown {
                if conn.is_write_empty() || now >= state.flush_deadline {
                    to_remove.push((id, state.reason));
                }
            }
        }
        for (id, reason) in to_remove {
            if let Some(conn) = self.connections.remove(&id) {
                let _ = sys::epoll_del(self.epoll_fd, conn.fd);
                sys::close(conn.fd);
                self.handler.on_teardown(id, reason);
            }
        }
    }

    /// Enqueue a frame to a specific connection. Returns `WriteOutcome`.
    pub fn send_to(&mut self, id: ConnectionId, frame: &[u8]) -> WriteOutcome {
        let Some(conn) = self.connections.get_mut(&id) else {
            return WriteOutcome::Rejected;
        };
        let result = conn.enqueue_frame(frame);
        if result == WriteOutcome::Queued && !conn.write_pending {
            conn.write_pending = true;
            // Register EPOLLOUT interest.
            let events =
                (libc::EPOLLIN | libc::EPOLLOUT | libc::EPOLLET | libc::EPOLLRDHUP) as u32;
            let _ = sys::epoll_mod(self.epoll_fd, conn.fd, events, id.into());
        }
        result
    }

    /// Returns the number of connected clients.
    pub fn connection_count(&self) -> usize {
        self.connections.len()
    }

    /// Drain outbound frames and teardowns from the handler and apply them.
    fn drain_handler_queues(&mut self) {
        let mut overflowed: Vec<ConnectionId> = Vec::new();
        let mut written_targets: Vec<ConnectionId> = Vec::new();
        for (target_id, frame) in self.handler.drain_outbound() {
            match self.send_to(target_id, &frame) {
                WriteOutcome::Overflow => overflowed.push(target_id),
                WriteOutcome::Queued => written_targets.push(target_id),
                _ => {}
            }
        }

        for &target_id in &overflowed {
            if let Some(conn) = self.connections.get_mut(&target_id) {
                conn.start_teardown(TeardownReason::SlowClient);
            }
        }

        for &target_id in &written_targets {
            if let Some(conn) = self.connections.get_mut(&target_id) {
                if conn.should_pause_reading() && conn.teardown.is_none() {
                    conn.read_paused = true;
                    let events =
                        (libc::EPOLLOUT | libc::EPOLLET | libc::EPOLLRDHUP) as u32;
                    let _ = sys::epoll_mod(self.epoll_fd, conn.fd, events, target_id.into());
                }
            }
        }

        for (teardown_id, reason) in self.handler.drain_teardowns() {
            if let Some(conn) = self.connections.get_mut(&teardown_id) {
                conn.start_teardown(reason);
            }
        }
    }
}

impl<H: EventHandler> Drop for Reactor<H> {
    fn drop(&mut self) {
        self.shutdown();
        sys::close(self.epoll_fd);
        if let Some(lfd) = self.listener_fd {
            sys::close(lfd);
        }
    }
}
