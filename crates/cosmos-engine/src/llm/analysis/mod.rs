use super::agentic::{
    call_llm_agentic, call_llm_agentic_report_back_only, schema_to_response_format,
    AgenticStreamEvent, AgenticStreamSink, AgenticTrace,
};
use super::client::{call_llm_with_usage, truncate_str};
use super::models::merge_usage;
use super::models::{Model, Usage};
use super::prompt_utils::format_repo_memory_section;
use super::prompts::ask_question_system;
use cosmos_core::context::WorkContext;
use cosmos_core::index::{CodebaseIndex, SymbolKind};
use cosmos_core::suggest::{
    Criticality, Suggestion, SuggestionCategory, SuggestionEvidenceRef, SuggestionKind,
    SuggestionValidationMetadata, SuggestionValidationState, VerificationState,
};
use futures::future::join_all;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use uuid::Uuid;

mod context_limits;
mod summary_normalization;

use context_limits::AdaptiveLimits;
use summary_normalization::{
    normalize_ethos_summary, normalize_grounded_detail, normalize_grounded_summary,
};

// ═══════════════════════════════════════════════════════════════════════════
//  THRESHOLDS AND CONSTANTS
// ═══════════════════════════════════════════════════════════════════════════

const EVIDENCE_TOP_WINDOW_COMMENT_RATIO_MAX: f64 = 0.80;
const EVIDENCE_EXECUTABLE_RATIO_MIN: f64 = 0.20;
const FAST_GROUNDED_PROVISIONAL_TARGET_MAX: usize = 24;
const AGENTIC_SUGGESTION_TARGET_MIN: usize = 4;
const AGENTIC_SUGGESTION_TARGET_MAX: usize = 16;
const AGENTIC_SUGGESTIONS_MAX_ITERATIONS_MIN: usize = 3;
const AGENTIC_SUGGESTIONS_MAX_ITERATIONS_MAX: usize = 6;
const AGENTIC_SUBAGENT_MIN: usize = 2;
const AGENTIC_SUBAGENT_MAX: usize = 6;
const AGENTIC_SUBAGENT_FILES_PER_AGENT: usize = 2;
const AGENTIC_SUBAGENT_MIN_COMMIT_WINDOW: usize = 80;
const AGENTIC_SUBAGENT_MAX_COMMIT_WINDOW: usize = 300;
const SUMMARY_MIN_WORDS: usize = 5;
const SUMMARY_MIN_CHARS: usize = 24;
const DEFAULT_MIN_IMPLEMENTATION_READINESS_SCORE: f32 = 0.30;
const DEFAULT_MAX_SMART_REWRITES_PER_RUN: usize = 8;
const ASK_ETHOS_MAX_CHARS: usize = 2_500;
const REVIEW_AGENT_ETHOS_MAX_CHARS: usize = 800;
const REVIEW_AGENT_MEMORY_MAX_CHARS: usize = 600;
const REVIEW_AGENT_RETRY_FEEDBACK_MAX_CHARS: usize = 500;
const DEFAULT_REVIEW_AGENT_TIMEOUT_MS: u64 = 120_000;
const DEFAULT_REVIEW_AGENT_MAX_ITERATIONS: usize = 8;
const MAX_SUGGESTION_ATTEMPTS_HARD_CAP: usize = 3;
const DETERMINISTIC_SUGGESTION_SOFT_TARGET_MIN: usize = 4;
const DETERMINISTIC_SUGGESTION_SOFT_TARGET_MAX: usize = 6;
const DETERMINISTIC_SUGGESTION_PER_FILE_MAX: usize = 2;

const RELACE_BUG_HUNTER_SYSTEM: &str = r#"You are bug_hunter.

Mission:
- Find verified runtime defects only.
- Explore the repository freely and follow evidence wherever it leads.
- Never guess; every finding must include exact `evidence_quote` copied from code.

Completion:
- Call `report_back` exactly once when done.
- If no verified defects remain, use `findings: []` and `files: []`.
- Follow the `report_back` tool schema exactly."#;

const RELACE_SECURITY_REVIEWER_SYSTEM: &str = r#"You are security_reviewer.

Mission:
- Find verified security vulnerabilities only.
- Explore the repository freely and follow evidence wherever it leads.
- Never guess; every finding must include exact `evidence_quote` copied from code.

Completion:
- Call `report_back` exactly once when done.
- If no verified issues remain, use `findings: []` and `files: []`.
- Follow the `report_back` tool schema exactly."#;

const AGENTIC_SUGGESTIONS_SYSTEM: &str = r#"You are Cosmos, a senior reviewer.

Goal:
- Return only VERIFIED bug/security findings.
- Use tools to inspect code directly.
- Skip uncertain claims.
- Keep language plain and user-facing in `summary`.
- Keep `detail` actionable (root cause + fix direction).

Return JSON only:
{
  "suggestions": [{
    "file": "repo/relative/path.ext",
    "line": 123,
    "kind": "bugfix|security",
    "priority": "high|medium|low",
    "confidence": "high|medium",
    "observed_behavior": "Concrete runtime behavior observed in code.",
    "impact_class": "correctness|reliability|security|data_integrity",
    "summary": "One plain-English sentence about visible product impact.",
    "detail": "Concise root cause + actionable change direction.",
    "evidence_quote": "Exact code text proving the claim."
  }]
}

Rules:
- No refactors, style nits, docs, or speculative risks.
- `evidence_quote` must be exact code text you inspected."#;

fn clamp_agentic_target(target: usize) -> usize {
    target.clamp(AGENTIC_SUGGESTION_TARGET_MIN, AGENTIC_SUGGESTION_TARGET_MAX)
}

fn agentic_iterations_for_target(target: usize) -> usize {
    let clamped = clamp_agentic_target(target);
    let extra = clamped.saturating_sub(AGENTIC_SUGGESTION_TARGET_MIN) / 5;
    (AGENTIC_SUGGESTIONS_MAX_ITERATIONS_MIN + extra).clamp(
        AGENTIC_SUGGESTIONS_MAX_ITERATIONS_MIN,
        AGENTIC_SUGGESTIONS_MAX_ITERATIONS_MAX,
    )
}

fn subagent_count_for_target(target: usize) -> usize {
    let clamped = clamp_agentic_target(target);
    clamped
        .saturating_add(3)
        .checked_div(4)
        .unwrap_or(AGENTIC_SUBAGENT_MIN)
        .clamp(AGENTIC_SUBAGENT_MIN, AGENTIC_SUBAGENT_MAX)
}

fn churn_commit_window_for_target(target: usize) -> usize {
    let clamped = clamp_agentic_target(target);
    let scaled = AGENTIC_SUBAGENT_MIN_COMMIT_WINDOW + (clamped * 10);
    scaled.clamp(
        AGENTIC_SUBAGENT_MIN_COMMIT_WINDOW,
        AGENTIC_SUBAGENT_MAX_COMMIT_WINDOW,
    )
}

fn normalize_churn_path(raw: &str) -> Option<PathBuf> {
    let trimmed = raw.trim().trim_start_matches("./").replace('\\', "/");
    if trimmed.is_empty() {
        return None;
    }
    Some(PathBuf::from(trimmed))
}

fn churn_counts_from_git(repo_root: &Path, commit_window: usize) -> HashMap<PathBuf, usize> {
    let window = commit_window.max(1);
    let output = Command::new("git")
        .current_dir(repo_root)
        .args([
            "log",
            "--format=",
            "--name-only",
            "--diff-filter=AMRT",
            "--no-merges",
            "-n",
            &window.to_string(),
        ])
        .output();
    let Ok(output) = output else {
        return HashMap::new();
    };
    if !output.status.success() {
        return HashMap::new();
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut counts = HashMap::new();
    for line in stdout.lines() {
        let Some(path) = normalize_churn_path(line) else {
            continue;
        };
        *counts.entry(path).or_insert(0) += 1;
    }
    counts
}

fn rank_top_churn_files_for_subagents(
    repo_root: &Path,
    index: &CodebaseIndex,
    context: &WorkContext,
    generation_target: usize,
    max_files: usize,
) -> Vec<PathBuf> {
    if max_files == 0 {
        return Vec::new();
    }

    let commit_window = churn_commit_window_for_target(generation_target);
    let churn_counts = churn_counts_from_git(repo_root, commit_window);
    let changed: HashSet<PathBuf> = context.all_changed_files().into_iter().cloned().collect();
    let mut ranked = index
        .files
        .iter()
        .filter(|(path, _)| !is_test_like_path(path))
        .map(|(path, file)| {
            let churn = churn_counts.get(path).copied().unwrap_or(0);
            let changed_boost = if changed.contains(path) { 24 } else { 0 };
            let complexity_score = if file.complexity.is_finite() && file.complexity > 0.0 {
                ((file.complexity / 10.0).round() as usize).min(20)
            } else {
                0
            };
            let loc_score = (file.loc / 250).min(6);
            let score = churn
                .saturating_mul(8)
                .saturating_add(changed_boost)
                .saturating_add(complexity_score)
                .saturating_add(loc_score)
                .max(1);
            (score, file.complexity, file.loc, path.clone())
        })
        .collect::<Vec<_>>();

    ranked.sort_by(|a, b| {
        b.0.cmp(&a.0)
            .then_with(|| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal))
            .then_with(|| b.2.cmp(&a.2))
            .then_with(|| a.3.cmp(&b.3))
    });

    ranked
        .iter()
        .take(max_files)
        .map(|(_, _, _, path)| path.clone())
        .collect::<Vec<_>>()
}

fn shard_subagent_focus_files(files: &[PathBuf], subagent_count: usize) -> Vec<Vec<PathBuf>> {
    if subagent_count == 0 {
        return Vec::new();
    }
    if files.is_empty() {
        return vec![Vec::new(); subagent_count];
    }

    let mut shards = vec![Vec::new(); subagent_count];
    for (idx, file) in files.iter().enumerate() {
        shards[idx % subagent_count].push(file.clone());
    }
    for idx in 0..subagent_count {
        if shards[idx].is_empty() {
            shards[idx].push(files[idx % files.len()].clone());
        }
    }
    shards
}

fn build_subagent_user_prompt(
    subagent_index: usize,
    subagent_count: usize,
    target_for_subagent: usize,
    focus_files: &[PathBuf],
    project_ethos: Option<&str>,
    retry_feedback: Option<&str>,
) -> String {
    let mut prompt = format!(
        "You are subagent {}/{}.\nFocus assigned files first.\nTarget about {}-{} VERIFIED findings.\n\
Return fewer if evidence is weak.\n\
Each finding must be bug/security only and include exact `evidence_quote` text from inspected code.",
        subagent_index + 1,
        subagent_count,
        target_for_subagent.saturating_sub(1).max(1),
        target_for_subagent.saturating_add(1).max(2),
    );

    if !focus_files.is_empty() {
        prompt.push_str("\n\nASSIGNED FILES (focus here first):");
        for path in focus_files {
            prompt.push_str("\n- ");
            prompt.push_str(&path.display().to_string());
        }
    }

    prompt.push_str(
        "\n\nQUALITY BAR:\n\
- Include only runtime defects or security vulnerabilities.\n\
- Exclude style/refactor/docs-only advice.\n\
- `summary`: one plain-language impact sentence.\n\
- `detail`: root cause and fix direction.\n\
- If uncertain, omit the claim.",
    );

    if let Some(ethos) = project_ethos.map(str::trim).filter(|text| !text.is_empty()) {
        prompt.push_str("\n\nPROJECT ETHOS (must follow):\n");
        prompt.push_str(truncate_str(ethos, REVIEW_AGENT_ETHOS_MAX_CHARS));
    }

    if let Some(feedback) = retry_feedback
        .map(str::trim)
        .filter(|text| !text.is_empty())
    {
        prompt.push_str("\n\nPrevious attempt feedback to correct:\n");
        prompt.push_str(truncate_str(
            feedback,
            REVIEW_AGENT_RETRY_FEEDBACK_MAX_CHARS,
        ));
    }

    prompt
}

/// Ask cosmos a general question about the codebase
/// Uses the Smart model for thoughtful, well-reasoned responses in plain English
pub async fn ask_question(
    index: &CodebaseIndex,
    context: &WorkContext,
    question: &str,
    repo_memory: Option<String>,
) -> anyhow::Result<(String, Option<Usage>)> {
    // Build context about the codebase
    let stats = index.stats();
    let limits =
        AdaptiveLimits::for_codebase_and_question(stats.file_count, stats.total_loc, question);

    let query_terms = tokenize_question_terms(question);
    let changed_paths = collect_changed_paths(context);
    let changed_roots = collect_changed_roots(&changed_paths);
    let focus_terms = context
        .inferred_focus
        .as_deref()
        .map(tokenize_question_terms)
        .unwrap_or_default();

    let file_list = rank_files_for_question(
        index,
        &query_terms,
        &focus_terms,
        &changed_paths,
        &changed_roots,
        limits.file_list_limit,
    );

    // Get symbols for context (used internally, not exposed to user).
    let symbols = rank_symbols_for_question(
        index,
        &query_terms,
        &focus_terms,
        &changed_paths,
        &changed_roots,
        limits.symbol_limit,
    );

    let memory_section = format_repo_memory_section(repo_memory.as_deref(), "PROJECT NOTES");
    let project_ethos = load_project_ethos(&context.repo_root);
    let system = ask_question_system(project_ethos.as_deref());

    let user = format!(
        r#"PROJECT CONTEXT:
- files: {}
- lines: {}
- symbols: {}
- branch: {}
- likely areas: {}

REFERENCE MAP (internal names):
{}
{}

QUESTION:
{}"#,
        stats.file_count,
        stats.total_loc,
        stats.symbol_count,
        context.branch,
        file_list.join(", "),
        symbols.join("\n"),
        memory_section,
        question
    );

    let response = call_llm_with_usage(&system, &user, Model::Smart, false).await?;
    Ok((response.content, response.usage))
}

