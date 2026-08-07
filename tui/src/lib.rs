//! A reusable, term_render-based chat TUI frontend.
//!
//! This crate is a *pure frontend*: it owns the screen and keyboard, and it
//! speaks a small, generic messaging model ([`UiAction`]/[`UiEvent`]) to an
//! application-supplied [`ChatClient`]. It has no knowledge of any particular
//! wire protocol. An app wires itself in by implementing [`ChatClient`] and
//! calling [`run_app`].
//!
//! See the `msgcli` binary in the `socket_messaging` workspace for a reference
//! integration that adapts this trait to its `msgclient` channel client.

use std::io;
use term_render::event_handler::KeyCode;
use term_render::render::{Colorize, ColorType, Span, Window};
use tokio::runtime::Runtime;

// ---------------------------------------------------------------------------
// generic messaging model — the only contract the app must satisfy

/// A high-level operation the TUI asks the app to perform.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UiAction {
    /// authenticate (the account is created if it doesn't exist yet)
    Login { user: String, password: String },
    /// create a conversation with the given members
    CreateConv { members: Vec<String> },
    /// list the conversations the authenticated user is a member of
    ListConvs,
    /// send a message into a conversation
    Send { conv: Vec<u8>, text: String },
    /// send a keepalive ping
    Ping,
    /// send a clean goodbye and close the connection
    Quit,
}

/// A high-level notification the app produces for the TUI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UiEvent {
    /// connection established (the transport is up)
    Connected,
    /// authentication succeeded; `created` is true when a new account was made
    LoginOk { created: bool },
    /// authentication was rejected
    LoginFail { reason: String },
    /// the server created a conversation and returned its id
    ConvCreated { id: Vec<u8> },
    /// the server's response to [`UiAction::ListConvs`]
    Convs { ids: Vec<Vec<u8>> },
    /// an inbound message (delivered to this connection)
    Message { conv: Vec<u8>, from: String, seq: u64, text: String },
    /// our [`UiAction::Send`] was accepted with a sequence number
    Delivered { seq: u64 },
    /// reply to [`UiAction::Ping`]
    Pong,
    /// the server reported an error
    Error { msg: String },
    /// the connection was closed
    Disconnected { reason: String },
}

/// The adapter the app implements so the TUI never touches the wire.
///
/// All socket I/O, framing, and protocol live behind this trait. The TUI
/// drives it by calling [`send`](ChatClient::send) and periodically draining
/// [`poll_events`](ChatClient::poll_events) on each rendered frame.
pub trait ChatClient: std::fmt::Debug {
    /// enqueue an action; returns false if the connection is gone
    fn send(&self, action: UiAction) -> bool;
    /// drain any pending events into a `Vec` (non-blocking)
    fn poll_events(&self) -> Vec<UiEvent>;
}

// ---------------------------------------------------------------------------
// entry point

/// Launch the TUI with the supplied client adapter.
///
/// `client` provides the connection; `user`/`password` cause an initial
/// [`UiAction::Login`] (skip by passing empty strings); `conv_hex` optionally
/// pre-seeds a conversation (hex-encoded id) and selects it.
///
/// This blocks until the user quits (Esc or `/quit`), running the term_render
/// event/render loop on a tokio runtime.
pub fn run_app(
    client: Box<dyn ChatClient>,
    user: &str,
    password: &str,
    conv_hex: Option<&str>,
) -> io::Result<()> {
    if !user.is_empty() {
        let _ = client.send(UiAction::Login { user: user.to_string(), password: password.to_string() });
    }

    let mut ui = UiState {
        connected: true,
        authenticated: false,
        user: user.to_string(),
        convs: Vec::new(),
        selected: None,
        focus: Focus::Input,
        input: String::new(),
        status: if user.is_empty() { "no credentials given".into() } else { "authenticating...".into() },
        quit: false,
        dirty: true,
    };
    if let Some(id) = decode_conv_hex_opt(conv_hex) {
        ui.convs.push(Conv::new(id));
        ui.selected = Some(0);
    }

    let data = AppData { client: Some(client), ui };
    let rt = Runtime::new()?;
    rt.block_on(async {
        let mut app = term_render::App::<AppData>::new()?;
        build_windows(&mut app);
        app.scene = None;
        let _ = app.run(data, tick).await;
        Ok::<(), io::Error>(())
    })
}

// decode an optional hex conversation id; returns None for "" or malformed hex
fn decode_conv_hex_opt(conv_hex: Option<&str>) -> Option<Vec<u8>> {
    let h = conv_hex?;
    let h = h.trim();
    if h.is_empty() {
        return None;
    }
    hex::decode(h).ok()
}

