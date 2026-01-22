use super::client::{call_llm_with_usage, truncate_str, LlmResponse};
use super::models::{Model, Usage};
use crate::suggest::{Priority, Suggestion, SuggestionKind, SuggestionSource};
use serde::Deserialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Strip markdown code fences from a response
fn strip_markdown_fences(text: &str) -> &str {
    let trimmed = text.trim();
    let clean = if trimmed.starts_with("```json") {
        trimmed.strip_prefix("```json").unwrap_or(trimmed)
    } else if trimmed.starts_with("```") {
        trimmed.strip_prefix("```").unwrap_or(trimmed)
    } else {
        trimmed
    };
    let clean = if clean.ends_with("```") {
        clean.strip_suffix("```").unwrap_or(clean)
    } else {
        clean
    };
    clean.trim()
}

/// Extract a JSON fragment between matching delimiters
fn extract_json_fragment<'a>(text: &'a str, open: char, close: char) -> Option<&'a str> {
    let start = text.find(open)?;
    let end = text.rfind(close)?;
    if start <= end {
        Some(&text[start..=end])
    } else {
        None
    }
}

/// Parse suggestions from codebase-wide analysis
pub(crate) fn parse_codebase_suggestions(response: &str) -> anyhow::Result<Vec<Suggestion>> {
    let clean = strip_markdown_fences(response);
    let sanitized = fix_json_issues(clean);

    // Handle both array format and object-with-suggestions format
    // Speed preset often returns {"suggestions": [...]} instead of just [...]
    let json_str = if let Some(obj_str) = extract_json_fragment(&sanitized, '{', '}') {
        // Try to extract "suggestions" array from object
        if let Ok(obj) = serde_json::from_str::<serde_json::Value>(obj_str) {
            if let Some(suggestions) = obj.get("suggestions") {
                // Convert suggestions array back to string for parsing
                serde_json::to_string(suggestions).unwrap_or_else(|_| obj_str.to_string())
            } else {
                obj_str.to_string()
            }
        } else {
            obj_str.to_string()
        }
    } else if let Some(array_str) = extract_json_fragment(&sanitized, '[', ']') {
        array_str.to_string()
    } else {
        sanitized
    };

    // Try to parse as array first
    let parsed: Vec<CodebaseSuggestionJson> = match serde_json::from_str(&json_str) {
        Ok(v) => v,
        Err(e) => {
            // Try to fix common JSON issues and retry
            let fixed = fix_json_issues(&json_str);
            match serde_json::from_str(&fixed) {
                Ok(v) => v,
                Err(_) => {
                    // If still failing, try to extract individual objects and parse them
                    match try_parse_individual_suggestions(&json_str) {
                        Ok(v) if !v.is_empty() => v,
                        _ => {
                            let preview = truncate_str(&json_str, 200);
                            return Err(anyhow::anyhow!(
                                "Suggestions could not be parsed ({}). Try regenerating. Response preview: {}",
                                e,
                                preview
                            ));
                        }
                    }
                }
            }
        }
    };

    let suggestions = parsed
        .into_iter()
        .map(|s| {
            let kind = match s.kind.as_str() {
                "bugfix" => SuggestionKind::BugFix,
                "feature" => SuggestionKind::Feature,
                "optimization" => SuggestionKind::Optimization,
                "quality" => SuggestionKind::Quality,
                "documentation" => SuggestionKind::Documentation,
                "testing" => SuggestionKind::Testing,
                "refactoring" => SuggestionKind::Refactoring,
                _ => SuggestionKind::Improvement,
            };

            let priority = match s.priority.as_str() {
                "high" => Priority::High,
                "low" => Priority::Low,
                _ => Priority::Medium,
            };

            let file_path = PathBuf::from(&s.file);
            let additional_files: Vec<PathBuf> =
                s.additional_files.into_iter().map(PathBuf::from).collect();

            let mut suggestion = Suggestion::new(
                kind,
                priority,
                file_path,
                s.summary,
                SuggestionSource::LlmDeep,
            )
            .with_detail(s.detail)
            .with_additional_files(additional_files);

            if let Some(line) = s.line {
                suggestion = suggestion.with_line(line);
            }

            suggestion
        })
        .collect();

    Ok(suggestions)
}

