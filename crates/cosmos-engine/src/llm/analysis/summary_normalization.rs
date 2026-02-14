use super::{SUMMARY_MIN_CHARS, SUMMARY_MIN_WORDS};
use regex::Regex;

fn fallback_never_match_regex() -> Regex {
    Regex::new("$^").expect("fallback regex should compile")
}

fn safe_regex(pattern: &str) -> Regex {
    Regex::new(pattern).unwrap_or_else(|_| fallback_never_match_regex())
}

fn collapse_whitespace(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn is_low_information_summary(summary: &str) -> bool {
    let trimmed = summary.trim();
    if trimmed.len() < SUMMARY_MIN_CHARS {
        return true;
    }
    let words = trimmed.split_whitespace().count();
    if words < SUMMARY_MIN_WORDS {
        return true;
    }
    let lower = trimmed.to_ascii_lowercase();
    let has_internal_reference = trimmed.contains('/')
        || trimmed.contains("::")
        || trimmed.contains('`')
        || trimmed
            .split_whitespace()
            .any(|token| token.contains('_') && token.len() >= 8)
        || lower.starts_with("in the code that")
        || lower.starts_with("the test ");
    let normalized_lower = lower.trim_end_matches(['.', '!', '?']);
    let has_vague_hidden_errors = normalized_lower.ends_with("hidden errors")
        || normalized_lower.starts_with("hidden errors")
        || normalized_lower.contains("hidden errors when")
        || normalized_lower.contains(", hidden errors")
        || normalized_lower.contains(" hidden errors,");
    lower == "when users"
        || lower == "when someone"
        || lower == "when a user"
        || has_internal_reference
        || has_vague_hidden_errors
        || lower.ends_with(" may")
        || lower.ends_with(" can")
        || lower.ends_with(" should")
}

fn sentence_like_fragment(text: &str) -> Option<String> {
    let cleaned = collapse_whitespace(text);
    if cleaned.is_empty() {
        return None;
    }
    for raw in cleaned.split(['.', '!', '?']) {
        let candidate = scrub_user_summary(raw).trim().to_string();
        if candidate.len() >= SUMMARY_MIN_CHARS
            && candidate.split_whitespace().count() >= SUMMARY_MIN_WORDS
        {
            return Some(candidate);
        }
    }
    None
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

pub(super) fn scrub_user_summary(summary: &str) -> String {
    // Extra safety: even if the model slips, ensure the user-facing title
    // doesn't contain file paths / line numbers / evidence markers.
    let mut s = summary.to_string();

    // Remove explicit evidence markers.
    let re_evidence = safe_regex(r"(?i)\b(evidence\s*id|evidence)\s*[:=]?\s*\d*\b");
    s = re_evidence.replace_all(&s, "").to_string();

    // Remove "(path:123)" style suffixes.
    let re_path_line = safe_regex(r"\s*\(([^)]*/[^)]*?):\d+\)");
    s = re_path_line.replace_all(&s, "").to_string();

    // Remove bare path-like tokens ("src/foo.rs", "foo.tsx", etc).
    let re_path_token =
        safe_regex(r"(?i)\b[\w./-]+\.(rs|tsx|ts|jsx|js|py|go|java|kt|cs|cpp|c|h)\b");
    s = re_path_token.replace_all(&s, "").to_string();
    let re_repo_path = safe_regex(r"(?i)\b(?:[a-z0-9_.-]+/){2,}[a-z0-9_.-]+\b");
    s = re_repo_path.replace_all(&s, "").to_string();

    // Remove formatting artifacts that commonly remain after path/token scrubbing.
    s = s.replace('`', "");
    let re_empty_parens = safe_regex(r"\(\s*[\.,;:]*\s*\)");
    s = re_empty_parens.replace_all(&s, "").to_string();

    // Collapse whitespace after removals.
    collapse_whitespace(&s)
}

pub(super) fn normalize_grounded_summary(
    summary: &str,
    detail: &str,
    _evidence_line: usize,
) -> String {
    let mut normalized = scrub_user_summary(summary);
    normalized = first_sentence_only(&normalized);

    if is_low_information_summary(&normalized) {
        if let Some(from_detail) = sentence_like_fragment(detail) {
            normalized = first_sentence_only(&from_detail);
        }
    }
    if is_low_information_summary(&normalized) {
        normalized =
            "When someone uses this flow, visible behavior can break. This matters because it can interrupt normal work."
                .to_string();
    }

    ensure_sentence_punctuation(collapse_whitespace(normalized.trim()))
}

pub(super) fn normalize_grounded_detail(detail: &str, summary: &str) -> String {
    let mut normalized = collapse_whitespace(detail);
    if normalized.len() < 40 {
        let fallback = summary.trim();
        if !fallback.is_empty() {
            normalized = format!(
                "{}. This matters because users can observe incorrect behavior when this path runs.",
                fallback
            );
        }
    }

    ensure_sentence_punctuation(normalized)
}
