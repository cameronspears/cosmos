use crate::analysis::{ChurnEntry, DangerZone, DustyFile, TodoEntry};
use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, Widget},
};

pub struct Panel;

impl Panel {
    /// Create the hotspots panel showing files with highest churn
    pub fn hotspots(entries: &[ChurnEntry], scroll_offset: usize, accent: Color) -> impl Widget + '_ {
        let items: Vec<ListItem> = entries
            .iter()
            .skip(scroll_offset)
            .enumerate()
            .map(|(idx, entry)| {
                let style = if idx == 0 {
                    Style::default().fg(accent).add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(Color::Rgb(226, 232, 240))
                };

                let line = format_hotspot_line(&entry.path, entry.change_count, entry.days_active);
                ListItem::new(Line::from(vec![Span::styled(line, style)]))
            })
            .collect();

        let title = format!(" Hotspots ({} files) ", entries.len());

        List::new(items)
            .block(
                Block::default()
                    .title(title)
                    .title_style(Style::default().fg(accent).add_modifier(Modifier::BOLD))
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(Color::Rgb(51, 65, 85)))
                    .style(Style::default().bg(Color::Rgb(15, 23, 42))),
            )
    }

    /// Create the dusty files panel showing old untouched files
    pub fn dusty_files(files: &[DustyFile], scroll_offset: usize, accent: Color) -> impl Widget + '_ {
        let items: Vec<ListItem> = files
            .iter()
            .skip(scroll_offset)
            .enumerate()
            .map(|(idx, file)| {
                let style = if idx == 0 {
                    Style::default().fg(accent).add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(Color::Rgb(226, 232, 240))
                };

                let line = format_dusty_line(&file.path, file.days_since_change, file.line_count);
                ListItem::new(Line::from(vec![Span::styled(line, style)]))
            })
            .collect();

        let title = format!(" Dusty Files ({} files) ", files.len());

        List::new(items)
            .block(
                Block::default()
                    .title(title)
                    .title_style(Style::default().fg(accent).add_modifier(Modifier::BOLD))
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(Color::Rgb(51, 65, 85)))
                    .style(Style::default().bg(Color::Rgb(15, 23, 42))),
            )
    }

    /// Create the TODOs panel showing all TODO/HACK/FIXME entries
    pub fn todos(entries: &[TodoEntry], scroll_offset: usize, accent: Color) -> impl Widget + '_ {
        let items: Vec<ListItem> = entries
            .iter()
            .skip(scroll_offset)
            .enumerate()
            .map(|(idx, entry)| {
                let kind_color = match entry.kind {
                    crate::analysis::scanner::TodoKind::Fixme => Color::Rgb(248, 113, 113), // Red
                    crate::analysis::scanner::TodoKind::Hack => Color::Rgb(251, 191, 36),   // Amber
                    crate::analysis::scanner::TodoKind::Todo => Color::Rgb(96, 165, 250),   // Blue
                    crate::analysis::scanner::TodoKind::Xxx => Color::Rgb(167, 139, 250),   // Purple
                };

                let base_style = if idx == 0 {
                    Style::default().add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                };

                let line = Line::from(vec![
                    Span::styled(
                        format!(" {:6} ", entry.kind.as_str()),
                        base_style.fg(kind_color),
                    ),
                    Span::styled(
                        format!("{}:{}", truncate_path(&entry.path, 30), entry.line_number),
                        base_style.fg(Color::Rgb(148, 163, 184)),
                    ),
                    Span::raw("  "),
                    Span::styled(
                        truncate_text(&entry.text, 40),
                        base_style.fg(Color::Rgb(226, 232, 240)),
                    ),
                ]);

                ListItem::new(line)
            })
            .collect();

        let title = format!(" TODOs & HACKs ({} items) ", entries.len());

        List::new(items)
            .block(
                Block::default()
                    .title(title)
                    .title_style(Style::default().fg(accent).add_modifier(Modifier::BOLD))
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(Color::Rgb(51, 65, 85)))
                    .style(Style::default().bg(Color::Rgb(15, 23, 42))),
            )
    }

    /// Create the danger zones panel showing high-churn + high-complexity files
    pub fn danger_zones(zones: &[DangerZone], scroll_offset: usize, accent: Color) -> impl Widget + '_ {
        let items: Vec<ListItem> = zones
            .iter()
            .skip(scroll_offset)
            .enumerate()
            .flat_map(|(idx, zone)| {
                // Color based on danger score
                let danger_color = if zone.danger_score >= 70.0 {
                    Color::Rgb(248, 113, 113) // Red - critical
                } else if zone.danger_score >= 50.0 {
                    Color::Rgb(251, 146, 60) // Orange - high
                } else {
                    Color::Rgb(250, 204, 21) // Yellow - medium
                };

                let risk_label = if zone.danger_score >= 70.0 {
                    "!!"
                } else if zone.danger_score >= 50.0 {
                    "! "
                } else {
                    ". "
                };

                let base_style = if idx == 0 {
                    Style::default().add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                };

                // Main line with file path
                let main_line = Line::from(vec![
                    Span::styled(
                        format!(" {} ", risk_label),
                        base_style.fg(danger_color),
                    ),
                    Span::styled(
                        truncate_path(&zone.path, 50),
                        base_style.fg(Color::Rgb(226, 232, 240)),
                    ),
                ]);

                // Detail line with stats and reason
                let detail_line = Line::from(vec![
                    Span::raw("      "),
                    Span::styled(
                        format!("{} changes", zone.change_count),
                        Style::default().fg(Color::Rgb(96, 165, 250)),
                    ),
                    Span::styled(
                        format!(" | complexity {:.1}", zone.complexity_score),
                        Style::default().fg(Color::Rgb(148, 163, 184)),
                    ),
                    Span::styled(
                        format!(" | {}", zone.reason),
                        Style::default().fg(Color::Rgb(134, 239, 172)),
                    ),
                ]);

                vec![ListItem::new(main_line), ListItem::new(detail_line)]
            })
            .collect();

        let title = format!(" Danger Zones ({} files) - high churn + high complexity ", zones.len());

        List::new(items)
            .block(
                Block::default()
                    .title(title)
                    .title_style(Style::default().fg(accent).add_modifier(Modifier::BOLD))
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(Color::Rgb(51, 65, 85)))
                    .style(Style::default().bg(Color::Rgb(15, 23, 42))),
            )
    }
}

