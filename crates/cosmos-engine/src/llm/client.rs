use super::models::{Model, Usage};
use cosmos_adapters::config::Config;
use serde::{Deserialize, Serialize};
use std::time::{Duration, Instant};
use tokio::time::timeout;

/// Groq OpenAI-compatible API URL (direct mode).
pub(crate) const GROQ_URL: &str = "https://api.groq.com/openai/v1/chat/completions";

fn backend_label() -> &'static str {
    "Groq"
}

pub(crate) fn chat_completions_url() -> &'static str {
    GROQ_URL
}

fn model_id_for_backend_impl(model: Model) -> String {
    let id = model.id();
    if id.starts_with("openai/gpt-oss-120b") || model == Model::Smart {
        "openai/gpt-oss-120b".to_string()
    } else {
        id.to_string()
    }
}

pub(crate) fn model_id_for_backend(model: Model) -> String {
    model_id_for_backend_impl(model)
}

fn normalize_groq_service_tier(value: Option<&str>) -> String {
    value
        .map(|v| v.trim().to_ascii_lowercase())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "on_demand".to_string())
}

pub(crate) fn groq_service_tier() -> Option<String> {
    let raw = std::env::var("COSMOS_GROQ_SERVICE_TIER").ok()?;
    Some(normalize_groq_service_tier(Some(raw.as_str())))
}

pub(crate) fn apply_backend_headers(
    builder: reqwest::RequestBuilder,
    api_key: &str,
) -> reqwest::RequestBuilder {
    builder
        .header("Content-Type", "application/json")
        .header("Authorization", format!("Bearer {}", api_key))
}

/// Maximum length for error content in surfaced messages.
/// Keep this high so provider diagnostics remain visible in the UI.
const MAX_ERROR_CONTENT_LEN: usize = 12_000;

/// Sanitize API response content for error messages to prevent credential leakage.
fn sanitize_api_response(content: &str) -> String {
    const SECRET_PATTERNS: &[&str] = &[
        "api_key",
        "apikey",
        "secret",
        "password",
        "credential",
        "bearer",
        "sk-", // common secret prefix
    ];

    // Check if the content might contain secrets
    let lower = content.to_lowercase();
    for pattern in SECRET_PATTERNS {
        if lower.contains(pattern) {
            return "(response details redacted - may contain sensitive data)".to_string();
        }
    }

    let total_chars = content.chars().count();
    if total_chars > MAX_ERROR_CONTENT_LEN {
        return format!(
            "{} … (truncated to {} chars)",
            truncate_str(content, MAX_ERROR_CONTENT_LEN),
            MAX_ERROR_CONTENT_LEN
        );
    }

    content.to_string()
}

fn push_unique_candidate(candidates: &mut Vec<String>, candidate: impl Into<String>) {
    let candidate = candidate.into();
    let trimmed = candidate.trim();
    if trimmed.is_empty() {
        return;
    }
    if !candidates.iter().any(|existing| existing == trimmed) {
        candidates.push(trimmed.to_string());
    }
}

fn strip_markdown_fences(content: &str) -> Option<String> {
    let trimmed = content.trim();
    if !trimmed.starts_with("```") {
        return None;
    }
    let without_open = trimmed.strip_prefix("```")?;
    let after_header = if let Some(newline_idx) = without_open.find('\n') {
        &without_open[newline_idx + 1..]
    } else {
        without_open
    };
    let end_idx = after_header.rfind("```")?;
    Some(after_header[..end_idx].trim().to_string())
}

fn unwrap_outer_wrapper(content: &str) -> Option<String> {
    let trimmed = content.trim();
    if trimmed.len() < 3 {
        return None;
    }
    if trimmed.starts_with('{') && trimmed.ends_with('}') {
        let inner = &trimmed[1..trimmed.len() - 1];
        let inner_trimmed = inner.trim_start();
        if inner_trimmed.starts_with('{') || inner_trimmed.starts_with('[') {
            return Some(inner.trim().to_string());
        }
    } else if trimmed.starts_with('[') && trimmed.ends_with(']') {
        let inner = &trimmed[1..trimmed.len() - 1];
        let inner_trimmed = inner.trim_start();
        if inner_trimmed.starts_with('[')
            || inner_trimmed.starts_with('{')
            || inner_trimmed.starts_with('"')
        {
            return Some(inner.trim().to_string());
        }
    }
    None
}

