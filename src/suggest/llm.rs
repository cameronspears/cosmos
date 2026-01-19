//! LLM-powered suggestions via OpenRouter
//!
//! BYOK only: user provides an OpenRouter API key, billed directly.
//!
//! Uses a 4-tier model system optimized for cost and quality:
//! - Speed (gpt-oss-120b): Fast summaries and file classification
//! - Balanced (claude-sonnet-4.5): Questions and fix previews
//! - Smart (claude-opus-4.5): Code generation and suggestions
//! - Reviewer (gpt-5.2): Adversarial bug-finding
//!
//! All models use :nitro suffix for high-throughput provider routing.

use super::{Priority, Suggestion, SuggestionKind, SuggestionSource};
use crate::cache::DomainGlossary;
use crate::config::Config;
use crate::context::WorkContext;
use crate::index::{CodebaseIndex, PatternKind, SymbolKind};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// OpenRouter direct API URL (BYOK mode)
const OPENROUTER_URL: &str = "https://openrouter.ai/api/v1/chat/completions";

/// Get the configured OpenRouter API key, if any.
fn api_key() -> Option<String> {
    Config::load().get_api_key()
}

// Model pricing per million tokens (estimated, check OpenRouter for current rates)
// Speed: openai/gpt-oss-120b - fast, cheap model for summaries
const SPEED_INPUT_COST: f64 = 0.10;   // $0.10 per 1M input tokens
const SPEED_OUTPUT_COST: f64 = 0.30;  // $0.30 per 1M output tokens
// Balanced: anthropic/claude-sonnet-4.5 - good reasoning at medium cost
const BALANCED_INPUT_COST: f64 = 3.0;   // $3 per 1M input tokens
const BALANCED_OUTPUT_COST: f64 = 15.0; // $15 per 1M output tokens
// Smart: anthropic/claude-opus-4.5 - best reasoning for code generation
const SMART_INPUT_COST: f64 = 15.0;   // $15 per 1M input tokens
const SMART_OUTPUT_COST: f64 = 75.0;  // $75 per 1M output tokens
// Reviewer: openai/gpt-5.2 - different model family for cognitive diversity
const REVIEWER_INPUT_COST: f64 = 5.0;  // $5 per 1M input tokens (estimated)
const REVIEWER_OUTPUT_COST: f64 = 15.0; // $15 per 1M output tokens (estimated)

/// Models available for suggestions
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Model {
    /// Speed tier - fast, cheap model for summaries and classification (gpt-oss-120b)
    Speed,
    /// Balanced tier - good reasoning at medium cost for questions/previews (claude-sonnet-4.5)
    Balanced,
    /// Smart tier - best reasoning for code generation (claude-opus-4.5)
    Smart,
    /// Reviewer tier - different model family for adversarial bug-finding (gpt-5.2)
    Reviewer,
}

/// Maximum tokens for all model tiers
const MODEL_MAX_TOKENS: u32 = 16384;

impl Model {
    pub fn id(&self) -> &'static str {
        match self {
            Model::Speed => "openai/gpt-oss-120b:nitro",
            Model::Balanced => "anthropic/claude-sonnet-4.5:nitro",
            Model::Smart => "anthropic/claude-opus-4.5:nitro",
            Model::Reviewer => "openai/gpt-5.2:nitro",
        }
    }

    pub fn max_tokens(&self) -> u32 {
        MODEL_MAX_TOKENS
    }
    
    pub fn display_name(&self) -> &'static str {
        match self {
            Model::Speed => "speed",
            Model::Balanced => "balanced",
            Model::Smart => "smart",
            Model::Reviewer => "reviewer",
        }
    }
    
    /// Calculate cost in USD based on token usage
    pub fn calculate_cost(&self, prompt_tokens: u32, completion_tokens: u32) -> f64 {
        let (input_rate, output_rate) = match self {
            Model::Speed => (SPEED_INPUT_COST, SPEED_OUTPUT_COST),
            Model::Balanced => (BALANCED_INPUT_COST, BALANCED_OUTPUT_COST),
            Model::Smart => (SMART_INPUT_COST, SMART_OUTPUT_COST),
            Model::Reviewer => (REVIEWER_INPUT_COST, REVIEWER_OUTPUT_COST),
        };
        
        let input_cost = (prompt_tokens as f64 / 1_000_000.0) * input_rate;
        let output_cost = (completion_tokens as f64 / 1_000_000.0) * output_rate;
        
        input_cost + output_cost
    }
}

/// API usage information from OpenRouter
#[derive(Deserialize, Clone, Debug, Default)]
pub struct Usage {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub total_tokens: u32,
}

impl Usage {
    pub fn calculate_cost(&self, model: Model) -> f64 {
        model.calculate_cost(self.prompt_tokens, self.completion_tokens)
    }
}

/// Response from LLM including content and usage stats
#[derive(Debug)]
pub struct LlmResponse {
    pub content: String,
    pub usage: Option<Usage>,
    #[allow(dead_code)]
    pub model: String,
}

#[derive(Serialize)]
struct ChatRequest {
    model: String,
    messages: Vec<Message>,
    max_tokens: u32,
    stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    response_format: Option<ResponseFormat>,
}

#[derive(Serialize)]
struct ResponseFormat {
    #[serde(rename = "type")]
    format_type: String,
}

#[derive(Serialize, Deserialize)]
struct Message {
    role: String,
    content: String,
}

#[derive(Deserialize)]
struct ChatResponse {
    choices: Vec<Choice>,
    usage: Option<Usage>,
    model: Option<String>,
}

#[derive(Deserialize)]
struct Choice {
    message: MessageContent,
}

#[derive(Deserialize)]
struct MessageContent {
    content: String,
}

/// Check if LLM is available (either BYOK or managed)
pub fn is_available() -> bool {
    api_key().is_some()
}

/// Call LLM API (returns content only, for backwards compatibility)
async fn call_llm(system: &str, user: &str, model: Model) -> anyhow::Result<String> {
    let response = call_llm_with_usage(system, user, model, false).await?;
    Ok(response.content)
}

/// Rate limit retry configuration
const MAX_RETRIES: u32 = 3;
const INITIAL_BACKOFF_MS: u64 = 2000;  // 2 seconds
const BACKOFF_MULTIPLIER: u64 = 2;     // Exponential backoff

/// Extract retry-after hint from OpenRouter response (if present)
fn parse_retry_after(text: &str) -> Option<u64> {
    // OpenRouter may include retry-after in response body or we estimate
    // Look for patterns like "retry after X seconds" or "wait X seconds"
    let text_lower = text.to_lowercase();
    if let Some(pos) = text_lower.find("retry") {
        // Try to extract a number after "retry"
        let after_retry = &text_lower[pos..];
        for word in after_retry.split_whitespace().skip(1).take(5) {
            if let Ok(secs) = word.trim_matches(|c: char| !c.is_numeric()).parse::<u64>() {
                if secs > 0 && secs < 300 {
                    return Some(secs);
                }
            }
        }
    }
    None
}

/// Call LLM API with full response including usage stats
/// Includes automatic retry with exponential backoff for rate limits
async fn call_llm_with_usage(
    system: &str, 
    user: &str, 
    model: Model,
    json_mode: bool,
) -> anyhow::Result<LlmResponse> {
    let api_key = api_key().ok_or_else(|| {
        anyhow::anyhow!("No API key configured. Run 'cosmos --setup' to get started.")
    })?;

    let client = reqwest::Client::new();
    let url = OPENROUTER_URL;

    let response_format = if json_mode {
        Some(ResponseFormat {
            format_type: "json_object".to_string(),
        })
    } else {
        None
    };

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
        max_tokens: model.max_tokens(),
        stream: false,
        response_format,
    };

    let mut last_error = String::new();
    let mut retry_count = 0;
    
    while retry_count <= MAX_RETRIES {
        // Build request with OpenRouter headers
        let response = client
            .post(url)
            .header("Content-Type", "application/json")
            .header("HTTP-Referer", "https://cosmos.dev")
            .header("X-Title", "Cosmos")
            .header("Authorization", format!("Bearer {}", api_key))
            .json(&request)
            .send()
            .await?;

        if response.status().is_success() {
            let chat_response: ChatResponse = response.json().await?;

            let content = chat_response
                .choices
                .first()
                .map(|c| c.message.content.clone())
                .ok_or_else(|| anyhow::anyhow!("No response from AI"))?;
            
            return Ok(LlmResponse {
                content,
                usage: chat_response.usage,
                model: chat_response.model.unwrap_or_else(|| model.id().to_string()),
            });
        }

        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        
        // Handle rate limiting with retry
        if status.as_u16() == 429 && retry_count < MAX_RETRIES {
            retry_count += 1;
            
            // Calculate backoff delay
            let retry_after = parse_retry_after(&text);
            let backoff_secs = retry_after.unwrap_or_else(|| {
                INITIAL_BACKOFF_MS * BACKOFF_MULTIPLIER.pow(retry_count - 1) / 1000
            });
            
            // Log the retry attempt (this will be visible in error log if it ultimately fails)
            last_error = format!(
                "Rate limited by OpenRouter (attempt {}/{}). Retrying in {}s...",
                retry_count, MAX_RETRIES + 1, backoff_secs
            );
            
            // Wait before retrying
            tokio::time::sleep(tokio::time::Duration::from_secs(backoff_secs)).await;
            continue;
        }
        
        // Non-retryable error or max retries exceeded
        let error_msg = match status.as_u16() {
            401 => {
                "Invalid API key. Run 'cosmos --setup' to update it.".to_string()
            }
            429 => {
                format!(
                    "Rate limited by OpenRouter after {} retries. Try again in a few minutes. (Press 'e' to view error log)",
                    retry_count
                )
            }
            500..=599 => {
                format!("OpenRouter server error ({}). The service may be temporarily unavailable.", status)
            }
            _ => format!("API error {}: {}", status, truncate_str(&text, 200)),
        };
        return Err(anyhow::anyhow!("{}", error_msg));
    }
    
    // Should not reach here, but handle it gracefully
    Err(anyhow::anyhow!("{}", last_error))
}

