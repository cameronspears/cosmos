//! LLM-powered suggestions via OpenRouter
//!
//! Supports two modes:
//! - BYOK (Bring Your Own Key): User provides OpenRouter API key, billed directly
//! - Managed (Pro): Cosmos proxy handles API calls, billed through license
//!
//! Uses @preset/speed for ultra-fast analysis/summaries, @preset/smart for quality code generation.
//! Uses smart context building to maximize insight per token.

#![allow(dead_code)]

use super::{Priority, Suggestion, SuggestionKind, SuggestionSource};
use crate::config::Config;
use crate::context::WorkContext;
use crate::index::{CodebaseIndex, PatternKind, SymbolKind};
use crate::license::LicenseManager;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

/// OpenRouter direct API URL (BYOK mode)
const OPENROUTER_URL: &str = "https://openrouter.ai/api/v1/chat/completions";

/// Cosmos managed API URL (Pro mode) - proxies to OpenRouter with usage tracking
/// TODO: Set this to your hosted endpoint when ready
/// For now, Pro users still use OpenRouter directly (with usage tracking locally)
const COSMOS_API_URL: &str = "https://openrouter.ai/api/v1/chat/completions";

/// API mode for LLM calls
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApiMode {
    /// User's own OpenRouter API key (free tier)
    Byok,
    /// Cosmos managed API (Pro/Team tier)
    Managed,
}

impl ApiMode {
    /// Determine the API mode based on license and config
    pub fn detect() -> (Self, Option<String>) {
        let license_manager = LicenseManager::load();
        let config = Config::load();
        
        // Check if user has an OpenRouter API key configured (BYOK)
        let byok_key = config.get_api_key();
        
        // Pro/Team users: 
        // - If managed API is ready (COSMOS_API_URL != OpenRouter), use managed mode
        // - Otherwise, fall back to BYOK if they have a key configured
        // - Track usage locally either way
        if license_manager.tier().has_managed_ai() {
            // For now, managed mode uses OpenRouter directly (until we have a proxy)
            // Pro users must also have BYOK configured until managed API is live
            if let Some(key) = byok_key {
                return (ApiMode::Managed, Some(key));
            }
            // Pro license but no API key - can't make calls yet
            // Return None so we show a helpful error
            return (ApiMode::Managed, None);
        }
        
        // Free tier: use BYOK if configured
        if let Some(key) = byok_key {
            return (ApiMode::Byok, Some(key));
        }
        
        // No API access available
        (ApiMode::Byok, None)
    }
    
    /// Get the API URL for this mode
    pub fn url(&self) -> &'static str {
        // Both modes use OpenRouter for now until managed proxy is live
        OPENROUTER_URL
    }
    
    /// Get the appropriate header name for the API key
    pub fn auth_header(&self) -> &'static str {
        match self {
            ApiMode::Byok => "Authorization",
            ApiMode::Managed => "Authorization", // Same for now, will change when proxy is live
        }
    }
}

// Model pricing per million tokens (as of 2024)
// Speed preset: ultra-fast routing for analysis
const SPEED_INPUT_COST: f64 = 0.25;   // $0.25 per 1M input tokens  
const SPEED_OUTPUT_COST: f64 = 0.69;  // $0.69 per 1M output tokens
// Smart preset: quality routing for code generation (cheaper than raw Opus)
const SMART_INPUT_COST: f64 = 3.0;    // ~$3 per 1M input tokens (estimated)
const SMART_OUTPUT_COST: f64 = 15.0;  // ~$15 per 1M output tokens (estimated)

/// Models available for suggestions
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Model {
    /// Speed preset - ultra-fast routing for analysis and summaries
    Speed,
    /// Smart preset - quality routing for code generation (replaces Opus)
    Smart,
}