fn extract_balanced_json_from(content: &str, start: usize) -> Option<String> {
    let mut stack: Vec<char> = Vec::new();
    let mut in_string = false;
    let mut escaped = false;
    for (offset, ch) in content[start..].char_indices() {
        if in_string {
            if escaped {
                escaped = false;
                continue;
            }
            if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_string = false;
            }
            continue;
        }

        match ch {
            '"' => in_string = true,
            '{' => stack.push('}'),
            '[' => stack.push(']'),
            '}' | ']' => {
                if stack.pop() != Some(ch) {
                    return None;
                }
                if stack.is_empty() {
                    let end = start + offset + ch.len_utf8();
                    return Some(content[start..end].to_string());
                }
            }
            _ => {}
        }
    }
    None
}

fn extract_json_candidates(content: &str, max_candidates: usize) -> Vec<String> {
    let mut out = Vec::new();
    if max_candidates == 0 {
        return out;
    }
    for (idx, ch) in content.char_indices() {
        if ch == '{' || ch == '[' {
            if let Some(candidate) = extract_balanced_json_from(content, idx) {
                push_unique_candidate(&mut out, candidate);
                if out.len() >= max_candidates {
                    break;
                }
            }
        }
    }
    out
}

pub(crate) fn parse_structured_content<T>(content: &str) -> anyhow::Result<T>
where
    T: serde::de::DeserializeOwned,
{
    let mut candidates = Vec::new();
    push_unique_candidate(&mut candidates, content);
    if let Some(stripped) = strip_markdown_fences(content) {
        push_unique_candidate(&mut candidates, stripped);
    }

    // Build a few deterministic salvage candidates for mildly malformed wrappers.
    let mut idx = 0usize;
    while idx < candidates.len() {
        let current = candidates[idx].clone();
        for extracted in extract_json_candidates(&current, 4) {
            push_unique_candidate(&mut candidates, extracted);
        }
        if let Some(unwrapped) = unwrap_outer_wrapper(&current) {
            push_unique_candidate(&mut candidates, unwrapped);
        }
        idx += 1;
    }

    let mut last_err: Option<String> = None;
    for candidate in candidates {
        match serde_json::from_str::<T>(&candidate) {
            Ok(data) => return Ok(data),
            Err(err) => last_err = Some(err.to_string()),
        }
    }

    Err(anyhow::anyhow!(
        "Failed to parse structured response: {}\nContent: {}",
        last_err.unwrap_or_else(|| "unknown parse error".to_string()),
        sanitize_api_response(content)
    ))
}

/// Get the configured API key for the active backend.
pub(crate) fn api_key() -> Option<String> {
    let mut config = Config::load();
    config
        .get_api_key()
        .or_else(|| std::env::var("GROQ_API_KEY").ok())
        .or_else(|| std::env::var("GROQ_API_TOKEN").ok())
        .or_else(|| std::env::var("OPENAI_API_KEY").ok())
}

pub(crate) fn missing_api_key_message() -> String {
    "No Groq API key configured. Run 'cosmos --setup' or set GROQ_API_KEY.".to_string()
}

/// Response from LLM including content and usage stats
#[derive(Debug)]
pub struct LlmResponse {
    pub content: String,
    pub usage: Option<Usage>,
}

#[derive(Serialize)]
struct ChatRequest {
    model: String,
    messages: Vec<Message>,
    #[serde(skip_serializing_if = "Option::is_none")]
    user: Option<String>,
    max_tokens: u32,
    stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    response_format: Option<ResponseFormat>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning: Option<ReasoningConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    service_tier: Option<String>,
}

/// Reasoning configuration for models that support it.
#[derive(Serialize)]
struct ReasoningConfig {
    effort: String,
    exclude: bool,
}

/// Response format configuration for structured output parsing.
/// Supports both simple JSON mode and structured output with schema
#[derive(Serialize)]
struct ResponseFormat {
    #[serde(rename = "type")]
    format_type: String,
    /// JSON Schema for structured output (optional)
    #[serde(skip_serializing_if = "Option::is_none")]
    json_schema: Option<JsonSchemaWrapper>,
}

/// Wrapper for JSON Schema in structured output mode
#[derive(Serialize)]
struct JsonSchemaWrapper {
    /// Name of the schema (used for reference)
    name: String,
    /// Whether to strictly enforce the schema
    strict: bool,
    /// The JSON schema definition
    schema: serde_json::Value,
}

