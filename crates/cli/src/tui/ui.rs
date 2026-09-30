//! Rendering functions and widgets for the interactive Sotto dashboard.

use std::time::Duration;

use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, Paragraph, Wrap};
use ratatui::Frame;

use crate::tui::app::TuiApp;
use crate::tui::theme::{to_ratatui_color, TuiStyles};

/// Render the complete TUI dashboard frame.
pub fn draw(f: &mut Frame, app: &TuiApp) {
    let size = f.area();

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3), // Header
            Constraint::Min(8),    // Split-pane body
            Constraint::Length(1), // Footer status / shortcuts
        ])
        .split(size);

    draw_header(f, app, chunks[0]);
    draw_body(f, app, chunks[1]);
    draw_footer(f, app, chunks[2]);

    if app.show_help {
        draw_help_modal(f, app, size);
    } else if app.show_theme_modal {
        draw_theme_modal(f, app, size);
    } else if app.show_secret_modal {
        draw_secret_modal(f, app, size);
    } else if app.show_delete_modal {
        draw_delete_modal(f, app, size);
    } else if app.show_history_modal {
        draw_history_modal(f, app, size);
    }
}

fn draw_header(f: &mut Frame, app: &TuiApp, area: Rect) {
    let styles = &app.styles;

    let header_line = Line::from(vec![
        Span::styled(" Sotto ", styles.bold_accent()),
        Span::styled(" [project: ", styles.muted()),
        Span::styled(&app.config.project, styles.text()),
        Span::styled("]  [env: ", styles.muted()),
        Span::styled(&app.config.environment, styles.bold_accent()),
        Span::styled("]  [status: ", styles.muted()),
        Span::styled("unlocked", styles.success()),
        Span::styled("] ", styles.muted()),
    ]);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(styles.border());

    let paragraph = Paragraph::new(header_line)
        .block(block)
        .alignment(Alignment::Left);

    f.render_widget(paragraph, area);
}

fn draw_body(f: &mut Frame, app: &TuiApp, area: Rect) {
    let body_chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(42), // Left: search + secrets list
            Constraint::Percentage(58), // Right: secret inspector
        ])
        .split(area);

    draw_left_pane(f, app, body_chunks[0]);
    draw_right_pane(f, app, body_chunks[1]);
}

fn draw_left_pane(f: &mut Frame, app: &TuiApp, area: Rect) {
    let styles = &app.styles;

    let left_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3), // Search bar
            Constraint::Min(5),    // Secrets list
        ])
        .split(area);

    // Search bar
    let search_title = if app.search_mode {
        " Search (typing... Esc to clear) "
    } else {
        " Search (/ to filter) "
    };

    let search_content = if app.search_mode {
        Line::from(vec![
            Span::styled("/ ", styles.accent()),
            Span::styled(&app.search_query, styles.text()),
            Span::styled("▏", styles.accent()),
        ])
    } else if app.search_query.is_empty() {
        Line::from(vec![Span::styled(
            "Press / to filter secrets...",
            styles.muted(),
        )])
    } else {
        Line::from(vec![
            Span::styled("/ ", styles.accent()),
            Span::styled(&app.search_query, styles.text()),
        ])
    };

    let search_block = Block::default()
        .borders(Borders::ALL)
        .border_style(if app.search_mode {
            styles.bold_accent()
        } else {
            styles.border()
        })
        .title(Span::styled(
            search_title,
            if app.search_mode {
                styles.bold_accent()
            } else {
                styles.muted()
            },
        ));

    f.render_widget(
        Paragraph::new(search_content).block(search_block),
        left_chunks[0],
    );

    // Secrets list
    let list_title = format!(
        " Secrets ({}/{}) ",
        app.filtered_indices.len(),
        app.secrets.len()
    );

    let list_block = Block::default()
        .borders(Borders::ALL)
        .border_style(styles.border())
        .title(Span::styled(list_title, styles.muted()));

    let items: Vec<ListItem> = if app.filtered_indices.is_empty() {
        vec![ListItem::new(Line::from(Span::styled(
            "  (no matching secrets)",
            styles.muted(),
        )))]
    } else {
        app.filtered_indices
            .iter()
            .enumerate()
            .map(|(filter_idx, &orig_idx)| {
                let is_selected = filter_idx == app.selected_filtered_index;
                let item = &app.secrets[orig_idx];

                if is_selected {
                    ListItem::new(Line::from(vec![
                        Span::styled("▸ ", styles.bold_accent()),
                        Span::styled(&item.name, styles.bold_accent()),
                        Span::styled(format!(" v{}", item.version), styles.muted()),
                    ]))
                } else {
                    ListItem::new(Line::from(vec![
                        Span::raw("  "),
                        Span::styled(&item.name, styles.text()),
                        Span::styled(format!(" v{}", item.version), styles.muted()),
                    ]))
                }
            })
            .collect()
    };

    let list = List::new(items).block(list_block);
    f.render_widget(list, left_chunks[1]);
}

