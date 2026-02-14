use super::client::{SpeedFailoverDiagnostics, SpeedFailoverError};
use super::fix::{
    generate_fix_content_with_model, generate_multi_file_fix_with_model, FileInput,
    FixGenerationErrorWithUsage, FixPreview,
};
use super::models::{merge_usage, Model, Usage};
use super::review::{
    fix_review_findings_with_model, verify_changes_bounded_with_model, FixContext, ReviewFinding,
};
use crate::lab::sandbox::SandboxSession;
use chrono::Utc;
use cosmos_adapters::cache::{Cache, ImplementationHarnessRecord};
use cosmos_adapters::git_ops;
use cosmos_adapters::util::{resolve_repo_path_allow_new, run_command_with_timeout, truncate};
use cosmos_core::index::parser::{parse_file, parse_file_has_errors};
use cosmos_core::index::Language;
use cosmos_core::suggest::{Suggestion, SuggestionValidationState};
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;
use uuid::Uuid;

mod defaults;
mod quick_checks;
mod review_helpers;

use defaults::{
    default_enable_quick_check_baseline, default_max_auto_syntax_fix_loops,
    default_max_smart_escalations_per_attempt, default_require_independent_review_on_pass,
    default_reserve_independent_review_cost_usd, default_reserve_independent_review_ms,
};
#[cfg(test)]
use quick_checks::quick_check_requires_real_node_modules;
use quick_checks::{
    command_to_string, detect_quick_check_command, is_eslint_fixable_failure,
    is_prettier_formatting_failure, program_available_on_path, run_eslint_fix, run_prettier_write,
    run_quick_checks, QuickCheckCommand,
};
use review_helpers::{
    blocking_findings, build_files_with_content, group_findings_by_file,
    is_probable_compile_error_false_positive,
};