#[derive(Serialize, Deserialize)]
struct Message {
    role: String,
    content: String,
}

// ═══════════════════════════════════════════════════════════════════════════
//  ANTHROPIC PROMPT CACHING SUPPORT
// ═══════════════════════════════════════════════════════════════════════════
// Anthropic models support prompt caching via multipart message format with
// cache_control breakpoints. Cached reads are 0.1x input pricing.

/// Cache control for Anthropic prompt caching
#[derive(Serialize, Clone)]
struct CacheControl {
    #[serde(rename = "type")]
    cache_type: String, // "ephemeral"
}

/// A content part in a multipart message (for caching)
#[derive(Serialize, Clone)]
struct ContentPart {
    #[serde(rename = "type")]
    part_type: String, // "text"
    text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    cache_control: Option<CacheControl>,
}

/// Content can be either a simple string or multipart array
#[derive(Serialize, Clone)]
#[serde(untagged)]
enum MessageContent2 {
    Text(String),
    Parts(Vec<ContentPart>),
}

/// Message with multipart content support (for caching)
#[derive(Serialize, Clone)]
struct CachedMessage {
    role: String,
    content: MessageContent2,
}

/// Chat request with cached message support
#[derive(Serialize)]
struct CachedChatRequest {
    model: String,
    messages: Vec<CachedMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    user: Option<String>,
    max_tokens: u32,
    stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    response_format: Option<ResponseFormat>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning: Option<ReasoningConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    service_tier: Option<String>,
}

/// Build messages with caching enabled on the system prompt
fn build_cached_messages(system: &str, user: &str) -> Vec<CachedMessage> {
    vec![
        CachedMessage {
            role: "system".to_string(),
            content: MessageContent2::Parts(vec![ContentPart {
                part_type: "text".to_string(),
                text: system.to_string(),
                cache_control: Some(CacheControl {
                    cache_type: "ephemeral".to_string(),
                }),
            }]),
        },
        CachedMessage {
            role: "user".to_string(),
            content: MessageContent2::Text(user.to_string()),
        },
    ]
}

#[derive(Deserialize)]
struct ChatResponse {
    choices: Vec<Choice>,
    usage: Option<Usage>,
}

#[derive(Deserialize)]
struct Choice {
    message: MessageContent,
}

#[derive(Deserialize)]
struct MessageContent {
    /// Content can be null in some API responses (e.g., when refusal or error occurs)
    #[serde(default)]
    content: Option<String>,
    /// Refusal reason - set when content is blocked by content moderation
    #[serde(default)]
    refusal: Option<String>,
}

/// Check if LLM is available (either BYOK or managed)
pub fn is_available() -> bool {
    api_key().is_some()
}

/// Rate limit retry configuration
pub(crate) const MAX_RETRIES: u32 = 3;
pub(crate) const INITIAL_BACKOFF_MS: u64 = 2000; // 2 seconds
pub(crate) const BACKOFF_MULTIPLIER: u64 = 2; // Exponential backoff
pub(crate) const REQUEST_TIMEOUT_SECS: u64 = 60;

