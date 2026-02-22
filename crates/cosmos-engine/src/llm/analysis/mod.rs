use super::agentic::{
    call_llm_agentic, call_llm_agentic_report_back_only, schema_to_response_format,
    AgenticStreamEvent, AgenticStreamSink, AgenticTrace,
};
use super::client::{call_llm_with_usage, truncate_str};
use super::models::merge_usage;
use super::models::{Model, Usage};
use super::prompt_utils::format_repo_memory_section;
use super::prompts::ask_question_system;
use chrono::Utc;
use cosmos_adapters::cache::Cache;
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
use summary_normalization::{normalize_grounded_detail, normalize_grounded_summary};

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
const ASK_ETHOS_MAX_CHARS: usize = 8_000;
const HYBRID_CANDIDATE_POOL_SIZE: usize = 60;
const HYBRID_CHURN_PERCENT: usize = 40;
const HYBRID_SECURITY_PERCENT: usize = 30;
const HYBRID_COMPLEXITY_PERCENT: usize = 20;
const HYBRID_DORMANT_PERCENT: usize = 10;
const CHURN_COMMIT_WINDOW: usize = 200;
const COVERAGE_CACHE_KEEP_LIMIT: usize = 4_000;

const RELACE_BUG_HUNTER_SYSTEM: &str = r#"You are a bug-hunting search agent.

Goals:
- Find concrete runtime defects.
- Prioritize high-value issues over quantity.
- Never fabricate claims.

When done, call report_back exactly once.
- If you find no verified issues, still call report_back with "findings": [] and "files": [].
- explanation MUST be a JSON object (not a quoted string) with this exact shape:
  {
    "role": "bug_hunter",
    "findings": [{
      "file": "relative/path",
      "line": 123,
      "category": "bug",
      "criticality": "critical|high|medium|low",
      "summary": "plain english impact",
      "detail": "root cause and fix direction",
      "evidence_quote": "exact code excerpt"
    }],
    "verified_findings": []
  }
- files must include supporting ranges for each reported finding."#;

const RELACE_SECURITY_REVIEWER_SYSTEM: &str = r#"You are a security-review search agent.

Goals:
- Find concrete security vulnerabilities.
- Prioritize high-value issues over quantity.
- Never fabricate claims.

When done, call report_back exactly once.
- If you find no verified issues, still call report_back with "findings": [] and "files": [].
- explanation MUST be a JSON object (not a quoted string) with this exact shape:
  {
    "role": "security_reviewer",
    "findings": [{
      "file": "relative/path",
      "line": 123,
      "category": "security",
      "criticality": "critical|high|medium|low",
      "summary": "plain english impact",
      "detail": "root cause and fix direction",
      "evidence_quote": "exact code excerpt"
    }],
    "verified_findings": []
  }
- files must include supporting ranges for each reported finding."#;

const AGENTIC_SUGGESTIONS_SYSTEM: &str = r#"You are Cosmos, a senior code reviewer.

Goal: find VERIFIED bugs and VERIFIED security flaws only.
- Use tools to inspect the codebase directly.
- Prioritize concrete runtime defects and security vulnerabilities.
- Do not invent facts.
- Return only VERIFIED claims.
- A claim is VERIFIED only if you inspected the relevant code and can quote exact supporting code text.
- If you cannot verify a claim from code, do not include it.
- Follow project ETHOS when provided.
- Use plain language. Avoid file paths, symbols, or implementation jargon in summaries.
- Keep every suggestion actionable: the detail should explain root cause and what to change.
- Output ONLY bug/security findings. No refactors, style advice, optimizations, documentation, or quality nits.

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
}"#;

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

fn security_signal_score(path: &Path, file: &cosmos_core::index::FileIndex) -> usize {
    let lower = path
        .to_string_lossy()
        .replace('\\', "/")
        .to_ascii_lowercase();
    let signal_terms = [
        "auth",
        "security",
        "token",
        "secret",
        "password",
        "session",
        "jwt",
        "oauth",
        "crypto",
        "permission",
        "policy",
        "sanitize",
        "csrf",
        "xss",
        "sql",
        "encrypt",
        "decrypt",
        "key",
    ];

    let mut score = 0usize;
    for term in signal_terms {
        if lower.contains(term) {
            score += 20;
        }
    }

    for symbol in &file.symbols {
        let symbol_lower = symbol.name.to_ascii_lowercase();
        if signal_terms.iter().any(|term| symbol_lower.contains(term)) {
            score += 6;
        }
    }

    if lower.contains("middleware") || lower.contains("guard") {
        score += 8;
    }
    score
}

