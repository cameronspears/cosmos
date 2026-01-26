use crate::ui::helpers::{centered_rect, wrap_text};
use crate::ui::theme::Theme;
use ratatui::{
    layout::{Constraint, Direction, Layout},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph, Wrap},
    Frame,
};
use std::path::Path;

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
    help_text.push(key_row("PgUp/Dn", "Page scroll"));
    help_text.push(key_row("Tab", "Switch between panels"));
    help_text.push(key_row("↵", "Expand/collapse or view details"));
    help_text.push(key_row("Esc", "Go back / cancel"));
    help_text.push(section_spacer());
    help_text.push(section_end());

    // File Explorer section
    help_text.extend(section_start("File Explorer"));
    help_text.push(section_spacer());
    help_text.push(key_row("/", "Search files"));
    help_text.push(key_row("g", "Toggle grouped/flat view"));
    help_text.push(section_spacer());
    help_text.push(section_end());

    // Actions section
    help_text.extend(section_start("Actions"));
    help_text.push(section_spacer());
    help_text.push(key_row("↵", "Select / apply"));
    help_text.push(key_row("a", "Ask Cosmos"));
    help_text.push(key_row("?", "Show help"));
    help_text.push(key_row("q", "Quit"));
    help_text.push(section_spacer());
    help_text.push(section_end());

    // Privacy and reset section
    help_text.extend(section_start("Privacy"));
    help_text.push(section_spacer());
    help_text.push(key_row("R", "Reset Cosmos"));
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
    file_index: &crate::index::FileIndex,
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
                crate::index::SymbolKind::Function | crate::index::SymbolKind::Method
            )
        })
        .count();
    let struct_count = file_index
        .symbols
        .iter()
        .filter(|s| {
            matches!(
                s.kind,
                crate::index::SymbolKind::Struct | crate::index::SymbolKind::Class
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

pub(super) fn render_reset_overlay(
    frame: &mut Frame,
    options: &[(crate::cache::ResetOption, bool)],
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