// ---------------------------------------------------------------------------
// app state

#[derive(Debug)]
struct AppData {
    client: Option<Box<dyn ChatClient>>,
    ui: UiState,
}

#[derive(Debug)]
struct UiState {
    connected: bool,
    authenticated: bool,
    user: String,
    convs: Vec<Conv>,
    selected: Option<usize>,
    focus: Focus,
    input: String,
    status: String,
    quit: bool,
    dirty: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Focus {
    Input,
    Convs,
}

#[derive(Debug)]
struct Conv {
    id: Vec<u8>,
    label: String,
    messages: Vec<Message>,
}

impl Conv {
    fn new(id: Vec<u8>) -> Self {
        let label = hex::encode(&id);
        let label = if label.len() > 8 { label[..8].to_string() } else { label };
        Conv { id, label, messages: Vec::new() }
    }
}

#[derive(Debug)]
struct Message {
    from: String,
    text: String,
}

// ---------------------------------------------------------------------------
// per-frame callback (called by term_render every ~10ms)

fn tick(data: &mut AppData, app: &mut term_render::App<AppData>) -> Result<bool, ()> {
    // 1. consume client events
    let pending: Vec<UiEvent> = match data.client.as_ref() {
        Some(client) => client.poll_events(),
        None => Vec::new(),
    };
    for ev in pending {
        apply_event(data, ev);
    }

    // 2. keyboard
    if handle_keys(data, app) {
        data.ui.quit = true;
    }
    if data.ui.quit {
        if let Some(c) = data.client.take() {
            let _ = c.send(UiAction::Quit);
        }
        return Ok(true);
    }

    // 3. render
    render(data, app);

    Ok(false)
}

fn apply_event(data: &mut AppData, ev: UiEvent) {
    match ev {
        UiEvent::Connected => {
            data.ui.connected = true;
            data.ui.dirty = true;
        }
        UiEvent::LoginOk { created } => {
            data.ui.authenticated = true;
            data.ui.status = if created { "authenticated (new account)".into() } else { "authenticated".into() };
            if let Some(c) = data.client.as_ref() {
                c.send(UiAction::ListConvs);
            }
            data.ui.dirty = true;
        }
        UiEvent::LoginFail { reason } => {
            data.ui.authenticated = false;
            data.ui.status = format!("auth failed: {reason}");
            data.ui.dirty = true;
        }
        UiEvent::ConvCreated { id } => {
            let idx = match data.ui.convs.iter().position(|c| c.id == id) {
                Some(i) => i,
                None => {
                    data.ui.convs.push(Conv::new(id));
                    data.ui.convs.len() - 1
                }
            };
            data.ui.selected = Some(idx);
            data.ui.focus = Focus::Input;
            data.ui.status = "conversation created".into();
            data.ui.dirty = true;
        }
        UiEvent::Convs { ids } => {
            for id in ids {
                if !data.ui.convs.iter().any(|c| c.id == id) {
                    data.ui.convs.push(Conv::new(id));
                }
            }
            if data.ui.selected.is_none() && !data.ui.convs.is_empty() {
                data.ui.selected = Some(0);
            }
            data.ui.status = format!("{} conversations", data.ui.convs.len());
            data.ui.dirty = true;
        }
        UiEvent::Message { conv, from, text, .. } => {
            let idx = match data.ui.convs.iter().position(|c| c.id == conv) {
                Some(i) => i,
                None => {
                    data.ui.convs.push(Conv::new(conv));
                    data.ui.convs.len() - 1
                }
            };
            data.ui.convs[idx].messages.push(Message { from, text });
            // jump to a conversation that just received a message so inbound
            // messages are immediately visible without manual navigation
            if data.ui.selected.is_none() {
                data.ui.selected = Some(idx);
            }
            data.ui.dirty = true;
        }
        UiEvent::Delivered { seq } => {
            data.ui.status = format!("delivered (seq {seq})");
            data.ui.dirty = true;
        }
        UiEvent::Pong => {
            data.ui.status = "pong".into();
            data.ui.dirty = true;
        }
        UiEvent::Error { msg } => {
            data.ui.status = format!("error: {msg}");
            data.ui.dirty = true;
        }
        UiEvent::Disconnected { reason } => {
            data.ui.connected = false;
            data.ui.authenticated = false;
            data.ui.status = format!("disconnected: {reason}");
            data.ui.dirty = true;
        }
    }
}

fn handle_keys(data: &mut AppData, app: &mut term_render::App<AppData>) -> bool {
    let events = app.events.read();

    if events.contains_key_code(KeyCode::Escape) {
        return true;
    }

    match data.ui.focus {
        Focus::Input => {
            for ch in &events.char_events {
                data.ui.input.push(*ch);
                data.ui.dirty = true;
            }
            if events.contains_key_code(KeyCode::Delete) {
                data.ui.input.pop();
                data.ui.dirty = true;
            }
            if events.contains_key_code(KeyCode::Return) {
                let line = data.ui.input.trim().to_string();
                data.ui.input.clear();
                data.ui.dirty = true;
                submit(data, &line);
            }
        }
        Focus::Convs => {
            if events.contains_key_code(KeyCode::Up) {
                move_sel(data, -1);
            }
            if events.contains_key_code(KeyCode::Down) {
                move_sel(data, 1);
            }
            if events.contains_key_code(KeyCode::Return) && data.ui.selected.is_some() {
                data.ui.focus = Focus::Input;
                data.ui.dirty = true;
            }
        }
    }

    if events.contains_key_code(KeyCode::Tab) {
        data.ui.focus = match data.ui.focus {
            Focus::Input => Focus::Convs,
            Focus::Convs => Focus::Input,
        };
        data.ui.dirty = true;
    }

    false
}

fn move_sel(data: &mut AppData, delta: isize) {
    let n = data.ui.convs.len();
    if n == 0 {
        return;
    }
    let cur = data.ui.selected.unwrap_or(0) as isize;
    let next = (cur + delta).rem_euclid(n as isize) as usize;
    data.ui.selected = Some(next);
    data.ui.dirty = true;
}

fn submit(data: &mut AppData, line: &str) {
    if line.is_empty() {
        return;
    }
    if let Some(cmd) = line.strip_prefix('/') {
        let mut parts = cmd.splitn(2, ' ');
        match parts.next().unwrap_or("") {
            "create" => {
                let members: Vec<String> = parts
                    .next()
                    .unwrap_or("")
                    .split(',')
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
                    .collect();
                if members.is_empty() {
                    data.ui.status = "usage: /create member1,member2".into();
                } else if let Some(c) = data.client.as_ref() {
                    c.send(UiAction::CreateConv { members });
                    data.ui.status = "creating conversation...".into();
                }
            }
            "list" => {
                if let Some(c) = data.client.as_ref() {
                    c.send(UiAction::ListConvs);
                    data.ui.status = "listing conversations...".into();
                }
            }
            "ping" => {
                if let Some(c) = data.client.as_ref() {
                    c.send(UiAction::Ping);
                    data.ui.status = "ping sent".into();
                }
            }
            "quit" | "exit" => {
                data.ui.quit = true;
            }
            other => {
                data.ui.status = format!("unknown command /{other}");
            }
        }
        data.ui.dirty = true;
        return;
    }

    match data.ui.selected {
        Some(i) => {
            let conv = data.ui.convs[i].id.clone();
            if let Some(c) = data.client.as_ref() {
                c.send(UiAction::Send { conv, text: line.to_string() });
            }
            // optimistic echo so the sender sees their own message immediately
            data.ui.convs[i].messages.push(Message { from: data.ui.user.clone(), text: line.to_string() });
            data.ui.status = "sent".into();
        }
        None => {
            data.ui.status = "select a conversation first (Tab to convs)".into();
        }
    }
    data.ui.dirty = true;
}

// ---------------------------------------------------------------------------
// windows

const INPUT_H: u16 = 3;
const STATUS_H: u16 = 1;

fn conv_width(w: u16) -> u16 {
    // leave at least 4 columns for the messages pane on narrow terminals,
    // so the convs pane never ends up wider than the screen
    (w / 5).clamp(20, 32).min(w.saturating_sub(4).max(2))
}

fn body_height(h: u16) -> u16 {
    h.saturating_sub(INPUT_H + STATUS_H).max(1)
}

fn build_windows(app: &mut term_render::App<AppData>) {
    let (w, h) = {
        let a = app.area.read();
        (a.width, a.height)
    };
    let cw = conv_width(w);
    let bh = body_height(h);

    let mut renderer = app.renderer.write();
    add_window(&mut renderer, "convs", (0, 0), (cw, bh), "conversations");
    add_window(&mut renderer, "messages", (cw, 0), (w.saturating_sub(cw).max(1), bh), "messages");
    add_window(&mut renderer, "input", (0, bh), (w, INPUT_H), "input");
    add_window(&mut renderer, "status", (0, h.saturating_sub(STATUS_H)), (w, STATUS_H), "");
}

fn add_window(app: &mut term_render::render::App, name: &str, pos: (u16, u16), size: (u16, u16), title: &str) {
    let bordered = name != "status";
    // bordered windows need at least 2x2 for the border lines themselves
    let size = if bordered { (size.0.max(2), size.1.max(2)) } else { (size.0.max(1), size.1.max(1)) };
    let mut win = Window::new(pos, 0, size);
    if bordered {
        win.bordered();
    }
    if !title.is_empty() {
        win.titled(title.to_string());
    }
    app.add_window(win, name.to_string(), vec![]);
}

fn render(data: &mut AppData, app: &mut term_render::App<AppData>) {
    let (w, h) = {
        let a = app.area.read();
        (a.width, a.height)
    };
    let cw = conv_width(w);
    let bh = body_height(h);

    let mut renderer = app.renderer.write();
    layout_window(&mut renderer, "convs", (0, 0), (cw, bh));
    layout_window(&mut renderer, "messages", (cw, 0), (w.saturating_sub(cw), bh));
    layout_window(&mut renderer, "input", (0, bh), (w, INPUT_H));
    layout_window(&mut renderer, "status", (0, h.saturating_sub(STATUS_H)), (w, STATUS_H));

    if !data.ui.dirty {
        return;
    }
    data.ui.dirty = false;

    render_convs(&mut renderer, data, bh);
    render_messages(&mut renderer, data, bh);
    render_input(&mut renderer, data);
    render_status(&mut renderer, data);
}

fn layout_window(app: &mut term_render::render::App, name: &str, pos: (u16, u16), size: (u16, u16)) {
    if !app.contains_window(name.into()) {
        return;
    }
    // bordered windows need at least 2x2 for the border lines themselves
    let size = if name == "status" {
        (size.0.max(1), size.1.max(1))
    } else {
        (size.0.max(2), size.1.max(2))
    };
    let win = app.get_window_reference_mut(name.into());
    win.resize(size);
    win.r#move(pos);
}

fn content_height(bh: u16) -> usize {
    bh.saturating_sub(2).max(1) as usize
}

fn render_convs(renderer: &mut term_render::render::App, data: &AppData, bh: u16) {
    let height = content_height(bh);
    let mut lines = Vec::new();
    for (i, conv) in data.ui.convs.iter().take(height).enumerate() {
        let sel = Some(i) == data.ui.selected;
        let text = format!("{} {}  {}", if sel { ">" } else { " " }, conv.label, conv.messages.len());
        let span = if sel {
            Span::from_tokens(vec![text.colorize(ColorType::BrightYellow)])
        } else {
            Span::from_tokens(vec![text.colorizes(vec![])])
        };
        lines.push(span);
    }
    renderer.get_window_reference_mut("convs".into()).try_update_lines(lines);
}

fn render_messages(renderer: &mut term_render::render::App, data: &AppData, bh: u16) {
    let height = content_height(bh);
    let mut lines = Vec::new();
    match data.ui.selected {
        Some(i) => {
            let msgs = &data.ui.convs[i].messages;
            let start = msgs.len().saturating_sub(height);
            for m in &msgs[start..] {
                let own = m.from == data.ui.user;
                let line = format!("{}: {}", m.from, m.text);
                let span = if own {
                    Span::from_tokens(vec![line.colorize(ColorType::BrightGreen)])
                } else {
                    Span::from_tokens(vec![line.colorizes(vec![])])
                };
                lines.push(span);
            }
        }
        None => {
            lines.push(Span::from_tokens(vec![
                "no conversation selected".colorize(ColorType::BrightBlack),
            ]));
        }
    }
    renderer.get_window_reference_mut("messages".into()).try_update_lines(lines);
}

fn render_input(renderer: &mut term_render::render::App, data: &AppData) {
    let cursor = if data.ui.focus == Focus::Input { "|" } else { "" };
    let line = if data.ui.input.is_empty() {
        if data.ui.focus == Focus::Input {
            "type a message — /create a,b /list /ping /quit".to_string()
        } else {
            "press Tab to type".to_string()
        }
    } else {
        format!("{}{}", data.ui.input, cursor)
    };
    renderer.get_window_reference_mut("input".into()).try_update_lines(vec![Span::from_tokens(vec![line.colorizes(vec![])])]);
}

fn render_status(renderer: &mut term_render::render::App, data: &AppData) {
    let conn = if data.ui.connected { "connected" } else { "disconnected" };
    let auth = if data.ui.authenticated { "auth" } else { "no-auth" };
    let conv = match data.ui.selected {
        Some(i) => data.ui.convs[i].label.clone(),
        None => "-".to_string(),
    };
    let line = format!("[{conn}] [{auth}] user={} conv={conv}  {status}", data.ui.user, status = data.ui.status);
    renderer.get_window_reference_mut("status".into()).try_update_lines(vec![Span::from_tokens(vec![line.colorize(ColorType::BrightBlack)])]);
}
