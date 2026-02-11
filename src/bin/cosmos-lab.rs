use anyhow::{anyhow, Context, Result};
use chrono::{DateTime, Utc};
use clap::{ArgAction, Args, Parser, Subcommand, ValueEnum};
use cosmos_tui::cache::{
    Cache, SelfIterationCommandOutcome, SelfIterationRunRecord, SelfIterationSuggestionMetrics,
};
use cosmos_tui::context::WorkContext;
use cosmos_tui::index::CodebaseIndex;
use cosmos_tui::lab::corpus::{sync_repo, CorpusManifest};
use cosmos_tui::lab::reliability::{
    classify_reliability_error, run_trial, run_trials, ReliabilityDiagnosticsSummary,
    ReliabilityTrialResult,
};
use cosmos_tui::lab::runner::{run_command, CommandSpec};
use cosmos_tui::lab::sandbox::SandboxSession;
use cosmos_tui::suggest::llm::{
    build_fix_preview_from_validated_suggestion, implement_validated_suggestion_with_harness,
    record_harness_finalization_outcome, run_fast_grounded_with_gate,
    ImplementationFinalizationStatus, ImplementationHarnessConfig, ImplementationHarnessRunContext,
    ImplementationReviewModel, ImplementationRunDiagnostics, SuggestionQualityGateConfig,
};
use cosmos_tui::suggest::{
    Priority, Suggestion, SuggestionEvidenceRef, SuggestionKind, SuggestionSource,
    SuggestionValidationState,
};
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use uuid::Uuid;

const DEFAULT_TARGET_REPO: &str = "/Users/cam/WebstormProjects/gielinor-gains";
const DEFAULT_CANARY_REPO_1: &str = "/Users/cam/WebstormProjects/stole-builder";
const DEFAULT_CANARY_REPO_2: &str = "/Users/cam/WebstormProjects/Jira-Ingress-Intel";

#[derive(Parser, Debug)]
#[command(
    name = "cosmos-lab",
    about = "Sandboxed maintainer validation and reliability loops for Cosmos"
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {
    Validate(ValidateArgs),
    Reliability(ReliabilityArgs),
    Implement(ImplementArgs),
}

#[derive(Args, Debug)]
struct ValidateArgs {
    #[arg(long, default_value = ".")]
    cosmos_repo: PathBuf,
    #[arg(long, default_value = DEFAULT_TARGET_REPO)]
    target_repo: PathBuf,
    #[arg(long, value_enum, default_value_t = ValidateMode::Fast)]
    mode: ValidateMode,
    #[arg(long, default_value_t = 4)]
    verify_sample: usize,
    #[arg(long)]
    output: Option<PathBuf>,
    #[arg(long)]
    keep_sandboxes: bool,
    #[arg(long)]
    enforce_quality_gate: bool,
    #[arg(long, default_value_t = 10)]
    gate_window: usize,
    #[arg(long, default_value_t = 0.95)]
    gate_min_displayed_validity: f64,
    #[arg(long, default_value_t = 10)]
    gate_min_final_count: usize,
    #[arg(long, default_value_t = 26_000)]
    gate_max_suggest_ms: u64,
    #[arg(long, default_value_t = 0.016)]
    gate_max_suggest_cost_usd: f64,
    #[arg(long, value_enum, default_value_t = GateSource::Both)]
    gate_source: GateSource,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq, ValueEnum)]
enum ValidateMode {
    Fast,
    Full,
}

impl ValidateMode {
    fn as_str(&self) -> &'static str {
        match self {
            ValidateMode::Fast => "fast",
            ValidateMode::Full => "full",
        }
    }
}

#[derive(Copy, Clone, Debug, Eq, PartialEq, ValueEnum)]
enum GateSource {
    Both,
    Validate,
    Reliability,
}

impl GateSource {
    fn as_str(&self) -> &'static str {
        match self {
            GateSource::Both => "both",
            GateSource::Validate => "validate",
            GateSource::Reliability => "reliability",
        }
    }

    fn includes_mode(&self, mode: &str) -> bool {
        match self {
            GateSource::Both => mode.starts_with("validate_") || mode == "reliability",
            GateSource::Validate => mode.starts_with("validate_"),
            GateSource::Reliability => mode == "reliability",
        }
    }
}

#[derive(Args, Debug)]
struct ReliabilityArgs {
    #[arg(long, default_value = DEFAULT_TARGET_REPO)]
    target_repo: PathBuf,
    #[arg(long, default_value_t = 3)]
    trials: usize,
    #[arg(long, default_value_t = 4)]
    verify_sample: usize,
    #[arg(long)]
    output: Option<PathBuf>,
    #[arg(long)]
    enforce_quality_gate: bool,
    #[arg(long, default_value_t = 10)]
    gate_window: usize,
    #[arg(long, default_value_t = 0.95)]
    gate_min_displayed_validity: f64,
    #[arg(long, default_value_t = 10)]
    gate_min_final_count: usize,
    #[arg(long, default_value_t = 26_000)]
    gate_max_suggest_ms: u64,
    #[arg(long, default_value_t = 0.016)]
    gate_max_suggest_cost_usd: f64,
    #[arg(long, value_enum, default_value_t = GateSource::Both)]
    gate_source: GateSource,
}

#[derive(Args, Debug)]
struct ImplementArgs {
    #[arg(long, default_value = ".")]
    cosmos_repo: PathBuf,
    #[arg(long, default_value = DEFAULT_TARGET_REPO)]
    target_repo: PathBuf,
    #[arg(
        long = "canary-repo",
        default_values = [DEFAULT_CANARY_REPO_1, DEFAULT_CANARY_REPO_2]
    )]
    canary_repos: Vec<PathBuf>,
    #[arg(long, default_value_t = 5)]
    sample_size: usize,
    #[arg(long)]
    output: Option<PathBuf>,
    #[arg(long)]
    keep_sandboxes: bool,
    #[arg(long)]
    enforce: bool,
    /// Run implement over a corpus manifest instead of a primary+canary set of local repos.
    #[arg(long)]
    corpus_manifest: Option<PathBuf>,
    /// Local corpus root where repos are cloned/checked out (defaults to .cosmos/corpus in the Cosmos repo).
    #[arg(long)]
    corpus_root: Option<PathBuf>,
    /// Sync (clone/fetch/checkout) corpus repos before running implement (default true; disable with --sync=false).
    #[arg(long, default_value_t = true, action = ArgAction::Set)]
    sync: bool,
    /// Limit how many enabled repos are used from the corpus manifest.
    #[arg(long)]
    max_repos: Option<usize>,
    /// Adversarial reviewer model for harness review gate (lab only).
    #[arg(long, value_enum, default_value_t = ImplementReviewModelArg::Smart)]
    review_model: ImplementReviewModelArg,
    /// Required number of consecutive passing corpus-manifest implement runs.
    /// Set to 0 to disable this gate.
    #[arg(long, default_value_t = 2)]
    require_consecutive_corpus_passes: usize,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq, ValueEnum)]
enum ImplementReviewModelArg {
    Speed,
    Smart,
}

