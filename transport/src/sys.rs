//! thin, safe wrappers around raw libc syscalls used on the hot path

#![deny(unsafe_op_in_unsafe_fn)]

use libc::{c_int, c_void, size_t};
use std::io;
use std::os::unix::io::RawFd;

// non-blocking fd management

/// set O_NONBLOCK on fd returns io::Error on failure (e.g bad fd)
pub fn set_nonblocking(fd: RawFd) -> io::Result<()> {
    let flags = unsafe {
        // SAFETY: fcntl with F_GETFL reads current file status flags; it is safe as long as fd is a valid open file descriptor, which the caller guarantees
        let fl = libc::fcntl(fd, libc::F_GETFL);
        if fl < 0 {
            return Err(io::Error::last_os_error());
        }
        fl
    };
    let rc = unsafe {
        // SAFETY: F_SETFL with O_NONBLOCK writes the (idempotent) flag; same fd validity assumptions as above
        libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK)
    };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// enable SO_REUSEPORT on a socket so multiple listeners on different cores can share the same port without a cross-thread accept handoff
pub fn set_reuseport(fd: RawFd) -> io::Result<()> {
    let optval: c_int = 1;
    let rc = unsafe {
        // SAFETY: setsockopt for SO_REUSEPORT writes an int-sized option; safe with a valid fd and valid pointer to optval
        libc::setsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_REUSEPORT,
            &optval as *const _ as *const c_void,
            std::mem::size_of_val(&optval) as libc::socklen_t,
        )
    };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// set SO_REUSEADDR for testing / fast restarts
pub fn set_reuseaddr(fd: RawFd) -> io::Result<()> {
    let optval: c_int = 1;
    let rc = unsafe {
        libc::setsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_REUSEADDR,
            &optval as *const _ as *const c_void,
            std::mem::size_of_val(&optval) as libc::socklen_t,
        )
    };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// disable Nagle's algorithm (TCP_NODELAY) on an accepted connection.
