# Protocol Reference — Messaging Service

This document describes the binary protocol spoken by `msgd`. It is the
authoritative reference for writing clients.

## Wire Format

Every message on the wire is a **frame**:

```
 0                   1                   2                   3
 0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1
+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
|     magic     |     ver       |    type + flags (u16 LE)       |
+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
|                       body length (u32 LE)                     |
+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
|                                                               |
+                            body                                +
|                                                               |
+                                                               +
|                               +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
|                               |     CRC32C (u32 LE)           |
+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
```

| Field | Type | Description |
|-------|------|-------------|
| magic | `u8` | Always `0x4D` (`'M'`). Used to reject unrelated traffic. |
| ver | `u8` | Protocol version. Currently `1`. |
| type+flags | `u16` LE | Low 13 bits = message type. High 3 bits = flags (see below). |
| body length | `u32` LE | Length of body in bytes. Max `16,777,216` (16 MiB). |
| body | `[u8; len]` | Payload. Format depends on message type. |
| CRC32C | `u32` LE | Castagnoli CRC32 over bytes `[ver .. body_end]`. |

### Flags (high 3 bits of type+flags)

| Bit | Constant | Meaning |
|-----|----------|---------|
| 15 | `FLAG_COMPRESSED` | Body is zlib-compressed. |
| 14 | `FLAG_ACK_REQ` | Sender requests an ack. |
| 13 | `FLAG_PRIORITY` | Mark for priority delivery. |

## Connection Lifecycle

### 1. Connect (TCP)

Open a TCP connection to `host:port`. The server does not speak first — the
client must send the first frame.

### 2. Auth Handshake

All frames flow over the same TCP connection. No multiplexing — the protocol
is strictly request–response in application order (though the server may push
frames like `Send` for inbox delivery at any time after auth).

**New account**:

```
  Client                          Server
    │                               │
    │  Hello(user_id)               │
    ├──────────────────────────────>│
    │                               │  derive token, store account
    │           AuthOk(token_32)    │
    │<──────────────────────────────│
    │                               │
    │  (session established)        │
```

**Existing account (reconnect)**:

```
  Client                          Server
    │                               │
    │  Hello(user_id)               │
    ├──────────────────────────────>│
    │                               │  lookup account: exists
    │    AuthChallenge(body)        │
    │<──────────────────────────────│
    │                               │
    │  AuthResponse(user_id\n       │
    │               token_32)       │
    ├──────────────────────────────>│
    │                               │  verify token
    │           AuthOk(token_32)    │
    │<──────────────────────────────│
    │                               │
    │  (session established)        │
```

### 3. Application Messages

After auth, send and receive application frames. The server may also push
stored inbox messages as `Send` frames (same body format as client-originated
`Send`).

### 4. Teardown

Either side may send `Goodbye` (empty body). The server also tears down on
protocol violations, idle timeout, or backpressure cap — in those cases the
peer receives a TCP RST or FIN without a `Goodbye`.

## Message Types

### `Hello` (1) — Client → Server

**Body**: `user_id` (UTF-8 bytes, 1–256 chars).

Initiates auth. For a new user the server creates the account; for an existing
user it responds with `AuthChallenge`.

### `AuthChallenge` (2) — Server → Client

**Body**: `token_required\n<user_id>`.

Sent when the server has a stored token for the given user. The client must
reply with `AuthResponse`.

### `AuthResponse` (3) — Client → Server

**Body**: `<user_id>\n<token>` (32-byte binary token).

The token is the raw 32 bytes received from the earlier `AuthOk` or derived
client-side.

### `AuthOk` (4) — Server → Client

**Body**: `<token>` (exactly 32 bytes).

Sent on successful auth. The client stores this token for reconnection.

### `AuthFail` (5) — Server → Client

**Body**: UTF-8 error string.

Sent when auth fails (invalid token, bad format, etc.).

### `Goodbye` (6) — Bidirectional

**Body**: empty.

Clean teardown. The server also tears down on errors — clients should be
prepared for abrupt disconnection.

### `Presence` (10) — Bidirectional

**Body**: implementation-defined.

Echoed to connected peers. Currently a pass-through — reserved for future
online/offline notification.

### `Typing` (11) — Client → Server

**Body**: `<conv_id>`.

Reserved. Not yet processed.

### `CreateConv` (20) — Client → Server

**Body**: comma-separated member user IDs, e.g. `alice,bob`.

Creates a deterministic conversation ID (hash of sorted member list). Minimum
2 members.

### `ConvCreated` (21) — Server → Client

**Body**: 8-byte conversation ID (little-endian u64 hash).

Sent in response to `CreateConv`.

### `ConvInvite` (22) — Client → Server

**Body**: `<conv_id>\n<user_id>`.

Adds a user to an existing conversation.

### `ConvJoin` (23) — Client → Server

**Body**: `conv_id`.

Reserved for explicit join.

### `ConvLeave` (24) — Client → Server

**Body**: `conv_id`.

Reserved for leaving a conversation.

### `ConvMemberEvent` (25) — Server → Client

**Body**: comma-separated member list.

Sent after a successful `ConvInvite` to all participants.

### `Send` (30) — Client → Server / Server → Client

**Client → Server body**: `<conv_id>\n<message_body>`

Sends a message to a conversation. The server assigns a sequence number and
persists it before replying `Delivered`.

