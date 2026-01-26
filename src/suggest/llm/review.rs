use super::client::call_llm_with_usage;
use super::fix::{apply_edits_with_context, normalize_generated_content, AppliedFix, FixResponse};
use super::models::{Model, Usage};
use super::parse::{merge_usage, parse_json_with_retry, truncate_content};
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

/// Perform deep adversarial review of code changes using the Reviewer model
///
/// This uses a different model (cognitive diversity) with adversarial prompting
/// to find issues that the implementing model might have missed.
///
/// On re-reviews (iteration > 1), the prompt is adjusted to focus on verifying fixes
/// rather than finding entirely new issues.
///
/// The `fix_context` parameter (when provided) tells the reviewer what the fix was
/// supposed to accomplish, allowing it to evaluate whether the fix was done correctly
/// rather than just finding any bugs in the code.
pub async fn verify_changes(
    files_with_content: &[(PathBuf, String, String)], // (path, old_content, new_content)
    iteration: u32,
    fixed_titles: &[String],
    fix_context: Option<&FixContext>,
) -> anyhow::Result<VerificationReview> {
    let system = review_system_prompt(iteration, fixed_titles, fix_context);

    // Build the diff context
    let mut changes_text = String::new();
    const MAX_REVIEW_CHARS_PER_FILE: usize = 40000;

    for (path, old_content, new_content) in files_with_content {
        let file_name = path.display().to_string();

        // Create a simple diff view
        changes_text.push_str(&format!("\n=== {} ===\n", file_name));

        let old_truncated = old_content.chars().count() > MAX_REVIEW_CHARS_PER_FILE;
        let new_truncated = new_content.chars().count() > MAX_REVIEW_CHARS_PER_FILE;
        let old_view = truncate_content(old_content, MAX_REVIEW_CHARS_PER_FILE);
        let new_view = truncate_content(new_content, MAX_REVIEW_CHARS_PER_FILE);

        if old_content.is_empty() {
            // New file
            changes_text.push_str("(NEW FILE)\n");
            if new_truncated {
                changes_text.push_str("(CONTENT TRUNCATED)\n");
            }
            changes_text.push_str(&add_line_numbers(&new_view));
        } else {
            // Show old and new with line numbers
            changes_text.push_str("--- BEFORE ---\n");
            if old_truncated {
                changes_text.push_str("(CONTENT TRUNCATED)\n");
            }
            changes_text.push_str(&add_line_numbers(&old_view));
            changes_text.push_str("\n--- AFTER ---\n");
            if new_truncated {
                changes_text.push_str("(CONTENT TRUNCATED)\n");
            }
            changes_text.push_str(&add_line_numbers(&new_view));
        }
        changes_text.push('\n');
    }

    let user = format!(
        "Review these code changes for bugs and issues:\n{}",
        changes_text
    );

    let response = call_llm_with_usage(&system, &user, Model::Reviewer, true).await?;

    // Parse the response with self-correction on failure
    let (parsed, correction_usage): (ReviewResponseJson, _) =
        parse_json_with_retry(&response.content, "code review").await?;

    // Merge usage from correction call if any
    let total_usage = merge_usage(response.usage, correction_usage);

    Ok(VerificationReview {
        findings: parsed.findings,
        summary: parsed.summary,
        usage: total_usage,
    })
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

    // Always use Smart model for fixes - getting it right the first time saves iterations
    let response = call_llm_with_usage(&system, &user, Model::Smart, true).await?;

    // Parse the JSON response with self-correction on failure
    let (parsed, correction_usage): (FixResponse, _) =
        parse_json_with_retry(&response.content, "review fix").await?;

    // Merge usage from correction call if any
    let total_usage = merge_usage(response.usage, correction_usage);

    let description = parsed
        .description
        .unwrap_or_else(|| "Fixed review findings".to_string());
    let modified_areas = parsed.modified_areas;
    let edits = parsed.edits;

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
        usage: total_usage,
    })
}