fn load_project_ethos(repo_root: &Path) -> Option<String> {
    let path = repo_root.join("ETHOS.md");
    let content = std::fs::read_to_string(&path).ok()?;
    let trimmed = content.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(truncate_str(trimmed, ASK_ETHOS_MAX_CHARS).to_string())
}

fn tokenize_question_terms(input: &str) -> Vec<String> {
    let mut seen = HashSet::new();
    input
        .split(|c: char| !c.is_ascii_alphanumeric() && c != '_' && c != '-')
        .filter_map(|raw| {
            let term = raw.trim().to_ascii_lowercase();
            if term.len() < 3 || !seen.insert(term.clone()) {
                return None;
            }
            Some(term)
        })
        .collect()
}

fn normalize_path(path: &Path) -> String {
    path.to_string_lossy()
        .replace('\\', "/")
        .to_ascii_lowercase()
}

fn collect_changed_paths(context: &WorkContext) -> HashSet<String> {
    context
        .all_changed_files()
        .into_iter()
        .map(|path| normalize_path(path))
        .collect()
}

fn collect_changed_roots(changed_paths: &HashSet<String>) -> HashSet<String> {
    changed_paths
        .iter()
        .filter_map(|path| path.split('/').next().map(str::to_string))
        .collect()
}

fn rank_files_for_question(
    index: &CodebaseIndex,
    query_terms: &[String],
    focus_terms: &[String],
    changed_paths: &HashSet<String>,
    changed_roots: &HashSet<String>,
    limit: usize,
) -> Vec<String> {
    let mut scored: Vec<(i32, String)> = index
        .files
        .keys()
        .map(|path| {
            let normalized = normalize_path(path);
            let score = score_file_path(
                &normalized,
                query_terms,
                focus_terms,
                changed_paths,
                changed_roots,
            );

            (score, path.display().to_string())
        })
        .collect();

    scored.sort_by(|left, right| right.0.cmp(&left.0).then_with(|| left.1.cmp(&right.1)));
    scored
        .into_iter()
        .take(limit)
        .map(|(_, path)| path)
        .collect()
}

fn score_file_path(
    normalized_path: &str,
    query_terms: &[String],
    focus_terms: &[String],
    changed_paths: &HashSet<String>,
    changed_roots: &HashSet<String>,
) -> i32 {
    let mut score = 0i32;

    if changed_paths.contains(normalized_path) {
        score += 700;
    } else if changed_roots
        .iter()
        .any(|root| normalized_path.starts_with(&format!("{root}/")))
    {
        score += 220;
    }

    for term in query_terms {
        if normalized_path.contains(term) {
            score += 45;
        }
    }
    for term in focus_terms {
        if normalized_path.contains(term) {
            score += 28;
        }
    }

    score
}

fn rank_symbols_for_question(
    index: &CodebaseIndex,
    query_terms: &[String],
    focus_terms: &[String],
    changed_paths: &HashSet<String>,
    changed_roots: &HashSet<String>,
    limit: usize,
) -> Vec<String> {
    let mut scored = Vec::new();

    for (path, file) in &index.files {
        let path_str = path.display().to_string();
        let normalized_path = normalize_path(path);
        for symbol in file.symbols.iter().filter(|symbol| {
            matches!(
                symbol.kind,
                SymbolKind::Function | SymbolKind::Struct | SymbolKind::Enum
            )
        }) {
            let mut score = match symbol.kind {
                SymbolKind::Struct | SymbolKind::Enum => 35,
                SymbolKind::Function => 28,
                _ => 10,
            };

            if changed_paths.contains(&normalized_path) {
                score += 500;
            } else if changed_roots
                .iter()
                .any(|root| normalized_path.starts_with(&format!("{root}/")))
            {
                score += 160;
            }

            let symbol_name = symbol.name.to_ascii_lowercase();
            for term in query_terms {
                if symbol_name.contains(term) {
                    score += 55;
                }
                if normalized_path.contains(term) {
                    score += 18;
                }
            }
            for term in focus_terms {
                if symbol_name.contains(term) || normalized_path.contains(term) {
                    score += 15;
                }
            }

            scored.push((
                score,
                format!("{:?}: {} ({})", symbol.kind, symbol.name, path_str),
            ));
        }
    }

    scored.sort_by(|left, right| right.0.cmp(&left.0).then_with(|| left.1.cmp(&right.1)));
    scored
        .into_iter()
        .take(limit)
        .map(|(_, symbol)| symbol)
        .collect()
}

#[cfg(test)]
mod ask_ranking_tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn changed_file_gets_higher_rank_than_plain_match() {
        let query_terms = vec!["api".to_string()];
        let focus_terms = Vec::new();
        let changed_paths = HashSet::from(["src/changed.rs".to_string()]);
        let changed_roots = HashSet::from(["src".to_string()]);

        let changed_score = score_file_path(
            "src/changed.rs",
            &query_terms,
            &focus_terms,
            &changed_paths,
            &changed_roots,
        );
        let plain_score = score_file_path(
            "docs/api-notes.md",
            &query_terms,
            &focus_terms,
            &changed_paths,
            &changed_roots,
        );

        assert!(changed_score > plain_score);
    }

    #[test]
    fn query_term_match_boosts_path_score() {
        let query_terms = vec!["retry".to_string()];
        let focus_terms = Vec::new();
        let changed_paths = HashSet::new();
        let changed_roots = HashSet::new();

        let matched = score_file_path(
            "src/network/retry_policy.rs",
            &query_terms,
            &focus_terms,
            &changed_paths,
            &changed_roots,
        );
        let unmatched = score_file_path(
            "src/network/client.rs",
            &query_terms,
            &focus_terms,
            &changed_paths,
            &changed_roots,
        );

        assert!(matched > unmatched);
    }

    #[test]
    fn load_project_ethos_returns_none_when_missing() {
        let mut root = std::env::temp_dir();
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        root.push(format!("cosmos_ethos_missing_test_{}", nanos));
        std::fs::create_dir_all(&root).unwrap();

        assert!(load_project_ethos(&root).is_none());

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn load_project_ethos_reads_file_when_present() {
        let mut root = std::env::temp_dir();
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        root.push(format!("cosmos_ethos_present_test_{}", nanos));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("ETHOS.md"), "Safety before speed.\n").unwrap();

        let ethos = load_project_ethos(&root).expect("expected ethos content");
        assert!(
            ethos.contains("Safety before speed."),
            "load_project_ethos should include ETHOS.md content"
        );

        let _ = std::fs::remove_dir_all(&root);
    }
}

#[derive(Debug, Clone)]
pub struct SuggestionDiagnostics {
    pub run_id: String,
    pub model: String,
    pub iterations: usize,
    pub tool_calls: usize,
    pub tool_names: Vec<String>,
    pub tool_exec_ms: u64,
    pub llm_ms: u64,
    pub batch_verify_ms: u64,
    pub forced_final: bool,
    pub formatting_pass: bool,
    pub response_format: bool,
    pub response_healing: bool,
    pub parse_strategy: String,
    pub parse_stripped_markdown: bool,
    pub parse_used_sanitized_fix: bool,
    pub parse_used_json_fix: bool,
    pub parse_used_individual_parse: bool,
    pub raw_count: usize,
    pub deduped_count: usize,
    pub grounding_filtered: usize,
    pub low_confidence_filtered: usize,
    pub batch_verify_attempted: usize,
    pub batch_verify_verified: usize,
    pub batch_verify_not_found: usize,
    pub batch_verify_errors: usize,
    pub truncated_count: usize,
    pub final_count: usize,
    pub response_chars: usize,
    pub response_preview: String,
    pub evidence_pack_ms: u64,
    /// Number of evidence snippets included in outbound LLM prompts.
    pub sent_snippet_count: usize,
    /// Rough outbound evidence size in bytes after redaction.
    pub sent_bytes: usize,
    pub pack_pattern_count: usize,
    pub pack_hotspot_count: usize,
    pub pack_core_count: usize,
    pub pack_line1_ratio: f64,
    pub provisional_count: usize,
    pub generation_waves: usize,
    pub generation_topup_calls: usize,
    pub generation_mapped_count: usize,
    pub validated_count: usize,
    pub rejected_count: usize,
    pub rejected_evidence_skipped_count: usize,
    pub validation_rejection_histogram: HashMap<String, usize>,
    pub validation_deadline_exceeded: bool,
    pub validation_deadline_ms: u64,
    pub batch_missing_index_count: usize,
    pub batch_no_reason_count: usize,
    pub transport_retry_count: usize,
    pub transport_recovered_count: usize,
    pub rewrite_recovered_count: usize,
    pub prevalidation_contradiction_count: usize,
    pub validation_transport_retry_count: usize,
    pub validation_transport_recovered_count: usize,
    pub regen_stopped_validation_budget: bool,
    pub attempt_index: usize,
    pub attempt_count: usize,
    pub gate_passed: bool,
    pub gate_fail_reasons: Vec<String>,
    pub attempt_cost_usd: f64,
    pub attempt_ms: u64,
    pub overclaim_rewrite_count: usize,
    pub overclaim_rewrite_validated_count: usize,
    pub smart_rewrite_count: usize,
    pub deterministic_auto_validated_count: usize,
    pub semantic_dedup_dropped_count: usize,
    pub file_balance_dropped_count: usize,
    pub speculative_impact_dropped_count: usize,
    pub dominant_topic_ratio: f64,
    pub unique_topic_count: usize,
    pub dominant_file_ratio: f64,
    pub unique_file_count: usize,
    pub readiness_filtered_count: usize,
    pub readiness_score_mean: f64,
    pub regeneration_attempts: usize,
    pub refinement_complete: bool,
    pub notes: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SuggestionReviewFocus {
    #[default]
    BugHunt,
    SecurityReview,
}

impl SuggestionReviewFocus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::BugHunt => "bug_hunt",
            Self::SecurityReview => "security_review",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::BugHunt => "Bug Hunt",
            Self::SecurityReview => "Security Review",
        }
    }

    pub fn toggle(self) -> Self {
        match self {
            Self::BugHunt => Self::SecurityReview,
            Self::SecurityReview => Self::BugHunt,
        }
    }
}

#[derive(Debug, Clone)]
pub struct SuggestionQualityGateConfig {
    pub min_final_count: usize,
    pub max_final_count: usize,
    pub min_displayed_valid_ratio: f64,
    pub min_implementation_readiness_score: f32,
    pub max_smart_rewrites_per_run: usize,
    pub max_suggest_cost_usd: f64,
    pub max_suggest_ms: u64,
    pub max_attempts: usize,
    pub review_focus: SuggestionReviewFocus,
}

impl Default for SuggestionQualityGateConfig {
    fn default() -> Self {
        Self {
            min_final_count: 1,
            max_final_count: 12,
            min_displayed_valid_ratio: 1.0,
            min_implementation_readiness_score: DEFAULT_MIN_IMPLEMENTATION_READINESS_SCORE,
            max_smart_rewrites_per_run: DEFAULT_MAX_SMART_REWRITES_PER_RUN,
            // 0 means unbounded.
            max_suggest_cost_usd: 0.0,
            // 0 means unbounded.
            max_suggest_ms: 0,
            max_attempts: 1,
            review_focus: SuggestionReviewFocus::default(),
        }
    }
}

pub type SuggestionStreamSink =
    Arc<dyn Fn(String, super::agentic::AgenticStreamKind, String) + Send + Sync>;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default)]
