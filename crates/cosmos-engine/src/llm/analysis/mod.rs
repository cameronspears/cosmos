use super::client::{
    call_llm_structured_limited, call_llm_structured_with_provider, call_llm_with_usage,
    truncate_str, StructuredResponse,
};
use super::models::merge_usage;
use super::models::{Model, Usage};
use super::prompt_utils::format_repo_memory_section;
use super::prompts::{ASK_QUESTION_SYSTEM, FAST_GROUNDED_SUGGESTIONS_SYSTEM};
use chrono::Utc;
use cosmos_core::context::WorkContext;
use cosmos_core::index::{
    CodebaseIndex, PatternKind, PatternReliability, PatternSeverity, SymbolKind,
};
use cosmos_core::suggest::{Suggestion, SuggestionEvidenceRef, SuggestionValidationState};
use futures::future::join_all;
use regex::Regex;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use uuid::Uuid;

mod context_limits;
mod summary_normalization;

use context_limits::AdaptiveLimits;
use summary_normalization::{
    normalize_grounded_detail, normalize_grounded_summary, scrub_user_summary,
};

// ═══════════════════════════════════════════════════════════════════════════
//  THRESHOLDS AND CONSTANTS
// ═══════════════════════════════════════════════════════════════════════════

use cosmos_core::index::GOD_MODULE_LOC_THRESHOLD;

/// Complexity threshold above which a file is considered a "hotspot"
const HIGH_COMPLEXITY_THRESHOLD: f64 = 20.0;
const FAST_EVIDENCE_PACK_MAX_ITEMS: usize = 60;
const FAST_EVIDENCE_SNIPPET_LINES_BEFORE: usize = 5;
const FAST_EVIDENCE_SNIPPET_LINES_AFTER: usize = 8;
const FAST_GROUNDED_FINAL_TARGET_MIN: usize = 10;
const FAST_GROUNDED_FINAL_TARGET_MAX: usize = 20;
const FAST_GROUNDED_VALIDATED_SOFT_FLOOR: usize = 10;
const FAST_GROUNDED_VALIDATED_HARD_TARGET: usize = 12;
const FAST_GROUNDED_VALIDATED_STRETCH_TARGET: usize = 20;
const FAST_GROUNDED_PROVISIONAL_TARGET_MIN: usize = 26;
const FAST_GROUNDED_PROVISIONAL_TARGET_MAX: usize = 40;
const FAST_EVIDENCE_SOURCE_PATTERN_MAX: usize = 24;
const FAST_EVIDENCE_SOURCE_HOTSPOT_MAX: usize = 20;
const FAST_EVIDENCE_SOURCE_CORE_MAX: usize = 16;
const FAST_EVIDENCE_KIND_GOD_MODULE_MAX: usize = 4;
const FAST_EVIDENCE_PER_FILE_MAX: usize = 3;
const FAST_EVIDENCE_ANCHORS_PER_FILE_MAX: usize = 3;
const FAST_EVIDENCE_CHANGED_FILE_MAX: usize = 10;
const FAST_EVIDENCE_NEIGHBOR_FILE_MAX: usize = 12;
const REFINEMENT_HARD_PHASE_MAX_ATTEMPTS: usize = 4;
const REFINEMENT_STRETCH_PHASE_MAX_ATTEMPTS: usize = 2;
const GENERATION_TOPUP_MAX_CALLS: usize = 4;
const GENERATION_TOPUP_TIMEOUT_MS: u64 = 4_500;
const REGEN_STRICT_MIN_PACK_SIZE: usize = FAST_GROUNDED_PROVISIONAL_TARGET_MIN;
const SUGGEST_BALANCED_BUDGET_MS: u64 = 60_000;
const SUGGEST_GATE_BUDGET_MS: u64 = 70_000;
const GATE_RETRY_MIN_REMAINING_BUDGET_MS: u64 = 8_000;
const GATE_RETRY_MAX_ATTEMPT_COST_FRACTION: f64 = 0.70;
const VALIDATION_CONCURRENCY: usize = 3;
const VALIDATION_RETRY_CONCURRENCY: usize = 1;
const PRIMARY_REQUEST_MIN: usize = 22;
const PRIMARY_REQUEST_MAX: usize = 30;
const PRIMARY_REQUEST_MAX_TOKENS: u32 = 1_800;
const PRIMARY_REQUEST_TIMEOUT_MS: u64 = 6_200;
const TOPUP_REQUEST_MAX_TOKENS: u32 = 1_000;
const REGEN_REQUEST_MAX_TOKENS: u32 = 800;
const VALIDATOR_MAX_TOKENS: u32 = 90;
const VALIDATOR_TIMEOUT_MS: u64 = 4_500;
const VALIDATOR_RETRY_TIMEOUT_MS: u64 = 3_200;
const VALIDATOR_BATCH_MAX_TOKENS: u32 = 320;
const VALIDATOR_BATCH_TIMEOUT_BUFFER_MS: u64 = 1_600;
const VALIDATION_RETRY_MAX_PER_SUGGESTION: usize = 1;
const VALIDATION_RETRY_MIN_REMAINING_BUDGET_MS: u64 = 4_000;
const VALIDATION_RUN_DEADLINE_MS: u64 = 30_000;
const VALIDATION_MIN_REMAINING_BUDGET_MS: u64 = 2_500;
const OVERCLAIM_REWRITE_MAX_TOKENS: u32 = 70;
const OVERCLAIM_REWRITE_TIMEOUT_MS: u64 = 2_000;
const OVERCLAIM_REVALIDATE_MAX_TOKENS: u32 = 70;
const OVERCLAIM_REVALIDATE_TIMEOUT_MS: u64 = 2_000;
const SMART_BORDERLINE_REWRITE_MAX_TOKENS: u32 = 90;
const SMART_BORDERLINE_REWRITE_TIMEOUT_MS: u64 = 2_600;
const STRETCH_PHASE_MAX_COST_USD: f64 = 0.012;
const STRETCH_PHASE_MIN_REMAINING_VALIDATION_MS: u64 = 6_000;
const SUMMARY_MIN_WORDS: usize = 5;
const SUMMARY_MIN_CHARS: usize = 24;
const DIVERSITY_DOMINANT_TOPIC_RATIO_MAX: f64 = 0.60;
const DIVERSITY_MIN_UNIQUE_TOPICS: usize = 4;
const DIVERSITY_DOMINANT_FILE_RATIO_MAX: f64 = 0.60;
const DIVERSITY_MIN_UNIQUE_FILES: usize = 4;
const DIVERSITY_FILE_BALANCE_PER_FILE_CAP: usize = 3;
const DEFAULT_MIN_IMPLEMENTATION_READINESS_SCORE: f32 = 0.45;
const DEFAULT_MAX_SMART_REWRITES_PER_RUN: usize = 8;
const SMART_REWRITE_READINESS_UPPER_BOUND: f32 = 0.60;

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
    let limits = AdaptiveLimits::for_codebase(stats.file_count, stats.total_loc);

    let file_list: Vec<_> = index
        .files
        .keys()
        .take(limits.file_list_limit)
        .map(|p| p.display().to_string())
        .collect();

    // Get symbols for context (used internally, not exposed to user)
    let symbols: Vec<_> = index
        .files
        .values()
        .flat_map(|f| f.symbols.iter())
        .filter(|s| {
            matches!(
                s.kind,
                SymbolKind::Function | SymbolKind::Struct | SymbolKind::Enum
            )
        })
        .take(limits.symbol_limit)
        .map(|s| format!("{:?}: {}", s.kind, s.name))
        .collect();

    let memory_section = format_repo_memory_section(repo_memory.as_deref(), "PROJECT NOTES");

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

    let response = call_llm_with_usage(ASK_QUESTION_SYSTEM, &user, Model::Smart, false).await?;
    Ok((response.content, response.usage))
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
            min_final_count: 10,
            max_final_count: 20,
            min_displayed_valid_ratio: 1.0,
            min_implementation_readiness_score: DEFAULT_MIN_IMPLEMENTATION_READINESS_SCORE,
            max_smart_rewrites_per_run: DEFAULT_MAX_SMART_REWRITES_PER_RUN,
            max_suggest_cost_usd: 0.035,
            max_suggest_ms: SUGGEST_GATE_BUDGET_MS,
            max_attempts: 2,
        }
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default)]
pub struct SuggestionGateSnapshot {
    pub final_count: usize,
    pub displayed_valid_ratio: f64,
    pub pending_count: usize,
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

#[derive(Debug, Clone)]
struct EvidenceItem {
    id: usize,
    file: PathBuf,
    line: usize, // 1-based
    snippet: String,
    why_interesting: String,
    source: EvidenceSource,
    pattern_kind: Option<PatternKind>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum EvidenceSource {
    Pattern,
    Hotspot,
    Core,
}

#[derive(Debug, Clone)]
struct EvidenceCandidate {
    score: f64,
    source_priority: usize,
    severity: PatternSeverity,
    item: EvidenceItem,
}

#[derive(Debug, Clone, Default)]
struct EvidencePackStats {
    pattern_count: usize,
    hotspot_count: usize,
    core_count: usize,
    line1_ratio: f64,
}

fn normalize_repo_relative(repo_root: &Path, path: &Path) -> Option<PathBuf> {
    if path.is_absolute() {
        path.strip_prefix(repo_root).ok().map(|p| p.to_path_buf())
    } else {
        Some(path.to_path_buf())
    }
}

fn read_snippet_around_line(
    repo_root: &Path,
    rel_path: &Path,
    line_1_based: usize,
) -> Option<String> {
    let full = repo_root.join(rel_path);
    let content = std::fs::read_to_string(&full).ok()?;
    let lines: Vec<&str> = content.lines().collect();
    if lines.is_empty() {
        return None;
    }
    let target = line_1_based.max(1).min(lines.len());
    let start = target
        .saturating_sub(FAST_EVIDENCE_SNIPPET_LINES_BEFORE)
        .max(1);
    let end = (target + FAST_EVIDENCE_SNIPPET_LINES_AFTER).min(lines.len());
    let snippet = lines
        .iter()
        .enumerate()
        .skip(start - 1)
        .take(end - start + 1)
        .map(|(i, l)| format!("{:4}| {}", i + 1, l))
        .collect::<Vec<_>>()
        .join("\n");
    Some(redact_obvious_secrets(&snippet))
}

fn redact_obvious_secrets(snippet: &str) -> String {
    // Deterministic redaction for common secret shapes before sending evidence to the LLM.
    let mut out = snippet.to_string();
    let patterns = [
        // Quoted key/value assignments.
        r#"(?i)\b(api[_-]?key|token|secret|password)\b\s*[:=]\s*["'][^"']{8,}["']"#,
        // Bearer tokens.
        r#"(?i)\b(bearer)\s+[A-Za-z0-9._-]{16,}"#,
        // OpenRouter/OpenAI/GitHub style keys.
        r#"\b(sk-[A-Za-z0-9_-]{16,})\b"#,
        r#"\b(gh[pousr]_[A-Za-z0-9]{16,})\b"#,
        // AWS access key IDs.
        r#"\b(AKIA[0-9A-Z]{16})\b"#,
        // Private keys in PEM blocks.
        r#"(?s)-----BEGIN [A-Z ]*PRIVATE KEY-----.*?-----END [A-Z ]*PRIVATE KEY-----"#,
    ];

    for pattern in patterns {
        if let Ok(re) = Regex::new(pattern) {
            out = re.replace_all(&out, "<redacted-secret>").to_string();
        }
    }

    out
}

fn evidence_payload_metrics(pack: &[EvidenceItem]) -> (usize, usize) {
    let snippet_count = pack.len();
    let bytes = pack.iter().map(|item| item.snippet.len()).sum::<usize>();
    (snippet_count, bytes)
}

fn severity_score(severity: PatternSeverity) -> f64 {
    match severity {
        PatternSeverity::High => 3.0,
        PatternSeverity::Medium => 2.0,
        PatternSeverity::Low => 1.0,
        PatternSeverity::Info => 0.5,
    }
}

fn reliability_score(reliability: PatternReliability) -> f64 {
    match reliability {
        PatternReliability::High => 0.55,
        PatternReliability::Medium => 0.3,
        PatternReliability::Low => 0.1,
    }
}

fn source_priority(source: EvidenceSource) -> usize {
    match source {
        EvidenceSource::Pattern => 3,
        EvidenceSource::Hotspot => 2,
        EvidenceSource::Core => 1,
    }
}

fn source_limit(source: EvidenceSource) -> usize {
    match source {
        EvidenceSource::Pattern => FAST_EVIDENCE_SOURCE_PATTERN_MAX,
        EvidenceSource::Hotspot => FAST_EVIDENCE_SOURCE_HOTSPOT_MAX,
        EvidenceSource::Core => FAST_EVIDENCE_SOURCE_CORE_MAX,
    }
}

fn best_function_anchor(file: &cosmos_core::index::FileIndex) -> usize {
    file.symbols
        .iter()
        .filter(|s| matches!(s.kind, SymbolKind::Function | SymbolKind::Method))
        .max_by(|a, b| {
            a.complexity
                .partial_cmp(&b.complexity)
                .unwrap_or(std::cmp::Ordering::Equal)
        })
        .map(|s| s.line)
        .unwrap_or(1)
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

fn is_test_like_snippet(snippet: &str) -> bool {
    let lower = snippet.to_ascii_lowercase();
    lower.contains("#[test]")
        || lower.contains("mod tests")
        || lower.contains("fn test_")
        || lower.contains("assert!(")
        || lower.contains("assert_eq!(")
        || lower.contains("assert_ne!(")
}

fn should_skip_evidence(path: &Path, snippet: &str) -> bool {
    is_test_like_path(path) || is_test_like_snippet(snippet)
}

fn exploratory_anchor_lines(file: &cosmos_core::index::FileIndex, max: usize) -> Vec<usize> {
    let max = max.max(1);
    let mut anchors: Vec<usize> = Vec::new();

    let mut symbols: Vec<_> = file
        .symbols
        .iter()
        .filter(|s| {
            matches!(
                s.kind,
                SymbolKind::Function
                    | SymbolKind::Method
                    | SymbolKind::Struct
                    | SymbolKind::Enum
                    | SymbolKind::Class
            )
        })
        .collect();

    symbols.sort_by(|a, b| {
        b.complexity
            .partial_cmp(&a.complexity)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| b.line_count().cmp(&a.line_count()))
            .then_with(|| a.line.cmp(&b.line))
    });

    for symbol in symbols {
        let line = symbol.line.max(1);
        if !anchors.contains(&line) {
            anchors.push(line);
        }
        if anchors.len() >= max {
            return anchors;
        }
    }

    let fallback = best_function_anchor(file).max(1);
    if !anchors.contains(&fallback) {
        anchors.push(fallback);
    }

    if file.loc > GOD_MODULE_LOC_THRESHOLD {
        let middle = (file.loc / 2).max(1);
        if !anchors.contains(&middle) {
            anchors.push(middle);
        }
    }

    let tail = file.loc.saturating_sub(20).max(1);
    if !anchors.contains(&tail) {
        anchors.push(tail);
    }

    anchors.truncate(max);
    anchors
}

fn build_evidence_pack(
    repo_root: &Path,
    index: &CodebaseIndex,
    context: &WorkContext,
) -> (Vec<EvidenceItem>, EvidencePackStats) {
    let changed: HashSet<PathBuf> = context.all_changed_files().into_iter().cloned().collect();
    let mut changed_in_index: Vec<(PathBuf, &cosmos_core::index::FileIndex)> = changed
        .iter()
        .filter_map(|path| index.files.get(path).map(|file| (path.clone(), file)))
        .collect();
    changed_in_index.sort_by(|(a_path, a_file), (b_path, b_file)| {
        b_file
            .complexity
            .partial_cmp(&a_file.complexity)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| b_file.loc.cmp(&a_file.loc))
            .then_with(|| a_path.cmp(b_path))
    });
    let mut candidates: Vec<EvidenceCandidate> = Vec::new();

    // Patterns (high-signal, deterministic ranking with reliability weighting)
    for file in index.files.values() {
        for p in &file.patterns {
            let Some(rel_file) = normalize_repo_relative(repo_root, &p.file) else {
                continue;
            };
            if matches!(
                p.kind,
                PatternKind::MissingErrorHandling | PatternKind::TodoMarker
            ) {
                continue;
            }
            let reliability = p.reliability;
            let severity = p.kind.severity();
            let pattern_bonus = match p.kind {
                PatternKind::PotentialResourceLeak => 0.35,
                PatternKind::GodModule => -0.35,
                _ => 0.0,
            };
            let changed_boost = if changed.contains(&rel_file) {
                0.2
            } else {
                0.0
            };
            let score = severity_score(severity)
                + reliability_score(reliability)
                + changed_boost
                + pattern_bonus;

            let anchor = if p.kind == PatternKind::GodModule {
                index
                    .files
                    .get(&rel_file)
                    .map(best_function_anchor)
                    .unwrap_or_else(|| p.line.max(1))
            } else {
                p.line.max(1)
            };

            if let Some(snippet) = read_snippet_around_line(repo_root, &rel_file, anchor) {
                if should_skip_evidence(&rel_file, &snippet) {
                    continue;
                }
                candidates.push(EvidenceCandidate {
                    score,
                    source_priority: source_priority(EvidenceSource::Pattern),
                    severity,
                    item: EvidenceItem {
                        id: 0,
                        file: rel_file,
                        line: anchor,
                        snippet,
                        why_interesting: format!("Detected {:?}: {}", p.kind, p.description),
                        source: EvidenceSource::Pattern,
                        pattern_kind: Some(p.kind),
                    },
                });
            }
        }
    }

