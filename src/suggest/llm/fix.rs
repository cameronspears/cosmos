use super::agentic::{call_llm_agentic, schema_to_response_format};
use super::client::{
    call_llm_structured_cached, call_llm_structured_limited_speed_with_failover,
    SpeedFailoverDiagnostics, StructuredResponse,
};
use super::models::{merge_usage, Model, Usage};
use super::parse::{truncate_content, truncate_content_around_line};
use super::prompt_utils::format_repo_memory_section;
use super::prompts::{fix_content_system, multi_file_fix_system, FIX_PREVIEW_AGENTIC_SYSTEM};
use crate::suggest::Suggestion;
use serde::{Deserialize, Serialize};
use serde_json::json;
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
    /// Speed-tier provider failover diagnostics for transparency (if applicable).
    pub speed_failover: Option<SpeedFailoverDiagnostics>,
}

const PYTHON_IMPLEMENTATION_GUARDRAILS: &str = "\
Python guardrails:
- If you reference a module, import it.
- Do not change exit codes or return values unless explicitly required by the plan.
- Keep the diff minimal and avoid refactors.";

// Context limits - tuned for low cost and reliability.
// We intentionally prefer focused excerpts over full-file dumps to keep token usage bounded.
const MAX_FIX_EXCERPT_CHARS: usize = 20000;
const MAX_MULTI_FILE_EXCERPT_CHARS: usize = 60000;
const MAX_EDIT_REPAIR_ATTEMPTS: usize = 3;

const MAX_FIX_RESPONSE_TOKENS_SPEED: u32 = 4096;
const MAX_MULTI_FILE_FIX_RESPONSE_TOKENS_SPEED: u32 = 6144;

struct PromptContent {
    content: String,
    note: Option<String>,
}

fn allocate_attempt_time_slices_ms(total_ms: u64, slots: usize) -> Vec<u64> {
    if slots == 0 {
        return Vec::new();
    }
    if slots == 1 {
        return vec![total_ms.max(1)];
    }

    // Reserve meaningful time per retry. If the caller budget is too small, try once
    // (the harness will enforce the overall timeout anyway).
    const MIN_PER_ATTEMPT_MS: u64 = 1_200;
    if total_ms < MIN_PER_ATTEMPT_MS.saturating_mul(slots as u64) {
        let mut out = vec![0; slots];
        out[0] = total_ms.max(1);
        return out;
    }

    // Front-load time onto attempt 1. If providers are slow (latency spikes, rate limiting),
    // splitting evenly tends to create "all providers timed out" failures. Retries are still
    // possible, but we bias toward a strong first attempt.
    let remaining_slots = slots.saturating_sub(1);
    let min_reserve_ms = MIN_PER_ATTEMPT_MS.saturating_mul(remaining_slots as u64);
    let max_first_ms = total_ms
        .saturating_sub(min_reserve_ms)
        .max(MIN_PER_ATTEMPT_MS);
    let first_ms = ((total_ms * 2) / 3).clamp(MIN_PER_ATTEMPT_MS, max_first_ms);

    let mut out = Vec::with_capacity(slots);
    out.push(first_ms.max(1));

    let remaining_ms = total_ms.saturating_sub(first_ms);
    if remaining_slots == 0 {
        return out;
    }

    let per = remaining_ms / remaining_slots as u64;
    let mut rem = remaining_ms.saturating_sub(per.saturating_mul(remaining_slots as u64));
    for _ in 0..remaining_slots {
        let mut slice = per.max(MIN_PER_ATTEMPT_MS);
        if rem > 0 {
            slice = slice.saturating_add(1);
            rem -= 1;
        }
        out.push(slice.max(1));
    }

    out
}

fn max_tokens_for_fix_response(model: Model) -> u32 {
    match model {
        Model::Speed => MAX_FIX_RESPONSE_TOKENS_SPEED,
        _ => model.max_tokens(),
    }
}

fn max_tokens_for_multi_file_fix_response(model: Model) -> u32 {
    match model {
        Model::Speed => MAX_MULTI_FILE_FIX_RESPONSE_TOKENS_SPEED,
        _ => model.max_tokens(),
    }
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

fn is_python_file(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| ext.eq_ignore_ascii_case("py"))
        .unwrap_or(false)
}

