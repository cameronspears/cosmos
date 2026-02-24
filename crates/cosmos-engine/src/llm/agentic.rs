//! Agentic LLM client with tool-calling support.
//!
//! Enables models to explore codebases by calling tools (grep, read, ls)
//! in a loop until they have enough context to complete their task.

use super::client::{
    api_key, apply_backend_headers, backoff_secs, chat_completions_url, create_http_client,
    is_retryable_network_error, missing_api_key_message, model_id_for_backend, parse_retry_after,
    send_with_retry, supports_parallel_tool_calls_for_backend, MAX_RETRIES, REQUEST_TIMEOUT_SECS,
};
use super::models::{merge_usage, Model, Usage};
#[cfg(test)]
use super::tools::get_relace_search_tool_definitions;
use super::tools::{
    execute_tool, get_relace_search_tool_definitions_cerebras, get_tool_definitions,
    parse_report_back_payload, ReportBackExplanation, ReportBackPayload, ToolCall, ToolDefinition,
};
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Semaphore;

const MAX_PARALLEL_TOOL_EXECUTIONS: usize = 4;
/// Extra turns reserved to nudge the model to finalize with `report_back`.
const REPORT_BACK_GRACE_ROUNDS: usize = 2;

/// Some providers occasionally return a response with no content and no tool calls.
/// Treat this as transient and retry a few times with backoff.
const EMPTY_RESPONSE_MAX_RETRIES: u32 = 3;
const TEXT_INSTEAD_OF_REPORT_BACK_MAX_RETRIES: u32 = 2;
const INVALID_REPORT_BACK_PAYLOAD_MAX_RETRIES: u32 = 2;
const REPEATED_TOOL_ERROR_THRESHOLD: u32 = 3;
const TOOL_ERROR_LOOP_EXTRA_RETRIES: u32 = 2;
const FINALIZATION_NON_REPORT_BACK_MAX_RETRIES: u32 = 3;
const STREAM_REASONING_PRINT_MAX_CHARS: usize = 8_000;
/// Use low temperatures for reliable tool calling with Cerebras tool-use.
const TOOL_CALL_TEMPERATURE: f32 = 0.2;
/// Retry once colder when tool-call generation fails validation.
const TOOL_CALL_RETRY_TEMPERATURE: f32 = 0.0;

fn agent_loop_timeout() -> Option<Duration> {
    std::env::var("COSMOS_AGENT_LOOP_TIMEOUT_MS")
        .ok()
        .and_then(|value| value.trim().parse::<u64>().ok())
        .and_then(|value| {
            if value == 0 {
                None
            } else {
                Some(Duration::from_millis(value))
            }
        })
}

fn loop_timeout_exceeded(start: Instant, timeout: Option<Duration>) -> bool {
    timeout
        .map(|limit| start.elapsed() > limit)
        .unwrap_or(false)
}

fn within_loop_timeout(start: Instant, timeout: Option<Duration>) -> bool {
    timeout.map(|limit| start.elapsed() < limit).unwrap_or(true)
}

fn iteration_limit_exceeded(iteration: usize, max_iterations: usize) -> bool {
    max_iterations > 0 && iteration > max_iterations
}

/// Response from an agentic LLM call
#[derive(Debug)]
pub struct AgenticResponse {
    pub content: String,
    pub usage: Option<Usage>,
}