fn draw_right_pane(f: &mut Frame, app: &TuiApp, area: Rect) {
    let styles = &app.styles;

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(styles.border())
        .title(Span::styled(" Secret Inspector ", styles.muted()));

    if let Some(item) = app.selected_secret() {
        let mut lines = Vec::new();

        lines.push(Line::from(vec![
            Span::styled("Key:         ", styles.muted()),
            Span::styled(&item.name, styles.bold_accent()),
        ]));
        lines.push(Line::from(vec![
            Span::styled("Environment: ", styles.muted()),
            Span::styled(&app.config.environment, styles.text()),
        ]));
        lines.push(Line::from(vec![
            Span::styled("Version:     ", styles.muted()),
            Span::styled(format!("v{}", item.version), styles.text()),
        ]));
        lines.push(Line::from(""));

        // Secret value inspection: ratatui's grapheme cell buffer structurally sanitises
        // ANSI escapes and control characters before terminal output, making manual
        // display_secret escaping unnecessary.
        lines.push(Line::from(Span::styled("Value:", styles.muted())));

        if app.revealed {
            if let Some(cache) = &app.decrypted_cache {
                match std::str::from_utf8(cache) {
                    Ok(text) => {
                        let is_animating = app
                            .reveal_animation_start
                            .map(|start| start.elapsed() < Duration::from_millis(150))
                            .unwrap_or(false);

                        if is_animating {
                            let elapsed = app
                                .reveal_animation_start
                                .map(|s| s.elapsed())
                                .unwrap_or_default();
                            let progress = (elapsed.as_secs_f32() / 0.150).clamp(0.0, 1.0);

                            for line in text.lines() {
                                lines.push(render_cipher_unscramble_line(
                                    line, progress, elapsed, styles,
                                ));
                            }
                        } else {
                            for line in text.lines() {
                                lines.push(Line::from(Span::styled(
                                    format!("  {line}"),
                                    styles.text(),
                                )));
                            }
                        }
                    }
                    Err(_) => {
                        lines.push(Line::from(Span::styled("  [binary data]", styles.muted())));
                    }
                }
            }
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                "⚠ plaintext unmasked (press `r` to conceal)",
                styles.warning(),
            )));
        } else {
            lines.push(Line::from(Span::styled(
                "  ••••••••••••••••••••",
                styles.muted(),
            )));
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                "press `r` to reveal plaintext",
                styles.muted(),
            )));
        }

        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled("Actions:", styles.muted())));
        lines.push(Line::from(vec![
            Span::styled("  [c] ", styles.bold_accent()),
            Span::styled("Copy secret to clipboard (45s clear)", styles.text()),
        ]));
        lines.push(Line::from(vec![
            Span::styled("  [r] ", styles.bold_accent()),
            Span::styled(
                if app.revealed {
                    "Conceal secret value"
                } else {
                    "Reveal secret value"
                },
                styles.text(),
            ),
        ]));
        lines.push(Line::from(vec![
            Span::styled("  [e] ", styles.bold_accent()),
            Span::styled("Edit secret value", styles.text()),
        ]));
        lines.push(Line::from(vec![
            Span::styled("  [d] ", styles.bold_accent()),
            Span::styled("Delete secret", styles.text()),
        ]));
        lines.push(Line::from(vec![
            Span::styled("  [h] ", styles.bold_accent()),
            Span::styled("View version history", styles.text()),
        ]));
        lines.push(Line::from(vec![
            Span::styled("  [Tab] ", styles.bold_accent()),
            Span::styled("Cycle active environment", styles.text()),
        ]));

        let paragraph = Paragraph::new(lines)
            .block(block)
            .wrap(Wrap { trim: false });
        f.render_widget(paragraph, area);
    } else {
        let empty_msg = Paragraph::new(Line::from(Span::styled(
            "No secret selected",
            styles.muted(),
        )))
        .block(block)
        .alignment(Alignment::Center);

        f.render_widget(empty_msg, area);
    }
}

fn draw_footer(f: &mut Frame, app: &TuiApp, area: Rect) {
    let styles = &app.styles;

    let content = if let Some(msg) = app.active_status() {
        Line::from(vec![
            Span::styled("✓ ", styles.success()),
            Span::styled(msg, styles.bold_accent()),
        ])
    } else {
        Line::from(vec![
            Span::styled("[?] ", styles.bold_accent()),
            Span::styled("Help  ", styles.muted()),
            Span::styled("[/] ", styles.bold_accent()),
            Span::styled("Search  ", styles.muted()),
            Span::styled("[Tab] ", styles.bold_accent()),
            Span::styled("Env  ", styles.muted()),
            Span::styled("[n] ", styles.bold_accent()),
            Span::styled("New  ", styles.muted()),
            Span::styled("[e] ", styles.bold_accent()),
            Span::styled("Edit  ", styles.muted()),
            Span::styled("[d] ", styles.bold_accent()),
            Span::styled("Delete  ", styles.muted()),
            Span::styled("[c] ", styles.bold_accent()),
            Span::styled("Copy  ", styles.muted()),
            Span::styled("[r] ", styles.bold_accent()),
            Span::styled("Reveal  ", styles.muted()),
            Span::styled("[t] ", styles.bold_accent()),
            Span::styled("Theme  ", styles.muted()),
            Span::styled("[q] ", styles.bold_accent()),
            Span::styled("Quit", styles.muted()),
        ])
    };

    let paragraph = Paragraph::new(content).alignment(Alignment::Left);
    f.render_widget(paragraph, area);
}

