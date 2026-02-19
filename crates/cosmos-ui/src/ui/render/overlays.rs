use crate::ui::helpers::{centered_rect, wrap_text};
use crate::ui::theme::Theme;
use crate::ui::{StartupAction, StartupMode};
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph, Wrap},
    Frame,
};
use std::path::{Path, PathBuf};

pub(super) fn render_alert(frame: &mut Frame, title: &str, message: &str, scroll: usize) {
    let viewport = frame.area();
    let max_width = viewport.width.saturating_sub(2).max(24);
    let preferred_width = viewport.width.saturating_sub(6).min(132).max(56);
    let width = preferred_width.min(max_width);
    let text_width = width.saturating_sub(8).max(16) as usize;
    let wrapped_message = wrap_text(message, text_width);

    // base lines: blank + title + blank + message + blank
    let desired_content_lines = 4usize.saturating_add(wrapped_message.len());
    // +1 for footer line, +2 for border
    let mut desired_height = (desired_content_lines.saturating_add(3)) as u16;
    let min_height = 9u16;
    let max_height = viewport.height.saturating_sub(2).max(min_height);
    desired_height = desired_height.clamp(min_height, max_height);

    let area = Rect::new(
        viewport.x + viewport.width.saturating_sub(width) / 2,
        viewport.y + viewport.height.saturating_sub(desired_height) / 2,
        width,
        desired_height,
    );
    frame.render_widget(Clear, area);

    let mut lines: Vec<Line> = vec![
        Line::from(""),
        Line::from(vec![Span::styled(
            format!("  {}", title),
            Style::default()
                .fg(Theme::WHITE)
                .add_modifier(Modifier::BOLD),
        )]),
        Line::from(""),
    ];

    for line in wrapped_message {
        lines.push(Line::from(vec![Span::styled(
            format!("  {}", line),
            Style::default().fg(Theme::GREY_200),
        )]));
    }
    lines.push(Line::from(""));

    let block = Block::default()
        .title(" Message ")
        .title_style(Style::default().fg(Theme::GREY_100))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Theme::GREY_400))
        .style(Style::default().bg(Theme::GREY_800));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(inner);
    let content_area = chunks[0];
    let footer_area = chunks[1];

    let max_scroll = lines
        .len()
        .saturating_sub(content_area.height.max(1) as usize);
    let effective_scroll = scroll.min(max_scroll);

    let content = Paragraph::new(lines)
        .wrap(Wrap { trim: false })
        .scroll((effective_scroll as u16, 0))
        .style(Style::default().bg(Theme::GREY_800));
    frame.render_widget(content, content_area);

    let footer_line = if max_scroll > 0 {
        Line::from(vec![
            Span::styled(
                format!(
                    "  ↑/↓ scroll ({}/{})  ",
                    effective_scroll + 1,
                    max_scroll + 1
                ),
                Style::default().fg(Theme::GREY_500),
            ),
            Span::styled(
                "Enter",
                Style::default().fg(Theme::GREY_900).bg(Theme::GREY_300),
            ),
            Span::styled(" or ", Style::default().fg(Theme::GREY_500)),
            Span::styled(
                "Esc",
                Style::default().fg(Theme::GREY_900).bg(Theme::GREY_300),
            ),
            Span::styled(" to close", Style::default().fg(Theme::GREY_500)),
        ])
    } else {
        Line::from(vec![
            Span::styled("  ", Style::default()),
            Span::styled(
                "Enter",
                Style::default().fg(Theme::GREY_900).bg(Theme::GREY_300),
            ),
            Span::styled(" or ", Style::default().fg(Theme::GREY_500)),
            Span::styled(
                "Esc",
                Style::default().fg(Theme::GREY_900).bg(Theme::GREY_300),
            ),
            Span::styled(" to close", Style::default().fg(Theme::GREY_500)),
        ])
    };
    frame.render_widget(
        Paragraph::new(vec![footer_line]).style(Style::default().bg(Theme::GREY_800)),
        footer_area,
    );
}

pub(super) fn render_help(frame: &mut Frame, scroll: usize) {
    let area = centered_rect(55, 80, frame.area());
    frame.render_widget(Clear, area);

    // Helper functions that return owned data
    fn section_start(title: &str) -> Vec<Line<'static>> {
        vec![
            Line::from(""),
            Line::from(vec![
                Span::styled("    ╭─ ".to_string(), Style::default().fg(Theme::GREY_600)),
                Span::styled(
                    title.to_string(),
                    Style::default()
                        .fg(Theme::WHITE)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    " ─────────────────────────╮".to_string(),
                    Style::default().fg(Theme::GREY_600),
                ),
            ]),
        ]
    }

    fn key_row(key: &str, desc: &str) -> Line<'static> {
        Line::from(vec![
            Span::styled("    │  ".to_string(), Style::default().fg(Theme::GREY_600)),
            Span::styled(
                format!(" {} ", key),
                Style::default().fg(Theme::GREY_900).bg(Theme::GREY_300),
            ),
            Span::styled(format!("  {}", desc), Style::default().fg(Theme::GREY_200)),
        ])
    }

    fn section_end() -> Line<'static> {
        Line::from(vec![Span::styled(
            "    ╰─────────────────────────────────────╯".to_string(),
            Style::default().fg(Theme::GREY_600),
        )])
    }

    fn section_spacer() -> Line<'static> {
        Line::from(vec![Span::styled(
            "    │".to_string(),
            Style::default().fg(Theme::GREY_600),
        )])
    }

    let mut help_text: Vec<Line<'static>> = vec![Line::from("")];

    // Navigation section
    help_text.extend(section_start("Navigation"));
    help_text.push(section_spacer());
    help_text.push(key_row("Tab", "Switch suggestions/ask"));
    help_text.push(key_row("↑↓", "Move up/down"));
    help_text.push(key_row("↵", "Preview / confirm action"));
    help_text.push(key_row("Esc", "Go back / cancel"));
    help_text.push(section_spacer());
    help_text.push(section_end());

    // Actions section
    help_text.extend(section_start("Actions"));
    help_text.push(section_spacer());
    help_text.push(key_row("↵", "Open apply plan / confirm"));
    help_text.push(key_row("r", "Refresh suggestions"));
    help_text.push(key_row("k", "Open Groq setup guide"));
    help_text.push(key_row("?", "Show help"));
    help_text.push(key_row("q", "Quit"));
    help_text.push(section_spacer());
    help_text.push(section_end());

    // Privacy and reset section
    help_text.extend(section_start("Privacy"));
    help_text.push(section_spacer());
    help_text.push(key_row("R", "Reset Cosmos"));
    help_text.push(key_row("U", "Check for updates"));
    help_text.push(section_spacer());
    help_text.push(section_end());

    let max_lines = (area.height as usize).saturating_sub(4);
    let visible = &help_text[scroll..help_text.len().min(scroll + max_lines)];

    let block = Paragraph::new(visible.to_vec())
        .block(
            Block::default()
                .title(" Help ")
                .title_style(Style::default().fg(Theme::GREY_100))
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Theme::GREY_400))
                .style(Style::default().bg(Theme::GREY_900)),
        )
        .scroll((scroll as u16, 0));

    frame.render_widget(block, area);
}