fn build_plan_text(plan: &FixPreview, extra_guardrails: Option<&str>) -> String {
    let mut tail = plan
        .modifier
        .as_ref()
        .map(|m| format!("\nUser modifications: {}", m))
        .unwrap_or_default();
    if let Some(extra) = extra_guardrails {
        let extra = extra.trim();
        if !extra.is_empty() {
            tail.push_str("\n\n");
            tail.push_str(extra);
        }
    }

    format!(
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
        tail
    )
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

fn is_retryable_edit_apply_error(message: &str) -> bool {
    let msg = message.to_ascii_lowercase();
    msg.contains("old_string")
        && (msg.contains("not found")
            || msg.contains("matches")
            || msg.contains("must be unique")
            || msg.contains("empty for non-empty"))
}

fn searched_for_fragment(message: &str) -> Option<String> {
    let idx = message.find("Searched for:")?;
    Some(message[idx..].trim().to_string())
}

pub(crate) fn format_edit_apply_repair_guidance(message: &str, code_block_label: &str) -> String {
    let msg = message.to_ascii_lowercase();
    let mut bullets: Vec<&str> = Vec::new();

    if msg.contains("no edits provided") || msg.contains("no file edits provided") {
        bullets.push("Your response did not include any edits.");
        bullets.push("Return at least one edit that changes the code to address the request.");
        bullets.push("Keep the diff minimal and scoped. Avoid unrelated reformatting.");
        bullets.push("Use exact `old_string` anchors copied verbatim from the code block.");
    } else if msg.contains("matches") || msg.contains("must be unique") {
        bullets.push("Your `old_string` was too generic and matched multiple places.");
        bullets.push("Use the provided match contexts in the error details: choose the occurrence closest to the target line, then expand the anchor with surrounding lines to make it unique.");
        bullets.push("Pick a larger anchor: include 3-10 surrounding lines and at least one unique identifier (function name, component name, string literal, or nearby props).");
        bullets
            .push("Avoid anchors like `</div>`, `</motion.div>`, `{}` blocks, or single braces.");
        bullets.push("If you need to change multiple occurrences, return multiple edits with different unique `old_string` values.");
    } else if msg.contains("not found") {
        bullets.push("Your `old_string` does not exist verbatim in the code block.");
        bullets.push("Copy/paste the exact text from the code block, including indentation and line endings.");
        bullets.push("Do not use placeholders, summaries, or ellipses. The match must be exact.");
    } else if msg.contains("empty for non-empty") {
        bullets.push("Do not use an empty `old_string` when the file already has content.");
        bullets.push("Choose an exact anchor from the code block that appears exactly once.");
    } else {
        bullets.push("Fix your `old_string` values so they match verbatim text from the code block exactly once.");
    }

    let searched_for = searched_for_fragment(message);
    let searched_for = searched_for
        .as_deref()
        .map(|frag| format!("\nPrevious attempt detail:\n{}", frag))
        .unwrap_or_default();

    let bullet_text = bullets
        .into_iter()
        .map(|b| format!("- {}", b))
        .collect::<Vec<_>>()
        .join("\n");

    format!(
        "\n\nIMPORTANT: Your previous edits could not be applied safely.\nError:\n{}\n\nWhen regenerating edits, use the {} and follow these rules:\n{}\n{}",
        message, code_block_label, bullet_text, searched_for
    )
}

/// A single search/replace edit operation
#[derive(Debug, Clone, Deserialize, Serialize)]
pub(crate) struct EditOp {
    /// The exact text to find (must match exactly once in the file)
    pub(crate) old_string: String,
    /// The replacement text
    pub(crate) new_string: String,
}

/// Response structure for fix generation
#[derive(Debug, Clone, Deserialize, Serialize)]
pub(crate) struct FixResponse {
    #[serde(default)]
    pub(crate) description: Option<String>,
    #[serde(default)]
    pub(crate) modified_areas: Vec<String>,
    pub(crate) edits: Vec<EditOp>,
}

/// JSON Schema for FixResponse - used for structured output
pub(crate) fn fix_response_schema() -> serde_json::Value {
    json!({
        "type": "object",
        "properties": {
            "description": {
                "type": "string",
                "description": "Brief description of what was changed"
            },
            "modified_areas": {
                "type": "array",
                "items": { "type": "string" },
                "description": "List of functions/areas that were modified"
            },
            "edits": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "old_string": {
                            "type": "string",
                            "description": "Exact text to find (must match exactly once)"
                        },
                        "new_string": {
                            "type": "string",
                            "description": "Replacement text"
                        }
                    },
                    "required": ["old_string", "new_string"],
                    "additionalProperties": false
                },
                "description": "Search/replace edit operations"
            }
        },
        "required": ["edits"],
        "additionalProperties": false
    })
}

/// Call LLM with structured output, caching, and fallback for context limits
///
/// Uses Anthropic prompt caching to reduce costs (~90% savings on cached prompts)
/// and potentially improve reliability.
async fn call_llm_structured_with_fallback<T>(
    system: &str,
    user_full: &str,
    user_excerpt: &str,
    model: Model,
    schema_name: &str,
    schema: serde_json::Value,
    prefer_full_prompt: bool,
    max_tokens: u32,
    timeout_ms: u64,
) -> anyhow::Result<StructuredResponse<T>>
where
    T: serde::de::DeserializeOwned,
{
    // Default policy: prefer the excerpt to keep token usage bounded. Escalate to full-file
    // context only when the caller decides it is necessary (typically after an edit-apply error).
    let primary = if prefer_full_prompt {
        user_full
    } else {
        user_excerpt
    };

    let primary_result = if model == Model::Speed {
        call_llm_structured_limited_speed_with_failover::<T>(
            system,
            primary,
            schema_name,
            schema.clone(),
            max_tokens,
            timeout_ms,
        )
        .await
    } else {
        // Cached version - caches the system prompt for Anthropic models.
        call_llm_structured_cached::<T>(system, primary, model, schema_name, schema.clone()).await
    };

    match primary_result {
        Ok(response) => Ok(response),
        Err(err) => {
            let message = err.to_string();
            // Handle context limit by retrying with the shorter excerpt.
            if prefer_full_prompt && is_context_limit_error(&message) && user_full != user_excerpt {
                if model == Model::Speed {
                    call_llm_structured_limited_speed_with_failover::<T>(
                        system,
                        user_excerpt,
                        schema_name,
                        schema,
                        max_tokens,
                        timeout_ms,
                    )
                    .await
                } else {
                    call_llm_structured_cached::<T>(
                        system,
                        user_excerpt,
                        model,
                        schema_name,
                        schema,
                    )
                    .await
                }
            } else {
                Err(err)
            }
        }
    }
}