fn draw_help_modal(f: &mut Frame, app: &TuiApp, area: Rect) {
    let styles = &app.styles;

    let popup_width = 54.min(area.width.saturating_sub(4));
    let popup_height = 24.min(area.height.saturating_sub(2));

    let x = (area.width.saturating_sub(popup_width)) / 2;
    let y = (area.height.saturating_sub(popup_height)) / 2;
    let popup_area = Rect::new(x, y, popup_width, popup_height);

    f.render_widget(Clear, popup_area);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(styles.bold_accent())
        .title(Span::styled(" Keyboard Shortcuts ", styles.bold_accent()));

    let shortcuts = vec![
        Line::from(vec![
            Span::styled("  ↑ / k       ", styles.bold_accent()),
            Span::styled("Move selection up", styles.text()),
        ]),
        Line::from(vec![
            Span::styled("  ↓ / j       ", styles.bold_accent()),
            Span::styled("Move selection down", styles.text()),
        ]),
        Line::from(vec![
            Span::styled("  Home / End  ", styles.bold_accent()),
            Span::styled("Jump to top / bottom", styles.text()),
        ]),
        Line::from(vec![
            Span::styled("  PgUp / PgDn ", styles.bold_accent()),
            Span::styled("Jump 10 items up / down", styles.text()),
        ]),
        Line::from(vec![
            Span::styled("  /           ", styles.bold_accent()),
            Span::styled("Focus search filter", styles.text()),
        ]),
        Line::from(vec![
            Span::styled("  Esc         ", styles.bold_accent()),
            Span::styled("Clear filter / close modal", styles.text()),
        ]),
        Line::from(vec![
            Span::styled("  Tab         ", styles.bold_accent()),
            Span::styled("Cycle active environment", styles.text()),
        ]),
        Line::from(vec![
            Span::styled("  n           ", styles.bold_accent()),
            Span::styled("Create new secret in environment", styles.text()),
        ]),
        Line::from(vec![
            Span::styled("  e           ", styles.bold_accent()),
            Span::styled("Edit selected secret value", styles.text()),
        ]),
        Line::from(vec![
            Span::styled("  d           ", styles.bold_accent()),
            Span::styled("Delete selected secret", styles.text()),
        ]),
        Line::from(vec![
            Span::styled("  h           ", styles.bold_accent()),
            Span::styled("View secret version history", styles.text()),
        ]),
        Line::from(vec![
            Span::styled("  c           ", styles.bold_accent()),
            Span::styled("Copy secret to clipboard (45s)", styles.text()),
        ]),
        Line::from(vec![
            Span::styled("  r           ", styles.bold_accent()),
            Span::styled("Toggle secret reveal / mask", styles.text()),
        ]),
        Line::from(vec![
            Span::styled("  t           ", styles.bold_accent()),
            Span::styled("Open live theme switcher modal", styles.text()),
        ]),
        Line::from(vec![
            Span::styled("  ?           ", styles.bold_accent()),
            Span::styled("Toggle this help screen", styles.text()),
        ]),
        Line::from(vec![
            Span::styled("  q / Ctrl+C  ", styles.bold_accent()),
            Span::styled("Quit Sotto", styles.text()),
        ]),
        Line::from(""),
        Line::from(Span::styled("  Press Esc or ? to close", styles.muted())),
    ];

    let paragraph = Paragraph::new(shortcuts)
        .block(block)
        .alignment(Alignment::Left);

    f.render_widget(paragraph, popup_area);
}

fn draw_theme_modal(f: &mut Frame, app: &TuiApp, area: Rect) {
    let styles = &app.styles;

    let theme_count = app.available_themes.len() as u16;
    let popup_width = 54.min(area.width.saturating_sub(4));
    let popup_height = (theme_count + 6).min(area.height.saturating_sub(2));

    let x = (area.width.saturating_sub(popup_width)) / 2;
    let y = (area.height.saturating_sub(popup_height)) / 2;
    let popup_area = Rect::new(x, y, popup_width, popup_height);

    f.render_widget(Clear, popup_area);

    let total_themes = app.available_themes.len();
    let max_visible = (popup_height.saturating_sub(5) as usize)
        .max(1)
        .min(total_themes);

    // Compute the scrolling window so selected_theme_index is always visible
    let start_idx = if app.selected_theme_index >= max_visible {
        (app.selected_theme_index + 1).saturating_sub(max_visible)
    } else {
        0
    };
    let end_idx = (start_idx + max_visible).min(total_themes);

    let title_text = if start_idx > 0 && end_idx < total_themes {
        " Theme Switcher (↑/↓ more) "
    } else if start_idx > 0 {
        " Theme Switcher (↑ more) "
    } else if end_idx < total_themes {
        " Theme Switcher (↓ more) "
    } else {
        " Theme Switcher "
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(styles.bold_accent())
        .title(Span::styled(title_text, styles.bold_accent()));

    let mut lines = Vec::new();
    lines.push(Line::from(""));

    for idx in start_idx..end_idx {
        let theme = &app.available_themes[idx];
        let is_selected = idx == app.selected_theme_index;
        let is_original = theme.name.eq_ignore_ascii_case(&app.original_theme.name);

        let mut spans = Vec::new();
        if is_selected {
            spans.push(Span::styled(" ▸ ", styles.bold_accent()));
            spans.push(Span::styled(
                format!("{:<13}", theme.name),
                styles.bold_accent(),
            ));
        } else {
            spans.push(Span::raw("   "));
            spans.push(Span::styled(format!("{:<13}", theme.name), styles.text()));
        }

        // Swatch previews
        spans.push(Span::styled(
            " ■",
            Style::default().fg(to_ratatui_color(&theme.accent, styles.active)),
        ));
        spans.push(Span::styled(
            "■",
            Style::default().fg(to_ratatui_color(&theme.success, styles.active)),
        ));
        spans.push(Span::styled(
            "■",
            Style::default().fg(to_ratatui_color(&theme.warning, styles.active)),
        ));
        spans.push(Span::styled(
            "■ ",
            Style::default().fg(to_ratatui_color(&theme.error, styles.active)),
        ));

        if is_selected {
            if is_original {
                spans.push(Span::styled(" (current)", styles.success()));
            } else {
                spans.push(Span::styled(" (preview)", styles.accent()));
            }
        } else if is_original {
            spans.push(Span::styled(" (current)", styles.muted()));
        }

        lines.push(Line::from(spans));
    }

    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        Span::styled("  [Enter] ", styles.bold_accent()),
        Span::styled("Apply   ", styles.text()),
        Span::styled("[Esc] ", styles.bold_accent()),
        Span::styled("Cancel   ", styles.text()),
        Span::styled("[↑/↓] ", styles.bold_accent()),
        Span::styled("Preview", styles.text()),
    ]));

    let paragraph = Paragraph::new(lines)
        .block(block)
        .alignment(Alignment::Left);

    f.render_widget(paragraph, popup_area);
}

