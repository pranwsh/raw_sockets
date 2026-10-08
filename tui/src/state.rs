//! Conversation list, message history, and input state for the chat TUI.
//!
//! This module owns everything the UI draws. It is deliberately free of any
//! wire knowledge: events arrive as [`chat_model::Event`] and are folded in by
//! [`State::apply`], and outbound intent leaves as [`chat_model::Action`].
//!
//! Conversations are addressed by their 8-byte wire id, but every lookup goes
//! through a hash map rather than a scan of the vector, so a message arriving
//! for any conversation is O(1) regardless of how many the user belongs to.

use chat_model::{Action, ConvInfo, Event};
use std::collections::HashMap;
use std::time::Instant;

/// how many messages to retain per conversation.
///
/// Bounded so a long-lived session cannot grow without limit. Older messages
/// are dropped from the front.
const MAX_MESSAGES_PER_CONV: usize = 500;

/// a message shown in the transcript
#[derive(Debug, Clone)]
pub struct Message {
    pub from: String,
    pub text: String,
    pub seq: u64,
    /// when we received it, for relative timestamps
    pub at: Instant,
    /// true when we sent it (rendered right-aligned / highlighted)
    pub own: bool,
}

/// one conversation and its transcript
#[derive(Debug)]
pub struct Conversation {
    pub id: Vec<u8>,
    /// sorted member ids, used to build the display label
    pub members: Vec<String>,
    pub messages: Vec<Message>,
    /// unread count, reset when the user opens the conversation
    pub unread: usize,
    /// one-line preview of the most recent message
    pub last_text: String,
    /// timestamp of the most recent message, for ordering the sidebar
    pub last_at: Option<Instant>,
    /// this conversation is new since the last listing
    pub is_new: bool,
}

impl Conversation {
    fn new(info: &ConvInfo) -> Self {
        Self {
            id: info.id.clone(),
            members: info.members.clone(),
            messages: Vec::new(),
            unread: 0,
            last_text: String::new(),
            last_at: None,
            is_new: false,
        }
    }

    /// a human label for the sidebar.
    ///
    /// Members other than the local user are the interesting part: "alice,
    /// bob" says who you are talking to, where the raw id says nothing. A
    /// self-conversation falls back to the user alone.
    fn label(&self, me: &str) -> String {
        let others: Vec<&str> =
            self.members.iter().map(String::as_str).filter(|m| *m != me).collect();
        if others.is_empty() {
            self.members.join(", ")
        } else {
            others.join(", ")
        }
    }

    /// a short stable id, used when the member list is empty or as a
    /// disambiguating suffix
    fn short_id(&self) -> String {
        let hex = crate::util::hex_prefix(&self.id);
        if hex.is_empty() {
            String::new()
        } else {
            format!(" #{hex}")
        }
    }

    fn push(&mut self, msg: Message, me: &str, select_hovered: bool) {
        let at = msg.at;
        self.last_text = msg.text.clone();
        self.last_at = Some(at);
        if !msg.own && select_hovered {
            self.unread += 1;
        }
        self.messages.push(msg);
        if self.messages.len() > MAX_MESSAGES_PER_CONV {
            let drop_n = self.messages.len() - MAX_MESSAGES_PER_CONV;
            self.messages.drain(..drop_n);
        }
        let _ = me;
    }
}

/// which pane has keyboard focus
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    /// the conversation sidebar is focused (arrow keys / enter to open)
    Sidebar,
    /// the message transcript is focused (page up/down scroll)
    Messages,
    /// the composer is focused (typing goes here)
    Composer,
}

/// transient one-line message shown in the status bar
#[derive(Debug, Clone)]
pub struct Status {
    pub text: String,
    pub is_error: bool,
    pub at: Instant,
}

impl Status {
    fn info(text: impl Into<String>) -> Self {
        Self { text: text.into(), is_error: false, at: Instant::now() }
    }

    fn error(text: impl Into<String>) -> Self {
        Self { text: text.into(), is_error: true, at: Instant::now() }
    }

    /// statuses fade after a while so the bar does not shout forever
    pub fn is_stale(&self) -> bool {
        self.at.elapsed().as_secs() >= STATUS_TTL_SECS
    }
}

const STATUS_TTL_SECS: u64 = 8;

