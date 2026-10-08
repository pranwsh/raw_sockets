//! A reusable, ratatui-based chat TUI frontend.
//!
//! This crate is a *pure frontend*: it owns the screen and the keyboard and
//! speaks the shared messaging model ([`chat_model::Action`] /
//! [`chat_model::Event`]) to an application-supplied [`ChatClient`]. It has no
//! knowledge of any particular wire protocol. An app wires itself in by
//! implementing [`ChatClient`] and calling [`run_app`].
//!
//! See the `msgtui` binary (`src/bin/msgtui.rs`) in this crate for a reference
//! integration that adapts this trait to its `msgclient` channel client.
//!
//! # Layout
//!
//! ```text
//! ┌──────────────┬────────────────────────────────┐
//! │ conversations│  transcript                    │
//! │  ▌● alice,bob│  12:04 ▸ me: hello             │
//! │    hey there │  12:05   bob: hi               │
//! ├──────────────┴────────────────────────────────┤
//! │ > message…                                   │
//! ├───────────────────────────────────────────────┤
//! │ ● online auth alice            Tab focus  …   │
//! └───────────────────────────────────────────────┘
//! ```
//!
//! # Key bindings
//!
//! | key | action |
//! |---|---|
//! | `Tab` / `Shift+Tab` | cycle focus sidebar → messages → composer |
//! | `↑` / `↓` | move the sidebar cursor / scroll the transcript |
//! | `Enter` | open the highlighted conversation, or send the composer line |
//! | `PageUp` / `PageDown` | scroll the transcript |
//! | `End` | jump to the newest message |
//! | `Ctrl+U` | clear the composer |
//! | `Ctrl+W` | delete the previous word |
//! | `Esc` | close the overlay / clear focus, else quit |
//! | `Ctrl+C` | quit |
//!
//! Slash commands typed into the composer: `/login`, `/logout`, `/create a,b`,
//! `/new`, `/list`, `/filter`, `/ping`, `/quit`.

pub mod render;
pub mod state;
pub mod util;

use chat_model::{Action, Event};
use ratatui::crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event as CtEvent, KeyCode as CtKey,
    KeyEvent, KeyEventKind, KeyModifiers,
};
use ratatui::crossterm::execute;
use std::io;
use std::time::{Duration, Instant};

use state::{Focus, LoginField, Overlay, State};

/// how long to block waiting for a key before waking anyway
///
/// Bounded so a background event (an inbound message, a status timeout) is
/// reflected promptly without spinning on `poll`.
const TICK: Duration = Duration::from_millis(50);

// ---------------------------------------------------------------------------
// client contract

/// The adapter the app implements so the TUI never touches the wire.
///
/// All socket I/O, framing, and protocol live behind this trait. The TUI
/// drives it by calling [`send`](ChatClient::send) and draining
/// [`drain_events`](ChatClient::drain_events) on each iteration.
pub trait ChatClient: std::fmt::Debug {
    /// enqueue an action; returns false if the connection is gone
    fn send(&self, action: Action) -> bool;
    /// drain any pending events into `out` (non-blocking), reusing its
    /// allocation; returns how many were drained
    fn drain_events(&self, out: &mut Vec<Event>) -> usize;
}

// ---------------------------------------------------------------------------
// entry point