#[derive(Deserialize)]
struct CodebaseSuggestionJson {
    file: String,
    #[serde(default)]
    additional_files: Vec<String>,
    #[serde(default)]
    kind: String,
    #[serde(default)]
    priority: String,
    summary: String,
    #[serde(default)]
    detail: String,
    line: Option<usize>,
}

/// Try to fix common JSON issues from LLM responses
fn fix_json_issues(json: &str) -> String {
    let mut fixed = json.to_string();

    // Remove trailing commas before ] or }
    fixed = fixed.replace(",]", "]");
    fixed = fixed.replace(",}", "}");

    // Fix common quote issues - smart quotes to regular quotes
    fixed = fixed.replace('\u{201C}', "\""); // Left double quote
    fixed = fixed.replace('\u{201D}', "\""); // Right double quote
    fixed = fixed.replace('\u{2018}', "'"); // Left single quote
    fixed = fixed.replace('\u{2019}', "'"); // Right single quote

    // Remove any control characters that might have slipped in
    fixed = fixed
        .chars()
        .filter(|c| !c.is_control() || *c == '\n' || *c == '\t')
        .collect();

    fixed
}

/// Try to parse individual suggestion objects if array parsing fails
fn try_parse_individual_suggestions(json: &str) -> anyhow::Result<Vec<CodebaseSuggestionJson>> {
    let mut suggestions = Vec::new();
    let mut depth: i32 = 0;
    let mut start = None;

    for (i, c) in json.char_indices() {
        match c {
            '{' => {
                if depth == 0 {
                    start = Some(i);
                }
                depth += 1;
            }
            '}' => {
                if depth == 0 {
                    continue;
                }
                depth -= 1;
                if depth == 0 {
                    if let Some(s) = start {
                        let obj_str = &json[s..=i];
                        if let Ok(suggestion) =
                            serde_json::from_str::<CodebaseSuggestionJson>(obj_str)
                        {
                            suggestions.push(suggestion);
                        }
                    }
                    start = None;
                }
            }
            _ => {}
        }
    }

    Ok(suggestions)
}

