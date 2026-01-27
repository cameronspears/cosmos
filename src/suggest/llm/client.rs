use super::models::{Model, Usage};
use crate::config::Config;
use serde::{Deserialize, Serialize};
use std::time::Duration;

/// OpenRouter direct API URL (BYOK mode)
pub(crate) const OPENROUTER_URL: &str = "https://openrouter.ai/api/v1/chat/completions";

/// Get the configured OpenRouter API key, if any.
pub(crate) fn api_key() -> Option<String> {
    let mut config = Config::load();
    config.get_api_key()
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
    max_tokens: u32,
    stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    response_format: Option<ResponseFormat>,
    /// OpenRouter provider configuration for automatic fallback
    #[serde(skip_serializing_if = "Option::is_none")]
    provider: Option<ProviderConfig>,
}

/// OpenRouter provider configuration
#[derive(Serialize)]
struct ProviderConfig {
    /// Allow OpenRouter to try other providers if the primary fails
    allow_fallbacks: bool,
}

/// Response format configuration for OpenRouter
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
    max_tokens: u32,
    stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    response_format: Option<ResponseFormat>,
    #[serde(skip_serializing_if = "Option::is_none")]
    provider: Option<ProviderConfig>,
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

/// OpenRouter error response (can come with 200 status for upstream errors)
#[derive(Deserialize)]
pub(crate) struct OpenRouterError {
    pub error: OpenRouterApiError,
}

#[derive(Deserialize)]
pub(crate) struct OpenRouterApiError {
    pub message: String,
    #[serde(default)]
    pub code: Option<i32>,
}

/// Send a request to OpenRouter with automatic retry on transient failures.
///
/// Handles:
/// - Network errors (timeout, connection failures)
/// - Rate limits (429)
/// - Server errors (5xx)
/// - OpenRouter's 200-with-error responses
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
        // Build request with OpenRouter headers
        let response = match client
            .post(OPENROUTER_URL)
            .header("Content-Type", "application/json")
            .header("HTTP-Referer", "https://cosmos.dev")
            .header("X-Title", "Cosmos")
            .header("Authorization", format!("Bearer {}", api_key))
            .json(request_body)
            .send()
            .await
        {
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
            // OpenRouter sometimes returns errors with 200 status (upstream provider issues)
            if let Ok(err_resp) = serde_json::from_str::<OpenRouterError>(&text) {
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
                    "OpenRouter error: {}",
                    truncate_str(&err_resp.error.message, 200)
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
            401 => "Invalid API key. Run 'cosmos --setup' to update it.".to_string(),
            429 => format!(
                "Rate limited by OpenRouter after {} retries. Try again in a few minutes. (Press 'e' to view error log)",
                retry_count
            ),
            500..=599 => format!(
                "OpenRouter server error ({}). The service may be temporarily unavailable.",
                status
            ),
            _ => format!("API error {}: {}", status, truncate_str(&text, 200)),
        };
        return Err(anyhow::anyhow!("{}", error_msg));
    }

    // Should not reach here, but handle gracefully
    Err(anyhow::anyhow!("{}", last_error))
}

/// Create a configured HTTP client for OpenRouter requests
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
    let api_key = api_key().ok_or_else(|| {
        anyhow::anyhow!("No API key configured. Run 'cosmos --setup' to get started.")
    })?;

    let client = create_http_client(REQUEST_TIMEOUT_SECS)?;

    let response_format = if json_mode {
        Some(ResponseFormat {
            format_type: "json_object".to_string(),
            json_schema: None,
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
        provider: Some(ProviderConfig {
            allow_fallbacks: true,
        }),
    };

    let text = send_with_retry(&client, &api_key, &request).await?;

    let parsed: ChatResponse = serde_json::from_str(&text)
        .map_err(|e| anyhow::anyhow!("Failed to parse OpenRouter response: {}\n{}", e, text))?;

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
}

/// Call LLM API with structured output AND Anthropic prompt caching.
///
/// This variant enables prompt caching for Anthropic models (Claude), which:
/// - Reduces costs by ~90% on cached prompt reads (0.1x pricing)
/// - Potentially improves reliability (OpenRouter routes to same provider)
/// - Has 5-minute cache TTL by default
///
/// Use this for repeated calls with the same system prompt (like fix generation).
///
/// # Arguments
/// * `system` - System prompt (will be cached)
/// * `user` - User message (not cached - changes each call)
/// * `model` - Model to use (caching only works with Anthropic models)
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
    let api_key = api_key().ok_or_else(|| {
        anyhow::anyhow!("No API key configured. Run 'cosmos --setup' to get started.")
    })?;

    let client = create_http_client(REQUEST_TIMEOUT_SECS)?;

    let response_format = ResponseFormat {
        format_type: "json_schema".to_string(),
        json_schema: Some(JsonSchemaWrapper {
            name: schema_name.to_string(),
            strict: true,
            schema,
        }),
    };

    // Use cached messages with cache_control on system prompt
    let request = CachedChatRequest {
        model: model.id().to_string(),
        messages: build_cached_messages(system, user),
        max_tokens: model.max_tokens(),
        stream: false,
        response_format: Some(response_format),
        provider: Some(ProviderConfig {
            allow_fallbacks: true,
        }),
    };

    let text = send_with_retry(&client, &api_key, &request).await?;

    let parsed: ChatResponse = serde_json::from_str(&text)
        .map_err(|e| anyhow::anyhow!("Failed to parse OpenRouter response: {}\n{}", e, text))?;

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

    let data: T = serde_json::from_str(&content).map_err(|e| {
        anyhow::anyhow!(
            "Failed to parse structured response: {}\nContent: {}",
            e,
            truncate_str(&content, 200)
        )
    })?;

    Ok(StructuredResponse {
        data,
        usage: parsed.usage,
    })
}

fn map_timeout_error(err: reqwest::Error) -> anyhow::Error {
    if err.is_timeout() {
        anyhow::anyhow!("OpenRouter request timed out. Please try again.")
    } else if err.is_connect() {
        anyhow::anyhow!("Could not connect to OpenRouter. Check your network and try again.")
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

/// OpenRouter credits API URL
const OPENROUTER_CREDITS_URL: &str = "https://openrouter.ai/api/v1/credits";

/// Response from OpenRouter credits endpoint
#[derive(Deserialize)]
struct CreditsResponse {
    data: CreditsData,
}

#[derive(Deserialize)]
struct CreditsData {
    total_credits: f64,
    total_usage: f64,
}

/// Fetch the current account balance from OpenRouter.
/// Returns the remaining credits (total_credits - total_usage).
pub async fn fetch_account_balance() -> anyhow::Result<f64> {
    let api_key = api_key().ok_or_else(|| anyhow::anyhow!("No API key configured"))?;

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()?;

    let response = client
        .get(OPENROUTER_CREDITS_URL)
        .header("Authorization", format!("Bearer {}", api_key))
        .send()
        .await?;

    if !response.status().is_success() {
        return Err(anyhow::anyhow!(
            "Failed to fetch balance: {}",
            response.status()
        ));
    }

    let credits: CreditsResponse = response.json().await?;
    let remaining = credits.data.total_credits - credits.data.total_usage;
    Ok(remaining)
}
