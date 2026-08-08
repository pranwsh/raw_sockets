//! protocol unit tests — highest value in the project

use super::*;

fn sample_frame(t: MsgType, body: &[u8]) -> Vec<u8> {
    let total = HEADER_LEN + body.len() + TRAILER_LEN;
    let mut buf = vec![0u8; total];
    encode_into(&mut buf, t, 0, body);
    buf
}

#[test]
fn round_trip_empty_body() {
    let buf = sample_frame(MsgType::Ping, &[]);
    match decode(&buf) {
        Decode::Complete { frame, consumed } => {
            assert_eq!(frame.body, &[][..]);
            assert_eq!(frame.msg_type, MsgType::Ping);
            assert_eq!(consumed, FRAME_OVERHEAD);
        }
        other => panic!("expected complete, got {other:?}"),
    }
}

#[test]
fn round_trip_with_body() {
    let body: &[u8] = b"hello, messaging world";
    let buf = sample_frame(MsgType::Send, body);
    match decode(&buf) {
        Decode::Complete { frame, consumed } => {
            assert_eq!(frame.body, body);
            assert_eq!(frame.msg_type, MsgType::Send);
            assert_eq!(consumed, buf.len());
        }
        other => panic!("expected complete, got {other:?}"),
    }
}

#[test]
fn flags_round_trip() {
    let flags = FLAG_ACK_REQ | FLAG_COMPRESSED;
    let buf = encode(MsgType::Send, flags, b"payload");
    match decode(&buf) {
        Decode::Complete { frame, .. } => {
            assert_eq!(frame.flags, flags);
            assert!(frame.ack_requested());
            assert!(frame.is_compressed());
        }
        other => panic!("expected complete, got {other:?}"),
    }
}

#[test]
fn split_at_every_byte_boundary() {
    // the transport may slice the wire stream at any byte offset; decoding any proper prefix must yield Need and the full buffer must decode to the expected frame
    for body in [b"".as_slice(), b"x", b"the quick brown fox".as_slice()] {
        let full = sample_frame(MsgType::Send, body);
        for cut in 0..=full.len() {
            match decode(&full[..cut]) {
                Decode::Need => {}
                Decode::Complete { frame, consumed } => {
                    // only acceptable at full length
                    assert_eq!(cut, full.len(), "decode completed early at cut={cut}");
                    assert_eq!(frame.body, body);
                    assert_eq!(consumed, full.len());
                }
                Decode::Err(e) => panic!("decode errored at cut={cut}: {e:?}"),
            }
        }
    }
}

#[test]
fn multiple_frames_back_to_back() {
    let mut buf = Vec::new();
    buf.extend_from_slice(&sample_frame(MsgType::Ping, &[]));
    buf.extend_from_slice(&sample_frame(MsgType::Send, b"one"));
    buf.extend_from_slice(&sample_frame(MsgType::Send, b"two"));

    let mut offset = 0;
    let mut frames = 0;
    loop {
        match decode(&buf[offset..]) {
            Decode::Complete { frame: _, consumed } => {
                frames += 1;
                offset += consumed;
                if offset == buf.len() {
                    break;
                }
            }
            other => panic!("unexpected at offset {offset}: {other:?}"),
        }
    }
    assert_eq!(frames, 3);
}

// malformed-input rejection

#[test]
fn rejects_bad_magic() {
    let mut buf = sample_frame(MsgType::Ping, &[]);
    buf[0] = 0x00;
    assert_eq!(decode(&buf), Decode::Err(DecodeError::BadMagic));
}

#[test]
fn rejects_unsupported_version() {
    let mut buf = sample_frame(MsgType::Ping, &[]);
    buf[1] = 99;
    assert_eq!(decode(&buf), Decode::Err(DecodeError::UnsupportedVersion(99)));
}

#[test]
fn rejects_unknown_msg_type() {
    let mut buf = sample_frame(MsgType::Ping, &[]);
    // set type field to a reserved value (0)
    buf[2] = 0x00;
    buf[3] = 0x00;
    // we corrupted the CRC too here; the type check fires first since it's before the length/body section, so we expect UnknownMsgType
    assert_eq!(decode(&buf), Decode::Err(DecodeError::UnknownMsgType(0)));
}

#[test]
fn rejects_body_too_large() {
    let mut buf = vec![0u8; HEADER_LEN];
    buf[0] = MAGIC;
    buf[1] = VERSION;
    // use a known-good type to reach the length check
    buf[2..4].copy_from_slice(&(MsgType::Send as u16).to_le_bytes());
    // declare MAX_BODY_LEN+1
    let huge = (MAX_BODY_LEN as u32) + 1;
    buf[4..8].copy_from_slice(&huge.to_le_bytes());
    assert_eq!(
        decode(&buf),
        Decode::Err(DecodeError::BodyTooLarge {
            declared: MAX_BODY_LEN + 1,
            max: MAX_BODY_LEN
        })
    );
}

#[test]
fn rejects_corrupt_body_crc_mismatch() {
    let mut buf = sample_frame(MsgType::Send, b"corrupt me");
    // flip a body bit
    buf[HEADER_LEN] ^= 0x01;
    assert_eq!(decode(&buf), Decode::Err(DecodeError::CrcMismatch));
}

#[test]
fn rejects_corrupt_header_crc_mismatch() {
    // toggle the high (compress) flag bit while leaving MsgType intact, so the only decoder path remaining is CRC verification — which must reject
    let mut buf = sample_frame(MsgType::Ping, b"some body to crc");
    buf[3] ^= 0x80; // top flag bit (FLAG_COMPRESSED occupies bit 15)
    assert_eq!(decode(&buf), Decode::Err(DecodeError::CrcMismatch));
}

#[test]
fn empty_buffer_needs_more() {
    assert!(matches!(decode(&[]), Decode::Need));
}

#[test]
fn sub_header_length_needs_more() {
    for n in 1..HEADER_LEN {
        assert!(matches!(decode(&vec![MAGIC; n]), Decode::Need));
    }
}

#[test]
fn crc_is_castagnoli() {
    // known-vector sanity check for crc32c against RFC 4960 / widely cited reference: crc32c of "123456789" = 0xE3069283
    assert_eq!(crc32c(b"123456789"), 0xE3069283);
}