fn draw_secret_modal(f: &mut Frame, app: &TuiApp, area: Rect) {
    let styles = &app.styles;

    let popup_width = 62.min(area.width.saturating_sub(4));
    let popup_height = 14.min(area.height.saturating_sub(2));

    let x = (area.width.saturating_sub(popup_width)) / 2;
    let y = (area.height.saturating_sub(popup_height)) / 2;
    let popup_area = Rect::new(x, y, popup_width, popup_height);

    f.render_widget(Clear, popup_area);

    let title = match app.secret_modal_mode {
        crate::tui::app::SecretModalMode::New => " New Secret ",
        crate::tui::app::SecretModalMode::Edit => " Edit Secret ",
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(styles.bold_accent())
        .title(Span::styled(title, styles.bold_accent()));

    let mut lines = Vec::new();
    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        Span::styled("  Environment: ", styles.muted()),
        Span::styled(&app.config.environment, styles.bold_accent()),
    ]));
    lines.push(Line::from(""));

    match app.secret_modal_mode {
        crate::tui::app::SecretModalMode::New => {
            let is_name_focused = app.secret_modal_field == crate::tui::app::SecretModalField::Name;
            let cursor = if is_name_focused { "▏" } else { "" };
            lines.push(Line::from(vec![
                Span::styled(
                    "  Name:  ",
                    if is_name_focused {
                        styles.bold_accent()
                    } else {
                        styles.muted()
                    },
                ),
                Span::styled(
                    format!("[{}{}]", app.secret_modal_name, cursor),
                    if is_name_focused {
                        styles.text()
                    } else {
                        styles.muted()
                    },
                ),
            ]));
        }
        crate::tui::app::SecretModalMode::Edit => {
            lines.push(Line::from(vec![
                Span::styled("  Name:  ", styles.muted()),
                Span::styled(
                    format!("{} (read-only)", app.secret_modal_name),
                    styles.text(),
                ),
            ]));
        }
    }

    lines.push(Line::from(""));

    let is_value_focused = app.secret_modal_field == crate::tui::app::SecretModalField::Value;
    let cursor = if is_value_focused { "▏" } else { "" };
    let mask_hint = if app.secret_modal_masked {
        " (masked)"
    } else {
        " (revealed)"
    };
    let displayed_value = if app.secret_modal_masked {
        "•".repeat(app.secret_modal_value.len())
    } else {
        app.secret_modal_value.to_string()
    };

    lines.push(Line::from(vec![
        Span::styled(
            "  Value: ",
            if is_value_focused {
                styles.bold_accent()
            } else {
                styles.muted()
            },
        ),
        Span::styled(
            format!("[{}{}]", displayed_value, cursor),
            if is_value_focused {
                styles.text()
            } else {
                styles.muted()
            },
        ),
        Span::styled(mask_hint, styles.muted()),
    ]));

    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        Span::styled("  [Tab] ", styles.bold_accent()),
        Span::styled("Switch Field   ", styles.muted()),
        Span::styled("[Ctrl+G] ", styles.bold_accent()),
        Span::styled("Generate   ", styles.muted()),
        Span::styled("[Ctrl+R] ", styles.bold_accent()),
        Span::styled("Mask/Reveal", styles.muted()),
    ]));
    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        Span::styled("  [Enter] ", styles.bold_accent()),
        Span::styled("Save   ", styles.success()),
        Span::styled("[Esc] ", styles.bold_accent()),
        Span::styled("Cancel", styles.muted()),
    ]));

    let paragraph = Paragraph::new(lines)
        .block(block)
        .wrap(Wrap { trim: false });
    f.render_widget(paragraph, popup_area);
}

