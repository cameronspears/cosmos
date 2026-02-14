use crate::ui::helpers::{centered_rect, wrap_text};
use crate::ui::theme::Theme;
use ratatui::{
    layout::{Constraint, Direction, Layout},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph, Wrap},
    Frame,
};
use std::path::{Path, PathBuf};

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
    help_text.push(key_row("↑↓", "Move up/down"));
    help_text.push(key_row("Esc", "Go back / cancel"));
    help_text.push(section_spacer());
    help_text.push(section_end());

    // Actions section
    help_text.extend(section_start("Actions"));
    help_text.push(section_spacer());
    help_text.push(key_row("↵", "Open shell action"));
    help_text.push(key_row("r", "Suggestions disabled"));
    help_text.push(key_row("x", "Suggestions disabled"));
    help_text.push(key_row("d", "Diagnostics disabled"));
    help_text.push(key_row("i", "Ask disabled"));
    help_text.push(key_row("k", "Open setup guide"));
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
    llm_summary: Option<&String>,
    _scroll: usize,
) {
    let area = centered_rect(70, 75, frame.area());
    frame.render_widget(Clear, area);

    let inner_width = area.width.saturating_sub(12) as usize;

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

    // Summary card
    lines.push(Line::from(vec![
        Span::styled("    ╭─ ", Style::default().fg(Theme::GREY_600)),
        Span::styled("Summary", Style::default().fg(Theme::GREY_300)),
        Span::styled(
            " ─".to_string() + &"─".repeat(inner_width.saturating_sub(15)) + "╮",
            Style::default().fg(Theme::GREY_600),
        ),
    ]));
    lines.push(Line::from(vec![Span::styled(
        "    │",
        Style::default().fg(Theme::GREY_600),
    )]));

    if let Some(summary) = llm_summary {
        let wrapped = wrap_text(summary, inner_width.saturating_sub(6));
        for line in &wrapped {
            lines.push(Line::from(vec![
                Span::styled("    │  ", Style::default().fg(Theme::GREY_600)),
                Span::styled(line.to_string(), Style::default().fg(Theme::GREY_50)),
            ]));
        }
    } else {
        let wrapped = wrap_text(&file_index.summary.purpose, inner_width.saturating_sub(6));
        for line in &wrapped {
            lines.push(Line::from(vec![
                Span::styled("    │  ", Style::default().fg(Theme::GREY_600)),
                Span::styled(line.to_string(), Style::default().fg(Theme::GREY_100)),
            ]));
        }
    }

    lines.push(Line::from(vec![Span::styled(
        "    │",
        Style::default().fg(Theme::GREY_600),
    )]));
    lines.push(Line::from(vec![Span::styled(
        "    ╰".to_string() + &"─".repeat(inner_width.saturating_sub(4)) + "╯",
        Style::default().fg(Theme::GREY_600),
    )]));
    lines.push(Line::from(""));

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

    // Dependencies section
    if !file_index.summary.exports.is_empty()
        || !file_index.summary.used_by.is_empty()
        || !file_index.summary.depends_on.is_empty()
    {
        lines.push(Line::from(vec![
            Span::styled("    ╭─ ", Style::default().fg(Theme::GREY_600)),
            Span::styled("Dependencies", Style::default().fg(Theme::GREY_300)),
            Span::styled(
                " ─".to_string() + &"─".repeat(inner_width.saturating_sub(19)) + "╮",
                Style::default().fg(Theme::GREY_600),
            ),
        ]));

        // Exports
        if !file_index.summary.exports.is_empty() {
            let exports_str = file_index.summary.exports.join(", ");
            let label = "↗ Exports: ";
            let label_width = label.chars().count();
            let content_width = inner_width.saturating_sub(6 + label_width);
            let wrapped = wrap_text(&exports_str, content_width);

            for (i, line) in wrapped.iter().enumerate() {
                if i == 0 {
                    lines.push(Line::from(vec![
                        Span::styled("    │  ", Style::default().fg(Theme::GREY_600)),
                        Span::styled(label, Style::default().fg(Theme::GREY_400)),
                        Span::styled(line.to_string(), Style::default().fg(Theme::GREY_200)),
                    ]));
                } else {
                    lines.push(Line::from(vec![
                        Span::styled("    │  ", Style::default().fg(Theme::GREY_600)),
                        Span::styled(" ".repeat(label_width), Style::default()),
                        Span::styled(line.to_string(), Style::default().fg(Theme::GREY_200)),
                    ]));
                }
            }
        }

        // Used by
        if !file_index.summary.used_by.is_empty() {
            let used_by_str: Vec<_> = file_index
                .summary
                .used_by
                .iter()
                .filter_map(|p| {
                    p.file_name()
                        .and_then(|n| n.to_str())
                        .map(|s| s.to_string())
                })
                .collect();
            let used_by_full = used_by_str.join(", ");
            let label = "← Used by: ";
            let label_width = label.chars().count();
            let content_width = inner_width.saturating_sub(6 + label_width);
            let wrapped = wrap_text(&used_by_full, content_width);

            for (i, line) in wrapped.iter().enumerate() {
                if i == 0 {
                    lines.push(Line::from(vec![
                        Span::styled("    │  ", Style::default().fg(Theme::GREY_600)),
                        Span::styled(label, Style::default().fg(Theme::GREY_400)),
                        Span::styled(line.to_string(), Style::default().fg(Theme::GREY_200)),
                    ]));
                } else {
                    lines.push(Line::from(vec![
                        Span::styled("    │  ", Style::default().fg(Theme::GREY_600)),
                        Span::styled(" ".repeat(label_width), Style::default()),
                        Span::styled(line.to_string(), Style::default().fg(Theme::GREY_200)),
                    ]));
                }
            }
        }

        // Depends on
        if !file_index.summary.depends_on.is_empty() {
            let deps_str: Vec<_> = file_index
                .summary
                .depends_on
                .iter()
                .filter_map(|p| {
                    p.file_name()
                        .and_then(|n| n.to_str())
                        .map(|s| s.to_string())
                })
                .collect();
            let deps_full = deps_str.join(", ");
            let label = "→ Depends: ";
            let label_width = label.chars().count();
            let content_width = inner_width.saturating_sub(6 + label_width);
            let wrapped = wrap_text(&deps_full, content_width);

            for (i, line) in wrapped.iter().enumerate() {
                if i == 0 {
                    lines.push(Line::from(vec![
                        Span::styled("    │  ", Style::default().fg(Theme::GREY_600)),
                        Span::styled(label, Style::default().fg(Theme::GREY_400)),
                        Span::styled(line.to_string(), Style::default().fg(Theme::GREY_200)),
                    ]));
                } else {
                    lines.push(Line::from(vec![
                        Span::styled("    │  ", Style::default().fg(Theme::GREY_600)),
                        Span::styled(" ".repeat(label_width), Style::default()),
                        Span::styled(line.to_string(), Style::default().fg(Theme::GREY_200)),
                    ]));
                }
            }
        }

        lines.push(Line::from(vec![Span::styled(
            "    ╰".to_string() + &"─".repeat(inner_width.saturating_sub(4)) + "╯",
            Style::default().fg(Theme::GREY_600),
        )]));
    }

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
            "  Connect OpenRouter to enable AI suggestions in Cosmos.",
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
                crate::ui::openrouter_keys_shortcut_display(),
                Style::default().fg(Theme::GREY_400),
            ),
            Span::styled(" for OpenRouter keys", Style::default().fg(Theme::GREY_500)),
        ]),
        Line::from(""),
        Line::from(vec![
            Span::styled("  2) ", Style::default().fg(Theme::GREEN)),
            Span::styled("Add credits", Style::default().fg(Theme::GREY_200)),
            Span::styled(
                " (required for model usage)",
                Style::default().fg(Theme::GREY_400),
            ),
        ]),
        Line::from(vec![
            Span::styled("      ", Style::default()),
            Span::styled("Press ", Style::default().fg(Theme::GREY_500)),
            Span::styled(
                crate::ui::openrouter_credits_shortcut_display(),
                Style::default().fg(Theme::GREY_400),
            ),
            Span::styled(
                " for OpenRouter credits",
                Style::default().fg(Theme::GREY_500),
            ),
        ]),
        Line::from(""),
        Line::from(vec![
            Span::styled("  3) ", Style::default().fg(Theme::GREEN)),
            Span::styled(
                "Paste your API key below (it usually starts with ",
                Style::default().fg(Theme::GREY_400),
            ),
            Span::styled("sk-", Style::default().fg(Theme::GREY_200)),
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
    let has_sk_prefix = normalized_key.starts_with("sk-");
    let mask = if key_len == 0 {
        "█".to_string()
    } else {
        let hidden_count = if has_sk_prefix {
            key_len.saturating_sub(3)
        } else {
            key_len
        };
        let shown = hidden_count.min(48);
        let hidden = "•".repeat(shown);
        if has_sk_prefix {
            format!("sk-{}█", hidden)
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
                " to save key and refresh suggestions.",
                Style::default().fg(Theme::GREY_300),
            ),
        ]));
    }

    if key_len > 0 {
        let prefix_status = if has_sk_prefix {
            "prefix sk- detected"
        } else {
            "prefix sk- not detected"
        };
        lines.push(Line::from(vec![
            Span::styled("  Key check: ", Style::default().fg(Theme::GREY_500)),
            Span::styled(
                format!("{}, {} chars entered.", prefix_status, key_len),
                Style::default().fg(Theme::GREY_300),
            ),
        ]));
        if !has_sk_prefix {
            lines.push(Line::from(vec![
                Span::styled("  ! ", Style::default().fg(Theme::YELLOW)),
                Span::styled(
                    "OpenRouter keys usually start with sk-",
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
        "  Data use: selected snippets + file paths may be sent to OpenRouter.",
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
        " save + refresh "
    };
    lines.push(Line::from(vec![
        Span::styled("   ", Style::default()),
        Span::styled(
            " Enter ",
            Style::default().fg(Theme::GREY_900).bg(Theme::GREEN),
        ),
        Span::styled(enter_label, Style::default().fg(Theme::GREY_300)),
        Span::styled(
            crate::ui::openrouter_keys_shortcut_chip(),
            Style::default().fg(Theme::GREY_900).bg(Theme::GREY_400),
        ),
        Span::styled(" keys  ", Style::default().fg(Theme::GREY_400)),
        Span::styled(
            crate::ui::openrouter_credits_shortcut_chip(),
            Style::default().fg(Theme::GREY_900).bg(Theme::GREY_400),
        ),
        Span::styled(" credits  ", Style::default().fg(Theme::GREY_400)),
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
            "Cosmos may send selected code snippets and file paths to OpenRouter to generate and validate suggestions. Local cache stays in .cosmos and can be cleared with Reset.",
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
    scroll: usize,
    confirming_discard: bool,
) {
    let area = centered_rect(55, 45, frame.area());
    frame.render_widget(Clear, area);

    let title = if confirming_discard {
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
        .constraints([Constraint::Min(1), Constraint::Length(2)])
        .split(inner_area);

    let body_area = layout[0];
    let footer_area = layout[1];

    // Build body content
    let mut lines: Vec<Line> = Vec::new();

    if confirming_discard {
        // Confirmation dialog for discard
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "  Are you sure?",
            Style::default()
                .fg(Theme::WHITE)
                .add_modifier(Modifier::BOLD),
        )));
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "  This will permanently remove your uncommitted changes.",
            Style::default().fg(Theme::GREY_300),
        )));
        lines.push(Line::from(""));
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "  ─────────────────────────────────────────────────",
            Style::default().fg(Theme::GREY_600),
        )));
        lines.push(Line::from(vec![
            Span::styled("   ", Style::default()),
            Span::styled(" y ", Style::default().fg(Theme::GREY_900).bg(Theme::RED)),
            Span::styled(" yes, discard  ", Style::default().fg(Theme::GREY_400)),
            Span::styled(
                " n ",
                Style::default().fg(Theme::GREY_900).bg(Theme::GREY_400),
            ),
            Span::styled(" cancel", Style::default().fg(Theme::GREY_400)),
        ]));
        lines.push(Line::from(""));
    } else {
        // Main startup check dialog
        lines.push(Line::from(""));
        let headline = if changed_count == 0 && current_branch != main_branch {
            "  You're on a non-main branch"
        } else {
            "  You have unsaved work"
        };
        lines.push(Line::from(Span::styled(
            headline,
            Style::default()
                .fg(Theme::WHITE)
                .add_modifier(Modifier::BOLD),
        )));
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "  Cosmos works best from a fresh starting point.",
            Style::default().fg(Theme::GREY_300),
        )));
        lines.push(Line::from(Span::styled(
            if changed_count == 0 {
                "  No uncommitted changes found.".to_string()
            } else {
                format!(
                    "  You have {} file{} with changes.",
                    changed_count,
                    if changed_count == 1 { "" } else { "s" }
                )
            },
            Style::default().fg(Theme::GREY_300),
        )));
        if current_branch != main_branch {
            lines.push(Line::from(Span::styled(
                format!("  Branch: {} (main: {})", current_branch, main_branch),
                Style::default().fg(Theme::GREY_400),
            )));
        }
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "  ─────────────────────────────────────────────────",
            Style::default().fg(Theme::GREY_600),
        )));
        lines.push(Line::from(""));

        // Option: Save
        lines.push(Line::from(vec![
            Span::styled("   ", Style::default()),
            Span::styled(" s ", Style::default().fg(Theme::GREY_900).bg(Theme::GREEN)),
            Span::styled(
                "  Save my work and start fresh",
                Style::default().fg(Theme::GREY_100),
            ),
        ]));
        lines.push(Line::from(Span::styled(
            "      Your changes are safely stored.",
            Style::default().fg(Theme::GREY_500),
        )));
        lines.push(Line::from(""));

        // Option: Discard
        lines.push(Line::from(vec![
            Span::styled("   ", Style::default()),
            Span::styled(
                " d ",
                Style::default().fg(Theme::GREY_900).bg(Theme::GREY_500),
            ),
            Span::styled(
                "  Discard and start fresh",
                Style::default().fg(Theme::GREY_100),
            ),
        ]));
        lines.push(Line::from(Span::styled(
            "      Remove all changes and start clean.",
            Style::default().fg(Theme::GREY_500),
        )));
        lines.push(Line::from(""));

        // Option: Continue
        lines.push(Line::from(vec![
            Span::styled("   ", Style::default()),
            Span::styled(
                " c ",
                Style::default().fg(Theme::GREY_900).bg(Theme::GREY_500),
            ),
            Span::styled("  Continue as-is", Style::default().fg(Theme::GREY_100)),
        ]));
        lines.push(Line::from(Span::styled(
            "      Keep your current branch as the base for applied fixes.",
            Style::default().fg(Theme::GREY_500),
        )));
        lines.push(Line::from(""));
    }

    // Render scrollable body
    let body = Paragraph::new(lines)
        .wrap(Wrap { trim: false })
        .scroll((scroll as u16, 0));
    frame.render_widget(body, body_area);

    // Render fixed footer with scroll hint
    let footer_lines = vec![
        Line::from(Span::styled(
            "  ─────────────────────────────────────────────────",
            Style::default().fg(Theme::GREY_600),
        )),
        Line::from(vec![
            Span::styled("   ", Style::default()),
            Span::styled(
                " ↑↓ ",
                Style::default().fg(Theme::GREY_900).bg(Theme::GREY_400),
            ),
            Span::styled(" scroll ", Style::default().fg(Theme::GREY_400)),
        ]),
    ];
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
            "  Data use: Cosmos sends selected code snippets + file paths to OpenRouter for AI generation and validation.",
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
        // Layout explanation
        Line::from(vec![Span::styled(
            "  The main view focuses on AI suggestions and workflow.",
            Style::default().fg(Theme::GREY_400),
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
                "  i  ",
                Style::default().fg(Theme::GREY_900).bg(Theme::GREY_300),
            ),
            Span::styled(
                " Ask a question about your code",
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