/// Extract retry-after hint from provider response text (if present).
fn parse_retry_after(text: &str) -> Option<u64> {
    // Look for patterns like "retry after X seconds" or "wait X seconds".
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

pub(crate) fn backoff_secs(retry_count: u32) -> u64 {
    let factor = BACKOFF_MULTIPLIER.pow(retry_count.saturating_sub(1));
    let ms = INITIAL_BACKOFF_MS.saturating_mul(factor);
    let secs = ms / 1000;
    if secs == 0 {
        1
    } else {
        secs
    }
}

pub(crate) fn is_retryable_network_error(err: &reqwest::Error) -> bool {
    err.is_timeout() || err.is_connect()
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ProviderFailureKind {
    Timeout,
    RateLimited,
    ServerError,
    NetworkError,
    Other,
}

fn classify_provider_error(message: &str) -> ProviderFailureKind {
    let lower = message.to_ascii_lowercase();
    if lower.contains("timed out after") || lower.contains("request timed out") {
        ProviderFailureKind::Timeout
    } else if lower.contains("no endpoints found") || lower.contains("404 not found") {
        // Some backends use 404 for routing/capability mismatches.
        ProviderFailureKind::ServerError
    } else if lower.contains("rate limited") {
        ProviderFailureKind::RateLimited
    } else if lower.contains("server error") {
        ProviderFailureKind::ServerError
    } else if lower.contains("could not connect") {
        ProviderFailureKind::NetworkError
    } else {
        ProviderFailureKind::Other
    }
}

fn provider_outcome_kind(kind: ProviderFailureKind) -> &'static str {
    match kind {
        ProviderFailureKind::Timeout => "timeout",
        ProviderFailureKind::RateLimited => "rate_limited",
        ProviderFailureKind::ServerError => "server_error",
        ProviderFailureKind::NetworkError => "network_error",
        ProviderFailureKind::Other => "other",
    }
}

/// Structured LLM call for the Speed tier with normalized diagnostics.
pub(crate) async fn call_llm_structured_limited_speed_with_failover<T>(
    system: &str,
    user: &str,
    schema_name: &str,
    schema: serde_json::Value,
    max_tokens: u32,
    timeout_ms: u64,
) -> anyhow::Result<StructuredResponse<T>>
where
    T: serde::de::DeserializeOwned,
{
    let call_start = Instant::now();
    let mut diagnostics = SpeedFailoverDiagnostics {
        total_timeout_ms: timeout_ms,
        attempts: Vec::new(),
        selected_provider: None,
    };

    match call_llm_structured_limited_with_reasoning::<T>(
        system,
        user,
        Model::Speed,
        schema_name,
        schema,
        max_tokens,
        timeout_ms,
        false,
    )
    .await
    {
        Ok(mut response) => {
            diagnostics.selected_provider = Some("groq".to_string());
            diagnostics.attempts.push(ProviderAttemptDiagnostics {
                provider_slug: "groq".to_string(),
                mode: "json_schema".to_string(),
                slice_timeout_ms: timeout_ms,
                elapsed_ms: call_start.elapsed().as_millis() as u64,
                outcome_kind: "success".to_string(),
                error_tail: None,
            });
            response.speed_failover = Some(diagnostics);
            Ok(response)
        }
        Err(err) => {
            let err_text = err.to_string();
            let kind = classify_provider_error(&err_text);
            diagnostics.attempts.push(ProviderAttemptDiagnostics {
                provider_slug: "groq".to_string(),
                mode: "json_schema".to_string(),
                slice_timeout_ms: timeout_ms,
                elapsed_ms: call_start.elapsed().as_millis() as u64,
                outcome_kind: provider_outcome_kind(kind).to_string(),
                error_tail: Some(sanitize_api_response(&err_text)),
            });

            Err(anyhow::Error::new(SpeedFailoverError {
                diagnostics,
                message: format!(
                    "Groq call failed for openai/gpt-oss-120b: {}",
                    sanitize_api_response(&err_text)
                ),
            }))
        }
    }
}

fn reasoning_config(model: Model) -> Option<ReasoningConfig> {
    let _ = model;
    // Groq's OpenAI-compatible endpoint can reject non-standard reasoning config fields.
    None
}

/// Generic provider error envelope (can arrive with HTTP 200 from proxy layers).
#[derive(Deserialize)]
pub(crate) struct ProviderErrorEnvelope {
    pub error: ProviderApiError,
}

#[derive(Deserialize)]
pub(crate) struct ProviderApiError {
    pub message: String,
    #[serde(default)]
    pub code: Option<i32>,
}

/// Send a request to the active LLM backend with automatic retry on transient failures.
///
/// Handles:
/// - Network errors (timeout, connection failures)
/// - Rate limits (429)
/// - Server errors (5xx)
/// - 200-with-error payloads from upstream proxy layers
///
/// Returns the response text on success, or an error after all retries exhausted.
pub(crate) async fn send_with_retry<T: Serialize>(
    client: &reqwest::Client,
    api_key: &str,
    request_body: &T,
) -> anyhow::Result<String> {
    let mut last_error = String::new();
    let mut retry_count = 0;

    while retry_count <= MAX_RETRIES {
        let request_builder = client.post(chat_completions_url()).json(request_body);
        let response = match apply_backend_headers(request_builder, api_key).send().await {
            Ok(response) => response,
            Err(err) => {
                last_error = err.to_string();
                if is_retryable_network_error(&err) && retry_count < MAX_RETRIES {
                    retry_count += 1;
                    let retry_after = backoff_secs(retry_count);
                    tokio::time::sleep(tokio::time::Duration::from_secs(retry_after)).await;
                    continue;
                }
                return Err(map_timeout_error(err));
            }
        };

        let status = response.status();
        let text = match response.text().await {
            Ok(text) => text,
            Err(err) => {
                last_error = err.to_string();
                if is_retryable_network_error(&err) && retry_count < MAX_RETRIES {
                    retry_count += 1;
                    let retry_after = backoff_secs(retry_count);
                    tokio::time::sleep(tokio::time::Duration::from_secs(retry_after)).await;
                    continue;
                }
                return Err(map_timeout_error(err));
            }
        };

        if status.is_success() {
            // Some upstream layers return provider errors with HTTP 200.
            if let Ok(err_resp) = serde_json::from_str::<ProviderErrorEnvelope>(&text) {
                let is_retryable = err_resp
                    .error
                    .code
                    .map(|c| c >= 500 || c == 429)
                    .unwrap_or(true);

                if is_retryable && retry_count < MAX_RETRIES {
                    retry_count += 1;
                    let retry_after = backoff_secs(retry_count);
                    tokio::time::sleep(tokio::time::Duration::from_secs(retry_after)).await;
                    continue;
                }

                return Err(anyhow::anyhow!(
                    "{} error: {}",
                    backend_label(),
                    sanitize_api_response(&err_resp.error.message)
                ));
            }

            return Ok(text);
        }

        last_error = text.clone();

        // Rate limit - retry with backoff
        if status.as_u16() == 429 && retry_count < MAX_RETRIES {
            retry_count += 1;
            let retry_after = parse_retry_after(&text).unwrap_or_else(|| backoff_secs(retry_count));
            tokio::time::sleep(tokio::time::Duration::from_secs(retry_after)).await;
            continue;
        }

        // Server errors - retry with backoff
        if status.is_server_error() && retry_count < MAX_RETRIES {
            retry_count += 1;
            let retry_after = backoff_secs(retry_count);
            tokio::time::sleep(tokio::time::Duration::from_secs(retry_after)).await;
            continue;
        }

        // Non-retryable error or max retries exceeded
        let error_msg = match status.as_u16() {
            401 => "Invalid Groq API key. Run 'cosmos --setup' or set GROQ_API_KEY and try again."
                .to_string(),
            429 => format!(
                "Rate limited by {} after {} retries. Try again in a few minutes. (Press 'e' to view error log)",
                backend_label(),
                retry_count,
            ),
            500..=599 => format!(
                "{} server error ({}). The service may be temporarily unavailable.",
                backend_label(),
                status,
            ),
            _ => format!("API error {}: {}", status, sanitize_api_response(&text)),
        };
        return Err(anyhow::anyhow!("{}", error_msg));
    }

    // Should not reach here, but handle gracefully
    Err(anyhow::anyhow!("{}", last_error))
}

/// Create a configured HTTP client for LLM requests.
pub(crate) fn create_http_client(timeout_secs: u64) -> anyhow::Result<reqwest::Client> {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(timeout_secs))
        .build()
        .map_err(|e| anyhow::anyhow!("Failed to create HTTP client: {}", e))
}