impl Model {
    pub fn id(&self) -> &'static str {
        match self {
            Model::Speed => "@preset/speed",
            Model::Smart => "@preset/smart",
        }
    }

    pub fn max_tokens(&self) -> u32 {
        match self {
            Model::Speed => 8192,
            Model::Smart => 8192,
        }
    }
    
    pub fn display_name(&self) -> &'static str {
        match self {
            Model::Speed => "speed",
            Model::Smart => "smart",
        }
    }
    
    /// Calculate cost in USD based on token usage
    pub fn calculate_cost(&self, prompt_tokens: u32, completion_tokens: u32) -> f64 {
        let (input_rate, output_rate) = match self {
            Model::Speed => (SPEED_INPUT_COST, SPEED_OUTPUT_COST),
            Model::Smart => (SMART_INPUT_COST, SMART_OUTPUT_COST),
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
    let (_, api_key) = ApiMode::detect();
    api_key.is_some()
}

/// Get current API mode (for display/debugging)
pub fn current_mode() -> ApiMode {
    let (mode, _) = ApiMode::detect();
    mode
}

/// Call LLM API (returns content only, for backwards compatibility)
async fn call_llm(system: &str, user: &str, model: Model) -> anyhow::Result<String> {
    let response = call_llm_with_usage(system, user, model, false).await?;
    Ok(response.content)
}

/// Call LLM API with full response including usage stats
/// Automatically routes to BYOK (OpenRouter) or Managed (Cosmos API) based on license
async fn call_llm_with_usage(
    system: &str, 
    user: &str, 
    model: Model,
    json_mode: bool,
) -> anyhow::Result<LlmResponse> {
    let (mode, api_key) = ApiMode::detect();
    let api_key = api_key.ok_or_else(|| {
        match mode {
            ApiMode::Managed => {
                // Pro user but no API key - managed proxy not live yet
                anyhow::anyhow!(
                    "Cosmos Pro managed API coming soon! For now, please also run 'cosmos --setup' to configure your OpenRouter key."
                )
            }
            ApiMode::Byok => {
                anyhow::anyhow!("No API key configured. Run 'cosmos --setup' to get started.")
            }
        }
    })?;

    let client = reqwest::Client::new();
    let url = mode.url();

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

    // Build request with appropriate headers for the API mode
    let mut request_builder = client
        .post(url)
        .header("Content-Type", "application/json")
        .header("HTTP-Referer", "https://cosmos.dev")
        .header("X-Title", "Cosmos");

    // Add auth header (both modes use OpenRouter directly for now)
    request_builder = request_builder.header("Authorization", format!("Bearer {}", api_key));

    let response = request_builder
        .json(&request)
        .send()
        .await?;

    if !response.status().is_success() {
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        
        // Provide helpful error messages based on status
        let error_msg = match status.as_u16() {
            401 => {
                match mode {
                    ApiMode::Byok => "Invalid API key. Run 'cosmos --setup' to update it.".to_string(),
                    ApiMode::Managed => "License invalid or expired. Check 'cosmos --status'.".to_string(),
                }
            }
            429 => {
                match mode {
                    ApiMode::Byok => "Rate limited by OpenRouter. Try again in a moment.".to_string(),
                    ApiMode::Managed => "Token quota exceeded. Upgrade at cosmos.dev/pro".to_string(),
                }
            }
            _ => format!("API error {}: {}", status, text),
        };
        return Err(anyhow::anyhow!("{}", error_msg));
    }

    let chat_response: ChatResponse = response.json().await?;

    let content = chat_response
        .choices
        .first()
        .map(|c| c.message.content.clone())
        .ok_or_else(|| anyhow::anyhow!("No response from AI"))?;
    
    // Record usage for Pro users (managed mode)
    if mode == ApiMode::Managed {
        if let Some(ref usage) = chat_response.usage {
            let mut license_manager = LicenseManager::load();
            license_manager.record_usage(usage.total_tokens as u64);
        }
    }
    
    Ok(LlmResponse {
        content,
        usage: chat_response.usage,
        model: chat_response.model.unwrap_or_else(|| model.id().to_string()),
    })
}

/// Ask cosmos a general question about the codebase
pub async fn ask_question(
    index: &CodebaseIndex,
    context: &WorkContext,
    question: &str,
    repo_memory: Option<String>,
) -> anyhow::Result<(String, Option<Usage>)> {
    let system = r#"You are Cosmos, a contemplative companion for codebases. The developer is asking you a question about their code.

Respond thoughtfully and concisely. Be specific to their codebase when you can.
Use the project context provided to give relevant answers.
If the question is about specific files or code, reference them by path.
Keep responses focused and actionable - developers appreciate brevity.

Format your response with markdown for readability:
- Use **bold** for emphasis
- Use `code` for file names, functions, and code snippets
- Use bullet points for lists
- Use ### for section headers if needed
- Keep it clean and scannable"#;

    // Build context about the codebase
    let stats = index.stats();
    let file_list: Vec<_> = index.files.keys()
        .take(50)  // Limit to avoid huge prompts
        .map(|p| p.display().to_string())
        .collect();
    
    // Get symbols for context
    let symbols: Vec<_> = index.files.values()
        .flat_map(|f| f.symbols.iter())
        .filter(|s| matches!(s.kind, SymbolKind::Function | SymbolKind::Struct | SymbolKind::Enum))
        .take(100)
        .map(|s| format!("{:?}: {}", s.kind, s.name))
        .collect();

    let memory_section = repo_memory
        .as_deref()
        .filter(|m| !m.trim().is_empty())
        .map(|m| format!("\n\nREPO MEMORY (follow these conventions):\n{}", m))
        .unwrap_or_default();

    let user = format!(
        r#"PROJECT CONTEXT:
- {} files, {} lines of code
- {} symbols total
- Branch: {}, {} uncommitted changes
- Key files: {}

KEY SYMBOLS:
{}

{}

QUESTION:
{}"#,
        stats.file_count,
        stats.total_loc,
        stats.symbol_count,
        context.branch,
        context.modified_count,
        file_list.join(", "),
        symbols.join("\n"),
        memory_section,
        question
    );

    let response = call_llm_with_usage(system, &user, Model::Speed, false).await?;
    Ok((response.content, response.usage))
}

// ═══════════════════════════════════════════════════════════════════════════
//  DIRECT CODE GENERATION (Human plan → Smart preset applies changes)
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

/// Generate the actual fixed code content based on a human-language plan
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
  "new_content": "THE COMPLETE UPDATED FILE CONTENT"
}

