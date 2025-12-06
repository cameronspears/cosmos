//! AI integration via OpenRouter
//!
//! Uses Grok 4.1 Fast for analysis/summaries, Opus 4.5 for code generation.

use crate::config::Config;
use serde::{Deserialize, Serialize};

const OPENROUTER_URL: &str = "https://openrouter.ai/api/v1/chat/completions";

#[derive(Debug, Clone, Copy)]
pub enum Model {
    GrokFast,  // x-ai/grok-4.1-fast - for analysis and summaries
    Opus,      // anthropic/claude-opus-4.5 - for code generation
}

impl Model {
    pub fn id(&self) -> &'static str {
        match self {
            Model::GrokFast => "x-ai/grok-4.1-fast",
            Model::Opus => "anthropic/claude-opus-4.5",
        }
    }

    pub fn name(&self) -> &'static str {
        match self {
            Model::GrokFast => "Grok 4.1 Fast",
            Model::Opus => "Opus 4.5",
        }
    }
}

#[derive(Serialize)]
struct ChatRequest {
    model: String,
    messages: Vec<Message>,
    max_tokens: u32,
    stream: bool,
}

#[derive(Serialize, Deserialize)]
struct Message {
    role: String,
    content: String,
}

#[derive(Deserialize)]
struct ChatResponse {
    choices: Vec<Choice>,
}

#[derive(Deserialize)]
struct Choice {
    message: MessageContent,
}

#[derive(Deserialize)]
struct MessageContent {
    content: String,
}

/// Get the OpenRouter API key from config or environment
pub fn get_api_key() -> Option<String> {
    Config::load().get_api_key()
}

/// Check if AI is available (API key is set)
pub fn is_available() -> bool {
    get_api_key().is_some()
}

/// Get setup instructions
pub fn setup_instructions() -> &'static str {
    "Run `codecosmos --setup` to configure your OpenRouter API key"
}

/// Call OpenRouter API with a prompt
pub async fn chat(prompt: &str, model: Model) -> Result<String, String> {
    let api_key = get_api_key()
        .ok_or_else(|| "OPENROUTER_API_KEY not set. Export it to enable AI features.".to_string())?;

    let client = reqwest::Client::new();
    
    let request = ChatRequest {
        model: model.id().to_string(),
        messages: vec![Message {
            role: "user".to_string(),
            content: prompt.to_string(),
        }],
        max_tokens: 4096,
        stream: false,
    };

    let response = client
        .post(OPENROUTER_URL)
        .header("Authorization", format!("Bearer {}", api_key))
        .header("Content-Type", "application/json")
        .header("HTTP-Referer", "https://github.com/codecosmos")
        .header("X-Title", "codecosmos")
        .json(&request)
        .send()
        .await
        .map_err(|e| format!("Request failed: {}", e))?;

    if !response.status().is_success() {
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        return Err(format!("API error {}: {}", status, text));
    }

    let chat_response: ChatResponse = response
        .json()
        .await
        .map_err(|e| format!("Failed to parse response: {}", e))?;

    chat_response
        .choices
        .first()
        .map(|c| c.message.content.clone())
        .ok_or_else(|| "No response from AI".to_string())
}

/// Generate a fix suggestion for a file issue (code generation)
pub async fn suggest_fix(prompt: &str) -> Result<String, String> {
    chat(prompt, Model::Opus).await
}

/// Quick analysis
pub async fn quick_analysis(prompt: &str) -> Result<String, String> {
    chat(prompt, Model::GrokFast).await
}