/// Generate the actual fixed code content based on a human-language plan.
/// Uses a search/replace approach for precise, validated edits.
pub async fn generate_fix_content_with_model(
    path: &Path,
    content: &str,
    suggestion: &Suggestion,
    plan: &FixPreview,
    repo_memory: Option<String>,
    is_new_file: bool,
    model: Model,
    timeout_ms: u64,
) -> anyhow::Result<AppliedFix> {
    let plan_text = build_plan_text(
        plan,
        if is_python_file(path) {
            Some(PYTHON_IMPLEMENTATION_GUARDRAILS)
        } else {
            None
        },
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

    let mut combined_usage: Option<Usage> = None;
    let mut last_apply_err: Option<String> = None;
    // Prefer excerpt-first to keep costs bounded; escalate to full-file prompt only after a
    // retryable edit-apply failure.
    let mut prefer_full_prompt = false;
    let slices = allocate_attempt_time_slices_ms(timeout_ms, MAX_EDIT_REPAIR_ATTEMPTS.max(1));

    for attempt in 1..=MAX_EDIT_REPAIR_ATTEMPTS.max(1) {
        let attempt_timeout_ms = slices
            .get(attempt.saturating_sub(1))
            .copied()
            .unwrap_or_else(|| timeout_ms.max(1));
        let (user_full_attempt, user_excerpt_attempt) = if let Some(ref err) = last_apply_err {
            let guidance = format_edit_apply_repair_guidance(err, "CURRENT CODE block");
            (
                format!("{}{}", user_full, guidance),
                format!("{}{}", user_excerpt, guidance),
            )
        } else {
            (user_full.clone(), user_excerpt.clone())
        };

        let response: StructuredResponse<FixResponse> = call_llm_structured_with_fallback(
            &fix_content_system(),
            &user_full_attempt,
            &user_excerpt_attempt,
            model,
            "fix_response",
            fix_response_schema(),
            prefer_full_prompt,
            max_tokens_for_fix_response(model),
            attempt_timeout_ms,
        )
        .await?;
        combined_usage = merge_usage(combined_usage, response.usage.clone());
        let speed_failover = response.speed_failover.clone();

        let description = response
            .data
            .description
            .unwrap_or_else(|| "Applied the requested fix".to_string());
        let modified_areas = response.data.modified_areas;
        let edits = response.data.edits;

        if edits.is_empty() {
            let message = "No edits provided in response".to_string();
            if attempt < MAX_EDIT_REPAIR_ATTEMPTS.max(1) {
                last_apply_err = Some(message);
                continue;
            }
            return Err(anyhow::anyhow!(message));
        }

        let anchor_line = plan
            .evidence_line
            .map(|l| l.max(1) as usize)
            .or(suggestion.line);
        let context_label = anchor_line
            .map(|line| format!("file (target around line {})", line))
            .unwrap_or_else(|| "file".to_string());

        match apply_edits_with_context(content, &edits, &context_label) {
            Ok(new_content) => {
                let new_content = normalize_generated_content(content, new_content, is_new_file);
                if new_content.trim().is_empty() {
                    return Err(anyhow::anyhow!("Generated content is empty"));
                }
                return Ok(AppliedFix {
                    description,
                    new_content,
                    modified_areas,
                    usage: combined_usage,
                    speed_failover,
                });
            }
            Err(err) => {
                let message = err.to_string();
                if attempt < MAX_EDIT_REPAIR_ATTEMPTS.max(1)
                    && is_retryable_edit_apply_error(&message)
                {
                    last_apply_err = Some(message);
                    // If we were using the excerpt prompt, try full-file context once on the next
                    // attempt to help the model choose a unique, verbatim anchor.
                    if !prefer_full_prompt {
                        prefer_full_prompt = true;
                    }
                    continue;
                }
                return Err(err);
            }
        }
    }

    Err(anyhow::anyhow!("Failed to generate applyable edits"))
}

/// Generate fixed code content using the default high-capability model.
pub async fn generate_fix_content(
    path: &Path,
    content: &str,
    suggestion: &Suggestion,
    plan: &FixPreview,
    repo_memory: Option<String>,
    is_new_file: bool,
) -> anyhow::Result<AppliedFix> {
    generate_fix_content_with_model(
        path,
        content,
        suggestion,
        plan,
        repo_memory,
        is_new_file,
        Model::Smart,
        60_000,
    )
    .await
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

        match find_unique_match_range(&new_content, &edit.old_string) {
            MatchRange::One { start, end } => {
                new_content.replace_range(start..end, &edit.new_string);
                continue;
            }
            MatchRange::Many(count) => {
                let contexts = match_contexts_for_error(&new_content, &edit.old_string, 2);
                return Err(anyhow::anyhow!(
                    "Edit {}: old_string matches {} times in {} (must be unique). Need more context.\nSearched for: {:?}{}",
                    i + 1,
                    count,
                    context_label,
                    truncate_for_error(&edit.old_string),
                    contexts
                ));
            }
            MatchRange::None => {}
        }

        // Normalize line endings when the file is CRLF but the model emitted LF.
        if edit.old_string.contains('\n') && new_content.contains("\r\n") {
            let crlf_old = edit.old_string.replace('\n', "\r\n");
            match find_unique_match_range(&new_content, &crlf_old) {
                MatchRange::One { start, end } => {
                    let replacement = edit.new_string.replace('\n', "\r\n");
                    new_content.replace_range(start..end, &replacement);
                    continue;
                }
                MatchRange::Many(count) => {
                    let contexts = match_contexts_for_error(&new_content, &crlf_old, 2);
                    return Err(anyhow::anyhow!(
                        "Edit {}: normalized old_string matches {} times in {} (must be unique).\nSearched for: {:?}{}",
                        i + 1,
                        count,
                        context_label,
                        truncate_for_error(&edit.old_string),
                        contexts
                    ));
                }
                MatchRange::None => {}
            }
        }

        // Tolerate boundary whitespace mismatches if a unique trimmed anchor exists.
        let trimmed_old = edit.old_string.trim();
        if !trimmed_old.is_empty() && trimmed_old != edit.old_string {
            match find_unique_match_range(&new_content, trimmed_old) {
                MatchRange::One { start, end } => {
                    new_content.replace_range(start..end, &edit.new_string);
                    continue;
                }
                MatchRange::Many(count) => {
                    let contexts = match_contexts_for_error(&new_content, trimmed_old, 2);
                    return Err(anyhow::anyhow!(
                        "Edit {}: trimmed old_string matches {} times in {} (must be unique).\nSearched for: {:?}{}",
                        i + 1,
                        count,
                        context_label,
                        truncate_for_error(&edit.old_string),
                        contexts
                    ));
                }
                MatchRange::None => {}
            }
        }

        return Err(anyhow::anyhow!(
            "Edit {}: old_string not found in {}. The LLM may have made an error.\nSearched for: {:?}",
            i + 1,
            context_label,
            truncate_for_error(&edit.old_string)
        ));
    }

    Ok(new_content)
}