pub(super) fn render_file_detail(
    frame: &mut Frame,
    path: &Path,
    file_index: &cosmos_core::index::FileIndex,
    _scroll: usize,
) {
    let area = centered_rect(70, 75, frame.area());
    frame.render_widget(Clear, area);

    // File name header
    let filename = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("unknown");

    let mut lines = vec![
        Line::from(""),
        Line::from(vec![
            Span::styled("    ", Style::default()),
            Span::styled(
                format!(" {} ", file_index.language.icon()),
                Style::default().fg(Theme::GREY_900).bg(Theme::GREY_300),
            ),
            Span::styled(
                format!("  {}", filename),
                Style::default()
                    .fg(Theme::WHITE)
                    .add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(vec![Span::styled(
            format!("       {}", path.display()),
            Style::default().fg(Theme::GREY_500),
        )]),
        Line::from(""),
    ];

    // Metrics bar
    let func_count = file_index
        .symbols
        .iter()
        .filter(|s| {
            matches!(
                s.kind,
                cosmos_core::index::SymbolKind::Function | cosmos_core::index::SymbolKind::Method
            )
        })
        .count();
    let struct_count = file_index
        .symbols
        .iter()
        .filter(|s| {
            matches!(
                s.kind,
                cosmos_core::index::SymbolKind::Struct | cosmos_core::index::SymbolKind::Class
            )
        })
        .count();

    lines.push(Line::from(vec![
        Span::styled("    ", Style::default()),
        Span::styled(
            format!(" {} ", file_index.loc),
            Style::default().fg(Theme::GREY_900).bg(Theme::GREY_500),
        ),
        Span::styled(" LOC  ", Style::default().fg(Theme::GREY_400)),
        Span::styled(
            format!(" {} ", func_count),
            Style::default().fg(Theme::GREY_900).bg(Theme::GREY_500),
        ),
        Span::styled(" funcs  ", Style::default().fg(Theme::GREY_400)),
        Span::styled(
            format!(" {} ", struct_count),
            Style::default().fg(Theme::GREY_900).bg(Theme::GREY_500),
        ),
        Span::styled(" structs", Style::default().fg(Theme::GREY_400)),
    ]));
    lines.push(Line::from(""));

    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        Span::styled("    ", Style::default()),
        Span::styled(
            " Esc ",
            Style::default().fg(Theme::GREY_900).bg(Theme::GREY_400),
        ),
        Span::styled(" close", Style::default().fg(Theme::GREY_400)),
    ]));
    lines.push(Line::from(""));

    let block = Paragraph::new(lines).wrap(Wrap { trim: false }).block(
        Block::default()
            .title(" › 𝘧𝘪𝘭𝘦 𝘥𝘦𝘵𝘢𝘪𝘭 ")
            .title_style(Style::default().fg(Theme::GREY_100))
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Theme::GREY_400))
            .style(Style::default().bg(Theme::GREY_900)),
    );

    frame.render_widget(block, area);
}

