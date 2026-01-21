//! Panel utilities for Cosmos UI
//!
//! Helper functions for rendering panels, buttons, and collapsible sections.

#![allow(dead_code)]

use crate::ui::theme::Theme;
use ratatui::{
    style::{Modifier, Style},
    text::{Line, Span},
};

// ═══════════════════════════════════════════════════════════════════════════
//  BUTTON COMPONENTS
// ═══════════════════════════════════════════════════════════════════════════

/// Button style variants for consistent appearance across the UI
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ButtonStyle {
    /// Primary action button (green background) - for main actions like "apply", "ship"
    Primary,
    /// Secondary button (grey background) - for standard actions
    #[default]
    Secondary,
    /// Danger button (red background) - for destructive actions like "discard", "reset"
    Danger,
    /// Ghost button (no background, just text) - for subtle/tertiary actions
    Ghost,
    /// Subtle button (dimmed) - for less prominent actions like "cancel", "back"
    Subtle,
}

impl ButtonStyle {
    /// Get the key style (the [k] part) for this button variant
    pub fn key_style(&self, highlighted: bool) -> Style {
        if highlighted {
            return Style::default()
                .fg(Theme::GREY_900)
                .bg(Theme::WHITE)
                .add_modifier(Modifier::BOLD);
        }

        match self {
            ButtonStyle::Primary => Style::default()
                .fg(Theme::GREY_900)
                .bg(Theme::GREEN),
            ButtonStyle::Secondary => Style::default()
                .fg(Theme::GREY_900)
                .bg(Theme::GREY_300),
            ButtonStyle::Danger => Style::default()
                .fg(Theme::GREY_900)
                .bg(Theme::RED),
            ButtonStyle::Ghost => Style::default()
                .fg(Theme::WHITE)
                .add_modifier(Modifier::BOLD),
            ButtonStyle::Subtle => Style::default()
                .fg(Theme::GREY_900)
                .bg(Theme::GREY_500),
        }
    }

    /// Get the label style (the " Label" part) for this button variant
    pub fn label_style(&self, highlighted: bool) -> Style {
        if highlighted {
            return Style::default()
                .fg(Theme::WHITE)
                .add_modifier(Modifier::BOLD);
        }

        match self {
            ButtonStyle::Primary => Style::default().fg(Theme::GREY_300),
            ButtonStyle::Secondary => Style::default().fg(Theme::GREY_300),
            ButtonStyle::Danger => Style::default().fg(Theme::GREY_400),
            ButtonStyle::Ghost => Style::default().fg(Theme::GREY_400),
            ButtonStyle::Subtle => Style::default().fg(Theme::GREY_500),
        }
    }
}

/// Create a styled button with the specified variant
/// Returns spans like: " k " " label "
pub fn styled_button<'a>(key: &str, label: &str, style: ButtonStyle, highlighted: bool) -> Vec<Span<'a>> {
    vec![
        Span::styled(format!(" {} ", key), style.key_style(highlighted)),
        Span::styled(format!(" {} ", label), style.label_style(highlighted)),
    ]
}

/// Create a button-style key hint like [a] Apply
/// Uses Secondary style by default, or highlighted white when selected
pub fn button<'a>(key: &str, label: &str, highlighted: bool) -> Vec<Span<'a>> {
    styled_button(key, label, ButtonStyle::Secondary, highlighted)
}

/// Create a primary action button (green) like [↵] Apply
pub fn button_primary<'a>(key: &str, label: &str) -> Vec<Span<'a>> {
    styled_button(key, label, ButtonStyle::Primary, false)
}

/// Create a danger button (red) like [d] Delete
pub fn button_danger<'a>(key: &str, label: &str) -> Vec<Span<'a>> {
    styled_button(key, label, ButtonStyle::Danger, false)
}

/// Create a subtle button (dimmed) like [Esc] Cancel
pub fn button_subtle<'a>(key: &str, label: &str) -> Vec<Span<'a>> {
    styled_button(key, label, ButtonStyle::Subtle, false)
}