impl From<ImplementReviewModelArg> for ImplementationReviewModel {
    fn from(value: ImplementReviewModelArg) -> Self {
        match value {
            ImplementReviewModelArg::Speed => ImplementationReviewModel::Speed,
            ImplementReviewModelArg::Smart => ImplementationReviewModel::Smart,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct LintCounts {
    errors: usize,
    warnings: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct LintBaselineFile {
    captured_at: DateTime<Utc>,
    command: String,
    counts: LintCounts,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct QualityGateResult {
    source: String,
    window: usize,
    min_displayed_validity: f64,
    min_final_count: usize,
    max_suggest_ms: u64,
    max_suggest_cost_usd: f64,
    evaluated_runs: usize,
    rolling_displayed_validity: Option<f64>,
    rolling_final_count: Option<f64>,
    rolling_suggest_ms: Option<f64>,
    rolling_suggest_cost_usd: Option<f64>,
    pending_violations: usize,
    warmup: bool,
    passed: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ValidateReport {
    timestamp: DateTime<Utc>,
    run_id: String,
    mode: String,
    cosmos_repo: PathBuf,
    target_repo: PathBuf,
    cosmos_sandbox: PathBuf,
    target_sandbox: PathBuf,
    command_outcomes: Vec<SelfIterationCommandOutcome>,
    lint_baseline: Option<LintCounts>,
    lint_result: Option<LintCounts>,
    lint_error_delta: Option<i64>,
    reliability_metrics: Option<SelfIterationSuggestionMetrics>,
    reliability_diagnostics: Option<ReliabilityDiagnosticsSummary>,
    #[serde(default)]
    reliability_failure_kind: Option<String>,
    #[serde(default)]
    quality_gate: Option<QualityGateResult>,
    passed: bool,
    notes: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ReliabilityReport {
    timestamp: DateTime<Utc>,
    run_id: String,
    cosmos_repo: PathBuf,
    target_repo: PathBuf,
    target_sandbox: PathBuf,
    trial_count: usize,
    verify_sample: usize,
    aggregated_metrics: Option<SelfIterationSuggestionMetrics>,
    trial_results: Vec<ReliabilityTrialResult>,
    #[serde(default)]
    reliability_failure_kind: Option<String>,
    #[serde(default)]
    quality_gate: Option<QualityGateResult>,
    passed: bool,
    notes: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ImplementSuggestionResult {
    repo_path: PathBuf,
    suggestion_id: String,
    summary: String,
    passed: bool,
    attempt_count: usize,
    total_ms: u64,
    total_cost_usd: f64,
    file_changes: usize,
    first_attempt_passed: bool,
    #[serde(default)]
    fail_reasons: Vec<String>,
    #[serde(default)]
    report_path: Option<PathBuf>,
    #[serde(default)]
    residual_blocking: usize,
    #[serde(default)]
    syntax_failure_after_pass: bool,
    #[serde(default)]
    mutation_on_failure: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct FailureCluster {
    gate: String,
    reason_code: String,
    count: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ImplementRepoReport {
    repo_path: PathBuf,
    #[serde(default)]
    repo_id: Option<String>,
    #[serde(default)]
    requested_ref: Option<String>,
    #[serde(default)]
    head_sha: Option<String>,
    #[serde(default)]
    subdir: Option<String>,
    sample_size: usize,
    candidate_count: usize,
    executed_count: usize,
    passed_count: usize,
    first_attempt_pass_count: usize,
    avg_total_ms: Option<f64>,
    avg_total_cost_usd: Option<f64>,
    pass_rate: Option<f64>,
    first_attempt_pass_rate: Option<f64>,
    #[serde(default)]
    residual_blocking_rate: Option<f64>,
    #[serde(default)]
    syntax_failure_after_pass_rate: Option<f64>,
    #[serde(default)]
    mutation_on_failure_rate: Option<f64>,
    #[serde(default)]
    quick_check_detected: bool,
    #[serde(default)]
    quick_check_command: Option<String>,
    #[serde(default)]
    quick_check_passed_cases: usize,
    #[serde(default)]
    quick_check_failed_cases: usize,
    #[serde(default)]
    quick_check_unavailable_cases: usize,
    #[serde(default)]
    independent_review_required_count: usize,
    #[serde(default)]
    independent_review_executed_count: usize,
    #[serde(default)]
    independent_review_miss_count: usize,
    #[serde(default)]
    failure_reason_histogram: HashMap<String, usize>,
    #[serde(default)]
    top_failure_clusters: Vec<FailureCluster>,
    results: Vec<ImplementSuggestionResult>,
    passed: bool,
    #[serde(default)]
    notes: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ImplementReport {
    timestamp: DateTime<Utc>,
    run_id: String,
    cosmos_repo: PathBuf,
    primary_target_repo: PathBuf,
    canary_repos: Vec<PathBuf>,
    #[serde(default)]
    corpus_manifest: Option<PathBuf>,
    sample_size: usize,
    total_candidate_count: usize,
    executed_count: usize,
    passed_count: usize,
    first_attempt_pass_count: usize,
    avg_total_ms: Option<f64>,
    avg_total_cost_usd: Option<f64>,
    pass_rate: Option<f64>,
    first_attempt_pass_rate: Option<f64>,
    #[serde(default)]
    residual_blocking_rate: Option<f64>,
    #[serde(default)]
    syntax_failure_after_pass_rate: Option<f64>,
    #[serde(default)]
    mutation_on_failure_rate: Option<f64>,
    #[serde(default)]
    independent_review_required_count: usize,
    #[serde(default)]
    independent_review_executed_count: usize,
    #[serde(default)]
    independent_review_miss_count: usize,
    #[serde(default)]
    required_consecutive_corpus_passes: Option<usize>,
    #[serde(default)]
    consecutive_corpus_pass_count: Option<usize>,
    #[serde(default)]
    failure_reason_histogram: HashMap<String, usize>,
    #[serde(default)]
    top_failure_clusters: Vec<FailureCluster>,
    repo_reports: Vec<ImplementRepoReport>,
    passed: bool,
    notes: Vec<String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Commands::Validate(args) => run_validate(args).await,
        Commands::Reliability(args) => run_reliability(args).await,
        Commands::Implement(args) => run_implement(args).await,
    }
}

async fn run_validate(args: ValidateArgs) -> Result<()> {
    let run_id = Uuid::new_v4().to_string();
    let cosmos_repo = canonical_repo_path(&args.cosmos_repo, "cosmos repo")?;
    let target_repo = canonical_repo_path(&args.target_repo, "target repo")?;

    let cosmos_sandbox = SandboxSession::create(&cosmos_repo, &run_id, "cosmos", true)?;
    let target_sandbox = SandboxSession::create(&target_repo, &run_id, "target-repo", true)?;
    let sandbox_env = SandboxSession::env_overrides();
    let mut notes = Vec::new();
    let mut outcomes = Vec::new();
    if let Some(install_outcome) =
        prepare_target_workspace(target_sandbox.path(), &sandbox_env, &mut notes)
    {
        outcomes.push(install_outcome);
    }

    let fast_specs = vec![
        CommandSpec::new("cosmos:cargo_test", cosmos_sandbox.path(), "cargo")
            .args(&["test", "--locked", "--", "--test-threads=1"])
            .timeout_secs(1_800)
            .with_env_overrides(&sandbox_env),
        CommandSpec::new("target:test_once", target_sandbox.path(), "pnpm")
            .args(&["test:once"])
            .timeout_secs(900)
            .with_env_overrides(&sandbox_env),
        CommandSpec::new("target:type_check", target_sandbox.path(), "pnpm")
            .args(&["type-check"])
            .timeout_secs(900)
            .with_env_overrides(&sandbox_env),
    ];
    for spec in fast_specs {
        outcomes.push(run_command(&spec));
    }

    let mut lint_baseline = None;
    let mut lint_result = None;
    let mut lint_error_delta = None;
    if args.mode == ValidateMode::Full {
        let full_specs = vec![
            CommandSpec::new("cosmos:perf_gate", cosmos_sandbox.path(), "bash")
                .args(&["scripts/perf/gate.sh"])
                .timeout_secs(2_400)
                .with_env_overrides(&sandbox_env),
            CommandSpec::new("target:build", target_sandbox.path(), "pnpm")
                .args(&["build"])
                .timeout_secs(2_400)
                .with_env_overrides(&sandbox_env),
        ];
        for spec in full_specs {
            outcomes.push(run_command(&spec));
        }

        match ensure_lint_baseline(&target_repo, &sandbox_env) {
            Ok(baseline) => {
                lint_baseline = Some(baseline.clone());
                let mut lint_outcome = run_command(
                    &CommandSpec::new("target:lint", target_sandbox.path(), "pnpm")
                        .args(&["lint"])
                        .timeout_secs(1_800)
                        .with_env_overrides(&sandbox_env),
                );
                lint_result = parse_lint_counts(&lint_outcome);

                if let Some(current) = lint_result.clone() {
                    let delta = current.errors as i64 - baseline.errors as i64;
                    lint_error_delta = Some(delta);
                    if delta <= 0 {
                        lint_outcome.success = true;
                        lint_outcome.note = Some(format!(
                            "Non-blocking baseline policy: current errors={} baseline errors={}",
                            current.errors, baseline.errors
                        ));
                    } else {
                        lint_outcome.note = Some(format!(
                            "Lint errors increased above baseline (current={}, baseline={})",
                            current.errors, baseline.errors
                        ));
                    }
                } else {
                    lint_outcome.note = Some(
                        "Could not parse lint error/warning counts from lint output".to_string(),
                    );
                }
                outcomes.push(lint_outcome);
            }
            Err(error) => {
                notes.push(format!("Failed to capture/read lint baseline: {}", error));
            }
        }
    }

    let mut reliability_metrics = None;
    let mut reliability_diagnostics = None;
    let mut reliability_failure_kind = None;
    if fake_reliability_enabled() {
        let trial = fake_trial_result(target_sandbox.path(), args.verify_sample);
        reliability_metrics = Some(trial.metrics.clone());
        reliability_diagnostics = Some(trial.diagnostics);
    } else {
        match run_trial(target_sandbox.path(), args.verify_sample).await {
            Ok(trial) => {
                reliability_metrics = Some(trial.metrics.clone());
                reliability_diagnostics = Some(trial.diagnostics);
            }
            Err(error) => {
                let kind = classify_reliability_error(&error);
                let kind_str = kind.as_str().to_string();
                reliability_failure_kind = Some(kind_str.clone());
                notes.push(format!(
                    "Reliability trial failed [kind={}]: {}",
                    kind_str, error
                ));
            }
        }
    }

    let mut passed =
        outcomes.iter().all(|outcome| outcome.success) && reliability_metrics.is_some();
    let mode_key = format!("validate_{}", args.mode.as_str());
    let cache = Cache::new(&cosmos_repo);
    let quality_gate = if args.enforce_quality_gate {
        let gate = evaluate_quality_gate(
            &cache,
            args.gate_source,
            args.gate_window,
            args.gate_min_displayed_validity,
            args.gate_min_final_count,
            args.gate_max_suggest_ms,
            args.gate_max_suggest_cost_usd,
            &mode_key,
            reliability_metrics.as_ref(),
        )?;
        if gate.warmup {
            notes.push(format!(
                "quality_gate_warmup: source={} runs={} window={} min_displayed_validity={:.2} min_final_count={} max_suggest_ms={} max_suggest_cost_usd={:.4}",
                gate.source,
                gate.evaluated_runs,
                gate.window,
                gate.min_displayed_validity,
                gate.min_final_count,
                gate.max_suggest_ms,
                gate.max_suggest_cost_usd
            ));
        } else if gate.passed {
            notes.push(format!(
                "quality_gate_pass: source={} displayed_validity={:.3} final_count={:.2} suggest_ms={:.2} suggest_cost_usd={:.5} window={}",
                gate.source,
                gate.rolling_displayed_validity.unwrap_or(0.0),
                gate.rolling_final_count.unwrap_or(0.0),
                gate.rolling_suggest_ms.unwrap_or(0.0),
                gate.rolling_suggest_cost_usd.unwrap_or(0.0),
                gate.window
            ));
        } else {
            passed = false;
            notes.push(format!(
                "quality_gate_fail: source={} displayed_validity={:.3}/{:.2} final_count={:.2}/{} suggest_ms={:.2}/{} suggest_cost_usd={:.5}/{:.5} pending_violations={} window={}",
                gate.source,
                gate.rolling_displayed_validity.unwrap_or(0.0),
                gate.min_displayed_validity,
                gate.rolling_final_count.unwrap_or(0.0),
                gate.min_final_count,
                gate.rolling_suggest_ms.unwrap_or(0.0),
                gate.max_suggest_ms,
                gate.rolling_suggest_cost_usd.unwrap_or(0.0),
                gate.max_suggest_cost_usd,
                gate.pending_violations,
                gate.window
            ));
        }
        Some(gate)
    } else {
        None
    };

    if args.keep_sandboxes {
        notes.push(format!(
            "Keeping sandboxes for debugging: '{}' and '{}'",
            cosmos_sandbox.path().display(),
            target_sandbox.path().display()
        ));
    } else {
        if let Err(error) = cosmos_sandbox.cleanup() {
            passed = false;
            notes.push(format!("Failed to cleanup cosmos sandbox: {}", error));
        }
        if let Err(error) = target_sandbox.cleanup() {
            passed = false;
            notes.push(format!("Failed to cleanup target sandbox: {}", error));
        }
    }

    let report = ValidateReport {
        timestamp: Utc::now(),
        run_id: run_id.clone(),
        mode: args.mode.as_str().to_string(),
        cosmos_repo: cosmos_repo.clone(),
        target_repo: target_repo.clone(),
        cosmos_sandbox: cosmos_sandbox.path().to_path_buf(),
        target_sandbox: target_sandbox.path().to_path_buf(),
        command_outcomes: outcomes.clone(),
        lint_baseline,
        lint_result,
        lint_error_delta,
        reliability_metrics: reliability_metrics.clone(),
        reliability_diagnostics,
        reliability_failure_kind,
        quality_gate: quality_gate.clone(),
        passed,
        notes: notes.clone(),
    };

    let output_path = output_path(
        args.output.as_ref(),
        &cosmos_repo,
        "validate",
        &run_id,
        args.mode.as_str(),
    );
    write_report_json(&output_path, &report)?;

    let telemetry = SelfIterationRunRecord {
        timestamp: Utc::now(),
        run_id: run_id.clone(),
        mode: mode_key,
        cosmos_repo: cosmos_repo.clone(),
        target_repo: target_repo.clone(),
        passed,
        command_outcomes: outcomes,
        reliability_metrics,
        report_path: Some(output_path.clone()),
        notes,
    };
    let _ = cache.append_self_iteration_run(&telemetry);

    println!("Run ID: {}", run_id);
    println!("Mode: {}", args.mode.as_str());
    println!("Passed: {}", passed);
    println!("Report: {}", output_path.display());
    Ok(())
}

async fn run_reliability(args: ReliabilityArgs) -> Result<()> {
    let run_id = Uuid::new_v4().to_string();
    let cosmos_repo = std::env::current_dir()
        .context("Failed to read current working directory")?
        .canonicalize()
        .context("Failed to resolve current working directory")?;
    let target_repo = canonical_repo_path(&args.target_repo, "target repo")?;

    let target_sandbox = SandboxSession::create(&target_repo, &run_id, "target-repo", true)?;
    let mut notes = Vec::new();
    let mut reliability_failure_kind = None;
    let cache = Cache::new(&cosmos_repo);

    let (aggregated_metrics, trial_results, mut passed) = if fake_reliability_enabled() {
        let mut trials = Vec::new();
        for _ in 0..args.trials.max(1) {
            trials.push(fake_trial_result(target_sandbox.path(), args.verify_sample));
        }
        let aggregate = cosmos_tui::lab::reliability::aggregate_trial_metrics(
            &trials
                .iter()
                .map(|trial| trial.metrics.clone())
                .collect::<Vec<_>>(),
        );
        (Some(aggregate), trials, true)
    } else {
        let run = run_trials(target_sandbox.path(), args.trials, args.verify_sample).await;
        match run {
            Ok(result) => (Some(result.aggregated), result.trials, true),
            Err(error) => {
                let kind = classify_reliability_error(&error);
                let kind_str = kind.as_str().to_string();
                reliability_failure_kind = Some(kind_str.clone());
                notes.push(format!(
                    "Reliability run failed [kind={}]: {}",
                    kind_str, error
                ));
                (None, Vec::new(), false)
            }
        }
    };

    let quality_gate = if args.enforce_quality_gate {
        let gate = evaluate_quality_gate(
            &cache,
            args.gate_source,
            args.gate_window,
            args.gate_min_displayed_validity,
            args.gate_min_final_count,
            args.gate_max_suggest_ms,
            args.gate_max_suggest_cost_usd,
            "reliability",
            aggregated_metrics.as_ref(),
        )?;
        if gate.warmup {
            notes.push(format!(
                "quality_gate_warmup: source={} runs={} window={} min_displayed_validity={:.2} min_final_count={} max_suggest_ms={} max_suggest_cost_usd={:.4}",
                gate.source,
                gate.evaluated_runs,
                gate.window,
                gate.min_displayed_validity,
                gate.min_final_count,
                gate.max_suggest_ms,
                gate.max_suggest_cost_usd
            ));
        } else if gate.passed {
            notes.push(format!(
                "quality_gate_pass: source={} displayed_validity={:.3} final_count={:.2} suggest_ms={:.2} suggest_cost_usd={:.5} window={}",
                gate.source,
                gate.rolling_displayed_validity.unwrap_or(0.0),
                gate.rolling_final_count.unwrap_or(0.0),
                gate.rolling_suggest_ms.unwrap_or(0.0),
                gate.rolling_suggest_cost_usd.unwrap_or(0.0),
                gate.window
            ));
        } else {
            passed = false;
            notes.push(format!(
                "quality_gate_fail: source={} displayed_validity={:.3}/{:.2} final_count={:.2}/{} suggest_ms={:.2}/{} suggest_cost_usd={:.5}/{:.5} pending_violations={} window={}",
                gate.source,
                gate.rolling_displayed_validity.unwrap_or(0.0),
                gate.min_displayed_validity,
                gate.rolling_final_count.unwrap_or(0.0),
                gate.min_final_count,
                gate.rolling_suggest_ms.unwrap_or(0.0),
                gate.max_suggest_ms,
                gate.rolling_suggest_cost_usd.unwrap_or(0.0),
                gate.max_suggest_cost_usd,
                gate.pending_violations,
                gate.window
            ));
        }
        Some(gate)
    } else {
        None
    };

    if let Err(error) = target_sandbox.cleanup() {
        passed = false;
        notes.push(format!("Failed to cleanup target sandbox: {}", error));
    }

    let report = ReliabilityReport {
        timestamp: Utc::now(),
        run_id: run_id.clone(),
        cosmos_repo: cosmos_repo.clone(),
        target_repo: target_repo.clone(),
        target_sandbox: target_sandbox.path().to_path_buf(),
        trial_count: args.trials.max(1),
        verify_sample: args.verify_sample,
        aggregated_metrics: aggregated_metrics.clone(),
        trial_results: trial_results.clone(),
        reliability_failure_kind,
        quality_gate: quality_gate.clone(),
        passed,
        notes: notes.clone(),
    };

    let output_path = output_path(
        args.output.as_ref(),
        &cosmos_repo,
        "reliability",
        &run_id,
        "",
    );
    write_report_json(&output_path, &report)?;

    let telemetry = SelfIterationRunRecord {
        timestamp: Utc::now(),
        run_id: run_id.clone(),
        mode: "reliability".to_string(),
        cosmos_repo: cosmos_repo.clone(),
        target_repo: target_repo.clone(),
        passed,
        command_outcomes: Vec::new(),
        reliability_metrics: aggregated_metrics,
        report_path: Some(output_path.clone()),
        notes,
    };
    let _ = cache.append_self_iteration_run(&telemetry);

    println!("Run ID: {}", run_id);
    println!("Passed: {}", passed);
    println!("Report: {}", output_path.display());
    Ok(())
}

async fn run_implement(args: ImplementArgs) -> Result<()> {
    let run_id = Uuid::new_v4().to_string();
    let cosmos_repo = canonical_repo_path(&args.cosmos_repo, "cosmos repo")?;
    let sandbox_env = SandboxSession::env_overrides();
    let mut notes = Vec::new();
    let fake_mode = fake_implement_enabled();
    let shadow_gate_blocking = implement_shadow_gates_blocking_enabled();
    let default_sample_size = args.sample_size.max(1);
    let mut repo_reports = Vec::new();

    println!("Implement Run ID: {}", run_id);
    println!("Fake mode: {}", fake_mode);
    println!("Default sample size: {}", default_sample_size);
    println!("Adversarial review model: {:?}", args.review_model);
    notes.push(format!("Adversarial review model: {:?}", args.review_model));
    notes.push(format!(
        "Shadow gate mode: {}",
        if shadow_gate_blocking {
            "blocking"
        } else {
            "advisory"
        }
    ));

    #[derive(Debug, Clone)]
    struct ImplementTarget {
        repo_path: PathBuf,
        repo_id: Option<String>,
        requested_ref: Option<String>,
        head_sha: Option<String>,
        subdir: Option<String>,
        sample_size: usize,
    }

    let mut targets: Vec<ImplementTarget> = Vec::new();
    let primary_target_repo: PathBuf;
    let canary_repos: Vec<PathBuf>;
    let mut corpus_manifest_path: Option<PathBuf> = None;

    if let Some(manifest_path) = args.corpus_manifest.as_ref() {
        let manifest_path = if manifest_path.is_absolute() {
            manifest_path.clone()
        } else {
            cosmos_repo.join(manifest_path)
        };
        let corpus_root = match args.corpus_root.as_ref() {
            Some(root) if root.is_absolute() => root.clone(),
            Some(root) => cosmos_repo.join(root),
            None => cosmos_repo.join(".cosmos").join("corpus"),
        };

        println!("Corpus manifest: {}", manifest_path.display());
        println!("Corpus root: {}", corpus_root.display());
        notes.push(format!("corpus_manifest={}", manifest_path.display()));
        corpus_manifest_path = Some(manifest_path.clone());

        let manifest = CorpusManifest::load(&manifest_path)?;
        let mut specs = manifest
            .repo
            .into_iter()
            .filter(|spec| spec.enabled)
            .collect::<Vec<_>>();
        if let Some(max) = args.max_repos {
            specs.truncate(max);
        }
        if specs.is_empty() {
            return Err(anyhow!(
                "Corpus manifest '{}' contains no enabled repos",
                manifest_path.display()
            ));
        }

        for spec in specs {
            let checkout = sync_repo(&spec, &corpus_root, args.sync)
                .with_context(|| format!("Failed to sync corpus repo '{}'", spec.id))?;
            let repo_path = canonical_repo_path(&checkout.local_path, "corpus repo")?;
            targets.push(ImplementTarget {
                repo_path,
                repo_id: Some(checkout.id),
                requested_ref: Some(checkout.requested_ref),
                head_sha: Some(checkout.head_sha),
                subdir: checkout.subdir,
                sample_size: spec
                    .sample_size_override
                    .unwrap_or(default_sample_size)
                    .max(1),
            });
        }

        primary_target_repo = targets
            .first()
            .map(|target| target.repo_path.clone())
            .unwrap_or_else(|| corpus_root.clone());
        canary_repos = targets
            .iter()
            .skip(1)
            .map(|target| target.repo_path.clone())
            .collect();
        notes.push(format!(
            "Using corpus manifest '{}' (root '{}')",
            manifest_path.display(),
            corpus_root.display()
        ));
    } else {
        primary_target_repo = canonical_repo_path(&args.target_repo, "target repo")?;
        let mut all_targets = vec![primary_target_repo.clone()];
        for canary in &args.canary_repos {
            match canonical_repo_path(canary, "canary repo") {
                Ok(path) => {
                    if !all_targets.contains(&path) {
                        all_targets.push(path);
                    }
                }
                Err(error) => {
                    return Err(anyhow!(
                        "Failed to resolve canary repo '{}': {}",
                        canary.display(),
                        error
                    ));
                }
            }
        }

        targets = all_targets
            .into_iter()
            .map(|repo_path| ImplementTarget {
                repo_path,
                repo_id: None,
                requested_ref: None,
                head_sha: None,
                subdir: None,
                sample_size: default_sample_size,
            })
            .collect();
        canary_repos = targets
            .iter()
            .skip(1)
            .map(|target| target.repo_path.clone())
            .collect();
    }

    let mut global_reason_histogram: HashMap<String, usize> = HashMap::new();

    println!("Repos to run: {}", targets.len());

    for (repo_idx, target) in targets.iter().enumerate() {
        let repo_root = &target.repo_path;
        let sample_size = target.sample_size.max(1);
        let mut repo_notes = Vec::new();
        let repo_run_id = format!("{}-repo-{}", run_id, repo_idx + 1);
        let repo_label = target
            .repo_id
            .as_deref()
            .map(|id| id.to_string())
            .unwrap_or_else(|| repo_root.display().to_string());
        println!();
        println!("[{}/{}] Repo: {}", repo_idx + 1, targets.len(), repo_label);
        println!("Path: {}", repo_root.display());
        if let Some(requested_ref) = target.requested_ref.as_deref() {
            println!("Requested ref: {}", requested_ref);
        }
        if let Some(head_sha) = target.head_sha.as_deref() {
            println!("HEAD SHA: {}", head_sha);
        }
        if let Some(subdir) = target.subdir.as_deref() {
            println!("Subdir: {}", subdir);
        }
        println!("Sample size: {}", sample_size);

        if !fake_mode {
            if let Some(tool) = missing_required_tool_for_repo(repo_root) {
                let note = format!(
                    "Skipped repo because required tool '{}' is not available on PATH",
                    tool
                );
                println!("{}", note);
                repo_notes.push(note);
                repo_reports.push(ImplementRepoReport {
                    repo_path: repo_root.clone(),
                    repo_id: target.repo_id.clone(),
                    requested_ref: target.requested_ref.clone(),
                    head_sha: target.head_sha.clone(),
                    subdir: target.subdir.clone(),
                    sample_size,
                    candidate_count: 0,
                    executed_count: 0,
                    passed_count: 0,
                    first_attempt_pass_count: 0,
                    avg_total_ms: None,
                    avg_total_cost_usd: None,
                    pass_rate: None,
                    first_attempt_pass_rate: None,
                    residual_blocking_rate: None,
                    syntax_failure_after_pass_rate: None,
                    mutation_on_failure_rate: None,
                    quick_check_detected: false,
                    quick_check_command: None,
                    quick_check_passed_cases: 0,
                    quick_check_failed_cases: 0,
                    quick_check_unavailable_cases: 0,
                    independent_review_required_count: 0,
                    independent_review_executed_count: 0,
                    independent_review_miss_count: 0,
                    failure_reason_histogram: HashMap::new(),
                    top_failure_clusters: Vec::new(),
                    results: Vec::new(),
                    passed: false,
                    notes: repo_notes,
                });
                continue;
            }
        }

        let base_sandbox =
            SandboxSession::create(repo_root, &repo_run_id, "target-impl-base", true)?;
        println!("Base sandbox: {}", base_sandbox.path().display());
        if !fake_mode {
            if let Some(install_outcome) =
                prepare_target_workspace(base_sandbox.path(), &sandbox_env, &mut repo_notes)
            {
                if !install_outcome.success {
                    repo_notes.push(
                        "Base sandbox dependency install failed; implementation runs may fail"
                            .to_string(),
                    );
                }
                println!(
                    "Dependency prep: {} ({}ms)",
                    if install_outcome.success {
                        "ok"
                    } else {
                        "failed"
                    },
                    install_outcome.duration_ms
                );
            }
        }

        let (analysis_root, path_prefix) = match target.subdir.as_deref() {
            Some(subdir) => {
                let root = base_sandbox.path().join(subdir);
                if !root.exists() {
                    return Err(anyhow!(
                        "Corpus repo subdir '{}' does not exist in base sandbox {}",
                        subdir,
                        base_sandbox.path().display()
                    ));
                }
                (root, Some(PathBuf::from(subdir)))
            }
            None => (base_sandbox.path().to_path_buf(), None),
        };

        let index = CodebaseIndex::new(&analysis_root).with_context(|| {
            format!(
                "Failed to build codebase index for implement command in {}",
                repo_root.display()
            )
        })?;
        let context = WorkContext::load(&analysis_root).with_context(|| {
            format!(
                "Failed to load work context for implement command in {}",
                repo_root.display()
            )
        })?;
        let sampled = if fake_mode {
            fake_validated_suggestions(&index, sample_size)
        } else {
            let cache = Cache::new(base_sandbox.path());
            let summaries = cache.load_llm_summaries_cache().map(|summaries_cache| {
                summaries_cache
                    .summaries
                    .into_iter()
                    .map(|(path, entry)| (path, entry.summary))
                    .collect::<HashMap<PathBuf, String>>()
            });

            let suggest_result = run_fast_grounded_with_gate(
                &analysis_root,
                &index,
                &context,
                None,
                summaries.as_ref(),
                SuggestionQualityGateConfig::default(),
            )
            .await
            .with_context(|| {
                format!(
                    "Failed to generate validated suggestions for {}",
                    repo_root.display()
                )
            })?;

            suggest_result
                .suggestions
                .into_iter()
                .filter(|suggestion| {
                    suggestion.validation_state == SuggestionValidationState::Validated
                })
                .take(sample_size)
                .collect::<Vec<Suggestion>>()
        };
        let sampled = if let Some(prefix) = path_prefix.as_ref() {
            sampled
                .into_iter()
                .map(|mut suggestion| {
                    prefix_suggestion_paths(&mut suggestion, prefix);
                    suggestion
                })
                .collect::<Vec<_>>()
        } else {
            sampled
        };

        let candidate_count = sampled.len();
        if candidate_count == 0 {
            repo_notes.push(
                "No validated suggestions available for implementation harness sampling"
                    .to_string(),
            );
        }
        println!("Validated suggestions (sampled): {}", candidate_count);

        let mut results = Vec::new();
        let mut passed_count = 0usize;
        let mut first_attempt_pass_count = 0usize;
        let mut total_ms_sum = 0u64;
        let mut total_cost_sum = 0.0f64;
        let mut blocking_residual_count = 0usize;
        let mut syntax_failure_after_pass_count = 0usize;
        let mut mutation_on_failure_count = 0usize;
        let mut reason_histogram: HashMap<String, usize> = HashMap::new();
        let mut quick_check_detected = false;
        let mut quick_check_command: Option<String> = None;
        let mut quick_check_passed_cases = 0usize;
        let mut quick_check_failed_cases = 0usize;
        let mut quick_check_unavailable_cases = 0usize;
        let mut independent_review_required_count = 0usize;
        let mut independent_review_executed_count = 0usize;
        let mut independent_review_miss_count = 0usize;
        let mut finalization_telemetry_error_count = 0usize;

        for (idx, suggestion) in sampled.into_iter().enumerate() {
            let case_run_id = format!("{}-case-{}", repo_run_id, idx + 1);
            let case_label = format!("target-impl-{}", idx + 1);
            let case_sandbox = SandboxSession::create(repo_root, &case_run_id, &case_label, true)
                .with_context(|| {
                format!(
                    "Failed to create sandbox for suggestion {} in {}",
                    idx + 1,
                    repo_root.display()
                )
            })?;
            println!(
                "  Case {}/{}: suggestion {}",
                idx + 1,
                candidate_count.max(1),
                suggestion.id
            );

            if !fake_mode {
                let linked = link_node_modules_from_base(
                    base_sandbox.path(),
                    case_sandbox.path(),
                    &mut repo_notes,
                );
                if !linked {
                    if let Some(install_outcome) =
                        prepare_target_workspace(case_sandbox.path(), &sandbox_env, &mut repo_notes)
                    {
                        if !install_outcome.success {
                            repo_notes.push(format!(
                                "Dependency install failed in case sandbox {} for {}",
                                idx + 1,
                                repo_root.display()
                            ));
                        }
                    }
                }
            }

            let preview = build_fix_preview_from_validated_suggestion(&suggestion);
            let run: Result<cosmos_tui::suggest::llm::ImplementationRunResult> = if fake_mode {
                Ok(cosmos_tui::suggest::llm::ImplementationRunResult {
                    description: "Fake harness run for deterministic lab test".to_string(),
                    file_changes: Vec::new(),
                    usage: None,
                    diagnostics: cosmos_tui::suggest::llm::ImplementationRunDiagnostics {
                        run_id: format!("fake-{}-{}", repo_idx + 1, idx + 1),
                        suggestion_id: suggestion.id.to_string(),
                        suggestion_summary: suggestion.summary.clone(),
                        model: "openai/gpt-oss-120b".to_string(),
                        strict_mode: true,
                        passed: true,
                        attempt_count: 1,
                        total_ms: 15_000,
                        total_cost_usd: 0.005,
                        reduced_confidence: false,
                        fail_reasons: Vec::new(),
                        attempts: vec![
                            cosmos_tui::suggest::llm::ImplementationAttemptDiagnostics {
                                attempt_index: 1,
                                passed: true,
                                fail_reasons: Vec::new(),
                                fail_reason_records: Vec::new(),
                                gates: Vec::new(),
                                changed_files: Vec::new(),
                                changed_lines_total: 0,
                                changed_lines_by_file: HashMap::new(),
                                quick_check_status:
                                    cosmos_tui::suggest::llm::ImplementationQuickCheckStatus::Passed,
                                quick_check_command: Some("fake check".to_string()),
                                quick_check_outcome: None,
                                quick_check_outcomes: Vec::new(),
                                quick_check_fix_loops: 0,
                                quick_check_failure_summary: None,
                                review_iterations: 1,
                                review_blocking_remaining: 0,
                                remaining_blocking_titles: Vec::new(),
                                remaining_blocking_categories: Vec::new(),
                                attempt_ms: 15_000,
                                attempt_cost_usd: 0.005,
                                llm_calls: Vec::new(),
                                notes: vec!["fake".to_string()],
                            },
                        ],
                        report_path: None,
                        finalization:
                            cosmos_tui::suggest::llm::ImplementationFinalizationDiagnostics::default(
                            ),
                    },
                })
            } else {
                let mut harness_config = ImplementationHarnessConfig::lab_strict();
                harness_config.adversarial_review_model = args.review_model.into();
                implement_validated_suggestion_with_harness(
                    case_sandbox.path(),
                    &suggestion,
                    &preview,
                    None,
                    harness_config,
                )
                .await
            };

            let mut case_result = ImplementSuggestionResult {
                repo_path: repo_root.clone(),
                suggestion_id: suggestion.id.to_string(),
                summary: suggestion.summary.clone(),
                passed: false,
                attempt_count: 0,
                total_ms: 0,
                total_cost_usd: 0.0,
                file_changes: 0,
                first_attempt_passed: false,
                fail_reasons: Vec::new(),
                report_path: None,
                residual_blocking: 0,
                syntax_failure_after_pass: false,
                mutation_on_failure: false,
            };

            match run {
                Ok(mut result) => {
                    let first_attempt_passed = result
                        .diagnostics
                        .attempts
                        .first()
                        .map(|attempt| attempt.passed)
                        .unwrap_or(false);
                    let residual_blocking = result
                        .diagnostics
                        .attempts
                        .last()
                        .map(|attempt| attempt.review_blocking_remaining)
                        .unwrap_or(0);
                    // "After pass" should reflect the *passing attempt*, not any earlier failed attempt.
                    // The harness is allowed to iterate; we only care that a reported pass never ships
                    // parse-broken output.
                    let syntax_failure_after_pass = result.diagnostics.passed
                        && result
                            .diagnostics
                            .attempts
                            .last()
                            .map(|attempt| {
                                attempt
                                    .gates
                                    .iter()
                                    .any(|gate| gate.gate == "post_review_syntax" && !gate.passed)
                            })
                            .unwrap_or(false);
                    let mutation_on_failure = if result.diagnostics.passed {
                        false
                    } else {
                        repo_has_uncommitted_mutations(case_sandbox.path())
                    };
                    if let Err(error) = record_harness_finalization_outcome(
                        case_sandbox.path(),
                        &mut result.diagnostics,
                        ImplementationFinalizationStatus::FailedBeforeFinalize,
                        Some(
                            "Lab run validates harness output without applying branch finalization"
                                .to_string(),
                        ),
                        Some(mutation_on_failure),
                        ImplementationHarnessRunContext::Lab,
                        Some(&cosmos_repo),
                    ) {
                        finalization_telemetry_error_count =
                            finalization_telemetry_error_count.saturating_add(1);
                        repo_notes.push(format!(
                            "Failed to write harness finalization telemetry for case {}: {}",
                            idx + 1,
                            error
                        ));
                    }
                    if let Some(source_report) = result.diagnostics.report_path.clone() {
                        let repo_id_label = target
                            .repo_id
                            .clone()
                            .unwrap_or_else(|| format!("repo-{}", repo_idx + 1));
                        match persist_harness_report(
                            &cosmos_repo,
                            &run_id,
                            &repo_id_label,
                            idx + 1,
                            &suggestion.id.to_string(),
                            &source_report,
                        ) {
                            Ok(dest) => {
                                result.diagnostics.report_path = Some(dest);
                            }
                            Err(error) => {
                                repo_notes.push(format!(
                                    "Failed to persist harness report for case {}: {}",
                                    idx + 1,
                                    error
                                ));
                            }
                        }
                    }

                    if residual_blocking > 0 {
                        blocking_residual_count += 1;
                    }
                    if syntax_failure_after_pass {
                        syntax_failure_after_pass_count += 1;
                    }
                    if mutation_on_failure {
                        mutation_on_failure_count += 1;
                    }

                    if result.diagnostics.passed {
                        passed_count += 1;
                    }
                    if first_attempt_passed {
                        first_attempt_pass_count += 1;
                    }
                    total_ms_sum += result.diagnostics.total_ms;
                    total_cost_sum += result.diagnostics.total_cost_usd;
                    if let Some(last_attempt) = result.diagnostics.attempts.last() {
                        match last_attempt.quick_check_status {
                            cosmos_tui::suggest::llm::ImplementationQuickCheckStatus::Passed => {
                                quick_check_passed_cases += 1;
                            }
                            cosmos_tui::suggest::llm::ImplementationQuickCheckStatus::Failed => {
                                quick_check_failed_cases += 1;
                            }
                            cosmos_tui::suggest::llm::ImplementationQuickCheckStatus::Unavailable => {
                                quick_check_unavailable_cases += 1;
                            }
                        }
                    }
                    if same_model_review_requires_independent(&result.diagnostics) {
                        independent_review_required_count += 1;
                        if diagnostics_has_independent_review(&result.diagnostics) {
                            independent_review_executed_count += 1;
                        } else {
                            independent_review_miss_count += 1;
                            repo_notes.push(format!(
                                "Case {} passed with same-model review but no independent second-opinion review call was recorded",
                                idx + 1
                            ));
                        }
                    }

                    for attempt in &result.diagnostics.attempts {
                        if let Some(cmd) = attempt
                            .quick_check_command
                            .as_ref()
                            .filter(|cmd| !cmd.trim().is_empty())
                        {
                            if !quick_check_detected {
                                quick_check_detected = true;
                                quick_check_command = Some(cmd.clone());
                            }
                        }

                        for record in &attempt.fail_reason_records {
                            let key = format!("{}:{}", record.gate, record.code);
                            *reason_histogram.entry(key.clone()).or_insert(0) += 1;
                            *global_reason_histogram.entry(key).or_insert(0) += 1;
                        }
                    }

                    case_result = ImplementSuggestionResult {
                        repo_path: repo_root.clone(),
                        suggestion_id: suggestion.id.to_string(),
                        summary: suggestion.summary.clone(),
                        passed: result.diagnostics.passed,
                        attempt_count: result.diagnostics.attempt_count,
                        total_ms: result.diagnostics.total_ms,
                        total_cost_usd: result.diagnostics.total_cost_usd,
                        file_changes: result.file_changes.len(),
                        first_attempt_passed,
                        fail_reasons: result.diagnostics.fail_reasons.clone(),
                        report_path: result.diagnostics.report_path.clone(),
                        residual_blocking,
                        syntax_failure_after_pass,
                        mutation_on_failure,
                    };
                }
                Err(error) => {
                    case_result.fail_reasons = vec![error.to_string()];
                }
            }

            results.push(case_result);
            if let Some(last) = results.last() {
                let report_display = last
                    .report_path
                    .as_ref()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| "none".to_string());
                println!(
                    "    Result: {} (attempts={}, cost=${:.4}, ms={}, files={}, report={})",
                    if last.passed { "PASS" } else { "FAIL" },
                    last.attempt_count,
                    last.total_cost_usd,
                    last.total_ms,
                    last.file_changes,
                    report_display
                );
                if !last.passed {
                    if let Some(reason) = last.fail_reasons.first() {
                        println!("    Reason: {}", reason);
                    }
                }
            }

            if args.keep_sandboxes {
                repo_notes.push(format!(
                    "Keeping implement case sandbox {} at {}",
                    idx + 1,
                    case_sandbox.path().display()
                ));
            } else if let Err(error) = case_sandbox.cleanup() {
                repo_notes.push(format!(
                    "Failed to cleanup implement case sandbox: {}",
                    error
                ));
            }
        }

        if args.keep_sandboxes {
            repo_notes.push(format!(
                "Keeping implement base sandbox at {}",
                base_sandbox.path().display()
            ));
        } else if let Err(error) = base_sandbox.cleanup() {
            repo_notes.push(format!(
                "Failed to cleanup implement base sandbox: {}",
                error
            ));
        }

        let executed_count = results.len();
        let avg_total_ms = if executed_count == 0 {
            None
        } else {
            Some(total_ms_sum as f64 / executed_count as f64)
        };
        let avg_total_cost_usd = if executed_count == 0 {
            None
        } else {
            Some(total_cost_sum / executed_count as f64)
        };
        let pass_rate = if executed_count == 0 {
            None
        } else {
            Some(passed_count as f64 / executed_count as f64)
        };
        let first_attempt_pass_rate = if executed_count == 0 {
            None
        } else {
            Some(first_attempt_pass_count as f64 / executed_count as f64)
        };
        let residual_blocking_rate = if executed_count == 0 {
            None
        } else {
            Some(blocking_residual_count as f64 / executed_count as f64)
        };
        let syntax_failure_after_pass_rate = if executed_count == 0 {
            None
        } else {
            Some(syntax_failure_after_pass_count as f64 / executed_count as f64)
        };
        let mutation_on_failure_rate = if executed_count == 0 {
            None
        } else {
            Some(mutation_on_failure_count as f64 / executed_count as f64)
        };

        let mut repo_passed = if executed_count == 0 {
            repo_notes.push(
                "No validated suggestions were sampled for this repo; excluded from repo-level gate evaluation"
                    .to_string(),
            );
            true
        } else {
            if executed_count < MIN_REPO_FIRST_ATTEMPT_GATE_SAMPLE_SIZE {
                repo_notes.push(format!(
                    "First-attempt gate waived for this repo ({} executed case(s) < {})",
                    executed_count, MIN_REPO_FIRST_ATTEMPT_GATE_SAMPLE_SIZE
                ));
            }
            implement_repo_gate_passes(
                executed_count,
                pass_rate,
                first_attempt_pass_rate,
                avg_total_cost_usd,
                avg_total_ms,
                residual_blocking_rate,
                syntax_failure_after_pass_rate,
                mutation_on_failure_rate,
            )
        };
        if executed_count > 0 && !quick_check_detected {
            repo_notes.push(format!(
                "No quick-check command was detected for one or more executed cases; quick-check detectability gate failed ({})",
                if shadow_gate_blocking {
                    "blocking"
                } else {
                    "advisory shadow gate"
                }
            ));
            if shadow_gate_blocking {
                repo_passed = false;
            }
        }
        if independent_review_miss_count > 0 {
            repo_notes.push(format!(
                "Independent second-opinion review was required for {} same-model pass case(s) but missing in {} case(s) ({})",
                independent_review_required_count,
                independent_review_miss_count,
                if shadow_gate_blocking {
                    "blocking"
                } else {
                    "advisory shadow gate"
                }
            ));
            if shadow_gate_blocking {
                repo_passed = false;
            }
        }
        if finalization_telemetry_error_count > 0 {
            repo_notes.push(format!(
                "Harness telemetry/report finalization failed in {} case(s) ({})",
                finalization_telemetry_error_count,
                if args.enforce {
                    "blocking"
                } else {
                    "advisory; set --enforce to fail"
                }
            ));
            if args.enforce {
                repo_passed = false;
            }
        }

        let top_failure_clusters = top_failure_clusters(&reason_histogram, 3);

        repo_reports.push(ImplementRepoReport {
            repo_path: repo_root.clone(),
            repo_id: target.repo_id.clone(),
            requested_ref: target.requested_ref.clone(),
            head_sha: target.head_sha.clone(),
            subdir: target.subdir.clone(),
            sample_size,
            candidate_count,
            executed_count,
            passed_count,
            first_attempt_pass_count,
            avg_total_ms,
            avg_total_cost_usd,
            pass_rate,
            first_attempt_pass_rate,
            residual_blocking_rate,
            syntax_failure_after_pass_rate,
            mutation_on_failure_rate,
            quick_check_detected,
            quick_check_command,
            quick_check_passed_cases,
            quick_check_failed_cases,
            quick_check_unavailable_cases,
            independent_review_required_count,
            independent_review_executed_count,
            independent_review_miss_count,
            failure_reason_histogram: reason_histogram,
            top_failure_clusters,
            results,
            passed: repo_passed,
            notes: repo_notes,
        });
    }

    let total_candidate_count = repo_reports
        .iter()
        .map(|report| report.candidate_count)
        .sum();
    let executed_count = repo_reports
        .iter()
        .map(|report| report.executed_count)
        .sum();
    let passed_count = repo_reports.iter().map(|report| report.passed_count).sum();
    let first_attempt_pass_count = repo_reports
        .iter()
        .map(|report| report.first_attempt_pass_count)
        .sum();
    let total_ms_sum: u64 = repo_reports
        .iter()
        .map(|report| {
            report
                .results
                .iter()
                .map(|result| result.total_ms)
                .sum::<u64>()
        })
        .sum();
    let total_cost_sum: f64 = repo_reports
        .iter()
        .map(|report| {
            report
                .results
                .iter()
                .map(|result| result.total_cost_usd)
                .sum::<f64>()
        })
        .sum();
    let residual_blocking_count: usize = repo_reports
        .iter()
        .map(|report| {
            report
                .results
                .iter()
                .filter(|result| result.residual_blocking > 0)
                .count()
        })
        .sum();
    let syntax_failure_after_pass_count: usize = repo_reports
        .iter()
        .map(|report| {
            report
                .results
                .iter()
                .filter(|result| result.syntax_failure_after_pass)
                .count()
        })
        .sum();
    let mutation_on_failure_count: usize = repo_reports
        .iter()
        .map(|report| {
            report
                .results
                .iter()
                .filter(|result| result.mutation_on_failure)
                .count()
        })
        .sum();
    let independent_review_required_count: usize = repo_reports
        .iter()
        .map(|report| report.independent_review_required_count)
        .sum();
    let independent_review_executed_count: usize = repo_reports
        .iter()
        .map(|report| report.independent_review_executed_count)
        .sum();
    let independent_review_miss_count: usize = repo_reports
        .iter()
        .map(|report| report.independent_review_miss_count)
        .sum();

    let avg_total_ms = if executed_count == 0 {
        None
    } else {
        Some(total_ms_sum as f64 / executed_count as f64)
    };
    let avg_total_cost_usd = if executed_count == 0 {
        None
    } else {
        Some(total_cost_sum / executed_count as f64)
    };
    let pass_rate = if executed_count == 0 {
        None
    } else {
        Some(passed_count as f64 / executed_count as f64)
    };
    let first_attempt_pass_rate = if executed_count == 0 {
        None
    } else {
        Some(first_attempt_pass_count as f64 / executed_count as f64)
    };
    let residual_blocking_rate = if executed_count == 0 {
        None
    } else {
        Some(residual_blocking_count as f64 / executed_count as f64)
    };
    let syntax_failure_after_pass_rate = if executed_count == 0 {
        None
    } else {
        Some(syntax_failure_after_pass_count as f64 / executed_count as f64)
    };
    let mutation_on_failure_rate = if executed_count == 0 {
        None
    } else {
        Some(mutation_on_failure_count as f64 / executed_count as f64)
    };

    let gate_passed = implement_gate_passes(
        pass_rate,
        first_attempt_pass_rate,
        avg_total_cost_usd,
        avg_total_ms,
        residual_blocking_rate,
        syntax_failure_after_pass_rate,
        mutation_on_failure_rate,
    );
    let independent_review_gate_passed = independent_review_miss_count == 0;
    if !independent_review_gate_passed {
        notes.push(format!(
            "Independent second-opinion gate failed: {} same-model pass case(s) required independent review, {} were recorded",
            independent_review_required_count, independent_review_executed_count
        ));
    }

    let all_repos_passed = repo_reports.iter().all(|report| report.passed);
    let mut passed = gate_passed
        && all_repos_passed
        && (independent_review_gate_passed || !shadow_gate_blocking);

    if !all_repos_passed {
        notes.push("At least one repo-level implement gate failed".to_string());
    }
    let mut required_consecutive_corpus_passes = None;
    let mut consecutive_corpus_pass_count = None;
    if let Some(manifest_path) = corpus_manifest_path.as_ref() {
        let required = args.require_consecutive_corpus_passes;
        required_consecutive_corpus_passes = Some(required);
        let consecutive = if required == 0 {
            0
        } else {
            match compute_consecutive_corpus_pass_count(
                &cosmos_repo,
                manifest_path,
                &run_id,
                passed,
                256,
            ) {
                Ok(value) => value,
                Err(error) => {
                    notes.push(format!(
                        "Consecutive corpus-pass evaluation failed: {}",
                        error
                    ));
                    0
                }
            }
        };
        consecutive_corpus_pass_count = Some(consecutive);
        if required > 0 {
            if consecutive < required {
                notes.push(format!(
                    "Consecutive corpus-pass gate failed: {} consecutive pass(es); require {}",
                    consecutive, required
                ));
                passed = false;
            } else {
                notes.push(format!(
                    "Consecutive corpus-pass gate satisfied: {} consecutive pass(es); require {}",
                    consecutive, required
                ));
            }
        } else {
            notes.push(
                "Consecutive corpus-pass gate disabled (--require-consecutive-corpus-passes=0)"
                    .to_string(),
            );
        }
    }

    let overall_top_failure_clusters = top_failure_clusters(&global_reason_histogram, 3);

    let report = ImplementReport {
        timestamp: Utc::now(),
        run_id: run_id.clone(),
        cosmos_repo: cosmos_repo.clone(),
        primary_target_repo: primary_target_repo.clone(),
        canary_repos,
        corpus_manifest: corpus_manifest_path.clone(),
        sample_size: default_sample_size,
        total_candidate_count,
        executed_count,
        passed_count,
        first_attempt_pass_count,
        avg_total_ms,
        avg_total_cost_usd,
        pass_rate,
        first_attempt_pass_rate,
        residual_blocking_rate,
        syntax_failure_after_pass_rate,
        mutation_on_failure_rate,
        independent_review_required_count,
        independent_review_executed_count,
        independent_review_miss_count,
        required_consecutive_corpus_passes,
        consecutive_corpus_pass_count,
        failure_reason_histogram: global_reason_histogram,
        top_failure_clusters: overall_top_failure_clusters,
        repo_reports,
        passed,
        notes,
    };

    let output_path = output_path(args.output.as_ref(), &cosmos_repo, "implement", &run_id, "");
    write_report_json(&output_path, &report)?;

    println!("Run ID: {}", run_id);
    println!("Executed: {}", report.executed_count);
    println!("Passed: {}", report.passed);
    println!("Report: {}", output_path.display());
    if !report.top_failure_clusters.is_empty() {
        println!("Top failure clusters:");
        for cluster in &report.top_failure_clusters {
            println!(
                "- {}:{} ({})",
                cluster.gate, cluster.reason_code, cluster.count
            );
        }
    }
    if args.enforce && !report.passed {
        return Err(anyhow!(
            "Implementation harness quality gate failed. Report: {}",
            output_path.display()
        ));
    }
    Ok(())
}

fn repo_has_uncommitted_mutations(repo_root: &Path) -> bool {
    cosmos_tui::git_ops::current_status(repo_root)
        .map(|status| !(status.staged.is_empty() && status.modified.is_empty()))
        .unwrap_or(true)
}

fn prefix_suggestion_paths(suggestion: &mut Suggestion, prefix: &Path) {
    suggestion.file = prefix.join(&suggestion.file);
    suggestion.additional_files = suggestion
        .additional_files
        .iter()
        .map(|path| prefix.join(path))
        .collect();
    suggestion.evidence_refs = suggestion
        .evidence_refs
        .iter()
        .cloned()
        .map(|mut evidence| {
            evidence.file = prefix.join(&evidence.file);
            evidence
        })
        .collect::<Vec<SuggestionEvidenceRef>>();
}

fn top_failure_clusters(
    histogram: &HashMap<String, usize>,
    max_clusters: usize,
) -> Vec<FailureCluster> {
    let mut items = histogram
        .iter()
        .map(|(key, count)| (key.clone(), *count))
        .collect::<Vec<_>>();
    items.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));

    items
        .into_iter()
        .take(max_clusters.max(1))
        .map(|(key, count)| {
            let (gate, reason_code) = key
                .split_once(':')
                .map(|(g, c)| (g.to_string(), c.to_string()))
                .unwrap_or_else(|| (key, "unknown".to_string()));
            FailureCluster {
                gate,
                reason_code,
                count,
            }
        })
        .collect()
}

fn canonicalize_or_identity(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

fn load_recent_implement_reports(cosmos_repo: &Path, limit: usize) -> Result<Vec<ImplementReport>> {
    if limit == 0 {
        return Ok(Vec::new());
    }
    let lab_dir = cosmos_repo.join(".cosmos").join("lab");
    if !lab_dir.exists() {
        return Ok(Vec::new());
    }

    let mut reports = Vec::new();
    for entry in std::fs::read_dir(&lab_dir).with_context(|| {
        format!(
            "Failed to read implement report directory '{}'",
            lab_dir.display()
        )
    })? {
        let entry = entry?;
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if !name.starts_with("implement-") || !name.ends_with(".json") {
            continue;
        }
        let content = match std::fs::read_to_string(&path) {
            Ok(content) => content,
            Err(_) => continue,
        };
        let report = match serde_json::from_str::<ImplementReport>(&content) {
            Ok(report) => report,
            Err(_) => continue,
        };
        reports.push(report);
    }

    reports.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));
    if reports.len() > limit {
        reports.truncate(limit);
    }
    Ok(reports)
}

fn compute_consecutive_corpus_pass_count(
    cosmos_repo: &Path,
    manifest_path: &Path,
    current_run_id: &str,
    current_passed: bool,
    scan_limit: usize,
) -> Result<usize> {
    if !current_passed {
        return Ok(0);
    }
    let manifest = canonicalize_or_identity(manifest_path);
    let reports = load_recent_implement_reports(cosmos_repo, scan_limit)?;
    let mut count = 1usize; // include current run
    for report in reports {
        if report.run_id == current_run_id {
            continue;
        }
        let Some(report_manifest) = report.corpus_manifest.as_ref() else {
            continue;
        };
        if canonicalize_or_identity(report_manifest) != manifest {
            continue;
        }
        if report.passed {
            count += 1;
        } else {
            break;
        }
    }
    Ok(count)
}

fn same_model_review_requires_independent(diagnostics: &ImplementationRunDiagnostics) -> bool {
    diagnostics.passed
        && diagnostics.attempts.iter().any(|attempt| {
            attempt.llm_calls.iter().any(|call| {
                matches!(call.kind.as_str(), "review" | "review_fix" | "rereview")
                    && call.model == diagnostics.model
            })
        })
}

fn diagnostics_has_independent_review(diagnostics: &ImplementationRunDiagnostics) -> bool {
    diagnostics.attempts.iter().any(|attempt| {
        attempt
            .llm_calls
            .iter()
            .any(|call| call.kind == "independent_review")
    })
}

fn implement_shadow_gates_blocking_enabled() -> bool {
    std::env::var("COSMOS_IMPLEMENT_SHADOW_GATES_BLOCKING")
        .map(|value| {
            matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
        .unwrap_or(false)
}

fn implement_gate_passes(
    pass_rate: Option<f64>,
    first_attempt_pass_rate: Option<f64>,
    avg_total_cost_usd: Option<f64>,
    avg_total_ms: Option<f64>,
    residual_blocking_rate: Option<f64>,
    syntax_failure_after_pass_rate: Option<f64>,
    mutation_on_failure_rate: Option<f64>,
) -> bool {
    pass_rate.unwrap_or(0.0) >= 0.90
        && first_attempt_pass_rate.unwrap_or(0.0) >= 0.70
        && avg_total_cost_usd.unwrap_or(f64::MAX) <= 0.015
        && avg_total_ms.unwrap_or(f64::MAX) <= 35_000.0
        && residual_blocking_rate.unwrap_or(1.0) == 0.0
        && syntax_failure_after_pass_rate.unwrap_or(1.0) == 0.0
        && mutation_on_failure_rate.unwrap_or(1.0) == 0.0
}

const MIN_REPO_FIRST_ATTEMPT_GATE_SAMPLE_SIZE: usize = 5;

fn implement_repo_gate_passes(
    executed_count: usize,
    pass_rate: Option<f64>,
    first_attempt_pass_rate: Option<f64>,
    avg_total_cost_usd: Option<f64>,
    avg_total_ms: Option<f64>,
    residual_blocking_rate: Option<f64>,
    syntax_failure_after_pass_rate: Option<f64>,
    mutation_on_failure_rate: Option<f64>,
) -> bool {
    pass_rate.unwrap_or(0.0) >= 0.90
        && (executed_count < MIN_REPO_FIRST_ATTEMPT_GATE_SAMPLE_SIZE
            || first_attempt_pass_rate.unwrap_or(0.0) >= 0.70)
        && avg_total_cost_usd.unwrap_or(f64::MAX) <= 0.015
        && avg_total_ms.unwrap_or(f64::MAX) <= 35_000.0
        && residual_blocking_rate.unwrap_or(1.0) == 0.0
        && syntax_failure_after_pass_rate.unwrap_or(1.0) == 0.0
        && mutation_on_failure_rate.unwrap_or(1.0) == 0.0
}

fn canonical_repo_path(path: &Path, label: &str) -> Result<PathBuf> {
    let canonical = path
        .canonicalize()
        .with_context(|| format!("Failed to resolve {} '{}'", label, path.display()))?;
    if !canonical.join(".git").exists() {
        return Err(anyhow!(
            "{} '{}' is not a git repository",
            label,
            canonical.display()
        ));
    }
    Ok(canonical)
}

fn output_path(
    requested: Option<&PathBuf>,
    cosmos_repo: &Path,
    prefix: &str,
    run_id: &str,
    mode: &str,
) -> PathBuf {
    match requested {
        Some(path) if path.is_absolute() => path.clone(),
        Some(path) => cosmos_repo.join(path),
        None => {
            let timestamp = Utc::now().format("%Y%m%d-%H%M%S");
            let short = run_id.chars().take(8).collect::<String>();
            let mode_part = if mode.is_empty() {
                "".to_string()
            } else {
                format!("-{}", mode)
            };
            cosmos_repo.join(".cosmos").join("lab").join(format!(
                "{}{}-{}-{}.json",
                prefix, mode_part, timestamp, short
            ))
        }
    }
}

fn write_report_json<T: Serialize>(path: &Path, report: &T) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create report directory '{}'", parent.display()))?;
    }
    let content = serde_json::to_string_pretty(report)?;
    std::fs::write(path, content)
        .with_context(|| format!("Failed to write report '{}'", path.display()))?;
    Ok(())
}

fn sanitize_fs_component(input: &str, fallback: &str) -> String {
    let cleaned = input
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
        .collect::<String>();
    if cleaned.is_empty() {
        fallback.to_string()
    } else {
        cleaned
    }
}

fn persist_harness_report(
    cosmos_repo: &Path,
    lab_run_id: &str,
    repo_id: &str,
    case_index: usize,
    suggestion_id: &str,
    source_path: &Path,
) -> Result<PathBuf> {
    let run_dir = sanitize_fs_component(lab_run_id, "run");
    let repo_dir = sanitize_fs_component(repo_id, "repo");
    let dest_dir = cosmos_repo
        .join(".cosmos")
        .join("lab")
        .join("harness_reports")
        .join(run_dir)
        .join(repo_dir);
    std::fs::create_dir_all(&dest_dir).with_context(|| {
        format!(
            "Failed to create harness report directory '{}'",
            dest_dir.display()
        )
    })?;

    let dest_path = dest_dir.join(format!("case-{}-{}.json", case_index, suggestion_id));

    let content = std::fs::read_to_string(source_path).with_context(|| {
        format!(
            "Failed to read harness report '{}' for persistence",
            source_path.display()
        )
    })?;
    let mut json: serde_json::Value = serde_json::from_str(&content).with_context(|| {
        format!(
            "Failed to parse harness report '{}' as JSON",
            source_path.display()
        )
    })?;
    if let Some(obj) = json.as_object_mut() {
        obj.insert(
            "report_path".to_string(),
            serde_json::Value::String(dest_path.display().to_string()),
        );
    }

    let dest_content = serde_json::to_string_pretty(&json)?;
    std::fs::write(&dest_path, dest_content).with_context(|| {
        format!(
            "Failed to write persisted harness report '{}'",
            dest_path.display()
        )
    })?;
    Ok(dest_path)
}

fn lint_baseline_path(target_repo: &Path) -> PathBuf {
    target_repo
        .join(".cosmos")
        .join("lab")
        .join("lint-baseline.json")
}

fn ensure_lint_baseline(target_repo: &Path, env: &[(String, String)]) -> Result<LintCounts> {
    let path = lint_baseline_path(target_repo);
    if path.exists() {
        let content = std::fs::read_to_string(&path)
            .with_context(|| format!("Failed to read lint baseline '{}'", path.display()))?;
        let baseline: LintBaselineFile = serde_json::from_str(&content)
            .with_context(|| format!("Failed to parse lint baseline '{}'", path.display()))?;
        return Ok(baseline.counts);
    }

    let outcome = run_command(
        &CommandSpec::new("target:lint_baseline_capture", target_repo, "pnpm")
            .args(&["lint"])
            .timeout_secs(1_800)
            .with_env_overrides(env),
    );
    let counts = parse_lint_counts(&outcome).ok_or_else(|| {
        anyhow!(
            "Could not parse lint counts while creating baseline; command output was insufficient"
        )
    })?;

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let baseline = LintBaselineFile {
        captured_at: Utc::now(),
        command: outcome.command,
        counts: counts.clone(),
    };
    let content = serde_json::to_string_pretty(&baseline)?;
    std::fs::write(&path, content)
        .with_context(|| format!("Failed to write lint baseline '{}'", path.display()))?;
    Ok(counts)
}

fn parse_lint_counts(outcome: &SelfIterationCommandOutcome) -> Option<LintCounts> {
    let combined = format!("{}\n{}", outcome.stdout_tail, outcome.stderr_tail);
    let summary_re = Regex::new(r"\((\d+)\s+errors?,\s+(\d+)\s+warnings?\)").ok()?;
    if let Some(captures) = summary_re.captures_iter(&combined).last() {
        let errors = captures.get(1)?.as_str().parse::<usize>().ok()?;
        let warnings = captures.get(2)?.as_str().parse::<usize>().ok()?;
        return Some(LintCounts { errors, warnings });
    }
    None
}

fn metric_displayed_valid_ratio(metrics: &SelfIterationSuggestionMetrics) -> f64 {
    if metrics.final_count > 0 {
        ratio(metrics.validated_count, metrics.final_count)
    } else if metrics.displayed_valid_ratio > 0.0 {
        metrics.displayed_valid_ratio
    } else {
        metrics.validated_ratio
    }
}

fn metric_pending_count(metrics: &SelfIterationSuggestionMetrics) -> usize {
    if metrics.final_count > 0 || metrics.pending_count > 0 {
        metrics
            .pending_count
            .max(metrics.final_count.saturating_sub(metrics.validated_count))
    } else {
        0
    }
}

fn metric_trial_weight(metrics: &SelfIterationSuggestionMetrics) -> f64 {
    metrics.trials.max(1) as f64
}

fn metric_final_count(metrics: &SelfIterationSuggestionMetrics) -> f64 {
    let trial_weight = metric_trial_weight(metrics);
    if metrics.final_count > 0 {
        metrics.final_count as f64 / trial_weight
    } else {
        (metrics.validated_count + metric_pending_count(metrics)) as f64 / trial_weight
    }
}

fn metric_suggest_ms(metrics: &SelfIterationSuggestionMetrics) -> f64 {
    metrics.suggest_total_ms as f64 / metric_trial_weight(metrics)
}

fn metric_suggest_cost_usd(metrics: &SelfIterationSuggestionMetrics) -> f64 {
    metrics.suggest_total_cost_usd / metric_trial_weight(metrics)
}

fn ratio(numerator: usize, denominator: usize) -> f64 {
    if denominator == 0 {
        0.0
    } else {
        numerator as f64 / denominator as f64
    }
}

#[derive(Debug, Clone, Copy)]
struct GateCandidate {
    displayed_validity: f64,
    final_count: f64,
    pending_count: usize,
    suggest_ms: Option<f64>,
    suggest_cost_usd: Option<f64>,
}

fn gate_candidate_from_metrics(metrics: &SelfIterationSuggestionMetrics) -> GateCandidate {
    GateCandidate {
        displayed_validity: metric_displayed_valid_ratio(metrics),
        final_count: metric_final_count(metrics),
        pending_count: metric_pending_count(metrics),
        suggest_ms: {
            let ms = metric_suggest_ms(metrics);
            if ms == 0.0 {
                None
            } else {
                Some(ms)
            }
        },
        suggest_cost_usd: {
            let cost = metric_suggest_cost_usd(metrics);
            if cost <= 0.0 {
                None
            } else {
                Some(cost)
            }
        },
    }
}

#[allow(clippy::too_many_arguments)]
fn evaluate_quality_gate(
    cache: &Cache,
    source: GateSource,
    window: usize,
    min_displayed_validity: f64,
    min_final_count: usize,
    max_suggest_ms: u64,
    max_suggest_cost_usd: f64,
    current_mode: &str,
    current_metrics: Option<&SelfIterationSuggestionMetrics>,
) -> Result<QualityGateResult> {
    let window = window.max(1);
    let mut candidates: Vec<GateCandidate> = cache
        .load_recent_self_iteration_runs(2_000)?
        .into_iter()
        .filter(|record| source.includes_mode(&record.mode))
        .filter_map(|record| {
            record
                .reliability_metrics
                .map(|metrics| gate_candidate_from_metrics(&metrics))
        })
        .collect();

    if source.includes_mode(current_mode) {
        if let Some(metrics) = current_metrics {
            candidates.push(gate_candidate_from_metrics(metrics));
        }
    }

    if candidates.len() > window {
        let split = candidates.len() - window;
        candidates.drain(0..split);
    }

    let evaluated_runs = candidates.len();
    let rolling_displayed_validity = if evaluated_runs == 0 {
        None
    } else {
        Some(
            candidates
                .iter()
                .map(|candidate| candidate.displayed_validity)
                .sum::<f64>()
                / evaluated_runs as f64,
        )
    };
    let rolling_final_count = if evaluated_runs == 0 {
        None
    } else {
        Some(
            candidates
                .iter()
                .map(|candidate| candidate.final_count)
                .sum::<f64>()
                / evaluated_runs as f64,
        )
    };
    let rolling_suggest_ms = if evaluated_runs == 0 {
        None
    } else {
        let values = candidates
            .iter()
            .filter_map(|candidate| candidate.suggest_ms)
            .collect::<Vec<_>>();
        if values.is_empty() {
            None
        } else {
            Some(values.iter().sum::<f64>() / values.len() as f64)
        }
    };
    let rolling_suggest_cost_usd = if evaluated_runs == 0 {
        None
    } else {
        let values = candidates
            .iter()
            .filter_map(|candidate| candidate.suggest_cost_usd)
            .collect::<Vec<_>>();
        if values.is_empty() {
            None
        } else {
            Some(values.iter().sum::<f64>() / values.len() as f64)
        }
    };
    let pending_violations = candidates
        .iter()
        .filter(|candidate| candidate.pending_count > 0)
        .count();

    if evaluated_runs < window {
        return Ok(QualityGateResult {
            source: source.as_str().to_string(),
            window,
            min_displayed_validity,
            min_final_count,
            max_suggest_ms,
            max_suggest_cost_usd,
            evaluated_runs,
            rolling_displayed_validity,
            rolling_final_count,
            rolling_suggest_ms,
            rolling_suggest_cost_usd,
            pending_violations,
            warmup: true,
            passed: true,
        });
    }

    let passed = rolling_displayed_validity.unwrap_or(0.0) >= min_displayed_validity
        && rolling_final_count.unwrap_or(0.0) >= min_final_count as f64
        && rolling_suggest_ms
            .map(|value| value <= max_suggest_ms as f64)
            .unwrap_or(false)
        && rolling_suggest_cost_usd
            .map(|value| value <= max_suggest_cost_usd)
            .unwrap_or(false)
        && pending_violations == 0usize;
    Ok(QualityGateResult {
        source: source.as_str().to_string(),
        window,
        min_displayed_validity,
        min_final_count,
        max_suggest_ms,
        max_suggest_cost_usd,
        evaluated_runs,
        rolling_displayed_validity,
        rolling_final_count,
        rolling_suggest_ms,
        rolling_suggest_cost_usd,
        pending_violations,
        warmup: false,
        passed,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum JsPackageManager {
    Pnpm,
    Yarn,
    Npm,
    Bun,
}

fn program_available_on_path(program: &str) -> bool {
    let program = program.trim();
    if program.is_empty() {
        return false;
    }
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(program);
        if !candidate.is_file() {
            continue;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if let Ok(meta) = std::fs::metadata(&candidate) {
                if meta.permissions().mode() & 0o111 != 0 {
                    return true;
                }
            }
        }
        #[cfg(not(unix))]
        {
            return true;
        }
    }
    false
}

fn detect_js_package_manager(repo_root: &Path) -> Option<JsPackageManager> {
    if repo_root.join("pnpm-lock.yaml").exists() {
        return Some(JsPackageManager::Pnpm);
    }
    if repo_root.join("yarn.lock").exists() {
        return Some(JsPackageManager::Yarn);
    }
    if repo_root.join("package-lock.json").exists()
        || repo_root.join("npm-shrinkwrap.json").exists()
    {
        return Some(JsPackageManager::Npm);
    }
    if repo_root.join("bun.lockb").exists() || repo_root.join("bun.lock").exists() {
        return Some(JsPackageManager::Bun);
    }
    None
}

fn missing_required_tool_for_repo(repo_root: &Path) -> Option<String> {
    if repo_root.join("Cargo.toml").exists() && !program_available_on_path("cargo") {
        return Some("cargo".to_string());
    }
    if repo_root.join("go.mod").exists() && !program_available_on_path("go") {
        return Some("go".to_string());
    }

    let has_python = repo_root.join("pyproject.toml").exists()
        || repo_root.join("requirements.txt").exists()
        || repo_root.join("setup.py").exists()
        || repo_root.join("setup.cfg").exists();
    if has_python && !(program_available_on_path("python3") || program_available_on_path("python"))
    {
        return Some("python3/python".to_string());
    }

    if repo_root.join("package.json").exists() {
        let pm = detect_js_package_manager(repo_root);
        match pm {
            Some(JsPackageManager::Pnpm) if !program_available_on_path("pnpm") => {
                return Some("pnpm".to_string());
            }
            Some(JsPackageManager::Yarn) if !program_available_on_path("yarn") => {
                return Some("yarn".to_string());
            }
            Some(JsPackageManager::Npm) if !program_available_on_path("npm") => {
                return Some("npm".to_string());
            }
            Some(JsPackageManager::Bun) if !program_available_on_path("bun") => {
                return Some("bun".to_string());
            }
            None if !program_available_on_path("npm") => {
                // Fallback quick-check runner uses npm when no lockfile exists.
                return Some("npm".to_string());
            }
            _ => {}
        }
    }

    None
}

fn link_node_modules_from_base(
    base_sandbox: &Path,
    case_sandbox: &Path,
    notes: &mut Vec<String>,
) -> bool {
    if !case_sandbox.join("package.json").exists() {
        return false;
    }
    let base_node_modules = base_sandbox.join("node_modules");
    if !base_node_modules.exists() {
        return false;
    }
    let case_node_modules = case_sandbox.join("node_modules");
    if case_node_modules.exists() {
        return true;
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::symlink;
        match symlink(&base_node_modules, &case_node_modules) {
            Ok(()) => {
                notes.push("Linked node_modules from base sandbox".to_string());
                true
            }
            Err(err) => {
                notes.push(format!(
                    "Failed to link node_modules from base sandbox: {}",
                    err
                ));
                false
            }
        }
    }

    #[cfg(not(unix))]
    {
        let _ = base_node_modules;
        let _ = case_node_modules;
        notes.push("Skipping node_modules link on non-unix platform".to_string());
        false
    }
}

fn prepare_target_workspace(
    sandbox_repo: &Path,
    env: &[(String, String)],
    notes: &mut Vec<String>,
) -> Option<SelfIterationCommandOutcome> {
    let sandbox_node_modules = sandbox_repo.join("node_modules");
    if sandbox_node_modules.exists() {
        return None;
    }
    if !sandbox_repo.join("package.json").exists() {
        return None;
    }

    let pm = detect_js_package_manager(sandbox_repo);
    let run_install = |program: &str, args: &[&str]| -> SelfIterationCommandOutcome {
        run_command(
            &CommandSpec::new("target:install", sandbox_repo, program)
                .args(args)
                .timeout_secs(1_800)
                .with_env_overrides(env),
        )
    };

    let outcome = match pm {
        Some(JsPackageManager::Pnpm) => {
            notes.push("Installing JS dependencies via pnpm (ignore scripts)".to_string());
            run_install("pnpm", &["install", "--prefer-offline", "--ignore-scripts"])
        }
        Some(JsPackageManager::Yarn) => {
            notes.push("Installing JS dependencies via yarn (ignore scripts)".to_string());
            let first = run_install("yarn", &["install", "--immutable", "--ignore-scripts"]);
            if first.success {
                first
            } else {
                let stderr_lower =
                    format!("{}\n{}", first.stdout_tail, first.stderr_tail).to_ascii_lowercase();
                let unknown_option = stderr_lower.contains("unknown option")
                    || stderr_lower.contains("unrecognized option")
                    || stderr_lower.contains("invalid option")
                    || stderr_lower.contains("illegal option");
                if unknown_option {
                    notes.push(
                        "Yarn immutable install unsupported; falling back to --frozen-lockfile"
                            .to_string(),
                    );
                    run_install(
                        "yarn",
                        &["install", "--frozen-lockfile", "--ignore-scripts"],
                    )
                } else {
                    first
                }
            }
        }
        Some(JsPackageManager::Npm) => {
            notes.push("Installing JS dependencies via npm ci (ignore scripts)".to_string());
            run_install("npm", &["ci", "--ignore-scripts"])
        }
        Some(JsPackageManager::Bun) => {
            notes.push("Installing JS dependencies via bun (ignore scripts)".to_string());
            run_install("bun", &["install", "--ignore-scripts"])
        }
        None => {
            notes.push(
                "No JS lockfile detected; skipping dependency install (node_modules missing)"
                    .to_string(),
            );
            return None;
        }
    };

    if outcome.success {
        notes.push("Installed target sandbox dependencies".to_string());
    } else {
        notes.push(
            "Failed to install target sandbox dependencies; downstream target commands may fail"
                .to_string(),
        );
    }

    Some(outcome)
}

fn fake_reliability_enabled() -> bool {
    std::env::var("COSMOS_LAB_FAKE_RELIABILITY")
        .ok()
        .map(|value| matches!(value.as_str(), "1" | "true" | "TRUE" | "yes"))
        .unwrap_or(false)
}

fn fake_implement_enabled() -> bool {
    std::env::var("COSMOS_LAB_FAKE_IMPLEMENT")
        .ok()
        .map(|value| matches!(value.as_str(), "1" | "true" | "TRUE" | "yes"))
        .unwrap_or(false)
}

fn fake_validated_suggestions(index: &CodebaseIndex, sample_size: usize) -> Vec<Suggestion> {
    let mut file_paths = index.files.keys().cloned().collect::<Vec<_>>();
    file_paths.sort();
    if file_paths.is_empty() {
        file_paths.push(PathBuf::from("src/lib.rs"));
    }

    file_paths
        .into_iter()
        .take(sample_size.max(1))
        .enumerate()
        .map(|(idx, path)| {
            Suggestion::new(
                SuggestionKind::Quality,
                Priority::High,
                path,
                format!("Fake validated suggestion {}", idx + 1),
                SuggestionSource::LlmDeep,
            )
            .with_validation_state(SuggestionValidationState::Validated)
            .with_line(1)
        })
        .collect()
}

fn fake_trial_result(target_repo: &Path, verify_sample: usize) -> ReliabilityTrialResult {
    let preview_sampled = verify_sample.clamp(1, 4);
    let preview_verified = preview_sampled.saturating_sub(1);
    let preview_contradicted = 1.min(preview_sampled);
    let mut source_mix = std::collections::HashMap::new();
    source_mix.insert("pattern".to_string(), 12usize);
    source_mix.insert("hotspot".to_string(), 8usize);
    source_mix.insert("core".to_string(), 5usize);

    let metrics = SelfIterationSuggestionMetrics {
        trials: 1,
        provisional_count: 8,
        final_count: 8,
        validated_count: 7,
        pending_count: 0,
        rejected_count: 1,
        displayed_valid_ratio: 0.875,
        validated_ratio: 0.875,
        rejected_ratio: 0.125,
        preview_sampled,
        preview_verified_count: preview_verified,
        preview_contradicted_count: preview_contradicted,
        preview_insufficient_count: 0,
        preview_error_count: 0,
        preview_precision: Some(
            preview_verified as f64 / (preview_verified + preview_contradicted) as f64,
        ),
        evidence_line1_ratio: 0.2,
        evidence_source_mix: source_mix.clone(),
        suggest_total_tokens: 2400,
        suggest_total_cost_usd: 0.0012,
        suggest_total_ms: 3200,
    };
    let diagnostics = ReliabilityDiagnosticsSummary {
        run_id: "fake-run".to_string(),
        model: "fake".to_string(),
        provisional_count: 8,
        final_count: 8,
        validated_count: 7,
        rejected_count: 1,
        regeneration_attempts: 1,
        generation_waves: 1,
        generation_topup_calls: 0,
        generation_mapped_count: 8,
        rejected_evidence_skipped_count: 0,
        validation_rejection_histogram: HashMap::new(),
        validation_deadline_exceeded: false,
        validation_deadline_ms: 0,
        validation_transport_retry_count: 0,
        validation_transport_recovered_count: 0,
        regen_stopped_validation_budget: false,
        attempt_index: 1,
        attempt_count: 1,
        gate_passed: false,
        gate_fail_reasons: Vec::new(),
        attempt_cost_usd: 0.0012,
        attempt_ms: 3200,
        overclaim_rewrite_count: 0,
        overclaim_rewrite_validated_count: 0,
        notes: Vec::new(),
        evidence_pack_line1_ratio: 0.2,
        evidence_source_mix: source_mix,
    };
    ReliabilityTrialResult {
        target_repo: target_repo.to_path_buf(),
        metrics,
        diagnostics,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::collections::HashSet;
    use std::process::Command;
    use std::sync::Mutex;
    use tempfile::tempdir;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn gate_metric_with_limits(
        displayed_valid_ratio: f64,
        final_count: usize,
        pending_count: usize,
        suggest_ms: u64,
        suggest_cost_usd: f64,
    ) -> SelfIterationSuggestionMetrics {
        let final_count = final_count.max(1);
        let validated_count = (displayed_valid_ratio * final_count as f64).round() as usize;
        let implied_pending = final_count.saturating_sub(validated_count);
        let pending_count = pending_count.max(implied_pending);
        SelfIterationSuggestionMetrics {
            trials: 1,
            provisional_count: final_count,
            final_count,
            validated_count,
            pending_count,
            rejected_count: 0,
            displayed_valid_ratio: validated_count as f64 / final_count as f64,
            validated_ratio: validated_count as f64 / final_count as f64,
            rejected_ratio: 0.0,
            preview_sampled: 0,
            preview_verified_count: 0,
            preview_contradicted_count: 0,
            preview_insufficient_count: 0,
            preview_error_count: 0,
            preview_precision: None,
            evidence_line1_ratio: 0.2,
            evidence_source_mix: HashMap::new(),
            suggest_total_tokens: 1000,
            suggest_total_cost_usd: suggest_cost_usd,
            suggest_total_ms: suggest_ms,
        }
    }

    fn gate_metric(
        displayed_valid_ratio: f64,
        pending_count: usize,
    ) -> SelfIterationSuggestionMetrics {
        gate_metric_with_limits(displayed_valid_ratio, 100, pending_count, 1_000, 0.001)
    }

    #[test]
    fn gate_candidate_normalizes_multi_trial_reliability_metrics() {
        let metrics = SelfIterationSuggestionMetrics {
            trials: 5,
            final_count: 60,
            validated_count: 60,
            pending_count: 0,
            displayed_valid_ratio: 1.0,
            suggest_total_ms: 100_000,
            suggest_total_cost_usd: 0.08,
            ..SelfIterationSuggestionMetrics::default()
        };

        let candidate = gate_candidate_from_metrics(&metrics);
        assert!((candidate.final_count - 12.0).abs() < f64::EPSILON);
        assert_eq!(candidate.suggest_ms, Some(20_000.0));
        assert!((candidate.suggest_cost_usd.unwrap_or_default() - 0.016).abs() < f64::EPSILON);
    }

    fn append_run(
        cache: &Cache,
        mode: &str,
        run_id: &str,
        metrics: SelfIterationSuggestionMetrics,
    ) {
        cache
            .append_self_iteration_run(&SelfIterationRunRecord {
                timestamp: Utc::now(),
                run_id: run_id.to_string(),
                mode: mode.to_string(),
                cosmos_repo: PathBuf::from("."),
                target_repo: PathBuf::from("."),
                passed: true,
                command_outcomes: Vec::new(),
                reliability_metrics: Some(metrics),
                report_path: None,
                notes: Vec::new(),
            })
            .unwrap();
    }

    #[test]
    fn parse_lint_counts_reads_eslint_summary() {
        let outcome = SelfIterationCommandOutcome {
            name: "lint".to_string(),
            command: "pnpm lint".to_string(),
            cwd: PathBuf::from("."),
            duration_ms: 1,
            success: false,
            exit_code: Some(1),
            timed_out: false,
            stdout_tail: "✖ 193 problems (22 errors, 171 warnings)".to_string(),
            stderr_tail: String::new(),
            note: None,
        };

        let parsed = parse_lint_counts(&outcome).unwrap();
        assert_eq!(parsed.errors, 22);
        assert_eq!(parsed.warnings, 171);
    }

    #[test]
    fn output_path_defaults_to_cosmos_lab_dir() {
        let root = PathBuf::from("/tmp/cosmos");
        let output = output_path(None, &root, "validate", "abcd1234efgh", "fast");
        assert!(output
            .to_string_lossy()
            .contains("/tmp/cosmos/.cosmos/lab/"));
        assert!(output.to_string_lossy().contains("validate-fast-"));
    }

    #[test]
    fn validate_cli_defaults_use_balanced_gate_profile() {
        let cli = Cli::parse_from(["cosmos-lab", "validate"]);
        match cli.command {
            Commands::Validate(args) => {
                assert_eq!(args.gate_min_displayed_validity, 0.95);
                assert_eq!(args.gate_min_final_count, 10);
                assert_eq!(args.gate_max_suggest_ms, 26_000);
                assert!((args.gate_max_suggest_cost_usd - 0.016).abs() < f64::EPSILON);
            }
            _ => panic!("expected validate command defaults"),
        }
    }

    #[test]
    fn reliability_cli_defaults_use_balanced_gate_profile() {
        let cli = Cli::parse_from(["cosmos-lab", "reliability"]);
        match cli.command {
            Commands::Reliability(args) => {
                assert_eq!(args.gate_min_displayed_validity, 0.95);
                assert_eq!(args.gate_min_final_count, 10);
                assert_eq!(args.gate_max_suggest_ms, 26_000);
                assert!((args.gate_max_suggest_cost_usd - 0.016).abs() < f64::EPSILON);
            }
            _ => panic!("expected reliability command defaults"),
        }
    }

    #[test]
    fn implement_cli_defaults_use_strict_profile() {
        let cli = Cli::parse_from(["cosmos-lab", "implement"]);
        match cli.command {
            Commands::Implement(args) => {
                assert_eq!(args.cosmos_repo, PathBuf::from("."));
                assert_eq!(args.sample_size, 5);
                assert!(!args.keep_sandboxes);
                assert_eq!(args.canary_repos.len(), 2);
                assert!(args.corpus_manifest.is_none());
                assert!(args.corpus_root.is_none());
                assert!(args.sync);
                assert!(args.max_repos.is_none());
                assert_eq!(args.require_consecutive_corpus_passes, 2);
            }
            _ => panic!("expected implement command defaults"),
        }
    }

    #[test]
    fn implement_gate_requires_elite_thresholds() {
        assert!(implement_gate_passes(
            Some(0.91),
            Some(0.75),
            Some(0.01),
            Some(30_000.0),
            Some(0.0),
            Some(0.0),
            Some(0.0)
        ));
        assert!(!implement_gate_passes(
            Some(0.89),
            Some(0.75),
            Some(0.01),
            Some(30_000.0),
            Some(0.0),
            Some(0.0),
            Some(0.0)
        ));
        assert!(!implement_gate_passes(
            Some(0.95),
            Some(0.75),
            Some(0.01),
            Some(30_000.0),
            Some(0.0),
            Some(0.1),
            Some(0.0)
        ));
    }

    #[test]
    fn repo_gate_waives_first_attempt_threshold_on_tiny_samples() {
        assert!(implement_repo_gate_passes(
            2,
            Some(1.0),
            Some(0.5),
            Some(0.01),
            Some(30_000.0),
            Some(0.0),
            Some(0.0),
            Some(0.0)
        ));
        assert!(!implement_repo_gate_passes(
            2,
            Some(0.5),
            Some(0.5),
            Some(0.01),
            Some(30_000.0),
            Some(0.0),
            Some(0.0),
            Some(0.0)
        ));
        assert!(!implement_repo_gate_passes(
            MIN_REPO_FIRST_ATTEMPT_GATE_SAMPLE_SIZE,
            Some(1.0),
            Some(0.5),
            Some(0.01),
            Some(30_000.0),
            Some(0.0),
            Some(0.0),
            Some(0.0)
        ));
    }

    #[test]
    fn js_package_manager_detection_prefers_lockfiles_in_expected_order() {
        let root = tempdir().unwrap();
        std::fs::write(root.path().join("pnpm-lock.yaml"), "").unwrap();
        std::fs::write(root.path().join("yarn.lock"), "").unwrap();
        std::fs::write(root.path().join("package-lock.json"), "").unwrap();
        std::fs::write(root.path().join("bun.lockb"), "").unwrap();

        assert_eq!(
            detect_js_package_manager(root.path()),
            Some(JsPackageManager::Pnpm)
        );

        std::fs::remove_file(root.path().join("pnpm-lock.yaml")).unwrap();
        assert_eq!(
            detect_js_package_manager(root.path()),
            Some(JsPackageManager::Yarn)
        );

        std::fs::remove_file(root.path().join("yarn.lock")).unwrap();
        assert_eq!(
            detect_js_package_manager(root.path()),
            Some(JsPackageManager::Npm)
        );

        std::fs::remove_file(root.path().join("package-lock.json")).unwrap();
        assert_eq!(
            detect_js_package_manager(root.path()),
            Some(JsPackageManager::Bun)
        );
    }

    #[test]
    fn telemetry_record_compatibility_shape() {
        let metrics = SelfIterationSuggestionMetrics {
            trials: 1,
            provisional_count: 8,
            final_count: 8,
            validated_count: 7,
            pending_count: 0,
            rejected_count: 1,
            displayed_valid_ratio: 0.875,
            validated_ratio: 0.875,
            rejected_ratio: 0.125,
            preview_sampled: 4,
            preview_verified_count: 3,
            preview_contradicted_count: 1,
            preview_insufficient_count: 0,
            preview_error_count: 0,
            preview_precision: Some(0.75),
            evidence_line1_ratio: 0.2,
            evidence_source_mix: HashMap::from([
                ("pattern".to_string(), 12usize),
                ("hotspot".to_string(), 8usize),
                ("core".to_string(), 5usize),
            ]),
            suggest_total_tokens: 2300,
            suggest_total_cost_usd: 0.0011,
            suggest_total_ms: 3000,
        };
        let record = SelfIterationRunRecord {
            timestamp: Utc::now(),
            run_id: "run-id".to_string(),
            mode: "validate_fast".to_string(),
            cosmos_repo: PathBuf::from("."),
            target_repo: PathBuf::from("."),
            passed: true,
            command_outcomes: Vec::new(),
            reliability_metrics: Some(metrics),
            report_path: None,
            notes: Vec::new(),
        };
        let json = serde_json::to_string(&record).unwrap();
        assert!(json.contains("\"mode\":\"validate_fast\""));
        assert!(json.contains("\"passed\":true"));
    }

    fn minimal_implement_report(
        run_id: &str,
        timestamp: DateTime<Utc>,
        passed: bool,
        corpus_manifest: Option<PathBuf>,
    ) -> ImplementReport {
        ImplementReport {
            timestamp,
            run_id: run_id.to_string(),
            cosmos_repo: PathBuf::from("/tmp/cosmos"),
            primary_target_repo: PathBuf::from("/tmp/target"),
            canary_repos: Vec::new(),
            corpus_manifest,
            sample_size: 1,
            total_candidate_count: 0,
            executed_count: 0,
            passed_count: 0,
            first_attempt_pass_count: 0,
            avg_total_ms: None,
            avg_total_cost_usd: None,
            pass_rate: None,
            first_attempt_pass_rate: None,
            residual_blocking_rate: None,
            syntax_failure_after_pass_rate: None,
            mutation_on_failure_rate: None,
            independent_review_required_count: 0,
            independent_review_executed_count: 0,
            independent_review_miss_count: 0,
            required_consecutive_corpus_passes: None,
            consecutive_corpus_pass_count: None,
            failure_reason_histogram: HashMap::new(),
            top_failure_clusters: Vec::new(),
            repo_reports: Vec::new(),
            passed,
            notes: Vec::new(),
        }
    }

    #[test]
    fn consecutive_corpus_pass_count_stops_on_first_failure() {
        let workspace = tempdir().unwrap();
        init_git_repo(workspace.path());
        let manifest = workspace.path().join("corpus.toml");
        std::fs::write(&manifest, "repo = []\n").unwrap();
        let lab_dir = workspace.path().join(".cosmos").join("lab");
        std::fs::create_dir_all(&lab_dir).unwrap();

        let manifest_other = workspace.path().join("other-corpus.toml");
        std::fs::write(&manifest_other, "repo = []\n").unwrap();

        let now = Utc::now();
        let older_fail = minimal_implement_report(
            "run-fail",
            now - chrono::Duration::minutes(3),
            false,
            Some(manifest.clone()),
        );
        let newer_pass = minimal_implement_report(
            "run-pass",
            now - chrono::Duration::minutes(2),
            true,
            Some(manifest.clone()),
        );
        let unrelated_manifest_pass = minimal_implement_report(
            "run-other",
            now - chrono::Duration::minutes(1),
            true,
            Some(manifest_other),
        );

        write_report_json(&lab_dir.join("implement-a.json"), &older_fail).unwrap();
        write_report_json(&lab_dir.join("implement-b.json"), &newer_pass).unwrap();
        write_report_json(&lab_dir.join("implement-c.json"), &unrelated_manifest_pass).unwrap();

        let count = compute_consecutive_corpus_pass_count(
            workspace.path(),
            &manifest,
            "current-run",
            true,
            32,
        )
        .unwrap();
        // Current run + latest matching pass, then stop at next matching failure.
        assert_eq!(count, 2);

        let failing_now = compute_consecutive_corpus_pass_count(
            workspace.path(),
            &manifest,
            "current-run",
            false,
            32,
        )
        .unwrap();
        assert_eq!(failing_now, 0);
    }

    fn test_run_diagnostics(
        model: &str,
        passed: bool,
        llm_calls: Vec<(&str, &str)>,
    ) -> ImplementationRunDiagnostics {
        let llm_calls = llm_calls
            .into_iter()
            .map(|(kind, call_model)| {
                cosmos_tui::suggest::llm::implementation::ImplementationLlmCallRecord {
                    kind: kind.to_string(),
                    independence_role: None,
                    model: call_model.to_string(),
                    timeout_ms: 1_000,
                    speed_failover: None,
                    error: None,
                }
            })
            .collect::<Vec<_>>();
        ImplementationRunDiagnostics {
            run_id: "run-1".to_string(),
            suggestion_id: "s-1".to_string(),
            suggestion_summary: "summary".to_string(),
            model: model.to_string(),
            strict_mode: true,
            passed,
            attempt_count: 1,
            total_ms: 1_000,
            total_cost_usd: 0.001,
            reduced_confidence: false,
            fail_reasons: Vec::new(),
            attempts: vec![cosmos_tui::suggest::llm::ImplementationAttemptDiagnostics {
                attempt_index: 1,
                passed,
                fail_reasons: Vec::new(),
                fail_reason_records: Vec::new(),
                gates: Vec::new(),
                changed_files: Vec::new(),
                changed_lines_total: 0,
                changed_lines_by_file: HashMap::new(),
                quick_check_status:
                    cosmos_tui::suggest::llm::ImplementationQuickCheckStatus::Passed,
                quick_check_command: Some("fake check".to_string()),
                quick_check_outcome: None,
                quick_check_outcomes: Vec::new(),
                quick_check_fix_loops: 0,
                quick_check_failure_summary: None,
                review_iterations: 1,
                review_blocking_remaining: 0,
                remaining_blocking_titles: Vec::new(),
                remaining_blocking_categories: Vec::new(),
                attempt_ms: 1_000,
                attempt_cost_usd: 0.001,
                llm_calls,
                notes: Vec::new(),
            }],
            report_path: None,
            finalization: cosmos_tui::suggest::llm::ImplementationFinalizationDiagnostics::default(
            ),
        }
    }

    #[test]
    fn same_model_review_pass_requires_independent_review() {
        let diag = test_run_diagnostics(
            "openai/gpt-oss-120b",
            true,
            vec![("review", "openai/gpt-oss-120b")],
        );
        assert!(same_model_review_requires_independent(&diag));
        assert!(!diagnostics_has_independent_review(&diag));

        let with_second_opinion = test_run_diagnostics(
            "openai/gpt-oss-120b",
            true,
            vec![
                ("review", "openai/gpt-oss-120b"),
                ("independent_review", "openai/gpt-oss-20b"),
            ],
        );
        assert!(same_model_review_requires_independent(&with_second_opinion));
        assert!(diagnostics_has_independent_review(&with_second_opinion));

        let cross_model_review = test_run_diagnostics(
            "openai/gpt-oss-120b",
            true,
            vec![("review", "openai/gpt-oss-20b")],
        );
        assert!(!same_model_review_requires_independent(&cross_model_review));
        assert!(!diagnostics_has_independent_review(&cross_model_review));
    }

    #[test]
    fn implement_shadow_gate_env_defaults_to_advisory() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::remove_var("COSMOS_IMPLEMENT_SHADOW_GATES_BLOCKING");
        assert!(!implement_shadow_gates_blocking_enabled());

        std::env::set_var("COSMOS_IMPLEMENT_SHADOW_GATES_BLOCKING", "true");
        assert!(implement_shadow_gates_blocking_enabled());

        std::env::set_var("COSMOS_IMPLEMENT_SHADOW_GATES_BLOCKING", "0");
        assert!(!implement_shadow_gates_blocking_enabled());

        std::env::remove_var("COSMOS_IMPLEMENT_SHADOW_GATES_BLOCKING");
    }

    #[test]
    fn validate_report_deserializes_without_failure_kind() {
        let report = ValidateReport {
            timestamp: Utc::now(),
            run_id: "run-1".to_string(),
            mode: "fast".to_string(),
            cosmos_repo: PathBuf::from("/tmp/cosmos"),
            target_repo: PathBuf::from("/tmp/target"),
            cosmos_sandbox: PathBuf::from("/tmp/sandbox/cosmos"),
            target_sandbox: PathBuf::from("/tmp/sandbox/target-repo"),
            command_outcomes: Vec::new(),
            lint_baseline: None,
            lint_result: None,
            lint_error_delta: None,
            reliability_metrics: None,
            reliability_diagnostics: None,
            reliability_failure_kind: Some("IndexEmpty".to_string()),
            quality_gate: None,
            passed: false,
            notes: Vec::new(),
        };
        let mut json = serde_json::to_value(&report).unwrap();
        json.as_object_mut()
            .unwrap()
            .remove("reliability_failure_kind");

        let parsed: ValidateReport = serde_json::from_value(json).unwrap();
        assert_eq!(parsed.reliability_failure_kind, None);
    }

    #[test]
    fn reliability_report_deserializes_without_failure_kind() {
        let report = ReliabilityReport {
            timestamp: Utc::now(),
            run_id: "run-2".to_string(),
            cosmos_repo: PathBuf::from("/tmp/cosmos"),
            target_repo: PathBuf::from("/tmp/target"),
            target_sandbox: PathBuf::from("/tmp/sandbox/target-repo"),
            trial_count: 3,
            verify_sample: 4,
            aggregated_metrics: None,
            trial_results: Vec::new(),
            reliability_failure_kind: Some("LlmUnavailable".to_string()),
            quality_gate: None,
            passed: false,
            notes: Vec::new(),
        };
        let mut json = serde_json::to_value(&report).unwrap();
        json.as_object_mut()
            .unwrap()
            .remove("reliability_failure_kind");

        let parsed: ReliabilityReport = serde_json::from_value(json).unwrap();
        assert_eq!(parsed.reliability_failure_kind, None);
    }

    #[test]
    fn report_serializes_failure_kind_with_and_without_value() {
        let with_kind = ValidateReport {
            timestamp: Utc::now(),
            run_id: "run-3".to_string(),
            mode: "fast".to_string(),
            cosmos_repo: PathBuf::from("/tmp/cosmos"),
            target_repo: PathBuf::from("/tmp/target"),
            cosmos_sandbox: PathBuf::from("/tmp/sandbox/cosmos"),
            target_sandbox: PathBuf::from("/tmp/sandbox/target-repo"),
            command_outcomes: Vec::new(),
            lint_baseline: None,
            lint_result: None,
            lint_error_delta: None,
            reliability_metrics: None,
            reliability_diagnostics: None,
            reliability_failure_kind: Some("IndexEmpty".to_string()),
            quality_gate: None,
            passed: false,
            notes: Vec::new(),
        };
        let with_json = serde_json::to_value(&with_kind).unwrap();
        assert_eq!(with_json["reliability_failure_kind"], "IndexEmpty");

        let without_kind = ValidateReport {
            reliability_failure_kind: None,
            ..with_kind
        };
        let without_json = serde_json::to_value(&without_kind).unwrap();
        assert!(without_json["reliability_failure_kind"].is_null());
    }

    #[test]
    fn quality_gate_returns_warmup_when_window_not_filled() {
        let workspace = tempdir().unwrap();
        init_git_repo(workspace.path());
        let cache = Cache::new(workspace.path());

        append_run(&cache, "validate_fast", "r1", gate_metric(0.95, 0));
        append_run(&cache, "reliability", "r2", gate_metric(0.96, 0));

        let gate = evaluate_quality_gate(
            &cache,
            GateSource::Both,
            5,
            0.95,
            10,
            30_000,
            0.01,
            "validate_fast",
            Some(&gate_metric(1.0, 0)),
        )
        .unwrap();
        assert!(gate.warmup);
        assert!(gate.passed);
        assert_eq!(gate.evaluated_runs, 3);
    }

    #[test]
    fn quality_gate_fails_when_pending_suggestions_exist() {
        let workspace = tempdir().unwrap();
        init_git_repo(workspace.path());
        let cache = Cache::new(workspace.path());

        for i in 0..9 {
            append_run(
                &cache,
                "validate_fast",
                &format!("run-{}", i),
                gate_metric(1.0, 0),
            );
        }

        let gate = evaluate_quality_gate(
            &cache,
            GateSource::Both,
            10,
            0.95,
            10,
            30_000,
            0.01,
            "validate_fast",
            Some(&gate_metric(1.0, 1)),
        )
        .unwrap();
        assert!(!gate.warmup);
        assert!(!gate.passed);
        assert_eq!(gate.pending_violations, 1);
    }

    #[test]
    fn quality_gate_fails_when_rolling_final_count_is_below_threshold() {
        let workspace = tempdir().unwrap();
        init_git_repo(workspace.path());
        let cache = Cache::new(workspace.path());

        for i in 0..10 {
            append_run(
                &cache,
                "validate_fast",
                &format!("run-{}", i),
                gate_metric_with_limits(1.0, 8, 0, 1_000, 0.001),
            );
        }

        let gate = evaluate_quality_gate(
            &cache,
            GateSource::Both,
            10,
            0.95,
            10,
            30_000,
            0.01,
            "validate_fast",
            None,
        )
        .unwrap();
        assert!(!gate.warmup);
        assert!(!gate.passed);
        assert!(gate.rolling_final_count.unwrap_or(0.0) < 10.0);
    }

    #[test]
    fn quality_gate_fails_when_rolling_suggest_ms_exceeds_threshold() {
        let workspace = tempdir().unwrap();
        init_git_repo(workspace.path());
        let cache = Cache::new(workspace.path());

        for i in 0..10 {
            append_run(
                &cache,
                "validate_fast",
                &format!("run-{}", i),
                gate_metric_with_limits(1.0, 12, 0, 45_000, 0.001),
            );
        }

        let gate = evaluate_quality_gate(
            &cache,
            GateSource::Both,
            10,
            0.95,
            10,
            30_000,
            0.01,
            "validate_fast",
            None,
        )
        .unwrap();
        assert!(!gate.warmup);
        assert!(!gate.passed);
        assert!(gate.rolling_suggest_ms.unwrap_or(0.0) > 30_000.0);
    }

    #[test]
    fn quality_gate_fails_when_rolling_suggest_cost_exceeds_threshold() {
        let workspace = tempdir().unwrap();
        init_git_repo(workspace.path());
        let cache = Cache::new(workspace.path());

        for i in 0..10 {
            append_run(
                &cache,
                "validate_fast",
                &format!("run-{}", i),
                gate_metric_with_limits(1.0, 12, 0, 1_000, 0.02),
            );
        }

        let gate = evaluate_quality_gate(
            &cache,
            GateSource::Both,
            10,
            0.95,
            10,
            30_000,
            0.01,
            "validate_fast",
            None,
        )
        .unwrap();
        assert!(!gate.warmup);
        assert!(!gate.passed);
        assert!(gate.rolling_suggest_cost_usd.unwrap_or(0.0) > 0.01);
    }

    #[test]
    fn quality_gate_filters_by_source_mode() {
        let workspace = tempdir().unwrap();
        init_git_repo(workspace.path());
        let cache = Cache::new(workspace.path());

        append_run(&cache, "validate_fast", "v1", gate_metric(1.0, 0));
        append_run(&cache, "validate_full", "v2", gate_metric(1.0, 0));
        append_run(&cache, "reliability", "r1", gate_metric(1.0, 0));
        append_run(&cache, "reliability", "r2", gate_metric(1.0, 0));

        let gate = evaluate_quality_gate(
            &cache,
            GateSource::Reliability,
            2,
            0.95,
            10,
            30_000,
            0.01,
            "validate_fast",
            Some(&gate_metric(1.0, 0)),
        )
        .unwrap();
        assert!(!gate.warmup);
        assert!(gate.passed);
        assert_eq!(gate.evaluated_runs, 2);
        assert_eq!(gate.source, "reliability");
    }

    #[test]
    fn validate_fast_smoke_writes_report_with_commands_and_metrics() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var("COSMOS_LAB_FAKE_RELIABILITY", "1");

        let workspace = tempdir().unwrap();
        let cosmos_repo = workspace.path().join("cosmos-mini");
        let target_repo = workspace.path().join("target-mini");
        std::fs::create_dir_all(cosmos_repo.join("src")).unwrap();
        std::fs::create_dir_all(&target_repo).unwrap();

        std::fs::write(
            cosmos_repo.join("Cargo.toml"),
            r#"[package]
name = "cosmos-mini"
version = "0.1.0"
edition = "2021"
"#,
        )
        .unwrap();
        std::fs::write(
            cosmos_repo.join("src/lib.rs"),
            "pub fn ok() -> bool { true }\n",
        )
        .unwrap();

        std::fs::write(
            target_repo.join("package.json"),
            r#"{
  "name": "target-mini",
  "private": true,
  "scripts": {
    "test:once": "node -e \"console.log('test-once')\"",
    "type-check": "node -e \"console.log('type-check')\""
  }
}
"#,
        )
        .unwrap();

        init_git_repo(&cosmos_repo);
        init_git_repo(&target_repo);

        let output = workspace.path().join("validate-report.json");
        let args = ValidateArgs {
            cosmos_repo: cosmos_repo.clone(),
            target_repo: target_repo.clone(),
            mode: ValidateMode::Fast,
            verify_sample: 3,
            output: Some(output.clone()),
            keep_sandboxes: false,
            enforce_quality_gate: false,
            gate_window: 10,
            gate_min_displayed_validity: 0.95,
            gate_min_final_count: 10,
            gate_max_suggest_ms: 26_000,
            gate_max_suggest_cost_usd: 0.016,
            gate_source: GateSource::Both,
        };

        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async { run_validate(args).await }).unwrap();

        let content = std::fs::read_to_string(&output).unwrap();
        let json: serde_json::Value = serde_json::from_str(&content).unwrap();
        let command_count = json["command_outcomes"].as_array().unwrap().len();
        assert!(command_count >= 3);
        assert!(json["reliability_metrics"].is_object());
        assert!(json["passed"].is_boolean());
        let target_sandbox = json["target_sandbox"].as_str().unwrap();
        assert!(target_sandbox.ends_with("target-repo"));

        std::env::remove_var("COSMOS_LAB_FAKE_RELIABILITY");
    }

    #[test]
    fn implement_smoke_writes_multi_repo_report_in_fake_mode() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var("COSMOS_LAB_FAKE_IMPLEMENT", "1");

        let workspace = tempdir().unwrap();
        let cosmos_repo = workspace.path().join("cosmos-mini");
        let primary_repo = workspace.path().join("target-primary");
        let canary_one = workspace.path().join("target-canary-one");
        let canary_two = workspace.path().join("target-canary-two");
        std::fs::create_dir_all(cosmos_repo.join("src")).unwrap();
        std::fs::write(
            cosmos_repo.join("src/lib.rs"),
            "pub fn ok() -> bool { true }\n",
        )
        .unwrap();
        init_git_repo(&cosmos_repo);

        for repo in [&primary_repo, &canary_one, &canary_two] {
            std::fs::create_dir_all(repo.join("src")).unwrap();
            std::fs::write(repo.join("src/lib.rs"), "pub fn ok() -> bool { true }\n").unwrap();
            init_git_repo(repo);
        }

        let output = workspace.path().join("implement-report.json");
        let args = ImplementArgs {
            cosmos_repo: cosmos_repo.clone(),
            target_repo: primary_repo.clone(),
            canary_repos: vec![canary_one.clone(), canary_two.clone()],
            sample_size: 2,
            output: Some(output.clone()),
            keep_sandboxes: false,
            enforce: false,
            corpus_manifest: None,
            corpus_root: None,
            sync: true,
            max_repos: None,
            review_model: ImplementReviewModelArg::Smart,
            require_consecutive_corpus_passes: 2,
        };

        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async { run_implement(args).await }).unwrap();

        let content = std::fs::read_to_string(&output).unwrap();
        let json: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert!(json["repo_reports"].is_array());
        assert_eq!(json["repo_reports"].as_array().unwrap().len(), 3);
        assert!(json["pass_rate"].is_number());
        assert!(json["mutation_on_failure_rate"].is_number());
        assert!(json["independent_review_required_count"].is_number());
        assert!(json["independent_review_executed_count"].is_number());
        assert!(json["independent_review_miss_count"].is_number());
        assert_eq!(json["independent_review_miss_count"], 0);
        let notes = json["notes"].as_array().cloned().unwrap_or_default();
        let note_set = notes
            .iter()
            .filter_map(|note| note.as_str())
            .collect::<HashSet<_>>();
        assert!(note_set.contains("Shadow gate mode: advisory"));

        for repo in json["repo_reports"].as_array().unwrap() {
            assert!(repo["quick_check_detected"].as_bool().unwrap_or(false));
            assert!(repo["quick_check_passed_cases"].is_number());
            assert!(repo["quick_check_failed_cases"].is_number());
            assert!(repo["quick_check_unavailable_cases"].is_number());
            assert!(repo["independent_review_required_count"].is_number());
            assert!(repo["independent_review_executed_count"].is_number());
            assert!(repo["independent_review_miss_count"].is_number());
            let executed = repo["executed_count"].as_u64().unwrap_or_default();
            let quick_check_total = repo["quick_check_passed_cases"]
                .as_u64()
                .unwrap_or_default()
                + repo["quick_check_failed_cases"]
                    .as_u64()
                    .unwrap_or_default()
                + repo["quick_check_unavailable_cases"]
                    .as_u64()
                    .unwrap_or_default();
            assert_eq!(quick_check_total, executed);
        }
        assert!(json["passed"].as_bool().unwrap_or(false));
        let cache = Cache::new(&cosmos_repo);
        let telemetry = cache.load_recent_implementation_harness(32).unwrap();
        assert_eq!(
            telemetry.len(),
            json["executed_count"].as_u64().unwrap_or_default() as usize
        );
        for row in telemetry {
            assert_eq!(row.schema_version, 3);
            assert_eq!(row.run_context, "lab");
            assert!(!row.independent_review_executed);
            assert_eq!(row.finalization_status, "failed_before_finalize");
            assert_eq!(row.mutation_on_failure, Some(false));
        }

        std::env::remove_var("COSMOS_LAB_FAKE_IMPLEMENT");
    }

    fn init_git_repo(path: &Path) {
        let init = Command::new("git")
            .current_dir(path)
            .args(["init"])
            .output()
            .unwrap();
        assert!(init.status.success(), "git init failed");

        let cfg_name = Command::new("git")
            .current_dir(path)
            .args(["config", "user.name", "Cosmos Test"])
            .output()
            .unwrap();
        assert!(cfg_name.status.success(), "git config user.name failed");

        let cfg_email = Command::new("git")
            .current_dir(path)
            .args(["config", "user.email", "cosmos@test.local"])
            .output()
            .unwrap();
        assert!(cfg_email.status.success(), "git config user.email failed");

        let add = Command::new("git")
            .current_dir(path)
            .args(["add", "."])
            .output()
            .unwrap();
        assert!(add.status.success(), "git add failed");

        let commit = Command::new("git")
            .current_dir(path)
            .args(["commit", "--allow-empty", "-m", "init"])
            .output()
            .unwrap();
        assert!(commit.status.success(), "git commit failed");
    }
}
