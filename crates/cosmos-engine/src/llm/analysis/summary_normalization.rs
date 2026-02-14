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
    let normalized_lower = lower.trim_end_matches(['.', '!', '?']);
    let has_vague_hidden_errors = normalized_lower.ends_with("hidden errors")
        || normalized_lower.starts_with("hidden errors")
        || normalized_lower.contains("hidden errors when")
        || normalized_lower.contains(", hidden errors")
        || normalized_lower.contains(" hidden errors,");
    lower == "when users"
        || lower == "when someone"
        || lower == "when a user"
        || lower.starts_with("when ")
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

fn strip_formulaic_impact_clause(text: &str) -> String {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return String::new();
    }

    let lower = trimmed.to_ascii_lowercase();
    if let Some(idx) = lower.find("this matters because") {
        return trimmed[..idx].trim().to_string();
    }
    trimmed.to_string()
}

fn capitalize_first(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => format!("{}{}", first.to_uppercase(), chars.as_str()),
        None => String::new(),
    }
}

fn lowercase_first(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => format!("{}{}", first.to_lowercase(), chars.as_str()),
        None => String::new(),
    }
}

fn rewrite_when_lead_to_plain_sentence(summary: &str) -> String {
    let trimmed = summary.trim();
    if !trimmed.to_ascii_lowercase().starts_with("when ") {
        return trimmed.to_string();
    }

    let Some(comma_idx) = trimmed.find(',') else {
        return trimmed.to_string();
    };

    let condition = trimmed[5..comma_idx]
        .trim()
        .trim_end_matches(['.', '!', '?']);
    let outcome = trimmed[comma_idx + 1..]
        .trim()
        .trim_start_matches("then ")
        .trim()
        .trim_end_matches(['.', '!', '?']);

    if condition.is_empty() || outcome.is_empty() {
        return trimmed.to_string();
    }

    let outcome = capitalize_first(outcome);
    let condition = lowercase_first(condition);
    format!("{outcome} when {condition}.")
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

    // Collapse whitespace after removals.
    collapse_whitespace(&s)
}

pub(super) fn normalize_grounded_summary(
    summary: &str,
    detail: &str,
    _evidence_line: usize,
) -> String {
    let mut normalized = scrub_user_summary(summary);
    normalized = strip_formulaic_impact_clause(&normalized);
    normalized = first_sentence_only(&normalized);
    normalized = rewrite_when_lead_to_plain_sentence(&normalized);

    if is_low_information_summary(&normalized) {
        if let Some(from_detail) = sentence_like_fragment(detail) {
            let mut detail_sentence = strip_formulaic_impact_clause(&from_detail);
            detail_sentence = first_sentence_only(&detail_sentence);
            normalized = rewrite_when_lead_to_plain_sentence(&detail_sentence);
        }
    }
    if is_low_information_summary(&normalized) {
        normalized =
            "A user-facing reliability issue can cause visible broken behavior in this flow"
                .to_string();
    }

    ensure_sentence_punctuation(normalized.trim().to_string())
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
