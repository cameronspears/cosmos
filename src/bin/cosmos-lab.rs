use anyhow::{anyhow, Context, Result};
use chrono::{DateTime, Utc};
use clap::{Args, Parser, Subcommand, ValueEnum};
use cosmos_tui::cache::{
    Cache, SelfIterationCommandOutcome, SelfIterationRunRecord, SelfIterationSuggestionMetrics,
};
use cosmos_tui::lab::reliability::{
    classify_reliability_error, run_trial, run_trials, ReliabilityDiagnosticsSummary,
    ReliabilityTrialResult,
};
use cosmos_tui::lab::runner::{run_command, CommandSpec};
use cosmos_tui::lab::sandbox::SandboxSession;
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use uuid::Uuid;

const DEFAULT_TARGET_REPO: &str = "/Users/cam/WebstormProjects/gielinor-gains";

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
    passed: bool,
    notes: Vec<String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Commands::Validate(args) => run_validate(args).await,
        Commands::Reliability(args) => run_reliability(args).await,
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

    let cache = Cache::new(&cosmos_repo);
    let telemetry = SelfIterationRunRecord {
        timestamp: Utc::now(),
        run_id: run_id.clone(),
        mode: format!("validate_{}", args.mode.as_str()),
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

    let cache = Cache::new(&cosmos_repo);
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

fn prepare_target_workspace(
    sandbox_repo: &Path,
    env: &[(String, String)],
    notes: &mut Vec<String>,
) -> Option<SelfIterationCommandOutcome> {
    let sandbox_node_modules = sandbox_repo.join("node_modules");
    if sandbox_node_modules.exists() {
        return None;
    }

    let outcome = run_command(
        &CommandSpec::new("target:install", sandbox_repo, "pnpm")
            .args(&["install", "--prefer-offline"])
            .timeout_secs(1_800)
            .with_env_overrides(env),
    );
    if outcome.success {
        notes.push("Installed target sandbox dependencies via pnpm install".to_string());
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

fn fake_trial_result(target_repo: &Path, verify_sample: usize) -> ReliabilityTrialResult {
    let preview_sampled = verify_sample.max(1).min(4);
    let preview_verified = preview_sampled.saturating_sub(1);
    let preview_contradicted = 1.min(preview_sampled);
    let mut source_mix = std::collections::HashMap::new();
    source_mix.insert("pattern".to_string(), 12usize);
    source_mix.insert("hotspot".to_string(), 8usize);
    source_mix.insert("core".to_string(), 5usize);

    let metrics = SelfIterationSuggestionMetrics {
        trials: 1,
        provisional_count: 8,
        validated_count: 7,
        rejected_count: 1,
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
    };
    let diagnostics = ReliabilityDiagnosticsSummary {
        run_id: "fake-run".to_string(),
        model: "fake".to_string(),
        provisional_count: 8,
        final_count: 8,
        validated_count: 7,
        rejected_count: 1,
        regeneration_attempts: 1,
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
    use std::process::Command;
    use std::sync::Mutex;
    use tempfile::tempdir;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

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
    fn telemetry_record_compatibility_shape() {
        let metrics = SelfIterationSuggestionMetrics {
            trials: 1,
            provisional_count: 8,
            validated_count: 7,
            rejected_count: 1,
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
    fn validate_fast_smoke_writes_report_with_commands_and_metrics() {
        let _guard = ENV_LOCK.lock().unwrap();
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
            .args(["commit", "-m", "init"])
            .output()
            .unwrap();
        assert!(commit.status.success(), "git commit failed");
    }
}
