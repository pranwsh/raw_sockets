//! msgd — high-performance raw-socket messaging server

use domain::Domain;
use storage::{Durability, Store};
use transport::dgram::DgramReactor;
use transport::Reactor;
use std::net::{SocketAddr, ToSocketAddrs};
use std::os::unix::io::RawFd;
use std::sync::Arc;

/// which network transport the server speaks
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Transport {
    /// TCP stream sockets: the default, and the only option that reaches
    /// another host
    Tcp,
    /// raw IP/UDP datagrams: needs `CAP_NET_RAW`, and is local-interface only
    RawIp,
}

impl Transport {
    fn parse(s: &str) -> Result<Transport, String> {
        match s {
            "tcp" => Ok(Transport::Tcp),
            "raw-ip" | "udp" => Ok(Transport::RawIp),
            other => Err(format!("unknown transport {other:?} (expected tcp or raw-ip)")),
        }
    }
}

struct Config {
    bind: String,
    data_path: String,
    durable: bool,
    transport: Transport,
}

fn parse_config() -> Config {
    let args: Vec<String> = std::env::args().collect();
    let mut c = Config {
        bind: "0.0.0.0:9723".into(),
        data_path: "/tmp/msgd.redb".into(),
        durable: false,
        transport: Transport::Tcp,
    };
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--bind" if i + 1 < args.len() => { c.bind = args[i + 1].clone(); i += 2; }
            "--data" if i + 1 < args.len() => { c.data_path = args[i + 1].clone(); i += 2; }
            // durable mode fsyncs every write (crash-safe sequence numbers); the
            // default fast mode skips the fsync on the message path
            "--durable" => { c.durable = true; i += 1; }
            "--fast" => { c.durable = false; i += 1; }
            "--transport" if i + 1 < args.len() => {
                match Transport::parse(&args[i + 1]) {
                    Ok(t) => c.transport = t,
                    Err(e) => { eprintln!("{e}"); std::process::exit(1); }
                }
                i += 2;
            }
            "--help" | "-h" => {
                eprintln!("Usage: msgd [--bind ADDR] [--data PATH] [--fast|--durable] [--transport tcp|raw-ip]");
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

    let durability_label = if config.durable { "durable" } else { "fast" };
    let domain = Domain::new(store);

    // The two transports differ only in their reactor: `Domain` is the
    // EventHandler for both, so all the application logic, storage and framing
    // are shared unchanged.
    match config.transport {
        Transport::Tcp => run_tcp(config, domain, wake_fd, durability_label),
        Transport::RawIp => run_raw_ip(config, domain, wake_fd, durability_label),
    }
}

/// serve over TCP stream sockets
fn run_tcp(config: Config, domain: Domain, wake_fd: RawFd, durability_label: &str) -> ! {
    let (listener_fd, addr) = match create_listener(&config.bind) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("Failed to bind: {}", e);
            std::process::exit(1);
        }
    };
    eprintln!(
        "msgd listening on {} (transport: tcp, data: {}, durability: {})",
        addr, config.data_path, durability_label
    );

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
    std::process::exit(0);
}

/// serve over raw IP/UDP datagrams
fn run_raw_ip(config: Config, domain: Domain, wake_fd: RawFd, durability_label: &str) -> ! {
    // Reuse the same `host:port` syntax as `--bind` so the two transports are
    // configured identically.
    let addr: SocketAddr = match config.bind.parse() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("Invalid --bind address {:?}: {e}", config.bind);
            std::process::exit(1);
        }
    };
    let SocketAddr::V4(a4) = addr else {
        eprintln!("raw-ip supports IPv4 only");
        std::process::exit(1);
    };

    let local = transport::packet::Endpoint {
        // network byte order, matching `sockaddr_in::sin_addr`
        addr: u32::from_be_bytes(a4.ip().octets()),
        port: a4.port(),
    };

    let mut reactor: DgramReactor<Domain> =
        match DgramReactor::new(domain, local, Some(wake_fd)) {
            Ok(r) => r,
            Err(e) => {
                // EPERM here is the common case: raw sockets need CAP_NET_RAW,
                // which an unprivileged container will not have.
                eprintln!("Failed to create raw IP socket: {e}");
                if e.kind() == std::io::ErrorKind::PermissionDenied {
                    eprintln!("raw-ip needs CAP_NET_RAW — run as root, or:");
                    eprintln!("  setcap cap_net_raw+ep {}", std::env::current_exe().unwrap_or_default().display());
                    eprintln!("or use --transport tcp, which needs no privileges.");
                }
                std::process::exit(1);
            }
        };

    eprintln!(
        "msgd listening on {} (transport: raw-ip, data: {}, durability: {})",
        addr, config.data_path, durability_label
    );
    eprintln!("note: raw-ip is local-interface only, and does not reassemble");
    eprintln!("      fragments on loopback, so payloads must fit one datagram there.");
    eprintln!("msgd ready.");
    if let Err(e) = reactor.run() {
        eprintln!("msgd error: {}", e);
    }
    eprintln!("msgd shut down.");
    std::process::exit(0);
}
