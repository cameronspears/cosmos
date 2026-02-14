use super::agentic::{call_llm_agentic, schema_to_response_format};
use super::client::{
    call_llm_structured_limited_no_reasoning, call_llm_structured_limited_speed_with_failover,
    call_llm_with_usage, parse_structured_content, SpeedFailoverDiagnostics, StructuredResponse,
};
use super::fix::{
    apply_edits_with_context, fix_response_schema, format_edit_apply_repair_guidance,
    normalize_generated_content, AppliedFix, FixResponse,
};
use super::models::{merge_usage, Model, Usage};
use super::parse::{truncate_content, truncate_content_around_line};
use super::prompt_utils::format_repo_memory_section;
use super::prompts::{review_fix_system_prompt, review_system_prompt};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::time::Duration;

/// Context about what a fix was supposed to accomplish
/// Used to help the reviewer evaluate whether the fix was done correctly
#[derive(Debug, Clone, Default)]
pub struct FixContext {
    /// What the problem was (in plain English)
    pub problem_summary: String,
    /// What the fix was supposed to achieve
    pub outcome: String,
    /// Technical description of what was changed
    pub description: String,
    /// Which areas/functions were modified
    pub modified_areas: Vec<String>,
}

// ============================================================================
// Deep Verification Review (Sweet Spot Flow)
// ============================================================================
// Flow: Reviewer reviews → User sees findings → User selects → Smart fixes → Done

/// A finding from the adversarial code reviewer
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReviewFinding {
    pub file: String,
    pub line: Option<u32>,
    pub severity: String,    // "critical", "warning", "suggestion", "nitpick"
    pub category: String,    // "bug", "security", "performance", "logic", "error-handling", "style"
    pub title: String,       // Short title
    pub description: String, // Detailed explanation in plain language
    pub recommended: bool, // Reviewer recommends fixing this (true = should fix, false = optional)
}

/// Response structure for code review (used for structured output parsing)
#[derive(Debug, Clone, Deserialize, Serialize)]
struct ReviewResponseJson {
    #[serde(default = "default_review_summary")]
    summary: String,
    #[serde(default)]
    findings: Vec<ReviewFindingJson>,
}

/// Finding structure for JSON parsing (with defaults for robustness)
#[derive(Debug, Clone, Deserialize, Serialize)]
struct ReviewFindingJson {
    #[serde(default)]
    file: String,
    #[serde(default)]
    line: Option<u32>,
    #[serde(default = "default_severity")]
    severity: String,
    #[serde(default)]
    category: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    description: String,
    #[serde(default = "default_recommended")]
    recommended: bool,
}

fn default_severity() -> String {
    "warning".to_string()
}

fn default_recommended() -> bool {
    true
}

impl From<ReviewFindingJson> for ReviewFinding {
    fn from(json: ReviewFindingJson) -> Self {
        ReviewFinding {
            file: json.file,
            line: json.line,
            severity: json.severity,
            category: json.category,
            title: json.title,
            description: json.description,
            recommended: json.recommended,
        }
    }
}

fn default_review_summary() -> String {
    "Review completed".to_string()
}

/// JSON Schema for ReviewResponse - used for structured output
/// This ensures the LLM returns valid, parseable JSON matching our expected format
pub(crate) fn review_response_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "summary": {
                "type": "string",
                "description": "Brief overall assessment of the code changes"
            },
            "findings": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "file": {
                            "type": "string",
                            "description": "Path to the file containing the issue"
                        },
                        "line": {
                            "type": ["integer", "null"],
                            "description": "Line number where the issue occurs"
                        },
                        "severity": {
                            "type": "string",
                            "enum": ["critical", "warning", "suggestion", "nitpick"],
                            "description": "Severity level of the finding"
                        },
                        "category": {
                            "type": "string",
                            "description": "Category like bug, security, performance, logic, error-handling, style"
                        },
                        "title": {
                            "type": "string",
                            "description": "Short title for the finding"
                        },
                        "description": {
                            "type": "string",
                            "description": "Detailed explanation in plain language"
                        },
                        "recommended": {
                            "type": "boolean",
                            "description": "Whether the reviewer recommends fixing this"
                        }
                    },
                    "required": ["severity", "title", "description", "recommended"],
                    "additionalProperties": false
                },
                "description": "List of issues found in the code"
            }
        },
        "required": ["summary", "findings"],
        "additionalProperties": false
    })
}