pub(super) fn render_api_key_overlay(
    frame: &mut Frame,
    input: &str,
    error: Option<&str>,
    save_armed: bool,
) {
    let area = centered_rect(72, 56, frame.area());
    frame.render_widget(Clear, area);

    let mut lines: Vec<Line> = vec![
        Line::from(""),
        Line::from(vec![Span::styled(
            "  Connect Groq to enable AI suggestions in Cosmos.",
            Style::default()
                .fg(Theme::WHITE)
                .add_modifier(Modifier::BOLD),
        )]),
        Line::from(vec![Span::styled(
            "  First time setup takes about a minute.",
            Style::default().fg(Theme::GREY_400),
        )]),
        Line::from(""),
        Line::from(vec![
            Span::styled("  1) ", Style::default().fg(Theme::GREEN)),
            Span::styled("Create/sign in", Style::default().fg(Theme::GREY_200)),
            Span::styled(" and generate a key", Style::default().fg(Theme::GREY_400)),
        ]),
        Line::from(vec![
            Span::styled("      ", Style::default()),
            Span::styled("Press ", Style::default().fg(Theme::GREY_500)),
            Span::styled(
                crate::ui::provider_keys_shortcut_display(),
                Style::default().fg(Theme::GREY_400),
            ),
            Span::styled(" for Groq keys", Style::default().fg(Theme::GREY_500)),
        ]),
        Line::from(""),
        Line::from(vec![
            Span::styled("  2) ", Style::default().fg(Theme::GREEN)),
            Span::styled(
                "Paste your API key below (it usually starts with ",
                Style::default().fg(Theme::GREY_400),
            ),
            Span::styled("gsk_", Style::default().fg(Theme::GREY_200)),
            Span::styled(")", Style::default().fg(Theme::GREY_400)),
        ]),
        Line::from(""),
    ];

    let normalized_key: String = input
        .trim()
        .trim_matches(|c| c == '"' || c == '\'' || c == '`')
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect();
    let key_len = normalized_key.chars().count();
    let visible_prefix = if normalized_key.starts_with("gsk_") {
        Some("gsk_")
    } else if normalized_key.starts_with("sk-") {
        Some("sk-")
    } else {
        None
    };
    let mask = if key_len == 0 {
        "█".to_string()
    } else {
        let hidden_count = if let Some(prefix) = visible_prefix {
            key_len.saturating_sub(prefix.chars().count())
        } else {
            key_len
        };
        let shown = hidden_count.min(48);
        let hidden = "•".repeat(shown);
        if let Some(prefix) = visible_prefix {
            format!("{}{}█", prefix, hidden)
        } else {
            format!("{}█", hidden)
        }
    };
    lines.push(Line::from(vec![
        Span::styled("  ", Style::default()),
        Span::styled(mask, Style::default().fg(Theme::WHITE)),
    ]));
    lines.push(Line::from(""));

    if save_armed {
        lines.push(Line::from(vec![
            Span::styled("  Press ", Style::default().fg(Theme::GREY_300)),
            Span::styled(
                " Enter ",
                Style::default().fg(Theme::GREY_900).bg(Theme::GREEN),
            ),
            Span::styled(
                " again to save this key anyway.",
                Style::default().fg(Theme::YELLOW),
            ),
        ]));
    } else {
        lines.push(Line::from(vec![
            Span::styled("  Press ", Style::default().fg(Theme::GREY_300)),
            Span::styled(
                " Enter ",
                Style::default().fg(Theme::GREY_900).bg(Theme::GREEN),
            ),
            Span::styled(
                " to save key and try refreshing suggestions.",
                Style::default().fg(Theme::GREY_300),
            ),
        ]));
    }

    if key_len > 0 {
        let prefix_status = if visible_prefix.is_some() {
            "prefix detected"
        } else {
            "prefix gsk_/sk- not detected"
        };
        lines.push(Line::from(vec![
            Span::styled("  Key check: ", Style::default().fg(Theme::GREY_500)),
            Span::styled(
                format!("{}, {} chars entered.", prefix_status, key_len),
                Style::default().fg(Theme::GREY_300),
            ),
        ]));
        if visible_prefix.is_none() {
            lines.push(Line::from(vec![
                Span::styled("  ! ", Style::default().fg(Theme::YELLOW)),
                Span::styled(
                    "Groq keys usually start with gsk_ (or sk-)",
                    Style::default().fg(Theme::GREY_300),
                ),
            ]));
        }
    }

    if let Some(message) = error {
        lines.push(Line::from(""));
        for line in wrap_text(message, area.width.saturating_sub(10) as usize) {
            lines.push(Line::from(vec![
                Span::styled("  ! ", Style::default().fg(Theme::YELLOW)),
                Span::styled(line, Style::default().fg(Theme::GREY_200)),
            ]));
        }
    }

    lines.push(Line::from(""));
    lines.push(Line::from(vec![Span::styled(
        "  Cosmos stores your key in your system credential store.",
        Style::default().fg(Theme::GREY_500),
    )]));
    lines.push(Line::from(vec![Span::styled(
        "  Data use: selected snippets + file paths may be sent to Groq.",
        Style::default().fg(Theme::GREY_500),
    )]));
    lines.push(Line::from(""));
    lines.push(Line::from(vec![Span::styled(
        "  ───────────────────────────────────────────────────────────────",
        Style::default().fg(Theme::GREY_600),
    )]));
    let enter_label = if save_armed {
        " save anyway "
    } else {
        " save + try refresh "
    };
    lines.push(Line::from(vec![
        Span::styled("   ", Style::default()),
        Span::styled(
            " Enter ",
            Style::default().fg(Theme::GREY_900).bg(Theme::GREEN),
        ),
        Span::styled(enter_label, Style::default().fg(Theme::GREY_300)),
        Span::styled(
            crate::ui::provider_keys_shortcut_chip(),
            Style::default().fg(Theme::GREY_900).bg(Theme::GREY_400),
        ),
        Span::styled(" keys  ", Style::default().fg(Theme::GREY_400)),
        Span::styled(
            " Backspace ",
            Style::default().fg(Theme::GREY_900).bg(Theme::GREY_400),
        ),
        Span::styled(" delete  ", Style::default().fg(Theme::GREY_400)),
        Span::styled(
            " Esc ",
            Style::default().fg(Theme::GREY_900).bg(Theme::GREY_400),
        ),
        Span::styled(" cancel", Style::default().fg(Theme::GREY_400)),
    ]));

    let block = Paragraph::new(lines).block(
        Block::default()
            .title(" API Key Setup ")
            .title_style(Style::default().fg(Theme::GREY_100))
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Theme::GREY_400))
            .style(Style::default().bg(Theme::GREY_900)),
    );
    frame.render_widget(block, area);
}

