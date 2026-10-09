//! Cycle-level cost breakdown of the message-send hot path.
//!
//! Run with:
//!     cargo bench -p server --bench send_cycles
//!
//! Three harnesses, because no single one can measure the whole path:
//!
//!   1. `pure`    — CPU-only protocol stages, timed in TSC cycles. Sub-100-cycle
//!                  work needs batched timing (see `batched` below).
//!   2. `store`   — the async storage hop in isolation, wall-clock (it crosses a
//!                  thread boundary, so a cycle delta across it is meaningless).
//!   3. `wire`    — full end-to-end send against a real msgd over loopback TCP.
//!
//! ## Reading the numbers
//!
//! The TSC is calibrated against `Instant` at startup; report results as cycles
//! *and* microseconds. Absolute disk latencies are machine-specific and noisy on
//! a shared/laptop host — the ratios and the pure-CPU cycle counts are the
//! portable result.
//!
//! ## Methodology notes (these matter; each was a real bug first)
//!
//! * `_rdtsc` is NOT a compiler barrier. LLVM models it as may-read-memory, so
//!   pure work can be hoisted across it. Each measured region is fenced with an
//!   `asm!` barrier and the result is forced to memory with `black_box`.
//! * Single-shot `rdtsc` pairs cost ~60 cycles of overhead on this host, so
//!   anything "instant" below that is measuring the timer. `batched()` times N
//!   ops inside one pair and divides, which resolves ~1 cycle.
//! * Batched loops must VARY THE INPUT. With a constant input LLVM hoists the
//!   whole computation out of the inner loop and reports a fantasy ~1.36 cyc/op.
//!   Every closure here takes a `u32` and perturbs the buffer with it.
//! * `write()` must never be hoisted either, and is measured with a real socket
//!   rather than a buffer, because the kernel entry is the point.
//!
//! ## Caveat
//!
//! This host is a Zen 3 (Ryzen 7 5825U), whose CRC32 has ~3-cycle latency and
//! ~0.5/cycle throughput. That is favourable: table-driven CRC is relatively
//! cheaper here than on older Intel, and much cheaper than on ARM without a CRC
//! extension. Re-run on target hardware before acting on the CRC numbers.

use std::arch::asm;
use std::arch::x86_64::{_mm_lfence, _rdtsc, __rdtscp};
use protocol::Payload;
use std::time::{Duration, Instant};

// ---------- TSC helpers ----------

/// Compiler barrier with a memory clobber. Without this, LLVM may hoist the
/// measured work across `_rdtsc` and the region measures nothing.
#[inline(always)]
fn barrier() {
    unsafe { asm!("", options(nostack, preserves_flags)) }
}

/// Serialized TSC read: LFENCE; RDTSC.
#[inline(always)]
fn ts() -> u64 {
    unsafe {
        _mm_lfence();
        _rdtsc()
    }
}

/// Serialized TSC read: RDTSCP; LFENCE.
#[inline(always)]
fn te() -> u64 {
    unsafe {
        let mut aux = 0u32;
        __rdtscp(&mut aux);
        _mm_lfence();
        _rdtsc()
    }
}

/// Calibrate TSC ticks per wall-clock second. Cycle counts are meaningless
/// without this — the TSC is not the core clock.
fn calibrate() -> f64 {
    let w0 = Instant::now();
    let t0 = ts();
    std::thread::sleep(Duration::from_millis(200));
    let t1 = te();
    (t1 - t0) as f64 / w0.elapsed().as_secs_f64()
}

/// Per-operation cycles for work too small for a single-shot measurement:
/// time `batch` ops inside one TSC pair and divide.
fn batched(name: &str, batch: u32, reps: u32, mut f: impl FnMut(u32)) {
    for i in 0..5000u32 {
        f(i);
    }
    let mut per = Vec::with_capacity(reps as usize);
    for r in 0..reps {
        barrier();
        let a = ts();
        for i in 0..batch {
            f(r.wrapping_add(i));
        }
        barrier();
        let b = te();
        per.push((b - a) as f64 / batch as f64);
    }
    per.sort_by(|x, y| x.partial_cmp(y).unwrap());
    println!(
        "  {name:<48} {:>8.1} cyc/op   best {:>8.1}",
        per[per.len() / 2],
        per[0]
    );
}

