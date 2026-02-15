use super::{SUMMARY_MIN_CHARS, SUMMARY_MIN_WORDS};
use regex::Regex;

fn collapse_whitespace(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn first_sentence_only(text: &str) -> String {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return String::new();
    }

    for (idx, ch) in trimmed.char_indices() {
        if matches!(ch, '.' | '!' | '?') {
            return trimmed[..=idx].trim().to_string();
        }
    }
    trimmed.to_string()
}

fn ensure_sentence_punctuation(mut text: String) -> String {
    if !text.ends_with('.') && !text.ends_with('!') && !text.ends_with('?') {
        text.push('.');
    }
    text
}

fn is_fragment_sentence(summary: &str) -> bool {
    let normalized = summary
        .trim()
        .trim_end_matches(['.', '!', '?'])
        .to_ascii_lowercase();
    let Some(last_token) = normalized.split_whitespace().last() else {
        return true;
    };
    matches!(
        last_token,
        "is" | "are" | "was" | "were" | "to" | "with" | "if" | "when"
    )
}

pub(super) fn is_valid_grounded_summary(summary: &str) -> bool {
    let trimmed = summary.trim();
    if trimmed.len() < SUMMARY_MIN_CHARS {
        return false;
    }
    if trimmed.split_whitespace().count() < SUMMARY_MIN_WORDS {
        return false;
    }
    if is_fragment_sentence(trimmed) {
        return false;
    }
    true
}

pub(super) fn scrub_user_summary(summary: &str) -> String {
    let without_ticks = summary.replace('`', "");
    let evidence_re = Regex::new(r"(?i)\bevidence(?:\s*id)?\s*[:=]?\s*\d+\b")
        .expect("evidence regex should compile");
    let cleaned = evidence_re.replace_all(&without_ticks, "");
    collapse_whitespace(cleaned.trim())
}

fn candidate_sentence(text: &str) -> Option<String> {
    let scrubbed = scrub_user_summary(text);
    if scrubbed.is_empty() {
        return None;
    }
    let first = first_sentence_only(&scrubbed);
    let normalized = ensure_sentence_punctuation(collapse_whitespace(first.trim()));
    is_valid_grounded_summary(&normalized).then_some(normalized)
}

pub(super) fn normalize_grounded_summary(
    summary: &str,
    detail: &str,
    _evidence_line: usize,
) -> String {
    candidate_sentence(summary)
        .or_else(|| candidate_sentence(detail))
        .unwrap_or_default()
}

pub(super) fn normalize_grounded_detail(detail: &str, summary: &str) -> String {
    let mut normalized = collapse_whitespace(detail);
    if normalized.len() < 40 {
        let fallback = summary.trim();
        if !fallback.is_empty() {
            normalized = fallback.to_string();
        }
    }

    ensure_sentence_punctuation(normalized)
}