/// which overlay is capturing input, if any
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Overlay {
    None,
    /// `/`-prefixed filter over the sidebar
    Filter,
    /// modal asking for credentials
    Login,
    /// modal asking for the members of a new conversation
    NewConv,
}

/// everything the UI draws
#[derive(Debug)]
pub struct State {
    /// conversation ids in display order, backed by [`convs`]
    ///
    /// Kept as an index vector plus a hash map: the sidebar renders in vector
    /// order while every event lookup is a hash hit.
    order: Vec<usize>,
    convs: HashMap<Vec<u8>, usize>,
    entries: Vec<Conversation>,

    /// the conversation whose transcript is shown; `None` = no selection
    selected: Option<usize>,
    /// index into `order` highlighted in the sidebar
    cursor: usize,

    /// transcript scroll offset in lines from the bottom (0 = pinned to newest)
    scroll: usize,

    pub me: String,
    pub connected: bool,
    pub authenticated: bool,
    pub focus: Focus,
    pub overlay: Overlay,

    pub input: String,
    /// previously submitted composer lines, most recent last
    pub history: Vec<String>,
    /// index into `history` while arrowing through it; `history.len()` = live
    pub history_pos: usize,
    /// the sidebar filter text
    pub filter: String,
    /// what the login overlay is collecting
    pub login_user: String,
    pub login_pass: String,
    /// which login field has focus
    pub login_field: LoginField,
    /// what the new-conversation overlay is collecting
    pub draft_members: String,

    pub status: Option<Status>,
    /// side panel hidden (full-screen transcript)
    pub wide: bool,

    /// cached result of [`State::visible`], rebuilt only when `filter_cache`
    /// no longer matches `filter` or `filter_epoch`
    visible_cache: Vec<usize>,
    /// the filter text `visible_cache` was built for
    filter_cache: String,
    /// conversation revision `visible_cache` was built against
    filter_epoch: u64,
    /// bumped whenever a conversation is added or its searchable text changes
    filter_epoch_counter: u64,

    /// true when something changed and a redraw is worthwhile
    pub dirty: bool,
    /// number of frames drawn, for diagnostics
    pub frames: u64,
}

/// which field the login overlay is editing
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoginField {
    User,
    Password,
}

impl Default for State {
    fn default() -> Self {
        Self::new()
    }
}

impl State {
    pub fn new() -> Self {
        Self {
            order: Vec::new(),
            convs: HashMap::new(),
            entries: Vec::new(),
            selected: None,
            cursor: 0,
            scroll: 0,
            me: String::new(),
            connected: false,
            authenticated: false,
            focus: Focus::Composer,
            overlay: Overlay::None,
            input: String::new(),
            history: Vec::new(),
            history_pos: 0,
            filter: String::new(),
            login_user: String::new(),
            login_pass: String::new(),
            login_field: LoginField::User,
            draft_members: String::new(),
            visible_cache: Vec::new(),
            filter_cache: String::new(),
            filter_epoch: u64::MAX,
            filter_epoch_counter: 0,
            status: None,
            wide: false,
            dirty: true,
            frames: 0,
        }
    }

    // conversation lookup

    /// index of `id` in `entries`, inserting it if absent
    fn index_of(&mut self, id: &[u8], members: Option<&[String]>) -> usize {
        if let Some(&i) = self.convs.get(id) {
            if let Some(m) = members {
                let cur = &self.entries[i].members;
                if !cur.is_empty() && cur != m {
                    self.entries[i].members = m.to_vec();
                    // the label feeds the filter, so the cache is now stale
                    self.touch_filter();
                }
            }
            return i;
        }
        let info = ConvInfo { id: id.to_vec(), members: members.map(|m| m.to_vec()).unwrap_or_default() };
        let idx = self.entries.len();
        self.entries.push(Conversation::new(&info));
        self.convs.insert(id.to_vec(), idx);
        self.order.push(idx);
        self.touch_filter();
        idx
    }

    /// the conversation currently shown in the transcript
    pub fn selected(&self) -> Option<&Conversation> {
        self.selected.map(|i| &self.entries[i])
    }

    /// the conversation the sidebar cursor is on
    pub fn cursor_conv(&mut self) -> Option<&Conversation> {
        let cursor = self.cursor;
        let i = *self.visible().get(cursor)?;
        self.entries.get(i)
    }

