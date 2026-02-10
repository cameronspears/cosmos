use super::fix::{
    generate_fix_content_with_model, generate_multi_file_fix_with_model, FileInput, FixPreview,
};
use super::models::{merge_usage, Model, Usage};
use super::review::{fix_review_findings_with_model, verify_changes, FixContext, ReviewFinding};
use crate::cache::{Cache, ImplementationHarnessRecord};
use crate::git_ops;
use crate::index::parser::{parse_file, parse_file_has_errors};
use crate::index::Language;
use crate::lab::sandbox::SandboxSession;
use crate::suggest::{Suggestion, SuggestionValidationState};
use crate::util::{resolve_repo_path_allow_new, run_command_with_timeout, truncate};
use chrono::Utc;
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;
use uuid::Uuid;

const APPLY_HARNESS_REPORT_DIR: &str = ".cosmos/apply_harness";
const MAX_COMMAND_OUTPUT_TAIL_CHARS: usize = 4_000;
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
    pub max_auto_review_fix_loops: usize,
    pub max_auto_quick_check_fix_loops: usize,
    pub quick_checks_mode: ImplementationQuickChecksMode,
    pub review_blocking_severities: Vec<String>,
    pub max_changed_files: usize,
    pub max_total_changed_lines: usize,
    pub max_changed_lines_per_file: usize,
    pub quick_check_timeout_ms: u64,
    pub require_quick_check_detectable: bool,
    pub fail_on_reduced_confidence: bool,
    pub quick_check_fix_requires_in_scope_error: bool,
}

impl Default for ImplementationHarnessConfig {
    fn default() -> Self {
        Self::interactive_strict()
    }
}

impl ImplementationHarnessConfig {
    pub fn interactive_strict() -> Self {
        Self {
            max_attempts: 4,
            max_total_ms: 45_000,
            max_total_cost_usd: 0.020,
            max_auto_review_fix_loops: 2,
            max_auto_quick_check_fix_loops: 1,
            quick_checks_mode: ImplementationQuickChecksMode::StrictAuto,
            review_blocking_severities: vec!["critical".to_string(), "warning".to_string()],
            max_changed_files: 6,
            max_total_changed_lines: 500,
            max_changed_lines_per_file: 220,
            quick_check_timeout_ms: 120_000,
            require_quick_check_detectable: false,
            fail_on_reduced_confidence: false,
            quick_check_fix_requires_in_scope_error: true,
        }
    }

    pub fn lab_strict() -> Self {
        let mut config = Self::interactive_strict();
        config.require_quick_check_detectable = true;
        config.fail_on_reduced_confidence = true;
        config
    }
}

#[derive(Debug, Clone)]
struct ImplementationBudget {
    started_at: std::time::Instant,
    max_total_ms: u64,
    max_total_cost_usd: f64,
}