pub(super) fn render_apply_plan(
    frame: &mut Frame,
    preview: &cosmos_engine::llm::FixPreview,
    affected_files: &[PathBuf],
    confirm_apply: bool,
    show_technical_details: bool,
    show_data_notice: bool,
    scroll: usize,
) {
    let area = centered_rect(72, 78, frame.area());
    frame.render_widget(Clear, area);

    let block = Block::default()
        .title(" Apply Plan ")
        .title_style(Style::default().fg(Theme::GREY_100))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Theme::GREY_400))
        .style(Style::default().bg(Theme::GREY_900));
    frame.render_widget(block, area);

    let inner = area.inner(ratatui::layout::Margin {
        vertical: 1,
        horizontal: 1,
    });
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(10),   // Scrollable body
            Constraint::Length(4), // Fixed controls
        ])
        .split(inner);

    let body_area = chunks[0];
    let footer_area = chunks[1];

    let mut lines: Vec<Line> = Vec::new();
    let text_width = body_area.width.saturating_sub(8) as usize;

    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        Span::styled("  ", Style::default()),
        Span::styled(
            "Preview scope before apply",
            Style::default()
                .fg(Theme::WHITE)
                .add_modifier(Modifier::BOLD),
        ),
    ]));
    lines.push(Line::from(vec![
        Span::styled("  ", Style::default()),
        Span::styled(
            "No files are changed until you confirm.",
            Style::default().fg(Theme::GREEN),
        ),
    ]));

    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        Span::styled("  ", Style::default()),
        Span::styled(
            "What goes wrong",
            Style::default()
                .fg(Theme::WHITE)
                .add_modifier(Modifier::BOLD),
        ),
    ]));
    for line in wrap_text(&preview.problem_summary, text_width) {
        lines.push(Line::from(vec![
            Span::styled("    ", Style::default()),
            Span::styled(line, Style::default().fg(Theme::GREY_300)),
        ]));
    }

    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        Span::styled("  ", Style::default()),
        Span::styled(
            "Why it matters",
            Style::default()
                .fg(Theme::WHITE)
                .add_modifier(Modifier::BOLD),
        ),
    ]));
    for line in wrap_text(&preview.description, text_width) {
        lines.push(Line::from(vec![
            Span::styled("    ", Style::default()),
            Span::styled(line, Style::default().fg(Theme::GREY_300)),
        ]));
    }

    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        Span::styled("  ", Style::default()),
        Span::styled(
            "What changes after apply",
            Style::default()
                .fg(Theme::WHITE)
                .add_modifier(Modifier::BOLD),
        ),
    ]));
    for line in wrap_text(&preview.outcome, text_width) {
        lines.push(Line::from(vec![
            Span::styled("    ", Style::default()),
            Span::styled(line, Style::default().fg(Theme::GREY_300)),
        ]));
    }

    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        Span::styled("  ", Style::default()),
        Span::styled(
            "Scope",
            Style::default()
                .fg(Theme::WHITE)
                .add_modifier(Modifier::BOLD),
        ),
    ]));
    lines.push(Line::from(vec![
        Span::styled("    ", Style::default()),
        Span::styled(
            format!(
                "{} file{} affected, estimated {} scope.",
                affected_files.len(),
                if affected_files.len() == 1 { "" } else { "s" },
                preview.scope.label()
            ),
            Style::default().fg(Theme::GREY_300),
        ),
    ]));

    for file in affected_files {
        lines.push(Line::from(vec![
            Span::styled("      - ", Style::default().fg(Theme::GREY_600)),
            Span::styled(
                file.display().to_string(),
                Style::default().fg(Theme::GREY_400),
            ),
        ]));
    }

    if show_data_notice {
        lines.push(Line::from(""));
        lines.push(Line::from(vec![
            Span::styled("  ", Style::default()),
            Span::styled(
                "Data use notice",
                Style::default()
                    .fg(Theme::WHITE)
                    .add_modifier(Modifier::BOLD),
            ),
        ]));
        for line in wrap_text(
            "Cosmos may send selected code snippets and file paths to Groq to generate and validate suggestions. Local cache stays in .cosmos and can be cleared with Reset.",
            text_width,
        ) {
            lines.push(Line::from(vec![
                Span::styled("    ", Style::default()),
                Span::styled(line, Style::default().fg(Theme::GREY_400)),
            ]));
        }
    }

    if show_technical_details {
        lines.push(Line::from(""));
        lines.push(Line::from(vec![
            Span::styled("  ", Style::default()),
            Span::styled(
                "Technical details",
                Style::default()
                    .fg(Theme::WHITE)
                    .add_modifier(Modifier::BOLD),
            ),
        ]));
        for line in wrap_text(
            &format!("Verification: {}", preview.verification_note),
            text_width,
        ) {
            lines.push(Line::from(vec![
                Span::styled("    ", Style::default()),
                Span::styled(line, Style::default().fg(Theme::GREY_500)),
            ]));
        }
        if !preview.affected_areas.is_empty() {
            for line in wrap_text(
                &format!("Affected areas: {}", preview.affected_areas.join(", ")),
                text_width,
            ) {
                lines.push(Line::from(vec![
                    Span::styled("    ", Style::default()),
                    Span::styled(line, Style::default().fg(Theme::GREY_500)),
                ]));
            }
        }
        if let Some(evidence_line) = preview.evidence_line {
            lines.push(Line::from(vec![
                Span::styled("    ", Style::default()),
                Span::styled(
                    format!("Evidence line: {}", evidence_line),
                    Style::default().fg(Theme::GREY_500),
                ),
            ]));
        }
        if let Some(snippet) = &preview.evidence_snippet {
            lines.push(Line::from(vec![
                Span::styled("    ", Style::default()),
                Span::styled("Evidence snippet:", Style::default().fg(Theme::GREY_500)),
            ]));
            for line in snippet.lines().take(10) {
                lines.push(Line::from(vec![
                    Span::styled("      ", Style::default()),
                    Span::styled(line.to_string(), Style::default().fg(Theme::GREY_600)),
                ]));
            }
        }
    }

    let body = Paragraph::new(lines)
        .wrap(Wrap { trim: false })
        .scroll((scroll as u16, 0));
    frame.render_widget(body, body_area);

    let apply_text = if confirm_apply {
        "applying..."
    } else {
        "confirm apply"
    };

    let footer = Paragraph::new(vec![
        Line::from(vec![Span::styled(
            "  ─────────────────────────────────────────────────────",
            Style::default().fg(Theme::GREY_600),
        )]),
        Line::from(vec![
            Span::styled("   ", Style::default()),
            Span::styled(
                " Enter/y ",
                Style::default().fg(Theme::GREY_900).bg(Theme::GREEN),
            ),
            Span::styled(
                format!(" {}  ", apply_text),
                Style::default().fg(Theme::GREY_300),
            ),
            Span::styled(
                " Esc/q ",
                Style::default().fg(Theme::GREY_900).bg(Theme::GREY_400),
            ),
            Span::styled(" cancel  ", Style::default().fg(Theme::GREY_400)),
            Span::styled(
                " t ",
                Style::default().fg(Theme::GREY_900).bg(Theme::GREY_500),
            ),
            Span::styled(" details  ↑↓ scroll", Style::default().fg(Theme::GREY_500)),
        ]),
    ]);
    frame.render_widget(footer, footer_area);
}