    /// Mark the cached filter result stale.
    fn touch_filter(&mut self) {
        self.filter_epoch_counter = self.filter_epoch_counter.wrapping_add(1);
    }

    /// sidebar entries matching the current filter, in display order
    ///
    /// The result is cached and rebuilt only when the filter text or the set of
    /// conversations changes. The sidebar redraws on every keystroke and every
    /// inbound message, and recomputing the scan (plus the label building and
    /// lowercasing it needs) each time would dominate the frame.
    pub fn visible(&mut self) -> &[usize] {
        if self.filter_cache == self.filter && self.filter_epoch == self.filter_epoch_counter {
            return &self.visible_cache;
        }
        self.visible_cache.clear();
        if self.filter.is_empty() {
            self.visible_cache.extend_from_slice(&self.order);
        } else {
            let needle = self.filter.to_lowercase();
            self.visible_cache.extend(self.order.iter().copied().filter(|&i| {
                let c = &self.entries[i];
                let label = c.label(&self.me).to_lowercase();
                label.contains(&needle)
                    || c.last_text.to_lowercase().contains(&needle)
                    || crate::util::hex_prefix(&c.id).contains(&needle)
            }));
        }
        self.filter_cache.clear();
        self.filter_cache.push_str(&self.filter);
        self.filter_epoch = self.filter_epoch_counter;
        &self.visible_cache
    }

    /// display label for a conversation index
    pub fn label_of(&self, i: usize) -> String {
        let c = &self.entries[i];
        let base = c.label(&self.me);
        if base.is_empty() {
            crate::util::hex_prefix(&c.id)
        } else {
            format!("{base}{}", c.short_id())
        }
    }

    /// select a conversation by index, clearing its unread count
    pub fn select(&mut self, idx: usize) {
        if idx >= self.entries.len() {
            return;
        }
        self.selected = Some(idx);
        self.entries[idx].unread = 0;
        self.scroll = 0;
        self.dirty = true;
    }

    /// select by wire id, e.g. when a message arrives for a known conversation
    pub fn select_by_id(&mut self, id: &[u8]) {
        if let Some(&i) = self.convs.get(id) {
            self.select(i);
        }
    }

    /// move the sidebar cursor, clamped to the visible list
    pub fn move_cursor(&mut self, delta: isize) {
        let n = self.visible().len();
        if n == 0 {
            self.cursor = 0;
            return;
        }
        let next = (self.cursor as isize + delta).rem_euclid(n as isize) as usize;
        self.cursor = next;
        self.dirty = true;
    }

    /// the conversation the sidebar cursor is on, by backing index
    pub fn cursor_index(&mut self) -> Option<usize> {
        let cursor = self.cursor;
        self.visible().get(cursor).copied()
    }

    /// open the conversation under the cursor
    pub fn open_cursor(&mut self) {
        if let Some(i) = self.cursor_index() {
            self.select(i);
            self.focus = Focus::Composer;
        }
    }


    /// scroll the transcript by `delta` lines; 0 is the newest message
    pub fn scroll_by(&mut self, delta: usize) {
        let max = self.transcript_lines().saturating_sub(1);
        self.scroll = (self.scroll + delta).min(max);
        self.dirty = true;
    }

    /// pin the transcript back to the newest message
    pub fn scroll_to_bottom(&mut self) {
        self.scroll = 0;
        self.dirty = true;
    }

    /// rough line count of the visible transcript, used to bound scrolling
    fn transcript_lines(&self) -> usize {
        self.selected().map(|c| c.messages.len()).unwrap_or(0)
    }

    /// total unread across all conversations
    pub fn total_unread(&self) -> usize {
        self.entries.iter().map(|c| c.unread).sum()
    }

    // event folding