fn build_hybrid_candidate_pool(
    repo_root: &Path,
    index: &CodebaseIndex,
    context: &WorkContext,
) -> Vec<PathBuf> {
    let cache = Cache::new(repo_root);
    let mut coverage = cache.load_suggestion_coverage_cache().unwrap_or_default();
    let _ = coverage.normalize_paths(repo_root);

    let all_paths = index
        .files
        .keys()
        .filter(|path| !is_test_like_path(path))
        .cloned()
        .collect::<Vec<_>>();

    let changed: HashSet<PathBuf> = context.all_changed_files().into_iter().cloned().collect();
    let churn_counts = churn_counts_from_git(repo_root, CHURN_COMMIT_WINDOW);

    let mut churn_ranked = all_paths
        .iter()
        .map(|path| {
            let churn = churn_counts.get(path).copied().unwrap_or(0);
            let changed_boost = if changed.contains(path) { 24 } else { 0 };
            let score = churn.saturating_mul(10).saturating_add(changed_boost);
            (score, path.clone())
        })
        .collect::<Vec<_>>();
    churn_ranked.sort_by(|left, right| right.0.cmp(&left.0).then_with(|| left.1.cmp(&right.1)));

    let mut security_ranked = all_paths
        .iter()
        .filter_map(|path| {
            index
                .files
                .get(path)
                .map(|file| (security_signal_score(path, file), path.clone()))
        })
        .collect::<Vec<_>>();
    security_ranked.sort_by(|left, right| right.0.cmp(&left.0).then_with(|| left.1.cmp(&right.1)));

    let mut complexity_ranked = all_paths
        .iter()
        .filter_map(|path| {
            index.files.get(path).map(|file| {
                let complexity = if file.complexity.is_finite() {
                    file.complexity.max(0.0)
                } else {
                    0.0
                };
                (complexity, file.loc, path.clone())
            })
        })
        .collect::<Vec<_>>();
    complexity_ranked.sort_by(|left, right| {
        right
            .0
            .partial_cmp(&left.0)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| right.1.cmp(&left.1))
            .then_with(|| left.2.cmp(&right.2))
    });

    let mut dormant_ranked = all_paths
        .iter()
        .map(|path| (coverage.scanned_at(path), path.clone()))
        .collect::<Vec<_>>();
    dormant_ranked.sort_by(|left, right| match (left.0, right.0) {
        (None, None) => left.1.cmp(&right.1),
        (None, Some(_)) => std::cmp::Ordering::Less,
        (Some(_), None) => std::cmp::Ordering::Greater,
        (Some(left_ts), Some(right_ts)) => {
            left_ts.cmp(&right_ts).then_with(|| left.1.cmp(&right.1))
        }
    });

    let churn_quota = HYBRID_CANDIDATE_POOL_SIZE * HYBRID_CHURN_PERCENT / 100;
    let security_quota = HYBRID_CANDIDATE_POOL_SIZE * HYBRID_SECURITY_PERCENT / 100;
    let complexity_quota = HYBRID_CANDIDATE_POOL_SIZE * HYBRID_COMPLEXITY_PERCENT / 100;
    let dormant_quota = HYBRID_CANDIDATE_POOL_SIZE * HYBRID_DORMANT_PERCENT / 100;

    let mut selected = Vec::with_capacity(HYBRID_CANDIDATE_POOL_SIZE);
    let mut seen = HashSet::new();

    let mut take_unique_paths = |paths: Vec<PathBuf>, quota: usize| {
        let mut added = 0usize;
        for path in paths {
            if added >= quota {
                break;
            }
            if seen.insert(path.clone()) {
                selected.push(path);
                added += 1;
            }
        }
    };

    take_unique_paths(
        churn_ranked
            .iter()
            .map(|(_, path)| path.clone())
            .collect::<Vec<_>>(),
        churn_quota,
    );
    take_unique_paths(
        security_ranked
            .iter()
            .map(|(_, path)| path.clone())
            .collect::<Vec<_>>(),
        security_quota,
    );
    take_unique_paths(
        complexity_ranked
            .iter()
            .map(|(_, _, path)| path.clone())
            .collect::<Vec<_>>(),
        complexity_quota,
    );
    take_unique_paths(
        dormant_ranked
            .iter()
            .map(|(_, path)| path.clone())
            .collect::<Vec<_>>(),
        dormant_quota,
    );

    if selected.len() < HYBRID_CANDIDATE_POOL_SIZE {
        let mut fallback = all_paths
            .iter()
            .map(|path| {
                let churn = churn_counts.get(path).copied().unwrap_or(0);
                let security = index
                    .files
                    .get(path)
                    .map(|file| security_signal_score(path, file))
                    .unwrap_or(0);
                let complexity = index
                    .files
                    .get(path)
                    .map(|file| {
                        if file.complexity.is_finite() {
                            file.complexity
                        } else {
                            0.0
                        }
                    })
                    .unwrap_or(0.0);
                let dormant = coverage
                    .scanned_at(path)
                    .map(|ts| (Utc::now() - ts).num_days().max(0) as usize)
                    .unwrap_or(365);
                let score = churn
                    .saturating_mul(8)
                    .saturating_add(security.saturating_mul(3))
                    .saturating_add((complexity.max(0.0) as usize).min(30))
                    .saturating_add(dormant.min(30));
                (score, path.clone())
            })
            .collect::<Vec<_>>();
        fallback.sort_by(|left, right| right.0.cmp(&left.0).then_with(|| left.1.cmp(&right.1)));
        for (_, path) in fallback {
            if selected.len() >= HYBRID_CANDIDATE_POOL_SIZE {
                break;
            }
            if seen.insert(path.clone()) {
                selected.push(path);
            }
        }
    }

    if selected.len() > HYBRID_CANDIDATE_POOL_SIZE {
        selected.truncate(HYBRID_CANDIDATE_POOL_SIZE);
    }

    coverage.record_scan(selected.clone());
    coverage.prune(COVERAGE_CACHE_KEEP_LIMIT);
    let _ = cache.save_suggestion_coverage_cache(&coverage);

    selected
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
        "You are subagent {}/{}.\nInspect the assigned high-churn files first.\nReturn {} to {} VERIFIED findings total.\n\
Each finding must be either a bugfix or a security flaw.\n\
Target mix: 1-2 bug findings and 1-2 security findings.\n\
If the assigned scope has fewer verified issues, return fewer and do not invent claims.\n\
Every finding must include an exact evidence_quote copied from code you inspected.",
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
- Only include runtime defects and security vulnerabilities.\n\
- Exclude refactors, style nits, architecture proposals, and pure performance tuning.\n\
- summary: one plain-language sentence about visible impact.\n\
- detail: root cause and concrete fix direction.\n\
- If uncertain, omit the claim.",
    );

    if let Some(ethos) = project_ethos.map(str::trim).filter(|text| !text.is_empty()) {
        prompt.push_str("\n\nPROJECT ETHOS (must follow):\n");
        prompt.push_str(truncate_str(ethos, 2_000));
    }

    if let Some(feedback) = retry_feedback
        .map(str::trim)
        .filter(|text| !text.is_empty())
    {
        prompt.push_str("\n\nPrevious attempt feedback to correct:\n");
        prompt.push_str(feedback);
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
- {} files, {} lines of code
- {} components/features total
- Currently on: {}
- Key areas: {}

INTERNAL STRUCTURE (for your reference, don't mention these names directly):
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
}

impl Default for SuggestionQualityGateConfig {
    fn default() -> Self {
        Self {
            min_final_count: 1,
            max_final_count: 12,
            min_displayed_valid_ratio: 1.0,
            min_implementation_readiness_score: DEFAULT_MIN_IMPLEMENTATION_READINESS_SCORE,
            max_smart_rewrites_per_run: DEFAULT_MAX_SMART_REWRITES_PER_RUN,
            max_suggest_cost_usd: 0.20,
            max_suggest_ms: 180_000,
            max_attempts: 4,
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

fn is_retryable_generation_error(error: &anyhow::Error) -> bool {
    let message = error.to_string().to_ascii_lowercase();
    message.contains("empty response")
        || message.contains("timed out")
        || message.contains("rate limited")
        || message.contains("too many requests")
        || message.contains("server error")
        || message.contains("503")
        || message.contains("502")
        || message.contains("504")
        || message.contains("failed to parse agentic suggestions json")
        || message.contains("structured output")
        || message.contains("invalid report_back payload")
        || message.contains("invalid agent explanation json")
        || message.contains("invalid reviewer explanation json")
        || message.contains("tool call validation failed")
        || message.contains("did not call report_back within iteration/time budget")
        || message.contains("did not call report_back")
        || message.contains("text instead of calling report_back")
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
        let summary = normalize_grounded_summary(
            if claim_summary.is_empty() {
                summary_seed
            } else {
                claim_summary.as_str()
            },
            &detail,
            line,
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
        let suggestion = Suggestion::new(
            kind,
            priority,
            file.clone(),
            summary,
            cosmos_core::suggest::SuggestionSource::LlmDeep,
        )
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
        out.push(suggestion);
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

        let raw_summary = if finding.summary.trim().is_empty() {
            finding.detail.as_str()
        } else {
            finding.summary.as_str()
        };
        let mut summary = normalize_grounded_summary(raw_summary, &finding.detail, line);
        if summary.is_empty() {
            summary = match category {
                SuggestionCategory::Security => "Potential security issue detected.".to_string(),
                SuggestionCategory::Bug => "Potential runtime bug detected.".to_string(),
            };
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

        let impact_class = match category {
            SuggestionCategory::Bug => "correctness".to_string(),
            SuggestionCategory::Security => "security".to_string(),
        };

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
    if suggest_total_cost_usd > config.max_suggest_cost_usd {
        fail_reasons.push(format!(
            "suggest_cost_above_max:{:.4}>{:.4}",
            suggest_total_cost_usd, config.max_suggest_cost_usd
        ));
    }
    if suggest_total_ms > config.max_suggest_ms {
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

fn build_dual_agent_user_prompt(
    role: &str,
    candidate_files: &[PathBuf],
    project_ethos: Option<&str>,
    retry_feedback: Option<&str>,
) -> String {
    let mut prompt = String::from(
        "I have uploaded a code repository in the /repo directory.\n\n\
Focus on these candidate files first (you may inspect related files as needed):\n",
    );
    for path in candidate_files {
        prompt.push_str("- ");
        prompt.push_str(&path.display().to_string());
        prompt.push('\n');
    }

    prompt.push_str(
        "\nTargets:\n- Find high-value, concrete issues only.\n- Prefer quality over quantity.\n- Dynamic volume: return as many strictly supported findings as you can validate.\n- Never fabricate evidence.\n- Finish only via report_back.\n- Do not stop early: inspect assigned files first before concluding.\n- If no verified findings remain after inspection, call report_back with findings: [] and files: [].\n",
    );

    if role == "bug_hunter" {
        prompt.push_str(
            "\nBug-hunt checklist:\n- unchecked unwrap/expect/panic paths\n- missing error handling on fallible operations\n- edge-case logic errors (null/empty/bounds)\n- race/concurrency misuse\n",
        );
    } else if role == "security_reviewer" {
        prompt.push_str(
            "\nSecurity checklist:\n- authn/authz bypasses across trust boundaries\n- injection risks (sql/shell/template)\n- unsafe deserialization/parsing of untrusted input\n- secret/token leakage paths\n- path traversal/file permission misuse\n- prefer externally reachable vulnerabilities over local-only hardening concerns\n- environment-variable/config-only concerns are hardening unless cross-boundary exploitability is concrete\n",
        );
    }

    if let Some(ethos) = project_ethos.map(str::trim).filter(|v| !v.is_empty()) {
        prompt.push_str("\nPROJECT ETHOS:\n");
        prompt.push_str(truncate_str(ethos, 2_000));
        prompt.push('\n');
    }
    if let Some(feedback) = retry_feedback.map(str::trim).filter(|v| !v.is_empty()) {
        prompt.push_str("\nRETRY FEEDBACK:\n");
        prompt.push_str(feedback);
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

fn dual_role_worker_timeout_ms() -> u64 {
    const DEFAULT_TIMEOUT_MS: u64 = 120_000;
    const MIN_TIMEOUT_MS: u64 = 60_000;
    const MAX_TIMEOUT_MS: u64 = 300_000;

    std::env::var("COSMOS_DUAL_WORKER_TIMEOUT_MS")
        .ok()
        .and_then(|value| value.trim().parse::<u64>().ok())
        .map(|value| value.clamp(MIN_TIMEOUT_MS, MAX_TIMEOUT_MS))
        .unwrap_or(DEFAULT_TIMEOUT_MS)
}

pub async fn analyze_codebase_dual_agent_reviewed(
    repo_root: &Path,
    index: &CodebaseIndex,
    context: &WorkContext,
    _repo_memory: Option<String>,
    retry_feedback: Option<&str>,
    stream_sink: Option<SuggestionStreamSink>,
) -> anyhow::Result<(Vec<Suggestion>, Option<Usage>, SuggestionDiagnostics)> {
    let run_id = Uuid::new_v4().to_string();
    let project_ethos = load_project_ethos(repo_root);
    let candidate_files = build_hybrid_candidate_pool(repo_root, index, context);
    // Keep fanout intentionally low to reduce rate-limit pressure on Speed-tier runs.
    // Run one role per attempt (security first, bug on retry) to avoid concurrent contention.
    let dual_iteration_budget = 2usize;
    const DUAL_ROLE_WORKER_COUNT: usize = 1;
    const DUAL_ROLE_MAX_INFLIGHT: usize = 1;
    const DUAL_ROLE_WORKER_FILE_SPAN_MIN: usize = 2;
    const DUAL_ROLE_WORKER_FILE_SPAN_MAX: usize = 3;
    let dual_role_worker_timeout_ms = dual_role_worker_timeout_ms();
    let worker_file_span = if candidate_files.is_empty() {
        0
    } else {
        ((candidate_files.len() + DUAL_ROLE_WORKER_COUNT - 1) / DUAL_ROLE_WORKER_COUNT).clamp(
            DUAL_ROLE_WORKER_FILE_SPAN_MIN,
            DUAL_ROLE_WORKER_FILE_SPAN_MAX,
        )
    };

    let mut focus_batches = vec![Vec::new(); DUAL_ROLE_WORKER_COUNT];
    for (idx, path) in candidate_files.iter().enumerate() {
        focus_batches[idx % DUAL_ROLE_WORKER_COUNT].push(path.clone());
    }
    for worker_idx in 0..DUAL_ROLE_WORKER_COUNT {
        if focus_batches[worker_idx].is_empty() && !candidate_files.is_empty() {
            focus_batches[worker_idx]
                .push(candidate_files[worker_idx % candidate_files.len()].clone());
        }
        if candidate_files.is_empty() {
            continue;
        }
        let mut refill_idx = worker_idx;
        while focus_batches[worker_idx].len() < worker_file_span {
            focus_batches[worker_idx]
                .push(candidate_files[refill_idx % candidate_files.len()].clone());
            refill_idx = refill_idx.saturating_add(DUAL_ROLE_WORKER_COUNT);
        }
        if focus_batches[worker_idx].len() > worker_file_span {
            focus_batches[worker_idx].truncate(worker_file_span);
        }
    }

    let role_configs: Vec<(&str, &str)> = if retry_feedback.is_some() {
        vec![("bug_hunter", RELACE_BUG_HUNTER_SYSTEM)]
    } else {
        vec![("security_reviewer", RELACE_SECURITY_REVIEWER_SYSTEM)]
    };
    let mut jobs: Vec<(String, usize, Vec<PathBuf>, String, String)> =
        Vec::with_capacity(DUAL_ROLE_WORKER_COUNT * role_configs.len());
    for (batch_idx, files) in focus_batches.iter().enumerate() {
        for (role, system_prompt) in &role_configs {
            let prompt =
                build_dual_agent_user_prompt(role, files, project_ethos.as_deref(), retry_feedback);
            jobs.push((
                (*role).to_string(),
                batch_idx,
                files.clone(),
                (*system_prompt).to_string(),
                prompt,
            ));
        }
    }
    let planned_worker_jobs = jobs.len();

    let dual_start = std::time::Instant::now();
    let mut dual_results = Vec::with_capacity(jobs.len());
    for batch in jobs.chunks(DUAL_ROLE_MAX_INFLIGHT.max(1)) {
        let batch_results = join_all(batch.iter().cloned().map(
            |(role, batch_idx, files, system, prompt)| {
                let stream_sink = stream_sink.clone();
                async move {
                    let worker_stream_sink = stream_sink.as_ref().map(|sink| {
                        let sink = Arc::clone(sink);
                        let worker = format!("{}#{}", role, batch_idx + 1);
                        Arc::new(move |event: AgenticStreamEvent| {
                            sink(worker.clone(), event.kind, event.line);
                        }) as AgenticStreamSink
                    });
                    let result = tokio::time::timeout(
                        std::time::Duration::from_millis(dual_role_worker_timeout_ms),
                        call_llm_agentic_report_back_only(
                            &system,
                            &prompt,
                            Model::Speed,
                            repo_root,
                            dual_iteration_budget,
                            worker_stream_sink,
                        ),
                    )
                    .await
                    .map_err(|_| {
                        anyhow::anyhow!("worker timed out after {}ms", dual_role_worker_timeout_ms)
                    })
                    .and_then(|inner| inner);
                    (role, batch_idx, files, result)
                }
            },
        ))
        .await;
        dual_results.extend(batch_results);
    }
    let dual_elapsed_ms = dual_start.elapsed().as_millis() as u64;

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

    for (role, batch_idx, files, result) in dual_results {
        let batch_label = format!(
            "{}#{}({})",
            role,
            batch_idx + 1,
            files
                .iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>()
                .join(",")
        );
        match result {
            Ok(agent_result) => {
                usage = merge_usage(usage, agent_result.usage);
                let trace_summary = summarize_agentic_trace(&agent_result.trace);
                worker_trace_notes.push(format!("{} trace: {}", batch_label, trace_summary));
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
                    "worker_summary:role/batch={}#{} termination_reason={} tool_calls={} report_back_iter={} repeated_tool_errors={} invalid_report_back={}",
                    role,
                    batch_idx + 1,
                    termination_reason,
                    tool_call_count,
                    report_back_iteration,
                    agent_result.trace.repeated_tool_error_count,
                    agent_result.trace.invalid_report_back_count
                ));
                if let Some(preview) = trace_response_preview(&agent_result.trace) {
                    response_preview_parts.push(format!("{}:{}", batch_label, preview));
                }
                match parse_agent_report(&agent_result.report_back) {
                    Ok(parsed) => {
                        worker_success_count += 1;
                        let finding_count = parsed.findings.len();
                        if role == "bug_hunter" {
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
                            batch_label,
                            truncate_str(&err.to_string(), 160)
                        ));
                    }
                }
            }
            Err(err) => {
                let err_text = err.to_string();
                let failure_kind = classify_worker_failure(&err_text);
                worker_trace_notes.push(format!(
                    "worker_summary:role/batch={}#{} termination_reason={} tool_calls=0 report_back_iter=0 repeated_tool_errors=0 invalid_report_back=0",
                    role,
                    batch_idx + 1,
                    failure_kind
                ));
                match classify_worker_failure(&err_text) {
                    "tool_error_loop" => {
                        worker_failure_tool_error_loop_count =
                            worker_failure_tool_error_loop_count.saturating_add(1)
                    }
                    "timeout" => {
                        worker_failure_timeout_count =
                            worker_failure_timeout_count.saturating_add(1)
                    }
                    "invalid_report_back" => {
                        worker_failure_invalid_report_back_count =
                            worker_failure_invalid_report_back_count.saturating_add(1)
                    }
                    _ => worker_failure_other_count = worker_failure_other_count.saturating_add(1),
                }
                worker_failures.push(format!(
                    "{} call_failed({}): {}",
                    batch_label,
                    failure_kind,
                    truncate_str(&err_text, 160)
                ));
            }
        }
    }

    if worker_success_count == 0 {
        return Err(anyhow::anyhow!(
            "All suggestion workers failed. {}",
            worker_failures.join(" | ")
        ));
    }

    let suggestions = map_report_findings_to_suggestions(repo_root, index, merged_findings);
    let response_preview = truncate_str(&response_preview_parts.join(" | "), 240).to_string();
    let response_chars = response_preview_parts
        .iter()
        .map(|part| part.chars().count())
        .sum();

    let mut notes = vec![
        format!("candidate_pool_size:{}", candidate_files.len()),
        format!(
            "candidate_pool_preview:{}",
            candidate_files
                .iter()
                .take(8)
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>()
                .join(",")
        ),
        format!("dual_agent_ms:{}", dual_elapsed_ms),
        format!("dual_worker_total:{}", planned_worker_jobs),
        format!("dual_worker_max_inflight:{}", DUAL_ROLE_MAX_INFLIGHT),
        format!("dual_worker_file_span:{}", worker_file_span),
        format!("dual_worker_success:{}", worker_success_count),
        format!("dual_worker_failures:{}", worker_failures.len()),
        format!(
            "dual_worker_failures_tool_error_loop:{}",
            worker_failure_tool_error_loop_count
        ),
        format!(
            "dual_worker_failures_timeout:{}",
            worker_failure_timeout_count
        ),
        format!(
            "dual_worker_failures_invalid_report_back:{}",
            worker_failure_invalid_report_back_count
        ),
        format!("dual_worker_failures_other:{}", worker_failure_other_count),
        format!("dual_worker_timeout_ms:{}", dual_role_worker_timeout_ms),
        format!("dual_iteration_budget:{}", dual_iteration_budget),
        format!("bug_findings_reported:{}", bug_findings_count),
        format!("security_findings_reported:{}", security_findings_count),
    ];
    notes.extend(worker_trace_notes);
    notes.extend(worker_failures);

    let diagnostics = SuggestionDiagnostics {
        run_id,
        model: Model::Speed.id().to_string(),
        iterations: planned_worker_jobs,
        tool_calls: 0,
        tool_names: role_configs
            .iter()
            .map(|(role, _)| (*role).to_string())
            .collect(),
        tool_exec_ms: dual_elapsed_ms,
        llm_ms: dual_elapsed_ms,
        batch_verify_ms: 0,
        forced_final: false,
        formatting_pass: false,
        response_format: false,
        response_healing: true,
        parse_strategy: "dual_relace_agents_direct_report".to_string(),
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
        generation_waves: planned_worker_jobs,
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
        attempt_ms: dual_elapsed_ms,
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
    let attempt_count = gate_config.max_attempts.max(2);

    let mut merged_usage: Option<Usage> = None;
    let mut retry_feedback: Option<String> = None;
    let mut best_failed_result: Option<GatedSuggestionRunResult> = None;
    let mut last_error: Option<anyhow::Error> = None;
    let mut attempts_executed = 0usize;

    for attempt_index in 1..=attempt_count {
        attempts_executed = attempt_index;
        let elapsed_ms = total_start.elapsed().as_millis() as u64;
        if elapsed_ms >= gate_config.max_suggest_ms {
            break;
        }
        let remaining_budget_ms = gate_config.max_suggest_ms.saturating_sub(elapsed_ms).max(1);

        let analyze_result = tokio::time::timeout(
            std::time::Duration::from_millis(remaining_budget_ms),
            analyze_codebase_dual_agent_reviewed(
                repo_root,
                index,
                context,
                repo_memory.clone(),
                retry_feedback.as_deref(),
                stream_sink.clone(),
            ),
        )
        .await
        .map_err(|_| {
            anyhow::anyhow!(
                "Generation attempt timed out after {}ms",
                remaining_budget_ms
            )
        })
        .and_then(|result| result);
        let (provisional, usage_a, diagnostics) = match analyze_result {
            Ok(result) => result,
            Err(err) => {
                let retryable = is_retryable_generation_error(&err);
                let err_text = truncate_str(&err.to_string(), 200).to_string();
                last_error = Some(err);
                let elapsed_ms = total_start.elapsed().as_millis() as u64;
                let budget_exhausted = elapsed_ms > gate_config.max_suggest_ms;
                if !retryable || budget_exhausted || attempt_index >= attempt_count {
                    break;
                }
                retry_feedback = Some(format!(
                    "Previous generation attempt failed: {}. Recover with the same grounding bar and broader file coverage.",
                    err_text
                ));
                continue;
            }
        };
        let (suggestions, usage_b, mut diagnostics) = {
            let mut diagnostics = diagnostics;
            diagnostics.refinement_complete = true;
            diagnostics.final_count = provisional.len();
            diagnostics.validated_count = provisional
                .iter()
                .filter(|suggestion| suggestion_is_verified_bug_or_security(suggestion))
                .count();
            diagnostics.rejected_count = 0;
            (provisional, None, diagnostics)
        };

        merged_usage = merge_usage(merged_usage, merge_usage(usage_a, usage_b));
        let total_cost_usd = merged_usage.as_ref().map(|u| u.cost()).unwrap_or(0.0);
        let total_ms = total_start.elapsed().as_millis() as u64;
        let gate = build_gate_snapshot(&gate_config, &suggestions, total_ms, total_cost_usd);

        diagnostics.attempt_index = attempt_index;
        diagnostics.attempt_count = attempt_count;
        diagnostics.gate_passed = gate.passed;
        diagnostics.gate_fail_reasons = gate.fail_reasons.clone();
        diagnostics.attempt_cost_usd = total_cost_usd;
        diagnostics.attempt_ms = total_ms;
        diagnostics.final_count = suggestions.len();

        if suggestions.is_empty() {
            let note_preview = diagnostics
                .notes
                .iter()
                .take(10)
                .cloned()
                .collect::<Vec<_>>()
                .join("; ");
            last_error = Some(anyhow::anyhow!(
                "No findings were returned by bug/security agents (raw_reported={}, strategy={}, notes={})",
                diagnostics.raw_count,
                diagnostics.parse_strategy,
                note_preview
            ));
            let elapsed_ms = total_start.elapsed().as_millis() as u64;
            let budget_exhausted = elapsed_ms >= gate_config.max_suggest_ms;
            if budget_exhausted || attempt_index >= attempt_count {
                break;
            }
            retry_feedback = Some(
                "Previous attempt returned zero findings. Continue exploring and return any concrete bug/security issue with direct code evidence."
                    .to_string(),
            );
            continue;
        }

        on_progress(attempt_index, attempt_count, &gate, &diagnostics);
        let attempt_result = GatedSuggestionRunResult {
            suggestions,
            usage: merged_usage.clone(),
            diagnostics,
            gate,
        };
        if attempt_result.gate.passed {
            return Ok(attempt_result);
        }

        let gate_reasons = if attempt_result.gate.fail_reasons.is_empty() {
            "unknown gate failure".to_string()
        } else {
            attempt_result.gate.fail_reasons.join("; ")
        };
        let distinct_files = attempt_result
            .suggestions
            .iter()
            .map(|suggestion| suggestion.file.display().to_string())
            .collect::<HashSet<_>>()
            .into_iter()
            .take(6)
            .collect::<Vec<_>>()
            .join(", ");
        best_failed_result = Some(attempt_result);

        let elapsed_ms = total_start.elapsed().as_millis() as u64;
        let budget_exhausted = elapsed_ms >= gate_config.max_suggest_ms;
        if budget_exhausted || attempt_index >= attempt_count {
            break;
        }
        retry_feedback = Some(format!(
            "Previous attempt failed quality gate: {}. Return more distinct bug/security findings with direct evidence from different files when possible. Do not repeat prior findings unless you have materially stronger evidence. Prior files: {}",
            gate_reasons,
            if distinct_files.is_empty() {
                "(none)".to_string()
            } else {
                distinct_files
            }
        ));
        continue;
    }

    if let Some(result) = best_failed_result {
        return Ok(result);
    }

    if let Some(err) = last_error {
        return Err(anyhow::anyhow!(
            "Suggestion generation failed after {} attempt(s): {}",
            attempts_executed.max(1),
            err
        ));
    }

    Err(anyhow::anyhow!(
        "Suggestion generation did not produce usable output."
    ))
}

#[cfg(test)]
mod tests;