CRITICAL RULES:
- new_content must be the COMPLETE file, not a snippet
- Preserve all existing functionality that isn't being changed
- Maintain the exact same coding style and conventions
- Only change what the plan describes
- Keep imports, comments, and structure intact"#;

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
        "File: {}\n\nOriginal Issue: {}\n{}\n{}\n\n{}\n\nCurrent Code:\n```\n{}\n```\n\nImplement the fix according to the plan. Output the complete updated file.",
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
    
    let new_content = parsed.get("new_content")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("Missing new_content in response"))?
        .to_string();
    
    // Validate the new content isn't empty or too short
    if new_content.trim().is_empty() {
        return Err(anyhow::anyhow!("Generated content is empty"));
    }
    
    // Basic sanity check - new content should be similar length to original
    let length_ratio = new_content.len() as f64 / content.len() as f64;
    if length_ratio < 0.3 || length_ratio > 3.0 {
        // Allow but warn - the change might be legitimate
        eprintln!("Warning: Generated content length differs significantly (ratio: {:.2})", length_ratio);
    }
    
    Ok(AppliedFix {
        description,
        new_content,
        modified_areas,
        usage: response.usage,
    })
}

// ═══════════════════════════════════════════════════════════════════════════
//  FAST FIX PREVIEW (Phase 1 of two-phase fix)
// ═══════════════════════════════════════════════════════════════════════════

/// Quick preview of what a fix will do - generated in <1 second
#[derive(Debug, Clone, PartialEq)]
pub struct FixPreview {
    /// Whether the issue was verified to exist in the code
    pub verified: bool,
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

/// Generate a quick preview of what the fix will do (uses Grok Fast for speed)
/// This is Phase 1 of the two-phase fix flow - verifies the issue and lets users approve before waiting for full diff
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
  "verification_note": "Brief explanation of whether the issue was found and where",
  "evidence_snippet": "const BATCH_SIZE = 1000;",
  "evidence_line": 42,
  "description": "1-2 sentence description of what will change (if verified)",
  "affected_areas": ["function_name", "another_function"],
  "scope": "small"
}