fn is_response_format_schema_error_text(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    lower.contains("invalid schema for response_format")
        || (lower.contains("invalid schema") && lower.contains("response_format"))
}

fn validate_review_response_semantics(parsed: &ReviewResponseJson) -> anyhow::Result<()> {
    const ALLOWED_SEVERITIES: &[&str] = &["critical", "warning", "suggestion", "nitpick"];
    for finding in &parsed.findings {
        let severity = finding.severity.trim().to_ascii_lowercase();
        if !ALLOWED_SEVERITIES.contains(&severity.as_str()) {
            return Err(anyhow::anyhow!(
                "Invalid review severity '{}'",
                finding.severity
            ));
        }
        if finding.title.trim().is_empty() {
            return Err(anyhow::anyhow!("Review finding title is empty"));
        }
        if finding.description.trim().is_empty() {
            return Err(anyhow::anyhow!("Review finding description is empty"));
        }
    }
    Ok(())
}

/// Result of a deep verification review
#[derive(Debug, Clone)]
pub struct VerificationReview {
    pub findings: Vec<ReviewFinding>,
    pub summary: String, // Overall assessment
    pub usage: Option<Usage>,
    pub speed_failover: Option<SpeedFailoverDiagnostics>,
    pub schema_fallback_used: bool,
}

/// Perform lean adversarial review of code changes
///
/// Uses the lean hybrid approach:
/// 1. Start with compact diff summary (not full files)
/// 2. Model can surgically read specific sections if needed
/// 3. Fast response with high accuracy
///
/// The `fix_context` parameter (when provided) tells the reviewer what the fix was
/// supposed to accomplish, allowing it to evaluate whether the fix was done correctly.
pub async fn verify_changes(
    files_with_content: &[(PathBuf, String, String)], // (path, old_content, new_content)
    iteration: u32,
    fixed_titles: &[String],
    fix_context: Option<&FixContext>,
) -> anyhow::Result<VerificationReview> {
    // Get repo root from first file path
    let repo_root = files_with_content
        .first()
        .and_then(|(p, _, _)| {
            // Walk up to find .git directory
            let mut path = p.as_path();
            while let Some(parent) = path.parent() {
                if parent.join(".git").exists() {
                    return Some(parent.to_path_buf());
                }
                path = parent;
            }
            None
        })
        .ok_or_else(|| {
            anyhow::anyhow!(
                "Could not locate repository root (.git directory) from file paths. \
                 Ensure files are within a git repository."
            )
        })?;

    let system = review_system_prompt(iteration, fixed_titles, fix_context);

    // Build compact diff summary (not full content)
    let user = build_lean_review_prompt(files_with_content, fix_context);

    // Use structured output to guarantee valid JSON response
    let response_format = schema_to_response_format("review_response", review_response_schema());

    // Use Speed model with high reasoning effort for cost-effective review
    // 4 iterations - diff already provided, occasional context needed
    let response = call_llm_agentic(
        &system,
        &user,
        Model::Speed,
        &repo_root,
        false,
        4,
        Some(response_format),
    )
    .await?;

    // Response is guaranteed to be valid JSON matching the schema
    let parsed: ReviewResponseJson = parse_structured_content(&response.content).map_err(|e| {
        anyhow::anyhow!(
            "Failed to parse review response: {}. Content: {}",
            e,
            &response.content.chars().take(200).collect::<String>()
        )
    })?;

    Ok(VerificationReview {
        findings: parsed.findings.into_iter().map(Into::into).collect(),
        summary: parsed.summary,
        usage: response.usage,
        speed_failover: None,
        schema_fallback_used: false,
    })
}

/// Perform a bounded adversarial review suitable for strict apply harness loops.
///
/// This avoids agentic multi-step execution to keep cost/time predictable, while still
/// requiring structured JSON output.
pub async fn verify_changes_bounded(
    files_with_content: &[(PathBuf, String, String)], // (path, old_content, new_content)
    iteration: u32,
    fixed_titles: &[String],
    fix_context: Option<&FixContext>,
    timeout_ms: u64,
) -> anyhow::Result<VerificationReview> {
    verify_changes_bounded_with_model(
        files_with_content,
        iteration,
        fixed_titles,
        fix_context,
        Model::Speed,
        timeout_ms,
    )
    .await
}