/// Create a small/compact button (ghost style - no background on key)
pub fn button_sm<'a>(key: &str, label: &str) -> Vec<Span<'a>> {
    vec![
        Span::styled(format!("{}", key), Style::default().fg(Theme::WHITE).add_modifier(Modifier::BOLD)),
        Span::styled(format!(" {} ", label), Style::default().fg(Theme::GREY_400)),
    ]
}

/// Create an action bar with multiple buttons
pub fn action_bar<'a>(buttons: Vec<(&str, &str, bool)>) -> Line<'a> {
    let mut spans = vec![Span::styled("  ", Style::default())];

    for (i, (key, label, highlighted)) in buttons.into_iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled("  ", Style::default()));
        }
        spans.extend(button(key, label, highlighted));
    }

    Line::from(spans)
}

/// Create a subtle action bar with smaller buttons
pub fn action_bar_subtle<'a>(buttons: Vec<(&str, &str)>) -> Line<'a> {
    let mut spans = vec![Span::styled("     ", Style::default())];

    for (i, (key, label)) in buttons.into_iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled("   ", Style::default()));
        }
        spans.extend(button_sm(key, label));
    }

    Line::from(spans)
}

// ═══════════════════════════════════════════════════════════════════════════
//  SECTION CONTAINERS
// ═══════════════════════════════════════════════════════════════════════════

/// Create a section header with expand/collapse indicator
pub fn section_header<'a>(
    icon: &str,
    title: &str,
    count: Option<usize>,
    expanded: bool,
    is_selected: bool,
) -> Line<'a> {
    let expand_icon = if expanded { "▼" } else { "▶" };

    let (title_style, count_style) = if is_selected {
        (
            Style::default().fg(Theme::WHITE).add_modifier(Modifier::BOLD),
            Style::default().fg(Theme::GREY_200),
        )
    } else {
        (
            Style::default().fg(Theme::GREY_100).add_modifier(Modifier::BOLD),
            Style::default().fg(Theme::GREY_500),
        )
    };

    let cursor = if is_selected { " › " } else { "   " };

    let mut spans = vec![
        Span::styled(cursor, Style::default().fg(Theme::GREY_100)),
        Span::styled(format!("{} ", icon), Style::default().fg(Theme::GREY_400)),
        Span::styled(expand_icon, Style::default().fg(Theme::GREY_500)),
        Span::styled(format!(" {}", title), title_style),
    ];

    if let Some(c) = count {
        spans.push(Span::styled(format!(" ({})", c), count_style));
    }

    Line::from(spans)
}

/// Create a subsection header (for nested groupings)
pub fn subsection_header<'a>(title: &str, count: Option<usize>, is_selected: bool) -> Line<'a> {
    let style = if is_selected {
        Style::default().fg(Theme::WHITE).add_modifier(Modifier::ITALIC)
    } else {
        Style::default().fg(Theme::GREY_200).add_modifier(Modifier::ITALIC)
    };

    let cursor = if is_selected { "   › " } else { "     " };

    let mut spans = vec![
        Span::styled(cursor, Style::default().fg(Theme::GREY_100)),
        Span::styled(title.to_string(), style),
    ];

    if let Some(c) = count {
        spans.push(Span::styled(format!(" ({})", c), Style::default().fg(Theme::GREY_500)));
    }

    Line::from(spans)
}

// ═══════════════════════════════════════════════════════════════════════════
//  CONTENT CARDS
// ═══════════════════════════════════════════════════════════════════════════

/// Create a content card with rounded corners
pub fn card_top<'a>(width: usize) -> Line<'a> {
    let inner = "─".repeat(width.saturating_sub(2));
    Line::from(vec![
        Span::styled(format!("     ╭{}╮", inner), Style::default().fg(Theme::GREY_600)),
    ])
}

pub fn card_bottom<'a>(width: usize) -> Line<'a> {
    let inner = "─".repeat(width.saturating_sub(2));
    Line::from(vec![
        Span::styled(format!("     ╰{}╯", inner), Style::default().fg(Theme::GREY_600)),
    ])
}

pub fn card_line<'a>(content: &str, width: usize, style: Style) -> Line<'a> {
    let padded = format!("{:<width$}", content, width = width.saturating_sub(2));
    let truncated = if padded.chars().count() > width.saturating_sub(2) {
        padded.chars().take(width.saturating_sub(5)).collect::<String>() + "..."
    } else {
        padded
    };

    Line::from(vec![
        Span::styled("     │", Style::default().fg(Theme::GREY_600)),
        Span::styled(truncated, style),
        Span::styled("│", Style::default().fg(Theme::GREY_600)),
    ])
}