    /// fold one server event into the UI state
    pub fn apply(&mut self, ev: Event) {
        match ev {
            Event::Connected => {
                self.connected = true;
                self.set_status(Status::info("connected"));
            }
            Event::AuthOk { created } => {
                self.authenticated = true;
                self.me = self.login_user.clone();
                self.set_status(Status::info(if created {
                    "authenticated, account created"
                } else {
                    "authenticated"
                }));
            }
            Event::AuthFail { reason } => {
                self.authenticated = false;
                self.set_status(Status::error(format!("auth failed: {reason}")));
                self.overlay = Overlay::Login;
            }
            Event::ConvCreated(info) => {
                let idx = self.index_of(&info.id, Some(&info.members));
                self.selected = Some(idx);
                self.entries[idx].unread = 0;
                self.focus = Focus::Composer;
                self.set_status(Status::info("conversation created"));
            }
            Event::Convs { convs } => {
                let mut added = 0;
                for info in &convs {
                    let before = self.entries.len();
                    let idx = self.index_of(&info.id, Some(&info.members));
                    if self.entries.len() > before {
                        self.entries[idx].is_new = true;
                        added += 1;
                    }
                }
                let total = self.entries.len();
                if self.selected.is_none() && total > 0 {
                    self.selected = Some(self.order[0]);
                }
                self.set_status(Status::info(format!(
                    "{total} conversation{} ({added} new)",
                    if total == 1 { "" } else { "s" },
                )));
            }
            Event::Message { conv, from, seq, text } => {
                let is_selected = self
                    .selected
                    .is_some_and(|s| self.entries[s].id == conv);
                let idx = self.index_of(&conv, None);
                let own = from == self.me;
                self.entries[idx].push(
                    Message { from, text, seq, at: Instant::now(), own },
                    &self.me,
                    !is_selected,
                );
                // the preview text feeds the sidebar filter, so the cache is stale
                self.touch_filter();
                // jump to a conversation that just received a message so
                // inbound traffic is immediately visible
                if self.selected.is_none() {
                    self.selected = Some(idx);
                }
            }
            Event::Delivered { seq } => {
                self.set_status(Status::info(format!("delivered seq {seq}")));
            }
            Event::Pong => self.set_status(Status::info("pong")),
            Event::Error { msg } => self.set_status(Status::error(msg)),
            Event::Disconnected { reason } => {
                self.connected = false;
                self.authenticated = false;
                self.set_status(Status::error(format!("disconnected: {reason}")));
            }
        }
        self.dirty = true;
    }

    fn set_status(&mut self, s: Status) {
        self.status = Some(s);
    }

    /// report a local problem (bad command, no selection) without an event
    pub fn warn(&mut self, msg: impl Into<String>) {
        self.set_status(Status::error(msg));
        self.dirty = true;
    }

    /// report a local success
    pub fn note(&mut self, msg: impl Into<String>) {
        self.set_status(Status::info(msg));
        self.dirty = true;
    }

    /// seed the conversation list from a hex id supplied on the command line
    pub fn seed_conv(&mut self, id: Vec<u8>) {
        let idx = self.index_of(&id, None);
        self.selected = Some(idx);
        self.dirty = true;
    }

    /// total conversations known, filtered or not
    pub fn entries_count(&self) -> usize {
        self.entries.len()
    }

    /// set the sidebar filter, invalidating the cached visible list
    pub fn set_filter(&mut self, filter: String) {
        if self.filter != filter {
            self.filter = filter;
            self.touch_filter();
        }
    }

    /// whether conversation `idx` is the one shown in the transcript
    pub fn is_selected(&self, idx: usize) -> bool {
        self.selected == Some(idx)
    }

    /// keep the sidebar cursor within `len` visible rows
    pub fn clamp_cursor(&mut self, len: usize) {
        if len == 0 {
            self.cursor = 0;
        } else if self.cursor >= len {
            self.cursor = len - 1;
        }
    }

    /// the sidebar row the cursor sits on
    pub fn cursor_on_row(&self) -> usize {
        self.cursor
    }

    /// borrow a conversation by its backing index
    pub fn entries_at(&self, idx: usize) -> &Conversation {
        &self.entries[idx]
    }

    /// label of the currently selected conversation
    pub fn selected_label(&self) -> String {
        match self.selected {
            Some(i) => self.label_of(i),
            None => "messages".to_string(),
        }
    }

    /// how far the transcript is scrolled back from the newest message
    pub fn scroll_offset(&self) -> usize {
        self.scroll
    }