/// Same as `verify_changes_bounded`, but allows explicitly selecting the reviewer model.
/// The strict implementation harness chooses this model explicitly:
/// default is `Model::Smart` for independent review quality, and if callers opt into
/// `Model::Speed` the harness can still require a final independent Smart pass.
pub async fn verify_changes_bounded_with_model(
    files_with_content: &[(PathBuf, String, String)], // (path, old_content, new_content)
    iteration: u32,
    fixed_titles: &[String],
    fix_context: Option<&FixContext>,
    model: Model,
    timeout_ms: u64,
) -> anyhow::Result<VerificationReview> {
    let system = review_system_prompt(iteration, fixed_titles, fix_context);
    let user = build_lean_review_prompt(files_with_content, fix_context);

    // Keep review cheap and predictable. The harness will re-run review after fixes.
    const MAX_TOKENS: u32 = 1200;
    let structured: anyhow::Result<StructuredResponse<ReviewResponseJson>> =
        if model == Model::Speed {
            call_llm_structured_limited_speed_with_failover(
                &system,
                &user,
                "review_response",
                review_response_schema(),
                MAX_TOKENS,
                timeout_ms,
            )
            .await
        } else {
            call_llm_structured_limited_no_reasoning(
                &system,
                &user,
                model,
                "review_response",
                review_response_schema(),
                MAX_TOKENS,
                timeout_ms,
            )
            .await
        };

    match structured {
        Ok(response) => {
            validate_review_response_semantics(&response.data)?;
            Ok(VerificationReview {
                findings: response.data.findings.into_iter().map(Into::into).collect(),
                summary: response.data.summary,
                usage: response.usage,
                speed_failover: response.speed_failover,
                schema_fallback_used: false,
            })
        }
        Err(err) => {
            let err_text = err.to_string();
            if !is_response_format_schema_error_text(&err_text) {
                return Err(err);
            }

            // Structured output fallback: ask for plain JSON and parse/validate locally.
            let fallback_user = format!(
                "{}\n\nFORMAT REQUIREMENT:\nReturn JSON only (no markdown fences) with this exact shape:\n{{\"summary\": string, \"findings\": [{{\"file\": string, \"line\": number|null, \"severity\": \"critical|warning|suggestion|nitpick\", \"category\": string, \"title\": string, \"description\": string, \"recommended\": boolean}}]}}",
                user
            );
            let fallback_call = tokio::time::timeout(
                Duration::from_millis(timeout_ms.max(1)),
                call_llm_with_usage(&system, &fallback_user, model, false),
            )
            .await;
            let fallback_response = match fallback_call {
                Ok(Ok(value)) => value,
                Ok(Err(fallback_err)) => return Err(fallback_err),
                Err(_) => {
                    return Err(anyhow::anyhow!(
                        "Review schema fallback timed out after {}ms",
                        timeout_ms
                    ));
                }
            };
            let parsed: ReviewResponseJson = parse_structured_content(&fallback_response.content)
                .map_err(|parse_err| {
                anyhow::anyhow!("Review schema fallback parse failed: {}", parse_err)
            })?;
            validate_review_response_semantics(&parsed)?;
            Ok(VerificationReview {
                findings: parsed.findings.into_iter().map(Into::into).collect(),
                summary: parsed.summary,
                usage: fallback_response.usage,
                speed_failover: None,
                schema_fallback_used: true,
            })
        }
    }
}

