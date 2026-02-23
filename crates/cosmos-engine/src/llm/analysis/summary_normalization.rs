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

fn uppercase_first_ascii(text: &str) -> String {
    let mut chars = text.chars();
    let Some(first) = chars.next() else {
        return String::new();
    };
    format!(
        "{}{}",
        first.to_ascii_uppercase(),
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

fn replace_case_insensitive(text: &str, pattern: &str, replacement: &str) -> String {
    Regex::new(pattern)
        .expect("replacement regex should compile")
        .replace_all(text, replacement)
        .to_string()
}

fn rewrite_crash_condition_sentence(text: &str) -> Option<String> {
    let trimmed = text.trim().trim_end_matches(['.', '!', '?']);
    if trimmed.is_empty() {
        return None;
    }

    let crash_if_re = Regex::new(
        r"(?i)^(?:[a-z][a-z0-9_ ]*?\s+)?(?:may\s+)?(?:crash(?:es)?|panic(?:s|ed|ing)?)\s+if\s+(.+)$",
    )
    .expect("crash-if regex should compile");
    if let Some(caps) = crash_if_re.captures(trimmed) {
        let condition = caps.get(1).map(|m| m.as_str().trim()).unwrap_or("");
        if !condition.is_empty() {
            return Some(format!(
                "If {}, this action can crash instead of showing a clear error",
                condition
            ));
        }
    }

    let crash_when_re = Regex::new(
        r"(?i)^(?:[a-z][a-z0-9_ ]*?\s+)?(?:may\s+)?(?:crash(?:es)?|panic(?:s|ed|ing)?)\s+when\s+(.+)$",
    )
    .expect("crash-when regex should compile");
    if let Some(caps) = crash_when_re.captures(trimmed) {
        let condition = caps.get(1).map(|m| m.as_str().trim()).unwrap_or("");
        if !condition.is_empty() {
            return Some(format!(
                "If {}, this action can crash instead of showing a clear error",
                condition
            ));
        }
    }

    None
}

fn rewrite_assumes_non_empty_sentence(text: &str) -> Option<String> {
    let trimmed = text.trim().trim_end_matches(['.', '!', '?']);
    if trimmed.is_empty() {
        return None;
    }

    let assumes_re = Regex::new(r"(?i)^assumes (.+?) is non[- ]empty when (.+)$")
        .expect("assumes regex should compile");
    let caps = assumes_re.captures(trimmed)?;
    let subject = caps.get(1).map(|m| m.as_str().trim()).unwrap_or("");
    let action = caps.get(2).map(|m| m.as_str().trim()).unwrap_or("");
    if subject.is_empty() || action.is_empty() {
        return None;
    }
    Some(format!("If {} is missing, {} can fail", subject, action))
}

fn soften_technical_summary_language(summary: &str) -> String {
    let mut text = collapse_whitespace(summary.trim());
    if text.is_empty() {
        return String::new();
    }

    text = replace_case_insensitive(&text, r"(?i)\bnon[- ]zero exit status\b", "an error");
    text = replace_case_insensitive(&text, r"(?i)\bunix[_-]?epoch\b", "1970");
    text = replace_case_insensitive(&text, r"(?i)\bsystemtime\b", "system clock");
    text = replace_case_insensitive(&text, r"(?i)\bfilesystem\b", "file system");
    text = replace_case_insensitive(&text, r"(?i)\bunwrap\b", "unchecked value handling");
    text = replace_case_insensitive(&text, r"\bHEAD\b", "current branch");
    text = replace_case_insensitive(&text, r"(?i)\bshorthand\b", "branch name");
    text = replace_case_insensitive(&text, r"\bNone\b", "not available");
    text = replace_case_insensitive(&text, r"(?i)\bsymbol name\b", "component name");
    text = replace_case_insensitive(&text, r"(?i)\bpanic(?:s|ed|ing)?\b", "crash");

    if let Some(rewritten) = rewrite_crash_condition_sentence(&text) {
        text = rewritten;
    } else if let Some(rewritten) = rewrite_assumes_non_empty_sentence(&text) {
        text = rewritten;
    }

    collapse_whitespace(text.trim().trim_matches(','))
}

fn impact_clause_for_class(impact_class: Option<&str>) -> Option<&'static str> {
    match impact_class {
        Some("correctness") => Some("This can lead to incorrect results."),
        Some("reliability") => Some("This can interrupt normal use."),
        Some("security") => Some("This can expose data or allow unsafe access."),
        Some("data_integrity") => Some("This can leave saved data in an inconsistent state."),
        _ => None,
    }
}

fn summary_already_mentions_impact(lower: &str) -> bool {
    [
        " can ",
        " may ",
        " might ",
        " will ",
        " fails",
        " fail ",
        " failed",
        " crash",
        " panic",
        " timeout",
        " times out",
        " error",
        " incorrect",
        " wrong",
        " blocked",
        " expose",
        " unsafe",
        " inconsistent",
        " stuck",
        " hang",
    ]
    .iter()
    .any(|marker| lower.contains(marker))
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
    if summary_already_mentions_impact(&format!(" {} ", lower))
        || lower.contains(&clause.to_ascii_lowercase())
        || core.len() > 180
    {
        return ensure_sentence_punctuation(core);
    }

    format!("{} {}", ensure_sentence_punctuation(core), clause)
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

    let softened = soften_technical_summary_language(&stripped);
    if softened.is_empty() {
        return String::new();
    }

    let shaped = ensure_sentence_punctuation(uppercase_first_ascii(&softened));

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
