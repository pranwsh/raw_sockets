use std::collections::VecDeque;
use std::io::{self, BufRead, Write};
use std::net::TcpStream;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use protocol::{self, MsgType, OwnedFrame, Decode, Frame};

pub fn run(host: &str, port: u16) {
    let mut sh = Shell::new(host, port);
    sh.repl();
}

struct Shell {
    host: String,
    port: u16,
    stream: Option<TcpStream>,
    writer: Option<TcpStream>,
    frame_rx: Option<mpsc::Receiver<OwnedFrame>>,
    _reader: Option<thread::JoinHandle<()>>,
    frame_queue: VecDeque<OwnedFrame>,
    listening: bool,
    authenticated: bool,
    token: Option<Vec<u8>>,
    user_id: Option<String>,
}

impl Shell {
    fn new(host: &str, port: u16) -> Self {
        Self {
            host: host.to_string(),
            port,
            stream: None,
            writer: None,
            frame_rx: None,
            _reader: None,
            frame_queue: VecDeque::new(),
            listening: false,
            authenticated: false,
            token: None,
            user_id: None,
        }
    }

    fn repl(&mut self) {
        let stdin = io::stdin();
        let mut stdout = io::stdout();

        self.print_help(&mut stdout);

        loop {
            let _ = stdout.write(b"msgcli> ");
            let _ = stdout.flush();

            let mut line = String::new();
            match stdin.lock().read_line(&mut line) {
                Ok(0) => break,
                Ok(_) => {}
                Err(e) => {
                    eprintln!("read error: {}", e);
                    break;
                }
            }

            let line = line.trim();
            if line.is_empty() {
                continue;
            }

            if !self.handle_command(line, &mut stdout) {
                break;
            }
        }
    }

    fn print_help(&self, w: &mut impl Write) {
        let _ = writeln!(w, "Commands:");
        let _ = writeln!(w, "  connect              Connect to {}:{}", self.host, self.port);
        let _ = writeln!(w, "  hello <user_id>      Authenticate (send Hello)");
        let _ = writeln!(w, "  auth-response <token_hex>  Respond to auth challenge");
        let _ = writeln!(w, "  create-conv <members> Create conversation (comma-separated)");
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
        let handle = thread::spawn(move || read_frames(reader, tx));

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
        let writer = match self.writer.as_ref() {
            Some(s) => s,
            None => {
                let _ = writeln!(w, "not connected");
                return true;
            }
        };

        if !self.authenticated {
            let _ = writeln!(w, "not authenticated, send 'hello <user_id>' first");
            return true;
        }

        let frame = protocol::encode(MsgType::CreateConv, 0, members.as_bytes());
        if let Err(e) = writer_write(writer, &frame) {
            let _ = writeln!(w, "write error: {}", e);
            return true;
        }

        match self.next_frame() {
            Ok(f) => {
                let _ = writeln!(w, "received: {}", format_frame(&f));
                if f.msg_type == MsgType::ConvCreated {
                    let _ = writeln!(w, "conv id: {}", hex::encode(&f.body));
                }
            }
            Err(e) => {
                let _ = writeln!(w, "recv error: {}", e);
            }
        }
        true
    }

    fn cmd_send(&mut self, conv_hex: &str, msg: &str, w: &mut impl Write) -> bool {
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

        let conv_id = match hex::decode(conv_hex) {
            Ok(id) => id,
            Err(e) => {
                let _ = writeln!(w, "invalid conv hex: {}", e);
                return true;
            }
        };

        let mut body = Vec::new();
        body.extend_from_slice(&conv_id);
        body.push(b'\n');
        body.extend_from_slice(msg.as_bytes());

        let frame = protocol::encode(MsgType::Send, 0, &body);
        if let Err(e) = writer_write(writer, &frame) {
            let _ = writeln!(w, "write error: {}", e);
            return true;
        }

        let _ = writeln!(w, "sent, waiting for delivery...");

        match self.next_frame() {
            Ok(f) => {
                let _ = writeln!(w, "received: {}", format_frame(&f));
            }
            Err(e) => {
                let _ = writeln!(w, "recv error: {}", e);
            }
        }
        true
    }

