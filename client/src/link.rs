//! The byte-level link to a server, shared by both transports.
//!
//! [`Client`] is written against these traits rather than against `TcpStream`,
//! so the framing, decode, and event plumbing exists once and both transports
//! reuse it. The two differ in more than the socket call:
//!
//! * **TCP** is a byte stream, so a read can return part of a frame, several
//!   frames, or a piece of one. Bytes accumulate and frames are decoded out as
//!   they complete.
//! * **raw IP** is a datagram transport. Each datagram carries one complete
//!   payload after the reliability layer has reordered it, so there is no
//!   partial read to handle — but sequencing, acks, retransmission, checksums
//!   and fragmentation have to be supplied instead.
//!
//! # Why reader and writer are separate objects
//!
//! They must not share a lock. The reader parks in the kernel waiting for
//! inbound data, and if it held a mutex across that wait the writer could never
//! acquire it to send — a deadlock that only appears when a message is sent
//! before any reply arrives. So each half owns its own handle on the socket
//! (a duplicated file descriptor for TCP, a duped raw socket otherwise) and the
//! two threads share nothing but the action and event channels.

use std::io::{self, Read};
use std::os::fd::{AsRawFd, RawFd};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// which transport a [`crate::Client`] speaks
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transport {
    /// TCP stream sockets. No privileges; reaches another host.
    Tcp,
    /// raw IP/UDP datagrams. Needs `CAP_NET_RAW`; local interface only.
    RawIp,
}

impl Transport {
    /// parse a transport name as accepted on the command line
    pub fn parse(s: &str) -> Result<Transport, String> {
        match s {
            "tcp" => Ok(Transport::Tcp),
            "raw-ip" | "udp" => Ok(Transport::RawIp),
            other => Err(format!(
                "unknown transport {other:?} (expected tcp or raw-ip)"
            )),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Transport::Tcp => "tcp",
            Transport::RawIp => "raw-ip",
        }
    }
}

/// one decoded inbound frame: the message type and its body
pub type InboundFrame = (protocol::MsgType, Vec<u8>);

/// the inbound half, owned solely by the reader thread
pub trait LinkReader: Send + 'static {
    /// read whatever is available, returning the frames it completed.
    ///
    /// Returns an empty vector when nothing arrived. Must not block while
    /// holding anything the writer needs.
    fn read(&mut self) -> io::Result<Vec<InboundFrame>>;
}

/// the outbound half, owned solely by the writer thread
pub trait LinkWriter: Send + 'static {
    /// send one complete frame
    fn write_frame(&mut self, frame: &[u8]) -> io::Result<()>;
    /// stop sending
    fn close(&mut self);
}

/// unblocks a parked reader from another thread
pub trait Closer: Send + Sync + 'static {
    fn close(&self);
}

/// the three pieces a [`crate::Client`] drives
pub struct LinkParts {
    pub reader: Box<dyn LinkReader>,
    pub writer: Box<dyn LinkWriter>,
    pub closer: Box<dyn Closer>,
}

/// open a link to `server`, over `transport`
pub fn open(transport: Transport, host: &str, port: u16) -> io::Result<LinkParts> {
    match transport {
        Transport::Tcp => open_tcp(host, port),
        Transport::RawIp => open_raw_ip(host, port),
    }
}

// ---------------------------------------------------------------- TCP

/// how long the TCP reader may park before re-checking liveness. This bounds
/// how long a silent server takes to be noticed; it is not a latency path — a
/// healthy connection is woken by the kernel the moment bytes arrive.
const READ_PARK_TIMEOUT: Duration = Duration::from_secs(60);

/// initial read-buffer size; grows to fit the largest frame seen
const READ_BUF_INIT: usize = 64 * 1024;
/// kernel read granularity
const READ_CHUNK: usize = 16 * 1024;

/// a blocking socket with a read timeout surfaces this, and so does a
/// non-blocking one, depending on platform
fn is_park_timeout(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
    )
}

fn open_tcp(host: &str, port: u16) -> io::Result<LinkParts> {
    let stream = std::net::TcpStream::connect(format!("{host}:{port}"))?;
    // Disable Nagle: message frames are small and lateness-sensitive, and an
    // unacknowledged in-flight write would otherwise hold up the next frame for
    // up to ~40ms (delayed-ACK).
    stream.set_nodelay(true)?;

    // `try_clone` dups the file descriptor, so the reader and the writer each
    // have their own handle and neither blocks the other.
    let reader_stream = stream.try_clone()?;
    reader_stream.set_read_timeout(Some(READ_PARK_TIMEOUT))?;
    let closer_stream = stream.try_clone()?;

    Ok(LinkParts {
        reader: Box::new(TcpReader {
            stream: reader_stream,
            buf: Vec::with_capacity(READ_BUF_INIT),
            scratch: vec![0u8; READ_CHUNK],
            offset: 0,
        }),
        writer: Box::new(TcpWriter { stream }),
        closer: Box::new(TcpCloser {
            stream: closer_stream,
        }),
    })
}

