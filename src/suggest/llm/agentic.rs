//! Agentic LLM client with tool-calling support.
//!
//! Enables models to explore codebases by calling tools (grep, read, ls)
//! in a loop until they have enough context to complete their task.

use super::client::{api_key, create_http_client, send_with_retry, REQUEST_TIMEOUT_SECS};
use super::models::Model;
use super::tools::{execute_tool, get_tool_definitions, ToolCall, ToolDefinition};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::Path;
use std::time::{Duration, Instant};

/// Overall timeout for the agentic loop to prevent indefinite hangs
const LOOP_TIMEOUT: Duration = Duration::from_secs(90);

/// Response from an agentic LLM call
#[derive(Debug)]
pub struct AgenticResponse {
    pub content: String,
    pub trace: AgenticTrace,
}

/// Trace metadata for troubleshooting agentic runs
#[derive(Debug, Clone, Default)]
pub struct AgenticTrace {
    pub iterations: usize,
    pub tool_calls: usize,
    pub tool_names: Vec<String>,
    pub forced_final: bool,
    pub formatting_pass: bool,
    pub response_healing_used: bool,
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
    plugins: Option<Vec<PluginConfig>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<ToolDefinition>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_choice: Option<ToolChoice>,
    #[serde(skip_serializing_if = "Option::is_none")]
    parallel_tool_calls: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    provider: Option<ProviderConfig>,
}

#[derive(Serialize, Clone)]
pub struct ResponseFormat {
    #[serde(rename = "type")]
    format_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    json_schema: Option<JsonSchemaConfig>,
}

#[derive(Serialize, Clone)]
pub struct JsonSchemaConfig {
    name: String,
    strict: bool,
    schema: serde_json::Value,
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "snake_case")]
enum ToolChoice {
    Auto,
}

#[derive(Serialize, Clone)]
struct PluginConfig {
    id: String,
}

#[derive(Serialize)]
struct ProviderConfig {
    allow_fallbacks: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    require_parameters: Option<bool>,
}

/// OpenRouter reasoning configuration for extended thinking
#[derive(Serialize)]
struct ReasoningConfig {
    effort: String,
    /// Exclude reasoning from the response (we only want the final answer)
    exclude: bool,
}

/// Create a ResponseFormat from a JSON schema value
/// Helper for callers that have a schema but need a ResponseFormat
pub fn schema_to_response_format(name: &str, schema: serde_json::Value) -> ResponseFormat {
    ResponseFormat {
        format_type: "json_schema".to_string(),
        json_schema: Some(JsonSchemaConfig {
            name: name.to_string(),
            strict: true,
            schema,
        }),
    }
}

/// JSON Schema for structured suggestion output
/// Enforces the LLM to return a valid array of suggestions
pub fn suggestion_schema() -> ResponseFormat {
    ResponseFormat {
        format_type: "json_schema".to_string(),
        json_schema: Some(JsonSchemaConfig {
            name: "suggestions".to_string(),
            strict: true,
            schema: serde_json::json!({
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "file": {
                            "type": "string",
                            "description": "Path to the file"
                        },
                        "additional_files": {
                            "type": "array",
                            "items": { "type": "string" },
                            "description": "Additional files affected"
                        },
                        "kind": {
                            "type": "string",
                            "enum": ["bugfix", "improvement", "optimization", "refactoring", "security", "reliability"],
                            "description": "Type of suggestion"
                        },
                        "priority": {
                            "type": "string",
                            "enum": ["high", "medium", "low"],
                            "description": "Priority level"
                        },
                        "confidence": {
                            "type": "string",
                            "enum": ["high", "medium", "low"],
                            "description": "Confidence level"
                        },
                        "summary": {
                            "type": "string",
                            "description": "Plain English user impact"
                        },
                        "detail": {
                            "type": "string",
                            "description": "Technical details and fix guidance"
                        },
                        "line": {
                            "type": "integer",
                            "description": "Line number in the file"
                        },
                        "evidence": {
                            "type": "string",
                            "description": "Code snippet proving the issue"
                        }
                    },
                    "required": ["file", "kind", "priority", "confidence", "summary", "detail"],
                    "additionalProperties": false
                }
            }),
        }),
    }
}

fn reasoning_config(model: Model) -> Option<ReasoningConfig> {
    model.reasoning_effort().map(|effort| ReasoningConfig {
        effort: effort.to_string(),
        exclude: true,
    })
}

fn response_healing_plugins() -> Vec<PluginConfig> {
    vec![PluginConfig {
        id: "response-healing".to_string(),
    }]
}

