use super::client::{
    call_llm_structured, call_llm_structured_limited, call_llm_structured_with_provider,
    call_llm_with_usage, truncate_str, StructuredResponse,
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
const FAST_EVIDENCE_PACK_MAX_ITEMS: usize = 36;
const FAST_EVIDENCE_SNIPPET_LINES_BEFORE: usize = 6;
const FAST_EVIDENCE_SNIPPET_LINES_AFTER: usize = 8;
const FAST_GROUNDED_FINAL_TARGET_MIN: usize = 10;
const FAST_GROUNDED_FINAL_TARGET_MAX: usize = 15;
const FAST_GROUNDED_VALIDATED_SOFT_FLOOR: usize = 10;
const FAST_GROUNDED_VALIDATED_STRETCH_TARGET: usize = FAST_GROUNDED_FINAL_TARGET_MAX;
const FAST_GROUNDED_PROVISIONAL_TARGET_MIN: usize = 18;
const FAST_GROUNDED_PROVISIONAL_TARGET_MAX: usize = 24;
const FAST_EVIDENCE_SOURCE_PATTERN_MAX: usize = 12;
const FAST_EVIDENCE_SOURCE_HOTSPOT_MAX: usize = 8;
const FAST_EVIDENCE_SOURCE_CORE_MAX: usize = 8;
const FAST_EVIDENCE_KIND_GOD_MODULE_MAX: usize = 4;
const REFINEMENT_MAX_ATTEMPTS: usize = 4;
const GENERATION_TOPUP_MAX_CALLS: usize = 2;
const GENERATION_TOPUP_TIMEOUT_MS: u64 = 7_800;
const SUGGEST_BALANCED_BUDGET_MS: u64 = 35_000;
const VALIDATION_CONCURRENCY: usize = 6;
const SHARD_COUNT: usize = 3;
const SHARD_REQUEST_MIN: usize = 6;
const SHARD_REQUEST_MAX: usize = 8;

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

    let response = call_llm_with_usage(ASK_QUESTION_SYSTEM, &user, Model::Balanced, false).await?;
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
    pub regeneration_attempts: usize,
    pub refinement_complete: bool,
    pub notes: Vec<String>,
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
    Some(snippet)
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

    let mut summary = scrub_user_summary(&s.summary);
    if summary.trim().is_empty() {
        summary = scrub_user_summary(
            s.detail
                .lines()
                .next()
                .unwrap_or("Potential improvement found.")
                .trim(),
        );
        summary = truncate_str(&summary, 120).to_string();
    }

    let suggestion = Suggestion::new(
        kind,
        priority,
        item.file.clone(),
        summary,
        crate::suggest::SuggestionSource::LlmDeep,
    )
    .with_confidence(confidence)
    .with_line(item.line)
    .with_detail(s.detail)
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

fn should_run_generation_topup(mapped_count: usize, topup_calls: usize) -> bool {
    mapped_count < FAST_GROUNDED_PROVISIONAL_TARGET_MIN && topup_calls < GENERATION_TOPUP_MAX_CALLS
}

fn generation_topup_request_count(deficit: usize) -> usize {
    deficit.saturating_add(4).clamp(4, 12)
}

fn regeneration_needed(validated_count: usize) -> usize {
    FAST_GROUNDED_VALIDATED_SOFT_FLOOR.saturating_sub(validated_count)
}

fn regeneration_needed_for_target(validated_count: usize, target: usize) -> usize {
    target.saturating_sub(validated_count)
}

fn regeneration_request_bounds(needed: usize) -> (usize, usize) {
    let min_requested = needed.saturating_mul(2).clamp(4, 12);
    let max_requested = needed.saturating_mul(3).clamp(4, 12).max(min_requested);
    (min_requested, max_requested)
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

    if strict.len() >= 4 || !allow_rejected_relaxation {
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

fn grounded_suggestion_schema(_pack_len: usize) -> serde_json::Value {
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
                                    "evidence_id": { "type": "integer" }
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

async fn validate_suggestion_with_model(
    suggestion: &Suggestion,
    memory_section: &str,
) -> anyhow::Result<(SuggestionValidationState, String, Option<Usage>)> {
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

    let response = call_llm_structured::<SuggestionValidationJson>(
        system,
        &user,
        Model::Speed,
        "suggestion_validation",
        suggestion_validation_schema(),
    )
    .await?;

    let state = match response.data.validation.trim().to_lowercase().as_str() {
        "validated" => SuggestionValidationState::Validated,
        "contradicted" | "insufficient_evidence" => SuggestionValidationState::Rejected,
        _ => SuggestionValidationState::Rejected,
    };

    Ok((state, response.data.reason, response.usage))
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
    SuggestionValidationState,
    String,
    Option<Usage>,
);

fn sort_validation_outcomes(outcomes: &mut [ValidationOutcome]) {
    outcomes.sort_by_key(|(idx, _, _, _, _)| *idx);
}

async fn validate_batch_suggestions(
    batch: Vec<Suggestion>,
    memory_section: &str,
    cache: &crate::cache::Cache,
    run_id: &str,
    validated: &mut Vec<Suggestion>,
    rejected_count: &mut usize,
    used_evidence_ids: &mut HashSet<usize>,
    rejected_evidence_ids: &mut HashSet<usize>,
) -> Option<Usage> {
    let mut usage: Option<Usage> = None;
    let mut queue: Vec<(usize, Suggestion)> = batch.into_iter().enumerate().collect();
    let memory_section_owned = memory_section.to_string();
    while !queue.is_empty() {
        if validated.len() >= FAST_GROUNDED_FINAL_TARGET_MAX {
            break;
        }
        let chunk_size = VALIDATION_CONCURRENCY.min(queue.len());
        let chunk: Vec<(usize, Suggestion)> = queue.drain(..chunk_size).collect();
        let mut outcomes: Vec<ValidationOutcome> =
            join_all(chunk.into_iter().map(|(idx, suggestion)| {
                let memory_section = memory_section_owned.clone();
                async move {
                    let (state, reason, call_usage) =
                        match validate_suggestion_with_model(&suggestion, &memory_section).await {
                            Ok(result) => result,
                            Err(err) => (
                                SuggestionValidationState::Rejected,
                                format!(
                                    "Validation failed: {}",
                                    truncate_str(&err.to_string(), 120)
                                ),
                                None,
                            ),
                        };
                    (idx, suggestion, state, reason, call_usage)
                }
            }))
            .await;
        sort_validation_outcomes(&mut outcomes);

        for (_idx, mut suggestion, state, reason, call_usage) in outcomes {
            usage = merge_usage(usage, call_usage);
            let primary_evidence = suggestion.evidence_refs.first().map(|r| r.snippet_id);
            match state {
                SuggestionValidationState::Validated => {
                    if let Some(eid) = primary_evidence {
                        if used_evidence_ids.insert(eid) {
                            suggestion.validation_state = SuggestionValidationState::Validated;
                            append_suggestion_quality_record(
                                cache,
                                run_id,
                                &suggestion,
                                "validated",
                                Some(reason),
                            );
                            validated.push(suggestion);
                        } else {
                            *rejected_count += 1;
                            rejected_evidence_ids.insert(eid);
                            suggestion.validation_state = SuggestionValidationState::Rejected;
                            append_suggestion_quality_record(
                                cache,
                                run_id,
                                &suggestion,
                                "rejected",
                                Some("Duplicate evidence_id after validation".to_string()),
                            );
                        }
                    } else {
                        *rejected_count += 1;
                        suggestion.validation_state = SuggestionValidationState::Rejected;
                        append_suggestion_quality_record(
                            cache,
                            run_id,
                            &suggestion,
                            "rejected",
                            Some("Missing evidence refs after validation".to_string()),
                        );
                    }
                }
                SuggestionValidationState::Rejected => {
                    *rejected_count += 1;
                    if let Some(eid) = primary_evidence {
                        rejected_evidence_ids.insert(eid);
                    }
                    suggestion.validation_state = SuggestionValidationState::Rejected;
                    let outcome = if reason.to_lowercase().contains("insufficient") {
                        "insufficient_evidence"
                    } else {
                        "rejected"
                    };
                    append_suggestion_quality_record(
                        cache,
                        run_id,
                        &suggestion,
                        outcome,
                        Some(reason),
                    );
                }
                SuggestionValidationState::Pending => {
                    *rejected_count += 1;
                    if let Some(eid) = primary_evidence {
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

            if validated.len() >= FAST_GROUNDED_FINAL_TARGET_MAX {
                break;
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

    if pack.len() < 12 {
        return Err(anyhow::anyhow!(
            "Not enough grounded evidence items found to generate suggestions. Try again after indexing completes."
        ));
    }

    let memory_section =
        format_repo_memory_section(repo_memory.as_deref(), "Repo conventions / decisions");
    let schema = grounded_suggestion_schema(pack.len());

    // Three sharded calls in parallel to maintain throughput while reducing
    // provider fan-out failures under bursty harness runs.
    let shard_count = SHARD_COUNT;
    let shard_size = (pack.len() + shard_count - 1) / shard_count;
    let shard_timeout_ms: u64 = 8_800;
    let shard_max_tokens: u32 = 620;
    let shard_prompts: Vec<String> = pack
        .chunks(shard_size.max(1))
        .filter(|chunk| !chunk.is_empty())
        .map(|chunk| {
            format_grounded_user_prompt(
                &memory_section,
                index,
                summaries,
                chunk,
                &format!(
                    "For this request, return {} to {} suggestions.",
                    SHARD_REQUEST_MIN, SHARD_REQUEST_MAX
                ),
            )
        })
        .collect();

    let llm_start = std::time::Instant::now();
    let shard_responses = join_all(shard_prompts.into_iter().map(|user| {
        let shard_schema = schema.clone();
        async move {
            call_grounded_suggestions_with_fallback(
                FAST_GROUNDED_SUGGESTIONS_SYSTEM,
                &user,
                "fast_grounded_suggestions",
                shard_schema,
                shard_max_tokens,
                shard_timeout_ms,
            )
            .await
        }
    }))
    .await;
    let mut llm_ms = llm_start.elapsed().as_millis() as u64;

    let mut usage = None;
    let mut raw_items: Vec<FastGroundedSuggestionJson> = Vec::new();
    let mut generation_errors: Vec<String> = Vec::new();
    for response in shard_responses {
        match response {
            Ok(r) => {
                usage = merge_usage(usage, r.usage);
                raw_items.extend(r.data.suggestions);
            }
            Err(err) => {
                generation_errors.push(truncate_str(&err.to_string(), 700).to_string());
            }
        }
    }

    let mut generation_waves = 1usize;
    let mut generation_topup_calls = 0usize;
    // Provider resilience: if the pinned provider yields no shard output, retry once with
    // normal provider routing before we declare failure.
    if raw_items.is_empty() {
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
                    "Return {} to {} suggestions and include evidence refs for each suggestion.",
                    FAST_GROUNDED_FINAL_TARGET_MIN, FAST_GROUNDED_FINAL_TARGET_MAX
                ),
            ),
            "fast_grounded_suggestions_rescue",
            schema.clone(),
            620,
            10_500,
        )
        .await;
        match rescue_result {
            Ok(r) => {
                llm_ms += rescue_start.elapsed().as_millis() as u64;
                usage = merge_usage(usage, r.usage);
                raw_items.extend(r.data.suggestions);
            }
            Err(err) => {
                generation_errors.push(truncate_str(&err.to_string(), 700).to_string());
            }
        }
    }

    let mut raw_count = raw_items.len();
    let (mut mapped, mut missing_or_invalid) = map_raw_items_to_grounded(raw_items, &pack);
    let mut generation_mapped_count = grounded_mapped_count(&mapped);

    while should_run_generation_topup(generation_mapped_count, generation_topup_calls) {
        if overall_start.elapsed().as_millis() as u64 >= SUGGEST_BALANCED_BUDGET_MS {
            break;
        }

        let deficit = FAST_GROUNDED_PROVISIONAL_TARGET_MIN.saturating_sub(generation_mapped_count);
        let request_count = generation_topup_request_count(deficit);
        generation_topup_calls += 1;
        generation_waves += 1;

        let extra_start = std::time::Instant::now();
        let topup_result = call_grounded_suggestions_with_fallback(
            FAST_GROUNDED_SUGGESTIONS_SYSTEM,
            &format_grounded_user_prompt(
                &memory_section,
                index,
                summaries,
                &pack,
                &format!(
                    "Return {} suggestions. Prefer diverse evidence refs and avoid reusing the same evidence_id unless necessary.",
                    request_count
                ),
            ),
            "fast_grounded_suggestions_topup",
            schema.clone(),
            560,
            GENERATION_TOPUP_TIMEOUT_MS,
        )
        .await;
        match topup_result {
            Ok(r) => {
                llm_ms += extra_start.elapsed().as_millis() as u64;
                usage = merge_usage(usage, r.usage);
                raw_count += r.data.suggestions.len();
                let (topup_mapped, topup_missing_or_invalid) =
                    map_raw_items_to_grounded(r.data.suggestions, &pack);
                mapped.extend(topup_mapped);
                missing_or_invalid += topup_missing_or_invalid;
                generation_mapped_count = grounded_mapped_count(&mapped);
            }
            Err(err) => {
                generation_errors.push(truncate_str(&err.to_string(), 700).to_string());
            }
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
            620,
            10_500,
        )
        .await;
        match rescue_result {
            Ok(r) => {
                llm_ms += rescue_start.elapsed().as_millis() as u64;
                usage = merge_usage(usage, r.usage);
                raw_count += r.data.suggestions.len();
                let (rescue_mapped, rescue_missing_or_invalid) =
                    map_raw_items_to_grounded(r.data.suggestions, &pack);
                mapped.extend(rescue_mapped);
                missing_or_invalid += rescue_missing_or_invalid;
                generation_mapped_count = grounded_mapped_count(&mapped);
            }
            Err(err) => {
                generation_errors.push(truncate_str(&err.to_string(), 700).to_string());
            }
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
        regeneration_attempts: 0,
        refinement_complete: false,
        notes: Vec::new(),
    };

    Ok((suggestions, usage, diagnostics))
}

pub async fn refine_grounded_suggestions(
    repo_root: &Path,
    index: &CodebaseIndex,
    context: &WorkContext,
    repo_memory: Option<String>,
    summaries: Option<&HashMap<PathBuf, String>>,
    provisional: Vec<Suggestion>,
    mut diagnostics: SuggestionDiagnostics,
) -> anyhow::Result<(Vec<Suggestion>, Option<Usage>, SuggestionDiagnostics)> {
    if provisional.is_empty() {
        diagnostics.refinement_complete = true;
        diagnostics.provisional_count = 0;
        diagnostics.validated_count = 0;
        diagnostics.rejected_count = 0;
        diagnostics.final_count = 0;
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
    let regeneration_target = FAST_GROUNDED_VALIDATED_STRETCH_TARGET;

    let batch_usage = validate_batch_suggestions(
        provisional.clone(),
        &memory_section,
        &cache,
        &diagnostics.run_id,
        &mut validated,
        &mut rejected_count,
        &mut used_evidence_ids,
        &mut rejected_evidence_ids,
    )
    .await;
    usage = merge_usage(usage, batch_usage);

    let (pack, pack_stats) = build_evidence_pack(repo_root, index, context);
    while validated.len() < regeneration_target && regeneration_attempts < REFINEMENT_MAX_ATTEMPTS {
        if refine_start.elapsed().as_millis() as u64 >= SUGGEST_BALANCED_BUDGET_MS {
            notes.push("regeneration_budget_reached".to_string());
            break;
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

        let needed = regeneration_needed_for_target(validated.len(), regeneration_target);
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
            520,
            8_200,
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
        )
        .await;
        usage = merge_usage(usage, batch_usage);
    }

    let validated = finalize_validated_suggestions(validated);
    let refinement_ms = refine_start.elapsed().as_millis() as u64;
    diagnostics.batch_verify_ms = refinement_ms;
    diagnostics.llm_ms += refinement_ms;
    diagnostics.pack_pattern_count = pack_stats.pattern_count;
    diagnostics.pack_hotspot_count = pack_stats.hotspot_count;
    diagnostics.pack_core_count = pack_stats.core_count;
    diagnostics.pack_line1_ratio = pack_stats.line1_ratio;
    diagnostics.provisional_count = provisional.len();
    diagnostics.validated_count = validated
        .iter()
        .filter(|s| s.validation_state == SuggestionValidationState::Validated)
        .count();
    diagnostics.rejected_count = rejected_count;
    diagnostics.rejected_evidence_skipped_count = rejected_evidence_skipped_ids.len();
    diagnostics.regeneration_attempts = regeneration_attempts;
    diagnostics.refinement_complete = true;
    diagnostics.final_count = validated.len();
    diagnostics.deduped_count = validated.len();
    diagnostics.parse_strategy = "fast_grounded_refined".to_string();
    diagnostics.notes = notes;
    if validated.len() < regeneration_target {
        diagnostics
            .notes
            .push("count_below_stretch_target".to_string());
    }
    if regeneration_needed(validated.len()) > 0 {
        diagnostics.notes.push("count_below_soft_floor".to_string());
    }

    Ok((validated, usage, diagnostics))
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
        let raw_count = FAST_GROUNDED_PROVISIONAL_TARGET_MIN + 12;
        assert!(raw_count > FAST_GROUNDED_PROVISIONAL_TARGET_MIN);
        assert!(should_run_generation_topup(
            FAST_GROUNDED_PROVISIONAL_TARGET_MIN - 1,
            0
        ));
        assert!(!should_run_generation_topup(
            FAST_GROUNDED_PROVISIONAL_TARGET_MIN,
            0
        ));
        assert!(!should_run_generation_topup(0, GENERATION_TOPUP_MAX_CALLS));
    }

    #[test]
    fn generation_topup_request_count_uses_deficit_plus_padding_with_cap() {
        assert_eq!(generation_topup_request_count(1), 5);
        assert_eq!(generation_topup_request_count(6), 10);
        assert_eq!(generation_topup_request_count(20), 12);
    }

    #[test]
    fn regeneration_request_bounds_scale_and_clamp_to_range() {
        assert_eq!(regeneration_request_bounds(1), (4, 4));
        assert_eq!(regeneration_request_bounds(2), (4, 6));
        assert_eq!(regeneration_request_bounds(4), (8, 12));
        assert_eq!(regeneration_request_bounds(10), (12, 12));
    }

    #[test]
    fn regeneration_needed_for_target_uses_target_count() {
        assert_eq!(regeneration_needed_for_target(0, 15), 15);
        assert_eq!(regeneration_needed_for_target(10, 15), 5);
        assert_eq!(regeneration_needed_for_target(15, 15), 0);
        assert_eq!(regeneration_needed_for_target(18, 15), 0);
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
                SuggestionValidationState::Validated,
                "ok".to_string(),
                None,
            ),
            (
                0,
                test_suggestion("a"),
                SuggestionValidationState::Validated,
                "ok".to_string(),
                None,
            ),
            (
                1,
                test_suggestion("b"),
                SuggestionValidationState::Rejected,
                "no".to_string(),
                None,
            ),
        ];
        sort_validation_outcomes(&mut outcomes);
        let summaries = outcomes
            .iter()
            .map(|(_, suggestion, _, _, _)| suggestion.summary.clone())
            .collect::<Vec<_>>();
        assert_eq!(summaries, vec!["a", "b", "c"]);
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
        assert!(evidence_refs.get("minItems").is_none());
        assert!(evidence_refs.get("maxItems").is_none());
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
