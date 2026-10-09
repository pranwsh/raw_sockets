//! Live raw-IP socket tests.
//!
//! These need `CAP_NET_RAW` (run as root, or grant with `setcap cap_net_raw+ep`).
//! Without it every test reports SKIP rather than failing, so the suite is safe
//! to run in an unprivileged container or CI.
//!
//! What is proven here, against the real kernel:
//!   * a hand-built IPv4/UDP packet from `SOCK_RAW` + `IP_HDRINCL` is accepted
//!     by a normal UDP listener, byte for byte;
//!   * the checksums we compute are the ones a receiver verifies — a wrong one
//!     is silently dropped, so delivery is the proof;
//!   * payloads of many sizes round-trip in order.
//!
//! Fragmentation is deliberately NOT asserted: Linux's loopback interface does
//! not reassemble hand-built IPv4 fragments for local UDP delivery, so that
//! cannot be tested here (see the note in `transport::packet`).

use std::io;

use transport::packet::{encode_datagram, Endpoint};
use transport::sys;

/// 127.0.0.1 in network byte order, as stored in `sockaddr_in::sin_addr`
const LOOPBACK_BE: u32 = 0x7F00_0001;

fn sockaddr(addr: u32, port: u16) -> libc::sockaddr_in {
    // delegates to the single place the byte order is decided
    sys::sockaddr_v4(addr, port)
}

/// a bound UDP socket with a receive timeout, used to prove delivery
struct UdpSink {
    fd: i32,
    addr: libc::sockaddr_in,
}

