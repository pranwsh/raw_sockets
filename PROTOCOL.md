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

## Connection Lifecycle

### 1. Connect (TCP)

Open a TCP connection to `host:port`. The server does not speak first — the
client must send the first frame.

### 2. Auth Handshake

All frames flow over the same TCP connection. No multiplexing — the protocol
is strictly request–response in application order (though the server may push
frames like `Send` for inbox delivery at any time after auth).

Auth is a single round trip. The client sends `Hello(user_id\npassword)`
where `password` is chosen by the user. The server either creates the account
(if the user id is new) or verifies the stored credential (if it exists):

```
  Client                          Server
    │                               │
    │  Hello(user_id\npassword)     │
    ├──────────────────────────────>│
    │                               │  lookup account
    │                               │  create or verify credential
    │        AuthOk(new_account)    │  or AuthFail(reason)
    │<──────────────────────────────│
    │                               │
    │  (session established)        │
```

`AuthOk` carries a 1-byte flag: `1` when the account was just created, `0`
when an existing account authenticated.

### 3. Application Messages

After auth, send and receive application frames. The server may also push
stored inbox messages as `Send` frames (same body format as client-originated
`Send`).

### 4. Teardown

Either side may send `Goodbye` (empty body). The server also tears down on
protocol violations or backpressure cap — in those cases the peer receives a
TCP RST or FIN without a `Goodbye`.

## Message Types

### `Hello` (1) — Client → Server

**Body**: `<user_id>\n<password>` (both UTF-8, 1–256 bytes).

Initiates auth and carries the user-chosen password. For a new user the server
creates the account (replying `AuthOk` with flag `1`); for an existing user it
verifies the password (replying `AuthOk` with flag `0` or `AuthFail`).

### `AuthOk` (4) — Server → Client

**Body**: 1 byte — `1` if the account was just created, `0` if an existing
account authenticated.

Sent on successful auth.

### `AuthFail` (5) — Server → Client

**Body**: UTF-8 error string.

Sent when auth fails (invalid password, bad format, etc.).

### `Goodbye` (6) — Bidirectional

**Body**: empty.

Clean teardown. The server also tears down on errors — clients should be
prepared for abrupt disconnection.

### `Presence` (10) — Bidirectional

**Body**: implementation-defined.

Echoed to connected peers. Currently a pass-through — reserved for future
online/offline notification.

### `CreateConv` (20) — Client → Server

**Body**: comma-separated member user IDs, e.g. `alice,bob`.

Creates a deterministic conversation ID (hash of sorted member list). Minimum
2 members.

### `ConvCreated` (21) — Server → Client

**Body**: 8-byte conversation ID (little-endian u64 hash).

Sent in response to `CreateConv`.

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

### `ListConvs` (37) — Client → Server

**Body**: empty.

Requests the calling user's conversation list. The server responds with a
single `ConvsResp` frame.

### `ConvsResp` (38) — Server → Client

**Body**: concatenated 8-byte conversation IDs (little-endian u64), one per
conversation.

Sent in response to `ListConvs`. An empty body means the user has no
conversations.

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

# Send Hello with the chosen password
sock.sendall(encode(1, b'alice\nhunter2'))

# Recv AuthOk (flag=1 new account, flag=0 existing) or AuthFail
frame = recv_frame(sock)
msg_type = ...          # parse from last recv
if msg_type == 5:       # AuthFail
    raise Exception(frame)

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

Application-level errors (`Error` type 99) do **not** tear down — the
connection remains usable.

## Version Notes

### v1 (current)

- Initial protocol version.
- Auth: user-chosen password in `Hello(user_id\npassword)`, salted SHA-256
  credential stored by the server (`salt ‖ sha256(salt ‖ password)`). The
  salt is read from the OS CSPRNG (`/dev/urandom`). For production, consider
  a key-derivation function with a work factor (bcrypt/argon2) and TLS for
  transport.
- Legacy token-based handshake (`AuthChallenge`/`AuthResponse` + 32-byte
  derived token) was replaced; those message types have since been removed
  from the protocol.
- Accounts created before this change stored a 32-byte derived token instead
  of a credential, so they cannot authenticate under the new scheme.