/// Percentiles for wall-clock samples, in microseconds.
fn report_us(name: &str, mut v: Vec<f64>, ghz: f64) {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let n = v.len();
    let mean = v.iter().sum::<f64>() / n as f64;
    println!("  {name}");
    println!(
        "      mean {mean:>10.2} us   p50 {:>10.2}   p99 {:>10.2}   (p50 = {:>9.0} cyc)",
        v[n / 2],
        v[(n as f64 * 0.99) as usize % n],
        v[n / 2] / 1e6 * ghz
    );
}

// ---------- hardware CRC32C (for comparison only) ----------

/// SSE4.2 `crc32` instruction. Uses `unsafe` + `#[target_feature]`; the
/// `protocol` crate itself is `#![forbid(unsafe_code)]`, so this lives in the
/// bench rather than in the library. Correctness is cross-checked below.
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse4.2")]
unsafe fn hw_crc32c(bytes: &[u8]) -> u32 {
    use std::arch::x86_64::{_mm_crc32_u64, _mm_crc32_u8};
    let mut crc: u64 = 0xFFFF_FFFF;
    let mut i = 0;
    while i + 8 <= bytes.len() {
        crc = _mm_crc32_u64(crc, u64::from_le_bytes(bytes[i..i + 8].try_into().unwrap()));
        i += 8;
    }
    let mut c = crc as u32;
    while i < bytes.len() {
        c = _mm_crc32_u8(c, bytes[i]);
        i += 1;
    }
    !c
}

#[cfg(not(target_arch = "x86_64"))]
unsafe fn hw_crc32c(_bytes: &[u8]) -> u32 {
    unimplemented!("hardware CRC bench is x86_64-only")
}

// ---------- 1. pure CPU stages ----------