/// Build a lean review prompt with diff summary instead of full content
fn build_lean_review_prompt(
    files_with_content: &[(PathBuf, String, String)],
    fix_context: Option<&FixContext>,
) -> String {
    let mut sections = Vec::new();

    // Fix context if available
    if let Some(ctx) = fix_context {
        sections.push(format!(
            "REVIEWING FIX FOR:\nProblem: {}\nIntended outcome: {}\nChanges made: {}",
            ctx.problem_summary, ctx.outcome, ctx.description
        ));
    }

    // For each file, show a compact diff (changed lines only with context)
    sections.push(String::from("\nCHANGED FILES:"));

    for (path, old_content, new_content) in files_with_content {
        let file_name = path.display().to_string();
        let diff = compute_compact_diff(old_content, new_content);

        if old_content.is_empty() {
            // New file - show first 50 lines
            let preview: String = new_content.lines().take(50).collect::<Vec<_>>().join("\n");
            sections.push(format!(
                "\n=== {} (NEW FILE, {} lines) ===\n{}{}",
                file_name,
                new_content.lines().count(),
                add_line_numbers(&preview),
                if new_content.lines().count() > 50 {
                    "\n... (use head/tail to see more)"
                } else {
                    ""
                }
            ));
        } else if diff.is_empty() {
            sections.push(format!("\n=== {} (no changes) ===", file_name));
        } else {
            sections.push(format!(
                "\n=== {} ({} lines changed) ===\n{}",
                file_name,
                diff.lines()
                    .filter(|l| l.starts_with('+') || l.starts_with('-'))
                    .count(),
                diff
            ));
        }
    }

    // Instructions
    sections.push(String::from(
        "\n\nREVIEW TASK:
Find bugs, logic errors, and issues in the diff above.

SURGICAL COMMANDS (if needed):
• grep -n 'pattern' <file> → find related code
• sed -n '50,80p' <file> → read around specific line

MINIMIZE tool calls - most issues should be visible in the diff.
Return findings as JSON.",
    ));

    sections.join("\n")
}

/// Compute a unified diff using git's algorithm for better accuracy
fn compute_compact_diff(old: &str, new: &str) -> String {
    use std::io::Write;
    use std::process::Command;

    // Create temp files for git diff
    let temp_dir = std::env::temp_dir();
    let nonce = format!("{}_{}", std::process::id(), uuid::Uuid::new_v4());
    let old_path = temp_dir.join(format!("cosmos_diff_old_{}.tmp", nonce));
    let new_path = temp_dir.join(format!("cosmos_diff_new_{}.tmp", nonce));

    // Write content to temp files
    if let (Ok(mut old_file), Ok(mut new_file)) = (
        std::fs::File::create(&old_path),
        std::fs::File::create(&new_path),
    ) {
        // Set restrictive permissions on temp files (Unix only)
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let perms = std::fs::Permissions::from_mode(0o600);
            let _ = old_file.set_permissions(perms.clone());
            let _ = new_file.set_permissions(perms);
        }

        let _ = old_file.write_all(old.as_bytes());
        let _ = new_file.write_all(new.as_bytes());

        // Run git diff with unified format, 3 lines context
        if let Ok(output) = Command::new("git")
            .args([
                "diff",
                "--no-index",
                "--no-color",
                "-U3", // 3 lines of context
                old_path.to_str().unwrap_or(""),
                new_path.to_str().unwrap_or(""),
            ])
            .output()
        {
            // Clean up temp files
            let _ = std::fs::remove_file(&old_path);
            let _ = std::fs::remove_file(&new_path);

            let diff_output = String::from_utf8_lossy(&output.stdout);

            // Skip the header lines (--- a/... and +++ b/...)
            let lines: Vec<&str> = diff_output
                .lines()
                .skip_while(|l| !l.starts_with("@@"))
                .take(150) // Limit to 150 lines
                .collect();

            if !lines.is_empty() {
                return lines.join("\n");
            }
        }

        // Clean up on error path too
        let _ = std::fs::remove_file(&old_path);
        let _ = std::fs::remove_file(&new_path);
    }

    // Fallback: simple line count comparison
    format!(
        "(diff unavailable - {} lines before, {} lines after)",
        old.lines().count(),
        new.lines().count()
    )
}

/// Add line numbers to code for review context
fn add_line_numbers(content: &str) -> String {
    content
        .lines()
        .enumerate()
        .map(|(i, line)| format!("{:4}| {}", i + 1, line))
        .collect::<Vec<_>>()
        .join("\n")
}

fn line_window_excerpt(
    content: &str,
    line: usize,
    _radius: usize,
    max_chars: usize,
) -> Option<String> {
    let numbered = add_line_numbers(content);
    let snippet = truncate_content_around_line(&numbered, line.max(1), max_chars)
        .unwrap_or_else(|| truncate_content(&numbered, max_chars));
    if snippet.trim().is_empty() {
        None
    } else {
        Some(snippet)
    }
}