/// Call LLM API with full response including usage stats
/// Includes automatic retry with exponential backoff for rate limits
pub(crate) async fn call_llm_with_usage(
    system: &str,
    user: &str,
    model: Model,
    json_mode: bool,
) -> anyhow::Result<LlmResponse> {
    let api_key = api_key().ok_or_else(|| anyhow::anyhow!(missing_api_key_message()))?;

    if json_mode && !model.supports_json_mode() {
        return Err(anyhow::anyhow!(
            "JSON mode isn't supported for {}. Try a different model.",
            model.id()
        ));
    }

    let client = create_http_client(REQUEST_TIMEOUT_SECS)?;

    let response_format = if json_mode {
        Some(ResponseFormat {
            format_type: "json_object".to_string(),
            json_schema: None,
        })
    } else {
        None
    };

    let stream = false;
    let reasoning = reasoning_config(model);

    let request = ChatRequest {
        model: model_id_for_backend(model),
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
        user: None,
        max_tokens: model.max_tokens(),
        stream,
        response_format,
        reasoning,
        service_tier: groq_service_tier(),
    };

    let text = send_with_retry(&client, &api_key, &request).await?;

    let parsed: ChatResponse = serde_json::from_str(&text).map_err(|e| {
        anyhow::anyhow!(
            "Failed to parse {} response: {}\n{}",
            backend_label(),
            e,
            sanitize_api_response(&text)
        )
    })?;

    let choice = parsed.choices.first();

    // Check for refusal (content moderation)
    if let Some(c) = choice {
        if let Some(refusal) = &c.message.refusal {
            return Err(anyhow::anyhow!(
                "Request was refused: {}",
                truncate_str(refusal, 200)
            ));
        }
    }

    // Extract content, handling null/empty cases
    let content = choice
        .and_then(|c| c.message.content.clone())
        .unwrap_or_default();

    if content.is_empty() {
        return Err(anyhow::anyhow!(
            "API returned empty response. The model may have been rate limited or failed to generate content. Please try again."
        ));
    }

    Ok(LlmResponse {
        content,
        usage: parsed.usage,
    })
}