    fn cmd_ping(&mut self, w: &mut impl Write) -> bool {
        let writer = match self.writer.as_ref() {
            Some(s) => s,
            None => {
                let _ = writeln!(w, "not connected");
                return true;
            }
        };

        let frame = protocol::encode(MsgType::Ping, 0, &[]);
        if let Err(e) = writer_write(writer, &frame) {
            let _ = writeln!(w, "write error: {}", e);
            return true;
        }

        match self.next_frame() {
            Ok(f) => {
                let _ = writeln!(w, "received: {}", format_frame(&f));
            }
            Err(e) => {
                let _ = writeln!(w, "recv error: {}", e);
            }
        }
        true
    }

    fn cmd_listen(&mut self, arg: &str, w: &mut impl Write) -> bool {
        match arg {
            "on" => {
                self.listening = true;
                let _ = writeln!(w, "listening enabled. incoming frames will be printed.");
                self.drain_frames(w);
            }
            "off" => {
                self.listening = false;
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
        let _ = writeln!(w, "listening: {}", self.listening);
        true
    }

    fn next_frame(&mut self) -> Result<OwnedFrame, String> {
        if let Some(f) = self.frame_queue.pop_front() {
            return Ok(f);
        }
        let rx = self.frame_rx.as_ref().ok_or("no frame receiver")?;
        match rx.recv_timeout(Duration::from_secs(10)) {
            Ok(f) => Ok(f),
            Err(mpsc::RecvTimeoutError::Timeout) => Err("timeout waiting for frame".into()),
            Err(mpsc::RecvTimeoutError::Disconnected) => Err("connection lost".into()),
        }
    }

    fn drain_frames(&mut self, w: &mut impl Write) {
        if !self.listening {
            return;
        }
        if let Some(ref rx) = self.frame_rx {
            loop {
                match rx.try_recv() {
                    Ok(f) => {
                        let _ = writeln!(w, "[incoming] {}", format_frame(&f));
                    }
                    Err(mpsc::TryRecvError::Empty) => break,
                    Err(mpsc::TryRecvError::Disconnected) => break,
                }
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

fn read_frames(mut reader: TcpStream, tx: mpsc::Sender<OwnedFrame>) {
    use std::io::Read;

    let mut buf = Vec::with_capacity(65536);
    let mut offset = 0;

    loop {
        let mut tmp = [0u8; 8192];
        let n = match reader.read(&mut tmp) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(10));
                continue;
            }
            Err(_) => break,
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
                    Decode::Err(_) => {
                        let _ = tx.send(OwnedFrame::from_borrowed(&Frame::new(
                            MsgType::Error,
                            b"decode error",
                        )));
                        return;
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
            let parts: Vec<&[u8]> = f.body.splitn(4, |&b| b == b'\n').collect();
            if parts.len() >= 4 {
                let conv_hex = hex::encode(parts[0]);
                let seq = u64::from_le_bytes(parts[1][..8].try_into().unwrap());
                let sender = String::from_utf8_lossy(parts[2]);
                let msg = String::from_utf8_lossy(parts[3]);
                format!("Send(conv={}, seq={}, from={}, msg={})", conv_hex, seq, sender, msg)
            } else if parts.len() >= 2 {
                let conv_hex = hex::encode(parts[0]);
                let rest = String::from_utf8_lossy(parts[1]);
                format!("Send(conv={}, rest={})", conv_hex, rest)
            } else {
                format!("Send: {} bytes", f.body.len())
            }
        }
        MsgType::Pong => "Pong".to_string(),
        MsgType::Error => format!("Error: {}", String::from_utf8_lossy(&f.body)),
        MsgType::Goodbye => "Goodbye".to_string(),
        MsgType::Presence => format!("Presence: {}", String::from_utf8_lossy(&f.body)),
        MsgType::Typing => format!("Typing: {}", String::from_utf8_lossy(&f.body)),
        MsgType::HistoryResp => format!("HistoryResp ({} bytes)", f.body.len()),
        MsgType::InboxResp => format!("InboxResp ({} bytes)", f.body.len()),
        _ => format!("{:?} ({} bytes)", f.msg_type, f.body.len()),
    }
}