/// Ask cosmos a general question about the codebase
/// Uses the Smart model for thoughtful, well-reasoned responses in plain English
pub async fn ask_question(
    index: &CodebaseIndex,
    context: &WorkContext,
    question: &str,
    repo_memory: Option<String>,
) -> anyhow::Result<(String, Option<Usage>)> {
    let system = r#"You are Cosmos, a thoughtful guide who helps people understand codebases without requiring technical knowledge.

The user is asking about their project. They may not be a developer, so:
- Write in plain English sentences and paragraphs
- Avoid code snippets, function names, and technical jargon
- Explain concepts as you would to a curious colleague
- Be conversational and helpful, not robotic
- Focus on the "what" and "why", not the "how it's implemented"
- Use analogies when they help clarify complex ideas

Keep responses clear and well-organized. Use short paragraphs for readability.
You may use **bold** for emphasis and bullet points for lists, but avoid code formatting."#;

    // Build context about the codebase
    let stats = index.stats();
    let file_list: Vec<_> = index.files.keys()
        .take(50)  // Limit to avoid huge prompts
        .map(|p| p.display().to_string())
        .collect();
    
    // Get symbols for context (used internally, not exposed to user)
    let symbols: Vec<_> = index.files.values()
        .flat_map(|f| f.symbols.iter())
        .filter(|s| matches!(s.kind, SymbolKind::Function | SymbolKind::Struct | SymbolKind::Enum))
        .take(100)
        .map(|s| format!("{:?}: {}", s.kind, s.name))
        .collect();

    let memory_section = repo_memory
        .as_deref()
        .filter(|m| !m.trim().is_empty())
        .map(|m| format!("\n\nPROJECT NOTES:\n{}", m))
        .unwrap_or_default();

    let user = format!(
        r#"PROJECT CONTEXT:
- {} files, {} lines of code
- {} components/features total
- Currently on: {}
- Key areas: {}

INTERNAL STRUCTURE (for your reference, don't mention these names directly):
{}
{}

QUESTION:
{}"#,
        stats.file_count,
        stats.total_loc,
        stats.symbol_count,
        context.branch,
        file_list.join(", "),
        symbols.join("\n"),
        memory_section,
        question
    );

    let response = call_llm_with_usage(system, &user, Model::Balanced, false).await?;
    Ok((response.content, response.usage))
}

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
struct EditOp {
    /// The exact text to find (must match exactly once in the file)
    old_string: String,
    /// The replacement text
    new_string: String,
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
    let system = r#"You are a senior developer implementing a code fix. You've been given a plan - now implement it.

OUTPUT FORMAT (JSON):
{
  "description": "1-2 sentence summary of what you changed",
  "modified_areas": ["function_name", "another_function"],
  "edits": [
    {
      "old_string": "exact text to find and replace",
      "new_string": "replacement text"
    }
  ]
}

CRITICAL RULES FOR EDITS:
- old_string must be EXACT text from the file (copy-paste precision)
- old_string must be UNIQUE in the file - include enough context (3-5 lines before/after the change)
- new_string is what replaces it (can be same length, longer, or shorter)
- Multiple edits are applied in order - each must be unique in the file at application time
- Preserve indentation exactly - spaces and tabs matter
- Do NOT include line numbers in old_string or new_string

EXAMPLE - Adding a null check:
{
  "description": "Added null check before accessing user.name",
  "modified_areas": ["getUserName"],
  "edits": [
    {
      "old_string": "function getUserName(user) {\n  return user.name;",
      "new_string": "function getUserName(user) {\n  if (!user) return null;\n  return user.name;"
    }
  ]
}"#;

    let plan_text = format!(
        "Verification: {} - {}\nPlan: {}\nScope: {}\nAffected areas: {}{}",
        if plan.verified { "CONFIRMED" } else { "UNCONFIRMED" },
        plan.verification_note,
        plan.description,
        plan.scope.label(),
        plan.affected_areas.join(", "),
        plan.modifier.as_ref().map(|m| format!("\nUser modifications: {}", m)).unwrap_or_default()
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

    let response = call_llm_with_usage(system, &user, Model::Smart, true).await?;
    
    // Parse the JSON response
    let json_str = extract_json_object(&response.content)
        .ok_or_else(|| anyhow::anyhow!("No JSON found in fix response"))?;
    
    let parsed: serde_json::Value = serde_json::from_str(json_str)
        .map_err(|e| anyhow::anyhow!("Failed to parse fix JSON: {}", e))?;
    
    let description = parsed.get("description")
        .and_then(|v| v.as_str())
        .unwrap_or("Applied the requested fix")
        .to_string();
    
    let modified_areas = parsed.get("modified_areas")
        .and_then(|v| v.as_array())
        .map(|arr| arr.iter().filter_map(|v| v.as_str().map(String::from)).collect())
        .unwrap_or_default();
    
    // Parse and apply edits
    let edits: Vec<EditOp> = parsed.get("edits")
        .and_then(|v| serde_json::from_value(v.clone()).ok())
        .ok_or_else(|| anyhow::anyhow!("Missing or invalid 'edits' array in response"))?;
    
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
        usage: response.usage,
    })
}