fn provider_config(require_parameters: bool) -> ProviderConfig {
    ProviderConfig {
        allow_fallbacks: true,
        require_parameters: if require_parameters { Some(true) } else { None },
    }
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
/// - Suggestions: 4 (focused exploration)
/// - Verification: 3 (code already provided)
/// - Review: 4 (diff already provided)
///
/// `final_response_format`: Optional structured output schema for the final response.
/// This is applied when the model finishes exploring and returns its final answer.
pub async fn call_llm_agentic(
    system: &str,
    user: &str,
    model: Model,
    repo_root: &Path,
    _json_mode: bool, // Deprecated: use final_response_format instead
    max_iterations: usize,
    final_response_format: Option<ResponseFormat>,
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
    let start = Instant::now();
    let mut tool_calls_total = 0;
    let mut tool_names = HashSet::new();
    let mut formatting_pass = false;
    let mut forced_final = false;
    let mut response_healing_used = false;

    loop {
        iteration += 1;

        // Check both iteration limit and wall-clock timeout
        if iteration > max_iterations || start.elapsed() > LOOP_TIMEOUT {
            // Force the model to respond with what it has
            forced_final = true;
            break;
        }
        // During exploration, don't use structured output (incompatible with tools for many models)
        // Structured output is only applied on the final forced response
        let request = ChatRequest {
            model: model.id().to_string(),
            messages: messages.clone(),
            max_tokens: model.max_tokens(),
            stream: false,
            response_format: None,
            reasoning: reasoning_config(model),
            plugins: None,
            tools: Some(tools.clone()),
            tool_choice: Some(ToolChoice::Auto),
            parallel_tool_calls: Some(true),
            provider: Some(provider_config(false)),
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
                tool_calls_total += tool_calls.len();
                for tc in tool_calls {
                    tool_names.insert(tc.function.name.clone());
                }
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
        // If we have structured output schema, the content should already be formatted
        // (structured output was requested but may not have been enforced during tool loop)
        let content = choice.message.content.clone().unwrap_or_default();

        // Validate we got actual content
        if content.trim().is_empty() {
            return Err(anyhow::anyhow!(
                "Model returned empty response. This may be due to rate limiting or an API issue. Try again."
            ));
        }

        // If structured output is requested but content doesn't look like valid JSON,
        // make one more call with structured output to format the response
        let needs_formatting = if final_response_format.is_some() {
            let trimmed = content.trim();
            // Check if content looks like valid JSON (starts with [ or {)
            !(trimmed.starts_with('[') || trimmed.starts_with('{'))
                || serde_json::from_str::<serde_json::Value>(trimmed).is_err()
        } else {
            false
        };

        if needs_formatting {
            formatting_pass = true;
            response_healing_used = true;
            // Content isn't valid JSON - ask for formatting with structured output
            messages.push(Message {
                role: "assistant".to_string(),
                content: Some(content.clone()),
                tool_calls: None,
                tool_call_id: None,
            });
            messages.push(Message {
                role: "user".to_string(),
                content: Some(
                    "Format your response as valid JSON matching the required schema.".to_string(),
                ),
                tool_calls: None,
                tool_call_id: None,
            });

            let format_request = ChatRequest {
                model: model.id().to_string(),
                messages: messages.clone(),
                max_tokens: model.max_tokens(),
                stream: false,
                response_format: final_response_format.clone(),
                reasoning: reasoning_config(model),
                plugins: Some(response_healing_plugins()),
                tools: None,
                tool_choice: None,
                parallel_tool_calls: None,
                provider: Some(provider_config(true)),
            };

            let text = send_with_retry(&client, &api_key, &format_request).await?;
            let parsed: ChatResponse = serde_json::from_str(&text)
                .map_err(|e| anyhow::anyhow!("Failed to parse format response: {}\n{}", e, text))?;

            let formatted = parsed
                .choices
                .first()
                .and_then(|c| c.message.content.clone())
                .unwrap_or(content);

            let mut tool_name_list: Vec<String> = tool_names.into_iter().collect();
            tool_name_list.sort();
            return Ok(AgenticResponse {
                content: formatted,
                trace: AgenticTrace {
                    iterations: iteration,
                    tool_calls: tool_calls_total,
                    tool_names: tool_name_list,
                    forced_final,
                    formatting_pass,
                    response_healing_used,
                },
            });
        }

        let mut tool_name_list: Vec<String> = tool_names.into_iter().collect();
        tool_name_list.sort();
        return Ok(AgenticResponse {
            content,
            trace: AgenticTrace {
                iterations: iteration,
                tool_calls: tool_calls_total,
                tool_names: tool_name_list,
                forced_final,
                formatting_pass,
                response_healing_used,
            },
        });
    }

    // If we broke out of loop (hit max iterations), make one final call WITHOUT tools
    // to force the model to respond with whatever it has
    let final_instruction = if final_response_format.is_some() {
        "You've gathered enough context. Now respond with valid JSON matching the required schema. No more tool calls."
    } else {
        "You've gathered enough context. Now respond based on what you've learned. No more tool calls."
    };
    messages.push(Message {
        role: "user".to_string(),
        content: Some(final_instruction.to_string()),
        tool_calls: None,
        tool_call_id: None,
    });

    // Final call: no tools at all, just ask for the response
    // Don't include tools or structured output to maximize compatibility
    let use_structured_output = final_response_format.is_some();
    let final_request = ChatRequest {
        model: model.id().to_string(),
        messages: messages.clone(),
        max_tokens: model.max_tokens(),
        stream: false,
        response_format: final_response_format,
        reasoning: reasoning_config(model),
        plugins: if use_structured_output {
            Some(response_healing_plugins())
        } else {
            None
        },
        tools: None,
        tool_choice: None,
        parallel_tool_calls: None,
        provider: Some(provider_config(use_structured_output)),
    };
    if use_structured_output {
        response_healing_used = true;
    }

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

    let mut tool_name_list: Vec<String> = tool_names.into_iter().collect();
    tool_name_list.sort();
    Ok(AgenticResponse {
        content,
        trace: AgenticTrace {
            iterations: iteration,
            tool_calls: tool_calls_total,
            tool_names: tool_name_list,
            forced_final,
            formatting_pass,
            response_healing_used,
        },
    })
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
            plugins: None,
            tools: Some(tools),
            tool_choice: Some(ToolChoice::Auto),
            parallel_tool_calls: Some(true),
            provider: None,
        };

        let json = serde_json::to_string(&request).unwrap();
        assert!(json.contains("\"tool_choice\":\"auto\""));
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
