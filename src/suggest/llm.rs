//! LLM-powered suggestions via OpenRouter
//!
//! Single Opus 4.5 call for high-quality codebase-wide analysis.
//! Uses smart context building to maximize insight per token.

use super::{Priority, Suggestion, SuggestionKind, SuggestionSource};
use crate::config::Config;
use crate::context::WorkContext;
use crate::index::{CodebaseIndex, FileIndex, PatternKind, SymbolKind};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

const OPENROUTER_URL: &str = "https://openrouter.ai/api/v1/chat/completions";

// Model pricing per million tokens (as of 2024)
const OPUS_INPUT_COST: f64 = 15.0;   // $15 per 1M input tokens
const OPUS_OUTPUT_COST: f64 = 75.0;  // $75 per 1M output tokens
const GROK_INPUT_COST: f64 = 5.0;    // $5 per 1M input tokens  
const GROK_OUTPUT_COST: f64 = 15.0;  // $15 per 1M output tokens

/// Models available for suggestions
#[derive(Debug, Clone, Copy)]
pub enum Model {
    /// Grok Fast - for quick categorization
    GrokFast,
    /// Opus 4.5 - for deep analysis
    Opus,
}

impl Model {
    pub fn id(&self) -> &'static str {
        match self {
            Model::GrokFast => "x-ai/grok-4.1-fast",
            Model::Opus => "anthropic/claude-opus-4.5",
        }
    }

    pub fn max_tokens(&self) -> u32 {
        match self {
            Model::GrokFast => 1024,
            Model::Opus => 8192,
        }
    }
    
    pub fn display_name(&self) -> &'static str {
        match self {
            Model::GrokFast => "grok-4.1-fast",
            Model::Opus => "opus-4.5",
        }
    }
    
    /// Calculate cost in USD based on token usage
    pub fn calculate_cost(&self, prompt_tokens: u32, completion_tokens: u32) -> f64 {
        let (input_rate, output_rate) = match self {
            Model::GrokFast => (GROK_INPUT_COST, GROK_OUTPUT_COST),
            Model::Opus => (OPUS_INPUT_COST, OPUS_OUTPUT_COST),
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

/// Get API key from config
fn get_api_key() -> Option<String> {
    Config::load().get_api_key()
}

/// Check if LLM is available
pub fn is_available() -> bool {
    get_api_key().is_some()
}

/// Call OpenRouter API (returns content only, for backwards compatibility)
async fn call_llm(system: &str, user: &str, model: Model) -> anyhow::Result<String> {
    let response = call_llm_with_usage(system, user, model, false).await?;
    Ok(response.content)
}

/// Call OpenRouter API with full response including usage stats
async fn call_llm_with_usage(
    system: &str, 
    user: &str, 
    model: Model,
    json_mode: bool,
) -> anyhow::Result<LlmResponse> {
    let api_key = get_api_key().ok_or_else(|| anyhow::anyhow!("No API key configured"))?;

    let client = reqwest::Client::new();

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

    let response = client
        .post(OPENROUTER_URL)
        .header("Authorization", format!("Bearer {}", api_key))
        .header("Content-Type", "application/json")
        .header("HTTP-Referer", "https://github.com/cosmos")
        .header("X-Title", "Cosmos")
        .json(&request)
        .send()
        .await?;

    if !response.status().is_success() {
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        return Err(anyhow::anyhow!("API error {}: {}", status, text));
    }

    let chat_response: ChatResponse = response.json().await?;

    let content = chat_response
        .choices
        .first()
        .map(|c| c.message.content.clone())
        .ok_or_else(|| anyhow::anyhow!("No response from AI"))?;
    
    Ok(LlmResponse {
        content,
        usage: chat_response.usage,
        model: chat_response.model.unwrap_or_else(|| model.id().to_string()),
    })
}

/// Quick file summary using Grok Fast
pub async fn quick_summary(path: &PathBuf, content: &str, file_index: &FileIndex) -> anyhow::Result<String> {
    let system = r#"You are a code analyst. Provide a brief summary of what this file does.
Output exactly 1-2 sentences. Be specific and technical."#;

    let user = format!(
        "File: {} ({} lines, {} functions)\n\n{}",
        path.display(),
        file_index.loc,
        file_index.symbols.len(),
        truncate_content(content, 2000)
    );

    call_llm(system, &user, Model::GrokFast).await
}

/// Deep analysis using Opus 4.5 (on-demand only)
pub async fn analyze_file_deep(
    path: &PathBuf,
    content: &str,
    file_index: &FileIndex,
) -> anyhow::Result<Vec<Suggestion>> {
    let system = r#"You are a senior code reviewer. Analyze this file and suggest improvements.

OUTPUT FORMAT (JSON array):
[
  {
    "kind": "improvement|bugfix|feature|optimization|quality|documentation|testing",
    "priority": "high|medium|low",
    "summary": "One-line description",
    "detail": "Explanation with specific recommendations",
    "line": null or line number
  }
]

GUIDELINES:
- Be specific and actionable
- Focus on the most impactful improvements
- Limit to 3-5 suggestions
- Consider: correctness, performance, maintainability, readability
- Only suggest changes that provide real value"#;

    let metrics = format!(
        "Metrics:\n- Lines: {}\n- Functions: {}\n- Complexity: {:.1}\n- Patterns detected: {}",
        file_index.loc,
        file_index.symbols.len(),
        file_index.complexity,
        file_index.patterns.len()
    );

    let user = format!(
        "File: {}\n\n{}\n\nCode:\n```\n{}\n```",
        path.display(),
        metrics,
        truncate_content(content, 8000)
    );

    let response = call_llm(system, &user, Model::Opus).await?;

    parse_suggestions(&response, path)
}

/// Inquiry-based suggestion - user asks "what should I improve?"
pub async fn inquiry(
    path: &PathBuf,
    content: &str,
    file_index: &FileIndex,
    context: Option<&str>,
) -> anyhow::Result<String> {
    let system = r#"You are a thoughtful code companion. The developer is asking for suggestions on what to improve.

Respond conversationally but concisely. Structure your response:

1. **Quick Assessment** (1 sentence)
2. **Top Recommendation** (2-3 sentences)
3. **Why it matters** (1 sentence)

Be specific to this code. Don't be generic."#;

    let context_text = context.map(|c| format!("\nContext: {}", c)).unwrap_or_default();

    let user = format!(
        "File: {} ({} lines)\n\nSymbols: {}\nPatterns found: {}{}\n\nCode:\n```\n{}\n```\n\nWhat should I improve?",
        path.display(),
        file_index.loc,
        file_index.symbols.iter().map(|s| s.name.as_str()).collect::<Vec<_>>().join(", "),
        file_index.patterns.len(),
        context_text,
        truncate_content(content, 4000)
    );

    call_llm(system, &user, Model::GrokFast).await
}

/// Generate a fix/change for a specific suggestion
pub async fn generate_fix(
    path: &PathBuf,
    content: &str,
    suggestion: &Suggestion,
) -> anyhow::Result<String> {
    let system = r#"You are a code improvement assistant. Generate a fix for the described issue.

OUTPUT FORMAT:
1. Brief explanation (2-3 sentences)
2. Code changes in unified diff format:
   --- a/filepath
   +++ b/filepath
   @@ context @@
    unchanged
   -removed
   +added

Be precise. Only change what's necessary."#;

    let user = format!(
        "File: {}\n\nIssue: {}\n{}\n\nCode:\n```\n{}\n```",
        path.display(),
        suggestion.summary,
        suggestion.detail.as_deref().unwrap_or(""),
        truncate_content(content, 6000)
    );

    call_llm(system, &user, Model::Opus).await
}

// ═══════════════════════════════════════════════════════════════════════════
//  UNIFIED CODEBASE ANALYSIS
// ═══════════════════════════════════════════════════════════════════════════

/// Analyze entire codebase with a single Opus 4.5 call
/// 
/// This is the main entry point for generating high-quality suggestions.
/// Uses smart context building to pack maximum insight into the prompt.
/// Returns suggestions and usage stats for cost tracking.
pub async fn analyze_codebase(
    index: &CodebaseIndex,
    context: &WorkContext,
) -> anyhow::Result<(Vec<Suggestion>, Option<Usage>)> {
    let system = r#"You are an expert code reviewer analyzing a codebase. Your goal is to find the most impactful improvements.

OUTPUT FORMAT (JSON array, 5-10 suggestions):
[
  {
    "file": "relative/path/to/file.rs",
    "kind": "improvement|bugfix|feature|optimization|quality|documentation|testing",
    "priority": "high|medium|low",
    "summary": "Concise one-line description of the issue/improvement",
    "detail": "Specific, actionable explanation with code context",
    "line": null or specific line number if applicable
  }
]

GUIDELINES:
- Focus on HIGH-VALUE changes: bugs, security issues, major refactors, performance wins
- Be specific: mention exact function names, patterns, and line numbers
- Prioritize files the developer is actively working on (marked CHANGED)
- Consider the codebase holistically - suggest architectural improvements
- Don't suggest trivial style changes or obvious fixes
- Each suggestion should be actionable and provide clear value
- Consider: correctness, security, performance, maintainability, testability"#;

    let user_prompt = build_codebase_context(index, context);
    
    let response = call_llm_with_usage(system, &user_prompt, Model::Opus, true).await?;
    
    let suggestions = parse_codebase_suggestions(&response.content)?;
    Ok((suggestions, response.usage))
}

/// Build rich context from codebase index for the LLM prompt
fn build_codebase_context(index: &CodebaseIndex, context: &WorkContext) -> String {
    let stats = index.stats();
    let project_name = index.root.file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("project");
    
    let mut sections = Vec::new();
    
    // Header with overview
    sections.push(format!(
        "CODEBASE: {} ({} files, {} LOC, {} symbols)\n\
         BRANCH: {} | FOCUS: {} | CHANGED FILES: {}",
        project_name,
        stats.file_count,
        stats.total_loc,
        stats.symbol_count,
        context.branch,
        context.inferred_focus.as_deref().unwrap_or("general"),
        context.modified_count
    ));
    
    // Files by priority (most important first)
    let mut files_section = String::from("\n\nFILES (by priority):");
    let files_by_priority = index.files_by_priority();
    
    for (path, file_index) in files_by_priority.iter().take(30) {
        let is_changed = context.all_changed_files().iter().any(|f| f == path);
        let changed_marker = if is_changed { " [CHANGED]" } else { "" };
        
        let func_count = file_index.symbols.iter()
            .filter(|s| matches!(s.kind, SymbolKind::Function | SymbolKind::Method))
            .count();
        
        let top_symbols: Vec<_> = file_index.symbols.iter()
            .filter(|s| matches!(s.kind, SymbolKind::Function | SymbolKind::Method))
            .take(5)
            .map(|s| s.name.as_str())
            .collect();
        
        files_section.push_str(&format!(
            "\n- {}: {} LOC, {} funcs, complexity {:.0}{}",
            path.display(),
            file_index.loc,
            func_count,
            file_index.complexity,
            changed_marker
        ));
        
        if !top_symbols.is_empty() {
            files_section.push_str(&format!("\n  symbols: {}", top_symbols.join(", ")));
        }
    }
    sections.push(files_section);
    
    // Detected patterns (code smells, issues)
    if !index.patterns.is_empty() {
        let mut patterns_section = String::from("\n\nDETECTED PATTERNS:");
        
        // Group patterns by severity
        let high_patterns: Vec<_> = index.patterns.iter()
            .filter(|p| matches!(p.kind, PatternKind::GodModule | PatternKind::DeepNesting | PatternKind::MissingErrorHandling))
            .take(10)
            .collect();
        
        for pattern in &high_patterns {
            patterns_section.push_str(&format!(
                "\n- {:?} at {}:{} - {}",
                pattern.kind,
                pattern.file.display(),
                pattern.line,
                truncate_str(&pattern.description, 60)
            ));
        }
        
        // Add other patterns summary
        let other_count = index.patterns.len() - high_patterns.len();
        if other_count > 0 {
            patterns_section.push_str(&format!("\n- ... and {} more patterns", other_count));
        }
        
        sections.push(patterns_section);
    }
    
    // High complexity functions (potential refactor targets)
    let mut complex_funcs: Vec<_> = index.symbols.iter()
        .filter(|s| matches!(s.kind, SymbolKind::Function | SymbolKind::Method))
        .filter(|s| s.complexity > 15.0 || s.line_count() > 50)
        .collect();
    complex_funcs.sort_by(|a, b| b.complexity.partial_cmp(&a.complexity).unwrap_or(std::cmp::Ordering::Equal));
    
    if !complex_funcs.is_empty() {
        let mut funcs_section = String::from("\n\nHIGH COMPLEXITY FUNCTIONS:");
        for func in complex_funcs.iter().take(15) {
            funcs_section.push_str(&format!(
                "\n- {}::{} ({}:{}) - {} lines, complexity {:.0}",
                func.file.file_name().and_then(|n| n.to_str()).unwrap_or("?"),
                func.name,
                func.file.display(),
                func.line,
                func.line_count(),
                func.complexity
            ));
        }
        sections.push(funcs_section);
    }
    
    // Recent changes context (what's being worked on)
    if !context.recent_commits.is_empty() {
        let mut commits_section = String::from("\n\nRECENT ACTIVITY:");
        for commit in context.recent_commits.iter().take(5) {
            commits_section.push_str(&format!(
                "\n- {}: {} ({})",
                commit.short_sha,
                truncate_str(&commit.message, 50),
                commit.files_changed.len()
            ));
        }
        sections.push(commits_section);
    }
    
    // Uncommitted changes (highest priority for review)
    if !context.uncommitted_files.is_empty() || !context.staged_files.is_empty() {
        let mut changes_section = String::from("\n\nUNCOMMITTED CHANGES (prioritize these):");
        for file in context.uncommitted_files.iter().chain(context.staged_files.iter()).take(10) {
            changes_section.push_str(&format!("\n- {}", file.display()));
        }
        sections.push(changes_section);
    }
    
    // Final instruction
    sections.push(String::from(
        "\n\nAnalyze this codebase and provide 5-10 high-value suggestions. \
         Focus on bugs, security issues, performance problems, and major refactoring opportunities. \
         Prioritize changed files and high-complexity areas."
    ));
    
    sections.join("")
}

/// Parse suggestions from codebase-wide analysis
fn parse_codebase_suggestions(response: &str) -> anyhow::Result<Vec<Suggestion>> {
    // Try to extract JSON array from response
    let json_str = if let Some(start) = response.find('[') {
        if let Some(end) = response.rfind(']') {
            &response[start..=end]
        } else {
            response
        }
    } else {
        response
    };

    let parsed: Vec<CodebaseSuggestionJson> = serde_json::from_str(json_str)
        .map_err(|e| anyhow::anyhow!("Failed to parse suggestions: {}", e))?;

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

/// Parse JSON suggestions from LLM response
fn parse_suggestions(response: &str, path: &PathBuf) -> anyhow::Result<Vec<Suggestion>> {
    // Try to extract JSON array from response
    let json_str = if let Some(start) = response.find('[') {
        if let Some(end) = response.rfind(']') {
            &response[start..=end]
        } else {
            response
        }
    } else {
        response
    };

    let parsed: Vec<SuggestionJson> = serde_json::from_str(json_str)
        .map_err(|e| anyhow::anyhow!("Failed to parse suggestions: {}", e))?;

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
pub async fn generate_file_summaries(
    index: &CodebaseIndex,
) -> anyhow::Result<(HashMap<PathBuf, String>, Option<Usage>)> {
    let files: Vec<_> = index.files.keys().cloned().collect();
    let batch_size = 4;
    
    let mut all_summaries = HashMap::new();
    let mut total_usage = Usage::default();
    
    for batch in files.chunks(batch_size) {
        match generate_summary_batch(index, batch).await {
            Ok(result) => {
                all_summaries.extend(result.summaries);
                if let Some(usage) = result.usage {
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
    
    let final_usage = if total_usage.total_tokens > 0 {
        Some(total_usage)
    } else {
        None
    };
    
    Ok((all_summaries, final_usage))
}

/// Generate summaries for a single batch of files
async fn generate_summary_batch(
    index: &CodebaseIndex,
    files: &[PathBuf],
) -> anyhow::Result<SummaryBatchResult> {
    let system = r#"You are a senior developer writing documentation. For each file, write a 2-6 sentence summary explaining:
- What this file IS (its purpose/role)
- What it DOES (key functionality, main exports)
- How it FITS (relationships to other parts)

Be specific and technical. Reference actual function/struct names.

OUTPUT: A JSON object mapping file paths to summary strings. Example:
{"src/main.rs": "This is the application entry point..."}"#;

    let user_prompt = build_batch_context(index, files);
    
    let response = call_llm_with_usage(system, &user_prompt, Model::Opus, true).await?;
    
    let summaries = parse_summaries_response(&response.content)?;
    
    Ok(SummaryBatchResult {
        summaries,
        usage: response.usage,
    })
}

/// Build context for a batch of files
fn build_batch_context(index: &CodebaseIndex, files: &[PathBuf]) -> String {
    let project_name = index.root.file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("project");
    
    let mut sections = Vec::new();
    sections.push(format!("PROJECT: {}\n\nFILES TO SUMMARIZE:", project_name));
    
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

    let parsed: HashMap<String, String> = serde_json::from_str(json_str)
        .map_err(|e| {
            // Try to provide helpful error context
            let preview = if json_str.len() > 100 {
                format!("{}...", &json_str[..100])
            } else {
                json_str.to_string()
            };
            anyhow::anyhow!("JSON parse error: {} | Preview: {}", e, preview)
        })?;

    let summaries = parsed
        .into_iter()
        .map(|(path, summary)| (PathBuf::from(path), summary))
        .collect();

    Ok(summaries)
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