struct TcpReader {
    stream: std::net::TcpStream,
    buf: Vec<u8>,
    scratch: Vec<u8>,
    offset: usize,
}

impl LinkReader for TcpReader {
    fn read(&mut self) -> io::Result<Vec<InboundFrame>> {
        // compact first so `buf[offset..]` is the unconsumed tail; `buf` grows
        // monotonically to the largest frame instead of being drained each round
        if self.offset > 0 {
            self.buf.copy_within(self.offset.., 0);
            self.buf.truncate(self.buf.len() - self.offset);
            self.offset = 0;
        }
        match self.stream.read(&mut self.scratch) {
            Ok(0) => return Err(io::Error::from(io::ErrorKind::UnexpectedEof)),
            Ok(n) => self.buf.extend_from_slice(&self.scratch[..n]),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => return Ok(Vec::new()),
            Err(e) if is_park_timeout(&e) => return Ok(Vec::new()), // liveness poll
            Err(e) => return Err(e),
        };

        let mut out = Vec::new();
        loop {
            match protocol::decode(&self.buf[self.offset..]) {
                protocol::Decode::Complete { frame, consumed } => {
                    self.offset += consumed;
                    out.push((frame.msg_type, frame.body.to_vec()));
                }
                protocol::Decode::Need => break,
                // misaligned garbage: skip one byte and resync
                protocol::Decode::Err(_) => self.offset += 1,
            }
        }
        Ok(out)
    }
}

struct TcpWriter {
    stream: std::net::TcpStream,
}

impl LinkWriter for TcpWriter {
    fn write_frame(&mut self, frame: &[u8]) -> io::Result<()> {
        use std::io::Write;
        self.stream.write_all(frame)
    }
    fn close(&mut self) {
        // a clean end-of-stream, so the server sees a prompt close rather than
        // waiting out its own keepalive
        let _ = self.stream.shutdown(std::net::Shutdown::Write);
    }
}

struct TcpCloser {
    stream: std::net::TcpStream,
}

impl Closer for TcpCloser {
    fn close(&self) {
        // shutting down both halves makes the reader's parked read return
        // immediately instead of holding the thread until the peer disconnects
        let _ = self.stream.shutdown(std::net::Shutdown::Both);
    }
}

// ---------------------------------------------------------------- raw IP

/// how long the raw-IP reader sleeps between polls when the socket is empty
///
/// A raw socket is non-blocking, so `recvfrom` reports "nothing yet" straight
/// away rather than parking. Polling with a small sleep keeps an idle client
/// cheap without the reader ever holding up the writer.
const RAW_POLL_INTERVAL: Duration = Duration::from_micros(250);

fn open_raw_ip(host: &str, port: u16) -> io::Result<LinkParts> {
    use transport::sys;

    let ip: std::net::Ipv4Addr = host.parse().map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("raw-ip needs a literal IPv4 address, got {host:?}"),
        )
    })?;
    let server = transport::packet::Endpoint {
        addr: u32::from_be_bytes(ip.octets()),
        port,
    };

    let socket = sys::socket_raw_ipv4()?;
    if let Err(e) = sys::set_ip_hdrincl(socket) {
        sys::close(socket);
        return Err(e);
    }

    // A raw socket has no UDP port of its own: `bind()` does not reserve one,
    // and `getsockname` reports 0.0.0.0 with the protocol number standing in
    // for the port. Both halves of our identity therefore have to be chosen here
    // and written into every header — a packet with src = 0.0.0.0 is discarded
    // by the receiver without an error, which is a silent failure.
    let local_port = std::net::UdpSocket::bind("0.0.0.0:0")
        .ok()
        .and_then(|s| s.local_addr().ok())
        .map(|a| a.port())
        .unwrap_or_else(|| port.wrapping_add(1));
    let local = transport::packet::Endpoint {
        addr: u32::from_be_bytes(ip.octets()),
        port: local_port,
    };

    // A raw socket is not demultiplexed by port — it sees every datagram on the
    // interface, including other processes' — so both halves filter on our own
    // address and discard loopback reflections of what we sent.
    let reader_fd = dup(socket)?;
    let closed = Arc::new(AtomicBool::new(false));

    Ok(LinkParts {
        reader: Box::new(RawIpReader {
            fd: reader_fd,
            local,
            reliable: transport::reliable::ReliabilityLayer::new(),
            buf: vec![0u8; 65_536],
            closed: closed.clone(),
        }),
        writer: Box::new(RawIpWriter {
            fd: socket,
            local,
            server,
            reliable: transport::reliable::ReliabilityLayer::new(),
        }),
        closer: Box::new(RawCloser { closed }),
    })
}

