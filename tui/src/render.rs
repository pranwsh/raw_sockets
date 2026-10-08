//! Draws the chat UI: sidebar, transcript, composer, and status bar.
//!
//! Rendering is a pure projection of [`State`] into the frame buffer. It
//! allocates only the `Line`/`Span` values ratatui needs; the expensive work
//! (filtering, label building) is done once per redraw and bounded by what is
//! actually on screen.

use crate::state::{Focus, LoginField, Overlay, State};
use crate::util;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, BorderType, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap,
};
use ratatui::Frame;

const ACCENT: Color = Color::Cyan;
const DIM: Color = Color::DarkGray;
const OWN: Color = Color::Green;
const WARN: Color = Color::Red;

/// minimum width for the sidebar before it is hidden entirely
const SIDEBAR_MIN_W: u16 = 34;
/// minimum width of the transcript pane
const TRANSCRIPT_MIN_W: u16 = 20;

/// Draw the whole frame.
pub fn draw(frame: &mut Frame, state: &mut State, epoch_base: std::time::Instant) {
    state.frames += 1;
    let area = frame.area();

    let show_sidebar = !state.wide && area.width >= SIDEBAR_MIN_W + TRANSCRIPT_MIN_W;
    let (sidebar_area, main_area) = if show_sidebar {
        // give the sidebar a third of the width, within sane bounds
        let w = (area.width / 3).clamp(SIDEBAR_MIN_W, 40);
        let [side, main] = Layout::horizontal([Constraint::Length(w), Constraint::Min(0)])
            .areas(area);
        (Some(side), main)
    } else {
        (None, area)
    };

    if let Some(side) = sidebar_area {
        draw_sidebar(frame, state, side);
    }
    draw_transcript(frame, state, main_area, epoch_base);
    draw_composer(frame, state, main_area);
    draw_status(frame, state, area);

    match state.overlay {
        Overlay::Filter => draw_filter(frame, state, area),
        Overlay::Login => draw_login(frame, state, area),
        Overlay::NewConv => draw_new_conv(frame, state, area),
        Overlay::None => {}
    }
}

// sidebar

/// Draw the conversation list.
fn draw_sidebar(frame: &mut Frame, state: &mut State, area: Rect) {
    let visible = state.visible();
    // clamp the cursor into range after a filter shrinks the list
    state.clamp_cursor(visible.len());

    // show how many of the total the filter is hiding
    let total = state.entries_count();
    let visible_len = visible.len();
    let title = if visible_len == total {
        format!("chats ({total})")
    } else {
        format!("chats ({visible_len}/{total})")
    };
    let border = if state.focus == Focus::Sidebar {
        Style::default().fg(ACCENT)
    } else {
        Style::default().fg(DIM)
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(border)
        .title(title);

    let inner = block.inner(area);
    if inner.height == 0 {
        frame.render_widget(block, area);
        return;
    }

    // only build as many rows as fit, plus one so the scroll math is visible
    let capacity = inner.height as usize;
    let start = visible.len().saturating_sub(capacity);
    let selected_pos = visible.iter().position(|&i| state.is_selected(i));

    let mut items: Vec<ListItem> = Vec::with_capacity(capacity);
    for (row, &idx) in visible.iter().enumerate().skip(start) {
        let is_selected = state.is_selected(idx);
        let is_cursor = row == state.cursor_on_row();
        items.push(conv_item(state, idx, is_selected, is_cursor));
    }

    frame.render_widget(block, area);

    let list = List::new(items).highlight_style(Style::default());
    let mut list_state = ListState::default();
    // ListState indexes within the rendered slice
    list_state.select(
        selected_pos
            .map(|p| p - start)
            .filter(|_| state.focus == Focus::Sidebar),
    );
    frame.render_stateful_widget(list, inner, &mut list_state);
}

/// One sidebar row: unread dot, label, and a preview line.
fn conv_item(state: &State, idx: usize, is_selected: bool, is_cursor: bool) -> ListItem<'static> {
    let c = &state.entries_at(idx);
    let marker = if is_cursor { "▌" } else { " " };
    let dot = if c.unread > 0 { "●" } else { " " };
    let dot_style = if c.unread > 0 {
        Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)
    } else {
        Style::default()
    };

    let count = if c.unread > 0 {
        format!(" {}", c.unread)
    } else {
        String::new()
    };

    let label_style = if is_selected {
        Style::default().fg(Color::Black).bg(ACCENT).add_modifier(Modifier::BOLD)
    } else if c.unread > 0 {
        Style::default().fg(Color::White).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::Gray)
    };

    let label = util::truncate(&state.label_of(idx), 28);
    let first = Line::from(vec![
        Span::styled(marker, Style::default().fg(ACCENT)),
        Span::styled(dot, dot_style),
        Span::styled(label, label_style),
        Span::styled(count, Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)),
    ]);

    let preview_style = if is_selected {
        Style::default().fg(Color::Black).bg(ACCENT)
    } else {
        Style::default().fg(DIM)
    };
    let preview = util::truncate(
        &if c.last_text.is_empty() { "(no messages yet)".to_string() } else { util::one_line(&c.last_text) },
        30,
    );
    let second = Line::from(vec![
        Span::styled("  ", preview_style),
        Span::styled(preview, preview_style),
    ]);

    ListItem::new(vec![first, second])
}

