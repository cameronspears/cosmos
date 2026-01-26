use super::agentic::call_llm_agentic;
use super::client::{call_llm_with_usage, LlmResponse};
use super::models::{Model, Usage};
use super::parse::{
    merge_usage, parse_json_with_retry, truncate_content, truncate_content_around_line,
};
use super::prompt_utils::format_repo_memory_section;
use super::prompts::{FIX_CONTENT_SYSTEM, FIX_PREVIEW_AGENTIC_SYSTEM, MULTI_FILE_FIX_SYSTEM};
use crate::suggest::Suggestion;
use serde::Deserialize;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

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

// Context limits - generous to let models work with full context.
// Modern models handle 100k+ tokens easily; these limits only kick in for unusually large files.
const MAX_FIX_EXCERPT_CHARS: usize = 60000;
const MAX_MULTI_FILE_EXCERPT_CHARS: usize = 120000;

struct PromptContent {
    content: String,
    note: Option<String>,
}

fn prompt_budget_per_file(file_count: usize) -> usize {
    if file_count == 0 {
        return 0;
    }
    let per_file = MAX_MULTI_FILE_EXCERPT_CHARS / file_count;
    per_file.clamp(1, MAX_FIX_EXCERPT_CHARS)
}

fn choose_fix_anchor_line(
    lines: &[&str],
    suggestion_line: Option<usize>,
    evidence_line: Option<u32>,
    hint_tokens: &[String],
) -> usize {
    let evidence_line = evidence_line.and_then(|line| {
        let line = line as usize;
        if line > 0 && line <= lines.len() {
            Some(line)
        } else {
            None
        }
    });
    if let Some(line) = evidence_line {
        return line;
    }
    choose_preview_anchor_line(lines, suggestion_line, hint_tokens)
}

fn build_fix_prompt_content(
    content: &str,
    file_path: &Path,
    suggestion: &Suggestion,
    plan: &FixPreview,
    max_chars: usize,
    is_primary_file: bool,
) -> PromptContent {
    if content.trim().is_empty() {
        return PromptContent {
            content: String::new(),
            note: None,
        };
    }

    let content_len = content.chars().count();
    if content_len <= max_chars {
        return PromptContent {
            content: content.to_string(),
            note: None,
        };
    }

    let lines: Vec<&str> = content.lines().collect();
    if lines.is_empty() {
        return PromptContent {
            content: truncate_content(content, max_chars),
            note: Some(format!(
                "NOTE: This file is large ({} chars). Showing a shortened excerpt to fit model input limits.",
                content_len
            )),
        };
    }

    let mut extra_texts = Vec::new();
    extra_texts.push(plan.description.as_str());
    extra_texts.extend(plan.affected_areas.iter().map(|area| area.as_str()));
    if let Some(snippet) = plan.evidence_snippet.as_deref() {
        extra_texts.push(snippet);
    }
    if let Some(modifier) = plan.modifier.as_deref() {
        extra_texts.push(modifier);
    }

    let hint_tokens = extract_hint_tokens_with_extras(
        &suggestion.summary,
        suggestion.detail.as_deref(),
        file_path,
        &extra_texts,
    );
    let suggestion_line = if is_primary_file {
        suggestion.line
    } else {
        None
    };
    let evidence_line = if is_primary_file {
        plan.evidence_line
    } else {
        None
    };
    let anchor_line = choose_fix_anchor_line(&lines, suggestion_line, evidence_line, &hint_tokens);
    let snippet = truncate_content_around_line(content, anchor_line, max_chars)
        .unwrap_or_else(|| truncate_content(content, max_chars));

    PromptContent {
        content: snippet,
        note: Some(format!(
            "NOTE: This file is large ({} chars). Showing an excerpt around line {} to fit model input limits.",
            content_len, anchor_line
        )),
    }
}

fn format_excerpt_guidance(note: Option<&str>) -> String {
    note.map(|note| {
        format!(
            "{}\nIMPORTANT: Only a focused excerpt is shown. Use search/replace edits anchored in the excerpt, but make old_string unique in the full file by including enough surrounding context.\n",
            note
        )
    })
    .unwrap_or_default()
}