**Server → Client body** (inbox delivery): `<conv_id>\n<seq:8le>\n<sender_id>\n<message_body>`

The server pushes stored messages as `Send` frames to the recipient after
auth (either immediately for online delivery or on reconnect for offline).

### `Delivered` (31) — Server → Client

**Body**: 8-byte sequence number (little-endian u64).

Acknowledges that a `Send` was persisted and will be delivered.

### `Read` (32) — Client → Server

**Body**: `<conv_id>\n<seq:8le>`.

Reserved for read receipts.

### `HistoryReq` (33) — Client → Server

**Body**: `<conv_id>\n<since_seq:8le>\n<limit:4le>`.

Reserved for fetching history.

### `HistoryResp` (34) — Server → Client

**Body**: TBD.

Reserved.

### `InboxFetch` (35) — Client → Server

**Body**: empty.

Requests any pending inbox messages. The server responds with queued `Send`
frames followed by an `InboxResp`.

### `InboxResp` (36) — Server → Client

**Body**: empty.

Sent after all pending inbox messages have been delivered.

### `Ping` (90) — Bidirectional

**Body**: empty.

Keepalive. The receiver responds with `Pong`. Useful for detecting dead
connections through NAT/firewalls.

### `Pong` (91) — Bidirectional

**Body**: empty.

Response to `Ping`.

### `Error` (99) — Server → Client

**Body**: UTF-8 error string.

Sent for application-level errors (bad format, storage failure, etc.). The
connection remains open. Clients should log and continue.

## CRC32C (Castagnoli)

The frame CRC uses polynomial `0x1EDC6F41` (reflected `0x82F63B78`),
covering bytes from `ver` (offset 1) through the end of the body. The
initial value is `0xFFFF_FFFF` and the final value is inverted (`!crc`).

Reference implementation (pure Rust, no dependencies):

```rust
const CRC32C_POLY: u32 = 0x82F63B78;

fn crc32c(bytes: &[u8]) -> u32 {
    let mut crc: u32 = 0xFFFF_FFFF;
    for &b in bytes {
        crc ^= b as u32;
        for _ in 0..8 {
            crc = if crc & 1 != 0 { (crc >> 1) ^ CRC32C_POLY } else { crc >> 1 };
        }
    }
    !crc
}
```

Precomputed table or hardware CRC32C intrinsics are strongly recommended in
production clients for throughput.

### Test Vectors

| Input | CRC32C |
|-------|--------|
| Empty (`[]`) | `0x00000000` |
| `[0x01, 0x01, 0x00, 0x00, 0x00, 0x00]` (Hello frame header sans magic) | computed per above |

## Client Implementation Walkthrough (Python Pseudocode)

```python
import socket, struct

MAGIC   = 0x4D
VERSION = 1

def encode(msg_type, body, flags=0):
    tf = (flags & 0xE000) | (msg_type & 0x1FFF)
    hdr = struct.pack('<BBHI', MAGIC, VERSION, tf, len(body))
    crc = crc32c(hdr[1:] + body)   # crc32c from ver through body
    return hdr + body + struct.pack('<I', crc)

def recv_frame(sock):
    buf = b''
    while True:
        if len(buf) < 8:
            buf += sock.recv(8192)
            continue
        body_len = struct.unpack_from('<I', buf, 4)[0]
        total = 8 + body_len + 4
        while len(buf) < total:
            buf += sock.recv(8192)
        # validate crc ...
        frame = buf[8:8+body_len]
        buf = buf[total:]   # advance buffer
        return frame

# Auth
sock = socket.create_connection(('127.0.0.1', 9000))

# Send Hello
sock.sendall(encode(1, b'alice'))

# Recv AuthOk (or AuthChallenge)
frame = recv_frame(sock)
msg_type = ...          # parse from last recv
if msg_type == 4:       # AuthOk
    token = frame
elif msg_type == 2:     # AuthChallenge
    # send AuthResponse
    sock.sendall(encode(3, b'alice\n' + token))
    frame2 = recv_frame(sock)
    assert msg_type == 4

# Create conversation
sock.sendall(encode(20, b'alice,bob'))
conv_id = recv_frame(sock)   # ConvCreated

# Send message
body = conv_id + b'\nHello, Bob!'
sock.sendall(encode(30, body))
ack = recv_frame(sock)       # Delivered

# Ping
sock.sendall(encode(90, b''))
pong = recv_frame(sock)      # Pong

# Goodbye
sock.sendall(encode(6, b''))
sock.close()
```

## Backpressure

The server applies per-connection backpressure:

| Outbound buffer size | Behaviour |
|---|---|
| `< 256 KiB` | Normal operation |
| `256 KiB – 1 MiB` | Inbound reads paused — drain catches up |
| `≥ 1 MiB` | Hard cap — new frames dropped, connection torn down |

Clients should consume inbound frames promptly to avoid being disconnected.

## Error Handling

The server tears down a connection (no `Goodbye` sent) when it receives:

- A bad magic byte or unsupported protocol version.
- A frame with `body_len > 16 MiB`.
- A CRC32C mismatch (wire corruption).
- An idle connection (no frames for the configured timeout).

Application-level errors (`Error` type 99) do **not** tear down — the
connection remains usable.

## Version Notes

### v1 (current)

- Initial protocol version.
- Token derivation: XOR-based hash (placeholder). In production, replace
  the server's `SERVER_SECRET` and use HMAC-SHA256 for token derivation.
