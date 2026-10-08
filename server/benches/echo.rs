//! benchmarks: throughput and latency of the messaging server

use criterion::{black_box, criterion_group, criterion_main, Criterion};
use std::io::{Read, Write};
use std::net::{TcpStream, SocketAddr};
use std::process::{Command, Child};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use protocol::{self, HelloReq, MsgType, Payload, Decode};

// helper: spawn a server instance

static SERVER_LOCK: Mutex<()> = Mutex::new(());

struct ServerProcess {
    child: Child,
    addr: SocketAddr,
    _lock: std::sync::MutexGuard<'static, ()>,
}

fn spawn_server() -> ServerProcess {
    let lock = SERVER_LOCK.lock().unwrap();
    let port = portpicker();
    let data_path = format!("/tmp/msgd_bench_{}", port);
    let bind = format!("127.0.0.1:{}", port);

    let child = Command::new(
        concat!(env!("CARGO_MANIFEST_DIR"), "/../target/release/msgd"),
    )
    .arg("--bind")
    .arg(&bind)
    .arg("--data")
    .arg(&data_path)
    .spawn()
    .expect("spawn msgd");

    std::thread::sleep(Duration::from_millis(500));

    let addr: SocketAddr = bind.parse().unwrap();
    ServerProcess { child, addr, _lock: lock }
}

impl Drop for ServerProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn portpicker() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.local_addr().unwrap().port()
}

// benchmarked operations

fn auth_round_trip(addr: SocketAddr) -> Duration {
    let mut stream = TcpStream::connect(addr).unwrap();
    stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let user_id = b"bench_user";
    let hello_body = HelloReq { user: user_id.to_vec(), password: b"benchpass".to_vec() }.encode();

    let hello_frame = protocol::encode(MsgType::Hello, 0, &hello_body);
    let start = Instant::now();
    stream.write_all(&hello_frame).unwrap();

    // read response (AuthOk)
    let mut buf = vec![0u8; 8192];
    let mut offset = 0;
    loop {
        match protocol::decode(&buf[..offset]) {
            Decode::Complete { .. } => {
                let elapsed = start.elapsed();
                // consume the rest of the frame
                return elapsed;
            }
            Decode::Need => {
                let n = stream.read(&mut buf[offset..]).unwrap();
                offset += n;
            }
            Decode::Err(_) => panic!("decode error"),
        }
    }
}

fn ping_round_trip(addr: SocketAddr) -> Duration {
    let mut stream = TcpStream::connect(addr).unwrap();
    stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let user_id = b"ping_bench";

    // authenticate first
    let hello_body = HelloReq { user: user_id.to_vec(), password: b"benchpass".to_vec() }.encode();
    let hello = protocol::encode(MsgType::Hello, 0, &hello_body);
    stream.write_all(&hello).unwrap();
    let mut buf = vec![0u8; 8192];
    let mut offset = 0;
    loop {
        match protocol::decode(&buf[..offset]) {
            Decode::Complete { consumed, .. } => {
                offset -= consumed;
                buf.copy_within(consumed.., 0);
                break;
            }
            Decode::Need => {
                let n = stream.read(&mut buf[offset..]).unwrap();
                offset += n;
            }
            Decode::Err(_) => panic!("decode error"),
        }
    }

    // measure ping-pong
    let ping = protocol::encode(MsgType::Ping, 0, &[]);
    let start = Instant::now();
    stream.write_all(&ping).unwrap();

    loop {
        match protocol::decode(&buf[..offset]) {
            Decode::Complete { .. } => {
                let elapsed = start.elapsed();
                return elapsed;
            }
            Decode::Need => {
                let n = stream.read(&mut buf[offset..]).unwrap();
                offset += n;
            }
            Decode::Err(_) => panic!("decode error"),
        }
    }
}

fn send_round_trip(addr: SocketAddr) -> Duration {
    let mut stream = TcpStream::connect(addr).unwrap();
    stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let user_id = b"send_bench";

    // authenticate
    let hello_body = HelloReq { user: user_id.to_vec(), password: b"benchpass".to_vec() }.encode();
    let hello = protocol::encode(MsgType::Hello, 0, &hello_body);
    stream.write_all(&hello).unwrap();
    let mut buf = vec![0u8; 8192];
    let mut offset = 0;
    loop {
        match protocol::decode(&buf[..offset]) {
            Decode::Complete { consumed, .. } => {
                offset -= consumed;
                buf.copy_within(consumed.., 0);
                break;
            }
            Decode::Need => {
                let n = stream.read(&mut buf[offset..]).unwrap();
                offset += n;
            }
            Decode::Err(_) => panic!("decode error"),
        }
    }

    // create a conversation with a second user so the send path is exercised (conversation lookup, sequence allocation, delivery)
    let conv_body = b"send_bench,recv_bench";
    stream.write_all(&protocol::encode(MsgType::CreateConv, 0, conv_body)).unwrap();
    let conv_id;
    loop {
        match protocol::decode(&buf[..offset]) {
            Decode::Complete { frame, consumed } => {
                if frame.msg_type == MsgType::ConvCreated {
                    conv_id = frame.body.to_vec();
                    offset -= consumed;
                    buf.copy_within(consumed.., 0);
                    break;
                }
                offset -= consumed;
                buf.copy_within(consumed.., 0);
            }
            Decode::Need => {
                let n = stream.read(&mut buf[offset..]).unwrap();
                offset += n;
            }
            Decode::Err(_) => panic!("decode error"),
        }
    }

    // measure send → delivered round-trip
    let mut msg = Vec::new();
    msg.extend_from_slice(&conv_id);
    msg.push(b'\n');
    msg.extend_from_slice(b"benchmark payload");
    let send_frame = protocol::encode(MsgType::Send, 0, &msg);
    let start = Instant::now();
    stream.write_all(&send_frame).unwrap();

    loop {
        match protocol::decode(&buf[..offset]) {
            Decode::Complete { frame, consumed } => {
                if frame.msg_type == MsgType::Delivered {
                    return start.elapsed();
                }
                offset -= consumed;
                buf.copy_within(consumed.., 0);
            }
            Decode::Need => {
                let n = stream.read(&mut buf[offset..]).unwrap();
                offset += n;
            }
            Decode::Err(_) => panic!("decode error"),
        }
    }
}

// criterion benches

fn bench_auth_latency(c: &mut Criterion) {
    let server = spawn_server();

    c.bench_function("auth_round_trip", |b| {
        b.iter(|| {
            black_box(auth_round_trip(server.addr));
        })
    });

    // server drops when server goes out of scope
    drop(server);
}

fn bench_ping_throughput(c: &mut Criterion) {
    let server = spawn_server();

    c.bench_function("ping_round_trip", |b| {
        b.iter(|| {
            black_box(ping_round_trip(server.addr));
        })
    });

    drop(server);
}

fn bench_send_throughput(c: &mut Criterion) {
    let server = spawn_server();

    c.bench_function("send_round_trip", |b| {
        b.iter(|| {
            black_box(send_round_trip(server.addr));
        })
    });

    drop(server);
}

criterion_group!(benches, bench_auth_latency, bench_ping_throughput, bench_send_throughput);
criterion_main!(benches);