pub(super) fn render_reset_overlay(
    frame: &mut Frame,
    options: &[(cosmos_adapters::cache::ResetOption, bool)],
    selected: usize,
    error: Option<&str>,
) {
    let area = centered_rect(55, 50, frame.area());
    frame.render_widget(Clear, area);

    let mut lines: Vec<Line> = Vec::new();

    // Header
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "  Select what to reset and regenerate:",
        Style::default().fg(Theme::GREY_300),
    )));
    lines.push(Line::from(""));

    // Options list
    for (i, (option, is_selected)) in options.iter().enumerate() {
        let is_focused = i == selected;

        // Checkbox
        let checkbox = if *is_selected { "[x]" } else { "[ ]" };
        let checkbox_color = if *is_selected {
            Theme::GREEN
        } else {
            Theme::GREY_500
        };

        // Selection indicator
        let indicator = if is_focused { "▸ " } else { "  " };

        // Format: "▸ [x] Label                (description)"
        let label = option.label();
        let desc = option.description();

        // Calculate padding for alignment
        let label_width = 22;
        let padded_label = format!("{:<width$}", label, width = label_width);

        let line_style = if is_focused {
            Style::default().bg(Theme::GREY_700)
        } else {
            Style::default()
        };

        lines.push(
            Line::from(vec![
                Span::styled(
                    format!("  {}", indicator),
                    Style::default().fg(Theme::ACCENT),
                ),
                Span::styled(
                    format!("{} ", checkbox),
                    Style::default().fg(checkbox_color),
                ),
                Span::styled(padded_label, Style::default().fg(Theme::GREY_100)),
                Span::styled(format!("({})", desc), Style::default().fg(Theme::GREY_500)),
            ])
            .style(line_style),
        );
    }

    if let Some(message) = error {
        lines.push(Line::from(""));
        lines.push(Line::from(vec![
            Span::styled("  ! ", Style::default().fg(Theme::YELLOW)),
            Span::styled(message.to_string(), Style::default().fg(Theme::GREY_200)),
        ]));
    }

    // Separator and help
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "  ─────────────────────────────────────────────────",
        Style::default().fg(Theme::GREY_600),
    )));
    lines.push(Line::from(vec![
        Span::styled("   ", Style::default()),
        Span::styled(
            " Space ",
            Style::default().fg(Theme::GREY_900).bg(Theme::GREY_400),
        ),
        Span::styled(" toggle  ", Style::default().fg(Theme::GREY_400)),
        Span::styled(
            " ↵ ",
            Style::default().fg(Theme::GREY_900).bg(Theme::GREY_400),
        ),
        Span::styled(" reset  ", Style::default().fg(Theme::GREY_400)),
        Span::styled(
            " Esc ",
            Style::default().fg(Theme::GREY_900).bg(Theme::GREY_400),
        ),
        Span::styled(" cancel", Style::default().fg(Theme::GREY_400)),
    ]));
    lines.push(Line::from(""));

    let block = Block::default()
        .title(" Reset Cosmos ")
        .title_style(Style::default().fg(Theme::GREY_100))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Theme::ACCENT))
        .style(Style::default().bg(Theme::GREY_800));

    let paragraph = Paragraph::new(lines)
        .block(block)
        .wrap(Wrap { trim: false });

    frame.render_widget(paragraph, area);
}