/// Generate a fix for an issue - AI explains what it found and proposes changes
pub async fn generate_fix(file_path: &str, content: &str, issue: &str) -> Result<String, String> {
    let loc = content.lines().count();
    let fn_count = content.matches("fn ").count() 
        + content.matches("function ").count()
        + content.matches("def ").count()
        + content.matches("func ").count();
    
    let system_prompt = r#"You are a code maintenance expert. Be direct and concise.

RESPONSE FORMAT:

=== ANALYSIS ===
Problems:
- [bullet points, 2-4 items max]
- [be specific: "3 functions over 80 lines" not "some long functions"]

=== RECOMMENDATION ===
[One sentence: what to do]
[One sentence: why, or key tradeoff]

=== CHANGES ===
[actual code changes below]

FOR SINGLE FILE FIX:
--- a/filepath
+++ b/filepath
@@ -line,count +line,count @@
 context
-removed
+added

FOR SPLITTING INTO MODULES:
=== CREATE path/to/new/file.ext ===
[full file content]

=== MODIFY path/to/original.ext ===
--- a/path/to/original.ext
+++ b/path/to/original.ext
[diff]

RULES:
- No greetings, no filler
- Split when: >300 lines, mixed responsibilities
- Fix in place when: bugs, missing error handling, small improvements
- Valid diff format in CHANGES section"#;

    let user_prompt = format!(
        "File: {} ({} lines, {} functions)\nIssue: {}\n\n```\n{}\n```",
        file_path, loc, fn_count, issue, content
    );

    chat_with_system(system_prompt, &user_prompt, Model::Opus).await
}

/// Parse AI response into explanation and changes
pub fn parse_ai_response(response: &str) -> (String, String) {
    let analysis_start = response.find("=== ANALYSIS ===");
    let recommendation_start = response.find("=== RECOMMENDATION ===");
    let changes_start = response.find("=== CHANGES ===");
    
    let explanation = if let (Some(a_start), Some(changes)) = (analysis_start, changes_start) {
        let analysis_content = &response[a_start + 16..changes];
        // Clean up and format the explanation
        analysis_content
            .replace("=== RECOMMENDATION ===", "\nRecommendation:")
            .replace("Problems:", "Problems:")
            .trim()
            .to_string()
    } else {
        // Fallback: if format isn't perfect, show everything before any diff markers
        let first_diff = response.find("---").or(response.find("=== CREATE"));
        match first_diff {
            Some(pos) => response[..pos].trim().to_string(),
            None => response.to_string(),
        }
    };
    
    let changes = if let Some(start) = changes_start {
        response[start + 15..].trim().to_string()
    } else {
        // Fallback: try to find diff content
        if let Some(pos) = response.find("---") {
            response[pos..].to_string()
        } else if let Some(pos) = response.find("=== CREATE") {
            response[pos..].to_string()
        } else {
            String::new()
        }
    };
    
    (explanation, changes)
}

/// Call OpenRouter API with system and user prompt
pub async fn chat_with_system(system: &str, user: &str, model: Model) -> Result<String, String> {
    let api_key = get_api_key()
        .ok_or_else(|| "OPENROUTER_API_KEY not set. Export it to enable AI features.".to_string())?;

    let client = reqwest::Client::new();
    
    let request = ChatRequest {
        model: model.id().to_string(),
        messages: vec![
            Message {
                role: "system".to_string(),
                content: system.to_string(),
            },
            Message {
                role: "user".to_string(),
                content: user.to_string(),
            },
        ],
        max_tokens: 8192,
        stream: false,
    };

    let response = client
        .post(OPENROUTER_URL)
        .header("Authorization", format!("Bearer {}", api_key))
        .header("Content-Type", "application/json")
        .header("HTTP-Referer", "https://github.com/codecosmos")
        .header("X-Title", "codecosmos")
        .json(&request)
        .send()
        .await
        .map_err(|e| format!("Request failed: {}", e))?;

    if !response.status().is_success() {
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        return Err(format!("API error {}: {}", status, text));
    }

    let chat_response: ChatResponse = response
        .json()
        .await
        .map_err(|e| format!("Failed to parse response: {}", e))?;

    chat_response
        .choices
        .first()
        .map(|c| c.message.content.clone())
        .ok_or_else(|| "No response from AI".to_string())
}

/// Result of AI code review
#[derive(Debug, Clone, PartialEq)]
pub struct ReviewResult {
    pub approved: bool,
    pub summary: String,
    pub issues: Vec<ReviewIssue>,
    pub suggestions: Vec<String>,
}

/// An issue found during review
#[derive(Debug, Clone, PartialEq)]
pub struct ReviewIssue {
    pub severity: IssueSeverity,
    pub description: String,
    pub line_hint: Option<usize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IssueSeverity {
    Error,
    Warning,
    Info,
}

impl IssueSeverity {
    pub fn emoji(&self) -> &'static str {
        match self {
            IssueSeverity::Error => "❌",
            IssueSeverity::Warning => "⚠️",
            IssueSeverity::Info => "💡",
        }
    }
}