const APPLY_HARNESS_REPORT_DIR: &str = ".cosmos/apply_harness";
const IMPLEMENTATION_MODEL: Model = Model::Speed;
const REASON_SCOPE_VIOLATION: &str = "scope_violation";
const REASON_DIFF_BUDGET_VIOLATION: &str = "diff_budget_violation";
const REASON_SYNTAX_VIOLATION: &str = "syntax_violation";
const REASON_BINARY_WRITE_VIOLATION: &str = "binary_write_violation";
const REASON_QUICK_CHECK_UNAVAILABLE: &str = "quick_check_unavailable";
const REASON_QUICK_CHECK_FAILED: &str = "quick_check_failed";
const REASON_BLOCKING_REVIEW_RESIDUAL: &str = "blocking_review_residual";
const REASON_PLAIN_LANGUAGE_FAILURE: &str = "plain_language_failure";
const REASON_NON_EMPTY_DIFF: &str = "non_empty_diff_violation";
const REASON_BUDGET_EXCEEDED: &str = "budget_exceeded";
const NOTE_QUICK_CHECK_FINGERPRINT_PREFIX: &str = "quick_check_failure_fingerprint:";
const BINARY_FILE_EXTENSIONS: &[&str] = &[
    "7z", "avi", "bmp", "class", "db", "dll", "dylib", "exe", "gif", "gz", "ico", "jar", "jpeg",
    "jpg", "mov", "mp3", "mp4", "ogg", "otf", "pdf", "png", "so", "sqlite", "tar", "tgz", "ttf",
    "wav", "webm", "woff", "woff2", "zip",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ImplementationQuickChecksMode {
    #[default]
    StrictAuto,
    Disabled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ImplementationQuickCheckStatus {
    Passed,
    Failed,
    #[default]
    Unavailable,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImplementationHarnessConfig {
    pub max_attempts: usize,
    pub max_total_ms: u64,
    pub max_total_cost_usd: f64,
    #[serde(default = "default_max_smart_escalations_per_attempt")]
    pub max_smart_escalations_per_attempt: usize,
    #[serde(default = "default_reserve_independent_review_ms")]
    pub reserve_independent_review_ms: u64,
    #[serde(default = "default_reserve_independent_review_cost_usd")]
    pub reserve_independent_review_cost_usd: f64,
    #[serde(default = "default_enable_quick_check_baseline")]
    pub enable_quick_check_baseline: bool,
    pub max_auto_review_fix_loops: usize,
    pub max_auto_quick_check_fix_loops: usize,
    /// Repair parse/syntax failures inside an attempt to improve first-attempt pass rate.
    /// This is only used for in-scope files and must keep diffs minimal.
    #[serde(default = "default_max_auto_syntax_fix_loops")]
    pub max_auto_syntax_fix_loops: usize,
    pub quick_checks_mode: ImplementationQuickChecksMode,
    pub review_blocking_severities: Vec<String>,
    pub max_changed_files: usize,
    pub max_total_changed_lines: usize,
    pub max_changed_lines_per_file: usize,
    pub quick_check_timeout_ms: u64,
    pub require_quick_check_detectable: bool,
    pub fail_on_reduced_confidence: bool,
    pub quick_check_fix_requires_in_scope_error: bool,
    #[serde(default = "default_require_independent_review_on_pass")]
    pub require_independent_review_on_pass: bool,
    #[serde(default)]
    pub adversarial_review_model: ImplementationReviewModel,
}

impl Default for ImplementationHarnessConfig {
    fn default() -> Self {
        Self::interactive_strict()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ImplementationReviewModel {
    #[default]
    Speed,
    Smart,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ImplementationHarnessRunContext {
    #[default]
    Interactive,
    Lab,
}

impl ImplementationHarnessRunContext {
    fn as_str(self) -> &'static str {
        match self {
            Self::Interactive => "interactive",
            Self::Lab => "lab",
        }
    }
}

impl ImplementationReviewModel {
    fn as_model(self) -> Model {
        match self {
            ImplementationReviewModel::Speed => Model::Speed,
            ImplementationReviewModel::Smart => Model::Smart,
        }
    }
}

impl ImplementationHarnessConfig {
    pub fn interactive_strict() -> Self {
        Self {
            max_attempts: 4,
            max_total_ms: 120_000,
            max_total_cost_usd: 0.080,
            max_smart_escalations_per_attempt: default_max_smart_escalations_per_attempt(),
            reserve_independent_review_ms: default_reserve_independent_review_ms(),
            reserve_independent_review_cost_usd: default_reserve_independent_review_cost_usd(),
            enable_quick_check_baseline: false,
            max_auto_review_fix_loops: 4,
            max_auto_quick_check_fix_loops: 2,
            max_auto_syntax_fix_loops: 2,
            quick_checks_mode: ImplementationQuickChecksMode::StrictAuto,
            review_blocking_severities: vec!["critical".to_string(), "warning".to_string()],
            max_changed_files: 6,
            max_total_changed_lines: 500,
            max_changed_lines_per_file: 220,
            quick_check_timeout_ms: 120_000,
            require_quick_check_detectable: false,
            fail_on_reduced_confidence: false,
            quick_check_fix_requires_in_scope_error: true,
            require_independent_review_on_pass: true,
            adversarial_review_model: ImplementationReviewModel::Speed,
        }
    }

    pub fn lab_strict() -> Self {
        let mut config = Self::interactive_strict();
        // Lab/CI uses a stricter policy surface (quick checks required), but we allow a small
        // amount of headroom above the *average* elite bars so the harness can finish a repair
        // loop when it is close to done.
        config.max_total_ms = 180_000;
        config.max_total_cost_usd = 0.120;
        config.max_auto_review_fix_loops = 8;
        // In lab/CI we prefer doing a bit more repair *within* attempt 1 to improve
        // first-attempt pass rate and avoid costly multi-attempt runs.
        config.max_auto_quick_check_fix_loops = 6;
        config.max_auto_syntax_fix_loops = 4;
        // Keep baseline off in "find success" mode to avoid spending most of an attempt budget
        // before generation/review.
        config.enable_quick_check_baseline = false;
        config.require_quick_check_detectable = true;
        config.fail_on_reduced_confidence = true;
        // Loosen mode: keep review fast to establish a successful envelope first.
        config.require_independent_review_on_pass = false;
        config.adversarial_review_model = ImplementationReviewModel::Speed;
        config
    }
}

#[derive(Debug, Clone)]
struct ImplementationBudget {
    started_at: std::time::Instant,
    max_total_ms: u64,
    max_total_cost_usd: f64,
}

const MIN_REMAINING_BUDGET_MS_FOR_LLM_CALL_MIN: u64 = 1_200;
const MIN_REMAINING_BUDGET_MS_FOR_LLM_CALL_MAX: u64 = 6_000;
const MIN_REMAINING_BUDGET_MS_FOR_LLM_CALL_RATIO: f64 = 0.15;
// Conservative buffer to avoid starting an LLM call when we're so close to the budget cap that
// normal token variance would likely overspend. The harness would rather stop and explain why
// than silently exceed its configured budget.
const MIN_REMAINING_BUDGET_USD_FOR_LLM_CALL_MIN: f64 = 0.00015;
const MIN_REMAINING_BUDGET_USD_FOR_LLM_CALL_MAX: f64 = 0.003;
const MIN_REMAINING_BUDGET_USD_FOR_LLM_CALL_RATIO: f64 = 0.02;
// Allow a tiny overrun margin for provider-side accounting/rounding jitter so we don't
// fail otherwise-good attempts on noise-level differences.
const BUDGET_COST_OVERRUN_TOLERANCE_USD: f64 = 0.00025;
const BUDGET_TIMEOUT_SLACK_MS: u64 = 250;
const MAX_GENERATION_TIMEOUT_MS: u64 = 75_000;
const MAX_REVIEW_TIMEOUT_MS: u64 = 90_000;
const MAX_FIX_TIMEOUT_MS: u64 = 70_000;
const MIN_MEANINGFUL_ATTEMPT_MS: u64 = 10_000;
// Late attempts need enough budget for at least one real generation+gate step.
// Smaller caps create "guaranteed budget failures" where a single call exceeds the cap.
const MIN_MEANINGFUL_ATTEMPT_COST_USD: f64 = 0.0025;

impl ImplementationBudget {
    fn min_remaining_ms_buffer(&self) -> u64 {
        ((self.max_total_ms as f64) * MIN_REMAINING_BUDGET_MS_FOR_LLM_CALL_RATIO) as u64
    }

    fn exhausted(&self, usage: &Option<Usage>) -> Option<ImplementationFailReason> {
        let elapsed_ms = self.started_at.elapsed().as_millis() as u64;
        if elapsed_ms >= self.max_total_ms {
            return Some(ImplementationFailReason {
                code: REASON_BUDGET_EXCEEDED.to_string(),
                gate: "budget".to_string(),
                message: format!(
                    "Stopped to respect the configured time budget ({}ms elapsed; limit {}ms)",
                    elapsed_ms, self.max_total_ms
                ),
                action: default_action_for_fail_reason("budget", REASON_BUDGET_EXCEEDED)
                    .to_string(),
            });
        }

        let cost_usd = usage.as_ref().map(|u| u.cost()).unwrap_or(0.0);
        if cost_usd >= (self.max_total_cost_usd + BUDGET_COST_OVERRUN_TOLERANCE_USD) {
            return Some(ImplementationFailReason {
                code: REASON_BUDGET_EXCEEDED.to_string(),
                gate: "budget".to_string(),
                message: format!(
                    "Stopped to respect the configured cost budget (${:0.4} spent; limit ${:0.4})",
                    cost_usd, self.max_total_cost_usd
                ),
                action: default_action_for_fail_reason("budget", REASON_BUDGET_EXCEEDED)
                    .to_string(),
            });
        }

        None
    }

    fn remaining_ms(&self) -> u64 {
        let elapsed_ms = self.started_at.elapsed().as_millis() as u64;
        self.max_total_ms.saturating_sub(elapsed_ms)
    }

    fn remaining_cost_usd(&self, usage: &Option<Usage>) -> f64 {
        let cost_usd = usage.as_ref().map(|u| u.cost()).unwrap_or(0.0);
        (self.max_total_cost_usd - cost_usd).max(0.0)
    }

    fn min_remaining_cost_buffer_usd(&self) -> f64 {
        (self.max_total_cost_usd * MIN_REMAINING_BUDGET_USD_FOR_LLM_CALL_RATIO).clamp(
            MIN_REMAINING_BUDGET_USD_FOR_LLM_CALL_MIN,
            MIN_REMAINING_BUDGET_USD_FOR_LLM_CALL_MAX,
        )
    }

    fn timeout_ms_for_next_llm_call(&self) -> u64 {
        self.remaining_ms()
            .saturating_sub(BUDGET_TIMEOUT_SLACK_MS)
            .max(1)
    }

    /// Guardrail to avoid starting a new LLM call when the remaining budget is too small
    /// to safely complete it without overspending.
    fn guard_before_llm_call(&self, usage: &Option<Usage>) -> Option<ImplementationFailReason> {
        if let Some(reason) = self.exhausted(usage) {
            return Some(reason);
        }

        let remaining_ms = self.remaining_ms();
        let min_ms_buffer = self.min_remaining_ms_buffer().clamp(
            MIN_REMAINING_BUDGET_MS_FOR_LLM_CALL_MIN,
            MIN_REMAINING_BUDGET_MS_FOR_LLM_CALL_MAX,
        );
        if remaining_ms < min_ms_buffer {
            return Some(ImplementationFailReason {
                code: REASON_BUDGET_EXCEEDED.to_string(),
                gate: "budget".to_string(),
                message: format!(
                    "Stopped to respect the configured time budget ({}ms remaining; limit {}ms)",
                    remaining_ms, self.max_total_ms
                ),
                action: default_action_for_fail_reason("budget", REASON_BUDGET_EXCEEDED)
                    .to_string(),
            });
        }

        let remaining_cost = self.remaining_cost_usd(usage);
        let min_cost_buffer = self.min_remaining_cost_buffer_usd();
        if remaining_cost < min_cost_buffer {
            return Some(ImplementationFailReason {
                code: REASON_BUDGET_EXCEEDED.to_string(),
                gate: "budget".to_string(),
                message: format!(
                    "Stopped to respect the configured cost budget (${:0.4} remaining; limit ${:0.4})",
                    remaining_cost, self.max_total_cost_usd
                ),
                action: default_action_for_fail_reason("budget", REASON_BUDGET_EXCEEDED)
                    .to_string(),
            });
        }

        None
    }
}

fn reserve_budget_for_quick_check_repair(
    budget: &ImplementationBudget,
    usage: &Option<Usage>,
    reserve_independent_review_ms: u64,
    reserve_independent_review_cost_usd: f64,
) -> Option<ImplementationFailReason> {
    if let Some(reason) = budget.guard_before_llm_call(usage) {
        return Some(reason);
    }

    let remaining_ms = budget.remaining_ms();
    let reserve_ms = reserve_independent_review_ms.max(1);
    if remaining_ms < reserve_ms {
        return Some(ImplementationFailReason {
            code: REASON_BUDGET_EXCEEDED.to_string(),
            gate: "budget".to_string(),
            message: format!(
                "Stopped to preserve independent-review budget before quick-check auto-fix ({}ms remaining; need at least {}ms reserved)",
                remaining_ms, reserve_ms
            ),
            action: default_action_for_fail_reason("budget", REASON_BUDGET_EXCEEDED).to_string(),
        });
    }

    let remaining_cost = budget.remaining_cost_usd(usage);
    let reserve_cost = reserve_independent_review_cost_usd
        .max(budget.min_remaining_cost_buffer_usd())
        .max(0.0);
    if remaining_cost < reserve_cost {
        return Some(ImplementationFailReason {
            code: REASON_BUDGET_EXCEEDED.to_string(),
            gate: "budget".to_string(),
            message: format!(
                "Stopped to preserve independent-review budget before quick-check auto-fix (${:.4} remaining; need at least ${:.4} reserved)",
                remaining_cost, reserve_cost
            ),
            action: default_action_for_fail_reason("budget", REASON_BUDGET_EXCEEDED).to_string(),
        });
    }

    None
}

fn attempt_budget_weights(max_attempts: usize) -> Vec<f64> {
    let max_attempts = max_attempts.max(1);
    if max_attempts == 1 {
        return vec![1.0];
    }
    if max_attempts == 2 {
        // Keep a strong first attempt while preserving a real fallback attempt.
        return vec![0.80, 0.20];
    }
    if max_attempts == 3 {
        return vec![0.70, 0.20, 0.10];
    }

    let mut weights = vec![0.0; max_attempts];
    weights[0] = 0.55;
    weights[1] = 0.25;
    let tail = max_attempts.saturating_sub(2);
    if tail == 0 {
        return weights;
    }
    let per = 0.20 / tail as f64;
    for idx in 0..tail {
        weights[idx + 2] = per;
    }
    weights
}

fn compute_attempt_budget_caps(
    global_budget: &ImplementationBudget,
    usage_so_far: &Option<Usage>,
    attempt_index: usize,
    weights: &[f64],
) -> (u64, f64) {
    let remaining_ms = global_budget.remaining_ms().max(1);
    let remaining_cost = global_budget.remaining_cost_usd(usage_so_far);
    if weights.is_empty() {
        return (remaining_ms, remaining_cost);
    }

    let idx = attempt_index
        .saturating_sub(1)
        .min(weights.len().saturating_sub(1));
    let remaining_weight_sum = weights[idx..].iter().sum::<f64>();
    let ratio = if remaining_weight_sum <= 0.0 {
        1.0
    } else {
        (weights[idx] / remaining_weight_sum).clamp(0.0, 1.0)
    };

    let min_ms_target = remaining_ms.min(MIN_MEANINGFUL_ATTEMPT_MS);
    let attempt_ms = (((remaining_ms as f64) * ratio).floor() as u64).max(min_ms_target);
    let attempt_ms = attempt_ms.clamp(1, remaining_ms);
    let min_cost_target = remaining_cost.min(MIN_MEANINGFUL_ATTEMPT_COST_USD);
    let attempt_cost = (remaining_cost * ratio)
        .max(min_cost_target)
        .min(remaining_cost);
    (attempt_ms, attempt_cost)
}

fn default_action_for_fail_reason(gate: &str, code: &str) -> &'static str {
    match code {
        REASON_BUDGET_EXCEEDED => {
            "Rerun apply with a smaller scoped change or a higher budget for this run."
        }
        REASON_QUICK_CHECK_FAILED => {
            "Fix the quick-check error in the scoped file and rerun apply."
        }
        REASON_QUICK_CHECK_UNAVAILABLE => {
            "Install or enable the required quick-check tool for this repo, then rerun apply."
        }
        REASON_SCOPE_VIOLATION => {
            "Regenerate the fix so it only edits files in the validated scope."
        }
        REASON_SYNTAX_VIOLATION => "Fix parse/syntax errors in changed files and rerun apply.",
        REASON_DIFF_BUDGET_VIOLATION => {
            "Reduce changed files/lines to stay within scope and rerun apply."
        }
        REASON_BLOCKING_REVIEW_RESIDUAL => {
            "Address blocking review findings in scope and rerun apply."
        }
        REASON_PLAIN_LANGUAGE_FAILURE => {
            "Rewrite the user-facing summary in plain language and rerun apply."
        }
        REASON_NON_EMPTY_DIFF => "Generate at least one in-scope file change and rerun apply.",
        _ if gate == "quick_check" => "Resolve the quick-check issue in scope and rerun apply.",
        _ => "Review the failure details and rerun apply.",
    }
}

fn normalize_fail_reason_message(gate: &str, code: &str, message: &str) -> String {
    let detail = message.trim();
    let plain_prefix = match code {
        REASON_BUDGET_EXCEEDED => {
            "Cosmos stopped before applying changes because the run budget was exhausted"
        }
        REASON_QUICK_CHECK_FAILED => {
            "Cosmos could not apply this change because project quick checks failed"
        }
        REASON_QUICK_CHECK_UNAVAILABLE => {
            "Cosmos could not run project quick checks in this environment"
        }
        REASON_SCOPE_VIOLATION => {
            "Cosmos stopped because the proposed edit went outside the validated scope"
        }
        REASON_SYNTAX_VIOLATION => {
            "Cosmos stopped because the proposed edit introduced a syntax problem"
        }
        REASON_DIFF_BUDGET_VIOLATION => {
            "Cosmos stopped because the proposed edit exceeded size limits"
        }
        REASON_BLOCKING_REVIEW_RESIDUAL => "Cosmos stopped because blocking review issues remained",
        REASON_PLAIN_LANGUAGE_FAILURE => {
            "Cosmos stopped because the user-facing description was not plain language"
        }
        _ if gate == "review" => "Cosmos stopped because review checks did not pass",
        _ if gate == "quick_check" => "Cosmos stopped because project quick checks did not pass",
        _ => "Cosmos stopped before applying changes",
    };

    if detail.is_empty() {
        return format!("{}.", plain_prefix);
    }
    if detail
        .to_ascii_lowercase()
        .starts_with(&plain_prefix.to_ascii_lowercase())
    {
        return detail.to_string();
    }
    format!("{}. {}", plain_prefix, detail)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ImplementationFinalizationStatus {
    Applied,
    RolledBack,
    #[default]
    FailedBeforeFinalize,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ImplementationFinalizationDiagnostics {
    pub status: ImplementationFinalizationStatus,
    #[serde(default)]
    pub detail: Option<String>,
    #[serde(default)]
    pub mutation_on_failure: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ImplementationFailReason {
    pub code: String,
    pub gate: String,
    pub message: String,
    #[serde(default)]
    pub action: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImplementationCommandOutcome {
    pub command: String,
    pub duration_ms: u64,
    pub success: bool,
    pub timed_out: bool,
    pub exit_code: Option<i32>,
    pub stdout_tail: String,
    pub stderr_tail: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImplementationLlmCallRecord {
    /// Logical stage in the harness attempt ("generation", "review", "review_fix", etc.)
    pub kind: String,
    #[serde(default)]
    pub independence_role: Option<String>,
    #[serde(default)]
    pub escalation_reason: Option<String>,
    pub model: String,
    pub timeout_ms: u64,
    #[serde(default)]
    pub schema_fallback_used: bool,
    #[serde(default)]
    pub speed_failover: Option<SpeedFailoverDiagnostics>,
    #[serde(default)]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImplementationGateSnapshot {
    pub gate: String,
    pub passed: bool,
    pub detail: String,
    #[serde(default)]
    pub reason_code: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImplementationAttemptDiagnostics {
    pub attempt_index: usize,
    pub passed: bool,
    pub fail_reasons: Vec<String>,
    #[serde(default)]
    pub fail_reason_records: Vec<ImplementationFailReason>,
    pub gates: Vec<ImplementationGateSnapshot>,
    pub changed_files: Vec<PathBuf>,
    pub changed_lines_total: usize,
    #[serde(default)]
    pub changed_lines_by_file: HashMap<PathBuf, usize>,
    pub quick_check_status: ImplementationQuickCheckStatus,
    #[serde(default)]
    pub quick_check_command: Option<String>,
    #[serde(default)]
    pub quick_check_outcome: Option<ImplementationCommandOutcome>,
    #[serde(default)]
    pub quick_check_outcomes: Vec<ImplementationCommandOutcome>,
    #[serde(default)]
    pub quick_check_fix_loops: usize,
    #[serde(default)]
    pub quick_check_failure_summary: Option<String>,
    pub review_iterations: usize,
    pub review_blocking_remaining: usize,
    #[serde(default)]
    pub remaining_blocking_titles: Vec<String>,
    #[serde(default)]
    pub remaining_blocking_categories: Vec<String>,
    pub attempt_ms: u64,
    pub attempt_cost_usd: f64,
    #[serde(default)]
    pub llm_calls: Vec<ImplementationLlmCallRecord>,
    #[serde(default)]
    pub notes: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImplementationRunDiagnostics {
    pub run_id: String,
    pub suggestion_id: String,
    pub suggestion_summary: String,
    pub model: String,
    pub strict_mode: bool,
    pub passed: bool,
    pub attempt_count: usize,
    pub total_ms: u64,
    pub total_cost_usd: f64,
    #[serde(default)]
    pub reduced_confidence: bool,
    #[serde(default)]
    pub fail_reasons: Vec<String>,
    #[serde(default)]
    pub attempts: Vec<ImplementationAttemptDiagnostics>,
    #[serde(default)]
    pub report_path: Option<PathBuf>,
    #[serde(default)]
    pub finalization: ImplementationFinalizationDiagnostics,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImplementationAppliedFile {
    pub path: PathBuf,
    pub summary: String,
    pub content: String,
}

#[derive(Debug, Clone)]
pub struct ImplementationRunResult {
    pub description: String,
    pub file_changes: Vec<ImplementationAppliedFile>,
    pub usage: Option<Usage>,
    pub diagnostics: ImplementationRunDiagnostics,
}

#[derive(Debug)]
struct AttemptExecution {
    diagnostics: ImplementationAttemptDiagnostics,
    usage: Option<Usage>,
    pass_payload: Option<AttemptPassPayload>,
}

#[derive(Debug)]
struct AttemptPassPayload {
    description: String,
    file_changes: Vec<ImplementationAppliedFile>,
}

#[derive(Debug, Clone)]
struct RepoChanges {
    files: Vec<PathBuf>,
    untracked: HashSet<PathBuf>,
}

fn strip_ansi_sequences(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\u{1b}' && matches!(chars.peek(), Some('[')) {
            // Skip ANSI CSI: ESC [ ... (letters)
            let _ = chars.next(); // '['
            for c in chars.by_ref() {
                if c.is_ascii_alphabetic() {
                    break;
                }
            }
            continue;
        }
        out.push(ch);
    }
    out
}

fn parse_path_line_col(raw: &str) -> Option<(String, u32, u32)> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    let parts = trimmed.split(':').collect::<Vec<_>>();
    if parts.len() < 3 {
        return None;
    }
    let col = parts.last()?.trim().parse::<u32>().ok()?;
    let line = parts
        .get(parts.len().saturating_sub(2))?
        .trim()
        .parse::<u32>()
        .ok()?;
    let file = parts[..parts.len().saturating_sub(2)].join(":");
    let file = file.trim_start_matches("./").to_string();
    let ext_ok = Path::new(&file)
        .extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| {
            matches!(
                ext.to_ascii_lowercase().as_str(),
                "ts" | "tsx" | "js" | "jsx" | "mjs" | "cjs" | "rs" | "go" | "py"
            )
        })
        .unwrap_or(false);
    if !ext_ok {
        return None;
    }
    Some((file, line, col))
}

fn parse_tsc_error_line(raw: &str) -> Option<(String, u32, u32, String)> {
    // Example:
    //   src/foo.ts(12,34): error TS2304: Cannot find name 'X'.
    let trimmed = raw.trim();
    let re = Regex::new(
        r"^\s*(?P<path>[^\s:(][^():]*)\((?P<line>\d+),(?P<col>\d+)\):\s*error\s*TS\d+:\s*(?P<msg>.+)$",
    )
    .ok()?;
    let caps = re.captures(trimmed)?;
    let path = caps.name("path")?.as_str();
    let path = path.trim_start_matches("./").replace('\\', "/");
    let line = caps.name("line")?.as_str().parse::<u32>().ok()?;
    let col = caps.name("col")?.as_str().parse::<u32>().ok()?;
    let msg = caps.name("msg").map(|m| m.as_str().trim().to_string())?;
    let ext_ok = Path::new(&path)
        .extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| {
            matches!(
                ext.to_ascii_lowercase().as_str(),
                "ts" | "tsx" | "js" | "jsx" | "mjs" | "cjs" | "rs" | "go" | "py"
            )
        })
        .unwrap_or(false);
    if !ext_ok {
        return None;
    }
    Some((path, line, col, msg))
}

fn parse_colon_error_line_with_message(raw: &str) -> Option<(String, u32, u32, String)> {
    // Common format (e.g. go/tsc in some modes):
    //   ./path/file.ts:12:34: message...
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    let re = Regex::new(
        r"^\s*(?:-->\s*)?(?:\./)?(?P<path>[^\s:]+?\.[A-Za-z0-9]+):(?P<line>\d+):(?P<col>\d+):\s*(?P<msg>.+)$",
    )
    .ok()?;
    let caps = re.captures(trimmed)?;
    let path = caps.name("path")?.as_str().trim_start_matches("./");
    let path = path.replace('\\', "/");
    let line = caps.name("line")?.as_str().parse::<u32>().ok()?;
    let col = caps.name("col")?.as_str().parse::<u32>().ok()?;
    let msg = caps.name("msg")?.as_str().trim().to_string();
    let ext_ok = Path::new(&path)
        .extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| {
            matches!(
                ext.to_ascii_lowercase().as_str(),
                "ts" | "tsx" | "js" | "jsx" | "mjs" | "cjs" | "rs" | "go" | "py"
            )
        })
        .unwrap_or(false);
    if !ext_ok {
        return None;
    }
    Some((path, line, col, msg))
}

fn strip_quick_check_subtask_prefix(raw: &str) -> &str {
    // Common pnpm/yarn stream prefixes:
    //   . test:lint: <actual line>
    //   . test:coverage: <actual line>
    let trimmed = raw.trim();
    let Some(rest) = trimmed.strip_prefix(". ") else {
        return trimmed;
    };
    let Some(split_idx) = rest.rfind(": ") else {
        return trimmed;
    };
    let (label, tail_with_sep) = rest.split_at(split_idx);
    if !label
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == ':' || c == '-' || c == '_')
    {
        return trimmed;
    }
    tail_with_sep.trim_start_matches(": ").trim()
}

fn parse_bracketed_path_line(raw: &str) -> Option<(String, String)> {
    // Example (prettier):
    //   [warn] lib/command.js
    let trimmed = strip_quick_check_subtask_prefix(raw);
    if trimmed.is_empty() {
        return None;
    }
    let re = Regex::new(r"^\s*\[(?P<tag>warn|error)\]\s+(?P<path>[^\s]+?\.[A-Za-z0-9]+)\b").ok()?;
    let caps = re.captures(trimmed)?;
    let tag = caps.name("tag")?.as_str().to_ascii_lowercase();
    let path = caps.name("path")?.as_str().trim_start_matches("./");
    let path = path.replace('\\', "/");
    Some((tag, path))
}

fn parse_python_compileall_error_line(raw: &str) -> Option<String> {
    // Example:
    //   *** Error compiling 'src/foo.py'...
    let trimmed = strip_quick_check_subtask_prefix(raw);
    if !trimmed.contains("Error compiling") {
        return None;
    }
    let re = Regex::new(r"^\s*\*{3}\s*Error compiling\s+'(?P<path>[^']+?\.py)'.*").ok()?;
    let caps = re.captures(trimmed)?;
    let path = caps.name("path")?.as_str().trim_start_matches("./");
    let path = path.replace('\\', "/");
    Some(path)
}

fn parse_python_file_line(raw: &str) -> Option<(String, u32)> {
    // Example:
    //   File "src/foo.py", line 12
    let trimmed = strip_quick_check_subtask_prefix(raw);
    if !trimmed.starts_with("File ") {
        return None;
    }
    let re =
        Regex::new(r#"^\s*File\s+"(?P<path>[^"]+?\.py)"\s*,\s*line\s*(?P<line>\d+)\b"#).ok()?;
    let caps = re.captures(trimmed)?;
    let path = caps.name("path")?.as_str().trim_start_matches("./");
    let path = path.replace('\\', "/");
    let line = caps.name("line")?.as_str().parse::<u32>().ok()?;
    Some((path, line))
}

fn parse_eslint_detail_line(raw: &str) -> Option<(u32, u32)> {
    // Example:
    //   12:34  error  message...
    let trimmed = strip_quick_check_subtask_prefix(raw);
    let re = Regex::new(r"^\s*(?P<line>\d+):(?P<col>\d+)\s+(?:error|warning)\b").ok()?;
    let caps = re.captures(trimmed)?;
    let line = caps.name("line")?.as_str().parse::<u32>().ok()?;
    let col = caps.name("col")?.as_str().parse::<u32>().ok()?;
    Some((line, col))
}

fn parse_rust_error_header_line(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.starts_with("error") {
        return Some(trimmed.to_string());
    }
    None
}

fn parse_rust_location_line(raw: &str) -> Option<(String, u32, u32)> {
    // Example:
    //   --> src/error.rs:471:39
    let trimmed = raw.trim();
    let re = Regex::new(r"^\s*-->\s*(?P<path>[^\s:]+?\.[A-Za-z0-9]+):(?P<line>\d+):(?P<col>\d+)")
        .ok()?;
    let caps = re.captures(trimmed)?;
    let path = caps.name("path")?.as_str().trim_start_matches("./");
    let path = path.replace('\\', "/");
    let line = caps.name("line")?.as_str().parse::<u32>().ok()?;
    let col = caps.name("col")?.as_str().parse::<u32>().ok()?;
    let ext_ok = Path::new(&path)
        .extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| ext.eq_ignore_ascii_case("rs"))
        .unwrap_or(false);
    if !ext_ok {
        return None;
    }
    Some((path, line, col))
}

fn summarize_quick_check_failure(outcome: &ImplementationCommandOutcome) -> Option<String> {
    let stderr = strip_ansi_sequences(&outcome.stderr_tail);
    let stdout = strip_ansi_sequences(&outcome.stdout_tail);

    for line in stderr.lines().chain(stdout.lines()).map(str::trim) {
        if let Some((file, ln, col, msg)) = parse_tsc_error_line(line) {
            return Some(format!(
                "Quick check failed ({}): {}:{}:{} {}",
                outcome.command, file, ln, col, msg
            ));
        }
    }

    // Next.js TypeScript errors often look like:
    //   ./path/file.ts:12:34
    //   Type error: Cannot find name 'X'.
    let stderr_lines = stderr.lines().map(str::trim).collect::<Vec<_>>();
    for (idx, line) in stderr_lines.iter().enumerate() {
        if let Some((file, ln, col)) = parse_path_line_col(line) {
            let ext = Path::new(&file)
                .extension()
                .and_then(|ext| ext.to_str())
                .map(|ext| ext.to_ascii_lowercase())
                .unwrap_or_default();
            if !matches!(ext.as_str(), "ts" | "tsx" | "js" | "jsx" | "mjs" | "cjs") {
                continue;
            }
            let next = stderr_lines.get(idx + 1).copied().unwrap_or("");
            let msg = next.strip_prefix("Type error:").unwrap_or(next).trim();
            if !msg.is_empty() {
                return Some(format!(
                    "Quick check failed ({}): {}:{}:{} {}",
                    outcome.command, file, ln, col, msg
                ));
            }
            return Some(format!(
                "Quick check failed ({}): {}:{}:{}",
                outcome.command, file, ln, col
            ));
        }
    }

    for line in stderr.lines().chain(stdout.lines()).map(str::trim) {
        if let Some((file, ln, col, msg)) = parse_colon_error_line_with_message(line) {
            return Some(format!(
                "Quick check failed ({}): {}:{}:{} {}",
                outcome.command, file, ln, col, msg
            ));
        }
    }

    let combined_lines = stderr
        .lines()
        .chain(stdout.lines())
        .map(str::trim)
        .collect::<Vec<_>>();
    for (idx, line) in combined_lines.iter().enumerate() {
        if let Some((file, ln, col)) = parse_rust_location_line(line) {
            let header = combined_lines
                .iter()
                .take(idx)
                .rev()
                .find_map(|prev| parse_rust_error_header_line(prev));
            if let Some(header) = header {
                return Some(format!(
                    "Quick check failed ({}): {}:{}:{} {}",
                    outcome.command, file, ln, col, header
                ));
            }
            return Some(format!(
                "Quick check failed ({}): {}:{}:{}",
                outcome.command, file, ln, col
            ));
        }
    }

    for line in stderr.lines().chain(stdout.lines()).map(str::trim) {
        if let Some((tag, file)) = parse_bracketed_path_line(line) {
            return Some(format!(
                "Quick check failed ({}): [{}] {}",
                outcome.command, tag, file
            ));
        }
    }

    for line in stderr.lines().chain(stdout.lines()).map(str::trim) {
        if let Some(file) = parse_python_compileall_error_line(line) {
            return Some(format!(
                "Quick check failed ({}): python compile error in {}",
                outcome.command, file
            ));
        }
        if let Some((file, ln)) = parse_python_file_line(line) {
            return Some(format!(
                "Quick check failed ({}): {}:{} (python compile error)",
                outcome.command, file, ln
            ));
        }
    }

    // JS quick checks often fail with actionable sub-tool output (eslint / size-limit), but the
    // overall runner exit line (e.g. ELIFECYCLE) is not helpful. Prefer the actionable detail.
    fn strip_test_prefix(line: &str) -> &str {
        strip_quick_check_subtask_prefix(line)
    }
    let combined = format!("{}\n{}", stderr, stdout);
    let mut parts: Vec<String> = Vec::new();
    if let Some(line) = combined.lines().map(str::trim).find(|l| {
        let lower = l.to_ascii_lowercase();
        lower.contains("coverage for lines")
            && lower.contains("does not meet")
            && lower.contains("threshold")
    }) {
        parts.push(strip_test_prefix(line).to_string());
    }
    if let Some(line) = combined.lines().map(str::trim).find(|l| {
        l.to_ascii_lowercase()
            .contains("package size limit has exceeded")
    }) {
        parts.push(strip_test_prefix(line).to_string());
    }
    if let Some(line) = combined.lines().map(str::trim).find(|l| {
        let lower = l.to_ascii_lowercase();
        lower.contains(" error ")
            && (lower.contains("eslint")
                || lower.contains("n/prefer-node-protocol")
                || lower.contains("@typescript-eslint")
                || lower.contains("tsc")
                || lower.contains("lint"))
    }) {
        parts.push(strip_test_prefix(line).to_string());
    }
    if !parts.is_empty() {
        return Some(format!(
            "Quick check failed ({}): {}",
            outcome.command,
            truncate(&parts.join(" | "), 260)
        ));
    }

    let pick_line = |text: &str| -> Option<String> {
        let mut best: Option<&str> = None;
        let mut best_score: i32 = i32::MIN;
        for line in text.lines().map(str::trim) {
            if line.is_empty() {
                continue;
            }
            let lower = line.to_ascii_lowercase();

            let is_progress = lower.starts_with("updating ")
                || lower.starts_with("checking ")
                || lower.starts_with("compiling ")
                || lower.starts_with("finished ")
                || lower.starts_with("downloading ")
                || lower.starts_with("locking ");
            // Many test runners print passing lines that include the word "error" (e.g.
            // "✔ handles error cases"). De-prioritize those so users see the real failure.
            let is_passing = line.starts_with('✔')
                || line.starts_with('✓')
                || lower.starts_with("pass ")
                || lower.starts_with("passed ")
                || lower.contains("0 errors")
                || lower.contains("0 failed")
                || lower.contains("no errors");

            let is_exit_wrapper = lower.contains("command failed") || lower.contains("elifecycle");
            let high_signal = lower.starts_with("fail")
                || lower.contains("npm err!")
                || lower.contains("yarn err!")
                || lower.contains("err!")
                || lower.contains("exit code")
                || lower.contains("assertionerror")
                || lower.contains("typeerror")
                || lower.contains("referenceerror")
                || lower.contains("syntaxerror")
                || lower.contains("panic")
                || lower.contains("fatal");
            let medium_signal =
                lower.contains("error") || lower.contains("failed") || lower.contains("cannot ");

            let mut score = 0i32;
            if is_progress {
                score -= 2;
            }
            if is_passing {
                score -= 4;
            }
            if is_exit_wrapper {
                score -= 3;
            }
            if high_signal {
                score += 10;
            } else if medium_signal {
                score += 5;
            }

            if score > best_score {
                best_score = score;
                best = Some(line);
            }

            if high_signal && !is_passing && !is_exit_wrapper {
                break;
            }
        }
        best.map(|s| s.to_string())
    };

    if let Some(line) = pick_line(&stderr) {
        return Some(format!(
            "Quick check failed ({}): {}",
            outcome.command, line
        ));
    }
    if let Some(line) = pick_line(&stdout) {
        return Some(format!(
            "Quick check failed ({}): {}",
            outcome.command, line
        ));
    }
    None
}

fn quick_check_failure_fingerprint(summary: &str) -> String {
    // Normalize volatile line/column and spacing noise so we can detect when repeated
    // auto-repair loops are failing for the same underlying reason.
    let mut out = String::with_capacity(summary.len());
    let mut in_digits = false;
    let mut last_was_space = false;
    for ch in summary.chars() {
        if ch.is_ascii_digit() {
            if !in_digits {
                out.push('#');
                in_digits = true;
                last_was_space = false;
            }
            continue;
        }
        in_digits = false;
        let lower = ch.to_ascii_lowercase();
        if lower.is_whitespace() {
            if !last_was_space {
                out.push(' ');
                last_was_space = true;
            }
            continue;
        }
        out.push(lower);
        last_was_space = false;
    }
    out.trim().to_string()
}

fn quick_check_repair_hint_from_summary(summary: &str) -> Option<String> {
    let lower = summary.to_ascii_lowercase();
    if lower.contains("error[e0277]") && lower.contains("`?` operator can only be used") {
        return Some(
            "Rust E0277 hint: remove `?` in functions that do not return `Result`/`Option`, or change the function return type to support `?`."
                .to_string(),
        );
    }
    None
}

fn note_quick_check_failure_fingerprint(notes: &mut Vec<String>, summary: Option<&str>) {
    let Some(summary) = summary else {
        return;
    };
    let fingerprint = quick_check_failure_fingerprint(summary);
    if fingerprint.is_empty() {
        return;
    }
    notes.push(format!(
        "{}{}",
        NOTE_QUICK_CHECK_FINGERPRINT_PREFIX, fingerprint
    ));
}

fn extract_prefixed_note_value<'a>(notes: &'a [String], prefix: &str) -> Option<&'a str> {
    notes
        .iter()
        .rev()
        .find_map(|note| note.strip_prefix(prefix).map(str::trim))
        .filter(|value| !value.is_empty())
}

fn extract_size_limit_exceeded(outcome: &ImplementationCommandOutcome) -> Vec<(String, u32)> {
    // Example (pnpm):
    //   . test:size:   non-secure nanoid
    //   . test:size:   Package size limit has exceeded by 25 B
    let stdout = strip_ansi_sequences(&outcome.stdout_tail);
    let stderr = strip_ansi_sequences(&outcome.stderr_tail);
    let combined = format!("{}\n{}", stdout, stderr);

    let re = Regex::new(r"(?i)package size limit has exceeded by\s+(?P<bytes>\d+)\s*b").ok();
    let mut current: Option<String> = None;
    let mut out: Vec<(String, u32)> = Vec::new();
    for raw in combined.lines().map(str::trim) {
        let mut line = raw;
        if let Some(rest) = line.strip_prefix(". test:size:") {
            line = rest.trim();
        }
        if line.is_empty() {
            continue;
        }
        let lower = line.to_ascii_lowercase();
        if !lower.starts_with("size limit:")
            && !lower.starts_with("size:")
            && !lower.starts_with("package size limit")
            && !lower.starts_with("try to reduce")
            && !lower.starts_with("failed")
        {
            current = Some(line.to_string());
        }
        if let Some(re) = re.as_ref() {
            if let Some(caps) = re.captures(line) {
                if let Ok(bytes) = caps
                    .name("bytes")
                    .map(|m| m.as_str())
                    .unwrap_or("0")
                    .parse::<u32>()
                {
                    let label = current.clone().unwrap_or_else(|| "unknown".to_string());
                    out.push((label, bytes));
                }
            }
        }
    }

    // De-dupe by label while preserving first-seen ordering; keep max exceeded bytes.
    let mut indices: HashMap<String, usize> = HashMap::new();
    let mut deduped: Vec<(String, u32)> = Vec::new();
    for (label, bytes) in out {
        if let Some(&idx) = indices.get(&label) {
            if bytes > deduped[idx].1 {
                deduped[idx].1 = bytes;
            }
            continue;
        }
        indices.insert(label.clone(), deduped.len());
        deduped.push((label, bytes));
    }
    deduped
}

fn is_safe_relative_path(path: &Path) -> bool {
    if path.is_absolute() {
        return false;
    }
    for component in path.components() {
        match component {
            std::path::Component::CurDir | std::path::Component::Normal(_) => {}
            _ => return false,
        }
    }
    true
}

fn relativize_under_repo_root(path: &Path, repo_root: &Path) -> Option<PathBuf> {
    // Prefer canonicalized comparisons so `/var` vs `/private/var` doesn't cause false negatives
    // on macOS and symlink-heavy temp dirs.
    let repo_root_canon =
        std::fs::canonicalize(repo_root).unwrap_or_else(|_| repo_root.to_path_buf());
    let path_canon = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());

    let rel = if let Ok(rel) = path_canon.strip_prefix(&repo_root_canon) {
        rel.to_path_buf()
    } else if let Ok(rel) = path.strip_prefix(repo_root) {
        rel.to_path_buf()
    } else {
        return None;
    };
    if !is_safe_relative_path(&rel) {
        return None;
    }
    Some(rel)
}

fn normalize_quick_check_path_in_repo(raw: &str, repo_root: &Path) -> Option<PathBuf> {
    let trimmed = raw
        .trim()
        .trim_matches('`')
        .trim_matches('"')
        .trim_matches('\'');
    if trimmed.is_empty() {
        return None;
    }
    let normalized = trimmed.replace('\\', "/");
    let normalized = normalized.trim_start_matches("./");
    let path = PathBuf::from(normalized);

    if path.is_absolute() {
        return relativize_under_repo_root(&path, repo_root);
    }

    if !is_safe_relative_path(&path) {
        return None;
    }
    Some(path)
}

fn normalize_stack_trace_path_in_repo(raw: &str, repo_root: &Path) -> Option<PathBuf> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    let trimmed = trimmed.strip_prefix("file://").unwrap_or(trimmed);
    if trimmed.starts_with("node:")
        || trimmed.starts_with("http:")
        || trimmed.starts_with("https:")
        || trimmed.starts_with("webpack:")
    {
        return None;
    }
    normalize_quick_check_path_in_repo(trimmed, repo_root)
}

fn extract_quick_check_error_paths(
    outcome: &ImplementationCommandOutcome,
    repo_root: &Path,
) -> Vec<PathBuf> {
    let combined = format!(
        "{}\n{}",
        strip_ansi_sequences(&outcome.stderr_tail),
        strip_ansi_sequences(&outcome.stdout_tail)
    );

    let mut out: Vec<PathBuf> = Vec::new();
    let mut seen: HashSet<PathBuf> = HashSet::new();

    let tsc_re = Regex::new(
        r"(?m)^\s*(?P<path>[^\s:(][^():]*)\((?P<line>\d+),(?P<col>\d+)\):\s*error\s*TS\d+:",
    );
    if let Ok(re) = tsc_re {
        for caps in re.captures_iter(&combined) {
            if let Some(path) = caps
                .name("path")
                .and_then(|m| normalize_quick_check_path_in_repo(m.as_str(), repo_root))
            {
                if seen.insert(path.clone()) {
                    out.push(path);
                }
            }
        }
    }

    let colon_re = Regex::new(
        r"(?m)^\s*(?:-->\s*)?(?:\./)?(?P<path>[^\s:]+?\.[A-Za-z0-9]+):(?P<line>\d+):(?P<col>\d+)",
    );
    if let Ok(re) = colon_re {
        for caps in re.captures_iter(&combined) {
            if let Some(path) = caps
                .name("path")
                .and_then(|m| normalize_quick_check_path_in_repo(m.as_str(), repo_root))
            {
                if seen.insert(path.clone()) {
                    out.push(path);
                }
            }
        }
    }

    // Node/Vitest/Jest stack traces often look like:
    //   at fn (src/file.ts:12:34)
    //   at src/file.ts:12:34
    let stack_paren_re = Regex::new(
        r"(?m)^\s*at\s+.*\((?P<path>[^()]+?\.[A-Za-z0-9]+):(?P<line>\d+):(?P<col>\d+)\)",
    );
    if let Ok(re) = stack_paren_re {
        for caps in re.captures_iter(&combined) {
            if let Some(path) = caps
                .name("path")
                .and_then(|m| normalize_stack_trace_path_in_repo(m.as_str(), repo_root))
            {
                if seen.insert(path.clone()) {
                    out.push(path);
                }
            }
        }
    }
    let stack_simple_re =
        Regex::new(r"(?m)^\s*at\s+(?P<path>[^\s()]+?\.[A-Za-z0-9]+):(?P<line>\d+):(?P<col>\d+)\b");
    if let Ok(re) = stack_simple_re {
        for caps in re.captures_iter(&combined) {
            if let Some(path) = caps
                .name("path")
                .and_then(|m| normalize_stack_trace_path_in_repo(m.as_str(), repo_root))
            {
                if seen.insert(path.clone()) {
                    out.push(path);
                }
            }
        }
    }

    let combined_lines = combined.lines().map(str::trim).collect::<Vec<_>>();
    for (idx, line) in combined_lines.iter().enumerate() {
        let normalized = strip_quick_check_subtask_prefix(line);

        if let Some((_tag, file)) = parse_bracketed_path_line(normalized) {
            if let Some(path) = normalize_quick_check_path_in_repo(&file, repo_root) {
                if seen.insert(path.clone()) {
                    out.push(path);
                }
            }
        }

        if let Some(file) = parse_python_compileall_error_line(normalized) {
            if let Some(path) = normalize_quick_check_path_in_repo(&file, repo_root) {
                if seen.insert(path.clone()) {
                    out.push(path);
                }
            }
        }

        if let Some((file, _ln)) = parse_python_file_line(normalized) {
            if let Some(path) = normalize_quick_check_path_in_repo(&file, repo_root) {
                if seen.insert(path.clone()) {
                    out.push(path);
                }
            }
        }

        // ESLint often prints the file path on its own line followed by `line:col  error ...`.
        if !normalized.contains(':') {
            if let Some(path) = normalize_quick_check_path_in_repo(normalized, repo_root) {
                let ext = path
                    .extension()
                    .and_then(|ext| ext.to_str())
                    .map(|ext| ext.to_ascii_lowercase())
                    .unwrap_or_default();
                if matches!(ext.as_str(), "js" | "jsx" | "ts" | "tsx" | "mjs" | "cjs")
                    && combined_lines
                        .get(idx + 1)
                        .and_then(|next| {
                            parse_eslint_detail_line(strip_quick_check_subtask_prefix(next))
                        })
                        .is_some()
                    && seen.insert(path.clone())
                {
                    out.push(path);
                }
            }
        }
    }

    out
}

fn extract_quick_check_error_locations(
    outcome: &ImplementationCommandOutcome,
    repo_root: &Path,
) -> Vec<(PathBuf, u32, u32)> {
    let combined = format!(
        "{}\n{}",
        strip_ansi_sequences(&outcome.stderr_tail),
        strip_ansi_sequences(&outcome.stdout_tail)
    );

    let mut out: Vec<(PathBuf, u32, u32)> = Vec::new();
    let mut seen: HashSet<(PathBuf, u32, u32)> = HashSet::new();

    let combined_lines = combined.lines().map(str::trim).collect::<Vec<_>>();
    let mut current_eslint_file: Option<PathBuf> = None;
    let stack_paren_re =
        Regex::new(r"^\s*at\s+.*\((?P<path>[^()]+?\.[A-Za-z0-9]+):(?P<line>\d+):(?P<col>\d+)\)")
            .ok();
    let stack_simple_re =
        Regex::new(r"^\s*at\s+(?P<path>[^\s()]+?\.[A-Za-z0-9]+):(?P<line>\d+):(?P<col>\d+)\b").ok();

    for raw in combined_lines.iter().copied() {
        let normalized = strip_quick_check_subtask_prefix(raw);

        if let Some((file, ln, col, _msg)) = parse_tsc_error_line(normalized) {
            if let Some(path) = normalize_quick_check_path_in_repo(&file, repo_root) {
                let key = (path.clone(), ln, col);
                if seen.insert(key.clone()) {
                    out.push(key);
                }
            }
            continue;
        }

        if let Some((file, ln, col, _msg)) = parse_colon_error_line_with_message(normalized) {
            if let Some(path) = normalize_quick_check_path_in_repo(&file, repo_root) {
                let key = (path.clone(), ln, col);
                if seen.insert(key.clone()) {
                    out.push(key);
                }
            }
            continue;
        }

        // Rust-style location lines: `--> path:line:col`
        if let Some((file, ln, col)) = parse_rust_location_line(normalized) {
            if let Some(path) = normalize_quick_check_path_in_repo(&file, repo_root) {
                let key = (path.clone(), ln, col);
                if seen.insert(key.clone()) {
                    out.push(key);
                }
            }
            continue;
        }

        // Next.js style: `./path/file.ts:line:col` (message may be on the next line)
        if let Some((file, ln, col)) = parse_path_line_col(normalized) {
            if let Some(path) = normalize_quick_check_path_in_repo(&file, repo_root) {
                let key = (path.clone(), ln, col);
                if seen.insert(key.clone()) {
                    out.push(key);
                }
            }
            continue;
        }

        // Node/Vitest/Jest stack traces: `at fn (path:line:col)` or `at path:line:col`.
        if let Some(re) = stack_paren_re.as_ref() {
            if let Some(caps) = re.captures(normalized) {
                let file = caps.name("path").map(|m| m.as_str()).unwrap_or_default();
                if let (Some(path), Ok(ln), Ok(col)) = (
                    normalize_stack_trace_path_in_repo(file, repo_root),
                    caps.name("line")
                        .map(|m| m.as_str())
                        .unwrap_or("0")
                        .parse::<u32>(),
                    caps.name("col")
                        .map(|m| m.as_str())
                        .unwrap_or("0")
                        .parse::<u32>(),
                ) {
                    let key = (path.clone(), ln.max(1), col.max(1));
                    if seen.insert(key.clone()) {
                        out.push(key);
                    }
                    continue;
                }
            }
        }
        if let Some(re) = stack_simple_re.as_ref() {
            if let Some(caps) = re.captures(normalized) {
                let file = caps.name("path").map(|m| m.as_str()).unwrap_or_default();
                if let (Some(path), Ok(ln), Ok(col)) = (
                    normalize_stack_trace_path_in_repo(file, repo_root),
                    caps.name("line")
                        .map(|m| m.as_str())
                        .unwrap_or("0")
                        .parse::<u32>(),
                    caps.name("col")
                        .map(|m| m.as_str())
                        .unwrap_or("0")
                        .parse::<u32>(),
                ) {
                    let key = (path.clone(), ln.max(1), col.max(1));
                    if seen.insert(key.clone()) {
                        out.push(key);
                    }
                    continue;
                }
            }
        }

        if let Some((file, ln)) = parse_python_file_line(normalized) {
            if let Some(path) = normalize_quick_check_path_in_repo(&file, repo_root) {
                // Python compile errors don't always include a column.
                let key = (path.clone(), ln, 1);
                if seen.insert(key.clone()) {
                    out.push(key);
                }
            }
            continue;
        }

        // ESLint path header line (relative path only)
        if !normalized.contains(':') {
            if let Some(path) = normalize_quick_check_path_in_repo(normalized, repo_root) {
                let ext = path
                    .extension()
                    .and_then(|ext| ext.to_str())
                    .map(|ext| ext.to_ascii_lowercase())
                    .unwrap_or_default();
                if matches!(ext.as_str(), "js" | "jsx" | "ts" | "tsx" | "mjs" | "cjs") {
                    current_eslint_file = Some(path);
                    continue;
                }
            }
        }

        if let Some((ln, col)) = parse_eslint_detail_line(normalized) {
            if let Some(path) = current_eslint_file.clone() {
                let key = (path.clone(), ln, col);
                if seen.insert(key.clone()) {
                    out.push(key);
                }
            }
            continue;
        }

        if normalized.is_empty() {
            current_eslint_file = None;
        }
    }

    out
}

fn snippet_around_line(content: &str, line: u32, context_lines: usize) -> Option<String> {
    if line == 0 {
        return None;
    }
    let lines = content.lines().collect::<Vec<_>>();
    if lines.is_empty() {
        return None;
    }
    let idx = line.saturating_sub(1) as usize;
    if idx >= lines.len() {
        return None;
    }
    let start = idx.saturating_sub(context_lines);
    let end = (idx + context_lines + 1).min(lines.len());
    let snippet = lines[start..end].join("\n");
    if snippet.trim().is_empty() {
        return None;
    }
    Some(snippet)
}

fn quick_check_read_only_context_excerpt(
    sandbox_root: &Path,
    outcome: &ImplementationCommandOutcome,
    target: &Path,
) -> Option<String> {
    let locations = extract_quick_check_error_locations(outcome, sandbox_root);
    for (path, ln, _col) in locations {
        if path == target {
            continue;
        }
        let resolved = resolve_repo_path_allow_new(sandbox_root, &path).ok()?;
        let content = std::fs::read_to_string(&resolved.absolute).ok()?;
        let snippet = snippet_around_line(&content, ln, 8)?;
        return Some(format!(
            "Read-only context from failing location (do not edit this file):\n- Location: {}:{}\n```\n{}\n```",
            path.display(),
            ln,
            snippet
        ));
    }
    None
}

fn quick_check_target_context_excerpt(
    sandbox_root: &Path,
    outcome: &ImplementationCommandOutcome,
    target: &Path,
    content: &str,
) -> Option<String> {
    extract_quick_check_error_locations(outcome, sandbox_root)
        .into_iter()
        .find(|(path, _, _)| path == target)
        .and_then(|(path, ln, _)| {
            snippet_around_line(content, ln, 8).map(|snippet| {
                format!(
                    "Focused context near the reported quick-check error in this file:\n- Location: {}:{}\n```\n{}\n```",
                    path.display(),
                    ln,
                    snippet
                )
            })
        })
}

fn format_quick_check_repair_modifier(
    existing: Option<&str>,
    error_summary: &str,
    outcome: &ImplementationCommandOutcome,
    target: &Path,
    target_context_excerpt: Option<&str>,
    repair_hint: Option<&str>,
) -> String {
    let mut parts = Vec::new();
    if let Some(existing) = existing {
        let trimmed = existing.trim();
        if !trimmed.is_empty() {
            parts.push(trimmed.to_string());
        }
    }
    let output_excerpt = {
        let stderr = strip_ansi_sequences(&outcome.stderr_tail);
        let stdout = strip_ansi_sequences(&outcome.stdout_tail);
        let excerpt = if !stderr.trim().is_empty() {
            stderr
        } else {
            stdout
        };
        let excerpt = truncate(&excerpt, 700);
        if excerpt.trim().is_empty() {
            None
        } else {
            Some(excerpt)
        }
    };
    let size_exceeded = extract_size_limit_exceeded(outcome);
    if !size_exceeded.is_empty() {
        let details = size_exceeded
            .iter()
            .take(4)
            .map(|(label, bytes)| format!("{} (+{}B)", label, bytes))
            .collect::<Vec<_>>()
            .join(", ");
        parts.push(format!(
            "Size-limit guidance:\n- The repo enforces bundle size limits and the check failed: {}.\n- Avoid adding new imports/dependencies unless the plan explicitly requires it.\n- Prefer the smallest possible change; if the plan is informational, implement it via comments/docs rather than runtime code.\n- Reduce output size enough to get under the configured limit.",
            details
        ));
    }
    parts.push(format!(
        "Quick-check repair request:\n- Quick-check failure: {}\n- File to repair: {}\n{}\nRules:\n- Modify only this file.\n- Fix the reported error.\n- Keep the diff minimal and avoid unrelated reformatting.\n- Do not change behavior outside what's needed for the error.",
        truncate(error_summary, 240),
        target.display(),
        output_excerpt
            .as_deref()
            .map(|excerpt| format!("- Quick-check output (truncated):\n{}", excerpt))
            .unwrap_or_default(),
    ));
    if let Some(hint) = repair_hint {
        let hint = hint.trim();
        if !hint.is_empty() {
            parts.push(format!("Compiler-specific hint:\n- {}", hint));
        }
    }
    if let Some(excerpt) = target_context_excerpt {
        let excerpt = excerpt.trim();
        if !excerpt.is_empty() {
            parts.push(excerpt.to_string());
        }
    }
    parts.join("\n\n")
}

fn format_syntax_repair_modifier(
    existing: Option<&str>,
    parse_error: &str,
    target: &Path,
) -> String {
    let mut parts = Vec::new();
    if let Some(existing) = existing {
        let trimmed = existing.trim();
        if !trimmed.is_empty() {
            parts.push(trimmed.to_string());
        }
    }
    parts.push(format!(
        "Syntax repair request:\n- The syntax/parse gate failed for: {}\n- Error (truncated): {}\nRules:\n- Modify only this file.\n- Fix syntax/parse errors only.\n- Keep the diff minimal and avoid unrelated refactors or reformatting.\n- Do not change behavior beyond what is required to restore valid syntax.",
        target.display(),
        truncate(parse_error, 300),
    ));
    parts.join("\n\n")
}

fn dedup_preserve_order(items: Vec<String>) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for item in items {
        if seen.insert(item.clone()) {
            out.push(item);
        }
    }
    out
}

fn feedback_reasons_for_next_attempt(diag: &ImplementationAttemptDiagnostics) -> Vec<String> {
    let mut out = Vec::new();

    if !diag.fail_reason_records.is_empty() {
        for record in &diag.fail_reason_records {
            if record.code == REASON_QUICK_CHECK_FAILED {
                if let Some(outcome) = &diag.quick_check_outcome {
                    if let Some(summary) = summarize_quick_check_failure(outcome) {
                        out.push(summary);
                        continue;
                    }
                }
            }
            out.push(record.message.clone());
        }
    } else {
        out.extend(diag.fail_reasons.clone());
    }

    if !diag.remaining_blocking_titles.is_empty() {
        let titles = diag
            .remaining_blocking_titles
            .iter()
            .take(4)
            .cloned()
            .collect::<Vec<_>>()
            .join("; ");
        if !titles.trim().is_empty() {
            out.push(format!("Blocking findings remained: {}", titles));
        }
    }

    if let Some(fingerprint) =
        extract_prefixed_note_value(&diag.notes, NOTE_QUICK_CHECK_FINGERPRINT_PREFIX)
    {
        out.push(format!(
            "The previous quick-check failure repeated the same fingerprint ({}). Use a different in-scope repair approach.",
            fingerprint
        ));
    }

    dedup_preserve_order(out)
}

fn attempt_quick_check_failure_fingerprint(
    diag: &ImplementationAttemptDiagnostics,
) -> Option<String> {
    if let Some(summary) = diag.quick_check_failure_summary.as_deref() {
        let fingerprint = quick_check_failure_fingerprint(summary);
        if !fingerprint.is_empty() {
            return Some(fingerprint);
        }
    }
    extract_prefixed_note_value(&diag.notes, NOTE_QUICK_CHECK_FINGERPRINT_PREFIX)
        .map(str::to_string)
}

fn push_gate(
    gates: &mut Vec<ImplementationGateSnapshot>,
    gate: &str,
    passed: bool,
    detail: impl Into<String>,
    reason_code: Option<&str>,
) {
    gates.push(ImplementationGateSnapshot {
        gate: gate.to_string(),
        passed,
        detail: detail.into(),
        reason_code: reason_code.map(str::to_string),
    });
}

fn upsert_gate(
    gates: &mut Vec<ImplementationGateSnapshot>,
    gate: &str,
    passed: bool,
    detail: impl Into<String>,
    reason_code: Option<&str>,
) {
    if let Some(existing) = gates.iter_mut().find(|g| g.gate == gate) {
        existing.passed = passed;
        existing.detail = detail.into();
        existing.reason_code = reason_code.map(str::to_string);
        return;
    }
    push_gate(gates, gate, passed, detail, reason_code);
}

fn push_fail_reason(
    fail_reasons: &mut Vec<String>,
    fail_reason_records: &mut Vec<ImplementationFailReason>,
    gate: &str,
    code: &str,
    message: impl Into<String>,
) {
    let msg = normalize_fail_reason_message(gate, code, &message.into());
    let action = default_action_for_fail_reason(gate, code).to_string();
    fail_reasons.push(msg.clone());
    fail_reason_records.push(ImplementationFailReason {
        code: code.to_string(),
        gate: gate.to_string(),
        message: msg,
        action,
    });
}

fn ensure_implementation_model(model: Model) -> anyhow::Result<()> {
    if model != IMPLEMENTATION_MODEL {
        return Err(anyhow::anyhow!(
            "Implementation harness policy violation: model '{}' is not allowed",
            model.id()
        ));
    }
    Ok(())
}

fn ensure_generation_model(model: Model) -> anyhow::Result<()> {
    const ALLOWED: &[Model] = &[Model::Speed, Model::Smart];
    if ALLOWED.contains(&model) {
        Ok(())
    } else {
        Err(anyhow::anyhow!(
            "Generation model '{}' is not allowed",
            model.id()
        ))
    }
}

fn ensure_adversarial_review_model(model: Model) -> anyhow::Result<()> {
    const ALLOWED: &[Model] = &[Model::Speed, Model::Smart];
    if ALLOWED.contains(&model) {
        Ok(())
    } else {
        Err(anyhow::anyhow!(
            "Adversarial review model '{}' is not allowed",
            model.id()
        ))
    }
}

fn is_response_format_schema_error_text(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    lower.contains("invalid schema for response_format")
        || (lower.contains("invalid schema") && lower.contains("response_format"))
}

fn usage_from_generation_error(err: &anyhow::Error) -> Option<Usage> {
    err.downcast_ref::<FixGenerationErrorWithUsage>()
        .and_then(|e| e.usage.clone())
}

fn generation_escalation_reason(error_text: &str) -> Option<&'static str> {
    let lower = error_text.to_ascii_lowercase();
    if lower.contains("old_string not found") {
        Some("apply_anchor_not_found")
    } else if lower.contains("ambiguous")
        || lower.contains("multiple matches")
        || lower.contains("more than one match")
    {
        Some("apply_anchor_ambiguous")
    } else if lower.contains("delimiter-only")
        || lower.contains("too generic")
        || lower.contains("old_string is too generic")
    {
        Some("delimiter_only_anchor")
    } else if lower.contains("placeholder ellipsis")
        || lower.contains("do not use `...`")
        || lower.contains("do not use ...")
    {
        Some("placeholder_ellipsis_anchor")
    } else {
        None
    }
}

