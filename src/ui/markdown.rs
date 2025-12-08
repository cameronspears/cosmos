//! Markdown to ratatui styled text converter
//!
//! Parses common markdown elements and renders them with appropriate styles.

#![allow(dead_code)]
#![allow(unused_mut)]

use ratatui::{
    style::{Modifier, Style},
    text::{Line, Span},
};
use super::theme::Theme;

/// Parse markdown text and convert to styled Lines
pub fn parse_markdown(text: &str, max_width: usize) -> Vec<Line<'static>> {
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut in_code_block = false;
    let mut code_block_content: Vec<String> = Vec::new();
    
    for line in text.lines() {
        // Handle code blocks
        if line.starts_with("```") {
            if in_code_block {
                // End of code block - render accumulated content
                for code_line in &code_block_content {
                    lines.push(render_code_block_line(code_line, max_width));
                }
                code_block_content.clear();
                in_code_block = false;
            } else {
                // Start of code block
                in_code_block = true;
            }
            continue;
        }
        
        if in_code_block {
            code_block_content.push(line.to_string());
            continue;
        }
        
        // Handle different line types
        if line.is_empty() {
            lines.push(Line::from(""));
        } else if line.starts_with("# ") {
            lines.push(render_h1(&line[2..], max_width));
        } else if line.starts_with("## ") {
            lines.push(render_h2(&line[3..], max_width));
        } else if line.starts_with("### ") {
            lines.push(render_h3(&line[4..], max_width));
        } else if line.starts_with("- ") || line.starts_with("* ") {
            let wrapped = wrap_and_parse_inline(&line[2..], max_width.saturating_sub(4));
            for (i, styled_line) in wrapped.into_iter().enumerate() {
                let prefix = if i == 0 { "  • " } else { "    " };
                let mut spans = vec![Span::styled(prefix, Style::default().fg(Theme::GREY_400))];
                spans.extend(styled_line.spans);
                lines.push(Line::from(spans));
            }
        } else if let Some(num_end) = line.find(". ") {
            // Numbered list (1. 2. etc.)
            if line[..num_end].chars().all(|c| c.is_ascii_digit()) {
                let content = &line[num_end + 2..];
                let wrapped = wrap_and_parse_inline(content, max_width.saturating_sub(5));
                for (i, styled_line) in wrapped.into_iter().enumerate() {
                    let prefix = if i == 0 { 
                        format!("  {}. ", &line[..num_end])
                    } else { 
                        "     ".to_string() 
                    };
                    let mut spans = vec![Span::styled(prefix, Style::default().fg(Theme::GREY_400))];
                    spans.extend(styled_line.spans);
                    lines.push(Line::from(spans));
                }
            } else {
                // Regular paragraph
                lines.extend(wrap_and_parse_inline(line, max_width));
            }
        } else if line.starts_with("> ") {
            // Block quote
            let wrapped = wrap_and_parse_inline(&line[2..], max_width.saturating_sub(4));
            for styled_line in wrapped {
                let mut spans = vec![Span::styled("  │ ", Style::default().fg(Theme::GREY_500))];
                spans.extend(styled_line.spans);
                lines.push(Line::from(spans));
            }
        } else {
            // Regular paragraph
            lines.extend(wrap_and_parse_inline(line, max_width));
        }
    }
    
    // Handle unclosed code block
    if in_code_block {
        for code_line in &code_block_content {
            lines.push(render_code_block_line(code_line, max_width));
        }
    }
    
    lines
}

/// Render a header level 1
fn render_h1(text: &str, _max_width: usize) -> Line<'static> {
    Line::from(vec![
        Span::styled(
            format!("✦ {}", text),
            Style::default()
                .fg(Theme::WHITE)
                .add_modifier(Modifier::BOLD)
        ),
    ])
}

/// Render a header level 2
fn render_h2(text: &str, _max_width: usize) -> Line<'static> {
    Line::from(vec![
        Span::styled(
            format!("◆ {}", text),
            Style::default()
                .fg(Theme::GREY_100)
                .add_modifier(Modifier::BOLD)
        ),
    ])
}

/// Render a header level 3
fn render_h3(text: &str, _max_width: usize) -> Line<'static> {
    Line::from(vec![
        Span::styled(
            format!("• {}", text),
            Style::default()
                .fg(Theme::GREY_200)
                .add_modifier(Modifier::BOLD)
        ),
    ])
}

/// Render a line in a code block
fn render_code_block_line(text: &str, _max_width: usize) -> Line<'static> {
    Line::from(vec![
        Span::styled("  ", Style::default()),
        Span::styled(
            text.to_string(),
            Style::default()
                .fg(Theme::GREY_200)
                .bg(Theme::GREY_700)
        ),
    ])
}