enum MatchRange {
    None,
    One { start: usize, end: usize },
    Many(usize),
}

fn find_unique_match_range(content: &str, needle: &str) -> MatchRange {
    let matches = content.match_indices(needle).collect::<Vec<_>>();
    match matches.len() {
        0 => MatchRange::None,
        1 => {
            let (start, matched) = matches[0];
            MatchRange::One {
                start,
                end: start + matched.len(),
            }
        }
        n => MatchRange::Many(n),
    }
}

fn byte_offset_to_line_number(content: &str, byte_offset: usize) -> usize {
    // Lines are 1-based for human readability.
    content
        .as_bytes()
        .iter()
        .take(byte_offset.min(content.len()))
        .filter(|b| **b == b'\n')
        .count()
        + 1
}

fn snippet_around_line_numbered(
    content: &str,
    line_number: usize,
    before: usize,
    after: usize,
) -> String {
    if line_number == 0 {
        return String::new();
    }
    let lines = content.lines().collect::<Vec<_>>();
    if lines.is_empty() {
        return String::new();
    }
    let idx = line_number.saturating_sub(1);
    if idx >= lines.len() {
        return String::new();
    }

    let start = idx.saturating_sub(before);
    let end = (idx + after + 1).min(lines.len());
    let mut out = String::new();
    for (offset, line) in lines[start..end].iter().enumerate() {
        let ln = start + offset + 1;
        out.push_str(&format!("{:4}| {}\n", ln, line));
    }

    // Keep this bounded; it's only meant to help the model choose a unique anchor.
    const MAX_SNIPPET_CHARS: usize = 700;
    truncate_content(&out, MAX_SNIPPET_CHARS)
}

fn match_contexts_for_error(content: &str, needle: &str, max_matches: usize) -> String {
    if max_matches == 0 || needle.is_empty() {
        return String::new();
    }
    let mut matches = content
        .match_indices(needle)
        .take(max_matches)
        .collect::<Vec<_>>();
    if matches.is_empty() {
        return String::new();
    }

    // Include a small, numbered excerpt around each match so the model can pick a unique anchor
    // without needing the entire file.
    let mut out = String::new();
    out.push_str("\n\nMatch contexts (first occurrences):");
    for (idx, (start, _)) in matches.drain(..).enumerate() {
        let line = byte_offset_to_line_number(content, start);
        let snippet = snippet_around_line_numbered(content, line, 2, 3);
        if snippet.trim().is_empty() {
            continue;
        }
        out.push_str(&format!(
            "\n- Match {} around line {}:\n{}",
            idx + 1,
            line,
            snippet
        ));
    }
    out
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
    /// Speed-tier provider failover diagnostics for transparency (if applicable).
    pub speed_failover: Option<SpeedFailoverDiagnostics>,
}