pub struct SuggestionGateSnapshot {
    pub final_count: usize,
    pub displayed_valid_ratio: f64,
    pub pending_count: usize,
    #[serde(default)]
    pub ethos_actionable_count: usize,
    pub suggest_total_ms: u64,
    pub suggest_total_cost_usd: f64,
    #[serde(default)]
    pub dominant_topic_ratio: f64,
    #[serde(default)]
    pub unique_topic_count: usize,
    #[serde(default)]
    pub dominant_file_ratio: f64,
    #[serde(default)]
    pub unique_file_count: usize,
    pub passed: bool,
    #[serde(default)]
    pub fail_reasons: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct GatedSuggestionRunResult {
    pub suggestions: Vec<Suggestion>,
    pub usage: Option<Usage>,
    pub diagnostics: SuggestionDiagnostics,
    pub gate: SuggestionGateSnapshot,
}

#[derive(Debug, Clone, Copy, Default)]
struct EvidenceSnippetQuality {
    comment_ratio: f64,
    top_window_comment_ratio: f64,
    executable_ratio: f64,
}

fn snippet_code_line(line: &str) -> &str {
    if let Some((_, rest)) = line.split_once('|') {
        rest
    } else {
        line
    }
}

fn snippet_code_is_comment_or_blank(line: &str) -> bool {
    let code = snippet_code_line(line).trim();
    code.is_empty()
        || code.starts_with("//")
        || code.starts_with("/*")
        || code.starts_with('*')
        || code.starts_with('#')
}

fn evidence_snippet_quality(snippet: &str) -> EvidenceSnippetQuality {
    let lines: Vec<&str> = snippet.lines().collect();
    if lines.is_empty() {
        return EvidenceSnippetQuality::default();
    }

    let mut nonempty = 0usize;
    let mut comment = 0usize;
    let mut executable = 0usize;
    for line in &lines {
        let code = snippet_code_line(line).trim();
        if code.is_empty() {
            continue;
        }
        nonempty += 1;
        if snippet_code_is_comment_or_blank(line) {
            comment += 1;
        } else {
            executable += 1;
        }
    }
    if nonempty == 0 {
        return EvidenceSnippetQuality::default();
    }

    let top_window = lines.iter().take(10).copied().collect::<Vec<_>>();
    let mut top_nonempty = 0usize;
    let mut top_comment = 0usize;
    for line in top_window {
        let code = snippet_code_line(line).trim();
        if code.is_empty() {
            continue;
        }
        top_nonempty += 1;
        if snippet_code_is_comment_or_blank(line) {
            top_comment += 1;
        }
    }

    EvidenceSnippetQuality {
        comment_ratio: comment as f64 / nonempty as f64,
        top_window_comment_ratio: if top_nonempty == 0 {
            0.0
        } else {
            top_comment as f64 / top_nonempty as f64
        },
        executable_ratio: executable as f64 / nonempty as f64,
    }
}

fn snippet_is_low_quality_for_grounding(quality: EvidenceSnippetQuality) -> bool {
    quality.top_window_comment_ratio >= EVIDENCE_TOP_WINDOW_COMMENT_RATIO_MAX
        || quality.executable_ratio < EVIDENCE_EXECUTABLE_RATIO_MIN
}

fn is_test_like_path(path: &Path) -> bool {
    let normalized = path
        .to_string_lossy()
        .replace('\\', "/")
        .to_ascii_lowercase();
    normalized.contains("/tests/")
        || normalized.contains("/test/")
        || normalized.ends_with("_test.rs")
        || normalized.ends_with(".test.ts")
        || normalized.ends_with(".test.tsx")
        || normalized.ends_with(".spec.ts")
        || normalized.ends_with(".spec.tsx")
        || normalized.ends_with(".test.js")
        || normalized.ends_with(".spec.js")
}

#[derive(Debug, Clone, serde::Deserialize)]
struct AgenticSuggestionJson {
    #[serde(default)]
    file: String,
    #[serde(default)]
    line: Option<usize>,
    #[serde(default)]
    kind: String,
    #[serde(default)]
    priority: String,
    #[serde(default)]
    confidence: String,
    #[serde(default)]
    observed_behavior: String,
    #[serde(default)]
    impact_class: String,
    #[serde(default)]
    summary: String,
    #[serde(default)]
    detail: String,
    #[serde(default)]
    evidence_quote: String,
}

#[derive(Debug, Clone, serde::Deserialize)]
struct AgenticSuggestionResponseJson {
    suggestions: Vec<AgenticSuggestionJson>,
}

type ReportFindingJson = super::tools::ReportBackFinding;
type AgentReportEnvelopeJson = super::tools::ReportBackExplanation;

fn suggestion_has_usable_evidence_quality(suggestion: &Suggestion) -> bool {
    if let Some(top_ratio) = suggestion.validation_metadata.snippet_top_comment_ratio {
        if top_ratio >= EVIDENCE_TOP_WINDOW_COMMENT_RATIO_MAX {
            return false;
        }
    }
    if let Some(executable_ratio) = suggestion.validation_metadata.evidence_quality_score {
        if executable_ratio < EVIDENCE_EXECUTABLE_RATIO_MIN {
            return false;
        }
    }
    true
}

fn suggestion_claim_is_grounded_for_acceptance(suggestion: &Suggestion) -> bool {
    let Some(snippet) = suggestion.evidence.as_deref() else {
        return false;
    };
    if let Some(observed) = suggestion
        .validation_metadata
        .claim_observed_behavior
        .as_deref()
    {
        if !observed.trim().is_empty() {
            return claim_tokens_grounded_in_snippet(snippet, observed);
        }
    }
    let fallback_claim = format!(
        "{} {}",
        suggestion.summary,
        suggestion.detail.as_deref().unwrap_or("")
    );
    claim_tokens_grounded_in_snippet(snippet, &fallback_claim)
}

#[derive(Debug, Clone, Default)]
struct SuggestionDiversityMetrics {
    dominant_topic_ratio: f64,
    unique_topic_count: usize,
    dominant_file_ratio: f64,
    unique_file_count: usize,
}

fn normalize_similarity_token(raw: &str) -> Option<String> {
    let mut token = raw.trim().to_ascii_lowercase();
    if token.len() < 3 || token.chars().all(|ch| ch.is_ascii_digit()) {
        return None;
    }

    let stop_words = [
        "the", "and", "for", "with", "from", "that", "this", "these", "those", "are", "was",
        "were", "will", "can", "could", "should", "would", "into", "after", "before", "about",
        "across", "without", "while", "when", "where", "which", "there", "their", "them", "then",
        "than", "because", "using", "used", "just", "only", "into", "onto", "each", "more", "most",
        "some", "many", "much", "very", "also", "does", "doesnt", "dont", "did", "has", "have",
        "had", "its", "itself", "it", "our", "your", "you", "users", "user", "app", "code", "flow",
        "path", "issue", "issues", "problem", "problems",
    ];
    if stop_words.contains(&token.as_str()) {
        return None;
    }

    for suffix in ["ing", "ed", "es", "s"] {
        if token.len() > 5 && token.ends_with(suffix) {
            token.truncate(token.len() - suffix.len());
            break;
        }
    }

    if token.len() < 3 {
        return None;
    }

    Some(token)
}

fn collect_similarity_tokens(text: &str) -> HashSet<String> {
    text.split(|ch: char| !ch.is_ascii_alphanumeric() && ch != '_')
        .filter_map(normalize_similarity_token)
        .collect()
}

fn collect_topic_tokens(text: &str) -> Vec<String> {
    let generic_topic_tokens = [
        "error", "errors", "fail", "failure", "silent", "silently", "ignore", "ignored", "catch",
        "block", "return", "value",
    ];

    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for raw in text.split(|ch: char| !ch.is_ascii_alphanumeric() && ch != '_') {
        let Some(token) = normalize_similarity_token(raw) else {
            continue;
        };
        if generic_topic_tokens.contains(&token.as_str()) {
            continue;
        }
        if seen.insert(token.clone()) {
            out.push(token);
        }
        if out.len() >= 3 {
            break;
        }
    }
    out
}

fn suggestion_topic_key(suggestion: &Suggestion) -> String {
    let text = format!(
        "{} {}",
        suggestion.summary,
        suggestion.detail.as_deref().unwrap_or("")
    );
    let tokens = collect_topic_tokens(&text);
    if tokens.is_empty() {
        format!(
            "{}:{}",
            format!("{:?}", suggestion.kind).to_ascii_lowercase(),
            suggestion.file.display()
        )
    } else {
        format!(
            "{}:{}",
            format!("{:?}", suggestion.kind).to_ascii_lowercase(),
            tokens.join("_")
        )
    }
}

fn compute_suggestion_diversity_metrics(suggestions: &[Suggestion]) -> SuggestionDiversityMetrics {
    if suggestions.is_empty() {
        return SuggestionDiversityMetrics::default();
    }

    let mut topic_counts: HashMap<String, usize> = HashMap::new();
    let mut file_counts: HashMap<PathBuf, usize> = HashMap::new();
    for suggestion in suggestions {
        *topic_counts
            .entry(suggestion_topic_key(suggestion))
            .or_insert(0) += 1;
        *file_counts.entry(suggestion.file.clone()).or_insert(0) += 1;
    }

    let dominant_topic_count = topic_counts.values().copied().max().unwrap_or(0);
    let dominant_file_count = file_counts.values().copied().max().unwrap_or(0);
    SuggestionDiversityMetrics {
        dominant_topic_ratio: dominant_topic_count as f64 / suggestions.len() as f64,
        unique_topic_count: topic_counts.len(),
        dominant_file_ratio: dominant_file_count as f64 / suggestions.len() as f64,
        unique_file_count: file_counts.len(),
    }
}

#[derive(Debug, Clone, Default)]
struct DeterministicSelectionOutcome {
    suggestions: Vec<Suggestion>,
    dedup_dropped_count: usize,
    file_balance_dropped_count: usize,
    speculative_dropped_count: usize,
}

fn deterministic_soft_target_count(config: &SuggestionQualityGateConfig) -> usize {
    let hard_max = config.max_final_count.max(1);
    let preferred = hard_max
        .min(DETERMINISTIC_SUGGESTION_SOFT_TARGET_MAX)
        .max(DETERMINISTIC_SUGGESTION_SOFT_TARGET_MIN.min(hard_max));
    preferred.max(config.min_final_count.max(1)).min(hard_max)
}

fn bounded_suggestion_attempt_count(config: &SuggestionQualityGateConfig) -> usize {
    config
        .max_attempts
        .max(1)
        .min(MAX_SUGGESTION_ATTEMPTS_HARD_CAP)
}

fn review_focus_for_attempt(
    base_focus: SuggestionReviewFocus,
    attempt_index: usize,
) -> SuggestionReviewFocus {
    if attempt_index % 2 == 1 {
        base_focus
    } else {
        base_focus.toggle()
    }
}

fn deterministic_criticality_rank(criticality: Criticality) -> i64 {
    match criticality {
        Criticality::Critical => 40,
        Criticality::High => 30,
        Criticality::Medium => 20,
        Criticality::Low => 10,
    }
}

fn deterministic_category_rank(category: SuggestionCategory) -> i64 {
    match category {
        SuggestionCategory::Security => 8,
        SuggestionCategory::Bug => 4,
    }
}

fn deterministic_confidence_rank(confidence: cosmos_core::suggest::Confidence) -> i64 {
    match confidence {
        cosmos_core::suggest::Confidence::High => 8,
        cosmos_core::suggest::Confidence::Medium => 5,
        cosmos_core::suggest::Confidence::Low => 2,
    }
}

fn deterministic_suggestion_score(suggestion: &Suggestion) -> i64 {
    let readiness = suggestion
        .implementation_readiness_score
        .unwrap_or(DEFAULT_MIN_IMPLEMENTATION_READINESS_SCORE)
        .clamp(0.0, 1.0);
    let evidence_quality = suggestion
        .validation_metadata
        .evidence_quality_score
        .unwrap_or(0.0)
        .clamp(0.0, 1.0);
    let grounded_bonus = if suggestion_claim_is_grounded_for_acceptance(suggestion) {
        6
    } else {
        0
    };
    let detail_bonus = suggestion
        .detail
        .as_deref()
        .map(detail_is_concrete_enough)
        .unwrap_or(false) as i64;

    (deterministic_criticality_rank(suggestion.criticality) * 1_000)
        + (deterministic_category_rank(suggestion.category) * 100)
        + (deterministic_confidence_rank(suggestion.confidence) * 60)
        + ((readiness * 100.0).round() as i64 * 8)
        + ((evidence_quality * 100.0).round() as i64 * 5)
        + (grounded_bonus * 12)
        + (detail_bonus * 10)
}

fn deterministic_suggestion_dedup_key(suggestion: &Suggestion) -> String {
    let snippet_key = suggestion
        .evidence_refs
        .first()
        .map(|reference| format!("snippet:{}", reference.snippet_id))
        .unwrap_or_else(|| "snippet:none".to_string());
    let line = suggestion.line.unwrap_or(0);
    let topic = suggestion_topic_key(suggestion);
    format!(
        "{:?}:{}:{}:{}:{}",
        suggestion.category,
        suggestion.file.display(),
        line,
        snippet_key,
        topic
    )
}

fn normalize_suggestion_language(mut suggestion: Suggestion) -> Suggestion {
    let impact_class = suggestion.validation_metadata.claim_impact_class.as_deref();
    let detail_seed = suggestion.detail.as_deref().unwrap_or("");
    let rewritten_summary = normalize_ethos_summary(&suggestion.summary, detail_seed, impact_class);
    if !rewritten_summary.is_empty() {
        suggestion.summary = rewritten_summary;
    }

    let normalized_detail = normalize_grounded_detail(detail_seed, &suggestion.summary);
    if !normalized_detail.trim().is_empty() {
        suggestion.detail = Some(normalized_detail);
    }

    annotate_implementation_readiness(suggestion)
}

fn deterministic_select_suggestions(
    candidates: &[Suggestion],
    desired_count: usize,
    hard_max: usize,
) -> DeterministicSelectionOutcome {
    let mut outcome = DeterministicSelectionOutcome::default();
    if candidates.is_empty() {
        return outcome;
    }

    let target_count = desired_count.max(1).min(hard_max.max(1));
    let mut ranked = Vec::new();
    for candidate in candidates.iter().cloned() {
        let normalized = normalize_suggestion_language(candidate);
        if !suggestion_is_verified_bug_or_security(&normalized) {
            continue;
        }
        if !suggestion_has_usable_evidence_quality(&normalized) {
            continue;
        }
        if !suggestion_claim_is_grounded_for_acceptance(&normalized) {
            continue;
        }
        if has_speculative_impact_language(&normalized.summary) {
            outcome.speculative_dropped_count = outcome.speculative_dropped_count.saturating_add(1);
            continue;
        }
        if deterministic_prevalidation_ethos_reason(&normalized).is_some() {
            continue;
        }
        ranked.push(normalized);
    }

    ranked.sort_by(|left, right| {
        deterministic_suggestion_score(right)
            .cmp(&deterministic_suggestion_score(left))
            .then_with(|| right.criticality.cmp(&left.criticality))
            .then_with(|| right.priority.cmp(&left.priority))
            .then_with(|| right.confidence.cmp(&left.confidence))
            .then_with(|| left.file.cmp(&right.file))
            .then_with(|| {
                left.line
                    .unwrap_or(usize::MAX)
                    .cmp(&right.line.unwrap_or(usize::MAX))
            })
            .then_with(|| left.summary.cmp(&right.summary))
    });

    let mut deduped = Vec::new();
    let mut seen_keys = HashSet::new();
    for suggestion in ranked {
        let key = deterministic_suggestion_dedup_key(&suggestion);
        if seen_keys.insert(key) {
            deduped.push(suggestion);
        } else {
            outcome.dedup_dropped_count = outcome.dedup_dropped_count.saturating_add(1);
        }
    }

    if deduped.is_empty() {
        return outcome;
    }
    let target_count = target_count.min(deduped.len());
    let mut selected = Vec::new();
    let mut selected_ids = HashSet::new();
    let mut per_file = HashMap::new();
    let mut file_balance_skips = 0usize;

    for per_file_limit in [1usize, DETERMINISTIC_SUGGESTION_PER_FILE_MAX, usize::MAX] {
        for suggestion in &deduped {
            if selected.len() >= target_count {
                break;
            }
            if selected_ids.contains(&suggestion.id) {
                continue;
            }
            let current = per_file.get(&suggestion.file).copied().unwrap_or(0usize);
            if per_file_limit != usize::MAX && current >= per_file_limit {
                file_balance_skips = file_balance_skips.saturating_add(1);
                continue;
            }
            selected.push(suggestion.clone());
            selected_ids.insert(suggestion.id);
            *per_file.entry(suggestion.file.clone()).or_insert(0usize) += 1;
        }
    }

    outcome.file_balance_dropped_count = file_balance_skips;
    outcome.suggestions = selected;
    outcome
}

fn evidence_strength_score(suggestion: &Suggestion) -> f32 {
    let mut score = 0.20f32;
    if !suggestion.evidence_refs.is_empty() {
        score += 0.45;
    }
    if suggestion
        .evidence
        .as_deref()
        .map(|snippet| snippet.trim().len() >= 40)
        .unwrap_or(false)
    {
        score += 0.20;
    }
    if suggestion.line.unwrap_or_default() > 0 {
        score += 0.15;
    }
    score.clamp(0.0, 1.0)
}

fn scope_tightness_score(suggestion: &Suggestion) -> f32 {
    let file_count = suggestion.file_count();
    let mut score: f32 = match file_count {
        0 | 1 => 1.0,
        2 => 0.82,
        3 => 0.64,
        _ => 0.45,
    };
    let detail = suggestion
        .detail
        .as_deref()
        .unwrap_or("")
        .to_ascii_lowercase();
    let summary = suggestion.summary.to_ascii_lowercase();
    let broad_markers = [
        "across files",
        "cross-file",
        "refactor",
        "restructure",
        "sweep",
        "multiple modules",
        "many files",
    ];
    if broad_markers
        .iter()
        .any(|marker| detail.contains(marker) || summary.contains(marker))
    {
        score -= 0.18;
    }
    score.clamp(0.0, 1.0)
}

fn quick_check_targetability_score(path: &Path) -> f32 {
    let Some(ext) = path.extension().and_then(|ext| ext.to_str()) else {
        return 0.45;
    };
    match ext.to_ascii_lowercase().as_str() {
        "rs" | "js" | "jsx" | "ts" | "tsx" | "mjs" | "cjs" | "py" => 1.0,
        "json" | "toml" | "yaml" | "yml" => 0.70,
        _ => 0.55,
    }
}

fn evidence_claim_grounding_score(suggestion: &Suggestion) -> f32 {
    let claim_text = suggestion
        .validation_metadata
        .claim_observed_behavior
        .as_deref()
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| {
            format!(
                "{} {}",
                suggestion.summary,
                suggestion.detail.as_deref().unwrap_or("")
            )
        });
    let claim_tokens = claim_specific_tokens(&claim_text);
    if claim_tokens.is_empty() {
        return 0.65;
    }