fn pure() {
    println!("\n=== 1. pure CPU stages (12-byte body, 24-byte frame) ===\n");

    let (batch, reps) = (1000u32, 2000u32);
    let mut buf = [0u8; 64];
    let body: &[u8] = b"12345678\nhi\n";

    // Encode a frame with a real, varying payload.
    let mut frame = vec![0u8; protocol::frame_len(body.len())];
    protocol::seal(&mut frame, protocol::MsgType::Send, 0, body.len());
    let crc_len = protocol::HEADER_LEN + body.len() - 1;

    batched("header write only, stack buf (no CRC)", batch, reps, |i| {
        let b = &mut buf;
        b[0] = 0x4D;
        b[1] = 1;
        b[2..4].copy_from_slice(&(30u16 ^ (i as u16)).to_le_bytes());
        b[4..8].copy_from_slice(&(body.len() as u32).to_le_bytes());
        b[8..20].copy_from_slice(body);
        std::hint::black_box(&b[..24]);
    });

    batched("header + crc32c TABLE", batch, reps, |i| {
        let b = &mut buf;
        b[0] = 0x4D;
        b[1] = 1;
        b[2..4].copy_from_slice(&(30u16 ^ (i as u16)).to_le_bytes());
        b[4..8].copy_from_slice(&(body.len() as u32).to_le_bytes());
        b[8..20].copy_from_slice(body);
        let c = protocol::crc32c(std::hint::black_box(&b[1..crc_len]));
        b[20..24].copy_from_slice(&c.to_le_bytes());
        std::hint::black_box(c);
    });

    batched("header + crc32c SSE4.2", batch, reps, |i| unsafe {
        let b = &mut buf;
        b[0] = 0x4D;
        b[1] = 1;
        b[2..4].copy_from_slice(&(30u16 ^ (i as u16)).to_le_bytes());
        b[4..8].copy_from_slice(&(body.len() as u32).to_le_bytes());
        b[8..20].copy_from_slice(body);
        let c = hw_crc32c(std::hint::black_box(&b[1..crc_len]));
        b[20..24].copy_from_slice(&c.to_le_bytes());
        std::hint::black_box(c);
    });

    batched("SendReq::decode (inbound parse)", batch, reps, |i| {
        let mut v = body.to_vec();
        v[0] = b'0' + (i % 10) as u8;
        let d = protocol::SendReq::decode(std::hint::black_box(&v));
        std::hint::black_box(&d);
    });

    batched("Delivery::encode", batch, reps, |i| {
        let mut v = body.to_vec();
        v[0] = b'0' + (i % 10) as u8;
        let d = protocol::Delivery {
            conv: v[..8].to_vec(),
            seq: (i as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15),
            sender: b"alice".to_vec(),
            text: b"hi".to_vec(),
        }
        .encode();
        std::hint::black_box(&d);
    });

    // What the server actually does per outbound message: one alloc, one body
    // copy, one header+CRC, shared via Arc across all recipients.
    batched("seal_shared: Arc alloc + copy + TABLE CRC", batch, reps, |i| {
        let mut v = body.to_vec();
        v[0] = b'0' + (i % 10) as u8;
        let total = protocol::frame_len(v.len());
        let mut b2 = vec![0u8; total].into_boxed_slice();
        b2[protocol::HEADER_LEN..protocol::HEADER_LEN + v.len()].copy_from_slice(&v);
        protocol::seal(&mut b2, protocol::MsgType::Send, 0, v.len());
        let arc: std::sync::Arc<[u8]> = std::sync::Arc::from(b2);
        std::hint::black_box(&arc);
    });

    // Upper bound on what removing the CRC entirely would buy.
    batched("seal_shared WITHOUT crc (alloc+copy+header)", batch, reps, |i| {
        let mut v = body.to_vec();
        v[0] = b'0' + (i % 10) as u8;
        let total = protocol::frame_len(v.len());
        let mut b2 = vec![0u8; total].into_boxed_slice();
        b2[0] = 0x4D;
        b2[1] = 1;
        b2[2..4].copy_from_slice(&30u16.to_le_bytes());
        b2[4..8].copy_from_slice(&(v.len() as u32).to_le_bytes());
        b2[protocol::HEADER_LEN..protocol::HEADER_LEN + v.len()].copy_from_slice(&v);
        let arc: std::sync::Arc<[u8]> = std::sync::Arc::from(b2);
        std::hint::black_box(&arc);
    });

    // CRC scaling — where the table version actually hurts.
    println!("\n  crc32c cost vs payload size");
    println!("      {:>8}  {:>12}  {:>14}  {:>9}", "bytes", "table (cyc)", "sse4.2 (cyc)", "speedup");
    for size in [64usize, 256, 1024, 4096, 16384] {
        let mut b = vec![0xABu8; size];
        let mut t = Vec::new();
        for r in 0..300u32 {
            b[0] = (r % 251) as u8;
            barrier();
            let a = ts();
            for _ in 0..200 {
                b[0] = b[0].wrapping_add(1);
                std::hint::black_box(protocol::crc32c(std::hint::black_box(&b)));
            }
            barrier();
            t.push((te() - a) as f64 / 200.0);
        }
        t.sort_by(|x, y| x.partial_cmp(y).unwrap());

        let mut h = Vec::new();
        for r in 0..300u32 {
            b[0] = (r % 251) as u8;
            barrier();
            let a = ts();
            for _ in 0..200 {
                unsafe {
                    b[0] = b[0].wrapping_add(1);
                    std::hint::black_box(hw_crc32c(std::hint::black_box(&b)));
                }
            }
            barrier();
            h.push((te() - a) as f64 / 200.0);
        }
        h.sort_by(|x, y| x.partial_cmp(y).unwrap());

        let (tb, hb) = (t[t.len() / 2], h[h.len() / 2]);
        println!(
            "      {size:>8}  {tb:>12.0}  {hb:>14.0}  {:>8.1}x",
            tb / hb
        );
    }

    // The hardware path is only a drop-in if it agrees bit-for-bit.
    let ramp: Vec<u8> = (0..=255u8).collect();
    let a = protocol::crc32c(&ramp);
    let b = unsafe { hw_crc32c(&ramp) };
    println!("\n  correctness: table={a:#010x} sse4.2={b:#010x} match={}", a == b);
    let _ = frame;
}

