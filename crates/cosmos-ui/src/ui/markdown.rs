//! Markdown to ratatui styled text converter.
//!
//! Uses a markdown event parser so Ask responses render consistent rich text.

use super::theme::Theme;
use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use ratatui::{
    style::{Modifier, Style},
    text::{Line, Span},
};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

#[derive(Clone, Copy, Default)]
struct InlineState {
    bold: usize,
    italic: usize,
    link: usize,
}

impl InlineState {
    fn style(&self, mut base: Style) -> Style {
        if self.bold > 0 {
            base = base.add_modifier(Modifier::BOLD).fg(Theme::WHITE);
        }
        if self.italic > 0 {
            base = base.add_modifier(Modifier::ITALIC);
        }
        if self.link > 0 {
            base = base.fg(Theme::ACCENT).add_modifier(Modifier::UNDERLINED);
        }
        base
    }
}

#[derive(Clone)]
struct StyledSegment {
    text: String,
    style: Style,
}

#[derive(Clone)]
struct BlockFormat {
    first_prefix: String,
    cont_prefix: String,
    prefix_style: Style,
    base_style: Style,
}

impl BlockFormat {
    fn paragraph(quote_depth: usize) -> Self {
        let quote = quote_prefix(quote_depth);
        Self {
            first_prefix: quote.clone(),
            cont_prefix: quote,
            prefix_style: Style::default().fg(Theme::GREY_500),
            base_style: Style::default().fg(Theme::GREY_100),
        }
    }

    fn heading(level: HeadingLevel, quote_depth: usize) -> Self {
        let base_style = match level {
            HeadingLevel::H1 => Style::default()
                .fg(Theme::WHITE)
                .add_modifier(Modifier::BOLD),
            HeadingLevel::H2 => Style::default()
                .fg(Theme::GREY_100)
                .add_modifier(Modifier::BOLD),
            _ => Style::default()
                .fg(Theme::GREY_200)
                .add_modifier(Modifier::BOLD),
        };

        let quote = quote_prefix(quote_depth);
        Self {
            first_prefix: quote.clone(),
            cont_prefix: quote,
            prefix_style: Style::default().fg(Theme::GREY_500),
            base_style,
        }
    }

    fn list_item(prefix: String, quote_depth: usize, list_depth: usize) -> Self {
        let indent = "  ".repeat(list_depth);
        let quote = quote_prefix(quote_depth);
        let first_prefix = format!("{}{}{}", quote, indent, prefix);
        let cont_prefix = format!("{}{}{}", quote, indent, " ".repeat(prefix.chars().count()));

        Self {
            first_prefix,
            cont_prefix,
            prefix_style: Style::default().fg(Theme::GREY_400),
            base_style: Style::default().fg(Theme::GREY_100),
        }
    }

    fn code_block(quote_depth: usize) -> Self {
        let quote = quote_prefix(quote_depth);
        Self {
            first_prefix: format!("{}│ ", quote),
            cont_prefix: format!("{}│ ", quote),
            prefix_style: Style::default().fg(Theme::GREY_500),
            base_style: Style::default().fg(Theme::GREY_200),
        }
    }
}

#[derive(Clone, Copy)]
enum ListState {
    Bullet,
    Ordered(u64),
}

fn quote_prefix(depth: usize) -> String {
    "│ ".repeat(depth)
}