pub fn card_line_raw<'a>(spans: Vec<Span<'a>>, _width: usize) -> Line<'a> {
    let mut all_spans = vec![
        Span::styled("     │ ", Style::default().fg(Theme::GREY_600)),
    ];
    all_spans.extend(spans);
    // Note: width padding would need proper calculation based on content
    all_spans.push(Span::styled(" │", Style::default().fg(Theme::GREY_600)));
    Line::from(all_spans)
}

// ═══════════════════════════════════════════════════════════════════════════
//  BADGES & INDICATORS
// ═══════════════════════════════════════════════════════════════════════════

/// Create a priority badge
pub fn priority_badge<'a>(priority: char) -> Span<'a> {
    let (icon, style) = match priority {
        '●' => ("●", Style::default().fg(Theme::WHITE).add_modifier(Modifier::BOLD)),
        '◐' => ("◐", Style::default().fg(Theme::GREY_200)),
        '○' => ("○", Style::default().fg(Theme::GREY_400)),
        _ => ("·", Style::default().fg(Theme::GREY_500)),
    };
    Span::styled(icon.to_string(), style)
}

/// Create a category/type badge
pub fn type_badge<'a>(label: &str, color: ratatui::style::Color) -> Vec<Span<'a>> {
    vec![
        Span::styled(
            format!(" {} ", label),
            Style::default().fg(Theme::GREY_900).bg(color),
        ),
    ]
}

/// Create a status indicator
pub fn status_indicator<'a>(status: &str, positive: bool) -> Span<'a> {
    let (icon, style) = if positive {
        (Theme::CHECK_MARK, Style::default().fg(Theme::GREEN))
    } else {
        (Theme::CROSS_MARK, Style::default().fg(Theme::RED))
    };
    Span::styled(format!("{} {}", icon, status), style)
}

// ═══════════════════════════════════════════════════════════════════════════
//  SEPARATORS & DIVIDERS
// ═══════════════════════════════════════════════════════════════════════════

/// Create a thin separator line
pub fn separator<'a>(width: usize) -> Line<'a> {
    Line::from(vec![
        Span::styled(
            format!("     {}", "─".repeat(width)),
            Style::default().fg(Theme::GREY_700),
        ),
    ])
}

/// Create a dotted separator
pub fn separator_dotted<'a>(width: usize) -> Line<'a> {
    let dots = "·".repeat(width);
    Line::from(vec![
        Span::styled(format!("     {}", dots), Style::default().fg(Theme::GREY_700)),
    ])
}

/// Create a labeled separator
pub fn separator_labeled<'a>(label: &str, width: usize) -> Line<'a> {
    let label_len = label.chars().count();
    let left_len = 3;
    let right_len = width.saturating_sub(label_len + left_len + 2);

    Line::from(vec![
        Span::styled(format!("     {}", "─".repeat(left_len)), Style::default().fg(Theme::GREY_600)),
        Span::styled(format!(" {} ", label), Style::default().fg(Theme::GREY_400).add_modifier(Modifier::ITALIC)),
        Span::styled("─".repeat(right_len), Style::default().fg(Theme::GREY_600)),
    ])
}

// ═══════════════════════════════════════════════════════════════════════════
//  INFO ROWS
// ═══════════════════════════════════════════════════════════════════════════

/// Create a label-value info row
pub fn info_row<'a>(label: &str, value: &str) -> Line<'a> {
    Line::from(vec![
        Span::styled(format!("     {} ", label), Style::default().fg(Theme::GREY_500)),
        Span::styled(value.to_string(), Style::default().fg(Theme::GREY_200)),
    ])
}

/// Create a metric with icon
pub fn metric<'a>(icon: &str, value: &str, label: &str) -> Vec<Span<'a>> {
    vec![
        Span::styled(format!("{} ", icon), Style::default().fg(Theme::GREY_400)),
        Span::styled(value.to_string(), Style::default().fg(Theme::GREY_100).add_modifier(Modifier::BOLD)),
        Span::styled(format!(" {}", label), Style::default().fg(Theme::GREY_400)),
    ]
}