fn build_fix_user_prompt(
    path: &Path,
    new_file_note: &str,
    suggestion: &Suggestion,
    memory_section: &str,
    plan_text: &str,
    excerpt_guidance: &str,
    content: &str,
) -> String {
    format!(
        "File: {}\n{}\n\nOriginal Issue: {}\n{}\n{}\n\n{}\n{}\nCurrent Code:\n```\n{}\n```\n\nImplement the fix using search/replace edits. Be precise with old_string - it must match exactly.",
        path.display(),
        new_file_note,
        suggestion.summary,
        suggestion.detail.as_deref().unwrap_or(""),
        memory_section,
        plan_text,
        excerpt_guidance,
        content
    )
}

fn build_multi_file_user_prompt(
    suggestion: &Suggestion,
    memory_section: &str,
    plan_text: &str,
    files_section: &str,
) -> String {
    format!(
        "Original Issue: {}\n{}\n{}\n\n{}\n\nFILES TO MODIFY:\n\n{}\n\nImplement the fix using search/replace edits for each file. For new files, use old_string=\"\" to insert full content. Ensure consistency across all files.",
        suggestion.summary,
        suggestion.detail.as_deref().unwrap_or(""),
        memory_section,
        plan_text,
        files_section
    )
}

fn is_context_limit_error(message: &str) -> bool {
    let msg = message.to_lowercase();
    if msg.contains("context length") || msg.contains("context window") {
        return true;
    }
    if msg.contains("request too large") || msg.contains("payload too large") || msg.contains("413")
    {
        return true;
    }
    let has_context = msg.contains("context")
        || msg.contains("token")
        || msg.contains("tokens")
        || msg.contains("prompt")
        || msg.contains("input");
    let has_limit = msg.contains("limit")
        || msg.contains("exceed")
        || msg.contains("too long")
        || msg.contains("too large")
        || msg.contains("length")
        || msg.contains("maximum");
    has_context && has_limit
}

