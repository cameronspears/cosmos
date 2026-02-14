use super::models::{Model, Usage};
use cosmos_adapters::config::Config;
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    sync::{Mutex, OnceLock},
    time::{Duration, Instant},
};
use tokio::time::timeout;
use uuid::Uuid;

/// OpenRouter direct API URL (BYOK mode)
pub(crate) const OPENROUTER_URL: &str = "https://openrouter.ai/api/v1/chat/completions";

/// Maximum length for error content in error messages
const MAX_ERROR_CONTENT_LEN: usize = 200;

/// Sanitize API response content for error messages to prevent credential leakage.
fn sanitize_api_response(content: &str) -> String {
    const SECRET_PATTERNS: &[&str] = &[
        "api_key",
        "apikey",
        "secret",
        "password",
        "credential",
        "bearer",
        "sk-", // OpenAI/OpenRouter key prefix
    ];

    let truncated = truncate_str(content, MAX_ERROR_CONTENT_LEN);

    // Check if the content might contain secrets
    let lower = truncated.to_lowercase();
    for pattern in SECRET_PATTERNS {
        if lower.contains(pattern) {
            return "(response details redacted - may contain sensitive data)".to_string();
        }
    }

    truncated.to_string()
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

/// Get the configured OpenRouter API key, if any.
pub(crate) fn api_key() -> Option<String> {
    let mut config = Config::load();
    config.get_api_key()
}

/// Stable anonymous identifier for OpenRouter's `user` field.
///
/// OpenRouter uses this for user tracking to improve routing stickiness and caching.
/// We store an anonymous UUID in config so the same user gets consistent routing.
pub(crate) fn openrouter_user() -> Option<String> {
    if cfg!(test) {
        return None;
    }
    let mut config = Config::load();
    if let Some(id) = config.openrouter_user_id.clone() {
        return Some(id);
    }
    let id = format!("cosmos_{}", Uuid::new_v4());
    config.openrouter_user_id = Some(id.clone());
    let _ = config.save();
    Some(id)
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
    plugins: Option<Vec<PluginConfig>>,
    /// OpenRouter provider configuration for automatic fallback
    #[serde(skip_serializing_if = "Option::is_none")]
    provider: Option<ProviderConfig>,
}

/// OpenRouter provider configuration
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

/// OpenRouter provider routing preferences
///
/// See: https://openrouter.ai/docs/guides/routing/provider-selection
#[derive(Serialize, Clone)]
pub(crate) struct ProviderConfig {
    /// List of provider slugs to try in order (e.g. ["cerebras/fp16"])
    #[serde(skip_serializing_if = "Option::is_none")]
    order: Option<Vec<String>>,
    /// Allow OpenRouter to try other providers if the primary fails
    allow_fallbacks: bool,
    /// Only use providers that support all parameters in the request
    #[serde(skip_serializing_if = "Option::is_none")]
    require_parameters: Option<bool>,
    /// Prefer providers below this latency (seconds). Deprioritizes slow providers.
    #[serde(skip_serializing_if = "Option::is_none")]
    preferred_max_latency: Option<ProviderThresholds>,
    /// Prefer providers above this throughput (tokens/sec). Deprioritizes slow providers.
    #[serde(skip_serializing_if = "Option::is_none")]
    preferred_min_throughput: Option<ProviderThresholds>,
    /// Filter by quantization levels (e.g. ["fp16"])
    #[serde(skip_serializing_if = "Option::is_none")]
    quantizations: Option<Vec<String>>,
}

/// OpenRouter plugin configuration
#[derive(Serialize)]
struct PluginConfig {
    id: String,
}

/// Reasoning configuration for models that support it.
#[derive(Serialize)]
struct ReasoningConfig {
    effort: String,
    exclude: bool,
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

const RESPONSE_HEALING_PLUGIN_ID: &str = "response-healing";

fn provider_config(response_format: &Option<ResponseFormat>) -> ProviderConfig {
    // Default routing for non-Speed models.
    ProviderConfig {
        order: None,
        allow_fallbacks: true,
        require_parameters: response_format.as_ref().map(|_| true),
        // Soft routing preferences to reduce cold-start/no-content responses without
        // hard-pinning to a single provider.
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

// Preferred OpenRouter provider chain for gpt-oss-120b (Speed tier).
//
// We keep Cerebras fp16 as the elite default, and use fast, trusted fallbacks
// when Cerebras is temporarily slow or unavailable.
const GPT_OSS_PROVIDER_ORDER: [&str; 3] = ["cerebras/fp16", "deepinfra/turbo", "groq"];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ProviderFailureKind {
    Timeout,
    RateLimited,
    ServerError,
    NetworkError,
    Other,
}

#[derive(Clone, Debug, Default)]
struct ProviderCircuitState {
    consecutive_timeouts: u32,
    consecutive_failures: u32,
    open_until: Option<Instant>,
}

impl ProviderCircuitState {
    fn is_open(&self, now: Instant) -> bool {
        self.open_until.is_some_and(|until| until > now)
    }
}

static PROVIDER_CIRCUITS: OnceLock<Mutex<HashMap<String, ProviderCircuitState>>> = OnceLock::new();

fn provider_circuits() -> &'static Mutex<HashMap<String, ProviderCircuitState>> {
    PROVIDER_CIRCUITS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn classify_provider_error(message: &str) -> ProviderFailureKind {
    let lower = message.to_ascii_lowercase();
    if lower.contains("timed out after") || lower.contains("request timed out") {
        ProviderFailureKind::Timeout
    } else if lower.contains("no endpoints found") || lower.contains("404 not found") {
        // OpenRouter uses 404 for "no endpoints match the constraints", which is
        // effectively a provider-side incompatibility/outage for this request shape.
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

fn record_provider_success(provider: &str) {
    let circuits = provider_circuits();
    let mut guard = circuits.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(state) = guard.get_mut(provider) {
        *state = ProviderCircuitState::default();
    }
}

fn record_provider_failure(provider: &str, kind: ProviderFailureKind) {
    // Conservative circuit breaker:
    // - Keep Cerebras as the default unless we see repeated timeouts.
    // - Open briefly to avoid hammering a degraded provider during a single run.
    const OPEN_ON_TIMEOUTS: u32 = 2;
    const OPEN_ON_FAILURES: u32 = 3;
    const OPEN_TIMEOUT_SECS: u64 = 30;
    const OPEN_RATE_LIMIT_SECS: u64 = 60;

    let circuits = provider_circuits();
    let mut guard = circuits.lock().unwrap_or_else(|e| e.into_inner());
    let state = guard.entry(provider.to_string()).or_default();

    state.consecutive_failures = state.consecutive_failures.saturating_add(1);
    if kind == ProviderFailureKind::Timeout || kind == ProviderFailureKind::NetworkError {
        state.consecutive_timeouts = state.consecutive_timeouts.saturating_add(1);
    }

    let now = Instant::now();
    let open_for = match kind {
        ProviderFailureKind::RateLimited => Some(Duration::from_secs(OPEN_RATE_LIMIT_SECS)),
        ProviderFailureKind::Timeout | ProviderFailureKind::NetworkError => {
            if state.consecutive_timeouts >= OPEN_ON_TIMEOUTS {
                Some(Duration::from_secs(OPEN_TIMEOUT_SECS))
            } else {
                None
            }
        }
        ProviderFailureKind::ServerError => Some(Duration::from_secs(OPEN_TIMEOUT_SECS)),
        ProviderFailureKind::Other => None,
    };

    if state.consecutive_failures >= OPEN_ON_FAILURES {
        state.open_until = Some(now + Duration::from_secs(OPEN_TIMEOUT_SECS));
        return;
    }
    if let Some(dur) = open_for {
        state.open_until = Some(now + dur);
    }
}

fn provider_is_open(provider: &str) -> bool {
    let circuits = provider_circuits();
    let guard = circuits.lock().unwrap_or_else(|e| e.into_inner());
    guard
        .get(provider)
        .is_some_and(|state| state.is_open(Instant::now()))
}

fn provider_single(provider: &str, require_parameters: bool) -> ProviderConfig {
    ProviderConfig {
        order: Some(vec![provider.to_string()]),
        allow_fallbacks: false,
        require_parameters: if require_parameters { Some(true) } else { None },
        preferred_max_latency: None,
        preferred_min_throughput: None,
        quantizations: None,
    }
}

fn allocate_time_slices_ms(total_ms: u64, slots: usize) -> Vec<u64> {
    // Split the total timeout across sequential provider attempts, giving the first
    // provider (Cerebras) most of the time while still reserving meaningful time for fallbacks.
    if slots == 0 {
        return Vec::new();
    }
    if slots == 1 {
        return vec![total_ms.max(1)];
    }

    const MIN_FALLBACK_MS: u64 = 2_000;
    const MIN_PRIMARY_MS: u64 = 2_500;
    const MAX_PRIMARY_MS: u64 = 20_000;
    const PRIMARY_FRACTION_NUM: u64 = 70;
    const PRIMARY_FRACTION_DEN: u64 = 100;

    let total_ms = total_ms.max(1);
    let min_needed =
        MIN_PRIMARY_MS.saturating_add(MIN_FALLBACK_MS.saturating_mul((slots - 1) as u64));
    if total_ms < min_needed {
        // Not enough time to meaningfully try multiple providers. Spend the whole
        // budget on the first provider in the chain.
        return vec![total_ms];
    }

    // Reserve minimum time for each fallback provider, then distribute the remaining
    // time in a Cerebras-first way.
    let max_primary_ms =
        total_ms.saturating_sub(MIN_FALLBACK_MS.saturating_mul((slots - 1) as u64));
    let primary_target = total_ms
        .saturating_mul(PRIMARY_FRACTION_NUM)
        .saturating_div(PRIMARY_FRACTION_DEN);
    let primary_ms = primary_target
        .clamp(MIN_PRIMARY_MS, max_primary_ms.max(MIN_PRIMARY_MS))
        .clamp(MIN_PRIMARY_MS, MAX_PRIMARY_MS);

    let mut out = Vec::with_capacity(slots);
    out.push(primary_ms);

    // Give each fallback its minimum slice first.
    let mut remaining_ms = total_ms.saturating_sub(primary_ms);
    let fallback_slots = slots - 1;
    out.extend(std::iter::repeat_n(MIN_FALLBACK_MS, fallback_slots));
    remaining_ms =
        remaining_ms.saturating_sub(MIN_FALLBACK_MS.saturating_mul(fallback_slots as u64));

    // Distribute any remainder evenly across fallback providers.
    if fallback_slots > 0 && remaining_ms > 0 {
        let per = remaining_ms / fallback_slots as u64;
        let mut extra = remaining_ms.saturating_sub(per.saturating_mul(fallback_slots as u64));
        for i in 0..fallback_slots {
            out[i + 1] = out[i + 1].saturating_add(per);
            if extra > 0 {
                out[i + 1] = out[i + 1].saturating_add(1);
                extra -= 1;
            }
        }
    }

    out
}

/// Structured LLM call for the Speed tier with latency-aware provider failover.
///
/// This keeps Cerebras fp16 as the default, but ensures we don't burn the entire
/// caller-provided timeout waiting on a slow/unstable provider.
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
    #[derive(Clone)]
    struct AttemptConfig {
        slug: &'static str,
        provider: ProviderConfig,
        mode: AttemptMode,
    }

    #[derive(Clone, Copy)]
    enum AttemptMode {
        JsonSchema,
        JsonObjectNoReasoning,
    }

    let providers_all: Vec<AttemptConfig> = vec![
        AttemptConfig {
            slug: "cerebras/fp16",
            provider: provider_cerebras_fp16(),
            mode: AttemptMode::JsonSchema,
        },
        AttemptConfig {
            slug: "deepinfra/turbo",
            provider: provider_single("deepinfra/turbo", true),
            mode: AttemptMode::JsonObjectNoReasoning,
        },
        AttemptConfig {
            slug: "groq",
            provider: provider_single("groq", true),
            mode: AttemptMode::JsonObjectNoReasoning,
        },
    ];

    // If a provider is temporarily circuit-open, skip it. If everything is open,
    // fall back to trying the full chain anyway.
    let mut providers: Vec<AttemptConfig> = providers_all
        .iter()
        .filter(|cfg| !provider_is_open(cfg.slug))
        .cloned()
        .collect();
    if providers.is_empty() {
        providers = providers_all.clone();
    }

    let call_start = Instant::now();
    let mut diagnostics = SpeedFailoverDiagnostics {
        total_timeout_ms: timeout_ms,
        attempts: Vec::new(),
        selected_provider: None,
    };
    let mut errs: Vec<String> = Vec::new();
    let mut failed_kinds: Vec<ProviderFailureKind> = Vec::new();

    // Kept explicit to keep retry diagnostics and budget propagation in one place.
    #[allow(clippy::too_many_arguments)]
    async fn run_chain<T>(
        providers: Vec<AttemptConfig>,
        chain_budget_ms: u64,
        system: &str,
        user: &str,
        schema_name: &str,
        schema: &serde_json::Value,
        max_tokens: u32,
        diagnostics: &mut SpeedFailoverDiagnostics,
        errs: &mut Vec<String>,
        failed_kinds: &mut Vec<ProviderFailureKind>,
    ) -> Result<StructuredResponse<T>, ()>
    where
        T: serde::de::DeserializeOwned,
    {
        let slices = allocate_time_slices_ms(chain_budget_ms, providers.len());
        for (cfg, slice_ms) in providers.into_iter().zip(slices.into_iter()) {
            if slice_ms < 800 {
                continue;
            }
            let start = Instant::now();
            let mode = match cfg.mode {
                AttemptMode::JsonSchema => "json_schema",
                AttemptMode::JsonObjectNoReasoning => "json_object",
            };
            let result = match cfg.mode {
                AttemptMode::JsonSchema => {
                    call_llm_structured_with_provider::<T>(
                        system,
                        user,
                        Model::Speed,
                        schema_name,
                        schema.clone(),
                        cfg.provider,
                        max_tokens,
                        slice_ms,
                    )
                    .await
                }
                AttemptMode::JsonObjectNoReasoning => {
                    call_llm_json_mode_with_provider::<T>(
                        system,
                        user,
                        Model::Speed,
                        cfg.provider,
                        max_tokens,
                        slice_ms,
                        false,
                    )
                    .await
                }
            };
            let elapsed_ms = start.elapsed().as_millis() as u64;

            match result {
                Ok(mut resp) => {
                    record_provider_success(cfg.slug);
                    diagnostics.selected_provider = Some(cfg.slug.to_string());
                    diagnostics.attempts.push(ProviderAttemptDiagnostics {
                        provider_slug: cfg.slug.to_string(),
                        mode: mode.to_string(),
                        slice_timeout_ms: slice_ms,
                        elapsed_ms,
                        outcome_kind: "success".to_string(),
                        error_tail: None,
                    });
                    resp.speed_failover = Some(diagnostics.clone());
                    return Ok(resp);
                }
                Err(err) => {
                    let msg = err.to_string();
                    let kind = classify_provider_error(&msg);
                    record_provider_failure(cfg.slug, kind);
                    failed_kinds.push(kind);

                    let outcome_kind = match kind {
                        ProviderFailureKind::Timeout => "timeout",
                        ProviderFailureKind::RateLimited => "rate_limited",
                        ProviderFailureKind::ServerError => "server_error",
                        ProviderFailureKind::NetworkError => "network_error",
                        ProviderFailureKind::Other => "other",
                    };
                    diagnostics.attempts.push(ProviderAttemptDiagnostics {
                        provider_slug: cfg.slug.to_string(),
                        mode: mode.to_string(),
                        slice_timeout_ms: slice_ms,
                        elapsed_ms,
                        outcome_kind: outcome_kind.to_string(),
                        error_tail: Some(truncate_str(&msg, 240).to_string()),
                    });
                    errs.push(format!(
                        "{} ({}ms): {}",
                        cfg.slug,
                        elapsed_ms,
                        truncate_str(&msg, 240)
                    ));
                }
            }
        }

        Err(())
    }

    // First pass.
    if let Ok(resp) = run_chain::<T>(
        providers.clone(),
        timeout_ms,
        system,
        user,
        schema_name,
        &schema,
        max_tokens,
        &mut diagnostics,
        &mut errs,
        &mut failed_kinds,
    )
    .await
    {
        return Ok(resp);
    }

    // Optional second pass, only when the failures look like timeouts/rate limits and we still
    // have meaningful time left (some errors return immediately, leaving budget unused).
    let elapsed_total_ms = call_start.elapsed().as_millis() as u64;
    let remaining_ms = timeout_ms.saturating_sub(elapsed_total_ms);
    let retryable_only = !failed_kinds.is_empty()
        && failed_kinds.iter().all(|k| {
            matches!(
                k,
                ProviderFailureKind::Timeout | ProviderFailureKind::RateLimited
            )
        });
    if retryable_only && remaining_ms >= 2_500 {
        let mut providers_retry: Vec<AttemptConfig> = providers_all
            .iter()
            .filter(|cfg| !provider_is_open(cfg.slug))
            .cloned()
            .collect();
        if providers_retry.is_empty() {
            providers_retry = providers_all;
        }
        if let Ok(resp) = run_chain::<T>(
            providers_retry,
            remaining_ms,
            system,
            user,
            schema_name,
            &schema,
            max_tokens,
            &mut diagnostics,
            &mut errs,
            &mut failed_kinds,
        )
        .await
        {
            return Ok(resp);
        }
    }

    let message = format!(
        "All providers failed for openai/gpt-oss-120b. {}",
        errs.join(" | ")
    );
    Err(anyhow::Error::new(SpeedFailoverError {
        diagnostics,
        message,
    }))
}

fn provider_config_for_model(
    model: Model,
    response_format: &Option<ResponseFormat>,
) -> ProviderConfig {
    // For gpt-oss-120b we strongly prefer Cerebras fp16 (elite baseline), while
    // still allowing a narrow, explicit fallback chain when Cerebras is unavailable.
    if model == Model::Speed {
        return ProviderConfig {
            order: Some(
                GPT_OSS_PROVIDER_ORDER
                    .iter()
                    .map(|p| p.to_string())
                    .collect(),
            ),
            // Restrict routing to the explicit order only (Cerebras fp16 first, then
            // explicitly-approved fallbacks). This avoids silently drifting to an
            // unrelated provider when Cerebras is flaky.
            allow_fallbacks: false,
            require_parameters: response_format.as_ref().map(|_| true),
            // With an explicit provider order, avoid secondary heuristics that could
            // fight the "use Cerebras first" requirement.
            preferred_max_latency: None,
            preferred_min_throughput: None,
            quantizations: None,
        };
    }

    provider_config(response_format)
}

pub(crate) fn provider_cerebras_fp16() -> ProviderConfig {
    ProviderConfig {
        order: Some(vec!["cerebras/fp16".to_string()]),
        allow_fallbacks: false,
        require_parameters: Some(true),
        preferred_max_latency: None,
        preferred_min_throughput: None,
        quantizations: Some(vec!["fp16".to_string()]),
    }
}

fn response_healing_plugins(
    response_format: &Option<ResponseFormat>,
    stream: bool,
) -> Option<Vec<PluginConfig>> {
    if response_format.is_some() && !stream {
        Some(vec![PluginConfig {
            id: RESPONSE_HEALING_PLUGIN_ID.to_string(),
        }])
    } else {
        None
    }
}

fn reasoning_config(model: Model) -> Option<ReasoningConfig> {
    model.reasoning_effort().map(|effort| ReasoningConfig {
        effort: effort.to_string(),
        exclude: true,
    })
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
            _ => format!("API error {}: {}", status, sanitize_api_response(&text)),
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
    let plugins = response_healing_plugins(&response_format, stream);
    let provider = provider_config_for_model(model, &response_format);
    let reasoning = reasoning_config(model);

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
        user: openrouter_user(),
        max_tokens: model.max_tokens(),
        stream,
        response_format,
        reasoning,
        plugins,
        provider: Some(provider),
    };

    let text = send_with_retry(&client, &api_key, &request).await?;

    let parsed: ChatResponse = serde_json::from_str(&text).map_err(|e| {
        anyhow::anyhow!(
            "Failed to parse OpenRouter response: {}\n{}",
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

#[allow(clippy::too_many_arguments)]
pub(crate) async fn call_llm_structured_with_provider<T>(
    system: &str,
    user: &str,
    model: Model,
    schema_name: &str,
    schema: serde_json::Value,
    provider: ProviderConfig,
    max_tokens: u32,
    timeout_ms: u64,
) -> anyhow::Result<StructuredResponse<T>>
where
    T: serde::de::DeserializeOwned,
{
    let api_key = api_key().ok_or_else(|| {
        anyhow::anyhow!("No API key configured. Run 'cosmos --setup' to get started.")
    })?;

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
    let plugins = response_healing_plugins(&response_format, stream);
    let reasoning = reasoning_config(model);

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
        user: openrouter_user(),
        max_tokens,
        stream,
        response_format,
        reasoning,
        plugins,
        provider: Some(provider),
    };

    let text = timeout(
        Duration::from_millis(timeout_ms),
        send_with_retry(&client, &api_key, &request),
    )
    .await
    .map_err(|_| anyhow::anyhow!("Timed out after {}ms.", timeout_ms))??;

    let parsed: ChatResponse = serde_json::from_str(&text).map_err(|e| {
        anyhow::anyhow!(
            "Failed to parse OpenRouter response: {}\n{}",
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

#[allow(clippy::too_many_arguments)]
async fn call_llm_json_mode_with_provider<T>(
    system: &str,
    user: &str,
    model: Model,
    provider: ProviderConfig,
    max_tokens: u32,
    timeout_ms: u64,
    include_reasoning: bool,
) -> anyhow::Result<StructuredResponse<T>>
where
    T: serde::de::DeserializeOwned,
{
    let api_key = api_key().ok_or_else(|| {
        anyhow::anyhow!("No API key configured. Run 'cosmos --setup' to get started.")
    })?;

    if !model.supports_json_mode() {
        return Err(anyhow::anyhow!(
            "JSON mode isn't supported for {}. Try a different model.",
            model.id()
        ));
    }

    let client = create_http_client(REQUEST_TIMEOUT_SECS)?;

    let response_format = Some(ResponseFormat {
        format_type: "json_object".to_string(),
        json_schema: None,
    });

    let stream = false;
    let plugins = response_healing_plugins(&response_format, stream);
    let reasoning = if include_reasoning {
        reasoning_config(model)
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
        user: openrouter_user(),
        max_tokens,
        stream,
        response_format,
        reasoning,
        plugins,
        provider: Some(provider),
    };

    let text = timeout(
        Duration::from_millis(timeout_ms),
        send_with_retry(&client, &api_key, &request),
    )
    .await
    .map_err(|_| anyhow::anyhow!("Timed out after {}ms.", timeout_ms))??;

    let parsed: ChatResponse = serde_json::from_str(&text).map_err(|e| {
        anyhow::anyhow!(
            "Failed to parse OpenRouter response: {}\n{}",
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

/// Call LLM API with structured output (strict JSON schema).
///
/// Uses OpenRouter's structured outputs feature (`json_schema`) and Response Healing.
/// This is the preferred path for JSON responses that should not rely on custom
/// "ask the model to fix JSON" retries.
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
    let api_key = api_key().ok_or_else(|| {
        anyhow::anyhow!("No API key configured. Run 'cosmos --setup' to get started.")
    })?;

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
    let plugins = response_healing_plugins(&response_format, stream);
    let provider = provider_config_for_model(model, &response_format);
    let reasoning = reasoning_config(model);

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
        user: openrouter_user(),
        max_tokens: model.max_tokens(),
        stream,
        response_format,
        reasoning,
        plugins,
        provider: Some(provider),
    };

    let text = send_with_retry(&client, &api_key, &request).await?;

    let parsed: ChatResponse = serde_json::from_str(&text).map_err(|e| {
        anyhow::anyhow!(
            "Failed to parse OpenRouter response: {}\n{}",
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

/// Call LLM API with structured output using default provider routing, while
/// enforcing max tokens and a request timeout.
pub(crate) async fn call_llm_structured_limited<T>(
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
        true,
    )
    .await
}

/// Same as `call_llm_structured_limited`, but disables model reasoning.
/// Useful for latency-sensitive review paths where structured output already constrains format.
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
    let api_key = api_key().ok_or_else(|| {
        anyhow::anyhow!("No API key configured. Run 'cosmos --setup' to get started.")
    })?;

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
    let plugins = response_healing_plugins(&response_format, stream);
    let provider = provider_config_for_model(model, &response_format);
    let reasoning = if include_reasoning {
        reasoning_config(model)
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
        user: openrouter_user(),
        max_tokens,
        stream,
        response_format,
        reasoning,
        plugins,
        provider: Some(provider),
    };

    let text = timeout(
        Duration::from_millis(timeout_ms),
        send_with_retry(&client, &api_key, &request),
    )
    .await
    .map_err(|_| anyhow::anyhow!("Timed out after {}ms.", timeout_ms))??;

    let parsed: ChatResponse = serde_json::from_str(&text).map_err(|e| {
        anyhow::anyhow!(
            "Failed to parse OpenRouter response: {}\n{}",
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
    let plugins = response_healing_plugins(&response_format, stream);
    let provider = provider_config_for_model(model, &response_format);
    let reasoning = reasoning_config(model);

    // Use cached messages with cache_control on system prompt
    let request = CachedChatRequest {
        model: model.id().to_string(),
        messages: build_cached_messages(system, user),
        user: openrouter_user(),
        max_tokens: model.max_tokens(),
        stream,
        response_format,
        reasoning,
        plugins,
        provider: Some(provider),
    };

    let text = send_with_retry(&client, &api_key, &request).await?;

    let parsed: ChatResponse = serde_json::from_str(&text).map_err(|e| {
        anyhow::anyhow!(
            "Failed to parse OpenRouter response: {}\n{}",
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

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, Deserialize, PartialEq, Eq)]
    struct ParseProbe {
        description: String,
    }

    #[test]
    fn test_provider_requires_parameters_only_with_response_format() {
        let provider = provider_config_for_model(Model::Smart, &None);
        let value = serde_json::to_value(provider).unwrap();
        assert!(value.get("require_parameters").is_none());

        let response_format = Some(ResponseFormat {
            format_type: "json_object".to_string(),
            json_schema: None,
        });
        let provider = provider_config_for_model(Model::Smart, &response_format);
        let value = serde_json::to_value(provider).unwrap();
        assert_eq!(
            value.get("require_parameters").and_then(|v| v.as_bool()),
            Some(true)
        );
    }

    #[test]
    fn test_speed_model_prefers_cerebras_fp16_with_explicit_fallbacks() {
        let provider = provider_config_for_model(Model::Speed, &None);
        let value = serde_json::to_value(provider).unwrap();
        let order = value
            .get("order")
            .and_then(|v| v.as_array())
            .expect("expected explicit provider order for Model::Speed");
        assert_eq!(
            order
                .first()
                .and_then(|v| v.as_str())
                .expect("expected first provider"),
            "cerebras/fp16"
        );
        assert_eq!(
            value.get("allow_fallbacks").and_then(|v| v.as_bool()),
            Some(false)
        );
    }

    #[test]
    fn test_response_healing_plugins_only_for_non_streaming_json() {
        let response_format = Some(ResponseFormat {
            format_type: "json_object".to_string(),
            json_schema: None,
        });

        let plugins = response_healing_plugins(&response_format, false).expect("expected plugin");
        assert_eq!(plugins.len(), 1);
        assert_eq!(plugins[0].id, RESPONSE_HEALING_PLUGIN_ID);

        assert!(response_healing_plugins(&response_format, true).is_none());
        assert!(response_healing_plugins(&None, false).is_none());
    }

    #[test]
    fn test_reasoning_config_excludes_output() {
        let reasoning = reasoning_config(Model::Speed).expect("expected reasoning config");
        let value = serde_json::to_value(reasoning).unwrap();
        assert_eq!(value.get("exclude").and_then(|v| v.as_bool()), Some(true));
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

    #[test]
    fn allocate_time_slices_primary_only_when_budget_too_small() {
        let slices = allocate_time_slices_ms(5_000, 3);
        assert_eq!(slices, vec![5_000]);
    }

    #[test]
    fn allocate_time_slices_sums_to_total_for_typical_speed_call() {
        let slices = allocate_time_slices_ms(35_000, 3);
        assert_eq!(slices.iter().sum::<u64>(), 35_000);
        assert_eq!(slices.len(), 3);
        // With the current policy, a 35s budget should give Cerebras a big slice, bounded at 20s.
        assert_eq!(slices[0], 20_000);
        assert!(slices[1] >= 2_000);
        assert!(slices[2] >= 2_000);
    }

    #[test]
    fn allocate_time_slices_reserves_meaningful_fallback_time() {
        let slices = allocate_time_slices_ms(10_000, 3);
        assert_eq!(slices.iter().sum::<u64>(), 10_000);
        assert_eq!(slices.len(), 3);
        assert_eq!(slices[0], 6_000);
        assert_eq!(slices[1], 2_000);
        assert_eq!(slices[2], 2_000);
    }

    #[test]
    fn allocate_time_slices_two_providers() {
        let slices = allocate_time_slices_ms(20_000, 2);
        assert_eq!(slices.iter().sum::<u64>(), 20_000);
        assert_eq!(slices.len(), 2);
        assert!(slices[0] >= 2_500);
        assert!(slices[1] >= 2_000);
    }
}
