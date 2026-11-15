//! Color & style tokens for the Microsoft Teams TUI.

use ratatui::style::{Color, Modifier, Style};

#[derive(Debug, Clone)]
pub struct Theme {
    pub fg: Color,
    pub muted: Color,
    pub accent: Color,
    pub border: Color,
    pub border_focus: Color,
    pub selection_bg: Color,
    pub selection_fg: Color,
    pub unread: Color,
    pub own: Color,
    pub error: Color,
    pub success: Color,
    pub warning: Color,
    pub date_separator: Color,
    pub divider: Color,
    pub toast: Color,
    pub reply: Color,
    pub code_fg: Color,
    pub code_bg: Color,
    pub subject: Color,
    pub author_palette: [Color; 6],
}

impl Default for Theme {
    fn default() -> Self {
        Self {
            fg: Color::Reset,
            muted: Color::DarkGray,
            accent: Color::Cyan,
            border: Color::DarkGray,
            border_focus: Color::Cyan,
            selection_bg: Color::Rgb(42, 46, 74),
            selection_fg: Color::White,
            unread: Color::Yellow,
            own: Color::Cyan,
            error: Color::Red,
            success: Color::Green,
            warning: Color::Yellow,
            date_separator: Color::DarkGray,
            divider: Color::Yellow,
            toast: Color::LightCyan,
            reply: Color::Blue,
            code_fg: Color::LightYellow,
            code_bg: Color::Rgb(32, 34, 46),
            subject: Color::LightCyan,
            author_palette: [
                Color::Green,
                Color::Yellow,
                Color::Magenta,
                Color::Blue,
                Color::LightGreen,
                Color::LightRed,
            ],
        }
    }
}

impl Theme {
    /// Returns a stable color for an author ID or display name.
    pub fn author(&self, key: &str) -> Color {
        let mut hash: u64 = 5381;
        for byte in key.bytes() {
            hash = hash.wrapping_mul(33).wrapping_add(byte as u64);
        }
        self.author_palette[(hash as usize) % self.author_palette.len()]
    }

    pub fn border_style(&self, focused: bool) -> Style {
        let color = if focused {
            self.border_focus
        } else {
            self.border
        };
        Style::default().fg(color)
    }

    pub fn selection(&self) -> Style {
        Style::default()
            .bg(self.selection_bg)
            .fg(self.selection_fg)
            .add_modifier(Modifier::BOLD)
    }

    pub fn muted_style(&self) -> Style {
        Style::default().fg(self.muted)
    }

    pub fn title(&self, focused: bool) -> Style {
        if focused {
            Style::default()
                .fg(self.accent)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(self.muted)
        }
    }

    pub fn accent_style(&self) -> Style {
        Style::default().fg(self.accent)
    }

    pub fn subject_style(&self) -> Style {
        Style::default()
            .fg(self.subject)
            .add_modifier(Modifier::BOLD)
    }

    pub fn code_style(&self) -> Style {
        Style::default().fg(self.code_fg).bg(self.code_bg)
    }

    pub fn quote_style(&self) -> Style {
        Style::default()
            .fg(self.reply)
            .add_modifier(Modifier::ITALIC)
    }

    pub fn reaction_style(&self, me_reacted: bool) -> Style {
        if me_reacted {
            Style::default()
                .fg(self.selection_fg)
                .bg(self.selection_bg)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(self.accent)
        }
    }

    /// Maps Microsoft Graph presence availability to a glyph and color.
    pub fn presence_indicator(&self, availability: Option<&str>) -> (&'static str, Color) {
        match availability {
            Some("Available" | "AvailableIdle") => ("●", self.success),
            Some("Busy" | "BusyIdle" | "DoNotDisturb") => ("●", self.error),
            Some("Away" | "BeRightBack") => ("●", self.warning),
            _ => ("○", self.muted),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn author_colors_are_stable_and_empty_safe() {
        let theme = Theme::default();
        assert_eq!(theme.author("alice@contoso.com"), theme.author("alice@contoso.com"));
        let _ = theme.author("");
    }

    #[test]
    fn presence_indicators_map_teams_states() {
        let theme = Theme::default();
        assert_eq!(theme.presence_indicator(Some("Available")), ("●", Color::Green));
        assert_eq!(theme.presence_indicator(Some("Busy")), ("●", Color::Red));
        assert_eq!(theme.presence_indicator(Some("DoNotDisturb")), ("●", Color::Red));
        assert_eq!(theme.presence_indicator(Some("Away")), ("●", Color::Yellow));
        assert_eq!(theme.presence_indicator(Some("Offline")), ("○", Color::DarkGray));
        assert_eq!(theme.presence_indicator(None), ("○", Color::DarkGray));
    }

    #[test]
    fn focused_border_uses_accents() {
        let theme = Theme::default();
        assert_ne!(theme.border_style(true).fg, theme.border_style(false).fg);
    }
}