impl UdpSink {
    fn bind() -> io::Result<UdpSink> {
        let fd = unsafe { libc::socket(libc::AF_INET, libc::SOCK_DGRAM, 0) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let addr = sockaddr(LOOPBACK_BE, 0);
        let rc = unsafe {
            libc::bind(
                fd,
                &addr as *const _ as *const libc::sockaddr,
                std::mem::size_of_val(&addr) as libc::socklen_t,
            )
        };
        if rc < 0 {
            let e = io::Error::last_os_error();
            unsafe { libc::close(fd) };
            return Err(e);
        }
        // discover the port the kernel assigned
        let mut real = sockaddr(LOOPBACK_BE, 0);
        let mut len = std::mem::size_of_val(&real) as libc::socklen_t;
        let rc = unsafe {
            libc::getsockname(
                fd,
                &mut real as *mut _ as *mut libc::sockaddr,
                &mut len,
            )
        };
        if rc < 0 {
            let e = io::Error::last_os_error();
            unsafe { libc::close(fd) };
            return Err(e);
        }
        // a dropped datagram must fail, not hang the suite
        let tv = libc::timeval {
            tv_sec: 2,
            tv_usec: 0,
        };
        unsafe {
            libc::setsockopt(
                fd,
                libc::SOL_SOCKET,
                libc::SO_RCVTIMEO,
                &tv as *const _ as *const libc::c_void,
                std::mem::size_of_val(&tv) as libc::socklen_t,
            )
        };
        Ok(UdpSink { fd, addr: real })
    }

    fn port(&self) -> u16 {
        u16::from_be(self.addr.sin_port)
    }

    fn recv(&self, buf: &mut [u8]) -> io::Result<usize> {
        let mut from = sockaddr(0, 0);
        let mut fromlen = std::mem::size_of_val(&from) as libc::socklen_t;
        let rc = unsafe {
            libc::recvfrom(
                self.fd,
                buf.as_mut_ptr() as *mut libc::c_void,
                buf.len(),
                0,
                &mut from as *mut _ as *mut libc::sockaddr,
                &mut fromlen,
            )
        };
        if rc < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(rc as usize)
    }
}

impl Drop for UdpSink {
    fn drop(&mut self) {
        unsafe { libc::close(self.fd) };
    }
}

struct RawFd(i32);
impl Drop for RawFd {
    fn drop(&mut self) {
        unsafe { libc::close(self.0) };
    }
}

/// open a raw socket, or `None` when `CAP_NET_RAW` is unavailable
fn raw_socket() -> Option<RawFd> {
    match sys::socket_raw_ipv4() {
        Ok(fd) => match sys::set_ip_hdrincl(fd) {
            Ok(()) => {
                let _ = sys::set_mtu_discover(fd);
                Some(RawFd(fd))
            }
            Err(e) => {
                eprintln!("SKIP: IP_HDRINCL unavailable: {e}");
                unsafe { libc::close(fd) };
                None
            }
        },
        Err(e) if e.kind() == io::ErrorKind::PermissionDenied => {
            eprintln!("SKIP: no CAP_NET_RAW ({e})");
            None
        }
        Err(e) => panic!("raw socket failed unexpectedly: {e}"),
    }
}

/// send `msg` from `raw` to `sink` and read it back
fn round_trip(raw: &RawFd, sink: &UdpSink, src_port: u16, msg: &[u8], mtu: usize, id: u16) -> usize {
    let src = Endpoint {
        addr: LOOPBACK_BE,
        port: src_port,
    };
    let dst = Endpoint {
        addr: LOOPBACK_BE,
        port: sink.port(),
    };
    let fs = encode_datagram(src, dst, msg, id, mtu);
    for p in &fs.packets {
        let n = sys::send_packet(raw.0, &p.bytes, &sink.addr).expect("send_packet");
        assert_eq!(n, p.bytes.len(), "kernel accepted {n} of {}", p.bytes.len());
    }
    let mut buf = vec![0u8; 70_000];
    let n = sink
        .recv(&mut buf)
        .expect("datagram must be delivered intact");
    assert_eq!(&buf[..n], msg, "payload must survive the round trip intact");
    n
}

#[test]
fn a_raw_built_datagram_is_delivered_intact() {
    let Some(raw) = raw_socket() else { return };
    let sink = UdpSink::bind().expect("bind UDP sink");
    let n = round_trip(&raw, &sink, 40_000, b"raw ip round trip payload", 1500, 0x1234);
    eprintln!("delivered {n} bytes intact");
}

#[test]
fn payloads_of_many_sizes_round_trip() {
    let Some(raw) = raw_socket() else { return };
    let sink = UdpSink::bind().expect("bind UDP sink");
    // loopback MTU is 64 KiB, so these stay unfragmented; fragment reassembly is
    // covered by the unit tests in `packet`, not here.
    for len in [1usize, 12, 64, 256, 1024, 2000, 8192] {
        let msg: Vec<u8> = (0..len).map(|i| (i % 251) as u8).collect();
        round_trip(&raw, &sink, 40_001, &msg, 65_536, len as u16);
    }
    eprintln!("7 payload sizes round-tripped");
}

#[test]
fn datagrams_flow_in_sequence() {
    let Some(raw) = raw_socket() else { return };
    let sink = UdpSink::bind().expect("bind UDP sink");
    for i in 0..16u32 {
        let msg = format!("frame-{i}");
        round_trip(&raw, &sink, 40_002, msg.as_bytes(), 1500, i as u16 + 1);
    }
    eprintln!("16 sequential datagrams round-tripped");
}

#[test]
fn a_deliberately_wrong_checksum_is_rejected() {
    // Documents why the checksums must be right: a bad one is dropped with no
    // error from sendto at all.
    let Some(raw) = raw_socket() else { return };
    let sink = UdpSink::bind().expect("bind UDP sink");
    let src = Endpoint {
        addr: LOOPBACK_BE,
        port: 40_003,
    };
    let dst = Endpoint {
        addr: LOOPBACK_BE,
        port: sink.port(),
    };
    let fs = encode_datagram(src, dst, b"corrupt me", 1, 1500);
    let mut pkt = fs.packets[0].bytes.clone();
    pkt[26] ^= 0xFF; // flip a bit in the UDP checksum

    let n = sys::send_packet(raw.0, &pkt, &sink.addr).expect("send_packet");
    assert_eq!(n, pkt.len(), "sendto still succeeds");

    let mut buf = vec![0u8; 2048];
    let err = sink
        .recv(&mut buf)
        .expect_err("a corrupt checksum must not be delivered");
    assert_eq!(err.kind(), io::ErrorKind::WouldBlock);
}

#[test]
fn a_corrupt_ipv4_header_is_rejected() {
    let Some(raw) = raw_socket() else { return };
    let sink = UdpSink::bind().expect("bind UDP sink");
    let src = Endpoint {
        addr: LOOPBACK_BE,
        port: 40_004,
    };
    let dst = Endpoint {
        addr: LOOPBACK_BE,
        port: sink.port(),
    };
    let fs = encode_datagram(src, dst, b"bad header", 1, 1500);
    let mut pkt = fs.packets[0].bytes.clone();
    pkt[15] ^= 0xFF; // corrupt the destination address, invalidating the header CRC

    let _ = sys::send_packet(raw.0, &pkt, &sink.addr).expect("send_packet");
    let mut buf = vec![0u8; 2048];
    let err = sink
        .recv(&mut buf)
        .expect_err("a corrupt IPv4 header must not be delivered");
    assert_eq!(err.kind(), io::ErrorKind::WouldBlock);
}
