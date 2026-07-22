use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

use protocol::{self, MsgType, OwnedFrame, Decode};

pub fn run(
    host: &str,
    port: u16,
    user: &str,
    conv: Option<&str>,
    create: Option<&str>,
    token: Option<&str>,
    message: &str,
    listen: bool,
) {
    let addr = format!("{}:{}", host, port);
    let mut stream = match TcpStream::connect(&addr) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("error: failed to connect: {}", e);
            std::process::exit(1);
        }
    };
    let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));

    if let Err(e) = do_auth(&mut stream, user.as_bytes(), token) {
        eprintln!("error: auth failed: {}", e);
        let _ = stream.shutdown(std::net::Shutdown::Both);
        std::process::exit(1);
    }

    let conv_id = if let Some(members) = create {
        let mut full_members = user.as_bytes().to_vec();
        for m in members.split(',') {
            let m = m.trim();
            if !m.is_empty() && m != user {
                full_members.push(b',');
                full_members.extend_from_slice(m.as_bytes());
            }
        }
        match create_conv(&mut stream, &full_members) {
            Ok(id) => {
                eprintln!("created conversation: {} (members: {},{})", hex::encode(&id), user, members);
                id
            }
            Err(e) => {
                eprintln!("error: create conversation failed: {}", e);
                std::process::exit(1);
            }
        }
    } else if let Some(hex_str) = conv {
        match hex::decode(hex_str) {
            Ok(id) => id,
            Err(e) => {
                eprintln!("error: invalid conv hex: {}", e);
                std::process::exit(1);
            }
        }
    } else {
        unreachable!()
    };

    if let Err(e) = send_msg(&mut stream, &conv_id, message.as_bytes(), listen) {
        eprintln!("error: send failed: {}", e);
        std::process::exit(1);
    }

    let goodbye = protocol::encode(MsgType::Goodbye, 0, &[]);
    let _ = stream.write_all(&goodbye);
    let _ = stream.flush();
}

fn do_auth(stream: &mut TcpStream, user_id: &[u8], token: Option<&str>) -> Result<(), String> {
    let hello = protocol::encode(MsgType::Hello, 0, user_id);
    stream.write_all(&hello).map_err(|e| format!("write error: {}", e))?;

    let frame = recv_frame(stream).map_err(|e| format!("recv error: {}", e))?;

    match frame.msg_type {
        MsgType::AuthOk => {
            let tok = frame.body.to_vec();
            eprintln!("auth ok (new account, save this token: {})", hex::encode(&tok));
            Ok(())
        }
        MsgType::AuthChallenge => {
            let tok_hex = token.ok_or_else(|| {
                "server requires auth token (previous session). use --token <hex>".to_string()
            })?;
            let tok = hex::decode(tok_hex).map_err(|e| format!("invalid token hex: {}", e))?;

            let mut body = Vec::new();
            body.extend_from_slice(user_id);
            body.push(b'\n');
            body.extend_from_slice(&tok);
            let resp = protocol::encode(MsgType::AuthResponse, 0, &body);
            stream.write_all(&resp).map_err(|e| format!("write error: {}", e))?;

            let frame = recv_frame(stream).map_err(|e| format!("recv error: {}", e))?;
            if frame.msg_type != MsgType::AuthOk {
                return Err(format!("auth failed: {:?}", frame.msg_type));
            }
            eprintln!("auth ok (reconnected)");
            Ok(())
        }
        _ => Err(format!("unexpected response to hello: {:?}", frame.msg_type)),
    }
}

fn create_conv(stream: &mut TcpStream, members: &[u8]) -> Result<Vec<u8>, String> {
    let req = protocol::encode(MsgType::CreateConv, 0, members);
    stream.write_all(&req).map_err(|e| format!("write error: {}", e))?;

    let frame = recv_frame(stream).map_err(|e| format!("recv error: {}", e))?;
    if frame.msg_type != MsgType::ConvCreated {
        let body = String::from_utf8_lossy(&frame.body);
        return Err(format!("expected ConvCreated, got {:?}: {}", frame.msg_type, body));
    }
    Ok(frame.body.to_vec())
}

fn send_msg(
    stream: &mut TcpStream,
    conv_id: &[u8],
    body: &[u8],
    listen: bool,
) -> Result<(), String> {
    let mut payload = Vec::new();
    payload.extend_from_slice(conv_id);
    payload.push(b'\n');
    payload.extend_from_slice(body);
    let req = protocol::encode(MsgType::Send, 0, &payload);
    stream.write_all(&req).map_err(|e| format!("write error: {}", e))?;

    loop {
        let frame = recv_frame(stream).map_err(|e| format!("recv error: {}", e))?;
        match frame.msg_type {
            MsgType::Delivered => {
                let seq = u64::from_le_bytes(
                    frame.body[..8].try_into().unwrap(),
                );
                println!("delivered seq={}", seq);
                if !listen {
                    break;
                }
            }
            MsgType::Send => {
                if let Some(sep) = frame.body.iter().position(|&b| b == b'\n') {
                    let conv_hex = hex::encode(&frame.body[..sep]);
                    let after = &frame.body[sep + 1..];
                    if after.len() >= 8 {
                        let seq = u64::from_le_bytes(after[..8].try_into().unwrap());
                        let msg_bytes = &after[8..];
                        let from_sep = msg_bytes.iter().position(|&b| b == b'\n');
                        let (sender, msg) = match from_sep {
                            Some(p) => (
                                String::from_utf8_lossy(&msg_bytes[..p]),
                                String::from_utf8_lossy(&msg_bytes[p + 1..]),
                            ),
                            None => (String::from_utf8_lossy(msg_bytes), "".into()),
                        };
                        eprintln!("echo: conv={} seq={} from={} msg={}", conv_hex, seq, sender, msg);
                    }
                }
            }
            _ => {
                println!("incoming: {:?} ({} bytes)", frame.msg_type, frame.body.len());
                if !listen {
                    break;
                }
            }
        }
    }
    Ok(())
}

fn recv_frame(stream: &mut TcpStream) -> Result<OwnedFrame, String> {
    use protocol::{HEADER_LEN, TRAILER_LEN, MAX_BODY_LEN};

    let mut header = [0u8; HEADER_LEN];
    stream.read_exact(&mut header).map_err(|e| format!("read header: {}", e))?;

    let body_len = u32::from_le_bytes(header[4..8].try_into().unwrap()) as usize;
    if body_len > MAX_BODY_LEN {
        return Err(format!("body too large: {} > {}", body_len, MAX_BODY_LEN));
    }

    let mut body_crc = vec![0u8; body_len + TRAILER_LEN];
    stream.read_exact(&mut body_crc).map_err(|e| format!("read body: {}", e))?;

    let mut full = Vec::with_capacity(HEADER_LEN + body_len + TRAILER_LEN);
    full.extend_from_slice(&header);
    full.extend_from_slice(&body_crc);

    match protocol::decode(&full) {
        Decode::Complete { frame, .. } => Ok(OwnedFrame::from_borrowed(&frame)),
        Decode::Need => Err("incomplete frame after read_exact".into()),
        Decode::Err(e) => Err(format!("decode error: {:?}", e)),
    }
}