// ---------- 2. the async storage hop ----------

fn store(dir: &str, durable: bool) {
    use storage::{Durability, Store};

    let path = format!("{dir}/msgd_bench_store_{}.redb", if durable { "dur" } else { "fast" });
    let _ = std::fs::remove_file(&path);
    let store = Store::open(
        &path,
        None,
        if durable { Durability::Immediate } else { Durability::Eventual },
    )
    .unwrap();
    let conv = vec![7u8; 8];

    let mut rt = Vec::new();
    for _ in 0..5000 {
        let t = Instant::now();
        let rx = store.next_sequence_async(&conv).unwrap();
        std::hint::black_box(rx.recv().unwrap());
        rt.push(t.elapsed().as_secs_f64() * 1e6);
    }
    report_us(
        &format!("next_sequence ({})", if durable { "Durability::Immediate, fsync" } else { "Durability::Eventual, no fsync" }),
        rt,
        1.9963e9,
    );

    let mut enq = Vec::new();
    for _ in 0..5000 {
        let t = ts();
        let rx = store.next_sequence_async(&conv).unwrap();
        enq.push(te() - t);
        std::hint::black_box(rx);
    }
    enq.sort_unstable();
    println!(
        "      of which, enqueue onto the mpsc (synchronous): p50 {} cyc",
        enq[enq.len() / 2]
    );
    std::thread::sleep(Duration::from_millis(200));
    let _ = std::fs::remove_file(&path);
}

// ---------- 3. end-to-end over a real socket ----------

mod wire {
    use super::*;
    use protocol::{Decode, HelloReq, MsgType, Payload};
    use std::io::{Read, Write};
    use std::net::{TcpStream, SocketAddr};
    use std::process::{Child, Command};

    struct Srv(Child, SocketAddr);
    impl Drop for Srv {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    fn spawn(durable: bool) -> Srv {
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let bind = format!("127.0.0.1:{port}");
        let data = format!("/tmp/msgd_bench_wire_{port}.redb");
        let _ = std::fs::remove_file(&data);
        let child = Command::new(format!("{}/../target/release/msgd", env!("CARGO_MANIFEST_DIR")))
            .arg("--bind")
            .arg(&bind)
            .arg("--data")
            .arg(&data)
            .arg(if durable { "--durable" } else { "--fast" })
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn msgd (run `cargo build --release` first)");
        std::thread::sleep(Duration::from_millis(400));
        Srv(child, bind.parse().unwrap())
    }

    /// Read frames until `want` arrives. Decodes only `buf[..off]` — the tail
    /// of `buf` is uninitialized zeros, which `decode` reports as a bad magic.
    fn await_frame(s: &mut TcpStream, buf: &mut [u8], off: &mut usize, want: MsgType) -> Vec<u8> {
        loop {
            match protocol::decode(&buf[..*off]) {
                Decode::Complete { frame, consumed } => {
                    let body = frame.body.to_vec();
                    let mt = frame.msg_type;
                    let old = *off;
                    buf.copy_within(consumed..old, 0);
                    *off = old - consumed;
                    if mt == want {
                        return body;
                    }
                }
                Decode::Need => {
                    let n = s.read(&mut buf[*off..]).unwrap();
                    if n == 0 {
                        panic!("server closed the connection");
                    }
                    *off += n;
                }
                Decode::Err(e) => panic!("decode error: {e}"),
            }
        }
    }