async fn call_llm_with_fallback(
    system: &str,
    user_full: &str,
    user_excerpt: &str,
    model: Model,
    json_mode: bool,
) -> anyhow::Result<LlmResponse> {
    match call_llm_with_usage(system, user_full, model, json_mode).await {
        Ok(response) => Ok(response),
        Err(err) => {
            let message = err.to_string();
            // Handle context limit by trying with smaller excerpt
            if is_context_limit_error(&message) && user_full != user_excerpt {
                call_llm_with_usage(system, user_excerpt, model, json_mode).await
            } else {
                Err(err)
            }
        }
    }
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
    path: &Path,
    content: &str,
    suggestion: &Suggestion,
    plan: &FixPreview,
    repo_memory: Option<String>,
    is_new_file: bool,
) -> anyhow::Result<AppliedFix> {
    let plan_text = format!(
        "Verification: {} - {}\nPlan: {}\nScope: {}\nAffected areas: {}{}",
        if plan.verified {
            "CONFIRMED"
        } else {
            "UNCONFIRMED"
        },
        plan.verification_note,
        plan.description,
        plan.scope.label(),
        plan.affected_areas.join(", "),
        plan.modifier
            .as_ref()
            .map(|m| format!("\nUser modifications: {}", m))
            .unwrap_or_default()
    );

    let memory_section =
        format_repo_memory_section(repo_memory.as_deref(), "Repo conventions / decisions");

    let new_file_note = if is_new_file {
        "\nNOTE: This file is new (currently empty). Use old_string=\"\" to insert full content."
    } else {
        ""
    };

    let prompt_content =
        build_fix_prompt_content(content, path, suggestion, plan, MAX_FIX_EXCERPT_CHARS, true);
    let excerpt_guidance = format_excerpt_guidance(prompt_content.note.as_deref());
    let user_full = build_fix_user_prompt(
        path,
        new_file_note,
        suggestion,
        &memory_section,
        &plan_text,
        "",
        content,
    );
    let user_excerpt = build_fix_user_prompt(
        path,
        new_file_note,
        suggestion,
        &memory_section,
        &plan_text,
        &excerpt_guidance,
        &prompt_content.content,
    );

    let response = call_llm_with_fallback(
        FIX_CONTENT_SYSTEM,
        &user_full,
        &user_excerpt,
        Model::Smart,
        true,
    )
    .await?;

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
    let new_content = apply_edits_with_context(content, &edits, "file")?;

    // Preserve whitespace and match trailing newline to original
    let new_content = normalize_generated_content(content, new_content, is_new_file);

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

pub(crate) fn apply_edits_with_context(
    content: &str,
    edits: &[EditOp],
    context_label: &str,
) -> anyhow::Result<String> {
    let mut new_content = content.to_string();
    for (i, edit) in edits.iter().enumerate() {
        if edit.old_string.is_empty() {
            if new_content.is_empty() {
                new_content = edit.new_string.clone();
                continue;
            }
            return Err(anyhow::anyhow!(
                "Edit {}: old_string is empty for non-empty {}. Provide more context.",
                i + 1,
                context_label
            ));
        }

        let matches: Vec<_> = new_content.match_indices(&edit.old_string).collect();

        if matches.is_empty() {
            return Err(anyhow::anyhow!(
                "Edit {}: old_string not found in {}. The LLM may have made an error.\nSearched for: {:?}",
                i + 1,
                context_label,
                truncate_for_error(&edit.old_string)
            ));
        }

        if matches.len() > 1 {
            return Err(anyhow::anyhow!(
                "Edit {}: old_string matches {} times in {} (must be unique). Need more context.\nSearched for: {:?}",
                i + 1,
                matches.len(),
                context_label,
                truncate_for_error(&edit.old_string)
            ));
        }

        new_content = new_content.replacen(&edit.old_string, &edit.new_string, 1);
    }

    Ok(new_content)
}

pub(crate) fn normalize_generated_content(
    original: &str,
    content: String,
    is_new_file: bool,
) -> String {
    if is_new_file {
        return content;
    }

    let original_ends_newline = original.ends_with('\n');
    let mut normalized = content;

    if original_ends_newline {
        if !normalized.ends_with('\n') {
            if original.ends_with("\r\n") {
                normalized.push_str("\r\n");
            } else {
                normalized.push('\n');
            }
        }
    } else {
        while normalized.ends_with('\n') {
            if normalized.ends_with("\r\n") {
                let new_len = normalized.len().saturating_sub(2);
                normalized.truncate(new_len);
            } else {
                let new_len = normalized.len().saturating_sub(1);
                normalized.truncate(new_len);
            }
        }
    }

    normalized
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

/// Input for a single file in a multi-file fix
#[derive(Debug, Clone)]
pub struct FileInput {
    pub path: PathBuf,
    pub content: String,
    pub is_new: bool,
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
    files: &[FileInput],
    suggestion: &Suggestion,
    plan: &FixPreview,
    repo_memory: Option<String>,
) -> anyhow::Result<MultiFileAppliedFix> {
    if files.is_empty() {
        return Err(anyhow::anyhow!("No files provided for multi-file fix"));
    }
    let per_file_budget = prompt_budget_per_file(files.len());

    let plan_text = format!(
        "Verification: {} - {}\nPlan: {}\nScope: {}\nAffected areas: {}{}",
        if plan.verified {
            "CONFIRMED"
        } else {
            "UNCONFIRMED"
        },
        plan.verification_note,
        plan.description,
        plan.scope.label(),
        plan.affected_areas.join(", "),
        plan.modifier
            .as_ref()
            .map(|m| format!("\nUser modifications: {}", m))
            .unwrap_or_default()
    );

    let memory_section =
        format_repo_memory_section(repo_memory.as_deref(), "Repo conventions / decisions");

    // Build full and excerpted file sections
    let files_section_full: String = files
        .iter()
        .map(|file| {
            let new_note = if file.is_new { "(NEW FILE)" } else { "" };
            format!(
                "=== {} {} ===\n```\n{}\n```",
                file.path.display(),
                new_note,
                file.content
            )
        })
        .collect::<Vec<_>>()
        .join("\n\n");

    let files_section_excerpt: String = files
        .iter()
        .map(|file| {
            let new_note = if file.is_new { "(NEW FILE)" } else { "" };
            let is_primary = file.path == suggestion.file;
            let prompt_content = build_fix_prompt_content(
                &file.content,
                &file.path,
                suggestion,
                plan,
                per_file_budget,
                is_primary,
            );
            let excerpt_guidance = format_excerpt_guidance(prompt_content.note.as_deref());
            if excerpt_guidance.is_empty() {
                format!(
                    "=== {} {} ===\n```\n{}\n```",
                    file.path.display(),
                    new_note,
                    prompt_content.content
                )
            } else {
                format!(
                    "=== {} {} ===\n{}\n```\n{}\n```",
                    file.path.display(),
                    new_note,
                    excerpt_guidance,
                    prompt_content.content
                )
            }
        })
        .collect::<Vec<_>>()
        .join("\n\n");

    let user_full =
        build_multi_file_user_prompt(suggestion, &memory_section, &plan_text, &files_section_full);
    let user_excerpt = build_multi_file_user_prompt(
        suggestion,
        &memory_section,
        &plan_text,
        &files_section_excerpt,
    );

    let response = call_llm_with_fallback(
        MULTI_FILE_FIX_SYSTEM,
        &user_full,
        &user_excerpt,
        Model::Smart,
        true,
    )
    .await?;

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
        let Some(file_input) = files.iter().find(|f| f.path == file_path) else {
            return Err(anyhow::anyhow!(
                "Multi-file fix references missing file: {}",
                file_path.display()
            ));
        };
        let new_content = file_input.content.clone();

        let mut modified_areas = Vec::new();
        for edit in &file_edit_json.edits {
            if let Some(area) = extract_modified_area(&edit.old_string) {
                modified_areas.push(area);
            }
        }

        let context = format!("file {}", file_path.display());
        let new_content = apply_edits_with_context(&new_content, &file_edit_json.edits, &context)?;

        // Preserve whitespace and match trailing newline to original
        let new_content =
            normalize_generated_content(&file_input.content, new_content, file_input.is_new);

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
}

/// Generate a preview using agentic verification.
///
/// Instead of spoon-feeding truncated code, this lets the model use tools
/// (grep, read, ls) to explore the codebase and find the evidence it needs.
/// This produces more accurate verification because the model finds context itself.
pub async fn generate_fix_preview_agentic(
    repo_root: &Path,
    suggestion: &Suggestion,
    modifier: Option<&str>,
    repo_memory: Option<String>,
) -> anyhow::Result<FixPreview> {
    let modifier_text = modifier
        .map(|m| format!("\n\nUser modification request: {}", m))
        .unwrap_or_default();

    let memory_section =
        format_repo_memory_section(repo_memory.as_deref(), "Repo conventions / decisions");

    // Build a focused prompt - the model will use tools to find the code
    let user = format!(
        r#"ISSUE TO VERIFY:
File: {}
Summary: {}
{}
{}{}

Use the available tools to:
1. Find and examine the relevant code in this file
2. Verify whether this issue actually exists
3. Return your findings as JSON"#,
        suggestion.file.display(),
        suggestion.summary,
        suggestion.detail.as_deref().unwrap_or(""),
        memory_section,
        modifier_text,
    );

    let response = call_llm_agentic(
        FIX_PREVIEW_AGENTIC_SYSTEM,
        &user,
        Model::Balanced,
        repo_root,
        false,
    )
    .await?;

    // Parse the final response as JSON
    let (parsed, _correction_usage): (serde_json::Value, _) =
        parse_json_with_retry(&response.content, "fix preview").await?;

    build_fix_preview(parsed, modifier.map(String::from))
}

// ═══════════════════════════════════════════════════════════════════════════
//  ANCHOR LINE SELECTION (used by fix generation for context selection)
// ═══════════════════════════════════════════════════════════════════════════

fn choose_preview_anchor_line(
    lines: &[&str],
    suggestion_line: Option<usize>,
    hint_tokens: &[String],
) -> usize {
    let valid_suggestion_line = suggestion_line.filter(|line| *line > 0 && *line <= lines.len());

    if let Some((best_line, best_score)) = find_best_anchor_line(lines, hint_tokens) {
        if let Some(suggested) = valid_suggestion_line {
            let suggested_score = score_line_window(lines, suggested, hint_tokens);
            if best_score > suggested_score {
                return best_line;
            }
            if suggested_score > 0 {
                return suggested;
            }
        }
        if best_score > 0 {
            return best_line;
        }
    }

    if let Some(suggested) = valid_suggestion_line {
        return suggested;
    }

    find_first_impl_or_fn_line(lines).unwrap_or(1)
}

fn find_best_anchor_line(lines: &[&str], hint_tokens: &[String]) -> Option<(usize, usize)> {
    find_best_line_for_tokens(lines, hint_tokens, true)
        .or_else(|| find_best_line_for_tokens(lines, hint_tokens, false))
}

fn find_best_line_for_tokens(
    lines: &[&str],
    hint_tokens: &[String],
    anchor_only: bool,
) -> Option<(usize, usize)> {
    if hint_tokens.is_empty() {
        return None;
    }

    let mut best: Option<(usize, usize)> = None;
    for (idx, line) in lines.iter().enumerate() {
        if anchor_only && !is_anchor_line(line) {
            continue;
        }
        let score = score_line_window(lines, idx + 1, hint_tokens);
        if score == 0 {
            continue;
        }
        match best {
            Some((_, best_score)) if best_score >= score => {}
            _ => best = Some((idx + 1, score)),
        }
    }

    best
}

fn score_line_window(lines: &[&str], line: usize, hint_tokens: &[String]) -> usize {
    if hint_tokens.is_empty() || line == 0 || line > lines.len() {
        return 0;
    }
    let idx = line.saturating_sub(1);
    let start = idx.saturating_sub(1);
    let end = (idx + 1).min(lines.len().saturating_sub(1));

    let mut window = String::new();
    for l in &lines[start..=end] {
        window.push_str(l);
        window.push(' ');
    }
    let lower = window.to_lowercase();
    hint_tokens
        .iter()
        .filter(|token| lower.contains(token.as_str()))
        .count()
}

fn is_anchor_line(line: &str) -> bool {
    let trimmed = line.trim_start();
    trimmed.starts_with("fn ")
        || trimmed.starts_with("async fn ")
        || trimmed.starts_with("pub fn ")
        || trimmed.starts_with("pub async fn ")
        || trimmed.starts_with("pub(crate) fn ")
        || trimmed.starts_with("impl ")
        || trimmed.starts_with("impl<")
        || trimmed.starts_with("pub struct ")
        || trimmed.starts_with("struct ")
        || trimmed.starts_with("pub enum ")
        || trimmed.starts_with("enum ")
        || trimmed.starts_with("pub trait ")
        || trimmed.starts_with("trait ")
        || trimmed.starts_with("pub type ")
        || trimmed.starts_with("type ")
}

fn find_first_impl_or_fn_line(lines: &[&str]) -> Option<usize> {
    lines
        .iter()
        .position(|line| {
            let trimmed = line.trim_start();
            trimmed.starts_with("impl ")
                || trimmed.starts_with("impl<")
                || trimmed.starts_with("fn ")
                || trimmed.starts_with("pub fn ")
                || trimmed.starts_with("pub(crate) fn ")
                || trimmed.starts_with("pub async fn ")
                || trimmed.starts_with("async fn ")
        })
        .map(|idx| idx + 1)
}

// ═══════════════════════════════════════════════════════════════════════════
//  TOKEN EXTRACTION (used by fix generation for context selection)
// ═══════════════════════════════════════════════════════════════════════════

fn extract_hint_tokens_with_extras(
    summary: &str,
    detail: Option<&str>,
    path: &Path,
    extra_texts: &[&str],
) -> Vec<String> {
    let mut tokens = Vec::new();
    if let Some(detail) = detail {
        tokens.extend(extract_backtick_tokens(detail));
        tokens.extend(extract_identifier_tokens(detail));
    }
    tokens.extend(extract_identifier_tokens(summary));
    tokens.extend(extract_path_tokens(path));
    for extra in extra_texts {
        tokens.extend(extract_backtick_tokens(extra));
        tokens.extend(extract_identifier_tokens(extra));
    }
    normalize_hint_tokens(tokens)
}

fn normalize_hint_tokens(tokens: Vec<String>) -> Vec<String> {
    let mut seen: HashSet<String> = HashSet::new();
    tokens
        .into_iter()
        .map(|token| token.to_lowercase())
        .filter(|token| token.len() >= 3 && !is_stopword(token))
        .filter(|token| seen.insert(token.clone()))
        .collect()
}

fn extract_backtick_tokens(text: &str) -> Vec<String> {
    let re = regex::Regex::new(r"`([^`]+)`").unwrap_or_else(|_| regex::Regex::new("$^").unwrap());
    let id_re = regex::Regex::new(r"[A-Za-z_][A-Za-z0-9_]*")
        .unwrap_or_else(|_| regex::Regex::new("$^").unwrap());
    let mut tokens = Vec::new();
    for caps in re.captures_iter(text) {
        let raw = caps.get(1).map(|m| m.as_str()).unwrap_or("");
        for ident in id_re.find_iter(raw) {
            tokens.push(ident.as_str().to_string());
        }
    }
    tokens
}

fn extract_identifier_tokens(text: &str) -> Vec<String> {
    let re = regex::Regex::new(r"[A-Za-z_][A-Za-z0-9_]*")
        .unwrap_or_else(|_| regex::Regex::new("$^").unwrap());
    re.find_iter(text).map(|m| m.as_str().to_string()).collect()
}

fn extract_path_tokens(path: &Path) -> Vec<String> {
    let mut tokens = Vec::new();
    if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
        tokens.extend(extract_identifier_tokens(stem));
    }
    if let Some(parent) = path
        .parent()
        .and_then(|p| p.file_name())
        .and_then(|s| s.to_str())
    {
        tokens.extend(extract_identifier_tokens(parent));
    }
    tokens
}

fn is_stopword(token: &str) -> bool {
    matches!(
        token,
        "a" | "an"
            | "the"
            | "and"
            | "or"
            | "but"
            | "if"
            | "when"
            | "while"
            | "with"
            | "without"
            | "for"
            | "from"
            | "to"
            | "of"
            | "in"
            | "on"
            | "at"
            | "by"
            | "as"
            | "is"
            | "are"
            | "was"
            | "were"
            | "be"
            | "been"
            | "being"
            | "this"
            | "that"
            | "these"
            | "those"
            | "it"
            | "its"
            | "they"
            | "them"
            | "their"
            | "we"
            | "our"
            | "you"
            | "your"
            | "should"
            | "could"
            | "would"
            | "can"
            | "may"
            | "might"
            | "will"
            | "do"
            | "does"
            | "did"
            | "done"
            | "use"
            | "uses"
            | "used"
            | "using"
            | "file"
            | "files"
            | "code"
            | "system"
            | "method"
            | "function"
            | "module"
            | "line"
            | "lines"
            | "path"
            | "paths"
            | "mod"
    )
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
            parsed
                .get("verified")
                .and_then(|v| v.as_str())
                .map(|s| s.eq_ignore_ascii_case("true"))
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::suggest::{Priority, SuggestionKind, SuggestionSource};
    use std::path::{Path, PathBuf};

    fn sample_preview(evidence_line: Option<u32>) -> FixPreview {
        FixPreview {
            verified: true,
            friendly_title: "Issue".to_string(),
            problem_summary: "Problem".to_string(),
            outcome: "Outcome".to_string(),
            verification_note: "Verified".to_string(),
            evidence_snippet: None,
            evidence_line,
            description: "Update behavior".to_string(),
            affected_areas: vec!["update_behavior".to_string()],
            scope: FixScope::Medium,
            modifier: None,
        }
    }

    fn sample_suggestion(path: PathBuf) -> Suggestion {
        Suggestion::new(
            SuggestionKind::BugFix,
            Priority::Medium,
            path,
            "Fix issue".to_string(),
            SuggestionSource::LlmDeep,
        )
        .with_detail("Details".to_string())
    }

    #[test]
    fn test_apply_edits_with_empty_old_string_on_empty_file() {
        let edits = vec![EditOp {
            old_string: "".to_string(),
            new_string: "hello".to_string(),
        }];
        let updated = apply_edits_with_context("", &edits, "file").unwrap();
        assert_eq!(updated, "hello");
    }

    #[test]
    fn test_apply_edits_empty_old_string_on_non_empty_file_fails() {
        let edits = vec![EditOp {
            old_string: "".to_string(),
            new_string: "hello".to_string(),
        }];
        let err = apply_edits_with_context("content", &edits, "file").unwrap_err();
        assert!(err.to_string().contains("old_string is empty"));
    }

    #[test]
    fn test_normalize_generated_content_adds_newline_when_original_had() {
        let original = "line1\n";
        let updated = "line1".to_string();
        let normalized = normalize_generated_content(original, updated, false);
        assert_eq!(normalized, "line1\n");
    }

    #[test]
    fn test_normalize_generated_content_strips_newline_when_original_missing() {
        let original = "line1";
        let updated = "line1\n\n".to_string();
        let normalized = normalize_generated_content(original, updated, false);
        assert_eq!(normalized, "line1");
    }

    #[test]
    fn test_normalize_generated_content_preserves_crlf() {
        let original = "line1\r\n";
        let updated = "line1".to_string();
        let normalized = normalize_generated_content(original, updated, false);
        assert_eq!(normalized, "line1\r\n");
    }

    #[test]
    fn test_normalize_generated_content_new_file_is_untouched() {
        let original = "";
        let updated = "line1\n".to_string();
        let normalized = normalize_generated_content(original, updated.clone(), true);
        assert_eq!(normalized, updated);
    }

    #[test]
    fn test_choose_preview_anchor_prefers_hint_match() {
        let content = [
            "line1",
            "pub struct FileSummary {",
            "}",
            "impl CodebaseIndex {",
            "    fn scan(&self) {}",
            "}",
        ]
        .join("\n");
        let lines: Vec<&str> = content.lines().collect();
        let hint_tokens = vec!["index".to_string()];

        let line = choose_preview_anchor_line(&lines, Some(2), &hint_tokens);
        assert_eq!(line, 4);
    }

    #[test]
    fn test_choose_preview_anchor_falls_back_to_suggestion_line() {
        let content = [
            "line1",
            "pub struct FileSummary {",
            "}",
            "impl CodebaseIndex {",
        ]
        .join("\n");
        let lines: Vec<&str> = content.lines().collect();
        let hint_tokens = vec!["missingtoken".to_string()];

        let line = choose_preview_anchor_line(&lines, Some(2), &hint_tokens);
        assert_eq!(line, 2);
    }

    #[test]
    fn test_choose_preview_anchor_uses_first_impl_when_missing_suggestion() {
        let content = [
            "line1",
            "pub struct FileSummary {",
            "}",
            "impl CodebaseIndex {",
        ]
        .join("\n");
        let lines: Vec<&str> = content.lines().collect();

        let line = choose_preview_anchor_line(&lines, None, &[]);
        assert_eq!(line, 4);
    }

    #[test]
    fn test_build_fix_prompt_content_uses_full_when_under_budget() {
        let content = "line1\nline2";
        let path = Path::new("src/lib.rs");
        let suggestion = sample_suggestion(path.to_path_buf());
        let plan = sample_preview(None);

        let prompt = build_fix_prompt_content(content, path, &suggestion, &plan, 200, true);

        assert_eq!(prompt.content, content);
        assert!(prompt.note.is_none());
    }

    #[test]
    fn test_build_fix_prompt_content_truncates_large_file() {
        let content = (1..=200)
            .map(|i| format!("fn line_{}() {{}}\n", i))
            .collect::<String>();
        let path = Path::new("src/lib.rs");
        let suggestion = sample_suggestion(path.to_path_buf());
        let plan = sample_preview(Some(150));

        let prompt = build_fix_prompt_content(&content, path, &suggestion, &plan, 200, true);

        assert!(prompt.content.chars().count() <= 200);
        let note = prompt.note.expect("expected truncation note");
        assert!(note.contains("line 150"));
    }

    #[test]
    fn test_is_context_limit_error_detects_context_length() {
        let msg = "API error 400: context length exceeded";
        assert!(is_context_limit_error(msg));
    }

    #[test]
    fn test_is_context_limit_error_ignores_unrelated_error() {
        let msg = "API error 400: invalid request payload";
        assert!(!is_context_limit_error(msg));
    }
}