/// Format a time duration in days as a human-readable string
pub fn format_time_ago(days: i64) -> String {
    if days == 0 {
        "today".to_string()
    } else if days == 1 {
        "1d".to_string()
    } else if days < 7 {
        format!("{}d", days)
    } else if days < 30 {
        format!("{}w", days / 7)
    } else if days < 365 {
        format!("{}mo", days / 30)
    } else {
        format!("{}y", days / 365)
    }
}

/// Truncate a file path for display
pub fn truncate_path(path: &str, max_len: usize) -> String {
    if path.len() <= max_len {
        path.to_string()
    } else {
        let start = path.len() - max_len + 3;
        format!("...{}", &path[start..])
    }
}

/// Truncate text for display
pub fn truncate_text(text: &str, max_len: usize) -> String {
    if text.len() <= max_len {
        text.to_string()
    } else {
        format!("{}...", &text[..max_len.saturating_sub(3)])
    }
}

/// Create a progress bar string
pub fn progress_bar(value: f64, width: usize) -> String {
    let filled = ((value.clamp(0.0, 1.0)) * width as f64) as usize;
    let mut bar = String::new();

    for i in 0..width {
        if i < filled {
            bar.push(Theme::BAR_FILLED);
        } else {
            bar.push(Theme::BAR_EMPTY);
        }
    }

    bar
}

/// Create a styled line for a file tree entry
pub fn tree_line(
    name: &str,
    depth: usize,
    is_dir: bool,
    is_selected: bool,
    has_suggestions: bool,
) -> Line<'static> {
    let indent = "  ".repeat(depth);
    let icon = if is_dir {
        Theme::TREE_FOLDER_OPEN
    } else {
        Theme::TREE_FILE
    };

    let style = if is_selected {
        Style::default().fg(Theme::WHITE).add_modifier(Modifier::BOLD)
    } else if has_suggestions {
        Style::default().fg(Theme::GREY_100)
    } else if is_dir {
        Style::default().fg(Theme::GREY_300)
    } else {
        Style::default().fg(Theme::GREY_400)
    };

    let cursor = if is_selected { "› " } else { "  " };

    Line::from(vec![
        Span::styled(cursor, Style::default().fg(Theme::WHITE)),
        Span::styled(format!("{}{} ", indent, icon), Style::default().fg(Theme::GREY_600)),
        Span::styled(name.to_string(), style),
    ])
}

/// Create a styled suggestion line
pub fn suggestion_line(
    priority_icon: char,
    kind_label: &str,
    summary: &str,
    is_selected: bool,
) -> Line<'static> {
    let priority_style = match priority_icon {
        '\u{25CF}' => Style::default().fg(Theme::WHITE),  // High
        '\u{25D0}' => Style::default().fg(Theme::GREY_300),  // Medium
        _ => Style::default().fg(Theme::GREY_500),  // Low
    };

    let text_style = if is_selected {
        Style::default().fg(Theme::WHITE).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Theme::GREY_200)
    };

    let cursor = if is_selected { "› " } else { "  " };

    Line::from(vec![
        Span::styled(cursor, Style::default().fg(Theme::WHITE)),
        Span::styled(format!("{} ", priority_icon), priority_style),
        Span::styled(format!("{}: ", kind_label), Style::default().fg(Theme::GREY_400)),
        Span::styled(truncate_text(summary, 45), text_style),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_time_ago() {
        assert_eq!(format_time_ago(0), "today");
        assert_eq!(format_time_ago(1), "1d");
        assert_eq!(format_time_ago(7), "1w");
        assert_eq!(format_time_ago(30), "1mo");
        assert_eq!(format_time_ago(365), "1y");
    }

    #[test]
    fn test_truncate_path() {
        let path = "src/very/long/path/to/file.rs";
        let truncated = truncate_path(path, 20);
        assert!(truncated.starts_with("..."));
        assert!(truncated.len() <= 20);
    }

    #[test]
    fn test_progress_bar() {
        let bar = progress_bar(0.5, 10);
        assert_eq!(bar.chars().count(), 10);
    }
}
