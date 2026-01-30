use super::agentic::{call_llm_agentic, suggestion_schema};
use super::client::{call_llm_with_usage, truncate_str};
use super::models::{Model, Usage};
use super::parse::{parse_structured_suggestions, ParseDiagnostics};
use super::prompt_utils::format_repo_memory_section;
use super::prompts::{ANALYZE_CODEBASE_AGENTIC_SYSTEM, ASK_QUESTION_SYSTEM};
use super::summaries::discover_project_context;
use crate::cache::DomainGlossary;
use crate::context::WorkContext;
use crate::index::{CodebaseIndex, PatternKind, SymbolKind};
use crate::suggest::Suggestion;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

// ═══════════════════════════════════════════════════════════════════════════
//  THRESHOLDS AND CONSTANTS
// ═══════════════════════════════════════════════════════════════════════════

use crate::index::GOD_MODULE_LOC_THRESHOLD;

/// Complexity threshold above which a file is considered a "hotspot"
const HIGH_COMPLEXITY_THRESHOLD: f64 = 20.0;

// ═══════════════════════════════════════════════════════════════════════════
//  ADAPTIVE CONTEXT LIMITS
// ═══════════════════════════════════════════════════════════════════════════

/// Adaptive limits for context building based on codebase size
struct AdaptiveLimits {
    /// Max files to list in ask_question
    file_list_limit: usize,
    /// Max symbols to include
    symbol_limit: usize,
    /// Max directories to show in key areas
    key_areas_limit: usize,
    /// Max changed files to show
    changed_files_limit: usize,
    /// Max complex files to show
    complex_files_limit: usize,
    /// Max TODO markers to show
    todo_limit: usize,
    /// Max files in all files section
    all_files_limit: usize,
    /// Max code preview files
    code_preview_files: usize,
    /// Max lines per code preview
    code_preview_lines: usize,
    /// Max glossary terms
    glossary_terms: usize,
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
                key_areas_limit: 8,
                changed_files_limit: 8,
                complex_files_limit: 6,
                todo_limit: 6,
                all_files_limit: file_count.min(50),
                code_preview_files: 4,
                code_preview_lines: 50,
                glossary_terms: 10,
            }
        } else if file_count < 200 {
            // Medium codebase: balanced
            Self {
                file_list_limit: 50,
                symbol_limit: 100,
                key_areas_limit: 6,
                changed_files_limit: 6,
                complex_files_limit: 4,
                todo_limit: 4,
                all_files_limit: 40,
                code_preview_files: 3,
                code_preview_lines: 40,
                glossary_terms: 8,
            }
        } else if file_count < 500 {
            // Large codebase: prioritize structure
            Self {
                file_list_limit: 40,
                symbol_limit: 80,
                key_areas_limit: 8,
                changed_files_limit: 5,
                complex_files_limit: 4,
                todo_limit: 4,
                all_files_limit: 35,
                code_preview_files: 3,
                code_preview_lines: 35,
                glossary_terms: 6,
            }
        } else {
            // Very large codebase: focus on key areas
            Self {
                file_list_limit: 30,
                symbol_limit: 60,
                key_areas_limit: 10,
                changed_files_limit: 4,
                complex_files_limit: 3,
                todo_limit: 3,
                all_files_limit: 30,
                code_preview_files: 2,
                code_preview_lines: 30,
                glossary_terms: 6,
            }
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════════
//  PATTERN SUMMARY
// ═══════════════════════════════════════════════════════════════════════════

use std::collections::HashMap;

/// Summarize all detected patterns in the codebase for LLM context.
///
/// Provides a high-level view of code health indicators:
/// - Long functions (>50 lines)
/// - Deep nesting issues
/// - God modules (>500 lines)
/// - TODO/FIXME markers
/// - etc.
pub fn summarize_patterns(index: &CodebaseIndex) -> String {
    let mut by_kind: HashMap<PatternKind, usize> = HashMap::new();

    for file in index.files.values() {
        for pattern in &file.patterns {
            *by_kind.entry(pattern.kind).or_default() += 1;
        }
    }

    if by_kind.is_empty() {
        return String::new();
    }

    let mut parts = Vec::new();

    if let Some(&count) = by_kind.get(&PatternKind::LongFunction) {
        if count > 0 {
            parts.push(format!("{} long functions (>50 lines)", count));
        }
    }

    if let Some(&count) = by_kind.get(&PatternKind::GodModule) {
        if count > 0 {
            parts.push(format!("{} large modules (>500 lines)", count));
        }
    }

    if let Some(&count) = by_kind.get(&PatternKind::DeepNesting) {
        if count > 0 {
            parts.push(format!("{} deep nesting issues", count));
        }
    }

    if let Some(&count) = by_kind.get(&PatternKind::TodoMarker) {
        if count > 0 {
            parts.push(format!("{} TODO/FIXME markers", count));
        }
    }

    if let Some(&count) = by_kind.get(&PatternKind::ManyParameters) {
        if count > 0 {
            parts.push(format!("{} functions with many parameters", count));
        }
    }

    if parts.is_empty() {
        String::new()
    } else {
        format!("Code health: {}", parts.join(", "))
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

// ═══════════════════════════════════════════════════════════════════════════
//  LEAN HYBRID ANALYSIS (Compact Context + Surgical Tool Use)
// ═══════════════════════════════════════════════════════════════════════════

use crate::suggest::Confidence;

/// Maximum suggestions to return after quality filtering
const MAX_SUGGESTIONS: usize = 15;

#[derive(Debug, Clone)]
pub struct SuggestionDiagnostics {
    pub model: String,
    pub iterations: usize,
    pub tool_calls: usize,
    pub tool_names: Vec<String>,
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
    pub low_confidence_filtered: usize,
    pub truncated_count: usize,
    pub final_count: usize,
    pub response_chars: usize,
    pub response_preview: String,
}

/// Analyze codebase with compact context and minimal, surgical tool use
///
/// Strategy:
/// 1. Start with synthesized context (summaries, not full files)
/// 2. Model can make 1-3 targeted tool calls to verify specific issues
/// 3. Uses gpt-oss-120b for cost efficiency
/// 4. Quality-gated: filters out low-confidence suggestions, caps at MAX_SUGGESTIONS
///
/// This balances accuracy (model can verify) with speed/cost (minimal calls).
pub async fn analyze_codebase_agentic(
    repo_root: &Path,
    index: &CodebaseIndex,
    context: &WorkContext,
    repo_memory: Option<String>,
    glossary: Option<&DomainGlossary>,
) -> anyhow::Result<(Vec<Suggestion>, Option<Usage>, SuggestionDiagnostics)> {
    let user_prompt = build_lean_analysis_prompt(index, context, repo_memory.as_deref(), glossary);

    // Use Speed model with high reasoning effort and tools for cost-effective analysis
    // 4 iterations is sufficient for: tree -> search -> read_range -> finalize
    // Structured output is only applied on the final call (no tools) to ensure valid JSON
    let response = call_llm_agentic(
        ANALYZE_CODEBASE_AGENTIC_SYSTEM,
        &user_prompt,
        Model::Speed,
        repo_root,
        false,
        4, // max iterations - focused exploration
        Some(suggestion_schema()),
    )
    .await?;

    // Parse directly - structured output guarantees valid JSON
    let (mut suggestions, parse_diagnostics) =
        parse_structured_suggestions(&response.content, &index.root)?;
    let raw_count = suggestions.len();

    // Deduplicate suggestions (LLM sometimes returns near-duplicates)
    suggestions = deduplicate_suggestions(suggestions);
    let deduped_count = suggestions.len();

    // Filter out low confidence suggestions
    let low_confidence_filtered = suggestions
        .iter()
        .filter(|s| s.confidence == Confidence::Low)
        .count();
    suggestions.retain(|s| s.confidence != Confidence::Low);

    // Sort by confidence (high first), then priority
    suggestions.sort_by(|a, b| {
        b.confidence
            .cmp(&a.confidence)
            .then_with(|| b.priority.cmp(&a.priority))
    });

    // Cap at MAX_SUGGESTIONS
    let truncated_count = suggestions.len().saturating_sub(MAX_SUGGESTIONS);
    suggestions.truncate(MAX_SUGGESTIONS);
    let final_count = suggestions.len();

    let diagnostics = build_suggestion_diagnostics(
        Model::Speed,
        &response.content,
        &response.trace,
        &parse_diagnostics,
        raw_count,
        deduped_count,
        low_confidence_filtered,
        truncated_count,
        final_count,
    );

    Ok((suggestions, response.usage, diagnostics))
}

fn build_suggestion_diagnostics(
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

    // Collect top priority files for code preview
    let mut priority_files: Vec<PathBuf> = changed
        .iter()
        .take(limits.code_preview_files.min(2))
        .cloned()
        .collect();

    if !changed.is_empty() {
        let mut s = String::from("\n\n═══ PRIORITY FILES ═══\n[CHANGED] Read these first:");
        for path in changed.iter().take(limits.changed_files_limit) {
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

    // Add top complex file to priority list
    if let Some(h) = hot.first() {
        if priority_files.len() < limits.code_preview_files {
            priority_files.push(h.path.clone());
        }
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

RULES:
- Only suggest issues you've verified by reading actual code
- Include confidence: high (verified), medium (likely), low (uncertain)
- Return as JSON array",
    ));

    sections.join("")
}