    let Some(snippet) = suggestion.evidence.as_deref() else {
        return 0.40;
    };
    let snippet_tokens = snippet_identifier_tokens(snippet);
    if snippet_tokens.is_empty() {
        return 0.35;
    }

    let overlap = claim_tokens.intersection(&snippet_tokens).count();
    let ratio = overlap as f32 / claim_tokens.len() as f32;
    if ratio >= 0.80 {
        1.0
    } else if ratio >= 0.60 {
        0.85
    } else if ratio >= 0.45 {
        0.70
    } else if ratio >= 0.30 {
        0.52
    } else {
        0.25
    }
}

fn has_low_information_language(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    [
        "this matters because",
        "incorrect behavior",
        "potential issue",
        "may fail",
        "might fail",
        "could fail",
        "could break",
        "improve reliability",
        "improve performance",
        "unexpected behavior",
        "this path",
        "this flow",
    ]
    .iter()
    .any(|marker| lower.contains(marker))
}

fn description_specificity_score(suggestion: &Suggestion) -> f32 {
    let text = format!(
        "{} {}",
        suggestion.summary,
        suggestion.detail.as_deref().unwrap_or("")
    );
    let token_count = collect_similarity_tokens(&text).len();
    let mut score: f32 = match token_count {
        0..=2 => 0.35,
        3..=4 => 0.55,
        5..=7 => 0.72,
        _ => 0.88,
    };

    if has_low_information_language(&text) {
        score -= 0.18;
    }
    if suggestion.line.unwrap_or_default() > 0 {
        score += 0.07;
    }

    score.clamp(0.0, 1.0)
}

fn historical_fail_penalty_score(suggestion: &Suggestion) -> f32 {
    let text = format!(
        "{} {}",
        suggestion.summary,
        suggestion.detail.as_deref().unwrap_or("")
    )
    .to_ascii_lowercase();
    let risky_markers = [
        "rename",
        "move",
        "restructure",
        "widespread",
        "global",
        "large refactor",
        "multi-step",
    ];
    if risky_markers.iter().any(|marker| text.contains(marker)) {
        0.45
    } else {
        1.0
    }
}

fn has_overclaim_wording(suggestion: &Suggestion) -> bool {
    let text = format!(
        "{} {}",
        suggestion.summary,
        suggestion.detail.as_deref().unwrap_or("")
    )
    .to_ascii_lowercase();
    let markers = [
        "users may",
        "users will",
        "business impact",
        "could break",
        "might fail",
        "in production",
        "customer",
        "campaign reach",
        "reducing trust",
        "reduced trust",
        "slower user",
        "resource leak",
        "memory leak",
        "lock up",
        "freeze forever",
        "stuck forever",
    ];
    markers.iter().any(|marker| text.contains(marker))
}

fn has_speculative_impact_language(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    [
        "campaign reach",
        "marketing targeting",
        "targeting",
        "outreach",
        "privacy concerns",
        "campaign effectiveness",
        "missed marketing",
        "important alert emails",
        "users think",
        "frustrate users",
        "smooth experience",
        "engagement",
        "trust",
        "annoyance",
        "annoying",
        "memory growth",
        "memory bloat",
        "slowing the browser",
    ]
    .iter()
    .any(|marker| lower.contains(marker))
}

fn build_implementation_sketch(suggestion: &Suggestion) -> String {
    let line = suggestion.line.unwrap_or(1);
    let summary = suggestion.summary.trim();
    format!(
        "Change {} around line {} to address: {}. Keep the edit scoped to the validated file(s) only.",
        suggestion.file.display(),
        line,
        summary
    )
}

fn annotate_implementation_readiness(mut suggestion: Suggestion) -> Suggestion {
    let evidence_strength = evidence_strength_score(&suggestion);
    let scope_tightness = scope_tightness_score(&suggestion);
    let quick_check_targetability = quick_check_targetability_score(&suggestion.file);
    let historical_fail_penalty = historical_fail_penalty_score(&suggestion);
    let grounding = evidence_claim_grounding_score(&suggestion);
    let specificity = description_specificity_score(&suggestion);
    let overclaim_penalty = if has_overclaim_wording(&suggestion) {
        0.30
    } else {
        1.0
    };
    let speculative_penalty = if has_speculative_impact_language(&suggestion.summary) {
        0.35
    } else {
        1.0
    };
    let base_readiness = (0.30 * evidence_strength)
        + (0.24 * scope_tightness)
        + (0.18 * quick_check_targetability)
        + (0.12 * historical_fail_penalty)
        + (0.08 * overclaim_penalty)
        + (0.08 * speculative_penalty);
    let grounding_multiplier = 0.60 + (0.40 * grounding);
    let specificity_multiplier = 0.70 + (0.30 * specificity);
    let mut readiness = base_readiness * grounding_multiplier * specificity_multiplier;

    let mut flags = Vec::new();
    if evidence_strength < 0.65 {
        flags.push("weak_evidence_anchor".to_string());
    }
    if scope_tightness < 0.65 {
        flags.push("broad_or_multi_file_scope".to_string());
    }
    if quick_check_targetability < 0.65 {
        flags.push("low_quick_check_targetability".to_string());
    }
    if historical_fail_penalty < 0.65 {
        flags.push("historical_fail_risk".to_string());
    }
    if grounding < 0.50 {
        flags.push("claim_not_grounded_in_snippet".to_string());
    }
    if grounding < 0.30 {
        readiness = readiness.min(DEFAULT_MIN_IMPLEMENTATION_READINESS_SCORE - 0.02);
    }
    if specificity < 0.55 {
        flags.push("generic_or_low_information_description".to_string());
    }
    if overclaim_penalty < 1.0 {
        flags.push("overclaim_language".to_string());
    }
    if speculative_penalty < 1.0 {
        flags.push("speculative_impact_language".to_string());
    }

    suggestion.implementation_readiness_score = Some(readiness.clamp(0.0, 1.0));
    suggestion.implementation_risk_flags = flags;
    suggestion.implementation_sketch = Some(build_implementation_sketch(&suggestion));
    suggestion
}

fn normalize_claim_impact_class(raw: &str) -> Option<String> {
    let normalized = raw.trim().to_ascii_lowercase().replace([' ', '-'], "_");
    match normalized.as_str() {
        "correctness" | "reliability" | "security" | "performance" | "operability"
        | "maintainability" | "data_integrity" => Some(normalized),
        _ => None,
    }
}

fn impact_class_is_bug_or_security(impact_class: &str) -> bool {
    matches!(
        impact_class,
        "correctness" | "reliability" | "security" | "data_integrity"
    )
}

fn suggestion_targets_bug_or_security_scope(suggestion: &Suggestion) -> bool {
    if suggestion.kind != SuggestionKind::BugFix {
        return false;
    }
    suggestion
        .validation_metadata
        .claim_impact_class
        .as_deref()
        .map(impact_class_is_bug_or_security)
        .unwrap_or(false)
}

fn suggestion_is_verified_bug_or_security(suggestion: &Suggestion) -> bool {
    suggestion.validation_state == SuggestionValidationState::Validated
        && suggestion.verification_state == VerificationState::Verified
        && suggestion_targets_bug_or_security_scope(suggestion)
}

fn impact_class_summary_clause(impact_class: &str) -> Option<&'static str> {
    match impact_class {
        "correctness" => Some("which can produce incorrect results"),
        "reliability" => Some("which can fail in normal use"),
        "security" => Some("which can open a security risk"),
        "performance" => Some("which can slow down requests"),
        "operability" => Some("which can make incidents harder to diagnose"),
        "maintainability" => Some("which can make safe changes harder"),
        "data_integrity" => Some("which can leave stored data in an inconsistent state"),
        _ => None,
    }
}

fn build_claim_summary(observed_behavior: &str, impact_class: Option<&str>) -> String {
    let observed = normalize_grounded_summary(observed_behavior, observed_behavior, 1);
    if observed.is_empty() {
        return String::new();
    }
    let observed_core = observed
        .trim()
        .trim_end_matches(['.', '!', '?'])
        .trim()
        .to_string();
    if observed_core.is_empty() {
        return String::new();
    }
    let Some(impact) = impact_class.and_then(impact_class_summary_clause) else {
        return format!("{observed_core}.");
    };
    let lower = observed_core.to_ascii_lowercase();
    if lower.contains(impact) {
        return format!("{observed_core}.");
    }
    format!("{observed_core}, {impact}.")
}

fn agentic_suggestion_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "suggestions": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "file": { "type": "string" },
                        "line": { "type": "integer", "minimum": 1 },
                        "kind": {
                            "type": "string",
                            "enum": ["bugfix", "security", "reliability"]
                        },
                        "priority": { "type": "string", "enum": ["high", "medium", "low"] },
                        "confidence": { "type": "string", "enum": ["high", "medium"] },
                        "observed_behavior": { "type": "string" },
                        "impact_class": {
                            "type": "string",
                            "enum": [
                                "correctness",
                                "reliability",
                                "security",
                                "data_integrity"
                            ]
                        },
                        "summary": { "type": "string" },
                        "detail": { "type": "string" },
                        "evidence_quote": { "type": "string" }
                    },
                    "required": [
                        "file",
                        "kind",
                        "priority",
                        "confidence",
                        "observed_behavior",
                        "impact_class",
                        "summary",
                        "detail",
                        "evidence_quote"
                    ],
                    "additionalProperties": false
                }
            }
        },
        "required": ["suggestions"],
        "additionalProperties": false
    })
}

