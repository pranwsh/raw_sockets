# Architecture — Raw-Socket Messaging Service

## Design Priorities

1. **Speed and scalability** — between two correct designs, choose the faster and more horizontally scalable one.
2. **Separation of concerns** — enforced by workspace crate boundaries, not just module walls.
3. **Low duplication** — shared framing/routing logic lives in `protocol` and `transport`, not copied between server and internal client code.

## Crate Layout

```
Cargo.toml                  # workspace root (resolver = "2")
protocol/                   # pure wire format: no I/O, no deps
transport/                  # epoll reactor, connection state machine, backpressure
  ├── sys.rs                # safe wrappers around libc/epoll/socket calls
  ├── conn.rs               # per-connection buffers, decoder integration, backpressure policy
  └── reactor.rs            # epoll-based single-threaded event loop + EventHandler trait
domain/                     # accounts, conversations, inbox, auth (no socket knowledge)
storage/                    # redb-backed persistent store, offloaded to background thread
routing/                    # inter-node mesh (user presence exchange, message forwarding)
server/                     # binary that wires everything together
benches/                    # criterion benchmarks
tests/                      # integration tests
```

## Wire Protocol (`protocol`)

Every message is framed as:

```
[magic:1][ver:1][type+flags:2][body_len:4][body:body_len][crc32c:4]
```

- **Magic** (`0x4D`) — fast reject of unrelated traffic.
- **Version** (`1`) — enables forward-compatible evolution.
- **Type+Flags** (u16 LE) — low 13 bits = message type; high 3 bits = flags (compressed, ack-req, priority).
- **Body length** (u32 LE) — capped at 16 MiB to bound buffering.
- **CRC32C** (Castagnoli) — covers header(ver..body_end) to detect corruption before dispatch.

The `Decoder` is a pure function `&[u8] → Decode<'_>` that never panics. All error variants map to a reset/teardown on the transport side. The encoding side (`encode_into` / `encode`) produces ready-to-wire byte sequences.

## Transport Layer (`transport`)

### Connection State Machine

Each `Connection` owns its fd, a growable read buffer, a two-segment write buffer (no memmove on partial writes), an idle deadline, and an optional `TeardownState`.

### Backpressure Policy