    // Hotspots (complexity/LOC)
    let mut hotspot_files: Vec<_> = index.files.values().collect();
    hotspot_files.sort_by(|a, b| {
        b.complexity
            .partial_cmp(&a.complexity)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| b.loc.cmp(&a.loc))
            .then_with(|| a.path.cmp(&b.path))
    });
    for f in hotspot_files
        .iter()
        .filter(|f| f.complexity > HIGH_COMPLEXITY_THRESHOLD || f.loc > GOD_MODULE_LOC_THRESHOLD)
        .take(10)
    {
        let Some(rel_file) = normalize_repo_relative(repo_root, &f.path) else {
            continue;
        };
        if is_test_like_path(&rel_file) {
            continue;
        }
        for (anchor_rank, anchor) in exploratory_anchor_lines(f, FAST_EVIDENCE_ANCHORS_PER_FILE_MAX)
            .into_iter()
            .enumerate()
        {
            if let Some(snippet) = read_snippet_around_line(repo_root, &rel_file, anchor) {
                if should_skip_evidence(&rel_file, &snippet) {
                    continue;
                }
                let changed_boost = if changed.contains(&rel_file) {
                    0.2
                } else {
                    0.0
                };
                let score = 2.1 + (f.complexity / 60.0).min(1.0) + changed_boost
                    - (anchor_rank as f64 * 0.10);
                candidates.push(EvidenceCandidate {
                    score,
                    source_priority: source_priority(EvidenceSource::Hotspot),
                    severity: PatternSeverity::High,
                    item: EvidenceItem {
                        id: 0,
                        file: rel_file.clone(),
                        line: anchor,
                        snippet,
                        why_interesting: format!(
                            "Hotspot sample {} (complexity {:.1}, {} LOC)",
                            anchor_rank + 1,
                            f.complexity,
                            f.loc
                        ),
                        source: EvidenceSource::Hotspot,
                        pattern_kind: None,
                    },
                });
            }
        }
    }

    // Core files (fan-in)
    let mut core_files: Vec<_> = index.files.values().collect();
    core_files.sort_by(|a, b| {
        b.summary
            .used_by
            .len()
            .cmp(&a.summary.used_by.len())
            .then_with(|| a.path.cmp(&b.path))
    });
    for f in core_files
        .iter()
        .filter(|f| f.summary.used_by.len() >= 3)
        .take(10)
    {
        let Some(rel_file) = normalize_repo_relative(repo_root, &f.path) else {
            continue;
        };
        if is_test_like_path(&rel_file) {
            continue;
        }
        for (anchor_rank, anchor) in exploratory_anchor_lines(f, 2).into_iter().enumerate() {
            if let Some(snippet) = read_snippet_around_line(repo_root, &rel_file, anchor) {
                if should_skip_evidence(&rel_file, &snippet) {
                    continue;
                }
                let changed_boost = if changed.contains(&rel_file) {
                    0.15
                } else {
                    0.0
                };
                let score = 1.7 + (f.summary.used_by.len() as f64 / 25.0).min(1.0) + changed_boost
                    - (anchor_rank as f64 * 0.08);
                candidates.push(EvidenceCandidate {
                    score,
                    source_priority: source_priority(EvidenceSource::Core),
                    severity: PatternSeverity::Medium,
                    item: EvidenceItem {
                        id: 0,
                        file: rel_file.clone(),
                        line: anchor.max(1),
                        snippet,
                        why_interesting: format!(
                            "Core file sample {} used by {} other files",
                            anchor_rank + 1,
                            f.summary.used_by.len()
                        ),
                        source: EvidenceSource::Core,
                        pattern_kind: None,
                    },
                });
            }
        }
    }

    // Changed-file exploration: include multiple anchors in actively edited files,
    // even if they don't trip static pattern thresholds.
    for (rel_file, file) in changed_in_index.iter().take(FAST_EVIDENCE_CHANGED_FILE_MAX) {
        if is_test_like_path(rel_file) {
            continue;
        }
        for (anchor_rank, anchor) in
            exploratory_anchor_lines(file, FAST_EVIDENCE_ANCHORS_PER_FILE_MAX)
                .into_iter()
                .enumerate()
        {
            if let Some(snippet) = read_snippet_around_line(repo_root, rel_file, anchor) {
                if should_skip_evidence(rel_file, &snippet) {
                    continue;
                }
                let score =
                    2.0 + (file.complexity / 70.0).min(1.0) + ((file.loc as f64) / 1200.0).min(0.3)
                        - (anchor_rank as f64 * 0.10);
                candidates.push(EvidenceCandidate {
                    score,
                    source_priority: source_priority(EvidenceSource::Hotspot),
                    severity: PatternSeverity::High,
                    item: EvidenceItem {
                        id: 0,
                        file: rel_file.clone(),
                        line: anchor,
                        snippet,
                        why_interesting: format!(
                            "Changed-file sample {} (complexity {:.1}, {} LOC)",
                            anchor_rank + 1,
                            file.complexity,
                            file.loc
                        ),
                        source: EvidenceSource::Hotspot,
                        pattern_kind: None,
                    },
                });
            }
        }
    }

    // Neighbor exploration: sample files directly connected to changed files
    // to broaden context across call/dependency boundaries.
    let mut neighbor_paths: HashSet<PathBuf> = HashSet::new();
    for (_path, file) in &changed_in_index {
        for dep in &file.summary.depends_on {
            neighbor_paths.insert(dep.clone());
        }
        for used_by in &file.summary.used_by {
            neighbor_paths.insert(used_by.clone());
        }
    }
    for changed_path in &changed {
        neighbor_paths.remove(changed_path);
    }

    let mut neighbor_files: Vec<(PathBuf, &cosmos_core::index::FileIndex)> = neighbor_paths
        .into_iter()
        .filter_map(|path| index.files.get(&path).map(|file| (path, file)))
        .collect();
    neighbor_files.sort_by(|(a_path, a_file), (b_path, b_file)| {
        b_file
            .summary
            .used_by
            .len()
            .cmp(&a_file.summary.used_by.len())
            .then_with(|| {
                b_file
                    .complexity
                    .partial_cmp(&a_file.complexity)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .then_with(|| a_path.cmp(b_path))
    });

    for (rel_file, file) in neighbor_files
        .into_iter()
        .take(FAST_EVIDENCE_NEIGHBOR_FILE_MAX)
    {
        if is_test_like_path(&rel_file) {
            continue;
        }
        let anchor = best_function_anchor(file).max(1);
        if let Some(snippet) = read_snippet_around_line(repo_root, &rel_file, anchor) {
            if should_skip_evidence(&rel_file, &snippet) {
                continue;
            }
            let score = 1.8
                + (file.summary.used_by.len() as f64 / 20.0).min(1.0)
                + (file.complexity / 80.0).min(0.8);
            candidates.push(EvidenceCandidate {
                score,
                source_priority: source_priority(EvidenceSource::Core),
                severity: PatternSeverity::Medium,
                item: EvidenceItem {
                    id: 0,
                    file: rel_file,
                    line: anchor,
                    snippet,
                    why_interesting: "Neighbor of changed code (dependency/call boundary sample)"
                        .to_string(),
                    source: EvidenceSource::Core,
                    pattern_kind: None,
                },
            });
        }
    }

    // Coverage fallback: some repos (or subprojects) won't trip any of our pattern/hotspot/core
    // heuristics, but Cosmos should still be able to generate grounded suggestions by sampling
    // representative code. Only do this when we have no other candidates at all.
    if candidates.is_empty() {
        let mut fallback_files: Vec<_> = index.files.values().collect();
        fallback_files.sort_by(|a, b| {
            b.complexity
                .partial_cmp(&a.complexity)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| b.loc.cmp(&a.loc))
                .then_with(|| a.path.cmp(&b.path))
        });

        for f in fallback_files.iter().take(FAST_EVIDENCE_PACK_MAX_ITEMS) {
            let Some(rel_file) = normalize_repo_relative(repo_root, &f.path) else {
                continue;
            };
            if is_test_like_path(&rel_file) {
                continue;
            }
            let anchor = best_function_anchor(f).max(1);
            if let Some(snippet) = read_snippet_around_line(repo_root, &rel_file, anchor) {
                if should_skip_evidence(&rel_file, &snippet) {
                    continue;
                }
                let score =
                    0.8 + (f.complexity / 40.0).min(1.0) + ((f.loc as f64) / 600.0).min(1.0) * 0.2;
                candidates.push(EvidenceCandidate {
                    score,
                    source_priority: 0,
                    severity: PatternSeverity::Info,
                    item: EvidenceItem {
                        id: 0,
                        file: rel_file,
                        line: anchor,
                        snippet,
                        why_interesting: "Coverage sample: scan this snippet for concrete issues visible in code."
                            .to_string(),
                        source: EvidenceSource::Hotspot,
                        pattern_kind: None,
                    },
                });
            }
        }
    }

    // Deterministic ranking:
    // score, source priority, severity, then file path + line.
    candidates.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| b.source_priority.cmp(&a.source_priority))
            .then_with(|| b.severity.cmp(&a.severity))
            .then_with(|| a.item.file.cmp(&b.item.file))
            .then_with(|| a.item.line.cmp(&b.item.line))
    });

    let mut seen: HashSet<(PathBuf, usize)> = HashSet::new();
    let mut out: Vec<EvidenceItem> = Vec::new();
    let mut source_counts: HashMap<EvidenceSource, usize> = HashMap::new();
    let mut file_counts: HashMap<PathBuf, usize> = HashMap::new();
    let mut god_module_count = 0usize;

    for candidate in &candidates {
        let key = (candidate.item.file.clone(), candidate.item.line);
        if seen.contains(&key) {
            continue;
        }
        if file_counts.get(&candidate.item.file).copied().unwrap_or(0) >= FAST_EVIDENCE_PER_FILE_MAX
        {
            continue;
        }
        if source_counts
            .get(&candidate.item.source)
            .copied()
            .unwrap_or(0)
            >= source_limit(candidate.item.source)
        {
            continue;
        }
        if candidate.item.pattern_kind == Some(PatternKind::GodModule)
            && god_module_count >= FAST_EVIDENCE_KIND_GOD_MODULE_MAX
        {
            continue;
        }
        if candidate.item.pattern_kind == Some(PatternKind::GodModule) {
            god_module_count += 1;
        }
        *source_counts.entry(candidate.item.source).or_insert(0) += 1;
        *file_counts.entry(candidate.item.file.clone()).or_insert(0) += 1;
        seen.insert(key);
        out.push(candidate.item.clone());
        if out.len() >= FAST_EVIDENCE_PACK_MAX_ITEMS {
            break;
        }
    }

    // Second pass: if quotas were too restrictive, fill remaining slots.
    if out.len() < FAST_EVIDENCE_PACK_MAX_ITEMS {
        for candidate in &candidates {
            let key = (candidate.item.file.clone(), candidate.item.line);
            if out.len() >= FAST_EVIDENCE_PACK_MAX_ITEMS || seen.contains(&key) {
                continue;
            }
            if file_counts.get(&candidate.item.file).copied().unwrap_or(0)
                >= FAST_EVIDENCE_PER_FILE_MAX
            {
                continue;
            }
            if candidate.item.pattern_kind == Some(PatternKind::GodModule)
                && god_module_count >= FAST_EVIDENCE_KIND_GOD_MODULE_MAX
            {
                continue;
            }
            if candidate.item.pattern_kind == Some(PatternKind::GodModule) {
                god_module_count += 1;
            }
            *file_counts.entry(candidate.item.file.clone()).or_insert(0) += 1;
            seen.insert(key);
            out.push(candidate.item.clone());
        }
    }

    for (id, item) in out.iter_mut().enumerate() {
        item.id = id;
    }

    let mut stats = EvidencePackStats::default();
    let line1 = out.iter().filter(|i| i.line == 1).count();
    for item in &out {
        match item.source {
            EvidenceSource::Pattern => stats.pattern_count += 1,
            EvidenceSource::Hotspot => stats.hotspot_count += 1,
            EvidenceSource::Core => stats.core_count += 1,
        }
    }
    if !out.is_empty() {
        stats.line1_ratio = line1 as f64 / out.len() as f64;
    }

    (out, stats)
}

#[derive(Debug, Clone, serde::Deserialize)]
struct FastGroundedSuggestionJson {
    #[serde(default)]
    evidence_refs: Vec<FastGroundedEvidenceRefJson>,
    #[serde(default)]
    evidence_id: Option<usize>,
    #[serde(default)]
    snippet_id: Option<usize>,
    #[serde(default)]
    kind: String,
    #[serde(default)]
    priority: String,
    #[serde(default)]
    confidence: String,
    #[serde(default)]
    summary: String,
    #[serde(default)]
    detail: String,
}

#[derive(Debug, Clone, serde::Deserialize)]
struct FastGroundedResponseJson {
    suggestions: Vec<FastGroundedSuggestionJson>,
}

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(untagged)]
enum FastGroundedEvidenceRefJson {
    Object {
        #[serde(default)]
        evidence_id: Option<usize>,
        #[serde(default)]
        snippet_id: Option<usize>,
    },
    Integer(usize),
    String(String),
}

#[derive(Debug, Clone, serde::Deserialize)]
struct SuggestionValidationJson {
    #[serde(default)]
    validation: String,
    #[serde(default)]
    reason: String,
}

#[derive(Debug, Clone, serde::Deserialize)]
struct SuggestionBatchValidationItemJson {
    #[serde(default)]
    local_index: usize,
    #[serde(default)]
    validation: String,
    #[serde(default)]
    reason: String,
}

#[derive(Debug, Clone, serde::Deserialize)]
struct SuggestionBatchValidationJson {
    #[serde(default)]
    validations: Vec<SuggestionBatchValidationItemJson>,
}

#[derive(Debug, Clone, serde::Deserialize)]
struct OverclaimRewriteJson {
    #[serde(default)]
    summary: String,
    #[serde(default)]
    detail: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ValidationRejectClass {
    Contradicted,
    InsufficientEvidence,
    Transport,
    Other,
}

#[derive(Debug, Clone, Default)]
struct ValidationRejectionStats {
    prevalidation: usize,
    validator_contradicted: usize,
    validator_insufficient_evidence: usize,
    validator_transport: usize,
    validator_other: usize,
    transport_retry_count: usize,
    transport_recovered_count: usize,
    overclaim_rewrite_count: usize,
    overclaim_rewrite_validated_count: usize,
    deterministic_auto_validated: usize,
    deadline_exceeded: bool,
}

fn build_validation_rejection_histogram(
    stats: &ValidationRejectionStats,
) -> HashMap<String, usize> {
    HashMap::from([
        ("prevalidation".to_string(), stats.prevalidation),
        (
            "validator_contradicted".to_string(),
            stats.validator_contradicted,
        ),
        (
            "validator_insufficient_evidence".to_string(),
            stats.validator_insufficient_evidence,
        ),
        ("validator_transport".to_string(), stats.validator_transport),
        ("validator_other".to_string(), stats.validator_other),
        (
            "deterministic_auto_validated".to_string(),
            stats.deterministic_auto_validated,
        ),
    ])
}

fn suggestion_validation_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "validation": {
                "type": "string",
                "enum": ["validated", "contradicted", "insufficient_evidence"]
            },
            "reason": { "type": "string" }
        },
        "required": ["validation", "reason"],
        "additionalProperties": false
    })
}

fn suggestion_batch_validation_schema(max_local_index: usize) -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "validations": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "local_index": {
                            "type": "integer",
                            "minimum": 0,
                            "maximum": max_local_index
                        },
                        "validation": {
                            "type": "string",
                            "enum": ["validated", "contradicted", "insufficient_evidence"]
                        },
                        "reason": { "type": "string" }
                    },
                    "required": ["local_index", "validation", "reason"],
                    "additionalProperties": false
                }
            }
        },
        "required": ["validations"],
        "additionalProperties": false
    })
}

fn suggestion_overclaim_rewrite_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "summary": { "type": "string" },
            "detail": { "type": "string" }
        },
        "required": ["summary", "detail"],
        "additionalProperties": false
    })
}

fn is_overclaim_validation_reason(reason: &str) -> bool {
    let lower = reason.to_ascii_lowercase();
    [
        "assumption",
        "beyond evidence",
        "impact",
        "ui behavior",
        "business impact",
    ]
    .iter()
    .any(|marker| lower.contains(marker))
}

fn build_validation_evidence_block(suggestion: &Suggestion) -> String {
    let mut evidence_block = String::new();
    for (idx, reference) in suggestion.evidence_refs.iter().take(3).enumerate() {
        evidence_block.push_str(&format!(
            "Evidence {}: {}:{} (id={})\n",
            idx + 1,
            reference.file.display(),
            reference.line,
            reference.snippet_id
        ));
    }
    if let Some(snippet) = &suggestion.evidence {
        evidence_block.push_str("\nPRIMARY SNIPPET:\n");
        evidence_block.push_str(snippet);
        evidence_block.push('\n');
    }
    evidence_block
}

fn parse_validation_state(
    validation: &str,
) -> (SuggestionValidationState, Option<ValidationRejectClass>) {
    let normalized = validation
        .trim()
        .to_ascii_lowercase()
        .replace([' ', '-'], "_");

    if normalized.contains("contradict")
        || normalized.contains("unsupported")
        || normalized.contains("not_supported")
        || normalized.contains("not_valid")
        || normalized.contains("assumption")
    {
        return (
            SuggestionValidationState::Rejected,
            Some(ValidationRejectClass::Contradicted),
        );
    }

    if normalized.contains("insufficient")
        || normalized.contains("not_enough_evidence")
        || normalized.contains("insufficient_evidence")
        || normalized.contains("unclear")
    {
        return (
            SuggestionValidationState::Rejected,
            Some(ValidationRejectClass::InsufficientEvidence),
        );
    }

    if matches!(
        normalized.as_str(),
        "validated" | "valid" | "supported" | "support" | "supported_by_evidence"
    ) || normalized.contains("validated")
    {
        return (SuggestionValidationState::Validated, None);
    }

    (
        SuggestionValidationState::Rejected,
        Some(ValidationRejectClass::Other),
    )
}