fn resolve_agentic_file(repo_root: &Path, raw_file: &str) -> Option<PathBuf> {
    let trimmed = raw_file.trim().trim_start_matches("./").replace('\\', "/");
    if trimmed.is_empty() {
        return None;
    }
    let candidate = PathBuf::from(trimmed);
    if candidate.is_absolute() {
        candidate
            .strip_prefix(repo_root)
            .ok()
            .map(|path| path.to_path_buf())
    } else {
        Some(candidate)
    }
}

fn resolve_index_file(index: &CodebaseIndex, candidate: &Path) -> Option<PathBuf> {
    if index.files.contains_key(candidate) {
        return Some(candidate.to_path_buf());
    }

    let candidate_str = candidate.to_string_lossy();
    if candidate_str.is_empty() {
        return None;
    }

    index
        .files
        .keys()
        .find(|path| {
            let indexed = path.to_string_lossy();
            indexed.ends_with(candidate_str.as_ref())
        })
        .cloned()
}

fn stable_evidence_id(file: &Path, line: usize, snippet: &str) -> usize {
    // Deterministic FNV-1a hash so evidence IDs remain stable across runs.
    let mut hash: u64 = 0xcbf29ce484222325;
    let mut feed = |bytes: &[u8]| {
        for byte in bytes {
            hash ^= *byte as u64;
            hash = hash.wrapping_mul(0x100000001b3);
        }
    };

    feed(file.to_string_lossy().as_bytes());
    feed(&line.to_le_bytes());
    feed(snippet.as_bytes());

    let masked = hash & (usize::MAX as u64);
    masked as usize
}

fn infer_agentic_impact_class(kind: &str, summary: &str, detail: &str) -> Option<String> {
    let text = format!("{} {}", summary, detail).to_ascii_lowercase();
    if text.contains("security")
        || text.contains("traversal")
        || text.contains("injection")
        || text.contains("unauthorized")
        || text.contains("secret")
        || text.contains("token")
    {
        return Some("security".to_string());
    }
    if text.contains("corrupt")
        || text.contains("inconsistent")
        || text.contains("duplicate")
        || text.contains("lost")
        || text.contains("overwrite")
    {
        return Some("data_integrity".to_string());
    }
    if text.contains("panic")
        || text.contains("crash")
        || text.contains("hang")
        || text.contains("stuck")
        || text.contains("retry")
    {
        return Some("reliability".to_string());
    }

    match kind.trim().to_ascii_lowercase().as_str() {
        "security" => Some("security".to_string()),
        "reliability" => Some("reliability".to_string()),
        "bugfix" => Some("correctness".to_string()),
        _ => None,
    }
}

fn map_agentic_suggestions(
    repo_root: &Path,
    index: &CodebaseIndex,
    raw: Vec<AgenticSuggestionJson>,
) -> Vec<Suggestion> {
    let mut out = Vec::new();
    let mut used_evidence_ids = HashSet::new();
    for item in raw {
        let normalized_kind = item.kind.trim().to_ascii_lowercase();
        if !matches!(
            normalized_kind.as_str(),
            "bugfix" | "security" | "reliability"
        ) {
            continue;
        }

        let Some(raw_file) = resolve_agentic_file(repo_root, &item.file) else {
            continue;
        };
        let Some(file) = resolve_index_file(index, &raw_file) else {
            continue;
        };

        let summary_seed = item.summary.trim();
        if summary_seed.is_empty() {
            continue;
        }
        let detail_seed = item.detail.trim();
        let evidence_quote = item.evidence_quote.trim();
        let line = item.line.unwrap_or(1).max(1);
        if evidence_quote.is_empty() {
            continue;
        }

        let claim_observed_seed = item.observed_behavior.trim();
        let claim_observed_behavior = normalize_grounded_summary(
            if claim_observed_seed.is_empty() {
                summary_seed
            } else {
                claim_observed_seed
            },
            detail_seed,
            line,
        );
        if claim_observed_behavior.is_empty() {
            continue;
        }
        let claim_impact_class = normalize_claim_impact_class(&item.impact_class)
            .or_else(|| infer_agentic_impact_class(&item.kind, summary_seed, detail_seed));
        let Some(claim_impact_class) =
            claim_impact_class.filter(|impact| impact_class_is_bug_or_security(impact))
        else {
            continue;
        };
        let claim_summary =
            build_claim_summary(&claim_observed_behavior, Some(claim_impact_class.as_str()));
        let detail = normalize_grounded_detail(
            if detail_seed.is_empty() {
                summary_seed
            } else {
                detail_seed
            },
            &claim_summary,
        );
        let summary = normalize_ethos_summary(
            if claim_summary.is_empty() {
                summary_seed
            } else {
                claim_summary.as_str()
            },
            &detail,
            Some(claim_impact_class.as_str()),
        );
        if summary.is_empty() {
            continue;
        }
        let quality = evidence_snippet_quality(evidence_quote);
        if snippet_is_low_quality_for_grounding(quality) {
            continue;
        }

        let mut evidence_id = stable_evidence_id(&file, line, evidence_quote);
        while !used_evidence_ids.insert(evidence_id) {
            evidence_id = evidence_id.wrapping_add(1);
        }
        let kind = SuggestionKind::BugFix;
        let priority = match item.priority.to_ascii_lowercase().as_str() {
            "high" => cosmos_core::suggest::Priority::High,
            "low" => cosmos_core::suggest::Priority::Low,
            _ => cosmos_core::suggest::Priority::Medium,
        };
        let confidence = match item.confidence.to_ascii_lowercase().as_str() {
            "high" => cosmos_core::suggest::Confidence::High,
            _ => cosmos_core::suggest::Confidence::Medium,
        };
        let category = if claim_impact_class == "security" {
            SuggestionCategory::Security
        } else {
            SuggestionCategory::Bug
        };

        let suggestion = Suggestion::new(
            kind,
            priority,
            file.clone(),
            summary,
            cosmos_core::suggest::SuggestionSource::LlmDeep,
        )
        .with_category(category)
        .with_confidence(confidence)
        .with_line(line)
        .with_detail(detail)
        .with_evidence(evidence_quote.to_string())
        .with_evidence_refs(vec![SuggestionEvidenceRef {
            snippet_id: evidence_id,
            file,
            line,
        }])
        .with_validation_metadata(SuggestionValidationMetadata {
            evidence_quality_score: Some(quality.executable_ratio),
            snippet_comment_ratio: Some(quality.comment_ratio),
            snippet_top_comment_ratio: Some(quality.top_window_comment_ratio),
            claim_observed_behavior: Some(claim_observed_behavior),
            claim_impact_class: Some(claim_impact_class),
            ..Default::default()
        })
        .with_validation_state(SuggestionValidationState::Validated)
        .with_verification_state(VerificationState::Verified);
        out.push(annotate_implementation_readiness(suggestion));
    }
    out
}

fn parse_report_category(raw: &str) -> Option<SuggestionCategory> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "bug" => Some(SuggestionCategory::Bug),
        "security" => Some(SuggestionCategory::Security),
        _ => None,
    }
}

fn parse_report_criticality(raw: &str) -> Option<Criticality> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "critical" => Some(Criticality::Critical),
        "high" => Some(Criticality::High),
        "medium" => Some(Criticality::Medium),
        "low" => Some(Criticality::Low),
        _ => None,
    }
}

fn map_report_findings_to_suggestions(
    repo_root: &Path,
    index: &CodebaseIndex,
    findings: Vec<ReportFindingJson>,
) -> Vec<Suggestion> {
    let mut out = Vec::new();
    let mut seen_evidence = HashSet::new();

    for finding in findings {
        let category_lower = finding.category.trim().to_ascii_lowercase();
        let category = if let Some(parsed) = parse_report_category(&finding.category) {
            parsed
        } else if category_lower.contains("sec") {
            SuggestionCategory::Security
        } else if category_lower.contains("bug") {
            SuggestionCategory::Bug
        } else {
            continue;
        };
        let criticality =
            parse_report_criticality(&finding.criticality).unwrap_or(Criticality::Medium);

        let file_hint = finding
            .file
            .trim()
            .trim_start_matches("/repo/")
            .trim_start_matches("./");
        let resolved_file =
            resolve_agentic_file(repo_root, &finding.file).or_else(|| match file_hint.is_empty() {
                true => None,
                false => Some(PathBuf::from(file_hint)),
            });
        let Some(raw_file) = resolved_file else {
            continue;
        };
        let file = resolve_index_file(index, &raw_file).unwrap_or(raw_file);
        let file_loc = index.files.get(&file).map(|f| f.loc).unwrap_or(usize::MAX);
        let mut line = finding.line.max(1);
        if file_loc != usize::MAX && file_loc > 0 {
            line = line.min(file_loc);
        }

        let impact_class = match category {
            SuggestionCategory::Bug => "correctness".to_string(),
            SuggestionCategory::Security => "security".to_string(),
        };
        let raw_summary = if finding.summary.trim().is_empty() {
            finding.detail.as_str()
        } else {
            finding.summary.as_str()
        };
        let mut summary =
            normalize_ethos_summary(raw_summary, &finding.detail, Some(impact_class.as_str()));
        if summary.is_empty() {
            let fallback_seed = match category {
                SuggestionCategory::Security => "Unsafe access may be possible in this flow",
                SuggestionCategory::Bug => "This flow can fail unexpectedly at runtime",
            };
            summary = normalize_ethos_summary(
                fallback_seed,
                &finding.detail,
                Some(impact_class.as_str()),
            );
        }
        let mut detail = normalize_grounded_detail(&finding.detail, &summary);
        if detail.trim().is_empty() {
            detail = summary.clone();
        }

        let evidence_text = if finding.evidence_quote.trim().is_empty() {
            continue;
        } else {
            finding.evidence_quote.trim().to_string()
        };
        let quality = evidence_snippet_quality(&evidence_text);
        if snippet_is_low_quality_for_grounding(quality) {
            continue;
        }
        let mut evidence_id = stable_evidence_id(&file, line, &evidence_text);
        while !seen_evidence.insert(evidence_id) {
            evidence_id = evidence_id.wrapping_add(1);
        }

        let claim_observed_behavior = normalize_grounded_summary(
            if finding.summary.trim().is_empty() {
                detail.as_str()
            } else {
                finding.summary.as_str()
            },
            &detail,
            line,
        );
        if claim_observed_behavior.is_empty() {
            continue;
        }

        let mut suggestion = Suggestion::new(
            SuggestionKind::BugFix,
            criticality.to_priority(),
            file.clone(),
            summary,
            cosmos_core::suggest::SuggestionSource::LlmDeep,
        )
        .with_category(category)
        .with_criticality(criticality)
        .with_confidence(cosmos_core::suggest::Confidence::High)
        .with_line(line)
        .with_detail(detail)
        .with_evidence(evidence_text)
        .with_evidence_refs(vec![SuggestionEvidenceRef {
            snippet_id: evidence_id,
            file,
            line,
        }])
        .with_validation_metadata(SuggestionValidationMetadata {
            evidence_quality_score: Some(quality.executable_ratio),
            snippet_comment_ratio: Some(quality.comment_ratio),
            snippet_top_comment_ratio: Some(quality.top_window_comment_ratio),
            claim_observed_behavior: Some(claim_observed_behavior),
            claim_impact_class: Some(impact_class),
            ..Default::default()
        });

        suggestion = annotate_implementation_readiness(suggestion);
        if !suggestion_has_usable_evidence_quality(&suggestion) {
            continue;
        }
        if !suggestion_claim_is_grounded_for_acceptance(&suggestion) {
            continue;
        }
        if deterministic_prevalidation_ethos_reason(&suggestion).is_some() {
            continue;
        }
        suggestion = suggestion
            .with_validation_state(SuggestionValidationState::Validated)
            .with_verification_state(VerificationState::Verified);
        out.push(suggestion);
    }

    out
}

fn summary_contains_internal_references(summary: &str) -> bool {
    let lower = summary.to_ascii_lowercase();
    summary.contains('`')
        || summary.contains("::")
        || summary.contains("->")
        || lower.contains(".rs")
        || lower.contains(".ts")
        || lower.contains(".js")
        || lower.contains(".py")
        || lower.contains("src/")
        || lower.contains("crates/")
        || lower.contains("line ")
}

fn summary_has_visible_runtime_outcome(summary: &str) -> bool {
    let lower = summary.to_ascii_lowercase();
    [
        "fails",
        "failure",
        "error",
        "errors",
        "crash",
        "panic",
        "hang",
        "stuck",
        "timeout",
        "times out",
        "returns",
        "wrong",
        "incorrect",
        "duplicate",
        "missing",
        "drops",
        "lost",
        "blocked",
        "cannot",
        "can't",
        "does not",
        "doesn't",
        "slow",
        "latency",
        "inconsistent",
    ]
    .iter()
    .any(|marker| lower.contains(marker))
}

fn detail_is_concrete_enough(detail: &str) -> bool {
    if detail.trim().len() < 40 {
        return false;
    }
    let lower = detail.to_ascii_lowercase();
    [
        "because", "when", "if", "without", "after", "before", "causes", "causing", "returns",
        "throws", "panic", "retry", "guard", "validate", "handle", "log",
    ]
    .iter()
    .any(|marker| lower.contains(marker))
}