/// Response from structured output call - parsed JSON and usage stats
#[derive(Debug)]
pub struct StructuredResponse<T> {
    pub data: T,
    pub usage: Option<Usage>,
    pub speed_failover: Option<SpeedFailoverDiagnostics>,
}

/// Diagnostics for Speed tier provider failover (gpt-oss-120b).
///
/// This is used by the apply harness for transparency: when something fails, we want
/// to know which providers were tried, with which timeouts, and what each returned.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SpeedFailoverDiagnostics {
    pub total_timeout_ms: u64,
    #[serde(default)]
    pub attempts: Vec<ProviderAttemptDiagnostics>,
    #[serde(default)]
    pub selected_provider: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderAttemptDiagnostics {
    pub provider_slug: String,
    pub mode: String,
    pub slice_timeout_ms: u64,
    pub elapsed_ms: u64,
    pub outcome_kind: String,
    #[serde(default)]
    pub error_tail: Option<String>,
}

#[derive(Debug)]
pub(crate) struct SpeedFailoverError {
    pub diagnostics: SpeedFailoverDiagnostics,
    pub message: String,
}

impl std::fmt::Display for SpeedFailoverError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for SpeedFailoverError {}

/// Call LLM API with structured output (strict JSON schema).
///
/// This is the preferred path for JSON responses that should not rely on
/// custom "ask the model to fix JSON" retries.
pub(crate) async fn call_llm_structured<T>(
    system: &str,
    user: &str,
    model: Model,
    schema_name: &str,
    schema: serde_json::Value,
) -> anyhow::Result<StructuredResponse<T>>
where
    T: serde::de::DeserializeOwned,
{
    let api_key = api_key().ok_or_else(|| anyhow::anyhow!(missing_api_key_message()))?;

    if !model.supports_structured_outputs() {
        return Err(anyhow::anyhow!(
            "Structured outputs aren't supported for {}. Try a different model.",
            model.id()
        ));
    }

    let client = create_http_client(REQUEST_TIMEOUT_SECS)?;

    let response_format = Some(ResponseFormat {
        format_type: "json_schema".to_string(),
        json_schema: Some(JsonSchemaWrapper {
            name: schema_name.to_string(),
            strict: true,
            schema,
        }),
    });

    let stream = false;
    let reasoning = reasoning_config(model);

    let request = ChatRequest {
        model: model_id_for_backend(model),
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
        user: None,
        max_tokens: model.max_tokens(),
        stream,
        response_format,
        reasoning,
        service_tier: groq_service_tier(),
    };

    let text = send_with_retry(&client, &api_key, &request).await?;

    let parsed: ChatResponse = serde_json::from_str(&text).map_err(|e| {
        anyhow::anyhow!(
            "Failed to parse {} response: {}\n{}",
            backend_label(),
            e,
            sanitize_api_response(&text)
        )
    })?;

    let choice = parsed.choices.first();

    // Check for refusal (content moderation)
    if let Some(c) = choice {
        if let Some(refusal) = &c.message.refusal {
            return Err(anyhow::anyhow!(
                "Request was refused: {}",
                truncate_str(refusal, 200)
            ));
        }
    }

    // Extract content, handling null/empty cases
    let content = choice
        .and_then(|c| c.message.content.clone())
        .unwrap_or_default();

    if content.is_empty() {
        return Err(anyhow::anyhow!(
            "API returned empty response. The model may have been rate limited or failed to generate content. Please try again."
        ));
    }

    let data: T = parse_structured_content(&content)?;

    Ok(StructuredResponse {
        data,
        usage: parsed.usage,
        speed_failover: None,
    })
}

