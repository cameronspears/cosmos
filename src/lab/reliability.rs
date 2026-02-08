use crate::cache::SelfIterationSuggestionMetrics;
use crate::context::WorkContext;
use crate::index::CodebaseIndex;
use crate::suggest::llm::{
    generate_fix_preview_agentic, run_fast_grounded_with_gate, SuggestionQualityGateConfig,
};
use crate::suggest::{Suggestion, SuggestionValidationState, VerificationState};
use anyhow::anyhow;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReliabilityDiagnosticsSummary {
    pub run_id: String,
    pub model: String,
    pub provisional_count: usize,
    pub final_count: usize,
    pub validated_count: usize,
    pub rejected_count: usize,
    pub regeneration_attempts: usize,
    #[serde(default)]
    pub generation_waves: usize,
    #[serde(default)]
    pub generation_topup_calls: usize,
    #[serde(default)]
    pub generation_mapped_count: usize,
    #[serde(default)]
    pub rejected_evidence_skipped_count: usize,
    #[serde(default)]
    pub validation_rejection_histogram: HashMap<String, usize>,
    #[serde(default)]
    pub validation_deadline_exceeded: bool,
    #[serde(default)]
    pub validation_deadline_ms: u64,
    #[serde(default)]
    pub validation_transport_retry_count: usize,
    #[serde(default)]
    pub validation_transport_recovered_count: usize,
    #[serde(default)]
    pub regen_stopped_validation_budget: bool,
    #[serde(default)]
    pub attempt_index: usize,
    #[serde(default)]
    pub attempt_count: usize,
    #[serde(default)]
    pub gate_passed: bool,
    #[serde(default)]
    pub gate_fail_reasons: Vec<String>,
    #[serde(default)]
    pub attempt_cost_usd: f64,
    #[serde(default)]
    pub attempt_ms: u64,
    #[serde(default)]
    pub overclaim_rewrite_count: usize,
    #[serde(default)]
    pub overclaim_rewrite_validated_count: usize,
    #[serde(default)]
    pub notes: Vec<String>,
    pub evidence_pack_line1_ratio: f64,
    pub evidence_source_mix: HashMap<String, usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReliabilityTrialResult {
    pub target_repo: PathBuf,
    pub metrics: SelfIterationSuggestionMetrics,
    pub diagnostics: ReliabilityDiagnosticsSummary,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReliabilityRunResult {
    pub target_repo: PathBuf,
    pub trial_count: usize,
    pub aggregated: SelfIterationSuggestionMetrics,
    pub trials: Vec<ReliabilityTrialResult>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReliabilityFailureKind {
    IndexEmpty,
    InsufficientEvidencePack,
    LlmUnavailable,
    Other,
}

impl ReliabilityFailureKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            ReliabilityFailureKind::IndexEmpty => "IndexEmpty",
            ReliabilityFailureKind::InsufficientEvidencePack => "InsufficientEvidencePack",
            ReliabilityFailureKind::LlmUnavailable => "LlmUnavailable",
            ReliabilityFailureKind::Other => "Other",
        }
    }
}

pub fn classify_reliability_error(error: &anyhow::Error) -> ReliabilityFailureKind {
    classify_reliability_error_message(&error.to_string())
}

fn classify_reliability_error_message(message: &str) -> ReliabilityFailureKind {
    let lower = message.to_ascii_lowercase();

    if lower.contains("codebase index is empty")
        || lower.contains("index is empty")
        || lower.contains("file_count == 0")
        || lower.contains("file_count=0")
    {
        return ReliabilityFailureKind::IndexEmpty;
    }

    if lower.contains("not enough grounded evidence items found")
        || lower.contains("insufficient evidence pack")
    {
        return ReliabilityFailureKind::InsufficientEvidencePack;
    }

    if lower.contains("no api key configured")
        || lower.contains("invalid api key")
        || lower.contains("openrouter")
        || lower.contains("rate limited")
        || lower.contains("rate limit")
        || lower.contains("timed out")
        || lower.contains("could not connect")
        || lower.contains("network")
        || lower.contains("authentication")
        || lower.contains("service may be temporarily unavailable")
        || lower.contains("api returned empty response")
    {
        return ReliabilityFailureKind::LlmUnavailable;
    }

    ReliabilityFailureKind::Other
}