fn deterministic_prevalidation_ethos_reason(suggestion: &Suggestion) -> Option<String> {
    let has_evidence_text = suggestion
        .evidence
        .as_deref()
        .map(str::trim)
        .map(|text| !text.is_empty())
        .unwrap_or(false);
    if !has_evidence_text {
        return None;
    }

    let summary = suggestion.summary.trim();
    let detail = suggestion.detail.as_deref().unwrap_or("").trim();
    let observed_behavior = suggestion
        .validation_metadata
        .claim_observed_behavior
        .as_deref()
        .unwrap_or("")
        .trim();
    if summary.is_empty() {
        return Some("Non-actionable summary: missing user-facing description.".to_string());
    }
    if summary_contains_internal_references(summary) {
        return Some(
            "Summary violates plain-language ethos: remove file paths or code-symbol jargon."
                .to_string(),
        );
    }
    if !summary_has_visible_runtime_outcome(summary)
        && !summary_has_visible_runtime_outcome(detail)
        && !summary_has_visible_runtime_outcome(observed_behavior)
    {
        return Some(
            "Summary lacks clear real-world outcome: explain what goes wrong for users."
                .to_string(),
        );
    }

    if !detail_is_concrete_enough(detail) {
        return Some(
            "Detail is not actionable enough: describe concrete cause and change direction."
                .to_string(),
        );
    }

    let specificity = description_specificity_score(suggestion);
    if specificity < 0.60 {
        return Some(
            "Description is too generic for safe action; add concrete behavior and cause."
                .to_string(),
        );
    }

    None
}

fn snippet_identifier_tokens(snippet: &str) -> HashSet<String> {
    let mut tokens = HashSet::new();
    for line in snippet.lines() {
        let code = snippet_code_line(line);
        for raw in code.split(|ch: char| !ch.is_ascii_alphanumeric() && ch != '_') {
            if let Some(token) = normalize_similarity_token(raw) {
                tokens.insert(token);
            }
        }
    }
    tokens
}

fn claim_specific_tokens(text: &str) -> HashSet<String> {
    let generic_claim_tokens = [
        "error",
        "errors",
        "silent",
        "silently",
        "ignore",
        "ignored",
        "swallow",
        "swallowed",
        "log",
        "logged",
        "logging",
        "catch",
        "exception",
        "exceptions",
        "failure",
        "failures",
        "hidden",
        "reported",
        "reporting",
        "block",
        "empty",
        "not",
        "failur",
        "ignor",
        "logg",
    ];
    collect_similarity_tokens(text)
        .into_iter()
        .filter(|token| !generic_claim_tokens.contains(&token.as_str()))
        .collect()
}

fn claim_tokens_grounded_in_snippet(snippet: &str, claim_text: &str) -> bool {
    let claim_tokens = claim_specific_tokens(claim_text);
    if claim_tokens.is_empty() {
        return true;
    }

    let snippet_tokens = snippet_identifier_tokens(snippet);
    if snippet_tokens.is_empty() {
        return false;
    }

    let overlap = claim_tokens.intersection(&snippet_tokens).count();
    let overlap_ratio = overlap as f64 / claim_tokens.len() as f64;
    let min_ratio = if claim_tokens.len() <= 8 { 0.10 } else { 0.15 };
    overlap >= 2 || (overlap >= 1 && overlap_ratio >= min_ratio)
}

#[allow(clippy::too_many_arguments)]
pub async fn analyze_codebase_fast_grounded(
    repo_root: &Path,
    index: &CodebaseIndex,
    context: &WorkContext,
    _repo_memory: Option<String>,
    generation_model: Model,
    generation_target: usize,
    retry_feedback: Option<&str>,
) -> anyhow::Result<(Vec<Suggestion>, Option<Usage>, SuggestionDiagnostics)> {
    ensure_non_summary_model(generation_model, "Suggestion generation")?;
    let run_id = Uuid::new_v4().to_string();
    let target = clamp_agentic_target(generation_target);
    let iteration_budget = agentic_iterations_for_target(target);
    let subagent_count = subagent_count_for_target(target);
    let focus_file_limit = subagent_count * AGENTIC_SUBAGENT_FILES_PER_AGENT;
    let focus_files =
        rank_top_churn_files_for_subagents(repo_root, index, context, target, focus_file_limit);
    let focus_shards = shard_subagent_focus_files(&focus_files, subagent_count);
    let project_ethos = load_project_ethos(repo_root);
    let mut subagent_targets = vec![(target / subagent_count).clamp(2, 4); subagent_count];
    let mut distributed = subagent_targets.iter().sum::<usize>();
    let mut cursor = 0usize;
    while distributed < target {
        if subagent_targets[cursor] < 4 {
            subagent_targets[cursor] += 1;
            distributed += 1;
        }
        cursor = (cursor + 1) % subagent_count;
        if cursor == 0 && subagent_targets.iter().all(|value| *value >= 4) {
            break;
        }
    }

    let call_start = std::time::Instant::now();
    let response_format =
        schema_to_response_format("agentic_suggestions", agentic_suggestion_schema());

    let agent_tasks = focus_shards
        .into_iter()
        .enumerate()
        .map(|(subagent_index, shard)| {
            let repo_root = repo_root.to_path_buf();
            let shard_for_prompt = shard.clone();
            let subagent_target = subagent_targets
                .get(subagent_index)
                .copied()
                .unwrap_or(2)
                .clamp(2, 4);
            let user_prompt = build_subagent_user_prompt(
                subagent_index,
                subagent_count,
                subagent_target,
                &shard_for_prompt,
                project_ethos.as_deref(),
                retry_feedback,
            );
            let response_format = response_format.clone();
            async move {
                let started = std::time::Instant::now();
                let response = call_llm_agentic(
                    AGENTIC_SUGGESTIONS_SYSTEM,
                    &user_prompt,
                    generation_model,
                    &repo_root,
                    false,
                    iteration_budget,
                    Some(response_format),
                )
                .await;
                (
                    subagent_index,
                    shard,
                    started.elapsed().as_millis() as u64,
                    response,
                )
            }
        });

    let agent_outputs = join_all(agent_tasks).await;
    let llm_ms = call_start.elapsed().as_millis() as u64;

    let mut usage: Option<Usage> = None;
    let mut response_preview_parts = Vec::new();
    let mut response_chars = 0usize;
    let mut raw_count = 0usize;
    let mut suggestions = Vec::new();
    let mut missing_or_invalid = 0usize;
    let mut parse_errors = Vec::new();
    let mut successful_subagents = 0usize;
    let mut tool_names = Vec::new();
    let mut tool_exec_ms = 0u64;

    for (subagent_index, shard, elapsed_ms, response_result) in agent_outputs {
        tool_exec_ms = tool_exec_ms.saturating_add(elapsed_ms);
        if !shard.is_empty() {
            let scope_preview = shard
                .iter()
                .take(2)
                .map(|path| path.display().to_string())
                .collect::<Vec<_>>()
                .join(",");
            tool_names.push(format!(
                "subagent_{}:[{}]",
                subagent_index + 1,
                scope_preview
            ));
        }

        let response = match response_result {
            Ok(value) => value,
            Err(err) => {
                parse_errors.push(format!(
                    "Subagent {} failed: {}",
                    subagent_index + 1,
                    truncate_str(&err.to_string(), 220)
                ));
                continue;
            }
        };

        successful_subagents += 1;
        usage = merge_usage(usage, response.usage.clone());
        response_chars = response_chars.saturating_add(response.content.len());
        response_preview_parts.push(format!(
            "a{}:{}",
            subagent_index + 1,
            truncate_str(&response.content, 80)
        ));

        match serde_json::from_str::<AgenticSuggestionResponseJson>(&response.content) {
            Ok(parsed) => {
                let raw_this = parsed.suggestions.len();
                raw_count = raw_count.saturating_add(raw_this);
                let mapped = map_agentic_suggestions(repo_root, index, parsed.suggestions);
                missing_or_invalid =
                    missing_or_invalid.saturating_add(raw_this.saturating_sub(mapped.len()));
                suggestions.extend(mapped);
            }
            Err(err) => {
                parse_errors.push(format!(
                    "Subagent {} parse failure: {}",
                    subagent_index + 1,
                    truncate_str(&err.to_string(), 220)
                ));
            }
        }
    }

    let response_preview = truncate_str(&response_preview_parts.join(" | "), 240).to_string();

    let mut run_notes: Vec<String> = Vec::new();
    let evidence_pack_ms = 0u64;
    let sent_snippet_count = 0usize;
    let sent_bytes = 0usize;

    let provisional_cap = FAST_GROUNDED_PROVISIONAL_TARGET_MAX;
    if suggestions.len() > provisional_cap {
        missing_or_invalid += suggestions.len().saturating_sub(provisional_cap);
        suggestions.truncate(provisional_cap);
    }

    if suggestions.is_empty() {
        let parse_suffix = parse_errors
            .first()
            .map(|text| format!(" {}", truncate_str(text, 260)))
            .unwrap_or_default();
        return Err(anyhow::anyhow!(
            "Suggestion generation completed but produced no usable suggestions.{}",
            parse_suffix
        ));
    }

    run_notes.push(format!("subagents_planned:{}", subagent_count));
    run_notes.push(format!(
        "subagents_successful:{}/{}",
        successful_subagents, subagent_count
    ));
    run_notes.push(format!("churn_focus_file_count:{}", focus_files.len()));
    if !focus_files.is_empty() {
        let focus_preview = focus_files
            .iter()
            .take(6)
            .map(|path| path.display().to_string())
            .collect::<Vec<_>>()
            .join(",");
        run_notes.push(format!("churn_focus_preview:{}", focus_preview));
    }
    if !parse_errors.is_empty() {
        run_notes.push(format!(
            "agentic_parse_errors:{}",
            truncate_str(&parse_errors.join(" | "), 180)
        ));
    }

    let diagnostics = SuggestionDiagnostics {
        run_id,
        model: generation_model.id().to_string(),
        iterations: subagent_count,
        tool_calls: iteration_budget.saturating_mul(successful_subagents.max(1)),
        tool_names,
        tool_exec_ms,
        llm_ms,
        batch_verify_ms: 0,
        forced_final: false,
        formatting_pass: false,
        response_format: true,
        response_healing: true,
        parse_strategy: "agentic_churn_subagents".to_string(),
        parse_stripped_markdown: false,
        parse_used_sanitized_fix: false,
        parse_used_json_fix: false,
        parse_used_individual_parse: false,
        raw_count,
        deduped_count: suggestions.len(),
        grounding_filtered: missing_or_invalid,
        low_confidence_filtered: 0,
        batch_verify_attempted: 0,
        batch_verify_verified: 0,
        batch_verify_not_found: 0,
        batch_verify_errors: 0,
        truncated_count: 0,
        final_count: suggestions.len(),
        response_chars,
        response_preview,
        evidence_pack_ms,
        sent_snippet_count,
        sent_bytes,
        pack_pattern_count: 0,
        pack_hotspot_count: 0,
        pack_core_count: 0,
        pack_line1_ratio: 0.0,
        provisional_count: suggestions.len(),
        generation_waves: subagent_count,
        generation_topup_calls: 0,
        generation_mapped_count: suggestions.len(),
        validated_count: 0,
        rejected_count: 0,
        rejected_evidence_skipped_count: 0,
        validation_rejection_histogram: HashMap::new(),
        validation_deadline_exceeded: false,
        validation_deadline_ms: 0,
        batch_missing_index_count: 0,
        batch_no_reason_count: 0,
        transport_retry_count: 0,
        transport_recovered_count: 0,
        rewrite_recovered_count: 0,
        prevalidation_contradiction_count: 0,
        validation_transport_retry_count: 0,
        validation_transport_recovered_count: 0,
        regen_stopped_validation_budget: false,
        attempt_index: 1,
        attempt_count: 1,
        gate_passed: false,
        gate_fail_reasons: Vec::new(),
        attempt_cost_usd: 0.0,
        attempt_ms: 0,
        overclaim_rewrite_count: 0,
        overclaim_rewrite_validated_count: 0,
        smart_rewrite_count: 0,
        deterministic_auto_validated_count: 0,
        semantic_dedup_dropped_count: 0,
        file_balance_dropped_count: 0,
        speculative_impact_dropped_count: 0,
        dominant_topic_ratio: 0.0,
        unique_topic_count: 0,
        dominant_file_ratio: 0.0,
        unique_file_count: 0,
        readiness_filtered_count: 0,
        readiness_score_mean: 0.0,
        regeneration_attempts: 0,
        refinement_complete: false,
        notes: run_notes,
    };

    Ok((suggestions, usage, diagnostics))
}

fn ratio(numerator: usize, denominator: usize) -> f64 {
    if denominator == 0 {
        0.0
    } else {
        numerator as f64 / denominator as f64
    }
}

fn suggestion_meets_ethos_contract(suggestion: &Suggestion) -> bool {
    suggestion_is_verified_bug_or_security(suggestion)
        && suggestion_has_usable_evidence_quality(suggestion)
        && suggestion_claim_is_grounded_for_acceptance(suggestion)
        && deterministic_prevalidation_ethos_reason(suggestion).is_none()
}