    /// select the conversation under the cursor without moving focus away
    ///
    /// Arrowing through the sidebar previews each conversation, so the user can
    /// see what they are about to open.
    pub fn open_cursor_soft(&mut self) {
        if let Some(i) = self.cursor_index() {
            if self.selected != Some(i) {
                self.selected = Some(i);
                self.scroll = 0;
                // peeking at a conversation should not silently clear its
                // unread badge; only an explicit open does that
                self.entries[i].unread = 0;
            }
            self.dirty = true;
        }
    }

    /// step the transcript by a signed amount
    pub fn scroll_by_step(&mut self, delta: isize) {
        if delta >= 0 {
            self.scroll_by(delta as usize);
        } else {
            let back = (-delta) as usize;
            self.scroll = self.scroll.saturating_sub(back);
            self.dirty = true;
        }
    }

    /// recall the previous submitted line into the composer
    pub fn history_prev(&mut self) {
        if self.history.is_empty() {
            return;
        }
        // first press snapshots what is being typed, so arrowing back and
        // forth does not lose it
        if self.history_pos == self.history.len() {
            self.history.push(std::mem::take(&mut self.input));
        }
        if self.history_pos > 0 {
            self.history_pos -= 1;
        }
        self.input = self.history[self.history_pos].clone();
        self.dirty = true;
    }

    /// recall the next (or live) line into the composer
    pub fn history_next(&mut self) {
        if self.history.is_empty() {
            return;
        }
        if self.history_pos < self.history.len() {
            self.history_pos += 1;
        }
        self.input = if self.history_pos == self.history.len() {
            // past the newest entry: restore the live line
            self.history.pop().unwrap_or_default()
        } else {
            self.history[self.history_pos].clone()
        };
        self.dirty = true;
    }

    /// remember a submitted line for the composer history
    pub fn push_history(&mut self, line: String) {
        if line.is_empty() {
            return;
        }
        // entering a fresh line resets the recall cursor
        self.history.push(line);
        self.history_pos = self.history.len();
        // keep the buffer bounded; a long session must not grow without limit
        const MAX_HISTORY: usize = 100;
        if self.history.len() > MAX_HISTORY {
            self.history.drain(..self.history.len() - MAX_HISTORY);
        }
        self.history_pos = self.history.len();
    }
}

