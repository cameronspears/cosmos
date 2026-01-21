use super::client::{call_llm, call_llm_with_usage};
use super::models::{Model, Usage};
use super::parse::{merge_usage, parse_json_with_retry, request_json_correction_generic};
use super::prompts::{FIX_CONTENT_SYSTEM, FIX_PREVIEW_SYSTEM, MULTI_FILE_FIX_SYSTEM};
use crate::suggest::Suggestion;
use serde::Deserialize;
use std::path::PathBuf;

// ═══════════════════════════════════════════════════════════════════════════
//  DIRECT CODE GENERATION (Human plan → Smart model applies changes)
// ═══════════════════════════════════════════════════════════════════════════

/// Result of generating and applying a fix
#[derive(Debug, Clone)]
pub struct AppliedFix {
    /// Human-readable description of what was changed
    pub description: String,
    /// The new file content (to be written directly)
    pub new_content: String,
    /// Which functions/areas were modified
    pub modified_areas: Vec<String>,
    /// Usage stats
    pub usage: Option<Usage>,
}

/// A single search/replace edit operation
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct EditOp {
    /// The exact text to find (must match exactly once in the file)
    pub(crate) old_string: String,
    /// The replacement text
    pub(crate) new_string: String,
}

/// Response structure for fix generation (used for JSON parsing with retry)
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct FixResponse {
    #[serde(default)]
    pub(crate) description: Option<String>,
    #[serde(default)]
    pub(crate) modified_areas: Vec<String>,
    pub(crate) edits: Vec<EditOp>,
}

/// Generate the actual fixed code content based on a human-language plan.
/// Uses a search/replace approach for precise, validated edits.
/// This is Phase 2 of the two-phase fix flow - Smart preset generates the actual changes
pub async fn generate_fix_content(
    path: &PathBuf,
    content: &str,
    suggestion: &Suggestion,
    plan: &FixPreview,
    repo_memory: Option<String>,
) -> anyhow::Result<AppliedFix> {
    let plan_text = format!(
        "Verification: {} - {}\nPlan: {}\nScope: {}\nAffected areas: {}{}",
        if plan.verified { "CONFIRMED" } else { "UNCONFIRMED" },
        plan.verification_note,
        plan.description,
        plan.scope.label(),
        plan.affected_areas.join(", "),
        plan.modifier
            .as_ref()
            .map(|m| format!("\nUser modifications: {}", m))
            .unwrap_or_default()
    );

    let memory_section = repo_memory
        .as_deref()
        .filter(|m| !m.trim().is_empty())
        .map(|m| format!("\n\nRepo conventions / decisions:\n{}", m))
        .unwrap_or_default();

    let user = format!(
        "File: {}\n\nOriginal Issue: {}\n{}\n{}\n\n{}\n\nCurrent Code:\n```\n{}\n```\n\nImplement the fix using search/replace edits. Be precise with old_string - it must match exactly.",
        path.display(),
        suggestion.summary,
        suggestion.detail.as_deref().unwrap_or(""),
        memory_section,
        plan_text,
        content
    );

    let response = call_llm_with_usage(FIX_CONTENT_SYSTEM, &user, Model::Smart, true).await?;

    // Parse the JSON response with self-correction on failure
    let (parsed, correction_usage): (FixResponse, _) =
        parse_json_with_retry(&response.content, "fix generation").await?;

    // Merge usage from correction call if any
    let total_usage = merge_usage(response.usage, correction_usage);

    let description = parsed
        .description
        .unwrap_or_else(|| "Applied the requested fix".to_string());
    let modified_areas = parsed.modified_areas;
    let edits = parsed.edits;

    if edits.is_empty() {
        return Err(anyhow::anyhow!("No edits provided in response"));
    }

    // Apply edits sequentially with validation
    let mut new_content = content.to_string();
    for (i, edit) in edits.iter().enumerate() {
        // Validate old_string exists exactly once
        let matches: Vec<_> = new_content.match_indices(&edit.old_string).collect();

        if matches.is_empty() {
            return Err(anyhow::anyhow!(
                "Edit {}: old_string not found in file. The LLM may have made an error.\nSearched for: {:?}",
                i + 1,
                truncate_for_error(&edit.old_string)
            ));
        }

        if matches.len() > 1 {
            return Err(anyhow::anyhow!(
                "Edit {}: old_string matches {} times (must be unique). Need more context.\nSearched for: {:?}",
                i + 1,
                matches.len(),
                truncate_for_error(&edit.old_string)
            ));
        }

        // Apply the replacement
        new_content = new_content.replacen(&edit.old_string, &edit.new_string, 1);
    }

    // Strip trailing whitespace from each line and ensure file ends with newline
    let mut new_content: String = new_content
        .lines()
        .map(|line| line.trim_end())
        .collect::<Vec<_>>()
        .join("\n");
    if !new_content.ends_with('\n') {
        new_content.push('\n');
    }

    // Validate the new content isn't empty
    if new_content.trim().is_empty() {
        return Err(anyhow::anyhow!("Generated content is empty"));
    }

    Ok(AppliedFix {
        description,
        new_content,
        modified_areas,
        usage: total_usage,
    })
}

