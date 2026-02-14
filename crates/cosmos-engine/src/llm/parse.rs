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

/// Extract a balanced JSON fragment between matching delimiters
/// Properly handles nested structures and ignores delimiters inside strings
fn extract_json_fragment(text: &str, open: char, close: char) -> Option<&str> {
    let mut depth = 0;
    let mut in_string = false;
    let mut escape_next = false;
    let mut start_idx = None;

    for (i, c) in text.char_indices() {
        if escape_next {
            escape_next = false;
            continue;
        }

        if c == '\\' && in_string {
            escape_next = true;
            continue;
        }

        if c == '"' {
            in_string = !in_string;
            continue;
        }

        if in_string {
            continue;
        }

        if c == open {
            if depth == 0 {
                start_idx = Some(i);
            }
            depth += 1;
        } else if c == close {
            depth -= 1;
            if depth == 0 {
                if let Some(start) = start_idx {
                    return Some(&text[start..=i]);
                }
            }
        }
    }

    None
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

/// Extract JSON from LLM response, handling markdown fences and noise
fn extract_json_object(response: &str) -> Option<&str> {
    let clean = strip_markdown_fences(response);
    extract_json_fragment(clean, '{', '}')
}

/// Normalize a path string to repo-relative format (wrapper around cache::normalize_summary_path)
fn normalize_path_str(raw: &str, root: &Path) -> PathBuf {
    cosmos_adapters::cache::normalize_summary_path(&PathBuf::from(raw.trim()), root)
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
                if let Ok(parsed) = serde_json::from_value::<HashMap<String, String>>(inner.clone())
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

    // Final fallback: provide helpful error without dumping raw JSON
    Err(anyhow::anyhow!(
        "Summary response format unexpected. The AI response may have been truncated or malformed."
    ))
}

/// Parse response containing both summaries and domain terms
#[derive(Debug)]
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
        .ok_or_else(|| anyhow::anyhow!("Summary response missing JSON structure"))?;
    let json_str = fix_json_issues(json_str);

    // Try to parse as the expected format: {summaries: {...}, terms: {...}}
    if let Ok(wrapper) = serde_json::from_str::<serde_json::Value>(&json_str) {
        let mut summaries = HashMap::new();
        let mut terms = HashMap::new();
        let mut terms_by_file = HashMap::new();

        // Extract summaries - handle both string values and other types
        if let Some(summaries_obj) = wrapper.get("summaries") {
            // First try direct parse as HashMap<String, String>
            if let Ok(parsed) =
                serde_json::from_value::<HashMap<String, String>>(summaries_obj.clone())
            {
                summaries = parsed
                    .into_iter()
                    .map(|(path, summary)| (normalize_path_str(&path, root), summary))
                    .collect();
            } else if let Some(obj) = summaries_obj.as_object() {
                // Fallback: manually extract values, converting non-strings to string repr
                for (path, value) in obj {
                    let summary = match value {
                        serde_json::Value::String(s) => s.clone(),
                        serde_json::Value::Null => continue, // Skip null values
                        other => {
                            // Convert objects/arrays/etc to compact string representation
                            // This handles cases where LLM returns nested objects
                            other.to_string()
                        }
                    };
                    if !summary.is_empty() {
                        summaries.insert(normalize_path_str(path, root), summary);
                    }
                }
            }
        }

        // Extract terms - same robust handling
        if let Some(terms_obj) = wrapper.get("terms") {
            if let Ok(parsed) = serde_json::from_value::<HashMap<String, String>>(terms_obj.clone())
            {
                terms = parsed;
            } else if let Some(obj) = terms_obj.as_object() {
                for (term, value) in obj {
                    if let serde_json::Value::String(def) = value {
                        terms.insert(term.clone(), def.clone());
                    }
                }
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

    // ═══════════════════════════════════════════════════════════════════════════
    //  SUMMARY PARSING ROBUSTNESS TESTS
    // ═══════════════════════════════════════════════════════════════════════════

    #[test]
    fn test_parse_summaries_valid_structure() {
        let json = r#"{"summaries": {"src/main.rs": "Entry point", "src/lib.rs": "Library"}}"#;
        let root = std::path::Path::new("/project");
        let result = parse_summaries_and_terms_response(json, root).unwrap();
        assert_eq!(result.summaries.len(), 2);
        assert!(result.summaries.values().any(|v| v == "Entry point"));
    }

    #[test]
    fn test_parse_summaries_with_terms() {
        let json = r#"{
            "summaries": {"src/main.rs": "Entry point"},
            "terms": {"Widget": "A UI component"}
        }"#;
        let root = std::path::Path::new("/project");
        let result = parse_summaries_and_terms_response(json, root).unwrap();
        assert_eq!(result.summaries.len(), 1);
        assert_eq!(
            result.terms.get("Widget"),
            Some(&"A UI component".to_string())
        );
    }

    #[test]
    fn test_parse_summaries_handles_nested_objects() {
        // LLM sometimes returns objects instead of strings - we should handle gracefully
        let json =
            r#"{"summaries": {"src/main.rs": {"description": "Entry point", "role": "main"}}}"#;
        let root = std::path::Path::new("/project");
        let result = parse_summaries_and_terms_response(json, root).unwrap();
        // Should convert the object to a string representation rather than fail
        assert_eq!(result.summaries.len(), 1);
    }

    #[test]
    fn test_parse_summaries_handles_null_values() {
        // Null values should be skipped, not cause failure
        let json = r#"{"summaries": {"src/main.rs": "Entry point", "src/empty.rs": null}}"#;
        let root = std::path::Path::new("/project");
        let result = parse_summaries_and_terms_response(json, root).unwrap();
        assert_eq!(result.summaries.len(), 1);
    }

    #[test]
    fn test_parse_summaries_handles_markdown_fences() {
        let json = "```json\n{\"summaries\": {\"src/main.rs\": \"Entry point\"}}\n```";
        let root = std::path::Path::new("/project");
        let result = parse_summaries_and_terms_response(json, root).unwrap();
        assert_eq!(result.summaries.len(), 1);
    }

    #[test]
    fn test_parse_summaries_error_is_user_friendly() {
        // Error messages should not dump raw JSON
        let bad_json = "this is not json at all";
        let root = std::path::Path::new("/project");
        let err = parse_summaries_and_terms_response(bad_json, root).unwrap_err();
        let err_msg = err.to_string();
        // Should be a clean message, not contain the raw input
        assert!(!err_msg.contains("this is not json"));
        assert!(err_msg.contains("JSON") || err_msg.contains("response"));
    }

    #[test]
    fn test_parse_summaries_fallback_to_simple_format() {
        // If no "summaries" key, try parsing as direct {path: summary} object
        let json = r#"{"src/main.rs": "Entry point", "src/lib.rs": "Library"}"#;
        let root = std::path::Path::new("/project");
        let result = parse_summaries_and_terms_response(json, root).unwrap();
        assert_eq!(result.summaries.len(), 2);
    }

    #[test]
    fn test_parse_summaries_handles_trailing_commas() {
        // Common LLM mistake - trailing commas
        let json = r#"{"summaries": {"src/main.rs": "Entry point",}}"#;
        let root = std::path::Path::new("/project");
        let result = parse_summaries_and_terms_response(json, root).unwrap();
        assert_eq!(result.summaries.len(), 1);
    }

    #[test]
    fn test_parse_summaries_empty_response_returns_empty() {
        // Empty object returns empty results rather than failing - graceful handling
        let json = "{}";
        let root = std::path::Path::new("/project");
        let result = parse_summaries_and_terms_response(json, root).unwrap();
        assert!(result.summaries.is_empty());
        assert!(result.terms.is_empty());
    }
}
