//! Small helpers shared by the state and render modules.

/// Render an 8-byte conversation id as a short, stable hex prefix.
///
/// The full id is 16 hex characters, which is unreadable in a narrow sidebar.
/// Eight characters (the low 32 bits) is short enough to scan while staying
/// unique across any realistic number of conversations.
pub fn hex_prefix(id: &[u8]) -> String {
    if id.is_empty() {
        return String::new();
    }
    // take the trailing bytes: the id is an FNV hash, so its low bits vary
    let tail = &id[id.len().saturating_sub(4)..];
    tail.iter().map(|b| format!("{b:02x}")).collect()
}

/// Shorten `text` to at most `max` display columns, ending with an ellipsis.
pub fn truncate(text: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    let count = text.chars().count();
    if count <= max {
        return text.to_string();
    }
    if max == 1 {
        return "…".to_string();
    }
    let mut out: String = text.chars().take(max - 1).collect();
    out.push('…');
    out
}

/// Collapse whitespace and clip to one line, for sidebar previews.
pub fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// A coarse human-readable age, for the transcript gutter.
pub fn age_short(at: std::time::Instant) -> String {
    let secs = at.elapsed().as_secs();
    match secs {
        0..=59 => format!("{}s", secs),
        60..=3599 => format!("{}m", secs / 60),
        3600..=86_399 => format!("{}h", secs / 3600),
        _ => format!("{}d", secs / 86_400),
    }
}

/// A timestamp in local wall-clock time, `HH:MM`.
///
/// `chrono`/`time` would be heavier than this frontend wants; the transcript
/// only needs a readable local clock.
pub fn clock(at: std::time::Instant, epoch_base: std::time::Instant) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let since = at.saturating_duration_since(epoch_base).as_secs();
    let t = now.saturating_sub(since);
    let mins = (t / 60) % 60;
    let hours = (t / 3600) % 24;
    format!("{hours:02}:{mins:02}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_prefix_uses_low_bytes() {
        // a full 8-byte id renders as its trailing 4 bytes (8 hex chars)
        assert_eq!(hex_prefix(&[0x11, 0x22, 0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff]), "ccddeeff");
        // shorter ids render in full
        assert_eq!(hex_prefix(&[1, 2]), "0102");
        assert_eq!(hex_prefix(&[]), "");
    }

    #[test]
    fn truncate_clips_with_ellipsis() {
        assert_eq!(truncate("hello", 10), "hello");
        assert_eq!(truncate("hello", 5), "hello");
        assert_eq!(truncate("hello", 4), "hel…");
        assert_eq!(truncate("hello", 1), "…");
        assert_eq!(truncate("hello", 0), "");
    }

    #[test]
    fn one_line_collapses_whitespace() {
        assert_eq!(one_line("  a  b\n c "), "a b c");
        assert_eq!(one_line(""), "");
    }
}
