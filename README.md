# msgd — Raw-Socket Messaging Service

A high-performance, dependency-light messaging server and client written in Rust.

No tokio, no async-std, no messaging middleware. The hot path (wire framing and
the I/O loop) is pure `std`/`libc`: a single-threaded, edge-triggered epoll
reactor with per-connection backpressure, backed by the embedded `redb` store
offloaded to a background thread.

```
[client] ──TCP──> msgd ──epoll reactor──> domain (auth, convs, delivery)
                          │                    │
                          │               redb store (bg thread)
```

## Features

- **Binary wire protocol** (`protocol`) — CRC32C-framed frames, magic-byte
  rejection, 16 MiB body cap, flags for compression/ack. See
  [PROTOCOL.md](PROTOCOL.md).
- **Fast transport** (`transport`) — single-threaded edge-triggered epoll
  reactor, per-connection growable buffers (no memmove on partial writes),
  unified teardown path, three-stage backpressure.
- **Accounts** (`domain`) — single-message handshake with a user-chosen
  password; accounts are auto-created on first connect, and reconnects verify
  the stored salted SHA-256 credential.
- **Conversations** — deterministic conv IDs from the sorted member list;
  per-conversation monotonic sequence numbers.
- **Store-and-forward inbox** — messages for offline users persist to `redb`
  and are delivered on reconnect (at-most-once).