| Threshold | Action |
|---|---|
| `< 256 KiB` | Normal read + write |
| `256 KiB – 1 MiB` | Pause reads (don't register EPOLLIN) — let the drain catch up |
| `≥ 1 MiB` (hard cap) | Drop new outgoing frames, queue `SlowConsumer` Error frame, tear down |

Rationale: no single slow reader can exhaust heap. Caps are per-connection. A future global memory pool with per-connection quotas would improve predictability but the per-connection cap is the correctness floor.

### Teardown — Single Unifying Path

Every error branch (malformed frame, idle timeout, peer RST, write exhaustion, client Goodbye) calls `Connection::start_teardown(reason)`. This:
1. Shuts down the read side immediately.
2. Queues a farewell frame (best-effort).
3. Keeps the write side open for a 3-second drain window.
4. Either drains fully or hits the deadline, then closes.

No duplicate cleanup logic anywhere.

### Event Loop

Edge-triggered epoll (`EPOLLET`) in a single thread. The reactor owns the connection map and dispatches decoded frames to an `EventHandler` trait implementation. After each `on_frame` call, the reactor drains the handler's `drain_outbound()` and `drain_teardowns()` queues — this is the only channel from the handler back to I/O, keeping the trait simple and single-threaded.

## Storage (`storage`)

### Why `redb`?

- **Embedded** — no external DB process; eliminates network round-trip on every message operation.
- **ACID transactions** — a crash doesn't corrupt state.
- **Single-writer** — pairs naturally with a background worker thread that serializes all operations.
- **No `unsafe`** — aligns with the project's safety stance.

### Non-Blocking Offload

The `Store` handle is `Clone + Send + Sync`. Every operation (put/get/next_sequence/range) sends a message to a background thread through an `mpsc` channel and receives the result on a one-shot channel. The event loop thread never blocks on storage I/O.

## Domain (`domain`)

### Auth Scheme

Token-based handshake over the binary protocol:

1. **Registration**: Client sends `Hello(user_id)`. Server derives a token (XOR-based hash of `user_id + server_secret`) and stores it in the account table. Returns `AuthOk(token)`.
2. **Reconnect**: Server sends `AuthChallenge`. Client responds with `AuthResponse(user_id \n token)`. Server recomputes the expected token and compares.
3. All tokens are 32 bytes. (In production, replace the derivation with HMAC-SHA256.)

### Conversations

- Created by `CreateConv(member1,member2,...)`. A deterministic ID is derived from the sorted member list.
- Messages are `Send(conv_id \n body)`. Each message gets a monotonically increasing sequence number per conversation (stored in redb's sequence table).
- Messages are persisted in the inbox table and delivered on next connect.

### Inbox / Delivery

- Messages for offline users are stored in the inbox table, keyed by `user_id / conv_id / big-endian-seq`.
- On connect (after auth), pending inbox messages are fetched and delivered, then deleted (at-most-once delivery).
- A `Delivered` ack frame is returned to the sender after store-and-forward.
- Read receipts are a future extension (the `Read` and `Delivered` frame types are reserved).

## Scaling Story

### What's Implemented Now (Single-Node)

- A single `Reactor<Domain>` event loop in one thread.
- All state (connections, sessions, routing table) is thread-local — no shared mutable state, no locks.
- Storage I/O is offloaded to a background thread, so the event loop never blocks.
- `SO_REUSEPORT` is enabled on the listener socket, ready for multi-shard.

### Multi-Core Scale-Up (Design Ready, Requires Wiring)

The architecture supports N reactor shards, one per core, each owning a subset of connections:

```
┌─────────────┐  ┌─────────────┐  ┌─────────────┐
│ Reactor 1   │  │ Reactor 2   │  │ Reactor N   │
│ (core 0)    │  │ (core 1)    │  │ (core N-1)  │
│ SO_REUSEPORT│  │ SO_REUSEPORT│  │ SO_REUSEPORT│
│ Domain[1]   │  │ Domain[2]   │  │ Domain[N]   │
└──────┬──────┘  └──────┬──────┘  └──────┬──────┘
       │                 │                 │
       └─────────────────┼─────────────────┘
                         ▼
              ┌─────────────────────┐
              │ Routing Layer       │
              │ (cross-shard pub/sub)│
              └─────────────────────┘
```

Each shard has its own `Domain` instance (no shared session/connection maps). When a message targets a user on a different shard, the routing layer forwards it. The routing layer itself would be a per-shard component that sends/receives frames through an `mpsc` channel to the peer shard's reactor.

The key insight: **there is no cross-shard lock contention on the hot path**. Connections and their state are strictly per-shard. Cross-shard communication goes through channels (lock-free mpsc), not mutex-protected shared state.

### Multi-Node Scale-Out (Design Ready, Requires Wiring)

The `routing` crate defines the mesh protocol:

- Each node opens a control connection to every peer.
- `RouteAnnounce` frames broadcast local user presence.
- The routing table (`user_id → set<node_id>`) is maintained by each node.
- `RouteDeliver` frames forward messages to the correct peer.
- Uses the same binary framing protocol — no new code for frame parsing.

To wire multi-node:
1. Spawn a separate `Reactor<Router>` for routing connections (same epoll loop, different listener).
2. Register local users via `Router::register_local()` called from `Domain::establish_session`.
3. When `on_frame` receives a `Send` for a non-local user, check the routing table and forward via `RouteDeliver` instead.

### State That Doesn't Bottleneck

| State | Location | Contention |
|---|---|---|
| Connection map | Per-shard `Reactor` | None (thread-local) |
| Session map | Per-shard `Domain` | None (thread-local) |
| Routing table | Per-node `Router` | None (thread-local after exchange) |
| Persistent store | Background thread | Single-writer (serialized by redb) |

The only shared write path is the storage database, which is single-writer by design. All hot-path state is thread-local.

## Testing Strategy

**Highest value**: protocol decoder tests (split-at-byte-boundary, malformed input). These live in `protocol/src/tests.rs` and are exhaustive across every `DecodeError` variant and every byte-boundary prefix.

**Integration tests**: in `tests/`, exercise the server binary with real sockets — concurrent clients, slow clients (backpressure), and (future) multi-node routing.

**Benchmarks**: criterion benches for echo throughput and p50/p99 latency under concurrent load. These prove the speed claims.