fn format_hotspot_line(path: &str, changes: usize, days_active: i64) -> String {
    let truncated = truncate_path(path, 45);
    let dots = ".".repeat(50usize.saturating_sub(truncated.len()));
    let time_str = format_time_ago(days_active);
    format!(" {} {} {} changes ({})", truncated, dots, changes, time_str)
}

fn format_dusty_line(path: &str, days: i64, lines: usize) -> String {
    let truncated = truncate_path(path, 45);
    let dots = ".".repeat(50usize.saturating_sub(truncated.len()));
    let time_str = format_time_ago(days);
    format!(" {} {} {} lines, untouched for {}", truncated, dots, lines, time_str)
}

fn format_time_ago(days: i64) -> String {
    if days == 0 {
        "today".to_string()
    } else if days == 1 {
        "1 day".to_string()
    } else if days < 7 {
        format!("{} days", days)
    } else if days < 30 {
        let weeks = days / 7;
        if weeks == 1 {
            "1 week".to_string()
        } else {
            format!("{} weeks", weeks)
        }
    } else if days < 365 {
        let months = days / 30;
        if months == 1 {
            "1 month".to_string()
        } else {
            format!("{} months", months)
        }
    } else {
        let years = days / 365;
        if years == 1 {
            "1 year".to_string()
        } else {
            format!("{} years", years)
        }
    }
}

fn truncate_path(path: &str, max_len: usize) -> String {
    if path.len() <= max_len {
        path.to_string()
    } else {
        let start = path.len() - max_len + 3;
        format!("...{}", &path[start..])
    }
}

fn truncate_text(text: &str, max_len: usize) -> String {
    if text.len() <= max_len {
        text.to_string()
    } else {
        format!("{}...", &text[..max_len - 3])
    }
}


