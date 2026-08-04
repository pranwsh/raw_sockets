//! thin, safe wrappers around raw libc syscalls used on the hot path

#![deny(unsafe_op_in_unsafe_fn)]

use libc::{c_int, c_void, size_t};
use std::io;
use std::os::unix::io::RawFd;
use std::time::Duration;

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
pub fn epoll_wait(epfd: RawFd, events: &mut [libc::epoll_event], timeout_ms: c_int) -> io::Result<usize> {
    loop {
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

// time helpers

/// monotonic millisecond timestamp for timeout calculations
pub fn now_ms() -> u64 {
    let mut ts: libc::timespec = unsafe { std::mem::zeroed() };
    unsafe {
        libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts);
    }
    (ts.tv_sec as u64) * 1000 + (ts.tv_nsec as u64) / 1_000_000
}

/// compute an absolute deadline in ms from now
pub fn deadline_after(dur: Duration) -> u64 {
    now_ms().saturating_add(dur.as_millis() as u64)
}
