use super::models::{Model, Usage};
use crate::config::Config;
use serde::{Deserialize, Serialize};

/// OpenRouter direct API URL (BYOK mode)
const OPENROUTER_URL: &str = "https://openrouter.ai/api/v1/chat/completions";

/// Get the configured OpenRouter API key, if any.
fn api_key() -> Option<String> {
    Config::load().get_api_key()
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
pub(crate) async fn call_llm(system: &str, user: &str, model: Model) -> anyhow::Result<String> {
    let response = call_llm_with_usage(system, user, model, false).await?;
    Ok(response.content)
}

/// Rate limit retry configuration
const MAX_RETRIES: u32 = 3;
const INITIAL_BACKOFF_MS: u64 = 2000; // 2 seconds
const BACKOFF_MULTIPLIER: u64 = 2; // Exponential backoff

/// Extract retry-after hint from OpenRouter response (if present)
fn parse_retry_after(text: &str) -> Option<u64> {
    // OpenRouter may include retry-after in response body or we estimate
    // Look for patterns like "retry after X seconds" or "wait X seconds"
    let text_lower = text.to_lowercase();
    if let Some(pos) = text_lower.find("retry") {
        // Try to extract a number after "retry"
        let after_retry = &text_lower[pos..];
        for word in after_retry.split_whitespace().skip(1).take(5) {
            if let Ok(secs) = word
                .trim_matches(|c: char| !c.is_numeric())
                .parse::<u64>()
            {
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
pub(crate) async fn call_llm_with_usage(
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

        let status = response.status();
        let text = response.text().await?;

        if status.is_success() {
            let parsed: ChatResponse = serde_json::from_str(&text).map_err(|e| {
                anyhow::anyhow!("Failed to parse OpenRouter response: {}\n{}", e, text)
            })?;

            let content = parsed
                .choices
                .first()
                .map(|c| c.message.content.clone())
                .unwrap_or_default();

            return Ok(LlmResponse {
                content,
                usage: parsed.usage,
                model: parsed.model.unwrap_or_default(),
            });
        }

        last_error = text.clone();

        // Check if we should retry (rate limits)
        if status.as_u16() == 429 && retry_count < MAX_RETRIES {
            retry_count += 1;

            // Try to parse retry-after
            let retry_after = parse_retry_after(&text).unwrap_or_else(|| {
                // Exponential backoff
                (INITIAL_BACKOFF_MS * BACKOFF_MULTIPLIER.pow(retry_count - 1)) / 1000
            });

            eprintln!(
                "  OpenRouter rate limited. Retrying in {}s (attempt {}/{})",
                retry_after, retry_count, MAX_RETRIES
            );
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

    // Should not reach here, but handle it gracefully
    Err(anyhow::anyhow!("{}", last_error))
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