pub async fn run_trial(
    repo_root: &Path,
    verify_sample: usize,
) -> anyhow::Result<ReliabilityTrialResult> {
    let suggest_start = std::time::Instant::now();
    let repo_root = repo_root
        .canonicalize()
        .map_err(|e| anyhow::anyhow!("Failed to resolve repo '{}': {}", repo_root.display(), e))?;
    let index = CodebaseIndex::new(&repo_root)?;
    let stats = index.stats();
    if stats.file_count == 0 {
        let root_name = repo_root
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("<unknown>");
        return Err(anyhow!(
            "Reliability preflight failed: codebase index is empty for '{}'. Root directory name '{}' may have matched an ignore rule (for example 'target'), or the repository has no supported source files.",
            repo_root.display(),
            root_name
        ));
    }
    let context = WorkContext::load(&repo_root)?;
    let cache = crate::cache::Cache::new(&repo_root);

    let summaries = cache.load_llm_summaries_cache().map(|summaries_cache| {
        summaries_cache
            .summaries
            .into_iter()
            .map(|(path, entry)| (path, entry.summary))
            .collect::<HashMap<PathBuf, String>>()
    });

    let gated = run_fast_grounded_with_gate(
        &repo_root,
        &index,
        &context,
        None,
        summaries.as_ref(),
        SuggestionQualityGateConfig::default(),
    )
    .await?;
    let refined = gated.suggestions;
    let diagnostics = gated.diagnostics;
    let gate = gated.gate;
    let total_usage = gated.usage;
    let suggest_total_ms = suggest_start.elapsed().as_millis() as u64;

    let validated_suggestions: Vec<Suggestion> = refined
        .iter()
        .filter(|s| s.validation_state == SuggestionValidationState::Validated)
        .cloned()
        .collect();
    let preview_sample_count = verify_sample.min(validated_suggestions.len());

    let mut preview_verified_count = 0usize;
    let mut preview_contradicted_count = 0usize;
    let mut preview_insufficient_count = 0usize;
    let mut preview_error_count = 0usize;
    for suggestion in validated_suggestions.iter().take(preview_sample_count) {
        match generate_fix_preview_agentic(&repo_root, suggestion, None, None).await {
            Ok((preview, _usage)) => match preview.verification_state {
                VerificationState::Verified => preview_verified_count += 1,
                VerificationState::Contradicted => preview_contradicted_count += 1,
                VerificationState::InsufficientEvidence => preview_insufficient_count += 1,
                VerificationState::Unverified => preview_error_count += 1,
            },
            Err(_) => {
                preview_error_count += 1;
            }
        }
    }

    let validated_count = diagnostics.validated_count.max(validated_suggestions.len());
    let rejected_count = diagnostics.rejected_count;
    let provisional_count = diagnostics
        .provisional_count
        .max(validated_count + rejected_count);
    let final_count = diagnostics
        .final_count
        .max(refined.len())
        .max(validated_count);
    let pending_count = final_count.saturating_sub(validated_count);
    let displayed_valid_ratio = ratio(validated_count, final_count);
    let validated_ratio = ratio(validated_count, provisional_count);
    let rejected_ratio = ratio(rejected_count, provisional_count);

    let preview_precision_denominator = preview_verified_count + preview_contradicted_count;
    let preview_precision = if preview_precision_denominator == 0 {
        None
    } else {
        Some(preview_verified_count as f64 / preview_precision_denominator as f64)
    };

    let mut source_mix = HashMap::new();
    source_mix.insert("pattern".to_string(), diagnostics.pack_pattern_count);
    source_mix.insert("hotspot".to_string(), diagnostics.pack_hotspot_count);
    source_mix.insert("core".to_string(), diagnostics.pack_core_count);

    let metrics = SelfIterationSuggestionMetrics {
        trials: 1,
        provisional_count,
        final_count,
        validated_count,
        pending_count,
        rejected_count,
        displayed_valid_ratio,
        validated_ratio,
        rejected_ratio,
        preview_sampled: preview_sample_count,
        preview_verified_count,
        preview_contradicted_count,
        preview_insufficient_count,
        preview_error_count,
        preview_precision,
        evidence_line1_ratio: diagnostics.pack_line1_ratio,
        evidence_source_mix: source_mix.clone(),
        suggest_total_tokens: total_usage.as_ref().map(|u| u.total_tokens).unwrap_or(0),
        suggest_total_cost_usd: total_usage.as_ref().map(|u| u.cost()).unwrap_or(0.0),
        suggest_total_ms,
    };

    let diagnostics_summary = ReliabilityDiagnosticsSummary {
        run_id: diagnostics.run_id,
        model: diagnostics.model,
        provisional_count: diagnostics.provisional_count,
        final_count: diagnostics.final_count,
        validated_count: diagnostics.validated_count,
        rejected_count: diagnostics.rejected_count,
        regeneration_attempts: diagnostics.regeneration_attempts,
        generation_waves: diagnostics.generation_waves,
        generation_topup_calls: diagnostics.generation_topup_calls,
        generation_mapped_count: diagnostics.generation_mapped_count,
        rejected_evidence_skipped_count: diagnostics.rejected_evidence_skipped_count,
        validation_rejection_histogram: diagnostics.validation_rejection_histogram.clone(),
        validation_deadline_exceeded: diagnostics.validation_deadline_exceeded,
        validation_deadline_ms: diagnostics.validation_deadline_ms,
        validation_transport_retry_count: diagnostics.validation_transport_retry_count,
        validation_transport_recovered_count: diagnostics.validation_transport_recovered_count,
        regen_stopped_validation_budget: diagnostics.regen_stopped_validation_budget,
        attempt_index: diagnostics.attempt_index,
        attempt_count: diagnostics.attempt_count,
        gate_passed: gate.passed,
        gate_fail_reasons: gate.fail_reasons.clone(),
        attempt_cost_usd: diagnostics.attempt_cost_usd,
        attempt_ms: diagnostics.attempt_ms,
        overclaim_rewrite_count: diagnostics.overclaim_rewrite_count,
        overclaim_rewrite_validated_count: diagnostics.overclaim_rewrite_validated_count,
        notes: diagnostics.notes.clone(),
        evidence_pack_line1_ratio: diagnostics.pack_line1_ratio,
        evidence_source_mix: source_mix,
    };

    Ok(ReliabilityTrialResult {
        target_repo: repo_root,
        metrics,
        diagnostics: diagnostics_summary,
    })
}