    pub fn run(ghz: f64) {
        for durable in [false, true] {
            let srv = spawn(durable);
            let mut s = TcpStream::connect(srv.1).unwrap();
            s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            s.set_nodelay(true).unwrap();

            let hello = HelloReq { user: b"alice".to_vec(), password: b"pw".to_vec() }.encode();
            s.write_all(&protocol::encode(MsgType::Hello, 0, &hello)).unwrap();
            let mut buf = vec![0u8; 256 * 1024];
            let mut off = 0usize;
            await_frame(&mut s, &mut buf, &mut off, MsgType::AuthOk);

            s.write_all(&protocol::encode(MsgType::CreateConv, 0, b"alice,bob")).unwrap();
            let created = await_frame(&mut s, &mut buf, &mut off, MsgType::ConvCreated);
            // `<8-byte id><members>\n` — the id is the FIRST 8 BYTES only.
            let conv_id = created[..8].to_vec();

            let mut body = conv_id.clone();
            body.push(b'\n');
            body.extend_from_slice(&vec![b'y'; 200]);
            let send = protocol::encode(MsgType::Send, 0, &body);
            let flen = send.len();

            // Control frame: same connection, same syscall path, but no storage
            // hop and no fanout. send - ping isolates what Send actually costs.
            let ping = protocol::encode(MsgType::Ping, 0, &[]);
            for _ in 0..1000 {
                s.write_all(&ping).unwrap();
                await_frame(&mut s, &mut buf, &mut off, MsgType::Pong);
            }
            let mut pv = Vec::new();
            for _ in 0..10_000 {
                let t = Instant::now();
                s.write_all(&ping).unwrap();
                await_frame(&mut s, &mut buf, &mut off, MsgType::Pong);
                pv.push(t.elapsed().as_secs_f64() * 1e6);
            }

            for _ in 0..1000 {
                s.write_all(&send).unwrap();
                await_frame(&mut s, &mut buf, &mut off, MsgType::Delivered);
            }
            let mut sv = Vec::new();
            for _ in 0..10_000 {
                let t = Instant::now();
                s.write_all(&send).unwrap();
                await_frame(&mut s, &mut buf, &mut off, MsgType::Delivered);
                sv.push(t.elapsed().as_secs_f64() * 1e6);
            }

            pv.sort_by(|a, b| a.partial_cmp(b).unwrap());
            sv.sort_by(|a, b| a.partial_cmp(b).unwrap());
            let pmean = pv.iter().sum::<f64>() / pv.len() as f64;
            let smean = sv.iter().sum::<f64>() / sv.len() as f64;
            println!("\n  {}  (frame {flen} B, 10k iterations, one warm connection)", if durable { "--durable" } else { "--fast" });
            println!("      ping (control)   p50 {:>9.2} us  -> {:>9.0} cyc", pv[pv.len() / 2], pv[pv.len() / 2] / 1e6 * ghz);
            println!("      send             p50 {:>9.2} us  -> {:>9.0} cyc", sv[sv.len() / 2], sv[sv.len() / 2] / 1e6 * ghz);
            println!("      delta (send-ping)      {:>9.2} us  -> {:>9.0} cyc", smean - pmean, (smean - pmean) / 1e6 * ghz);
            println!("      send p99          {:>9.2} us  (connection setup excluded)", sv[(sv.len() as f64 * 0.99) as usize]);
            drop(srv);
        }
    }
}

fn main() {
    let ghz = calibrate();
    println!("send_cycles — TSC {ghz:.6e} Hz ({:.4} GHz), 1 cycle = {:.4} ns", ghz / 1e9, 1e9 / ghz);
    println!("arch: {}", std::env::consts::ARCH);

    pure();

    println!("\n=== 2. async storage hop (isolated) ===\n");
    let data_dir = std::env::var("MSGD_BENCH_DATA_DIR").unwrap_or_else(|_| "/tmp".into());
    println!("  data dir: {data_dir}");
    println!("  NOTE: if that dir is tmpfs, fsync is nearly free and --fast and");
    println!("  --durable will look identical. Point MSGD_BENCH_DATA_DIR at real storage.");
    store(&data_dir, false);
    store(&data_dir, true);

    println!("\n=== 3. end-to-end send over loopback TCP ===\n");
    println!("  requires `cargo build --release` (spawns target/release/msgd)");
    wire::run(ghz);

    println!("\nDone. Absolute disk numbers are machine-specific; the pure-CPU cycle");
    println!("counts and the send-vs-ping ratio are the portable results.");
}