RULES:
- verified: boolean true if issue exists, false if it doesn't exist or was already fixed
- verification_note: explain what you found
- evidence_snippet: 1-3 lines of the ACTUAL code from the file that proves your claim. Only include the relevant code, not surrounding context. Omit if no specific code evidence is needed.
- evidence_line: the line number where the evidence snippet starts
- scope: one of "small", "medium", or "large"

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

    let response = call_llm(system, &user, Model::Speed).await?;
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
pub async fn analyze_codebase(
    index: &CodebaseIndex,
    context: &WorkContext,
    repo_memory: Option<String>,
) -> anyhow::Result<(Vec<Suggestion>, Option<Usage>)> {
    let system = r#"You are a senior developer reviewing a codebase. Your job is to find genuinely useful improvements - things that will make the app better, not just cleaner.

OUTPUT FORMAT (JSON array, 5-10 suggestions):
[
  {
    "file": "relative/path/to/file.rs",
    "kind": "improvement|bugfix|feature|optimization|quality|documentation|testing",
    "priority": "high|medium|low",
    "summary": "One-line description of what to do and why it matters",
    "detail": "Brief explanation with specific guidance",
    "line": null or specific line number if applicable
  }
]

WHAT TO LOOK FOR (aim for variety across these categories):
- **Bugs & Edge Cases**: Race conditions, off-by-one errors, null/None handling, error swallowing
- **Security**: Hardcoded secrets, SQL injection, XSS, path traversal, insecure defaults
- **Performance**: N+1 queries, unnecessary allocations, blocking in async, missing caching opportunities
- **API Design**: Confusing function signatures, missing validation, inconsistent return types
- **User Experience**: Error messages that don't help, missing loading states, accessibility gaps
- **Reliability**: Missing retries for network calls, no timeouts, silent failures
- **Feature Gaps**: Obvious missing functionality, half-implemented features, TODO items worth addressing
- **Testing Blind Spots**: Critical paths without tests, brittle test setups

AVOID:
- "Split this file" or "break this function up" unless it's genuinely causing problems
- Generic advice like "add more comments" or "improve naming"
- Suggestions that would just make the code "cleaner" without real benefit
- Anything a linter would catch

PRIORITIZE:
- Files marked [CHANGED] - the developer is actively working there
- Things that could cause bugs or outages
- Quick wins that provide immediate value
- Suggestions specific to THIS codebase, not generic best practices"#;

    let user_prompt = build_codebase_context(index, context, repo_memory.as_deref());
    
    // Use Smart preset for quality reasoning on suggestions
    let response = call_llm_with_usage(system, &user_prompt, Model::Smart, true).await?;
    
    let suggestions = parse_codebase_suggestions(&response.content)?;
    Ok((suggestions, response.usage))
}