fn reconcile_validation_from_reason(
    state: SuggestionValidationState,
    reject_class: Option<ValidationRejectClass>,
    reason: &str,
) -> (SuggestionValidationState, Option<ValidationRejectClass>) {
    let lower = reason.to_ascii_lowercase();
    if state == SuggestionValidationState::Validated
        && [
            "not support",
            "does not support",
            "cannot support",
            "insufficient",
            "contradict",
            "beyond evidence",
            "assumption",
        ]
        .iter()
        .any(|marker| lower.contains(marker))
    {
        let class = if [
            "insufficient",
            "not enough evidence",
            "cannot verify",
            "unclear from evidence",
        ]
        .iter()
        .any(|marker| lower.contains(marker))
        {
            ValidationRejectClass::InsufficientEvidence
        } else {
            ValidationRejectClass::Contradicted
        };
        return (SuggestionValidationState::Rejected, Some(class));
    }

    if !(state == SuggestionValidationState::Rejected
        && matches!(reject_class, Some(ValidationRejectClass::Other)))
    {
        return (state, reject_class);
    }
    let has_negative_marker = [
        "not support",
        "does not support",
        "cannot support",
        "insufficient",
        "contradict",
        "beyond evidence",
        "assumption",
        "deadline exceeded",
        "validation failed",
        "missing batch result",
    ]
    .iter()
    .any(|marker| lower.contains(marker));

    if !has_negative_marker
        && [
            "evidence shows",
            "evidence contains",
            "supports",
            "supported",
            "confirm",
            "directly shown",
        ]
        .iter()
        .any(|marker| lower.contains(marker))
    {
        return (SuggestionValidationState::Validated, None);
    }

    if [
        "insufficient",
        "not enough evidence",
        "cannot verify",
        "unclear from evidence",
    ]
    .iter()
    .any(|marker| lower.contains(marker))
    {
        return (
            SuggestionValidationState::Rejected,
            Some(ValidationRejectClass::InsufficientEvidence),
        );
    }

    if [
        "contradict",
        "beyond evidence",
        "assumption",
        "not supported",
    ]
    .iter()
    .any(|marker| lower.contains(marker))
    {
        return (
            SuggestionValidationState::Rejected,
            Some(ValidationRejectClass::Contradicted),
        );
    }

    (state, reject_class)
}

fn pack_item_by_id(pack: &[EvidenceItem], id: usize) -> Option<&EvidenceItem> {
    pack.iter().find(|item| item.id == id)
}

fn renumber_pack(items: &[EvidenceItem]) -> (Vec<EvidenceItem>, HashMap<usize, usize>) {
    let mut local_pack = Vec::with_capacity(items.len());
    let mut local_to_original = HashMap::with_capacity(items.len());

    for (local_id, item) in items.iter().enumerate() {
        local_to_original.insert(local_id, item.id);
        let mut cloned = item.clone();
        cloned.id = local_id;
        local_pack.push(cloned);
    }

    (local_pack, local_to_original)
}

fn remap_suggestion_to_original_ids(
    suggestion: &mut Suggestion,
    local_to_original: &HashMap<usize, usize>,
    full_pack: &[EvidenceItem],
) -> bool {
    let mut remapped_refs = Vec::new();
    let mut seen = HashSet::new();
    for reference in &suggestion.evidence_refs {
        let Some(original_id) = local_to_original.get(&reference.snippet_id).copied() else {
            continue;
        };
        if !seen.insert(original_id) {
            continue;
        }
        let Some(item) = pack_item_by_id(full_pack, original_id) else {
            continue;
        };
        remapped_refs.push(SuggestionEvidenceRef {
            snippet_id: item.id,
            file: item.file.clone(),
            line: item.line,
        });
    }

    if remapped_refs.is_empty() {
        return false;
    }

    suggestion.evidence_refs = remapped_refs;
    if let Some(primary) = suggestion.evidence_refs.first() {
        if let Some(item) = pack_item_by_id(full_pack, primary.snippet_id) {
            suggestion.file = item.file.clone();
            suggestion.line = Some(item.line);
            suggestion.evidence = Some(item.snippet.clone());
        }
    }
    true
}

fn dedupe_and_cap_grounded_suggestions(
    mapped: Vec<(usize, Suggestion)>,
    cap: usize,
) -> Vec<Suggestion> {
    let mut seen_ids: HashSet<usize> = HashSet::new();
    let mut unique = Vec::new();
    for (evidence_id, suggestion) in mapped {
        if seen_ids.insert(evidence_id) {
            unique.push(suggestion);
        }
    }
    unique.truncate(cap);
    unique
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

fn suggestion_similarity_tokens(suggestion: &Suggestion) -> HashSet<String> {
    collect_similarity_tokens(&format!(
        "{} {}",
        suggestion.summary,
        suggestion.detail.as_deref().unwrap_or("")
    ))
}

fn jaccard_similarity(a: &HashSet<String>, b: &HashSet<String>) -> f64 {
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    let intersection = a.intersection(b).count();
    let union = a.len() + b.len() - intersection;
    if union == 0 {
        0.0
    } else {
        intersection as f64 / union as f64
    }
}

fn overlap_coefficient(a: &HashSet<String>, b: &HashSet<String>) -> f64 {
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    let intersection = a.intersection(b).count();
    let min_size = a.len().min(b.len());
    if min_size == 0 {
        0.0
    } else {
        intersection as f64 / min_size as f64
    }
}

fn suggestions_semantically_overlap(a: &Suggestion, b: &Suggestion) -> bool {
    if a.file == b.file {
        if let (Some(a_line), Some(b_line)) = (a.line, b.line) {
            if a_line.abs_diff(b_line) <= 4 {
                return true;
            }
        }
    }

    let a_tokens = suggestion_similarity_tokens(a);
    let b_tokens = suggestion_similarity_tokens(b);
    let similarity = jaccard_similarity(&a_tokens, &b_tokens);
    let overlap_count = a_tokens.intersection(&b_tokens).count();
    let overlap_score = overlap_coefficient(&a_tokens, &b_tokens);

    if similarity >= 0.84 {
        return true;
    }
    if a.kind == b.kind && similarity >= 0.66 {
        return true;
    }
    if a.kind == b.kind && overlap_count >= 4 && overlap_score >= 0.5 {
        return true;
    }
    if a.file == b.file && similarity >= 0.58 {
        return true;
    }
    false
}

fn semantic_dedupe_validated_suggestions(
    mut validated: Vec<Suggestion>,
) -> (Vec<Suggestion>, usize) {
    validated.sort_by(|a, b| {
        b.priority
            .cmp(&a.priority)
            .then_with(|| b.confidence.cmp(&a.confidence))
            .then_with(|| {
                b.implementation_readiness_score
                    .partial_cmp(&a.implementation_readiness_score)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .then_with(|| b.created_at.cmp(&a.created_at))
    });

    let mut deduped = Vec::new();
    let mut dropped = 0usize;
    for suggestion in validated {
        if deduped
            .iter()
            .any(|existing| suggestions_semantically_overlap(existing, &suggestion))
        {
            dropped += 1;
            continue;
        }
        deduped.push(suggestion);
    }

    (deduped, dropped)
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

fn extract_metric_label(snippet: &str) -> Option<&'static str> {
    let lower = snippet.to_ascii_lowercase();
    if lower.contains("largest-contentful-paint") || lower.contains("lcp") {
        Some("LCP")
    } else if lower.contains("layout-shift") || lower.contains("cls") {
        Some("CLS")
    } else if lower.contains("first-contentful-paint") || lower.contains("fcp") {
        Some("FCP")
    } else if lower.contains("ttfb") || lower.contains("navigationtiming") {
        Some("TTFB")
    } else if lower.contains("inp") || lower.contains("durationthreshold") {
        Some("INP")
    } else {
        None
    }
}

fn conservative_summary_from_evidence(snippet: &str) -> Option<String> {
    let lower = snippet.to_ascii_lowercase();
    let has_catch = snippet_contains_empty_catch(snippet) || lower.contains("catch");

    if has_catch
        && (lower.contains("performanceobserver")
            || lower.contains("largest-contentful-paint")
            || lower.contains("layout-shift")
            || lower.contains("first-contentful-paint")
            || lower.contains("ttfb")
            || lower.contains("inp"))
    {
        if let Some(metric) = extract_metric_label(snippet) {
            return Some(format!(
                "{metric} telemetry can be missing when this error path is silently ignored."
            ));
        }
        return Some(
            "Performance telemetry can be missing when this error path is silently ignored."
                .to_string(),
        );
    }

    if lower.contains("kv not configured")
        && lower.contains("status: 'skipped'")
        && lower.contains("reason: 'kv not configured'")
    {
        return Some(
            "Requests are skipped when key-value storage is not configured, so this endpoint cannot serve cached data."
                .to_string(),
        );
    }

    if lower.contains("dump_alert_audience_set") || lower.contains("marketing:dump_alert:audience")
    {
        if lower.contains("srem(") {
            return Some(
                "Audience unsubscribe state can drift when this cache update fails silently."
                    .to_string(),
            );
        }
        if lower.contains("sadd(") {
            return Some(
                "Audience membership state can drift when this cache update fails silently."
                    .to_string(),
            );
        }
    }

    if lower.contains("lock") && lower.contains(".del(") && has_catch {
        return Some(
            "Lock cleanup failures can leave stale locks until timeout, delaying later jobs."
                .to_string(),
        );
    }

    None
}

fn trim_speculative_impact_clause(summary: &str) -> String {
    let trimmed = summary.trim();
    if trimmed.is_empty() {
        return String::new();
    }

    let lower = trimmed.to_ascii_lowercase();
    let connectors = [
        " causing ",
        " leading to ",
        " resulting in ",
        " so that ",
        " so users ",
        " so teams ",
    ];
    let mut cut_at: Option<usize> = None;
    for connector in connectors {
        if let Some(idx) = lower.find(connector) {
            cut_at = Some(cut_at.map(|current| current.min(idx)).unwrap_or(idx));
        }
    }
    if cut_at.is_none() {
        if let Some(idx) = trimmed.find(", so ") {
            cut_at = Some(idx);
        } else if let Some(idx) = trimmed.find(", which ") {
            cut_at = Some(idx);
        }
    }

    let core = cut_at
        .map(|idx| trimmed[..idx].trim())
        .unwrap_or(trimmed)
        .trim_end_matches(['.', ',', ';', ':'])
        .trim();
    if core.is_empty() {
        return String::new();
    }
    format!("{core}.")
}

fn filter_speculative_impact_suggestions(suggestions: Vec<Suggestion>) -> (Vec<Suggestion>, usize) {
    if suggestions.is_empty() {
        return (suggestions, 0);
    }

    let mut kept = Vec::with_capacity(suggestions.len());
    let mut dropped = 0usize;
    for mut suggestion in suggestions {
        let summary_is_speculative = has_speculative_impact_language(&suggestion.summary);
        let summary_is_valid =
            summary_normalization::is_valid_grounded_summary(&suggestion.summary);

        if summary_is_valid && !summary_is_speculative {
            kept.push(suggestion);
            continue;
        }

        if let Some(snippet) = suggestion.evidence.as_deref() {
            if let Some(rewritten) = conservative_summary_from_evidence(snippet) {
                let normalized = normalize_grounded_summary(
                    &rewritten,
                    suggestion.detail.as_deref().unwrap_or(""),
                    suggestion.line.unwrap_or_default(),
                );
                if summary_normalization::is_valid_grounded_summary(&normalized)
                    && (!has_speculative_impact_language(&normalized)
                        || claim_tokens_grounded_in_snippet(snippet, &normalized))
                {
                    suggestion.summary = normalized;
                    kept.push(suggestion);
                    continue;
                }
            }

            let grounded = claim_tokens_grounded_in_snippet(snippet, &suggestion.summary);
            if summary_is_valid
                && grounded
                && !summary_is_speculative
                && !has_overclaim_wording(&suggestion)
            {
                kept.push(suggestion);
                continue;
            }

            let trimmed = trim_speculative_impact_clause(&suggestion.summary);
            if !trimmed.is_empty() {
                let normalized = normalize_grounded_summary(
                    &trimmed,
                    suggestion.detail.as_deref().unwrap_or(""),
                    suggestion.line.unwrap_or_default(),
                );
                if summary_normalization::is_valid_grounded_summary(&normalized)
                    && (!has_speculative_impact_language(&normalized)
                        || claim_tokens_grounded_in_snippet(snippet, &normalized))
                {
                    suggestion.summary = normalized;
                    kept.push(suggestion);
                    continue;
                }
            }
        }

        dropped += 1;
    }

    (kept, dropped)
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
    let readiness = (0.35 * evidence_strength)
        + (0.35 * scope_tightness)
        + (0.20 * quick_check_targetability)
        + (0.10 * historical_fail_penalty);

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

    suggestion.implementation_readiness_score = Some(readiness.clamp(0.0, 1.0));
    suggestion.implementation_risk_flags = flags;
    suggestion.implementation_sketch = Some(build_implementation_sketch(&suggestion));
    suggestion
}

fn apply_readiness_filter(
    validated: Vec<Suggestion>,
    min_score: f32,
) -> (Vec<Suggestion>, usize, f64) {
    if validated.is_empty() {
        return (Vec::new(), 0, 0.0);
    }

    let annotated = validated
        .into_iter()
        .map(annotate_implementation_readiness)
        .collect::<Vec<_>>();
    let mean = annotated
        .iter()
        .map(|s| s.implementation_readiness_score.unwrap_or(0.0) as f64)
        .sum::<f64>()
        / annotated.len() as f64;

    let filtered = annotated
        .iter()
        .filter(|s| s.implementation_readiness_score.unwrap_or(0.0) >= min_score)
        .cloned()
        .collect::<Vec<_>>();
    let filtered_count = annotated.len().saturating_sub(filtered.len());
    (filtered, filtered_count, mean)
}

fn collect_valid_evidence_refs(
    suggestion: &FastGroundedSuggestionJson,
    pack: &[EvidenceItem],
) -> Vec<SuggestionEvidenceRef> {
    fn push_evidence_id(
        evidence_id: usize,
        pack: &[EvidenceItem],
        seen: &mut HashSet<usize>,
        refs: &mut Vec<SuggestionEvidenceRef>,
    ) {
        if seen.insert(evidence_id) {
            if let Some(item) = pack_item_by_id(pack, evidence_id) {
                refs.push(SuggestionEvidenceRef {
                    snippet_id: item.id,
                    file: item.file.clone(),
                    line: item.line,
                });
            }
        }
    }

    let mut refs = Vec::new();
    let mut seen = HashSet::new();

    let parse_ref_id = |reference: &FastGroundedEvidenceRefJson| -> Option<usize> {
        match reference {
            FastGroundedEvidenceRefJson::Object {
                evidence_id,
                snippet_id,
                ..
            } => (*evidence_id).or(*snippet_id),
            FastGroundedEvidenceRefJson::Integer(id) => Some(*id),
            FastGroundedEvidenceRefJson::String(raw) => raw.trim().parse::<usize>().ok(),
        }
    };

    for r in &suggestion.evidence_refs {
        if let Some(id) = parse_ref_id(r) {
            push_evidence_id(id, pack, &mut seen, &mut refs);
        }
    }

    // Minimal compatibility for older payloads that emitted top-level ids.
    if refs.is_empty() {
        if let Some(id) = suggestion.evidence_id.or(suggestion.snippet_id) {
            push_evidence_id(id, pack, &mut seen, &mut refs);
        }
    }

    // Enforce one evidence ref per suggestion at mapping time for stability.
    refs.truncate(1);
    refs
}

fn convert_raw_suggestion(
    s: FastGroundedSuggestionJson,
    pack: &[EvidenceItem],
) -> Option<(usize, Suggestion)> {
    let evidence_refs = collect_valid_evidence_refs(&s, pack);
    let evidence_id = evidence_refs.first().map(|r| r.snippet_id)?;
    let item = pack_item_by_id(pack, evidence_id)?;

    let kind = match s.kind.to_lowercase().as_str() {
        "bugfix" => cosmos_core::suggest::SuggestionKind::BugFix,
        "optimization" => cosmos_core::suggest::SuggestionKind::Optimization,
        "refactoring" => cosmos_core::suggest::SuggestionKind::Refactoring,
        "security" => cosmos_core::suggest::SuggestionKind::BugFix,
        "reliability" => cosmos_core::suggest::SuggestionKind::Quality,
        _ => cosmos_core::suggest::SuggestionKind::Improvement,
    };
    let priority = match s.priority.to_lowercase().as_str() {
        "high" => cosmos_core::suggest::Priority::High,
        "low" => cosmos_core::suggest::Priority::Low,
        _ => cosmos_core::suggest::Priority::Medium,
    };
    let confidence = match s.confidence.to_lowercase().as_str() {
        "high" => cosmos_core::suggest::Confidence::High,
        _ => cosmos_core::suggest::Confidence::Medium,
    };

    let detail = normalize_grounded_detail(&s.detail, &s.summary);
    let summary = normalize_grounded_summary(&s.summary, &detail, item.line);
    if summary.is_empty() {
        return None;
    }

    let suggestion = Suggestion::new(
        kind,
        priority,
        item.file.clone(),
        summary,
        cosmos_core::suggest::SuggestionSource::LlmDeep,
    )
    .with_confidence(confidence)
    .with_line(item.line)
    .with_detail(detail)
    .with_evidence(item.snippet.clone())
    .with_evidence_refs(evidence_refs)
    .with_validation_state(SuggestionValidationState::Pending);

    Some((evidence_id, suggestion))
}

fn map_raw_items_to_grounded(
    raw_items: Vec<FastGroundedSuggestionJson>,
    pack: &[EvidenceItem],
) -> (Vec<(usize, Suggestion)>, usize) {
    let mut mapped: Vec<(usize, Suggestion)> = Vec::new();
    let mut missing_or_invalid = 0usize;
    for s in raw_items {
        if let Some(converted) = convert_raw_suggestion(s, pack) {
            mapped.push(converted);
        } else {
            missing_or_invalid += 1;
        }
    }
    (mapped, missing_or_invalid)
}

fn should_run_mapping_rescue(raw_count: usize, mapped_count: usize) -> bool {
    raw_count > 0 && mapped_count == 0
}

fn grounded_mapped_count(mapped: &[(usize, Suggestion)]) -> usize {
    mapped
        .iter()
        .map(|(evidence_id, _)| *evidence_id)
        .collect::<HashSet<_>>()
        .len()
}

#[cfg(test)]
fn should_run_generation_topup(
    mapped_count: usize,
    topup_calls: usize,
    elapsed_ms: u64,
    budget_ms: u64,
) -> bool {
    if mapped_count >= FAST_GROUNDED_VALIDATED_HARD_TARGET
        || topup_calls >= GENERATION_TOPUP_MAX_CALLS
    {
        return false;
    }

    let remaining_budget_ms = budget_ms.saturating_sub(elapsed_ms);
    remaining_budget_ms >= GENERATION_TOPUP_TIMEOUT_MS
}

fn generation_topup_request_count(deficit: usize) -> usize {
    deficit.saturating_add(3).clamp(4, 10)
}

fn regeneration_needed(validated_count: usize) -> usize {
    FAST_GROUNDED_VALIDATED_SOFT_FLOOR.saturating_sub(validated_count)
}

fn regeneration_needed_for_target(validated_count: usize, target: usize) -> usize {
    target.saturating_sub(validated_count)
}

fn regeneration_request_bounds(needed: usize) -> (usize, usize) {
    let min_requested = needed.saturating_mul(2).clamp(4, 12);
    let max_requested = needed.saturating_mul(3).clamp(4, 14).max(min_requested);
    (min_requested, max_requested)
}

fn choose_regeneration_phase_target(
    validated_count: usize,
    hard_target: usize,
    stretch_target: usize,
    hard_phase_attempts: usize,
    stretch_phase_attempts: usize,
) -> Option<usize> {
    if validated_count < hard_target {
        return (hard_phase_attempts < REFINEMENT_HARD_PHASE_MAX_ATTEMPTS).then_some(hard_target);
    }
    if validated_count < stretch_target {
        return (stretch_phase_attempts < REFINEMENT_STRETCH_PHASE_MAX_ATTEMPTS)
            .then_some(stretch_target);
    }
    None
}

fn remaining_validation_budget_ms(validation_deadline: std::time::Instant) -> u64 {
    validation_deadline
        .saturating_duration_since(std::time::Instant::now())
        .as_millis() as u64
}

fn should_stop_regeneration_for_validation_budget(
    validation_deadline_exceeded: bool,
    remaining_validation_budget_ms: u64,
) -> bool {
    validation_deadline_exceeded
        || remaining_validation_budget_ms < VALIDATION_MIN_REMAINING_BUDGET_MS
}

fn should_retry_transport_rejection(
    class: ValidationRejectClass,
    attempts: usize,
    validation_deadline: std::time::Instant,
) -> bool {
    let remaining_budget_ms = remaining_validation_budget_ms(validation_deadline);
    matches!(class, ValidationRejectClass::Transport)
        && attempts < VALIDATION_RETRY_MAX_PER_SUGGESTION
        && remaining_budget_ms >= VALIDATION_RETRY_MIN_REMAINING_BUDGET_MS
}

fn build_remaining_pack_for_regeneration(
    pack: &[EvidenceItem],
    used_evidence_ids: &HashSet<usize>,
    rejected_evidence_ids: &HashSet<usize>,
    allow_rejected_relaxation: bool,
) -> (Vec<EvidenceItem>, bool, Vec<usize>) {
    let mut strict = Vec::new();
    let mut skipped_rejected_ids = Vec::new();

    for item in pack {
        if used_evidence_ids.contains(&item.id) {
            continue;
        }
        if rejected_evidence_ids.contains(&item.id) {
            skipped_rejected_ids.push(item.id);
            continue;
        }
        strict.push(item.clone());
    }

    if strict.len() >= REGEN_STRICT_MIN_PACK_SIZE || !allow_rejected_relaxation {
        return (strict, false, skipped_rejected_ids);
    }

    let relaxed = pack
        .iter()
        .filter(|item| !used_evidence_ids.contains(&item.id))
        .cloned()
        .collect::<Vec<_>>();
    (relaxed, true, Vec::new())
}

fn finalize_validated_suggestions(mut validated: Vec<Suggestion>) -> Vec<Suggestion> {
    // Defensive filter: refinement should only surface validated suggestions.
    validated.retain(|s| s.validation_state == SuggestionValidationState::Validated);
    validated.truncate(FAST_GROUNDED_FINAL_TARGET_MAX);
    validated
}

fn balance_suggestions_across_files(
    suggestions: Vec<Suggestion>,
    per_file_cap: usize,
    min_count: usize,
) -> (Vec<Suggestion>, usize) {
    if suggestions.is_empty() || per_file_cap == 0 {
        return (suggestions, 0);
    }

    let original_len = suggestions.len();
    let mut per_file_counts: HashMap<PathBuf, usize> = HashMap::new();
    let mut balanced = Vec::with_capacity(original_len);
    let mut overflow = Vec::new();

    for suggestion in suggestions {
        let file = suggestion.file.clone();
        let count = per_file_counts.entry(file).or_insert(0);
        if *count < per_file_cap {
            *count += 1;
            balanced.push(suggestion);
        } else {
            overflow.push(suggestion);
        }
    }

    let target_min_count = min_count.min(original_len);
    if balanced.len() < target_min_count {
        for suggestion in overflow {
            if balanced.len() >= target_min_count {
                break;
            }
            balanced.push(suggestion);
        }
    }

    let dropped = original_len.saturating_sub(balanced.len());
    (balanced, dropped)
}

fn grounded_suggestion_schema(pack_len: usize) -> serde_json::Value {
    let max_evidence_id = pack_len.saturating_sub(1);
    serde_json::json!({
        "type": "object",
        "properties": {
            "suggestions": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "evidence_refs": {
                            "type": "array",
                            "items": {
                                "type": "object",
                                "properties": {
                                    "evidence_id": {
                                        "type": "integer",
                                        "minimum": 0,
                                        "maximum": max_evidence_id
                                    }
                                },
                                "required": ["evidence_id"],
                                "additionalProperties": false
                            }
                        },
                        "kind": {
                            "type": "string",
                            "enum": ["bugfix", "improvement", "optimization", "refactoring", "security", "reliability"]
                        },
                        "priority": { "type": "string", "enum": ["high", "medium", "low"] },
                        "confidence": { "type": "string", "enum": ["high", "medium"] },
                        "summary": { "type": "string" },
                        "detail": { "type": "string" }
                    },
                    "required": ["evidence_refs", "kind", "priority", "confidence", "summary", "detail"],
                    "additionalProperties": false
                }
            }
        },
        "required": ["suggestions"],
        "additionalProperties": false
    })
}

