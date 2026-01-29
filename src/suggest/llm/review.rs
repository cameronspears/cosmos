use super::agentic::call_llm_agentic;
use super::client::{call_llm_structured_cached, StructuredResponse};
use super::fix::{
    apply_edits_with_context, fix_response_schema, normalize_generated_content, AppliedFix,
    FixResponse,
};
use super::models::{Model, Usage};
use super::parse::parse_json_with_retry;
use super::prompt_utils::format_repo_memory_section;
use super::prompts::{review_fix_system_prompt, review_system_prompt};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

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

/// Response structure for code review (used for JSON parsing with retry)
#[derive(Debug, Clone, Deserialize)]
struct ReviewResponseJson {
    summary: String,
    findings: Vec<ReviewFinding>,
}

/// Result of a deep verification review
#[derive(Debug, Clone)]
pub struct VerificationReview {
    pub findings: Vec<ReviewFinding>,
    pub summary: String, // Overall assessment
    pub usage: Option<Usage>,
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

    // Use Speed model with high reasoning effort for cost-effective review
    // 4 iterations - diff already provided, occasional context needed
    let response = call_llm_agentic(&system, &user, Model::Speed, &repo_root, false, 4).await?;

    // Parse the response with self-correction on failure
    let (parsed, correction_usage): (ReviewResponseJson, _) =
        parse_json_with_retry(&response.content, "code review").await?;

    Ok(VerificationReview {
        findings: parsed.findings,
        summary: parsed.summary,
        usage: correction_usage, // Agentic doesn't track usage, but correction might
    })
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
    let old_path = temp_dir.join("cosmos_diff_old.tmp");
    let new_path = temp_dir.join("cosmos_diff_new.tmp");

    // Write content to temp files
    if let (Ok(mut old_file), Ok(mut new_file)) = (
        std::fs::File::create(&old_path),
        std::fs::File::create(&new_path),
    ) {
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

    // For iterations > 1, include original content so model can see the full evolution
    let original_section = if iteration > 1 {
        original_content
            .filter(|o| !o.is_empty())
            .map(|o| format!("\n\nORIGINAL CODE (before any fixes):\n```\n{}\n```", o))
            .unwrap_or_default()
    } else {
        String::new()
    };

    let user = format!(
        "File: {}\n\nFINDINGS TO FIX:\n{}\n{}{}\n\nCURRENT CODE:\n```\n{}\n```\n\nFix all listed findings. Think carefully about edge cases.",
        path.display(),
        findings_text.join("\n\n"),
        memory_section,
        original_section,
        content
    );

    // Use structured output with caching - guarantees valid JSON and reduces costs
    let response: StructuredResponse<FixResponse> = call_llm_structured_cached(
        &system,
        &user,
        Model::Smart,
        "fix_response",
        fix_response_schema(),
    )
    .await?;

    let description = response
        .data
        .description
        .unwrap_or_else(|| "Fixed review findings".to_string());
    let modified_areas = response.data.modified_areas;
    let edits = response.data.edits;

    if edits.is_empty() {
        return Err(anyhow::anyhow!("No edits provided in response"));
    }

    // Apply edits sequentially with validation
    let new_content = apply_edits_with_context(content, &edits, "file")?;

    // Preserve whitespace and match trailing newline to original
    let new_content = normalize_generated_content(content, new_content, false);

    if new_content.trim().is_empty() {
        return Err(anyhow::anyhow!("Generated content is empty"));
    }

    Ok(AppliedFix {
        description,
        new_content,
        modified_areas,
        usage: response.usage,
    })
}