/// Review code changes
pub async fn review_changes(original: &str, modified: &str, file_path: &str) -> Result<ReviewResult, String> {
    let system_prompt = r#"You are a senior code reviewer. Review the changes and provide feedback.

Output format (JSON):
{
  "approved": true/false,
  "summary": "Brief summary of the changes",
  "issues": [
    {"severity": "error|warning|info", "description": "Issue description", "line": null}
  ],
  "suggestions": ["Optional improvement suggestions"]
}

Focus on:
- Correctness: Does the change break anything?
- Best practices: Is the code clean and maintainable?
- Edge cases: Are there unhandled scenarios?
- Security: Any security concerns?

Be concise. Only flag real issues, not style preferences."#;

    let user_prompt = format!(
        "File: {}\n\nOriginal:\n```\n{}\n```\n\nModified:\n```\n{}\n```\n\nReview these changes:",
        file_path, original, modified
    );

    let response = chat_with_system(system_prompt, &user_prompt, Model::GrokFast).await?;
    
    // Parse the JSON response
    parse_review_response(&response)
}

fn parse_review_response(response: &str) -> Result<ReviewResult, String> {
    // Try to extract JSON from the response
    let json_str = if let Some(start) = response.find('{') {
        if let Some(end) = response.rfind('}') {
            &response[start..=end]
        } else {
            response
        }
    } else {
        response
    };

    // Parse JSON
    if let Ok(json) = serde_json::from_str::<serde_json::Value>(json_str) {
        let approved = json.get("approved").and_then(|v| v.as_bool()).unwrap_or(false);
        let summary = json.get("summary").and_then(|v| v.as_str()).unwrap_or("").to_string();
        
        let mut issues = Vec::new();
        if let Some(issues_arr) = json.get("issues").and_then(|v| v.as_array()) {
            for issue in issues_arr {
                let severity = match issue.get("severity").and_then(|v| v.as_str()) {
                    Some("error") => IssueSeverity::Error,
                    Some("warning") => IssueSeverity::Warning,
                    _ => IssueSeverity::Info,
                };
                let description = issue.get("description")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let line_hint = issue.get("line").and_then(|v| v.as_u64()).map(|n| n as usize);
                
                issues.push(ReviewIssue { severity, description, line_hint });
            }
        }
        
        let mut suggestions = Vec::new();
        if let Some(sugg_arr) = json.get("suggestions").and_then(|v| v.as_array()) {
            for sugg in sugg_arr {
                if let Some(s) = sugg.as_str() {
                    suggestions.push(s.to_string());
                }
            }
        }
        
        Ok(ReviewResult { approved, summary, issues, suggestions })
    } else {
        // Fallback: treat the whole response as a summary
        Ok(ReviewResult {
            approved: !response.to_lowercase().contains("error") 
                && !response.to_lowercase().contains("bug")
                && !response.to_lowercase().contains("issue"),
            summary: response.to_string(),
            issues: Vec::new(),
            suggestions: Vec::new(),
        })
    }
}

/// Generate a commit message for changes
pub async fn generate_commit_message(diff: &str, file_path: &str) -> Result<String, String> {
    let system_prompt = r#"Generate a concise git commit message for the given changes.

Rules:
- First line: type(scope): description (50 chars max)
- Types: fix, feat, refactor, perf, docs, test, chore
- Be specific about what changed
- No period at the end of the first line

Example:
fix(api): handle null user preferences correctly"#;

    let user_prompt = format!(
        "File: {}\n\nDiff:\n```diff\n{}\n```\n\nGenerate commit message:",
        file_path, diff
    );

    let response = chat_with_system(system_prompt, &user_prompt, Model::GrokFast).await?;
    
    // Clean up the response - extract just the commit message
    let message = response.lines()
        .find(|line| !line.is_empty() && !line.starts_with("```"))
        .unwrap_or(&response)
        .trim()
        .trim_matches('`')
        .to_string();
    
    Ok(message)
}