fn draw_delete_modal(f: &mut Frame, app: &TuiApp, area: Rect) {
    let styles = &app.styles;

    let popup_width = 54.min(area.width.saturating_sub(4));
    let popup_height = 9.min(area.height.saturating_sub(2));

    let x = (area.width.saturating_sub(popup_width)) / 2;
    let y = (area.height.saturating_sub(popup_height)) / 2;
    let popup_area = Rect::new(x, y, popup_width, popup_height);

    f.render_widget(Clear, popup_area);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(styles.error())
        .title(Span::styled(" Delete Secret ", styles.error()));

    let lines = vec![
        Line::from(""),
        Line::from(vec![
            Span::styled("  Delete `", styles.text()),
            Span::styled(&app.delete_modal_secret_name, styles.bold_accent()),
            Span::styled(
                format!("` from `{}`?", app.config.environment),
                styles.text(),
            ),
        ]),
        Line::from(Span::styled(
            "  This action cannot be undone.",
            styles.warning(),
        )),
        Line::from(""),
        Line::from(vec![
            Span::styled("  [y/Enter] ", styles.error()),
            Span::styled("Delete Secret    ", styles.error()),
            Span::styled("[n/Esc] ", styles.bold_accent()),
            Span::styled("Cancel", styles.muted()),
        ]),
    ];

    let paragraph = Paragraph::new(lines)
        .block(block)
        .wrap(Wrap { trim: false });
    f.render_widget(paragraph, popup_area);
}

fn format_relative_time(created_at_ms: i64) -> String {
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64;
    let diff_ms = (now_ms - created_at_ms).max(0);
    let diff_secs = diff_ms / 1000;
    if diff_secs < 60 {
        "just now".to_string()
    } else if diff_secs < 3600 {
        let mins = diff_secs / 60;
        format!("{mins}m ago")
    } else if diff_secs < 86400 {
        let hours = diff_secs / 3600;
        format!("{hours}h ago")
    } else {
        let days = diff_secs / 86400;
        format!("{days}d ago")
    }
}

fn draw_history_modal(f: &mut Frame, app: &TuiApp, area: Rect) {
    let styles = &app.styles;

    let popup_width = 72.min(area.width.saturating_sub(4));
    let popup_height = 18.min(area.height.saturating_sub(2));

    let x = (area.width.saturating_sub(popup_width)) / 2;
    let y = (area.height.saturating_sub(popup_height)) / 2;
    let popup_area = Rect::new(x, y, popup_width, popup_height);

    f.render_widget(Clear, popup_area);

    let title = format!(" Version History: {} ", app.history_modal_secret_name);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(styles.bold_accent())
        .title(Span::styled(title, styles.bold_accent()));

    let mut lines = Vec::new();
    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        Span::styled("  Environment: ", styles.muted()),
        Span::styled(&app.config.environment, styles.bold_accent()),
        Span::styled("  |  Total: ", styles.muted()),
        Span::styled(
            format!("{} version(s)", app.history_modal_items.len()),
            styles.text(),
        ),
    ]));
    lines.push(Line::from(""));

    if app.history_modal_items.is_empty() {
        lines.push(Line::from(Span::styled(
            "  No versions recorded for this secret.",
            styles.muted(),
        )));
    } else {
        let available_slots = popup_height.saturating_sub(8) as usize;
        let max_visible = available_slots.max(1);
        let total_items = app.history_modal_items.len();
        let selected = app.history_modal_selected_index;

        let start = if total_items <= max_visible || selected < max_visible / 2 {
            0
        } else if selected + max_visible / 2 >= total_items {
            total_items.saturating_sub(max_visible)
        } else {
            selected.saturating_sub(max_visible / 2)
        };
        let end = (start + max_visible).min(total_items);

        if start > 0 {
            lines.push(Line::from(Span::styled(
                "  (↑ more versions above)",
                styles.muted(),
            )));
        }

        for (idx, item) in app
            .history_modal_items
            .iter()
            .enumerate()
            .take(end)
            .skip(start)
        {
            let is_selected = idx == selected;
            let cursor = if is_selected { "▸ " } else { "  " };
            let latest_tag = if idx == 0 { " (latest)" } else { "" };
            let time_str = format_relative_time(item.created_at);

            let value_repr = if app.history_modal_revealed {
                if let Some(val) = &item.value {
                    match std::str::from_utf8(val) {
                        Ok(text) => text.lines().next().unwrap_or("").to_string(),
                        Err(_) => "[binary data]".to_string(),
                    }
                } else {
                    "[unreadable]".to_string()
                }
            } else if let Some(val) = &item.value {
                format!("•••••••••••••••••••• ({}B)", val.len())
            } else {
                "[unreadable]".to_string()
            };

            let row_spans = vec![
                Span::styled(
                    cursor,
                    if is_selected {
                        styles.bold_accent()
                    } else {
                        styles.muted()
                    },
                ),
                Span::styled(
                    format!("v{}{latest_tag:<9}", item.version),
                    if is_selected {
                        styles.bold_accent()
                    } else {
                        styles.text()
                    },
                ),
                Span::styled(format!("{time_str:<10}"), styles.muted()),
                Span::styled(
                    format!(" {value_repr}"),
                    if is_selected {
                        styles.selected()
                    } else {
                        styles.text()
                    },
                ),
            ];
            lines.push(Line::from(row_spans));
        }

        if end < total_items {
            lines.push(Line::from(Span::styled(
                "  (↓ more versions below)",
                styles.muted(),
            )));
        }
    }

    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        Span::styled("  [r] ", styles.bold_accent()),
        Span::styled(
            if app.history_modal_revealed {
                "Conceal   "
            } else {
                "Reveal   "
            },
            styles.muted(),
        ),
        Span::styled("[c] ", styles.bold_accent()),
        Span::styled("Copy   ", styles.muted()),
        Span::styled("[Enter] ", styles.bold_accent()),
        Span::styled("Restore   ", styles.success()),
        Span::styled("[Esc] ", styles.bold_accent()),
        Span::styled("Close", styles.muted()),
    ]));

    let paragraph = Paragraph::new(lines)
        .block(block)
        .wrap(Wrap { trim: false });
    f.render_widget(paragraph, popup_area);
}