pub async fn run_trials(
    repo_root: &Path,
    trial_count: usize,
    verify_sample: usize,
) -> anyhow::Result<ReliabilityRunResult> {
    let trial_count = trial_count.max(1);
    let mut trials = Vec::with_capacity(trial_count);
    for _ in 0..trial_count {
        let trial = run_trial(repo_root, verify_sample).await?;
        trials.push(trial);
    }

    let aggregated = aggregate_trial_metrics(
        &trials
            .iter()
            .map(|trial| trial.metrics.clone())
            .collect::<Vec<_>>(),
    );

    Ok(ReliabilityRunResult {
        target_repo: repo_root.to_path_buf(),
        trial_count,
        aggregated,
        trials,
    })
}

pub fn aggregate_trial_metrics(
    metrics: &[SelfIterationSuggestionMetrics],
) -> SelfIterationSuggestionMetrics {
    if metrics.is_empty() {
        return SelfIterationSuggestionMetrics::default();
    }

    let mut aggregated = SelfIterationSuggestionMetrics::default();
    aggregated.trials = metrics.len();

    let mut weighted_line1_numerator = 0.0f64;
    let mut weighted_line1_denominator = 0usize;
    let mut mix_totals: HashMap<String, usize> = HashMap::new();

    for metric in metrics {
        aggregated.provisional_count += metric.provisional_count;
        aggregated.final_count += metric.final_count;
        aggregated.validated_count += metric.validated_count;
        aggregated.pending_count += metric.pending_count;
        aggregated.rejected_count += metric.rejected_count;
        aggregated.preview_sampled += metric.preview_sampled;
        aggregated.preview_verified_count += metric.preview_verified_count;
        aggregated.preview_contradicted_count += metric.preview_contradicted_count;
        aggregated.preview_insufficient_count += metric.preview_insufficient_count;
        aggregated.preview_error_count += metric.preview_error_count;
        aggregated.suggest_total_tokens += metric.suggest_total_tokens;
        aggregated.suggest_total_cost_usd += metric.suggest_total_cost_usd;
        aggregated.suggest_total_ms += metric.suggest_total_ms;

        let weight = metric.provisional_count.max(1);
        weighted_line1_numerator += metric.evidence_line1_ratio * weight as f64;
        weighted_line1_denominator += weight;

        for (source, count) in &metric.evidence_source_mix {
            *mix_totals.entry(source.clone()).or_insert(0) += *count;
        }
    }

    aggregated.displayed_valid_ratio = ratio(aggregated.validated_count, aggregated.final_count);
    aggregated.validated_ratio = ratio(aggregated.validated_count, aggregated.provisional_count);
    aggregated.rejected_ratio = ratio(aggregated.rejected_count, aggregated.provisional_count);

    let precision_denom = aggregated.preview_verified_count + aggregated.preview_contradicted_count;
    aggregated.preview_precision = if precision_denom == 0 {
        None
    } else {
        Some(aggregated.preview_verified_count as f64 / precision_denom as f64)
    };

    if weighted_line1_denominator > 0 {
        aggregated.evidence_line1_ratio =
            weighted_line1_numerator / weighted_line1_denominator as f64;
    }
    aggregated.evidence_source_mix = mix_totals;

    aggregated
}

