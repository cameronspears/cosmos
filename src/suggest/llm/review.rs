use super::client::call_llm_with_usage;
use super::fix::{truncate_for_error, AppliedFix, FixResponse};
use super::models::{Model, Usage};
use super::parse::{merge_usage, parse_json_with_retry};
use super::prompts::{review_fix_system_prompt, review_system_prompt};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

// ============================================================================
// Deep Verification Review (Sweet Spot Flow)
// ============================================================================
// Flow: Reviewer reviews → User sees findings → User selects → Smart fixes → Done

/// A finding from the adversarial code reviewer
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReviewFinding {
    pub file: String,
    pub line: Option<u32>,
    pub severity: String, // "critical", "warning", "suggestion", "nitpick"
    pub category: String, // "bug", "security", "performance", "logic", "error-handling", "style"
    pub title: String,    // Short title
    pub description: String, // Detailed explanation in plain language
    pub recommended: bool, // Reviewer recommends fixing this (true = should fix, false = optional)
}

/// Response structure for code review (used for JSON parsing with retry)
#[derive(Debug, Clone, Deserialize)]
struct ReviewResponseJson {
    summary: String,
    #[serde(default)]
    pass: Option<bool>,
    findings: Vec<ReviewFinding>,
}

/// Result of a deep verification review
#[derive(Debug, Clone)]
pub struct VerificationReview {
    pub findings: Vec<ReviewFinding>,
    pub summary: String, // Overall assessment
    #[allow(dead_code)]
    pub pass: bool, // True if no critical/warning issues
    pub usage: Option<Usage>,
}

/// Perform deep adversarial review of code changes using the Reviewer model
///
/// This uses a different model (cognitive diversity) with adversarial prompting
/// to find issues that the implementing model might have missed.
///
/// On re-reviews (iteration > 1), the prompt is adjusted to focus on verifying fixes
/// rather than finding entirely new issues.
pub async fn verify_changes(
    files_with_content: &[(PathBuf, String, String)], // (path, old_content, new_content)
    iteration: u32,
    fixed_titles: &[String],
) -> anyhow::Result<VerificationReview> {
    let system = review_system_prompt(iteration, fixed_titles);

    // Build the diff context
    let mut changes_text = String::new();
    for (path, old_content, new_content) in files_with_content {
        let file_name = path.display().to_string();

        // Create a simple diff view
        changes_text.push_str(&format!("\n=== {} ===\n", file_name));

        if old_content.is_empty() {
            // New file
            changes_text.push_str("(NEW FILE)\n");
            changes_text.push_str(&add_line_numbers(new_content));
        } else {
            // Show old and new with line numbers
            changes_text.push_str("--- BEFORE ---\n");
            changes_text.push_str(&add_line_numbers(old_content));
            changes_text.push_str("\n--- AFTER ---\n");
            changes_text.push_str(&add_line_numbers(new_content));
        }
        changes_text.push('\n');
    }

    let user = format!("Review these code changes for bugs and issues:\n{}", changes_text);

    let response = call_llm_with_usage(&system, &user, Model::Reviewer, true).await?;

    // Parse the response with self-correction on failure
    let (parsed, correction_usage): (ReviewResponseJson, _) =
        parse_json_with_retry(&response.content, "code review").await?;

    // Merge usage from correction call if any
    let total_usage = merge_usage(response.usage, correction_usage);

    // Determine pass based on findings if not explicitly set
    let pass = parsed.pass.unwrap_or_else(|| {
        !parsed
            .findings
            .iter()
            .any(|f| f.severity == "critical" || f.severity == "warning")
    });

    Ok(VerificationReview {
        findings: parsed.findings,
        summary: parsed.summary,
        pass,
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
    path: &PathBuf,
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

    let memory_section = repo_memory
        .as_deref()
        .filter(|m| !m.trim().is_empty())
        .map(|m| format!("\n\nRepo conventions:\n{}", m))
        .unwrap_or_default();

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
    let mut new_content = content.to_string();
    for (i, edit) in edits.iter().enumerate() {
        let matches: Vec<_> = new_content.match_indices(&edit.old_string).collect();

        if matches.is_empty() {
            return Err(anyhow::anyhow!(
                "Edit {}: old_string not found in file.\nSearched for: {:?}",
                i + 1,
                truncate_for_error(&edit.old_string)
            ));
        }

        if matches.len() > 1 {
            return Err(anyhow::anyhow!(
                "Edit {}: old_string matches {} times (must be unique).\nSearched for: {:?}",
                i + 1,
                matches.len(),
                truncate_for_error(&edit.old_string)
            ));
        }

        new_content = new_content.replacen(&edit.old_string, &edit.new_string, 1);
    }

    // Normalize whitespace
    let mut new_content: String = new_content
        .lines()
        .map(|line| line.trim_end())
        .collect::<Vec<_>>()
        .join("\n");
    if !new_content.ends_with('\n') {
        new_content.push('\n');
    }

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