fn review_fix_finding_context_section(content: &str, findings: &[ReviewFinding]) -> Option<String> {
    const MAX_SNIPPETS: usize = 4;
    const SNIPPET_RADIUS_LINES: usize = 4;
    const SNIPPET_MAX_CHARS: usize = 800;

    let snippets = findings
        .iter()
        .filter_map(|finding| finding.line.map(|line| (finding, line.max(1) as usize)))
        .take(MAX_SNIPPETS)
        .filter_map(|(finding, line)| {
            line_window_excerpt(content, line, SNIPPET_RADIUS_LINES, SNIPPET_MAX_CHARS).map(
                |snippet| {
                    format!(
                        "- {} (line {}): {}\n```text\n{}\n```",
                        finding.title, line, finding.description, snippet
                    )
                },
            )
        })
        .collect::<Vec<_>>();

    if snippets.is_empty() {
        None
    } else {
        Some(format!(
            "FINDING CONTEXT SNIPPETS (prefer copying anchors from these exact lines):\n{}",
            snippets.join("\n\n")
        ))
    }
}

fn allocate_attempt_time_slices_ms(total_ms: u64, slots: usize) -> Vec<u64> {
    if slots == 0 {
        return Vec::new();
    }
    if slots == 1 {
        return vec![total_ms.max(1)];
    }

    const MIN_PER_ATTEMPT_MS: u64 = 1_200;
    if total_ms < MIN_PER_ATTEMPT_MS.saturating_mul(slots as u64) {
        let mut out = vec![0; slots];
        out[0] = total_ms.max(1);
        return out;
    }

    // Front-load time onto attempt 1 to reduce "everyone timed out" failures during
    // provider latency spikes. Retries are still possible but less common.
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

/// Fix selected review findings
///
/// Takes the content and findings to address, returns fixed content.
/// On later iterations, includes original content and fix history for better context.
pub async fn fix_review_findings(
    path: &std::path::Path,
    content: &str,
    original_content: Option<&str>,
    findings: &[ReviewFinding],
    repo_memory: Option<String>,
    iteration: u32,
    fixed_titles: &[String],
) -> anyhow::Result<AppliedFix> {
    fix_review_findings_with_model(
        path,
        content,
        original_content,
        findings,
        repo_memory,
        iteration,
        fixed_titles,
        Model::Smart,
        60_000,
    )
    .await
}

/// Fix selected review findings with an explicit model choice.
#[allow(clippy::too_many_arguments)]
pub async fn fix_review_findings_with_model(
    path: &std::path::Path,
    content: &str,
    original_content: Option<&str>,
    findings: &[ReviewFinding],
    repo_memory: Option<String>,
    iteration: u32,
    fixed_titles: &[String],
    model: Model,
    timeout_ms: u64,
) -> anyhow::Result<AppliedFix> {
    if findings.is_empty() {
        return Err(anyhow::anyhow!("No findings to fix"));
    }

    let system = review_fix_system_prompt(iteration, fixed_titles);

    // Format findings for the prompt
    let findings_text: Vec<String> = findings
        .iter()
        .enumerate()
        .map(|(i, f)| {
            let line_info = f.line.map(|l| format!(" (line {})", l)).unwrap_or_default();
            format!(
                "{}. [{}] {}{}\n   {}\n   Category: {}",
                i + 1,
                f.severity.to_uppercase(),
                f.title,
                line_info,
                f.description,
                f.category
            )
        })
        .collect();

    let memory_section = format_repo_memory_section(repo_memory.as_deref(), "Repo conventions");

    const MAX_REVIEW_FIX_EXCERPT_CHARS: usize = 18_000;
    const MAX_REVIEW_FIX_EXPANDED_EXCERPT_CHARS: usize = 32_000;
    const MAX_REVIEW_FIX_FULL_CONTEXT_CHARS: usize = 60_000;
    let anchor_line = findings
        .iter()
        .filter_map(|f| f.line)
        .next()
        .unwrap_or(1)
        .max(1) as usize;
    let build_code_section = |label: &str, code: &str, prefer_full: bool| -> String {
        let len = code.chars().count();
        if prefer_full && len <= MAX_REVIEW_FIX_FULL_CONTEXT_CHARS {
            return format!("\n\n{}:\n```\n{}\n```", label, code);
        }
        if len <= MAX_REVIEW_FIX_EXCERPT_CHARS {
            return format!("\n\n{}:\n```\n{}\n```", label, code);
        }
        let excerpt_chars = if prefer_full {
            MAX_REVIEW_FIX_EXPANDED_EXCERPT_CHARS
        } else {
            MAX_REVIEW_FIX_EXCERPT_CHARS
        };
        let snippet = truncate_content_around_line(code, anchor_line, excerpt_chars)
            .unwrap_or_else(|| truncate_content(code, excerpt_chars));
        format!(
            "\n\n{} (EXCERPT):\nNOTE: This file is large ({} chars). Showing an excerpt around line {} to keep the review-fix loop fast.\nIMPORTANT: Only an excerpt is shown. Your `old_string` values must still be unique in the full file, so include enough surrounding context.\n```\n{}\n```",
            label, len, anchor_line, snippet
        )
    };

    let mut combined_usage: Option<Usage> = None;
    let mut last_apply_err: Option<String> = None;
    let mut prefer_full_prompt = false;
    const MAX_REVIEW_FIX_EDIT_REPAIR_ATTEMPTS: usize = 3;
    let slices =
        allocate_attempt_time_slices_ms(timeout_ms, MAX_REVIEW_FIX_EDIT_REPAIR_ATTEMPTS.max(1));
    // Keep review-fix calls bounded; allowing full model max output here can consume most of the
    // attempt budget in a single loop and reduce multi-attempt recovery.
    const MAX_REVIEW_FIX_RESPONSE_TOKENS_SPEED: u32 = 3072;

    for attempt in 1..=MAX_REVIEW_FIX_EDIT_REPAIR_ATTEMPTS.max(1) {
        let attempt_timeout_ms = slices
            .get(attempt.saturating_sub(1))
            .copied()
            .unwrap_or_else(|| timeout_ms.max(1));

        // Escalate prompt context on retry after anchor-application failures. This helps
        // the model pick verbatim anchors in larger files while keeping attempt-1 cheap.
        let original_section = if iteration > 1 {
            original_content
                .filter(|o| !o.is_empty())
                .map(|o| {
                    build_code_section("ORIGINAL CODE (before any fixes)", o, prefer_full_prompt)
                })
                .unwrap_or_default()
        } else {
            String::new()
        };
        let current_section = build_code_section("CURRENT CODE", content, prefer_full_prompt);
        let finding_context_section =
            review_fix_finding_context_section(content, findings).unwrap_or_default();
        let user_base = format!(
            "File: {}\n\nFINDINGS TO FIX:\n{}\n\n{}\n{}{}\n{}\n\nFix all listed findings. Think carefully about edge cases.",
            path.display(),
            findings_text.join("\n\n"),
            finding_context_section,
            memory_section,
            original_section,
            current_section
        );
        let user_attempt = if let Some(ref err) = last_apply_err {
            format!(
                "{}{}",
                user_base,
                format_edit_apply_repair_guidance(err, "CURRENT CODE block")
            )
        } else {
            user_base
        };

        let structured: anyhow::Result<StructuredResponse<FixResponse>> = if model == Model::Speed {
            call_llm_structured_limited_speed_with_failover(
                &system,
                &user_attempt,
                "fix_response",
                fix_response_schema(),
                MAX_REVIEW_FIX_RESPONSE_TOKENS_SPEED,
                attempt_timeout_ms,
            )
            .await
        } else {
            // Keep non-speed review-fix paths bounded too (same timeout envelope as speed).
            call_llm_structured_limited_no_reasoning(
                &system,
                &user_attempt,
                model,
                "fix_response",
                fix_response_schema(),
                MAX_REVIEW_FIX_RESPONSE_TOKENS_SPEED,
                attempt_timeout_ms,
            )
            .await
        };

        let (response_data, response_usage, speed_failover, schema_fallback_used) = match structured
        {
            Ok(response) => (
                response.data,
                response.usage,
                response.speed_failover,
                false,
            ),
            Err(err) => {
                let err_text = err.to_string();
                if !is_response_format_schema_error_text(&err_text) {
                    return Err(err);
                }

                let fallback_user = format!(
                    "{}\n\nFORMAT REQUIREMENT:\nReturn JSON only (no markdown fences) with this exact shape:\n{{\"description\": string, \"modified_areas\": [string], \"edits\": [{{\"old_string\": string, \"new_string\": string}}]}}",
                    user_attempt
                );
                let fallback_call = tokio::time::timeout(
                    Duration::from_millis(attempt_timeout_ms.max(1)),
                    call_llm_with_usage(&system, &fallback_user, model, false),
                )
                .await;
                let fallback_response = match fallback_call {
                    Ok(Ok(value)) => value,
                    Ok(Err(fallback_err)) => return Err(fallback_err),
                    Err(_) => {
                        return Err(anyhow::anyhow!(
                            "Review-fix schema fallback timed out after {}ms",
                            attempt_timeout_ms
                        ));
                    }
                };
                let parsed: FixResponse = parse_structured_content(&fallback_response.content)
                    .map_err(|parse_err| {
                        anyhow::anyhow!("Review-fix schema fallback parse failed: {}", parse_err)
                    })?;
                (parsed, fallback_response.usage, None, true)
            }
        };

        combined_usage = merge_usage(combined_usage, response_usage.clone());

        let description = response_data
            .description
            .unwrap_or_else(|| "Fixed review findings".to_string());
        let modified_areas = response_data.modified_areas;
        let edits = response_data.edits;

        if edits.is_empty() {
            let message = "No edits provided in response".to_string();
            if attempt < MAX_REVIEW_FIX_EDIT_REPAIR_ATTEMPTS.max(1) {
                last_apply_err = Some(message);
                continue;
            }
            return Err(anyhow::anyhow!(message));
        }

        let context_label = findings
            .iter()
            .filter_map(|f| f.line)
            .next()
            .map(|line| format!("file (finding around line {})", line.max(1)))
            .unwrap_or_else(|| "file".to_string());

        match apply_edits_with_context(content, &edits, &context_label) {
            Ok(new_content) => {
                let new_content = normalize_generated_content(content, new_content, false);
                if new_content.trim().is_empty() {
                    return Err(anyhow::anyhow!("Generated content is empty"));
                }
                return Ok(AppliedFix {
                    description,
                    new_content,
                    modified_areas,
                    usage: combined_usage,
                    speed_failover,
                    schema_fallback_used,
                });
            }
            Err(err) => {
                let message = err.to_string();
                let msg = message.to_ascii_lowercase();
                let retryable = msg.contains("old_string")
                    && (msg.contains("not found")
                        || msg.contains("matches")
                        || msg.contains("must be unique")
                        || msg.contains("empty for non-empty"));
                if attempt < MAX_REVIEW_FIX_EDIT_REPAIR_ATTEMPTS.max(1) && retryable {
                    last_apply_err = Some(message);
                    if !prefer_full_prompt {
                        prefer_full_prompt = true;
                    }
                    continue;
                }
                return Err(err);
            }
        }
    }

    Err(anyhow::anyhow!("Failed to apply review fix edits"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn review_fix_finding_context_section_includes_line_anchored_snippets() {
        let content = "fn a() {\n    let x = 1;\n    println!(\"{}\", x);\n}\n";
        let findings = vec![ReviewFinding {
            file: "src/lib.rs".to_string(),
            line: Some(2),
            severity: "warning".to_string(),
            category: "bug".to_string(),
            title: "Missing validation".to_string(),
            description: "Value should be validated before use.".to_string(),
            recommended: true,
        }];

        let section =
            review_fix_finding_context_section(content, &findings).expect("context section");
        assert!(section.contains("Missing validation"));
        assert!(section.contains("2|     let x = 1;"), "{}", section);
    }

    #[test]
    fn review_fix_finding_context_section_omits_findings_without_lines() {
        let content = "fn a() {}\n";
        let findings = vec![ReviewFinding {
            file: "src/lib.rs".to_string(),
            line: None,
            severity: "warning".to_string(),
            category: "bug".to_string(),
            title: "No line".to_string(),
            description: "No line available.".to_string(),
            recommended: true,
        }];
        assert!(review_fix_finding_context_section(content, &findings).is_none());
    }
}
