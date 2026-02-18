/// Truncate file contents for prompt safety (keep beginning + end)
pub(crate) fn truncate_content(content: &str, max_chars: usize) -> String {
    if content.chars().count() <= max_chars {
        content.to_string()
    } else {
        let head: String = content.chars().take(max_chars / 2).collect();
        let tail: String = content
            .chars()
            .rev()
            .take(max_chars / 2)
            .collect::<String>();
        format!(
            "{}\n\n... [truncated] ...\n\n{}",
            head,
            tail.chars().rev().collect::<String>()
        )
    }
}

/// Truncate content around a specific line number (1-based).
pub(crate) fn truncate_content_around_line(
    content: &str,
    line_number: usize,
    max_chars: usize,
) -> Option<String> {
    if max_chars == 0 {
        return None;
    }

    let lines: Vec<&str> = content.lines().collect();
    if lines.is_empty() {
        return None;
    }

    let target = line_number.saturating_sub(1);
    if target >= lines.len() {
        return None;
    }

    let max_radius = target.max(lines.len().saturating_sub(1).saturating_sub(target));
    let mut best: Option<(usize, usize)> = None;
    let mut lo = 0usize;
    let mut hi = max_radius;

    while lo <= hi {
        let mid = (lo + hi) / 2;
        let start = target.saturating_sub(mid);
        let end = (target + mid).min(lines.len() - 1);
        let snippet = lines[start..=end].join("\n");
        if snippet.chars().count() <= max_chars {
            best = Some((start, end));
            lo = mid + 1;
        } else if mid == 0 {
            break;
        } else {
            hi = mid - 1;
        }
    }

    if let Some((start, end)) = best {
        return Some(lines[start..=end].join("\n"));
    }

    Some(truncate_line_to_chars(lines[target], max_chars))
}

fn truncate_line_to_chars(line: &str, max_chars: usize) -> String {
    let count = line.chars().count();
    if count <= max_chars {
        return line.to_string();
    }
    if max_chars <= 3 {
        return line.chars().take(max_chars).collect();
    }
    let prefix: String = line.chars().take(max_chars - 3).collect();
    format!("{}...", prefix)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_truncate_content() {
        let content = "line1\nline2\nline3\nline4\nline5";
        let truncated = truncate_content(content, 15);
        assert!(truncated.contains("truncated"));
        assert!(truncated.len() < content.len() + 20);
    }

    #[test]
    fn test_truncate_content_around_line_prefers_nearby_context() {
        let content = "a\nb\nc\nd\ne\nf\ng";
        let snippet = truncate_content_around_line(content, 4, 5).expect("snippet");
        assert!(snippet.contains('d'));
    }
}
