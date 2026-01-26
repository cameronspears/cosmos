//! Agentic LLM client with tool-calling support.
//!
//! Enables models to explore codebases by calling tools (grep, read, ls)
//! in a loop until they have enough context to complete their task.

use super::models::Model;
use super::tools::{execute_tool, get_tool_definitions, ToolCall, ToolDefinition};
use crate::config::Config;
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::time::Duration;

const OPENROUTER_URL: &str = "https://openrouter.ai/api/v1/chat/completions";
const REQUEST_TIMEOUT_SECS: u64 = 90; // Longer timeout for agentic loops

/// Response from an agentic LLM call
#[derive(Debug)]
pub struct AgenticResponse {
    pub content: String,
}

/// A message in the conversation
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: String,
    pub content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCallMessage>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallMessage {
    pub id: String,
    #[serde(rename = "type")]
    pub call_type: String,
    pub function: FunctionCallMessage,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FunctionCallMessage {
    pub name: String,
    pub arguments: String,
}

#[derive(Serialize)]
struct ChatRequest {
    model: String,
    messages: Vec<Message>,
    max_tokens: u32,
    stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    response_format: Option<ResponseFormat>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<ToolDefinition>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    provider: Option<ProviderConfig>,
}

#[derive(Serialize)]
struct ResponseFormat {
    #[serde(rename = "type")]
    format_type: String,
}

#[derive(Serialize)]
struct ProviderConfig {
    allow_fallbacks: bool,
}

#[derive(Deserialize)]
struct ChatResponse {
    choices: Vec<Choice>,
}

#[derive(Deserialize)]
struct Choice {
    message: ResponseMessage,
}

#[derive(Deserialize)]
struct ResponseMessage {
    content: Option<String>,
    tool_calls: Option<Vec<ToolCallMessage>>,
}

#[derive(Deserialize)]
struct ErrorResponse {
    error: ApiError,
}

#[derive(Deserialize)]
struct ApiError {
    message: String,
}

fn api_key() -> Option<String> {
    let mut config = Config::load();
    config.get_api_key()
}

/// Call LLM with tool-calling capability.
///
/// The model can call tools (grep, read, ls) to explore the codebase.
/// The function loops until the model returns a final text response.
pub async fn call_llm_agentic(
    system: &str,
    user: &str,
    model: Model,
    repo_root: &Path,
    json_mode: bool,
) -> anyhow::Result<AgenticResponse> {
    let api_key = api_key().ok_or_else(|| {
        anyhow::anyhow!("No API key configured. Run 'cosmos --setup' to get started.")
    })?;

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(REQUEST_TIMEOUT_SECS))
        .build()?;

    let tools = get_tool_definitions();
    let mut messages = vec![
        Message {
            role: "system".to_string(),
            content: Some(system.to_string()),
            tool_calls: None,
            tool_call_id: None,
        },
        Message {
            role: "user".to_string(),
            content: Some(user.to_string()),
            tool_calls: None,
            tool_call_id: None,
        },
    ];

    // Lean hybrid: allow only 1-3 surgical tool calls, then must respond
    const MAX_ITERATIONS: usize = 5;
    let mut iteration = 0;

    loop {
        iteration += 1;

        if iteration > MAX_ITERATIONS {
            // Don't error - just force the model to respond with what it has
            // by making one final call without tools
            break;
        }
        // Note: json_mode is accepted for API compatibility but not currently used
        // during the agentic loop since tool calls don't use JSON response format
        let response_format: Option<ResponseFormat> = None;
        let _ = json_mode; // Silence unused warning

        let request = ChatRequest {
            model: model.id().to_string(),
            messages: messages.clone(),
            max_tokens: model.max_tokens(),
            stream: false,
            response_format,
            tools: Some(tools.clone()),
            provider: Some(ProviderConfig {
                allow_fallbacks: true,
            }),
        };

        let response = client
            .post(OPENROUTER_URL)
            .header("Content-Type", "application/json")
            .header("HTTP-Referer", "https://cosmos.dev")
            .header("X-Title", "Cosmos")
            .header("Authorization", format!("Bearer {}", api_key))
            .json(&request)
            .send()
            .await?;

        let status = response.status();
        let text = response.text().await?;

        if !status.is_success() {
            // Check for error response
            if let Ok(err_resp) = serde_json::from_str::<ErrorResponse>(&text) {
                return Err(anyhow::anyhow!("API error: {}", err_resp.error.message));
            }
            return Err(anyhow::anyhow!("API error {}: {}", status, text));
        }

        let parsed: ChatResponse = serde_json::from_str(&text)
            .map_err(|e| anyhow::anyhow!("Failed to parse response: {}\n{}", e, text))?;

        let choice = parsed
            .choices
            .first()
            .ok_or_else(|| anyhow::anyhow!("No response from model"))?;

        // Check if model wants to call tools
        if let Some(tool_calls) = &choice.message.tool_calls {
            if !tool_calls.is_empty() {
                // Add assistant message with tool calls
                messages.push(Message {
                    role: "assistant".to_string(),
                    content: choice.message.content.clone(),
                    tool_calls: Some(tool_calls.clone()),
                    tool_call_id: None,
                });

                // Execute each tool and add results
                for tc in tool_calls {
                    let tool_call = ToolCall {
                        id: tc.id.clone(),
                        function: super::tools::FunctionCall {
                            name: tc.function.name.clone(),
                            arguments: tc.function.arguments.clone(),
                        },
                    };

                    let result = execute_tool(repo_root, &tool_call);

                    messages.push(Message {
                        role: "tool".to_string(),
                        content: Some(result.content),
                        tool_calls: None,
                        tool_call_id: Some(tc.id.clone()),
                    });
                }

                // Continue loop to get next response
                continue;
            }
        }

        // Model returned final response (no tool calls)
        let content = choice.message.content.clone().unwrap_or_default();

        return Ok(AgenticResponse { content });
    }

    // If we broke out of loop (hit max iterations), make one final call WITHOUT tools
    // to force the model to respond with whatever it has
    messages.push(Message {
        role: "user".to_string(),
        content: Some("You've gathered enough context. Now respond with your JSON suggestions based on what you've learned. No more tool calls.".to_string()),
        tool_calls: None,
        tool_call_id: None,
    });

    let final_request = ChatRequest {
        model: model.id().to_string(),
        messages: messages.clone(),
        max_tokens: model.max_tokens(),
        stream: false,
        response_format: None,
        tools: None, // No tools - force text response
        provider: Some(ProviderConfig {
            allow_fallbacks: true,
        }),
    };

    let response = client
        .post(OPENROUTER_URL)
        .header("Content-Type", "application/json")
        .header("HTTP-Referer", "https://cosmos.dev")
        .header("X-Title", "Cosmos")
        .header("Authorization", format!("Bearer {}", api_key))
        .json(&final_request)
        .send()
        .await?;

    let text = response.text().await?;
    let parsed: ChatResponse = serde_json::from_str(&text)
        .map_err(|e| anyhow::anyhow!("Failed to parse final response: {}\n{}", e, text))?;

    let content = parsed
        .choices
        .first()
        .and_then(|c| c.message.content.clone())
        .unwrap_or_default();

    Ok(AgenticResponse { content })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_message_serialization_basic() {
        let msg = Message {
            role: "user".to_string(),
            content: Some("hello".to_string()),
            tool_calls: None,
            tool_call_id: None,
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains("user"));
        assert!(json.contains("hello"));
        // Should not contain tool_calls when None
        assert!(!json.contains("tool_calls"));
    }

    #[test]
    fn test_message_with_tool_calls() {
        let msg = Message {
            role: "assistant".to_string(),
            content: None,
            tool_calls: Some(vec![ToolCallMessage {
                id: "call_123".to_string(),
                call_type: "function".to_string(),
                function: FunctionCallMessage {
                    name: "shell".to_string(),
                    arguments: r#"{"command": "ls"}"#.to_string(),
                },
            }]),
            tool_call_id: None,
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains("tool_calls"));
        assert!(json.contains("call_123"));
        assert!(json.contains("shell"));
    }

    #[test]
    fn test_tool_result_message() {
        let msg = Message {
            role: "tool".to_string(),
            content: Some("file1.rs\nfile2.rs".to_string()),
            tool_calls: None,
            tool_call_id: Some("call_123".to_string()),
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains("tool"));
        assert!(json.contains("call_123"));
        assert!(json.contains("file1.rs"));
    }

    #[test]
    fn test_tool_definition_serialization() {
        let tools = get_tool_definitions();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].function.name, "shell");

        let json = serde_json::to_string(&tools[0]).unwrap();
        assert!(json.contains("shell"));
        assert!(json.contains("command"));
    }
}