pub(super) fn render_startup_check(
    frame: &mut Frame,
    changed_count: usize,
    current_branch: &str,
    main_branch: &str,
    mode: StartupMode,
    selected_action: StartupAction,
) {
    let viewport = frame.area();
    let is_branch_only = changed_count == 0 && current_branch != main_branch;
    let desired_width = if mode == StartupMode::ConfirmDiscard {
        74
    } else {
        98
    };
    let desired_height = if mode == StartupMode::ConfirmDiscard {
        14
    } else if is_branch_only {
        18
    } else {
        20
    };
    let max_width = viewport.width.saturating_sub(2);
    let max_height = viewport.height.saturating_sub(2);
    let width = desired_width.min(max_width).max(40).min(viewport.width);
    let height = desired_height.min(max_height).max(10).min(viewport.height);
    let area = Rect::new(
        viewport.x + viewport.width.saturating_sub(width) / 2,
        viewport.y + viewport.height.saturating_sub(height) / 2,
        width,
        height,
    );
    frame.render_widget(Clear, area);

    let title = if mode == StartupMode::ConfirmDiscard {
        " Confirm "
    } else {
        " Startup Check "
    };

    // Outer block with border
    let outer_block = Block::default()
        .title(title)
        .title_style(Style::default().fg(Theme::GREY_100))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Theme::ACCENT))
        .style(Style::default().bg(Theme::GREY_800));

    let inner_area = outer_block.inner(area);
    frame.render_widget(outer_block, area);

    // Split inner area: scrollable body + fixed footer
    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(inner_area);

    let body_area = layout[0];
    let footer_area = layout[1];

    // Build body content
    let mut lines: Vec<Line> = Vec::new();
    let compact = mode == StartupMode::Choose && body_area.height <= 10;
    let show_selected_desc = !compact && body_area.height >= 12;

    if mode == StartupMode::ConfirmDiscard {
        // Confirmation dialog for discard
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "  Discard uncommitted changes?",
            Style::default()
                .fg(Theme::WHITE)
                .add_modifier(Modifier::BOLD),
        )));
        lines.push(Line::from(Span::styled(
            "  This permanently removes local changes on this branch.",
            Style::default().fg(Theme::GREY_300),
        )));
        lines.push(Line::from(""));
        lines.push(Line::from(vec![
            Span::styled("  ", Style::default()),
            Span::styled(
                " irreversible ",
                Style::default().fg(Theme::GREY_900).bg(Theme::RED),
            ),
        ]));
        lines.push(Line::from(""));
        lines.push(Line::from(vec![
            Span::styled("  ", Style::default()),
            Span::styled(" y ", Style::default().fg(Theme::GREY_900).bg(Theme::RED)),
            Span::styled(" discard now  ", Style::default().fg(Theme::GREY_300)),
            Span::styled(
                " n ",
                Style::default().fg(Theme::GREY_900).bg(Theme::GREY_400),
            ),
            Span::styled(" go back", Style::default().fg(Theme::GREY_400)),
        ]));
    } else {
        // Main startup check dialog (guided selection)
        lines.push(Line::from(""));
        let headline = if is_branch_only {
            "  You're on a non-main branch"
        } else {
            "  Local changes detected"
        };
        lines.push(Line::from(Span::styled(
            headline,
            Style::default()
                .fg(Theme::WHITE)
                .add_modifier(Modifier::BOLD),
        )));

        if compact {
            lines.push(Line::from(Span::styled(
                if current_branch != main_branch {
                    format!(
                        "  files: {}   branch: {} -> {}",
                        changed_count, current_branch, main_branch
                    )
                } else {
                    format!("  files changed: {}", changed_count)
                },
                Style::default().fg(Theme::GREY_300),
            )));
        } else {
            lines.push(Line::from(Span::styled(
                "  Choose how Cosmos should start this session.",
                Style::default().fg(Theme::GREY_300),
            )));
            lines.push(Line::from(""));
            lines.push(Line::from(vec![
                Span::styled("  ", Style::default()),
                Span::styled(
                    " Status ",
                    Style::default().fg(Theme::GREY_900).bg(Theme::GREY_400),
                ),
            ]));
            lines.push(Line::from(Span::styled(
                format!("  Changed files: {}", changed_count),
                Style::default().fg(Theme::GREY_300),
            )));
            if current_branch != main_branch {
                lines.push(Line::from(Span::styled(
                    format!("  Branch: {} (main: {})", current_branch, main_branch),
                    Style::default().fg(Theme::GREY_400),
                )));
            }
        }
        lines.push(Line::from(""));

        let mut selected_desc: Option<String> = None;
        let mut push_action = |action: StartupAction,
                               key: &str,
                               key_style: Style,
                               label: &str,
                               tag: Option<(&str, Style)>,
                               desc: &str| {
            let selected = selected_action == action;
            let row_bg = if selected {
                Theme::GREY_700
            } else {
                Theme::GREY_800
            };
            let indicator = if selected { " ▸ " } else { "   " };

            let mut row = vec![
                Span::styled(
                    indicator,
                    Style::default()
                        .fg(if selected {
                            Theme::ACCENT
                        } else {
                            Theme::GREY_600
                        })
                        .bg(row_bg),
                ),
                Span::styled(format!(" {} ", key), key_style),
                Span::styled(
                    format!(" {}", label),
                    Style::default()
                        .fg(if selected {
                            Theme::WHITE
                        } else {
                            Theme::GREY_100
                        })
                        .bg(row_bg)
                        .add_modifier(if selected {
                            Modifier::BOLD
                        } else {
                            Modifier::empty()
                        }),
                ),
            ];
            if let Some((tag_text, tag_style)) = tag {
                row.push(Span::styled(" ", Style::default().bg(row_bg)));
                row.push(Span::styled(format!(" {} ", tag_text), tag_style));
            }
            lines.push(Line::from(row));
            if selected {
                selected_desc = Some(desc.to_string());
            }
        };

        if is_branch_only {
            push_action(
                StartupAction::SwitchToMain,
                "m",
                Style::default().fg(Theme::GREY_900).bg(Theme::GREEN),
                "switch to main branch",
                Some((
                    "recommended",
                    Style::default().fg(Theme::GREY_900).bg(Theme::YELLOW),
                )),
                "Start from the repository's default branch.",
            );
            push_action(
                StartupAction::ContinueAsIs,
                "c",
                Style::default().fg(Theme::GREY_900).bg(Theme::GREY_500),
                "continue on current branch",
                None,
                "Keep this branch as the base for this session.",
            );
        } else {
            push_action(
                StartupAction::SaveStartFresh,
                "s",
                Style::default().fg(Theme::GREY_900).bg(Theme::GREEN),
                "save and start fresh",
                Some((
                    "recommended",
                    Style::default().fg(Theme::GREY_900).bg(Theme::YELLOW),
                )),
                "Stashes your local changes safely before continuing.",
            );
            push_action(
                StartupAction::DiscardStartFresh,
                "d",
                Style::default().fg(Theme::GREY_900).bg(Theme::GREY_500),
                "discard and start fresh",
                Some(("permanent", Style::default().fg(Theme::RED))),
                "Permanently removes uncommitted local changes.",
            );
            push_action(
                StartupAction::ContinueAsIs,
                "c",
                Style::default().fg(Theme::GREY_900).bg(Theme::GREY_500),
                "continue as-is",
                None,
                "Use your current branch and local state as the base.",
            );
        }

        lines.push(Line::from(""));
        if show_selected_desc {
            if let Some(desc) = selected_desc {
                lines.push(Line::from(vec![
                    Span::styled("  ", Style::default()),
                    Span::styled(
                        "Selected: ",
                        Style::default()
                            .fg(Theme::GREY_300)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(desc, Style::default().fg(Theme::GREY_500)),
                ]));
            }
        } else {
            lines.push(Line::from(vec![
                Span::styled("  ", Style::default()),
                Span::styled(
                    "Tip: use ↑↓ to see every option",
                    Style::default().fg(Theme::GREY_500),
                ),
            ]));
        }
    }

    // Render body
    let body = Paragraph::new(lines).wrap(Wrap { trim: false });
    frame.render_widget(body, body_area);

    // Render fixed footer controls
    let footer_lines = if mode == StartupMode::ConfirmDiscard {
        vec![Line::from(vec![
            Span::styled("  ", Style::default()),
            Span::styled(" y ", Style::default().fg(Theme::GREY_900).bg(Theme::RED)),
            Span::styled(" discard  ", Style::default().fg(Theme::GREY_300)),
            Span::styled(
                " n ",
                Style::default().fg(Theme::GREY_900).bg(Theme::GREY_400),
            ),
            Span::styled(" cancel  ", Style::default().fg(Theme::GREY_400)),
            Span::styled(
                " Esc ",
                Style::default().fg(Theme::GREY_900).bg(Theme::GREY_400),
            ),
            Span::styled(" back", Style::default().fg(Theme::GREY_400)),
        ])]
    } else if is_branch_only {
        vec![Line::from(vec![
            Span::styled("  ", Style::default()),
            Span::styled(
                " ↑↓ ",
                Style::default().fg(Theme::GREY_900).bg(Theme::GREY_400),
            ),
            Span::styled(" move  ", Style::default().fg(Theme::GREY_400)),
            Span::styled(
                " Enter ",
                Style::default().fg(Theme::GREY_900).bg(Theme::GREEN),
            ),
            Span::styled(" choose  ", Style::default().fg(Theme::GREY_300)),
            Span::styled(
                " m/c ",
                Style::default().fg(Theme::GREY_900).bg(Theme::GREY_400),
            ),
            Span::styled(" quick  ", Style::default().fg(Theme::GREY_400)),
            Span::styled(
                " Esc ",
                Style::default().fg(Theme::GREY_900).bg(Theme::GREY_400),
            ),
            Span::styled(" continue", Style::default().fg(Theme::GREY_400)),
        ])]
    } else {
        vec![Line::from(vec![
            Span::styled("  ", Style::default()),
            Span::styled(
                " ↑↓ ",
                Style::default().fg(Theme::GREY_900).bg(Theme::GREY_400),
            ),
            Span::styled(" move  ", Style::default().fg(Theme::GREY_400)),
            Span::styled(
                " Enter ",
                Style::default().fg(Theme::GREY_900).bg(Theme::GREEN),
            ),
            Span::styled(" choose  ", Style::default().fg(Theme::GREY_300)),
            Span::styled(
                " s/d/c ",
                Style::default().fg(Theme::GREY_900).bg(Theme::GREY_400),
            ),
            Span::styled(" quick  ", Style::default().fg(Theme::GREY_400)),
            Span::styled(
                " Esc ",
                Style::default().fg(Theme::GREY_900).bg(Theme::GREY_400),
            ),
            Span::styled(" continue", Style::default().fg(Theme::GREY_400)),
        ])]
    };
    let footer = Paragraph::new(footer_lines);
    frame.render_widget(footer, footer_area);
}