/// Launch the TUI with the supplied client adapter.
///
/// `client` provides the connection; a non-empty `user` causes an immediate
/// [`Action::Hello`]; `conv_hex` optionally pre-selects a conversation
/// (hex-encoded id).
///
/// This blocks until the user quits (`Ctrl+C`, `Esc`, or `/quit`), restoring
/// the terminal on the way out.
pub fn run_app(
    client: Box<dyn ChatClient>,
    user: &str,
    password: &str,
    conv_hex: Option<&str>,
) -> io::Result<()> {
    let mut state = State::new();
    state.login_user = user.to_string();
    state.login_pass = password.to_string();

    if !user.is_empty() {
        state.me = user.to_string();
        state.authenticated = true;
        let ok = client.send(Action::Hello {
            user: user.to_string(),
            password: password.to_string(),
        });
        if !ok {
            state.warn("failed to queue login");
        }
    } else {
        // no credentials supplied: ask for them instead of silently idling
        state.overlay = Overlay::Login;
        state.note("press Enter to sign in");
    }

    if let Some(id) = decode_conv_hex(conv_hex) {
        state.seed_conv(id);
    }

    // the anchor for turning relative Instant ages into wall-clock times
    let epoch_base = Instant::now();

    let result = run_loop(client.as_ref(), &mut state, epoch_base);
    // best effort: tell the server we are going away
    client.send(Action::Goodbye);
    result
}

/// the terminal event/render loop
fn run_loop(
    client: &dyn ChatClient,
    state: &mut State,
    epoch_base: Instant,
) -> io::Result<()> {
    let mut terminal = ratatui::init();
    // mouse reporting is only used for scroll wheel support
    let _ = execute!(terminal.backend_mut(), EnableMouseCapture);

    let mut events: Vec<Event> = Vec::new();
    let mut quit = false;

    while !quit {
        // 1. drain client events (reuses `events`, so no per-tick allocation)
        events.clear();
        if client.drain_events(&mut events) > 0 {
            for ev in events.drain(..) {
                state.apply(ev);
            }
            state.dirty = true;
        }

        // 2. wait for a key, bounded so redraws stay responsive
        if event::poll(TICK)? {
            match event::read()? {
                CtEvent::Key(key) if key.kind != KeyEventKind::Release => {
                    if handle_key(client, state, key) {
                        quit = true;
                    }
                }
                CtEvent::Resize(_, _) => state.dirty = true,
                _ => {}
            }
        }

        // 3. clear an expired status so the bar does not linger
        if state.status.as_ref().is_some_and(|s| s.is_stale()) {
            state.status = None;
            state.dirty = true;
        }

        // 4. draw, but only when something changed
        if state.dirty || state.overlay != Overlay::None {
            terminal.draw(|frame| render::draw(frame, state, epoch_base))?;
            state.dirty = false;
        }
    }

    let _ = execute!(terminal.backend_mut(), DisableMouseCapture);
    ratatui::restore();
    Ok(())
}

/// decode a hex conversation id; returns `None` for empty or malformed input
fn decode_conv_hex(conv_hex: Option<&str>) -> Option<Vec<u8>> {
    let h = conv_hex?.trim();
    if h.is_empty() {
        return None;
    }
    hex::decode(h).ok()
}

// ---------------------------------------------------------------------------
// input handling

/// Handle one key. Returns true when the app should quit.
fn handle_key(client: &dyn ChatClient, state: &mut State, key: KeyEvent) -> bool {
    // Ctrl+C always quits, whatever has focus
    if key.code == CtKey::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
        return true;
    }

    // overlays capture all input except Esc (and Ctrl+C, handled above)
    match state.overlay {
        Overlay::Login => return handle_login_key(client, state, key),
        Overlay::NewConv => return handle_new_conv_key(client, state, key),
        Overlay::Filter => return handle_filter_key(state, key),
        Overlay::None => {}
    }

    match key.code {
        CtKey::Esc => {
            // Esc drops composer focus first, then quits
            if state.focus != Focus::Composer || !state.input.is_empty() {
                state.input.clear();
                state.focus = Focus::Sidebar;
                state.dirty = true;
                return false;
            }
            return true;
        }
        CtKey::Tab => {
            state.focus = next_focus(state.focus, key.modifiers.contains(KeyModifiers::SHIFT));
            state.dirty = true;
        }
        CtKey::BackTab => {
            state.focus = next_focus(state.focus, true);
            state.dirty = true;
        }
        CtKey::Up => match state.focus {
            Focus::Composer => state.history_prev(),
            _ => {
                state.move_cursor(-1);
                state.open_cursor_soft();
            }
        },
        CtKey::Down => match state.focus {
            Focus::Composer => state.history_next(),
            _ => {
                state.move_cursor(1);
                state.open_cursor_soft();
            }
        },
        CtKey::PageUp => {
            state.scroll_by(10);
            return false;
        }
        CtKey::PageDown => {
            state.scroll_by_step(-10);
            return false;
        }
        CtKey::Home | CtKey::End => {
            state.scroll_to_bottom();
            return false;
        }
        CtKey::Enter => {
            if state.focus == Focus::Composer {
                return submit_composer(client, state);
            }
            state.open_cursor();
            return false;
        }
        CtKey::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            state.input.clear();
            state.dirty = true;
        }
        CtKey::Char('w') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            delete_word(&mut state.input);
            state.dirty = true;
        }
        CtKey::Backspace => {
            state.input.pop();
            state.dirty = true;
        }
        CtKey::Char(c) => {
            // a printable character returns to the composer: typing always types
            state.focus = Focus::Composer;
            state.input.push(c);
            state.dirty = true;
        }
        _ => {}
    }
    false
}