/// a second handle on the same raw socket
///
/// The reader and the writer must not share a mutex — the reader polls while the
/// writer sends — so they need independent handles on one socket. `dup2` is a
/// synchronous syscall, but this crate is `#![forbid(unsafe_code)]`, so the
/// duplicate is taken inside the `transport` crate, which already wraps libc
/// with the documented invariants this needs.
fn dup(fd: RawFd) -> io::Result<RawFd> {
    transport::sys::dup_raw_socket(fd)
}

struct RawIpReader {
    fd: RawFd,
    local: transport::packet::Endpoint,
    reliable: transport::reliable::ReliabilityLayer,
    buf: Vec<u8>,
    closed: Arc<AtomicBool>,
}

impl RawIpReader {
    /// read whatever is queued, filtering to traffic addressed to us
    fn drain(&mut self) -> io::Result<Vec<InboundFrame>> {
        use transport::sys;
        let mut out = Vec::new();
        loop {
            let mut from = sys::sockaddr_v4(0, 0);
            let n = match sys::recv_packet(self.fd, &mut self.buf, &mut from) {
                Ok(n) => n,
                // nothing queued: the socket is drained for now
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) => return Err(e),
            };
            let datagram = &self.buf[..n];
            let Some(headers) = transport::packet::parse_headers(datagram) else {
                continue;
            };
            // not addressed to us
            if headers.dst != self.local {
                continue;
            }
            // our own outgoing datagrams come back on loopback
            if headers.src == self.local {
                continue;
            }
            if !transport::packet::verify_ipv4_checksum(datagram) {
                continue;
            }
            let Some(payload) = transport::packet::payload_of(datagram) else {
                continue;
            };
            let Some(env) = transport::reliable::Envelope::decode(payload) else {
                continue;
            };
            let now = sys::now_ms();
            // the reliability layer drops duplicates and reorders; anything it
            // releases is a complete payload waiting to be framed
            for ready in self.reliable.on_envelope(&env, now) {
                if let protocol::Decode::Complete { frame, .. } = protocol::decode(&ready) {
                    out.push((frame.msg_type, frame.body.to_vec()));
                }
            }
        }
        Ok(out)
    }
}

impl LinkReader for RawIpReader {
    fn read(&mut self) -> io::Result<Vec<InboundFrame>> {
        if self.closed.load(Ordering::Relaxed) {
            return Err(io::Error::from(io::ErrorKind::NotConnected));
        }
        let frames = self.drain()?;
        if frames.is_empty() {
            // nothing waiting: sleep outside any shared state so the writer is
            // never held up by an idle reader
            std::thread::sleep(RAW_POLL_INTERVAL);
        }
        Ok(frames)
    }
}

impl Drop for RawIpReader {
    fn drop(&mut self) {
        if self.fd >= 0 {
            transport::sys::close(self.fd);
        }
    }
}

struct RawIpWriter {
    fd: RawFd,
    local: transport::packet::Endpoint,
    server: transport::packet::Endpoint,
    reliable: transport::reliable::ReliabilityLayer,
}

impl LinkWriter for RawIpWriter {
    fn write_frame(&mut self, frame: &[u8]) -> io::Result<()> {
        use transport::sys;
        let now = sys::now_ms();
        let (_, envelope) = self.reliable.wrap(frame, now);
        let encoded = transport::packet::encode_datagram(
            self.local,
            self.server,
            &envelope,
            next_identification(),
            transport::packet::DEFAULT_MTU,
        );
        let target = sys::sockaddr_v4(self.server.addr, self.server.port);
        for p in &encoded.packets {
            sys::send_packet(self.fd, &p.bytes, &target)?;
        }
        Ok(())
    }

    fn close(&mut self) {
        // nothing to drain: a datagram is either sent or gone
    }
}

impl Drop for RawIpWriter {
    fn drop(&mut self) {
        if self.fd >= 0 {
            transport::sys::close(self.fd);
        }
    }
}

struct RawCloser {
    closed: Arc<AtomicBool>,
}

impl Closer for RawCloser {
    fn close(&self) {
        // A raw socket has no shutdown(), so the reader is woken by the flag: it
        // notices within one poll interval and unwinds.
        self.closed.store(true, Ordering::Relaxed);
    }
}

/// monotonic packet identification
fn next_identification() -> u16 {
    use std::sync::atomic::AtomicU16;
    static COUNTER: AtomicU16 = AtomicU16::new(1);
    COUNTER.fetch_add(1, Ordering::Relaxed)
}

/// silence the unused-import lint on platforms where `AsRawFd` is not needed
#[allow(dead_code)]
fn _fd_hint(fd: &impl AsRawFd) -> RawFd {
    fd.as_raw_fd()
}
