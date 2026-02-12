use super::client::{
    call_llm_structured_limited, call_llm_structured_with_provider, call_llm_with_usage,
    truncate_str, StructuredResponse,
};
use super::models::merge_usage;
use super::models::{Model, Usage};
use super::prompt_utils::format_repo_memory_section;
use super::prompts::{ASK_QUESTION_SYSTEM, FAST_GROUNDED_SUGGESTIONS_SYSTEM};
use crate::context::WorkContext;
use crate::index::{CodebaseIndex, PatternKind, PatternReliability, PatternSeverity, SymbolKind};
use crate::suggest::{Suggestion, SuggestionEvidenceRef, SuggestionValidationState};
use chrono::Utc;
use futures::future::join_all;
use regex::Regex;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use uuid::Uuid;

// ═══════════════════════════════════════════════════════════════════════════
//  THRESHOLDS AND CONSTANTS
// ═══════════════════════════════════════════════════════════════════════════

use crate::index::GOD_MODULE_LOC_THRESHOLD;

/// Complexity threshold above which a file is considered a "hotspot"
const HIGH_COMPLEXITY_THRESHOLD: f64 = 20.0;
const FAST_EVIDENCE_PACK_MAX_ITEMS: usize = 30;
const FAST_EVIDENCE_SNIPPET_LINES_BEFORE: usize = 5;
const FAST_EVIDENCE_SNIPPET_LINES_AFTER: usize = 8;
const FAST_GROUNDED_FINAL_TARGET_MIN: usize = 10;
const FAST_GROUNDED_FINAL_TARGET_MAX: usize = 15;
const FAST_GROUNDED_VALIDATED_SOFT_FLOOR: usize = 10;
const FAST_GROUNDED_VALIDATED_HARD_TARGET: usize = 10;
const FAST_GROUNDED_VALIDATED_STRETCH_TARGET: usize = 15;
const FAST_GROUNDED_PROVISIONAL_TARGET_MIN: usize = 18;
const FAST_GROUNDED_PROVISIONAL_TARGET_MAX: usize = 24;
const FAST_EVIDENCE_SOURCE_PATTERN_MAX: usize = 12;
const FAST_EVIDENCE_SOURCE_HOTSPOT_MAX: usize = 8;
const FAST_EVIDENCE_SOURCE_CORE_MAX: usize = 8;
const FAST_EVIDENCE_KIND_GOD_MODULE_MAX: usize = 4;
const REFINEMENT_HARD_PHASE_MAX_ATTEMPTS: usize = 3;
const REFINEMENT_STRETCH_PHASE_MAX_ATTEMPTS: usize = 1;
const GENERATION_TOPUP_MAX_CALLS: usize = 2;
const GENERATION_TOPUP_TIMEOUT_MS: u64 = 4_500;
const REGEN_STRICT_MIN_PACK_SIZE: usize = FAST_GROUNDED_PROVISIONAL_TARGET_MIN;
const SUGGEST_BALANCED_BUDGET_MS: u64 = 60_000;
const SUGGEST_GATE_BUDGET_MS: u64 = 70_000;
const GATE_RETRY_MIN_REMAINING_BUDGET_MS: u64 = 8_000;
const GATE_RETRY_MAX_ATTEMPT_COST_FRACTION: f64 = 0.70;
const VALIDATION_CONCURRENCY: usize = 3;
const VALIDATION_RETRY_CONCURRENCY: usize = 1;
const PRIMARY_REQUEST_MIN: usize = 14;
const PRIMARY_REQUEST_MAX: usize = 18;
const PRIMARY_REQUEST_MAX_TOKENS: u32 = 1_200;
const PRIMARY_REQUEST_TIMEOUT_MS: u64 = 6_200;
const TOPUP_REQUEST_MAX_TOKENS: u32 = 700;
const REGEN_REQUEST_MAX_TOKENS: u32 = 800;
const VALIDATOR_MAX_TOKENS: u32 = 90;
const VALIDATOR_TIMEOUT_MS: u64 = 4_500;
const VALIDATOR_RETRY_TIMEOUT_MS: u64 = 3_200;
const VALIDATOR_BATCH_MAX_TOKENS: u32 = 320;
const VALIDATOR_BATCH_TIMEOUT_BUFFER_MS: u64 = 1_600;
const VALIDATION_RETRY_MAX_PER_SUGGESTION: usize = 1;
const VALIDATION_RETRY_MIN_REMAINING_BUDGET_MS: u64 = 4_000;
const VALIDATION_RUN_DEADLINE_MS: u64 = 22_000;
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
const DEFAULT_MIN_IMPLEMENTATION_READINESS_SCORE: f32 = 0.45;
const DEFAULT_MAX_SMART_REWRITES_PER_RUN: usize = 4;
const SMART_REWRITE_READINESS_UPPER_BOUND: f32 = 0.60;

// ═══════════════════════════════════════════════════════════════════════════
//  ADAPTIVE CONTEXT LIMITS
// ═══════════════════════════════════════════════════════════════════════════

/// Adaptive limits for context building based on codebase size
struct AdaptiveLimits {
    /// Max files to list in ask_question
    file_list_limit: usize,
    /// Max symbols to include
    symbol_limit: usize,
}