/// turn a submitted composer line into an [`Action`].
///
/// Returns `None` for an empty line. `/`-prefixed lines are commands; anything
/// else is a message into the selected conversation.
pub fn submit(state: &mut State, line: &str) -> Option<Action> {
    let line = line.trim();
    if line.is_empty() {
        return None;
    }
    let Some(cmd) = line.strip_prefix('/') else {
        // a plain line sends into the selected conversation
        let idx = state.selected?;
        let conv = state.entries[idx].id.clone();
        let own = true;
        state.entries[idx].push(
            Message {
                from: state.me.clone(),
                text: line.to_string(),
                seq: 0,
                at: Instant::now(),
                own,
            },
            &state.me,
            false,
        );
        return Some(Action::Send { conv, text: line.to_string() });
    };

    let (name, rest) = cmd.split_once(' ').unwrap_or((cmd, ""));
    let rest = rest.trim();
    match name {
        "create" | "new" => {
            let members: Vec<String> = rest
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect();
            if members.is_empty() {
                state.warn("usage: /create alice,bob");
                return None;
            }
            Some(Action::CreateConv { members })
        }
        "list" => Some(Action::ListConvs),
        "ping" => Some(Action::Ping),
        "quit" | "exit" => None, // handled by the caller as an exit request
        other => {
            state.warn(format!("unknown command /{other}"));
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(conv: &[u8], from: &str, text: &str) -> Event {
        Event::Message { conv: conv.to_vec(), from: from.into(), seq: 1, text: text.into() }
    }

    fn conv_info(id: u8, members: &[&str]) -> ConvInfo {
        ConvInfo {
            id: vec![id; 8],
            members: members.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn listing_labels_conversations_by_members() {
        let mut s = State::new();
        s.me = "alice".into();
        s.apply(Event::Convs {
            convs: vec![conv_info(1, &["alice", "bob"]), conv_info(2, &["alice", "carol", "dave"])],
        });
        // the label drops the local user and shows who you are talking to
        // conv_info repeats its byte, so the 8-byte id renders as 8 hex chars
        assert_eq!(s.label_of(0), "bob #01010101");
        assert_eq!(s.label_of(1), "carol, dave #02020202");
    }

    #[test]
    fn a_message_for_a_new_conversation_creates_it() {
        let mut s = State::new();
        s.me = "alice".into();
        s.apply(msg(&[7u8; 8], "bob", "hi"));
        assert_eq!(s.entries_count(), 1);
        // first message auto-selects so inbound traffic is visible
        assert!(s.is_selected(0));
        assert_eq!(s.selected().unwrap().messages.len(), 1);
    }

    #[test]
    fn messages_route_to_the_right_conversation() {
        let mut s = State::new();
        s.me = "alice".into();
        s.apply(Event::Convs { convs: vec![conv_info(1, &["alice", "bob"]), conv_info(2, &["alice", "carol"])] });
        s.apply(msg(&[1u8; 8], "bob", "to bob"));
        s.apply(msg(&[2u8; 8], "carol", "to carol"));
        s.apply(msg(&[2u8; 8], "carol", "to carol again"));
        // each conversation kept its own transcript
        assert_eq!(s.entries_at(0).messages.len(), 1);
        assert_eq!(s.entries_at(1).messages.len(), 2);
        assert_eq!(s.entries_at(1).messages[1].text, "to carol again");
    }

    #[test]
    fn unread_counts_only_for_unselected_conversations() {
        let mut s = State::new();
        s.me = "alice".into();
        s.apply(Event::Convs { convs: vec![conv_info(1, &["alice", "bob"]), conv_info(2, &["alice", "carol"])] });
        s.select(0);

        // selected conversation: no badge
        s.apply(msg(&[1u8; 8], "bob", "seen"));
        assert_eq!(s.entries_at(0).unread, 0);

        // background conversation: badge
        s.apply(msg(&[2u8; 8], "carol", "unseen"));
        s.apply(msg(&[2u8; 8], "carol", "also unseen"));
        assert_eq!(s.entries_at(1).unread, 2);
        assert_eq!(s.total_unread(), 2);

        // opening it clears the badge
        s.select(1);
        assert_eq!(s.total_unread(), 0);
    }

    #[test]
    fn own_messages_never_count_as_unread() {
        let mut s = State::new();
        s.me = "alice".into();
        s.apply(Event::Convs { convs: vec![conv_info(1, &["alice", "bob"])] });
        s.apply(msg(&[1u8; 8], "alice", "mine"));
        assert_eq!(s.total_unread(), 0);
    }

    #[test]
    fn transcript_is_bounded() {
        let mut s = State::new();
        s.me = "alice".into();
        for i in 0..(MAX_MESSAGES_PER_CONV + 50) {
            s.apply(msg(&[1u8; 8], "bob", &format!("m{i}")));
        }
        // old messages are dropped from the front, not the end
        assert_eq!(s.entries_at(0).messages.len(), MAX_MESSAGES_PER_CONV);
        assert_eq!(s.entries_at(0).messages[0].text, "m50");
        assert_eq!(
            s.entries_at(0).messages[MAX_MESSAGES_PER_CONV - 1].text,
            format!("m{}", MAX_MESSAGES_PER_CONV + 49),
        );
    }

    #[test]
    fn filter_matches_label_preview_and_id() {
        let mut s = State::new();
        s.me = "alice".into();
        s.apply(Event::Convs { convs: vec![conv_info(1, &["alice", "bob"]), conv_info(2, &["alice", "carol"])] });
        s.apply(msg(&[2u8; 8], "carol", "about penguins"));

        // by member name
        s.set_filter("carol".into());
        assert_eq!(s.visible(), vec![1]);
        // by message preview
        s.set_filter("penguins".into());
        assert_eq!(s.visible(), vec![1]);
        // by hex id
        s.set_filter("0101".into());
        assert_eq!(s.visible(), vec![0]);
        // case insensitive
        s.set_filter("CAROL".into());
        assert_eq!(s.visible(), vec![1]);
        // no match
        s.set_filter("zzz".into());
        assert!(s.visible().is_empty());
    }

    #[test]
    fn status_expires() {
        let mut s = State::new();
        s.note("hello");
        assert!(!s.status.as_ref().unwrap().is_stale());
    }

    #[test]
    fn submit_turns_lines_into_actions() {
        let mut s = State::new();
        s.me = "alice".into();
        s.apply(Event::Convs { convs: vec![conv_info(1, &["alice", "bob"])] });
        s.select(0);

        // plain text sends into the selected conversation and echoes locally
        let action = submit(&mut s, "hello bob").expect("action");
        match action {
            Action::Send { conv, text } => {
                assert_eq!(conv, vec![1u8; 8]);
                assert_eq!(text, "hello bob");
            }
            other => panic!("expected Send, got {other:?}"),
        }
        assert_eq!(s.entries_at(0).messages.len(), 1);
        assert!(s.entries_at(0).messages[0].own);

        // commands
        assert_eq!(submit(&mut s, "/list"), Some(Action::ListConvs));
        assert_eq!(submit(&mut s, "/ping"), Some(Action::Ping));
        match submit(&mut s, "/create bob,carol") {
            Some(Action::CreateConv { members }) => assert_eq!(members, vec!["bob", "carol"]),
            other => panic!("expected CreateConv, got {other:?}"),
        }

        // blank and unknown lines produce nothing
        assert_eq!(submit(&mut s, "   "), None);
        assert_eq!(submit(&mut s, "/nope"), None);
    }

    #[test]
    fn history_recalls_previous_lines() {
        let mut s = State::new();
        s.push_history("first".into());
        s.push_history("second".into());
        s.input = "draft".into();

        s.history_prev();
        assert_eq!(s.input, "second");
        s.history_prev();
        assert_eq!(s.input, "first");
        // walking back past the newest entry restores the live draft
        s.history_next();
        s.history_next();
        assert_eq!(s.input, "draft");
    }

    #[test]
    fn cursor_wraps_within_the_filtered_list() {
        let mut s = State::new();
        s.apply(Event::Convs { convs: vec![conv_info(1, &["alice", "bob"]), conv_info(2, &["alice", "carol"])] });
        s.move_cursor(1);
        assert_eq!(s.cursor_on_row(), 1);
        // wraps past the end back to the top
        s.move_cursor(1);
        assert_eq!(s.cursor_on_row(), 0);
        s.move_cursor(-1);
        assert_eq!(s.cursor_on_row(), 1);
    }
}

#[cfg(test)]
mod filter_cache_tests {
    use super::*;
    use chat_model::Event;

    fn with_two() -> State {
        let mut s = State::new();
        s.me = "alice".into();
        s.apply(Event::Convs {
            convs: vec![
                ConvInfo { id: vec![1u8; 8], members: vec!["alice".into(), "bob".into()] },
                ConvInfo { id: vec![2u8; 8], members: vec!["alice".into(), "carol".into()] },
            ],
        });
        s
    }

    #[test]
    fn cache_tracks_filter_edits() {
        let mut s = with_two();
        assert_eq!(s.visible().len(), 2);
        s.set_filter("carol".into());
        assert_eq!(s.visible().to_vec(), vec![1]);
        s.set_filter(String::new());
        assert_eq!(s.visible().len(), 2);
    }

    #[test]
    fn cache_invalidated_when_a_conversation_arrives() {
        let mut s = with_two();
        s.set_filter("dave".into());
        assert!(s.visible().is_empty());
        // a new conversation whose members match must show up immediately
        s.apply(Event::Convs {
            convs: vec![ConvInfo { id: vec![3u8; 8], members: vec!["alice".into(), "dave".into()] }],
        });
        assert_eq!(s.visible().to_vec(), vec![2]);
    }

    #[test]
    fn cache_invalidated_when_a_message_changes_the_preview() {
        let mut s = with_two();
        s.set_filter("penguins".into());
        assert!(s.visible().is_empty());
        // the preview text is searchable, so a new message must invalidate
        s.apply(Event::Message {
            conv: vec![1u8; 8],
            from: "bob".into(),
            seq: 1,
            text: "we should discuss penguins".into(),
        });
        assert_eq!(s.visible().to_vec(), vec![0]);
    }

    #[test]
    fn cache_is_consistent_across_repeated_reads() {
        let mut s = with_two();
        let a = s.visible().to_vec();
        let b = s.visible().to_vec();
        assert_eq!(a, b, "a cached read must not change the result");
    }
}