/// Truncate a string for error messages (UTF-8 safe)
pub(crate) fn truncate_for_error(s: &str) -> String {
    const MAX_CHARS: usize = 100;
    // Use char iteration to avoid panicking on multi-byte UTF-8 boundaries
    // (same pattern as hash_summary in history.rs)
    if s.chars().count() <= MAX_CHARS {
        s.to_string()
    } else {
        format!("{}...", s.chars().take(MAX_CHARS).collect::<String>())
    }
}

// ═══════════════════════════════════════════════════════════════════════════
//  MULTI-FILE FIX GENERATION
// ═══════════════════════════════════════════════════════════════════════════

/// A single file's edit within a multi-file fix
#[derive(Debug, Clone)]
pub struct FileEdit {
    pub path: PathBuf,
    pub new_content: String,
    pub modified_areas: Vec<String>,
}

/// Result of generating a multi-file fix
#[derive(Debug, Clone)]
pub struct MultiFileAppliedFix {
    /// Human-readable description of what was changed
    pub description: String,
    /// Edits for each file
    pub file_edits: Vec<FileEdit>,
    /// Usage stats
    pub usage: Option<Usage>,
}

/// Edits for a single file in the JSON response
#[derive(Debug, Clone, Deserialize)]
struct FileEditsJson {
    file: String,
    edits: Vec<EditOp>,
}

/// Response structure for multi-file fix generation
#[derive(Debug, Clone, Deserialize)]
struct MultiFileFixResponse {
    #[serde(default)]
    description: Option<String>,
    file_edits: Vec<FileEditsJson>,
}

/// Generate coordinated fixes across multiple files
///
/// This function handles multi-file refactors like:
/// - Renaming a function and updating all callers
/// - Extracting shared code and updating imports
/// - Interface changes that affect multiple files
pub async fn generate_multi_file_fix(
    files: &[(PathBuf, String)], // (path, content) pairs
    suggestion: &Suggestion,
    plan: &FixPreview,
    repo_memory: Option<String>,
) -> anyhow::Result<MultiFileAppliedFix> {
    let plan_text = format!(
        "Verification: {} - {}\nPlan: {}\nScope: {}\nAffected areas: {}{}",
        if plan.verified { "CONFIRMED" } else { "UNCONFIRMED" },
        plan.verification_note,
        plan.description,
        plan.scope.label(),
        plan.affected_areas.join(", "),
        plan.modifier
            .as_ref()
            .map(|m| format!("\nUser modifications: {}", m))
            .unwrap_or_default()
    );

    let memory_section = repo_memory
        .as_deref()
        .filter(|m| !m.trim().is_empty())
        .map(|m| format!("\n\nRepo conventions / decisions:\n{}", m))
        .unwrap_or_default();

    // Build the files section
    let files_section: String = files
        .iter()
        .map(|(path, content)| format!("=== {} ===\n```\n{}\n```", path.display(), content))
        .collect::<Vec<_>>()
        .join("\n\n");

    let user = format!(
        "Original Issue: {}\n{}\n{}\n\n{}\n\nFILES TO MODIFY:\n\n{}\n\nImplement the fix using search/replace edits for each file. Ensure consistency across all files.",
        suggestion.summary,
        suggestion.detail.as_deref().unwrap_or(""),
        memory_section,
        plan_text,
        files_section
    );

    let response =
        call_llm_with_usage(MULTI_FILE_FIX_SYSTEM, &user, Model::Smart, true).await?;

    // Parse the JSON response with self-correction on failure
    let (parsed, correction_usage): (MultiFileFixResponse, _) =
        parse_json_with_retry(&response.content, "multi-file fix").await?;

    // Merge usage from correction call if any
    let total_usage = merge_usage(response.usage, correction_usage);

    let description = parsed
        .description
        .unwrap_or_else(|| "Applied the requested multi-file fix".to_string());
    let file_edits_json = parsed.file_edits;

    if file_edits_json.is_empty() {
        return Err(anyhow::anyhow!("No file edits provided in response"));
    }

    // Apply edits to each file
    let mut file_edits = Vec::new();

    for file_edit_json in file_edits_json {
        let file_path = PathBuf::from(&file_edit_json.file);
        let mut new_content = files
            .iter()
            .find(|(path, _)| path == &file_path)
            .map(|(_, content)| content.clone())
            .unwrap_or_default();

        let mut modified_areas = Vec::new();
        for edit in &file_edit_json.edits {
            let matches: Vec<_> = new_content.match_indices(&edit.old_string).collect();

            if matches.is_empty() {
                return Err(anyhow::anyhow!(
                    "old_string not found in file {}. Searched for: {:?}",
                    file_path.display(),
                    truncate_for_error(&edit.old_string)
                ));
            }

            if matches.len() > 1 {
                return Err(anyhow::anyhow!(
                    "old_string matches {} times in file {} (must be unique). Searched for: {:?}",
                    matches.len(),
                    file_path.display(),
                    truncate_for_error(&edit.old_string)
                ));
            }

            // Extract function/area name from old_string if possible
            if let Some(area) = extract_modified_area(&edit.old_string) {
                modified_areas.push(area);
            }

            // Apply edit
            new_content = new_content.replacen(&edit.old_string, &edit.new_string, 1);
        }

        // Strip trailing whitespace and ensure file ends with newline
        let mut new_content: String = new_content
            .lines()
            .map(|line| line.trim_end())
            .collect::<Vec<_>>()
            .join("\n");
        if !new_content.ends_with('\n') {
            new_content.push('\n');
        }

        file_edits.push(FileEdit {
            path: file_path,
            new_content,
            modified_areas,
        });
    }

    Ok(MultiFileAppliedFix {
        description,
        file_edits,
        usage: total_usage,
    })
}