impl ImplementationBudget {
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
            });
        }

        let cost_usd = usage.as_ref().map(|u| u.cost()).unwrap_or(0.0);
        if cost_usd >= self.max_total_cost_usd {
            return Some(ImplementationFailReason {
                code: REASON_BUDGET_EXCEEDED.to_string(),
                gate: "budget".to_string(),
                message: format!(
                    "Stopped to respect the configured cost budget (${:0.4} spent; limit ${:0.4})",
                    cost_usd, self.max_total_cost_usd
                ),
            });
        }

        None
    }
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
        if ch == '\u{1b}' {
            if matches!(chars.peek(), Some('[')) {
                // Skip ANSI CSI: ESC [ ... (letters)
                let _ = chars.next(); // '['
                while let Some(c) = chars.next() {
                    if c.is_ascii_alphabetic() {
                        break;
                    }
                }
                continue;
            }
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

    let pick_line = |text: &str| -> Option<String> {
        let mut best: Option<&str> = None;
        for line in text.lines().map(str::trim) {
            if line.is_empty() {
                continue;
            }
            let lower = line.to_ascii_lowercase();
            if lower.contains("error") || lower.contains("failed") || lower.contains("cannot ") {
                best = Some(line);
                break;
            }
            if best.is_none() {
                best = Some(line);
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

fn normalize_quick_check_path(raw: &str) -> Option<PathBuf> {
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
    if !is_safe_relative_path(&path) {
        return None;
    }
    Some(path)
}

fn extract_quick_check_error_paths(outcome: &ImplementationCommandOutcome) -> Vec<PathBuf> {
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
                .and_then(|m| normalize_quick_check_path(m.as_str()))
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
                .and_then(|m| normalize_quick_check_path(m.as_str()))
            {
                if seen.insert(path.clone()) {
                    out.push(path);
                }
            }
        }
    }

    out
}

fn format_quick_check_repair_modifier(
    existing: Option<&str>,
    error_summary: &str,
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
        "Quick-check repair request:\n- Quick-check failure: {}\n- File to repair: {}\nRules:\n- Modify only this file.\n- Fix the reported error.\n- Keep the diff minimal and avoid unrelated reformatting.\n- Do not change behavior outside what's needed for the error.",
        truncate(error_summary, 240),
        target.display()
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

    dedup_preserve_order(out)
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

fn push_fail_reason(
    fail_reasons: &mut Vec<String>,
    fail_reason_records: &mut Vec<ImplementationFailReason>,
    gate: &str,
    code: &str,
    message: impl Into<String>,
) {
    let msg = message.into();
    fail_reasons.push(msg.clone());
    fail_reason_records.push(ImplementationFailReason {
        code: code.to_string(),
        gate: gate.to_string(),
        message: msg,
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
    let budget = ImplementationBudget {
        started_at: start,
        max_total_ms: config.max_total_ms,
        max_total_cost_usd: config.max_total_cost_usd,
    };
    let mut usage: Option<Usage> = None;
    let mut attempts = Vec::new();
    let mut pass_payload: Option<AttemptPassPayload> = None;
    let mut feedback_reasons: Vec<String> = Vec::new();
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

    for attempt_index in 1..=config.max_attempts.max(1) {
        if let Some(reason) = budget.exhausted(&usage) {
            feedback_reasons.push(reason.message);
            break;
        }

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
            &budget,
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
        if attempt.pass_payload.is_some() {
            pass_payload = attempt.pass_payload;
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
    budget: &ImplementationBudget,
    usage_so_far: &Option<Usage>,
    attempt_index: usize,
    run_id: &str,
    feedback: Option<&str>,
) -> anyhow::Result<AttemptExecution> {
    let attempt_start = std::time::Instant::now();
    let mut gates = Vec::new();
    let mut fail_reasons = Vec::new();
    let mut fail_reason_records = Vec::new();
    let mut usage: Option<Usage> = None;
    let mut notes = Vec::new();

    if let Some(reason) = budget.exhausted(usage_so_far) {
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
            quick_check_command: None,
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
            notes,
        };
        return Ok(AttemptExecution {
            diagnostics: diag,
            usage: None,
            pass_payload: None,
        });
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
                quick_check_command: None,
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

    if let Some(reason) = budget.exhausted(&merge_usage(usage_so_far.clone(), usage.clone())) {
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
            quick_check_command: None,
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
            notes,
        };
        return Ok(AttemptExecution {
            diagnostics: diag,
            usage,
            pass_payload: None,
        });
    }

    let generation = generate_attempt_candidate(
        sandbox.path(),
        suggestion,
        &feedback_preview,
        repo_memory.clone(),
        allowed_files,
    )
    .await;

    let mut generated = match generation {
        Ok(value) => value,
        Err(err) => {
            let message = truncate(&format!("Generation failed: {}", err), 240);
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
                quick_check_command: None,
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
                notes,
            };
            return Ok(AttemptExecution {
                diagnostics: diag,
                usage: None,
                pass_payload: None,
            });
        }
    };

    usage = merge_usage(usage, generated.usage.take());
    if let Some(reason) = budget.exhausted(&merge_usage(usage_so_far.clone(), usage.clone())) {
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
            quick_check_command: None,
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

    let (changed_total, changed_by_file) =
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

    let syntax_ok = syntax_gate(sandbox.path(), &repo_changes.files).is_ok();
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
    if let Err(err) = syntax_gate(sandbox.path(), &repo_changes.files) {
        push_fail_reason(
            &mut fail_reasons,
            &mut fail_reason_records,
            "syntax",
            REASON_SYNTAX_VIOLATION,
            err,
        );
    }

    let mut review_iterations = 0usize;
    let mut blocking_remaining = 0usize;
    let mut remaining_blocking_titles = Vec::new();
    let mut remaining_blocking_categories = Vec::new();
    let mut fixed_titles = Vec::new();
    let mut files_changed_set = repo_changes.files.iter().cloned().collect::<HashSet<_>>();
    let review_result = run_review_gate(
        sandbox.path(),
        suggestion,
        &generated.description,
        &generated.old_contents,
        &repo_changes.files,
        repo_memory.clone(),
        blocking_severities,
        config.max_auto_review_fix_loops,
        budget,
        usage_so_far,
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
    match &review_result {
        Ok(()) => {}
        Err(ReviewGateError::BudgetExceeded(reason)) => {
            review_budget_exceeded = true;
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
        if blocking_remaining == 0 {
            format!(
                "Review gate passed after {} iteration(s)",
                review_iterations.max(1)
            )
        } else {
            format!(
                "Review found {} blocking finding(s) after {} iteration(s)",
                blocking_remaining,
                review_iterations.max(1)
            )
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

    let mut final_changed_files = files_changed_set.iter().cloned().collect::<Vec<_>>();
    final_changed_files.sort();
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

    let mut quick_check_outcomes: Vec<ImplementationCommandOutcome> = Vec::new();
    let mut quick_check_fix_loops = 0usize;
    let mut quick_check_failure_summary: Option<String> = None;
    let (mut quick_status, mut quick_command, mut quick_outcome) = run_quick_checks(
        sandbox.path(),
        Some(repo_root),
        &mut notes,
        config.quick_checks_mode,
        config.quick_check_timeout_ms,
    )?;

    if let Some(outcome) = quick_outcome.clone() {
        quick_check_failure_summary = summarize_quick_check_failure(&outcome);
        quick_check_outcomes.push(outcome);
    }

    let eligible_for_quick_check_repair = quick_status == ImplementationQuickCheckStatus::Failed
        && config.max_auto_quick_check_fix_loops > 0
        && fail_reasons.is_empty();
    if eligible_for_quick_check_repair {
        for loop_index in 0..config.max_auto_quick_check_fix_loops {
            let Some(outcome) = quick_outcome.as_ref() else {
                break;
            };
            let candidates = extract_quick_check_error_paths(outcome);
            let target = candidates.into_iter().find(|path| {
                if config.quick_check_fix_requires_in_scope_error {
                    allowed_files.contains(path)
                } else {
                    allowed_files.contains(path) || files_changed_set.contains(path)
                }
            });
            let Some(target) = target else {
                notes.push("quick_check_repair_skipped_no_in_scope_error_path".to_string());
                break;
            };

            quick_check_fix_loops = loop_index + 1;
            notes.push(format!("quick_check_fix_loop_{}", quick_check_fix_loops));

            if let Some(reason) =
                budget.exhausted(&merge_usage(usage_so_far.clone(), usage.clone()))
            {
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
            repair_preview.modifier = Some(format_quick_check_repair_modifier(
                feedback_preview.modifier.as_deref(),
                error_summary,
                &target,
            ));

            ensure_implementation_model(IMPLEMENTATION_MODEL)?;
            let fix = match generate_fix_content_with_model(
                &target,
                &current_content,
                suggestion,
                &repair_preview,
                repo_memory.clone(),
                is_new_file,
                IMPLEMENTATION_MODEL,
            )
            .await
            {
                Ok(value) => value,
                Err(err) => {
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
            };
            usage = merge_usage(usage, fix.usage.clone());
            if let Some(reason) =
                budget.exhausted(&merge_usage(usage_so_far.clone(), usage.clone()))
            {
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
                    "post_review_syntax",
                    REASON_SYNTAX_VIOLATION,
                    err,
                );
                break;
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
                repo_memory.clone(),
                blocking_severities,
                config.max_auto_review_fix_loops,
                budget,
                usage_so_far,
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

            let (status, command, outcome) = run_quick_checks(
                sandbox.path(),
                Some(repo_root),
                &mut notes,
                config.quick_checks_mode,
                config.quick_check_timeout_ms,
            )?;
            quick_status = status;
            quick_command = command;
            quick_outcome = outcome;
            if let Some(outcome) = quick_outcome.clone() {
                quick_check_failure_summary = summarize_quick_check_failure(&outcome);
                quick_check_outcomes.push(outcome);
            }
            if quick_status != ImplementationQuickCheckStatus::Failed {
                break;
            }
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

async fn generate_attempt_candidate(
    sandbox_root: &Path,
    suggestion: &Suggestion,
    preview: &FixPreview,
    repo_memory: Option<String>,
    allowed_files: &HashSet<PathBuf>,
) -> anyhow::Result<GeneratedCandidate> {
    ensure_implementation_model(IMPLEMENTATION_MODEL)?;

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
            IMPLEMENTATION_MODEL,
        )
        .await?;

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
        IMPLEMENTATION_MODEL,
    )
    .await?;

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
        let rel = PathBuf::from(path);
        files.insert(rel.clone());
    }
    for path in &status.untracked {
        untracked.insert(PathBuf::from(path));
    }
    Ok(RepoChanges {
        files: files.into_iter().collect::<Vec<_>>(),
        untracked,
    })
}

fn deterministic_scope_gate(changed_files: &[PathBuf], allowed_files: &HashSet<PathBuf>) -> bool {
    !changed_files.is_empty()
        && changed_files
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
    repo_memory: Option<String>,
    blocking_severities: &HashSet<String>,
    max_fix_loops: usize,
    budget: &ImplementationBudget,
    usage_so_far: &Option<Usage>,
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

    if let Some(reason) = budget.exhausted(&merge_usage(usage_so_far.clone(), usage.clone())) {
        return Err(ReviewGateError::BudgetExceeded(reason));
    }
    let mut review = verify_changes(
        &files_with_content,
        iteration,
        fixed_titles,
        Some(&FixContext {
            problem_summary: suggestion.summary.clone(),
            outcome: suggestion
                .detail
                .clone()
                .unwrap_or_else(|| suggestion.summary.clone()),
            description: description.to_string(),
            modified_areas: Vec::new(),
        }),
    )
    .await
    .map_err(|e| {
        ReviewGateError::Failed(format!("Review failed: {}", truncate(&e.to_string(), 180)))
    })?;
    *usage = merge_usage(usage.take(), review.usage.clone());

    let mut blocking = blocking_findings(&review.findings, blocking_severities);
    if blocking.len() > 6 {
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
        return Err(ReviewGateError::Failed(
            "Review found too many blocking issues to auto-fix safely within budget (more than 6)"
                .to_string(),
        ));
    }
    if let Some(reason) = budget.exhausted(&merge_usage(usage_so_far.clone(), usage.clone())) {
        return Err(ReviewGateError::BudgetExceeded(reason));
    }

    *review_iterations = 1;
    while !blocking.is_empty() && (*review_iterations - 1) < max_fix_loops {
        if let Some(reason) = budget.exhausted(&merge_usage(usage_so_far.clone(), usage.clone())) {
            return Err(ReviewGateError::BudgetExceeded(reason));
        }

        let grouped = group_findings_by_file(&blocking, changed_files);
        if grouped.is_empty() {
            break;
        }
        for (path, findings) in grouped {
            if let Some(reason) =
                budget.exhausted(&merge_usage(usage_so_far.clone(), usage.clone()))
            {
                return Err(ReviewGateError::BudgetExceeded(reason));
            }
            let resolved = resolve_repo_path_allow_new(sandbox_root, &path).map_err(|e| {
                ReviewGateError::Failed(format!("Unsafe review fix path {}: {}", path.display(), e))
            })?;
            let current_content = std::fs::read_to_string(&resolved.absolute).map_err(|e| {
                ReviewGateError::Failed(format!("Failed reading {}: {}", path.display(), e))
            })?;
            let original = old_contents.get(&path).map(String::as_str);
            ensure_implementation_model(IMPLEMENTATION_MODEL).map_err(|e| {
                ReviewGateError::Failed(format!("Implementation model policy check failed: {}", e))
            })?;
            let fix = fix_review_findings_with_model(
                &resolved.absolute,
                &current_content,
                original,
                &findings,
                repo_memory.clone(),
                *review_iterations as u32,
                fixed_titles,
                IMPLEMENTATION_MODEL,
            )
            .await
            .map_err(|e| {
                ReviewGateError::Failed(format!(
                    "Review auto-fix failed: {}",
                    truncate(&e.to_string(), 180)
                ))
            })?;
            *usage = merge_usage(usage.take(), fix.usage.clone());
            if let Some(reason) =
                budget.exhausted(&merge_usage(usage_so_far.clone(), usage.clone()))
            {
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
        if let Some(reason) = budget.exhausted(&merge_usage(usage_so_far.clone(), usage.clone())) {
            return Err(ReviewGateError::BudgetExceeded(reason));
        }
        review = verify_changes(&files_with_content, iteration, fixed_titles, None)
            .await
            .map_err(|e| {
                ReviewGateError::Failed(format!(
                    "Re-review failed: {}",
                    truncate(&e.to_string(), 180)
                ))
            })?;
        *usage = merge_usage(usage.take(), review.usage.clone());
        if let Some(reason) = budget.exhausted(&merge_usage(usage_so_far.clone(), usage.clone())) {
            return Err(ReviewGateError::BudgetExceeded(reason));
        }
        blocking = blocking_findings(&review.findings, blocking_severities);
    }

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
    Ok(())
}

fn build_files_with_content(
    sandbox_root: &Path,
    old_contents: &HashMap<PathBuf, String>,
    files: &[PathBuf],
) -> Result<Vec<(PathBuf, String, String)>, String> {
    files
        .iter()
        .map(|path| {
            let resolved = resolve_repo_path_allow_new(sandbox_root, path)
                .map_err(|e| format!("Unsafe path {}: {}", path.display(), e))?;
            let new_content = std::fs::read_to_string(&resolved.absolute).unwrap_or_default();
            let old_content = old_contents.get(path).cloned().unwrap_or_default();
            Ok((resolved.absolute, old_content, new_content))
        })
        .collect::<Result<Vec<_>, _>>()
}

fn blocking_findings(
    findings: &[ReviewFinding],
    blocking_severities: &HashSet<String>,
) -> Vec<ReviewFinding> {
    findings
        .iter()
        .filter(|finding| {
            finding.recommended
                && blocking_severities.contains(&finding.severity.to_ascii_lowercase())
        })
        .cloned()
        .collect()
}

fn group_findings_by_file(
    findings: &[ReviewFinding],
    candidates: &[PathBuf],
) -> HashMap<PathBuf, Vec<ReviewFinding>> {
    let mut grouped: HashMap<PathBuf, Vec<ReviewFinding>> = HashMap::new();
    for finding in findings {
        if let Some(path) = resolve_finding_file_path(&finding.file, candidates) {
            grouped.entry(path).or_default().push(finding.clone());
        }
    }
    grouped
}

fn resolve_finding_file_path(finding_file: &str, candidates: &[PathBuf]) -> Option<PathBuf> {
    let normalized = finding_file.replace('\\', "/");
    let candidate = PathBuf::from(&normalized);
    if candidates.iter().any(|p| p == &candidate) {
        return Some(candidate);
    }

    for path in candidates {
        let p = path.to_string_lossy().replace('\\', "/");
        if normalized.ends_with(&p) {
            return Some(path.clone());
        }
    }

    let normalized_path = PathBuf::from(&normalized);
    let file_name = normalized_path.file_name().and_then(|name| name.to_str())?;
    let mut matches = candidates
        .iter()
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .map(|name| name == file_name)
                .unwrap_or(false)
        })
        .cloned()
        .collect::<Vec<_>>();
    if matches.len() == 1 {
        return matches.pop();
    }
    None
}

#[derive(Debug, Clone)]
enum QuickCheckCommand {
    Shell(String),
    Program { program: String, args: Vec<String> },
}

fn detect_quick_check_command(repo_root: &Path) -> Option<QuickCheckCommand> {
    if let Ok(shell_cmd) = std::env::var("COSMOS_FIX_HARNESS_CHECK_CMD") {
        if !shell_cmd.trim().is_empty() {
            return Some(QuickCheckCommand::Shell(shell_cmd));
        }
    }

    if repo_root.join("Cargo.toml").exists() {
        return Some(QuickCheckCommand::Program {
            program: "cargo".to_string(),
            args: vec!["check".to_string(), "--locked".to_string()],
        });
    }

    let package_json = repo_root.join("package.json");
    if package_json.exists() {
        if let Ok(content) = std::fs::read_to_string(&package_json) {
            if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&content) {
                if let Some(scripts) = parsed.get("scripts").and_then(|v| v.as_object()) {
                    let deps = parsed.get("dependencies").and_then(|v| v.as_object());
                    let dev_deps = parsed.get("devDependencies").and_then(|v| v.as_object());

                    for candidate in [
                        "typecheck",
                        "type-check",
                        "check",
                        "test:once",
                        "test",
                        "lint",
                        "build",
                    ] {
                        let Some(script_value) = scripts.get(candidate) else {
                            continue;
                        };
                        let script_cmd = script_value.as_str().unwrap_or_default();
                        if should_skip_js_quick_check_script(candidate, script_cmd, deps, dev_deps)
                        {
                            continue;
                        }
                        return Some(js_script_quick_check_command(repo_root, candidate));
                    }
                }
            }
        }
    }

    if repo_root.join("go.mod").exists() {
        return Some(QuickCheckCommand::Program {
            program: "go".to_string(),
            args: vec!["test".to_string(), "./...".to_string()],
        });
    }

    None
}

fn should_skip_js_quick_check_script(
    script_name: &str,
    script_cmd: &str,
    deps: Option<&serde_json::Map<String, serde_json::Value>>,
    dev_deps: Option<&serde_json::Map<String, serde_json::Value>>,
) -> bool {
    if script_name != "lint" {
        return false;
    }

    let cmd = script_cmd.to_ascii_lowercase();

    // Common footgun: lint script uses `eslint` but the repo doesn't include it as a dependency.
    // Running it will always fail and makes the harness look unreliable.
    if cmd.contains("eslint") && !has_js_dep("eslint", deps, dev_deps) {
        return true;
    }

    // Next.js v16 removed `next lint`. Some repos still carry a legacy `lint: next lint` script,
    // which fails with "Invalid project directory .../lint". Prefer other checks (like build).
    if cmd.contains("next lint") {
        let next_major = js_dep_major_version("next", deps, dev_deps).unwrap_or(0);
        if next_major >= 16 {
            return true;
        }
    }

    false
}

fn has_js_dep(
    name: &str,
    deps: Option<&serde_json::Map<String, serde_json::Value>>,
    dev_deps: Option<&serde_json::Map<String, serde_json::Value>>,
) -> bool {
    deps.map(|m| m.contains_key(name)).unwrap_or(false)
        || dev_deps.map(|m| m.contains_key(name)).unwrap_or(false)
}

fn js_dep_major_version(
    name: &str,
    deps: Option<&serde_json::Map<String, serde_json::Value>>,
    dev_deps: Option<&serde_json::Map<String, serde_json::Value>>,
) -> Option<u32> {
    let raw = deps
        .and_then(|m| m.get(name))
        .or_else(|| dev_deps.and_then(|m| m.get(name)))?
        .as_str()?;
    parse_major_version(raw)
}

fn parse_major_version(raw: &str) -> Option<u32> {
    // Handles common semver-ish specifiers: "^16.1.1", "~16.0.0", ">=16", "16".
    let trimmed = raw.trim();
    let digits = trimmed
        .trim_start_matches(|c: char| !c.is_ascii_digit())
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect::<String>();
    digits.parse::<u32>().ok()
}

fn js_script_quick_check_command(repo_root: &Path, script: &str) -> QuickCheckCommand {
    if repo_root.join("pnpm-lock.yaml").exists() {
        return QuickCheckCommand::Program {
            program: "pnpm".to_string(),
            args: vec![script.to_string()],
        };
    }
    if repo_root.join("yarn.lock").exists() {
        return QuickCheckCommand::Program {
            program: "yarn".to_string(),
            args: vec![script.to_string()],
        };
    }
    if repo_root.join("bun.lockb").exists() || repo_root.join("bun.lock").exists() {
        return QuickCheckCommand::Program {
            program: "bun".to_string(),
            args: vec!["run".to_string(), script.to_string()],
        };
    }
    QuickCheckCommand::Program {
        program: "npm".to_string(),
        args: vec![
            "run".to_string(),
            script.to_string(),
            "--silent".to_string(),
        ],
    }
}

fn command_to_string(command: &QuickCheckCommand) -> String {
    match command {
        QuickCheckCommand::Shell(cmd) => format!("sh -lc '{}'", cmd),
        QuickCheckCommand::Program { program, args } => {
            if args.is_empty() {
                program.clone()
            } else {
                format!("{} {}", program, args.join(" "))
            }
        }
    }
}

fn read_package_json_script(repo_root: &Path, script_name: &str) -> Option<String> {
    let package_json = repo_root.join("package.json");
    let content = std::fs::read_to_string(package_json).ok()?;
    let parsed = serde_json::from_str::<serde_json::Value>(&content).ok()?;
    parsed
        .get("scripts")
        .and_then(|v| v.as_object())
        .and_then(|scripts| scripts.get(script_name))
        .and_then(|v| v.as_str())
        .map(|v| v.to_string())
}

fn invoked_js_script(command: &QuickCheckCommand) -> Option<String> {
    let QuickCheckCommand::Program { program, args } = command else {
        return None;
    };
    let program = program.to_ascii_lowercase();
    if program == "npm" || program == "bun" {
        if args.len() >= 2 && args[0] == "run" {
            return Some(args[1].clone());
        }
        return None;
    }
    if program == "pnpm" || program == "yarn" {
        return args.first().cloned();
    }
    None
}

fn quick_check_requires_real_node_modules(repo_root: &Path, command: &QuickCheckCommand) -> bool {
    match command {
        QuickCheckCommand::Shell(cmd) => {
            let lower = cmd.to_ascii_lowercase();
            lower.contains("next build") || lower.contains("turbopack") || lower.contains("--turbo")
        }
        QuickCheckCommand::Program { program, args } => {
            let program = program.to_ascii_lowercase();
            if program == "next" {
                return args
                    .first()
                    .map(|arg| arg.to_ascii_lowercase() == "build")
                    .unwrap_or(false);
            }

            let Some(script) = invoked_js_script(command) else {
                return false;
            };
            let Some(script_cmd) = read_package_json_script(repo_root, &script) else {
                return false;
            };
            let lower = script_cmd.to_ascii_lowercase();
            lower.contains("next build") || lower.contains("turbopack") || lower.contains("--turbo")
        }
    }
}

fn is_node_modules_symlink(path: &Path) -> bool {
    std::fs::symlink_metadata(path)
        .map(|meta| meta.file_type().is_symlink())
        .unwrap_or(false)
}

fn command_needs_node_modules(repo_root: &Path, command: &QuickCheckCommand) -> bool {
    if !repo_root.join("package.json").exists() {
        return false;
    }
    match command {
        QuickCheckCommand::Shell(cmd) => {
            let lower = cmd.to_ascii_lowercase();
            lower.contains("npm ")
                || lower.contains("pnpm ")
                || lower.contains("yarn ")
                || lower.contains("bun ")
                || lower.contains("npx ")
                || lower.contains("node ")
        }
        QuickCheckCommand::Program { program, .. } => {
            matches!(
                program.to_ascii_lowercase().as_str(),
                "npm" | "pnpm" | "yarn" | "bun" | "npx" | "node"
            )
        }
    }
}

fn copy_node_modules_from_source(
    repo_root: &Path,
    source_node_modules: &Path,
    node_modules: &Path,
    notes: &mut Vec<String>,
) -> anyhow::Result<()> {
    if node_modules.exists() {
        return Ok(());
    }

    if let Some(parent) = node_modules.parent() {
        std::fs::create_dir_all(parent)?;
    }

    // Ensure we are not copying into a symlink path.
    if let Ok(meta) = std::fs::symlink_metadata(node_modules) {
        if meta.is_dir() {
            let _ = std::fs::remove_dir_all(node_modules);
        } else {
            let _ = std::fs::remove_file(node_modules);
        }
    }

    let src = source_node_modules.to_string_lossy().to_string();
    let dst = node_modules.to_string_lossy().to_string();

    // Prefer copy-on-write / reflink where available, but fall back to a plain archive copy.
    // Note: This is best-effort and must not cause apply to mutate tracked state.
    let mut attempts: Vec<Vec<&str>> = Vec::new();
    if cfg!(target_os = "macos") {
        attempts.push(vec!["-c", "-a"]);
        attempts.push(vec!["-a"]);
    } else {
        attempts.push(vec!["-a", "--reflink=auto"]);
        attempts.push(vec!["-a"]);
    }

    for (idx, opts) in attempts.iter().enumerate() {
        let mut cmd = Command::new("cp");
        cmd.current_dir(repo_root).args(opts).arg(&src).arg(&dst);
        for (k, v) in SandboxSession::env_overrides() {
            cmd.env(k, v);
        }

        let output = run_command_with_timeout(&mut cmd, Duration::from_secs(90))
            .map_err(|e| anyhow::anyhow!("Failed to start cp to copy node_modules: {}", e))?;
        if output.timed_out {
            return Err(anyhow::anyhow!(
                "Timed out copying node_modules from {}",
                source_node_modules.display()
            ));
        }

        if output.status.map(|s| s.success()).unwrap_or(false) {
            notes.push("copied_node_modules_from_source".to_string());
            return Ok(());
        }

        let stderr = output.stderr.to_ascii_lowercase();
        let unknown_option = stderr.contains("illegal option")
            || stderr.contains("unrecognized option")
            || stderr.contains("unknown option")
            || stderr.contains("invalid option");
        if unknown_option && idx + 1 < attempts.len() {
            continue;
        }

        return Err(anyhow::anyhow!(
            "Failed to copy node_modules from {}: {}",
            source_node_modules.display(),
            truncate(&output.stderr, 240)
        ));
    }

    Err(anyhow::anyhow!(
        "Failed to copy node_modules from {}",
        source_node_modules.display()
    ))
}

fn tail_chars(s: &str, max_chars: usize) -> String {
    if max_chars == 0 {
        return String::new();
    }
    let count = s.chars().count();
    if count <= max_chars {
        return s.to_string();
    }
    s.chars()
        .skip(count.saturating_sub(max_chars))
        .collect::<String>()
}

fn run_quick_checks(
    repo_root: &Path,
    source_repo_root: Option<&Path>,
    notes: &mut Vec<String>,
    mode: ImplementationQuickChecksMode,
    timeout_ms: u64,
) -> anyhow::Result<(
    ImplementationQuickCheckStatus,
    Option<String>,
    Option<ImplementationCommandOutcome>,
)> {
    if mode == ImplementationQuickChecksMode::Disabled {
        return Ok((ImplementationQuickCheckStatus::Unavailable, None, None));
    }

    let Some(command) = detect_quick_check_command(repo_root) else {
        return Ok((ImplementationQuickCheckStatus::Unavailable, None, None));
    };

    if let Err(err) = ensure_quick_check_prereqs(repo_root, source_repo_root, &command, notes) {
        notes.push(format!(
            "quick_check_prereq_failed: {}",
            truncate(&err.to_string(), 160)
        ));
    }

    // If this looks like a JS repo check but deps are missing (or unusable for this check),
    // treat quick checks as unavailable rather than failing the whole apply.
    if command_needs_node_modules(repo_root, &command) {
        let node_modules = repo_root.join("node_modules");
        if !node_modules.exists() {
            notes.push("quick_check_unavailable_missing_node_modules".to_string());
            return Ok((
                ImplementationQuickCheckStatus::Unavailable,
                Some(command_to_string(&command)),
                None,
            ));
        }
        if quick_check_requires_real_node_modules(repo_root, &command)
            && is_node_modules_symlink(&node_modules)
        {
            notes.push("quick_check_unavailable_symlinked_node_modules".to_string());
            return Ok((
                ImplementationQuickCheckStatus::Unavailable,
                Some(command_to_string(&command)),
                None,
            ));
        }
    }

    let command_str = command_to_string(&command);
    let mut cmd = match command {
        QuickCheckCommand::Shell(shell_cmd) => {
            let mut command = Command::new("sh");
            command.current_dir(repo_root).arg("-lc").arg(shell_cmd);
            command
        }
        QuickCheckCommand::Program { program, args } => {
            let mut command = Command::new(program);
            command.current_dir(repo_root).args(args);
            command
        }
    };
    for (k, v) in SandboxSession::env_overrides() {
        cmd.env(k, v);
    }

    let start = std::time::Instant::now();
    let output = run_command_with_timeout(&mut cmd, Duration::from_millis(timeout_ms))
        .map_err(|e| anyhow::anyhow!("Quick check failed to start: {}", e))?;
    let outcome = ImplementationCommandOutcome {
        command: command_str.clone(),
        duration_ms: start.elapsed().as_millis() as u64,
        success: !output.timed_out && output.status.map(|s| s.success()).unwrap_or(false),
        timed_out: output.timed_out,
        exit_code: output.status.and_then(|s| s.code()),
        stdout_tail: tail_chars(&output.stdout, MAX_COMMAND_OUTPUT_TAIL_CHARS),
        stderr_tail: tail_chars(&output.stderr, MAX_COMMAND_OUTPUT_TAIL_CHARS),
    };
    let status = if outcome.success {
        ImplementationQuickCheckStatus::Passed
    } else {
        // Known sandbox limitation: Next/Turbopack rejects a symlinked `node_modules` root.
        // If we detect this, treat quick checks as unavailable (interactive can continue with
        // reduced confidence; lab/CI policies will still block).
        let stderr_lower = outcome.stderr_tail.to_ascii_lowercase();
        if stderr_lower.contains("symlink node_modules is invalid") {
            notes.push("quick_check_unavailable_next_symlink_rejected".to_string());
            ImplementationQuickCheckStatus::Unavailable
        } else {
            ImplementationQuickCheckStatus::Failed
        }
    };
    Ok((status, Some(command_str), Some(outcome)))
}

fn ensure_quick_check_prereqs(
    repo_root: &Path,
    source_repo_root: Option<&Path>,
    command: &QuickCheckCommand,
    notes: &mut Vec<String>,
) -> anyhow::Result<()> {
    // Most common failure in worktree sandboxes: JS deps are installed in the outer sandbox
    // but not present in nested attempt worktrees, so `pnpm type-check` fails immediately.
    // We keep this as a best-effort prereq step: it must not mutate repo-tracked state.
    let needs_js = repo_root.join("package.json").exists()
        && matches!(
            command,
            QuickCheckCommand::Shell(_) | QuickCheckCommand::Program { .. }
        );
    if !needs_js {
        return Ok(());
    }

    ensure_node_modules_present(repo_root, source_repo_root, command, notes)?;
    Ok(())
}

fn ensure_node_modules_present(
    repo_root: &Path,
    source_repo_root: Option<&Path>,
    command: &QuickCheckCommand,
    notes: &mut Vec<String>,
) -> anyhow::Result<()> {
    let node_modules = repo_root.join("node_modules");
    let needs_real_node_modules = quick_check_requires_real_node_modules(repo_root, command);

    if node_modules.exists() {
        if needs_real_node_modules && is_node_modules_symlink(&node_modules) {
            // Replace the symlink with a real directory so Next/Turbopack can run.
            let _ = std::fs::remove_file(&node_modules);
            let _ = std::fs::remove_dir_all(&node_modules);
        } else {
            return Ok(());
        }
    }

    let Some(source_root) = source_repo_root else {
        notes.push("node_modules_missing_no_source".to_string());
        return Ok(());
    };

    let source_node_modules = source_root.join("node_modules");
    if !source_node_modules.exists() {
        notes.push("node_modules_missing_in_source".to_string());
        return Ok(());
    }

    // Some tooling (notably Next/Turbopack) rejects a symlinked `node_modules` root.
    // Prefer a real directory for those checks.
    if needs_real_node_modules {
        match copy_node_modules_from_source(repo_root, &source_node_modules, &node_modules, notes) {
            Ok(()) => return Ok(()),
            Err(err) => {
                notes.push(format!(
                    "node_modules_copy_failed: {}",
                    truncate(&err.to_string(), 180)
                ));
                return Ok(());
            }
        }
    }

    // Default: create a symlink so quick checks use the already-installed dependencies from the source.
    // This preserves harness timing budgets and avoids re-installing packages per attempt.
    #[cfg(unix)]
    {
        use std::os::unix::fs::symlink;
        if node_modules.exists() {
            return Ok(());
        }
        // If a broken path exists, clear it.
        if let Ok(meta) = std::fs::symlink_metadata(&node_modules) {
            if meta.file_type().is_symlink() || meta.is_dir() || meta.is_file() {
                let _ = std::fs::remove_file(&node_modules);
                let _ = std::fs::remove_dir_all(&node_modules);
            }
        }
        symlink(&source_node_modules, &node_modules).map_err(|e| {
            anyhow::anyhow!(
                "Failed to symlink node_modules from {}: {}",
                source_node_modules.display(),
                e
            )
        })?;
        notes.push("linked_node_modules_from_source".to_string());
        Ok(())
    }

    #[cfg(not(unix))]
    {
        let _ = source_root; // avoid unused warnings on windows builds
        notes.push("node_modules_missing_on_non_unix".to_string());
        Ok(())
    }
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
) -> anyhow::Result<()> {
    diagnostics.finalization = ImplementationFinalizationDiagnostics {
        status,
        detail,
        mutation_on_failure,
    };
    let report_path = write_harness_report(repo_root, diagnostics)?;
    diagnostics.report_path = Some(report_path);
    append_harness_telemetry(repo_root, diagnostics)?;
    Ok(())
}

fn append_harness_telemetry(
    repo_root: &Path,
    diagnostics: &ImplementationRunDiagnostics,
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
    let record = ImplementationHarnessRecord {
        schema_version: 2,
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
    };
    let _ = cache.append_implementation_harness(&record);
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
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn plain_language_gate_rejects_jargony_text() {
        assert!(!is_plain_language_text(
            "Updated src/app.rs by changing fn do_work() -> Result<()> and serde impl details"
        ));
    }

    #[test]
    fn plain_language_gate_accepts_short_user_facing_text() {
        assert!(is_plain_language_text(
            "Users now see a clear error instead of a silent failure in this flow."
        ));
    }

    #[test]
    fn diff_line_parser_ignores_headers() {
        let sample = "\
diff --git a/a.rs b/a.rs
--- a/a.rs
+++ b/a.rs
@@ -1 +1 @@
-old
+new
";
        assert_eq!(parse_diff_changed_lines(sample), 2);
    }

    #[test]
    fn model_policy_uses_speed_tier() {
        assert_eq!(IMPLEMENTATION_MODEL.id(), "openai/gpt-oss-120b");
    }

    #[test]
    fn model_policy_rejects_non_speed_model() {
        assert!(ensure_implementation_model(Model::Smart).is_err());
        assert!(ensure_implementation_model(Model::Balanced).is_err());
        assert!(ensure_implementation_model(Model::Speed).is_ok());
    }

    #[test]
    fn deterministic_scope_gate_rejects_out_of_scope_files() {
        let changed_files = vec![PathBuf::from("src/a.rs"), PathBuf::from("src/b.rs")];
        let allowed_files = HashSet::from([PathBuf::from("src/a.rs")]);
        assert!(!deterministic_scope_gate(&changed_files, &allowed_files));
    }

    #[test]
    fn syntax_gate_rejects_parse_broken_outputs() {
        let root = tempdir().unwrap();
        std::fs::write(root.path().join("broken.rs"), "fn broken( {").unwrap();
        let result = syntax_gate(root.path(), &[PathBuf::from("broken.rs")]);
        assert!(result.is_err());
    }

    #[test]
    fn binary_write_gate_rejects_binary_extension() {
        let root = tempdir().unwrap();
        std::fs::write(root.path().join("logo.png"), "not-a-real-image").unwrap();
        let result = binary_write_gate(root.path(), &[PathBuf::from("logo.png")]);
        assert!(result.is_err());
    }

    #[test]
    fn quick_checks_disabled_returns_unavailable() {
        let root = tempdir().unwrap();
        let (status, command, outcome) = run_quick_checks(
            root.path(),
            None,
            &mut Vec::new(),
            ImplementationQuickChecksMode::Disabled,
            100,
        )
        .unwrap();
        assert_eq!(status, ImplementationQuickCheckStatus::Unavailable);
        assert!(command.is_none());
        assert!(outcome.is_none());
    }

    #[test]
    fn quick_check_policy_matrix_matches_profiles() {
        let interactive = ImplementationHarnessConfig::interactive_strict();
        let lab = ImplementationHarnessConfig::lab_strict();
        assert!(!interactive.require_quick_check_detectable);
        assert!(lab.require_quick_check_detectable);
        assert!(quick_check_passes_policy(
            ImplementationQuickCheckStatus::Unavailable,
            &interactive
        ));
        assert!(!quick_check_passes_policy(
            ImplementationQuickCheckStatus::Unavailable,
            &lab
        ));
    }

    #[test]
    fn quick_check_skips_next_lint_on_next16_and_falls_back_to_build() {
        let root = tempdir().unwrap();
        std::fs::write(
            root.path().join("package.json"),
            r#"{
  "name": "x",
  "private": true,
  "scripts": { "lint": "next lint", "build": "next build" },
  "dependencies": { "next": "^16.1.1" }
}"#,
        )
        .unwrap();

        let command = detect_quick_check_command(root.path()).expect("expected check command");
        match command {
            QuickCheckCommand::Program { program, args } => {
                assert_eq!(program, "npm");
                assert_eq!(
                    args,
                    vec![
                        "run".to_string(),
                        "build".to_string(),
                        "--silent".to_string()
                    ]
                );
            }
            _ => panic!("expected program quick check"),
        }
    }

    #[test]
    fn quick_check_skips_eslint_lint_when_eslint_missing_and_prefers_build() {
        let root = tempdir().unwrap();
        std::fs::write(root.path().join("pnpm-lock.yaml"), "").unwrap();
        std::fs::write(
            root.path().join("package.json"),
            r#"{
  "name": "x",
  "private": true,
  "scripts": { "lint": "eslint .", "build": "next build" },
  "dependencies": { "next": "16.0.10" }
}"#,
        )
        .unwrap();

        let command = detect_quick_check_command(root.path()).expect("expected check command");
        match command {
            QuickCheckCommand::Program { program, args } => {
                assert_eq!(program, "pnpm");
                assert_eq!(args, vec!["build".to_string()]);
            }
            _ => panic!("expected program quick check"),
        }
    }

    #[test]
    fn gate_reason_records_capture_gate_and_code() {
        let mut reasons = Vec::new();
        let mut records = Vec::new();
        push_fail_reason(
            &mut reasons,
            &mut records,
            "quick_check",
            REASON_QUICK_CHECK_UNAVAILABLE,
            "check command unavailable",
        );
        let mut gates = Vec::new();
        push_gate(
            &mut gates,
            "quick_check",
            false,
            "No detectable quick-check command",
            Some(REASON_QUICK_CHECK_UNAVAILABLE),
        );

        assert_eq!(reasons.len(), 1);
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].gate, "quick_check");
        assert_eq!(records[0].code, REASON_QUICK_CHECK_UNAVAILABLE);
        assert_eq!(
            gates[0].reason_code.as_deref(),
            Some(REASON_QUICK_CHECK_UNAVAILABLE)
        );
    }

    #[test]
    fn quick_check_failure_summary_extracts_next_ts_error() {
        let outcome = ImplementationCommandOutcome {
            command: "npm run build --silent".to_string(),
            duration_ms: 0,
            success: false,
            timed_out: false,
            exit_code: Some(1),
            stdout_tail: String::new(),
            stderr_tail: "Failed to compile.\n\n./lib/constants.ts:60:44\nType error: Cannot find name 'FontPreferenceId'.\n".to_string(),
        };

        let summary = summarize_quick_check_failure(&outcome).expect("expected summary");
        assert!(summary.contains("npm run build --silent"));
        assert!(summary.contains("lib/constants.ts:60:44"));
        assert!(summary.contains("Cannot find name 'FontPreferenceId'"));
    }

    #[test]
    fn quick_check_failure_summary_extracts_tsc_format() {
        let outcome = ImplementationCommandOutcome {
            command: "pnpm type-check".to_string(),
            duration_ms: 0,
            success: false,
            timed_out: false,
            exit_code: Some(2),
            stdout_tail: "src/foo.ts(12,34): error TS2304: Cannot find name 'X'.\n".to_string(),
            stderr_tail: String::new(),
        };

        let summary = summarize_quick_check_failure(&outcome).expect("expected summary");
        assert!(summary.contains("pnpm type-check"));
        assert!(summary.contains("src/foo.ts:12:34"));
        assert!(summary.contains("Cannot find name 'X'"));
    }

    #[test]
    fn quick_check_error_path_extraction_handles_multiple_formats_and_rejects_traversal() {
        let outcome = ImplementationCommandOutcome {
            command: "pnpm type-check".to_string(),
            duration_ms: 0,
            success: false,
            timed_out: false,
            exit_code: Some(2),
            stdout_tail: "src/foo.ts(12,34): error TS2304: Cannot find name 'X'.\n../oops.ts(1,1): error TS2304: bad\n"
                .to_string(),
            stderr_tail: "--> src/main.rs:7:9\nerror[E0425]: cannot find value\n".to_string(),
        };

        let paths = extract_quick_check_error_paths(&outcome);
        assert!(paths.contains(&PathBuf::from("src/foo.ts")), "{:?}", paths);
        assert!(paths.contains(&PathBuf::from("src/main.rs")), "{:?}", paths);
        assert!(!paths.contains(&PathBuf::from("../oops.ts")), "{:?}", paths);
    }

    #[test]
    fn budget_exhausted_triggers_cost_gate() {
        let budget = ImplementationBudget {
            started_at: std::time::Instant::now(),
            max_total_ms: u64::MAX,
            max_total_cost_usd: 0.01,
        };
        let usage = Usage {
            cost: Some(0.02),
            ..Usage::default()
        };

        let reason = budget
            .exhausted(&Some(usage))
            .expect("expected budget to be exhausted");
        assert_eq!(reason.gate, "budget");
        assert_eq!(reason.code, REASON_BUDGET_EXCEEDED);
        assert!(reason.message.to_ascii_lowercase().contains("cost"));
    }
}