fn format_grounded_user_prompt(
    memory_section: &str,
    index: &CodebaseIndex,
    summaries: Option<&HashMap<PathBuf, String>>,
    items: &[EvidenceItem],
    count_hint: &str,
) -> String {
    let mut user = String::new();
    if !memory_section.trim().is_empty() {
        user.push_str(memory_section);
        user.push_str("\n\n");
    }
    if let Some(summaries) = summaries {
        user.push_str("FILE SUMMARIES (grounding context):\n");
        for item in items {
            if let Some(summary) = summaries.get(&item.file) {
                user.push_str(&format!(
                    "- {}: {}\n",
                    item.file.display(),
                    truncate_str(summary, 180)
                ));
            } else if let Some(file_index) = index.files.get(&item.file) {
                user.push_str(&format!(
                    "- {}: {}\n",
                    item.file.display(),
                    truncate_str(&file_index.summary.purpose, 180)
                ));
            }
        }
        user.push('\n');
    }
    user.push_str(count_hint);
    user.push_str(
        "\nPrefer high-signal variety across product flows and files. Avoid concentrating suggestions in one file unless the evidence pack is genuinely narrow.",
    );
    user.push_str("\n\nEVIDENCE PACK (internal grounding only):\n");
    for item in items {
        user.push_str(&format!(
            "EVIDENCE {id}:\nSignal: {why}\nSNIPPET:\n{snippet}\n\n",
            id = item.id,
            why = item.why_interesting,
            snippet = item.snippet
        ));
    }
    user
}

async fn call_grounded_suggestions_with_fallback(
    system: &str,
    user: &str,
    model: Model,
    schema_name: &str,
    schema: serde_json::Value,
    max_tokens: u32,
    timeout_ms: u64,
) -> anyhow::Result<StructuredResponse<FastGroundedResponseJson>> {
    let primary = call_llm_structured_with_provider::<FastGroundedResponseJson>(
        system,
        user,
        model,
        schema_name,
        schema.clone(),
        super::client::provider_cerebras_fp16(),
        max_tokens,
        timeout_ms,
    )
    .await;

    match primary {
        Ok(response) => Ok(response),
        Err(primary_err) => {
            let fallback = call_llm_structured_limited::<FastGroundedResponseJson>(
                system,
                user,
                model,
                schema_name,
                schema,
                max_tokens,
                timeout_ms,
            )
            .await;

            match fallback {
                Ok(response) => Ok(response),
                Err(fallback_err) => Err(anyhow::anyhow!(
                    "Primary provider call failed: {} | Fallback routing failed: {}",
                    truncate_str(&primary_err.to_string(), 700),
                    truncate_str(&fallback_err.to_string(), 700)
                )),
            }
        }
    }
}

async fn call_validation_structured_with_fallback<T>(
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
    let primary_timeout_ms = timeout_ms.saturating_mul(2) / 3;
    let primary_timeout_ms = primary_timeout_ms.clamp(1_000, timeout_ms.max(1_000));
    let fallback_timeout_ms = timeout_ms.saturating_sub(primary_timeout_ms);

    let primary = call_llm_structured_with_provider::<T>(
        system,
        user,
        model,
        schema_name,
        schema.clone(),
        super::client::provider_cerebras_fp16(),
        max_tokens,
        primary_timeout_ms,
    )
    .await;

    match primary {
        Ok(response) => Ok(response),
        Err(primary_err) => {
            if fallback_timeout_ms < 800 {
                return Err(anyhow::anyhow!(
                    "Primary provider call failed: {}",
                    truncate_str(&primary_err.to_string(), 700)
                ));
            }
            let fallback = call_llm_structured_limited::<T>(
                system,
                user,
                model,
                schema_name,
                schema,
                max_tokens,
                fallback_timeout_ms,
            )
            .await;

            match fallback {
                Ok(response) => Ok(response),
                Err(fallback_err) => Err(anyhow::anyhow!(
                    "Primary provider call failed: {} | Fallback routing failed: {}",
                    truncate_str(&primary_err.to_string(), 700),
                    truncate_str(&fallback_err.to_string(), 700)
                )),
            }
        }
    }
}

async fn validate_suggestion_with_model_budget(
    suggestion: &Suggestion,
    memory_section: &str,
    validation_model: Model,
    max_tokens: u32,
    timeout_ms: u64,
) -> anyhow::Result<(
    SuggestionValidationState,
    String,
    Option<Usage>,
    Option<ValidationRejectClass>,
)> {
    let evidence_block = build_validation_evidence_block(suggestion);

    let system = r#"You are a strict evidence-grounded suggestion validator.
Use ONLY the provided suggestion and evidence snippets.

Validation rubric:
- Mark `validated` only if the suggestion's claim is directly supported by evidence.
- If the suggestion makes assumptions beyond the snippets (UI behavior, user state, rollback needs, business impact), mark `contradicted`.
- If evidence hints at an issue but cannot safely support the stated claim, mark `insufficient_evidence`.
- Do not infer unstated behavior.

Return JSON:
{
  "validation": "validated|contradicted|insufficient_evidence",
  "reason": "one short sentence"
}"#;

    let user = format!(
        "{}\n\nSUGGESTION SUMMARY:\n{}\n\nTECHNICAL DETAIL:\n{}\n\nEVIDENCE:\n{}\n\nDecide whether the suggestion is supported by the evidence only.",
        memory_section,
        suggestion.summary,
        suggestion.detail.as_deref().unwrap_or(""),
        evidence_block
    );

    let response = call_validation_structured_with_fallback::<SuggestionValidationJson>(
        system,
        &user,
        validation_model,
        "suggestion_validation",
        suggestion_validation_schema(),
        max_tokens,
        timeout_ms,
    )
    .await?;

    let normalized = response.data.validation.trim().to_lowercase();
    let (state, reject_class) = parse_validation_state(normalized.as_str());
    let (state, reject_class) =
        reconcile_validation_from_reason(state, reject_class, &response.data.reason);

    Ok((state, response.data.reason, response.usage, reject_class))
}

async fn rewrite_overclaim_suggestion_with_model(
    suggestion: &Suggestion,
    memory_section: &str,
    model: Model,
    max_tokens: u32,
    timeout_ms: u64,
) -> anyhow::Result<(Suggestion, Option<Usage>)> {
    let evidence_block = build_validation_evidence_block(suggestion);

    let system = r#"You rewrite suggestions to be strictly evidence-grounded.
Use only the provided snippet evidence.
- Keep the same core issue.
- Remove speculative user-impact claims and assumptions.
- Keep wording concise and concrete.

Return JSON:
{
  "summary": "one sentence, no speculation",
  "detail": "short technical detail grounded in evidence"
}"#;

    let user = format!(
        "{}\n\nCURRENT SUMMARY:\n{}\n\nCURRENT DETAIL:\n{}\n\nEVIDENCE:\n{}\n\nRewrite this suggestion conservatively so every claim is directly supported by evidence.",
        memory_section,
        suggestion.summary,
        suggestion.detail.as_deref().unwrap_or(""),
        evidence_block
    );

    let response = call_llm_structured_limited::<OverclaimRewriteJson>(
        system,
        &user,
        model,
        "overclaim_rewrite",
        suggestion_overclaim_rewrite_schema(),
        max_tokens,
        timeout_ms,
    )
    .await?;

    let mut rewritten = suggestion.clone();
    let rewritten_summary = scrub_user_summary(response.data.summary.trim());
    let summary_seed = if rewritten_summary.is_empty() {
        suggestion.summary.clone()
    } else {
        rewritten_summary
    };
    let rewritten_detail = response.data.detail.trim().to_string();
    let detail_seed = if rewritten_detail.is_empty() {
        suggestion.detail.as_deref().unwrap_or("").to_string()
    } else {
        rewritten_detail
    };
    let normalized_detail = normalize_grounded_detail(&detail_seed, &summary_seed);
    let normalized_summary = normalize_grounded_summary(
        &summary_seed,
        &normalized_detail,
        suggestion.line.unwrap_or_default(),
    );
    rewritten.summary = if normalized_summary.is_empty() {
        suggestion.summary.clone()
    } else {
        normalized_summary
    };
    rewritten.detail = Some(normalized_detail);
    Ok((rewritten, response.usage))
}

fn append_suggestion_quality_record(
    cache: &cosmos_adapters::cache::Cache,
    run_id: &str,
    suggestion: &Suggestion,
    outcome: &str,
    reason: Option<String>,
) {
    let record = cosmos_adapters::cache::SuggestionQualityRecord {
        timestamp: Utc::now(),
        run_id: run_id.to_string(),
        suggestion_id: suggestion.id.to_string(),
        evidence_ids: suggestion
            .evidence_refs
            .iter()
            .map(|r| r.snippet_id)
            .collect::<Vec<_>>(),
        validation_outcome: outcome.to_string(),
        validation_reason: reason,
        user_verify_outcome: None,
    };
    let _ = cache.append_suggestion_quality(&record);
}

type ValidationOutcome = (
    usize,
    Suggestion,
    usize,
    SuggestionValidationState,
    String,
    Option<Usage>,
    Option<ValidationRejectClass>,
);

type BatchValidationDecision = (
    SuggestionValidationState,
    String,
    Option<ValidationRejectClass>,
);

fn sort_validation_outcomes(outcomes: &mut [ValidationOutcome]) {
    outcomes.sort_by_key(|(idx, _, _, _, _, _, _)| *idx);
}

fn infer_validation_reject_class(
    reason: &str,
    reject_class: Option<ValidationRejectClass>,
) -> ValidationRejectClass {
    if let Some(class) = reject_class {
        return class;
    }
    let lowered = reason.to_ascii_lowercase();
    if lowered.starts_with("validation failed:") {
        ValidationRejectClass::Transport
    } else if lowered.contains("assumption")
        || lowered.contains("beyond evidence")
        || lowered.contains("business impact")
    {
        ValidationRejectClass::Contradicted
    } else if lowered.contains("insufficient") {
        ValidationRejectClass::InsufficientEvidence
    } else {
        ValidationRejectClass::Other
    }
}

fn map_batch_validation_response(
    chunk_len: usize,
    response: SuggestionBatchValidationJson,
) -> Vec<BatchValidationDecision> {
    let mut decisions: Vec<Option<BatchValidationDecision>> = vec![None; chunk_len];

    for item in response.validations {
        if item.local_index >= chunk_len || decisions[item.local_index].is_some() {
            continue;
        }

        let normalized = item.validation.trim().to_ascii_lowercase();
        let reason = if item.reason.trim().is_empty() {
            "Batch validator returned no reason".to_string()
        } else {
            truncate_str(item.reason.trim(), 180).to_string()
        };
        let (state, reject_class) = parse_validation_state(normalized.as_str());
        let (state, reject_class) = reconcile_validation_from_reason(state, reject_class, &reason);
        decisions[item.local_index] = Some((state, reason, reject_class));
    }

    decisions
        .into_iter()
        .map(|decision| {
            decision.unwrap_or((
                SuggestionValidationState::Rejected,
                "Validation failed: missing batch result".to_string(),
                Some(ValidationRejectClass::Transport),
            ))
        })
        .collect()
}