#[derive(Debug, Clone)]
pub struct AgenticReportBackResponse {
    pub report_back: ReportBackPayload,
    pub usage: Option<Usage>,
    pub trace: AgenticTrace,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum AgenticStreamKind {
    Reasoning,
    Tool,
    Notice,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgenticStreamEvent {
    pub kind: AgenticStreamKind,
    pub line: String,
}

pub type AgenticStreamSink = Arc<dyn Fn(AgenticStreamEvent) + Send + Sync>;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AgenticTrace {
    #[serde(default)]
    pub steps: Vec<AgenticTraceStep>,
    #[serde(default)]
    pub finalized_with_report_back: bool,
    #[serde(default)]
    pub termination_reason: Option<String>,
    #[serde(default)]
    pub repeated_tool_error_count: u32,
    #[serde(default)]
    pub invalid_report_back_count: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AgenticTraceStep {
    pub iteration: usize,
    pub finalization_round: bool,
    #[serde(default)]
    pub assistant_content_preview: Option<String>,
    #[serde(default)]
    pub reasoning_preview: Option<String>,
    #[serde(default)]
    pub tool_call_names: Vec<String>,
    pub report_back_called: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ToolErrorLoopAction {
    None,
    InjectCorrective,
    Fail,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
enum FinalizationNonReportBackAction {
    Retry,
    Fail,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InvalidReportBackAction {
    Retry,
    Fail,
}

#[derive(Debug, Default)]
struct ToolErrorLoopTracker {
    last_signature: Option<String>,
    consecutive: u32,
    max_consecutive: u32,
    threshold_triggered: bool,
    corrective_retries: u32,
}

impl ToolErrorLoopTracker {
    fn observe(&mut self, signature: Option<String>) -> ToolErrorLoopAction {
        let Some(signature) = signature else {
            self.last_signature = None;
            self.consecutive = 0;
            self.threshold_triggered = false;
            self.corrective_retries = 0;
            return ToolErrorLoopAction::None;
        };

        if self.last_signature.as_deref() == Some(signature.as_str()) {
            self.consecutive = self.consecutive.saturating_add(1);
        } else {
            self.last_signature = Some(signature);
            self.consecutive = 1;
            self.threshold_triggered = false;
            self.corrective_retries = 0;
        }

        self.max_consecutive = self.max_consecutive.max(self.consecutive);
        if self.consecutive < REPEATED_TOOL_ERROR_THRESHOLD {
            return ToolErrorLoopAction::None;
        }

        if !self.threshold_triggered {
            self.threshold_triggered = true;
            return ToolErrorLoopAction::InjectCorrective;
        }

        if self.corrective_retries < TOOL_ERROR_LOOP_EXTRA_RETRIES {
            self.corrective_retries = self.corrective_retries.saturating_add(1);
            return ToolErrorLoopAction::InjectCorrective;
        }

        ToolErrorLoopAction::Fail
    }

    fn max_consecutive(&self) -> u32 {
        self.max_consecutive
    }
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

#[derive(Serialize, Clone)]
struct ChatRequest {
    model: String,
    messages: Vec<Message>,
    #[serde(skip_serializing_if = "Option::is_none")]
    user: Option<String>,
    max_completion_tokens: u32,
    stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    response_format: Option<ResponseFormat>,
    #[serde(skip_serializing_if = "Option::is_none")]
    disable_reasoning: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    clear_thinking: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    plugins: Option<Vec<PluginConfig>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<ToolDefinition>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_choice: Option<ToolChoice>,
    #[serde(skip_serializing_if = "Option::is_none")]
    parallel_tool_calls: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    disable_tool_validation: Option<bool>,
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
#[serde(untagged)]
enum ToolChoice {
    Mode(ToolChoiceMode),
    #[allow(dead_code)]
    Function(ToolChoiceFunctionSelection),
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "snake_case")]
enum ToolChoiceMode {
    Auto,
}

#[derive(Serialize, Clone)]
struct ToolChoiceFunctionSelection {
    #[serde(rename = "type")]
    choice_type: &'static str,
    function: ToolChoiceFunctionName,
}

#[derive(Serialize, Clone)]
struct ToolChoiceFunctionName {
    name: &'static str,
}

#[derive(Serialize, Clone)]
struct PluginConfig {
    id: String,
}

#[derive(Serialize, Clone)]
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

#[derive(Serialize, Clone)]
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

#[derive(Clone, Default)]
struct ReasoningRequestFields {
    disable_reasoning: Option<bool>,
    clear_thinking: Option<bool>,
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

fn reasoning_config_for_model(_model: Model, include_output: bool) -> ReasoningRequestFields {
    ReasoningRequestFields {
        // Tool-calling loops run at lower temperature for stability. Disable reasoning by default
        // unless explicitly requested via env for trace visibility.
        disable_reasoning: Some(!include_output),
        // Preserve prior turn thinking in multi-turn coding/tool workflows.
        clear_thinking: Some(false),
    }
}

fn reasoning_config(model: Model) -> ReasoningRequestFields {
    reasoning_config_for_model(model, include_reasoning_output())
}

fn parallel_tool_calls_setting(model: Model, enabled: bool) -> Option<bool> {
    if supports_parallel_tool_calls_for_backend(model) {
        Some(enabled)
    } else {
        None
    }
}

fn env_flag(name: &str) -> bool {
    std::env::var(name)
        .ok()
        .map(|value| {
            matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
        .unwrap_or(false)
}

fn stream_reasoning_output_enabled() -> bool {
    if cfg!(test) {
        return false;
    }
    env_flag("COSMOS_STREAM_REASONING")
}

fn include_reasoning_output() -> bool {
    if cfg!(test) {
        return false;
    }
    env_flag("COSMOS_INCLUDE_REASONING") || stream_reasoning_output_enabled()
}

fn preview_text(value: Option<&str>, limit_chars: usize) -> Option<String> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| {
            let mut out = value.chars().take(limit_chars).collect::<String>();
            if value.chars().count() > limit_chars {
                out.push('…');
            }
            out
        })
}

fn preview_reasoning(value: Option<&serde_json::Value>, limit_chars: usize) -> Option<String> {
    let value = value?;
    match value {
        serde_json::Value::String(text) => preview_text(Some(text), limit_chars),
        other => {
            let serialized = other.to_string();
            preview_text(Some(&serialized), limit_chars)
        }
    }
}

#[derive(Debug, Default)]
struct StreamPrintState {
    printed_reasoning_chars: usize,
    reasoning_truncated: bool,
    last_tool_line: Option<String>,
}

fn format_streamed_reasoning_chunk(text: &str, state: &mut StreamPrintState) -> Option<String> {
    if text.is_empty() || state.reasoning_truncated {
        return None;
    }
    let remaining = STREAM_REASONING_PRINT_MAX_CHARS.saturating_sub(state.printed_reasoning_chars);
    if remaining == 0 {
        state.reasoning_truncated = true;
        return Some(format!(
            "\n[reasoning-stream] output truncated at {} chars\n",
            STREAM_REASONING_PRINT_MAX_CHARS
        ));
    }

    let char_count = text.chars().count();
    if char_count <= remaining {
        state.printed_reasoning_chars = state.printed_reasoning_chars.saturating_add(char_count);
        return Some(text.to_string());
    }

    let mut output = text.chars().take(remaining).collect::<String>();
    state.printed_reasoning_chars = STREAM_REASONING_PRINT_MAX_CHARS;
    state.reasoning_truncated = true;
    output.push_str(&format!(
        "\n[reasoning-stream] output truncated at {} chars\n",
        STREAM_REASONING_PRINT_MAX_CHARS
    ));
    Some(output)
}

fn normalize_tool_error_signature(tool_name: &str, content: &str) -> Option<String> {
    let first_line = content.lines().next().unwrap_or_default().trim();
    if first_line.is_empty() {
        return Some(format!("{}:empty_output", tool_name));
    }

    let lower = first_line.to_ascii_lowercase();
    let normalized = if lower.starts_with("invalid path")
        || lower.contains("path escapes repository")
        || lower.contains("absolute paths are not allowed")
        || lower.contains("parent traversal is not allowed")
    {
        "path_contract_violation"
    } else if lower.starts_with("invalid arguments") {
        "invalid_arguments"
    } else if lower.starts_with("file not found") {
        "file_not_found"
    } else if lower.starts_with("directory not found") {
        "directory_not_found"
    } else if lower.starts_with("search timed out") || lower.starts_with("command timed out") {
        "tool_timeout"
    } else if lower.starts_with("failed to execute command")
        || lower.starts_with("search failed")
        || lower.starts_with("failed to read file")
    {
        "tool_execution_failed"
    } else {
        return None;
    };

    Some(format!("{}:{}", tool_name, normalized))
}

fn build_tool_error_loop_corrective_prompt(signature: &str) -> String {
    format!(
        "You are repeating the same failing tool pattern ({signature}). \
Use repo-relative paths only (examples: `.` or `crates/cosmos-engine/src/llm/agentic.rs`). \
Do not use absolute filesystem paths. In finalization, call report_back exactly once with a valid JSON object payload."
    )
}

fn build_invalid_report_back_retry_prompt(first_error: &str, latest_error: &str) -> String {
    format!(
        "Your report_back payload is invalid.\n\
First validation error: {first_error}\n\
Latest validation error: {latest_error}\n\
Call report_back again with strict schema:\n\
1) explanation must be an object with role/findings/verified_findings\n\
2) files must be a map or list of {{path,ranges}}\n\
3) ranges must be [start,end] with start>=1 and end>=start\n\
Return only a valid report_back call."
    )
}

fn is_report_back_tool_name(name: &str) -> bool {
    matches!(name, "report_back" | "repo_browser.report_back")
}

fn infer_report_back_role(system: &str, user: &str) -> &'static str {
    let combined = format!("{}\n{}", system, user).to_ascii_lowercase();
    if combined.contains("security_reviewer") || combined.contains("security reviewer") {
        "security_reviewer"
    } else if combined.contains("bug_hunter") || combined.contains("bug hunter") {
        "bug_hunter"
    } else {
        "final_reviewer"
    }
}

fn empty_report_back_payload(role: &str) -> ReportBackPayload {
    ReportBackPayload {
        explanation: ReportBackExplanation {
            role: role.to_string(),
            findings: Vec::new(),
            verified_findings: Vec::new(),
        },
        files: std::collections::HashMap::new(),
    }
}

fn finalization_non_report_back_action(retries: u32) -> FinalizationNonReportBackAction {
    if retries < FINALIZATION_NON_REPORT_BACK_MAX_RETRIES {
        FinalizationNonReportBackAction::Retry
    } else {
        FinalizationNonReportBackAction::Fail
    }
}

fn invalid_report_back_action(retries: u32, within_time_budget: bool) -> InvalidReportBackAction {
    if retries < INVALID_REPORT_BACK_PAYLOAD_MAX_RETRIES && within_time_budget {
        InvalidReportBackAction::Retry
    } else {
        InvalidReportBackAction::Fail
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
    reasoning: Option<serde_json::Value>,
    #[serde(default)]
    refusal: Option<String>,
}

#[derive(Deserialize, Default)]
struct StreamChunk {
    #[serde(default)]
    choices: Vec<StreamChoice>,
    #[serde(default)]
    usage: Option<Usage>,
}

#[derive(Deserialize, Default)]
struct StreamChoice {
    #[serde(default)]
    delta: StreamDelta,
}

#[derive(Deserialize, Default)]
struct StreamDelta {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    reasoning: Option<serde_json::Value>,
    #[serde(default)]
    tool_calls: Option<Vec<StreamToolCallDelta>>,
    #[serde(default)]
    refusal: Option<String>,
}

#[derive(Deserialize, Default)]
struct StreamToolCallDelta {
    #[serde(default)]
    index: Option<usize>,
    #[serde(default)]
    id: Option<String>,
    #[serde(rename = "type")]
    #[serde(default)]
    call_type: Option<String>,
    #[serde(default)]
    function: Option<StreamFunctionCallDelta>,
}

#[derive(Deserialize, Default)]
struct StreamFunctionCallDelta {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    arguments: Option<String>,
}

fn empty_tool_call_message() -> ToolCallMessage {
    ToolCallMessage {
        id: String::new(),
        call_type: "function".to_string(),
        function: FunctionCallMessage {
            name: String::new(),
            arguments: String::new(),
        },
    }
}

fn reasoning_text(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(text) => text.clone(),
        serde_json::Value::Array(items) => items.iter().map(reasoning_text).collect::<String>(),
        serde_json::Value::Object(map) => {
            if let Some(text) = map.get("text").and_then(|value| value.as_str()) {
                text.to_string()
            } else {
                value.to_string()
            }
        }
        _ => value.to_string(),
    }
}

fn apply_tool_call_deltas(
    output: &mut Vec<ToolCallMessage>,
    deltas: Vec<StreamToolCallDelta>,
) -> Vec<String> {
    let mut names = Vec::new();
    for delta in deltas {
        let index = delta.index.unwrap_or(output.len());
        while output.len() <= index {
            output.push(empty_tool_call_message());
        }
        let call = &mut output[index];
        if let Some(id) = delta.id {
            if !id.is_empty() {
                call.id = id;
            }
        }
        if let Some(call_type) = delta.call_type {
            if !call_type.is_empty() {
                call.call_type = call_type;
            }
        }
        if let Some(function) = delta.function {
            if let Some(name) = function.name {
                if !name.is_empty() {
                    if call.function.name.is_empty() {
                        call.function.name = name.clone();
                    } else if call.function.name != name && !call.function.name.ends_with(&name) {
                        call.function.name.push_str(&name);
                    }
                    names.push(name);
                }
            }
            if let Some(arguments) = function.arguments {
                if !arguments.is_empty() {
                    call.function.arguments.push_str(&arguments);
                }
            }
        }
    }
    names
}

fn process_sse_payload(
    payload: &str,
    content: &mut String,
    reasoning: &mut String,
    tool_calls: &mut Vec<ToolCallMessage>,
    refusal: &mut Option<String>,
    usage: &mut Option<Usage>,
    stream_reasoning: bool,
    print_state: &mut StreamPrintState,
    stream_sink: Option<&AgenticStreamSink>,
) -> anyhow::Result<bool> {
    let payload = payload.trim();
    if payload.is_empty() {
        return Ok(false);
    }
    if payload == "[DONE]" {
        return Ok(true);
    }

    let chunk: StreamChunk = serde_json::from_str(payload)
        .map_err(|err| anyhow::anyhow!("Failed to parse stream chunk: {}", err))?;
    if let Some(chunk_usage) = chunk.usage {
        *usage = Some(chunk_usage);
    }
    for choice in chunk.choices {
        if let Some(refusal_text) = choice.delta.refusal {
            if !refusal_text.trim().is_empty() {
                *refusal = Some(refusal_text);
            }
        }
        if let Some(text) = choice.delta.content {
            content.push_str(&text);
        }
        if let Some(reasoning_value) = choice.delta.reasoning {
            let text = reasoning_text(&reasoning_value);
            if !text.is_empty() {
                reasoning.push_str(&text);
                if stream_reasoning {
                    if let Some(output) = format_streamed_reasoning_chunk(&text, print_state) {
                        if let Some(sink) = stream_sink {
                            for segment in output.split('\n') {
                                if segment.is_empty() {
                                    continue;
                                }
                                sink(AgenticStreamEvent {
                                    kind: AgenticStreamKind::Reasoning,
                                    line: segment.to_string(),
                                });
                            }
                        } else {
                            eprint!("{}", output);
                            let _ = std::io::stderr().flush();
                        }
                    }
                }
            }
        }
        if let Some(delta_calls) = choice.delta.tool_calls {
            let names = apply_tool_call_deltas(tool_calls, delta_calls);
            if stream_reasoning && !names.is_empty() {
                let tool_line = names.join(", ");
                if print_state.last_tool_line.as_deref() != Some(tool_line.as_str()) {
                    if let Some(sink) = stream_sink {
                        sink(AgenticStreamEvent {
                            kind: AgenticStreamKind::Tool,
                            line: tool_line.clone(),
                        });
                    } else {
                        eprintln!("\n[tool] {}", tool_line);
                    }
                    print_state.last_tool_line = Some(tool_line);
                }
            }
        }
    }
    Ok(false)
}

async fn send_streaming_chat_request(
    client: &reqwest::Client,
    api_key: &str,
    request: &ChatRequest,
    stream_sink: Option<&AgenticStreamSink>,
) -> anyhow::Result<ChatResponse> {
    let mut retry_count = 0;

    loop {
        let request_builder = client.post(chat_completions_url()).json(request);
        let response = match apply_backend_headers(request_builder, api_key).send().await {
            Ok(response) => response,
            Err(err) => {
                if is_retryable_network_error(&err) && retry_count < MAX_RETRIES {
                    retry_count += 1;
                    let retry_after = backoff_secs(retry_count);
                    tokio::time::sleep(Duration::from_secs(retry_after)).await;
                    continue;
                }
                return Err(err.into());
            }
        };

        let status = response.status();
        let retry_after_hint = parse_retry_after_header(response.headers());
        if !status.is_success() {
            let text = response.text().await.unwrap_or_default();
            if status.as_u16() == 429 && retry_count < MAX_RETRIES {
                retry_count += 1;
                let retry_after = retry_after_hint
                    .or_else(|| parse_retry_after(&text))
                    .unwrap_or_else(|| backoff_secs(retry_count));
                tokio::time::sleep(Duration::from_secs(retry_after)).await;
                continue;
            }
            if status.is_server_error() && retry_count < MAX_RETRIES {
                retry_count += 1;
                let retry_after = backoff_secs(retry_count);
                tokio::time::sleep(Duration::from_secs(retry_after)).await;
                continue;
            }
            return Err(anyhow::anyhow!("Streaming API error {}: {}", status, text));
        }

        return consume_streaming_chat_response(response, stream_sink).await;
    }
}

fn parse_retry_after_header(headers: &reqwest::header::HeaderMap) -> Option<u64> {
    headers
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().parse::<u64>().ok())
        .filter(|secs| *secs > 0 && *secs < 300)
}

async fn consume_streaming_chat_response(
    response: reqwest::Response,
    stream_sink: Option<&AgenticStreamSink>,
) -> anyhow::Result<ChatResponse> {
    let mut stream = response.bytes_stream();
    let mut buffer = String::new();
    let mut event_data = String::new();

    let mut content = String::new();
    let mut reasoning = String::new();
    let mut tool_calls = Vec::new();
    let mut refusal = None;
    let mut usage = None;
    let stream_reasoning = stream_reasoning_output_enabled() || stream_sink.is_some();
    let mut printed_header = false;
    let mut print_state = StreamPrintState::default();

    while let Some(chunk) = stream.next().await {
        let bytes = chunk.map_err(|err| anyhow::anyhow!("Stream read failed: {}", err))?;
        buffer.push_str(&String::from_utf8_lossy(&bytes));

        while let Some(pos) = buffer.find('\n') {
            let mut line = buffer[..pos].to_string();
            buffer.drain(..=pos);
            if line.ends_with('\r') {
                line.pop();
            }
            let line = line.trim_end();

            if line.is_empty() {
                if !event_data.is_empty() {
                    if stream_reasoning && !printed_header {
                        if let Some(sink) = stream_sink {
                            sink(AgenticStreamEvent {
                                kind: AgenticStreamKind::Notice,
                                line: "reasoning-stream".to_string(),
                            });
                        } else {
                            eprintln!("\n[reasoning-stream]");
                        }
                        printed_header = true;
                    }
                    if process_sse_payload(
                        &event_data,
                        &mut content,
                        &mut reasoning,
                        &mut tool_calls,
                        &mut refusal,
                        &mut usage,
                        stream_reasoning,
                        &mut print_state,
                        stream_sink,
                    )? {
                        if stream_reasoning && printed_header && stream_sink.is_none() {
                            eprintln!();
                        }
                        return Ok(ChatResponse {
                            choices: vec![Choice {
                                message: ResponseMessage {
                                    content: if content.is_empty() {
                                        None
                                    } else {
                                        Some(content)
                                    },
                                    tool_calls: if tool_calls.is_empty() {
                                        None
                                    } else {
                                        Some(tool_calls)
                                    },
                                    reasoning: if reasoning.is_empty() {
                                        None
                                    } else {
                                        Some(serde_json::Value::String(reasoning))
                                    },
                                    refusal,
                                },
                            }],
                            usage,
                        });
                    }
                    event_data.clear();
                }
                continue;
            }

            if let Some(data) = line.strip_prefix("data:") {
                let data = data.trim_start();
                if !event_data.is_empty() {
                    event_data.push('\n');
                }
                event_data.push_str(data);
            }
        }
    }

    if !event_data.trim().is_empty() {
        let _ = process_sse_payload(
            &event_data,
            &mut content,
            &mut reasoning,
            &mut tool_calls,
            &mut refusal,
            &mut usage,
            stream_reasoning,
            &mut print_state,
            stream_sink,
        )?;
    }
    if stream_reasoning && printed_header && stream_sink.is_none() {
        eprintln!();
    }

    Ok(ChatResponse {
        choices: vec![Choice {
            message: ResponseMessage {
                content: if content.is_empty() {
                    None
                } else {
                    Some(content)
                },
                tool_calls: if tool_calls.is_empty() {
                    None
                } else {
                    Some(tool_calls)
                },
                reasoning: if reasoning.is_empty() {
                    None
                } else {
                    Some(serde_json::Value::String(reasoning))
                },
                refusal,
            },
        }],
        usage,
    })
}

async fn send_report_back_text_with_speed_fallback(
    client: &reqwest::Client,
    api_key: &str,
    request: &mut ChatRequest,
) -> anyhow::Result<String> {
    match send_with_retry(client, api_key, request).await {
        Ok(text) => Ok(text),
        Err(err) => {
            if is_tool_call_validation_error(&err)
                && request.temperature.unwrap_or(TOOL_CALL_TEMPERATURE)
                    > (TOOL_CALL_RETRY_TEMPERATURE + f32::EPSILON)
            {
                request.temperature = Some(TOOL_CALL_RETRY_TEMPERATURE);
                send_with_retry(client, api_key, request).await
            } else {
                Err(err)
            }
        }
    }
}

fn is_tool_call_validation_error(err: &anyhow::Error) -> bool {
    let text = err.to_string().to_ascii_lowercase();
    text.contains("tool call validation failed")
        || (text.contains("attempted to call tool") && text.contains("not in request"))
        || text.contains("failed_generation")
}

async fn maybe_format_agentic_content(
    client: &reqwest::Client,
    api_key: &str,
    model: Model,
    user_prompt: &str,
    draft_content: String,
    final_response_format: Option<ResponseFormat>,
    usage: Option<Usage>,
) -> anyhow::Result<AgenticResponse> {
    let Some(response_format) = final_response_format else {
        return Ok(AgenticResponse {
            content: draft_content,
            usage,
        });
    };

    let reasoning = reasoning_config(model);
    let format_request = ChatRequest {
        model: model_id_for_backend(model),
        messages: vec![
            Message {
                role: "system".to_string(),
                content: Some(
                    "Convert the draft answer into the required JSON schema. Return only valid JSON matching the schema exactly. Do not call tools and do not include markdown fences.".to_string(),
                ),
                tool_calls: None,
                tool_call_id: None,
            },
            Message {
                role: "user".to_string(),
                content: Some(format!(
                    "Original task:\n{}\n\nDraft answer:\n{}\n\nReturn only valid JSON.",
                    user_prompt, draft_content
                )),
                tool_calls: None,
                tool_call_id: None,
            },
        ],
        user: None,
        max_completion_tokens: model.max_tokens(),
        stream: false,
        temperature: Some(TOOL_CALL_RETRY_TEMPERATURE),
        response_format: Some(response_format),
        disable_reasoning: reasoning.disable_reasoning,
        clear_thinking: reasoning.clear_thinking,
        plugins: None,
        tools: None,
        tool_choice: None,
        parallel_tool_calls: None,
        disable_tool_validation: None,
        provider: None,
    };

    let text = send_with_retry(client, api_key, &format_request).await?;
    let parsed: ChatResponse = serde_json::from_str(&text)
        .map_err(|e| anyhow::anyhow!("Failed to parse format response: {}\n{}", e, text))?;
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

    let formatted = choice.message.content.clone().unwrap_or_default();
    if formatted.trim().is_empty() {
        return Err(anyhow::anyhow!(
            "Model returned empty response during structured formatting. This may be due to rate limiting or an API issue. Try again."
        ));
    }

    Ok(AgenticResponse {
        content: formatted,
        usage: merge_usage(usage, parsed.usage),
    })
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
    let api_key = api_key().ok_or_else(|| anyhow::anyhow!(missing_api_key_message()))?;

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
    let loop_timeout = agent_loop_timeout();
    let mut total_usage: Option<Usage> = None;
    let mut empty_response_retries: u32 = 0;

    loop {
        iteration += 1;

        // Check both iteration limit and wall-clock timeout
        if iteration_limit_exceeded(iteration, max_iterations)
            || loop_timeout_exceeded(start, loop_timeout)
        {
            // Force the model to respond with what it has
            break;
        }
        // During exploration, don't use structured output (incompatible with tools for many models)
        // Structured output is only applied on the final forced response
        let reasoning = reasoning_config(model);
        let request = ChatRequest {
            model: model_id_for_backend(model),
            messages: messages.clone(),
            user: None,
            max_completion_tokens: model.max_tokens(),
            stream: false,
            temperature: Some(TOOL_CALL_TEMPERATURE),
            response_format: None,
            disable_reasoning: reasoning.disable_reasoning,
            clear_thinking: reasoning.clear_thinking,
            plugins: None,
            tools: Some(tools.clone()),
            tool_choice: Some(ToolChoice::Mode(ToolChoiceMode::Auto)),
            parallel_tool_calls: parallel_tool_calls_setting(model, true),
            disable_tool_validation: None,
            provider: None,
        };
        let mut request = request;

        // Use shared retry helper - handles timeouts, rate limits, server errors
        let text =
            send_report_back_text_with_speed_fallback(&client, &api_key, &mut request).await?;

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
        let content = choice.message.content.clone().unwrap_or_default();

        // Validate we got actual content
        if content.trim().is_empty() {
            if empty_response_retries < EMPTY_RESPONSE_MAX_RETRIES
                && within_loop_timeout(start, loop_timeout)
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

        return maybe_format_agentic_content(
            &client,
            &api_key,
            model,
            user,
            content,
            final_response_format.clone(),
            total_usage,
        )
        .await;
    }

    // If we broke out of loop (hit max iterations), make one final call WITHOUT tools
    // to force the model to respond with whatever it has
    let final_instruction =
        "You've gathered enough context. Now respond based on what you've learned. No more tool calls.";
    messages.push(Message {
        role: "user".to_string(),
        content: Some(final_instruction.to_string()),
        tool_calls: None,
        tool_call_id: None,
    });

    // Final call: no tools at all, just ask for the response
    let reasoning = reasoning_config(model);
    let final_request = ChatRequest {
        model: model_id_for_backend(model),
        messages: messages.clone(),
        user: None,
        max_completion_tokens: model.max_tokens(),
        stream: false,
        temperature: None,
        response_format: None,
        disable_reasoning: reasoning.disable_reasoning,
        clear_thinking: reasoning.clear_thinking,
        plugins: None,
        tools: None,
        tool_choice: None,
        parallel_tool_calls: None,
        disable_tool_validation: None,
        provider: None,
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
                    if attempt < EMPTY_RESPONSE_MAX_RETRIES
                        && within_loop_timeout(start, loop_timeout)
                    {
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
                if attempt < EMPTY_RESPONSE_MAX_RETRIES && within_loop_timeout(start, loop_timeout)
                {
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

    maybe_format_agentic_content(
        &client,
        &api_key,
        model,
        user,
        content,
        final_response_format,
        total_usage,
    )
    .await
}

/// Agentic call variant that only succeeds when the model completes via `report_back`.
///
/// This is used by suggestion generation workflows that require strict tool-based completion.
pub async fn call_llm_agentic_report_back_only(
    system: &str,
    user: &str,
    model: Model,
    repo_root: &Path,
    max_iterations: usize,
    stream_sink: Option<AgenticStreamSink>,
) -> anyhow::Result<AgenticReportBackResponse> {
    let api_key = api_key().ok_or_else(|| anyhow::anyhow!(missing_api_key_message()))?;
    let client = create_http_client(REQUEST_TIMEOUT_SECS)?;
    let tools = get_relace_search_tool_definitions_cerebras();

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

    let mut iteration = 0usize;
    let start = Instant::now();
    let loop_timeout = agent_loop_timeout();
    let fallback_role = infer_report_back_role(system, user);
    let mut total_usage: Option<Usage> = None;
    let mut empty_response_retries: u32 = 0;
    let mut text_response_retries: u32 = 0;
    let mut invalid_report_back_retries: u32 = 0;
    let mut first_invalid_report_back_error: Option<String> = None;
    let mut finalization_non_report_back_retries: u32 = 0;
    let mut tool_error_loop_tracker = ToolErrorLoopTracker::default();
    let max_total_iterations = if max_iterations == 0 {
        0
    } else {
        max_iterations.saturating_add(REPORT_BACK_GRACE_ROUNDS)
    };
    let mut forced_report_back_mode = false;
    let mut trace = AgenticTrace::default();
    // When a UI sink is attached, stream output by default so the suggestion pane can render
    // live progress during normal `cargo run` usage.
    let mut stream_reasoning = stream_sink.is_some() || stream_reasoning_output_enabled();
    let include_reasoning = include_reasoning_output();
    let mut stream_fallback_logged = false;

    loop {
        iteration += 1;
        if iteration_limit_exceeded(iteration, max_total_iterations)
            || loop_timeout_exceeded(start, loop_timeout)
        {
            if forced_report_back_mode {
                trace.termination_reason = Some("timeout_fallback_empty_report_back".to_string());
                trace.repeated_tool_error_count = trace
                    .repeated_tool_error_count
                    .max(tool_error_loop_tracker.max_consecutive());
                trace.invalid_report_back_count = invalid_report_back_retries;
                return Ok(AgenticReportBackResponse {
                    report_back: empty_report_back_payload(fallback_role),
                    usage: total_usage,
                    trace,
                });
            }
            return Err(anyhow::anyhow!(
                "termination_reason=timeout Agent did not call report_back within iteration/time budget."
            ));
        }

        let near_timeout = loop_timeout
            .map(|timeout| {
                let elapsed_ms = start.elapsed().as_millis();
                let timeout_ms = timeout.as_millis().max(1);
                elapsed_ms.saturating_mul(100) >= timeout_ms.saturating_mul(75)
            })
            .unwrap_or(false);
        // Reserve the configured grace rounds for finalization *after* the normal
        // exploration budget has been used. The previous threshold entered
        // finalization too early for small iteration budgets (for example, budget=2).
        let finalization_round =
            near_timeout || iteration_limit_exceeded(iteration, max_iterations);
        if finalization_round && !forced_report_back_mode {
            forced_report_back_mode = true;
            messages.push(Message {
                role: "user".to_string(),
                content: Some(
                    "Time budget is nearly exhausted. Stop exploring and call report_back now. If you have no verified findings, send findings: [] and files: []."
                        .to_string(),
                ),
                tool_calls: None,
                tool_call_id: None,
            });
        }
        let tool_choice = if finalization_round {
            Some(ToolChoice::Function(ToolChoiceFunctionSelection {
                choice_type: "function",
                function: ToolChoiceFunctionName {
                    name: "report_back",
                },
            }))
        } else {
            Some(ToolChoice::Mode(ToolChoiceMode::Auto))
        };
        let reasoning = reasoning_config_for_model(model, include_reasoning);
        let request = ChatRequest {
            model: model_id_for_backend(model),
            messages: messages.clone(),
            user: None,
            max_completion_tokens: model.max_tokens(),
            stream: stream_reasoning,
            temperature: Some(TOOL_CALL_TEMPERATURE),
            response_format: None,
            disable_reasoning: reasoning.disable_reasoning,
            clear_thinking: reasoning.clear_thinking,
            plugins: None,
            tools: Some(tools.clone()),
            tool_choice,
            parallel_tool_calls: parallel_tool_calls_setting(model, !finalization_round),
            disable_tool_validation: None,
            provider: None,
        };
        let mut request = request;
        let parsed: ChatResponse = if stream_reasoning {
            match send_streaming_chat_request(&client, &api_key, &request, stream_sink.as_ref())
                .await
            {
                Ok(parsed) => parsed,
                Err(stream_err) => {
                    if !stream_fallback_logged {
                        let line = format!("fallback to buffered mode: {}", stream_err);
                        if let Some(sink) = stream_sink.as_ref() {
                            sink(AgenticStreamEvent {
                                kind: AgenticStreamKind::Notice,
                                line,
                            });
                        } else {
                            eprintln!("\n[reasoning-stream] {}", line);
                        }
                        stream_fallback_logged = true;
                    }
                    stream_reasoning = false;
                    request.stream = false;
                    let reasoning = reasoning_config_for_model(model, include_reasoning);
                    request.disable_reasoning = reasoning.disable_reasoning;
                    request.clear_thinking = reasoning.clear_thinking;
                    let text =
                        send_report_back_text_with_speed_fallback(&client, &api_key, &mut request)
                            .await?;
                    serde_json::from_str(&text)
                        .map_err(|e| anyhow::anyhow!("Failed to parse response: {}\n{}", e, text))?
                }
            }
        } else {
            let text =
                send_report_back_text_with_speed_fallback(&client, &api_key, &mut request).await?;
            serde_json::from_str(&text)
                .map_err(|e| anyhow::anyhow!("Failed to parse response: {}\n{}", e, text))?
        };
        total_usage = merge_usage(total_usage, parsed.usage.clone());

        let choice = parsed
            .choices
            .first()
            .ok_or_else(|| anyhow::anyhow!("No response from model"))?;

        if let Some(refusal) = &choice.message.refusal {
            return Err(anyhow::anyhow!(
                "termination_reason=refusal Request was refused: {}",
                refusal.chars().take(200).collect::<String>()
            ));
        }

        let assistant_content_preview = preview_text(choice.message.content.as_deref(), 180);
        let reasoning_preview = preview_reasoning(choice.message.reasoning.as_ref(), 180);

        if let Some(tool_calls) = &choice.message.tool_calls {
            if !tool_calls.is_empty() {
                let tool_call_names = tool_calls
                    .iter()
                    .map(|tool_call| tool_call.function.name.clone())
                    .collect::<Vec<_>>();
                let report_back_called = tool_calls
                    .iter()
                    .any(|tool_call| is_report_back_tool_name(&tool_call.function.name));
                trace.steps.push(AgenticTraceStep {
                    iteration,
                    finalization_round,
                    assistant_content_preview: assistant_content_preview.clone(),
                    reasoning_preview: reasoning_preview.clone(),
                    tool_call_names,
                    report_back_called,
                });

                messages.push(Message {
                    role: "assistant".to_string(),
                    content: choice.message.content.clone(),
                    tool_calls: Some(tool_calls.clone()),
                    tool_call_id: None,
                });

                if let Some(report_call) = tool_calls
                    .iter()
                    .find(|tool_call| is_report_back_tool_name(&tool_call.function.name))
                {
                    match parse_report_back_payload(&report_call.function.arguments) {
                        Ok(payload) => {
                            trace.finalized_with_report_back = true;
                            trace.termination_reason = Some("report_back_ok".to_string());
                            trace.repeated_tool_error_count = trace
                                .repeated_tool_error_count
                                .max(tool_error_loop_tracker.max_consecutive());
                            trace.invalid_report_back_count = invalid_report_back_retries;
                            return Ok(AgenticReportBackResponse {
                                report_back: payload,
                                usage: total_usage,
                                trace,
                            });
                        }
                        Err(err) => {
                            let action = invalid_report_back_action(
                                invalid_report_back_retries,
                                within_loop_timeout(start, loop_timeout),
                            );
                            invalid_report_back_retries =
                                invalid_report_back_retries.saturating_add(1);
                            trace.invalid_report_back_count = invalid_report_back_retries;
                            if first_invalid_report_back_error.is_none() {
                                first_invalid_report_back_error = Some(err.clone());
                            }
                            match action {
                                InvalidReportBackAction::Retry => {
                                    let first_error = first_invalid_report_back_error
                                        .as_deref()
                                        .unwrap_or(err.as_str());
                                    messages.push(Message {
                                        role: "user".to_string(),
                                        content: Some(build_invalid_report_back_retry_prompt(
                                            first_error,
                                            &err,
                                        )),
                                        tool_calls: None,
                                        tool_call_id: None,
                                    });
                                    continue;
                                }
                                InvalidReportBackAction::Fail => {}
                            }
                            if finalization_round {
                                trace.termination_reason =
                                    Some("invalid_report_back_fallback_empty".to_string());
                                trace.repeated_tool_error_count = trace
                                    .repeated_tool_error_count
                                    .max(tool_error_loop_tracker.max_consecutive());
                                return Ok(AgenticReportBackResponse {
                                    report_back: empty_report_back_payload(fallback_role),
                                    usage: total_usage,
                                    trace,
                                });
                            }
                            return Err(anyhow::anyhow!(
                                "termination_reason=invalid_report_back_exhausted invalid_report_back_count={} Invalid report_back payload: {}",
                                invalid_report_back_retries,
                                err
                            ));
                        }
                    }
                }

                if finalization_round {
                    let action =
                        finalization_non_report_back_action(finalization_non_report_back_retries);
                    finalization_non_report_back_retries =
                        finalization_non_report_back_retries.saturating_add(1);
                    match action {
                        FinalizationNonReportBackAction::Retry => {
                            messages.push(Message {
                                role: "user".to_string(),
                                content: Some(
                                    "Finalization mode is active. You must call report_back now. Do not call other tools. If no verified findings exist, send findings: [] and files: []."
                                        .to_string(),
                                ),
                                tool_calls: None,
                                tool_call_id: None,
                            });
                            continue;
                        }
                        FinalizationNonReportBackAction::Fail => {
                            trace.termination_reason =
                                Some("finalization_non_report_back_fallback_empty".to_string());
                            trace.repeated_tool_error_count = trace
                                .repeated_tool_error_count
                                .max(tool_error_loop_tracker.max_consecutive());
                            trace.invalid_report_back_count = invalid_report_back_retries;
                            return Ok(AgenticReportBackResponse {
                                report_back: empty_report_back_payload(fallback_role),
                                usage: total_usage,
                                trace,
                            });
                        }
                    }
                }

                let repo_root_buf = repo_root.to_path_buf();
                let inputs: Vec<(PathBuf, ToolCall)> = tool_calls
                    .iter()
                    .map(|tc| {
                        let tool_call_id = tc.id.clone();
                        let tool_call = ToolCall {
                            id: tool_call_id,
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

                let mut round_error_signatures = Vec::new();
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
                    if let Some(signature) =
                        normalize_tool_error_signature(&tc.function.name, &result.content)
                    {
                        round_error_signatures.push(signature);
                    }
                    messages.push(Message {
                        role: "tool".to_string(),
                        content: Some(result.content),
                        tool_calls: None,
                        tool_call_id: Some(tc_id),
                    });
                }

                let round_signature = if round_error_signatures.is_empty() {
                    None
                } else {
                    Some(round_error_signatures.join("|"))
                };
                let loop_action = tool_error_loop_tracker.observe(round_signature.clone());
                trace.repeated_tool_error_count = trace
                    .repeated_tool_error_count
                    .max(tool_error_loop_tracker.max_consecutive());

                match loop_action {
                    ToolErrorLoopAction::None => {}
                    ToolErrorLoopAction::InjectCorrective => {
                        if let Some(signature) = round_signature {
                            iteration = iteration.saturating_sub(1);
                            messages.push(Message {
                                role: "user".to_string(),
                                content: Some(build_tool_error_loop_corrective_prompt(&signature)),
                                tool_calls: None,
                                tool_call_id: None,
                            });
                        }
                    }
                    ToolErrorLoopAction::Fail => {
                        return Err(anyhow::anyhow!(
                            "termination_reason=tool_error_loop repeated_tool_errors={} invalid_report_back_count={} Agent repeated the same failing tool pattern.",
                            tool_error_loop_tracker.max_consecutive(),
                            invalid_report_back_retries
                        ));
                    }
                }
                continue;
            }
        }

        let _ = tool_error_loop_tracker.observe(None);
        let content = choice.message.content.clone().unwrap_or_default();
        trace.steps.push(AgenticTraceStep {
            iteration,
            finalization_round,
            assistant_content_preview: assistant_content_preview.clone(),
            reasoning_preview: reasoning_preview.clone(),
            tool_call_names: Vec::new(),
            report_back_called: false,
        });
        if content.trim().is_empty() {
            if empty_response_retries < EMPTY_RESPONSE_MAX_RETRIES
                && within_loop_timeout(start, loop_timeout)
            {
                empty_response_retries += 1;
                let delay_ms = 250u64 * (1 << empty_response_retries);
                if finalization_round {
                    messages.push(Message {
                        role: "user".to_string(),
                        content: Some(
                            "Finalization mode is active. The previous response was empty. Call report_back now. If no verified findings exist, send findings: [] and files: []."
                                .to_string(),
                        ),
                        tool_calls: None,
                        tool_call_id: None,
                    });
                }
                tokio::time::sleep(tokio::time::Duration::from_millis(delay_ms)).await;
                continue;
            }
            if finalization_round {
                trace.termination_reason = Some("empty_response_fallback_empty".to_string());
                trace.repeated_tool_error_count = trace
                    .repeated_tool_error_count
                    .max(tool_error_loop_tracker.max_consecutive());
                trace.invalid_report_back_count = invalid_report_back_retries;
                return Ok(AgenticReportBackResponse {
                    report_back: empty_report_back_payload(fallback_role),
                    usage: total_usage,
                    trace,
                });
            }
            return Err(anyhow::anyhow!(
                "termination_reason=empty_response Model returned empty response and did not call report_back."
            ));
        }
        if !finalization_round
            && text_response_retries < TEXT_INSTEAD_OF_REPORT_BACK_MAX_RETRIES
            && within_loop_timeout(start, loop_timeout)
        {
            text_response_retries += 1;
            messages.push(Message {
                role: "assistant".to_string(),
                content: Some(content),
                tool_calls: None,
                tool_call_id: None,
            });
            messages.push(Message {
                role: "user".to_string(),
                content: Some(if finalization_round {
                    "Finalization mode is active. Call report_back exactly once now. Do not return plain text."
                        .to_string()
                } else {
                    "Continue exploring with tools if needed, then finish by calling report_back exactly once. Do not return plain text."
                        .to_string()
                }),
                tool_calls: None,
                tool_call_id: None,
            });
            continue;
        }

        if finalization_round {
            if let Ok(payload) = parse_report_back_payload(content.trim()) {
                trace.finalized_with_report_back = true;
                trace.termination_reason = Some("text_report_back_ok".to_string());
                trace.repeated_tool_error_count = trace
                    .repeated_tool_error_count
                    .max(tool_error_loop_tracker.max_consecutive());
                trace.invalid_report_back_count = invalid_report_back_retries;
                return Ok(AgenticReportBackResponse {
                    report_back: payload,
                    usage: total_usage,
                    trace,
                });
            }
            trace.termination_reason = Some("text_fallback_empty".to_string());
            trace.repeated_tool_error_count = trace
                .repeated_tool_error_count
                .max(tool_error_loop_tracker.max_consecutive());
            trace.invalid_report_back_count = invalid_report_back_retries;
            return Ok(AgenticReportBackResponse {
                report_back: empty_report_back_payload(fallback_role),
                usage: total_usage,
                trace,
            });
        }

        return Err(anyhow::anyhow!(
            "termination_reason=text_instead_of_report_back Model returned text instead of calling report_back. Last content: {}",
            content.chars().take(200).collect::<String>()
        ));
    }
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
            max_completion_tokens: 128,
            stream: false,
            temperature: Some(TOOL_CALL_TEMPERATURE),
            response_format: None,
            disable_reasoning: None,
            clear_thinking: None,
            plugins: None,
            tools: Some(tools),
            tool_choice: Some(ToolChoice::Mode(ToolChoiceMode::Auto)),
            parallel_tool_calls: Some(true),
            disable_tool_validation: None,
            provider: None,
        };

        let json = serde_json::to_string(&request).unwrap();
        assert!(json.contains("\"tool_choice\":\"auto\""));
        assert!(json.contains("\"parallel_tool_calls\":true"));
        assert!(!json.contains("\"disable_tool_validation\""));
        assert!(json.contains("\"tools\""));
    }

    #[test]
    fn test_chat_request_forced_function_tool_choice_shape() {
        let request = ChatRequest {
            model: "test-model".to_string(),
            messages: vec![Message {
                role: "user".to_string(),
                content: Some("hi".to_string()),
                tool_calls: None,
                tool_call_id: None,
            }],
            user: None,
            max_completion_tokens: 128,
            stream: false,
            temperature: Some(TOOL_CALL_TEMPERATURE),
            response_format: None,
            disable_reasoning: None,
            clear_thinking: None,
            plugins: None,
            tools: Some(get_relace_search_tool_definitions()),
            tool_choice: Some(ToolChoice::Function(ToolChoiceFunctionSelection {
                choice_type: "function",
                function: ToolChoiceFunctionName {
                    name: "report_back",
                },
            })),
            parallel_tool_calls: Some(false),
            disable_tool_validation: None,
            provider: None,
        };

        let value: serde_json::Value = serde_json::to_value(request).unwrap();
        assert_eq!(value["tool_choice"]["type"], "function");
        assert_eq!(value["tool_choice"]["function"]["name"], "report_back");
        assert!(value.get("disable_tool_validation").is_none());
    }

    #[test]
    fn test_reasoning_config_maps_to_glm_controls() {
        use super::Model;
        let speed = reasoning_config(Model::Speed);
        assert_eq!(speed.disable_reasoning, Some(true));
        assert_eq!(speed.clear_thinking, Some(false));

        let smart = reasoning_config_for_model(Model::Smart, true);
        assert_eq!(smart.disable_reasoning, Some(false));
        assert_eq!(smart.clear_thinking, Some(false));
    }

    #[test]
    fn test_chat_request_without_provider_when_none() {
        let request = ChatRequest {
            model: "test-model".to_string(),
            messages: vec![Message {
                role: "user".to_string(),
                content: Some("hi".to_string()),
                tool_calls: None,
                tool_call_id: None,
            }],
            user: None,
            max_completion_tokens: 64,
            stream: false,
            temperature: None,
            response_format: None,
            disable_reasoning: None,
            clear_thinking: None,
            plugins: None,
            tools: None,
            tool_choice: None,
            parallel_tool_calls: None,
            disable_tool_validation: None,
            provider: None,
        };
        let value = serde_json::to_value(&request).expect("serialize request");
        assert!(value.get("provider").is_none());
        assert!(value.get("disable_tool_validation").is_none());
    }

    #[test]
    fn test_detects_tool_call_validation_errors() {
        let err = anyhow::anyhow!(
            "API error 400 Bad Request: {{\"error\":{{\"message\":\"Tool call validation failed: attempted to call tool 'json' which was not in request\"}}}}"
        );
        assert!(is_tool_call_validation_error(&err));
    }

    #[test]
    fn test_tool_error_loop_tracker_triggers_fail_after_retry_budget() {
        let mut tracker = ToolErrorLoopTracker::default();
        let signature = Some("view_directory:path_contract_violation".to_string());

        assert_eq!(
            tracker.observe(signature.clone()),
            ToolErrorLoopAction::None
        );
        assert_eq!(
            tracker.observe(signature.clone()),
            ToolErrorLoopAction::None
        );
        assert_eq!(
            tracker.observe(signature.clone()),
            ToolErrorLoopAction::InjectCorrective
        );
        assert_eq!(
            tracker.observe(signature.clone()),
            ToolErrorLoopAction::InjectCorrective
        );
        assert_eq!(
            tracker.observe(signature.clone()),
            ToolErrorLoopAction::InjectCorrective
        );
        assert_eq!(tracker.observe(signature), ToolErrorLoopAction::Fail);
    }

    #[test]
    fn test_finalization_non_report_back_action_is_bounded() {
        assert_eq!(
            finalization_non_report_back_action(0),
            FinalizationNonReportBackAction::Retry
        );
        assert_eq!(
            finalization_non_report_back_action(FINALIZATION_NON_REPORT_BACK_MAX_RETRIES),
            FinalizationNonReportBackAction::Fail
        );
    }

    #[test]
    fn test_invalid_report_back_action_is_bounded() {
        assert_eq!(
            invalid_report_back_action(0, true),
            InvalidReportBackAction::Retry
        );
        assert_eq!(
            invalid_report_back_action(INVALID_REPORT_BACK_PAYLOAD_MAX_RETRIES, true),
            InvalidReportBackAction::Fail
        );
        assert_eq!(
            invalid_report_back_action(0, false),
            InvalidReportBackAction::Fail
        );
    }

    #[test]
    fn test_streamed_reasoning_output_truncates_after_cap() {
        let mut state = StreamPrintState::default();
        let large = "a".repeat(STREAM_REASONING_PRINT_MAX_CHARS + 25);
        let first = format_streamed_reasoning_chunk(&large, &mut state).expect("expected output");
        assert!(first.contains("output truncated"));
        assert!(state.reasoning_truncated);
        assert_eq!(
            state.printed_reasoning_chars,
            STREAM_REASONING_PRINT_MAX_CHARS
        );

        let second = format_streamed_reasoning_chunk("still more", &mut state);
        assert!(second.is_none());
    }

    #[test]
    fn test_parse_retry_after_header_accepts_seconds() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            reqwest::header::RETRY_AFTER,
            reqwest::header::HeaderValue::from_static("7"),
        );

        assert_eq!(parse_retry_after_header(&headers), Some(7));
    }

    #[test]
    fn test_parse_retry_after_header_rejects_invalid_values() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            reqwest::header::RETRY_AFTER,
            reqwest::header::HeaderValue::from_static("later"),
        );
        assert_eq!(parse_retry_after_header(&headers), None);

        headers.insert(
            reqwest::header::RETRY_AFTER,
            reqwest::header::HeaderValue::from_static("0"),
        );
        assert_eq!(parse_retry_after_header(&headers), None);

        headers.insert(
            reqwest::header::RETRY_AFTER,
            reqwest::header::HeaderValue::from_static("600"),
        );
        assert_eq!(parse_retry_after_header(&headers), None);
    }

    #[test]
    fn test_normalize_tool_error_signature_for_path_contract() {
        let signature = normalize_tool_error_signature(
            "view_directory",
            "Invalid path '/repo': Path escapes repository: .",
        )
        .expect("expected normalized signature");
        assert_eq!(signature, "view_directory:path_contract_violation");
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