/// Call LLM API with structured output while
/// enforcing max tokens and a request timeout with reasoning disabled.
/// Useful for latency-sensitive paths where structured output already constrains format.
pub(crate) async fn call_llm_structured_limited_no_reasoning<T>(
    system: &str,
    user: &str,
    model: Model,
    schema_name: &str,
    schema: serde_json::Value,
    max_tokens: u32,
    timeout_ms: u64,
) -> anyhow::Result<StructuredResponse<T>>
where
    T: serde::de::DeserializeOwned,
{
    call_llm_structured_limited_with_reasoning(
        system,
        user,
        model,
        schema_name,
        schema,
        max_tokens,
        timeout_ms,
        false,
    )
    .await
}

// Internal helper keeps structured-call knobs explicit for callers.
#[allow(clippy::too_many_arguments)]
async fn call_llm_structured_limited_with_reasoning<T>(
    system: &str,
    user: &str,
    model: Model,
    schema_name: &str,
    schema: serde_json::Value,
    max_tokens: u32,
    timeout_ms: u64,
    include_reasoning: bool,
) -> anyhow::Result<StructuredResponse<T>>
where
    T: serde::de::DeserializeOwned,
{
    let api_key = api_key().ok_or_else(|| anyhow::anyhow!(missing_api_key_message()))?;

    if !model.supports_structured_outputs() {
        return Err(anyhow::anyhow!(
            "Structured outputs aren't supported for {}. Try a different model.",
            model.id()
        ));
    }

    let client = create_http_client(REQUEST_TIMEOUT_SECS)?;

    let response_format = Some(ResponseFormat {
        format_type: "json_schema".to_string(),
        json_schema: Some(JsonSchemaWrapper {
            name: schema_name.to_string(),
            strict: true,
            schema,
        }),
    });

    let stream = false;
    let reasoning = if include_reasoning {
        reasoning_config(model)
    } else {
        None
    };

    let request = ChatRequest {
        model: model_id_for_backend(model),
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
        user: None,
        max_tokens,
        stream,
        response_format,
        reasoning,
        service_tier: groq_service_tier(),
    };

    let text = timeout(
        Duration::from_millis(timeout_ms),
        send_with_retry(&client, &api_key, &request),
    )
    .await
    .map_err(|_| anyhow::anyhow!("Timed out after {}ms.", timeout_ms))??;

    let parsed: ChatResponse = serde_json::from_str(&text).map_err(|e| {
        anyhow::anyhow!(
            "Failed to parse {} response: {}\n{}",
            backend_label(),
            e,
            sanitize_api_response(&text)
        )
    })?;

    let choice = parsed.choices.first();
    if let Some(c) = choice {
        if let Some(refusal) = &c.message.refusal {
            return Err(anyhow::anyhow!(
                "Request was refused: {}",
                truncate_str(refusal, 200)
            ));
        }
    }

    let content = choice
        .and_then(|c| c.message.content.clone())
        .unwrap_or_default();
    if content.is_empty() {
        return Err(anyhow::anyhow!(
            "API returned empty response. The model may have been rate limited or failed to generate content. Please try again."
        ));
    }

    let data: T = parse_structured_content(&content)?;

    Ok(StructuredResponse {
        data,
        usage: parsed.usage,
        speed_failover: None,
    })
}