async fn validate_suggestions_batch_with_model_budget(
    chunk: &[(usize, Suggestion, usize)],
    memory_section: &str,
    validation_model: Model,
    max_tokens: u32,
    timeout_ms: u64,
) -> anyhow::Result<(Vec<BatchValidationDecision>, Option<Usage>)> {
    if chunk.is_empty() {
        return Ok((Vec::new(), None));
    }

    let system = r#"You are a strict evidence-grounded suggestion validator.
Validate each suggestion independently using ONLY its evidence snippets.

Validation rubric:
- Mark `validated` only if the suggestion claim is directly supported by evidence.
- If the suggestion makes assumptions beyond snippets (UI behavior, user state, rollback needs, business impact), mark `contradicted`.
- If evidence hints at an issue but cannot safely support the stated claim, mark `insufficient_evidence`.
- Do not infer unstated behavior.

Return JSON:
{
  "validations": [
    {
      "local_index": 0,
      "validation": "validated|contradicted|insufficient_evidence",
      "reason": "one short sentence"
    }
  ]
}"#;

    let mut user = String::new();
    user.push_str(memory_section);
    user.push_str("\n\nValidate all items below and return one result for each local_index.\n");

    for (local_index, (_idx, suggestion, _attempts)) in chunk.iter().enumerate() {
        user.push_str(&format!(
            "\nITEM {local_index}\nSUGGESTION SUMMARY:\n{summary}\n\nTECHNICAL DETAIL:\n{detail}\n\nEVIDENCE:\n{evidence}\n",
            local_index = local_index,
            summary = suggestion.summary,
            detail = suggestion.detail.as_deref().unwrap_or(""),
            evidence = build_validation_evidence_block(suggestion)
        ));
    }

    let response = call_validation_structured_with_fallback::<SuggestionBatchValidationJson>(
        system,
        &user,
        validation_model,
        "suggestion_batch_validation",
        suggestion_batch_validation_schema(chunk.len().saturating_sub(1)),
        max_tokens,
        timeout_ms,
    )
    .await?;

    Ok((
        map_batch_validation_response(chunk.len(), response.data),
        response.usage,
    ))
}

async fn try_overclaim_rewrite_revalidation(
    suggestion: &Suggestion,
    memory_section: &str,
    validation_model: Model,
    validation_deadline: std::time::Instant,
) -> Option<(
    Suggestion,
    String,
    Option<Usage>,
    SuggestionValidationState,
    Option<ValidationRejectClass>,
)> {
    if std::time::Instant::now() >= validation_deadline {
        return None;
    }

    let (rewritten, rewrite_usage) = rewrite_overclaim_suggestion_with_model(
        suggestion,
        memory_section,
        Model::Speed,
        OVERCLAIM_REWRITE_MAX_TOKENS,
        OVERCLAIM_REWRITE_TIMEOUT_MS,
    )
    .await
    .ok()?;

    let remaining_ms = remaining_validation_budget_ms(validation_deadline);
    let timeout_ms = remaining_ms.min(OVERCLAIM_REVALIDATE_TIMEOUT_MS);
    if timeout_ms == 0 {
        return Some((
            rewritten,
            "Validation failed: deadline exceeded".to_string(),
            rewrite_usage,
            SuggestionValidationState::Rejected,
            Some(ValidationRejectClass::Transport),
        ));
    }

    let validation = validate_suggestion_with_model_budget(
        &rewritten,
        memory_section,
        validation_model,
        OVERCLAIM_REVALIDATE_MAX_TOKENS,
        timeout_ms,
    )
    .await;

    match validation {
        Ok((state, reason, validate_usage, reject_class)) => Some((
            rewritten,
            reason,
            merge_usage(rewrite_usage, validate_usage),
            state,
            reject_class,
        )),
        Err(err) => Some((
            rewritten,
            format!(
                "Validation failed after rewrite: {}",
                truncate_str(&err.to_string(), 120)
            ),
            rewrite_usage,
            SuggestionValidationState::Rejected,
            Some(ValidationRejectClass::Transport),
        )),
    }
}

fn should_smart_rewrite_suggestion(
    suggestion: &Suggestion,
    min_implementation_readiness_score: f32,
) -> bool {
    let score = suggestion.implementation_readiness_score.unwrap_or(0.0);
    (score >= min_implementation_readiness_score && score <= SMART_REWRITE_READINESS_UPPER_BOUND)
        || has_overclaim_wording(suggestion)
}

async fn apply_selective_smart_rewrites(
    validated: Vec<Suggestion>,
    memory_section: &str,
    min_implementation_readiness_score: f32,
    max_smart_rewrites_per_run: usize,
    validation_model: Model,
    validation_deadline: std::time::Instant,
) -> (Vec<Suggestion>, usize, Option<Usage>) {
    if validated.is_empty() || max_smart_rewrites_per_run == 0 {
        return (validated, 0, None);
    }

    let mut rewrites = 0usize;
    let mut usage: Option<Usage> = None;
    let mut out = Vec::with_capacity(validated.len());

    for suggestion in validated {
        if rewrites >= max_smart_rewrites_per_run
            || !should_smart_rewrite_suggestion(&suggestion, min_implementation_readiness_score)
        {
            out.push(suggestion);
            continue;
        }

        let rewrite_timeout_ms = remaining_validation_budget_ms(validation_deadline)
            .clamp(1, SMART_BORDERLINE_REWRITE_TIMEOUT_MS);

        let rewrite = rewrite_overclaim_suggestion_with_model(
            &suggestion,
            memory_section,
            Model::Smart,
            SMART_BORDERLINE_REWRITE_MAX_TOKENS,
            rewrite_timeout_ms,
        )
        .await;
        let (rewritten, rewrite_usage) = match rewrite {
            Ok(value) => value,
            Err(_) => {
                out.push(suggestion);
                continue;
            }
        };
        usage = merge_usage(usage, rewrite_usage);

        let validate_timeout_ms = remaining_validation_budget_ms(validation_deadline)
            .clamp(1, OVERCLAIM_REVALIDATE_TIMEOUT_MS);
        let revalidated = validate_suggestion_with_model_budget(
            &rewritten,
            memory_section,
            validation_model,
            OVERCLAIM_REVALIDATE_MAX_TOKENS,
            validate_timeout_ms,
        )
        .await;
        match revalidated {
            Ok((SuggestionValidationState::Validated, _reason, validate_usage, _class)) => {
                usage = merge_usage(usage, validate_usage);
                rewrites += 1;
                let mut rewritten_valid = rewritten;
                rewritten_valid.validation_state = SuggestionValidationState::Validated;
                out.push(rewritten_valid);
            }
            Ok((_state, _reason, validate_usage, _class)) => {
                usage = merge_usage(usage, validate_usage);
                out.push(suggestion);
            }
            Err(_) => {
                out.push(suggestion);
            }
        }
    }

    (out, rewrites, usage)
}

async fn run_validation_attempts(
    chunk: Vec<(usize, Suggestion, usize)>,
    memory_section: &str,
    validation_model: Model,
    validation_deadline: std::time::Instant,
    per_call_timeout_ms: u64,
) -> Vec<ValidationOutcome> {
    if chunk.is_empty() {
        return Vec::new();
    }

    let remaining_budget = validation_deadline.saturating_duration_since(std::time::Instant::now());
    if remaining_budget.is_zero() {
        return chunk
            .into_iter()
            .map(|(idx, suggestion, attempts)| {
                (
                    idx,
                    suggestion,
                    attempts,
                    SuggestionValidationState::Rejected,
                    "Validation failed: deadline exceeded".to_string(),
                    None,
                    Some(ValidationRejectClass::Transport),
                )
            })
            .collect();
    }

    if chunk.len() > 1 {
        let remaining_budget_ms = remaining_budget.as_millis() as u64;
        let batch_timeout_ms = remaining_budget_ms
            .min(per_call_timeout_ms.saturating_add(VALIDATOR_BATCH_TIMEOUT_BUFFER_MS));
        if batch_timeout_ms > 0 {
            let dynamic_tokens = VALIDATOR_MAX_TOKENS.saturating_mul(chunk.len() as u32);
            let batch_tokens =
                dynamic_tokens.clamp(VALIDATOR_MAX_TOKENS, VALIDATOR_BATCH_MAX_TOKENS);
            let batch_call = tokio::time::timeout(
                std::time::Duration::from_millis(batch_timeout_ms),
                validate_suggestions_batch_with_model_budget(
                    &chunk,
                    memory_section,
                    validation_model,
                    batch_tokens,
                    batch_timeout_ms,
                ),
            )
            .await;

            if let Ok(Ok((decisions, batch_usage))) = batch_call {
                let mut usage_slot = batch_usage;
                let mut outcomes: Vec<ValidationOutcome> = chunk
                    .into_iter()
                    .zip(decisions.into_iter())
                    .map(
                        |((idx, suggestion, attempts), (state, reason, reject_class))| {
                            (
                                idx,
                                suggestion,
                                attempts,
                                state,
                                reason,
                                usage_slot.take(),
                                reject_class,
                            )
                        },
                    )
                    .collect();
                sort_validation_outcomes(&mut outcomes);
                return outcomes;
            }
        }
    }

    let memory_section_owned = memory_section.to_string();
    let mut outcomes: Vec<ValidationOutcome> =
        join_all(chunk.into_iter().map(|(idx, suggestion, attempts)| {
            let memory_section = memory_section_owned.clone();
            let deadline = validation_deadline;
            async move {
                let remaining = deadline.saturating_duration_since(std::time::Instant::now());
                if remaining.is_zero() {
                    return (
                        idx,
                        suggestion,
                        attempts,
                        SuggestionValidationState::Rejected,
                        "Validation failed: deadline exceeded".to_string(),
                        None,
                        Some(ValidationRejectClass::Transport),
                    );
                }

                let timeout = remaining.min(std::time::Duration::from_millis(per_call_timeout_ms));
                if timeout.is_zero() {
                    return (
                        idx,
                        suggestion,
                        attempts,
                        SuggestionValidationState::Rejected,
                        "Validation failed: deadline exceeded".to_string(),
                        None,
                        Some(ValidationRejectClass::Transport),
                    );
                }

                let timeout_ms = timeout.as_millis() as u64;
                let (state, reason, call_usage, reject_class) = match tokio::time::timeout(
                    timeout,
                    validate_suggestion_with_model_budget(
                        &suggestion,
                        &memory_section,
                        validation_model,
                        VALIDATOR_MAX_TOKENS,
                        timeout_ms,
                    ),
                )
                .await
                {
                    Ok(Ok(result)) => result,
                    Ok(Err(err)) => (
                        SuggestionValidationState::Rejected,
                        format!("Validation failed: {}", truncate_str(&err.to_string(), 120)),
                        None,
                        Some(ValidationRejectClass::Transport),
                    ),
                    Err(_) => (
                        SuggestionValidationState::Rejected,
                        "Validation failed: deadline exceeded".to_string(),
                        None,
                        Some(ValidationRejectClass::Transport),
                    ),
                };

                (
                    idx,
                    suggestion,
                    attempts,
                    state,
                    reason,
                    call_usage,
                    reject_class,
                )
            }
        }))
        .await;
    sort_validation_outcomes(&mut outcomes);
    outcomes
}

#[allow(clippy::too_many_arguments)]
fn record_rejected_validation(
    cache: &cosmos_adapters::cache::Cache,
    run_id: &str,
    suggestion: &mut Suggestion,
    reason: String,
    class: ValidationRejectClass,
    rejected_count: &mut usize,
    rejection_stats: &mut ValidationRejectionStats,
    rejected_evidence_ids: &mut HashSet<usize>,
) {
    *rejected_count += 1;
    if reason.to_ascii_lowercase().contains("deadline exceeded") {
        rejection_stats.deadline_exceeded = true;
    }
    match class {
        ValidationRejectClass::Contradicted => rejection_stats.validator_contradicted += 1,
        ValidationRejectClass::InsufficientEvidence => {
            rejection_stats.validator_insufficient_evidence += 1
        }
        ValidationRejectClass::Transport => rejection_stats.validator_transport += 1,
        ValidationRejectClass::Other => rejection_stats.validator_other += 1,
    }
    if !matches!(class, ValidationRejectClass::Transport) {
        if let Some(eid) = primary_evidence_id(suggestion) {
            rejected_evidence_ids.insert(eid);
        }
    }
    suggestion.validation_state = SuggestionValidationState::Rejected;
    let outcome = if matches!(class, ValidationRejectClass::InsufficientEvidence) {
        "insufficient_evidence"
    } else {
        "rejected"
    };
    append_suggestion_quality_record(cache, run_id, suggestion, outcome, Some(reason));
}

fn primary_evidence_id(suggestion: &Suggestion) -> Option<usize> {
    suggestion
        .evidence_refs
        .first()
        .map(|reference| reference.snippet_id)
}

fn prevalidation_rejection_reason(
    suggestion: &Suggestion,
    used_evidence_ids: &HashSet<usize>,
    chunk_seen_evidence_ids: &mut HashSet<usize>,
) -> Option<(String, Option<usize>, bool)> {
    let Some(evidence_id) = primary_evidence_id(suggestion) else {
        return Some((
            "Missing primary evidence ref before validation".to_string(),
            None,
            false,
        ));
    };

    if used_evidence_ids.contains(&evidence_id) {
        return Some((
            "Duplicate evidence_id already validated; skipped before validation".to_string(),
            Some(evidence_id),
            false,
        ));
    }

    if !chunk_seen_evidence_ids.insert(evidence_id) {
        return Some((
            "Duplicate evidence_id in validation batch; skipped before validation".to_string(),
            Some(evidence_id),
            false,
        ));
    }

    if let Some(reason) = deterministic_prevalidation_contradiction_reason(suggestion) {
        return Some((reason, Some(evidence_id), false));
    }

    if let Some(reason) = deterministic_prevalidation_non_actionable_reason(suggestion) {
        return Some((reason, Some(evidence_id), false));
    }

    None
}

fn normalize_claim_text_for_matching(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut prev_space = true;
    for ch in text.chars() {
        let mapped = if ch.is_ascii_alphanumeric() {
            ch.to_ascii_lowercase()
        } else {
            ' '
        };
        if mapped == ' ' {
            if prev_space {
                continue;
            }
            prev_space = true;
        } else {
            prev_space = false;
        }
        out.push(mapped);
    }
    out
}

fn snippet_code_line(line: &str) -> &str {
    if let Some((_, rest)) = line.split_once('|') {
        rest
    } else {
        line
    }
}

fn parse_leading_quoted_literal(rhs: &str) -> Option<String> {
    let mut chars = rhs.chars();
    let quote = chars.next()?;
    if quote != '"' && quote != '\'' {
        return None;
    }
    let mut value = String::new();
    let mut escaped = false;
    for ch in chars {
        if escaped {
            value.push(ch);
            escaped = false;
            continue;
        }
        if ch == '\\' {
            escaped = true;
            continue;
        }
        if ch == quote {
            return Some(value);
        }
        value.push(ch);
    }
    None
}

fn is_placeholder_client_id(value: &str) -> bool {
    let lower = value.trim().to_ascii_lowercase();
    if lower.is_empty() {
        return true;
    }
    [
        "your_client_id_here",
        "client_id_here",
        "placeholder",
        "replace",
        "changeme",
        "todo",
        "example",
    ]
    .iter()
    .any(|marker| lower.contains(marker))
}

fn snippet_has_configured_client_id_literal(snippet: &str) -> bool {
    for raw_line in snippet.lines() {
        let code = snippet_code_line(raw_line).trim();
        let lower = code.to_ascii_lowercase();
        if !lower.contains("client_id") {
            continue;
        }
        let Some((_, rhs)) = code.split_once('=') else {
            continue;
        };
        let Some(value) = parse_leading_quoted_literal(rhs.trim()) else {
            continue;
        };
        if !is_placeholder_client_id(&value) {
            return true;
        }
    }
    false
}

fn claim_targets_unconfigured_client_id(claim: &str) -> bool {
    claim.contains("client id")
        && (claim.contains("not configured")
            || claim.contains("missing")
            || claim.contains("placeholder")
            || claim.contains("not replaced"))
}

fn claim_targets_absolute_path_guard(claim: &str) -> bool {
    if !claim.contains("absolute path") {
        return false;
    }
    [
        "error", "fail", "cannot", "cant", "blocking", "stopping", "break", "trigger", "prevent",
    ]
    .iter()
    .any(|marker| claim.contains(marker))
}

fn snippet_has_absolute_path_guard(snippet: &str) -> bool {
    let lower = snippet.to_ascii_lowercase();
    lower.contains("is_absolute()") && lower.contains("absolute paths are not allowed")
}

fn claim_targets_cache_dir_creation(claim: &str) -> bool {
    let mentions_cache_dir = claim.contains("cache directory")
        || claim.contains("cache dir")
        || claim.contains("cache folder");
    if !mentions_cache_dir {
        return false;
    }
    claim.contains("not automatically created")
        || claim.contains("not created")
        || claim.contains("missing")
}

fn snippet_has_cache_dir_creation(snippet: &str) -> bool {
    let lower = snippet.to_ascii_lowercase();
    let mentions_cache_dir = lower.contains("cache_dir") || lower.contains("cache directory");
    let creates_or_ensures = lower.contains("create_dir_all")
        || lower.contains("ensure_dir()?")
        || lower.contains("ensure_dir()");
    mentions_cache_dir && creates_or_ensures
}