fn ratio(numerator: usize, denominator: usize) -> f64 {
    if denominator == 0 {
        0.0
    } else {
        numerator as f64 / denominator as f64
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn aggregate_metrics_computes_validated_and_rejected_ratios() {
        let metrics = vec![
            SelfIterationSuggestionMetrics {
                trials: 1,
                provisional_count: 10,
                final_count: 8,
                validated_count: 7,
                pending_count: 1,
                rejected_count: 3,
                displayed_valid_ratio: 0.875,
                preview_sampled: 5,
                preview_verified_count: 4,
                preview_contradicted_count: 1,
                preview_insufficient_count: 0,
                preview_error_count: 0,
                preview_precision: Some(0.8),
                evidence_line1_ratio: 0.2,
                evidence_source_mix: HashMap::from([
                    ("pattern".to_string(), 10usize),
                    ("hotspot".to_string(), 8usize),
                    ("core".to_string(), 7usize),
                ]),
                suggest_total_tokens: 2000,
                suggest_total_cost_usd: 0.002,
                suggest_total_ms: 1200,
                validated_ratio: 0.7,
                rejected_ratio: 0.3,
            },
            SelfIterationSuggestionMetrics {
                trials: 1,
                provisional_count: 5,
                final_count: 4,
                validated_count: 4,
                pending_count: 0,
                rejected_count: 1,
                displayed_valid_ratio: 1.0,
                preview_sampled: 4,
                preview_verified_count: 2,
                preview_contradicted_count: 2,
                preview_insufficient_count: 0,
                preview_error_count: 0,
                preview_precision: Some(0.5),
                evidence_line1_ratio: 0.4,
                evidence_source_mix: HashMap::from([
                    ("pattern".to_string(), 5usize),
                    ("hotspot".to_string(), 4usize),
                    ("core".to_string(), 3usize),
                ]),
                suggest_total_tokens: 1500,
                suggest_total_cost_usd: 0.0015,
                suggest_total_ms: 900,
                validated_ratio: 0.8,
                rejected_ratio: 0.2,
            },
        ];

        let aggregate = aggregate_trial_metrics(&metrics);
        assert_eq!(aggregate.trials, 2);
        assert_eq!(aggregate.provisional_count, 15);
        assert_eq!(aggregate.final_count, 12);
        assert_eq!(aggregate.validated_count, 11);
        assert_eq!(aggregate.pending_count, 1);
        assert_eq!(aggregate.rejected_count, 4);
        assert!((aggregate.displayed_valid_ratio - (11.0 / 12.0)).abs() < f64::EPSILON);
        assert!((aggregate.validated_ratio - (11.0 / 15.0)).abs() < f64::EPSILON);
        assert!((aggregate.rejected_ratio - (4.0 / 15.0)).abs() < f64::EPSILON);
        assert_eq!(aggregate.suggest_total_tokens, 3500);
        assert!((aggregate.suggest_total_cost_usd - 0.0035).abs() < f64::EPSILON);
        assert_eq!(aggregate.suggest_total_ms, 2100);
    }

    #[test]
    fn aggregate_metrics_computes_preview_precision() {
        let metrics = vec![
            SelfIterationSuggestionMetrics {
                preview_verified_count: 3,
                preview_contradicted_count: 1,
                ..SelfIterationSuggestionMetrics::default()
            },
            SelfIterationSuggestionMetrics {
                preview_verified_count: 1,
                preview_contradicted_count: 2,
                ..SelfIterationSuggestionMetrics::default()
            },
        ];

        let aggregate = aggregate_trial_metrics(&metrics);
        assert_eq!(aggregate.preview_verified_count, 4);
        assert_eq!(aggregate.preview_contradicted_count, 3);
        assert!(aggregate.preview_precision.is_some());
        assert!((aggregate.preview_precision.unwrap() - (4.0 / 7.0)).abs() < 0.000001);
    }

    #[test]
    fn aggregate_metrics_empty_input_returns_default() {
        let aggregate = aggregate_trial_metrics(&[]);
        assert_eq!(aggregate.trials, 0);
        assert_eq!(aggregate.provisional_count, 0);
        assert_eq!(aggregate.validated_count, 0);
        assert_eq!(aggregate.rejected_count, 0);
        assert_eq!(aggregate.preview_precision, None);
    }

    #[test]
    fn classify_failure_kind_index_empty() {
        let error = anyhow::anyhow!(
            "Reliability preflight failed: codebase index is empty for '/tmp/target'. Root directory name 'target' may have matched an ignore rule."
        );
        assert_eq!(
            classify_reliability_error(&error),
            ReliabilityFailureKind::IndexEmpty
        );
    }

    #[test]
    fn classify_failure_kind_insufficient_evidence_pack() {
        let error = anyhow::anyhow!(
            "Not enough grounded evidence items found to generate suggestions. Try again after indexing completes."
        );
        assert_eq!(
            classify_reliability_error(&error),
            ReliabilityFailureKind::InsufficientEvidencePack
        );
    }

    #[test]
    fn classify_failure_kind_llm_unavailable() {
        let error = anyhow::anyhow!("No API key configured. Run 'cosmos --setup' to get started.");
        assert_eq!(
            classify_reliability_error(&error),
            ReliabilityFailureKind::LlmUnavailable
        );
    }

    #[test]
    fn classify_failure_kind_other() {
        let error = anyhow::anyhow!("Unexpected failure while reading cache telemetry");
        assert_eq!(
            classify_reliability_error(&error),
            ReliabilityFailureKind::Other
        );
    }

    #[test]
    fn diagnostics_summary_deserializes_without_optimization_fields() {
        let row = json!({
            "run_id": "r1",
            "model": "openai/gpt-oss-120b",
            "provisional_count": 14,
            "final_count": 12,
            "validated_count": 12,
            "rejected_count": 2,
            "regeneration_attempts": 1,
            "evidence_pack_line1_ratio": 0.2,
            "evidence_source_mix": {
                "pattern": 8,
                "hotspot": 2,
                "core": 2
            }
        });

        let parsed: ReliabilityDiagnosticsSummary = serde_json::from_value(row).unwrap();
        assert_eq!(parsed.generation_waves, 0);
        assert_eq!(parsed.generation_topup_calls, 0);
        assert_eq!(parsed.generation_mapped_count, 0);
        assert_eq!(parsed.rejected_evidence_skipped_count, 0);
        assert!(parsed.validation_rejection_histogram.is_empty());
        assert!(!parsed.validation_deadline_exceeded);
        assert_eq!(parsed.validation_deadline_ms, 0);
        assert_eq!(parsed.validation_transport_retry_count, 0);
        assert_eq!(parsed.validation_transport_recovered_count, 0);
        assert!(!parsed.regen_stopped_validation_budget);
        assert!(parsed.notes.is_empty());
    }
}