/// Parse markdown text and convert to styled lines constrained by `max_width`.
pub fn parse_markdown(text: &str, max_width: usize) -> Vec<Line<'static>> {
    let options = Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_TASKLISTS
        | Options::ENABLE_SMART_PUNCTUATION;
    let parser = Parser::new_ext(text, options);

    let mut lines = Vec::new();
    let mut inline_state = InlineState::default();
    let mut blockquote_depth = 0usize;
    let mut list_stack: Vec<ListState> = Vec::new();

    let mut block = BlockFormat::paragraph(blockquote_depth);
    let mut current_segments: Vec<StyledSegment> = Vec::new();
    let mut in_code_block = false;

    let flush_block = |lines: &mut Vec<Line<'static>>,
                       block: &BlockFormat,
                       current_segments: &mut Vec<StyledSegment>| {
        if current_segments.is_empty() {
            return;
        }
        let wrapped = wrap_segments(
            current_segments,
            block,
            max_width.max(1),
            Style::default().fg(Theme::GREY_100),
        );
        lines.extend(wrapped);
        current_segments.clear();
    };

    for event in parser {
        match event {
            Event::Start(tag) => match tag {
                Tag::Paragraph => {
                    flush_block(&mut lines, &block, &mut current_segments);
                    block = BlockFormat::paragraph(blockquote_depth);
                }
                Tag::Heading { level, .. } => {
                    flush_block(&mut lines, &block, &mut current_segments);
                    block = BlockFormat::heading(level, blockquote_depth);
                }
                Tag::BlockQuote(_) => {
                    flush_block(&mut lines, &block, &mut current_segments);
                    blockquote_depth += 1;
                    block = BlockFormat::paragraph(blockquote_depth);
                }
                Tag::List(Some(start)) => list_stack.push(ListState::Ordered(start)),
                Tag::List(None) => list_stack.push(ListState::Bullet),
                Tag::Item => {
                    flush_block(&mut lines, &block, &mut current_segments);
                    let list_depth = list_stack.len().saturating_sub(1);
                    let marker = match list_stack.last_mut() {
                        Some(ListState::Ordered(next)) => {
                            let current = *next;
                            *next = next.saturating_add(1);
                            format!("{}. ", current)
                        }
                        _ => "• ".to_string(),
                    };
                    block = BlockFormat::list_item(marker, blockquote_depth, list_depth);
                }
                Tag::Emphasis => inline_state.italic += 1,
                Tag::Strong => inline_state.bold += 1,
                Tag::Link { .. } => inline_state.link += 1,
                Tag::CodeBlock(kind) => {
                    flush_block(&mut lines, &block, &mut current_segments);
                    in_code_block = true;
                    block = BlockFormat::code_block(blockquote_depth);
                    if let CodeBlockKind::Fenced(lang) = kind {
                        let lang = lang.trim();
                        if !lang.is_empty() {
                            current_segments.push(StyledSegment {
                                text: format!("[{}]", lang),
                                style: Style::default().fg(Theme::GREY_500),
                            });
                            flush_block(&mut lines, &block, &mut current_segments);
                        }
                    }
                }
                _ => {}
            },
            Event::End(tag) => match tag {
                TagEnd::Paragraph | TagEnd::Heading(_) | TagEnd::Item => {
                    flush_block(&mut lines, &block, &mut current_segments);
                    block = BlockFormat::paragraph(blockquote_depth);
                }
                TagEnd::BlockQuote(_) => {
                    flush_block(&mut lines, &block, &mut current_segments);
                    blockquote_depth = blockquote_depth.saturating_sub(1);
                    block = BlockFormat::paragraph(blockquote_depth);
                }
                TagEnd::List(_) => {
                    flush_block(&mut lines, &block, &mut current_segments);
                    list_stack.pop();
                    block = BlockFormat::paragraph(blockquote_depth);
                }
                TagEnd::Emphasis => inline_state.italic = inline_state.italic.saturating_sub(1),
                TagEnd::Strong => inline_state.bold = inline_state.bold.saturating_sub(1),
                TagEnd::Link => inline_state.link = inline_state.link.saturating_sub(1),
                TagEnd::CodeBlock => {
                    flush_block(&mut lines, &block, &mut current_segments);
                    in_code_block = false;
                    block = BlockFormat::paragraph(blockquote_depth);
                }
                _ => {}
            },
            Event::Text(content) => {
                let style = if in_code_block {
                    block.base_style
                } else {
                    inline_state.style(block.base_style)
                };
                current_segments.push(StyledSegment {
                    text: content.into_string(),
                    style,
                });
            }
            Event::Code(content) => {
                current_segments.push(StyledSegment {
                    text: content.into_string(),
                    style: Style::default()
                        .fg(Theme::GREY_200)
                        .add_modifier(Modifier::BOLD),
                });
            }
            Event::SoftBreak => {
                current_segments.push(StyledSegment {
                    text: " ".to_string(),
                    style: block.base_style,
                });
            }
            Event::HardBreak => {
                current_segments.push(StyledSegment {
                    text: "\n".to_string(),
                    style: block.base_style,
                });
            }
            Event::Rule => {
                flush_block(&mut lines, &block, &mut current_segments);
                let rule_len = max_width.clamp(8, 64);
                lines.push(Line::from(vec![Span::styled(
                    "─".repeat(rule_len),
                    Style::default().fg(Theme::GREY_500),
                )]));
            }
            Event::Html(content) | Event::InlineHtml(content) => {
                current_segments.push(StyledSegment {
                    text: content.into_string(),
                    style: block.base_style,
                });
            }
            Event::FootnoteReference(label) => {
                current_segments.push(StyledSegment {
                    text: format!("[{}]", label),
                    style: Style::default().fg(Theme::GREY_400),
                });
            }
            Event::TaskListMarker(done) => {
                let marker = if done { "[x] " } else { "[ ] " };
                current_segments.push(StyledSegment {
                    text: marker.to_string(),
                    style: Style::default().fg(Theme::GREY_400),
                });
            }
            _ => {}
        }
    }

    flush_block(&mut lines, &block, &mut current_segments);
    if lines.is_empty() {
        lines.push(Line::from(""));
    }
    lines
}

