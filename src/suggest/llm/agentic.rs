//! Agentic LLM client with tool-calling support.
//!
//! Enables models to explore codebases by calling tools (grep, read, ls)
//! in a loop until they have enough context to complete their task.

use super::client::{api_key, create_http_client, send_with_retry, REQUEST_TIMEOUT_SECS};
use super::models::Model;
use super::tools::{execute_tool, get_tool_definitions, ToolCall, ToolDefinition};
use serde::{Deserialize, Serialize};
use std::path::Path;

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
    reasoning: Option<ReasoningConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<ToolDefinition>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_choice: Option<ToolChoice>,
    #[serde(skip_serializing_if = "Option::is_none")]
    parallel_tool_calls: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    provider: Option<ProviderConfig>,
}

#[derive(Serialize)]
struct ResponseFormat {
    #[serde(rename = "type")]
    format_type: String,
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "snake_case")]
enum ToolChoice {
    Auto,
    None,
}

#[derive(Serialize)]
struct ProviderConfig {
    allow_fallbacks: bool,
}

/// OpenRouter reasoning configuration for extended thinking
#[derive(Serialize)]
struct ReasoningConfig {
    effort: String,
    /// Exclude reasoning from the response (we only want the final answer)
    exclude: bool,
}

fn reasoning_config(model: Model) -> Option<ReasoningConfig> {
    model.reasoning_effort().map(|effort| ReasoningConfig {
        effort: effort.to_string(),
        exclude: true,
    })
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

/// Call LLM with tool-calling capability.
///
/// The model can call tools (grep, read, ls) to explore the codebase.
/// The function loops until the model returns a final text response.
/// Now includes automatic retry with exponential backoff for transient failures.
///
/// `max_iterations`: Maximum tool-calling rounds before forcing a response.
/// - Suggestions: 8 (needs exploration)
/// - Verification: 3 (code already provided)
/// - Review: 4 (diff already provided)
pub async fn call_llm_agentic(
    system: &str,
    user: &str,
    model: Model,
    repo_root: &Path,
    json_mode: bool,
    max_iterations: usize,
) -> anyhow::Result<AgenticResponse> {
    let api_key = api_key().ok_or_else(|| {
        anyhow::anyhow!("No API key configured. Run 'cosmos --setup' to get started.")
    })?;

    let client = create_http_client(REQUEST_TIMEOUT_SECS)?;

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

    let mut iteration = 0;

    loop {
        iteration += 1;

        if iteration > max_iterations {
            // Force the model to respond with what it has
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
            reasoning: reasoning_config(model),
            tools: Some(tools.clone()),
            tool_choice: Some(ToolChoice::Auto),
            parallel_tool_calls: Some(true),
            provider: Some(ProviderConfig {
                allow_fallbacks: true,
            }),
        };

        // Use shared retry helper - handles timeouts, rate limits, server errors
        let text = send_with_retry(&client, &api_key, &request).await?;

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

        // Validate we got actual content
        if content.trim().is_empty() {
            return Err(anyhow::anyhow!(
                "Model returned empty response. This may be due to rate limiting or an API issue. Try again."
            ));
        }

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
        reasoning: reasoning_config(model),
        tools: Some(tools),
        tool_choice: Some(ToolChoice::None),
        parallel_tool_calls: Some(true),
        provider: Some(ProviderConfig {
            allow_fallbacks: true,
        }),
    };

    // Use shared retry helper for final request too
    let text = send_with_retry(&client, &api_key, &final_request).await?;

    let parsed: ChatResponse = serde_json::from_str(&text)
        .map_err(|e| anyhow::anyhow!("Failed to parse final response: {}\n{}", e, text))?;

    let content = parsed
        .choices
        .first()
        .and_then(|c| c.message.content.clone())
        .unwrap_or_default();

    // Validate we got actual content
    if content.trim().is_empty() {
        return Err(anyhow::anyhow!(
            "Model returned empty response after exploration. This may be due to rate limiting or an API issue. Try again."
        ));
    }

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
        assert_eq!(tools.len(), 5); // tree, head, search, read_range, shell

        // First tool should be tree (for top-down exploration)
        assert_eq!(tools[0].function.name, "tree");

        // Last tool should be shell (fallback)
        assert_eq!(tools[4].function.name, "shell");

        let json = serde_json::to_string(&tools[0]).unwrap();
        assert!(json.contains("tree"));
    }

    #[test]
    fn test_chat_request_tool_controls() {
        let tools = get_tool_definitions();
        let request = ChatRequest {
            model: "test-model".to_string(),
            messages: vec![Message {
                role: "user".to_string(),
                content: Some("hi".to_string()),
                tool_calls: None,
                tool_call_id: None,
            }],
            max_tokens: 128,
            stream: false,
            response_format: None,
            reasoning: None,
            tools: Some(tools),
            tool_choice: Some(ToolChoice::None),
            parallel_tool_calls: Some(true),
            provider: None,
        };

        let json = serde_json::to_string(&request).unwrap();
        assert!(json.contains("\"tool_choice\":\"none\""));
        assert!(json.contains("\"parallel_tool_calls\":true"));
        assert!(json.contains("\"tools\""));
    }

    #[test]
    fn test_reasoning_config_for_all_models() {
        use super::Model;

        // All models should get high reasoning effort
        let speed = reasoning_config(Model::Speed).expect("Speed should have reasoning");
        assert_eq!(speed.effort, "high");
        assert!(speed.exclude);

        let balanced = reasoning_config(Model::Balanced).expect("Balanced should have reasoning");
        assert_eq!(balanced.effort, "high");

        let smart = reasoning_config(Model::Smart).expect("Smart should have reasoning");
        assert_eq!(smart.effort, "high");
    }
}
