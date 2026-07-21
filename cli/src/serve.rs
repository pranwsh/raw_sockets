use domain::Domain;
use storage::Store;
use transport::Reactor;
use std::net::{SocketAddr, ToSocketAddrs};
use std::os::unix::io::RawFd;

pub fn run(bind: &str, data_path: &str) {
    let store = match Store::open(data_path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("Failed to open store: {}", e);
            std::process::exit(1);
        }
    };

    let (listener_fd, addr) = match create_listener(bind) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("Failed to bind: {}", e);
            std::process::exit(1);
        }
    };
    eprintln!("msgcli serve listening on {} (data: {})", addr, data_path);

    let domain = Domain::new(store);

    let mut reactor: Reactor<Domain> = match Reactor::new(domain, Some(listener_fd)) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("Failed to create reactor: {}", e);
            std::process::exit(1);
        }
    };

    eprintln!("msgcli serve ready.");
    if let Err(e) = reactor.run() {
        eprintln!("msgcli serve error: {}", e);
    }
    eprintln!("msgcli serve shut down.");
}

fn create_listener(bind: &str) -> std::io::Result<(RawFd, SocketAddr)> {
    let addr: SocketAddr = bind
        .to_socket_addrs()
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?
        .next()
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "resolve failed"))?;

    let fd = unsafe { libc::socket(libc::AF_INET, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }

    transport::sys::set_reuseport(fd)?;
    transport::sys::set_reuseaddr(fd)?;
    transport::sys::set_nonblocking(fd)?;

    let SocketAddr::V4(a4) = addr else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "IPv4 only",
        ));
    };
    let addr_in = libc::sockaddr_in {
        sin_family: libc::AF_INET as libc::sa_family_t,
        sin_port: a4.port().to_be(),
        sin_addr: libc::in_addr { s_addr: u32::from_ne_bytes(a4.ip().octets()) },
        sin_zero: [0u8; 8],
    };
    transport::sys::bind_v4(fd, &addr_in)?;
    transport::sys::listen(fd, 128)?;
    Ok((fd, addr))
}