/// Truncate a string for error messages (UTF-8 safe)
fn truncate_for_error(s: &str) -> String {
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

/// Generate coordinated fixes across multiple files
/// 
/// This function handles multi-file refactors like:
/// - Renaming a function and updating all callers
/// - Extracting shared code and updating imports
/// - Interface changes that affect multiple files
pub async fn generate_multi_file_fix(
    files: &[(PathBuf, String)],  // (path, content) pairs
    suggestion: &Suggestion,
    plan: &FixPreview,
    repo_memory: Option<String>,
) -> anyhow::Result<MultiFileAppliedFix> {
    let system = r#"You are a senior developer implementing a multi-file refactor. You've been given a plan - now implement coordinated changes across all files.

OUTPUT FORMAT (JSON):
{
  "description": "1-2 sentence summary of what you changed across all files",
  "file_edits": [
    {
      "file": "path/to/file.rs",
      "edits": [
        {
          "old_string": "exact text to find and replace",
          "new_string": "replacement text"
        }
      ]
    }
  ]
}

CRITICAL RULES FOR EDITS:
- old_string must be EXACT text from the file (copy-paste precision)
- old_string must be UNIQUE within its file - include enough context (3-5 lines)
- new_string is what replaces it (can be same length, longer, or shorter)
- Multiple edits per file are applied in order
- Preserve indentation exactly - spaces and tabs matter
- Do NOT include line numbers in old_string or new_string
- Include ALL files that need changes - don't leave any file half-refactored

MULTI-FILE CONSISTENCY:
- Ensure renamed symbols match across all files
- Update all import statements that reference moved/renamed items
- Keep function signatures consistent between definition and call sites

EXAMPLE - Renaming a function across files:
{
  "description": "Renamed process_batch to handle_batch_items and updated all callers",
  "file_edits": [
    {
      "file": "src/processor.rs",
      "edits": [
        {
          "old_string": "pub fn process_batch(",
          "new_string": "pub fn handle_batch_items("
        }
      ]
    },
    {
      "file": "src/main.rs",
      "edits": [
        {
          "old_string": "processor::process_batch(",
          "new_string": "processor::handle_batch_items("
        }
      ]
    }
  ]
}"#;

    let plan_text = format!(
        "Verification: {} - {}\nPlan: {}\nScope: {}\nAffected areas: {}{}",
        if plan.verified { "CONFIRMED" } else { "UNCONFIRMED" },
        plan.verification_note,
        plan.description,
        plan.scope.label(),
        plan.affected_areas.join(", "),
        plan.modifier.as_ref().map(|m| format!("\nUser modifications: {}", m)).unwrap_or_default()
    );

    let memory_section = repo_memory
        .as_deref()
        .filter(|m| !m.trim().is_empty())
        .map(|m| format!("\n\nRepo conventions / decisions:\n{}", m))
        .unwrap_or_default();

    // Build the files section
    let files_section: String = files.iter()
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

    let response = call_llm_with_usage(system, &user, Model::Smart, true).await?;
    
    // Parse the JSON response
    let json_str = extract_json_object(&response.content)
        .ok_or_else(|| anyhow::anyhow!("No JSON found in multi-file fix response"))?;
    
    let parsed: serde_json::Value = serde_json::from_str(json_str)
        .map_err(|e| anyhow::anyhow!("Failed to parse multi-file fix JSON: {}", e))?;
    
    let description = parsed.get("description")
        .and_then(|v| v.as_str())
        .unwrap_or("Applied the requested multi-file fix")
        .to_string();
    
    // Parse file edits
    let file_edits_json: Vec<FileEditsJson> = parsed.get("file_edits")
        .and_then(|v| serde_json::from_value(v.clone()).ok())
        .ok_or_else(|| anyhow::anyhow!("Missing or invalid 'file_edits' array in response"))?;
    
    if file_edits_json.is_empty() {
        return Err(anyhow::anyhow!("No file edits provided in response"));
    }
    
    // Apply edits to each file
    let mut file_edits = Vec::new();
    
    for file_edit_json in file_edits_json {
        let file_path = PathBuf::from(&file_edit_json.file);
        
        // Find the original content for this file
        let original_content = files.iter()
            .find(|(p, _)| {
                // Match by file name or full path
                p == &file_path || 
                p.file_name() == file_path.file_name() ||
                p.ends_with(&file_path)
            })
            .map(|(_, content)| content.as_str())
            .ok_or_else(|| anyhow::anyhow!("File {} not found in provided files", file_edit_json.file))?;
        
        // Apply edits sequentially
        let mut new_content = original_content.to_string();
        let mut modified_areas = Vec::new();
        
        for (i, edit) in file_edit_json.edits.iter().enumerate() {
            let matches: Vec<_> = new_content.match_indices(&edit.old_string).collect();
            
            if matches.is_empty() {
                return Err(anyhow::anyhow!(
                    "Edit {} in {}: old_string not found.\nSearched for: {:?}",
                    i + 1,
                    file_edit_json.file,
                    truncate_for_error(&edit.old_string)
                ));
            }
            
            if matches.len() > 1 {
                return Err(anyhow::anyhow!(
                    "Edit {} in {}: old_string matches {} times (must be unique).\nSearched for: {:?}",
                    i + 1,
                    file_edit_json.file,
                    matches.len(),
                    truncate_for_error(&edit.old_string)
                ));
            }
            
            new_content = new_content.replacen(&edit.old_string, &edit.new_string, 1);
            
            // Try to extract function/area name from the edit
            if let Some(area) = extract_modified_area(&edit.old_string) {
                if !modified_areas.contains(&area) {
                    modified_areas.push(area);
                }
            }
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
            return Err(anyhow::anyhow!("Generated content for {} is empty", file_edit_json.file));
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
        usage: response.usage,
    })
}

/// Try to extract a function/struct name from an edit string
fn extract_modified_area(old_string: &str) -> Option<String> {
    // Look for common patterns like "fn name(", "pub fn name(", "struct Name", etc.
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
    Small,   // Few lines changed
    Medium,  // A function or two
    Large,   // Multiple functions or file restructure
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
    let system = r#"You are a code assistant. First VERIFY whether this issue actually exists in the code, then describe what changes would fix it.

OUTPUT FORMAT (JSON):
{
  "verified": true,
  "friendly_title": "Batch Processing",
  "problem_summary": "When processing multiple items at once, if any single item fails, all remaining items are abandoned.",
  "outcome": "Each item will be handled independently - one failure won't stop the rest from completing.",
  "verification_note": "Brief explanation of whether the issue was found and where",
  "evidence_snippet": "const BATCH_SIZE = 1000;",
  "evidence_line": 42,
  "description": "1-2 sentence description of what will change (if verified)",
  "affected_areas": ["function_name", "another_function"],
  "scope": "small"
}

RULES:
- verified: boolean true if issue exists, false if it doesn't exist or was already fixed
- friendly_title: A short, non-technical topic name (2-4 words). NO file names, NO function names.
- problem_summary: Describe what HAPPENS (behavior) not HOW it works (code). Write for someone who doesn't know programming. 1-2 sentences max.
- outcome: Describe what will be DIFFERENT after the fix. Focus on the result, not the implementation. 1 sentence.
- verification_note: explain what you found (technical, for internal use)
- evidence_snippet: 1-3 lines of the ACTUAL code from the file that proves your claim. Only include the relevant code, not surrounding context. Omit if no specific code evidence is needed.
- evidence_line: the line number where the evidence snippet starts
- scope: one of "small", "medium", or "large"

IMPORTANT for friendly_title, problem_summary, and outcome:
- Write for a NON-TECHNICAL audience who doesn't know programming
- NEVER use: function names, variable names, file names, code syntax
- NEVER use: try/catch, Promise, async/await, callback, API, endpoint, etc.
- Describe BEHAVIOR (what happens to the user/system) not IMPLEMENTATION (what code does)
- Use simple, everyday language

Be concise. The verification note should explain the finding in plain English. The evidence snippet shows proof."#;

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

    let response = call_llm(system, &user, Model::Balanced).await?;
    parse_fix_preview(&response, modifier.map(String::from))
}

/// Parse the preview JSON response
fn parse_fix_preview(response: &str, modifier: Option<String>) -> anyhow::Result<FixPreview> {
    // Extract JSON from response
    let json_str = extract_json_object(response)
        .ok_or_else(|| anyhow::anyhow!("No JSON found in preview response"))?;

    let parsed: serde_json::Value = serde_json::from_str(json_str)
        .map_err(|e| anyhow::anyhow!("Failed to parse preview JSON: {}", e))?;

    // Handle verified as either boolean or string
    let verified = parsed.get("verified")
        .map(|v| {
            if let Some(b) = v.as_bool() {
                b
            } else if let Some(s) = v.as_str() {
                s.eq_ignore_ascii_case("true")
            } else {
                true // Default to true
            }
        })
        .unwrap_or(true); // Default to true for backwards compatibility

    let verification_note = parsed.get("verification_note")
        .and_then(|v| v.as_str())
        .unwrap_or(if verified { "Issue verified" } else { "Issue not found" })
        .to_string();

    // Parse user-facing fields (non-technical)
    let friendly_title = parsed.get("friendly_title")
        .and_then(|v| v.as_str())
        .unwrap_or("Issue")
        .to_string();

    let problem_summary = parsed.get("problem_summary")
        .and_then(|v| v.as_str())
        .unwrap_or("An issue was found that needs attention.")
        .to_string();

    let outcome = parsed.get("outcome")
        .and_then(|v| v.as_str())
        .unwrap_or("This will be fixed.")
        .to_string();

    let evidence_snippet = parsed.get("evidence_snippet")
        .and_then(|v| v.as_str())
        .filter(|s| !s.trim().is_empty())
        .map(|s| s.to_string());

    let evidence_line = parsed.get("evidence_line")
        .and_then(|v| v.as_u64())
        .map(|n| n as u32);

    let description = parsed.get("description")
        .and_then(|v| v.as_str())
        .unwrap_or("Fix the identified issue")
        .to_string();

    let affected_areas = parsed.get("affected_areas")
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

// ═══════════════════════════════════════════════════════════════════════════
//  UNIFIED CODEBASE ANALYSIS
// ═══════════════════════════════════════════════════════════════════════════

/// Analyze entire codebase with @preset/smart for quality suggestions
/// 
/// This is the main entry point for generating high-quality suggestions.
/// Uses smart context building to pack maximum insight into the prompt.
/// Returns suggestions and usage stats for cost tracking.
/// 
/// The optional `glossary` provides domain-specific terminology to help
/// the LLM use the correct terms in suggestion summaries.
pub async fn analyze_codebase(
    index: &CodebaseIndex,
    context: &WorkContext,
    repo_memory: Option<String>,
    glossary: Option<&DomainGlossary>,
) -> anyhow::Result<(Vec<Suggestion>, Option<Usage>)> {
    let system = r#"You are a senior developer reviewing a codebase. Your job is to find genuinely useful improvements - things that will make the app better, not just cleaner.

OUTPUT FORMAT (JSON array, 5-10 suggestions):
[
  {
    "file": "relative/path/to/file.rs",
    "additional_files": ["other/file.rs"],
    "kind": "improvement|bugfix|feature|optimization|quality|documentation|testing",
    "priority": "high|medium|low",
    "summary": "Plain-language description of the problem and its impact on users",
    "detail": "Technical explanation with specific guidance for developers",
    "line": null or specific line number if applicable
  }
]

MULTI-FILE SUGGESTIONS:
Use "additional_files" when a change requires coordinated edits across multiple files:
- Renaming a function/type and updating all callers
- Extracting shared code into a new module and updating imports
- Fixing an interface change that affects multiple implementations
- Refactoring that requires updating both definition and usage sites
Leave "additional_files" empty or omit it for single-file changes.

SUMMARY FORMAT - WRITE FOR NON-TECHNICAL READERS:
Describe what HAPPENS to users, not what code does. A product manager should understand this.

GOOD EXAMPLES:
- "When processing a batch of items, if one item fails, all remaining items are skipped and never processed"
- "Price alerts sometimes fail to send during brief network hiccups, so users miss time-sensitive deals"
- "The trading calculator shows invalid results when there's not enough price history, confusing users"
- "Bulk imports can hang indefinitely if a single record has bad data, with no indication of what went wrong"

BAD EXAMPLES (rejected - too technical):
- "processEmailQueue() throws on empty batch" (users don't know what functions are)
- "divides by zero when dataset < trim_count" (technical jargon)
- "no retry logic for Resend API 5xx errors" (meaningless to non-developers)
- "Promise.all rejects" or "async/await" or "try/catch" (code concepts)

NEVER USE IN SUMMARIES:
- Function names, variable names, or file names
- Technical terms: API, async, callback, exception, null, undefined, NaN, array, object
- Code concepts: try/catch, Promise, error handling, retry logic, race condition
- Jargon: 5xx, 4xx, HTTP, JSON, SQL, query, endpoint

INSTEAD, DESCRIBE:
- What the user sees or experiences
- What action fails or behaves unexpectedly
- What business outcome is affected

WHAT TO LOOK FOR (aim for variety):
- **Bugs & Edge Cases**: Race conditions, off-by-one errors, null/None handling, error swallowing
- **Security**: Hardcoded secrets, SQL injection, XSS, path traversal, insecure defaults
- **Performance**: N+1 queries, unnecessary allocations, blocking in async, missing caching
- **Reliability**: Missing retries for network calls, no timeouts, silent failures
- **User Experience**: Error messages that don't help, missing loading states

AVOID:
- Technical jargon in summaries (save that for the "detail" field)
- Function names, code syntax, or programming concepts in summaries
- "Split this file" or "break this function up" unless it's genuinely causing problems
- Generic advice like "add more comments" or "improve naming"
- Suggestions that would just make the code "cleaner" without real benefit

PRIORITIZE:
- Files marked [CHANGED] - the developer is actively working there
- Things that could cause bugs or outages
- Quick wins that provide immediate value
- Use DOMAIN TERMINOLOGY when provided (use this project's specific business terms, not code terms)"#;

    let user_prompt = build_codebase_context(index, context, repo_memory.as_deref(), glossary);
    
    // Use Smart preset for quality reasoning on suggestions
    let response = call_llm_with_usage(system, &user_prompt, Model::Smart, true).await?;
    
    let suggestions = parse_codebase_suggestions(&response.content)?;
    Ok((suggestions, response.usage))
}

/// Build rich context from codebase index for the LLM prompt
fn build_codebase_context(
    index: &CodebaseIndex, 
    context: &WorkContext, 
    repo_memory: Option<&str>,
    glossary: Option<&DomainGlossary>,
) -> String {
    let stats = index.stats();
    let project_name = index.root.file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("project");
    
    let mut sections = Vec::new();
    
    // Header with overview
    sections.push(format!(
        "CODEBASE: {} ({} files, {} LOC)\nBRANCH: {} | FOCUS: {}",
        project_name,
        stats.file_count,
        stats.total_loc,
        context.branch,
        context.inferred_focus.as_deref().unwrap_or("general"),
    ));
    
    // Uncommitted changes FIRST (highest priority)
    if !context.uncommitted_files.is_empty() || !context.staged_files.is_empty() {
        let mut changes_section = String::from("\n\nACTIVELY WORKING ON [CHANGED]:");
        for file in context.uncommitted_files.iter().chain(context.staged_files.iter()).take(15) {
            // Include file details if we have them
            if let Some(file_index) = index.files.get(file) {
                let exports: Vec<_> = file_index.symbols.iter()
                    .filter(|s| s.visibility == crate::index::Visibility::Public)
                    .take(5)
                    .map(|s| s.name.as_str())
                    .collect();
                let exports_str = if exports.is_empty() { String::new() } else { format!(" exports: {}", exports.join(", ")) };
                changes_section.push_str(&format!("\n- {} ({} LOC){}",
                    file.display(), file_index.loc, exports_str));
            } else {
                changes_section.push_str(&format!("\n- {}", file.display()));
            }
        }
        sections.push(changes_section);
    }

    // Blast radius: files affected by the current changes (direct importers + direct deps)
    if !context.all_changed_files().is_empty() {
        let changed: std::collections::HashSet<PathBuf> = context
            .all_changed_files()
            .into_iter()
            .cloned()
            .collect();
        let mut related: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();

        for c in &changed {
            if let Some(file_index) = index.files.get(c) {
                // Who imports this file?
                for u in file_index.summary.used_by.iter().take(10) {
                    related.insert(u.clone());
                }
                // What does this file depend on?
                for d in file_index.summary.depends_on.iter().take(10) {
                    related.insert(d.clone());
                }
            }
        }
        for c in &changed {
            related.remove(c);
        }

        if !related.is_empty() {
            let mut list: Vec<_> = related.into_iter().collect();
            list.sort();
            let mut blast = String::from("\n\nBLAST RADIUS (related to [CHANGED]):");
            for path in list.into_iter().take(15) {
                blast.push_str(&format!("\n- {}", path.display()));
            }
            sections.push(blast);
        }
    }

    // Repo memory / conventions (solo dev “second brain”)
    if let Some(mem) = repo_memory {
        let mem = mem.trim();
        if !mem.is_empty() {
            sections.push(format!("\n\nREPO MEMORY (follow these conventions):\n{}", mem));
        }
    }

    // Domain glossary - terminology specific to this codebase
    if let Some(g) = glossary {
        let glossary_context = g.to_prompt_context(15);
        if !glossary_context.is_empty() {
            sections.push(format!("\n\n{}", glossary_context));
        }
    }
    
    // Recent commits for understanding what's being worked on
    if !context.recent_commits.is_empty() {
        let mut commits_section = String::from("\n\nRECENT COMMITS:");
        for commit in context.recent_commits.iter().take(5) {
            commits_section.push_str(&format!(
                "\n- {}: {}",
                commit.short_sha,
                truncate_str(&commit.message, 60),
            ));
        }
        sections.push(commits_section);
    }
    
    // Key files with their purpose (not just metrics)
    let mut files_section = String::from("\n\nKEY FILES:");
    let files_by_priority = index.files_by_priority();
    
    for (path, file_index) in files_by_priority.iter().take(25) {
        let is_changed = context.all_changed_files().iter().any(|f| f == path);
        if is_changed { continue; } // Already listed above
        
        // Get public exports to understand what this file does
        let exports: Vec<_> = file_index.symbols.iter()
            .filter(|s| s.visibility == crate::index::Visibility::Public)
            .take(4)
            .map(|s| s.name.as_str())
            .collect();
        
        let exports_str = if exports.is_empty() { 
            String::new() 
        } else { 
            format!(" → {}", exports.join(", ")) 
        };
        
        files_section.push_str(&format!(
            "\n- {} ({} LOC){}",
            path.display(),
            file_index.loc,
            exports_str
        ));
    }
    sections.push(files_section);
    
    // TODOs and FIXMEs found in code (actionable items from the developer)
    let todos: Vec<_> = index.patterns.iter()
        .filter(|p| matches!(p.kind, PatternKind::TodoMarker))
        .take(10)
        .collect();
    
    if !todos.is_empty() {
        let mut todos_section = String::from("\n\nTODO/FIXME MARKERS IN CODE:");
        for todo in &todos {
            todos_section.push_str(&format!(
                "\n- {}:{} - {}",
                todo.file.file_name().and_then(|n| n.to_str()).unwrap_or("?"),
                todo.line,
                truncate_str(&todo.description, 70)
            ));
        }
        sections.push(todos_section);
    }
    
    // Final instruction - open-ended
    sections.push(String::from(
        "\n\nLook for bugs, security issues, performance problems, missing error handling, \
         UX improvements, and feature opportunities. Prioritize the [CHANGED] files (and BLAST RADIUS). \
         Give me varied, specific suggestions - not just code organization advice."
    ));
    
    sections.join("")
}

/// Parse suggestions from codebase-wide analysis
fn parse_codebase_suggestions(response: &str) -> anyhow::Result<Vec<Suggestion>> {
    // Strip markdown code fences if present
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

    // Handle both array format and object-with-suggestions format
    // Speed preset often returns {"suggestions": [...]} instead of just [...]
    let json_str = if clean.starts_with('{') {
        // Try to extract "suggestions" array from object
        if let Ok(obj) = serde_json::from_str::<serde_json::Value>(clean) {
            if let Some(suggestions) = obj.get("suggestions") {
                // Convert suggestions array back to string for parsing
                match serde_json::to_string(suggestions) {
                    Ok(s) => s,
                    Err(_) => clean.to_string(),
                }
            } else {
                clean.to_string()
            }
        } else {
            clean.to_string()
        }
    } else if let Some(start) = clean.find('[') {
        // Extract JSON array from response
        if let Some(end) = clean.rfind(']') {
            clean[start..=end].to_string()
        } else {
            clean.to_string()
        }
    } else {
        clean.to_string()
    };

    // Try to parse as array first
    let parsed: Vec<CodebaseSuggestionJson> = match serde_json::from_str(&json_str) {
        Ok(v) => v,
        Err(e) => {
            // Try to fix common JSON issues and retry
            let fixed = fix_json_issues(&json_str);
            match serde_json::from_str(&fixed) {
                Ok(v) => v,
                Err(_) => {
                    // If still failing, try to extract individual objects and parse them
                    match try_parse_individual_suggestions(&json_str) {
                        Ok(v) if !v.is_empty() => v,
                        _ => {
                            // Log the error but return empty instead of crashing
                            eprintln!("Warning: Failed to parse LLM suggestions: {}", e);
                            eprintln!("Response preview: {}", truncate_str(&json_str, 300));
                            return Ok(Vec::new());
                        }
                    }
                }
            }
        }
    };

    let suggestions = parsed
        .into_iter()
        .map(|s| {
            let kind = match s.kind.as_str() {
                "bugfix" => SuggestionKind::BugFix,
                "feature" => SuggestionKind::Feature,
                "optimization" => SuggestionKind::Optimization,
                "quality" => SuggestionKind::Quality,
                "documentation" => SuggestionKind::Documentation,
                "testing" => SuggestionKind::Testing,
                _ => SuggestionKind::Improvement,
            };

            let priority = match s.priority.as_str() {
                "high" => Priority::High,
                "low" => Priority::Low,
                _ => Priority::Medium,
            };

            let file_path = PathBuf::from(&s.file);
            let additional_files: Vec<PathBuf> = s.additional_files
                .into_iter()
                .map(|f| PathBuf::from(f))
                .collect();
            
            let mut suggestion = Suggestion::new(
                kind,
                priority,
                file_path,
                s.summary,
                SuggestionSource::LlmDeep,
            )
            .with_detail(s.detail)
            .with_additional_files(additional_files);

            if let Some(line) = s.line {
                suggestion = suggestion.with_line(line);
            }

            suggestion
        })
        .collect();

    Ok(suggestions)
}

#[derive(Deserialize)]
struct CodebaseSuggestionJson {
    file: String,
    #[serde(default)]
    additional_files: Vec<String>,
    kind: String,
    priority: String,
    summary: String,
    detail: String,
    line: Option<usize>,
}

/// Truncate a string for display (Unicode-safe)
fn truncate_str(s: &str, max_chars: usize) -> &str {
    if s.chars().count() <= max_chars {
        s
    } else {
        // Find byte index of the max_chars-th character
        let byte_idx = s.char_indices()
            .nth(max_chars)
            .map(|(i, _)| i)
            .unwrap_or(s.len());
        &s[..byte_idx]
    }
}

/// Try to fix common JSON issues from LLM responses
fn fix_json_issues(json: &str) -> String {
    let mut fixed = json.to_string();
    
    // Remove trailing commas before ] or }
    fixed = fixed.replace(",]", "]");
    fixed = fixed.replace(",}", "}");
    
    // Fix common quote issues - smart quotes to regular quotes
    fixed = fixed.replace('\u{201C}', "\"");  // Left double quote
    fixed = fixed.replace('\u{201D}', "\"");  // Right double quote
    fixed = fixed.replace('\u{2018}', "'");   // Left single quote
    fixed = fixed.replace('\u{2019}', "'");   // Right single quote
    
    // Remove any control characters that might have slipped in
    fixed = fixed.chars().filter(|c| !c.is_control() || *c == '\n' || *c == '\t').collect();
    
    fixed
}

/// Try to parse individual suggestion objects if array parsing fails
fn try_parse_individual_suggestions(json: &str) -> anyhow::Result<Vec<CodebaseSuggestionJson>> {
    let mut suggestions = Vec::new();
    let mut depth = 0;
    let mut start = None;
    
    for (i, c) in json.char_indices() {
        match c {
            '{' => {
                if depth == 0 {
                    start = Some(i);
                }
                depth += 1;
            }
            '}' => {
                depth -= 1;
                if depth == 0 {
                    if let Some(s) = start {
                        let obj_str = &json[s..=i];
                        if let Ok(suggestion) = serde_json::from_str::<CodebaseSuggestionJson>(obj_str) {
                            suggestions.push(suggestion);
                        }
                    }
                    start = None;
                }
            }
            _ => {}
        }
    }
    
    Ok(suggestions)
}

/// Parse JSON suggestions from LLM response
#[allow(dead_code)]
fn parse_suggestions(response: &str, path: &PathBuf) -> anyhow::Result<Vec<Suggestion>> {
    // Strip markdown code fences if present
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

    // Try to extract JSON array from response
    let json_str = if let Some(start) = clean.find('[') {
        if let Some(end) = clean.rfind(']') {
            &clean[start..=end]
        } else {
            clean
        }
    } else {
        clean
    };

    let parsed: Vec<SuggestionJson> = serde_json::from_str(json_str)
        .map_err(|e| anyhow::anyhow!("Failed to parse suggestions: {} | Response preview: {}", e, truncate_str(json_str, 200)))?;

    let suggestions = parsed
        .into_iter()
        .map(|s| {
            let kind = match s.kind.as_str() {
                "bugfix" => SuggestionKind::BugFix,
                "feature" => SuggestionKind::Feature,
                "optimization" => SuggestionKind::Optimization,
                "quality" => SuggestionKind::Quality,
                "documentation" => SuggestionKind::Documentation,
                "testing" => SuggestionKind::Testing,
                _ => SuggestionKind::Improvement,
            };

            let priority = match s.priority.as_str() {
                "high" => Priority::High,
                "low" => Priority::Low,
                _ => Priority::Medium,
            };

            let mut suggestion = Suggestion::new(
                kind,
                priority,
                path.clone(),
                s.summary,
                SuggestionSource::LlmDeep,
            )
            .with_detail(s.detail);

            if let Some(line) = s.line {
                suggestion = suggestion.with_line(line);
            }

            suggestion
        })
        .collect();

    Ok(suggestions)
}

#[derive(Deserialize)]
struct SuggestionJson {
    kind: String,
    priority: String,
    summary: String,
    detail: String,
    line: Option<usize>,
}

/// Truncate content for API calls
#[allow(dead_code)]
fn truncate_content(content: &str, max_chars: usize) -> String {
    if content.len() <= max_chars {
        content.to_string()
    } else {
        // Try to truncate at a line boundary
        let truncated = &content[..max_chars];
        if let Some(last_newline) = truncated.rfind('\n') {
            format!("{}\n... (truncated)", &content[..last_newline])
        } else {
            format!("{}... (truncated)", truncated)
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════════
//  PROJECT CONTEXT DISCOVERY
// ═══════════════════════════════════════════════════════════════════════════

/// Discover project context from README, package files, and structure.
/// Returns a concise description to help the LLM understand file purposes.
pub fn discover_project_context(index: &CodebaseIndex) -> String {
    let project_name = index.root.file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("project");
    
    let mut context_parts = Vec::new();
    
    // 1. Try to read README.md
    let readme_content = try_read_readme(&index.root);
    if let Some(readme) = readme_content {
        context_parts.push(readme);
    }
    
    // 2. Try to read package description from Cargo.toml, package.json, etc.
    let package_desc = try_read_package_description(&index.root);
    if let Some(desc) = package_desc {
        context_parts.push(desc);
    }
    
    // 3. Analyze file structure for domain hints
    let structure_hints = analyze_project_structure(index);
    if !structure_hints.is_empty() {
        context_parts.push(structure_hints);
    }
    
    // Combine and truncate
    if context_parts.is_empty() {
        format!("Project: {}", project_name)
    } else {
        let combined = context_parts.join("\n\n");
        // Truncate to ~1000 chars to keep prompt size manageable
        if combined.len() > 1000 {
            format!("{}...", &combined[..1000])
        } else {
            combined
        }
    }
}

/// Try to read and extract key info from README.md
fn try_read_readme(root: &std::path::Path) -> Option<String> {
    // Try common README filenames
    let readme_names = ["README.md", "readme.md", "README.MD", "README", "readme"];
    
    for name in readme_names {
        let path = root.join(name);
        if path.exists() {
            if let Ok(content) = std::fs::read_to_string(&path) {
                return Some(extract_readme_summary(&content));
            }
        }
    }
    None
}

/// Extract the first meaningful section from README
fn extract_readme_summary(content: &str) -> String {
    let mut result = Vec::new();
    let mut in_code_block = false;
    let mut found_header = false;
    let mut line_count = 0;
    
    for line in content.lines() {
        // Skip code blocks
        if line.starts_with("```") {
            in_code_block = !in_code_block;
            continue;
        }
        if in_code_block {
            continue;
        }
        
        // Skip badges and empty lines at the start
        if !found_header && (line.trim().is_empty() || line.contains("![") || line.contains("[![")) {
            continue;
        }
        
        // Found meaningful content
        found_header = true;
        
        // Skip table of contents style lines
        if line.starts_with("- [") || line.starts_with("* [") {
            continue;
        }
        
        // Add line
        let trimmed = line.trim();
        if !trimmed.is_empty() {
            result.push(trimmed.to_string());
            line_count += 1;
        }
        
        // Get first ~10 meaningful lines
        if line_count >= 10 {
            break;
        }
    }
    
    if result.is_empty() {
        return String::new();
    }
    
    format!("README:\n{}", result.join("\n"))
}

/// Try to read project description from package files
fn try_read_package_description(root: &std::path::Path) -> Option<String> {
    // Try Cargo.toml
    let cargo_path = root.join("Cargo.toml");
    if cargo_path.exists() {
        if let Ok(content) = std::fs::read_to_string(&cargo_path) {
            if let Some(desc) = extract_cargo_description(&content) {
                return Some(desc);
            }
        }
    }
    
    // Try package.json
    let package_path = root.join("package.json");
    if package_path.exists() {
        if let Ok(content) = std::fs::read_to_string(&package_path) {
            if let Some(desc) = extract_package_json_description(&content) {
                return Some(desc);
            }
        }
    }
    
    // Try pyproject.toml
    let pyproject_path = root.join("pyproject.toml");
    if pyproject_path.exists() {
        if let Ok(content) = std::fs::read_to_string(&pyproject_path) {
            if let Some(desc) = extract_pyproject_description(&content) {
                return Some(desc);
            }
        }
    }
    
    None
}

fn extract_cargo_description(content: &str) -> Option<String> {
    let mut name = None;
    let mut description = None;
    
    for line in content.lines() {
        let line = line.trim();
        if line.starts_with("name = ") {
            name = line.split('"').nth(1).map(|s| s.to_string());
        }
        if line.starts_with("description = ") {
            description = line.split('"').nth(1).map(|s| s.to_string());
        }
    }
    
    match (name, description) {
        (Some(n), Some(d)) => Some(format!("Package: {} - {}", n, d)),
        (Some(n), None) => Some(format!("Package: {}", n)),
        (None, Some(d)) => Some(format!("Description: {}", d)),
        _ => None,
    }
}

fn extract_package_json_description(content: &str) -> Option<String> {
    // Simple JSON parsing for name and description
    let mut name = None;
    let mut description = None;
    
    for line in content.lines() {
        let line = line.trim();
        if line.starts_with("\"name\"") {
            name = line.split(':').nth(1)
                .and_then(|s| s.trim().trim_matches(|c| c == '"' || c == ',').split('"').next())
                .map(|s| s.to_string());
        }
        if line.starts_with("\"description\"") {
            description = line.split(':').nth(1)
                .and_then(|s| s.trim().trim_matches(|c| c == '"' || c == ',').split('"').next())
                .map(|s| s.to_string());
        }
    }
    
    match (name, description) {
        (Some(n), Some(d)) => Some(format!("Package: {} - {}", n, d)),
        (Some(n), None) => Some(format!("Package: {}", n)),
        (None, Some(d)) => Some(format!("Description: {}", d)),
        _ => None,
    }
}

fn extract_pyproject_description(content: &str) -> Option<String> {
    let mut name = None;
    let mut description = None;
    
    for line in content.lines() {
        let line = line.trim();
        if line.starts_with("name = ") {
            name = line.split('"').nth(1).map(|s| s.to_string());
        }
        if line.starts_with("description = ") {
            description = line.split('"').nth(1).map(|s| s.to_string());
        }
    }
    
    match (name, description) {
        (Some(n), Some(d)) => Some(format!("Project: {} - {}", n, d)),
        (Some(n), None) => Some(format!("Project: {}", n)),
        (None, Some(d)) => Some(format!("Description: {}", d)),
        _ => None,
    }
}

/// Analyze project structure for domain hints
fn analyze_project_structure(index: &CodebaseIndex) -> String {
    let mut hints = Vec::new();
    
    // Count files by directory
    let mut dir_counts: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for path in index.files.keys() {
        if let Some(parent) = path.parent() {
            let dir = parent.to_string_lossy().to_string();
            *dir_counts.entry(dir).or_insert(0) += 1;
        }
    }
    
    // Identify key directories
    let key_dirs: Vec<_> = dir_counts.iter()
        .filter(|(_, count)| **count > 2)
        .map(|(dir, count)| format!("{} ({} files)", dir, count))
        .take(5)
        .collect();
    
    if !key_dirs.is_empty() {
        hints.push(format!("Key directories: {}", key_dirs.join(", ")));
    }
    
    // Identify technologies
    let mut technologies = Vec::new();
    let files: Vec<_> = index.files.keys().collect();
    
    if files.iter().any(|p| p.extension().map(|e| e == "rs").unwrap_or(false)) {
        technologies.push("Rust");
    }
    if files.iter().any(|p| p.extension().map(|e| e == "ts" || e == "tsx").unwrap_or(false)) {
        technologies.push("TypeScript");
    }
    if files.iter().any(|p| p.extension().map(|e| e == "js" || e == "jsx").unwrap_or(false)) {
        technologies.push("JavaScript");
    }
    if files.iter().any(|p| p.extension().map(|e| e == "py").unwrap_or(false)) {
        technologies.push("Python");
    }
    if files.iter().any(|p| p.extension().map(|e| e == "go").unwrap_or(false)) {
        technologies.push("Go");
    }
    
    if !technologies.is_empty() {
        hints.push(format!("Technologies: {}", technologies.join(", ")));
    }
    
    // File count summary
    hints.push(format!("Total: {} files, {} symbols", index.files.len(), index.symbols.len()));
    
    hints.join("\n")
}

// ═══════════════════════════════════════════════════════════════════════════
//  FILE SUMMARIES GENERATION
// ═══════════════════════════════════════════════════════════════════════════

/// Result from a single batch of file summaries
pub struct SummaryBatchResult {
    pub summaries: HashMap<PathBuf, String>,
    /// Domain terms extracted from these files
    pub terms: HashMap<String, String>,
    pub usage: Option<Usage>,
}

/// Generate rich, context-aware summaries for all files in the codebase
/// 
/// Uses batched approach (4 files per call) for reliability.
/// Returns all summaries, domain glossary, and total usage stats.
/// 
/// DEPRECATED: Use generate_file_summaries_incremental instead for caching support.
#[allow(dead_code)]
pub async fn generate_file_summaries(
    index: &CodebaseIndex,
) -> anyhow::Result<(HashMap<PathBuf, String>, DomainGlossary, Option<Usage>)> {
    let project_context = discover_project_context(index);
    let files: Vec<_> = index.files.keys().cloned().collect();
    generate_summaries_for_files(index, &files, &project_context).await
}

/// Generate summaries for a specific list of files with project context
/// Uses aggressive parallel batch processing for speed
/// Also extracts domain terminology for the glossary
pub async fn generate_summaries_for_files(
    index: &CodebaseIndex,
    files: &[PathBuf],
    project_context: &str,
) -> anyhow::Result<(HashMap<PathBuf, String>, DomainGlossary, Option<Usage>)> {
    // Large batch size for fewer API calls
    let batch_size = 16;
    // Higher concurrency for faster processing (Speed preset handles this well)
    let concurrency = 4;
    
    let batches: Vec<_> = files.chunks(batch_size).collect();
    
    let mut all_summaries = HashMap::new();
    let mut glossary = DomainGlossary::new();
    let mut total_usage = Usage::default();
    
    // Process batches with limited concurrency
    for batch_group in batches.chunks(concurrency) {
        // Run concurrent batches
        let futures: Vec<_> = batch_group
            .iter()
            .map(|batch| generate_summary_batch(index, batch, project_context))
            .collect();
        
        let results = futures::future::join_all(futures).await;
        
        for result in results {
            match result {
                Ok(batch_result) => {
                    // Collect summaries
                    all_summaries.extend(batch_result.summaries.clone());
                    
                    // Collect terms into glossary
                    for (term, definition) in batch_result.terms {
                        // Associate term with files from this batch
                        for file in batch_result.summaries.keys() {
                            glossary.add_term(term.clone(), definition.clone(), file.clone());
                        }
                    }
                    
                    if let Some(usage) = batch_result.usage {
                        total_usage.prompt_tokens += usage.prompt_tokens;
                        total_usage.completion_tokens += usage.completion_tokens;
                        total_usage.total_tokens += usage.total_tokens;
                    }
                }
                Err(e) => {
                    // Log error but continue with other batches
                    eprintln!("Warning: Failed to generate summaries for batch: {}", e);
                }
            }
        }
    }
    
    let final_usage = if total_usage.total_tokens > 0 {
        Some(total_usage)
    } else {
        None
    };
    
    Ok((all_summaries, glossary, final_usage))
}

/// Priority tier for file summarization
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SummaryPriority {
    /// Tier 1: Changed files, high complexity - summarize immediately
    High,
    /// Tier 2: Files with suggestions, focus directories - summarize soon  
    Medium,
    /// Tier 3: Everything else - background processing
    Low,
}

/// Categorize files by priority for smart summarization
pub fn prioritize_files_for_summary(
    index: &CodebaseIndex,
    context: &crate::context::WorkContext,
    files_needing_summary: &[PathBuf],
) -> (Vec<PathBuf>, Vec<PathBuf>, Vec<PathBuf>) {
    let mut high_priority = Vec::new();
    let mut medium_priority = Vec::new();
    let mut low_priority = Vec::new();
    
    let changed_files: std::collections::HashSet<_> = context.all_changed_files().into_iter().collect();
    
    for path in files_needing_summary {
        // Check if file is in the index
        let file_index = match index.files.get(path) {
            Some(fi) => fi,
            None => {
                low_priority.push(path.clone());
                continue;
            }
        };
        
        // Tier 1: Changed files or high complexity
        if changed_files.contains(path) || file_index.complexity > 20.0 || file_index.loc > 500 {
            high_priority.push(path.clone());
            continue;
        }
        
        // Tier 2: Recent modification or in focus area
        let is_recent = file_index.last_modified.timestamp() > 
            (chrono::Utc::now() - chrono::Duration::days(7)).timestamp();
        let in_focus = context.inferred_focus.as_ref()
            .map(|focus| path.to_string_lossy().contains(focus))
            .unwrap_or(false);
        
        if is_recent || in_focus {
            medium_priority.push(path.clone());
            continue;
        }
        
        // Tier 3: Everything else
        low_priority.push(path.clone());
    }
    
    (high_priority, medium_priority, low_priority)
}

/// Generate summaries for a single batch of files
/// Also extracts domain-specific terminology for the glossary
async fn generate_summary_batch(
    index: &CodebaseIndex,
    files: &[PathBuf],
    project_context: &str,
) -> anyhow::Result<SummaryBatchResult> {
    let system = r#"You are a senior developer writing documentation. For each file, write a 2-6 sentence summary explaining:
- What this file IS (its purpose/role)
- What it DOES (key functionality, main exports)
- How it FITS (relationships to other parts)

ALSO extract domain-specific terminology - terms that are unique to THIS codebase and wouldn't be obvious to someone new. Look for:
- Business concepts (e.g., "DumpAlert" = price drop notification)
- Custom abstractions (e.g., "TaskQueue" = background job system)
- Domain entities (e.g., "Listing" = item for sale, "Watchlist" = user's tracked items)

IMPORTANT: Use the PROJECT CONTEXT provided to understand what this codebase is for. 
Write definitive statements like "This file handles X" not vague guesses.
Be specific and technical. Reference actual function/struct names.

OUTPUT: A JSON object with two keys:
{
  "summaries": {
    "src/main.rs": "This is the application entry point..."
  },
  "terms": {
    "DumpAlert": "Price drop notification sent to users when a watched item's price falls",
    "BatchProcessor": "System for handling bulk CSV imports of inventory data"
  }
}

For "terms": only include 3-8 domain-specific terms per batch. Skip generic programming terms (like "Controller", "Service", "Handler"). Focus on business/domain concepts that need explanation."#;

    let user_prompt = build_batch_context(index, files, project_context);
    
    let response = call_llm_with_usage(system, &user_prompt, Model::Speed, true).await?;
    
    let (summaries, terms) = parse_summaries_and_terms_response(&response.content, &index.root)?;
    
    Ok(SummaryBatchResult {
        summaries,
        terms,
        usage: response.usage,
    })
}

/// Build context for a batch of files
fn build_batch_context(index: &CodebaseIndex, files: &[PathBuf], project_context: &str) -> String {
    let project_name = index.root.file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("project");
    
    let mut sections = Vec::new();
    
    // Include project context at the top
    sections.push(format!(
        "PROJECT: {}\n\n=== PROJECT CONTEXT (use this to understand file purposes) ===\n{}\n=== END PROJECT CONTEXT ===\n\nFILES TO SUMMARIZE:",
        project_name,
        project_context
    ));
    
    for path in files {
        if let Some(file_index) = index.files.get(path) {
            let func_count = file_index.symbols.iter()
                .filter(|s| matches!(s.kind, SymbolKind::Function | SymbolKind::Method))
                .count();
            
            let struct_count = file_index.symbols.iter()
                .filter(|s| matches!(s.kind, SymbolKind::Struct | SymbolKind::Class | SymbolKind::Interface | SymbolKind::Trait))
                .count();
            
            // Get public exports
            let exports: Vec<_> = file_index.symbols.iter()
                .filter(|s| s.visibility == crate::index::Visibility::Public)
                .take(6)
                .map(|s| s.name.as_str())
                .collect();
            
            // Get imports
            let deps: Vec<_> = file_index.dependencies.iter()
                .filter(|d| !d.is_external)
                .take(4)
                .map(|d| d.import_path.as_str())
                .collect();
            
            let exports_str = if exports.is_empty() { 
                "none".to_string() 
            } else { 
                exports.join(", ") 
            };
            let deps_str = if deps.is_empty() { 
                "none".to_string() 
            } else { 
                deps.join(", ") 
            };
            
            sections.push(format!(
                "\n---\nFILE: {}\n{} LOC | {} functions | {} structs\nExports: {}\nImports: {}",
                path.display(),
                file_index.loc,
                func_count,
                struct_count,
                exports_str,
                deps_str
            ));
            
            // Add doc comments if available
            if let Ok(content) = std::fs::read_to_string(index.root.join(path)) {
                let doc_lines: Vec<_> = content.lines()
                    .take(10)
                    .filter(|l| l.starts_with("//!") || l.starts_with("///") || l.starts_with("#") || l.starts_with("\"\"\""))
                    .take(2)
                    .collect();
                
                if !doc_lines.is_empty() {
                    sections.push(format!("Doc: {}", doc_lines.join(" ")));
                }
            }
        }
    }
    
    sections.join("")
}

/// Extract JSON from LLM response, handling markdown fences and noise
fn extract_json_object(response: &str) -> Option<&str> {
    let trimmed = response.trim();
    
    // Remove markdown code fences
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
    
    // Find JSON object boundaries
    let start = clean.find('{')?;
    let end = clean.rfind('}')?;
    
    if start <= end {
        Some(&clean[start..=end])
    } else {
        None
    }
}

/// Normalize a path string to repo-relative format (wrapper around cache::normalize_summary_path)
fn normalize_path_str(raw: &str, root: &Path) -> PathBuf {
    crate::cache::normalize_summary_path(&PathBuf::from(raw.trim()), root)
}

#[allow(dead_code)]
fn parse_summaries_response(response: &str, root: &Path) -> anyhow::Result<HashMap<PathBuf, String>> {
    let json_str = extract_json_object(response)
        .ok_or_else(|| anyhow::anyhow!("No JSON object found in response"))?;

    // First try to parse as a simple {path: summary} object
    if let Ok(parsed) = serde_json::from_str::<HashMap<String, String>>(json_str) {
        let summaries = parsed
            .into_iter()
            .map(|(path, summary)| (normalize_path_str(&path, root), summary))
            .collect();
        return Ok(summaries);
    }

    // Try to parse as a wrapper object (e.g., {"analysis": {...}, "summaries": {...}})
    if let Ok(wrapper) = serde_json::from_str::<serde_json::Value>(json_str) {
        // Look for common wrapper keys that might contain summaries
        for key in ["summaries", "files", "results", "data"] {
            if let Some(inner) = wrapper.get(key) {
                if let Ok(parsed) = serde_json::from_value::<HashMap<String, String>>(inner.clone()) {
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

    // Final fallback: provide helpful error
    let preview = if json_str.len() > 200 {
        format!("{}...", &json_str[..200])
    } else {
        json_str.to_string()
    };
    Err(anyhow::anyhow!("Could not extract summaries from response. Preview: {}", preview))
}

/// Parse response containing both summaries and domain terms
fn parse_summaries_and_terms_response(
    response: &str, 
    root: &Path
) -> anyhow::Result<(HashMap<PathBuf, String>, HashMap<String, String>)> {
    let json_str = extract_json_object(response)
        .ok_or_else(|| anyhow::anyhow!("No JSON object found in response"))?;

    // Try to parse as the expected format: {summaries: {...}, terms: {...}}
    if let Ok(wrapper) = serde_json::from_str::<serde_json::Value>(json_str) {
        let mut summaries = HashMap::new();
        let mut terms = HashMap::new();

        // Extract summaries
        if let Some(summaries_obj) = wrapper.get("summaries") {
            if let Some(obj) = summaries_obj.as_object() {
                for (path, summary) in obj {
                    if let Some(s) = summary.as_str() {
                        summaries.insert(normalize_path_str(path, root), s.to_string());
                    }
                }
            }
        }

        // Extract terms
        if let Some(terms_obj) = wrapper.get("terms") {
            if let Some(obj) = terms_obj.as_object() {
                for (term, definition) in obj {
                    if let Some(d) = definition.as_str() {
                        // Only include non-empty definitions
                        if !d.trim().is_empty() {
                            terms.insert(term.clone(), d.to_string());
                        }
                    }
                }
            }
        }

        // If we got summaries, return (even if no terms)
        if !summaries.is_empty() {
            return Ok((summaries, terms));
        }

        // Fallback: maybe it's the old format (just summaries, no terms wrapper)
        // Try to extract file paths directly from the wrapper
        if let Some(obj) = wrapper.as_object() {
            for (key, value) in obj {
                // Skip known meta keys
                if key == "terms" || key == "analysis" || key == "notes" {
                    continue;
                }
                if let Some(summary) = value.as_str() {
                    summaries.insert(normalize_path_str(key, root), summary.to_string());
                }
            }
            if !summaries.is_empty() {
                return Ok((summaries, terms));
            }
        }
    }

    // Try old format as complete fallback
    let summaries = parse_summaries_response(response, root)?;
    Ok((summaries, HashMap::new()))
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
    pub severity: String,       // "critical", "warning", "suggestion", "nitpick"
    pub category: String,       // "bug", "security", "performance", "logic", "error-handling", "style"
    pub title: String,          // Short title
    pub description: String,    // Detailed explanation in plain language
    pub recommended: bool,      // Reviewer recommends fixing this (true = should fix, false = optional)
}

/// Result of a deep verification review
#[derive(Debug, Clone)]
pub struct VerificationReview {
    pub findings: Vec<ReviewFinding>,
    pub summary: String,        // Overall assessment
    #[allow(dead_code)]
    pub pass: bool,             // True if no critical/warning issues
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
    // First review: full adversarial mode
    // Re-reviews: focus on verifying fixes were correct, not finding new issues
    let system = if iteration <= 1 {
        r#"You are a skeptical senior code reviewer. Your job is to find bugs, security issues, and problems that the implementing developer might have missed.

BE ADVERSARIAL: Assume the code has bugs until proven otherwise. Look for:
- Logic errors and edge cases
- Off-by-one errors, null/None handling, empty collections
- Race conditions, deadlocks, resource leaks
- Security vulnerabilities (injection, XSS, path traversal, secrets)
- Error handling gaps (swallowed errors, missing validation)
- Performance issues (N+1 queries, unbounded loops, memory leaks)
- Type confusion, incorrect casts, precision loss
- Incorrect assumptions about input data

DO NOT praise good code. Your only job is to find problems.

OUTPUT FORMAT (JSON):
{
  "summary": "Brief overall assessment in plain language",
  "pass": false,
  "findings": [
    {
      "file": "path/to/file.rs",
      "line": 42,
      "severity": "critical",
      "category": "bug",
      "title": "Short description",
      "description": "Plain language explanation of what's wrong and why it matters. No code snippets.",
      "recommended": true
    }
  ]
}

SEVERITY LEVELS:
- critical: Must fix before shipping. Bugs, security issues, data loss risks.
- warning: Should fix. Logic issues, poor error handling, reliability concerns.
- suggestion: Consider fixing. Performance, maintainability, edge cases.
- nitpick: Minor. Style, naming, documentation. (Use sparingly)

RECOMMENDED FIELD - BE THOUGHTFUL:
- Set "recommended": true ONLY for issues that:
  * Are objectively bugs (logic errors, crashes, data corruption)
  * Can be fixed with CODE CHANGES to this file
  * The developer can reasonably fix right now

- Set "recommended": false for:
  * Architectural concerns ("should use Redis", "should use external queue")
  * Infrastructure requirements ("needs rate limiting at CDN/proxy level")
  * Issues requiring new dependencies or services
  * Security hardening that's nice-to-have but not a vulnerability
  * Concerns about deployment environments the code can't control
  * Theoretical edge cases that are unlikely in practice

RULES:
- Write descriptions in plain human language, no code snippets or technical jargon
- Explain WHY it's a problem and what could go wrong
- Focus on the CHANGES, not pre-existing code
- If an issue requires infrastructure changes, mention it but mark recommended: false
- Don't pile on - 2-3 high-quality findings are better than 10 marginal ones
- Return empty findings array if the code is genuinely solid"#.to_string()
    } else {
        // Re-review mode: ONLY verify fixes were correct, do not expand scope
        format!(r#"You are verifying that previously reported issues were fixed correctly.

This is RE-REVIEW #{iteration}. Your ONLY job is to verify the fixes work correctly.

PREVIOUSLY FIXED ISSUES:
{fixed_list}

VERIFY ONLY:
1. Were the specific issues above actually fixed?
2. Did the fix itself introduce a regression or new bug?

STRICT RULES - DO NOT REPORT:
- Architectural concerns (e.g., "should use Redis", "should use external service")
- Issues requiring infrastructure changes
- Security hardening that wasn't part of the original scope
- Edge cases in code that WASN'T changed by the fix
- Improvements to pre-existing code
- Theoretical concerns about deployment environments
- Style, naming, or documentation

SET recommended: false FOR:
- Any issue requiring significant refactoring
- Concerns about infrastructure or architecture
- Issues that are "nice to have" rather than bugs

SET recommended: true ONLY FOR:
- The fix is objectively broken (doesn't solve the stated problem)
- The fix introduced a clear bug (null pointer, infinite loop, data corruption)

IMPORTANT: If you already reported an issue and it was fixed, do NOT report a "deeper" version of the same issue. The developer addressed it; move on.

After {iteration} rounds, if fixes are reasonable, PASS. Perfect is the enemy of good.

OUTPUT FORMAT (JSON):
{{
  "summary": "Brief assessment - be concise",
  "pass": true,
  "findings": [
    {{
      "file": "path/to/file.rs",
      "line": 42,
      "severity": "warning",
      "category": "bug",
      "title": "Short description",
      "description": "Plain language explanation",
      "recommended": true
    }}
  ]
}}

If no issues found, use "findings": []"#,
            fixed_list = if fixed_titles.is_empty() {
                "(none recorded)".to_string()
            } else {
                fixed_titles.iter().map(|t| format!("- {}", t)).collect::<Vec<_>>().join("\n")
            }
        )
    };

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
    
    // Parse the response
    let json_str = extract_json_object(&response.content)
        .ok_or_else(|| anyhow::anyhow!("No JSON found in review response"))?;
    
    #[derive(Deserialize)]
    struct ReviewResponse {
        summary: String,
        pass: Option<bool>,
        findings: Vec<ReviewFinding>,
    }
    
    let parsed: ReviewResponse = serde_json::from_str(json_str)
        .map_err(|e| anyhow::anyhow!("Failed to parse review JSON: {}", e))?;
    
    // Determine pass based on findings if not explicitly set
    let pass = parsed.pass.unwrap_or_else(|| {
        !parsed.findings.iter().any(|f| 
            f.severity == "critical" || f.severity == "warning"
        )
    });
    
    Ok(VerificationReview {
        findings: parsed.findings,
        summary: parsed.summary,
        pass,
        usage: response.usage,
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

    // For later iterations, use a more detailed prompt that acknowledges the history
    let system = if iteration <= 1 {
        r#"You are a senior developer fixing issues found during code review.

For each finding, implement a fix using search/replace edits.

OUTPUT FORMAT (JSON):
{
  "description": "Brief summary of all fixes applied",
  "modified_areas": ["function_name", "another_function"],
  "edits": [
    {
      "old_string": "exact text to find and replace",
      "new_string": "replacement text"
    }
  ]
}

CRITICAL RULES FOR EDITS:
- old_string must be EXACT text from the file (copy-paste precision)
- old_string must be UNIQUE in the file - include enough context
- Preserve indentation exactly
- Fix the ROOT CAUSE, not just the symptom
- Don't introduce new issues while fixing old ones
- If a finding seems incorrect, still make a reasonable improvement"#.to_string()
    } else {
        format!(r#"You are a senior developer fixing issues found during code review.

IMPORTANT CONTEXT: This is fix attempt #{iteration}. Previous fix attempts have not fully resolved all issues.

Previously fixed issues:
{fixed_list}

The reviewer keeps finding problems because fixes are addressing symptoms, not root causes.
This time, think more carefully:
1. Look at the ORIGINAL code to understand what the change was trying to do
2. Consider the ENTIRE flow, not just the specific line mentioned
3. Fix the UNDERLYING DESIGN ISSUE if the same area keeps getting flagged
4. Think about edge cases: initialization order, race conditions, error states

For each finding, implement a COMPLETE fix using search/replace edits.

OUTPUT FORMAT (JSON):
{{
  "description": "Brief summary of all fixes applied",
  "modified_areas": ["function_name", "another_function"],
  "edits": [
    {{
      "old_string": "exact text to find and replace",
      "new_string": "replacement text"
    }}
  ]
}}

CRITICAL RULES FOR EDITS:
- old_string must be EXACT text from the file (copy-paste precision)
- old_string must be UNIQUE in the file - include enough context
- Preserve indentation exactly  
- Fix the ROOT CAUSE this time, not just the symptom
- Consider all edge cases the reviewer might check"#,
            fixed_list = if fixed_titles.is_empty() {
                "(none recorded)".to_string()
            } else {
                fixed_titles.iter().map(|t| format!("- {}", t)).collect::<Vec<_>>().join("\n")
            }
        )
    };

    // Format findings for the prompt
    let findings_text: Vec<String> = findings.iter().enumerate().map(|(i, f)| {
        let line_info = f.line.map(|l| format!(" (line {})", l)).unwrap_or_default();
        format!(
            "{}. [{}] {}{}\n   {}\n   Category: {}",
            i + 1, f.severity.to_uppercase(), f.title, line_info,
            f.description, f.category
        )
    }).collect();

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
    
    // Parse the JSON response
    let json_str = extract_json_object(&response.content)
        .ok_or_else(|| anyhow::anyhow!("No JSON found in fix response"))?;
    
    let parsed: serde_json::Value = serde_json::from_str(json_str)
        .map_err(|e| anyhow::anyhow!("Failed to parse fix JSON: {}", e))?;
    
    let description = parsed.get("description")
        .and_then(|v| v.as_str())
        .unwrap_or("Fixed review findings")
        .to_string();
    
    let modified_areas = parsed.get("modified_areas")
        .and_then(|v| v.as_array())
        .map(|arr| arr.iter().filter_map(|v| v.as_str().map(String::from)).collect())
        .unwrap_or_default();
    
    // Parse and apply edits
    let edits: Vec<EditOp> = parsed.get("edits")
        .and_then(|v| serde_json::from_value(v.clone()).ok())
        .ok_or_else(|| anyhow::anyhow!("Missing or invalid 'edits' array in response"))?;
    
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
                i + 1, truncate_for_error(&edit.old_string)
            ));
        }
        
        if matches.len() > 1 {
            return Err(anyhow::anyhow!(
                "Edit {}: old_string matches {} times (must be unique).\nSearched for: {:?}",
                i + 1, matches.len(), truncate_for_error(&edit.old_string)
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
        usage: response.usage,
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

    #[test]
    fn test_parse_suggestions() {
        let json = r#"[
            {
                "kind": "improvement",
                "priority": "high",
                "summary": "Test suggestion",
                "detail": "Test detail",
                "line": 10
            }
        ]"#;

        let path = PathBuf::from("test.rs");
        let suggestions = parse_suggestions(json, &path).unwrap();

        assert_eq!(suggestions.len(), 1);
        assert_eq!(suggestions[0].priority, Priority::High);
    }
}
