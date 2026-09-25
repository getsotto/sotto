//! Rendering functions and widgets for the interactive Sotto dashboard.

use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, Paragraph, Wrap};
use ratatui::Frame;

use crate::tui::app::TuiApp;

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

        // Secret value inspection
        lines.push(Line::from(Span::styled("Value:", styles.muted())));

        if app.revealed {
            if let Some(cache) = &app.decrypted_cache {
                match std::str::from_utf8(cache) {
                    Ok(text) => {
                        for line in text.lines() {
                            lines
                                .push(Line::from(Span::styled(format!("  {line}"), styles.text())));
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
            Span::styled("Switch Env  ", styles.muted()),
            Span::styled("[c] ", styles.bold_accent()),
            Span::styled("Copy  ", styles.muted()),
            Span::styled("[r] ", styles.bold_accent()),
            Span::styled("Reveal  ", styles.muted()),
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
    let popup_height = 18.min(area.height.saturating_sub(2));

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
            Span::styled("  c           ", styles.bold_accent()),
            Span::styled("Copy secret to clipboard (45s)", styles.text()),
        ]),
        Line::from(vec![
            Span::styled("  r           ", styles.bold_accent()),
            Span::styled("Toggle secret reveal / mask", styles.text()),
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
}