/// Input for a single file in a multi-file fix
#[derive(Debug, Clone)]
pub struct FileInput {
    pub path: PathBuf,
    pub content: String,
    pub is_new: bool,
}

/// Edits for a single file in the JSON response
#[derive(Debug, Clone, Deserialize, Serialize)]
struct FileEditsJson {
    file: String,
    edits: Vec<EditOp>,
}

/// Response structure for multi-file fix generation
#[derive(Debug, Clone, Deserialize, Serialize)]
struct MultiFileFixResponse {
    #[serde(default)]
    description: Option<String>,
    file_edits: Vec<FileEditsJson>,
}

/// JSON Schema for MultiFileFixResponse - used for structured output
fn multi_file_fix_response_schema() -> serde_json::Value {
    json!({
        "type": "object",
        "properties": {
            "description": {
                "type": "string",
                "description": "Brief description of what was changed across files"
            },
            "file_edits": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "file": {
                            "type": "string",
                            "description": "Path to the file being edited"
                        },
                        "edits": {
                            "type": "array",
                            "items": {
                                "type": "object",
                                "properties": {
                                    "old_string": {
                                        "type": "string",
                                        "description": "Exact text to find (must match exactly once)"
                                    },
                                    "new_string": {
                                        "type": "string",
                                        "description": "Replacement text"
                                    }
                                },
                                "required": ["old_string", "new_string"],
                                "additionalProperties": false
                            },
                            "description": "Search/replace edit operations for this file"
                        }
                    },
                    "required": ["file", "edits"],
                    "additionalProperties": false
                },
                "description": "Edits grouped by file"
            }
        },
        "required": ["file_edits"],
        "additionalProperties": false
    })
}

/// Generate coordinated fixes across multiple files
///
/// This function handles multi-file refactors like:
/// - Renaming a function and updating all callers
/// - Extracting shared code and updating imports
/// - Interface changes that affect multiple files
pub async fn generate_multi_file_fix_with_model(
    files: &[FileInput],
    suggestion: &Suggestion,
    plan: &FixPreview,
    repo_memory: Option<String>,
    model: Model,
    timeout_ms: u64,
) -> anyhow::Result<MultiFileAppliedFix> {
    if files.is_empty() {
        return Err(anyhow::anyhow!("No files provided for multi-file fix"));
    }
    let per_file_budget = prompt_budget_per_file(files.len());

    let plan_text = build_plan_text(
        plan,
        if files.iter().any(|f| is_python_file(&f.path)) {
            Some(PYTHON_IMPLEMENTATION_GUARDRAILS)
        } else {
            None
        },
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

    let mut combined_usage: Option<Usage> = None;
    let mut last_apply_err: Option<String> = None;
    // Prefer excerpt-first; only allow full prompt if it is reasonably sized.
    let allow_full_prompt = files_section_full.chars().count() <= MAX_MULTI_FILE_EXCERPT_CHARS;
    let mut prefer_full_prompt = false;
    let slices = allocate_attempt_time_slices_ms(timeout_ms, MAX_EDIT_REPAIR_ATTEMPTS.max(1));

    for attempt in 1..=MAX_EDIT_REPAIR_ATTEMPTS.max(1) {
        let attempt_timeout_ms = slices
            .get(attempt.saturating_sub(1))
            .copied()
            .unwrap_or_else(|| timeout_ms.max(1));
        let (user_full_attempt, user_excerpt_attempt) = if let Some(ref err) = last_apply_err {
            let guidance = format_edit_apply_repair_guidance(err, "FILES TO MODIFY code blocks");
            (
                format!("{}{}", user_full, guidance),
                format!("{}{}", user_excerpt, guidance),
            )
        } else {
            (user_full.clone(), user_excerpt.clone())
        };

        let response: StructuredResponse<MultiFileFixResponse> = call_llm_structured_with_fallback(
            &multi_file_fix_system(),
            &user_full_attempt,
            &user_excerpt_attempt,
            model,
            "multi_file_fix_response",
            multi_file_fix_response_schema(),
            prefer_full_prompt,
            max_tokens_for_multi_file_fix_response(model),
            attempt_timeout_ms,
        )
        .await?;
        combined_usage = merge_usage(combined_usage, response.usage.clone());
        let speed_failover = response.speed_failover.clone();

        let description = response
            .data
            .description
            .unwrap_or_else(|| "Applied the requested multi-file fix".to_string());
        let file_edits_json = response.data.file_edits;

        if file_edits_json.is_empty() {
            let message = "No file edits provided in response".to_string();
            if attempt < MAX_EDIT_REPAIR_ATTEMPTS.max(1) {
                last_apply_err = Some(message);
                continue;
            }
            return Err(anyhow::anyhow!(message));
        }

        let mut file_edits = Vec::new();
        let mut apply_error: Option<anyhow::Error> = None;

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

            let anchor_line = if file_path == suggestion.file {
                plan.evidence_line
                    .map(|l| l.max(1) as usize)
                    .or(suggestion.line)
            } else {
                None
            };
            let context = if let Some(line) = anchor_line {
                format!("file {} (target around line {})", file_path.display(), line)
            } else {
                format!("file {}", file_path.display())
            };
            let new_content =
                match apply_edits_with_context(&new_content, &file_edit_json.edits, &context) {
                    Ok(value) => value,
                    Err(err) => {
                        apply_error = Some(err);
                        break;
                    }
                };

            let new_content =
                normalize_generated_content(&file_input.content, new_content, file_input.is_new);
            file_edits.push(FileEdit {
                path: file_path,
                new_content,
                modified_areas,
            });
        }

        if let Some(err) = apply_error {
            let message = err.to_string();
            if attempt < MAX_EDIT_REPAIR_ATTEMPTS.max(1) && is_retryable_edit_apply_error(&message)
            {
                last_apply_err = Some(message);
                if allow_full_prompt && !prefer_full_prompt {
                    prefer_full_prompt = true;
                }
                continue;
            }
            return Err(err);
        }

        return Ok(MultiFileAppliedFix {
            description,
            file_edits,
            usage: combined_usage,
            speed_failover,
        });
    }

    Err(anyhow::anyhow!(
        "Failed to generate applyable multi-file edits"
    ))
}