fn build_gate_snapshot(
    config: &SuggestionQualityGateConfig,
    suggestions: &[Suggestion],
    suggest_total_ms: u64,
    suggest_total_cost_usd: f64,
) -> SuggestionGateSnapshot {
    let final_count = suggestions.len();
    let validated_count = suggestions
        .iter()
        .filter(|s| suggestion_is_verified_bug_or_security(s))
        .count();
    let ethos_actionable_count = suggestions
        .iter()
        .filter(|s| suggestion_meets_ethos_contract(s))
        .count();
    let pending_count = final_count.saturating_sub(validated_count);
    let displayed_valid_ratio = ratio(validated_count, final_count);
    let diversity_metrics = compute_suggestion_diversity_metrics(suggestions);
    let mut fail_reasons = Vec::new();
    if final_count < config.min_final_count {
        fail_reasons.push(format!(
            "final_count_below_min:{}<{}",
            final_count, config.min_final_count
        ));
    }
    if final_count > config.max_final_count {
        fail_reasons.push(format!(
            "final_count_above_max:{}>{}",
            final_count, config.max_final_count
        ));
    }
    if displayed_valid_ratio < config.min_displayed_valid_ratio {
        fail_reasons.push(format!(
            "displayed_valid_ratio_below_min:{:.3}<{:.3}",
            displayed_valid_ratio, config.min_displayed_valid_ratio
        ));
    }
    if config.max_suggest_cost_usd > 0.0 && suggest_total_cost_usd > config.max_suggest_cost_usd {
        fail_reasons.push(format!(
            "suggest_cost_above_max:{:.4}>{:.4}",
            suggest_total_cost_usd, config.max_suggest_cost_usd
        ));
    }
    if config.max_suggest_ms > 0 && suggest_total_ms > config.max_suggest_ms {
        fail_reasons.push(format!(
            "suggest_time_above_max:{}>{}",
            suggest_total_ms, config.max_suggest_ms
        ));
    }

    SuggestionGateSnapshot {
        final_count,
        displayed_valid_ratio,
        pending_count,
        ethos_actionable_count,
        suggest_total_ms,
        suggest_total_cost_usd,
        dominant_topic_ratio: diversity_metrics.dominant_topic_ratio,
        unique_topic_count: diversity_metrics.unique_topic_count,
        dominant_file_ratio: diversity_metrics.dominant_file_ratio,
        unique_file_count: diversity_metrics.unique_file_count,
        passed: fail_reasons.is_empty(),
        fail_reasons,
    }
}

fn build_review_agent_user_prompt(
    role: &str,
    project_ethos: Option<&str>,
    repo_memory: Option<&str>,
    retry_feedback: Option<&str>,
) -> String {
    let mut prompt = String::from(
        "Repository is mounted at /repo.\n\
Explore freely with tools (`view_directory`, `grep_search`, `view_file`) and choose the most promising areas yourself.\n\
Do not wait for assigned files; investigate independently and follow evidence across related code.\n",
    );

    prompt.push_str(
        "\nTargets:\n- Find concrete, high-signal verified issues only.\n- Never fabricate evidence.\n- Finish only with `report_back`.\n- If no verified issues remain, call `report_back` with findings: [] and files: [].\n",
    );

    if role == "bug_hunter" {
        prompt.push_str(
            "\nBug checklist:\n- unchecked panic/unwrap paths\n- missing error handling for fallible operations\n- edge-case logic errors (null/empty/bounds)\n- concurrency misuse/races\n",
        );
    } else if role == "security_reviewer" {
        prompt.push_str(
        "\nSecurity checklist:\n- auth/authz bypass across trust boundaries\n- injection risks (sql/shell/template)\n- unsafe parsing/deserialization of untrusted input\n- secrets/credentials exposure\n- path traversal/unsafe filesystem access\n",
        );
    }

    if let Some(ethos) = project_ethos.map(str::trim).filter(|v| !v.is_empty()) {
        prompt.push_str("\nPROJECT ETHOS:\n");
        prompt.push_str(truncate_str(ethos, REVIEW_AGENT_ETHOS_MAX_CHARS));
        prompt.push('\n');
    }
    if let Some(memory) = repo_memory.map(str::trim).filter(|v| !v.is_empty()) {
        prompt.push_str("\nREPO MEMORY:\n");
        prompt.push_str(truncate_str(memory, REVIEW_AGENT_MEMORY_MAX_CHARS));
        prompt.push('\n');
    }
    if let Some(feedback) = retry_feedback.map(str::trim).filter(|v| !v.is_empty()) {
        prompt.push_str("\nRETRY FEEDBACK:\n");
        prompt.push_str(truncate_str(
            feedback,
            REVIEW_AGENT_RETRY_FEEDBACK_MAX_CHARS,
        ));
        prompt.push('\n');
    }

    prompt.push_str("\nRole: ");
    prompt.push_str(role);
    prompt
}

fn parse_agent_report(
    payload: &super::tools::ReportBackPayload,
) -> anyhow::Result<AgentReportEnvelopeJson> {
    validate_agent_report(payload.explanation.clone())
}

fn validate_agent_report(
    mut parsed: AgentReportEnvelopeJson,
) -> anyhow::Result<AgentReportEnvelopeJson> {
    let role = parsed.role.trim().to_ascii_lowercase();
    if role != "bug_hunter" && role != "security_reviewer" {
        return Err(anyhow::anyhow!(
            "Agent report role must be bug_hunter or security_reviewer (got '{}')",
            parsed.role
        ));
    }
    if !parsed.verified_findings.is_empty() {
        parsed.findings.append(&mut parsed.verified_findings);
    }
    Ok(parsed)
}

fn summarize_agentic_trace(trace: &AgenticTrace) -> String {
    if trace.steps.is_empty() {
        let termination_reason = trace.termination_reason.as_deref().unwrap_or("unknown");
        return format!(
            "steps=0 termination_reason={} repeated_tool_errors={} invalid_report_back={}",
            termination_reason, trace.repeated_tool_error_count, trace.invalid_report_back_count
        );
    }

    let tool_call_count: usize = trace
        .steps
        .iter()
        .map(|step| step.tool_call_names.len())
        .sum();
    let report_back_iteration = trace
        .steps
        .iter()
        .find(|step| step.report_back_called)
        .map(|step| step.iteration)
        .unwrap_or(0);
    let termination_reason = trace.termination_reason.as_deref().unwrap_or("unknown");
    let rationale_preview = trace
        .steps
        .iter()
        .rev()
        .find_map(|step| {
            step.reasoning_preview
                .as_ref()
                .or(step.assistant_content_preview.as_ref())
        })
        .map(|text| truncate_str(text, 96).to_string())
        .unwrap_or_default();

    if rationale_preview.is_empty() {
        format!(
            "steps={} tool_calls={} report_back_iter={} termination_reason={} repeated_tool_errors={} invalid_report_back={}",
            trace.steps.len(),
            tool_call_count,
            report_back_iteration,
            termination_reason,
            trace.repeated_tool_error_count,
            trace.invalid_report_back_count
        )
    } else {
        format!(
            "steps={} tool_calls={} report_back_iter={} termination_reason={} repeated_tool_errors={} invalid_report_back={} rationale={}",
            trace.steps.len(),
            tool_call_count,
            report_back_iteration,
            termination_reason,
            trace.repeated_tool_error_count,
            trace.invalid_report_back_count,
            rationale_preview
        )
    }
}

fn classify_worker_failure(error_text: &str) -> &'static str {
    let lower = error_text.to_ascii_lowercase();
    if lower.contains("termination_reason=tool_error_loop") || lower.contains("tool_error_loop") {
        "tool_error_loop"
    } else if lower.contains("termination_reason=timeout") || lower.contains("timed out") {
        "timeout"
    } else if lower.contains("termination_reason=invalid_report_back_exhausted")
        || lower.contains("invalid report_back")
    {
        "invalid_report_back"
    } else if lower.contains("termination_reason=finalization_non_report_back") {
        "finalization_non_report_back"
    } else {
        "other"
    }
}

fn trace_response_preview(trace: &AgenticTrace) -> Option<String> {
    let preview = trace.steps.iter().rev().find_map(|step| {
        step.reasoning_preview
            .as_ref()
            .or(step.assistant_content_preview.as_ref())
    })?;
    Some(truncate_str(preview, 72).to_string())
}

fn review_agent_timeout_ms() -> Option<u64> {
    // Keep a bounded default so bug/security review workers cannot run forever.
    // Set COSMOS_DUAL_WORKER_TIMEOUT_MS=0 to opt into unbounded workers.
    let timeout_ms = std::env::var("COSMOS_DUAL_WORKER_TIMEOUT_MS")
        .ok()
        .and_then(|value| value.trim().parse::<u64>().ok())
        .unwrap_or(DEFAULT_REVIEW_AGENT_TIMEOUT_MS);
    if timeout_ms == 0 {
        None
    } else {
        Some(timeout_ms)
    }
}

fn review_agent_iteration_budget() -> usize {
    // Default to a bounded but generous exploration budget to avoid runaway TPM spikes.
    // Set COSMOS_DUAL_WORKER_MAX_ITERATIONS=0 to explicitly allow unbounded loops.
    std::env::var("COSMOS_DUAL_WORKER_MAX_ITERATIONS")
        .ok()
        .and_then(|value| value.trim().parse::<usize>().ok())
        .unwrap_or(DEFAULT_REVIEW_AGENT_MAX_ITERATIONS)
}

fn role_config_for_focus(review_focus: SuggestionReviewFocus) -> (&'static str, &'static str) {
    match review_focus {
        SuggestionReviewFocus::BugHunt => ("bug_hunter", RELACE_BUG_HUNTER_SYSTEM),
        SuggestionReviewFocus::SecurityReview => {
            ("security_reviewer", RELACE_SECURITY_REVIEWER_SYSTEM)
        }
    }
}