- **Online delivery** — messages reach connected members in real time.
- **Keepalive** — `Ping`/`Pong` for NAT/firewall traversal.
- **Two binaries** — `msgd` (server) and `msgtui` (the interactive chat TUI).
- **Benchmarks** (`server/benches/`) — criterion round-trip benches plus a
  cycle-level breakdown of the send hot path. See [Benchmarks](#benchmarks).

## Quickstart

Requires a stable Rust toolchain (workspace `edition = "2024"`).

On NixOS the C toolchain lives in the Nix store and is only on `PATH` inside an
interactive shell, so `.cargo/config.toml` names the gcc wrapper as the linker.
This keeps `cargo build` working from IDEs, containers and CI, which otherwise
fail with `error: linker 'cc' not found`. Update that `linker` path if your Nix
hash differs.

```sh
cargo build --release
# run the server
target/release/msgd --bind 0.0.0.0:9723 --data /tmp/msgd.redb
```

In another terminal:

```sh
# interactive chat TUI
target/release/msgtui --host 127.0.0.1 --port 9723 --user alice --password hunter2
```

The first time a user connects, the server creates the account; the password
is chosen by the user (pass `--user`/`--password` to `msgtui`, or log in from
within the TUI). Subsequent logins with the same user id must supply the same
password.

```
auth ok (new account created)
created conversation: 8ee70d5b0871a75a (members: alice,bob)
delivered seq=1
```

(Inside the TUI, the same flow is `/create bob`, then typing a line.)

## Binaries

### `msgd` — the server

```
Usage: msgd [--bind ADDR] [--data PATH] [--fast|--durable]
```

| Option | Default | Description |
|---|---|---|
| `--bind` | `0.0.0.0:9723` | Listen address (IPv4). Uses `SO_REUSEPORT`. |
| `--data` | `/tmp/msgd.redb` | Path to the `redb` store file. |
| `--fast` | yes (default) | Skip the per-write `fsync` (`Durability::Eventual`) — lowest latency. |
| `--durable` | no | `fsync` every write (`Durability::Immediate`) — sequence numbers survive a crash. |
| `--help` / `-h` | — | Show usage and exit. |

The server logs to stderr. Shut down with SIGINT/SIGTERM.

### `msgtui` — the chat TUI

```
Usage: msgtui [--host HOST] [--port PORT] [--user USER] [--password PASSWORD] [--conv HEX]
```

| Option | Default | Description |
|---|---|---|
| `--host` | `127.0.0.1` | Server address. |
| `--port` | `9723` | Server port. |
| `--user` | *(empty)* | User id to authenticate as (empty = log in from within the TUI). |
| `--password` | *(empty)* | Password chosen by the user (creates the account on first use). |
| `--conv` | *(none)* | Conversation id (hex) to pre-select. |
| `--help` / `-h` | — | Show usage and exit. |

Interactive ratatui TUI (see [`msgtui`](#rust-crate-api)). Slash
commands inside the input box: `/login`, `/logout`, `/create a,b`, `/new`,
`/list`, `/filter`, `/ping`, `/quit`; typing a plain line sends it into the
selected conversation. Conversations are listed by member name (`alice, bob`)
rather than opaque ids, with unread badges and a message preview. `Tab` cycles
focus between the conversation list, transcript and input box; `↑`/`↓` move,
`Enter` opens, `PageUp`/`PageDown` scroll, `Esc` or `/quit` exits.

The `msgtui` binary is a launcher for the reusable `msgtui` crate: it connects
through the channel-based `msgclient` and adapts it to the TUI's `ChatClient`
trait.


## Wire-Protocol API (message types)

All application traffic is framed (see [PROTOCOL.md](PROTOCOL.md)). The
following types are **implemented** by the server today:

| ID | Type | Direction | Purpose | Body |
|---|---|---|---|---|
| 1 | `Hello` | C→S | Start auth; new accounts are auto-created | `<user_id>\n<password>` |
| 4 | `AuthOk` | S→C | Auth success; 1-byte flag (1=new, 0=existing) | 1 byte |
| 5 | `AuthFail` | S→C | Auth failure (connection stays open) | UTF-8 error string |
| 6 | `Goodbye` | both | Clean teardown | empty |
| 10 | `Presence` | both | Pass-through echo (reserved for presence) | opaque |
| 20 | `CreateConv` | C→S | Create a conversation (≥ 2 members) | comma-separated ids |
| 21 | `ConvCreated` | S→C | The new conversation | `<8-byte id><members>\n` record |
| 30 | `Send` | C→S | Send a message | `<conv_id>\n<message_body>` |
| 30 | `Send` | S→C | Deliver a message (inbox push) | `<conv_id>\n<seq:8le>\n<sender>\n<body>` |
| 31 | `Delivered` | S→C | Ack: message persisted and queued | `<seq:8le>` |
| 37 | `ListConvs` | C→S | List the user's conversations | empty |
| 38 | `ConvsResp` | S→C | The user's conversations, with member names | concatenated `<8-byte id><members>\n` records |
| 90 | `Ping` | both | Keepalive | empty |
| 91 | `Pong` | both | Ping reply | empty |
| 99 | `Error` | S→C | Application error; connection stays open | UTF-8 string |

### Auth flow

Single round trip: `Hello(user_id\npassword)`. A new account is auto-created
(reply `AuthOk` flag `1`); an existing account verifies the stored credential
(reply `AuthOk` flag `0`, or `AuthFail`). After `AuthOk` the server
immediately pushes any stored inbox `Send` frames.

### Delivery semantics

- A sender's `Send` is persisted, assigned a per-conversation monotonic
  sequence number, then acked with `Delivered(seq)`.
- Connected members receive the message in real time as a `Send` frame.
- Offline members receive it on their next successful auth (store-and-forward,
  at-most-once delivery).
- Conversation membership is not enforced at send time; any member of the
  stored member list receives delivery.

### Backpressure

| Outbound buffer | Behaviour |
|---|---|
| `< 256 KiB` | Normal read + write |
| `256 KiB – 1 MiB` | Inbound reads paused, drain catches up |
| `≥ 1 MiB` | Hard cap — new frames dropped, connection torn down |

Clients must consume inbound frames promptly.

## Rust Crate API

Workspace crates: `protocol`, `transport`, `storage`, `domain`, `server`,
`client` (msgclient), `tui` (msgtui), `chat-model`. Boundaries mirror
separation of concerns — the wire format has no I/O, transport has no
application logic, domain has no socket knowledge, `msgtui` has no wire
knowledge.

- **`protocol` — wire format.** No dependencies, no I/O, `#![forbid(unsafe_code)]`.
  `MAGIC = 0x4D`, `VERSION = 1`, 8-byte header, 4-byte CRC32C trailer,
  `MAX_BODY_LEN = 16 MiB`. `MsgType` is generated from a single `Name = id`
  registry table (`msg_types!`), so the enum and its numeric mapping can never
  desync. Decode via `decode(&[u8]) -> Decode<'_>` (`Complete`/`Need`/`Err`,
  non-panicking); encode via `encode` / `encode_into`, or `seal()` to fill
  header+CRC in a pre-sized buffer in a single allocation (the server hot
  path). Each message body is one `Payload` struct, so each wire format lives
  in exactly one place.
- **`transport` — I/O reactor.** `EventHandler` is the only channel from
  handler to I/O (`on_accept`, `on_frame`, `on_teardown`, `tick`,
  `drain_outbound`, `drain_teardowns`). `Reactor<H>` is a single-threaded,
  edge-triggered epoll loop. `Connection` owns per-connection buffers with
  three-stage backpressure; `sys` wraps the libc/epoll calls.
- **`storage` — persistent store.** `Store` is `Clone + Send + Sync`; every
  operation is offloaded to a single background writer thread over `mpsc` and
  returns a `Receiver<StoreResult>` immediately, so the reactor never blocks.
  Tables: accounts, conversations, per-conversation sequence counters, inbox.
- **`domain` — application logic.** The `EventHandler` implementation: auth,
  conversation create, message send with store-and-forward delivery,
  conversation listing, `Ping`/`Pong`, `Presence`. Derives conversation ids
  from the sorted member list and stores salted SHA-256 credentials
  (`salt ‖ sha256(salt ‖ password)`) with the salt from `/dev/urandom` — a KDF
  with a work factor (bcrypt/argon2) is recommended for production.
- **`msgtui` — the TUI frontend.** A reusable ratatui chat frontend driving a
  generic `ChatClient` trait; `Action`/`Event` live in `chat-model`. Split into
  `state` (model, event folding, input) and `render` (drawing), both
  unit-tested — render tests use ratatui's `TestBackend`, so layout
  regressions are caught without a terminal. Conversations are keyed by id in
  a hash map, making inbound lookup O(1) regardless of how many
  conversations the user belongs to; transcripts and composer history are
  capped so a long session cannot grow without limit.

## Configuration & Operation

- Listen socket uses `SO_REUSEPORT` (multi-shard ready) and `SO_REUSEADDR`;
  listener backlog is 128; connections are non-blocking.
- Data is stored in a single `redb` file; `redb` uses ACID transactions and
  no `unsafe`.

## Adding a message type

The plumbing for each message is centralized in single registries, so an
addition is a mechanical diff across a few sites. Using `ReadReceipt` as an
example:

| # | File | Change |
|---|---|---|
| 1 | `protocol/src/lib.rs` | Register the wire id in the `msg_types!` table (`ReadReceipt = 25,`). The enum, `from_u16`/`as_u16` and `ALL` are all generated from it. |
| 2 | `protocol/src/lib.rs` | Define the body once as a `Payload` struct implementing `encode`/`decode`. Empty or opaque bodies reuse `Empty`/`Opaque`. |
| 3 | `chat-model/src/lib.rs` | Add `Action::ReadReceipt` (C→S) and/or `Event::ReadReceipt` (S→C). The model stays wire-free. |
| 4 | `client/src/msgs/` | Add a bridge module (`TYPE` + `decode` → `Event`, `encode(Action)`), then add its name to the matching `messages!` list in `mod.rs`, which generates `encode_action`/`decode_event`. |
| 5 | `domain/src/lib.rs` | Add a handler method that decodes the inbound `Payload` and updates state, then add a `MsgType => handler` row to `server_msgs!` (generates `on_frame`). Handlers with an async store hop also get a `PendingKind` arm. |
| 6 | `tui/src/lib.rs` | Handle the new `Event` in `apply_event` if it is user-visible. |
| 7 | `PROTOCOL.md` + this README | Add a row to the message table describing the body layout. |
| 8 | `protocol/src/tests.rs` | Add a `Payload` round-trip case. The registry/duplicate-id tests run automatically against `MsgType::ALL`. |

Steps 4–5 are the ones that can silently do nothing if you forget a row, so
the type checks there plus the step-8 tests are what guarantee completeness.

## Tests

```sh
cargo test
```

Integration tests (`server/tests/integration.rs`) spin up the real server over
real sockets and cover auth (new + reconnect), conversation create + send, and
ping/pong. Client tests (`client/tests/`) exercise the `msgclient` channel API
against a live server (connect, idle, latency).

## Benchmarks

```sh
cargo build --release                             # the wire bench spawns target/release/msgd
cargo bench -p server --bench echo                # criterion round-trips
cargo bench -p server --bench send_cycles         # cycle-level breakdown of the send path

# storage numbers are only meaningful on real disk — /tmp is tmpfs on many
# systems, which makes fsync look free:
MSGD_BENCH_DATA_DIR=/var/tmp cargo bench -p server --bench send_cycles
```

### Methodology

No single harness can measure the whole send path, so `send_cycles` has three
parts: pure-CPU stages in TSC cycles, the storage hop in isolation, and full
end-to-end over loopback TCP. The TSC is calibrated against `Instant` at
startup, because the TSC is not the core clock and raw counts are otherwise
meaningless.

Three things that will silently corrupt these numbers if you skip them:

1. **`_rdtsc` is not a compiler barrier.** LLVM models it as may-read-memory,
   so pure work gets hoisted across it and the measured region measures
   nothing. Every region is fenced with an `asm!` barrier and its result forced
   to memory.
2. **Single-shot `rdtsc` pairs cost ~60 cycles of overhead** on the reference
   host, so anything "instant" below that is measuring the timer. Sub-100-cycle
   work is timed in batches: N operations inside one TSC pair, divided.
3. **Batched loops must vary their input.** With a constant input LLVM hoists
   the whole computation out of the inner loop and reports a fantasy ~1.36
   cyc/op. Every measured closure takes a counter and perturbs its buffer with
   it.

One more trap worth stating: **`/tmp` is frequently tmpfs.** There, `fsync` costs
almost nothing and `--fast` and `--durable` measure identically. On the
reference host that difference was 18 µs on tmpfs versus 333 µs vs 439 µs on
ext4 — a ~20× swing from the filesystem alone.

### Results

Reference host: AMD Ryzen 7 5825U (Zen 3, 8c/16t), TSC 1.9963 GHz
(1 cycle = 0.5009 ns). Treat absolute disk figures as machine-specific; the
pure-CPU cycle counts and the send-vs-ping ratio are the portable parts.

Pure CPU, 12-byte body / 24-byte frame:

| Stage | cycles/op |
|---|---|
| Header write only, stack buffer (no CRC) | 1.7 |
| `seal_shared` without CRC (alloc + copy + header) | 34 |
| `SendReq::decode` (inbound parse) | 43 |
| `Delivery::encode` | 70 |
| **`seal_shared` — Arc alloc + copy + table CRC** | **79** |
| header + `crc32c` 19 B, table | 36 |
| header + `crc32c` 19 B, SSE4.2 | 15 |

`crc32c` is byte-at-a-time table lookup; this CPU has a hardware CRC32C
instruction, which is bit-identical (verified: `0x9c44184b` both ways on a
256-byte ramp):

| bytes | table | SSE4.2 | speedup |
|---|---|---|---|
| 64 | 189 | 22 | 8.4× |
| 256 | 885 | 58 | 15.4× |
| 1024 | 3,592 | 184 | 19.5× |
| 4096 | 14,499 | 690 | 21.0× |
| 16384 | 57,836 | 2,714 | 21.3× |

Storage hop and end-to-end, on ext4 (`MSGD_BENCH_DATA_DIR` on real storage):

| Component | p50 | cycles |
|---|---|---|
| storage hop, `--fast` (`Eventual`) | 333 µs | ~666,000 |
| storage hop, `--durable` (fsync) | 439 µs | ~877,000 |
| `enqueue` onto the mpsc (synchronous part) | 0.11 µs | ~220 |
| `Ping` round trip (control, same connection) | 17.0 µs | ~34,000 |
| `Send` round trip, end-to-end | 39.5 µs | ~79,000 |
| **`Send` − `Ping`** | **26.6 µs** | **~53,000** |

On tmpfs the same code measures 18 µs for the storage hop and ~39 µs end-to-end.

### What the numbers say

A send is **~79,000 cycles end-to-end but only ~79 cycles of CPU** — about 99.9%
of it is I/O wait. The cost is dominated by opening and committing a `redb`
write transaction for the per-message sequence number; `fsync` adds ~26% on top
of that. Optimising the CRC (a ~20× win on the checksum itself) would move the
end-to-end time by far less than a rounding error.

In priority order:

1. **Block-allocate sequence numbers** — allocate N per write transaction
   instead of one per message. This is the only change that matters by orders
   of magnitude, and it also shrinks how often `--durable` pays for `fsync`.
2. **Batch writes per `write()` syscall** if you ever chase throughput — one
   syscall is ~9,700 cycles on this host.
3. **Hardware CRC** — cheap to adopt and a real win at 4 KB+ payloads, but it
   needs an `unsafe` island (`protocol` is `#![forbid(unsafe_code)]`) and is
   worth ~20 cycles per small message, which is noise against a 39 µs send.

Note that Zen 3's CRC32 (3-cycle latency, ~0.5/cycle throughput) is favourable:
the table-driven penalty is *smaller* here than on older Intel, and much smaller
than on ARM without a CRC extension. Re-run before committing to hardware CRC.

### A bug this benchmarking found

`server/benches/echo.rs` used `frame.body.to_vec()` as the conversation id, but
a `ConvCreated` body is `<8-byte id><members>\n`. The server rejected the
bogus id with `conv_not_found`, and the bench then blocked on a read waiting
for a `Delivered` that never came — `cargo bench -p server --bench echo` failed
outright with a `WouldBlock` panic rather than reporting a number. Fixed to take
`frame.body[..8]`. If you write a new client, `ConvCreated` and `ConvsResp` are
the two places that carry a length-less record you have to know the layout of.

## Layout

```
protocol/    pure wire format: no I/O, no deps
transport/   epoll reactor, connection state machine, backpressure
  ├── sys.rs    safe libc/epoll wrappers
  ├── conn.rs   per-connection buffers, decoder integration, teardown
  └── reactor.rs event loop + EventHandler trait
domain/      accounts, conversations, inbox, auth (no socket knowledge)
storage/     redb-backed persistent store, background-thread offload
server/      msgd binary — wires everything together (benches/ under this)
client/      msgclient — channel-based client (send Actions, poll Events)
chat-model/  shared Action/Event types used by the client and TUI
tui/         msgtui — protocol-agnostic ratatui frontend + launcher bin
  ├── lib.rs      the ChatClient trait + app (no wire knowledge)
  └── bin/        msgtui binary: connects via msgclient, adapts it to the TUI
```

See [PROTOCOL.md](PROTOCOL.md) for the authoritative wire-format reference.