fn claim_is_safeguard_praise(claim: &str) -> bool {
    let has_positive_guard_word = [
        "prevent",
        "prevents",
        "protect",
        "protects",
        "blocks",
        "refuses",
        "rejects",
        "stops",
        "ensures",
        "guards",
        "mitigat",
        "secure",
        "safeguard",
    ]
    .iter()
    .any(|marker| claim.contains(marker));
    if !has_positive_guard_word {
        return false;
    }

    [
        "attacker",
        "malicious",
        "traversal",
        "arbitrary file",
        "security",
        "unsafe path",
        "outside the repository",
        "outside repository",
        "path validation",
        "path guard",
    ]
    .iter()
    .any(|marker| claim.contains(marker))
}

fn claim_mentions_defect_risk(claim: &str) -> bool {
    [
        "bypass",
        "bypassed",
        "missing",
        "fails",
        "failure",
        "broken",
        "bug",
        "vulnerab",
        "unsafe",
        "panic",
        "crash",
        "incorrect",
        "regression",
        "not validated",
        "not checked",
        "can still",
        "allows",
        "allowing",
        "exposes",
        "exploitable",
    ]
    .iter()
    .any(|marker| claim.contains(marker))
}

fn claim_has_strong_defect_risk(claim: &str) -> bool {
    [
        "vulnerab",
        "exploitable",
        "bypass",
        "data loss",
        "corrupt",
        "panic",
        "crash",
        "race condition",
        "deadlock",
        "stale lock",
        "unsafe write",
        "arbitrary file",
    ]
    .iter()
    .any(|marker| claim.contains(marker))
}

fn claim_is_non_security_praise(claim: &str) -> bool {
    [
        "clear error",
        "readable error",
        "friendly error",
        "helpful error",
        "clear message",
        "readable message",
        "instead of hanging",
        "instead of a silent failure",
        "instead of silent failure",
        "doesn t fail silently",
        "doesnt fail silently",
        "prevents silent",
        "preventing silent",
        "automatically retried",
        "retries automatically",
        "retried automatically",
        "now reflects",
        "now shows",
        "no longer result in confusing",
        "no longer produces confusing",
        "prevents confusion",
    ]
    .iter()
    .any(|marker| claim.contains(marker))
}

fn snippet_has_explicit_guard_check(snippet: &str) -> bool {
    let lower = snippet.to_ascii_lowercase();
    let has_guard_signal = [
        "is_absolute()",
        "absolute paths are not allowed",
        "parent traversal is not allowed",
        "path escapes repository",
        "symlinks are not allowed",
        "path contains symlink",
        "resolve_repo_path_allow_new",
        "component::parentdir",
    ]
    .iter()
    .any(|marker| lower.contains(marker));
    if !has_guard_signal {
        return false;
    }

    [
        "return err",
        "return false",
        "not allowed",
        "escapes repository",
        "invalid path",
    ]
    .iter()
    .any(|marker| lower.contains(marker))
}

fn snippet_has_explicit_non_security_handling(snippet: &str) -> bool {
    let lower = snippet.to_ascii_lowercase();
    let explicit_error_handling = [
        "return err(",
        ".map_err(",
        "anyhow::anyhow!",
        "show_toast(",
        "prompt_api_key_setup(",
        "failed to",
    ]
    .iter()
    .any(|marker| lower.contains(marker));
    let explicit_retry_handling = ["retry", "send_with_retry", "attempt", "backoff"]
        .iter()
        .any(|marker| lower.contains(marker));
    let explicit_ordering = ["sort_by(", "sort_by_key(", ".cmp("]
        .iter()
        .any(|marker| lower.contains(marker));
    explicit_error_handling || explicit_retry_handling || explicit_ordering
}

fn deterministic_prevalidation_contradiction_reason(suggestion: &Suggestion) -> Option<String> {
    let snippet = suggestion.evidence.as_deref()?;
    let claim = normalize_claim_text_for_matching(&format!(
        "{} {}",
        suggestion.summary,
        suggestion.detail.as_deref().unwrap_or("")
    ));

    if claim_targets_unconfigured_client_id(&claim)
        && snippet_has_configured_client_id_literal(snippet)
    {
        return Some(
            "Contradicted by evidence: client ID appears configured with a concrete value in the snippet."
                .to_string(),
        );
    }

    if claim_targets_absolute_path_guard(&claim) && snippet_has_absolute_path_guard(snippet) {
        return Some(
            "Contradicted by evidence: snippet shows an explicit absolute-path security guard."
                .to_string(),
        );
    }

    if claim_targets_cache_dir_creation(&claim) && snippet_has_cache_dir_creation(snippet) {
        return Some(
            "Contradicted by evidence: snippet already includes cache-directory creation/ensure logic."
                .to_string(),
        );
    }

    None
}

fn deterministic_prevalidation_non_actionable_reason(suggestion: &Suggestion) -> Option<String> {
    let snippet = suggestion.evidence.as_deref()?;
    let claim = normalize_claim_text_for_matching(&format!(
        "{} {}",
        suggestion.summary,
        suggestion.detail.as_deref().unwrap_or("")
    ));

    if claim_is_safeguard_praise(&claim)
        && !claim_mentions_defect_risk(&claim)
        && snippet_has_explicit_guard_check(snippet)
    {
        return Some(
            "Non-actionable safeguard description: snippet shows intentional guard behavior rather than a defect."
                .to_string(),
        );
    }

    if claim_is_non_security_praise(&claim)
        && !claim_has_strong_defect_risk(&claim)
        && snippet_has_explicit_non_security_handling(snippet)
    {
        return Some(
            "Non-actionable behavior description: suggestion praises existing handling rather than identifying a concrete defect."
                .to_string(),
        );
    }

    None
}

fn snippet_contains_empty_catch(snippet: &str) -> bool {
    let mut code_lines = Vec::new();
    for line in snippet.lines() {
        let code = snippet_code_line(line);
        code_lines.push(code.trim().to_string());
    }

    for idx in 0..code_lines.len() {
        let line = code_lines[idx].to_ascii_lowercase();
        if line.contains("catch {}") || line.contains("catch{}") {
            return true;
        }
        if !line.contains("catch") || !line.contains('{') {
            continue;
        }
        let after_brace = line
            .split_once('{')
            .map(|(_, rest)| rest.trim())
            .unwrap_or("");
        if !after_brace.is_empty() && after_brace != "}" {
            continue;
        }

        let mut next_idx = idx + 1;
        while next_idx < code_lines.len() {
            let next = code_lines[next_idx].trim();
            if next.is_empty() || next.starts_with("//") {
                next_idx += 1;
                continue;
            }
            if next == "}" {
                return true;
            }
            break;
        }
    }

    false
}

fn has_silent_error_language(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    [
        "silent",
        "silently",
        "ignored",
        "swallow",
        "not logged",
        "without logging",
        "hidden error",
        "not captured",
        "not reported",
        "go unnoticed",
        "suppressed errors",
    ]
    .iter()
    .any(|marker| lower.contains(marker))
}

fn has_high_speculation_impact_language(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    [
        "revenue",
        "profit",
        "spam",
        "phishing",
        "chargeback",
        "lawsuit",
        "financial loss",
        "support tickets",
        "support requests",
        "customer churn",
    ]
    .iter()
    .any(|marker| lower.contains(marker))
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
    overlap >= 1 && overlap_ratio >= 0.40
}

fn deterministic_auto_validation_reason(suggestion: &Suggestion) -> Option<String> {
    let snippet = suggestion.evidence.as_deref()?;
    if !snippet_contains_empty_catch(snippet) {
        return None;
    }

    let summary = suggestion.summary.as_str();
    let detail = suggestion.detail.as_deref().unwrap_or("");
    let combined = format!("{} {}", summary, detail);
    if !has_silent_error_language(&combined) {
        return None;
    }
    if has_high_speculation_impact_language(summary) {
        return None;
    }
    if has_overclaim_wording(suggestion) {
        return None;
    }
    if !claim_tokens_grounded_in_snippet(snippet, &combined) {
        return None;
    }

    Some(
        "Deterministic validation: snippet shows an empty catch block, summary describes silent handling, and claim terms are grounded in snippet tokens.".to_string(),
    )
}

#[allow(clippy::too_many_arguments)]
fn accept_validated_suggestion(
    cache: &cosmos_adapters::cache::Cache,
    run_id: &str,
    mut suggestion: Suggestion,
    reason: String,
    validated: &mut Vec<Suggestion>,
    used_evidence_ids: &mut HashSet<usize>,
    rejected_count: &mut usize,
    rejection_stats: &mut ValidationRejectionStats,
) -> bool {
    if let Some(eid) = primary_evidence_id(&suggestion) {
        if used_evidence_ids.insert(eid) {
            suggestion.validation_state = SuggestionValidationState::Validated;
            append_suggestion_quality_record(cache, run_id, &suggestion, "validated", Some(reason));
            validated.push(suggestion);
            return true;
        }
        *rejected_count += 1;
        rejection_stats.prevalidation += 1;
        suggestion.validation_state = SuggestionValidationState::Rejected;
        append_suggestion_quality_record(
            cache,
            run_id,
            &suggestion,
            "rejected",
            Some("Duplicate evidence_id after validation".to_string()),
        );
        return false;
    }

    *rejected_count += 1;
    rejection_stats.prevalidation += 1;
    suggestion.validation_state = SuggestionValidationState::Rejected;
    append_suggestion_quality_record(
        cache,
        run_id,
        &suggestion,
        "rejected",
        Some("Missing evidence refs after validation".to_string()),
    );
    false
}

#[allow(clippy::too_many_arguments)]
async fn validate_batch_suggestions(
    batch: Vec<Suggestion>,
    memory_section: &str,
    validation_model: Model,
    cache: &cosmos_adapters::cache::Cache,
    run_id: &str,
    validated: &mut Vec<Suggestion>,
    rejected_count: &mut usize,
    used_evidence_ids: &mut HashSet<usize>,
    rejected_evidence_ids: &mut HashSet<usize>,
    validation_deadline: std::time::Instant,
    rejection_stats: &mut ValidationRejectionStats,
) -> Option<Usage> {
    let mut usage: Option<Usage> = None;
    let mut queue: Vec<(usize, Suggestion, usize)> = batch
        .into_iter()
        .enumerate()
        .map(|(idx, suggestion)| (idx, suggestion, 0))
        .collect();
    while !queue.is_empty() {
        if std::time::Instant::now() >= validation_deadline {
            rejection_stats.deadline_exceeded = true;
            break;
        }
        if validated.len() >= FAST_GROUNDED_FINAL_TARGET_MAX {
            break;
        }
        let chunk_size = VALIDATION_CONCURRENCY.min(queue.len());
        let raw_chunk: Vec<(usize, Suggestion, usize)> = queue.drain(..chunk_size).collect();
        let mut chunk = Vec::new();
        let mut chunk_seen_evidence_ids: HashSet<usize> = HashSet::new();

        for (idx, mut suggestion, attempts) in raw_chunk {
            if let Some((reason, evidence_id, block_for_regeneration)) =
                prevalidation_rejection_reason(
                    &suggestion,
                    used_evidence_ids,
                    &mut chunk_seen_evidence_ids,
                )
            {
                *rejected_count += 1;
                rejection_stats.prevalidation += 1;
                if block_for_regeneration {
                    if let Some(eid) = evidence_id {
                        rejected_evidence_ids.insert(eid);
                    }
                }
                suggestion.validation_state = SuggestionValidationState::Rejected;
                append_suggestion_quality_record(
                    cache,
                    run_id,
                    &suggestion,
                    "rejected",
                    Some(reason),
                );
                continue;
            }

            if let Some(reason) = deterministic_auto_validation_reason(&suggestion) {
                if accept_validated_suggestion(
                    cache,
                    run_id,
                    suggestion,
                    reason,
                    validated,
                    used_evidence_ids,
                    rejected_count,
                    rejection_stats,
                ) {
                    rejection_stats.deterministic_auto_validated += 1;
                }
                continue;
            }
            chunk.push((idx, suggestion, attempts));
        }

        if chunk.is_empty() {
            continue;
        }

        let outcomes = run_validation_attempts(
            chunk,
            memory_section,
            validation_model,
            validation_deadline,
            VALIDATOR_TIMEOUT_MS,
        )
        .await;
        let mut retry_queue: Vec<(usize, Suggestion, usize)> = Vec::new();

        for (idx, mut suggestion, attempts, state, reason, call_usage, reject_class) in outcomes {
            usage = merge_usage(usage, call_usage);
            match state {
                SuggestionValidationState::Validated => {
                    let _ = accept_validated_suggestion(
                        cache,
                        run_id,
                        suggestion,
                        reason,
                        validated,
                        used_evidence_ids,
                        rejected_count,
                        rejection_stats,
                    );
                }
                SuggestionValidationState::Rejected => {
                    let mut reason = reason;
                    let mut class = infer_validation_reject_class(&reason, reject_class);
                    if !matches!(class, ValidationRejectClass::Transport)
                        && is_overclaim_validation_reason(&reason)
                    {
                        rejection_stats.overclaim_rewrite_count += 1;
                        if let Some((
                            rewritten,
                            rewritten_reason,
                            rewrite_usage,
                            rewritten_state,
                            rewritten_reject_class,
                        )) = try_overclaim_rewrite_revalidation(
                            &suggestion,
                            memory_section,
                            validation_model,
                            validation_deadline,
                        )
                        .await
                        {
                            usage = merge_usage(usage, rewrite_usage);
                            if rewritten_state == SuggestionValidationState::Validated {
                                if accept_validated_suggestion(
                                    cache,
                                    run_id,
                                    rewritten,
                                    format!("validated after rewrite: {}", rewritten_reason),
                                    validated,
                                    used_evidence_ids,
                                    rejected_count,
                                    rejection_stats,
                                ) {
                                    rejection_stats.overclaim_rewrite_validated_count += 1;
                                }
                                continue;
                            }
                            suggestion = rewritten;
                            reason = format!("{} (after rewrite)", rewritten_reason);
                            class = infer_validation_reject_class(&reason, rewritten_reject_class);
                        }
                    }

                    if validated.len() < FAST_GROUNDED_VALIDATED_HARD_TARGET
                        && should_retry_transport_rejection(class, attempts, validation_deadline)
                    {
                        rejection_stats.transport_retry_count += 1;
                        retry_queue.push((idx, suggestion, attempts + 1));
                        continue;
                    }
                    record_rejected_validation(
                        cache,
                        run_id,
                        &mut suggestion,
                        reason,
                        class,
                        rejected_count,
                        rejection_stats,
                        rejected_evidence_ids,
                    );
                }
                SuggestionValidationState::Pending => {
                    *rejected_count += 1;
                    rejection_stats.validator_other += 1;
                    if let Some(eid) = primary_evidence_id(&suggestion) {
                        rejected_evidence_ids.insert(eid);
                    }
                    suggestion.validation_state = SuggestionValidationState::Rejected;
                    append_suggestion_quality_record(
                        cache,
                        run_id,
                        &suggestion,
                        "rejected",
                        Some("Validator returned pending".to_string()),
                    );
                }
            }

            if validated.len() >= FAST_GROUNDED_VALIDATED_HARD_TARGET {
                break;
            }
        }

        while !retry_queue.is_empty() {
            if std::time::Instant::now() >= validation_deadline {
                rejection_stats.deadline_exceeded = true;
                break;
            }
            if validated.len() >= FAST_GROUNDED_FINAL_TARGET_MAX {
                break;
            }

            let retry_chunk_size = VALIDATION_RETRY_CONCURRENCY.min(retry_queue.len());
            let retry_chunk: Vec<(usize, Suggestion, usize)> =
                retry_queue.drain(..retry_chunk_size).collect();
            let retry_outcomes = run_validation_attempts(
                retry_chunk,
                memory_section,
                validation_model,
                validation_deadline,
                VALIDATOR_RETRY_TIMEOUT_MS,
            )
            .await;

            for (_idx, mut suggestion, attempts, state, reason, call_usage, reject_class) in
                retry_outcomes
            {
                usage = merge_usage(usage, call_usage);
                match state {
                    SuggestionValidationState::Validated => {
                        if accept_validated_suggestion(
                            cache,
                            run_id,
                            suggestion,
                            reason,
                            validated,
                            used_evidence_ids,
                            rejected_count,
                            rejection_stats,
                        ) && attempts > 0
                        {
                            rejection_stats.transport_recovered_count += 1;
                        }
                    }
                    SuggestionValidationState::Rejected => {
                        let mut reason = reason;
                        let mut class = infer_validation_reject_class(&reason, reject_class);
                        if !matches!(class, ValidationRejectClass::Transport)
                            && is_overclaim_validation_reason(&reason)
                        {
                            rejection_stats.overclaim_rewrite_count += 1;
                            if let Some((
                                rewritten,
                                rewritten_reason,
                                rewrite_usage,
                                rewritten_state,
                                rewritten_reject_class,
                            )) = try_overclaim_rewrite_revalidation(
                                &suggestion,
                                memory_section,
                                validation_model,
                                validation_deadline,
                            )
                            .await
                            {
                                usage = merge_usage(usage, rewrite_usage);
                                if rewritten_state == SuggestionValidationState::Validated {
                                    if accept_validated_suggestion(
                                        cache,
                                        run_id,
                                        rewritten,
                                        format!("validated after rewrite: {}", rewritten_reason),
                                        validated,
                                        used_evidence_ids,
                                        rejected_count,
                                        rejection_stats,
                                    ) {
                                        rejection_stats.overclaim_rewrite_validated_count += 1;
                                        if attempts > 0 {
                                            rejection_stats.transport_recovered_count += 1;
                                        }
                                    }
                                    continue;
                                }
                                suggestion = rewritten;
                                reason = format!("{} (after rewrite)", rewritten_reason);
                                class =
                                    infer_validation_reject_class(&reason, rewritten_reject_class);
                            }
                        }
                        record_rejected_validation(
                            cache,
                            run_id,
                            &mut suggestion,
                            reason,
                            class,
                            rejected_count,
                            rejection_stats,
                            rejected_evidence_ids,
                        );
                    }
                    SuggestionValidationState::Pending => {
                        *rejected_count += 1;
                        rejection_stats.validator_other += 1;
                        if let Some(eid) = primary_evidence_id(&suggestion) {
                            rejected_evidence_ids.insert(eid);
                        }
                        suggestion.validation_state = SuggestionValidationState::Rejected;
                        append_suggestion_quality_record(
                            cache,
                            run_id,
                            &suggestion,
                            "rejected",
                            Some("Validator returned pending".to_string()),
                        );
                    }
                }

                if validated.len() >= FAST_GROUNDED_VALIDATED_HARD_TARGET {
                    break;
                }
            }
        }

        if !retry_queue.is_empty() && std::time::Instant::now() >= validation_deadline {
            rejection_stats.deadline_exceeded = true;
            for (_idx, mut suggestion, _attempts) in retry_queue {
                record_rejected_validation(
                    cache,
                    run_id,
                    &mut suggestion,
                    "Validation failed: deadline exceeded".to_string(),
                    ValidationRejectClass::Transport,
                    rejected_count,
                    rejection_stats,
                    rejected_evidence_ids,
                );
            }
        }
    }
    usage
}