pub(super) fn render_welcome(frame: &mut Frame) {
    let area = centered_rect(60, 70, frame.area());
    frame.render_widget(Clear, area);

    let lines: Vec<Line> = vec![
        // Header
        Line::from(""),
        Line::from(vec![
            Span::styled(
                "  Welcome to ",
                Style::default()
                    .fg(Theme::WHITE)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                "Cosmos",
                Style::default()
                    .fg(Theme::GREEN)
                    .add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(""),
        Line::from(vec![Span::styled(
            "  Data use: Cosmos sends selected code snippets + file paths to Groq for AI generation and validation.",
            Style::default().fg(Theme::GREY_400),
        )]),
        Line::from(vec![Span::styled(
            "  Local runtime/cache data stays in .cosmos; use Reset to clear it any time.",
            Style::default().fg(Theme::GREY_500),
        )]),
        Line::from(""),
        // Intro
        Line::from(vec![Span::styled(
            "  Cosmos analyzes your codebase and suggests improvements.",
            Style::default().fg(Theme::GREY_300),
        )]),
        Line::from(""),
        // Workflow explanation
        Line::from(vec![Span::styled(
            "  How it works:",
            Style::default()
                .fg(Theme::WHITE)
                .add_modifier(Modifier::BOLD),
        )]),
        Line::from(""),
        Line::from(vec![
            Span::styled("    1. ", Style::default().fg(Theme::GREEN)),
            Span::styled("Select", Style::default().fg(Theme::WHITE)),
            Span::styled(
                " a suggestion with arrow keys",
                Style::default().fg(Theme::GREY_400),
            ),
        ]),
        Line::from(vec![
            Span::styled("    2. ", Style::default().fg(Theme::GREEN)),
            Span::styled("Press Enter", Style::default().fg(Theme::WHITE)),
            Span::styled(" to open scope preview", Style::default().fg(Theme::GREY_400)),
        ]),
        Line::from(vec![
            Span::styled("    3. ", Style::default().fg(Theme::GREEN)),
            Span::styled("Confirm", Style::default().fg(Theme::WHITE)),
            Span::styled(" from preview, then review/fix", Style::default().fg(Theme::GREY_400)),
        ]),
        Line::from(vec![
            Span::styled("    4. ", Style::default().fg(Theme::GREEN)),
            Span::styled("Ship", Style::default().fg(Theme::WHITE)),
            Span::styled(
                " creates a PR for you",
                Style::default().fg(Theme::GREY_400),
            ),
        ]),
        Line::from(""),
        // Key shortcuts
        Line::from(vec![Span::styled(
            "  Quick keys:",
            Style::default()
                .fg(Theme::WHITE)
                .add_modifier(Modifier::BOLD),
        )]),
        Line::from(""),
        Line::from(vec![
            Span::styled("    ", Style::default()),
            Span::styled(
                " Tab ",
                Style::default().fg(Theme::GREY_900).bg(Theme::GREY_300),
            ),
            Span::styled(
                " Switch between suggestions and ask",
                Style::default().fg(Theme::GREY_400),
            ),
        ]),
        Line::from(vec![
            Span::styled("    ", Style::default()),
            Span::styled(
                "  ?  ",
                Style::default().fg(Theme::GREY_900).bg(Theme::GREY_300),
            ),
            Span::styled(
                " Show all keyboard shortcuts",
                Style::default().fg(Theme::GREY_400),
            ),
        ]),
        Line::from(""),
        Line::from(""),
        // Dismiss prompt
        Line::from(vec![
            Span::styled("  Press ", Style::default().fg(Theme::GREY_500)),
            Span::styled(
                " Enter ",
                Style::default().fg(Theme::GREY_900).bg(Theme::GREEN),
            ),
            Span::styled(" or ", Style::default().fg(Theme::GREY_500)),
            Span::styled(
                " Esc ",
                Style::default().fg(Theme::GREY_900).bg(Theme::GREY_300),
            ),
            Span::styled(" to get started", Style::default().fg(Theme::GREY_500)),
        ]),
    ];

    let block = Paragraph::new(lines)
        .block(
            Block::default()
                .title(" Getting Started ")
                .title_style(Style::default().fg(Theme::GREY_100))
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Theme::GREY_400))
                .style(Style::default().bg(Theme::GREY_900)),
        )
        .wrap(Wrap { trim: false });

    frame.render_widget(block, area);
}

pub(super) fn render_update_overlay(
    frame: &mut Frame,
    current_version: &str,
    target_version: &str,
    progress: Option<u8>,
    error: Option<&str>,
) {
    let area = centered_rect(50, 40, frame.area());
    frame.render_widget(Clear, area);

    let mut lines: Vec<Line> = Vec::new();

    // Header
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "  A new version of Cosmos is available",
        Style::default()
            .fg(Theme::WHITE)
            .add_modifier(Modifier::BOLD),
    )));
    lines.push(Line::from(""));

    // Version info
    lines.push(Line::from(vec![
        Span::styled(
            "    Current version:  ",
            Style::default().fg(Theme::GREY_400),
        ),
        Span::styled(
            format!("v{}", current_version),
            Style::default().fg(Theme::GREY_300),
        ),
    ]));
    lines.push(Line::from(vec![
        Span::styled(
            "    New version:      ",
            Style::default().fg(Theme::GREY_400),
        ),
        Span::styled(
            format!("v{}", target_version),
            Style::default()
                .fg(Theme::GREEN)
                .add_modifier(Modifier::BOLD),
        ),
    ]));
    lines.push(Line::from(""));

    // Progress or error display
    if let Some(err) = error {
        // Split error message if it contains the manual install hint
        let (error_part, hint_part) = if let Some(idx) = err.find(". Try manually:") {
            (&err[..idx], Some(&err[idx + 2..]))
        } else {
            (err, None)
        };

        lines.push(Line::from(Span::styled(
            format!("  {}", error_part),
            Style::default().fg(Theme::RED),
        )));
        lines.push(Line::from(""));

        if let Some(hint) = hint_part {
            lines.push(Line::from(Span::styled(
                format!("  {}", hint),
                Style::default().fg(Theme::GREY_300),
            )));
            lines.push(Line::from(""));
        }

        lines.push(Line::from(Span::styled(
            "  Press Enter to retry or Esc to cancel.",
            Style::default().fg(Theme::GREY_400),
        )));
    } else if let Some(pct) = progress {
        // Show progress bar
        let bar_width = 30;
        let filled = (pct as usize * bar_width) / 100;
        let empty = bar_width - filled;

        let progress_bar = format!("  [{}{}] {}%", "█".repeat(filled), "░".repeat(empty), pct);

        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            progress_bar,
            Style::default().fg(Theme::ACCENT),
        )));
        lines.push(Line::from(""));

        if pct < 100 {
            lines.push(Line::from(Span::styled(
                "  Installing update (this may take a minute)...",
                Style::default().fg(Theme::GREY_400),
            )));
        } else {
            lines.push(Line::from(Span::styled(
                "  Restarting...",
                Style::default().fg(Theme::GREEN),
            )));
        }
    } else {
        // Not started - show confirmation prompt
        lines.push(Line::from(Span::styled(
            "  Would you like to download and install it?",
            Style::default().fg(Theme::GREY_300),
        )));
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "  ─────────────────────────────────────────",
            Style::default().fg(Theme::GREY_600),
        )));
        lines.push(Line::from(""));
        lines.push(Line::from(vec![
            Span::styled("   ", Style::default()),
            Span::styled(" y ", Style::default().fg(Theme::GREY_900).bg(Theme::GREEN)),
            Span::styled(" Yes, update  ", Style::default().fg(Theme::GREY_300)),
            Span::styled(
                " n ",
                Style::default().fg(Theme::GREY_900).bg(Theme::GREY_500),
            ),
            Span::styled(" No, later", Style::default().fg(Theme::GREY_400)),
        ]));
    }

    lines.push(Line::from(""));

    let block = Block::default()
        .title(" Update Available ")
        .title_style(Style::default().fg(Theme::GREY_100))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Theme::GREEN))
        .style(Style::default().bg(Theme::GREY_800));

    let paragraph = Paragraph::new(lines)
        .block(block)
        .wrap(Wrap { trim: false });

    frame.render_widget(paragraph, area);
}
