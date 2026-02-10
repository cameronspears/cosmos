//! Agentic LLM client with tool-calling support.
//!
//! Enables models to explore codebases by calling tools (grep, read, ls)
//! in a loop until they have enough context to complete their task.

use super::client::{
    api_key, create_http_client, openrouter_user, send_with_retry, REQUEST_TIMEOUT_SECS,
};
use super::models::{merge_usage, Model, Usage};
use super::tools::{execute_tool, get_tool_definitions, ToolCall, ToolDefinition};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Semaphore;

/// Overall timeout for the agentic loop to prevent indefinite hangs
const LOOP_TIMEOUT: Duration = Duration::from_secs(90);
const MAX_PARALLEL_TOOL_EXECUTIONS: usize = 4;

/// Some providers occasionally return a response with no content and no tool calls.
/// Treat this as transient and retry a few times with backoff.
const EMPTY_RESPONSE_MAX_RETRIES: u32 = 3;

/// Response from an agentic LLM call
#[derive(Debug)]
pub struct AgenticResponse {
    pub content: String,
    pub usage: Option<Usage>,
}

async fn run_parallel_ordered_blocking<I, O, F>(
    inputs: Vec<I>,
    max_parallel: usize,
    f: Arc<F>,
) -> Vec<Option<O>>
where
    I: Send + 'static,
    O: Send + 'static,
    F: Fn(I) -> O + Send + Sync + 'static,
{
    if inputs.is_empty() || max_parallel == 0 {
        return Vec::new();
    }

    let semaphore = Arc::new(Semaphore::new(max_parallel));
    let mut handles = Vec::with_capacity(inputs.len());
    let mut results: Vec<Option<O>> = std::iter::repeat_with(|| None).take(inputs.len()).collect();

    for (idx, input) in inputs.into_iter().enumerate() {
        let permit = match semaphore.clone().acquire_owned().await {
            Ok(p) => p,
            Err(_) => continue,
        };
        let f = f.clone();
        handles.push((
            idx,
            tokio::task::spawn_blocking(move || {
                let _permit = permit;
                (f)(input)
            }),
        ));
    }

    for (idx, handle) in handles {
        results[idx] = handle.await.ok();
    }

    results
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
    #[serde(skip_serializing_if = "Option::is_none")]
    user: Option<String>,
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
struct ProviderThresholds {
    #[serde(skip_serializing_if = "Option::is_none")]
    p50: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    p75: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    p90: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    p99: Option<f64>,
}

#[derive(Serialize)]
struct ProviderConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    order: Option<Vec<String>>,
    allow_fallbacks: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    require_parameters: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    preferred_max_latency: Option<ProviderThresholds>,
    #[serde(skip_serializing_if = "Option::is_none")]
    preferred_min_throughput: Option<ProviderThresholds>,
    #[serde(skip_serializing_if = "Option::is_none")]
    quantizations: Option<Vec<String>>,
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

const GPT_OSS_PROVIDER_ORDER: [&str; 3] = ["cerebras/fp16", "crusoe/bf16", "deepinfra/turbo"];

fn provider_config(model: Model, require_parameters: bool) -> ProviderConfig {
    let require_parameters = if require_parameters { Some(true) } else { None };

    // For gpt-oss-120b (Speed tier), strongly prefer Cerebras fp16, with explicit fallbacks.
    if model == Model::Speed {
        return ProviderConfig {
            order: Some(
                GPT_OSS_PROVIDER_ORDER
                    .iter()
                    .map(|p| p.to_string())
                    .collect(),
            ),
            // Restrict routing to the explicit order only (Cerebras fp16 first, then
            // explicitly-approved fallbacks).
            allow_fallbacks: false,
            require_parameters,
            preferred_max_latency: None,
            preferred_min_throughput: None,
            quantizations: None,
        };
    }

    ProviderConfig {
        order: None,
        allow_fallbacks: true,
        require_parameters,
        preferred_max_latency: Some(ProviderThresholds {
            p50: None,
            p75: None,
            p90: Some(8.0),
            p99: None,
        }),
        preferred_min_throughput: Some(ProviderThresholds {
            p50: None,
            p75: None,
            p90: Some(15.0),
            p99: None,
        }),
        quantizations: None,
    }
}

#[derive(Deserialize)]
struct ChatResponse {
    choices: Vec<Choice>,
    usage: Option<Usage>,
}

#[derive(Deserialize)]
struct Choice {
    message: ResponseMessage,
}

#[derive(Deserialize)]
struct ResponseMessage {
    content: Option<String>,
    tool_calls: Option<Vec<ToolCallMessage>>,
    #[serde(default)]
    refusal: Option<String>,
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
    let mut total_usage: Option<Usage> = None;
    let mut empty_response_retries: u32 = 0;

    loop {
        iteration += 1;

        // Check both iteration limit and wall-clock timeout
        if iteration > max_iterations || start.elapsed() > LOOP_TIMEOUT {
            // Force the model to respond with what it has
            break;
        }
        // During exploration, don't use structured output (incompatible with tools for many models)
        // Structured output is only applied on the final forced response
        let request = ChatRequest {
            model: model.id().to_string(),
            messages: messages.clone(),
            user: openrouter_user(),
            max_tokens: model.max_tokens(),
            stream: false,
            response_format: None,
            reasoning: reasoning_config(model),
            plugins: None,
            tools: Some(tools.clone()),
            tool_choice: Some(ToolChoice::Auto),
            parallel_tool_calls: Some(true),
            provider: Some(provider_config(model, false)),
        };

        // Use shared retry helper - handles timeouts, rate limits, server errors
        let text = send_with_retry(&client, &api_key, &request).await?;

        let parsed: ChatResponse = serde_json::from_str(&text)
            .map_err(|e| anyhow::anyhow!("Failed to parse response: {}\n{}", e, text))?;
        total_usage = merge_usage(total_usage, parsed.usage.clone());

        let choice = parsed
            .choices
            .first()
            .ok_or_else(|| anyhow::anyhow!("No response from model"))?;

        // Refusals can come back with no content/tool calls.
        if let Some(refusal) = &choice.message.refusal {
            return Err(anyhow::anyhow!(
                "Request was refused: {}",
                refusal.chars().take(200).collect::<String>()
            ));
        }

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

                // Execute tool calls in parallel (bounded) and add results in stable order.
                let repo_root_buf = repo_root.to_path_buf();
                let inputs: Vec<(PathBuf, ToolCall)> = tool_calls
                    .iter()
                    .map(|tc| {
                        let tool_call_id = tc.id.clone();
                        let tool_call = ToolCall {
                            id: tool_call_id.clone(),
                            function: super::tools::FunctionCall {
                                name: tc.function.name.clone(),
                                arguments: tc.function.arguments.clone(),
                            },
                        };
                        (repo_root_buf.clone(), tool_call)
                    })
                    .collect();

                let results = run_parallel_ordered_blocking(
                    inputs,
                    MAX_PARALLEL_TOOL_EXECUTIONS,
                    Arc::new(|(repo_root, tool_call): (PathBuf, ToolCall)| {
                        execute_tool(&repo_root, &tool_call)
                    }),
                )
                .await;

                for (idx, tc) in tool_calls.iter().enumerate() {
                    let tc_id = tc.id.clone();
                    let result = results
                        .get(idx)
                        .and_then(|r| r.as_ref())
                        .cloned()
                        .unwrap_or_else(|| super::tools::ToolResult {
                            tool_call_id: tc_id.clone(),
                            content:
                                "Tool execution failed. Please try again. (no tool result returned)"
                                    .to_string(),
                        });
                    messages.push(Message {
                        role: "tool".to_string(),
                        content: Some(result.content),
                        tool_calls: None,
                        tool_call_id: Some(tc_id),
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
            if empty_response_retries < EMPTY_RESPONSE_MAX_RETRIES && start.elapsed() < LOOP_TIMEOUT
            {
                empty_response_retries += 1;
                // Don't count empty response against iteration budget.
                iteration = iteration.saturating_sub(1);
                let delay_ms = 250u64 * (1 << empty_response_retries);
                tokio::time::sleep(tokio::time::Duration::from_millis(delay_ms)).await;
                continue;
            }
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
                user: openrouter_user(),
                max_tokens: model.max_tokens(),
                stream: false,
                response_format: final_response_format.clone(),
                reasoning: reasoning_config(model),
                plugins: Some(response_healing_plugins()),
                tools: None,
                tool_choice: None,
                parallel_tool_calls: None,
                provider: Some(provider_config(model, true)),
            };

            let text = send_with_retry(&client, &api_key, &format_request).await?;
            let parsed: ChatResponse = serde_json::from_str(&text)
                .map_err(|e| anyhow::anyhow!("Failed to parse format response: {}\n{}", e, text))?;
            total_usage = merge_usage(total_usage, parsed.usage.clone());

            let choice = parsed
                .choices
                .first()
                .ok_or_else(|| anyhow::anyhow!("No response from model"))?;

            if let Some(refusal) = &choice.message.refusal {
                return Err(anyhow::anyhow!(
                    "Request was refused: {}",
                    refusal.chars().take(200).collect::<String>()
                ));
            }

            let formatted = choice.message.content.clone().unwrap_or(content);
            if formatted.trim().is_empty() {
                return Err(anyhow::anyhow!(
                    "Model returned empty response during formatting. This may be due to rate limiting or an API issue. Try again."
                ));
            }

            return Ok(AgenticResponse {
                content: formatted,
                usage: total_usage,
            });
        }

        return Ok(AgenticResponse {
            content,
            usage: total_usage,
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
        user: openrouter_user(),
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
        provider: Some(provider_config(model, use_structured_output)),
    };
    // Use shared retry helper for final request too, plus a few retries for empty content.
    let mut last_error: Option<anyhow::Error> = None;
    let mut parsed: Option<ChatResponse> = None;
    for attempt in 0..=EMPTY_RESPONSE_MAX_RETRIES {
        match send_with_retry(&client, &api_key, &final_request).await {
            Ok(text) => {
                let p: ChatResponse = serde_json::from_str(&text).map_err(|e| {
                    anyhow::anyhow!("Failed to parse final response: {}\n{}", e, text)
                })?;
                total_usage = merge_usage(total_usage, p.usage.clone());
                let choice = p
                    .choices
                    .first()
                    .ok_or_else(|| anyhow::anyhow!("No response from model"))?;
                if let Some(refusal) = &choice.message.refusal {
                    return Err(anyhow::anyhow!(
                        "Request was refused: {}",
                        refusal.chars().take(200).collect::<String>()
                    ));
                }
                let content = choice.message.content.clone().unwrap_or_default();
                if content.trim().is_empty() {
                    if attempt < EMPTY_RESPONSE_MAX_RETRIES && start.elapsed() < LOOP_TIMEOUT {
                        let delay_ms = 250u64 * (1 << attempt);
                        tokio::time::sleep(tokio::time::Duration::from_millis(delay_ms)).await;
                        continue;
                    }
                    return Err(anyhow::anyhow!(
                        "Model returned empty response after exploration. This may be due to rate limiting or an API issue. Try again."
                    ));
                }
                parsed = Some(p);
                break;
            }
            Err(e) => {
                last_error = Some(e);
                if attempt < EMPTY_RESPONSE_MAX_RETRIES && start.elapsed() < LOOP_TIMEOUT {
                    let delay_ms = 250u64 * (1 << attempt);
                    tokio::time::sleep(tokio::time::Duration::from_millis(delay_ms)).await;
                    continue;
                }
            }
        }
    }
    let parsed = match (parsed, last_error) {
        (Some(p), _) => p,
        (None, Some(e)) => return Err(e),
        (None, None) => {
            return Err(anyhow::anyhow!(
                "Model returned empty response after exploration. This may be due to rate limiting or an API issue. Try again."
            ))
        }
    };

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

    Ok(AgenticResponse {
        content,
        usage: total_usage,
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
            user: None,
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

        // Speed gets low, Balanced high, Smart xhigh
        let speed = reasoning_config(Model::Speed).expect("Speed should have reasoning");
        assert_eq!(speed.effort, "low");
        assert!(speed.exclude);

        let balanced = reasoning_config(Model::Balanced).expect("Balanced should have reasoning");
        assert_eq!(balanced.effort, "high");

        let smart = reasoning_config(Model::Smart).expect("Smart should have reasoning");
        assert_eq!(smart.effort, "xhigh");
    }

    #[tokio::test]
    async fn parallel_blocking_runner_preserves_input_order() {
        let inputs = vec![3u64, 1u64, 2u64];
        let results = run_parallel_ordered_blocking(
            inputs,
            2,
            Arc::new(|v| {
                // Invert delay so completion order differs from input order.
                std::thread::sleep(std::time::Duration::from_millis(50 * (4 - v)));
                v
            }),
        )
        .await;

        let out: Vec<u64> = results.into_iter().map(|r| r.unwrap()).collect();
        assert_eq!(out, vec![3, 1, 2]);
    }
}