/// Truncate file contents for prompt safety (keep beginning + end)
pub(crate) fn truncate_content(content: &str, max_chars: usize) -> String {
    if content.chars().count() <= max_chars {
        content.to_string()
    } else {
        let head: String = content.chars().take(max_chars / 2).collect();
        let tail: String = content.chars().rev().take(max_chars / 2).collect::<String>();
        format!("{}\n\n... [truncated] ...\n\n{}", head, tail.chars().rev().collect::<String>())
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

/// Extract JSON from LLM response, handling markdown fences and noise
fn extract_json_object(response: &str) -> Option<&str> {
    let clean = strip_markdown_fences(response);
    extract_json_fragment(clean, '{', '}')
}

// ═══════════════════════════════════════════════════════════════════════════
//  JSON SELF-CORRECTION
// ═══════════════════════════════════════════════════════════════════════════

/// Parse JSON from LLM response with automatic self-correction on failure.
///
/// If the initial parse fails, asks the LLM to fix its own JSON output.
/// Uses the Speed model for corrections since it's a simple task.
/// Returns the parsed value and any additional usage from the correction call.
pub(crate) async fn parse_json_with_retry<T>(
    response: &str,
    context_hint: &str,
) -> anyhow::Result<(T, Option<Usage>)>
where
    T: serde::de::DeserializeOwned,
{
    // Extract JSON from response
    let json_str = extract_json_object(response)
        .ok_or_else(|| anyhow::anyhow!("No JSON object found in {} response", context_hint))?;

    // First attempt
    match serde_json::from_str::<T>(json_str) {
        Ok(parsed) => Ok((parsed, None)),
        Err(initial_error) => {
            let fixed_json = fix_json_issues(json_str);
            if fixed_json != json_str {
                if let Ok(parsed) = serde_json::from_str::<T>(&fixed_json) {
                    return Ok((parsed, None));
                }
            }
            // Self-correction: ask LLM to fix its own JSON
            let correction_response =
                request_json_correction(response, &initial_error, context_hint).await?;

            // Extract and parse the corrected JSON
            let corrected_json = extract_json_object(&correction_response.content).ok_or_else(
                || anyhow::anyhow!("No JSON found in correction response for {}", context_hint),
            )?;

            let corrected_json = fix_json_issues(corrected_json);
            let parsed = serde_json::from_str::<T>(&corrected_json).map_err(|e| {
                anyhow::anyhow!(
                    "JSON still invalid after self-correction for {}: {}\nOriginal error: {}",
                    context_hint,
                    e,
                    initial_error
                )
            })?;

            Ok((parsed, correction_response.usage))
        }
    }
}

/// Ask the LLM to fix malformed JSON from a previous response.
/// Uses Speed model since JSON correction is a simple task.
pub(crate) async fn request_json_correction(
    original_response: &str,
    parse_error: &serde_json::Error,
    context_hint: &str,
) -> anyhow::Result<LlmResponse> {
    request_json_correction_generic(original_response, &parse_error.to_string(), context_hint)
        .await
}

/// Ask the LLM to fix malformed JSON (generic version taking error as string).
pub(crate) async fn request_json_correction_generic(
    original_response: &str,
    error_message: &str,
    context_hint: &str,
) -> anyhow::Result<LlmResponse> {
    let system = r#"You are a JSON repair assistant. Your ONLY job is to fix malformed JSON.

RULES:
- Output ONLY the corrected JSON, nothing else
- No explanations, no markdown fences, no commentary
- Preserve all the original data and structure
- Fix syntax errors: missing commas, unclosed brackets, invalid escapes
- Ensure strings are properly quoted and escaped
- Ensure the JSON is complete (not truncated)"#;

    let user = format!(
        "The following {} response contains invalid JSON.\n\n\
         Parse error: {}\n\n\
         Original response:\n{}\n\n\
         Output ONLY the corrected, valid JSON:",
        context_hint,
        error_message,
        truncate_str(original_response, 4000) // Limit size for correction prompt
    );

    call_llm_with_usage(system, &user, Model::Speed, true).await
}

/// Merge two optional Usage values, summing their token counts and costs
pub(crate) fn merge_usage(primary: Option<Usage>, secondary: Option<Usage>) -> Option<Usage> {
    match (primary, secondary) {
        (Some(p), Some(s)) => Some(Usage {
            prompt_tokens: p.prompt_tokens + s.prompt_tokens,
            completion_tokens: p.completion_tokens + s.completion_tokens,
            total_tokens: p.total_tokens + s.total_tokens,
            cost: match (p.cost, s.cost) {
                (Some(pc), Some(sc)) => Some(pc + sc),
                (Some(pc), None) => Some(pc),
                (None, Some(sc)) => Some(sc),
                (None, None) => None,
            },
        }),
        (Some(p), None) => Some(p),
        (None, Some(s)) => Some(s),
        (None, None) => None,
    }
}

/// Normalize a path string to repo-relative format (wrapper around cache::normalize_summary_path)
fn normalize_path_str(raw: &str, root: &Path) -> PathBuf {
    crate::cache::normalize_summary_path(&PathBuf::from(raw.trim()), root)
}

pub(crate) fn parse_summaries_response(
    response: &str,
    root: &Path,
) -> anyhow::Result<HashMap<PathBuf, String>> {
    let json_str = extract_json_object(response)
        .ok_or_else(|| anyhow::anyhow!("No JSON object found in response"))?;
    let json_str = fix_json_issues(json_str);

    // First try to parse as a simple {path: summary} object
    if let Ok(parsed) = serde_json::from_str::<HashMap<String, String>>(&json_str) {
        let summaries = parsed
            .into_iter()
            .map(|(path, summary)| (normalize_path_str(&path, root), summary))
            .collect();
        return Ok(summaries);
    }

    // Try to parse as a wrapper object (e.g., {"analysis": {...}, "summaries": {...}})
    if let Ok(wrapper) = serde_json::from_str::<serde_json::Value>(&json_str) {
        // Look for common wrapper keys that might contain summaries
        for key in ["summaries", "files", "results", "data"] {
            if let Some(inner) = wrapper.get(key) {
                if let Ok(parsed) =
                    serde_json::from_value::<HashMap<String, String>>(inner.clone())
                {
                    let summaries = parsed
                        .into_iter()
                        .map(|(path, summary)| (normalize_path_str(&path, root), summary))
                        .collect();
                    return Ok(summaries);
                }
            }
        }

        // If the wrapper is an object, try to extract string values directly
        // (handles case where LLM adds extra keys like "analysis" alongside file paths)
        if let Some(obj) = wrapper.as_object() {
            let mut summaries = HashMap::new();
            for (key, value) in obj {
                // Skip meta keys that aren't file paths
                if key == "analysis" || key == "notes" || key == "summary" {
                    continue;
                }
                if let Some(summary) = value.as_str() {
                    summaries.insert(normalize_path_str(key, root), summary.to_string());
                }
            }
            if !summaries.is_empty() {
                return Ok(summaries);
            }
        }
    }

    // Final fallback: provide helpful error
    let preview = if json_str.len() > 200 {
        format!("{}...", &json_str[..200])
    } else {
        json_str.to_string()
    };
    Err(anyhow::anyhow!(
        "Could not extract summaries from response. Preview: {}",
        preview
    ))
}

/// Parse response containing both summaries and domain terms
pub(crate) struct SummariesAndTerms {
    pub summaries: HashMap<PathBuf, String>,
    pub terms: HashMap<String, String>,
    pub terms_by_file: HashMap<PathBuf, HashMap<String, String>>,
}

pub(crate) fn parse_summaries_and_terms_response(
    response: &str,
    root: &Path,
) -> anyhow::Result<SummariesAndTerms> {
    let json_str = extract_json_object(response)
        .ok_or_else(|| anyhow::anyhow!("No JSON object found in response"))?;
    let json_str = fix_json_issues(json_str);

    // Try to parse as the expected format: {summaries: {...}, terms: {...}}
    if let Ok(wrapper) = serde_json::from_str::<serde_json::Value>(&json_str) {
        let mut summaries = HashMap::new();
        let mut terms = HashMap::new();
        let mut terms_by_file = HashMap::new();

        // Extract summaries
        if let Some(summaries_obj) = wrapper.get("summaries") {
            if let Ok(parsed) =
                serde_json::from_value::<HashMap<String, String>>(summaries_obj.clone())
            {
                summaries = parsed
                    .into_iter()
                    .map(|(path, summary)| (normalize_path_str(&path, root), summary))
                    .collect();
            }
        }

        // Extract terms
        if let Some(terms_obj) = wrapper.get("terms") {
            if let Ok(parsed) =
                serde_json::from_value::<HashMap<String, String>>(terms_obj.clone())
            {
                terms = parsed;
            }
        }

        // Extract terms by file (preferred mapping)
        if let Some(terms_by_file_obj) = wrapper
            .get("terms_by_file")
            .or_else(|| wrapper.get("termsByFile"))
        {
            if let Some(obj) = terms_by_file_obj.as_object() {
                for (path, term_map) in obj {
                    if let Ok(parsed) =
                        serde_json::from_value::<HashMap<String, String>>(term_map.clone())
                    {
                        if !parsed.is_empty() {
                            terms_by_file.insert(normalize_path_str(path, root), parsed);
                        }
                    }
                }
            }
        }

        if !summaries.is_empty() || !terms.is_empty() || !terms_by_file.is_empty() {
            return Ok(SummariesAndTerms {
                summaries,
                terms,
                terms_by_file,
            });
        }
    }

    // Fallback: try to parse as simple summaries object
    let summaries = parse_summaries_response(response, root)?;
    Ok(SummariesAndTerms {
        summaries,
        terms: HashMap::new(),
        terms_by_file: HashMap::new(),
    })
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
    fn test_parse_individual_suggestions_ignores_unmatched_braces() {
        let json = "}\n{\"file\":\"src/lib.rs\",\"kind\":\"bugfix\",\"priority\":\"high\",\"summary\":\"Issue\",\"detail\":\"Details\"}";
        let parsed = try_parse_individual_suggestions(json).unwrap();
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].file, "src/lib.rs");
    }

    #[test]
    fn test_parse_codebase_suggestions_missing_detail() {
        let json = r#"[{"file":"src/lib.rs","kind":"bugfix","priority":"low","summary":"Issue"}]"#;
        let parsed = parse_codebase_suggestions(json).unwrap();
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].summary, "Issue");
    }
}