fn wrap_segments(
    segments: &[StyledSegment],
    format: &BlockFormat,
    max_width: usize,
    space_style: Style,
) -> Vec<Line<'static>> {
    #[derive(Clone)]
    enum Token {
        Word(String, Style),
        Break,
    }

    let mut tokens = Vec::new();
    for seg in segments {
        let mut first_part = true;
        for part in seg.text.split('\n') {
            if !first_part {
                tokens.push(Token::Break);
            }
            first_part = false;
            for word in part.split_whitespace() {
                tokens.push(Token::Word(word.to_string(), seg.style));
            }
        }
    }

    if tokens.is_empty() {
        return vec![Line::from("")];
    }

    let mut lines = Vec::new();
    let mut line_spans: Vec<Span<'static>> = Vec::new();
    let mut line_width = 0usize;
    let mut line_has_word = false;
    let mut first_line = true;

    let start_line = |line_spans: &mut Vec<Span<'static>>, first: bool| {
        let prefix = if first {
            &format.first_prefix
        } else {
            &format.cont_prefix
        };
        if !prefix.is_empty() {
            line_spans.push(Span::styled(prefix.clone(), format.prefix_style));
        }
    };

    let line_limit = |first: bool| -> usize {
        let prefix = if first {
            &format.first_prefix
        } else {
            &format.cont_prefix
        };
        max_width.saturating_sub(prefix.width()).max(1)
    };

    start_line(&mut line_spans, first_line);

    let push_line = |lines: &mut Vec<Line<'static>>,
                     line_spans: &mut Vec<Span<'static>>,
                     first_line: &mut bool,
                     line_width: &mut usize,
                     line_has_word: &mut bool| {
        lines.push(Line::from(std::mem::take(line_spans)));
        *first_line = false;
        *line_width = 0;
        *line_has_word = false;
        start_line(line_spans, false);
    };

    for token in tokens {
        match token {
            Token::Break => {
                push_line(
                    &mut lines,
                    &mut line_spans,
                    &mut first_line,
                    &mut line_width,
                    &mut line_has_word,
                );
            }
            Token::Word(word, style) => {
                let limit = line_limit(first_line);
                let word_width = word.width();
                let needed = if line_has_word {
                    line_width.saturating_add(1 + word_width)
                } else {
                    word_width
                };

                if line_has_word && needed > limit {
                    push_line(
                        &mut lines,
                        &mut line_spans,
                        &mut first_line,
                        &mut line_width,
                        &mut line_has_word,
                    );
                }

                if line_has_word {
                    line_spans.push(Span::styled(" ".to_string(), space_style));
                    line_width = line_width.saturating_add(1);
                }

                if word_width <= line_limit(first_line) {
                    line_spans.push(Span::styled(word, style));
                    line_width = line_width.saturating_add(word_width);
                    line_has_word = true;
                    continue;
                }

                let mut chunk = String::new();
                let mut chunk_width = 0usize;
                for ch in word.chars() {
                    let ch_width = ch.width().unwrap_or(1);
                    let chunk_limit = line_limit(first_line);
                    if line_has_word && chunk.is_empty() && line_width + ch_width > chunk_limit {
                        push_line(
                            &mut lines,
                            &mut line_spans,
                            &mut first_line,
                            &mut line_width,
                            &mut line_has_word,
                        );
                    }
                    if chunk_width + ch_width > chunk_limit && !chunk.is_empty() {
                        line_spans.push(Span::styled(chunk.clone(), style));
                        line_width = line_width.saturating_add(chunk_width);
                        line_has_word = true;
                        push_line(
                            &mut lines,
                            &mut line_spans,
                            &mut first_line,
                            &mut line_width,
                            &mut line_has_word,
                        );
                        chunk.clear();
                        chunk_width = 0;
                    }
                    chunk.push(ch);
                    chunk_width += ch_width;
                }
                if !chunk.is_empty() {
                    line_spans.push(Span::styled(chunk, style));
                    line_width = line_width.saturating_add(chunk_width);
                    line_has_word = true;
                }
            }
        }
    }

    lines.push(Line::from(line_spans));
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_markdown_renders_core_elements() {
        let input = "# Title\n\nThis has **bold** and *italic* and a [link](https://example.com).\n\n- One\n- Two\n\n> Quote\n\n```rust\nfn main() {}\n```";
        let lines = parse_markdown(input, 80);
        let rendered = lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .map(|span| span.content.as_ref())
            .collect::<String>();

        assert!(rendered.contains("Title"));
        assert!(rendered.contains("bold"));
        assert!(rendered.contains("italic"));
        assert!(rendered.contains("One"));
        assert!(rendered.contains("Two"));
        assert!(rendered.contains("Quote"));
        assert!(rendered.contains("fn main"));
    }

    #[test]
    fn parse_markdown_wraps_long_lines() {
        let input = "This is a long paragraph that should wrap across multiple terminal lines for readability.";
        let lines = parse_markdown(input, 24);
        assert!(lines.len() > 1);
    }

    #[test]
    fn parse_markdown_keeps_code_block_content() {
        let input = "```\nlet value = 42;\nprintln!(\"{}\", value);\n```";
        let lines = parse_markdown(input, 80);
        let rendered = lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert!(rendered.contains("let value = 42;"));
        assert!(rendered.contains("println!"));
    }
}