pub async fn analyze_codebase_single_agent_reviewed(
    repo_root: &Path,
    index: &CodebaseIndex,
    _context: &WorkContext,
    repo_memory: Option<String>,
    review_focus: SuggestionReviewFocus,
    attempt_index: usize,
    retry_feedback: Option<&str>,
    stream_sink: Option<SuggestionStreamSink>,
) -> anyhow::Result<(Vec<Suggestion>, Option<Usage>, SuggestionDiagnostics)> {
    let run_id = Uuid::new_v4().to_string();
    let project_ethos = load_project_ethos(repo_root);
    // Keep one autonomous worker per mode so the selected role explores freely.
    let iteration_budget = review_agent_iteration_budget();
    let review_timeout_ms = review_agent_timeout_ms();

    let (review_role, review_system_prompt) = role_config_for_focus(review_focus);
    let prompt = build_review_agent_user_prompt(
        review_role,
        project_ethos.as_deref(),
        repo_memory.as_deref(),
        retry_feedback,
    );
    let planned_worker_jobs = 1usize;

    let started = std::time::Instant::now();
    let worker_stream_sink = stream_sink.as_ref().map(|sink| {
        let sink = Arc::clone(sink);
        let worker = format!("{}#1", review_role);
        Arc::new(move |event: AgenticStreamEvent| {
            sink(worker.clone(), event.kind, event.line);
        }) as AgenticStreamSink
    });
    let worker_result = if let Some(timeout_ms) = review_timeout_ms {
        tokio::time::timeout(
            std::time::Duration::from_millis(timeout_ms),
            call_llm_agentic_report_back_only(
                review_system_prompt,
                &prompt,
                Model::Speed,
                repo_root,
                iteration_budget,
                worker_stream_sink,
            ),
        )
        .await
        .map_err(|_| anyhow::anyhow!("worker timed out after {}ms", timeout_ms))
        .and_then(|inner| inner)
    } else {
        call_llm_agentic_report_back_only(
            review_system_prompt,
            &prompt,
            Model::Speed,
            repo_root,
            iteration_budget,
            worker_stream_sink,
        )
        .await
    };
    let elapsed_ms = started.elapsed().as_millis() as u64;

    let mut usage: Option<Usage> = None;
    let mut merged_findings = Vec::new();
    let mut bug_findings_count = 0usize;
    let mut security_findings_count = 0usize;
    let mut worker_success_count = 0usize;
    let mut worker_failures = Vec::new();
    let mut worker_trace_notes = Vec::new();
    let mut response_preview_parts = Vec::new();
    let mut worker_failure_timeout_count = 0usize;
    let mut worker_failure_tool_error_loop_count = 0usize;
    let mut worker_failure_invalid_report_back_count = 0usize;
    let mut worker_failure_other_count = 0usize;
    let worker_label = format!("{}#1", review_role);
    match worker_result {
        Ok(agent_result) => {
            usage = merge_usage(usage, agent_result.usage);
            let trace_summary = summarize_agentic_trace(&agent_result.trace);
            worker_trace_notes.push(format!("{} trace: {}", worker_label, trace_summary));
            let tool_call_count: usize = agent_result
                .trace
                .steps
                .iter()
                .map(|step| step.tool_call_names.len())
                .sum();
            let report_back_iteration = agent_result
                .trace
                .steps
                .iter()
                .find(|step| step.report_back_called)
                .map(|step| step.iteration)
                .unwrap_or(0);
            let termination_reason = agent_result
                .trace
                .termination_reason
                .as_deref()
                .unwrap_or("unknown");
            worker_trace_notes.push(format!(
                "worker_summary:role/batch={}#1 termination_reason={} tool_calls={} report_back_iter={} repeated_tool_errors={} invalid_report_back={}",
                review_role,
                termination_reason,
                tool_call_count,
                report_back_iteration,
                agent_result.trace.repeated_tool_error_count,
                agent_result.trace.invalid_report_back_count
            ));
            if let Some(preview) = trace_response_preview(&agent_result.trace) {
                response_preview_parts.push(format!("{}:{}", worker_label, preview));
            }
            match parse_agent_report(&agent_result.report_back) {
                Ok(parsed) => {
                    worker_success_count += 1;
                    let finding_count = parsed.findings.len();
                    if review_role == "bug_hunter" {
                        bug_findings_count = bug_findings_count.saturating_add(finding_count);
                    } else {
                        security_findings_count =
                            security_findings_count.saturating_add(finding_count);
                    }
                    merged_findings.extend(parsed.findings);
                }
                Err(err) => {
                    worker_failures.push(format!(
                        "{} parse_failed: {}",
                        worker_label,
                        truncate_str(&err.to_string(), 160)
                    ));
                }
            }
        }
        Err(err) => {
            let err_text = err.to_string();
            let failure_kind = classify_worker_failure(&err_text);
            worker_trace_notes.push(format!(
                "worker_summary:role/batch={}#1 termination_reason={} tool_calls=0 report_back_iter=0 repeated_tool_errors=0 invalid_report_back=0",
                review_role,
                failure_kind
            ));
            match classify_worker_failure(&err_text) {
                "tool_error_loop" => {
                    worker_failure_tool_error_loop_count =
                        worker_failure_tool_error_loop_count.saturating_add(1)
                }
                "timeout" => {
                    worker_failure_timeout_count = worker_failure_timeout_count.saturating_add(1)
                }
                "invalid_report_back" => {
                    worker_failure_invalid_report_back_count =
                        worker_failure_invalid_report_back_count.saturating_add(1)
                }
                _ => worker_failure_other_count = worker_failure_other_count.saturating_add(1),
            }
            worker_failures.push(format!(
                "{} call_failed({}): {}",
                worker_label,
                failure_kind,
                truncate_str(&err_text, 160)
            ));
        }
    }

    if worker_success_count == 0 {
        let reason = worker_failures
            .first()
            .cloned()
            .unwrap_or_else(|| "worker did not return a valid report".to_string());
        return Err(anyhow::anyhow!("Suggestion worker failed: {}", reason));
    }

    let suggestions = map_report_findings_to_suggestions(repo_root, index, merged_findings);
    let response_preview = truncate_str(&response_preview_parts.join(" | "), 240).to_string();
    let response_chars = response_preview_parts
        .iter()
        .map(|part| part.chars().count())
        .sum();

    let mut notes = vec![
        format!("attempt_index:{}", attempt_index),
        format!("review_focus:{}", review_focus.as_str()),
        format!("single_agent_ms:{}", elapsed_ms),
        format!("single_agent_total:{}", planned_worker_jobs),
        format!("single_agent_success:{}", worker_success_count),
        format!("single_agent_failures:{}", worker_failures.len()),
        format!(
            "single_agent_failures_tool_error_loop:{}",
            worker_failure_tool_error_loop_count
        ),
        format!(
            "single_agent_failures_timeout:{}",
            worker_failure_timeout_count
        ),
        format!(
            "single_agent_failures_invalid_report_back:{}",
            worker_failure_invalid_report_back_count
        ),
        format!("single_agent_failures_other:{}", worker_failure_other_count),
        format!(
            "single_agent_timeout_ms:{}",
            review_timeout_ms
                .map(|value| value.to_string())
                .unwrap_or_else(|| "unbounded".to_string())
        ),
        format!(
            "iteration_budget:{}",
            if iteration_budget == 0 {
                "unbounded".to_string()
            } else {
                iteration_budget.to_string()
            }
        ),
        format!("bug_findings_reported:{}", bug_findings_count),
        format!("security_findings_reported:{}", security_findings_count),
    ];
    notes.extend(worker_trace_notes);
    notes.extend(worker_failures);

    let diagnostics = SuggestionDiagnostics {
        run_id,
        model: Model::Speed.id().to_string(),
        iterations: 1,
        tool_calls: 0,
        tool_names: vec![review_role.to_string()],
        tool_exec_ms: elapsed_ms,
        llm_ms: elapsed_ms,
        batch_verify_ms: 0,
        forced_final: false,
        formatting_pass: false,
        response_format: false,
        response_healing: true,
        parse_strategy: "single_agent_direct_report".to_string(),
        parse_stripped_markdown: false,
        parse_used_sanitized_fix: false,
        parse_used_json_fix: false,
        parse_used_individual_parse: false,
        raw_count: bug_findings_count.saturating_add(security_findings_count),
        deduped_count: suggestions.len(),
        grounding_filtered: 0,
        low_confidence_filtered: 0,
        batch_verify_attempted: 0,
        batch_verify_verified: 0,
        batch_verify_not_found: 0,
        batch_verify_errors: 0,
        truncated_count: 0,
        final_count: suggestions.len(),
        response_chars,
        response_preview,
        evidence_pack_ms: 0,
        sent_snippet_count: 0,
        sent_bytes: 0,
        pack_pattern_count: 0,
        pack_hotspot_count: 0,
        pack_core_count: 0,
        pack_line1_ratio: 0.0,
        provisional_count: suggestions.len(),
        generation_waves: 1,
        generation_topup_calls: 0,
        generation_mapped_count: suggestions.len(),
        validated_count: suggestions.len(),
        rejected_count: 0,
        rejected_evidence_skipped_count: 0,
        validation_rejection_histogram: HashMap::new(),
        validation_deadline_exceeded: false,
        validation_deadline_ms: 0,
        batch_missing_index_count: 0,
        batch_no_reason_count: 0,
        transport_retry_count: 0,
        transport_recovered_count: 0,
        rewrite_recovered_count: 0,
        prevalidation_contradiction_count: 0,
        validation_transport_retry_count: 0,
        validation_transport_recovered_count: 0,
        regen_stopped_validation_budget: false,
        attempt_index: 1,
        attempt_count: 1,
        gate_passed: true,
        gate_fail_reasons: Vec::new(),
        attempt_cost_usd: usage.as_ref().map(|u| u.cost()).unwrap_or(0.0),
        attempt_ms: elapsed_ms,
        overclaim_rewrite_count: 0,
        overclaim_rewrite_validated_count: 0,
        smart_rewrite_count: 0,
        deterministic_auto_validated_count: 0,
        semantic_dedup_dropped_count: 0,
        file_balance_dropped_count: 0,
        speculative_impact_dropped_count: 0,
        dominant_topic_ratio: 0.0,
        unique_topic_count: 0,
        dominant_file_ratio: 0.0,
        unique_file_count: 0,
        readiness_filtered_count: 0,
        readiness_score_mean: 0.0,
        regeneration_attempts: 0,
        refinement_complete: true,
        notes,
    };

    Ok((suggestions, usage, diagnostics))
}

fn ensure_non_summary_model(model: Model, operation: &str) -> anyhow::Result<()> {
    if model == Model::Speed {
        return Err(anyhow::anyhow!(
            "{} must not use {} (speed tier is not allowed for this workflow)",
            operation,
            model.id()
        ));
    }
    Ok(())
}

pub async fn run_fast_grounded_with_gate(
    repo_root: &Path,
    index: &CodebaseIndex,
    context: &WorkContext,
    repo_memory: Option<String>,
    gate_config: SuggestionQualityGateConfig,
) -> anyhow::Result<GatedSuggestionRunResult> {
    run_fast_grounded_with_gate_with_progress(
        repo_root,
        index,
        context,
        repo_memory,
        gate_config,
        |_, _, _, _| {},
    )
    .await
}

pub async fn run_fast_grounded_with_gate_with_progress<F>(
    repo_root: &Path,
    index: &CodebaseIndex,
    context: &WorkContext,
    repo_memory: Option<String>,
    gate_config: SuggestionQualityGateConfig,
    on_progress: F,
) -> anyhow::Result<GatedSuggestionRunResult>
where
    F: FnMut(usize, usize, &SuggestionGateSnapshot, &SuggestionDiagnostics),
{
    run_fast_grounded_with_gate_with_progress_and_stream(
        repo_root,
        index,
        context,
        repo_memory,
        gate_config,
        None,
        on_progress,
    )
    .await
}

pub async fn run_fast_grounded_with_gate_with_progress_and_stream<F>(
    repo_root: &Path,
    index: &CodebaseIndex,
    context: &WorkContext,
    repo_memory: Option<String>,
    gate_config: SuggestionQualityGateConfig,
    stream_sink: Option<SuggestionStreamSink>,
    mut on_progress: F,
) -> anyhow::Result<GatedSuggestionRunResult>
where
    F: FnMut(usize, usize, &SuggestionGateSnapshot, &SuggestionDiagnostics),
{
    let total_start = std::time::Instant::now();
    let attempt_count = bounded_suggestion_attempt_count(&gate_config);
    let deterministic_target_count = deterministic_soft_target_count(&gate_config);
    let mut aggregate_usage: Option<Usage> = None;
    let mut retry_feedback: Option<String> = None;
    let mut last_error: Option<String> = None;

    for attempt_index in 1..=attempt_count {
        let attempt_focus = review_focus_for_attempt(gate_config.review_focus, attempt_index);
        let elapsed_before_attempt_ms = total_start.elapsed().as_millis() as u64;
        if gate_config.max_suggest_ms > 0 && elapsed_before_attempt_ms >= gate_config.max_suggest_ms
        {
            last_error = Some(format!(
                "Suggestion generation timed out after {}ms",
                gate_config.max_suggest_ms
            ));
            break;
        }

        let remaining_budget_ms = if gate_config.max_suggest_ms == 0 {
            None
        } else {
            Some(
                gate_config
                    .max_suggest_ms
                    .saturating_sub(elapsed_before_attempt_ms),
            )
        };

        let analyze_result = if let Some(remaining_budget_ms) = remaining_budget_ms {
            tokio::time::timeout(
                std::time::Duration::from_millis(remaining_budget_ms.max(1)),
                analyze_codebase_single_agent_reviewed(
                    repo_root,
                    index,
                    context,
                    repo_memory.clone(),
                    attempt_focus,
                    attempt_index,
                    retry_feedback.as_deref(),
                    stream_sink.clone(),
                ),
            )
            .await
            .map_err(|_| {
                anyhow::anyhow!(
                    "Suggestion generation timed out after {}ms",
                    gate_config.max_suggest_ms
                )
            })
            .and_then(|result| result)
        } else {
            analyze_codebase_single_agent_reviewed(
                repo_root,
                index,
                context,
                repo_memory.clone(),
                attempt_focus,
                attempt_index,
                retry_feedback.as_deref(),
                stream_sink.clone(),
            )
            .await
        };

        let (provisional, attempt_usage, mut diagnostics) = match analyze_result {
            Ok(value) => value,
            Err(err) => {
                let err_text = format!(
                    "Suggestion generation failed: {}",
                    truncate_str(&err.to_string(), 220)
                );
                last_error = Some(err_text);
                if attempt_index < attempt_count {
                    retry_feedback = Some(format!(
                        "Attempt {} failed (focus: {}). Re-scan different areas and prioritize directly verifiable findings with concrete code evidence.",
                        attempt_index,
                        attempt_focus.as_str()
                    ));
                    continue;
                }
                break;
            }
        };

        aggregate_usage = merge_usage(aggregate_usage, attempt_usage.clone());
        let selection = deterministic_select_suggestions(
            &provisional,
            deterministic_target_count,
            gate_config.max_final_count,
        );
        let suggestions = selection.suggestions;

        diagnostics.refinement_complete = true;
        diagnostics.final_count = suggestions.len();
        diagnostics.validated_count = suggestions
            .iter()
            .filter(|suggestion| suggestion_is_verified_bug_or_security(suggestion))
            .count();
        diagnostics.rejected_count = provisional.len().saturating_sub(suggestions.len());
        diagnostics.semantic_dedup_dropped_count = selection.dedup_dropped_count;
        diagnostics.file_balance_dropped_count = selection.file_balance_dropped_count;
        diagnostics.speculative_impact_dropped_count = selection.speculative_dropped_count;
        diagnostics
            .notes
            .push(format!("single_pass_target:{}", deterministic_target_count));
        diagnostics
            .notes
            .push(format!("single_pass_selected:{}", suggestions.len()));
        diagnostics
            .notes
            .push(format!("attempt_review_focus:{}", attempt_focus.as_str()));

        let total_cost_usd = aggregate_usage.as_ref().map(|u| u.cost()).unwrap_or(0.0);
        let total_ms = total_start.elapsed().as_millis() as u64;
        let gate = build_gate_snapshot(&gate_config, &suggestions, total_ms, total_cost_usd);

        diagnostics.attempt_index = attempt_index;
        diagnostics.attempt_count = attempt_count;
        diagnostics.gate_passed = gate.passed;
        diagnostics.gate_fail_reasons = gate.fail_reasons.clone();
        diagnostics.attempt_cost_usd = attempt_usage.as_ref().map(|u| u.cost()).unwrap_or(0.0);
        diagnostics.regeneration_attempts = attempt_index.saturating_sub(1);

        on_progress(attempt_index, attempt_count, &gate, &diagnostics);

        if !suggestions.is_empty() {
            return Ok(GatedSuggestionRunResult {
                suggestions,
                usage: aggregate_usage,
                diagnostics,
                gate,
            });
        }

        if attempt_index < attempt_count {
            retry_feedback = Some(format!(
                "Attempt {} returned zero verified findings (focus: {}). Expand coverage and prioritize likely defect hotspots such as panic/error paths, bounds checks, auth boundaries, and unsafe input handling.",
                attempt_index,
                attempt_focus.as_str()
            ));
        }
    }

    if let Some(error) = last_error {
        return Err(anyhow::anyhow!("{}", error));
    }

    Err(anyhow::anyhow!(
        "No verified findings were produced after {} attempt(s).",
        attempt_count
    ))
}

#[cfg(test)]
mod tests;
