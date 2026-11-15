//! Desktop notifications via `notify-rust` on a background thread.

pub const MAX_BODY_CHARS: usize = 120;

/// Returns whether a notification should be emitted for an incoming message.
pub fn should_notify(notify_enabled: bool, is_active_conv: bool, is_self: bool) -> bool {
    notify_enabled && !is_active_conv && !is_self
}

/// Truncates text by character count and collapses newlines to spaces.
pub fn truncate(text: &str, max_chars: usize) -> String {
    let single_line = text.replace(['\n', '\r'], " ");
    let mut chars = single_line.chars();
    let mut out: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() {
        out.push('…');
    }
    out
}

pub fn chat_body(sender: &str, preview: &str) -> String {
    truncate(&format!("{sender}: {preview}"), MAX_BODY_CHARS)
}

/// Fires a desktop notification on a detached thread; errors are silently ignored.
pub fn show(summary: &str, body: &str) {
    let summary = summary.to_string();
    let body = body.to_string();
    std::thread::spawn(move || {
        let _ = notify_rust::Notification::new()
            .appname("teams")
            .summary(&summary)
            .body(&body)
            .timeout(notify_rust::Timeout::Milliseconds(6000))
            .show();
    });
}

#[cfg(test)]
mod tests {
    use super::{chat_body, should_notify, truncate, MAX_BODY_CHARS};

    #[test]
    fn notify_skips_active_room_self_and_disabled() {
        assert!(should_notify(true, false, false));
        assert!(!should_notify(false, false, false));
        assert!(!should_notify(true, true, false));
        assert!(!should_notify(true, false, true));
    }

    #[test]
    fn truncate_strips_newlines_and_limits_chars() {
        assert_eq!(truncate("hello\nworld", 100), "hello world");
        assert_eq!(truncate("가나다라마", 3), "가나다…");
        assert_eq!(truncate("abc", 3), "abc");
    }

    #[test]
    fn chat_body_contains_sender_and_fits_limit() {
        let body = chat_body("보낸이", "아주 긴 메시지");
        assert!(body.starts_with("보낸이: "));
        assert!(body.chars().count() <= MAX_BODY_CHARS + 1);
    }
}