pub async fn implement_validated_suggestion_with_harness(
    repo_root: &Path,
    suggestion: &Suggestion,
    preview: &FixPreview,
    repo_memory: Option<String>,
    config: ImplementationHarnessConfig,
) -> anyhow::Result<ImplementationRunResult> {
    implement_validated_suggestion_with_harness_with_progress(
        repo_root,
        suggestion,
        preview,
        repo_memory,
        config,
        |_, _, _| {},
    )
    .await
}

pub async fn implement_validated_suggestion_with_harness_with_progress<F>(
    repo_root: &Path,
    suggestion: &Suggestion,
    preview: &FixPreview,
    repo_memory: Option<String>,
    config: ImplementationHarnessConfig,
    mut on_progress: F,
) -> anyhow::Result<ImplementationRunResult>
where
    F: FnMut(usize, usize, &ImplementationAttemptDiagnostics),
{
    if suggestion.validation_state != SuggestionValidationState::Validated {
        return Err(anyhow::anyhow!(
            "Implementation harness only accepts validated suggestions"
        ));
    }

    let repo_root = repo_root.canonicalize().map_err(|e| {
        anyhow::anyhow!(
            "Failed to resolve repo root '{}': {}",
            repo_root.display(),
            e
        )
    })?;
    let run_id = Uuid::new_v4().to_string();
    let start = std::time::Instant::now();
    let global_budget = ImplementationBudget {
        started_at: start,
        max_total_ms: config.max_total_ms,
        max_total_cost_usd: config.max_total_cost_usd,
    };
    let mut usage: Option<Usage> = None;
    let mut attempts = Vec::new();
    let mut pass_payload: Option<AttemptPassPayload> = None;
    let mut feedback_reasons: Vec<String> = Vec::new();
    let mut last_quick_check_failure_fingerprint: Option<String> = None;
    let mut reduced_confidence = false;
    let allowed_files: HashSet<PathBuf> = suggestion
        .affected_files()
        .into_iter()
        .cloned()
        .collect::<HashSet<_>>();
    let blocking_severities = config
        .review_blocking_severities
        .iter()
        .map(|s| s.to_ascii_lowercase())
        .collect::<HashSet<_>>();
    let attempt_weights = attempt_budget_weights(config.max_attempts.max(1));

    for attempt_index in 1..=config.max_attempts.max(1) {
        if let Some(reason) = global_budget.guard_before_llm_call(&usage) {
            feedback_reasons.push(reason.message);
            break;
        }

        let (attempt_budget_ms, attempt_budget_cost_usd) =
            compute_attempt_budget_caps(&global_budget, &usage, attempt_index, &attempt_weights);

        let feedback = if feedback_reasons.is_empty() {
            None
        } else {
            Some(
                feedback_reasons
                    .iter()
                    .take(4)
                    .map(|r| format!("- {}", r))
                    .collect::<Vec<_>>()
                    .join("\n"),
            )
        };

        let attempt = run_attempt(
            &repo_root,
            suggestion,
            preview,
            repo_memory.clone(),
            &allowed_files,
            &blocking_severities,
            &config,
            &global_budget,
            attempt_budget_ms,
            attempt_budget_cost_usd,
            &usage,
            attempt_index,
            &run_id,
            feedback.as_deref(),
        )
        .await?;
        usage = merge_usage(usage, attempt.usage.clone());
        feedback_reasons = feedback_reasons_for_next_attempt(&attempt.diagnostics);
        if attempt.diagnostics.quick_check_status == ImplementationQuickCheckStatus::Unavailable
            && attempt
                .diagnostics
                .gates
                .iter()
                .any(|gate| gate.gate == "quick_check")
        {
            reduced_confidence = true;
        }
        on_progress(
            attempt_index,
            config.max_attempts.max(1),
            &attempt.diagnostics,
        );
        let current_quick_check_failure_fingerprint =
            attempt_quick_check_failure_fingerprint(&attempt.diagnostics);
        let repeated_quick_check_failure = current_quick_check_failure_fingerprint
            .as_deref()
            .map(|fp| last_quick_check_failure_fingerprint.as_deref() == Some(fp))
            .unwrap_or(false);
        if let Some(fp) = current_quick_check_failure_fingerprint {
            last_quick_check_failure_fingerprint = Some(fp);
        }
        if attempt.pass_payload.is_some() {
            pass_payload = attempt.pass_payload;
            attempts.push(attempt.diagnostics);
            break;
        }
        if repeated_quick_check_failure {
            feedback_reasons.push(
                "Quick checks kept failing for the same reason across attempts, so Cosmos stopped to avoid repeating low-value retries."
                    .to_string(),
            );
            attempts.push(attempt.diagnostics);
            break;
        }
        attempts.push(attempt.diagnostics);
    }

    if config.fail_on_reduced_confidence && reduced_confidence {
        feedback_reasons.push(
            "Quick checks were unavailable at least once and strict policy blocks reduced-confidence passes"
                .to_string(),
        );
        pass_payload = None;
    }

    let mut diagnostics = ImplementationRunDiagnostics {
        run_id: run_id.clone(),
        suggestion_id: suggestion.id.to_string(),
        suggestion_summary: suggestion.summary.clone(),
        model: IMPLEMENTATION_MODEL.id().to_string(),
        strict_mode: true,
        passed: pass_payload.is_some(),
        attempt_count: attempts.len(),
        total_ms: start.elapsed().as_millis() as u64,
        total_cost_usd: usage.as_ref().map(|u| u.cost()).unwrap_or(0.0),
        reduced_confidence,
        fail_reasons: Vec::new(),
        attempts,
        report_path: None,
        finalization: ImplementationFinalizationDiagnostics::default(),
    };

    if !diagnostics.passed {
        diagnostics.fail_reasons = feedback_reasons.clone();
        if diagnostics.fail_reasons.is_empty() {
            diagnostics
                .fail_reasons
                .push("No passing attempt completed within harness budget".to_string());
        }
    }

    let report_path = write_harness_report(&repo_root, &diagnostics)?;
    diagnostics.report_path = Some(report_path.clone());

    if let Some(payload) = pass_payload {
        return Ok(ImplementationRunResult {
            description: payload.description,
            file_changes: payload.file_changes,
            usage,
            diagnostics,
        });
    }

    let summary = diagnostics
        .fail_reasons
        .iter()
        .take(3)
        .cloned()
        .collect::<Vec<_>>()
        .join("; ");
    Ok(ImplementationRunResult {
        description: if summary.is_empty() {
            "Implementation harness failed to produce a safe passing fix.".to_string()
        } else {
            format!(
                "Implementation harness failed to produce a safe passing fix: {}",
                summary
            )
        },
        file_changes: Vec::new(),
        usage,
        diagnostics,
    })
}