pub async fn analyze_codebase_fast_grounded(
    repo_root: &Path,
    index: &CodebaseIndex,
    context: &WorkContext,
    repo_memory: Option<String>,
    summaries: Option<&HashMap<PathBuf, String>>,
    generation_model: Model,
) -> anyhow::Result<(Vec<Suggestion>, Option<Usage>, SuggestionDiagnostics)> {
    let run_id = Uuid::new_v4().to_string();
    let overall_start = std::time::Instant::now();
    let pack_start = std::time::Instant::now();
    let (pack, pack_stats) = build_evidence_pack(repo_root, index, context);
    let evidence_pack_ms = pack_start.elapsed().as_millis() as u64;
    let (sent_snippet_count, sent_bytes) = evidence_payload_metrics(&pack);

    if pack.is_empty() {
        return Err(anyhow::anyhow!(
            "Not enough grounded evidence items found to generate suggestions. Cosmos couldn't extract any representative code snippets from this repo."
        ));
    }

    let memory_section =
        format_repo_memory_section(repo_memory.as_deref(), "Repo conventions / decisions");
    let schema = grounded_suggestion_schema(pack.len());
    let hard_target = FAST_GROUNDED_VALIDATED_HARD_TARGET.min(pack.len());

    let mut llm_ms = 0u64;
    let mut usage = None;
    let mut generation_errors: Vec<String> = Vec::new();
    let mut generation_waves = 1usize;
    let mut generation_topup_calls = 0usize;
    let mut raw_items: Vec<FastGroundedSuggestionJson> = Vec::new();

    let primary_start = std::time::Instant::now();
    let request_max = PRIMARY_REQUEST_MAX.min(pack.len()).max(1);
    let request_min = PRIMARY_REQUEST_MIN.min(request_max).max(1);
    let primary_result = call_grounded_suggestions_with_fallback(
        FAST_GROUNDED_SUGGESTIONS_SYSTEM,
        &format_grounded_user_prompt(
            &memory_section,
            index,
            summaries,
            &pack,
            &format!(
                "For this request, return {} to {} suggestions.",
                request_min, request_max
            ),
        ),
        generation_model,
        "fast_grounded_suggestions_primary",
        schema.clone(),
        PRIMARY_REQUEST_MAX_TOKENS,
        PRIMARY_REQUEST_TIMEOUT_MS,
    )
    .await;
    llm_ms += primary_start.elapsed().as_millis() as u64;

    match primary_result {
        Ok(r) => {
            usage = merge_usage(usage, r.usage);
            raw_items.extend(r.data.suggestions);
        }
        Err(err) => generation_errors.push(truncate_str(&err.to_string(), 700).to_string()),
    }

    let mut raw_count = raw_items.len();
    let (mut mapped, mut missing_or_invalid) = map_raw_items_to_grounded(raw_items, &pack);
    let mut generation_mapped_count = grounded_mapped_count(&mapped);

    while generation_mapped_count < hard_target
        && generation_topup_calls < GENERATION_TOPUP_MAX_CALLS
        && (overall_start.elapsed().as_millis() as u64) < SUGGEST_BALANCED_BUDGET_MS
    {
        let mapped_before_topup = generation_mapped_count;
        let deficit = hard_target.saturating_sub(generation_mapped_count);
        let request_count = generation_topup_request_count(deficit);
        let used_ids = mapped
            .iter()
            .map(|(evidence_id, _)| *evidence_id)
            .collect::<HashSet<_>>();
        let unused_ids = pack
            .iter()
            .map(|item| item.id)
            .filter(|id| !used_ids.contains(id))
            .take(24)
            .collect::<Vec<_>>();
        let unused_hint = if unused_ids.is_empty() {
            String::new()
        } else {
            format!(
                " Use ONLY evidence_id values from this set for this top-up: [{}].",
                unused_ids
                    .iter()
                    .map(|id| id.to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        };
        generation_topup_calls += 1;
        generation_waves += 1;

        let topup_start = std::time::Instant::now();
        let topup_result = call_grounded_suggestions_with_fallback(
            FAST_GROUNDED_SUGGESTIONS_SYSTEM,
            &format_grounded_user_prompt(
                &memory_section,
                index,
                summaries,
                &pack,
                &format!(
                    "Return {} suggestions. Prefer diverse evidence refs and avoid reusing the same evidence_id unless necessary.{}",
                    request_count, unused_hint
                ),
            ),
            generation_model,
            "fast_grounded_suggestions_topup",
            schema.clone(),
            TOPUP_REQUEST_MAX_TOKENS,
            GENERATION_TOPUP_TIMEOUT_MS,
        )
        .await;
        llm_ms += topup_start.elapsed().as_millis() as u64;
        match topup_result {
            Ok(r) => {
                usage = merge_usage(usage, r.usage);
                raw_count += r.data.suggestions.len();
                let (topup_mapped, topup_missing_or_invalid) =
                    map_raw_items_to_grounded(r.data.suggestions, &pack);
                mapped.extend(topup_mapped);
                missing_or_invalid += topup_missing_or_invalid;
                generation_mapped_count = grounded_mapped_count(&mapped);
            }
            Err(err) => generation_errors.push(truncate_str(&err.to_string(), 700).to_string()),
        }

        // Stop if the top-up produced no additional mapped evidence ids.
        if generation_mapped_count <= mapped_before_topup {
            break;
        }
    }

    // If generation returned content but nothing mapped, retry once with a strict full-pack call.
    if should_run_mapping_rescue(raw_count, generation_mapped_count) {
        generation_waves += 1;
        let rescue_start = std::time::Instant::now();
        let rescue_result = call_grounded_suggestions_with_fallback(
            FAST_GROUNDED_SUGGESTIONS_SYSTEM,
            &format_grounded_user_prompt(
                &memory_section,
                index,
                summaries,
                &pack,
                &format!(
                    "Return {} to {} suggestions. Every suggestion must include exactly one evidence reference.",
                    FAST_GROUNDED_FINAL_TARGET_MIN,
                    FAST_GROUNDED_FINAL_TARGET_MAX
                ),
            ),
            generation_model,
            "fast_grounded_mapping_rescue",
            schema.clone(),
            PRIMARY_REQUEST_MAX_TOKENS,
            PRIMARY_REQUEST_TIMEOUT_MS,
        )
        .await;
        llm_ms += rescue_start.elapsed().as_millis() as u64;
        match rescue_result {
            Ok(r) => {
                usage = merge_usage(usage, r.usage);
                raw_count += r.data.suggestions.len();
                let (rescue_mapped, rescue_missing_or_invalid) =
                    map_raw_items_to_grounded(r.data.suggestions, &pack);
                mapped.extend(rescue_mapped);
                missing_or_invalid += rescue_missing_or_invalid;
                generation_mapped_count = grounded_mapped_count(&mapped);
            }
            Err(err) => generation_errors.push(truncate_str(&err.to_string(), 700).to_string()),
        }
    }

    if mapped.is_empty() {
        let detail = generation_errors
            .first()
            .map(|e| format!(" Latest generation error: {}", e))
            .unwrap_or_default();
        return Err(anyhow::anyhow!(
            "AI suggestions arrived without valid evidence links, so Cosmos could not safely ground them. Please try again.{}",
            detail
        ));
    }

    let suggestions =
        dedupe_and_cap_grounded_suggestions(mapped, FAST_GROUNDED_PROVISIONAL_TARGET_MAX);

    let diagnostics = SuggestionDiagnostics {
        run_id,
        model: generation_model.id().to_string(),
        iterations: 1,
        tool_calls: 0,
        tool_names: Vec::new(),
        tool_exec_ms: 0,
        llm_ms,
        batch_verify_ms: 0,
        forced_final: false,
        formatting_pass: false,
        response_format: true,
        response_healing: true,
        parse_strategy: "fast_grounded".to_string(),
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
        response_chars: 0,
        response_preview: String::new(),
        evidence_pack_ms,
        sent_snippet_count,
        sent_bytes,
        pack_pattern_count: pack_stats.pattern_count,
        pack_hotspot_count: pack_stats.hotspot_count,
        pack_core_count: pack_stats.core_count,
        pack_line1_ratio: pack_stats.line1_ratio,
        provisional_count: suggestions.len(),
        generation_waves,
        generation_topup_calls,
        generation_mapped_count,
        validated_count: 0,
        rejected_count: 0,
        rejected_evidence_skipped_count: 0,
        validation_rejection_histogram: HashMap::new(),
        validation_deadline_exceeded: false,
        validation_deadline_ms: VALIDATION_RUN_DEADLINE_MS,
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
        notes: Vec::new(),
    };

    Ok((suggestions, usage, diagnostics))
}

// This orchestrator keeps all refinement controls at the callsite for deterministic gating.
#[allow(clippy::too_many_arguments)]
pub async fn refine_grounded_suggestions(
    repo_root: &Path,
    index: &CodebaseIndex,
    context: &WorkContext,
    repo_memory: Option<String>,
    summaries: Option<&HashMap<PathBuf, String>>,
    generation_model: Model,
    validation_model: Model,
    provisional: Vec<Suggestion>,
    min_implementation_readiness_score: f32,
    max_smart_rewrites_per_run: usize,
    mut diagnostics: SuggestionDiagnostics,
) -> anyhow::Result<(Vec<Suggestion>, Option<Usage>, SuggestionDiagnostics)> {
    if provisional.is_empty() {
        diagnostics.refinement_complete = true;
        diagnostics.provisional_count = 0;
        diagnostics.validated_count = 0;
        diagnostics.rejected_count = 0;
        diagnostics.final_count = 0;
        diagnostics.validation_transport_retry_count = 0;
        diagnostics.validation_transport_recovered_count = 0;
        diagnostics.regen_stopped_validation_budget = false;
        diagnostics.smart_rewrite_count = 0;
        diagnostics.deterministic_auto_validated_count = 0;
        diagnostics.semantic_dedup_dropped_count = 0;
        diagnostics.file_balance_dropped_count = 0;
        diagnostics.speculative_impact_dropped_count = 0;
        diagnostics.dominant_topic_ratio = 0.0;
        diagnostics.unique_topic_count = 0;
        diagnostics.dominant_file_ratio = 0.0;
        diagnostics.unique_file_count = 0;
        diagnostics.readiness_filtered_count = 0;
        diagnostics.readiness_score_mean = 0.0;
        diagnostics.notes = Vec::new();
        return Ok((Vec::new(), None, diagnostics));
    }

    let memory_section =
        format_repo_memory_section(repo_memory.as_deref(), "Repo conventions / decisions");
    let cache = cosmos_adapters::cache::Cache::new(repo_root);
    let refine_start = std::time::Instant::now();
    let mut usage: Option<Usage> = None;
    let mut rejected_count = 0usize;
    let mut regeneration_attempts = 0usize;
    let mut used_evidence_ids: HashSet<usize> = HashSet::new();
    let mut rejected_evidence_ids: HashSet<usize> = HashSet::new();
    let mut rejected_evidence_skipped_ids: HashSet<usize> = HashSet::new();
    let mut relaxed_rejected_filter_used = false;
    let mut validated: Vec<Suggestion> = Vec::new();
    let mut notes = diagnostics.notes.clone();
    let mut hard_phase_attempts = 0usize;
    let mut stretch_phase_attempts = 0usize;
    let mut regen_stopped_validation_budget = false;
    let validation_deadline = refine_start
        + std::time::Duration::from_millis(
            VALIDATION_RUN_DEADLINE_MS.min(SUGGEST_BALANCED_BUDGET_MS),
        );
    let mut rejection_stats = ValidationRejectionStats::default();

    let batch_usage = validate_batch_suggestions(
        provisional.clone(),
        &memory_section,
        validation_model,
        &cache,
        &diagnostics.run_id,
        &mut validated,
        &mut rejected_count,
        &mut used_evidence_ids,
        &mut rejected_evidence_ids,
        validation_deadline,
        &mut rejection_stats,
    )
    .await;
    usage = merge_usage(usage, batch_usage);

    let (pack, pack_stats) = build_evidence_pack(repo_root, index, context);
    let (sent_snippet_count, sent_bytes) = evidence_payload_metrics(&pack);
    let hard_target = FAST_GROUNDED_VALIDATED_HARD_TARGET
        .min(pack.len())
        .min(FAST_GROUNDED_FINAL_TARGET_MAX);
    let stretch_target = FAST_GROUNDED_VALIDATED_STRETCH_TARGET
        .min(pack.len())
        .min(FAST_GROUNDED_FINAL_TARGET_MAX);
    while validated.len() < stretch_target {
        let remaining_validation_budget = remaining_validation_budget_ms(validation_deadline);
        if should_stop_regeneration_for_validation_budget(
            rejection_stats.deadline_exceeded,
            remaining_validation_budget,
        ) {
            regen_stopped_validation_budget = true;
            notes.push("regen_stopped_validation_budget".to_string());
            break;
        }

        if refine_start.elapsed().as_millis() as u64 >= SUGGEST_BALANCED_BUDGET_MS {
            notes.push("regeneration_budget_reached".to_string());
            break;
        }

        let Some(phase_target) = choose_regeneration_phase_target(
            validated.len(),
            hard_target,
            stretch_target,
            hard_phase_attempts,
            stretch_phase_attempts,
        ) else {
            if validated.len() < hard_target {
                notes.push("hard_target_attempt_budget_reached".to_string());
            }
            break;
        };

        if phase_target == hard_target {
            hard_phase_attempts += 1;
        } else {
            let current_cost = usage.as_ref().map(|u| u.cost()).unwrap_or(0.0);
            if current_cost >= STRETCH_PHASE_MAX_COST_USD {
                notes.push("stretch_skipped_cost_budget".to_string());
                break;
            }
            if remaining_validation_budget < STRETCH_PHASE_MIN_REMAINING_VALIDATION_MS {
                notes.push("stretch_skipped_validation_budget".to_string());
                break;
            }
            stretch_phase_attempts += 1;
        }

        regeneration_attempts += 1;

        let (remaining_pack_original, used_relaxed_filter, skipped_rejected_ids) =
            build_remaining_pack_for_regeneration(
                &pack,
                &used_evidence_ids,
                &rejected_evidence_ids,
                !relaxed_rejected_filter_used,
            );
        if used_relaxed_filter {
            relaxed_rejected_filter_used = true;
        } else {
            rejected_evidence_skipped_ids.extend(skipped_rejected_ids);
        }

        if remaining_pack_original.len() < 4 {
            break;
        }
        let (remaining_pack_local, local_to_original) = renumber_pack(&remaining_pack_original);

        let needed = regeneration_needed_for_target(validated.len(), phase_target);
        if needed == 0 {
            break;
        }
        let (request_min, request_max) = regeneration_request_bounds(needed);
        let schema = grounded_suggestion_schema(remaining_pack_local.len());
        let user = format_grounded_user_prompt(
            &memory_section,
            index,
            summaries,
            &remaining_pack_local,
            &format!(
                "Return {} to {} suggestions. Avoid reusing evidence ids and prioritize high-confidence issues.",
                request_min, request_max
            ),
        );

        let regen_response = call_grounded_suggestions_with_fallback(
            FAST_GROUNDED_SUGGESTIONS_SYSTEM,
            &user,
            generation_model,
            "fast_grounded_regeneration",
            schema,
            REGEN_REQUEST_MAX_TOKENS,
            7_200,
        )
        .await;

        let rebuilt = match regen_response {
            Ok(response) => response,
            Err(_) => continue,
        };
        usage = merge_usage(usage, rebuilt.usage);
        let (mapped, _missing_or_invalid) =
            map_raw_items_to_grounded(rebuilt.data.suggestions, &remaining_pack_local);
        let mut regenerated = Vec::new();
        for (_local_id, mut suggestion) in mapped {
            if remap_suggestion_to_original_ids(&mut suggestion, &local_to_original, &pack) {
                regenerated.push(suggestion);
            }
        }
        if regenerated.is_empty() {
            continue;
        }

        let batch_usage = validate_batch_suggestions(
            regenerated,
            &memory_section,
            validation_model,
            &cache,
            &diagnostics.run_id,
            &mut validated,
            &mut rejected_count,
            &mut used_evidence_ids,
            &mut rejected_evidence_ids,
            validation_deadline,
            &mut rejection_stats,
        )
        .await;
        usage = merge_usage(usage, batch_usage);
    }

    let validated = finalize_validated_suggestions(validated);
    let (semantic_deduped, semantic_dedup_dropped_count) =
        semantic_dedupe_validated_suggestions(validated);
    let (readiness_filtered, readiness_filtered_count, readiness_score_mean) =
        apply_readiness_filter(semantic_deduped, min_implementation_readiness_score);
    let (validated, smart_rewrite_count, smart_rewrite_usage) = apply_selective_smart_rewrites(
        readiness_filtered,
        &memory_section,
        min_implementation_readiness_score,
        max_smart_rewrites_per_run,
        validation_model,
        validation_deadline,
    )
    .await;
    usage = merge_usage(usage, smart_rewrite_usage);
    let (impact_filtered, speculative_impact_dropped_count) =
        filter_speculative_impact_suggestions(validated);
    let impact_filtered_len = impact_filtered.len();
    let (validated, file_balance_dropped_count) = balance_suggestions_across_files(
        impact_filtered,
        DIVERSITY_FILE_BALANCE_PER_FILE_CAP,
        FAST_GROUNDED_FINAL_TARGET_MIN.min(impact_filtered_len),
    );
    let diversity_metrics = compute_suggestion_diversity_metrics(&validated);
    let refinement_ms = refine_start.elapsed().as_millis() as u64;
    diagnostics.batch_verify_ms = refinement_ms;
    diagnostics.llm_ms += refinement_ms;
    diagnostics.pack_pattern_count = pack_stats.pattern_count;
    diagnostics.pack_hotspot_count = pack_stats.hotspot_count;
    diagnostics.pack_core_count = pack_stats.core_count;
    diagnostics.pack_line1_ratio = pack_stats.line1_ratio;
    diagnostics.sent_snippet_count = sent_snippet_count;
    diagnostics.sent_bytes = sent_bytes;
    diagnostics.provisional_count = provisional.len();
    diagnostics.validated_count = validated
        .iter()
        .filter(|s| s.validation_state == SuggestionValidationState::Validated)
        .count();
    diagnostics.rejected_count = rejected_count;
    diagnostics.rejected_evidence_skipped_count = rejected_evidence_skipped_ids.len();
    diagnostics.validation_rejection_histogram =
        build_validation_rejection_histogram(&rejection_stats);
    diagnostics.validation_deadline_exceeded = rejection_stats.deadline_exceeded;
    diagnostics.validation_deadline_ms = VALIDATION_RUN_DEADLINE_MS;
    diagnostics.validation_transport_retry_count = rejection_stats.transport_retry_count;
    diagnostics.validation_transport_recovered_count = rejection_stats.transport_recovered_count;
    diagnostics.regen_stopped_validation_budget = regen_stopped_validation_budget;
    diagnostics.overclaim_rewrite_count = rejection_stats.overclaim_rewrite_count;
    diagnostics.overclaim_rewrite_validated_count =
        rejection_stats.overclaim_rewrite_validated_count;
    diagnostics.smart_rewrite_count = smart_rewrite_count;
    diagnostics.deterministic_auto_validated_count = rejection_stats.deterministic_auto_validated;
    diagnostics.semantic_dedup_dropped_count = semantic_dedup_dropped_count;
    diagnostics.file_balance_dropped_count = file_balance_dropped_count;
    diagnostics.speculative_impact_dropped_count = speculative_impact_dropped_count;
    diagnostics.dominant_topic_ratio = diversity_metrics.dominant_topic_ratio;
    diagnostics.unique_topic_count = diversity_metrics.unique_topic_count;
    diagnostics.dominant_file_ratio = diversity_metrics.dominant_file_ratio;
    diagnostics.unique_file_count = diversity_metrics.unique_file_count;
    diagnostics.readiness_filtered_count = readiness_filtered_count;
    diagnostics.readiness_score_mean = readiness_score_mean;
    diagnostics.regeneration_attempts = regeneration_attempts;
    diagnostics.refinement_complete = true;
    diagnostics.final_count = validated.len();
    diagnostics.deduped_count = validated.len();
    diagnostics.parse_strategy = "fast_grounded_refined".to_string();
    diagnostics.notes = notes;
    if diagnostics.readiness_filtered_count > 0 {
        diagnostics.notes.push(format!(
            "readiness_filtered:{}",
            diagnostics.readiness_filtered_count
        ));
    }
    if diagnostics.semantic_dedup_dropped_count > 0 {
        diagnostics.notes.push(format!(
            "semantic_dedup_dropped:{}",
            diagnostics.semantic_dedup_dropped_count
        ));
    }
    if diagnostics.file_balance_dropped_count > 0 {
        diagnostics.notes.push(format!(
            "file_balance_dropped:{}",
            diagnostics.file_balance_dropped_count
        ));
    }
    if diagnostics.speculative_impact_dropped_count > 0 {
        diagnostics.notes.push(format!(
            "speculative_impact_dropped:{}",
            diagnostics.speculative_impact_dropped_count
        ));
    }
    if diagnostics.dominant_topic_ratio > DIVERSITY_DOMINANT_TOPIC_RATIO_MAX {
        diagnostics.notes.push(format!(
            "dominant_topic_ratio:{:.2}",
            diagnostics.dominant_topic_ratio
        ));
    }
    let min_unique_topics = DIVERSITY_MIN_UNIQUE_TOPICS.min(validated.len().max(1));
    if diagnostics.unique_topic_count < min_unique_topics {
        diagnostics.notes.push(format!(
            "unique_topic_count:{} below {}",
            diagnostics.unique_topic_count, min_unique_topics
        ));
    }
    if diagnostics.dominant_file_ratio > DIVERSITY_DOMINANT_FILE_RATIO_MAX {
        diagnostics.notes.push(format!(
            "dominant_file_ratio:{:.2}",
            diagnostics.dominant_file_ratio
        ));
    }
    let min_unique_files = DIVERSITY_MIN_UNIQUE_FILES.min(validated.len().max(1));
    if diagnostics.unique_file_count < min_unique_files {
        diagnostics.notes.push(format!(
            "unique_file_count:{} below {}",
            diagnostics.unique_file_count, min_unique_files
        ));
    }
    if validated.len() < hard_target {
        diagnostics
            .notes
            .push("count_below_hard_target".to_string());
    }
    if validated.len() < stretch_target {
        diagnostics
            .notes
            .push("count_below_stretch_target".to_string());
    }
    if regeneration_needed(validated.len()) > 0 {
        diagnostics.notes.push("count_below_soft_floor".to_string());
    }
    if diagnostics.validation_deadline_exceeded {
        diagnostics
            .notes
            .push("validation_deadline_exceeded".to_string());
    }

    Ok((validated, usage, diagnostics))
}

fn ratio(numerator: usize, denominator: usize) -> f64 {
    if denominator == 0 {
        0.0
    } else {
        numerator as f64 / denominator as f64
    }
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
        .filter(|s| s.validation_state == SuggestionValidationState::Validated)
        .count();
    let pending_count = final_count.saturating_sub(validated_count);
    let displayed_valid_ratio = ratio(validated_count, final_count);
    let below_readiness_count = suggestions
        .iter()
        .filter(|s| {
            s.implementation_readiness_score.unwrap_or(0.0)
                < config.min_implementation_readiness_score
        })
        .count();
    let diversity_metrics = compute_suggestion_diversity_metrics(suggestions);
    let min_unique_topics = DIVERSITY_MIN_UNIQUE_TOPICS.min(final_count.max(1));
    let min_unique_files = DIVERSITY_MIN_UNIQUE_FILES.min(final_count.max(1));

    let mut fail_reasons = Vec::new();
    if final_count < config.min_final_count {
        fail_reasons.push(format!(
            "final_count {} below min {}",
            final_count, config.min_final_count
        ));
    }
    if final_count > config.max_final_count {
        fail_reasons.push(format!(
            "final_count {} above max {}",
            final_count, config.max_final_count
        ));
    }
    if displayed_valid_ratio < config.min_displayed_valid_ratio {
        fail_reasons.push(format!(
            "displayed_valid_ratio {:.3} below {:.3}",
            displayed_valid_ratio, config.min_displayed_valid_ratio
        ));
    }
    if pending_count > 0 {
        fail_reasons.push(format!("pending_count {} > 0", pending_count));
    }
    if diversity_metrics.dominant_topic_ratio > DIVERSITY_DOMINANT_TOPIC_RATIO_MAX {
        fail_reasons.push(format!(
            "dominant_topic_ratio {:.3} above {:.3}",
            diversity_metrics.dominant_topic_ratio, DIVERSITY_DOMINANT_TOPIC_RATIO_MAX
        ));
    }
    if diversity_metrics.unique_topic_count < min_unique_topics {
        fail_reasons.push(format!(
            "unique_topic_count {} below {}",
            diversity_metrics.unique_topic_count, min_unique_topics
        ));
    }
    if diversity_metrics.dominant_file_ratio > DIVERSITY_DOMINANT_FILE_RATIO_MAX {
        fail_reasons.push(format!(
            "dominant_file_ratio {:.3} above {:.3}",
            diversity_metrics.dominant_file_ratio, DIVERSITY_DOMINANT_FILE_RATIO_MAX
        ));
    }
    if diversity_metrics.unique_file_count < min_unique_files {
        fail_reasons.push(format!(
            "unique_file_count {} below {}",
            diversity_metrics.unique_file_count, min_unique_files
        ));
    }
    if below_readiness_count > 0 {
        fail_reasons.push(format!(
            "implementation_readiness_below_min {} below {:.2}",
            below_readiness_count, config.min_implementation_readiness_score
        ));
    }
    if suggest_total_cost_usd > config.max_suggest_cost_usd {
        fail_reasons.push(format!(
            "suggest_total_cost_usd {:.6} above {:.6}",
            suggest_total_cost_usd, config.max_suggest_cost_usd
        ));
    }
    if suggest_total_ms > config.max_suggest_ms {
        fail_reasons.push(format!(
            "suggest_total_ms {} above {}",
            suggest_total_ms, config.max_suggest_ms
        ));
    }

    SuggestionGateSnapshot {
        final_count,
        displayed_valid_ratio,
        pending_count,
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

fn gate_snapshot_is_better(
    candidate: &SuggestionGateSnapshot,
    current: &SuggestionGateSnapshot,
) -> bool {
    let cand_key = (
        candidate.passed as u8,
        candidate.displayed_valid_ratio,
        -(candidate.dominant_topic_ratio),
        candidate.unique_topic_count,
        -(candidate.dominant_file_ratio),
        candidate.unique_file_count,
        candidate.final_count,
        -candidate.suggest_total_cost_usd,
        -(candidate.suggest_total_ms as f64),
    );
    let curr_key = (
        current.passed as u8,
        current.displayed_valid_ratio,
        -(current.dominant_topic_ratio),
        current.unique_topic_count,
        -(current.dominant_file_ratio),
        current.unique_file_count,
        current.final_count,
        -current.suggest_total_cost_usd,
        -(current.suggest_total_ms as f64),
    );
    cand_key > curr_key
}

fn should_retry_after_gate_miss(
    config: &SuggestionQualityGateConfig,
    gate: &SuggestionGateSnapshot,
    attempt_cost_usd: f64,
    remaining_budget_ms: u64,
) -> bool {
    if remaining_budget_ms < GATE_RETRY_MIN_REMAINING_BUDGET_MS {
        return false;
    }
    if attempt_cost_usd > config.max_suggest_cost_usd * GATE_RETRY_MAX_ATTEMPT_COST_FRACTION {
        return false;
    }
    gate.final_count < config.min_final_count
        || gate.displayed_valid_ratio < config.min_displayed_valid_ratio
        || gate.pending_count > 0
        || gate
            .fail_reasons
            .iter()
            .any(|reason| reason.starts_with("dominant_topic_ratio"))
        || gate
            .fail_reasons
            .iter()
            .any(|reason| reason.starts_with("unique_topic_count"))
        || gate
            .fail_reasons
            .iter()
            .any(|reason| reason.starts_with("dominant_file_ratio"))
        || gate
            .fail_reasons
            .iter()
            .any(|reason| reason.starts_with("unique_file_count"))
        || gate
            .fail_reasons
            .iter()
            .any(|reason| reason.starts_with("implementation_readiness_below_min"))
}

pub async fn run_fast_grounded_with_gate(
    repo_root: &Path,
    index: &CodebaseIndex,
    context: &WorkContext,
    repo_memory: Option<String>,
    summaries: Option<&HashMap<PathBuf, String>>,
    gate_config: SuggestionQualityGateConfig,
) -> anyhow::Result<GatedSuggestionRunResult> {
    run_fast_grounded_with_gate_with_progress(
        repo_root,
        index,
        context,
        repo_memory,
        summaries,
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
    summaries: Option<&HashMap<PathBuf, String>>,
    gate_config: SuggestionQualityGateConfig,
    mut on_progress: F,
) -> anyhow::Result<GatedSuggestionRunResult>
where
    F: FnMut(usize, usize, &SuggestionGateSnapshot, &SuggestionDiagnostics),
{
    let max_attempts = gate_config.max_attempts.max(1);
    let overall_start = std::time::Instant::now();
    let mut cumulative_usage: Option<Usage> = None;
    let mut best_result: Option<GatedSuggestionRunResult> = None;
    let mut last_error: Option<anyhow::Error> = None;
    let mut attempts_executed = 0usize;
    let mut next_attempt_model = Model::Speed;

    for attempt_index in 1..=max_attempts {
        if attempt_index > 1 {
            let remaining_budget_ms = gate_config
                .max_suggest_ms
                .saturating_sub(overall_start.elapsed().as_millis() as u64);
            if remaining_budget_ms < GATE_RETRY_MIN_REMAINING_BUDGET_MS {
                break;
            }
        }
        let attempt_model = if attempt_index == 1 {
            Model::Speed
        } else {
            next_attempt_model
        };
        let attempt_start = std::time::Instant::now();

        let analyze = analyze_codebase_fast_grounded(
            repo_root,
            index,
            context,
            repo_memory.clone(),
            summaries,
            attempt_model,
        )
        .await;
        let (provisional, usage_a, diagnostics) = match analyze {
            Ok(result) => result,
            Err(err) => {
                last_error = Some(err);
                continue;
            }
        };

        let refine = refine_grounded_suggestions(
            repo_root,
            index,
            context,
            repo_memory.clone(),
            summaries,
            attempt_model,
            attempt_model,
            provisional,
            gate_config.min_implementation_readiness_score,
            gate_config.max_smart_rewrites_per_run,
            diagnostics,
        )
        .await;
        let (suggestions, usage_b, mut diagnostics) = match refine {
            Ok(result) => result,
            Err(err) => {
                last_error = Some(err);
                continue;
            }
        };

        attempts_executed += 1;
        let attempt_usage = merge_usage(usage_a, usage_b);
        let attempt_cost_usd = attempt_usage.as_ref().map(|u| u.cost()).unwrap_or(0.0);
        let attempt_ms = attempt_start.elapsed().as_millis() as u64;
        cumulative_usage = merge_usage(cumulative_usage, attempt_usage.clone());
        let gate = build_gate_snapshot(&gate_config, &suggestions, attempt_ms, attempt_cost_usd);

        diagnostics.attempt_index = attempt_index;
        diagnostics.attempt_count = attempts_executed;
        diagnostics.gate_passed = gate.passed;
        diagnostics.gate_fail_reasons = gate.fail_reasons.clone();
        diagnostics.attempt_cost_usd = attempt_cost_usd;
        diagnostics.attempt_ms = attempt_ms;
        diagnostics.final_count = suggestions.len();
        diagnostics
            .notes
            .retain(|note| note != "quality_gate_failed");
        if !gate.passed {
            diagnostics.notes.push("quality_gate_failed".to_string());
        }

        on_progress(attempt_index, max_attempts, &gate, &diagnostics);
        let auto_validation_overused = diagnostics.provisional_count > 0
            && diagnostics
                .deterministic_auto_validated_count
                .saturating_mul(2)
                >= diagnostics.provisional_count;

        let candidate = GatedSuggestionRunResult {
            suggestions,
            usage: attempt_usage,
            diagnostics,
            gate: gate.clone(),
        };

        if best_result
            .as_ref()
            .map(|best| gate_snapshot_is_better(&candidate.gate, &best.gate))
            .unwrap_or(true)
        {
            best_result = Some(candidate);
        }

        if gate.passed {
            break;
        }

        if attempt_index < max_attempts {
            let diversity_gate_failed = gate
                .fail_reasons
                .iter()
                .any(|reason| reason.starts_with("dominant_topic_ratio"))
                || gate
                    .fail_reasons
                    .iter()
                    .any(|reason| reason.starts_with("unique_topic_count"))
                || gate
                    .fail_reasons
                    .iter()
                    .any(|reason| reason.starts_with("dominant_file_ratio"))
                || gate
                    .fail_reasons
                    .iter()
                    .any(|reason| reason.starts_with("unique_file_count"));
            next_attempt_model = if diversity_gate_failed || auto_validation_overused {
                Model::Smart
            } else {
                Model::Speed
            };

            let remaining_budget_ms = gate_config
                .max_suggest_ms
                .saturating_sub(overall_start.elapsed().as_millis() as u64);
            if !should_retry_after_gate_miss(
                &gate_config,
                &gate,
                attempt_cost_usd,
                remaining_budget_ms,
            ) {
                break;
            }
        }
    }

    if let Some(mut result) = best_result {
        result.diagnostics.attempt_count = attempts_executed.max(1);
        result.usage = cumulative_usage.clone();
        if !result.gate.passed {
            let reasons = if result.gate.fail_reasons.is_empty() {
                "unknown quality gate failure".to_string()
            } else {
                result.gate.fail_reasons.join("; ")
            };
            result.diagnostics.notes.push(format!(
                "quality_gate_missed_best_effort:{}",
                truncate_str(&reasons, 240)
            ));
        }
        return Ok(result);
    }

    if let Some(err) = last_error {
        return Err(err);
    }

    Err(anyhow::anyhow!(
        "Suggestion quality gate failed without any successful generation attempts"
    ))
}

#[cfg(test)]
mod tests;