/// Parse inline markdown (bold, italic, code) and wrap text
fn wrap_and_parse_inline(text: &str, max_width: usize) -> Vec<Line<'static>> {
    // First wrap the text, then parse inline elements
    let wrapped = wrap_text_simple(text, max_width);
    wrapped.into_iter()
        .map(|line| parse_inline_markdown(&line))
        .collect()
}

/// Simple text wrapping
fn wrap_text_simple(text: &str, max_width: usize) -> Vec<String> {
    if max_width == 0 {
        return vec![text.to_string()];
    }
    
    let mut lines = Vec::new();
    let mut current_line = String::new();
    
    for word in text.split_whitespace() {
        if current_line.is_empty() {
            current_line = word.to_string();
        } else if current_line.len() + 1 + word.len() <= max_width {
            current_line.push(' ');
            current_line.push_str(word);
        } else {
            lines.push(current_line);
            current_line = word.to_string();
        }
    }
    
    if !current_line.is_empty() {
        lines.push(current_line);
    }
    
    if lines.is_empty() {
        lines.push(String::new());
    }
    
    lines
}

/// Parse inline markdown elements (bold, italic, code)
fn parse_inline_markdown(text: &str) -> Line<'static> {
    let mut spans = Vec::new();
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;
    let mut current_text = String::new();
    
    while i < chars.len() {
        // Check for inline code: `code`
        if chars[i] == '`' {
            // Flush current text
            if !current_text.is_empty() {
                spans.push(Span::styled(current_text.clone(), Style::default().fg(Theme::GREY_100)));
                current_text.clear();
            }
            
            // Find closing backtick
            let start = i + 1;
            i += 1;
            while i < chars.len() && chars[i] != '`' {
                i += 1;
            }
            
            if i < chars.len() {
                let code: String = chars[start..i].iter().collect();
                spans.push(Span::styled(
                    code,
                    Style::default()
                        .fg(Theme::WHITE)
                        .bg(Theme::GREY_700)
                ));
                i += 1;
            }
            continue;
        }
        
        // Check for bold: **text** or __text__
        if i + 1 < chars.len() && 
           ((chars[i] == '*' && chars[i + 1] == '*') || 
            (chars[i] == '_' && chars[i + 1] == '_')) {
            let marker = chars[i];
            
            // Flush current text
            if !current_text.is_empty() {
                spans.push(Span::styled(current_text.clone(), Style::default().fg(Theme::GREY_100)));
                current_text.clear();
            }
            
            // Find closing **
            let start = i + 2;
            i += 2;
            while i + 1 < chars.len() && !(chars[i] == marker && chars[i + 1] == marker) {
                i += 1;
            }
            
            if i + 1 < chars.len() {
                let bold_text: String = chars[start..i].iter().collect();
                spans.push(Span::styled(
                    bold_text,
                    Style::default()
                        .fg(Theme::WHITE)
                        .add_modifier(Modifier::BOLD)
                ));
                i += 2;
            }
            continue;
        }
        
        // Check for italic: *text* or _text_ (single)
        if (chars[i] == '*' || chars[i] == '_') && 
           (i + 1 >= chars.len() || (chars[i + 1] != chars[i])) {
            let marker = chars[i];
            
            // Look ahead to see if there's a closing marker
            let mut j = i + 1;
            while j < chars.len() && chars[j] != marker {
                j += 1;
            }
            
            if j < chars.len() && j > i + 1 {
                // Found closing marker
                if !current_text.is_empty() {
                    spans.push(Span::styled(current_text.clone(), Style::default().fg(Theme::GREY_100)));
                    current_text.clear();
                }
                
                let italic_text: String = chars[i + 1..j].iter().collect();
                spans.push(Span::styled(
                    italic_text,
                    Style::default()
                        .fg(Theme::GREY_200)
                        .add_modifier(Modifier::ITALIC)
                ));
                i = j + 1;
                continue;
            }
        }
        
        // Regular character
        current_text.push(chars[i]);
        i += 1;
    }
    
    // Flush remaining text
    if !current_text.is_empty() {
        spans.push(Span::styled(current_text, Style::default().fg(Theme::GREY_100)));
    }
    
    if spans.is_empty() {
        spans.push(Span::styled("", Style::default()));
    }
    
    Line::from(spans)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_header() {
        let lines = parse_markdown("# Hello World", 80);
        assert_eq!(lines.len(), 1);
    }

    #[test]
    fn test_parse_bold() {
        let line = parse_inline_markdown("This is **bold** text");
        assert!(line.spans.len() >= 3);
    }

    #[test]
    fn test_parse_code() {
        let line = parse_inline_markdown("Use the `println!` macro");
        assert!(line.spans.len() >= 3);
    }

    #[test]
    fn test_parse_list() {
        let lines = parse_markdown("- Item 1\n- Item 2", 80);
        assert_eq!(lines.len(), 2);
    }
}

