use std::io::{self, BufRead, Write};
use std::net::TcpStream;
use std::sync::mpsc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use protocol::{self, MsgType, OwnedFrame, Decode, Frame};

pub fn run(host: &str, port: u16) {
    let mut sh = Shell::new(host, port);
    sh.repl();
}

struct ConvInfo {
    id: Vec<u8>,
    members: String,
}

struct Shell {
    host: String,
    port: u16,
    stream: Option<TcpStream>,
    writer: Option<TcpStream>,
    frame_rx: Option<mpsc::Receiver<OwnedFrame>>,
    _reader: Option<thread::JoinHandle<()>>,
    cmd_rx: mpsc::Receiver<String>,
    listening: Arc<AtomicBool>,
    authenticated: bool,
    token: Option<Vec<u8>>,
    user_id: Option<String>,
    conversations: Vec<ConvInfo>,
}

impl Shell {
    fn new(host: &str, port: u16) -> Self {
        let (cmd_tx, cmd_rx) = mpsc::channel::<String>();

        // Spawn stdin reader thread
        thread::spawn(move || {
            let stdin = io::stdin();
            let mut buf = String::new();
            loop {
                buf.clear();
                match stdin.lock().read_line(&mut buf) {
                    Ok(0) => break,
                    Ok(_) => {
                        let line = buf.trim().to_string();
                        if cmd_tx.send(line).is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        });

        Self {
            host: host.to_string(),
            port,
            stream: None,
            writer: None,
            frame_rx: None,
            _reader: None,
            cmd_rx,
            listening: Arc::new(AtomicBool::new(true)),
            authenticated: false,
            token: None,
            user_id: None,
            conversations: Vec::new(),
        }
    }

    fn repl(&mut self) {
        let mut stdout = io::stdout();

        self.print_help(&mut stdout);

        loop {
            // Check for incoming frames first (non-blocking)
            self.drain_and_print_frames(&mut stdout);

            // Check for commands from stdin thread
            match self.cmd_rx.try_recv() {
                Ok(line) => {
                    if line.is_empty() {
                        continue;
                    }
                    if !self.handle_command(&line, &mut stdout) {
                        break;
                    }
                }
                Err(mpsc::TryRecvError::Empty) => {
                    // No command available, sleep briefly to avoid busy-wait
                    thread::sleep(Duration::from_millis(50));
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    let _ = writeln!(stdout, "stdin reader disconnected");
                    break;
                }
            }
        }
    }

    fn print_help(&self, w: &mut impl Write) {
        let _ = writeln!(w, "Commands:");
        let _ = writeln!(w, "  connect              Connect to {}:{}", self.host, self.port);
        let _ = writeln!(w, "  hello <user_id>      Authenticate (send Hello)");
        let _ = writeln!(w, "  auth-response <token_hex>  Respond to auth challenge");
        let _ = writeln!(w, "  create-conv <members> Create conversation (comma-separated)");
        let _ = writeln!(w, "  convs                List known conversations (local cache)");
        let _ = writeln!(w, "  list-convs           List conversations from the server");
        let _ = writeln!(w, "  send <conv_hex> <msg> Send a message");
        let _ = writeln!(w, "  ping                 Send a ping");
        let _ = writeln!(w, "  listen on|off        Toggle listening for incoming frames");
        let _ = writeln!(w, "  status               Show connection/auth status");
        let _ = writeln!(w, "  help                 Print this help");
        let _ = writeln!(w, "  quit|exit            Disconnect and exit");
    }

    fn handle_command(&mut self, line: &str, w: &mut impl Write) -> bool {
        let parts: Vec<&str> = line.splitn(3, ' ').collect();
        let cmd = parts[0];

        match cmd {
            "connect" => self.cmd_connect(w),
            "hello" => {
                if parts.len() < 2 {
                    let _ = writeln!(w, "usage: hello <user_id>");
                    return true;
                }
                self.cmd_hello(parts[1], w)
            }
            "auth-response" => {
                if parts.len() < 2 {
                    let _ = writeln!(w, "usage: auth-response <token_hex>");
                    return true;
                }
                self.cmd_auth_response(parts[1], w)
            }
            "create-conv" => {
                if parts.len() < 2 {
                    let _ = writeln!(w, "usage: create-conv <member1,member2,...>");
                    return true;
                }
                self.cmd_create_conv(parts[1], w)
            }
            "convs" => self.cmd_convs(w),
            "list-convs" => self.cmd_list_convs(w),
            "send" => {
                if parts.len() < 3 {
                    let _ = writeln!(w, "usage: send <conv_hex> <message>");
                    return true;
                }
                self.cmd_send(parts[1], parts[2], w)
            }
            "ping" => self.cmd_ping(w),
            "listen" => {
                if parts.len() < 2 {
                    let _ = writeln!(w, "usage: listen on|off");
                    return true;
                }
                self.cmd_listen(parts[1], w)
            }
            "status" => self.cmd_status(w),
            "help" => {
                self.print_help(w);
                true
            }
            "quit" | "exit" => {
                self.cmd_disconnect(w);
                false
            }
            _ => {
                let _ = writeln!(w, "unknown command: {}. type 'help'", cmd);
                true
            }
        }
    }

    fn cmd_connect(&mut self, w: &mut impl Write) -> bool {
        if self.stream.is_some() {
            let _ = writeln!(w, "already connected, disconnect first");
            return true;
        }

        let addr = format!("{}:{}", self.host, self.port);
        let stream = match TcpStream::connect(&addr) {
            Ok(s) => s,
            Err(e) => {
                let _ = writeln!(w, "connect failed: {}", e);
                return true;
            }
        };
        let _ = stream.set_read_timeout(Some(Duration::from_secs(30)));

        let reader = match stream.try_clone() {
            Ok(r) => r,
            Err(e) => {
                let _ = writeln!(w, "clone failed: {}", e);
                return true;
            }
        };

        let (tx, rx) = mpsc::channel();
        let listening = self.listening.clone();
        let handle = thread::spawn(move || read_frames(reader, tx, listening));

        self.stream = Some(stream.try_clone().unwrap());
        self.writer = Some(stream);
        self.frame_rx = Some(rx);
        self._reader = Some(handle);
        self.authenticated = false;
        self.token = None;
        self.user_id = None;

        let _ = writeln!(w, "connected to {}", addr);
        true
    }

    fn cmd_disconnect(&mut self, w: &mut impl Write) {
        if let Some(ref mut stream) = self.writer {
            let goodbye = protocol::encode(MsgType::Goodbye, 0, &[]);
            let _ = stream.write_all(&goodbye);
            let _ = stream.flush();
        }
        self.stream = None;
        self.writer = None;
        self.frame_rx = None;
        self._reader = None;
        self.authenticated = false;
        self.token = None;
        self.user_id = None;
        let _ = writeln!(w, "disconnected");
    }

    fn cmd_hello(&mut self, user_id: &str, w: &mut impl Write) -> bool {
        let writer = match self.writer.as_ref() {
            Some(s) => s,
            None => {
                let _ = writeln!(w, "not connected. use 'connect' first");
                return true;
            }
        };

        let frame = protocol::encode(MsgType::Hello, 0, user_id.as_bytes());
        if let Err(e) = writer_write(writer, &frame) {
            let _ = writeln!(w, "write error: {}", e);
            return true;
        }
        self.user_id = Some(user_id.to_string());

        match self.next_frame() {
            Ok(f) => {
                let _ = writeln!(w, "received: {}", format_frame(&f));
                match f.msg_type {
                    MsgType::AuthOk => {
                        self.token = Some(f.body.to_vec());
                        self.authenticated = true;
                        let _ = writeln!(w, "authenticated (token: {})", hex::encode(&f.body));
                    }
                    MsgType::AuthChallenge => {
                        let _ = writeln!(w, "auth challenged — send 'auth-response <token_hex>'");
                    }
                    _ => {
                        let _ = writeln!(w, "unexpected response to hello");
                    }
                }
            }
            Err(e) => {
                let _ = writeln!(w, "recv error: {}", e);
            }
        }
        true
    }

    fn cmd_auth_response(&mut self, token_hex: &str, w: &mut impl Write) -> bool {
        let writer = match self.writer.as_ref() {
            Some(s) => s,
            None => {
                let _ = writeln!(w, "not connected");
                return true;
            }
        };

        let token = match hex::decode(token_hex) {
            Ok(t) => t,
            Err(e) => {
                let _ = writeln!(w, "invalid hex: {}", e);
                return true;
            }
        };

        let user_id = self.user_id.as_deref().unwrap_or_default();
        let mut body = Vec::new();
        body.extend_from_slice(user_id.as_bytes());
        body.push(b'\n');
        body.extend_from_slice(&token);

        let frame = protocol::encode(MsgType::AuthResponse, 0, &body);
        if let Err(e) = writer_write(writer, &frame) {
            let _ = writeln!(w, "write error: {}", e);
            return true;
        }

        match self.next_frame() {
            Ok(f) => {
                let _ = writeln!(w, "received: {}", format_frame(&f));
                if f.msg_type == MsgType::AuthOk {
                    self.token = Some(f.body.to_vec());
                    self.authenticated = true;
                    let _ = writeln!(w, "authenticated");
                }
            }
            Err(e) => {
                let _ = writeln!(w, "recv error: {}", e);
            }
        }
        true
    }

    fn cmd_create_conv(&mut self, members: &str, w: &mut impl Write) -> bool {
        if self.writer.is_none() {
            let _ = writeln!(w, "not connected");
            return true;
        }
        if !self.authenticated {
            let _ = writeln!(w, "not authenticated, send 'hello <user_id>' first");
            return true;
        }

        let writer = self.writer.as_ref().unwrap();
        let frame = protocol::encode(MsgType::CreateConv, 0, members.as_bytes());
        if let Err(e) = writer_write(writer, &frame) {
            let _ = writeln!(w, "write error: {}", e);
            return true;
        }

        loop {
            match self.next_frame() {
                Ok(f) => {
                    let _ = writeln!(w, "received: {}", format_frame(&f));
                    if f.msg_type == MsgType::ConvCreated {
                        let conv_hex = hex::encode(&f.body);
                        let _ = writeln!(w, "conv id: {}", conv_hex);
                        self.conversations.push(ConvInfo {
                            id: f.body.to_vec(),
                            members: members.to_string(),
                        });
                        break;
                    }
                }
                Err(e) => {
                    let _ = writeln!(w, "recv error: {}", e);
                    break;
                }
            }
        }
        true
    }

    fn cmd_convs(&mut self, w: &mut impl Write) -> bool {
        if self.conversations.is_empty() {
            let _ = writeln!(w, "no conversations yet. create one with 'create-conv <members>'");
        } else {
            let _ = writeln!(w, "Conversations:");
            for (i, conv) in self.conversations.iter().enumerate() {
                let _ = writeln!(w, "  {}: {} (members: {})", i, hex::encode(&conv.id), conv.members);
            }
        }
        true
    }

    fn cmd_list_convs(&mut self, w: &mut impl Write) -> bool {
        let writer = match self.writer.as_ref() {
            Some(s) => s,
            None => {
                let _ = writeln!(w, "not connected");
                return true;
            }
        };
        if !self.authenticated {
            let _ = writeln!(w, "not authenticated");
            return true;
        }

        let frame = protocol::encode(MsgType::ListConvs, 0, &[]);
        if let Err(e) = writer_write(writer, &frame) {
            let _ = writeln!(w, "write error: {}", e);
            return true;
        }

        match self.next_frame() {
            Ok(f) => {
                if f.msg_type == MsgType::ConvsResp {
                    let chunk = 8;
                    if f.body.is_empty() {
                        let _ = writeln!(w, "no conversations on server");
                    } else {
                        let count = f.body.len() / chunk;
                        let _ = writeln!(w, "Conversations ({}):", count);
                        for i in 0..count {
                            let conv_id = &f.body[i * chunk..(i + 1) * chunk];
                            let _ = writeln!(w, "  {}: {}", i, hex::encode(conv_id));
                            if !self.conversations.iter().any(|c| c.id == conv_id) {
                                self.conversations.push(ConvInfo {
                                    id: conv_id.to_vec(),
                                    members: "?".to_string(),
                                });
                            }
                        }
                    }
                } else {
                    let _ = writeln!(w, "unexpected: {}", format_frame(&f));
                }
            }
            Err(e) => {
                let _ = writeln!(w, "recv error: {}", e);
            }
        }
        true
    }

    fn cmd_send(&mut self, conv_hex: &str, msg: &str, w: &mut impl Write) -> bool {
        if self.writer.is_none() {
            let _ = writeln!(w, "not connected");
            return true;
        }
        if !self.authenticated {
            let _ = writeln!(w, "not authenticated");
            return true;
        }

        let conv_id = match hex::decode(conv_hex) {
            Ok(id) => id,
            Err(e) => {
                let _ = writeln!(w, "invalid conv hex: {}", e);
                return true;
            }
        };

        let writer = self.writer.as_ref().unwrap();
        let mut body = Vec::new();
        body.extend_from_slice(&conv_id);
        body.push(b'\n');
        body.extend_from_slice(msg.as_bytes());

        let frame = protocol::encode(MsgType::Send, 0, &body);
        if let Err(e) = writer_write(writer, &frame) {
            let _ = writeln!(w, "write error: {}", e);
            return true;
        }

        let _ = writeln!(w, "sent");
        true
    }

    fn cmd_ping(&mut self, w: &mut impl Write) -> bool {
        if self.writer.is_none() {
            let _ = writeln!(w, "not connected");
            return true;
        }

        let writer = self.writer.as_ref().unwrap();
        let frame = protocol::encode(MsgType::Ping, 0, &[]);
        if let Err(e) = writer_write(writer, &frame) {
            let _ = writeln!(w, "write error: {}", e);
            return true;
        }
        let _ = writeln!(w, "ping sent");
        true
    }

    fn cmd_listen(&mut self, arg: &str, w: &mut impl Write) -> bool {
        match arg {
            "on" => {
                self.listening.store(true, Ordering::Relaxed);
                let _ = writeln!(w, "listening enabled");
            }
            "off" => {
                self.listening.store(false, Ordering::Relaxed);
                let _ = writeln!(w, "listening disabled");
            }
            _ => {
                let _ = writeln!(w, "usage: listen on|off");
            }
        }
        true
    }

    fn cmd_status(&mut self, w: &mut impl Write) -> bool {
        let _ = writeln!(w, "connected: {}", self.stream.is_some());
        let _ = writeln!(w, "authenticated: {}", self.authenticated);
        if let Some(ref uid) = self.user_id {
            let _ = writeln!(w, "user: {}", uid);
        }
        if let Some(ref tok) = self.token {
            let _ = writeln!(w, "token: {}", hex::encode(tok));
        }
        let _ = writeln!(w, "listening: {}", self.listening.load(Ordering::Relaxed));
        let _ = writeln!(w, "conversations: {}", self.conversations.len());
        true
    }

    fn next_frame(&mut self) -> Result<OwnedFrame, String> {
        let rx = self.frame_rx.as_ref().ok_or("no frame receiver")?;
        match rx.recv_timeout(Duration::from_secs(10)) {
            Ok(f) => Ok(f),
            Err(mpsc::RecvTimeoutError::Timeout) => Err("timeout waiting for frame".into()),
            Err(mpsc::RecvTimeoutError::Disconnected) => Err("connection lost".into()),
        }
    }

    fn drain_and_print_frames(&mut self, w: &mut impl Write) {
        if !self.listening.load(Ordering::Relaxed) {
            return;
        }
        let rx = match self.frame_rx.as_ref() {
            Some(rx) => rx,
            None => return,
        };
        loop {
            match rx.try_recv() {
                Ok(f) => {
                    let formatted = format_frame(&f);
                    let _ = writeln!(w, "\r[incoming] {}", formatted);
                    // Track conversations from incoming Send frames
                    if f.msg_type == MsgType::Send {
                        if let Some(sep) = f.body.iter().position(|&b| b == b'\n') {
                            let conv_id = f.body[..sep].to_vec();
                            if !self.conversations.iter().any(|c| c.id == conv_id) {
                                self.conversations.push(ConvInfo {
                                    id: conv_id,
                                    members: "?".to_string(),
                                });
                            }
                        }
                    }
                    if f.msg_type == MsgType::ConvCreated {
                        let conv_hex = hex::encode(&f.body);
                        if !self.conversations.iter().any(|c| hex::encode(&c.id) == conv_hex) {
                            self.conversations.push(ConvInfo {
                                id: f.body.to_vec(),
                                members: "?".to_string(),
                            });
                        }
                    }
                }
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => break,
            }
        }
    }

}

fn writer_write(stream: &TcpStream, data: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let mut s = stream;
    s.write_all(data)?;
    s.flush()
}

fn read_frames(mut reader: TcpStream, tx: mpsc::Sender<OwnedFrame>, _listening: Arc<AtomicBool>) {
    use std::io::{ErrorKind, Read};

    let mut buf = Vec::with_capacity(65536);
    let mut offset = 0;

    loop {
        let mut tmp = [0u8; 8192];
        let n = match reader.read(&mut tmp) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                thread::sleep(Duration::from_millis(10));
                continue;
            }
            Err(e) => {
                let msg = format!("reader: {}", e);
                let _ = tx.send(OwnedFrame::from_borrowed(&Frame::new(
                    MsgType::Error, msg.as_bytes(),
                )));
                break;
            }
        };
        buf.extend_from_slice(&tmp[..n]);

        loop {
            let consumed;
            let owned = {
                let slice = &buf[offset..];
                match protocol::decode(slice) {
                    Decode::Complete { frame, consumed: c } => {
                        consumed = c;
                        OwnedFrame::from_borrowed(&frame)
                    }
                    Decode::Need => break,
                    Decode::Err(e) => {
                        let msg = format!("protocol error: {:?}", e);
                        let _ = tx.send(OwnedFrame::from_borrowed(&Frame::new(
                            MsgType::Error, msg.as_bytes(),
                        )));
                        offset += 1;
                        if offset > 4096 {
                            buf.drain(..offset);
                            offset = 0;
                        }
                        continue;
                    }
                }
            };
            offset += consumed;

            if tx.send(owned).is_err() {
                return;
            }
        }

        if offset > 4096 {
            buf.drain(..offset);
            offset = 0;
        }
    }
}

fn format_frame(f: &OwnedFrame) -> String {
    match f.msg_type {
        MsgType::AuthOk => format!("AuthOk ({} bytes)", f.body.len()),
        MsgType::AuthChallenge => format!(
            "AuthChallenge: {}",
            String::from_utf8_lossy(&f.body)
        ),
        MsgType::AuthFail => format!("AuthFail: {}", String::from_utf8_lossy(&f.body)),
        MsgType::ConvCreated => format!("ConvCreated: {}", hex::encode(&f.body)),
        MsgType::ConvInvite => format!("ConvInvite: {}", hex::encode(&f.body)),
        MsgType::Delivered => {
            if f.body.len() >= 8 {
                let seq = u64::from_le_bytes(f.body[..8].try_into().unwrap());
                format!("Delivered (seq: {})", seq)
            } else {
                format!("Delivered (short body)")
            }
        }
        MsgType::Send => {
            let sep = match f.body.iter().position(|&b| b == b'\n') {
                Some(p) => p,
                None => return format!("Send: {} bytes", f.body.len()),
            };
            let conv_hex = hex::encode(&f.body[..sep]);
            let after_sep = &f.body[sep + 1..];
            if after_sep.len() < 8 {
                return format!("Send(conv={}, short body)", conv_hex);
            }
            let _seq = u64::from_le_bytes(after_sep[..8].try_into().unwrap());
            let msg_bytes = &after_sep[8..];
            let from_sep = msg_bytes.iter().position(|&b| b == b'\n');
            let (sender, msg) = match from_sep {
                Some(p) => (
                    String::from_utf8_lossy(&msg_bytes[..p]),
                    String::from_utf8_lossy(&msg_bytes[p + 1..]),
                ),
                None => (String::from_utf8_lossy(msg_bytes), "".into()),
            };
            format!("[{}] {}: {}", &conv_hex[..8], sender, msg)
        }
        MsgType::Pong => "Pong".to_string(),
        MsgType::Error => format!("Error: {}", String::from_utf8_lossy(&f.body)),
        MsgType::Goodbye => "Goodbye".to_string(),
        MsgType::Presence => format!("Presence: {}", String::from_utf8_lossy(&f.body)),
        MsgType::Typing => format!("Typing: {}", String::from_utf8_lossy(&f.body)),
        MsgType::HistoryResp => format!("HistoryResp ({} bytes)", f.body.len()),
        MsgType::InboxResp => format!("InboxResp ({} bytes)", f.body.len()),
        MsgType::ConvsResp => {
            let count = f.body.len() / 8;
            format!("ConvsResp ({} conversations)", count)
        }
        _ => format!("{:?} ({} bytes)", f.msg_type, f.body.len()),
    }
}