const CIPHER_GLYPHS: &[char] = &['%', '#', '*', '@', '&', '?', '0', '1', '$', '!'];

fn render_cipher_unscramble_line<'a>(
    line: &str,
    progress: f32,
    elapsed: Duration,
    styles: &'a TuiStyles,
) -> Line<'a> {
    let char_count = line.chars().count();
    if char_count == 0 {
        return Line::from(Span::raw("  "));
    }

    let mut spans = vec![Span::raw("  ")];
    let revealed_count = ((char_count as f32) * progress).floor() as usize;
    let tick = (elapsed.as_millis() / 25) as usize;

    for (idx, ch) in line.chars().enumerate() {
        if idx < revealed_count {
            spans.push(Span::styled(ch.to_string(), styles.text()));
        } else {
            let glyph_idx = (idx + tick + (ch as usize)) % CIPHER_GLYPHS.len();
            let cipher_char = CIPHER_GLYPHS[glyph_idx];
            spans.push(Span::styled(cipher_char.to_string(), styles.bold_accent()));
        }
    }

    Line::from(spans)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::App;
    use crate::config::Config;
    use crate::keychain::MemoryKeychain;
    use crate::session;
    use crate::store::Store;
    use crate::theme::Theme;
    use crate::vault::Vault;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;
    use std::time::Duration;

    fn unlocked() -> (Store, MemoryKeychain, Config) {
        let store = Store::open_in_memory().unwrap();
        let keychain = MemoryKeychain::default();
        session::init(&store, &keychain, b"pw", Duration::from_secs(3600)).unwrap();
        let master = session::current_master_key(&keychain).unwrap().unwrap();
        let keypair = session::account_keypair(&store, &master).unwrap();
        let project = Vault::create_project(&store, &keypair, "acme").unwrap();
        let config = Config {
            project_id: project.id,
            project: "acme".into(),
            environment: "dev".into(),
            org_id: None,
        };
        (store, keychain, config)
    }

    #[test]
    fn render_ui_with_version_and_unlocked_status() {
        let (store, keychain, config) = unlocked();
        let theme = Theme::default();
        let app = App::new(&store, &keychain);
        app.set(&config, "DATABASE_URL", b"postgres://localhost")
            .unwrap();

        let tui_app = TuiApp::new(&app, &store, config, &theme).unwrap();
        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).unwrap();

        terminal.draw(|f| draw(f, &tui_app)).unwrap();
        let buffer = terminal.backend().buffer();
        let content = format!("{buffer:?}");

        // Verify "unlocked" appears in status
        assert!(content.contains("unlocked"));
        // Verify version badge appears in list
        assert!(content.contains("DATABASE_URL"));
        assert!(content.contains("v1"));
    }

    #[test]
    fn terminal_size_boundary_render_sweep() {
        let (store, keychain, config) = unlocked();
        let theme = Theme::default();
        let app = App::new(&store, &keychain);
        app.set(&config, "DATABASE_URL", b"postgres://localhost")
            .unwrap();
        app.set(&config, "API_KEY", b"super-secret-token").unwrap();

        let sizes: [(u16, u16); 10] = [
            (80, 24),
            (40, 12),
            (20, 10),
            (10, 6),
            (5, 4),
            (3, 3),
            (2, 2),
            (1, 1),
            (80, 1),
            (1, 24),
        ];

        for (width, height) in sizes {
            for modal_state in 0..7 {
                let mut tui_app = TuiApp::new(&app, &store, config.clone(), &theme).unwrap();
                match modal_state {
                    0 => {} // Normal view
                    1 => tui_app.show_help = true,
                    2 => tui_app.open_theme_modal(),
                    3 => tui_app.open_new_secret_modal(),
                    4 => tui_app.open_edit_secret_modal().unwrap(),
                    5 => tui_app.open_delete_modal(),
                    6 => tui_app.open_history_modal().unwrap(),
                    _ => {}
                }
                let backend = TestBackend::new(width, height);
                let mut terminal = Terminal::new(backend).unwrap();
                terminal.draw(|f| draw(f, &tui_app)).unwrap_or_else(|e| {
                    panic!(
                        "failed rendering at size {width}x{height} (modal_state={modal_state}): {e}"
                    );
                });
            }
        }
    }

    #[test]
    fn render_masked_and_revealed_inspector() {
        let (store, keychain, config) = unlocked();
        let theme = Theme::default();
        let app = App::new(&store, &keychain);
        app.set(&config, "SECRET_TOKEN", b"super-secret-cleartext")
            .unwrap();

        let mut tui_app = TuiApp::new(&app, &store, config, &theme).unwrap();
        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).unwrap();

        // Masked state
        terminal.draw(|f| draw(f, &tui_app)).unwrap();
        let buffer = terminal.backend().buffer();
        let content = format!("{buffer:?}");
        assert!(content.contains("••••••••••••••••••••"));
        assert!(content.contains("press `r` to reveal plaintext"));
        assert!(!content.contains("super-secret-cleartext"));

        // Revealed state (completed animation)
        tui_app.toggle_reveal().unwrap();
        tui_app.reveal_animation_start =
            Some(std::time::Instant::now() - Duration::from_millis(250));
        terminal.draw(|f| draw(f, &tui_app)).unwrap();
        let buffer = terminal.backend().buffer();
        let content = format!("{buffer:?}");
        assert!(content.contains("super-secret-cleartext"));
        assert!(content.contains("plaintext unmasked"));
    }

    #[test]
    fn render_cipher_unscramble_animation() {
        let (store, keychain, config) = unlocked();
        let theme = Theme::default();
        let app = App::new(&store, &keychain);
        app.set(&config, "SECRET_TOKEN", b"super-secret-cleartext")
            .unwrap();

        let mut tui_app = TuiApp::new(&app, &store, config, &theme).unwrap();
        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).unwrap();

        // Trigger reveal with animation starting now (0ms elapsed)
        tui_app.toggle_reveal().unwrap();
        tui_app.reveal_animation_start = Some(std::time::Instant::now());

        terminal.draw(|f| draw(f, &tui_app)).unwrap();
        let buffer = terminal.backend().buffer();
        let content = format!("{buffer:?}");

        // In mid-animation at 0ms, cipher glyphs appear and full cleartext has not fully resolved
        let has_cipher_glyph = CIPHER_GLYPHS.iter().any(|&g| content.contains(g));
        assert!(
            has_cipher_glyph,
            "mid-animation render must contain cipher glyphs"
        );
        assert!(!content.contains("super-secret-cleartext"));

        // Fast-forward animation past 150ms
        tui_app.reveal_animation_start =
            Some(std::time::Instant::now() - Duration::from_millis(200));
        terminal.draw(|f| draw(f, &tui_app)).unwrap();
        let buffer = terminal.backend().buffer();
        let content = format!("{buffer:?}");
        assert!(content.contains("super-secret-cleartext"));
    }

    #[test]
    fn render_theme_modal_and_swatches() {
        let (store, keychain, config) = unlocked();
        let theme = Theme::nord();
        let app = App::new(&store, &keychain);

        let mut tui_app = TuiApp::new(&app, &store, config, &theme).unwrap();
        tui_app.open_theme_modal();

        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).unwrap();

        terminal.draw(|f| draw(f, &tui_app)).unwrap();
        let buffer = terminal.backend().buffer();
        let content = format!("{buffer:?}");

        assert!(content.contains("Theme Switcher"));
        assert!(content.contains("nord"));
        assert!(content.contains("sordino"));
        assert!(content.contains("terminal"));
        assert!(content.contains("monochrome"));
        assert!(content.contains("tokyo-night"));
        assert!(content.contains("(current)"));
        assert!(content.contains("Apply"));
        assert!(content.contains("Cancel"));
    }

    #[test]
    fn render_help_modal_shortcuts() {
        let (store, keychain, config) = unlocked();
        let theme = Theme::default();
        let app = App::new(&store, &keychain);

        let mut tui_app = TuiApp::new(&app, &store, config, &theme).unwrap();
        tui_app.show_help = true;

        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).unwrap();

        terminal.draw(|f| draw(f, &tui_app)).unwrap();
        let buffer = terminal.backend().buffer();
        let content = format!("{buffer:?}");

        assert!(content.contains("Keyboard Shortcuts"));
        assert!(content.contains("Toggle this help screen"));
        assert!(content.contains("Copy secret to clipboard"));
        assert!(content.contains("Open live theme switcher modal"));
        assert!(content.contains("Cycle active environment"));
    }

    #[test]
    fn render_search_mode_and_filtering() {
        let (store, keychain, config) = unlocked();
        let theme = Theme::default();
        let app = App::new(&store, &keychain);
        app.set(&config, "DATABASE_URL", b"postgres://localhost")
            .unwrap();
        app.set(&config, "API_TOKEN", b"token").unwrap();

        let mut tui_app = TuiApp::new(&app, &store, config, &theme).unwrap();
        tui_app.search_mode = true;
        tui_app.search_query = "DATA".into();
        tui_app.apply_filter();

        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).unwrap();

        terminal.draw(|f| draw(f, &tui_app)).unwrap();
        let buffer = terminal.backend().buffer();
        let content = format!("{buffer:?}");

        assert!(content.contains("Search (typing... Esc to clear)"));
        assert!(content.contains("DATABASE_URL"));
        assert!(!content.contains("API_TOKEN"));
    }

    #[test]
    fn render_theme_modal_scrolls_viewport_when_overflowing() {
        let (store, keychain, config) = unlocked();
        let theme = Theme::nord();
        let app = App::new(&store, &keychain);

        let mut tui_app = TuiApp::new(&app, &store, config, &theme).unwrap();
        // Construct 15 dummy themes to trigger overflow
        tui_app.available_themes = (0..15)
            .map(|i| {
                let mut t = Theme::nord();
                t.name = format!("custom-{i:02}");
                t
            })
            .collect();
        tui_app.show_theme_modal = true;

        // Terminal with limited height (height 12 -> max visible ~ 5)
        let backend = TestBackend::new(80, 12);
        let mut terminal = Terminal::new(backend).unwrap();

        // When selection is at 0, custom-00 is visible, scroll down indicator shown
        tui_app.selected_theme_index = 0;
        terminal.draw(|f| draw(f, &tui_app)).unwrap();
        let content = format!("{:?}", terminal.backend().buffer());
        assert!(content.contains("custom-00"));
        assert!(content.contains("↓ more"));
        assert!(content.contains("Apply"));

        // When selection jumps to end, custom-14 is visible, scroll up indicator shown
        tui_app.selected_theme_index = 14;
        terminal.draw(|f| draw(f, &tui_app)).unwrap();
        let content = format!("{:?}", terminal.backend().buffer());
        assert!(content.contains("custom-14"));
        assert!(content.contains("↑ more"));
        assert!(content.contains("Apply"));
    }

    #[test]
    fn render_secret_modal_new() {
        let (store, keychain, config) = unlocked();
        let theme = Theme::nord();
        let app = App::new(&store, &keychain);

        let mut tui_app = TuiApp::new(&app, &store, config, &theme).unwrap();
        tui_app.open_new_secret_modal();
        tui_app.secret_modal_name = "STRIPE_KEY".into();
        tui_app.secret_modal_value = zeroize::Zeroizing::new("sk_test_123".into());

        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).unwrap();

        terminal.draw(|f| draw(f, &tui_app)).unwrap();
        let content = format!("{:?}", terminal.backend().buffer());

        assert!(content.contains("New Secret"));
        assert!(content.contains("STRIPE_KEY"));
        assert!(content.contains("(masked)"));
        assert!(content.contains("Save"));
        assert!(content.contains("Cancel"));
        assert!(content.contains("Generate"));
    }

    #[test]
    fn render_secret_modal_edit() {
        let (store, keychain, config) = unlocked();
        let theme = Theme::nord();
        let app = App::new(&store, &keychain);
        app.set(&config, "EXISTING_VAR", b"secret-pass").unwrap();

        let mut tui_app = TuiApp::new(&app, &store, config, &theme).unwrap();
        tui_app.open_edit_secret_modal().unwrap();
        tui_app.secret_modal_masked = false;

        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).unwrap();

        terminal.draw(|f| draw(f, &tui_app)).unwrap();
        let content = format!("{:?}", terminal.backend().buffer());

        assert!(content.contains("Edit Secret"));
        assert!(content.contains("EXISTING_VAR"));
        assert!(content.contains("(read-only)"));
        assert!(content.contains("secret-pass"));
        assert!(content.contains("(revealed)"));
    }

    #[test]
    fn render_delete_modal() {
        let (store, keychain, config) = unlocked();
        let theme = Theme::nord();
        let app = App::new(&store, &keychain);
        app.set(&config, "TO_REMOVE", b"val").unwrap();

        let mut tui_app = TuiApp::new(&app, &store, config, &theme).unwrap();
        tui_app.open_delete_modal();

        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).unwrap();

        terminal.draw(|f| draw(f, &tui_app)).unwrap();
        let content = format!("{:?}", terminal.backend().buffer());

        assert!(content.contains("Delete Secret"));
        assert!(content.contains("TO_REMOVE"));
        assert!(content.contains("This action cannot be undone."));
        assert!(content.contains("Delete Secret"));
        assert!(content.contains("Cancel"));
    }

    #[test]
    fn render_history_modal_masked_and_revealed() {
        let (store, keychain, config) = unlocked();
        let theme = Theme::nord();
        let app = App::new(&store, &keychain);
        app.set(&config, "PAYMENT_KEY", b"v1-secret").unwrap();
        app.set(&config, "PAYMENT_KEY", b"v2-secret").unwrap();

        let mut tui_app = TuiApp::new(&app, &store, config, &theme).unwrap();
        tui_app.open_history_modal().unwrap();

        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).unwrap();

        // Masked render
        terminal.draw(|f| draw(f, &tui_app)).unwrap();
        let content = format!("{:?}", terminal.backend().buffer());
        assert!(content.contains("Version History: PAYMENT_KEY"));
        assert!(content.contains("v2"));
        assert!(content.contains("(latest)"));
        assert!(content.contains("v1"));
        assert!(content.contains("••••••••••••••••••••"));
        assert!(!content.contains("v1-secret"));
        assert!(content.contains("Reveal"));
        assert!(content.contains("Restore"));

        // Revealed render
        tui_app.toggle_history_reveal();
        terminal.draw(|f| draw(f, &tui_app)).unwrap();
        let revealed_content = format!("{:?}", terminal.backend().buffer());
        assert!(revealed_content.contains("v2-secret"));
        assert!(revealed_content.contains("v1-secret"));
        assert!(revealed_content.contains("Conceal"));
    }

    #[test]
    fn render_history_modal_scrolling() {
        let (store, keychain, config) = unlocked();
        let theme = Theme::nord();
        let app = App::new(&store, &keychain);
        for i in 1..=15 {
            app.set(&config, "MANY_VERSIONS", format!("val-{i}").as_bytes())
                .unwrap();
        }

        let mut tui_app = TuiApp::new(&app, &store, config, &theme).unwrap();
        tui_app.open_history_modal().unwrap();
        assert_eq!(tui_app.history_modal_items.len(), 15);

        // Terminal with constrained height (height 14)
        let backend = TestBackend::new(90, 14);
        let mut terminal = Terminal::new(backend).unwrap();

        // Selected at top: v15 visible, scroll down indicator shown
        tui_app.history_modal_selected_index = 0;
        terminal.draw(|f| draw(f, &tui_app)).unwrap();
        let content = format!("{:?}", terminal.backend().buffer());
        assert!(content.contains("v15"));
        assert!(content.contains("more versions below"));

        // Jump to end: v1 visible, scroll up indicator shown
        tui_app.history_modal_end();
        terminal.draw(|f| draw(f, &tui_app)).unwrap();
        let end_content = format!("{:?}", terminal.backend().buffer());
        assert!(end_content.contains("v1"));
        assert!(end_content.contains("more versions above"));
    }
}
