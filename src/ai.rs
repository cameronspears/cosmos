//! AI integration via OpenRouter
//!
//! Uses Claude Sonnet 4 for complex analysis and DeepSeek for simpler tasks.

use crate::config::Config;
use serde::{Deserialize, Serialize};

const OPENROUTER_URL: &str = "https://openrouter.ai/api/v1/chat/completions";

#[derive(Debug, Clone, Copy)]
pub enum Model {
    Claude,    // anthropic/claude-sonnet-4 - for complex refactoring
    DeepSeek,  // deepseek/deepseek-chat - for simpler analysis
}

impl Model {
    pub fn id(&self) -> &'static str {
        match self {
            Model::Claude => "anthropic/claude-sonnet-4",
            Model::DeepSeek => "deepseek/deepseek-chat",
        }
    }

    pub fn name(&self) -> &'static str {
        match self {
            Model::Claude => "Claude Sonnet 4",
            Model::DeepSeek => "DeepSeek",
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

/// Generate a fix suggestion for a file issue (prose analysis)
pub async fn suggest_fix(prompt: &str) -> Result<String, String> {
    // Use Claude for complex refactoring suggestions
    chat(prompt, Model::Claude).await
}

/// Quick analysis using cheaper model
pub async fn quick_analysis(prompt: &str) -> Result<String, String> {
    chat(prompt, Model::DeepSeek).await
}

/// Generate a unified diff patch to fix an issue
pub async fn generate_fix(file_path: &str, content: &str, issue: &str) -> Result<String, String> {
    let system_prompt = r#"You are a code refactoring expert. Your ONLY output should be a unified diff patch.

Rules:
1. Output ONLY a valid unified diff, nothing else
2. Start with --- a/filepath and +++ b/filepath  
3. Include @@ line numbers for each hunk
4. Use - for removed lines, + for added lines, space for context
5. Include 3 lines of context around changes
6. Do NOT include explanations, markdown, or any other text
7. The diff should be directly applicable with `patch -p1`

Example format:
--- a/src/example.ts
+++ b/src/example.ts
@@ -10,7 +10,8 @@ function example() {
   const x = 1;
-  const y = badCode();
+  const y = goodCode();
+  const z = additionalFix();
   return x + y;
 }"#;

    let user_prompt = format!(
        "File: {}\n\nIssue to fix: {}\n\nCurrent file content:\n```\n{}\n```\n\nGenerate a unified diff patch to fix this issue:",
        file_path, issue, content
    );

    chat_with_system(system_prompt, &user_prompt, Model::Claude).await
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

/// Review code changes using DeepSeek (cheaper model)
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

    let response = chat_with_system(system_prompt, &user_prompt, Model::DeepSeek).await?;
    
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

    let response = chat_with_system(system_prompt, &user_prompt, Model::DeepSeek).await?;
    
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

    chat_with_system(system_prompt, &user_prompt, Model::DeepSeek).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_model_ids() {
        assert!(Model::Claude.id().contains("claude"));
        assert!(Model::DeepSeek.id().contains("deepseek"));
    }
    
    #[test]
    fn test_parse_review_response() {
        let json = r#"{"approved": true, "summary": "Good changes", "issues": [], "suggestions": []}"#;
        let result = parse_review_response(json).unwrap();
        assert!(result.approved);
        assert_eq!(result.summary, "Good changes");
    }
}

