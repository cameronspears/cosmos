use super::client::{call_llm_structured_with_provider, call_llm_with_usage, truncate_str};
use super::models::merge_usage;
use super::models::{Model, Usage};
use super::prompt_utils::format_repo_memory_section;
use super::prompts::{ASK_QUESTION_SYSTEM, FAST_GROUNDED_SUGGESTIONS_SYSTEM};
use crate::context::WorkContext;
use crate::index::{CodebaseIndex, PatternSeverity, SymbolKind};
use crate::suggest::Suggestion;
use regex::Regex;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

// ═══════════════════════════════════════════════════════════════════════════
//  THRESHOLDS AND CONSTANTS
// ═══════════════════════════════════════════════════════════════════════════

use crate::index::GOD_MODULE_LOC_THRESHOLD;

/// Complexity threshold above which a file is considered a "hotspot"
const HIGH_COMPLEXITY_THRESHOLD: f64 = 20.0;
const FAST_EVIDENCE_PACK_MAX_ITEMS: usize = 25;
const FAST_EVIDENCE_SNIPPET_LINES_BEFORE: usize = 8;
const FAST_EVIDENCE_SNIPPET_LINES_AFTER: usize = 12;

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
}

#[derive(Debug, Clone)]
struct EvidenceItem {
    id: usize,
    file: PathBuf,
    line: usize, // 1-based
    snippet: String,
    why_interesting: String,
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

fn build_evidence_pack(
    repo_root: &Path,
    index: &CodebaseIndex,
    context: &WorkContext,
) -> Vec<EvidenceItem> {
    let changed: HashSet<PathBuf> = context.all_changed_files().into_iter().cloned().collect();
    let mut candidates: Vec<(f64, EvidenceItem)> = Vec::new();

    // Patterns (high signal)
    for file in index.files.values() {
        for p in &file.patterns {
            let Some(rel_file) = normalize_repo_relative(repo_root, &p.file) else {
                continue;
            };
            let severity_score = match p.kind.severity() {
                PatternSeverity::High => 3.0,
                PatternSeverity::Medium => 2.0,
                PatternSeverity::Low => 1.0,
                PatternSeverity::Info => 0.5,
            };
            let changed_boost = if changed.contains(&rel_file) {
                0.2
            } else {
                0.0
            };
            let score = severity_score + changed_boost;
            if let Some(snippet) = read_snippet_around_line(repo_root, &rel_file, p.line) {
                candidates.push((
                    score,
                    EvidenceItem {
                        id: 0,
                        file: rel_file,
                        line: p.line.max(1),
                        snippet,
                        why_interesting: format!("Detected {:?}: {}", p.kind, p.description),
                    },
                ));
            }
        }
    }

    // Hotspots (complexity/LOC)
    let mut hotspot_files: Vec<_> = index.files.values().collect();
    hotspot_files.sort_by(|a, b| {
        b.complexity
            .partial_cmp(&a.complexity)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    for f in hotspot_files
        .iter()
        .filter(|f| f.complexity > HIGH_COMPLEXITY_THRESHOLD || f.loc > GOD_MODULE_LOC_THRESHOLD)
        .take(6)
    {
        let Some(rel_file) = normalize_repo_relative(repo_root, &f.path) else {
            continue;
        };
        let anchor = f
            .symbols
            .iter()
            .filter(|s| matches!(s.kind, SymbolKind::Function | SymbolKind::Method))
            .max_by(|a, b| {
                a.complexity
                    .partial_cmp(&b.complexity)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .map(|s| s.line)
            .unwrap_or(1);
        if let Some(snippet) = read_snippet_around_line(repo_root, &rel_file, anchor) {
            candidates.push((
                2.0 + (f.complexity / 50.0).min(1.0),
                EvidenceItem {
                    id: 0,
                    file: rel_file,
                    line: anchor.max(1),
                    snippet,
                    why_interesting: format!(
                        "Hotspot file (complexity {:.1}, {} LOC)",
                        f.complexity, f.loc
                    ),
                },
            ));
        }
    }

    // Core files (fan-in)
    let mut core_files: Vec<_> = index.files.values().collect();
    core_files.sort_by(|a, b| b.summary.used_by.len().cmp(&a.summary.used_by.len()));
    for f in core_files
        .iter()
        .filter(|f| f.summary.used_by.len() >= 3)
        .take(6)
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
            candidates.push((
                1.5 + (f.summary.used_by.len() as f64 / 20.0).min(1.0),
                EvidenceItem {
                    id: 0,
                    file: rel_file,
                    line: anchor.max(1),
                    snippet,
                    why_interesting: format!(
                        "Core file used by {} other files",
                        f.summary.used_by.len()
                    ),
                },
            ));
        }
    }

    // Rank and dedupe by (file,line)
    candidates.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    let mut seen: HashSet<(PathBuf, usize)> = HashSet::new();
    let mut out: Vec<EvidenceItem> = Vec::new();
    for (_score, mut item) in candidates {
        let key = (item.file.clone(), item.line);
        if seen.insert(key) {
            item.id = out.len();
            out.push(item);
        }
        if out.len() >= FAST_EVIDENCE_PACK_MAX_ITEMS {
            break;
        }
    }
    out
}

#[derive(Debug, Clone, serde::Deserialize)]
struct FastGroundedSuggestionJson {
    #[serde(default)]
    evidence_id: Option<usize>,
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

fn extract_evidence_id(text: &str) -> Option<usize> {
    // Accept a few common patterns:
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
                // skip separators like ':', '=', whitespace
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

pub async fn analyze_codebase_fast_grounded(
    repo_root: &Path,
    index: &CodebaseIndex,
    context: &WorkContext,
    repo_memory: Option<String>,
) -> anyhow::Result<(Vec<Suggestion>, Option<Usage>, SuggestionDiagnostics)> {
    let overall_start = std::time::Instant::now();
    let pack_start = std::time::Instant::now();
    let pack = build_evidence_pack(repo_root, index, context);
    let evidence_pack_ms = pack_start.elapsed().as_millis() as u64;

    if pack.len() < 12 {
        return Err(anyhow::anyhow!(
            "Not enough grounded evidence items found to generate suggestions. Try again after indexing completes."
        ));
    }

    let memory_section =
        format_repo_memory_section(repo_memory.as_deref(), "Repo conventions / decisions");
    let format_user = |items: &[EvidenceItem], count_hint: &str| -> String {
        let mut user = String::new();
        if !memory_section.trim().is_empty() {
            user.push_str(&memory_section);
            user.push_str("\n\n");
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
    };

    let schema = serde_json::json!({
        "type": "object",
        "properties": {
            "suggestions": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "evidence_id": { "type": "integer", "minimum": 0, "maximum": pack.len().saturating_sub(1) },
                        "kind": {
                            "type": "string",
                            "enum": ["bugfix", "improvement", "optimization", "refactoring", "security", "reliability"]
                        },
                        "priority": { "type": "string", "enum": ["high", "medium", "low"] },
                        "confidence": { "type": "string", "enum": ["high", "medium"] },
                        "summary": { "type": "string" },
                        "detail": { "type": "string" }
                    },
                    "required": ["evidence_id", "kind", "priority", "confidence", "summary", "detail"],
                    "additionalProperties": false
                }
            }
        },
        "required": ["suggestions"],
        "additionalProperties": false
    });

    // Two sharded calls in parallel to reduce tail latency and improve chance of
    // reaching 10+ usable suggestions under provider quirks.
    let mid = pack.len() / 2;
    let (left, right) = pack.split_at(mid);

    let shard_timeout_ms: u64 = 6_800;
    let shard_max_tokens: u32 = 420;
    let user_left = format_user(left, "For this request, return 7 to 9 suggestions.");
    let user_right = format_user(right, "For this request, return 7 to 9 suggestions.");

    let llm_start = std::time::Instant::now();
    let (resp_a, resp_b) = tokio::join!(
        call_llm_structured_with_provider::<FastGroundedResponseJson>(
            FAST_GROUNDED_SUGGESTIONS_SYSTEM,
            &user_left,
            Model::Speed,
            "fast_grounded_suggestions",
            schema.clone(),
            super::client::provider_cerebras_fp16(),
            shard_max_tokens,
            shard_timeout_ms,
        ),
        call_llm_structured_with_provider::<FastGroundedResponseJson>(
            FAST_GROUNDED_SUGGESTIONS_SYSTEM,
            &user_right,
            Model::Speed,
            "fast_grounded_suggestions",
            schema.clone(),
            super::client::provider_cerebras_fp16(),
            shard_max_tokens,
            shard_timeout_ms,
        ),
    );
    let mut llm_ms = llm_start.elapsed().as_millis() as u64;

    let mut usage = None;
    let mut raw_items: Vec<FastGroundedSuggestionJson> = Vec::new();
    if let Ok(r) = resp_a {
        usage = merge_usage(usage, r.usage);
        raw_items.extend(r.data.suggestions);
    }
    if let Ok(r) = resp_b {
        usage = merge_usage(usage, r.usage);
        raw_items.extend(r.data.suggestions);
    }

    // If we still don't have enough material, try one more fast call with the remaining budget.
    if raw_items.len() < 10 {
        let total_budget_ms: u64 = 10_000;
        let elapsed = overall_start.elapsed().as_millis() as u64;
        let remaining = total_budget_ms.saturating_sub(elapsed).saturating_sub(250);
        if remaining >= 900 {
            let extra_start = std::time::Instant::now();
            if let Ok(r) = call_llm_structured_with_provider::<FastGroundedResponseJson>(
                FAST_GROUNDED_SUGGESTIONS_SYSTEM,
                &format_user(&pack, "Return 10 to 15 suggestions. Avoid repeating the same evidence_id across suggestions."),
                Model::Speed,
                "fast_grounded_suggestions",
                schema,
                super::client::provider_cerebras_fp16(),
                380,
                remaining,
            )
            .await
            {
                llm_ms += extra_start.elapsed().as_millis() as u64;
                usage = merge_usage(usage, r.usage);
                raw_items.extend(r.data.suggestions);
            }
        }
    }

    let mut mapped: Vec<(usize, Suggestion)> = Vec::new();
    let mut missing_or_invalid = 0usize;

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

    for s in raw_items {
        let evidence_id = s
            .evidence_id
            .or_else(|| extract_evidence_id(&s.summary))
            .or_else(|| extract_evidence_id(&s.detail));
        let Some(evidence_id) = evidence_id else {
            missing_or_invalid += 1;
            continue;
        };
        let Some(item) = pack.get(evidence_id) else {
            missing_or_invalid += 1;
            continue;
        };
        let kind = match s.kind.to_lowercase().as_str() {
            "bugfix" => crate::suggest::SuggestionKind::BugFix,
            "optimization" => crate::suggest::SuggestionKind::Optimization,
            "refactoring" => crate::suggest::SuggestionKind::Quality,
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
            // Provider didn't follow schema; recover a minimally useful summary.
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
        .with_evidence(item.snippet.clone());

        mapped.push((evidence_id, suggestion));
    }

    if mapped.is_empty() {
        return Err(anyhow::anyhow!(
            "AI returned no usable grounded suggestions. Try again."
        ));
    }

    // Prefer unique evidence_id, but allow duplicates if needed to reach 10.
    let mut seen_ids: HashSet<usize> = HashSet::new();
    let mut unique = Vec::new();
    let mut dupes = Vec::new();
    for (eid, s) in mapped {
        if seen_ids.insert(eid) {
            unique.push(s);
        } else {
            dupes.push(s);
        }
    }
    let mut suggestions = unique;
    if suggestions.len() < 10 {
        let need = 10usize.saturating_sub(suggestions.len());
        suggestions.extend(dupes.into_iter().take(need));
    }
    suggestions.truncate(15);

    let diagnostics = SuggestionDiagnostics {
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
        raw_count: suggestions.len(),
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
    };

    Ok((suggestions, usage, diagnostics))
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
