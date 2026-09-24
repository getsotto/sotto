//! Ratatui theme adapter mapping Sotto theme tokens to terminal styles.

use ratatui::style::{Color as RatatuiColor, Modifier, Style};

use crate::theme::{Color, Theme};

/// Convert a Sotto [`Color`] to a Ratatui [`RatatuiColor`].
pub fn to_ratatui_color(color: &Color, active: bool) -> RatatuiColor {
    if !active {
        return RatatuiColor::Reset;
    }
    match color {
        Color::Rgb(r, g, b) => RatatuiColor::Rgb(*r, *g, *b),
        Color::Ansi(c) => RatatuiColor::Indexed(*c),
        Color::Default => RatatuiColor::Reset,
    }
}

/// Resolved Ratatui styles derived from a [`Theme`].
#[derive(Debug, Clone)]
pub struct TuiStyles {
    pub bg: RatatuiColor,
    pub fg: RatatuiColor,
    pub accent: RatatuiColor,
    pub success: RatatuiColor,
    pub warning: RatatuiColor,
    pub error: RatatuiColor,
    pub muted: RatatuiColor,
    pub border: RatatuiColor,
    pub active: bool,
}

impl TuiStyles {
    pub fn from_theme(theme: &Theme) -> Self {
        let active = theme.active;
        Self {
            bg: theme
                .bg
                .as_ref()
                .map(|c| to_ratatui_color(c, active))
                .unwrap_or(RatatuiColor::Reset),
            fg: to_ratatui_color(&theme.fg, active),
            accent: to_ratatui_color(&theme.accent, active),
            success: to_ratatui_color(&theme.success, active),
            warning: to_ratatui_color(&theme.warning, active),
            error: to_ratatui_color(&theme.error, active),
            muted: to_ratatui_color(&theme.muted, active),
            border: to_ratatui_color(&theme.border, active),
            active,
        }
    }

    pub fn text(&self) -> Style {
        if !self.active {
            Style::default()
        } else {
            Style::default().fg(self.fg)
        }
    }

    pub fn muted(&self) -> Style {
        if !self.active {
            Style::default()
        } else {
            Style::default().fg(self.muted)
        }
    }

    pub fn bold_accent(&self) -> Style {
        if !self.active {
            Style::default().add_modifier(Modifier::BOLD)
        } else {
            Style::default()
                .fg(self.accent)
                .add_modifier(Modifier::BOLD)
        }
    }

    pub fn accent(&self) -> Style {
        if !self.active {
            Style::default()
        } else {
            Style::default().fg(self.accent)
        }
    }

    pub fn success(&self) -> Style {
        if !self.active {
            Style::default()
        } else {
            Style::default().fg(self.success)
        }
    }

    pub fn warning(&self) -> Style {
        if !self.active {
            Style::default()
        } else {
            Style::default().fg(self.warning)
        }
    }

    pub fn border(&self) -> Style {
        if !self.active {
            Style::default()
        } else {
            Style::default().fg(self.border)
        }
    }

    pub fn selected(&self) -> Style {
        if !self.active {
            Style::default().add_modifier(Modifier::REVERSED)
        } else {
            Style::default()
                .fg(self.accent)
                .add_modifier(Modifier::BOLD)
        }
    }
}