/// Build rich context from codebase index for the LLM prompt
fn build_codebase_context(index: &CodebaseIndex, context: &WorkContext, repo_memory: Option<&str>) -> String {
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
            
            let mut suggestion = Suggestion::new(
                kind,
                priority,
                file_path,
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
struct CodebaseSuggestionJson {
    file: String,
    kind: String,
    priority: String,
    summary: String,
    detail: String,
    line: Option<usize>,
}

/// Truncate a string for display
fn truncate_str(s: &str, max: usize) -> &str {
    if s.len() <= max {
        s
    } else {
        &s[..max]
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
    pub usage: Option<Usage>,
}

/// Generate rich, context-aware summaries for all files in the codebase
/// 
/// Uses batched approach (4 files per call) for reliability.
/// Returns all summaries and total usage stats.
/// 
/// DEPRECATED: Use generate_file_summaries_incremental instead for caching support.
pub async fn generate_file_summaries(
    index: &CodebaseIndex,
) -> anyhow::Result<(HashMap<PathBuf, String>, Option<Usage>)> {
    let project_context = discover_project_context(index);
    let files: Vec<_> = index.files.keys().cloned().collect();
    generate_summaries_for_files(index, &files, &project_context).await
}

/// Generate summaries for a specific list of files with project context
/// Uses aggressive parallel batch processing for speed
pub async fn generate_summaries_for_files(
    index: &CodebaseIndex,
    files: &[PathBuf],
    project_context: &str,
) -> anyhow::Result<(HashMap<PathBuf, String>, Option<Usage>)> {
    // Large batch size for fewer API calls
    let batch_size = 16;
    // Higher concurrency for faster processing (Speed preset handles this well)
    let concurrency = 4;
    
    let batches: Vec<_> = files.chunks(batch_size).collect();
    
    let mut all_summaries = HashMap::new();
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
                    all_summaries.extend(batch_result.summaries);
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
    
    Ok((all_summaries, final_usage))
}

/// Priority tier for file summarization
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
async fn generate_summary_batch(
    index: &CodebaseIndex,
    files: &[PathBuf],
    project_context: &str,
) -> anyhow::Result<SummaryBatchResult> {
    let system = r#"You are a senior developer writing documentation. For each file, write a 2-6 sentence summary explaining:
- What this file IS (its purpose/role)
- What it DOES (key functionality, main exports)
- How it FITS (relationships to other parts)

IMPORTANT: Use the PROJECT CONTEXT provided to understand what this codebase is for. 
Write definitive statements like "This file handles X" not vague guesses like "This seems to be related to Y".
Be specific and technical. Reference actual function/struct names.

OUTPUT: A JSON object mapping file paths to summary strings. Example:
{"src/main.rs": "This is the application entry point..."}"#;

    let user_prompt = build_batch_context(index, files, project_context);
    
    let response = call_llm_with_usage(system, &user_prompt, Model::Speed, true).await?;
    
    let summaries = parse_summaries_response(&response.content)?;
    
    Ok(SummaryBatchResult {
        summaries,
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

/// Parse the summaries JSON response with robust error handling
fn parse_summaries_response(response: &str) -> anyhow::Result<HashMap<PathBuf, String>> {
    let json_str = extract_json_object(response)
        .ok_or_else(|| anyhow::anyhow!("No JSON object found in response"))?;

    // First try to parse as a simple {path: summary} object
    if let Ok(parsed) = serde_json::from_str::<HashMap<String, String>>(json_str) {
        let summaries = parsed
            .into_iter()
            .map(|(path, summary)| (PathBuf::from(path), summary))
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
                        .map(|(path, summary)| (PathBuf::from(path), summary))
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
                    summaries.insert(PathBuf::from(key), summary.to_string());
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

// ============================================================================
// PR Review with AI
// ============================================================================

/// A single file review comment from the AI
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PRFileReview {
    pub file: String,
    pub severity: String, // "praise", "info", "suggest", "warning"
    pub comment: String,
}

/// Review changes using LLM for thorough code review
pub async fn review_changes(
    files_changed: &[(PathBuf, String)], // (file_path, diff)
) -> anyhow::Result<(Vec<crate::ui::PRReviewComment>, Usage)> {
    let config = crate::config::Config::load();
    let _api_key = config.get_api_key()
        .ok_or_else(|| anyhow::anyhow!("API key not configured"))?;
    
    // Build the review prompt
    let mut changes_text = String::new();
    for (path, diff) in files_changed {
        let file_name = path.file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("unknown");
        changes_text.push_str(&format!("\n--- {} ---\n{}\n", file_name, diff));
    }
    
    let system_prompt = r#"You are a senior code reviewer. Review the following changes and provide concise, actionable feedback.

For each file, provide ONE comment with:
- severity: "praise" (good code), "info" (FYI), "suggest" (could improve), or "warning" (should fix)
- A brief comment (1-2 sentences max)

Respond with a JSON array of objects:
[
  {"file": "filename.ts", "severity": "suggest", "comment": "Consider adding error handling for the async call."},
  {"file": "another.ts", "severity": "praise", "comment": "Clean refactor, good separation of concerns."}
]

Be constructive and focused. Skip trivial issues. Highlight the most important points."#;

    let user_prompt = format!("Review these changes:\n{}", changes_text);
    
    // Use Grok Fast for thorough review
    let response = call_llm_with_usage(system_prompt, &user_prompt, Model::Speed, true).await?;
    
    let usage = response.usage.unwrap_or_default();
    
    // Parse the response
    let reviews = parse_review_response(&response.content, files_changed)?;
    
    Ok((reviews, usage))
}

/// Parse the JSON review response into PRReviewComments
fn parse_review_response(
    content: &str, 
    files_changed: &[(PathBuf, String)]
) -> anyhow::Result<Vec<crate::ui::PRReviewComment>> {
    use crate::ui::{PRReviewComment, ReviewSeverity};
    
    // Strip markdown code fences if present
    let trimmed = content.trim();
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

    // Extract JSON from response
    let json_start = clean.find('[').unwrap_or(0);
    let json_end = clean.rfind(']').map(|i| i + 1).unwrap_or(clean.len());
    let json_str = &clean[json_start..json_end];
    
    let reviews: Vec<PRFileReview> = serde_json::from_str(json_str)
        .map_err(|e| anyhow::anyhow!("Failed to parse review JSON: {} | Response preview: {}", e, truncate_str(json_str, 200)))?;
    
    // Convert to PRReviewComment
    let mut comments = Vec::new();
    for review in reviews {
        // Find the matching file path
        let file_path = files_changed.iter()
            .find(|(p, _)| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .map(|n| n == review.file || review.file.ends_with(n))
                    .unwrap_or(false)
            })
            .map(|(p, _)| p.clone())
            .unwrap_or_else(|| PathBuf::from(&review.file));
        
        let severity = match review.severity.to_lowercase().as_str() {
            "praise" => ReviewSeverity::Praise,
            "info" => ReviewSeverity::Info,
            "suggest" => ReviewSeverity::Suggest,
            "warning" => ReviewSeverity::Warning,
            _ => ReviewSeverity::Info,
        };
        
        comments.push(PRReviewComment {
            file: file_path,
            comment: review.comment,
            severity,
        });
    }
    
    Ok(comments)
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