///
/// Messaging frames are small; without NODELAY the kernel may coalesce them
/// into fewer packets and wait on a delayed ACK (~40ms) before flushing, which
/// is noticeable latency on top of the store round-trips. NODELAY makes small
/// frames leave immediately.
pub fn set_tcp_nodelay(fd: RawFd) -> io::Result<()> {
    let optval: c_int = 1;
    let rc = unsafe {
        // SAFETY: setsockopt for IPPROTO_TCP/TCP_NODELAY writes an int-sized
        // option; safe with a valid fd and a valid pointer to optval.
        libc::setsockopt(
            fd,
            libc::IPPROTO_TCP,
            libc::TCP_NODELAY,
            &optval as *const _ as *const c_void,
            std::mem::size_of_val(&optval) as libc::socklen_t,
        )
    };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// bind to a local sockaddr_in returns io::Error on failure
pub fn bind_v4(fd: RawFd, addr: &libc::sockaddr_in) -> io::Result<()> {
    let rc = unsafe {
        // SAFETY: bind writes no memory; valid fd, valid addr pointer
        libc::bind(fd, addr as *const _ as *const libc::sockaddr, std::mem::size_of_val(addr) as libc::socklen_t)
    };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// listen on a socket with the given backlog
pub fn listen(fd: RawFd, backlog: c_int) -> io::Result<()> {
    let rc = unsafe {
        // SAFETY: listen takes a fd and an int; safe
        libc::listen(fd, backlog)
    };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// accept a new connection, returning (client_fd, peer_addr)
pub fn accept(fd: RawFd) -> io::Result<(RawFd, libc::sockaddr_in)> {
    let mut addr: libc::sockaddr_in = unsafe { std::mem::zeroed() };
    let mut addrlen: libc::socklen_t = std::mem::size_of_val(&addr) as libc::socklen_t;
    let rc = unsafe {
        // SAFETY: accept writes addrlen bytes into addr; zeroed memory has enough capacity and the kernel respects addrlen
        libc::accept4(fd, &mut addr as *mut _ as *mut libc::sockaddr, &mut addrlen, libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK)
    };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok((rc, addr))
}

// non-blocking i/o

/// non-blocking read
pub fn read(fd: RawFd, buf: &mut [u8]) -> io::Result<usize> {
    loop {
        let rc = unsafe {
            // SAFETY: read into a valid mutable slice; buf's length is the capacity
            libc::read(fd, buf.as_mut_ptr() as *mut c_void, buf.len() as size_t)
        };
        if rc == -1 {
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::Interrupted {
                continue; // SA_RESTART not guaranteed on every platform.
            }
            return Err(err);
        }
        return Ok(rc as usize);
    }
}

/// non-blocking write of buf contents
pub fn write(fd: RawFd, buf: &[u8]) -> io::Result<usize> {
    loop {
        let rc = unsafe {
            // SAFETY: write reads from buf; no memory is written
            libc::write(fd, buf.as_ptr() as *const c_void, buf.len() as size_t)
        };
        if rc == -1 {
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(err);
        }
        return Ok(rc as usize);
    }
}

/// shut down the read side (SHUT_RD) or both sides (SHUT_RDWR)
pub fn shutdown(fd: RawFd, how: c_int) -> io::Result<()> {
    let rc = unsafe {
        // SAFETY: shutdown takes a fd; safe
        libc::shutdown(fd, how)
    };
    if rc < 0 {
        // if the socket is already closed or not connected, ignore
        let err = io::Error::last_os_error();
        if err.kind() == io::ErrorKind::NotConnected {
            return Ok(());
        }
        return Err(err);
    }
    Ok(())
}

/// close a file descriptor
pub fn close(fd: RawFd) {
    unsafe {
        // SAFETY: close is safe; even if fd is invalid it returns EBADF without UB (POSIX guarantee, confirmed by linux man page)
        libc::close(fd);
    }
}

// epoll wrappers

/// create an epoll fd with EPOLL_CLOEXEC
pub fn epoll_create() -> io::Result<RawFd> {
    let fd = unsafe {
        // SAFETY: epoll_create1 is safe; the kernel allocates a backing data structure and returns a new fd handle
        libc::epoll_create1(libc::EPOLL_CLOEXEC)
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(fd)
}

/// epoll_ctl wrapper: add fd with events
pub fn epoll_add(epfd: RawFd, fd: RawFd, events: u32, ptr: u64) -> io::Result<()> {
    let mut ev = libc::epoll_event { events, u64: ptr };
    let rc = unsafe {
        // SAFETY: epoll_ctl copies ev into kernel space; no ownership transfer
        libc::epoll_ctl(epfd, libc::EPOLL_CTL_ADD, fd, &mut ev)
    };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// epoll_ctl wrapper: modify events for an existing fd
pub fn epoll_mod(epfd: RawFd, fd: RawFd, events: u32, ptr: u64) -> io::Result<()> {
    let mut ev = libc::epoll_event { events, u64: ptr };
    let rc = unsafe {
        libc::epoll_ctl(epfd, libc::EPOLL_CTL_MOD, fd, &mut ev)
    };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// epoll_ctl wrapper: delete fd
pub fn epoll_del(epfd: RawFd, fd: RawFd) -> io::Result<()> {
    let rc = unsafe {
        // epoll_ctl with EPOLL_CTL_DEL ignores the event pointer; passing null is valid per man page
        libc::epoll_ctl(epfd, libc::EPOLL_CTL_DEL, fd, std::ptr::null_mut())
    };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// blocking wait for events
pub fn epoll_wait(epfd: RawFd, events: &mut [libc::epoll_event], timeout_ms: c_int) -> io::Result<usize> {    loop {
        let n = unsafe {
            // SAFETY: events is a mutable slice; epoll_wait will write at most events.len() events into it
            libc::epoll_wait(epfd, events.as_mut_ptr(), events.len() as c_int, timeout_ms)
        };
        if n < 0 {
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(err);
        }
        return Ok(n as usize);
    }
}

// raw ip socket — the datagram transport's network layer
//
// The server owns its own IPv4/UDP headers (`packet` module) and hands the
// kernel a complete packet via `IP_HDRINCL`. That requires `CAP_NET_RAW`, so
// every function here can fail with EPERM on a host without the capability.

/// create a raw IPv4 socket for datagrams (`SOCK_RAW` + `IPPROTO_RAW`)
///
/// With `IPPROTO_RAW` the protocol is taken from each packet's IP header, so one
/// socket can carry every protocol. Requires `CAP_NET_RAW`.
pub fn socket_raw_ipv4() -> io::Result<RawFd> {
    let fd = unsafe {
        // SAFETY: socket() allocates a descriptor and returns it; IPPROTO_RAW
        // defers the protocol to the per-packet IP header, so the kernel does
        // not filter by protocol here.
        libc::socket(
            libc::AF_INET,
            libc::SOCK_RAW | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK,
            libc::IPPROTO_RAW,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(fd)
}

/// enable `IP_HDRINCL` so the caller supplies the IP header
///
/// Without this the kernel refuses packets and overwrites the header.
pub fn set_ip_hdrincl(fd: RawFd) -> io::Result<()> {
    let optval: c_int = 1;
    let rc = unsafe {
        // SAFETY: setsockopt with IPPROTO_IP/IP_HDRINCL writes an int-sized
        // option; valid fd and valid pointer to optval.
        libc::setsockopt(
            fd,
            libc::IPPROTO_IP,
            libc::IP_HDRINCL,
            &optval as *const _ as *const c_void,
            std::mem::size_of_val(&optval) as libc::socklen_t,
        )
    };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// build a `sockaddr_in` for an IPv4 endpoint
///
/// `sin_addr.s_addr` holds the four address octets in **network byte order in
/// memory**, which is what the kernel reads. `Endpoint::addr` is already in
/// network byte order as a number, so it must be byte-swapped once here:
/// assigning it directly writes the octets in reverse on a little-endian host
/// and the kernel reports `EADDRNOTAVAIL`.
///
/// Use this rather than constructing the struct inline; getting the order wrong
/// is silent until a bind or send fails.
pub fn sockaddr_v4(addr: u32, port: u16) -> libc::sockaddr_in {
    libc::sockaddr_in {
        sin_family: libc::AF_INET as libc::sa_family_t,
        sin_port: port.to_be(),
        sin_addr: libc::in_addr { s_addr: addr.to_be() },
        sin_zero: [0u8; 8],
    }
}

/// bind a raw socket to a local IPv4 address/port
///
/// Unlike a stream socket this does not reserve the port exclusively, so it is
/// not a substitute for `listen`.
pub fn bind_raw_v4(fd: RawFd, addr: &libc::sockaddr_in) -> io::Result<()> {
    let rc = unsafe {
        // SAFETY: bind reads the address; valid fd and valid addr pointer.
        libc::bind(
            fd,
            addr as *const _ as *const libc::sockaddr,
            std::mem::size_of_val(addr) as libc::socklen_t,
        )
    };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// send one complete IPv4 packet to `addr`
///
/// Returns the number of bytes the kernel accepted, which is the whole packet
/// on success. `EMSGSIZE` means it exceeded the interface MTU and must be
/// fragmented by the caller (see `packet::encode_datagram`).
pub fn send_packet(fd: RawFd, packet: &[u8], addr: &libc::sockaddr_in) -> io::Result<usize> {
    loop {
        let rc = unsafe {
            // SAFETY: send_packet reads from `packet` and writes no memory.
            libc::sendto(
                fd,
                packet.as_ptr() as *const c_void,
                packet.len() as size_t,
                0,
                addr as *const _ as *const libc::sockaddr,
                std::mem::size_of_val(addr) as libc::socklen_t,
            )
        };
        if rc < 0 {
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(err);
        }
        return Ok(rc as usize);
    }
}

/// receive one packet, reporting the sender in `from`
///
/// `MSG_TRUNC` is passed so the return value is the REAL datagram size even when
/// it did not fit the buffer — without it a truncated datagram is
/// indistinguishable from a small one, and the frame would be silently corrupt.
pub fn recv_packet(fd: RawFd, buf: &mut [u8], from: &mut libc::sockaddr_in) -> io::Result<usize> {
    loop {
        let mut fromlen = std::mem::size_of_val(from) as libc::socklen_t;
        let rc = unsafe {
            // SAFETY: recv_packet writes at most buf.len() bytes into buf and up
            // to fromlen bytes into `from`, whose size we pass in.
            libc::recvfrom(
                fd,
                buf.as_mut_ptr() as *mut c_void,
                buf.len() as size_t,
                libc::MSG_TRUNC,
                from as *mut _ as *mut libc::sockaddr,
                &mut fromlen,
            )
        };
        if rc < 0 {
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(err);
        }
        return Ok(rc as usize);
    }
}

/// set the outgoing interface MTU hint used when choosing fragment size
pub fn set_mtu_discover(fd: RawFd) -> io::Result<()> {
    let optval: c_int = libc::IP_PMTUDISC_DO;
    let rc = unsafe {
        // SAFETY: setsockopt with IPPROTO_IP/IP_MTU_DISCOVER writes an int-sized
        // option; valid fd and valid pointer to optval.
        libc::setsockopt(
            fd,
            libc::IPPROTO_IP,
            libc::IP_MTU_DISCOVER,
            &optval as *const _ as *const c_void,
            std::mem::size_of_val(&optval) as libc::socklen_t,
        )
    };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

// eventfd wakeup — lets the store worker nudge the reactor's epoll loop so
// async results are observed immediately instead of after the epoll_wait
// timeout. The single fd is both the readable side (registered with epoll) and
// the writable side (poked by the worker).

/// create a non-blocking eventfd used to wake the reactor out of epoll_wait
pub fn create_eventfd() -> io::Result<RawFd> {
    let fd = unsafe {
        // SAFETY: eventfd is safe; the kernel allocates a counter and returns a new fd handle
        libc::eventfd(0, libc::EFD_NONBLOCK | libc::EFD_CLOEXEC)
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(fd)
}

/// increment the eventfd counter to wake the reactor (best-effort)
pub fn wake_eventfd(fd: RawFd) {
    let val: u64 = 1;
    let rc = unsafe {
        // SAFETY: write of a u64 counter value to a valid eventfd; best-effort, ignores EAGAIN
        libc::write(fd, &val as *const u64 as *const c_void, std::mem::size_of_val(&val) as size_t)
    };
    if rc < 0 {
        let err = io::Error::last_os_error();
        if matches!(err.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted) {
            // counter is full (only possible if the reactor never drains) or
            // interrupted — both safe to ignore
        }
    }
}

/// drain the eventfd counter so epoll stops reporting it as readable
pub fn drain_eventfd(fd: RawFd) {
    let mut buf = [0u8; 8];
    let _ = unsafe {
        // SAFETY: read of the 8-byte counter value into buf; safe on a valid eventfd
        libc::read(fd, buf.as_mut_ptr() as *mut c_void, buf.len() as size_t)
    };
}

// time helpers

/// monotonic millisecond timestamp for timeout calculations
pub fn now_ms() -> u64 {
    let mut ts: libc::timespec = unsafe { std::mem::zeroed() };
    unsafe {
        libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts);
    }
    (ts.tv_sec as u64) * 1000 + (ts.tv_nsec as u64) / 1_000_000
}