// transcript

/// Draw the message history for the selected conversation.
fn draw_transcript(
    frame: &mut Frame,
    state: &State,
    area: Rect,
    epoch_base: std::time::Instant,
) {
    let border = if state.focus == Focus::Messages {
        Style::default().fg(ACCENT)
    } else {
        Style::default().fg(DIM)
    };
    let title = match state.selected() {
        Some(c) => {
            let unread = if c.unread > 0 { format!(" ({} unread)", c.unread) } else { String::new() };
            format!("{}{}", state.selected_label(), unread)
        }
        None => "messages".to_string(),
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(border)
        .title(title);

    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.height == 0 {
        return;
    }

    let Some(conv) = state.selected() else {
        let hint = Paragraph::new(vec![
            Line::from(""),
            Line::from(Span::styled(
                "no conversation selected",
                Style::default().fg(DIM),
            )),
            Line::from(""),
            Line::from(Span::styled("press Tab to focus the list, /create alice,bob to start", Style::default().fg(DIM))),
        ])
        .alignment(Alignment::Center)
        .wrap(Wrap { trim: true });
        frame.render_widget(hint, inner);
        return;
    };

    let h = inner.height as usize;
    let total = conv.messages.len();
    // pinned to the newest message unless the user scrolled up
    let end = total.saturating_sub(state.scroll_offset());
    let start = end.saturating_sub(h);

    let lines: Vec<Line> = conv.messages[start..end]
        .iter()
        .map(|m| {
            let time = util::clock(m.at, epoch_base);
            let who = util::truncate(&m.from, 16);
            let (who_style, marker) = if m.own {
                (Style::default().fg(OWN), "▸ ")
            } else {
                (Style::default().fg(ACCENT), "  ")
            };
            Line::from(vec![
                Span::styled(time, Style::default().fg(DIM)),
                Span::raw(" "),
                Span::styled(marker, Style::default().fg(DIM)),
                Span::styled(format!("{who}: "), who_style.add_modifier(Modifier::BOLD)),
                Span::raw(m.text.clone()),
            ])
        })
        .collect();

    let p = Paragraph::new(lines).wrap(Wrap { trim: false });
    frame.render_widget(p, inner);

    // "scrolled" marker so the user knows they are not at the live edge
    if state.scroll_offset() > 0 {
        let tag = Rect { x: inner.x + inner.width.saturating_sub(12), y: inner.y, width: 12, height: 1 };
        frame.render_widget(
            Paragraph::new(Span::styled(
                " ▼ live ",
                Style::default().fg(Color::Black).bg(WARN),
            )),
            tag,
        );
    }
}

// composer

/// Draw the input box with a placeholder when empty.
fn draw_composer(frame: &mut Frame, state: &State, area: Rect) {
    let border = if state.focus == Focus::Composer && state.overlay == Overlay::None {
        Style::default().fg(ACCENT)
    } else {
        Style::default().fg(DIM)
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(border)
        .title(" input ");

    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.height == 0 {
        return;
    }

    let (text, style) = if state.input.is_empty() {
        let hint = if state.authenticated {
            "message…  (/create alice,bob  /list  /filter  /new  /quit)"
        } else {
            "press /login to authenticate"
        };
        (hint.to_string(), Style::default().fg(DIM).add_modifier(Modifier::ITALIC))
    } else {
        (state.input.clone(), Style::default().fg(Color::White))
    };

    let mut spans = vec![
        Span::styled("> ", Style::default().fg(ACCENT)),
        Span::styled(text, style),
    ];
    if state.focus == Focus::Composer && state.overlay == Overlay::None {
        // a block cursor keeps the insertion point obvious
        spans.push(Span::styled("▏", Style::default().fg(ACCENT)));
    }
    let line = Line::from(spans);

    frame.render_widget(Paragraph::new(line).wrap(Wrap { trim: true }), inner);
}

// status bar

/// Draw the one-line status bar.
fn draw_status(frame: &mut Frame, state: &State, area: Rect) {
    if area.height == 0 {
        return;
    }
    let conn = if state.connected { "● online" } else { "○ offline" };
    let conn_style = if state.connected { Style::default().fg(OWN) } else { Style::default().fg(WARN) };
    let auth = if state.authenticated { "auth" } else { "no-auth" };
    let me = if state.me.is_empty() { "-" } else { state.me.as_str() };

    let mut spans = vec![
        Span::styled(format!(" {conn} "), conn_style),
        Span::styled(format!(" {auth} "), Style::default().fg(DIM)),
        Span::styled(format!(" {me} "), Style::default().fg(Color::White).add_modifier(Modifier::BOLD)),
    ];

    if let Some(s) = &state.status {
        if !s.is_stale() {
            let style = if s.is_error {
                Style::default().fg(WARN)
            } else {
                Style::default().fg(DIM)
            };
            spans.push(Span::styled(format!("  {} ", s.text), style));
        }
    }

    let keys = "Tab focus  ↑↓ pick  Enter open  /filter  /login  ^C quit";
    let right_w = keys.len() as u16 + 2;
    if area.width > right_w + 20 {
        spans.push(Span::raw(" ".repeat((area.width.saturating_sub(right_w)) as usize)));
        spans.push(Span::styled(format!(" {keys} "), Style::default().fg(DIM)));
    }

    frame.render_widget(Paragraph::new(Line::from(spans)), Rect { height: 1, ..area });
}

// overlays

/// Centered modal of a fixed size, as fractions of the screen.
fn modal(area: Rect, w_pct: u16, h_pct: u16) -> Rect {
    let w = (area.width * w_pct / 100).max(20).min(area.width);
    let h = (area.height * h_pct / 100).max(3).min(area.height);
    Rect {
        x: area.x + (area.width.saturating_sub(w)) / 2,
        y: area.y + (area.height.saturating_sub(h)) / 2,
        width: w,
        height: h,
    }
}

/// Sidebar filter prompt.
fn draw_filter(frame: &mut Frame, state: &State, area: Rect) {
    let m = modal(area, 60, 20);
    frame.render_widget(Clear, m);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(ACCENT))
        .title(" filter ");
    let inner = block.inner(m);
    frame.render_widget(block, m);
    let line = Line::from(vec![
        Span::styled("search: ", Style::default().fg(DIM)),
        Span::styled(state.filter.clone(), Style::default().fg(Color::White)),
        Span::styled("▏", Style::default().fg(ACCENT)),
    ]);
    frame.render_widget(Paragraph::new(line), inner);
}

/// Credentials prompt.
fn draw_login(frame: &mut Frame, state: &State, area: Rect) {
    let m = modal(area, 70, 40);
    frame.render_widget(Clear, m);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(ACCENT))
        .title(" login ");
    let inner = block.inner(m);
    frame.render_widget(block, m);

    let field_style = |active: bool| {
        if active { Style::default().fg(Color::White) } else { Style::default().fg(DIM) }
    };
    let cursor = Span::styled("▏", Style::default().fg(ACCENT));
    let user_active = state.login_field == LoginField::User;
    let pass_active = state.login_field == LoginField::Password;

    let lines = vec![
        Line::from(vec![
            Span::styled("user    ", Style::default().fg(DIM)),
            Span::styled(state.login_user.clone(), field_style(user_active)),
            if user_active { cursor.clone() } else { Span::raw("") },
        ]),
        Line::from(vec![
            Span::styled("pass    ", Style::default().fg(DIM)),
            // mask the password
            Span::styled("*".repeat(state.login_pass.chars().count()), field_style(pass_active)),
            if pass_active { cursor } else { Span::raw("") },
        ]),
        Line::from(""),
        Line::from(Span::styled("Enter field  ·  Tab field  ·  Ctrl+Enter connect", Style::default().fg(DIM))),
    ];
    frame.render_widget(Paragraph::new(lines), inner);
}

/// New conversation prompt.
fn draw_new_conv(frame: &mut Frame, state: &State, area: Rect) {
    let m = modal(area, 70, 30);
    frame.render_widget(Clear, m);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(ACCENT))
        .title(" new conversation ");
    let inner = block.inner(m);
    frame.render_widget(block, m);

    let lines = vec![
        Line::from(vec![
            Span::styled("members ", Style::default().fg(DIM)),
            Span::styled(state.draft_members.clone(), Style::default().fg(Color::White)),
            Span::styled("▏", Style::default().fg(ACCENT)),
        ]),
        Line::from(""),
        Line::from(Span::styled("comma separated, e.g. bob,carol   ·   Enter create  ·   Esc cancel", Style::default().fg(DIM))),
    ];
    frame.render_widget(Paragraph::new(lines), inner);
}