impl AdaptiveLimits {
    fn for_codebase(file_count: usize, _total_loc: usize) -> Self {
        // Scale limits based on codebase size
        // Smaller codebases: more detail per file
        // Larger codebases: broader coverage
        if file_count < 50 {
            // Small codebase: show more detail
            Self {
                file_list_limit: file_count.min(50),
                symbol_limit: 150,
            }
        } else if file_count < 200 {
            // Medium codebase: balanced
            Self {
                file_list_limit: 50,
                symbol_limit: 100,
            }
        } else if file_count < 500 {
            // Large codebase: prioritize structure
            Self {
                file_list_limit: 40,
                symbol_limit: 80,
            }
        } else {
            // Very large codebase: focus on key areas
            Self {
                file_list_limit: 30,
                symbol_limit: 60,
            }
        }
    }
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
            max_final_count: 15,
            min_displayed_valid_ratio: 1.0,
            min_implementation_readiness_score: DEFAULT_MIN_IMPLEMENTATION_READINESS_SCORE,
            max_smart_rewrites_per_run: DEFAULT_MAX_SMART_REWRITES_PER_RUN,
            max_suggest_cost_usd: 0.035,
            max_suggest_ms: SUGGEST_GATE_BUDGET_MS,
            max_attempts: 1,
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

fn best_function_anchor(file: &crate::index::FileIndex) -> usize {
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

fn build_evidence_pack(
    repo_root: &Path,
    index: &CodebaseIndex,
    context: &WorkContext,
) -> (Vec<EvidenceItem>, EvidencePackStats) {
    let changed: HashSet<PathBuf> = context.all_changed_files().into_iter().cloned().collect();
    let mut candidates: Vec<EvidenceCandidate> = Vec::new();

    // Patterns (high-signal, deterministic ranking with reliability weighting)
    for file in index.files.values() {
        for p in &file.patterns {
            let Some(rel_file) = normalize_repo_relative(repo_root, &p.file) else {
                continue;
            };
            let reliability = p.reliability;
            let severity = p.kind.severity();
            let pattern_bonus = match p.kind {
                PatternKind::MissingErrorHandling => 0.4,
                PatternKind::PotentialResourceLeak => 0.35,
                PatternKind::GodModule => -0.35,
                PatternKind::TodoMarker => -0.2,
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
        let anchor = best_function_anchor(f).max(1);
        if let Some(snippet) = read_snippet_around_line(repo_root, &rel_file, anchor) {
            let changed_boost = if changed.contains(&rel_file) {
                0.2
            } else {
                0.0
            };
            let score = 2.1 + (f.complexity / 60.0).min(1.0) + changed_boost;
            candidates.push(EvidenceCandidate {
                score,
                source_priority: source_priority(EvidenceSource::Hotspot),
                severity: PatternSeverity::High,
                item: EvidenceItem {
                    id: 0,
                    file: rel_file,
                    line: anchor,
                    snippet,
                    why_interesting: format!(
                        "Hotspot file (complexity {:.1}, {} LOC)",
                        f.complexity, f.loc
                    ),
                    source: EvidenceSource::Hotspot,
                    pattern_kind: None,
                },
            });
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
        let anchor = f
            .symbols
            .iter()
            .find(|s| {
                matches!(
                    s.kind,
                    SymbolKind::Function | SymbolKind::Struct | SymbolKind::Enum
                )
            })
            .map(|s| s.line)
            .unwrap_or(1);
        if let Some(snippet) = read_snippet_around_line(repo_root, &rel_file, anchor) {
            let changed_boost = if changed.contains(&rel_file) {
                0.15
            } else {
                0.0
            };
            let score = 1.7 + (f.summary.used_by.len() as f64 / 25.0).min(1.0) + changed_boost;
            candidates.push(EvidenceCandidate {
                score,
                source_priority: source_priority(EvidenceSource::Core),
                severity: PatternSeverity::Medium,
                item: EvidenceItem {
                    id: 0,
                    file: rel_file,
                    line: anchor.max(1),
                    snippet,
                    why_interesting: format!(
                        "Core file used by {} other files",
                        f.summary.used_by.len()
                    ),
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
            let anchor = best_function_anchor(f).max(1);
            if let Some(snippet) = read_snippet_around_line(repo_root, &rel_file, anchor) {
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
    let mut god_module_count = 0usize;

    for candidate in &candidates {
        let key = (candidate.item.file.clone(), candidate.item.line);
        if seen.contains(&key) {
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
            if candidate.item.pattern_kind == Some(PatternKind::GodModule)
                && god_module_count >= FAST_EVIDENCE_KIND_GOD_MODULE_MAX
            {
                continue;
            }
            if candidate.item.pattern_kind == Some(PatternKind::GodModule) {
                god_module_count += 1;
            }
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
    file: Option<String>,
    #[serde(default)]
    line: Option<usize>,
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
        #[serde(default)]
        file: Option<String>,
        #[serde(default)]
        line: Option<usize>,
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

    match normalized.as_str() {
        "validated" => (SuggestionValidationState::Validated, None),
        "contradicted" => (
            SuggestionValidationState::Rejected,
            Some(ValidationRejectClass::Contradicted),
        ),
        "insufficient_evidence" => (
            SuggestionValidationState::Rejected,
            Some(ValidationRejectClass::InsufficientEvidence),
        ),
        _ => (
            SuggestionValidationState::Rejected,
            Some(ValidationRejectClass::Other),
        ),
    }
}

fn reconcile_validation_from_reason(
    state: SuggestionValidationState,
    reject_class: Option<ValidationRejectClass>,
    reason: &str,
) -> (SuggestionValidationState, Option<ValidationRejectClass>) {
    if !(state == SuggestionValidationState::Rejected
        && matches!(reject_class, Some(ValidationRejectClass::Other)))
    {
        return (state, reject_class);
    }

    let lower = reason.to_ascii_lowercase();
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

fn extract_evidence_id(text: &str) -> Option<usize> {
    // Accept common variants:
    // - "EVIDENCE 12"
    // - "(EVIDENCE 12)"
    // - "evidence_id: 12"
    let hay = text.as_bytes();
    let needles: [&[u8]; 3] = [b"EVIDENCE ", b"evidence ", b"evidence_id"];

    for needle in needles {
        let mut i = 0;
        while i + needle.len() <= hay.len() {
            if &hay[i..i + needle.len()] == needle {
                let mut j = i + needle.len();
                while j < hay.len() && matches!(hay[j], b' ' | b'\t' | b':' | b'=') {
                    j += 1;
                }
                let start = j;
                while j < hay.len() && hay[j].is_ascii_digit() {
                    j += 1;
                }
                if j > start {
                    if let Ok(v) = std::str::from_utf8(&hay[start..j]).ok()?.parse::<usize>() {
                        return Some(v);
                    }
                }
            }
            i += 1;
        }
    }

    None
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
    ];
    markers.iter().any(|marker| text.contains(marker))
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

fn scrub_user_summary(summary: &str) -> String {
    // Extra safety: even if the model slips, ensure the user-facing title
    // doesn't contain file paths / line numbers / evidence markers.
    let mut s = summary.to_string();

    // Remove explicit evidence markers.
    let re_evidence = Regex::new(r"(?i)\b(evidence\s*id|evidence)\s*[:=]?\s*\d*\b")
        .unwrap_or_else(|_| Regex::new("$^").unwrap());
    s = re_evidence.replace_all(&s, "").to_string();

    // Remove "(path:123)" style suffixes.
    let re_path_line =
        Regex::new(r"\s*\(([^)]*/[^)]*?):\d+\)").unwrap_or_else(|_| Regex::new("$^").unwrap());
    s = re_path_line.replace_all(&s, "").to_string();

    // Remove bare path-like tokens ("src/foo.rs", "foo.tsx", etc).
    let re_path_token =
        Regex::new(r"(?i)\b[\w./-]+\.(rs|tsx|ts|jsx|js|py|go|java|kt|cs|cpp|c|h)\b")
            .unwrap_or_else(|_| Regex::new("$^").unwrap());
    s = re_path_token.replace_all(&s, "").to_string();

    // Collapse whitespace after removals.
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn is_low_information_summary(summary: &str) -> bool {
    let trimmed = summary.trim();
    if trimmed.len() < SUMMARY_MIN_CHARS {
        return true;
    }
    let words = trimmed.split_whitespace().count();
    if words < SUMMARY_MIN_WORDS {
        return true;
    }
    let lower = trimmed.to_ascii_lowercase();
    let normalized_lower = lower.trim_end_matches(['.', '!', '?']);
    let has_vague_hidden_errors = normalized_lower.ends_with("hidden errors")
        || normalized_lower.starts_with("hidden errors")
        || normalized_lower.contains("hidden errors when")
        || normalized_lower.contains(", hidden errors")
        || normalized_lower.contains(" hidden errors,");
    lower == "when users"
        || lower == "when someone"
        || lower == "when a user"
        || lower.starts_with("when ")
        || has_vague_hidden_errors
        || lower.ends_with(" may")
        || lower.ends_with(" can")
        || lower.ends_with(" should")
}

fn sentence_like_fragment(text: &str) -> Option<String> {
    let cleaned = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if cleaned.is_empty() {
        return None;
    }
    for raw in cleaned.split(['.', '!', '?']) {
        let candidate = scrub_user_summary(raw).trim().to_string();
        if candidate.len() >= SUMMARY_MIN_CHARS
            && candidate.split_whitespace().count() >= SUMMARY_MIN_WORDS
        {
            return Some(candidate);
        }
    }
    None
}

fn first_sentence_only(text: &str) -> String {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return String::new();
    }

    for (idx, ch) in trimmed.char_indices() {
        if matches!(ch, '.' | '!' | '?') {
            return trimmed[..=idx].trim().to_string();
        }
    }
    trimmed.to_string()
}

fn strip_formulaic_impact_clause(text: &str) -> String {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return String::new();
    }

    let lower = trimmed.to_ascii_lowercase();
    if let Some(idx) = lower.find("this matters because") {
        return trimmed[..idx].trim().to_string();
    }
    trimmed.to_string()
}

fn capitalize_first(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => format!("{}{}", first.to_uppercase(), chars.as_str()),
        None => String::new(),
    }
}

fn lowercase_first(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => format!("{}{}", first.to_lowercase(), chars.as_str()),
        None => String::new(),
    }
}

fn rewrite_when_lead_to_plain_sentence(summary: &str) -> String {
    let trimmed = summary.trim();
    if !trimmed.to_ascii_lowercase().starts_with("when ") {
        return trimmed.to_string();
    }

    let Some(comma_idx) = trimmed.find(',') else {
        return trimmed.to_string();
    };

    let condition = trimmed[5..comma_idx]
        .trim()
        .trim_end_matches(['.', '!', '?']);
    let outcome = trimmed[comma_idx + 1..]
        .trim()
        .trim_start_matches("then ")
        .trim()
        .trim_end_matches(['.', '!', '?']);

    if condition.is_empty() || outcome.is_empty() {
        return trimmed.to_string();
    }

    let outcome = capitalize_first(outcome);
    let condition = lowercase_first(condition);
    format!("{outcome} when {condition}.")
}

fn normalize_grounded_summary(summary: &str, detail: &str, _evidence_line: usize) -> String {
    let mut normalized = scrub_user_summary(summary);
    normalized = strip_formulaic_impact_clause(&normalized);
    normalized = first_sentence_only(&normalized);
    normalized = rewrite_when_lead_to_plain_sentence(&normalized);

    if is_low_information_summary(&normalized) {
        if let Some(from_detail) = sentence_like_fragment(detail) {
            let mut detail_sentence = strip_formulaic_impact_clause(&from_detail);
            detail_sentence = first_sentence_only(&detail_sentence);
            normalized = rewrite_when_lead_to_plain_sentence(&detail_sentence);
        }
    }
    if is_low_information_summary(&normalized) {
        normalized =
            "A user-facing reliability issue can cause visible broken behavior in this flow"
                .to_string();
    }
    let mut display = normalized.trim().to_string();
    if !display.ends_with('.') && !display.ends_with('!') && !display.ends_with('?') {
        display.push('.');
    }
    display
}

fn normalize_grounded_detail(detail: &str, summary: &str) -> String {
    let mut normalized = detail.split_whitespace().collect::<Vec<_>>().join(" ");
    if normalized.len() < 40 {
        let fallback = summary.trim();
        if !fallback.is_empty() {
            normalized = format!(
                "{}. This matters because users can observe incorrect behavior when this path runs.",
                fallback
            );
        }
    }
    if !normalized.ends_with('.') && !normalized.ends_with('!') && !normalized.ends_with('?') {
        normalized.push('.');
    }
    normalized
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

    let resolve_by_file_line = |file: &str, line: Option<usize>| -> Option<usize> {
        let normalized = file.replace('\\', "/");
        let target_line = line.unwrap_or(0);

        // First try exact path match with nearest line.
        let mut best_exact: Option<(usize, usize)> = None; // (distance, id)
        for item in pack {
            let item_path = item.file.to_string_lossy().replace('\\', "/");
            if item_path == normalized || normalized.ends_with(&item_path) {
                let distance = if target_line > 0 {
                    item.line.abs_diff(target_line)
                } else {
                    0
                };
                match best_exact {
                    Some((best_dist, _)) if distance >= best_dist => {}
                    _ => best_exact = Some((distance, item.id)),
                }
            }
        }
        if let Some((_, id)) = best_exact {
            return Some(id);
        }

        // Fallback to basename match if unique.
        let file_name = std::path::Path::new(&normalized)
            .file_name()
            .and_then(|n| n.to_str())?;
        let mut matched: Option<usize> = None;
        for item in pack {
            let candidate = item.file.file_name().and_then(|n| n.to_str());
            if candidate == Some(file_name) {
                if matched.is_some() {
                    return None;
                }
                matched = Some(item.id);
            }
        }
        matched
    };

    let infer_id_from_text = |text: &str| -> Option<usize> {
        let text_norm = text.to_lowercase();
        for item in pack {
            let item_path = item
                .file
                .to_string_lossy()
                .replace('\\', "/")
                .to_lowercase();
            if text_norm.contains(&item_path) {
                return Some(item.id);
            }
        }
        None
    };

    let parse_ref_id = |reference: &FastGroundedEvidenceRefJson| -> Option<usize> {
        match reference {
            FastGroundedEvidenceRefJson::Object {
                evidence_id,
                snippet_id,
                file,
                line,
            } => (*evidence_id)
                .or(*snippet_id)
                .or_else(|| file.as_deref().and_then(|f| resolve_by_file_line(f, *line))),
            FastGroundedEvidenceRefJson::Integer(id) => Some(*id),
            FastGroundedEvidenceRefJson::String(raw) => raw
                .trim()
                .parse::<usize>()
                .ok()
                .or_else(|| extract_evidence_id(raw))
                .or_else(|| infer_id_from_text(raw)),
        }
    };

    for r in &suggestion.evidence_refs {
        if let Some(id) = parse_ref_id(r) {
            push_evidence_id(id, pack, &mut seen, &mut refs);
        }
    }

    // Backward compatibility: older suggestion shape used top-level `evidence_id`.
    if refs.is_empty() {
        if let Some(id) = suggestion.evidence_id {
            push_evidence_id(id, pack, &mut seen, &mut refs);
        }
    }

    if refs.is_empty() {
        if let Some(id) = suggestion.snippet_id {
            push_evidence_id(id, pack, &mut seen, &mut refs);
        }
    }

    if refs.is_empty() {
        if let Some(id) = suggestion
            .file
            .as_deref()
            .and_then(|f| resolve_by_file_line(f, suggestion.line))
        {
            push_evidence_id(id, pack, &mut seen, &mut refs);
        }
    }

    // Last-chance compatibility: extract explicit evidence markers from text.
    if refs.is_empty() {
        if let Some(id) = suggestion
            .evidence_id
            .or(suggestion.snippet_id)
            .or_else(|| extract_evidence_id(&suggestion.summary))
            .or_else(|| extract_evidence_id(&suggestion.detail))
            .or_else(|| infer_id_from_text(&suggestion.summary))
            .or_else(|| infer_id_from_text(&suggestion.detail))
        {
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
        "bugfix" => crate::suggest::SuggestionKind::BugFix,
        "optimization" => crate::suggest::SuggestionKind::Optimization,
        "refactoring" => crate::suggest::SuggestionKind::Refactoring,
        "security" => crate::suggest::SuggestionKind::BugFix,
        "reliability" => crate::suggest::SuggestionKind::Quality,
        _ => crate::suggest::SuggestionKind::Improvement,
    };
    let priority = match s.priority.to_lowercase().as_str() {
        "high" => crate::suggest::Priority::High,
        "low" => crate::suggest::Priority::Low,
        _ => crate::suggest::Priority::Medium,
    };
    let confidence = match s.confidence.to_lowercase().as_str() {
        "high" => crate::suggest::Confidence::High,
        _ => crate::suggest::Confidence::Medium,
    };

    let detail = normalize_grounded_detail(&s.detail, &s.summary);
    let summary = normalize_grounded_summary(&s.summary, &detail, item.line);

    let suggestion = Suggestion::new(
        kind,
        priority,
        item.file.clone(),
        summary,
        crate::suggest::SuggestionSource::LlmDeep,
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
    schema_name: &str,
    schema: serde_json::Value,
    max_tokens: u32,
    timeout_ms: u64,
) -> anyhow::Result<StructuredResponse<FastGroundedResponseJson>> {
    let primary = call_llm_structured_with_provider::<FastGroundedResponseJson>(
        system,
        user,
        Model::Speed,
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
                Model::Speed,
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
        Model::Speed,
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
                Model::Speed,
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
    rewritten.summary = normalize_grounded_summary(
        &summary_seed,
        &normalized_detail,
        suggestion.line.unwrap_or_default(),
    );
    rewritten.detail = Some(normalized_detail);
    Ok((rewritten, response.usage))
}

fn append_suggestion_quality_record(
    cache: &crate::cache::Cache,
    run_id: &str,
    suggestion: &Suggestion,
    outcome: &str,
    reason: Option<String>,
) {
    let record = crate::cache::SuggestionQualityRecord {
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
    cache: &crate::cache::Cache,
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

    None
}

fn snippet_contains_empty_catch(snippet: &str) -> bool {
    let mut code_lines = Vec::new();
    for line in snippet.lines() {
        let code = if let Some((_, rest)) = line.split_once('|') {
            rest
        } else {
            line
        };
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

    Some(
        "Deterministic validation: snippet shows an empty catch block and summary describes silent error handling."
            .to_string(),
    )
}

#[allow(clippy::too_many_arguments)]
fn accept_validated_suggestion(
    cache: &crate::cache::Cache,
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
    cache: &crate::cache::Cache,
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
        model: Model::Speed.id().to_string(),
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
        diagnostics.readiness_filtered_count = 0;
        diagnostics.readiness_score_mean = 0.0;
        diagnostics.notes = Vec::new();
        return Ok((Vec::new(), None, diagnostics));
    }

    let memory_section =
        format_repo_memory_section(repo_memory.as_deref(), "Repo conventions / decisions");
    let cache = crate::cache::Cache::new(repo_root);
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
    let (readiness_filtered, readiness_filtered_count, readiness_score_mean) =
        apply_readiness_filter(validated, min_implementation_readiness_score);
    let (validated, smart_rewrite_count, smart_rewrite_usage) = apply_selective_smart_rewrites(
        readiness_filtered,
        &memory_section,
        min_implementation_readiness_score,
        max_smart_rewrites_per_run,
        validation_deadline,
    )
    .await;
    usage = merge_usage(usage, smart_rewrite_usage);
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
        candidate.final_count,
        -candidate.suggest_total_cost_usd,
        -(candidate.suggest_total_ms as f64),
    );
    let curr_key = (
        current.passed as u8,
        current.displayed_valid_ratio,
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

    for attempt_index in 1..=max_attempts {
        if attempt_index > 1 {
            let remaining_budget_ms = gate_config
                .max_suggest_ms
                .saturating_sub(overall_start.elapsed().as_millis() as u64);
            if remaining_budget_ms < GATE_RETRY_MIN_REMAINING_BUDGET_MS {
                break;
            }
        }
        let attempt_start = std::time::Instant::now();

        let analyze = analyze_codebase_fast_grounded(
            repo_root,
            index,
            context,
            repo_memory.clone(),
            summaries,
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
            result
                .diagnostics
                .notes
                .push("quality_gate_failed_showing_best_effort".to_string());
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
mod grounded_parser_tests {
    use super::*;
    use crate::context::WorkContext;
    use crate::index::{
        CodebaseIndex, FileIndex, FileSummary, Language, Pattern, PatternKind, PatternReliability,
        Symbol, SymbolKind, Visibility,
    };
    use crate::suggest::{Priority, SuggestionKind, SuggestionSource};
    use chrono::Utc;
    use serde_json::json;
    use std::collections::{HashMap, HashSet};
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::time::{SystemTime, UNIX_EPOCH};

    fn test_suggestion(summary: &str) -> Suggestion {
        Suggestion::new(
            SuggestionKind::Improvement,
            Priority::Medium,
            std::path::PathBuf::from("src/lib.rs"),
            summary.to_string(),
            SuggestionSource::LlmDeep,
        )
    }

    fn test_evidence_item(id: usize) -> EvidenceItem {
        EvidenceItem {
            id,
            file: PathBuf::from(format!("src/file_{}.rs", id)),
            line: id + 1,
            snippet: format!("{}| let value = {};", id + 1, id),
            why_interesting: "test".to_string(),
            source: EvidenceSource::Pattern,
            pattern_kind: None,
        }
    }

    #[test]
    fn redacts_secret_like_tokens_from_snippets() {
        let snippet = r#"  10| const API_KEY = "sk-1234567890abcdefghijkl";
  11| authorization = "Bearer ghp_abcdefghijklmnopqrstuvwxyz123456";
  12| password = "super-secret-value";
"#;
        let redacted = redact_obvious_secrets(snippet);
        assert!(!redacted.contains("sk-1234567890abcdefghijkl"));
        assert!(!redacted.contains("ghp_abcdefghijklmnopqrstuvwxyz123456"));
        assert!(!redacted.contains("super-secret-value"));
        assert!(redacted.contains("<redacted-secret>"));
    }

    fn temp_root(label: &str) -> PathBuf {
        let mut root = std::env::temp_dir();
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        root.push(format!("cosmos_analysis_test_{}_{}", label, nanos));
        fs::create_dir_all(&root).unwrap();
        root
    }

    fn write_fixture_file(root: &Path, rel: &str, lines: usize) {
        let full = root.join(rel);
        if let Some(parent) = full.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        let mut content = String::new();
        for i in 1..=lines.max(8) {
            content.push_str(&format!("fn line_{}() {{}}\n", i));
        }
        fs::write(full, content).unwrap();
    }

    fn mk_file_index(
        rel: &str,
        loc: usize,
        complexity: f64,
        patterns: Vec<Pattern>,
        symbols: Vec<Symbol>,
        used_by: usize,
    ) -> (PathBuf, FileIndex) {
        let path = PathBuf::from(rel);
        let index = FileIndex {
            path: path.clone(),
            language: Language::Rust,
            loc,
            content_hash: format!("hash-{}", rel),
            symbols,
            dependencies: Vec::new(),
            patterns,
            complexity,
            last_modified: Utc::now(),
            summary: FileSummary {
                purpose: "test file".to_string(),
                exports: Vec::new(),
                used_by: (0..used_by)
                    .map(|i| PathBuf::from(format!("src/dep_{}.rs", i)))
                    .collect(),
                depends_on: Vec::new(),
            },
            layer: None,
            feature: None,
        };
        (path, index)
    }

    fn empty_context(root: &Path) -> WorkContext {
        WorkContext {
            branch: "test".to_string(),
            uncommitted_files: Vec::new(),
            staged_files: Vec::new(),
            untracked_files: Vec::new(),
            inferred_focus: None,
            modified_count: 0,
            repo_root: root.to_path_buf(),
        }
    }

    #[test]
    fn parses_legacy_top_level_evidence_id_shape() {
        let parsed: FastGroundedSuggestionJson = serde_json::from_value(json!({
            "evidence_id": 7,
            "kind": "bugfix",
            "priority": "high",
            "confidence": "high",
            "summary": "Legacy shape",
            "detail": "Still supported"
        }))
        .expect("legacy shape should deserialize");

        assert_eq!(parsed.evidence_id, Some(7));
        assert!(parsed.evidence_refs.is_empty());
    }

    #[test]
    fn parses_mixed_evidence_refs_shapes() {
        let parsed: FastGroundedSuggestionJson = serde_json::from_value(json!({
            "evidence_refs": [1, "2", {"evidence_id": 3}],
            "kind": "improvement",
            "priority": "medium",
            "confidence": "medium",
            "summary": "Mixed shape",
            "detail": "Accepted for robustness"
        }))
        .expect("mixed evidence_refs shape should deserialize");

        assert!(matches!(
            parsed.evidence_refs[0],
            FastGroundedEvidenceRefJson::Integer(1)
        ));
        assert!(matches!(
            parsed.evidence_refs[1],
            FastGroundedEvidenceRefJson::String(ref raw) if raw == "2"
        ));
        assert!(matches!(
            parsed.evidence_refs[2],
            FastGroundedEvidenceRefJson::Object {
                evidence_id: Some(3),
                ..
            }
        ));
    }

    #[test]
    fn parses_object_evidence_ref_with_snippet_and_file_line() {
        let parsed: FastGroundedSuggestionJson = serde_json::from_value(json!({
            "evidence_refs": [{
                "snippet_id": 5,
                "file": "src/main.rs",
                "line": 42
            }],
            "kind": "reliability",
            "priority": "high",
            "confidence": "medium",
            "summary": "Object shape",
            "detail": "Should deserialize robustly"
        }))
        .expect("object evidence ref shape should deserialize");

        match &parsed.evidence_refs[0] {
            FastGroundedEvidenceRefJson::Object {
                evidence_id,
                snippet_id,
                file,
                line,
            } => {
                assert_eq!(*evidence_id, None);
                assert_eq!(*snippet_id, Some(5));
                assert_eq!(file.as_deref(), Some("src/main.rs"));
                assert_eq!(*line, Some(42));
            }
            _ => panic!("expected object evidence ref"),
        }
    }

    #[test]
    fn extracts_evidence_id_from_common_text_markers() {
        assert_eq!(extract_evidence_id("EVIDENCE 12"), Some(12));
        assert_eq!(extract_evidence_id("evidence_id: 4"), Some(4));
        assert_eq!(extract_evidence_id("No marker here"), None);
    }

    #[test]
    fn grounded_finalizer_does_not_backfill_duplicates() {
        let mapped = vec![
            (1, test_suggestion("a")),
            (1, test_suggestion("a-duplicate")),
            (2, test_suggestion("b")),
            (2, test_suggestion("b-duplicate")),
        ];

        let result =
            dedupe_and_cap_grounded_suggestions(mapped, FAST_GROUNDED_PROVISIONAL_TARGET_MAX);

        assert_eq!(result.len(), 2);
    }

    #[test]
    fn grounded_finalizer_caps_results_at_provisional_target_max() {
        let mapped: Vec<(usize, Suggestion)> = (0..40)
            .map(|i| (i, test_suggestion(&format!("item-{}", i))))
            .collect();

        let result =
            dedupe_and_cap_grounded_suggestions(mapped, FAST_GROUNDED_PROVISIONAL_TARGET_MAX);

        assert_eq!(result.len(), FAST_GROUNDED_PROVISIONAL_TARGET_MAX);
    }

    #[test]
    fn build_evidence_pack_is_deterministic_with_tie_breakers() {
        let root = temp_root("deterministic");
        write_fixture_file(&root, "src/a.rs", 80);
        write_fixture_file(&root, "src/b.rs", 80);
        write_fixture_file(&root, "src/c.rs", 80);

        let mut files = HashMap::new();
        for rel in ["src/a.rs", "src/b.rs", "src/c.rs"] {
            let pattern = Pattern {
                kind: PatternKind::MissingErrorHandling,
                file: PathBuf::from(rel),
                line: 12,
                description: "Unchecked unwrap".to_string(),
                reliability: PatternReliability::High,
            };
            let symbol = Symbol {
                name: "handle".to_string(),
                kind: SymbolKind::Function,
                file: PathBuf::from(rel),
                line: 12,
                end_line: 30,
                complexity: 12.0,
                visibility: Visibility::Public,
            };
            let (path, index) = mk_file_index(rel, 120, 30.0, vec![pattern], vec![symbol], 3);
            files.insert(path, index);
        }
        let index = CodebaseIndex {
            root: root.clone(),
            files,
            index_errors: Vec::new(),
            git_head: None,
        };
        let context = empty_context(&root);

        let (pack_a, _) = build_evidence_pack(&root, &index, &context);
        let (pack_b, _) = build_evidence_pack(&root, &index, &context);

        let ids_a: Vec<_> = pack_a.iter().map(|i| (i.file.clone(), i.line)).collect();
        let ids_b: Vec<_> = pack_b.iter().map(|i| (i.file.clone(), i.line)).collect();
        assert_eq!(ids_a, ids_b);

        let first_paths: Vec<_> = pack_a
            .iter()
            .take(3)
            .map(|i| i.file.display().to_string())
            .collect();
        assert!(first_paths.windows(2).all(|w| w[0] <= w[1]));

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn build_evidence_pack_enforces_source_and_godmodule_quotas() {
        let root = temp_root("quotas");
        let mut files = HashMap::new();

        for i in 0..24 {
            let rel = format!("src/f{}.rs", i);
            write_fixture_file(&root, &rel, 120);
            let pattern = Pattern {
                kind: PatternKind::GodModule,
                file: PathBuf::from(&rel),
                line: 1,
                description: "Large module".to_string(),
                reliability: PatternReliability::Low,
            };
            let symbol = Symbol {
                name: format!("work_{}", i),
                kind: SymbolKind::Function,
                file: PathBuf::from(&rel),
                line: 40,
                end_line: 70,
                complexity: 20.0 + i as f64,
                visibility: Visibility::Public,
            };
            let (path, index) =
                mk_file_index(&rel, 900, 40.0 + i as f64, vec![pattern], vec![symbol], 4);
            files.insert(path, index);
        }

        let index = CodebaseIndex {
            root: root.clone(),
            files,
            index_errors: Vec::new(),
            git_head: None,
        };
        let context = empty_context(&root);
        let (pack, stats) = build_evidence_pack(&root, &index, &context);

        let godmodule_count = pack
            .iter()
            .filter(|item| item.pattern_kind == Some(PatternKind::GodModule))
            .count();
        assert!(stats.pattern_count <= FAST_EVIDENCE_SOURCE_PATTERN_MAX);
        assert!(godmodule_count <= FAST_EVIDENCE_KIND_GOD_MODULE_MAX);
        assert!(pack.len() <= FAST_EVIDENCE_PACK_MAX_ITEMS);
        assert!(stats.line1_ratio <= 0.5);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn godmodule_anchor_prefers_complex_function_line_not_line1() {
        let root = temp_root("godmodule_anchor");
        let rel = "src/module.rs";
        write_fixture_file(&root, rel, 140);

        let pattern = Pattern {
            kind: PatternKind::GodModule,
            file: PathBuf::from(rel),
            line: 1,
            description: "File has many lines".to_string(),
            reliability: PatternReliability::Low,
        };
        let symbol = Symbol {
            name: "critical_path".to_string(),
            kind: SymbolKind::Function,
            file: PathBuf::from(rel),
            line: 72,
            end_line: 110,
            complexity: 88.0,
            visibility: Visibility::Public,
        };
        let (path, index_file) = mk_file_index(rel, 300, 10.0, vec![pattern], vec![symbol], 0);
        let mut files = HashMap::new();
        files.insert(path, index_file);
        let index = CodebaseIndex {
            root: root.clone(),
            files,
            index_errors: Vec::new(),
            git_head: None,
        };
        let context = empty_context(&root);
        let (pack, _) = build_evidence_pack(&root, &index, &context);

        let godmodule_item = pack
            .iter()
            .find(|item| item.pattern_kind == Some(PatternKind::GodModule))
            .expect("expected godmodule evidence item");
        assert_eq!(godmodule_item.line, 72);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn regeneration_needed_uses_soft_floor_ten() {
        assert_eq!(regeneration_needed(0), 10);
        assert_eq!(regeneration_needed(1), 9);
        assert_eq!(regeneration_needed(9), 1);
        assert_eq!(regeneration_needed(10), 0);
        assert_eq!(regeneration_needed(14), 0);
    }

    #[test]
    fn finalize_validated_suggestions_drops_pending_and_caps_at_final_target_max() {
        let mut input = (0..24)
            .map(|i| {
                test_suggestion(&format!("v{}", i))
                    .with_validation_state(SuggestionValidationState::Validated)
            })
            .collect::<Vec<_>>();
        input.push(
            test_suggestion("pending").with_validation_state(SuggestionValidationState::Pending),
        );

        let out = finalize_validated_suggestions(input);
        assert_eq!(out.len(), FAST_GROUNDED_FINAL_TARGET_MAX);
        assert!(out
            .iter()
            .all(|s| s.validation_state == SuggestionValidationState::Validated));
    }

    #[test]
    fn should_run_mapping_rescue_only_when_raw_exists_and_mapped_is_empty() {
        assert!(should_run_mapping_rescue(3, 0));
        assert!(!should_run_mapping_rescue(0, 0));
        assert!(!should_run_mapping_rescue(3, 1));
    }

    #[test]
    fn generation_topup_decision_is_based_on_mapped_count_and_call_budget() {
        assert!(should_run_generation_topup(
            FAST_GROUNDED_VALIDATED_HARD_TARGET - 1,
            0,
            0,
            SUGGEST_BALANCED_BUDGET_MS
        ));
        assert!(!should_run_generation_topup(
            FAST_GROUNDED_VALIDATED_HARD_TARGET,
            0,
            0,
            SUGGEST_BALANCED_BUDGET_MS
        ));
        assert!(!should_run_generation_topup(
            0,
            GENERATION_TOPUP_MAX_CALLS,
            0,
            SUGGEST_BALANCED_BUDGET_MS
        ));
    }

    #[test]
    fn generation_topup_request_count_uses_deficit_plus_padding_with_cap() {
        assert_eq!(generation_topup_request_count(1), 4);
        assert_eq!(generation_topup_request_count(6), 9);
        assert_eq!(generation_topup_request_count(20), 10);
    }

    #[test]
    fn generation_topup_requires_remaining_budget() {
        assert!(should_run_generation_topup(
            FAST_GROUNDED_VALIDATED_HARD_TARGET - 1,
            0,
            0,
            SUGGEST_BALANCED_BUDGET_MS
        ));
        assert!(!should_run_generation_topup(
            FAST_GROUNDED_VALIDATED_HARD_TARGET - 1,
            0,
            SUGGEST_BALANCED_BUDGET_MS - GENERATION_TOPUP_TIMEOUT_MS + 1,
            SUGGEST_BALANCED_BUDGET_MS
        ));
    }

    #[test]
    fn regeneration_request_bounds_scale_and_clamp_to_range() {
        assert_eq!(regeneration_request_bounds(1), (4, 4));
        assert_eq!(regeneration_request_bounds(2), (4, 6));
        assert_eq!(regeneration_request_bounds(4), (8, 12));
        assert_eq!(regeneration_request_bounds(5), (10, 14));
        assert_eq!(regeneration_request_bounds(10), (12, 14));
    }

    #[test]
    fn regeneration_needed_for_target_uses_target_count() {
        assert_eq!(regeneration_needed_for_target(0, 15), 15);
        assert_eq!(regeneration_needed_for_target(10, 15), 5);
        assert_eq!(regeneration_needed_for_target(15, 15), 0);
        assert_eq!(regeneration_needed_for_target(18, 15), 0);
    }

    #[test]
    fn choose_regeneration_phase_target_prioritizes_hard_then_stretch_target() {
        assert_eq!(
            choose_regeneration_phase_target(
                9,
                FAST_GROUNDED_VALIDATED_HARD_TARGET,
                FAST_GROUNDED_VALIDATED_STRETCH_TARGET,
                0,
                0
            ),
            Some(FAST_GROUNDED_VALIDATED_HARD_TARGET)
        );
        assert_eq!(
            choose_regeneration_phase_target(
                9,
                FAST_GROUNDED_VALIDATED_HARD_TARGET,
                FAST_GROUNDED_VALIDATED_STRETCH_TARGET,
                REFINEMENT_HARD_PHASE_MAX_ATTEMPTS,
                0
            ),
            None
        );
        assert_eq!(
            choose_regeneration_phase_target(
                FAST_GROUNDED_VALIDATED_HARD_TARGET,
                FAST_GROUNDED_VALIDATED_HARD_TARGET,
                FAST_GROUNDED_VALIDATED_STRETCH_TARGET,
                REFINEMENT_HARD_PHASE_MAX_ATTEMPTS,
                0
            ),
            Some(FAST_GROUNDED_VALIDATED_STRETCH_TARGET)
        );
        assert_eq!(
            choose_regeneration_phase_target(
                FAST_GROUNDED_VALIDATED_HARD_TARGET,
                FAST_GROUNDED_VALIDATED_HARD_TARGET,
                FAST_GROUNDED_VALIDATED_STRETCH_TARGET,
                REFINEMENT_HARD_PHASE_MAX_ATTEMPTS,
                REFINEMENT_STRETCH_PHASE_MAX_ATTEMPTS
            ),
            None
        );
        assert_eq!(
            choose_regeneration_phase_target(
                FAST_GROUNDED_VALIDATED_STRETCH_TARGET,
                FAST_GROUNDED_VALIDATED_HARD_TARGET,
                FAST_GROUNDED_VALIDATED_STRETCH_TARGET,
                0,
                0
            ),
            None
        );
    }

    #[test]
    fn build_remaining_pack_excludes_rejected_evidence_when_strict_pack_is_large_enough() {
        let pack = (0..8).map(test_evidence_item).collect::<Vec<_>>();
        let used = HashSet::from([0usize]);
        let rejected = HashSet::from([1usize, 2usize]);

        let (remaining, used_relaxed_filter, skipped_rejected_ids) =
            build_remaining_pack_for_regeneration(&pack, &used, &rejected, false);

        assert!(!used_relaxed_filter);
        let remaining_ids = remaining.iter().map(|item| item.id).collect::<Vec<_>>();
        assert_eq!(remaining_ids, vec![3, 4, 5, 6, 7]);
        assert_eq!(skipped_rejected_ids, vec![1, 2]);
    }

    #[test]
    fn build_remaining_pack_relaxes_rejected_filter_once_when_strict_pack_is_too_small() {
        let pack = (0..8).map(test_evidence_item).collect::<Vec<_>>();
        let used = HashSet::from([0usize, 6usize, 7usize]);
        let rejected = HashSet::from([1usize, 2usize, 3usize]);

        let (remaining, used_relaxed_filter, skipped_rejected_ids) =
            build_remaining_pack_for_regeneration(&pack, &used, &rejected, true);

        assert!(used_relaxed_filter);
        let remaining_ids = remaining.iter().map(|item| item.id).collect::<Vec<_>>();
        assert_eq!(remaining_ids, vec![1, 2, 3, 4, 5]);
        assert!(skipped_rejected_ids.is_empty());
    }

    #[test]
    fn sort_validation_outcomes_restores_input_order_for_parallel_results() {
        let mut outcomes: Vec<ValidationOutcome> = vec![
            (
                2,
                test_suggestion("c"),
                0,
                SuggestionValidationState::Validated,
                "ok".to_string(),
                None,
                None,
            ),
            (
                0,
                test_suggestion("a"),
                0,
                SuggestionValidationState::Validated,
                "ok".to_string(),
                None,
                None,
            ),
            (
                1,
                test_suggestion("b"),
                0,
                SuggestionValidationState::Rejected,
                "no".to_string(),
                None,
                Some(ValidationRejectClass::Other),
            ),
        ];
        sort_validation_outcomes(&mut outcomes);
        let summaries = outcomes
            .iter()
            .map(|(_, suggestion, _, _, _, _, _)| suggestion.summary.clone())
            .collect::<Vec<_>>();
        assert_eq!(summaries, vec!["a", "b", "c"]);
    }

    #[test]
    fn should_stop_regeneration_for_validation_budget_blocks_deadline_or_low_budget() {
        assert!(should_stop_regeneration_for_validation_budget(true, 10_000));
        assert!(should_stop_regeneration_for_validation_budget(
            false,
            VALIDATION_MIN_REMAINING_BUDGET_MS - 1
        ));
        assert!(!should_stop_regeneration_for_validation_budget(
            false,
            VALIDATION_MIN_REMAINING_BUDGET_MS
        ));
    }

    #[test]
    fn should_retry_transport_rejection_allows_single_retry_with_time_remaining() {
        let future_deadline = std::time::Instant::now()
            + std::time::Duration::from_millis(VALIDATION_RETRY_MIN_REMAINING_BUDGET_MS + 200);
        let near_deadline = std::time::Instant::now()
            + std::time::Duration::from_millis(VALIDATION_RETRY_MIN_REMAINING_BUDGET_MS - 100);
        let past_deadline = std::time::Instant::now() - std::time::Duration::from_millis(1);
        assert!(should_retry_transport_rejection(
            ValidationRejectClass::Transport,
            0,
            future_deadline
        ));
        assert!(!should_retry_transport_rejection(
            ValidationRejectClass::Transport,
            VALIDATION_RETRY_MAX_PER_SUGGESTION,
            future_deadline
        ));
        assert!(!should_retry_transport_rejection(
            ValidationRejectClass::Contradicted,
            0,
            future_deadline
        ));
        assert!(!should_retry_transport_rejection(
            ValidationRejectClass::Transport,
            0,
            past_deadline
        ));
        assert!(!should_retry_transport_rejection(
            ValidationRejectClass::Transport,
            0,
            near_deadline
        ));
    }

    #[test]
    fn prevalidation_rejection_reason_catches_missing_and_duplicate_primary_evidence() {
        let mut chunk_seen_evidence_ids: HashSet<usize> = HashSet::new();
        let used_evidence_ids: HashSet<usize> = HashSet::new();

        let missing = test_suggestion("missing refs");
        let missing_reason = prevalidation_rejection_reason(
            &missing,
            &used_evidence_ids,
            &mut chunk_seen_evidence_ids,
        )
        .expect("missing evidence should be rejected");
        assert!(missing_reason.0.contains("Missing primary evidence ref"));
        assert!(missing_reason.1.is_none());

        let first = test_suggestion("first").with_evidence_refs(vec![SuggestionEvidenceRef {
            snippet_id: 3,
            file: PathBuf::from("src/a.rs"),
            line: 10,
        }]);
        assert!(prevalidation_rejection_reason(
            &first,
            &used_evidence_ids,
            &mut chunk_seen_evidence_ids
        )
        .is_none());

        let duplicate_in_chunk =
            test_suggestion("duplicate chunk").with_evidence_refs(vec![SuggestionEvidenceRef {
                snippet_id: 3,
                file: PathBuf::from("src/a.rs"),
                line: 10,
            }]);
        let duplicate_reason = prevalidation_rejection_reason(
            &duplicate_in_chunk,
            &used_evidence_ids,
            &mut chunk_seen_evidence_ids,
        )
        .expect("duplicate in batch should be rejected");
        assert!(duplicate_reason
            .0
            .contains("Duplicate evidence_id in validation batch"));
        assert_eq!(duplicate_reason.1, Some(3));

        let mut chunk_seen_evidence_ids: HashSet<usize> = HashSet::new();
        let used_evidence_ids = HashSet::from([9usize]);
        let duplicate_used =
            test_suggestion("duplicate used").with_evidence_refs(vec![SuggestionEvidenceRef {
                snippet_id: 9,
                file: PathBuf::from("src/b.rs"),
                line: 22,
            }]);
        let duplicate_used_reason = prevalidation_rejection_reason(
            &duplicate_used,
            &used_evidence_ids,
            &mut chunk_seen_evidence_ids,
        )
        .expect("duplicate against used set should be rejected");
        assert!(duplicate_used_reason
            .0
            .contains("Duplicate evidence_id already validated"));
        assert_eq!(duplicate_used_reason.1, Some(9));
    }

    #[test]
    fn remap_suggestion_to_original_ids_handles_non_contiguous_ids() {
        let full_pack = vec![
            EvidenceItem {
                id: 10,
                file: PathBuf::from("src/a.rs"),
                line: 7,
                snippet: "7| let a = 1;".to_string(),
                why_interesting: "pattern".to_string(),
                source: EvidenceSource::Pattern,
                pattern_kind: Some(PatternKind::MissingErrorHandling),
            },
            EvidenceItem {
                id: 42,
                file: PathBuf::from("src/b.rs"),
                line: 11,
                snippet: "11| let b = 2;".to_string(),
                why_interesting: "hotspot".to_string(),
                source: EvidenceSource::Hotspot,
                pattern_kind: None,
            },
        ];
        let (local_pack, local_to_original) = renumber_pack(&full_pack);
        assert_eq!(local_pack[0].id, 0);
        assert_eq!(local_pack[1].id, 1);

        let mut suggestion = test_suggestion("local-id")
            .with_line(local_pack[1].line)
            .with_evidence(local_pack[1].snippet.clone())
            .with_evidence_refs(vec![SuggestionEvidenceRef {
                snippet_id: 1,
                file: local_pack[1].file.clone(),
                line: local_pack[1].line,
            }]);

        assert!(remap_suggestion_to_original_ids(
            &mut suggestion,
            &local_to_original,
            &full_pack
        ));
        assert_eq!(suggestion.evidence_refs[0].snippet_id, 42);
        assert_eq!(suggestion.file, PathBuf::from("src/b.rs"));
        assert_eq!(suggestion.line, Some(11));
    }

    #[test]
    fn finalize_validated_suggestions_drops_pending_without_backfill() {
        let out = finalize_validated_suggestions(vec![
            test_suggestion("v1").with_validation_state(SuggestionValidationState::Validated),
            test_suggestion("pending").with_validation_state(SuggestionValidationState::Pending),
            test_suggestion("v2").with_validation_state(SuggestionValidationState::Validated),
        ]);
        assert_eq!(out.len(), 2);
        assert!(out
            .iter()
            .all(|s| s.validation_state == SuggestionValidationState::Validated));
    }

    #[test]
    fn grounded_schema_enforces_single_evidence_ref() {
        let schema = grounded_suggestion_schema(10);
        let evidence_refs =
            &schema["properties"]["suggestions"]["items"]["properties"]["evidence_refs"];
        let evidence_id = &evidence_refs["items"]["properties"]["evidence_id"];
        assert!(evidence_refs.get("minItems").is_none());
        assert!(evidence_refs.get("maxItems").is_none());
        assert_eq!(evidence_id.get("minimum").and_then(|v| v.as_u64()), Some(0));
        assert_eq!(evidence_id.get("maximum").and_then(|v| v.as_u64()), Some(9));
    }

    #[test]
    fn collect_valid_evidence_refs_truncates_to_one_ref() {
        let pack = vec![test_evidence_item(0), test_evidence_item(1)];
        let suggestion = FastGroundedSuggestionJson {
            evidence_refs: vec![
                FastGroundedEvidenceRefJson::Integer(0),
                FastGroundedEvidenceRefJson::Integer(1),
            ],
            evidence_id: None,
            snippet_id: None,
            file: None,
            line: None,
            kind: "improvement".to_string(),
            priority: "medium".to_string(),
            confidence: "medium".to_string(),
            summary: "test".to_string(),
            detail: "detail".to_string(),
        };

        let refs = collect_valid_evidence_refs(&suggestion, &pack);
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].snippet_id, 0);
    }

    #[test]
    fn suggestion_batch_validation_schema_sets_local_index_bounds() {
        let schema = suggestion_batch_validation_schema(5);
        let local_index =
            &schema["properties"]["validations"]["items"]["properties"]["local_index"];
        assert_eq!(local_index.get("minimum").and_then(|v| v.as_u64()), Some(0));
        assert_eq!(local_index.get("maximum").and_then(|v| v.as_u64()), Some(5));
    }

    #[test]
    fn map_batch_validation_response_fills_missing_entries() {
        let mapped = map_batch_validation_response(
            3,
            SuggestionBatchValidationJson {
                validations: vec![
                    SuggestionBatchValidationItemJson {
                        local_index: 1,
                        validation: "validated".to_string(),
                        reason: "supported by snippet".to_string(),
                    },
                    SuggestionBatchValidationItemJson {
                        local_index: 2,
                        validation: "unexpected".to_string(),
                        reason: String::new(),
                    },
                    SuggestionBatchValidationItemJson {
                        local_index: 9,
                        validation: "validated".to_string(),
                        reason: "ignored out of range".to_string(),
                    },
                ],
            },
        );

        assert_eq!(mapped.len(), 3);

        let (state0, reason0, class0) = &mapped[0];
        assert_eq!(*state0, SuggestionValidationState::Rejected);
        assert!(reason0.contains("missing batch result"));
        assert!(matches!(class0, Some(ValidationRejectClass::Transport)));

        let (state1, _reason1, class1) = &mapped[1];
        assert_eq!(*state1, SuggestionValidationState::Validated);
        assert!(class1.is_none());

        let (state2, reason2, class2) = &mapped[2];
        assert_eq!(*state2, SuggestionValidationState::Rejected);
        assert!(reason2.contains("no reason"));
        assert!(matches!(class2, Some(ValidationRejectClass::Other)));
    }

    #[test]
    fn gate_snapshot_reports_fail_reasons_for_count_and_cost() {
        let config = SuggestionQualityGateConfig::default();
        let suggestions = vec![
            test_suggestion("one").with_validation_state(SuggestionValidationState::Validated),
            test_suggestion("two").with_validation_state(SuggestionValidationState::Validated),
        ];
        let gate = build_gate_snapshot(&config, &suggestions, 3_000, 0.04);
        assert!(!gate.passed);
        assert!(gate
            .fail_reasons
            .iter()
            .any(|reason| reason.contains("final_count")));
        assert!(gate
            .fail_reasons
            .iter()
            .any(|reason| reason.contains("suggest_total_cost_usd")));
    }

    #[test]
    fn gate_snapshot_prefers_higher_validity_and_count() {
        let better = SuggestionGateSnapshot {
            final_count: 12,
            displayed_valid_ratio: 1.0,
            pending_count: 0,
            suggest_total_ms: 20_000,
            suggest_total_cost_usd: 0.012,
            passed: true,
            fail_reasons: Vec::new(),
        };
        let worse = SuggestionGateSnapshot {
            final_count: 8,
            displayed_valid_ratio: 0.9,
            pending_count: 0,
            suggest_total_ms: 15_000,
            suggest_total_cost_usd: 0.010,
            passed: false,
            fail_reasons: vec!["count".to_string()],
        };
        assert!(gate_snapshot_is_better(&better, &worse));
        assert!(!gate_snapshot_is_better(&worse, &better));
    }

    #[test]
    fn overclaim_reason_detector_matches_expected_markers() {
        assert!(is_overclaim_validation_reason(
            "Suggestion makes assumptions beyond evidence about business impact"
        ));
        assert!(is_overclaim_validation_reason(
            "Claims UI behavior without proof from snippet"
        ));
        assert!(!is_overclaim_validation_reason(
            "Validation failed: deadline exceeded"
        ));
    }

    #[test]
    fn normalize_grounded_summary_avoids_dangling_when_users_titles() {
        let summary = normalize_grounded_summary(
            "When users",
            "When users submit malformed HTML, the raw message is passed through without escaping and can render unsafely in email clients.",
            42,
        );
        assert!(summary.len() >= SUMMARY_MIN_CHARS);
        assert_ne!(summary.to_ascii_lowercase(), "when users");
    }

    #[test]
    fn normalize_grounded_summary_rewrites_when_template_to_plain_sentence() {
        let summary = normalize_grounded_summary(
            "When the page hides, CLS errors are ignored, so layout-shift problems may go unnoticed. This matters because undetected CLS bugs can degrade user experience.",
            "CLS metric updates can fail silently during page hide events.",
            42,
        );
        let lower = summary.to_ascii_lowercase();
        assert!(!lower.starts_with("when "));
        assert!(!lower.contains("this matters because"));
        assert!(lower.contains("when the page hides"));
    }

    #[test]
    fn normalize_grounded_summary_replaces_vague_hidden_errors_phrase() {
        let summary = normalize_grounded_summary(
            "When the page experiences layout shifts, hidden errors.",
            "Layout-shift metric collection errors are swallowed, so the CLS metric is never reported to analytics.",
            42,
        );
        let lower = summary.to_ascii_lowercase();
        assert!(!lower.contains("hidden errors"));
        assert!(lower.contains("cls metric"));
    }

    #[test]
    fn normalize_grounded_summary_discourages_when_openers_without_comma() {
        let summary = normalize_grounded_summary(
            "When users save settings they may lose data",
            "Saving settings can silently fail, so people think their changes were saved when they were not.",
            42,
        );
        let lower = summary.to_ascii_lowercase();
        assert!(!lower.starts_with("when "));
        assert!(lower.contains("saving settings"));
    }

    #[test]
    fn normalize_grounded_summary_rewrites_low_information_summary_from_detail() {
        let summary = normalize_grounded_summary(
            "Fix issue",
            "Parsing failures currently return a default value silently, which hides bad input and makes debugging harder.",
            42,
        );
        let lower = summary.to_ascii_lowercase();
        assert_ne!(lower, "fix issue");
        assert!(lower.contains("parsing failures"));
    }

    #[test]
    fn deterministic_auto_validation_accepts_empty_catch_with_silent_error_language() {
        let suggestion = test_suggestion("Errors are silently ignored in this flow.")
            .with_detail("A catch block is empty, so failures are not logged.".to_string())
            .with_evidence(
                " 10| try {\n 11|   runTask();\n 12| } catch (error) {\n 13| }\n".to_string(),
            )
            .with_evidence_refs(vec![SuggestionEvidenceRef {
                snippet_id: 7,
                file: PathBuf::from("src/a.ts"),
                line: 12,
            }]);

        let reason = deterministic_auto_validation_reason(&suggestion);
        assert!(reason.is_some());
    }

    #[test]
    fn deterministic_auto_validation_rejects_non_empty_catch() {
        let suggestion = test_suggestion("Errors are silently ignored in this flow.")
            .with_detail("A catch block is empty, so failures are not logged.".to_string())
            .with_evidence(
                " 10| try {\n 11|   runTask();\n 12| } catch (error) {\n 13|   console.error(error);\n 14| }\n".to_string(),
            )
            .with_evidence_refs(vec![SuggestionEvidenceRef {
                snippet_id: 7,
                file: PathBuf::from("src/a.ts"),
                line: 12,
            }]);

        let reason = deterministic_auto_validation_reason(&suggestion);
        assert!(reason.is_none());
    }

    #[test]
    fn parse_validation_state_accepts_common_synonyms() {
        let (state, class) = parse_validation_state("supported_by_evidence");
        assert_eq!(state, SuggestionValidationState::Validated);
        assert!(class.is_none());

        let (state, class) = parse_validation_state("insufficient evidence");
        assert_eq!(state, SuggestionValidationState::Rejected);
        assert_eq!(class, Some(ValidationRejectClass::InsufficientEvidence));

        let (state, class) = parse_validation_state("not supported");
        assert_eq!(state, SuggestionValidationState::Rejected);
        assert_eq!(class, Some(ValidationRejectClass::Contradicted));
    }

    #[test]
    fn reconcile_validation_from_reason_recovers_supported_other_label() {
        let (state, class) = reconcile_validation_from_reason(
            SuggestionValidationState::Rejected,
            Some(ValidationRejectClass::Other),
            "Evidence contains an empty catch block, confirming this suggestion is supported.",
        );
        assert_eq!(state, SuggestionValidationState::Validated);
        assert!(class.is_none());
    }

    #[test]
    fn should_retry_after_gate_miss_skips_cost_only_misses() {
        let config = SuggestionQualityGateConfig::default();
        let gate = SuggestionGateSnapshot {
            final_count: config.min_final_count + 1,
            displayed_valid_ratio: config.min_displayed_valid_ratio,
            pending_count: 0,
            suggest_total_ms: config.max_suggest_ms + 100,
            suggest_total_cost_usd: config.max_suggest_cost_usd + 0.001,
            passed: false,
            fail_reasons: vec!["cost".to_string(), "latency".to_string()],
        };
        assert!(!should_retry_after_gate_miss(
            &config,
            &gate,
            config.max_suggest_cost_usd * 0.95,
            GATE_RETRY_MIN_REMAINING_BUDGET_MS + 1
        ));
    }

    #[test]
    fn choose_regeneration_phase_target_returns_stretch_after_hard_is_met() {
        let selected = choose_regeneration_phase_target(
            FAST_GROUNDED_VALIDATED_HARD_TARGET,
            FAST_GROUNDED_VALIDATED_HARD_TARGET,
            FAST_GROUNDED_VALIDATED_STRETCH_TARGET,
            REFINEMENT_HARD_PHASE_MAX_ATTEMPTS,
            0,
        );
        assert_eq!(selected, Some(FAST_GROUNDED_VALIDATED_STRETCH_TARGET));
    }
}

/* fn build_suggestion_diagnostics(
    model: Model,
    response_content: &str,
    trace: &super::agentic::AgenticTrace,
    parse: &ParseDiagnostics,
    raw_count: usize,
    deduped_count: usize,
    low_confidence_filtered: usize,
    truncated_count: usize,
    final_count: usize,
) -> SuggestionDiagnostics {
    SuggestionDiagnostics {
        model: model.id().to_string(),
        iterations: trace.iterations,
        tool_calls: trace.tool_calls,
        tool_names: trace.tool_names.clone(),
        forced_final: trace.forced_final,
        formatting_pass: trace.formatting_pass,
        response_format: true,
        response_healing: trace.response_healing_used,
        parse_strategy: parse.strategy.clone(),
        parse_stripped_markdown: parse.used_markdown_fences,
        parse_used_sanitized_fix: parse.used_sanitized_fix,
        parse_used_json_fix: parse.used_json_fix,
        parse_used_individual_parse: parse.used_individual_parse,
        raw_count,
        deduped_count,
        low_confidence_filtered,
        truncated_count,
        final_count,
        response_chars: response_content.len(),
        response_preview: truncate_str(response_content, 240).to_string(),
    }
}

/// Remove near-duplicate suggestions from a list
///
/// Considers suggestions duplicates if they match on any of:
/// - Same file + same summary (exact duplicate)
/// - Same file + same line (same location = likely same issue)
/// - Same file + same kind (same category in same file = often duplicate)
fn deduplicate_suggestions(suggestions: Vec<Suggestion>) -> Vec<Suggestion> {
    let mut seen_summaries: HashSet<(PathBuf, String)> = HashSet::new();
    let mut seen_locations: HashSet<(PathBuf, usize)> = HashSet::new();
    let mut seen_file_kinds: HashSet<(PathBuf, crate::suggest::SuggestionKind)> = HashSet::new();
    let mut result = Vec::new();

    for s in suggestions {
        let summary_key = (s.file.clone(), s.summary.clone());
        let location_key = s.line.map(|l| (s.file.clone(), l));
        let kind_key = (s.file.clone(), s.kind);

        let is_dup_summary = seen_summaries.contains(&summary_key);
        let is_dup_location = location_key
            .as_ref()
            .map(|k| seen_locations.contains(k))
            .unwrap_or(false);
        let is_dup_kind = seen_file_kinds.contains(&kind_key);

        if !is_dup_summary && !is_dup_location && !is_dup_kind {
            seen_summaries.insert(summary_key);
            if let Some(loc) = location_key {
                seen_locations.insert(loc);
            }
            seen_file_kinds.insert(kind_key);
            result.push(s);
        }
    }

    result
}

/// Build a lean prompt using summaries, not full file content
///
/// The model gets:
/// - Project context and purpose
/// - Compact file summaries (what each file does)
///
/// Tiers (from high-level to detailed):
/// 0. The Gist - what the project IS
/// 1. Key Areas - main directories/modules
/// 2. Priority Files - changed/complex with summaries
/// 3. All Files - just paths for scanning
/// 4. Code - model uses `head` to read
fn build_lean_analysis_prompt(
    index: &CodebaseIndex,
    context: &WorkContext,
    repo_memory: Option<&str>,
    glossary: Option<&DomainGlossary>,
) -> String {
    let stats = index.stats();
    let limits = AdaptiveLimits::for_codebase(stats.file_count, stats.total_loc);
    let project_name = index
        .root
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("project");

    let mut sections = Vec::new();

    // ═══ TIER 0: THE GIST ═══
    let project_context = discover_project_context(index);
    let pattern_summary = summarize_patterns(index);
    sections.push(format!(
        "═══ THE GIST ═══\n{} ({} files, {} LOC)\n{}{}",
        project_name,
        stats.file_count,
        stats.total_loc,
        if !project_context.trim().is_empty() {
            truncate_str(&project_context, 300).to_string()
        } else {
            "A software project.".to_string()
        },
        if !pattern_summary.is_empty() {
            format!("\n{}", pattern_summary)
        } else {
            String::new()
        }
    ));

    // ═══ TIER 1: KEY AREAS ═══
    let mut dirs: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for path in index.files.keys() {
        if let Some(parent) = path.parent() {
            let dir = parent.to_string_lossy().to_string();
            if !dir.is_empty() {
                *dirs.entry(dir).or_insert(0) += 1;
            }
        }
    }
    let mut dir_list: Vec<_> = dirs.into_iter().collect();
    dir_list.sort_by(|a, b| b.1.cmp(&a.1));

    if !dir_list.is_empty() {
        let areas: Vec<_> = dir_list
            .iter()
            .take(limits.key_areas_limit)
            .map(|(d, c)| format!("{}/ ({} files)", d, c))
            .collect();
        sections.push(format!("\n\n═══ KEY AREAS ═══\n{}", areas.join("\n")));
    }

    // ═══ TIER 2: PRIORITY FILES ═══
    let changed: HashSet<PathBuf> = context.all_changed_files().into_iter().cloned().collect();
    let mut changed_list: Vec<PathBuf> = changed.iter().cloned().collect();
    changed_list.sort();

    // We'll build a balanced set of preview files so the model doesn't
    // over-fit to whatever was recently edited.
    let mut priority_files: Vec<PathBuf> = Vec::new();

    let mut push_unique = |p: PathBuf| {
        if !priority_files.contains(&p) {
            priority_files.push(p);
        }
    };

    if !changed_list.is_empty() {
        let mut s = String::from(
            "\n\n═══ PRIORITY FILES ═══\nRECENT WORK (context only — don't limit suggestions to these):",
        );
        for path in changed_list.iter().take(limits.changed_files_limit) {
            let summary = index
                .files
                .get(path)
                .map(|f| f.summary.purpose.as_str())
                .unwrap_or("(new)");
            s.push_str(&format!(
                "\n• {} - {}",
                path.display(),
                truncate_str(summary, 60)
            ));
        }
        sections.push(s);
    }

    // Complex files
    let mut hotspots = index.files.values().collect::<Vec<_>>();
    hotspots.sort_by(|a, b| {
        b.complexity
            .partial_cmp(&a.complexity)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let hot: Vec<_> = hotspots
        .iter()
        .filter(|f| f.complexity > HIGH_COMPLEXITY_THRESHOLD || f.loc > GOD_MODULE_LOC_THRESHOLD)
        .filter(|f| !changed.contains(&f.path))
        .take(limits.complex_files_limit)
        .collect();

    // Core files: high fan-in (many other files use/import them)
    let mut core_files: Vec<_> = index.files.values().collect();
    core_files.sort_by(|a, b| b.summary.used_by.len().cmp(&a.summary.used_by.len()));
    let core = core_files
        .iter()
        .filter(|f| !changed.contains(&f.path))
        .filter(|f| f.summary.used_by.len() >= 2)
        .take(2)
        .map(|f| f.path.clone())
        .collect::<Vec<_>>();

    // Build preview list in a balanced order:
    // - one core file (if any)
    // - one hotspot (if any)
    // - one recently changed file (if any)
    if let Some(c) = core.first() {
        push_unique(c.clone());
    }
    if let Some(h) = hot.first() {
        push_unique(h.path.clone());
    }
    if let Some(ch) = changed_list.first() {
        push_unique(ch.clone());
    }

    if !hot.is_empty() {
        let mut s = String::from("\n[COMPLEX] Likely bugs:");
        for f in hot {
            s.push_str(&format!(
                "\n• {} ({} LOC) - {}",
                f.path.display(),
                f.loc,
                truncate_str(&f.summary.purpose, 50)
            ));
        }
        sections.push(s);
    }

    if !core.is_empty() {
        let mut s = String::from("\n[CORE] Widely used modules (good for global issues):");
        for p in core.iter().take(2) {
            let summary = index
                .files
                .get(p)
                .map(|f| f.summary.purpose.as_str())
                .unwrap_or("");
            s.push_str(&format!(
                "\n• {} - {}",
                p.display(),
                truncate_str(summary, 60)
            ));
        }
        sections.push(s);
    }

    // TODOs - collect from all files
    let todos: Vec<_> = index
        .files
        .values()
        .flat_map(|f| f.patterns.iter())
        .filter(|p| matches!(p.kind, PatternKind::TodoMarker))
        .take(limits.todo_limit)
        .collect();
    if !todos.is_empty() {
        let mut s = String::from("\n[TODO] Known issues:");
        for t in &todos {
            s.push_str(&format!(
                "\n• {}:{} - {}",
                t.file.display(),
                t.line,
                truncate_str(&t.description, 40)
            ));
        }
        sections.push(s);
    }

    // ═══ CODE PREVIEW ═══
    if !priority_files.is_empty() {
        let mut preview_section = String::from("\n\n═══ CODE PREVIEW ═══");
        for path in priority_files.iter().take(limits.code_preview_files) {
            let full_path = index.root.join(path);
            if let Ok(content) = std::fs::read_to_string(&full_path) {
                let lines: String = content
                    .lines()
                    .take(limits.code_preview_lines)
                    .enumerate()
                    .map(|(i, l)| format!("{:3}| {}", i + 1, l))
                    .collect::<Vec<_>>()
                    .join("\n");
                preview_section.push_str(&format!("\n\n── {} ──\n{}", path.display(), lines));
            }
        }
        sections.push(preview_section);
    }

    // ═══ TIER 3: ALL FILES ═══
    let all_paths: Vec<_> = index
        .files
        .keys()
        .take(limits.all_files_limit)
        .map(|p| p.display().to_string())
        .collect();
    if !all_paths.is_empty() {
        sections.push(format!("\n\n═══ ALL FILES ═══\n{}", all_paths.join("\n")));
    }

    // ═══ CONTEXT ═══
    let memory_section = format_repo_memory_section(repo_memory, "CONVENTIONS");
    if !memory_section.is_empty() {
        sections.push(memory_section);
    }

    if let Some(g) = glossary {
        if !g.is_empty() {
            let terms = g.to_prompt_context(limits.glossary_terms);
            if !terms.trim().is_empty() {
                sections.push(format!("\n\nTERMINOLOGY:\n{}", terms));
            }
        }
    }

    // ═══ INSTRUCTIONS ═══
    sections.push(String::from(
        "\n\n═══ YOUR TASK ═══
Generate up to 15 high-quality suggestions. Quality over quantity.

TIERED DISCOVERY:
1. THE GIST → understand project purpose
2. KEY AREAS → identify interesting modules
3. PRIORITY FILES → pick files to investigate
4. Use tree/search/read_range to find and read code

PREFERRED TOOLS (built-in, fast):
• tree → see directory structure
• search → find patterns with context
• read_range → read specific line ranges
• head → read file starts

EXAMPLE WORKFLOW:
1. See 'handles API calls' in summary
2. search 'async fn' in src/api.rs → find functions
3. read_range to examine specific sections
4. Find issue → record with evidence and confidence

BALANCE:
- Recent edits are useful context, but avoid making all suggestions about only the changed files.
- Prefer high-impact issues that affect the overall app (core or complex areas), even if unrelated to the latest diff.

RULES:
- Only suggest issues you've verified by reading actual code
- Include confidence: high (verified), medium (likely), low (uncertain)
- Return as JSON array",
    ));

    sections.join("")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::{
        CodebaseIndex, Dependency, FileIndex, FileSummary, Language, Pattern, Symbol,
    };
    use chrono::Utc;
    use std::collections::HashMap;
    use std::fs;
    use tempfile::tempdir;

    fn make_file_index(rel: &Path, purpose: &str, used_by: Vec<PathBuf>) -> FileIndex {
        let mut summary = FileSummary::default();
        summary.purpose = purpose.to_string();
        summary.used_by = used_by;

        FileIndex {
            path: rel.to_path_buf(),
            language: Language::Rust,
            loc: 10,
            content_hash: "x".to_string(),
            symbols: Vec::<Symbol>::new(),
            dependencies: Vec::<Dependency>::new(),
            patterns: Vec::<Pattern>::new(),
            complexity: 1.0,
            last_modified: Utc::now(),
            summary,
            layer: None,
            feature: None,
        }
    }

    #[test]
    fn analysis_prompt_does_not_tell_model_to_read_changed_first() {
        assert!(
            !ANALYZE_CODEBASE_AGENTIC_SYSTEM.contains("Read [CHANGED] files"),
            "System prompt should not instruct reading changed files first"
        );
        assert!(
            ANALYZE_CODEBASE_AGENTIC_SYSTEM.contains("Use [CHANGED] files as a hint"),
            "System prompt should mention changed files as a hint"
        );
    }

    #[test]
    fn lean_prompt_mentions_recent_work_without_overweighting_it() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();

        // Create files for CODE PREVIEW reads
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/changed.rs"), "pub fn a() {}\n").unwrap();
        fs::write(root.join("src/core.rs"), "pub fn b() {}\n").unwrap();
        fs::write(root.join("src/hot.rs"), "pub fn c() {}\n").unwrap();

        let mut files = HashMap::new();
        files.insert(
            PathBuf::from("src/changed.rs"),
            make_file_index(
                Path::new("src/changed.rs"),
                "Recently edited module",
                vec![],
            ),
        );
        files.insert(
            PathBuf::from("src/core.rs"),
            make_file_index(
                Path::new("src/core.rs"),
                "Core module used by many",
                vec![PathBuf::from("src/one.rs"), PathBuf::from("src/two.rs")],
            ),
        );
        // Hotspot: make it exceed threshold
        let mut hot = make_file_index(Path::new("src/hot.rs"), "Complex area", vec![]);
        hot.complexity = HIGH_COMPLEXITY_THRESHOLD + 1.0;
        files.insert(PathBuf::from("src/hot.rs"), hot);

        let index = CodebaseIndex {
            root: root.clone(),
            files,
            index_errors: Vec::new(),
            git_head: None,
        };

        let context = WorkContext {
            branch: "test".to_string(),
            uncommitted_files: vec![PathBuf::from("src/changed.rs")],
            staged_files: Vec::new(),
            untracked_files: Vec::new(),
            inferred_focus: None,
            modified_count: 1,
            repo_root: root,
        };

        let prompt = build_lean_analysis_prompt(&index, &context, None, None);
        assert!(
            prompt.contains("RECENT WORK (context only"),
            "Prompt should label recent work as context-only"
        );
        assert!(
            !prompt.contains("Read these first"),
            "Prompt should not instruct reading changed files first"
        );
        assert!(
            prompt.contains("BALANCE:"),
            "Prompt should include explicit balancing guidance"
        );
    }
}
*/
