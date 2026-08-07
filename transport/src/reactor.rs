//! epoll-based, single-threaded event loop (one per core shard)

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

// EventHandler trait — separates i/o from application logic

/// implemented by the server binary (or test harness)
pub trait EventHandler {
    /// a new connection was accepted and registered
    fn on_accept(&mut self, id: ConnectionId, peer: SocketAddrV4);

    /// a complete frame was decoded from the connection's read buffer
    fn on_frame(&mut self, id: ConnectionId, frame: OwnedFrame);

    /// a connection has been fully closed and removed
    fn on_teardown(&mut self, id: ConnectionId, reason: TeardownReason);

    /// called once per reactor tick after all i/o events have been dispatched
    fn tick(&mut self) {}

    /// drain any outgoing frames queued by the handler into the provided buffer
    fn drain_outbound(&mut self, _out: &mut Vec<(ConnectionId, Box<[u8]>)>) {}

    /// drain any connection teardowns requested by the handler into the provided buffer
    fn drain_teardowns(&mut self, _out: &mut Vec<(ConnectionId, TeardownReason)>) {}
}

// reactor

pub struct Reactor<H: EventHandler> {
    /// index → next ConnectionId
    next_id: u64,

    epoll_fd: RawFd,
    /// listener socket fd for accepting new connections (none for routing)
    listener_fd: Option<RawFd>,
    /// eventfd used by the store worker to wake the reactor when an async
    /// result is ready (none when the reactor has no async source)
    wake_fd: Option<RawFd>,

    connections: HashMap<ConnectionId, Connection>,
    handler: H,

    /// scratch buffer for reads (reused across events to reduce allocations)
    read_scratch: Vec<u8>,
    /// reusable epoll event array
    events: Vec<libc::epoll_event>,
    /// reusable buffer for outbound frames drained from the handler
    outbound_buf: Vec<(ConnectionId, Box<[u8]>)>,
    /// reusable buffer for teardown requests drained from the handler
    teardown_buf: Vec<(ConnectionId, TeardownReason)>,
    /// reusable buffer for decoded frames from a single read iteration
    read_frames: Vec<OwnedFrame>,
    /// reusable buffer for overflowed connections during backpressure
    overflow_buf: Vec<ConnectionId>,
    /// reusable buffer for connections that received writes during backpressure
    written_buf: Vec<ConnectionId>,

    /// whether the loop should exit on the next tick
    pub shutdown: bool,

    // stats / debug
    pub total_connections: u64,
}

/// epoll u64 sentinel for the wake eventfd (no connection ID can equal this)
const WAKE_SENTINEL: u64 = u64::MAX;

impl<H: EventHandler> Reactor<H> {
    /// create a new reactor; `wake_fd` is an eventfd whose write end the store
    /// worker pokes whenever an async result is ready, letting epoll_wait
    /// return immediately instead of waiting out its timeout
    pub fn new(handler: H, listener_fd: Option<RawFd>, wake_fd: Option<RawFd>) -> io::Result<Self> {
        let epoll_fd = sys::epoll_create()?;

        if let Some(lfd) = listener_fd {
            // add the listener in EPOLLIN | EPOLLET mode, with a sentinel u64 = 0 (no connection ID can be 0)
            sys::epoll_add(epoll_fd, lfd, (libc::EPOLLIN | libc::EPOLLET) as u32, 0)?;
        }

        if let Some(wfd) = wake_fd {
            // edge-triggered so a drained counter stops reporting until the next poke
            sys::epoll_add(epoll_fd, wfd, (libc::EPOLLIN | libc::EPOLLET) as u32, WAKE_SENTINEL)?;
        }

        Ok(Self {
            next_id: 1,
            epoll_fd,
            listener_fd,
            wake_fd,
            connections: HashMap::new(),
            handler,
            read_scratch: vec![0u8; 65536],
            events: vec![libc::epoll_event { events: 0, u64: 0 }; 1024],
            outbound_buf: Vec::new(),
            teardown_buf: Vec::new(),
            read_frames: Vec::with_capacity(4),
            overflow_buf: Vec::new(),
            written_buf: Vec::new(),
            shutdown: false,
            total_connections: 0,
        })
    }

    /// shut down this reactor, closing all connections
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

    /// return a mutable reference to the event handler (e.g for tests that want to inspect state)
    pub fn handler(&mut self) -> &mut H {
        &mut self.handler
    }

