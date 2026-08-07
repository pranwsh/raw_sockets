//! msgd — high-performance raw-socket messaging server

use domain::Domain;
use storage::{Durability, Store};
use transport::Reactor;
use std::net::{SocketAddr, ToSocketAddrs};
use std::os::unix::io::RawFd;
use std::sync::Arc;

struct Config {
    bind: String,
    data_path: String,
    durable: bool,
}

fn parse_config() -> Config {
    let args: Vec<String> = std::env::args().collect();
    let mut c = Config { bind: "0.0.0.0:9723".into(), data_path: "/tmp/msgd.redb".into(), durable: false };
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--bind" if i + 1 < args.len() => { c.bind = args[i + 1].clone(); i += 2; }
            "--data" if i + 1 < args.len() => { c.data_path = args[i + 1].clone(); i += 2; }
            // durable mode fsyncs every write (crash-safe sequence numbers); the
            // default fast mode skips the fsync on the message path
            "--durable" => { c.durable = true; i += 1; }
            "--fast" => { c.durable = false; i += 1; }
            "--help" | "-h" => {
                eprintln!("Usage: msgd [--bind ADDR] [--data PATH] [--fast|--durable]");
                std::process::exit(0);
            }
            _ => { eprintln!("Unknown: {}", args[i]); std::process::exit(1); }
        }
    }
    c
}

fn create_listener(bind: &str) -> std::io::Result<(RawFd, SocketAddr)> {
    let addr: SocketAddr = bind.to_socket_addrs()
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?
        .next().ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "resolve failed"))?;

    let fd = unsafe {
        libc::socket(libc::AF_INET, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0)
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }

    transport::sys::set_reuseport(fd)?;
    transport::sys::set_reuseaddr(fd)?;
    transport::sys::set_nonblocking(fd)?;

    let SocketAddr::V4(a4) = addr else {
        return Err(std::io::Error::new(std::io::ErrorKind::Unsupported, "IPv4 only"));
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

fn main() {
    let config = parse_config();

    // eventfd: the store worker pokes it after each op so the reactor wakes
    // immediately to drain async results instead of waiting out its epoll
    // timeout. Same fd is shared by the reactor (registered in epoll) and the
    // store worker (write side via the notify closure).
    let wake_fd = match transport::sys::create_eventfd() {
        Ok(fd) => fd,
        Err(e) => {
            eprintln!("Failed to create eventfd: {e}");
            std::process::exit(1);
        }
    };
    let notify_fd: RawFd = wake_fd;
    let notify: storage::Notify =
        Some(Arc::new(move || transport::sys::wake_eventfd(notify_fd)));

    let durability = if config.durable { Durability::Immediate } else { Durability::Eventual };

    let store = match Store::open(&config.data_path, notify, durability) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("Failed to open store: {}", e);
            std::process::exit(1);
        }
    };

    let (listener_fd, addr) = match create_listener(&config.bind) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("Failed to bind: {}", e);
            std::process::exit(1);
        }
    };
    eprintln!("msgd listening on {} (data: {}, durability: {})", addr, config.data_path, if config.durable { "durable" } else { "fast" });

    let domain = Domain::new(store);

    let mut reactor: Reactor<Domain> = match Reactor::new(domain, Some(listener_fd), Some(wake_fd)) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("Failed to create reactor: {}", e);
            std::process::exit(1);
        }
    };

    eprintln!("msgd ready.");
    if let Err(e) = reactor.run() {
        eprintln!("msgd error: {}", e);
    }
    eprintln!("msgd shut down.");
}