/// Generate PR description from commits
pub async fn generate_pr_description(commits: &[String], files_changed: &[String]) -> Result<String, String> {
    let system_prompt = r#"Generate a GitHub PR description.

Format:
## Summary
Brief description of what this PR does.

## Changes
- List of key changes

## Testing
How was this tested?

Be concise and informative."#;

    let commits_text = commits.join("\n");
    let files_text = files_changed.join("\n");
    
    let user_prompt = format!(
        "Commits:\n{}\n\nFiles changed:\n{}\n\nGenerate PR description:",
        commits_text, files_text
    );

    chat_with_system(system_prompt, &user_prompt, Model::GrokFast).await
}

/// Result of file summary generation
#[derive(Debug, Clone, PartialEq)]
pub struct FileSummary {
    pub what_it_does: String,
    pub why_problematic: String,
    pub suggested_fix: String,
}

/// Generate a quick file summary explaining what the file does and why it's problematic
pub async fn generate_file_summary(
    file_path: &str,
    content: &str,
    metrics: &FileSummaryMetrics,
) -> Result<FileSummary, String> {
    let system_prompt = r#"You are a code analyst. Analyze this file and provide a brief summary.

OUTPUT FORMAT (use these exact headers):
=== WHAT IT DOES ===
[1-2 sentences: what this file/module is responsible for]

=== WHY FLAGGED ===
[2-3 bullet points: specific reasons this file is problematic based on the metrics]

=== SUGGESTED FIX ===
[1 sentence: the most impactful fix to improve this file]

RULES:
- Be specific, use numbers from the metrics
- No greetings or filler
- Keep each section brief"#;

    let metrics_text = format!(
        "Metrics:\n- Lines: {}\n- Functions: {}\n- Danger score: {:.0}/100\n- Changes (churn): {}\n- Days since change: {}\n- Primary author owns: {:.0}%\n- Has tests: {}",
        metrics.loc,
        metrics.function_count,
        metrics.danger_score,
        metrics.change_count,
        metrics.days_since_change,
        metrics.primary_author_pct,
        if metrics.has_tests { "yes" } else { "no" }
    );

    let user_prompt = format!(
        "File: {}\n\n{}\n\nCode (first 200 lines):\n```\n{}\n```",
        file_path,
        metrics_text,
        content.lines().take(200).collect::<Vec<_>>().join("\n")
    );

    let response = chat_with_system(system_prompt, &user_prompt, Model::GrokFast).await?;
    parse_file_summary(&response)
}

/// Metrics passed to file summary generation
#[derive(Debug, Clone, Default)]
pub struct FileSummaryMetrics {
    pub loc: usize,
    pub function_count: usize,
    pub danger_score: f64,
    pub change_count: usize,
    pub days_since_change: usize,
    pub primary_author_pct: f64,
    pub has_tests: bool,
}

fn parse_file_summary(response: &str) -> Result<FileSummary, String> {
    let what_start = response.find("=== WHAT IT DOES ===");
    let why_start = response.find("=== WHY FLAGGED ===");
    let fix_start = response.find("=== SUGGESTED FIX ===");

    let what_it_does = if let (Some(start), Some(end)) = (what_start, why_start) {
        response[start + 20..end].trim().to_string()
    } else {
        "Unable to determine file purpose.".to_string()
    };

    let why_problematic = if let (Some(start), Some(end)) = (why_start, fix_start) {
        response[start + 18..end].trim().to_string()
    } else if let Some(start) = why_start {
        response[start + 18..].lines().take(5).collect::<Vec<_>>().join("\n").trim().to_string()
    } else {
        "Flagged based on metrics.".to_string()
    };

    let suggested_fix = if let Some(start) = fix_start {
        // "=== SUGGESTED FIX ===" is 21 characters
        response[start + 21..].trim().lines().next().unwrap_or("").to_string()
    } else {
        "Review and refactor as needed.".to_string()
    };

    Ok(FileSummary {
        what_it_does,
        why_problematic,
        suggested_fix,
    })
}

/// Enhancement option for "yes, and..." flow
#[derive(Debug, Clone, PartialEq)]
pub struct EnhancementOption {
    pub key: char,
    pub title: String,
    pub description: String,
    pub changes: String,
}