fn next_focus(current: Focus, backwards: bool) -> Focus {
    const ORDER: [Focus; 3] = [Focus::Sidebar, Focus::Messages, Focus::Composer];
    let i = ORDER.iter().position(|f| *f == current).unwrap_or(0);
    let n = ORDER.len() as isize;
    let next = if backwards { (i as isize - 1).rem_euclid(n) } else { (i as isize + 1) % n };
    ORDER[next as usize]
}

/// Delete the word before the cursor.
fn delete_word(input: &mut String) {
    let trimmed = input.trim_end();
    let cut = match trimmed.rfind(' ') {
        Some(i) => i,
        None => 0,
    };
    input.truncate(cut);
}

/// Handle the composer's Enter: run a command or send a message.
fn submit_composer(client: &dyn ChatClient, state: &mut State) -> bool {
    let raw = std::mem::take(&mut state.input);
    let line = raw.trim().to_string();
    if line.is_empty() {
        return false;
    }
    state.push_history(line.clone());

    // commands are handled before the generic submit path so that actions like
    // /quit and /login never fall through to the message path
    if let Some(cmd) = line.strip_prefix('/') {
        match cmd.split_whitespace().next().unwrap_or("") {
            "q" | "quit" | "exit" => return true,
            "login" => {
                state.overlay = Overlay::Login;
                state.login_field = LoginField::User;
                state.dirty = true;
                return false;
            }
            "logout" => {
                client.send(Action::Goodbye);
                state.warn("signed out");
                return false;
            }
            "new" => {
                state.overlay = Overlay::NewConv;
                state.draft_members.clear();
                state.dirty = true;
                return false;
            }
            "filter" => {
                state.overlay = Overlay::Filter;
                state.dirty = true;
                return false;
            }
            _ => {}
        }
    }

    match state::submit(state, &line) {
        Some(action) => {
            if !client.send(action) {
                state.warn("connection closed; action dropped");
            } else {
                state.note("sent");
            }
        }
        None => {
            // `/quit` reaches here as an unknown command; re-check it
            if matches!(line.as_str(), "/quit" | "/exit") {
                return true;
            }
        }
    }
    state.dirty = true;
    false
}

// --- filter overlay

fn handle_filter_key(state: &mut State, key: KeyEvent) -> bool {
    // every edit goes through `set_filter` so the cached visible list is
    // invalidated exactly once, here
    match key.code {
        CtKey::Esc => {
            state.overlay = Overlay::None;
            state.set_filter(String::new());
            state.dirty = true;
        }
        CtKey::Backspace => {
            let mut f = state.filter.clone();
            f.pop();
            state.set_filter(f);
            state.dirty = true;
        }
        CtKey::Enter => {
            state.overlay = Overlay::None;
            state.focus = Focus::Sidebar;
            state.dirty = true;
        }
        CtKey::Char(c) => {
            let mut f = state.filter.clone();
            f.push(c);
            state.set_filter(f);
            state.dirty = true;
        }
        _ => {}
    }
    false
}

