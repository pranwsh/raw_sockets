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
  unified teardown path, and three-stage backpressure.
- **Account + auth** (`domain`) — single-message handshake with a
  user-chosen password; new accounts are auto-created on first connect, and
  reconnects verify the stored salted SHA-256 credential.
- **Conversations** — deterministic conv IDs derived from the sorted member
  list; creation; per-conversation sequence numbers.
- **Store-and-forward inbox** — messages for offline users persist to redb and
  are delivered on reconnect (at-most-once: read then delete).
- **Online delivery** — messages reach connected members in real time.
- **Keepalive** — `Ping`/`Pong` for NAT/firewall traversal.
- **Two binaries** — `msgd` (server) and `msgtui` (the interactive chat TUI).
- **Benchmarks** (`server/benches/`) — criterion echo-throughput and latency benches.

## Quickstart

Requires a stable Rust toolchain (workspace `edition = "2024"`).

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

Interactive term_render TUI (see [`msgtui`](#msgtui--the-tui-frontend)). Slash
commands inside the input box: `/create a,b`, `/list`, `/ping`, `/quit`; typing a
plain line sends it into the selected conversation. Tab switches focus between
the conversation list and the input box; Esc or `/quit` exits.

`msgtui` is a launcher for the reusable `msgtui` crate: it connects through the
channel-based `msgclient` client and adapts it to the TUI's `ChatClient` trait.


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
| 21 | `ConvCreated` | S→C | Conv id for the new conversation | 8-byte u64 LE id |
| 30 | `Send` | C→S | Send a message | `<conv_id>\n<message_body>` |
| 30 | `Send` | S→C | Deliver a message (inbox push) | `<conv_id>\n<seq:8le>\n<sender>\n<body>` |
| 31 | `Delivered` | S→C | Ack: message persisted and queued | `<seq:8le>` |
| 37 | `ListConvs` | C→S | List the user's conversations | empty |
| 38 | `ConvsResp` | S→C | Conversation ids | concatenated 8-byte u64 LE ids |
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

Workspace: `protocol`, `transport`, `storage`, `domain`, `server`,
`client` (msgclient), `tui` (msgtui), `chat-model`. Crate
boundaries mirror separation of concerns — wire format has no I/O, transport
has no application logic, domain has no socket knowledge, `msgtui` has no wire
knowledge.

### `protocol` — wire format

Constants: `MAGIC = 0x4D`, `VERSION = 1`, `HEADER_LEN = 8`,
`TRAILER_LEN = 4`, `FRAME_OVERHEAD = 12`, `MAX_BODY_LEN = 16 MiB`, flags
`FLAG_COMPRESSED` / `FLAG_ACK_REQ` (bits 15/14 of the type+flags u16),
`TYPE_MASK = 0x1FFF`.

- `MsgType` — enum of all message types with `from_u16(u16) -> Option<Self>`.
- `Frame<'a>` — decoded frame: `version`, `flags`, `msg_type`, `body`.
- `OwnedFrame` — owned variant (`from_borrowed(&Frame)`, `to_owned`).
- `Decode<'a>` — result enum: `Complete { frame, consumed }`, `Need`, `Err`.
- `decode(&[u8]) -> Decode<'_>` — pure, non-panicking frame decoder.
- `encode_into` / `encode(msg_type, flags, body) -> Box<[u8]>` — frame encoder.
- `seal(buf, msg_type, flags, body_len)` — fill header+CRC in a pre-sized
  buffer (single-allocation delivery, used on the server hot path).
- `crc32c(&[u8]) -> u32` — Castagnoli checksum over `[ver .. body_end]`.
- Wire-body helpers: `hello_body`/`split_hello`, `send_body`/`split_send`,
  `delivery_body`/`split_delivery`, `write_u64_le`/`read_u64_le` — the
  canonical body encoders/decoders so clients and server never diverge.

### `transport` — I/O reactor

- `EventHandler` (trait) — `on_accept`, `on_frame`, `on_teardown`, `tick`,
  `drain_outbound`, `drain_teardowns`. The only channel from handler to I/O.
- `Reactor<H: EventHandler>` — `new(handler, listener_fd, wake_fd)`, `run()`.
  Single-threaded edge-triggered epoll loop.
- `ConnectionId(u64)` — per-connection handle.
- `Connection` — per-connection state: `new`, `do_read`, `try_decode_frame`,
  `enqueue_frame` → `WriteOutcome` (`Queued` / `Overflow` / `Rejected`),
  `do_write`, `is_write_empty`, `should_pause_reading`, `start_teardown`,
  `advance_teardown`, `close_immediately`.
- `TeardownReason` — `ClientGoodbye`, `ProtocolViolation`, `PeerClosed`,
  `SlowClient`, `Shutdown`.
- `sys` — safe wrappers over libc: `set_nonblocking`, `set_reuseport`,
  `set_reuseaddr`, `set_tcp_nodelay`, `bind_v4`, `listen`, `accept`, `read`,
  `write`, `shutdown`, `close`, `epoll_create/add/mod/del/wait`,
  `create_eventfd`/`wake_eventfd`/`drain_eventfd`, `now_ms`.

### `storage` — persistent store

`Store` is `Clone + Send + Sync`; every operation is offloaded to a background
single-writer thread via `mpsc` and returned on a one-shot channel. The API is
async (non-blocking, used by the event loop):
`put_account_async`, `get_account_async`, `put_conversation_async`,
`get_conversation_async`, `next_sequence_async`, `put_inbox_async`,
`get_inbox_range_async`, `delete_inbox_async` — each returns a
`Receiver<StoreResult>`.

- `Store::open(path, notify, durable)`. Results: `StoreResult` enum
  (`Stored`, `Account`, `Sequence`, `InboxRange`, …) and `StoreError`.
- Backing tables: accounts (`user_id → credential: salt‖hash`), conversations
  (`conv_id → member list`), per-conversation sequence counters, and the
  inbox (`user_id / conv_id / seq → delivery body`).

### `domain` — application logic

- `Domain::new(Store)` — the `EventHandler` implementation wired into the
  server. Holds sessions, per-user connection sets, user→conversation
  membership, and in-flight async storage operations.
- Implements auth (Hello with user-chosen password), conversation create,
  message send with store-and-forward delivery, conversation listing,
  `Ping`→`Pong`, and `Presence` echo.
- Internally derives conversation ids (sorted-member hash) and stores salted
  SHA-256 password credentials (`salt ‖ sha256(salt ‖ password)`) with the
  salt read from `/dev/urandom`. A KDF with a work factor (bcrypt/argon2) is
  recommended for production.

### `msgtui` — the TUI frontend

The [`msgtui`](tui) crate is a reusable, term_render-based chat TUI that drives
a generic `ChatClient` trait (`send(Action)`, `poll_events() -> Vec<Event>`).
The `Action`/`Event` types live in the [`chat-model`](chat-model) crate. The
`msgtui` binary in `tui/src/bin/` wires the TUI to
[`msgclient`](#msgclient--the-channel-client) through `tui_client.rs` — the
adapter that implements `ChatClient` for the channel client.

`msgtui` vendors the patched `term_render` / `term_render_macros` crates under
`tui/vendor/` (their manifests set `doctest = false` because the vendored doc
examples don't compile unmodified). The `msgtui` lib itself has no knowledge of
the wire protocol — only the `msgtui` binary ties the two together through
`tui_client.rs`.

## Configuration & Operation

- Listen socket uses `SO_REUSEPORT` (multi-shard ready) and `SO_REUSEADDR`;
  listener backlog is 128; connections are non-blocking.
- Data is stored in a single `redb` file; `redb` uses ACID transactions and
  no `unsafe`.

## Tests & Benchmarks

```sh
cargo test          # protocol unit tests + server integration tests + client tests
cargo bench         # criterion echo-throughput / latency benches (benches/echo.rs)
```

Integration tests (`server/tests/integration.rs`) spin up the real server over
real sockets and cover auth (new + reconnect), conversation create + send, and
ping/pong. Client tests (`client/tests/`) exercise the `msgclient` channel API
against a live server (connect, idle, latency).

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
tui/         msgtui — protocol-agnostic term_render frontend + launcher bin
  ├── lib.rs      the ChatClient trait + app (no wire knowledge)
  └── bin/        msgtui binary: connects via msgclient, adapts it to the TUI
```

See [PROTOCOL.md](PROTOCOL.md) for the authoritative wire-format reference.