/// Generate enhancement options for the "yes, and..." flow
pub async fn generate_enhancements(
    file_path: &str,
    content: &str,
    base_fix: &str,
) -> Result<Vec<EnhancementOption>, String> {
    let system_prompt = r#"Given a base fix, suggest 4 enhancements that could be applied ON TOP of the base fix.

OUTPUT FORMAT:
=== OPTION A ===
Title: [short title, e.g. "Add error handling"]
Description: [1 sentence]
Changes:
[unified diff format]

=== OPTION B ===
Title: [short title, e.g. "Add unit tests"]  
Description: [1 sentence]
Changes:
[unified diff format]

=== OPTION C ===
Title: [short title, e.g. "Optimize performance"]
Description: [1 sentence]
Changes:
[unified diff format]

=== OPTION D ===
Title: [short title, e.g. "Improve documentation"]
Description: [1 sentence]
Changes:
[unified diff format]

RULES:
- Each option should be INDEPENDENT (can be applied separately)
- Options should cover different aspects: error handling, tests, performance, readability
- Use valid unified diff format for changes
- Be specific and actionable"#;

    let user_prompt = format!(
        "File: {}\n\nBase fix already applied:\n{}\n\nCurrent code:\n```\n{}\n```\n\nGenerate 4 enhancement options:",
        file_path,
        base_fix,
        content.lines().take(150).collect::<Vec<_>>().join("\n")
    );

    let response = chat_with_system(system_prompt, &user_prompt, Model::Opus).await?;
    parse_enhancements(&response)
}

fn parse_enhancements(response: &str) -> Result<Vec<EnhancementOption>, String> {
    let mut options = Vec::new();
    let option_markers = [
        ("=== OPTION A ===", 'a'),
        ("=== OPTION B ===", 'b'),
        ("=== OPTION C ===", 'c'),
        ("=== OPTION D ===", 'd'),
    ];

    for (i, (marker, key)) in option_markers.iter().enumerate() {
        if let Some(start) = response.find(marker) {
            let end = if i + 1 < option_markers.len() {
                response.find(option_markers[i + 1].0).unwrap_or(response.len())
            } else {
                response.len()
            };

            let section = &response[start + marker.len()..end];
            
            let title = section.lines()
                .find(|l| l.starts_with("Title:"))
                .map(|l| l.trim_start_matches("Title:").trim().to_string())
                .unwrap_or_else(|| format!("Option {}", key.to_ascii_uppercase()));

            let description = section.lines()
                .find(|l| l.starts_with("Description:"))
                .map(|l| l.trim_start_matches("Description:").trim().to_string())
                .unwrap_or_else(|| "Enhancement option".to_string());

            let changes_start = section.find("Changes:");
            let changes = if let Some(cs) = changes_start {
                section[cs + 8..].trim().to_string()
            } else if let Some(diff_start) = section.find("---") {
                section[diff_start..].trim().to_string()
            } else {
                String::new()
            };

            options.push(EnhancementOption {
                key: *key,
                title,
                description,
                changes,
            });
        }
    }

    if options.is_empty() {
        return Err("Could not parse enhancement options".to_string());
    }

    Ok(options)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_model_ids() {
        assert!(Model::GrokFast.id().contains("grok"));
        assert!(Model::Opus.id().contains("opus"));
    }
    
    #[test]
    fn test_parse_review_response() {
        let json = r#"{"approved": true, "summary": "Good changes", "issues": [], "suggestions": []}"#;
        let result = parse_review_response(json).unwrap();
        assert!(result.approved);
        assert_eq!(result.summary, "Good changes");
    }

    #[test]
    fn test_parse_file_summary() {
        let response = r#"=== WHAT IT DOES ===
This file handles user authentication and session management.

=== WHY FLAGGED ===
- High complexity score (45) with 12 functions
- 847 lines of code with mixed responsibilities
- Only 1 contributor owns 95% of the code

=== SUGGESTED FIX ===
Split into separate auth.rs and session.rs modules."#;

        let result = parse_file_summary(response).unwrap();
        assert!(result.what_it_does.contains("authentication"));
        assert!(result.why_problematic.contains("High complexity"));
        assert!(result.suggested_fix.contains("Split"));
    }
}