// --- login overlay

fn handle_login_key(client: &dyn ChatClient, state: &mut State, key: KeyEvent) -> bool {
    match key.code {
        CtKey::Esc => {
            state.overlay = Overlay::None;
            state.dirty = true;
        }
        CtKey::Tab | CtKey::BackTab => {
            state.login_field = if state.login_field == LoginField::User {
                LoginField::Password
            } else {
                LoginField::User
            };
            state.dirty = true;
        }
        CtKey::Backspace => {
            match state.login_field {
                LoginField::User => state.login_user.pop(),
                LoginField::Password => state.login_pass.pop(),
            };
            state.dirty = true;
        }
        CtKey::Char('n') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            // Ctrl+N submits from either field
            submit_login(client, state);
        }
        CtKey::Enter => {
            // Enter moves between fields, and submits from the last one
            if state.login_field == LoginField::User {
                state.login_field = LoginField::Password;
                state.dirty = true;
            } else {
                submit_login(client, state);
            }
        }
        CtKey::Char(c) => match state.login_field {
            LoginField::User => state.login_user.push(c),
            LoginField::Password => state.login_pass.push(c),
        },
        _ => {}
    }
    false
}

fn submit_login(client: &dyn ChatClient, state: &mut State) {
    if state.login_user.is_empty() {
        state.warn("user id required");
        return;
    }
    state.me = state.login_user.clone();
    let ok = client.send(Action::Hello {
        user: state.login_user.clone(),
        password: std::mem::take(&mut state.login_pass),
    });
    state.overlay = Overlay::None;
    state.login_pass.clear();
    if ok {
        state.note("authenticating…");
    } else {
        state.warn("connection closed");
    }
    state.dirty = true;
}

// --- new conversation overlay

fn handle_new_conv_key(client: &dyn ChatClient, state: &mut State, key: KeyEvent) -> bool {
    match key.code {
        CtKey::Esc => {
            state.overlay = Overlay::None;
            state.draft_members.clear();
            state.dirty = true;
        }
        CtKey::Backspace => {
            state.draft_members.pop();
            state.dirty = true;
        }
        CtKey::Enter => {
            let members: Vec<String> = state
                .draft_members
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect();
            state.overlay = Overlay::None;
            state.draft_members.clear();
            if members.is_empty() {
                state.warn("need at least one other member");
            } else if client.send(Action::CreateConv { members }) {
                state.note("creating conversation…");
            } else {
                state.warn("connection closed");
            }
            state.dirty = true;
        }
        CtKey::Char(c) => {
            state.draft_members.push(c);
            state.dirty = true;
        }
        _ => {}
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_hex_conv_ids() {
        assert_eq!(decode_conv_hex(Some("a1b2")), Some(vec![0xa1, 0xb2]));
        assert_eq!(decode_conv_hex(Some("")), None);
        assert_eq!(decode_conv_hex(None), None);
        assert_eq!(decode_conv_hex(Some("zz")), None);
    }

    #[test]
    fn focus_cycles_both_ways() {
        assert_eq!(next_focus(Focus::Composer, false), Focus::Sidebar);
        assert_eq!(next_focus(Focus::Sidebar, false), Focus::Messages);
        assert_eq!(next_focus(Focus::Sidebar, true), Focus::Composer);
    }

    #[test]
    fn deletes_previous_word() {
        let mut s = String::from("hello brave world");
        delete_word(&mut s);
        assert_eq!(s, "hello brave");
        delete_word(&mut s);
        assert_eq!(s, "hello");
        delete_word(&mut s);
        assert_eq!(s, "");
    }
}