    /// run one iteration of the event loop
    pub fn tick(&mut self) -> io::Result<()> {
        if self.shutdown {
            return Ok(());
        }

        // Wait at most ~10ms so the Domain's async store-result drain runs
        // promptly even when the connection is otherwise idle. A Send needs two
        // sequential store round-trips; with a 1s timeout each hop could wait
        // up to 1s for the next epoll wake-up (~2s end-to-end). At 10ms the
        // worst case drops to ~20ms while busy loops still return immediately.
        let max_wait_ms: libc::c_int = 10;
        let n = sys::epoll_wait(self.epoll_fd, &mut self.events, max_wait_ms)?;

        for i in 0..n {
            let ev = &self.events[i];
            let ptr = ev.u64;

            if ptr == 0 {
                // listener event
                self.handle_accept()?;
            } else if ptr == WAKE_SENTINEL {
                // store worker poked us: an async result is ready. Drain the
                // counter so the edge-triggered eventfd stops reporting; the
                // handler.tick() below picks up the result immediately.
                if let Some(wfd) = self.wake_fd {
                    sys::drain_eventfd(wfd);
                }
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

        // let the handler poll async work (e.g storage results) and queue outbound frames
        self.handler.tick();

        // drain any frames/teardowns queued by tick() (or earlier by on_frame() handlers that were not drained inside handle_read)
        self.drain_handler_queues();

        // idle timeout sweep (runs every tick; cheap and keeps teardowns timely).
        // NOTE: idle marking is currently disabled — see sweep_timeouts().
        self.sweep_timeouts();

        Ok(())
    }

    /// blocking run — loops tick() until shutdown is set
    pub fn run(&mut self) -> io::Result<()> {
        while !self.shutdown {
            self.tick()?;
        }
        Ok(())
    }

    // event handlers

    fn handle_accept(&mut self) -> io::Result<()> {
        let Some(lfd) = self.listener_fd else { return Ok(()) };
        // edge-triggered: accept() until EAGAIN
        loop {
            match sys::accept(lfd) {
                Ok((fd, addr)) => {
                    // disable Nagle: message frames are small and lateness-sensitive.
                    // Best-effort: a failure here doesn't make the socket unusable.
                    if let Err(e) = sys::set_tcp_nodelay(fd) {
                        eprintln!("warn: TCP_NODELAY failed on accepted fd {fd}: {e}");
                    }
                    let id = ConnectionId(self.next_id);
                    self.next_id += 1;
                    let peer = SocketAddrV4::new(
                        std::net::Ipv4Addr::from(u32::from_be(addr.sin_addr.s_addr)),
                        u16::from_be(addr.sin_port),
                    );
                    let conn = Connection::new(fd, peer, id);
                    // register in epoll (ET | IN for reads)
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
        self.try_read_frames(id);
        for frame in self.read_frames.drain(..) {
            self.handler.on_frame(id, frame);
        }

        // drain outbound frames, tracking connections for backpressure
        // swap out the buffer to avoid borrowing self while iterating
        self.handler.drain_outbound(&mut self.outbound_buf);
        let mut outbound = std::mem::take(&mut self.outbound_buf);
        self.overflow_buf.clear();
        self.written_buf.clear();
        for (target_id, frame) in outbound.drain(..) {
            match self.send_to(target_id, &frame) {
                WriteOutcome::Overflow => self.overflow_buf.push(target_id),
                WriteOutcome::Queued => self.written_buf.push(target_id),
                _ => {}
            }
        }
        self.outbound_buf = outbound;

        // tear down connections whose write buffer exceeded HARD_CAP
        for &target_id in &self.overflow_buf {
            if let Some(conn) = self.connections.get_mut(&target_id) {
                conn.start_teardown(TeardownReason::SlowClient);
            }
        }

        // apply backpressure to target connections with high write buffer
        for &target_id in &self.written_buf {
            if let Some(conn) = self.connections.get_mut(&target_id) {
                if conn.should_pause_reading() && conn.teardown.is_none() {
                    conn.read_paused = true;
                    let events =
                        (libc::EPOLLOUT | libc::EPOLLET | libc::EPOLLRDHUP) as u32;
                    let _ = sys::epoll_mod(self.epoll_fd, conn.fd, events, target_id.into());
                }
            }
        }

        // also apply backpressure to the source connection (original behavior)
        if let Some(conn) = self.connections.get_mut(&id) {
            if conn.should_pause_reading() && conn.teardown.is_none() {
                conn.read_paused = true;
                let events = (libc::EPOLLOUT | libc::EPOLLET | libc::EPOLLRDHUP) as u32;
                let _ = sys::epoll_mod(self.epoll_fd, conn.fd, events, id.into());
            }
        }

        // drain teardown requests from the handler
        self.handler.drain_teardowns(&mut self.teardown_buf);
        let mut teardowns = std::mem::take(&mut self.teardown_buf);
        for (teardown_id, reason) in teardowns.drain(..) {
            if let Some(conn) = self.connections.get_mut(&teardown_id) {
                conn.start_teardown(reason);
            }
        }
        self.teardown_buf = teardowns;
    }

    /// read and decode any available frames into the reusable buffer
    fn try_read_frames(&mut self, id: ConnectionId) {
        self.read_frames.clear();
        let conn = match self.connections.get_mut(&id) {
            Some(c) => c,
            None => return,
        };
        if conn.read_paused {
            return;
        }
        loop {
            let scratch = &mut self.read_scratch[..];
            match conn.do_read(scratch) {
                Ok(0) => break,
                Ok(_) => {
                    if conn.read_buffered_bytes() > READ_BUFFER_HARD_CAP {
                        conn.start_teardown(TeardownReason::ProtocolViolation);
                        self.read_frames.clear();
                        return;
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(_) => {
                    conn.start_teardown(TeardownReason::PeerClosed);
                    self.read_frames.clear();
                    return;
                }
            }
        }
        loop {
            match conn.try_decode_frame() {
                Ok(Some(f)) => self.read_frames.push(f),
                Ok(None) => break,
                Err(_) => {
                    conn.start_teardown(TeardownReason::ProtocolViolation);
                    self.read_frames.clear();
                    return;
                }
            }
        }
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
                // normal write flush
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

    /// check idle deadlines; tear down stale connections
    fn sweep_timeouts(&mut self) {
        let now = sys::now_ms();

        // 1 mark idle connections for teardown.
        //
        // The idle timeout is currently DISABLED: a chat client sends only on
        // user input, so it is legitimately silent for long stretches. Killing
        // it after 60s of inactivity breaks long-lived sessions. Keep the
        // teardown-completion pass below (genuine EOFs and explicit Goodbyes
        // still get reaped). To re-enable idle pruning, set this to true and
        // make sure the client auto-pings well within DEFAULT_IDLE_TIMEOUT.
        const ENFORCE_IDLE_TIMEOUT: bool = false;

        if ENFORCE_IDLE_TIMEOUT {
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
        }

        // 2 close connections whose teardown is complete
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

    /// enqueue a frame to a specific connection
    pub fn send_to(&mut self, id: ConnectionId, frame: &[u8]) -> WriteOutcome {
        let Some(conn) = self.connections.get_mut(&id) else {
            return WriteOutcome::Rejected;
        };
        let result = conn.enqueue_frame(frame);
        if result == WriteOutcome::Queued && !conn.write_pending {
            conn.write_pending = true;
            // register EPOLLOUT interest
            let events =
                (libc::EPOLLIN | libc::EPOLLOUT | libc::EPOLLET | libc::EPOLLRDHUP) as u32;
            let _ = sys::epoll_mod(self.epoll_fd, conn.fd, events, id.into());
        }
        result
    }

    /// returns the number of connected clients
    pub fn connection_count(&self) -> usize {
        self.connections.len()
    }

    /// drain outbound frames and teardowns from the handler and apply them
    fn drain_handler_queues(&mut self) {
        self.handler.drain_outbound(&mut self.outbound_buf);
        let mut outbound = std::mem::take(&mut self.outbound_buf);
        self.overflow_buf.clear();
        self.written_buf.clear();
        for (target_id, frame) in outbound.drain(..) {
            match self.send_to(target_id, &frame) {
                WriteOutcome::Overflow => self.overflow_buf.push(target_id),
                WriteOutcome::Queued => self.written_buf.push(target_id),
                _ => {}
            }
        }
        self.outbound_buf = outbound;

        for &target_id in &self.overflow_buf {
            if let Some(conn) = self.connections.get_mut(&target_id) {
                conn.start_teardown(TeardownReason::SlowClient);
            }
        }

        for &target_id in &self.written_buf {
            if let Some(conn) = self.connections.get_mut(&target_id) {
                if conn.should_pause_reading() && conn.teardown.is_none() {
                    conn.read_paused = true;
                    let events =
                        (libc::EPOLLOUT | libc::EPOLLET | libc::EPOLLRDHUP) as u32;
                    let _ = sys::epoll_mod(self.epoll_fd, conn.fd, events, target_id.into());
                }
            }
        }

        self.handler.drain_teardowns(&mut self.teardown_buf);
        let mut teardowns = std::mem::take(&mut self.teardown_buf);
        for (teardown_id, reason) in teardowns.drain(..) {
            if let Some(conn) = self.connections.get_mut(&teardown_id) {
                conn.start_teardown(reason);
            }
        }
        self.teardown_buf = teardowns;
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