/// Try to extract the modified function or area name from old_string
fn extract_modified_area(old_string: &str) -> Option<String> {
    let patterns = [
        (r"fn\s+(\w+)\s*\(", 1),
        (r"pub\s+fn\s+(\w+)\s*\(", 1),
        (r"struct\s+(\w+)", 1),
        (r"impl\s+(\w+)", 1),
        (r"trait\s+(\w+)", 1),
        (r"enum\s+(\w+)", 1),
    ];

    for (pattern, group) in patterns {
        if let Ok(re) = regex::Regex::new(pattern) {
            if let Some(caps) = re.captures(old_string) {
                if let Some(m) = caps.get(group) {
                    return Some(m.as_str().to_string());
                }
            }
        }
    }

    None
}

// ═══════════════════════════════════════════════════════════════════════════
//  FAST FIX PREVIEW (Phase 1 of two-phase fix)
// ═══════════════════════════════════════════════════════════════════════════

/// Quick preview of what a fix will do - generated in <1 second
#[derive(Debug, Clone, PartialEq)]
pub struct FixPreview {
    /// Whether the issue was verified to exist in the code
    pub verified: bool,

    // ─── User-facing fields (non-technical) ───────────────────────────────
    /// Friendly topic name for non-technical users (e.g. "Batch Processing")
    pub friendly_title: String,
    /// Behavior-focused problem description (what happens, not how code works)
    pub problem_summary: String,
    /// What happens after the fix (outcome, not implementation)
    pub outcome: String,

    // ─── Technical fields (for internal/developer use) ────────────────────
    /// Explanation of verification result
    pub verification_note: String,
    /// Code snippet that proves the claim (evidence)
    pub evidence_snippet: Option<String>,
    /// Starting line number of the evidence snippet
    pub evidence_line: Option<u32>,
    /// Human-readable description of what will change (1-2 sentences)
    pub description: String,
    /// Which functions/areas are affected
    pub affected_areas: Vec<String>,
    /// Estimated scope: small (few lines), medium (function), large (multiple functions/file restructure)
    pub scope: FixScope,
    /// Optional user modifier to refine the fix
    pub modifier: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FixScope {
    Small,  // Few lines changed
    Medium, // A function or two
    Large,  // Multiple functions or file restructure
}

impl FixScope {
    pub fn label(&self) -> &'static str {
        match self {
            FixScope::Small => "small",
            FixScope::Medium => "medium",
            FixScope::Large => "large",
        }
    }