#[allow(clippy::too_many_arguments)]
async fn run_attempt(
    repo_root: &Path,
    suggestion: &Suggestion,
    preview: &FixPreview,
    repo_memory: Option<String>,
    allowed_files: &HashSet<PathBuf>,
    blocking_severities: &HashSet<String>,
    config: &ImplementationHarnessConfig,
    global_budget: &ImplementationBudget,
    attempt_budget_ms: u64,
    attempt_budget_cost_usd: f64,
    usage_so_far: &Option<Usage>,
    attempt_index: usize,
    run_id: &str,
    feedback: Option<&str>,
) -> anyhow::Result<AttemptExecution> {
    let attempt_start = std::time::Instant::now();
    let attempt_budget = ImplementationBudget {
        started_at: attempt_start,
        max_total_ms: attempt_budget_ms.max(1),
        max_total_cost_usd: attempt_budget_cost_usd.max(0.0),
    };
    let mut gates = Vec::new();
    let mut fail_reasons = Vec::new();
    let mut fail_reason_records = Vec::new();
    let mut usage: Option<Usage> = None;
    let mut notes = Vec::new();
    let mut llm_calls: Vec<ImplementationLlmCallRecord> = Vec::new();
    // Detect the repo's quick-check command up-front so diagnostics can still surface it even if
    // the attempt fails before reaching the quick-check gate (e.g. budget exhaustion during generation).
    let detected_quick_check = detect_quick_check_command(repo_root);
    let detected_quick_check_command = detected_quick_check.as_ref().map(command_to_string);

    if let Some(reason) = global_budget.guard_before_llm_call(usage_so_far) {
        notes.push("budget_exceeded".to_string());
        push_fail_reason(
            &mut fail_reasons,
            &mut fail_reason_records,
            &reason.gate,
            &reason.code,
            reason.message.clone(),
        );
        push_gate(
            &mut gates,
            "budget",
            false,
            reason.message,
            Some(REASON_BUDGET_EXCEEDED),
        );
        let diag = ImplementationAttemptDiagnostics {
            attempt_index,
            passed: false,
            fail_reasons,
            fail_reason_records,
            gates,
            changed_files: Vec::new(),
            changed_lines_total: 0,
            changed_lines_by_file: HashMap::new(),
            quick_check_status: ImplementationQuickCheckStatus::Unavailable,
            quick_check_command: detected_quick_check_command.clone(),
            quick_check_outcome: None,
            quick_check_outcomes: Vec::new(),
            quick_check_fix_loops: 0,
            quick_check_failure_summary: None,
            review_iterations: 0,
            review_blocking_remaining: 0,
            remaining_blocking_titles: Vec::new(),
            remaining_blocking_categories: Vec::new(),
            attempt_ms: attempt_start.elapsed().as_millis() as u64,
            attempt_cost_usd: 0.0,
            llm_calls,
            notes,
        };
        return Ok(AttemptExecution {
            diagnostics: diag,
            usage: None,
            pass_payload: None,
        });
    }

    // In lab/CI strict mode we require quick checks to be detectable. If the repo's quick-check
    // command isn't detectable or can't even start (missing toolchain), fail early before any LLM
    // calls to avoid wasting budget on an attempt that cannot pass policy.
    if config.require_quick_check_detectable {
        let (quick_command, tool_required, tool_ok) = match detected_quick_check.as_ref() {
            None => (None, None, false),
            Some(cmd) => {
                let quick_command = Some(command_to_string(cmd));
                match cmd {
                    QuickCheckCommand::Shell(_) => (
                        quick_command,
                        Some("sh".to_string()),
                        program_available_on_path("sh"),
                    ),
                    QuickCheckCommand::Program { program, .. } if program == "python3" => (
                        quick_command,
                        Some("python3 or python".to_string()),
                        program_available_on_path("python3") || program_available_on_path("python"),
                    ),
                    QuickCheckCommand::Program { program, .. } => (
                        quick_command,
                        Some(program.clone()),
                        program_available_on_path(program),
                    ),
                }
            }
        };
        if !tool_ok {
            let message = if detected_quick_check.is_none() {
                "No quick-check command could be detected for this repo, and strict policy requires quick checks."
                    .to_string()
            } else {
                format!(
                    "Quick checks require '{}' but it isn't available in this environment.",
                    tool_required.unwrap_or_else(|| "required tool".to_string())
                )
            };
            notes.push("quick_check_unavailable".to_string());
            push_fail_reason(
                &mut fail_reasons,
                &mut fail_reason_records,
                "quick_check",
                REASON_QUICK_CHECK_UNAVAILABLE,
                message.clone(),
            );
            push_gate(
                &mut gates,
                "quick_check",
                false,
                message,
                Some(REASON_QUICK_CHECK_UNAVAILABLE),
            );
            let diag = ImplementationAttemptDiagnostics {
                attempt_index,
                passed: false,
                fail_reasons,
                fail_reason_records,
                gates,
                changed_files: Vec::new(),
                changed_lines_total: 0,
                changed_lines_by_file: HashMap::new(),
                quick_check_status: ImplementationQuickCheckStatus::Unavailable,
                quick_check_command: quick_command,
                quick_check_outcome: None,
                quick_check_outcomes: Vec::new(),
                quick_check_fix_loops: 0,
                quick_check_failure_summary: None,
                review_iterations: 0,
                review_blocking_remaining: 0,
                remaining_blocking_titles: Vec::new(),
                remaining_blocking_categories: Vec::new(),
                attempt_ms: attempt_start.elapsed().as_millis() as u64,
                attempt_cost_usd: 0.0,
                llm_calls,
                notes,
            };
            return Ok(AttemptExecution {
                diagnostics: diag,
                usage: None,
                pass_payload: None,
            });
        }
    }

    let sandbox_label = format!("apply-attempt-{}-{}", attempt_index, run_id);
    let sandbox = match SandboxSession::create(repo_root, run_id, &sandbox_label, false) {
        Ok(s) => s,
        Err(err) => {
            let message = format!("Failed to create sandbox worktree: {}", err);
            push_fail_reason(
                &mut fail_reasons,
                &mut fail_reason_records,
                "sandbox",
                "sandbox_create_failed",
                message.clone(),
            );
            push_gate(
                &mut gates,
                "sandbox",
                false,
                message,
                Some("sandbox_create_failed"),
            );
            let diag = ImplementationAttemptDiagnostics {
                attempt_index,
                passed: false,
                fail_reasons,
                fail_reason_records,
                gates,
                changed_files: Vec::new(),
                changed_lines_total: 0,
                changed_lines_by_file: HashMap::new(),
                quick_check_status: ImplementationQuickCheckStatus::Unavailable,
                quick_check_command: detected_quick_check_command.clone(),
                quick_check_outcome: None,
                quick_check_outcomes: Vec::new(),
                quick_check_fix_loops: 0,
                quick_check_failure_summary: None,
                review_iterations: 0,
                review_blocking_remaining: 0,
                remaining_blocking_titles: Vec::new(),
                remaining_blocking_categories: Vec::new(),
                attempt_ms: attempt_start.elapsed().as_millis() as u64,
                attempt_cost_usd: 0.0,
                llm_calls,
                notes,
            };
            return Ok(AttemptExecution {
                diagnostics: diag,
                usage: None,
                pass_payload: None,
            });
        }
    };

    let mut feedback_preview = preview.clone();
    if config.enable_quick_check_baseline {
        let baseline_timeout_ms = config.quick_check_timeout_ms.min(
            attempt_budget
                .remaining_ms()
                .saturating_sub(BUDGET_TIMEOUT_SLACK_MS)
                .max(1),
        );
        let (baseline_status, baseline_command, baseline_outcome) = run_quick_checks(
            sandbox.path(),
            Some(repo_root),
            &mut notes,
            config.quick_checks_mode,
            baseline_timeout_ms,
        )?;
        if baseline_status == ImplementationQuickCheckStatus::Failed {
            let summary = baseline_outcome
                .as_ref()
                .and_then(summarize_quick_check_failure);
            note_quick_check_failure_fingerprint(&mut notes, summary.as_deref());

            let baseline_paths = baseline_outcome
                .as_ref()
                .map(|outcome| extract_quick_check_error_paths(outcome, sandbox.path()))
                .unwrap_or_default();
            let has_in_scope_baseline_path = baseline_paths
                .iter()
                .any(|path| allowed_files.contains(path));
            if !has_in_scope_baseline_path {
                notes.push("baseline_quick_check_failfast".to_string());
                push_fail_reason(
                    &mut fail_reasons,
                    &mut fail_reason_records,
                    "quick_check",
                    REASON_QUICK_CHECK_FAILED,
                    "Quick checks were already failing before Cosmos made changes, and the failure is outside this suggestion's scoped files",
                );
                push_gate(
                    &mut gates,
                    "quick_check",
                    false,
                    "Pre-existing quick-check failure unrelated to scoped files".to_string(),
                    Some(REASON_QUICK_CHECK_FAILED),
                );
                let _ = sandbox.cleanup();
                let attempt_cost_usd = usage.as_ref().map(|u| u.cost()).unwrap_or(0.0);
                let mut quick_check_outcomes = Vec::new();
                if let Some(outcome) = baseline_outcome.clone() {
                    quick_check_outcomes.push(outcome);
                }
                let diag = ImplementationAttemptDiagnostics {
                    attempt_index,
                    passed: false,
                    fail_reasons,
                    fail_reason_records,
                    gates,
                    changed_files: Vec::new(),
                    changed_lines_total: 0,
                    changed_lines_by_file: HashMap::new(),
                    quick_check_status: baseline_status,
                    quick_check_command: baseline_command,
                    quick_check_outcome: baseline_outcome,
                    quick_check_outcomes,
                    quick_check_fix_loops: 0,
                    quick_check_failure_summary: summary,
                    review_iterations: 0,
                    review_blocking_remaining: 0,
                    remaining_blocking_titles: Vec::new(),
                    remaining_blocking_categories: Vec::new(),
                    attempt_ms: attempt_start.elapsed().as_millis() as u64,
                    attempt_cost_usd,
                    llm_calls,
                    notes,
                };
                return Ok(AttemptExecution {
                    diagnostics: diag,
                    usage,
                    pass_payload: None,
                });
            }
        }
    }
    if let Some(feedback) = feedback {
        feedback_preview.modifier = Some(match feedback_preview.modifier.as_deref() {
            Some(existing) if !existing.trim().is_empty() => {
                format!(
                    "{}\n\nHarness feedback from previous attempt:\n{}",
                    existing, feedback
                )
            }
            _ => format!("Harness feedback from previous attempt:\n{}", feedback),
        });
    }

    if let Some(reason) = attempt_budget.guard_before_llm_call(&usage) {
        notes.push("budget_exceeded".to_string());
        push_fail_reason(
            &mut fail_reasons,
            &mut fail_reason_records,
            &reason.gate,
            &reason.code,
            reason.message.clone(),
        );
        push_gate(
            &mut gates,
            "budget",
            false,
            reason.message,
            Some(REASON_BUDGET_EXCEEDED),
        );
        let _ = sandbox.cleanup();
        let diag = ImplementationAttemptDiagnostics {
            attempt_index,
            passed: false,
            fail_reasons,
            fail_reason_records,
            gates,
            changed_files: Vec::new(),
            changed_lines_total: 0,
            changed_lines_by_file: HashMap::new(),
            quick_check_status: ImplementationQuickCheckStatus::Unavailable,
            quick_check_command: detected_quick_check_command.clone(),
            quick_check_outcome: None,
            quick_check_outcomes: Vec::new(),
            quick_check_fix_loops: 0,
            quick_check_failure_summary: None,
            review_iterations: 0,
            review_blocking_remaining: 0,
            remaining_blocking_titles: Vec::new(),
            remaining_blocking_categories: Vec::new(),
            attempt_ms: attempt_start.elapsed().as_millis() as u64,
            attempt_cost_usd: usage.as_ref().map(|u| u.cost()).unwrap_or(0.0),
            llm_calls,
            notes,
        };
        return Ok(AttemptExecution {
            diagnostics: diag,
            usage,
            pass_payload: None,
        });
    }

    let generation_timeout_ms = attempt_budget
        .timeout_ms_for_next_llm_call()
        .min(MAX_GENERATION_TIMEOUT_MS);
    let generation = tokio::time::timeout(
        Duration::from_millis(generation_timeout_ms),
        generate_attempt_candidate(
            sandbox.path(),
            suggestion,
            &feedback_preview,
            repo_memory.clone(),
            allowed_files,
            &mut llm_calls,
            generation_timeout_ms,
            IMPLEMENTATION_MODEL,
            None,
        ),
    )
    .await;

    let generation = match generation {
        Ok(result) => result,
        Err(_) => {
            let message = format!(
                "Stopped to respect the configured time budget (generation timed out after {}ms; limit {}ms)",
                generation_timeout_ms, attempt_budget.max_total_ms
            );
            notes.push("budget_exceeded".to_string());
            push_fail_reason(
                &mut fail_reasons,
                &mut fail_reason_records,
                "budget",
                REASON_BUDGET_EXCEEDED,
                message.clone(),
            );
            push_gate(
                &mut gates,
                "budget",
                false,
                message,
                Some(REASON_BUDGET_EXCEEDED),
            );
            let _ = sandbox.cleanup();
            let attempt_cost_usd = usage.as_ref().map(|u| u.cost()).unwrap_or(0.0);
            let diag = ImplementationAttemptDiagnostics {
                attempt_index,
                passed: false,
                fail_reasons,
                fail_reason_records,
                gates,
                changed_files: Vec::new(),
                changed_lines_total: 0,
                changed_lines_by_file: HashMap::new(),
                quick_check_status: ImplementationQuickCheckStatus::Unavailable,
                quick_check_command: detected_quick_check_command.clone(),
                quick_check_outcome: None,
                quick_check_outcomes: Vec::new(),
                quick_check_fix_loops: 0,
                quick_check_failure_summary: None,
                review_iterations: 0,
                review_blocking_remaining: 0,
                remaining_blocking_titles: Vec::new(),
                remaining_blocking_categories: Vec::new(),
                attempt_ms: attempt_start.elapsed().as_millis() as u64,
                attempt_cost_usd,
                llm_calls,
                notes,
            };
            return Ok(AttemptExecution {
                diagnostics: diag,
                usage,
                pass_payload: None,
            });
        }
    };

    let mut generated = match generation {
        Ok(value) => value,
        Err(err) => {
            usage = merge_usage(usage, usage_from_generation_error(&err));
            let first_error_text = err.to_string();
            let escalation_reason = generation_escalation_reason(&first_error_text)
                .filter(|_| config.max_smart_escalations_per_attempt > 0);

            if let Some(escalation_reason) = escalation_reason {
                if let Some(reason) = attempt_budget.guard_before_llm_call(&usage) {
                    notes.push("smart_escalation_skipped_budget".to_string());
                    notes.push("budget_exceeded".to_string());
                    push_fail_reason(
                        &mut fail_reasons,
                        &mut fail_reason_records,
                        &reason.gate,
                        &reason.code,
                        reason.message.clone(),
                    );
                    push_gate(
                        &mut gates,
                        "budget",
                        false,
                        reason.message,
                        Some(REASON_BUDGET_EXCEEDED),
                    );
                    let _ = sandbox.cleanup();
                    let attempt_cost_usd = usage.as_ref().map(|u| u.cost()).unwrap_or(0.0);
                    let diag = ImplementationAttemptDiagnostics {
                        attempt_index,
                        passed: false,
                        fail_reasons,
                        fail_reason_records,
                        gates,
                        changed_files: Vec::new(),
                        changed_lines_total: 0,
                        changed_lines_by_file: HashMap::new(),
                        quick_check_status: ImplementationQuickCheckStatus::Unavailable,
                        quick_check_command: detected_quick_check_command.clone(),
                        quick_check_outcome: None,
                        quick_check_outcomes: Vec::new(),
                        quick_check_fix_loops: 0,
                        quick_check_failure_summary: None,
                        review_iterations: 0,
                        review_blocking_remaining: 0,
                        remaining_blocking_titles: Vec::new(),
                        remaining_blocking_categories: Vec::new(),
                        attempt_ms: attempt_start.elapsed().as_millis() as u64,
                        attempt_cost_usd,
                        llm_calls,
                        notes,
                    };
                    return Ok(AttemptExecution {
                        diagnostics: diag,
                        usage,
                        pass_payload: None,
                    });
                }

                notes.push(format!("smart_escalation:generation:{}", escalation_reason));
                let escalation_timeout_ms = attempt_budget
                    .timeout_ms_for_next_llm_call()
                    .min(MAX_GENERATION_TIMEOUT_MS);
                let escalation = tokio::time::timeout(
                    Duration::from_millis(escalation_timeout_ms),
                    generate_attempt_candidate(
                        sandbox.path(),
                        suggestion,
                        &feedback_preview,
                        repo_memory.clone(),
                        allowed_files,
                        &mut llm_calls,
                        escalation_timeout_ms,
                        Model::Smart,
                        Some(escalation_reason),
                    ),
                )
                .await;
                let escalation = match escalation {
                    Ok(value) => value,
                    Err(_) => {
                        let message = format!(
                            "Stopped to respect the configured time budget (smart-escalated generation timed out after {}ms; limit {}ms)",
                            escalation_timeout_ms, attempt_budget.max_total_ms
                        );
                        notes.push("budget_exceeded".to_string());
                        push_fail_reason(
                            &mut fail_reasons,
                            &mut fail_reason_records,
                            "budget",
                            REASON_BUDGET_EXCEEDED,
                            message.clone(),
                        );
                        push_gate(
                            &mut gates,
                            "budget",
                            false,
                            message,
                            Some(REASON_BUDGET_EXCEEDED),
                        );
                        let _ = sandbox.cleanup();
                        let attempt_cost_usd = usage.as_ref().map(|u| u.cost()).unwrap_or(0.0);
                        let diag = ImplementationAttemptDiagnostics {
                            attempt_index,
                            passed: false,
                            fail_reasons,
                            fail_reason_records,
                            gates,
                            changed_files: Vec::new(),
                            changed_lines_total: 0,
                            changed_lines_by_file: HashMap::new(),
                            quick_check_status: ImplementationQuickCheckStatus::Unavailable,
                            quick_check_command: detected_quick_check_command.clone(),
                            quick_check_outcome: None,
                            quick_check_outcomes: Vec::new(),
                            quick_check_fix_loops: 0,
                            quick_check_failure_summary: None,
                            review_iterations: 0,
                            review_blocking_remaining: 0,
                            remaining_blocking_titles: Vec::new(),
                            remaining_blocking_categories: Vec::new(),
                            attempt_ms: attempt_start.elapsed().as_millis() as u64,
                            attempt_cost_usd,
                            llm_calls,
                            notes,
                        };
                        return Ok(AttemptExecution {
                            diagnostics: diag,
                            usage,
                            pass_payload: None,
                        });
                    }
                };
                match escalation {
                    Ok(value) => value,
                    Err(escalation_err) => {
                        usage = merge_usage(usage, usage_from_generation_error(&escalation_err));
                        let attempt_cost_usd = usage.as_ref().map(|u| u.cost()).unwrap_or(0.0);
                        let message = truncate(
                            &format!(
                                "Generation failed after smart escalation: {}",
                                escalation_err
                            ),
                            700,
                        );
                        push_fail_reason(
                            &mut fail_reasons,
                            &mut fail_reason_records,
                            "generation",
                            "generation_failed",
                            message.clone(),
                        );
                        push_gate(
                            &mut gates,
                            "generation",
                            false,
                            message,
                            Some("generation_failed"),
                        );
                        let _ = sandbox.cleanup();
                        let diag = ImplementationAttemptDiagnostics {
                            attempt_index,
                            passed: false,
                            fail_reasons,
                            fail_reason_records,
                            gates,
                            changed_files: Vec::new(),
                            changed_lines_total: 0,
                            changed_lines_by_file: HashMap::new(),
                            quick_check_status: ImplementationQuickCheckStatus::Unavailable,
                            quick_check_command: detected_quick_check_command.clone(),
                            quick_check_outcome: None,
                            quick_check_outcomes: Vec::new(),
                            quick_check_fix_loops: 0,
                            quick_check_failure_summary: None,
                            review_iterations: 0,
                            review_blocking_remaining: 0,
                            remaining_blocking_titles: Vec::new(),
                            remaining_blocking_categories: Vec::new(),
                            attempt_ms: attempt_start.elapsed().as_millis() as u64,
                            attempt_cost_usd,
                            llm_calls,
                            notes,
                        };
                        return Ok(AttemptExecution {
                            diagnostics: diag,
                            usage,
                            pass_payload: None,
                        });
                    }
                }
            } else {
                let attempt_cost_usd = usage.as_ref().map(|u| u.cost()).unwrap_or(0.0);
                let message = truncate(&format!("Generation failed: {}", first_error_text), 700);
                push_fail_reason(
                    &mut fail_reasons,
                    &mut fail_reason_records,
                    "generation",
                    "generation_failed",
                    message.clone(),
                );
                push_gate(
                    &mut gates,
                    "generation",
                    false,
                    message,
                    Some("generation_failed"),
                );
                let _ = sandbox.cleanup();
                let diag = ImplementationAttemptDiagnostics {
                    attempt_index,
                    passed: false,
                    fail_reasons,
                    fail_reason_records,
                    gates,
                    changed_files: Vec::new(),
                    changed_lines_total: 0,
                    changed_lines_by_file: HashMap::new(),
                    quick_check_status: ImplementationQuickCheckStatus::Unavailable,
                    quick_check_command: detected_quick_check_command.clone(),
                    quick_check_outcome: None,
                    quick_check_outcomes: Vec::new(),
                    quick_check_fix_loops: 0,
                    quick_check_failure_summary: None,
                    review_iterations: 0,
                    review_blocking_remaining: 0,
                    remaining_blocking_titles: Vec::new(),
                    remaining_blocking_categories: Vec::new(),
                    attempt_ms: attempt_start.elapsed().as_millis() as u64,
                    attempt_cost_usd,
                    llm_calls,
                    notes,
                };
                return Ok(AttemptExecution {
                    diagnostics: diag,
                    usage,
                    pass_payload: None,
                });
            }
        }
    };

    usage = merge_usage(usage, generated.usage.take());
    if let Some(reason) = attempt_budget.exhausted(&usage) {
        notes.push("budget_exceeded".to_string());
        push_fail_reason(
            &mut fail_reasons,
            &mut fail_reason_records,
            &reason.gate,
            &reason.code,
            reason.message.clone(),
        );
        push_gate(
            &mut gates,
            "budget",
            false,
            reason.message,
            Some(REASON_BUDGET_EXCEEDED),
        );
        let _ = sandbox.cleanup();
        let attempt_cost_usd = usage.as_ref().map(|u| u.cost()).unwrap_or(0.0);
        let diag = ImplementationAttemptDiagnostics {
            attempt_index,
            passed: false,
            fail_reasons,
            fail_reason_records,
            gates,
            changed_files: Vec::new(),
            changed_lines_total: 0,
            changed_lines_by_file: HashMap::new(),
            quick_check_status: ImplementationQuickCheckStatus::Unavailable,
            quick_check_command: detected_quick_check_command.clone(),
            quick_check_outcome: None,
            quick_check_outcomes: Vec::new(),
            quick_check_fix_loops: 0,
            quick_check_failure_summary: None,
            review_iterations: 0,
            review_blocking_remaining: 0,
            remaining_blocking_titles: Vec::new(),
            remaining_blocking_categories: Vec::new(),
            attempt_ms: attempt_start.elapsed().as_millis() as u64,
            attempt_cost_usd,
            llm_calls,
            notes,
        };
        return Ok(AttemptExecution {
            diagnostics: diag,
            usage,
            pass_payload: None,
        });
    }
    let mut repo_changes = collect_repo_changes(sandbox.path())?;
    repo_changes.files.sort();
    let out_of_scope_files = repo_changes
        .files
        .iter()
        .filter(|path| !allowed_files.contains(*path))
        .cloned()
        .collect::<Vec<_>>();
    if !out_of_scope_files.is_empty() {
        match revert_out_of_scope_changes(sandbox.path(), &repo_changes, &out_of_scope_files) {
            Ok(()) => {
                notes.push(format!(
                    "reverted_out_of_scope_files: {}",
                    out_of_scope_files
                        .iter()
                        .map(|p| p.display().to_string())
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
                repo_changes = collect_repo_changes(sandbox.path())?;
                repo_changes.files.sort();
            }
            Err(err) => {
                notes.push(format!(
                    "revert_out_of_scope_failed: {}",
                    truncate(&err.to_string(), 180)
                ));
            }
        }
    }
    let scope_ok = deterministic_scope_gate(&repo_changes.files, allowed_files);
    push_gate(
        &mut gates,
        "scope",
        scope_ok,
        if scope_ok {
            format!("{} files changed in attempt", repo_changes.files.len())
        } else {
            format!(
                "Found out-of-scope file changes: {}",
                repo_changes
                    .files
                    .iter()
                    .filter(|p| !allowed_files.contains(*p))
                    .map(|p| p.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        },
        if scope_ok {
            None
        } else {
            Some(REASON_SCOPE_VIOLATION)
        },
    );
    if !scope_ok {
        push_fail_reason(
            &mut fail_reasons,
            &mut fail_reason_records,
            "scope",
            REASON_SCOPE_VIOLATION,
            "Attempt changed files outside the validated suggestion scope",
        );
    }

    if repo_changes.files.is_empty() {
        push_gate(
            &mut gates,
            "non_empty_diff",
            false,
            "No file changes produced",
            Some(REASON_NON_EMPTY_DIFF),
        );
        push_fail_reason(
            &mut fail_reasons,
            &mut fail_reason_records,
            "non_empty_diff",
            REASON_NON_EMPTY_DIFF,
            "Attempt produced no code changes",
        );
    } else {
        push_gate(
            &mut gates,
            "non_empty_diff",
            true,
            "Code changes detected",
            None,
        );
    }

    if repo_changes.files.len() > config.max_changed_files {
        push_fail_reason(
            &mut fail_reasons,
            &mut fail_reason_records,
            "diff_budget",
            REASON_DIFF_BUDGET_VIOLATION,
            format!(
                "Attempt changed {} files (limit {})",
                repo_changes.files.len(),
                config.max_changed_files
            ),
        );
    }

    let (mut changed_total, mut changed_by_file) =
        compute_changed_lines(sandbox.path(), &repo_changes.files, &repo_changes.untracked)?;
    if changed_total > config.max_total_changed_lines {
        push_fail_reason(
            &mut fail_reasons,
            &mut fail_reason_records,
            "diff_budget",
            REASON_DIFF_BUDGET_VIOLATION,
            format!(
                "Attempt changed {} lines total (limit {})",
                changed_total, config.max_total_changed_lines
            ),
        );
    }
    for (file, count) in &changed_by_file {
        if *count > config.max_changed_lines_per_file {
            push_fail_reason(
                &mut fail_reasons,
                &mut fail_reason_records,
                "diff_budget",
                REASON_DIFF_BUDGET_VIOLATION,
                format!(
                    "{} changed {} lines (limit {})",
                    file.display(),
                    count,
                    config.max_changed_lines_per_file
                ),
            );
        }
    }
    let diff_budget_ok = !fail_reason_records
        .iter()
        .any(|reason| reason.gate == "diff_budget" && reason.code == REASON_DIFF_BUDGET_VIOLATION);
    push_gate(
        &mut gates,
        "diff_budget",
        diff_budget_ok,
        if diff_budget_ok {
            "Diff-size budgets passed".to_string()
        } else {
            "Diff-size budgets exceeded".to_string()
        },
        if diff_budget_ok {
            None
        } else {
            Some(REASON_DIFF_BUDGET_VIOLATION)
        },
    );

    // Parse/syntax gate with a bounded in-attempt repair loop. This converts common
    // "attempt 2 fixes it" cases into "attempt 1 repairs it" without expanding scope.
    let mut syntax_err = syntax_gate(sandbox.path(), &repo_changes.files).err();
    if syntax_err.is_some() && fail_reasons.is_empty() {
        let mut syntax_fix_loops_done = 0usize;
        while syntax_err.is_some() && syntax_fix_loops_done < config.max_auto_syntax_fix_loops {
            syntax_fix_loops_done = syntax_fix_loops_done.saturating_add(1);
            notes.push(format!("syntax_fix_loop_{}", syntax_fix_loops_done));

            let failures = collect_syntax_failures(sandbox.path(), &repo_changes.files);
            if failures.is_empty() {
                // We couldn't attribute the parse failure to a specific file; don't
                // spend more budget guessing.
                break;
            }

            for (target, parse_error) in failures.into_iter() {
                if let Some(reason) = attempt_budget.guard_before_llm_call(&usage) {
                    notes.push("budget_exceeded".to_string());
                    push_fail_reason(
                        &mut fail_reasons,
                        &mut fail_reason_records,
                        &reason.gate,
                        &reason.code,
                        reason.message.clone(),
                    );
                    push_gate(
                        &mut gates,
                        "budget",
                        false,
                        reason.message,
                        Some(REASON_BUDGET_EXCEEDED),
                    );
                    break;
                }

                let resolved =
                    resolve_repo_path_allow_new(sandbox.path(), &target).map_err(|e| {
                        anyhow::anyhow!("Unsafe syntax repair path {}: {}", target.display(), e)
                    })?;
                let current_content = match std::fs::read_to_string(&resolved.absolute) {
                    Ok(content) => content,
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
                    Err(e) => {
                        push_fail_reason(
                            &mut fail_reasons,
                            &mut fail_reason_records,
                            "syntax",
                            REASON_SYNTAX_VIOLATION,
                            format!(
                                "Syntax auto-repair failed reading {}: {}",
                                target.display(),
                                truncate(&e.to_string(), 180)
                            ),
                        );
                        break;
                    }
                };
                let is_new_file = !resolved.absolute.exists() || current_content.trim().is_empty();

                let mut repair_preview = feedback_preview.clone();
                repair_preview.modifier = Some(format_syntax_repair_modifier(
                    feedback_preview.modifier.as_deref(),
                    &parse_error,
                    &target,
                ));

                ensure_implementation_model(IMPLEMENTATION_MODEL)?;
                let repair_timeout_ms = attempt_budget
                    .timeout_ms_for_next_llm_call()
                    .min(MAX_FIX_TIMEOUT_MS);
                let fix = tokio::time::timeout(
                    Duration::from_millis(repair_timeout_ms),
                    generate_fix_content_with_model(
                        &target,
                        &current_content,
                        suggestion,
                        &repair_preview,
                        repo_memory.clone(),
                        is_new_file,
                        IMPLEMENTATION_MODEL,
                        repair_timeout_ms,
                    ),
                )
                .await;
                let fix = match fix {
                    Ok(Ok(value)) => {
                        llm_calls.push(ImplementationLlmCallRecord {
                            kind: "syntax_repair".to_string(),
                            independence_role: Some("implementation".to_string()),
                            escalation_reason: None,
                            model: IMPLEMENTATION_MODEL.id().to_string(),
                            timeout_ms: value
                                .speed_failover
                                .as_ref()
                                .map(|d| d.total_timeout_ms)
                                .unwrap_or(repair_timeout_ms),
                            schema_fallback_used: false,
                            speed_failover: value.speed_failover.clone(),
                            error: None,
                        });
                        value
                    }
                    Ok(Err(err)) => {
                        let speed_failover = err
                            .downcast_ref::<SpeedFailoverError>()
                            .map(|e| e.diagnostics.clone())
                            .or_else(|| {
                                err.downcast_ref::<FixGenerationErrorWithUsage>()
                                    .and_then(|e| e.speed_failover.clone())
                            });
                        if let Some(u) = err
                            .downcast_ref::<FixGenerationErrorWithUsage>()
                            .and_then(|e| e.usage.clone())
                        {
                            usage = merge_usage(usage, Some(u));
                        }
                        llm_calls.push(ImplementationLlmCallRecord {
                            kind: "syntax_repair".to_string(),
                            independence_role: Some("implementation".to_string()),
                            escalation_reason: None,
                            model: IMPLEMENTATION_MODEL.id().to_string(),
                            timeout_ms: speed_failover
                                .as_ref()
                                .map(|d| d.total_timeout_ms)
                                .unwrap_or(repair_timeout_ms),
                            schema_fallback_used: false,
                            speed_failover,
                            error: Some(truncate(&err.to_string(), 240)),
                        });
                        push_fail_reason(
                            &mut fail_reasons,
                            &mut fail_reason_records,
                            "syntax",
                            REASON_SYNTAX_VIOLATION,
                            format!(
                                "Syntax auto-repair failed: {}",
                                truncate(&err.to_string(), 180)
                            ),
                        );
                        break;
                    }
                    Err(_) => {
                        llm_calls.push(ImplementationLlmCallRecord {
                            kind: "syntax_repair".to_string(),
                            independence_role: Some("implementation".to_string()),
                            escalation_reason: None,
                            model: IMPLEMENTATION_MODEL.id().to_string(),
                            timeout_ms: repair_timeout_ms,
                            schema_fallback_used: false,
                            speed_failover: None,
                            error: Some(format!("Timed out after {}ms", repair_timeout_ms)),
                        });
                        notes.push("budget_exceeded".to_string());
                        let message = format!(
                            "Stopped to respect the configured time budget (syntax repair timed out after {}ms; limit {}ms)",
                            repair_timeout_ms, attempt_budget.max_total_ms
                        );
                        push_fail_reason(
                            &mut fail_reasons,
                            &mut fail_reason_records,
                            "budget",
                            REASON_BUDGET_EXCEEDED,
                            message.clone(),
                        );
                        push_gate(
                            &mut gates,
                            "budget",
                            false,
                            message,
                            Some(REASON_BUDGET_EXCEEDED),
                        );
                        break;
                    }
                };
                usage = merge_usage(usage, fix.usage.clone());
                if let Some(reason) = attempt_budget.exhausted(&usage) {
                    notes.push("budget_exceeded".to_string());
                    push_fail_reason(
                        &mut fail_reasons,
                        &mut fail_reason_records,
                        &reason.gate,
                        &reason.code,
                        reason.message.clone(),
                    );
                    push_gate(
                        &mut gates,
                        "budget",
                        false,
                        reason.message,
                        Some(REASON_BUDGET_EXCEEDED),
                    );
                    break;
                }

                if let Some(parent) = resolved.absolute.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                std::fs::write(&resolved.absolute, &fix.new_content).map_err(|e| {
                    anyhow::anyhow!("Failed writing syntax repair {}: {}", target.display(), e)
                })?;
                generated
                    .modified_areas_by_file
                    .entry(target.clone())
                    .or_default()
                    .extend(fix.modified_areas.clone());
            }

            if !fail_reasons.is_empty() {
                break;
            }

            // Re-check diff budgets after syntax repair to ensure repairs don't bloat the diff.
            repo_changes = collect_repo_changes(sandbox.path())?;
            repo_changes.files.sort();
            let (new_total, new_by_file) = compute_changed_lines(
                sandbox.path(),
                &repo_changes.files,
                &repo_changes.untracked,
            )?;
            changed_total = new_total;
            changed_by_file = new_by_file;
            let diff_budget_ok_after = repo_changes.files.len() <= config.max_changed_files
                && changed_total <= config.max_total_changed_lines
                && changed_by_file
                    .iter()
                    .all(|(_f, c)| *c <= config.max_changed_lines_per_file);
            upsert_gate(
                &mut gates,
                "diff_budget",
                diff_budget_ok_after,
                if diff_budget_ok_after {
                    "Diff-size budgets passed".to_string()
                } else {
                    "Diff-size budgets exceeded".to_string()
                },
                if diff_budget_ok_after {
                    None
                } else {
                    Some(REASON_DIFF_BUDGET_VIOLATION)
                },
            );
            if !diff_budget_ok_after {
                push_fail_reason(
                    &mut fail_reasons,
                    &mut fail_reason_records,
                    "diff_budget",
                    REASON_DIFF_BUDGET_VIOLATION,
                    "Syntax repair exceeded configured diff-size budgets",
                );
                break;
            }

            syntax_err = syntax_gate(sandbox.path(), &repo_changes.files).err();
        }
    }

    let syntax_ok = syntax_err.is_none();
    push_gate(
        &mut gates,
        "syntax",
        syntax_ok,
        if syntax_ok {
            "All changed files parsed successfully".to_string()
        } else {
            "Parse failures detected in changed files".to_string()
        },
        if syntax_ok {
            None
        } else {
            Some(REASON_SYNTAX_VIOLATION)
        },
    );
    if let Some(err) = syntax_err {
        push_fail_reason(
            &mut fail_reasons,
            &mut fail_reason_records,
            "syntax",
            REASON_SYNTAX_VIOLATION,
            err,
        );
    }

    // If deterministic gates already failed, don't spend more time/cost running review or checks.
    // This keeps budgets meaningful and avoids muddying failure reasons with downstream noise.
    if !fail_reasons.is_empty() {
        notes.push("attempt_failed_before_review".to_string());
        let _ = sandbox.cleanup();
        let attempt_cost_usd = usage.as_ref().map(|u| u.cost()).unwrap_or(0.0);
        let diagnostics = ImplementationAttemptDiagnostics {
            attempt_index,
            passed: false,
            fail_reasons,
            fail_reason_records,
            gates,
            changed_files: repo_changes.files,
            changed_lines_total: changed_total,
            changed_lines_by_file: changed_by_file,
            quick_check_status: ImplementationQuickCheckStatus::Unavailable,
            quick_check_command: detected_quick_check_command.clone(),
            quick_check_outcome: None,
            quick_check_outcomes: Vec::new(),
            quick_check_fix_loops: 0,
            quick_check_failure_summary: None,
            review_iterations: 0,
            review_blocking_remaining: 0,
            remaining_blocking_titles: Vec::new(),
            remaining_blocking_categories: Vec::new(),
            attempt_ms: attempt_start.elapsed().as_millis() as u64,
            attempt_cost_usd,
            llm_calls,
            notes,
        };
        return Ok(AttemptExecution {
            pass_payload: None,
            diagnostics,
            usage,
        });
    }

    // Run quick checks before the (LLM) review gate. This prevents spending review budget on code
    // that doesn't even build/typecheck, and improves first-attempt pass rate by repairing common
    // compiler/typechecker failures in-attempt.
    let mut quick_check_outcomes: Vec<ImplementationCommandOutcome> = Vec::new();
    let mut quick_check_fix_loops = 0usize;
    let mut quick_check_failure_summary: Option<String> = None;

    let mut files_changed_set = repo_changes.files.iter().cloned().collect::<HashSet<_>>();
    let mut final_changed_files = repo_changes.files.clone();
    final_changed_files.sort();

    let pre_review_quick_check_timeout_ms = config.quick_check_timeout_ms.min(
        attempt_budget
            .remaining_ms()
            .saturating_sub(BUDGET_TIMEOUT_SLACK_MS)
            .max(1),
    );
    let (mut quick_status, mut quick_command, mut quick_outcome) = run_quick_checks(
        sandbox.path(),
        Some(repo_root),
        &mut notes,
        config.quick_checks_mode,
        pre_review_quick_check_timeout_ms,
    )?;

    if let Some(outcome) = quick_outcome.clone() {
        if quick_status == ImplementationQuickCheckStatus::Failed {
            quick_check_failure_summary = summarize_quick_check_failure(&outcome);
            note_quick_check_failure_fingerprint(
                &mut notes,
                quick_check_failure_summary.as_deref(),
            );
        } else {
            quick_check_failure_summary = None;
        }
        quick_check_outcomes.push(outcome);
    }

    let eligible_for_pre_review_quick_check_repair = quick_status
        == ImplementationQuickCheckStatus::Failed
        && config.max_auto_quick_check_fix_loops > 0
        && fail_reasons.is_empty();
    if eligible_for_pre_review_quick_check_repair {
        let remaining_loops = config
            .max_auto_quick_check_fix_loops
            .saturating_sub(quick_check_fix_loops);
        if let Some(reason) = reserve_budget_for_quick_check_repair(
            &attempt_budget,
            &usage,
            config.reserve_independent_review_ms,
            config.reserve_independent_review_cost_usd,
        ) {
            notes.push("budget_exceeded".to_string());
            push_fail_reason(
                &mut fail_reasons,
                &mut fail_reason_records,
                &reason.gate,
                &reason.code,
                reason.message.clone(),
            );
            push_gate(
                &mut gates,
                "budget",
                false,
                reason.message,
                Some(REASON_BUDGET_EXCEEDED),
            );
        }
        let mut previous_failure_fingerprint = quick_check_failure_summary
            .as_deref()
            .map(quick_check_failure_fingerprint);
        for _ in 0..remaining_loops {
            if !fail_reasons.is_empty() {
                break;
            }
            let Some(outcome) = quick_outcome.as_ref() else {
                break;
            };
            let candidates = extract_quick_check_error_paths(outcome, sandbox.path());
            let mut target = candidates.into_iter().find(|path| {
                if config.quick_check_fix_requires_in_scope_error {
                    allowed_files.contains(path)
                } else {
                    allowed_files.contains(path) || files_changed_set.contains(path)
                }
            });
            if target.is_none() && files_changed_set.len() == 1 {
                if let Some(only) = files_changed_set.iter().next().cloned() {
                    if allowed_files.contains(&only) {
                        notes.push("quick_check_repair_fallback_single_changed_file".to_string());
                        target = Some(only);
                    }
                }
            }
            let Some(target) = target else {
                notes.push("quick_check_repair_skipped_no_in_scope_error_path".to_string());
                break;
            };

            quick_check_fix_loops = quick_check_fix_loops.saturating_add(1);
            notes.push(format!("quick_check_fix_loop_{}", quick_check_fix_loops));

            // Deterministic JS fast-paths: prefer local tool fixes (Prettier/ESLint --fix)
            // before spending LLM budget on likely formatting/lint-only failures.
            let mut repaired_by_tool = false;
            if is_prettier_formatting_failure(outcome) {
                let prettier_timeout_ms = 15_000.min(
                    attempt_budget
                        .remaining_ms()
                        .saturating_sub(BUDGET_TIMEOUT_SLACK_MS)
                        .max(1),
                );
                match run_prettier_write(sandbox.path(), &target, prettier_timeout_ms) {
                    Ok(prettier_outcome) => {
                        notes.push(format!(
                            "quick_check_prettier_write_{}",
                            if prettier_outcome.success {
                                "ok"
                            } else {
                                "failed"
                            }
                        ));
                        repaired_by_tool = prettier_outcome.success;
                    }
                    Err(err) => {
                        notes.push(format!(
                            "quick_check_prettier_write_failed: {}",
                            truncate(&err.to_string(), 180)
                        ));
                    }
                }
            }
            if !repaired_by_tool && is_eslint_fixable_failure(outcome) {
                let eslint_timeout_ms = 15_000.min(
                    attempt_budget
                        .remaining_ms()
                        .saturating_sub(BUDGET_TIMEOUT_SLACK_MS)
                        .max(1),
                );
                match run_eslint_fix(sandbox.path(), &target, eslint_timeout_ms) {
                    Ok(eslint_outcome) => {
                        notes.push(format!(
                            "quick_check_eslint_fix_{}",
                            if eslint_outcome.success {
                                "ok"
                            } else {
                                "failed"
                            }
                        ));
                        repaired_by_tool = eslint_outcome.success;
                    }
                    Err(err) => {
                        notes.push(format!(
                            "quick_check_eslint_fix_failed: {}",
                            truncate(&err.to_string(), 180)
                        ));
                    }
                }
            }
            if repaired_by_tool {
                files_changed_set.insert(target.clone());
                generated
                    .modified_areas_by_file
                    .entry(target.clone())
                    .or_default();

                final_changed_files = files_changed_set.iter().cloned().collect::<Vec<_>>();
                final_changed_files.sort();
                if let Err(err) = syntax_gate(sandbox.path(), &final_changed_files) {
                    push_fail_reason(
                        &mut fail_reasons,
                        &mut fail_reason_records,
                        "syntax",
                        REASON_SYNTAX_VIOLATION,
                        err,
                    );
                    break;
                }

                let (status, command, outcome) = run_quick_checks(
                    sandbox.path(),
                    Some(repo_root),
                    &mut notes,
                    config.quick_checks_mode,
                    config.quick_check_timeout_ms.min(
                        attempt_budget
                            .remaining_ms()
                            .saturating_sub(BUDGET_TIMEOUT_SLACK_MS)
                            .max(1),
                    ),
                )?;
                quick_status = status;
                quick_command = command;
                quick_outcome = outcome;
                if let Some(outcome) = quick_outcome.clone() {
                    if quick_status == ImplementationQuickCheckStatus::Failed {
                        quick_check_failure_summary = summarize_quick_check_failure(&outcome);
                        note_quick_check_failure_fingerprint(
                            &mut notes,
                            quick_check_failure_summary.as_deref(),
                        );
                    } else {
                        quick_check_failure_summary = None;
                    }
                    quick_check_outcomes.push(outcome);
                }
                if quick_status == ImplementationQuickCheckStatus::Failed {
                    continue;
                }
                break;
            }

            if let Some(reason) = attempt_budget.guard_before_llm_call(&usage) {
                notes.push("budget_exceeded".to_string());
                push_fail_reason(
                    &mut fail_reasons,
                    &mut fail_reason_records,
                    &reason.gate,
                    &reason.code,
                    reason.message.clone(),
                );
                push_gate(
                    &mut gates,
                    "budget",
                    false,
                    reason.message,
                    Some(REASON_BUDGET_EXCEEDED),
                );
                break;
            }

            let resolved = resolve_repo_path_allow_new(sandbox.path(), &target).map_err(|e| {
                anyhow::anyhow!("Unsafe quick-check repair path {}: {}", target.display(), e)
            })?;
            let current_content = match std::fs::read_to_string(&resolved.absolute) {
                Ok(content) => content,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
                Err(e) => {
                    push_fail_reason(
                        &mut fail_reasons,
                        &mut fail_reason_records,
                        "quick_check",
                        REASON_QUICK_CHECK_FAILED,
                        format!(
                            "Quick check auto-repair failed reading {}: {}",
                            target.display(),
                            truncate(&e.to_string(), 180)
                        ),
                    );
                    break;
                }
            };
            let is_new_file = !resolved.absolute.exists() || current_content.trim().is_empty();

            let mut repair_preview = feedback_preview.clone();
            let error_summary = quick_check_failure_summary
                .as_deref()
                .unwrap_or("Quick checks failed");
            let target_context_excerpt = quick_check_target_context_excerpt(
                sandbox.path(),
                outcome,
                &target,
                &current_content,
            );
            let repair_hint =
                quick_check_repair_hint_from_summary(error_summary).unwrap_or_default();
            repair_preview.modifier = Some(format_quick_check_repair_modifier(
                feedback_preview.modifier.as_deref(),
                error_summary,
                outcome,
                &target,
                target_context_excerpt.as_deref(),
                if repair_hint.is_empty() {
                    None
                } else {
                    Some(repair_hint.as_str())
                },
            ));
            if !is_new_file {
                if let Some((_, ln, _col)) =
                    extract_quick_check_error_locations(outcome, sandbox.path())
                        .into_iter()
                        .find(|(path, _, _)| path == &target)
                {
                    repair_preview.evidence_line = Some(ln);
                    repair_preview.evidence_snippet = snippet_around_line(&current_content, ln, 8);
                }
            }
            if repair_preview.evidence_snippet.is_none() {
                if let Some(extra) =
                    quick_check_read_only_context_excerpt(sandbox.path(), outcome, &target)
                {
                    repair_preview.modifier = Some(format!(
                        "{}\n\n{}",
                        repair_preview.modifier.clone().unwrap_or_default(),
                        extra
                    ));
                }
            }

            ensure_implementation_model(IMPLEMENTATION_MODEL)?;
            let repair_timeout_ms = attempt_budget
                .timeout_ms_for_next_llm_call()
                .min(MAX_FIX_TIMEOUT_MS);
            let fix = tokio::time::timeout(
                Duration::from_millis(repair_timeout_ms),
                generate_fix_content_with_model(
                    &target,
                    &current_content,
                    suggestion,
                    &repair_preview,
                    repo_memory.clone(),
                    is_new_file,
                    IMPLEMENTATION_MODEL,
                    repair_timeout_ms,
                ),
            )
            .await;
            let fix = match fix {
                Ok(Ok(value)) => {
                    llm_calls.push(ImplementationLlmCallRecord {
                        kind: "quick_check_repair".to_string(),
                        independence_role: Some("implementation".to_string()),
                        escalation_reason: None,
                        model: IMPLEMENTATION_MODEL.id().to_string(),
                        timeout_ms: value
                            .speed_failover
                            .as_ref()
                            .map(|d| d.total_timeout_ms)
                            .unwrap_or(repair_timeout_ms),
                        schema_fallback_used: false,
                        speed_failover: value.speed_failover.clone(),
                        error: None,
                    });
                    value
                }
                Ok(Err(err)) => {
                    let speed_failover = err
                        .downcast_ref::<SpeedFailoverError>()
                        .map(|e| e.diagnostics.clone())
                        .or_else(|| {
                            err.downcast_ref::<FixGenerationErrorWithUsage>()
                                .and_then(|e| e.speed_failover.clone())
                        });
                    if let Some(u) = err
                        .downcast_ref::<FixGenerationErrorWithUsage>()
                        .and_then(|e| e.usage.clone())
                    {
                        usage = merge_usage(usage, Some(u));
                    }
                    llm_calls.push(ImplementationLlmCallRecord {
                        kind: "quick_check_repair".to_string(),
                        independence_role: Some("implementation".to_string()),
                        escalation_reason: None,
                        model: IMPLEMENTATION_MODEL.id().to_string(),
                        timeout_ms: speed_failover
                            .as_ref()
                            .map(|d| d.total_timeout_ms)
                            .unwrap_or(repair_timeout_ms),
                        schema_fallback_used: false,
                        speed_failover,
                        error: Some(truncate(&err.to_string(), 240)),
                    });
                    push_fail_reason(
                        &mut fail_reasons,
                        &mut fail_reason_records,
                        "quick_check",
                        REASON_QUICK_CHECK_FAILED,
                        format!(
                            "Quick check auto-repair failed: {}",
                            truncate(&err.to_string(), 180)
                        ),
                    );
                    break;
                }
                Err(_) => {
                    llm_calls.push(ImplementationLlmCallRecord {
                        kind: "quick_check_repair".to_string(),
                        independence_role: Some("implementation".to_string()),
                        escalation_reason: None,
                        model: IMPLEMENTATION_MODEL.id().to_string(),
                        timeout_ms: repair_timeout_ms,
                        schema_fallback_used: false,
                        speed_failover: None,
                        error: Some(format!("Timed out after {}ms", repair_timeout_ms)),
                    });
                    notes.push("budget_exceeded".to_string());
                    let message = format!(
                        "Stopped to respect the configured time budget (quick-check repair timed out after {}ms; limit {}ms)",
                        repair_timeout_ms, attempt_budget.max_total_ms
                    );
                    push_fail_reason(
                        &mut fail_reasons,
                        &mut fail_reason_records,
                        "budget",
                        REASON_BUDGET_EXCEEDED,
                        message.clone(),
                    );
                    push_gate(
                        &mut gates,
                        "budget",
                        false,
                        message,
                        Some(REASON_BUDGET_EXCEEDED),
                    );
                    break;
                }
            };
            usage = merge_usage(usage, fix.usage.clone());
            if let Some(reason) = attempt_budget.exhausted(&usage) {
                notes.push("budget_exceeded".to_string());
                push_fail_reason(
                    &mut fail_reasons,
                    &mut fail_reason_records,
                    &reason.gate,
                    &reason.code,
                    reason.message.clone(),
                );
                push_gate(
                    &mut gates,
                    "budget",
                    false,
                    reason.message,
                    Some(REASON_BUDGET_EXCEEDED),
                );
                break;
            }

            if let Some(parent) = resolved.absolute.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&resolved.absolute, &fix.new_content).map_err(|e| {
                anyhow::anyhow!(
                    "Failed writing quick-check repair {}: {}",
                    target.display(),
                    e
                )
            })?;
            files_changed_set.insert(target.clone());
            generated
                .modified_areas_by_file
                .entry(target.clone())
                .or_default()
                .extend(fix.modified_areas.clone());

            final_changed_files = files_changed_set.iter().cloned().collect::<Vec<_>>();
            final_changed_files.sort();

            if let Err(err) = syntax_gate(sandbox.path(), &final_changed_files) {
                push_fail_reason(
                    &mut fail_reasons,
                    &mut fail_reason_records,
                    "syntax",
                    REASON_SYNTAX_VIOLATION,
                    err,
                );
                break;
            }

            // Re-run quick checks immediately after repair. Review only runs once we have a
            // candidate that builds/typechecks.
            let (status, command, outcome) = run_quick_checks(
                sandbox.path(),
                Some(repo_root),
                &mut notes,
                config.quick_checks_mode,
                config.quick_check_timeout_ms.min(
                    attempt_budget
                        .remaining_ms()
                        .saturating_sub(BUDGET_TIMEOUT_SLACK_MS)
                        .max(1),
                ),
            )?;
            quick_status = status;
            quick_command = command;
            quick_outcome = outcome;
            if let Some(outcome) = quick_outcome.clone() {
                if quick_status == ImplementationQuickCheckStatus::Failed {
                    quick_check_failure_summary = summarize_quick_check_failure(&outcome);
                    note_quick_check_failure_fingerprint(
                        &mut notes,
                        quick_check_failure_summary.as_deref(),
                    );
                } else {
                    quick_check_failure_summary = None;
                }
                quick_check_outcomes.push(outcome);
            }
            if quick_status == ImplementationQuickCheckStatus::Failed {
                let current_fingerprint = quick_check_failure_summary
                    .as_deref()
                    .map(quick_check_failure_fingerprint);
                if current_fingerprint.is_some()
                    && previous_failure_fingerprint.as_ref() == current_fingerprint.as_ref()
                {
                    notes.push("quick_check_repair_stopped_no_progress".to_string());
                    break;
                }
                previous_failure_fingerprint = current_fingerprint;
                continue;
            }

            break;
        }
    }

    // If quick checks don't pass policy, fail early before spending review budget.
    if quick_status == ImplementationQuickCheckStatus::Failed {
        push_fail_reason(
            &mut fail_reasons,
            &mut fail_reason_records,
            "quick_check",
            REASON_QUICK_CHECK_FAILED,
            "Quick project checks failed",
        );
    } else if quick_status == ImplementationQuickCheckStatus::Unavailable {
        notes.push("quick_check_unavailable".to_string());
        if config.require_quick_check_detectable {
            push_fail_reason(
                &mut fail_reasons,
                &mut fail_reason_records,
                "quick_check",
                REASON_QUICK_CHECK_UNAVAILABLE,
                "Quick checks were unavailable and strict policy requires a detectable check command",
            );
        }
    }
    let quick_check_ok_pre = quick_check_passes_policy(quick_status, config);
    if !quick_check_ok_pre {
        let quick_reason_code = match quick_status {
            ImplementationQuickCheckStatus::Passed => None,
            ImplementationQuickCheckStatus::Failed => Some(REASON_QUICK_CHECK_FAILED),
            ImplementationQuickCheckStatus::Unavailable
                if config.require_quick_check_detectable =>
            {
                Some(REASON_QUICK_CHECK_UNAVAILABLE)
            }
            ImplementationQuickCheckStatus::Unavailable => None,
        };
        push_gate(
            &mut gates,
            "quick_check",
            quick_check_ok_pre,
            match quick_status {
                ImplementationQuickCheckStatus::Passed => "Quick checks passed".to_string(),
                ImplementationQuickCheckStatus::Failed => "Quick checks failed".to_string(),
                ImplementationQuickCheckStatus::Unavailable => {
                    "No detectable quick-check command".to_string()
                }
            },
            quick_reason_code,
        );
        notes.push("attempt_failed_before_review".to_string());
        let _ = sandbox.cleanup();
        let attempt_cost_usd = usage.as_ref().map(|u| u.cost()).unwrap_or(0.0);
        let diagnostics = ImplementationAttemptDiagnostics {
            attempt_index,
            passed: false,
            fail_reasons,
            fail_reason_records,
            gates,
            changed_files: final_changed_files,
            changed_lines_total: changed_total,
            changed_lines_by_file: changed_by_file,
            quick_check_status: quick_status,
            quick_check_command: quick_command,
            quick_check_outcome: quick_outcome,
            quick_check_outcomes,
            quick_check_fix_loops,
            quick_check_failure_summary,
            review_iterations: 0,
            review_blocking_remaining: 0,
            remaining_blocking_titles: Vec::new(),
            remaining_blocking_categories: Vec::new(),
            attempt_ms: attempt_start.elapsed().as_millis() as u64,
            attempt_cost_usd,
            llm_calls,
            notes,
        };
        return Ok(AttemptExecution {
            pass_payload: None,
            diagnostics,
            usage,
        });
    }

    // LLM review gate (adversarial) runs only after the code builds/typechecks.
    let mut review_iterations = 0usize;
    let mut blocking_remaining = 0usize;
    let mut remaining_blocking_titles = Vec::new();
    let mut remaining_blocking_categories = Vec::new();
    let mut fixed_titles = Vec::new();
    let review_result = run_review_gate(
        sandbox.path(),
        suggestion,
        &generated.description,
        &generated.old_contents,
        &final_changed_files,
        &mut llm_calls,
        repo_memory.clone(),
        quick_status,
        quick_command.as_deref(),
        blocking_severities,
        config.adversarial_review_model.as_model(),
        config.require_independent_review_on_pass,
        config.max_auto_review_fix_loops,
        &attempt_budget,
        &mut usage,
        &mut review_iterations,
        &mut blocking_remaining,
        &mut remaining_blocking_titles,
        &mut remaining_blocking_categories,
        &mut fixed_titles,
        &mut files_changed_set,
    )
    .await;
    let mut review_budget_exceeded = false;
    let mut review_budget_message: Option<String> = None;
    let mut review_error: Option<String> = None;
    match &review_result {
        Ok(()) => {}
        Err(ReviewGateError::BudgetExceeded(reason)) => {
            review_budget_exceeded = true;
            review_budget_message = Some(reason.message.clone());
            notes.push("budget_exceeded".to_string());
            push_fail_reason(
                &mut fail_reasons,
                &mut fail_reason_records,
                &reason.gate,
                &reason.code,
                reason.message.clone(),
            );
            push_gate(
                &mut gates,
                "budget",
                false,
                reason.message.clone(),
                Some(REASON_BUDGET_EXCEEDED),
            );
        }
        Err(ReviewGateError::Failed(err)) => {
            review_error = Some(err.clone());
            push_fail_reason(
                &mut fail_reasons,
                &mut fail_reason_records,
                "review",
                REASON_BLOCKING_REVIEW_RESIDUAL,
                err.clone(),
            );
        }
    }

    let review_ok = !review_budget_exceeded && blocking_remaining == 0 && review_result.is_ok();
    push_gate(
        &mut gates,
        "review",
        review_ok,
        if review_ok {
            format!(
                "Review gate passed after {} iteration(s)",
                review_iterations.max(1)
            )
        } else if review_budget_exceeded {
            review_budget_message
                .clone()
                .unwrap_or_else(|| "Stopped to respect the configured budget".to_string())
        } else if let Some(err) = review_error.as_deref() {
            err.to_string()
        } else if blocking_remaining > 0 {
            format!(
                "Review found {} blocking finding(s) after {} iteration(s)",
                blocking_remaining,
                review_iterations.max(1)
            )
        } else {
            "Review failed".to_string()
        },
        if review_ok {
            None
        } else if review_budget_exceeded {
            Some(REASON_BUDGET_EXCEEDED)
        } else {
            Some(REASON_BLOCKING_REVIEW_RESIDUAL)
        },
    );
    if !review_budget_exceeded && review_result.is_ok() && blocking_remaining > 0 {
        push_fail_reason(
            &mut fail_reasons,
            &mut fail_reason_records,
            "review",
            REASON_BLOCKING_REVIEW_RESIDUAL,
            format!(
                "Blocking review findings remained after {} auto-fix loop(s)",
                config.max_auto_review_fix_loops
            ),
        );
    }

    final_changed_files = files_changed_set.iter().cloned().collect::<Vec<_>>();
    final_changed_files.sort();

    // If review failed (including due to budget exhaustion), stop immediately. Downstream gates
    // are only meaningful for passing candidates. Record the passing quick-check gate snapshot
    // for transparency since we won't reach the post-review quick-check phase.
    if !fail_reasons.is_empty() {
        if !gates.iter().any(|gate| gate.gate == "quick_check") {
            push_gate(
                &mut gates,
                "quick_check",
                true,
                match quick_status {
                    ImplementationQuickCheckStatus::Passed => "Quick checks passed".to_string(),
                    ImplementationQuickCheckStatus::Failed => "Quick checks failed".to_string(),
                    ImplementationQuickCheckStatus::Unavailable => {
                        "No detectable quick-check command".to_string()
                    }
                },
                None,
            );
        }
        notes.push("attempt_failed_after_review".to_string());
        let _ = sandbox.cleanup();
        let attempt_cost_usd = usage.as_ref().map(|u| u.cost()).unwrap_or(0.0);
        let diagnostics = ImplementationAttemptDiagnostics {
            attempt_index,
            passed: false,
            fail_reasons,
            fail_reason_records,
            gates,
            changed_files: final_changed_files,
            changed_lines_total: changed_total,
            changed_lines_by_file: changed_by_file,
            quick_check_status: quick_status,
            quick_check_command: quick_command,
            quick_check_outcome: quick_outcome,
            quick_check_outcomes,
            quick_check_fix_loops,
            quick_check_failure_summary,
            review_iterations,
            review_blocking_remaining: blocking_remaining,
            remaining_blocking_titles,
            remaining_blocking_categories,
            attempt_ms: attempt_start.elapsed().as_millis() as u64,
            attempt_cost_usd,
            llm_calls,
            notes,
        };
        return Ok(AttemptExecution {
            pass_payload: None,
            diagnostics,
            usage,
        });
    }

    let binary_ok = match binary_write_gate(sandbox.path(), &final_changed_files) {
        Ok(()) => true,
        Err(err) => {
            push_fail_reason(
                &mut fail_reasons,
                &mut fail_reason_records,
                "binary_write",
                REASON_BINARY_WRITE_VIOLATION,
                err,
            );
            false
        }
    };
    push_gate(
        &mut gates,
        "binary_write",
        binary_ok,
        if binary_ok {
            "No binary writes detected".to_string()
        } else {
            "Binary write attempt detected or non-UTF-8 output produced".to_string()
        },
        if binary_ok {
            None
        } else {
            Some(REASON_BINARY_WRITE_VIOLATION)
        },
    );
    let post_review_syntax = syntax_gate(sandbox.path(), &final_changed_files);
    if let Err(err) = post_review_syntax {
        push_fail_reason(
            &mut fail_reasons,
            &mut fail_reason_records,
            "post_review_syntax",
            REASON_SYNTAX_VIOLATION,
            err,
        );
        push_gate(
            &mut gates,
            "post_review_syntax",
            false,
            "Post-review parse failures detected",
            Some(REASON_SYNTAX_VIOLATION),
        );
    } else {
        push_gate(
            &mut gates,
            "post_review_syntax",
            true,
            "Post-review parse gate passed",
            None,
        );
    }

    // Fail-fast if we already know this attempt cannot pass.
    if !fail_reasons.is_empty() {
        notes.push("attempt_failed_before_quick_check".to_string());
        if !gates.iter().any(|gate| gate.gate == "quick_check") {
            push_gate(
                &mut gates,
                "quick_check",
                true,
                match quick_status {
                    ImplementationQuickCheckStatus::Passed => "Quick checks passed".to_string(),
                    ImplementationQuickCheckStatus::Failed => "Quick checks failed".to_string(),
                    ImplementationQuickCheckStatus::Unavailable => {
                        "No detectable quick-check command".to_string()
                    }
                },
                None,
            );
        }
        let _ = sandbox.cleanup();
        let attempt_cost_usd = usage.as_ref().map(|u| u.cost()).unwrap_or(0.0);
        let diagnostics = ImplementationAttemptDiagnostics {
            attempt_index,
            passed: false,
            fail_reasons,
            fail_reason_records,
            gates,
            changed_files: final_changed_files,
            changed_lines_total: changed_total,
            changed_lines_by_file: changed_by_file,
            quick_check_status: quick_status,
            quick_check_command: quick_command.clone(),
            quick_check_outcome: quick_outcome.clone(),
            quick_check_outcomes: quick_check_outcomes.clone(),
            quick_check_fix_loops,
            quick_check_failure_summary: quick_check_failure_summary.clone(),
            review_iterations,
            review_blocking_remaining: blocking_remaining,
            remaining_blocking_titles,
            remaining_blocking_categories,
            attempt_ms: attempt_start.elapsed().as_millis() as u64,
            attempt_cost_usd,
            llm_calls,
            notes,
        };
        return Ok(AttemptExecution {
            pass_payload: None,
            diagnostics,
            usage,
        });
    }

    if let Some(reason) = attempt_budget.exhausted(&usage) {
        notes.push("budget_exceeded".to_string());
        push_fail_reason(
            &mut fail_reasons,
            &mut fail_reason_records,
            &reason.gate,
            &reason.code,
            reason.message.clone(),
        );
        push_gate(
            &mut gates,
            "budget",
            false,
            reason.message,
            Some(REASON_BUDGET_EXCEEDED),
        );
        if !gates.iter().any(|gate| gate.gate == "quick_check") {
            push_gate(
                &mut gates,
                "quick_check",
                true,
                match quick_status {
                    ImplementationQuickCheckStatus::Passed => "Quick checks passed".to_string(),
                    ImplementationQuickCheckStatus::Failed => "Quick checks failed".to_string(),
                    ImplementationQuickCheckStatus::Unavailable => {
                        "No detectable quick-check command".to_string()
                    }
                },
                None,
            );
        }
        let _ = sandbox.cleanup();
        let attempt_cost_usd = usage.as_ref().map(|u| u.cost()).unwrap_or(0.0);
        let diagnostics = ImplementationAttemptDiagnostics {
            attempt_index,
            passed: false,
            fail_reasons,
            fail_reason_records,
            gates,
            changed_files: final_changed_files,
            changed_lines_total: changed_total,
            changed_lines_by_file: changed_by_file,
            quick_check_status: quick_status,
            quick_check_command: quick_command.clone(),
            quick_check_outcome: quick_outcome.clone(),
            quick_check_outcomes: quick_check_outcomes.clone(),
            quick_check_fix_loops,
            quick_check_failure_summary: quick_check_failure_summary.clone(),
            review_iterations,
            review_blocking_remaining: blocking_remaining,
            remaining_blocking_titles,
            remaining_blocking_categories,
            attempt_ms: attempt_start.elapsed().as_millis() as u64,
            attempt_cost_usd,
            llm_calls,
            notes,
        };
        return Ok(AttemptExecution {
            pass_payload: None,
            diagnostics,
            usage,
        });
    }

    let quick_check_timeout_ms = config.quick_check_timeout_ms.min(
        attempt_budget
            .remaining_ms()
            .saturating_sub(BUDGET_TIMEOUT_SLACK_MS)
            .max(1),
    );
    let (status, command, outcome) = run_quick_checks(
        sandbox.path(),
        Some(repo_root),
        &mut notes,
        config.quick_checks_mode,
        quick_check_timeout_ms,
    )?;
    quick_status = status;
    quick_command = command;
    quick_outcome = outcome;

    if let Some(outcome) = quick_outcome.clone() {
        if quick_status == ImplementationQuickCheckStatus::Failed {
            quick_check_failure_summary = summarize_quick_check_failure(&outcome);
            note_quick_check_failure_fingerprint(
                &mut notes,
                quick_check_failure_summary.as_deref(),
            );
        } else {
            quick_check_failure_summary = None;
        }
        quick_check_outcomes.push(outcome);
    }

    let remaining_quick_check_fix_loops = config
        .max_auto_quick_check_fix_loops
        .saturating_sub(quick_check_fix_loops);
    let eligible_for_quick_check_repair = quick_status == ImplementationQuickCheckStatus::Failed
        && remaining_quick_check_fix_loops > 0
        && fail_reasons.is_empty();
    if eligible_for_quick_check_repair {
        if let Some(reason) = reserve_budget_for_quick_check_repair(
            &attempt_budget,
            &usage,
            config.reserve_independent_review_ms,
            config.reserve_independent_review_cost_usd,
        ) {
            notes.push("budget_exceeded".to_string());
            push_fail_reason(
                &mut fail_reasons,
                &mut fail_reason_records,
                &reason.gate,
                &reason.code,
                reason.message.clone(),
            );
            push_gate(
                &mut gates,
                "budget",
                false,
                reason.message,
                Some(REASON_BUDGET_EXCEEDED),
            );
        }
        let mut previous_failure_fingerprint = quick_check_failure_summary
            .as_deref()
            .map(quick_check_failure_fingerprint);
        for _ in 0..remaining_quick_check_fix_loops {
            if !fail_reasons.is_empty() {
                break;
            }
            let Some(outcome) = quick_outcome.as_ref() else {
                break;
            };
            let candidates = extract_quick_check_error_paths(outcome, sandbox.path());
            let mut target = candidates.into_iter().find(|path| {
                if config.quick_check_fix_requires_in_scope_error {
                    allowed_files.contains(path)
                } else {
                    allowed_files.contains(path) || files_changed_set.contains(path)
                }
            });
            if target.is_none() && files_changed_set.len() == 1 {
                if let Some(only) = files_changed_set.iter().next().cloned() {
                    if allowed_files.contains(&only) {
                        notes.push("quick_check_repair_fallback_single_changed_file".to_string());
                        target = Some(only);
                    }
                }
            }
            let Some(target) = target else {
                notes.push("quick_check_repair_skipped_no_in_scope_error_path".to_string());
                break;
            };

            quick_check_fix_loops = quick_check_fix_loops.saturating_add(1);
            notes.push(format!("quick_check_fix_loop_{}", quick_check_fix_loops));

            let mut repaired_by_tool = false;
            if is_prettier_formatting_failure(outcome) {
                let prettier_timeout_ms = 15_000.min(
                    attempt_budget
                        .remaining_ms()
                        .saturating_sub(BUDGET_TIMEOUT_SLACK_MS)
                        .max(1),
                );
                match run_prettier_write(sandbox.path(), &target, prettier_timeout_ms) {
                    Ok(prettier_outcome) => {
                        notes.push(format!(
                            "quick_check_prettier_write_{}",
                            if prettier_outcome.success {
                                "ok"
                            } else {
                                "failed"
                            }
                        ));
                        if prettier_outcome.success {
                            repaired_by_tool = true;
                            files_changed_set.insert(target.clone());
                            generated
                                .modified_areas_by_file
                                .entry(target.clone())
                                .or_default();
                        }
                    }
                    Err(err) => {
                        notes.push(format!(
                            "quick_check_prettier_write_failed: {}",
                            truncate(&err.to_string(), 180)
                        ));
                    }
                }
            }
            if !repaired_by_tool && is_eslint_fixable_failure(outcome) {
                let eslint_timeout_ms = 15_000.min(
                    attempt_budget
                        .remaining_ms()
                        .saturating_sub(BUDGET_TIMEOUT_SLACK_MS)
                        .max(1),
                );
                match run_eslint_fix(sandbox.path(), &target, eslint_timeout_ms) {
                    Ok(eslint_outcome) => {
                        notes.push(format!(
                            "quick_check_eslint_fix_{}",
                            if eslint_outcome.success {
                                "ok"
                            } else {
                                "failed"
                            }
                        ));
                        if eslint_outcome.success {
                            repaired_by_tool = true;
                            files_changed_set.insert(target.clone());
                            generated
                                .modified_areas_by_file
                                .entry(target.clone())
                                .or_default();
                        }
                    }
                    Err(err) => {
                        notes.push(format!(
                            "quick_check_eslint_fix_failed: {}",
                            truncate(&err.to_string(), 180)
                        ));
                    }
                }
            }

            if !repaired_by_tool {
                if let Some(reason) = attempt_budget.guard_before_llm_call(&usage) {
                    notes.push("budget_exceeded".to_string());
                    push_fail_reason(
                        &mut fail_reasons,
                        &mut fail_reason_records,
                        &reason.gate,
                        &reason.code,
                        reason.message.clone(),
                    );
                    push_gate(
                        &mut gates,
                        "budget",
                        false,
                        reason.message,
                        Some(REASON_BUDGET_EXCEEDED),
                    );
                    break;
                }

                let resolved =
                    resolve_repo_path_allow_new(sandbox.path(), &target).map_err(|e| {
                        anyhow::anyhow!(
                            "Unsafe quick-check repair path {}: {}",
                            target.display(),
                            e
                        )
                    })?;
                let current_content = match std::fs::read_to_string(&resolved.absolute) {
                    Ok(content) => content,
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
                    Err(e) => {
                        push_fail_reason(
                            &mut fail_reasons,
                            &mut fail_reason_records,
                            "quick_check",
                            REASON_QUICK_CHECK_FAILED,
                            format!(
                                "Quick check auto-repair failed reading {}: {}",
                                target.display(),
                                truncate(&e.to_string(), 180)
                            ),
                        );
                        break;
                    }
                };
                let is_new_file = !resolved.absolute.exists() || current_content.trim().is_empty();

                let mut repair_preview = feedback_preview.clone();
                let error_summary = quick_check_failure_summary
                    .as_deref()
                    .unwrap_or("Quick checks failed");
                let target_context_excerpt = quick_check_target_context_excerpt(
                    sandbox.path(),
                    outcome,
                    &target,
                    &current_content,
                );
                let repair_hint =
                    quick_check_repair_hint_from_summary(error_summary).unwrap_or_default();
                repair_preview.modifier = Some(format_quick_check_repair_modifier(
                    feedback_preview.modifier.as_deref(),
                    error_summary,
                    outcome,
                    &target,
                    target_context_excerpt.as_deref(),
                    if repair_hint.is_empty() {
                        None
                    } else {
                        Some(repair_hint.as_str())
                    },
                ));
                if !is_new_file {
                    if let Some((_, ln, _col)) =
                        extract_quick_check_error_locations(outcome, sandbox.path())
                            .into_iter()
                            .find(|(path, _, _)| path == &target)
                    {
                        repair_preview.evidence_line = Some(ln);
                        repair_preview.evidence_snippet =
                            snippet_around_line(&current_content, ln, 8);
                    }
                }
                if repair_preview.evidence_snippet.is_none() {
                    if let Some(extra) =
                        quick_check_read_only_context_excerpt(sandbox.path(), outcome, &target)
                    {
                        repair_preview.modifier = Some(format!(
                            "{}\n\n{}",
                            repair_preview.modifier.clone().unwrap_or_default(),
                            extra
                        ));
                    }
                }

                ensure_implementation_model(IMPLEMENTATION_MODEL)?;
                let repair_timeout_ms = attempt_budget
                    .timeout_ms_for_next_llm_call()
                    .min(MAX_FIX_TIMEOUT_MS);
                let fix = tokio::time::timeout(
                    Duration::from_millis(repair_timeout_ms),
                    generate_fix_content_with_model(
                        &target,
                        &current_content,
                        suggestion,
                        &repair_preview,
                        repo_memory.clone(),
                        is_new_file,
                        IMPLEMENTATION_MODEL,
                        repair_timeout_ms,
                    ),
                )
                .await;
                let fix = match fix {
                    Ok(Ok(value)) => {
                        llm_calls.push(ImplementationLlmCallRecord {
                            kind: "quick_check_repair".to_string(),
                            independence_role: Some("implementation".to_string()),
                            escalation_reason: None,
                            model: IMPLEMENTATION_MODEL.id().to_string(),
                            timeout_ms: value
                                .speed_failover
                                .as_ref()
                                .map(|d| d.total_timeout_ms)
                                .unwrap_or(repair_timeout_ms),
                            schema_fallback_used: false,
                            speed_failover: value.speed_failover.clone(),
                            error: None,
                        });
                        value
                    }
                    Ok(Err(err)) => {
                        let speed_failover = err
                            .downcast_ref::<SpeedFailoverError>()
                            .map(|e| e.diagnostics.clone())
                            .or_else(|| {
                                err.downcast_ref::<FixGenerationErrorWithUsage>()
                                    .and_then(|e| e.speed_failover.clone())
                            });
                        if let Some(u) = err
                            .downcast_ref::<FixGenerationErrorWithUsage>()
                            .and_then(|e| e.usage.clone())
                        {
                            usage = merge_usage(usage, Some(u));
                        }
                        llm_calls.push(ImplementationLlmCallRecord {
                            kind: "quick_check_repair".to_string(),
                            independence_role: Some("implementation".to_string()),
                            escalation_reason: None,
                            model: IMPLEMENTATION_MODEL.id().to_string(),
                            timeout_ms: speed_failover
                                .as_ref()
                                .map(|d| d.total_timeout_ms)
                                .unwrap_or(repair_timeout_ms),
                            schema_fallback_used: false,
                            speed_failover,
                            error: Some(truncate(&err.to_string(), 240)),
                        });
                        push_fail_reason(
                            &mut fail_reasons,
                            &mut fail_reason_records,
                            "quick_check",
                            REASON_QUICK_CHECK_FAILED,
                            format!(
                                "Quick check auto-repair failed: {}",
                                truncate(&err.to_string(), 180)
                            ),
                        );
                        break;
                    }
                    Err(_) => {
                        llm_calls.push(ImplementationLlmCallRecord {
                            kind: "quick_check_repair".to_string(),
                            independence_role: Some("implementation".to_string()),
                            escalation_reason: None,
                            model: IMPLEMENTATION_MODEL.id().to_string(),
                            timeout_ms: repair_timeout_ms,
                            schema_fallback_used: false,
                            speed_failover: None,
                            error: Some(format!("Timed out after {}ms", repair_timeout_ms)),
                        });
                        notes.push("budget_exceeded".to_string());
                        let message = format!(
                            "Stopped to respect the configured time budget (quick-check repair timed out after {}ms; limit {}ms)",
                            repair_timeout_ms, attempt_budget.max_total_ms
                        );
                        push_fail_reason(
                            &mut fail_reasons,
                            &mut fail_reason_records,
                            "budget",
                            REASON_BUDGET_EXCEEDED,
                            message.clone(),
                        );
                        push_gate(
                            &mut gates,
                            "budget",
                            false,
                            message,
                            Some(REASON_BUDGET_EXCEEDED),
                        );
                        break;
                    }
                };
                usage = merge_usage(usage, fix.usage.clone());
                if let Some(reason) = attempt_budget.exhausted(&usage) {
                    notes.push("budget_exceeded".to_string());
                    push_fail_reason(
                        &mut fail_reasons,
                        &mut fail_reason_records,
                        &reason.gate,
                        &reason.code,
                        reason.message.clone(),
                    );
                    push_gate(
                        &mut gates,
                        "budget",
                        false,
                        reason.message,
                        Some(REASON_BUDGET_EXCEEDED),
                    );
                    break;
                }

                if let Some(parent) = resolved.absolute.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                std::fs::write(&resolved.absolute, &fix.new_content).map_err(|e| {
                    anyhow::anyhow!(
                        "Failed writing quick-check repair {}: {}",
                        target.display(),
                        e
                    )
                })?;
                files_changed_set.insert(target.clone());
                generated
                    .modified_areas_by_file
                    .entry(target.clone())
                    .or_default()
                    .extend(fix.modified_areas.clone());
            }

            final_changed_files = files_changed_set.iter().cloned().collect::<Vec<_>>();
            final_changed_files.sort();

            if let Err(err) = syntax_gate(sandbox.path(), &final_changed_files) {
                push_fail_reason(
                    &mut fail_reasons,
                    &mut fail_reason_records,
                    "post_review_syntax",
                    REASON_SYNTAX_VIOLATION,
                    err,
                );
                break;
            }

            // Re-run the quick check immediately after repair. Don't spend review budget
            // until the code builds/typechecks. We'll do a single review rerun after the
            // quick check passes, to ensure the final code is still safe and correct.
            let (status, command, outcome) = run_quick_checks(
                sandbox.path(),
                Some(repo_root),
                &mut notes,
                config.quick_checks_mode,
                config.quick_check_timeout_ms.min(
                    attempt_budget
                        .remaining_ms()
                        .saturating_sub(BUDGET_TIMEOUT_SLACK_MS)
                        .max(1),
                ),
            )?;
            quick_status = status;
            quick_command = command;
            quick_outcome = outcome;
            if let Some(outcome) = quick_outcome.clone() {
                if quick_status == ImplementationQuickCheckStatus::Failed {
                    quick_check_failure_summary = summarize_quick_check_failure(&outcome);
                    note_quick_check_failure_fingerprint(
                        &mut notes,
                        quick_check_failure_summary.as_deref(),
                    );
                } else {
                    quick_check_failure_summary = None;
                }
                quick_check_outcomes.push(outcome);
            }
            if quick_status == ImplementationQuickCheckStatus::Failed {
                let current_fingerprint = quick_check_failure_summary
                    .as_deref()
                    .map(quick_check_failure_fingerprint);
                if current_fingerprint.is_some()
                    && previous_failure_fingerprint.as_ref() == current_fingerprint.as_ref()
                {
                    notes.push("quick_check_repair_stopped_no_progress".to_string());
                    break;
                }
                previous_failure_fingerprint = current_fingerprint;
                continue;
            }

            let initial_review_iterations = review_iterations;
            let mut rerun_review_iterations = 0usize;
            let mut rerun_blocking_remaining = 0usize;
            let mut rerun_remaining_titles = Vec::new();
            let mut rerun_remaining_categories = Vec::new();
            let review_rerun = run_review_gate(
                sandbox.path(),
                suggestion,
                &generated.description,
                &generated.old_contents,
                &final_changed_files,
                &mut llm_calls,
                repo_memory.clone(),
                quick_status,
                quick_command.as_deref(),
                blocking_severities,
                config.adversarial_review_model.as_model(),
                config.require_independent_review_on_pass,
                config.max_auto_review_fix_loops,
                &attempt_budget,
                &mut usage,
                &mut rerun_review_iterations,
                &mut rerun_blocking_remaining,
                &mut rerun_remaining_titles,
                &mut rerun_remaining_categories,
                &mut fixed_titles,
                &mut files_changed_set,
            )
            .await;
            match review_rerun {
                Ok(()) => {}
                Err(ReviewGateError::BudgetExceeded(reason)) => {
                    notes.push("budget_exceeded".to_string());
                    push_fail_reason(
                        &mut fail_reasons,
                        &mut fail_reason_records,
                        &reason.gate,
                        &reason.code,
                        reason.message.clone(),
                    );
                    push_gate(
                        &mut gates,
                        "budget",
                        false,
                        reason.message,
                        Some(REASON_BUDGET_EXCEEDED),
                    );
                    break;
                }
                Err(ReviewGateError::Failed(err)) => {
                    push_fail_reason(
                        &mut fail_reasons,
                        &mut fail_reason_records,
                        "review",
                        REASON_BLOCKING_REVIEW_RESIDUAL,
                        err,
                    );
                    break;
                }
            }

            final_changed_files = files_changed_set.iter().cloned().collect::<Vec<_>>();
            final_changed_files.sort();

            review_iterations = initial_review_iterations + rerun_review_iterations;
            blocking_remaining = rerun_blocking_remaining;
            remaining_blocking_titles = rerun_remaining_titles;
            remaining_blocking_categories = rerun_remaining_categories;

            if blocking_remaining > 0 {
                push_fail_reason(
                    &mut fail_reasons,
                    &mut fail_reason_records,
                    "review",
                    REASON_BLOCKING_REVIEW_RESIDUAL,
                    "Blocking review findings appeared after quick-check repair",
                );
                break;
            }

            if let Err(err) = syntax_gate(sandbox.path(), &final_changed_files) {
                push_fail_reason(
                    &mut fail_reasons,
                    &mut fail_reason_records,
                    "post_review_syntax",
                    REASON_SYNTAX_VIOLATION,
                    err,
                );
                break;
            }

            // Review fixes could re-break the build/typecheck, so re-run quick checks once.
            let (status, command, outcome) = run_quick_checks(
                sandbox.path(),
                Some(repo_root),
                &mut notes,
                config.quick_checks_mode,
                config.quick_check_timeout_ms.min(
                    attempt_budget
                        .remaining_ms()
                        .saturating_sub(BUDGET_TIMEOUT_SLACK_MS)
                        .max(1),
                ),
            )?;
            quick_status = status;
            quick_command = command;
            quick_outcome = outcome;
            if let Some(outcome) = quick_outcome.clone() {
                if quick_status == ImplementationQuickCheckStatus::Failed {
                    quick_check_failure_summary = summarize_quick_check_failure(&outcome);
                    note_quick_check_failure_fingerprint(
                        &mut notes,
                        quick_check_failure_summary.as_deref(),
                    );
                } else {
                    quick_check_failure_summary = None;
                }
                quick_check_outcomes.push(outcome);
            }

            break;
        }
    }

    if quick_status == ImplementationQuickCheckStatus::Failed {
        push_fail_reason(
            &mut fail_reasons,
            &mut fail_reason_records,
            "quick_check",
            REASON_QUICK_CHECK_FAILED,
            "Quick project checks failed",
        );
    } else if quick_status == ImplementationQuickCheckStatus::Unavailable {
        notes.push("quick_check_unavailable".to_string());
        if config.require_quick_check_detectable {
            push_fail_reason(
                &mut fail_reasons,
                &mut fail_reason_records,
                "quick_check",
                REASON_QUICK_CHECK_UNAVAILABLE,
                "Quick checks were unavailable and strict policy requires a detectable check command",
            );
        }
    }
    let quick_check_ok = quick_check_passes_policy(quick_status, config);
    let quick_reason_code = match quick_status {
        ImplementationQuickCheckStatus::Passed => None,
        ImplementationQuickCheckStatus::Failed => Some(REASON_QUICK_CHECK_FAILED),
        ImplementationQuickCheckStatus::Unavailable if config.require_quick_check_detectable => {
            Some(REASON_QUICK_CHECK_UNAVAILABLE)
        }
        ImplementationQuickCheckStatus::Unavailable => None,
    };
    push_gate(
        &mut gates,
        "quick_check",
        quick_check_ok,
        match quick_status {
            ImplementationQuickCheckStatus::Passed => "Quick checks passed".to_string(),
            ImplementationQuickCheckStatus::Failed => "Quick checks failed".to_string(),
            ImplementationQuickCheckStatus::Unavailable => {
                "No detectable quick-check command".to_string()
            }
        },
        quick_reason_code,
    );

    // Re-evaluate deterministic scope + diff-size budgets on the *final* sandbox state (after any
    // in-attempt repairs). This guarantees we never accept a passing payload that drifted out of
    // scope or exceeded budgets during review/repair loops.
    let final_repo_changes = collect_repo_changes(sandbox.path())?;
    let mut final_repo_files = final_repo_changes.files;
    if final_repo_files.is_empty() && !final_changed_files.is_empty() {
        // git status can occasionally miss transient changes in heavily scripted repos.
        // Fall back to our tracked changed-file set so scope/diff gates stay deterministic.
        notes.push("final_change_detection_fallback_used".to_string());
        final_repo_files = final_changed_files.clone();
    }
    final_repo_files.sort();
    final_changed_files = final_repo_files.clone();

    let non_empty_diff_final = !final_changed_files.is_empty();
    upsert_gate(
        &mut gates,
        "non_empty_diff",
        non_empty_diff_final,
        if non_empty_diff_final {
            "Code changes detected".to_string()
        } else {
            "No file changes produced".to_string()
        },
        if non_empty_diff_final {
            None
        } else {
            Some(REASON_NON_EMPTY_DIFF)
        },
    );
    if !non_empty_diff_final
        && !fail_reason_records
            .iter()
            .any(|r| r.code == REASON_NON_EMPTY_DIFF)
    {
        push_fail_reason(
            &mut fail_reasons,
            &mut fail_reason_records,
            "non_empty_diff",
            REASON_NON_EMPTY_DIFF,
            "Attempt produced no code changes",
        );
    }

    let scope_ok_final = deterministic_scope_gate(&final_changed_files, allowed_files);
    upsert_gate(
        &mut gates,
        "scope",
        scope_ok_final,
        if scope_ok_final {
            format!("{} files changed in attempt", final_changed_files.len())
        } else {
            format!(
                "Found out-of-scope file changes: {}",
                final_changed_files
                    .iter()
                    .filter(|p| !allowed_files.contains(*p))
                    .map(|p| p.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        },
        if scope_ok_final {
            None
        } else {
            Some(REASON_SCOPE_VIOLATION)
        },
    );
    if !scope_ok_final {
        push_fail_reason(
            &mut fail_reasons,
            &mut fail_reason_records,
            "scope",
            REASON_SCOPE_VIOLATION,
            "Attempt changed files outside the validated suggestion scope",
        );
    }

    let (final_changed_total, final_changed_by_file) = compute_changed_lines(
        sandbox.path(),
        &final_changed_files,
        &final_repo_changes.untracked,
    )?;
    changed_total = final_changed_total;
    changed_by_file = final_changed_by_file;

    let diff_budget_ok_final = final_changed_files.len() <= config.max_changed_files
        && changed_total <= config.max_total_changed_lines
        && changed_by_file
            .iter()
            .all(|(_f, c)| *c <= config.max_changed_lines_per_file);
    upsert_gate(
        &mut gates,
        "diff_budget",
        diff_budget_ok_final,
        if diff_budget_ok_final {
            "Diff-size budgets passed".to_string()
        } else {
            "Diff-size budgets exceeded".to_string()
        },
        if diff_budget_ok_final {
            None
        } else {
            Some(REASON_DIFF_BUDGET_VIOLATION)
        },
    );
    if !diff_budget_ok_final {
        push_fail_reason(
            &mut fail_reasons,
            &mut fail_reason_records,
            "diff_budget",
            REASON_DIFF_BUDGET_VIOLATION,
            "Attempt exceeded configured diff-size budgets",
        );
    }

    // Plain-language gate is only meaningful for candidates that otherwise pass all technical gates.
    if fail_reasons.is_empty() {
        let plain_language_ok = is_plain_language_text(&generated.description);
        push_gate(
            &mut gates,
            "plain_language",
            plain_language_ok,
            if plain_language_ok {
                "Description passed plain-language heuristic".to_string()
            } else {
                "Description was too technical or noisy".to_string()
            },
            if plain_language_ok {
                None
            } else {
                Some(REASON_PLAIN_LANGUAGE_FAILURE)
            },
        );
        if !plain_language_ok {
            push_fail_reason(
                &mut fail_reasons,
                &mut fail_reason_records,
                "plain_language",
                REASON_PLAIN_LANGUAGE_FAILURE,
                "Description did not meet plain-language quality standard",
            );
        }
    }

    let passed = fail_reasons.is_empty();
    let file_changes = if passed {
        match collect_sandbox_results(
            sandbox.path(),
            &final_changed_files,
            &generated.modified_areas_by_file,
        ) {
            Ok(changes) => Some(changes),
            Err(err) => {
                push_fail_reason(
                    &mut fail_reasons,
                    &mut fail_reason_records,
                    "finalize_payload",
                    "finalize_payload_failed",
                    format!("Failed to collect passing changes from sandbox: {}", err),
                );
                None
            }
        }
    } else {
        None
    };
    let _ = sandbox.cleanup();

    let attempt_cost_usd = usage.as_ref().map(|u| u.cost()).unwrap_or(0.0);
    let diagnostics = ImplementationAttemptDiagnostics {
        attempt_index,
        passed: file_changes.is_some() && fail_reasons.is_empty(),
        fail_reasons,
        fail_reason_records,
        gates,
        changed_files: final_changed_files,
        changed_lines_total: changed_total,
        changed_lines_by_file: changed_by_file,
        quick_check_status: quick_status,
        quick_check_command: quick_command,
        quick_check_outcome: quick_outcome,
        quick_check_outcomes,
        quick_check_fix_loops,
        quick_check_failure_summary,
        review_iterations,
        review_blocking_remaining: blocking_remaining,
        remaining_blocking_titles,
        remaining_blocking_categories,
        attempt_ms: attempt_start.elapsed().as_millis() as u64,
        attempt_cost_usd,
        llm_calls,
        notes,
    };

    Ok(AttemptExecution {
        pass_payload: if diagnostics.passed {
            Some(AttemptPassPayload {
                description: generated.description,
                file_changes: file_changes.unwrap_or_default(),
            })
        } else {
            None
        },
        diagnostics,
        usage,
    })
}

fn quick_check_passes_policy(
    status: ImplementationQuickCheckStatus,
    config: &ImplementationHarnessConfig,
) -> bool {
    match status {
        ImplementationQuickCheckStatus::Passed => true,
        ImplementationQuickCheckStatus::Failed => false,
        ImplementationQuickCheckStatus::Unavailable => !config.require_quick_check_detectable,
    }
}

#[derive(Debug)]
struct GeneratedCandidate {
    description: String,
    usage: Option<Usage>,
    old_contents: HashMap<PathBuf, String>,
    modified_areas_by_file: HashMap<PathBuf, Vec<String>>,
}

// Keeps per-attempt generation controls explicit for harness telemetry and retries.
#[allow(clippy::too_many_arguments)]
async fn generate_attempt_candidate(
    sandbox_root: &Path,
    suggestion: &Suggestion,
    preview: &FixPreview,
    repo_memory: Option<String>,
    allowed_files: &HashSet<PathBuf>,
    llm_calls: &mut Vec<ImplementationLlmCallRecord>,
    timeout_ms: u64,
    model: Model,
    escalation_reason: Option<&str>,
) -> anyhow::Result<GeneratedCandidate> {
    ensure_generation_model(model)?;
    let escalation_reason = escalation_reason.map(|reason| reason.to_string());

    let mut old_contents = HashMap::new();
    for rel in allowed_files {
        let resolved = resolve_repo_path_allow_new(sandbox_root, rel)
            .map_err(|e| anyhow::anyhow!("Unsafe suggestion path {}: {}", rel.display(), e))?;
        let content = match std::fs::read_to_string(&resolved.absolute) {
            Ok(content) => content,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(e) => {
                return Err(anyhow::anyhow!("Failed to read {}: {}", rel.display(), e));
            }
        };
        old_contents.insert(rel.clone(), content);
    }

    if suggestion.is_multi_file() {
        let mut file_inputs = Vec::new();
        for rel in suggestion.affected_files() {
            let resolved = resolve_repo_path_allow_new(sandbox_root, rel)
                .map_err(|e| anyhow::anyhow!("Unsafe suggestion path {}: {}", rel.display(), e))?;
            let content = old_contents.get(rel).cloned().unwrap_or_else(String::new);
            let is_new = !resolved.absolute.exists();
            file_inputs.push(FileInput {
                path: resolved.relative,
                content,
                is_new,
            });
        }

        let result = generate_multi_file_fix_with_model(
            &file_inputs,
            suggestion,
            preview,
            repo_memory,
            model,
            timeout_ms,
        )
        .await;

        let result = match result {
            Ok(value) => {
                llm_calls.push(ImplementationLlmCallRecord {
                    kind: "generation".to_string(),
                    independence_role: Some("implementation".to_string()),
                    escalation_reason: escalation_reason.clone(),
                    model: model.id().to_string(),
                    timeout_ms: value
                        .speed_failover
                        .as_ref()
                        .map(|d| d.total_timeout_ms)
                        .unwrap_or(timeout_ms),
                    schema_fallback_used: false,
                    speed_failover: value.speed_failover.clone(),
                    error: None,
                });
                value
            }
            Err(err) => {
                let speed_failover = err
                    .downcast_ref::<SpeedFailoverError>()
                    .map(|e| e.diagnostics.clone())
                    .or_else(|| {
                        err.downcast_ref::<FixGenerationErrorWithUsage>()
                            .and_then(|e| e.speed_failover.clone())
                    });
                llm_calls.push(ImplementationLlmCallRecord {
                    kind: "generation".to_string(),
                    independence_role: Some("implementation".to_string()),
                    escalation_reason: escalation_reason.clone(),
                    model: model.id().to_string(),
                    timeout_ms: speed_failover
                        .as_ref()
                        .map(|d| d.total_timeout_ms)
                        .unwrap_or(timeout_ms),
                    schema_fallback_used: false,
                    speed_failover,
                    error: Some(truncate(&err.to_string(), 240)),
                });
                return Err(err);
            }
        };

        let mut modified_areas_by_file = HashMap::new();
        for file_edit in &result.file_edits {
            if !allowed_files.contains(&file_edit.path) {
                return Err(anyhow::anyhow!(
                    "Out-of-scope file from generation: {}",
                    file_edit.path.display()
                ));
            }
            let resolved =
                resolve_repo_path_allow_new(sandbox_root, &file_edit.path).map_err(|e| {
                    anyhow::anyhow!("Unsafe generated path {}: {}", file_edit.path.display(), e)
                })?;
            if let Some(parent) = resolved.absolute.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&resolved.absolute, &file_edit.new_content)?;
            modified_areas_by_file.insert(file_edit.path.clone(), file_edit.modified_areas.clone());
        }

        return Ok(GeneratedCandidate {
            description: result.description,
            usage: result.usage,
            old_contents,
            modified_areas_by_file,
        });
    }

    let target = &suggestion.file;
    let resolved = resolve_repo_path_allow_new(sandbox_root, target)
        .map_err(|e| anyhow::anyhow!("Unsafe suggestion path {}: {}", target.display(), e))?;
    let current_content = old_contents
        .get(target)
        .cloned()
        .unwrap_or_else(String::new);
    let is_new_file = !resolved.absolute.exists();
    let result = generate_fix_content_with_model(
        target,
        &current_content,
        suggestion,
        preview,
        repo_memory,
        is_new_file,
        model,
        timeout_ms,
    )
    .await;

    let result = match result {
        Ok(value) => {
            llm_calls.push(ImplementationLlmCallRecord {
                kind: "generation".to_string(),
                independence_role: Some("implementation".to_string()),
                escalation_reason: escalation_reason.clone(),
                model: model.id().to_string(),
                timeout_ms: value
                    .speed_failover
                    .as_ref()
                    .map(|d| d.total_timeout_ms)
                    .unwrap_or(timeout_ms),
                schema_fallback_used: false,
                speed_failover: value.speed_failover.clone(),
                error: None,
            });
            value
        }
        Err(err) => {
            let speed_failover = err
                .downcast_ref::<SpeedFailoverError>()
                .map(|e| e.diagnostics.clone())
                .or_else(|| {
                    err.downcast_ref::<FixGenerationErrorWithUsage>()
                        .and_then(|e| e.speed_failover.clone())
                });
            llm_calls.push(ImplementationLlmCallRecord {
                kind: "generation".to_string(),
                independence_role: Some("implementation".to_string()),
                escalation_reason: escalation_reason.clone(),
                model: model.id().to_string(),
                timeout_ms: speed_failover
                    .as_ref()
                    .map(|d| d.total_timeout_ms)
                    .unwrap_or(timeout_ms),
                schema_fallback_used: false,
                speed_failover,
                error: Some(truncate(&err.to_string(), 240)),
            });
            return Err(err);
        }
    };

    if let Some(parent) = resolved.absolute.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&resolved.absolute, &result.new_content)?;

    let mut modified_areas_by_file = HashMap::new();
    modified_areas_by_file.insert(target.clone(), result.modified_areas);

    Ok(GeneratedCandidate {
        description: result.description,
        usage: result.usage,
        old_contents,
        modified_areas_by_file,
    })
}

fn collect_repo_changes(repo_root: &Path) -> anyhow::Result<RepoChanges> {
    let status = git_ops::current_status(repo_root)?;
    let mut files = HashSet::new();
    let mut untracked = HashSet::new();
    for path in status
        .staged
        .iter()
        .chain(status.modified.iter())
        .chain(status.untracked.iter())
    {
        let Some(rel) = normalize_repo_change_path(path) else {
            continue;
        };
        files.insert(rel.clone());
    }
    for path in &status.untracked {
        if let Some(rel) = normalize_repo_change_path(path) {
            untracked.insert(rel);
        }
    }
    Ok(RepoChanges {
        files: files.into_iter().collect::<Vec<_>>(),
        untracked,
    })
}

fn revert_out_of_scope_changes(
    repo_root: &Path,
    repo_changes: &RepoChanges,
    out_of_scope_files: &[PathBuf],
) -> anyhow::Result<()> {
    if out_of_scope_files.is_empty() {
        return Ok(());
    }

    for path in out_of_scope_files {
        let resolved = resolve_repo_path_allow_new(repo_root, path)
            .map_err(|e| anyhow::anyhow!("Unsafe out-of-scope file {}: {}", path.display(), e))?;

        if repo_changes.untracked.contains(path) {
            if !resolved.absolute.exists() {
                continue;
            }
            let metadata = std::fs::symlink_metadata(&resolved.absolute).map_err(|e| {
                anyhow::anyhow!(
                    "Failed reading metadata for out-of-scope file {}: {}",
                    path.display(),
                    e
                )
            })?;
            if metadata.file_type().is_dir() && !metadata.file_type().is_symlink() {
                std::fs::remove_dir_all(&resolved.absolute).map_err(|e| {
                    anyhow::anyhow!(
                        "Failed removing out-of-scope directory {}: {}",
                        path.display(),
                        e
                    )
                })?;
            } else {
                std::fs::remove_file(&resolved.absolute).map_err(|e| {
                    anyhow::anyhow!(
                        "Failed removing out-of-scope file {}: {}",
                        path.display(),
                        e
                    )
                })?;
            }
            continue;
        }

        let mut cmd = Command::new("git");
        cmd.current_dir(repo_root)
            .arg("checkout")
            .arg("--")
            .arg(path);
        for (k, v) in SandboxSession::env_overrides() {
            cmd.env(k, v);
        }
        let output = run_command_with_timeout(&mut cmd, Duration::from_secs(15)).map_err(|e| {
            anyhow::anyhow!(
                "Failed restoring out-of-scope file {}: {}",
                path.display(),
                e
            )
        })?;
        if output.timed_out {
            return Err(anyhow::anyhow!(
                "Timed out restoring out-of-scope file {}",
                path.display()
            ));
        }
        if !output.status.map(|s| s.success()).unwrap_or(false) {
            return Err(anyhow::anyhow!(
                "Failed restoring out-of-scope file {}: {}",
                path.display(),
                truncate(&format!("{}\n{}", output.stderr, output.stdout), 180)
            ));
        }
    }

    Ok(())
}

fn normalize_repo_change_path(path: &str) -> Option<PathBuf> {
    let trimmed = path.trim();
    if trimmed.is_empty() || trimmed == "." || trimmed == "./" {
        return None;
    }
    let normalized = trimmed.trim_start_matches("./");
    if normalized.is_empty() {
        return None;
    }
    Some(PathBuf::from(normalized))
}

fn deterministic_scope_gate(changed_files: &[PathBuf], allowed_files: &HashSet<PathBuf>) -> bool {
    changed_files
        .iter()
        .all(|path| allowed_files.contains(path))
}

fn parse_diff_changed_lines(stdout: &str) -> usize {
    stdout
        .lines()
        .filter(|line| {
            (line.starts_with('+') || line.starts_with('-'))
                && !line.starts_with("+++")
                && !line.starts_with("---")
        })
        .count()
}

fn compute_changed_lines(
    repo_root: &Path,
    changed_files: &[PathBuf],
    untracked: &HashSet<PathBuf>,
) -> anyhow::Result<(usize, HashMap<PathBuf, usize>)> {
    let mut totals = HashMap::new();
    let mut total = 0usize;

    for file in changed_files {
        let count = if untracked.contains(file) {
            let resolved = resolve_repo_path_allow_new(repo_root, file)
                .map_err(|e| anyhow::anyhow!("Unsafe changed file {}: {}", file.display(), e))?;
            let content = std::fs::read_to_string(&resolved.absolute).unwrap_or_default();
            content.lines().count().max(1)
        } else {
            let mut cmd = Command::new("git");
            cmd.current_dir(repo_root)
                .arg("diff")
                .arg("--unified=0")
                .arg("--")
                .arg(file);
            for (k, v) in SandboxSession::env_overrides() {
                cmd.env(k, v);
            }
            let output =
                run_command_with_timeout(&mut cmd, Duration::from_secs(15)).map_err(|e| {
                    anyhow::anyhow!("Failed to compute diff for {}: {}", file.display(), e)
                })?;
            if output.timed_out {
                return Err(anyhow::anyhow!(
                    "Timed out computing diff for {}",
                    file.display()
                ));
            }
            parse_diff_changed_lines(&output.stdout)
        };
        totals.insert(file.clone(), count);
        total += count;
    }

    Ok((total, totals))
}

fn syntax_gate(repo_root: &Path, changed_files: &[PathBuf]) -> Result<(), String> {
    for file in changed_files {
        let Some(ext) = file.extension().and_then(|e| e.to_str()) else {
            continue;
        };
        let language = Language::from_extension(ext);
        if language == Language::Unknown {
            continue;
        }
        let resolved = resolve_repo_path_allow_new(repo_root, file)
            .map_err(|e| format!("Unsafe changed file {}: {}", file.display(), e))?;
        let content = std::fs::read_to_string(&resolved.absolute)
            .map_err(|e| format!("Failed reading {}: {}", file.display(), e))?;
        parse_file(file, &content, language).map_err(|e| {
            format!(
                "Parse gate failed for {}: {}",
                file.display(),
                truncate(&e.to_string(), 180)
            )
        })?;
        let has_errors = parse_file_has_errors(file, &content, language).map_err(|e| {
            format!(
                "Parse gate failed for {}: {}",
                file.display(),
                truncate(&e.to_string(), 180)
            )
        })?;
        if has_errors {
            return Err(format!(
                "Parse gate failed for {}: syntax errors detected",
                file.display()
            ));
        }
    }
    Ok(())
}

fn collect_syntax_failures(repo_root: &Path, changed_files: &[PathBuf]) -> Vec<(PathBuf, String)> {
    let mut out = Vec::new();
    for file in changed_files {
        let Some(ext) = file.extension().and_then(|e| e.to_str()) else {
            continue;
        };
        let language = Language::from_extension(ext);
        if language == Language::Unknown {
            continue;
        }

        let resolved = match resolve_repo_path_allow_new(repo_root, file) {
            Ok(r) => r,
            Err(e) => {
                out.push((
                    file.clone(),
                    format!("Unsafe changed file {}: {}", file.display(), e),
                ));
                continue;
            }
        };
        let content = match std::fs::read_to_string(&resolved.absolute) {
            Ok(content) => content,
            Err(e) => {
                out.push((
                    file.clone(),
                    format!("Failed reading {}: {}", file.display(), e),
                ));
                continue;
            }
        };

        if let Err(err) = parse_file(file, &content, language) {
            out.push((
                file.clone(),
                format!(
                    "Parse gate failed for {}: {}",
                    file.display(),
                    truncate(&err.to_string(), 180)
                ),
            ));
            continue;
        }
        match parse_file_has_errors(file, &content, language) {
            Ok(true) => out.push((
                file.clone(),
                format!(
                    "Parse gate failed for {}: syntax errors detected",
                    file.display()
                ),
            )),
            Ok(false) => {}
            Err(err) => out.push((
                file.clone(),
                format!(
                    "Parse gate failed for {}: {}",
                    file.display(),
                    truncate(&err.to_string(), 180)
                ),
            )),
        }
    }
    out
}

fn is_binary_extension(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| BINARY_FILE_EXTENSIONS.contains(&ext.to_ascii_lowercase().as_str()))
        .unwrap_or(false)
}

fn binary_write_gate(repo_root: &Path, changed_files: &[PathBuf]) -> Result<(), String> {
    for file in changed_files {
        if is_binary_extension(file) {
            return Err(format!("Binary writes are not allowed: {}", file.display()));
        }

        let resolved = resolve_repo_path_allow_new(repo_root, file)
            .map_err(|e| format!("Unsafe changed file {}: {}", file.display(), e))?;
        let bytes = std::fs::read(&resolved.absolute)
            .map_err(|e| format!("Failed reading {}: {}", file.display(), e))?;
        if bytes.contains(&0) {
            return Err(format!(
                "Binary writes are not allowed: {} (NUL byte detected)",
                file.display()
            ));
        }
        if std::str::from_utf8(&bytes).is_err() {
            return Err(format!(
                "Binary writes are not allowed: {} (non-UTF-8 content)",
                file.display()
            ));
        }
    }
    Ok(())
}

#[derive(Debug)]
enum ReviewGateError {
    BudgetExceeded(ImplementationFailReason),
    Failed(String),
}

#[allow(clippy::too_many_arguments)]
async fn run_review_gate(
    sandbox_root: &Path,
    suggestion: &Suggestion,
    description: &str,
    old_contents: &HashMap<PathBuf, String>,
    changed_files: &[PathBuf],
    llm_calls: &mut Vec<ImplementationLlmCallRecord>,
    repo_memory: Option<String>,
    quick_check_status: ImplementationQuickCheckStatus,
    quick_check_command: Option<&str>,
    blocking_severities: &HashSet<String>,
    review_model: Model,
    require_independent_review_on_pass: bool,
    max_fix_loops: usize,
    budget: &ImplementationBudget,
    usage: &mut Option<Usage>,
    review_iterations: &mut usize,
    blocking_remaining: &mut usize,
    remaining_blocking_titles: &mut Vec<String>,
    remaining_blocking_categories: &mut Vec<String>,
    fixed_titles: &mut Vec<String>,
    files_changed_set: &mut HashSet<PathBuf>,
) -> Result<(), ReviewGateError> {
    if changed_files.is_empty() {
        *blocking_remaining = 0;
        return Ok(());
    }

    let mut files_with_content =
        build_files_with_content(sandbox_root, old_contents, changed_files)
            .map_err(ReviewGateError::Failed)?;
    let mut iteration = 1u32;
    let review_fix_context = FixContext {
        problem_summary: suggestion.summary.clone(),
        outcome: suggestion
            .detail
            .clone()
            .unwrap_or_else(|| suggestion.summary.clone()),
        description: description.to_string(),
        modified_areas: Vec::new(),
    };

    let mut snapshot_blocking = |current_blocking: &[ReviewFinding]| {
        *blocking_remaining = current_blocking.len();
        *remaining_blocking_titles = dedup_preserve_order(
            current_blocking
                .iter()
                .map(|finding| finding.title.clone())
                .collect(),
        );
        *remaining_blocking_categories = dedup_preserve_order(
            current_blocking
                .iter()
                .map(|finding| finding.category.clone())
                .collect(),
        );
    };

    if let Some(reason) = budget.guard_before_llm_call(&*usage) {
        snapshot_blocking(&[]);
        return Err(ReviewGateError::BudgetExceeded(reason));
    }
    let review_timeout_ms = budget
        .timeout_ms_for_next_llm_call()
        .min(MAX_REVIEW_TIMEOUT_MS);
    ensure_adversarial_review_model(review_model)
        .map_err(|e| ReviewGateError::Failed(format!("Review model policy check failed: {}", e)))?;

    let review = tokio::time::timeout(
        Duration::from_millis(review_timeout_ms),
        verify_changes_bounded_with_model(
            &files_with_content,
            iteration,
            fixed_titles,
            Some(&review_fix_context),
            review_model,
            review_timeout_ms,
        ),
    )
    .await;
    let mut review = match review {
        Ok(Ok(value)) => {
            llm_calls.push(ImplementationLlmCallRecord {
                kind: "review".to_string(),
                independence_role: Some("adversarial".to_string()),
                escalation_reason: None,
                model: review_model.id().to_string(),
                timeout_ms: value
                    .speed_failover
                    .as_ref()
                    .map(|d| d.total_timeout_ms)
                    .unwrap_or(review_timeout_ms),
                schema_fallback_used: value.schema_fallback_used,
                speed_failover: value.speed_failover.clone(),
                error: None,
            });
            value
        }
        Ok(Err(err)) => {
            let speed_failover = err
                .downcast_ref::<SpeedFailoverError>()
                .map(|e| e.diagnostics.clone())
                .or_else(|| {
                    err.downcast_ref::<FixGenerationErrorWithUsage>()
                        .and_then(|e| e.speed_failover.clone())
                });
            llm_calls.push(ImplementationLlmCallRecord {
                kind: "review".to_string(),
                independence_role: Some("adversarial".to_string()),
                escalation_reason: None,
                model: review_model.id().to_string(),
                timeout_ms: speed_failover
                    .as_ref()
                    .map(|d| d.total_timeout_ms)
                    .unwrap_or(review_timeout_ms),
                schema_fallback_used: false,
                speed_failover,
                error: Some(truncate(&err.to_string(), 240)),
            });
            return Err(ReviewGateError::Failed(format!(
                "Review failed: {}",
                truncate(&err.to_string(), 180)
            )));
        }
        Err(_) => {
            llm_calls.push(ImplementationLlmCallRecord {
                kind: "review".to_string(),
                independence_role: Some("adversarial".to_string()),
                escalation_reason: None,
                model: review_model.id().to_string(),
                timeout_ms: review_timeout_ms,
                schema_fallback_used: false,
                speed_failover: None,
                error: Some(format!("Timed out after {}ms", review_timeout_ms)),
            });
            snapshot_blocking(&[]);
            return Err(ReviewGateError::BudgetExceeded(ImplementationFailReason {
                code: REASON_BUDGET_EXCEEDED.to_string(),
                gate: "budget".to_string(),
                message: format!(
                    "Stopped to respect the configured time budget (review timed out after {}ms; limit {}ms)",
                    review_timeout_ms, budget.max_total_ms
                ),
                action: default_action_for_fail_reason("budget", REASON_BUDGET_EXCEEDED)
                    .to_string(),
            }));
        }
    };
    *usage = merge_usage(usage.take(), review.usage.clone());

    let mut blocking = blocking_findings(&review.findings, blocking_severities);
    // If the repo's quick check (e.g. `cargo check`) already passed, treat certain classes of
    // "missing import / undefined symbol" findings as non-blocking. These are typically false
    // positives (the compiler would have failed) and failing the attempt on them harms trust and
    // first-attempt pass rate.
    if quick_check_status == ImplementationQuickCheckStatus::Passed
        && quick_check_command
            .map(|cmd| cmd.contains("cargo check"))
            .unwrap_or(false)
    {
        blocking.retain(|finding| !is_probable_compile_error_false_positive(&finding.title));
    }
    if blocking.len() > 6 {
        snapshot_blocking(&blocking);
        return Err(ReviewGateError::Failed(
            "Review found too many blocking issues to auto-fix safely within budget (more than 6)"
                .to_string(),
        ));
    }
    if let Some(reason) = budget.guard_before_llm_call(&*usage) {
        snapshot_blocking(&blocking);
        return Err(ReviewGateError::BudgetExceeded(reason));
    }

    *review_iterations = 1;
    while !blocking.is_empty() && (*review_iterations - 1) < max_fix_loops {
        if let Some(reason) = budget.guard_before_llm_call(&*usage) {
            snapshot_blocking(&blocking);
            return Err(ReviewGateError::BudgetExceeded(reason));
        }

        let grouped = group_findings_by_file(&blocking, changed_files);
        if grouped.is_empty() {
            break;
        }
        for (path, findings) in grouped {
            if let Some(reason) = budget.guard_before_llm_call(&*usage) {
                snapshot_blocking(&blocking);
                return Err(ReviewGateError::BudgetExceeded(reason));
            }
            let resolved = resolve_repo_path_allow_new(sandbox_root, &path).map_err(|e| {
                ReviewGateError::Failed(format!("Unsafe review fix path {}: {}", path.display(), e))
            })?;
            let current_content = std::fs::read_to_string(&resolved.absolute).map_err(|e| {
                ReviewGateError::Failed(format!("Failed reading {}: {}", path.display(), e))
            })?;
            let original = old_contents.get(&path).map(String::as_str);
            ensure_adversarial_review_model(review_model).map_err(|e| {
                ReviewGateError::Failed(format!("Review model policy check failed: {}", e))
            })?;
            let fix_timeout_ms = budget
                .timeout_ms_for_next_llm_call()
                .min(MAX_FIX_TIMEOUT_MS);
            let fix = tokio::time::timeout(
                Duration::from_millis(fix_timeout_ms),
                fix_review_findings_with_model(
                    &resolved.absolute,
                    &current_content,
                    original,
                    &findings,
                    repo_memory.clone(),
                    *review_iterations as u32,
                    fixed_titles,
                    review_model,
                    fix_timeout_ms,
                ),
            )
            .await;
            let fix = match fix {
                Ok(Ok(value)) => {
                    llm_calls.push(ImplementationLlmCallRecord {
                        kind: "review_fix".to_string(),
                        independence_role: Some("adversarial".to_string()),
                        escalation_reason: None,
                        model: review_model.id().to_string(),
                        timeout_ms: value
                            .speed_failover
                            .as_ref()
                            .map(|d| d.total_timeout_ms)
                            .unwrap_or(fix_timeout_ms),
                        schema_fallback_used: value.schema_fallback_used,
                        speed_failover: value.speed_failover.clone(),
                        error: None,
                    });
                    value
                }
                Ok(Err(err)) => {
                    let speed_failover = err
                        .downcast_ref::<SpeedFailoverError>()
                        .map(|e| e.diagnostics.clone())
                        .or_else(|| {
                            err.downcast_ref::<FixGenerationErrorWithUsage>()
                                .and_then(|e| e.speed_failover.clone())
                        });
                    if let Some(u) = err
                        .downcast_ref::<FixGenerationErrorWithUsage>()
                        .and_then(|e| e.usage.clone())
                    {
                        *usage = merge_usage(usage.take(), Some(u));
                    }
                    llm_calls.push(ImplementationLlmCallRecord {
                        kind: "review_fix".to_string(),
                        independence_role: Some("adversarial".to_string()),
                        escalation_reason: None,
                        model: review_model.id().to_string(),
                        timeout_ms: speed_failover
                            .as_ref()
                            .map(|d| d.total_timeout_ms)
                            .unwrap_or(fix_timeout_ms),
                        schema_fallback_used: false,
                        speed_failover,
                        error: Some(truncate(&err.to_string(), 240)),
                    });
                    *blocking_remaining = blocking.len();
                    *remaining_blocking_titles = dedup_preserve_order(
                        blocking
                            .iter()
                            .map(|finding| finding.title.clone())
                            .collect(),
                    );
                    *remaining_blocking_categories = dedup_preserve_order(
                        blocking
                            .iter()
                            .map(|finding| finding.category.clone())
                            .collect(),
                    );
                    return Err(ReviewGateError::Failed(format!(
                        "Review auto-fix failed for {}: {}",
                        path.display(),
                        truncate(&err.to_string(), 180)
                    )));
                }
                Err(_) => {
                    llm_calls.push(ImplementationLlmCallRecord {
                        kind: "review_fix".to_string(),
                        independence_role: Some("adversarial".to_string()),
                        escalation_reason: None,
                        model: review_model.id().to_string(),
                        timeout_ms: fix_timeout_ms,
                        schema_fallback_used: false,
                        speed_failover: None,
                        error: Some(format!("Timed out after {}ms", fix_timeout_ms)),
                    });
                    snapshot_blocking(&blocking);
                    return Err(ReviewGateError::BudgetExceeded(ImplementationFailReason {
                        code: REASON_BUDGET_EXCEEDED.to_string(),
                        gate: "budget".to_string(),
                        message: format!(
                            "Stopped to respect the configured time budget (review auto-fix timed out after {}ms; limit {}ms)",
                            fix_timeout_ms, budget.max_total_ms
                        ),
                        action: default_action_for_fail_reason("budget", REASON_BUDGET_EXCEEDED)
                            .to_string(),
                    }));
                }
            };
            *usage = merge_usage(usage.take(), fix.usage.clone());
            if let Some(reason) = budget.exhausted(&*usage) {
                snapshot_blocking(&blocking);
                return Err(ReviewGateError::BudgetExceeded(reason));
            }
            std::fs::write(&resolved.absolute, &fix.new_content).map_err(|e| {
                ReviewGateError::Failed(format!(
                    "Failed writing review fix {}: {}",
                    path.display(),
                    e
                ))
            })?;
            files_changed_set.insert(path.clone());
            for finding in findings {
                fixed_titles.push(finding.title);
            }
        }

        iteration += 1;
        *review_iterations += 1;
        let review_files = files_changed_set.iter().cloned().collect::<Vec<_>>();
        files_with_content = build_files_with_content(sandbox_root, old_contents, &review_files)
            .map_err(ReviewGateError::Failed)?;
        if let Some(reason) = budget.guard_before_llm_call(&*usage) {
            snapshot_blocking(&blocking);
            return Err(ReviewGateError::BudgetExceeded(reason));
        }
        let rereview_timeout_ms = budget
            .timeout_ms_for_next_llm_call()
            .min(MAX_REVIEW_TIMEOUT_MS);
        let rereview = tokio::time::timeout(
            Duration::from_millis(rereview_timeout_ms),
            verify_changes_bounded_with_model(
                &files_with_content,
                iteration,
                fixed_titles,
                None,
                review_model,
                rereview_timeout_ms,
            ),
        )
        .await;
        review = match rereview {
            Ok(Ok(value)) => {
                llm_calls.push(ImplementationLlmCallRecord {
                    kind: "rereview".to_string(),
                    independence_role: Some("adversarial".to_string()),
                    escalation_reason: None,
                    model: review_model.id().to_string(),
                    timeout_ms: value
                        .speed_failover
                        .as_ref()
                        .map(|d| d.total_timeout_ms)
                        .unwrap_or(rereview_timeout_ms),
                    schema_fallback_used: value.schema_fallback_used,
                    speed_failover: value.speed_failover.clone(),
                    error: None,
                });
                value
            }
            Ok(Err(err)) => {
                let speed_failover = err
                    .downcast_ref::<SpeedFailoverError>()
                    .map(|e| e.diagnostics.clone())
                    .or_else(|| {
                        err.downcast_ref::<FixGenerationErrorWithUsage>()
                            .and_then(|e| e.speed_failover.clone())
                    });
                llm_calls.push(ImplementationLlmCallRecord {
                    kind: "rereview".to_string(),
                    independence_role: Some("adversarial".to_string()),
                    escalation_reason: None,
                    model: review_model.id().to_string(),
                    timeout_ms: speed_failover
                        .as_ref()
                        .map(|d| d.total_timeout_ms)
                        .unwrap_or(rereview_timeout_ms),
                    schema_fallback_used: false,
                    speed_failover,
                    error: Some(truncate(&err.to_string(), 240)),
                });
                return Err(ReviewGateError::Failed(format!(
                    "Re-review failed: {}",
                    truncate(&err.to_string(), 180)
                )));
            }
            Err(_) => {
                llm_calls.push(ImplementationLlmCallRecord {
                    kind: "rereview".to_string(),
                    independence_role: Some("adversarial".to_string()),
                    escalation_reason: None,
                    model: review_model.id().to_string(),
                    timeout_ms: rereview_timeout_ms,
                    schema_fallback_used: false,
                    speed_failover: None,
                    error: Some(format!("Timed out after {}ms", rereview_timeout_ms)),
                });
                snapshot_blocking(&blocking);
                return Err(ReviewGateError::BudgetExceeded(ImplementationFailReason {
                    code: REASON_BUDGET_EXCEEDED.to_string(),
                    gate: "budget".to_string(),
                    message: format!(
                        "Stopped to respect the configured time budget (re-review timed out after {}ms; limit {}ms)",
                        rereview_timeout_ms, budget.max_total_ms
                    ),
                    action: default_action_for_fail_reason("budget", REASON_BUDGET_EXCEEDED)
                        .to_string(),
                }));
            }
        };
        *usage = merge_usage(usage.take(), review.usage.clone());
        if let Some(reason) = budget.exhausted(&*usage) {
            snapshot_blocking(&blocking);
            return Err(ReviewGateError::BudgetExceeded(reason));
        }
        blocking = blocking_findings(&review.findings, blocking_severities);
        if quick_check_status == ImplementationQuickCheckStatus::Passed
            && quick_check_command
                .map(|cmd| cmd.contains("cargo check"))
                .unwrap_or(false)
        {
            blocking.retain(|finding| !is_probable_compile_error_false_positive(&finding.title));
        }
    }

    // If the same model family did implementation and review, require one final independent
    // pass using Smart before declaring success. This reduces same-model blind spots while
    // keeping the gate deterministic and bounded.
    if blocking.is_empty()
        && require_independent_review_on_pass
        && review_model == IMPLEMENTATION_MODEL
    {
        let mut independent_review = None;
        let mut independent_error: Option<String> = None;
        let independent_models = [Model::Smart];
        for independent_model in independent_models {
            ensure_adversarial_review_model(independent_model).map_err(|e| {
                ReviewGateError::Failed(format!("Review model policy check failed: {}", e))
            })?;
            if let Some(reason) = budget.guard_before_llm_call(&*usage) {
                snapshot_blocking(&[]);
                return Err(ReviewGateError::BudgetExceeded(reason));
            }
            let independent_timeout_ms = budget
                .timeout_ms_for_next_llm_call()
                .min(MAX_REVIEW_TIMEOUT_MS);
            let review_attempt = tokio::time::timeout(
                Duration::from_millis(independent_timeout_ms),
                verify_changes_bounded_with_model(
                    &files_with_content,
                    iteration + 1,
                    fixed_titles,
                    Some(&review_fix_context),
                    independent_model,
                    independent_timeout_ms,
                ),
            )
            .await;
            *review_iterations += 1;
            match review_attempt {
                Ok(Ok(value)) => {
                    llm_calls.push(ImplementationLlmCallRecord {
                        kind: "independent_review".to_string(),
                        independence_role: Some("independent_second_opinion".to_string()),
                        escalation_reason: None,
                        model: independent_model.id().to_string(),
                        timeout_ms: value
                            .speed_failover
                            .as_ref()
                            .map(|d| d.total_timeout_ms)
                            .unwrap_or(independent_timeout_ms),
                        schema_fallback_used: value.schema_fallback_used,
                        speed_failover: value.speed_failover.clone(),
                        error: None,
                    });
                    independent_review = Some(value);
                    break;
                }
                Ok(Err(err)) => {
                    let speed_failover = err
                        .downcast_ref::<SpeedFailoverError>()
                        .map(|e| e.diagnostics.clone())
                        .or_else(|| {
                            err.downcast_ref::<FixGenerationErrorWithUsage>()
                                .and_then(|e| e.speed_failover.clone())
                        });
                    if let Some(u) = err
                        .downcast_ref::<FixGenerationErrorWithUsage>()
                        .and_then(|e| e.usage.clone())
                    {
                        *usage = merge_usage(usage.take(), Some(u));
                    }
                    let err_text = err.to_string();
                    llm_calls.push(ImplementationLlmCallRecord {
                        kind: "independent_review".to_string(),
                        independence_role: Some("independent_second_opinion".to_string()),
                        escalation_reason: None,
                        model: independent_model.id().to_string(),
                        timeout_ms: speed_failover
                            .as_ref()
                            .map(|d| d.total_timeout_ms)
                            .unwrap_or(independent_timeout_ms),
                        schema_fallback_used: false,
                        speed_failover,
                        error: Some(truncate(&err_text, 240)),
                    });
                    independent_error = Some(err_text.clone());
                    if is_response_format_schema_error_text(&err_text) {
                        independent_error = Some(format!(
                            "Provider rejected structured output schema for independent review model {}",
                            independent_model.id()
                        ));
                    }
                }
                Err(_) => {
                    llm_calls.push(ImplementationLlmCallRecord {
                        kind: "independent_review".to_string(),
                        independence_role: Some("independent_second_opinion".to_string()),
                        escalation_reason: None,
                        model: independent_model.id().to_string(),
                        timeout_ms: independent_timeout_ms,
                        schema_fallback_used: false,
                        speed_failover: None,
                        error: Some(format!("Timed out after {}ms", independent_timeout_ms)),
                    });
                    snapshot_blocking(&[]);
                    return Err(ReviewGateError::BudgetExceeded(ImplementationFailReason {
                        code: REASON_BUDGET_EXCEEDED.to_string(),
                        gate: "budget".to_string(),
                        message: format!(
                            "Stopped to respect the configured time budget (independent review timed out after {}ms; limit {}ms)",
                            independent_timeout_ms, budget.max_total_ms
                        ),
                        action: default_action_for_fail_reason("budget", REASON_BUDGET_EXCEEDED)
                            .to_string(),
                    }));
                }
            }
        }
        let independent_review = if let Some(value) = independent_review {
            value
        } else {
            let detail = independent_error
                .as_deref()
                .map(|err| truncate(err, 180))
                .unwrap_or_else(|| "Unknown independent review failure".to_string());
            return Err(ReviewGateError::Failed(format!(
                "Independent adversarial review failed: {}",
                detail
            )));
        };
        *usage = merge_usage(usage.take(), independent_review.usage.clone());
        if let Some(reason) = budget.exhausted(&*usage) {
            snapshot_blocking(&[]);
            return Err(ReviewGateError::BudgetExceeded(reason));
        }
        let mut independent_blocking =
            blocking_findings(&independent_review.findings, blocking_severities);
        if quick_check_status == ImplementationQuickCheckStatus::Passed
            && quick_check_command
                .map(|cmd| cmd.contains("cargo check"))
                .unwrap_or(false)
        {
            independent_blocking
                .retain(|finding| !is_probable_compile_error_false_positive(&finding.title));
        }
        if !independent_blocking.is_empty() {
            snapshot_blocking(&independent_blocking);
            return Err(ReviewGateError::Failed(format!(
                "Independent adversarial review found {} blocking finding(s)",
                independent_blocking.len()
            )));
        }
    }

    snapshot_blocking(&blocking);
    Ok(())
}

fn is_plain_language_text(text: &str) -> bool {
    let normalized = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if normalized.len() < 24 || normalized.len() > 280 {
        return false;
    }
    let lower = normalized.to_ascii_lowercase();
    let technical_markers = [
        "fn ", "impl ", "pub ", "src/", "::", "line ", "panic", "unwrap(", "serde", "trait ",
    ];
    let marker_hits = technical_markers
        .iter()
        .filter(|marker| lower.contains(**marker))
        .count();
    marker_hits <= 2 && normalized.split_whitespace().count() >= 5
}

fn collect_sandbox_results(
    sandbox_root: &Path,
    changed_files: &[PathBuf],
    modified_areas: &HashMap<PathBuf, Vec<String>>,
) -> anyhow::Result<Vec<ImplementationAppliedFile>> {
    let mut out = Vec::new();
    for rel_path in changed_files {
        let sandbox_resolved = resolve_repo_path_allow_new(sandbox_root, rel_path)
            .map_err(|e| anyhow::anyhow!("Unsafe sandbox path {}: {}", rel_path.display(), e))?;
        let content = std::fs::read_to_string(&sandbox_resolved.absolute).map_err(|e| {
            anyhow::anyhow!(
                "Failed to read sandbox result {}: {}",
                rel_path.display(),
                e
            )
        })?;

        let summary = modified_areas
            .get(rel_path)
            .filter(|areas| !areas.is_empty())
            .map(|areas| format!("Modified: {}", areas.join(", ")))
            .unwrap_or_else(|| "Modified".to_string());
        out.push(ImplementationAppliedFile {
            path: rel_path.clone(),
            summary,
            content,
        });
    }
    Ok(out)
}

fn write_harness_report(
    repo_root: &Path,
    diagnostics: &ImplementationRunDiagnostics,
) -> anyhow::Result<PathBuf> {
    let report_dir = repo_root.join(APPLY_HARNESS_REPORT_DIR);
    std::fs::create_dir_all(&report_dir)?;
    let report_path = report_dir.join(format!("{}.json", diagnostics.run_id));
    let content = serde_json::to_string_pretty(diagnostics)?;
    std::fs::write(&report_path, content)?;
    Ok(report_path)
}

pub fn record_harness_finalization_outcome(
    repo_root: &Path,
    diagnostics: &mut ImplementationRunDiagnostics,
    status: ImplementationFinalizationStatus,
    detail: Option<String>,
    mutation_on_failure: Option<bool>,
    run_context: ImplementationHarnessRunContext,
    telemetry_repo_root: Option<&Path>,
) -> anyhow::Result<()> {
    diagnostics.finalization = ImplementationFinalizationDiagnostics {
        status,
        detail,
        mutation_on_failure,
    };
    let report_path = write_harness_report(repo_root, diagnostics)?;
    diagnostics.report_path = Some(report_path);
    let telemetry_root = telemetry_repo_root.unwrap_or(repo_root);
    append_harness_telemetry(telemetry_root, diagnostics, run_context)?;
    Ok(())
}

fn diagnostics_has_independent_review(diagnostics: &ImplementationRunDiagnostics) -> bool {
    diagnostics.attempts.iter().any(|attempt| {
        attempt
            .llm_calls
            .iter()
            .any(|call| call.kind == "independent_review")
    })
}

fn append_harness_telemetry(
    repo_root: &Path,
    diagnostics: &ImplementationRunDiagnostics,
    run_context: ImplementationHarnessRunContext,
) -> anyhow::Result<()> {
    let cache = Cache::new(repo_root);
    let quick_status = diagnostics
        .attempts
        .last()
        .map(|attempt| format!("{:?}", attempt.quick_check_status).to_ascii_lowercase())
        .unwrap_or_else(|| "unavailable".to_string());
    let changed_file_count = diagnostics
        .attempts
        .iter()
        .filter_map(|attempt| {
            if attempt.passed {
                Some(attempt.changed_files.len())
            } else {
                None
            }
        })
        .next_back()
        .unwrap_or(0);
    let schema_fallback_count = diagnostics
        .attempts
        .iter()
        .flat_map(|attempt| attempt.llm_calls.iter())
        .filter(|call| call.schema_fallback_used)
        .count();
    let smart_escalation_count = diagnostics
        .attempts
        .iter()
        .flat_map(|attempt| attempt.llm_calls.iter())
        .filter(|call| call.escalation_reason.is_some())
        .count();
    let baseline_quick_check_failfast_count = diagnostics
        .attempts
        .iter()
        .filter(|attempt| {
            attempt
                .notes
                .iter()
                .any(|note| note == "baseline_quick_check_failfast")
        })
        .count();
    let record = ImplementationHarnessRecord {
        schema_version: 4,
        timestamp: Utc::now(),
        run_id: diagnostics.run_id.clone(),
        suggestion_id: diagnostics.suggestion_id.clone(),
        passed: diagnostics.passed,
        attempt_count: diagnostics.attempt_count,
        total_ms: diagnostics.total_ms,
        total_cost_usd: diagnostics.total_cost_usd,
        changed_file_count,
        quick_check_status: quick_status,
        fail_reasons: diagnostics.fail_reasons.clone(),
        report_path: diagnostics.report_path.clone(),
        finalization_status: finalization_status_label(diagnostics.finalization.status).to_string(),
        mutation_on_failure: diagnostics.finalization.mutation_on_failure,
        run_context: run_context.as_str().to_string(),
        independent_review_executed: diagnostics_has_independent_review(diagnostics),
        schema_fallback_count,
        smart_escalation_count,
        baseline_quick_check_failfast_count,
    };
    cache
        .append_implementation_harness(&record)
        .map_err(|e| anyhow::anyhow!("Failed to append implementation harness telemetry: {}", e))?;
    Ok(())
}

fn finalization_status_label(status: ImplementationFinalizationStatus) -> &'static str {
    match status {
        ImplementationFinalizationStatus::Applied => "applied",
        ImplementationFinalizationStatus::RolledBack => "rolled_back",
        ImplementationFinalizationStatus::FailedBeforeFinalize => "failed_before_finalize",
    }
}

#[cfg(test)]
mod tests;