/// Call LLM API with structured output and cache hints for repeated prompts.
///
/// # Arguments
/// * `system` - System prompt (will be cached)
/// * `user` - User message (not cached - changes each call)
/// * `model` - Model to use
/// * `schema_name` - Name for the schema (e.g., "fix_content")
/// * `schema` - JSON Schema definition
///
/// # Returns
/// Parsed response matching type T and usage stats
pub(crate) async fn call_llm_structured_cached<T>(
    system: &str,
    user: &str,
    model: Model,
    schema_name: &str,
    schema: serde_json::Value,
) -> anyhow::Result<StructuredResponse<T>>
where
    T: serde::de::DeserializeOwned,
{
    let api_key = api_key().ok_or_else(|| anyhow::anyhow!(missing_api_key_message()))?;

    if !model.supports_structured_outputs() {
        return Err(anyhow::anyhow!(
            "Structured outputs aren't supported for {}. Try a different model.",
            model.id()
        ));
    }

    let client = create_http_client(REQUEST_TIMEOUT_SECS)?;

    let response_format = Some(ResponseFormat {
        format_type: "json_schema".to_string(),
        json_schema: Some(JsonSchemaWrapper {
            name: schema_name.to_string(),
            strict: true,
            schema,
        }),
    });

    let stream = false;
    let reasoning = reasoning_config(model);

    // Use cached messages with cache_control on system prompt
    let request = CachedChatRequest {
        model: model_id_for_backend(model),
        messages: build_cached_messages(system, user),
        user: None,
        max_tokens: model.max_tokens(),
        stream,
        response_format,
        reasoning,
        service_tier: groq_service_tier(),
    };

    let text = send_with_retry(&client, &api_key, &request).await?;

    let parsed: ChatResponse = serde_json::from_str(&text).map_err(|e| {
        anyhow::anyhow!(
            "Failed to parse {} response: {}\n{}",
            backend_label(),
            e,
            sanitize_api_response(&text)
        )
    })?;

    let choice = parsed.choices.first();

    // Check for refusal (content moderation)
    if let Some(c) = choice {
        if let Some(refusal) = &c.message.refusal {
            return Err(anyhow::anyhow!(
                "Request was refused: {}",
                truncate_str(refusal, 200)
            ));
        }
    }

    // Extract content, handling null/empty cases
    let content = choice
        .and_then(|c| c.message.content.clone())
        .unwrap_or_default();

    if content.is_empty() {
        return Err(anyhow::anyhow!(
            "API returned empty response. The model may have been rate limited or failed to generate content. Please try again."
        ));
    }

    let data: T = parse_structured_content(&content)?;

    Ok(StructuredResponse {
        data,
        usage: parsed.usage,
        speed_failover: None,
    })
}

fn map_timeout_error(err: reqwest::Error) -> anyhow::Error {
    if err.is_timeout() {
        anyhow::anyhow!("{} request timed out. Please try again.", backend_label())
    } else if err.is_connect() {
        anyhow::anyhow!(
            "Could not connect to {}. Check your network and try again.",
            backend_label()
        )
    } else {
        err.into()
    }
}

/// Truncate a string for display (Unicode-safe)
pub(crate) fn truncate_str(s: &str, max_chars: usize) -> &str {
    if s.chars().count() <= max_chars {
        s
    } else {
        // Find byte index of the max_chars-th character
        let byte_idx = s
            .char_indices()
            .nth(max_chars)
            .map(|(i, _)| i)
            .unwrap_or(s.len());
        &s[..byte_idx]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, Deserialize, PartialEq, Eq)]
    struct ParseProbe {
        description: String,
    }

    #[test]
    fn test_model_id_normalization_for_groq_backend_and_smart_model() {
        let groq_id = model_id_for_backend_impl(Model::Speed);
        assert_eq!(groq_id, "openai/gpt-oss-120b");
        let smart_id = model_id_for_backend_impl(Model::Smart);
        assert_eq!(smart_id, "openai/gpt-oss-120b");
    }

    #[test]
    fn test_normalize_groq_service_tier_defaults_to_on_demand() {
        assert_eq!(normalize_groq_service_tier(None), "on_demand");
        assert_eq!(normalize_groq_service_tier(Some("  ")), "on_demand");
        assert_eq!(normalize_groq_service_tier(Some("FLEX")), "flex");
        assert_eq!(
            normalize_groq_service_tier(Some("performance")),
            "performance"
        );
    }

    #[test]
    fn test_reasoning_config_disabled_for_direct_groq() {
        assert!(reasoning_config(Model::Speed).is_none());
        assert!(reasoning_config(Model::Smart).is_none());
    }

    #[test]
    fn test_parse_structured_content_handles_extra_wrapper_braces() {
        let malformed = "{\n {\"description\":\"hello\"}\n}";
        let parsed: ParseProbe = parse_structured_content(malformed).unwrap();
        assert_eq!(
            parsed,
            ParseProbe {
                description: "hello".to_string()
            }
        );
    }

    #[test]
    fn test_parse_structured_content_handles_markdown_fences() {
        let fenced = "```json\n{\"description\":\"hello\"}\n```";
        let parsed: ParseProbe = parse_structured_content(fenced).unwrap();
        assert_eq!(
            parsed,
            ParseProbe {
                description: "hello".to_string()
            }
        );
    }

    #[test]
    fn test_parse_structured_content_handles_leading_garbage_before_double_object() {
        let malformed = ".{\n{\"description\":\"hello\"}\n}";
        let parsed: ParseProbe = parse_structured_content(malformed).unwrap();
        assert_eq!(
            parsed,
            ParseProbe {
                description: "hello".to_string()
            }
        );
    }
}