    pub fn icon(&self) -> &'static str {
        match self {
            FixScope::Small => "·",
            FixScope::Medium => "◐",
            FixScope::Large => "●",
        }
    }
}

/// Generate a preview of what the fix will do with smart verification
/// This is Phase 1 of the two-phase fix flow - uses Smart model to thoroughly verify the issue exists before users approve
pub async fn generate_fix_preview(
    path: &PathBuf,
    suggestion: &Suggestion,
    modifier: Option<&str>,
    repo_memory: Option<String>,
) -> anyhow::Result<FixPreview> {
    let modifier_text = modifier
        .map(|m| format!("\n\nUser wants: {}", m))
        .unwrap_or_default();

    let memory_section = repo_memory
        .as_deref()
        .filter(|m| !m.trim().is_empty())
        .map(|m| format!("\n\nRepo conventions / decisions:\n{}", m))
        .unwrap_or_default();

    let user = format!(
        "File: {}\nIssue: {}\n{}{}{}",
        path.display(),
        suggestion.summary,
        suggestion.detail.as_deref().unwrap_or(""),
        memory_section,
        modifier_text
    );

    let response = call_llm(FIX_PREVIEW_SYSTEM, &user, Model::Balanced).await?;

    // Try parsing, with self-correction retry on failure
    match try_parse_preview_json(&response) {
        Ok(parsed) => build_fix_preview(parsed, modifier.map(String::from)),
        Err(initial_error) => {
            // Try LLM self-correction
            if let Ok(correction) = request_json_correction_generic(
                &response,
                &initial_error.to_string(),
                "fix preview",
            )
            .await
            {
                if let Ok(parsed) = try_parse_preview_json(&correction.content) {
                    return build_fix_preview(parsed, modifier.map(String::from));
                }
            }
            Err(initial_error)
        }
    }
}

/// Attempt to parse fix preview JSON from a response
fn try_parse_preview_json(response: &str) -> anyhow::Result<serde_json::Value> {
    // Strip code fences
    let trimmed = response.trim();
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
    let clean = clean.trim();

    serde_json::from_str(clean)
        .map_err(|e| anyhow::anyhow!("Failed to parse fix preview JSON: {}", e))
}

fn build_fix_preview(
    parsed: serde_json::Value,
    modifier: Option<String>,
) -> anyhow::Result<FixPreview> {
    let verified = parsed
        .get("verified")
        .and_then(|v| v.as_bool())
        .or_else(|| {
            // Handle string "true"/"false" in case of JSON issues
            if let Some(s) = parsed.get("verified").and_then(|v| v.as_str()) {
                Some(s.eq_ignore_ascii_case("true"))
            } else {
                None
            }
        })
        .unwrap_or(true); // Default to true for backwards compatibility

    let verification_note = parsed
        .get("verification_note")
        .and_then(|v| v.as_str())
        .unwrap_or(if verified {
            "Issue verified"
        } else {
            "Issue not found"
        })
        .to_string();

    // Parse user-facing fields (non-technical)
    let friendly_title = parsed
        .get("friendly_title")
        .and_then(|v| v.as_str())
        .unwrap_or("Issue")
        .to_string();

    let problem_summary = parsed
        .get("problem_summary")
        .and_then(|v| v.as_str())
        .unwrap_or("An issue was found that needs attention.")
        .to_string();

    let outcome = parsed
        .get("outcome")
        .and_then(|v| v.as_str())
        .unwrap_or("This will be fixed.")
        .to_string();

    let evidence_snippet = parsed
        .get("evidence_snippet")
        .and_then(|v| v.as_str())
        .filter(|s| !s.trim().is_empty())
        .map(|s| s.to_string());

    let evidence_line = parsed
        .get("evidence_line")
        .and_then(|v| v.as_u64())
        .map(|n| n as u32);

    let description = parsed
        .get("description")
        .and_then(|v| v.as_str())
        .unwrap_or("Fix the identified issue")
        .to_string();

    let affected_areas = parsed
        .get("affected_areas")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();

    let scope = match parsed.get("scope").and_then(|v| v.as_str()) {
        Some("small") => FixScope::Small,
        Some("large") => FixScope::Large,
        _ => FixScope::Medium,
    };

    Ok(FixPreview {
        verified,
        friendly_title,
        problem_summary,
        outcome,
        verification_note,
        evidence_snippet,
        evidence_line,
        description,
        affected_areas,
        scope,
        modifier,
    })
}
