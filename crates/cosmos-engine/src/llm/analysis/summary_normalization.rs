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

fn lowercase_first_ascii(text: &str) -> String {
    let mut chars = text.chars();
    let Some(first) = chars.next() else {
        return String::new();
    };
    format!(
        "{}{}",
        first.to_ascii_lowercase(),
        chars.collect::<String>()
    )
}

fn strip_summary_lead_in(summary: &str) -> String {
    let lead_re = Regex::new(r"(?i)^(fix|bug|issue|risk|warning)\s*[:\-]\s*")
        .expect("summary lead-in regex should compile");
    let mut text = lead_re
        .replace(summary.trim(), "")
        .to_string()
        .trim()
        .to_string();

    let weak_prefixes = [
        "potential ",
        "possible ",
        "maybe ",
        "might ",
        "could ",
        "likely ",
        "probably ",
    ];
    let lower = text.to_ascii_lowercase();
    for prefix in weak_prefixes {
        if lower.starts_with(prefix) {
            text = text[prefix.len()..].trim().to_string();
            break;
        }
    }

    collapse_whitespace(&text)
}

fn normalize_outcome_clause(outcome: &str) -> String {
    let core = collapse_whitespace(
        outcome
            .trim()
            .trim_end_matches(['.', '!', '?'])
            .trim_matches(',')
            .trim(),
    );
    if core.is_empty() {
        return String::new();
    }

    let lower = core.to_ascii_lowercase();
    if lower.starts_with("the action ")
        || lower.starts_with("the app ")
        || lower.starts_with("it ")
        || lower.starts_with("this ")
        || lower.starts_with("someone ")
        || lower.starts_with("users ")
        || lower.starts_with("user ")
    {
        return core;
    }

    if lower.starts_with("can ")
        || lower.starts_with("may ")
        || lower.starts_with("might ")
        || lower.starts_with("will ")
    {
        return format!("the action {}", lower);
    }

    let noun_like_outcomes = [
        "panic",
        "crash",
        "hang",
        "timeout",
        "time out",
        "failure",
        "fail",
        "fails",
        "error",
        "errors",
        "delete",
        "deletion",
        "overwrite",
        "overwrites",
        "leak",
        "expose",
        "bypass",
        "stuck",
        "drop",
        "skip",
        "duplicate",
        "corrupt",
        "lose",
    ];
    if noun_like_outcomes
        .iter()
        .any(|marker| lower.starts_with(marker))
    {
        return format!("the action can {}", lowercase_first_ascii(&core));
    }

    lowercase_first_ascii(&core)
}

fn rewrite_if_clause_to_when(summary: &str) -> Option<String> {
    let lower = summary.to_ascii_lowercase();
    let idx = lower.find(" if ")?;
    let outcome = summary[..idx].trim().trim_matches(',');
    let condition = summary[idx + 4..].trim().trim_end_matches(['.', '!', '?']);
    if outcome.is_empty() || condition.is_empty() {
        return None;
    }

    let outcome_clause = normalize_outcome_clause(outcome);
    if outcome_clause.is_empty() {
        return None;
    }

    Some(ensure_sentence_punctuation(format!(
        "When {}, {}",
        collapse_whitespace(condition),
        outcome_clause
    )))
}

fn impact_clause_for_class(impact_class: Option<&str>) -> Option<&'static str> {
    match impact_class {
        Some("correctness") => Some("which can give people incorrect results"),
        Some("reliability") => Some("which can block normal use"),
        Some("security") => Some("which can expose data or allow unsafe access"),
        Some("data_integrity") => Some("which can leave saved data in an inconsistent state"),
        _ => None,
    }
}

fn add_impact_clause(summary: &str, impact_class: Option<&str>) -> String {
    let core = collapse_whitespace(summary)
        .trim_end_matches(['.', '!', '?'])
        .trim()
        .to_string();
    if core.is_empty() {
        return String::new();
    }

    let Some(clause) = impact_clause_for_class(impact_class) else {
        return ensure_sentence_punctuation(core);
    };
    let lower = core.to_ascii_lowercase();
    if lower.contains("which can") || lower.contains(clause) || core.len() > 180 {
        return ensure_sentence_punctuation(core);
    }

    ensure_sentence_punctuation(format!("{core}, {clause}"))
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

    let path_like_re = Regex::new(
        r"(?i)\b[a-z0-9_./-]+\.(rs|ts|tsx|js|jsx|py|go|java|rb|php|c|cpp|h|hpp|cs|swift|kt|toml|json|ya?ml)\b",
    )
    .expect("path-like regex should compile");
    let source_path_re = Regex::new(r"(?i)\b(?:src|crates|apps|packages)/[a-z0-9_./-]+\b")
        .expect("source path regex should compile");
    let symbol_ref_re =
        Regex::new(r"(?i)\b[a-z_][a-z0-9_]*::[a-z0-9_:]+\b").expect("symbol regex should compile");

    let no_paths = path_like_re.replace_all(cleaned.as_ref(), "this flow");
    let no_source_paths = source_path_re.replace_all(no_paths.as_ref(), "this flow");
    let no_symbols = symbol_ref_re.replace_all(no_source_paths.as_ref(), "this code path");
    collapse_whitespace(no_symbols.trim())
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

pub(super) fn normalize_ethos_summary(
    summary: &str,
    detail: &str,
    impact_class: Option<&str>,
) -> String {
    let base = candidate_sentence(summary)
        .or_else(|| candidate_sentence(detail))
        .unwrap_or_default();
    if base.is_empty() {
        return String::new();
    }

    let stripped = strip_summary_lead_in(&base);
    if stripped.is_empty() {
        return String::new();
    }

    let lower = stripped.to_ascii_lowercase();
    let shaped = if lower.starts_with("when ") {
        ensure_sentence_punctuation(stripped)
    } else if let Some(rewritten) = rewrite_if_clause_to_when(&stripped) {
        rewritten
    } else {
        let outcome = normalize_outcome_clause(&stripped);
        if outcome.is_empty() {
            return String::new();
        }
        ensure_sentence_punctuation(format!("When someone uses this flow, {}", outcome))
    };

    let final_summary = add_impact_clause(&shaped, impact_class);
    if is_valid_grounded_summary(&final_summary) {
        final_summary
    } else {
        normalize_grounded_summary(summary, detail, 1)
    }
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
