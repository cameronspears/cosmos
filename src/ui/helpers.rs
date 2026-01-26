//! UI helper functions and utilities

use ratatui::layout::{Constraint, Direction, Layout, Rect};

/// Create a centered rect using up certain percentage of the available rect
pub fn centered_rect(percent_x: u16, percent_y: u16, r: Rect) -> Rect {
    let popup_layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(r);

    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(popup_layout[1])[1]
}

/// Wrap text to fit within a given width
pub fn wrap_text(text: &str, width: usize) -> Vec<String> {
    wrap_text_variable_width(text, width, width)
}

/// Wrap text with different widths for first line vs continuation lines
/// This is useful when the first line has a prefix (like "Fix: ") that takes up space
pub fn wrap_text_variable_width(
    text: &str,
    first_line_width: usize,
    continuation_width: usize,
) -> Vec<String> {
    if first_line_width == 0 || continuation_width == 0 {
        return vec![text.to_string()];
    }

    let mut lines = Vec::new();
    let mut current_line = String::new();

    for word in text.split_whitespace() {
        // Use first_line_width for the first line, continuation_width for others
        let current_width = if lines.is_empty() {
            first_line_width
        } else {
            continuation_width
        };

        if current_line.is_empty() {
            if word.len() > current_width {
                // Word is longer than width, force break it
                let mut remaining = word;
                while remaining.len() > current_width {
                    lines.push(remaining[..current_width].to_string());
                    remaining = &remaining[current_width..];
                }
                current_line = remaining.to_string();
            } else {
                current_line = word.to_string();
            }
        } else if current_line.len() + 1 + word.len() <= current_width {
            current_line.push(' ');
            current_line.push_str(word);
        } else {
            lines.push(current_line);
            // After pushing, we're now on a continuation line
            let next_width = continuation_width;
            if word.len() > next_width {
                let mut remaining = word;
                while remaining.len() > next_width {
                    lines.push(remaining[..next_width].to_string());
                    remaining = &remaining[next_width..];
                }
                current_line = remaining.to_string();
            } else {
                current_line = word.to_string();
            }
        }
    }

    if !current_line.is_empty() {
        lines.push(current_line);
    }

    if lines.is_empty() {
        vec![String::new()]
    } else {
        lines
    }
}

/// Convert the first character of a string to lowercase
pub fn lowercase_first(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        None => String::new(),
        Some(c) => c.to_lowercase().chain(chars).collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_wrap_text_single_line() {
        let result = wrap_text("hello", 10);
        assert_eq!(result, vec!["hello"]);
    }

    #[test]
    fn test_wrap_text_multiple_lines() {
        let result = wrap_text("hello world foo bar", 10);
        assert!(result.len() > 1);
        for line in &result {
            assert!(line.len() <= 10);
        }
    }

    #[test]
    fn test_wrap_text_empty() {
        let result = wrap_text("", 10);
        assert_eq!(result, vec![""]);
    }

    #[test]
    fn test_lowercase_first_basic() {
        assert_eq!(lowercase_first("Hello"), "hello");
        assert_eq!(lowercase_first("WORLD"), "wORLD");
    }

    #[test]
    fn test_lowercase_first_empty() {
        assert_eq!(lowercase_first(""), "");
    }

    #[test]
    fn test_lowercase_first_already_lowercase() {
        assert_eq!(lowercase_first("already"), "already");
    }

    #[test]
    fn test_centered_rect() {
        use ratatui::layout::Rect;
        let parent = Rect::new(0, 0, 100, 100);
        let centered = centered_rect(50, 50, parent);
        // Should be centered
        assert!(centered.x > 0);
        assert!(centered.y > 0);
        assert!(centered.width < 100);
        assert!(centered.height < 100);
    }
}