/// Generate coordinated fixes across multiple files with the default model.
pub async fn generate_multi_file_fix(
    files: &[FileInput],
    suggestion: &Suggestion,
    plan: &FixPreview,
    repo_memory: Option<String>,
) -> anyhow::Result<MultiFileAppliedFix> {
    generate_multi_file_fix_with_model(files, suggestion, plan, repo_memory, Model::Smart, 60_000)
        .await
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
    /// Explicit verification contract result.
    pub verification_state: crate::suggest::VerificationState,

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

/// Build a lightweight fix plan from an already-validated suggestion.
///
/// This is used by the real-app runtime when the legacy pre-apply Verify stage
/// is bypassed. The plan preserves suggestion grounding and feeds fix generation.
pub fn build_fix_preview_from_validated_suggestion(suggestion: &Suggestion) -> FixPreview {
    let affected_areas = if suggestion.additional_files.is_empty() {
        vec![suggestion.file.display().to_string()]
    } else {
        suggestion
            .affected_files()
            .iter()
            .map(|path| path.display().to_string())
            .collect()
    };
    let description = suggestion
        .detail
        .clone()
        .unwrap_or_else(|| suggestion.summary.clone());
    let outcome = suggestion
        .detail
        .as_deref()
        .and_then(|detail| detail.lines().next())
        .map(|line| line.trim().to_string())
        .filter(|line| !line.is_empty())
        .unwrap_or_else(|| suggestion.summary.clone());

    FixPreview {
        verified: true,
        verification_state: crate::suggest::VerificationState::Verified,
        friendly_title: suggestion.kind.label().to_string(),
        problem_summary: suggestion.summary.clone(),
        outcome,
        verification_note: "Using pre-validated suggestion evidence.".to_string(),
        evidence_snippet: suggestion.evidence.clone(),
        evidence_line: suggestion.line.map(|line| line as u32),
        description,
        affected_areas,
        scope: FixScope::Medium,
        modifier: None,
    }
}

/// JSON Schema for FixPreview - used for structured output on final agentic response
/// This ensures the LLM returns valid, parseable JSON matching our expected format
pub(crate) fn fix_preview_schema() -> serde_json::Value {
    json!({
        "type": "object",
        "properties": {
            "verified": {
                "type": "boolean",
                "description": "Whether the issue was verified to exist in the code"
            },
            "verification_state": {
                "type": "string",
                "enum": ["verified", "contradicted", "insufficient_evidence"],
                "description": "Explicit verification contract result"
            },
            "friendly_title": {
                "type": "string",
                "description": "Friendly topic name for non-technical users"
            },
            "problem_summary": {
                "type": "string",
                "description": "Behavior-focused problem description"
            },
            "outcome": {
                "type": "string",
                "description": "What happens after the fix"
            },
            "verification_note": {
                "type": "string",
                "description": "Explanation of verification result"
            },
            "evidence_snippet": {
                "type": ["string", "null"],
                "description": "Code snippet that proves the claim"
            },
            "evidence_line": {
                "type": ["integer", "null"],
                "description": "Starting line number of the evidence snippet"
            },
            "description": {
                "type": "string",
                "description": "Human-readable description of what will change"
            },
            "affected_areas": {
                "type": "array",
                "items": { "type": "string" },
                "description": "Which functions/areas are affected"
            },
            "scope": {
                "type": "string",
                "enum": ["small", "medium", "large"],
                "description": "Estimated scope of the fix"
            }
        },
        "required": ["verification_state", "friendly_title", "problem_summary", "outcome", "verification_note", "description", "affected_areas", "scope"],
        "additionalProperties": false
    })
}

/// Response structure for fix preview (for structured output parsing)
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct FixPreviewJson {
    #[serde(default)]
    pub verified: bool,
    #[serde(default)]
    pub verification_state: String,
    #[serde(default)]
    pub friendly_title: String,
    #[serde(default)]
    pub problem_summary: String,
    #[serde(default)]
    pub outcome: String,
    #[serde(default)]
    pub verification_note: String,
    pub evidence_snippet: Option<String>,
    pub evidence_line: Option<u32>,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub affected_areas: Vec<String>,
    #[serde(default = "default_scope")]
    pub scope: String,
}

fn default_scope() -> String {
    "medium".to_string()
}

fn parse_verification_state(
    verification_state: &str,
    verified_fallback: bool,
) -> crate::suggest::VerificationState {
    match verification_state.trim().to_lowercase().as_str() {
        "verified" => crate::suggest::VerificationState::Verified,
        "contradicted" => crate::suggest::VerificationState::Contradicted,
        "insufficient_evidence" => crate::suggest::VerificationState::InsufficientEvidence,
        // Backward compatibility for older responses that only include `verified`
        _ => {
            if verified_fallback {
                crate::suggest::VerificationState::Verified
            } else {
                crate::suggest::VerificationState::Contradicted
            }
        }
    }
}

fn fix_preview_from_json(parsed: FixPreviewJson, modifier: Option<&str>) -> FixPreview {
    let verification_state = parse_verification_state(&parsed.verification_state, parsed.verified);
    let scope = match parsed.scope.as_str() {
        "small" => FixScope::Small,
        "large" => FixScope::Large,
        _ => FixScope::Medium,
    };

    FixPreview {
        verified: verification_state == crate::suggest::VerificationState::Verified,
        verification_state,
        friendly_title: if parsed.friendly_title.is_empty() {
            "Issue".to_string()
        } else {
            parsed.friendly_title
        },
        problem_summary: if parsed.problem_summary.is_empty() {
            "An issue was found that needs attention.".to_string()
        } else {
            parsed.problem_summary
        },
        outcome: if parsed.outcome.is_empty() {
            "This will be fixed.".to_string()
        } else {
            parsed.outcome
        },
        verification_note: if parsed.verification_note.is_empty() {
            if parsed.verified {
                "Issue verified".to_string()
            } else {
                "Issue not found".to_string()
            }
        } else {
            parsed.verification_note
        },
        evidence_snippet: parsed.evidence_snippet.filter(|s| !s.trim().is_empty()),
        evidence_line: parsed.evidence_line,
        description: if parsed.description.is_empty() {
            "Fix the identified issue".to_string()
        } else {
            parsed.description
        },
        affected_areas: parsed.affected_areas,
        scope,
        modifier: modifier.map(String::from),
    }
}

fn preview_target_line_for_suggestion(suggestion: &Suggestion, line_count: usize) -> usize {
    let line_count = line_count.max(1);
    let fallback_line = suggestion.line.unwrap_or(1).max(1).min(line_count);
    let evidence_line = suggestion
        .evidence_refs
        .iter()
        .filter(|r| r.file == suggestion.file)
        .map(|r| r.line)
        .min_by_key(|line| line.abs_diff(fallback_line));
    evidence_line
        .unwrap_or(fallback_line)
        .max(1)
        .min(line_count)
}

/// Generate a preview using lean hybrid verification.
///
/// Strategy:
/// 1. Pre-read the relevant section of the file (we know exactly where to look)
/// 2. Include that code directly in the prompt
/// 3. Model verifies with the code in front of it
/// 4. Allow 1-2 surgical tool calls if model needs more context
///
/// This is faster than full agentic because we already know the file and location.
pub async fn generate_fix_preview_agentic(
    repo_root: &Path,
    suggestion: &Suggestion,
    modifier: Option<&str>,
    repo_memory: Option<String>,
) -> anyhow::Result<(FixPreview, Option<Usage>)> {
    let modifier_text = modifier
        .map(|m| format!("\n\nUser modification request: {}", m))
        .unwrap_or_default();

    let memory_section =
        format_repo_memory_section(repo_memory.as_deref(), "Repo conventions / decisions");

    // Pre-read the relevant file content (we know exactly where to look)
    let file_path = repo_root.join(&suggestion.file);
    let file_content = std::fs::read_to_string(&file_path).unwrap_or_default();
    let lines: Vec<&str> = file_content.lines().collect();
    let target_line = preview_target_line_for_suggestion(suggestion, lines.len());

    let render_excerpt = |line: usize, before: usize, after: usize| -> (usize, usize, String) {
        if lines.is_empty() {
            return (1, 1, String::new());
        }
        let start = line.saturating_sub(before).max(1);
        let end = (line + after).min(lines.len());
        let snippet = lines
            .iter()
            .enumerate()
            .skip(start - 1)
            .take(end - start + 1)
            .map(|(i, line)| format!("{:4}| {}", i + 1, line))
            .collect::<Vec<_>>()
            .join("\n");
        (start, end, snippet)
    };

    let evidence_context = {
        let mut block = String::new();
        for (idx, reference) in suggestion.evidence_refs.iter().take(3).enumerate() {
            block.push_str(&format!(
                "- Evidence {}: {}:{} (id={})\n",
                idx + 1,
                reference.file.display(),
                reference.line,
                reference.snippet_id
            ));
        }
        if let Some(snippet) = suggestion.evidence.as_deref() {
            block.push_str("\nPrimary evidence snippet:\n");
            block.push_str(snippet);
        }
        block
    };

    let build_user_prompt = |start: usize, end: usize, code_section: &str, fallback: bool| {
        format!(
            r#"ISSUE TO VERIFY:
File: {}
Line: ~{}
Summary: {}
{}
{}{}

EVIDENCE REFERENCES:
{}

CODE (lines {}-{}):
{}

VERIFY:
1. Does this issue exist in the code above?
2. If you need more context:
   • grep -n 'pattern' {} → find related code
   • sed -n 'X,Yp' {} → read specific lines
3. Return JSON immediately (minimize tool calls).{}"#,
            suggestion.file.display(),
            target_line,
            suggestion.summary,
            suggestion.detail.as_deref().unwrap_or(""),
            memory_section,
            modifier_text,
            evidence_context,
            start,
            end,
            code_section,
            suggestion.file.display(),
            suggestion.file.display(),
            if fallback {
                " If uncertain, prefer insufficient_evidence over contradicted."
            } else {
                ""
            }
        )
    };

    let (start, end, code_section) = render_excerpt(target_line, 30, 50);
    let user = build_user_prompt(start, end, &code_section, false);

    // Use structured output to guarantee valid JSON response
    let response_format = schema_to_response_format("fix_preview", fix_preview_schema());

    // Use Speed model with high reasoning effort for cost-effective fix planning
    // 3 iterations - code already provided, minimal exploration needed
    let response = call_llm_agentic(
        FIX_PREVIEW_AGENTIC_SYSTEM,
        &user,
        Model::Speed,
        repo_root,
        false,
        3, // max iterations - verification has code upfront
        Some(response_format.clone()),
    )
    .await?;

    // Response is guaranteed to be valid JSON matching the schema
    let parsed: FixPreviewJson = serde_json::from_str(&response.content).map_err(|e| {
        anyhow::anyhow!(
            "Failed to parse fix preview response: {}. Content: {}",
            e,
            &response.content.chars().take(200).collect::<String>()
        )
    })?;

    let mut preview = fix_preview_from_json(parsed, modifier);
    let mut usage = response.usage;

    if preview.verification_state == crate::suggest::VerificationState::Contradicted {
        // One bounded fallback pass with broader context to reduce false contradictions.
        let (fallback_start, fallback_end, fallback_code) = render_excerpt(target_line, 120, 160);
        let fallback_user = build_user_prompt(fallback_start, fallback_end, &fallback_code, true);
        if let Ok(fallback_response) = call_llm_agentic(
            FIX_PREVIEW_AGENTIC_SYSTEM,
            &fallback_user,
            Model::Balanced,
            repo_root,
            false,
            4,
            Some(response_format.clone()),
        )
        .await
        {
            usage = merge_usage(usage, fallback_response.usage);
            if let Ok(parsed_fallback) =
                serde_json::from_str::<FixPreviewJson>(&fallback_response.content)
            {
                preview = fix_preview_from_json(parsed_fallback, modifier);
            }
        }
    }

    Ok((preview, usage))
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::suggest::{Priority, SuggestionEvidenceRef, SuggestionKind, SuggestionSource};
    use std::path::{Path, PathBuf};

    fn sample_preview(evidence_line: Option<u32>) -> FixPreview {
        FixPreview {
            verified: true,
            verification_state: crate::suggest::VerificationState::Verified,
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
    fn test_build_plan_text_adds_python_guardrails_only_when_requested() {
        let preview = sample_preview(Some(12));
        let no_extra = build_plan_text(&preview, None);
        assert!(!no_extra.contains("Python guardrails:"), "{}", no_extra);

        let with_extra = build_plan_text(&preview, Some(PYTHON_IMPLEMENTATION_GUARDRAILS));
        assert!(with_extra.contains("Python guardrails:"), "{}", with_extra);
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
    fn test_apply_edits_uses_trimmed_fallback_unique_match() {
        let edits = vec![EditOp {
            old_string: "    let value = compute();\n".to_string(),
            new_string: "    let value = compute_fast();\n".to_string(),
        }];
        let content = "let value = compute();\n";
        let updated = apply_edits_with_context(content, &edits, "file").unwrap();
        assert!(updated.contains("compute_fast"));
    }

    #[test]
    fn test_apply_edits_handles_crlf_old_string_normalization() {
        let edits = vec![EditOp {
            old_string: "let a = 1;\nlet b = 2;\n".to_string(),
            new_string: "let a = 1;\nlet b = 3;\n".to_string(),
        }];
        let content = "let a = 1;\r\nlet b = 2;\r\n";
        let updated = apply_edits_with_context(content, &edits, "file").unwrap();
        assert!(updated.contains("let b = 3;"));
        assert!(updated.contains("\r\n"));
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
    fn test_preview_target_line_prefers_same_file_evidence_ref() {
        let mut suggestion = sample_suggestion(PathBuf::from("src/lib.rs"));
        suggestion.line = Some(12);
        suggestion.evidence_refs = vec![
            SuggestionEvidenceRef {
                snippet_id: 1,
                file: PathBuf::from("src/other.rs"),
                line: 80,
            },
            SuggestionEvidenceRef {
                snippet_id: 2,
                file: PathBuf::from("src/lib.rs"),
                line: 45,
            },
        ];

        let target = preview_target_line_for_suggestion(&suggestion, 120);
        assert_eq!(target, 45);
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
