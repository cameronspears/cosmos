use super::agentic::call_llm_agentic;
use super::client::{call_llm_with_usage, truncate_str};
use super::models::{Model, Usage};
use super::parse::parse_codebase_suggestions;
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
    let file_list: Vec<_> = index
        .files
        .keys()
        .take(50) // Limit to avoid huge prompts
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
        .take(100)
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

/// Minimum number of suggestions we require from analysis
const MIN_SUGGESTIONS: usize = 10;

/// Maximum continuation attempts to reach MIN_SUGGESTIONS
const MAX_CONTINUATION_ATTEMPTS: usize = 2;

/// Analyze codebase with compact context and minimal, surgical tool use
///
/// Strategy:
/// 1. Start with synthesized context (summaries, not full files)
/// 2. Model can make 1-3 targeted tool calls to verify specific issues
/// 3. Uses gpt-oss-120b for cost efficiency
/// 4. If fewer than MIN_SUGGESTIONS returned, makes continuation calls to get more
///
/// This balances accuracy (model can verify) with speed/cost (minimal calls).
pub async fn analyze_codebase_agentic(
    repo_root: &Path,
    index: &CodebaseIndex,
    context: &WorkContext,
    repo_memory: Option<String>,
    glossary: Option<&DomainGlossary>,
) -> anyhow::Result<(Vec<Suggestion>, Option<Usage>)> {
    let user_prompt = build_lean_analysis_prompt(index, context, repo_memory.as_deref(), glossary);

    // Use Speed model (gpt-oss-120b) with surgical tool access
    // 8 iterations allows for good exploration
    let response = call_llm_agentic(
        ANALYZE_CODEBASE_AGENTIC_SYSTEM,
        &user_prompt,
        Model::Speed,
        repo_root,
        false,
        8, // max iterations - suggestions need exploration
    )
    .await?;

    let mut suggestions = parse_codebase_suggestions(&response.content)?;

    // Deduplicate initial suggestions (LLM sometimes returns near-duplicates)
    suggestions = deduplicate_suggestions(suggestions);

    // If we got fewer than MIN_SUGGESTIONS, make continuation calls to get more
    let mut attempts = 0;
    while suggestions.len() < MIN_SUGGESTIONS && attempts < MAX_CONTINUATION_ATTEMPTS {
        attempts += 1;
        let needed = MIN_SUGGESTIONS - suggestions.len();

        let continuation_prompt = build_continuation_prompt(&suggestions, needed);
        let continuation_response = call_llm_agentic(
            ANALYZE_CODEBASE_AGENTIC_SYSTEM,
            &continuation_prompt,
            Model::Speed,
            repo_root,
            false,
            4, // fewer iterations for continuation - context already gathered
        )
        .await?;

        match parse_codebase_suggestions(&continuation_response.content) {
            Ok(additional) => {
                // Deduplicate using multiple criteria to catch near-duplicates:
                // 1. Exact file+summary match
                // 2. Same file+line (same location = likely same issue)
                // 3. Same file+kind (same category in same file = often duplicate)
                let existing_summaries: std::collections::HashSet<_> = suggestions
                    .iter()
                    .map(|s| (s.file.clone(), s.summary.clone()))
                    .collect();
                let existing_locations: std::collections::HashSet<_> = suggestions
                    .iter()
                    .filter_map(|s| s.line.map(|l| (s.file.clone(), l)))
                    .collect();
                let existing_file_kinds: std::collections::HashSet<_> = suggestions
                    .iter()
                    .map(|s| (s.file.clone(), s.kind))
                    .collect();

                for s in additional {
                    let dominated_by_summary =
                        existing_summaries.contains(&(s.file.clone(), s.summary.clone()));
                    let dominated_by_location = s
                        .line
                        .map(|l| existing_locations.contains(&(s.file.clone(), l)))
                        .unwrap_or(false);
                    let dominated_by_kind = existing_file_kinds.contains(&(s.file.clone(), s.kind));

                    if !dominated_by_summary && !dominated_by_location && !dominated_by_kind {
                        suggestions.push(s);
                    }
                }
            }
            Err(_) => {
                // If continuation parsing fails, keep what we have
                break;
            }
        }
    }

    Ok((suggestions, None))
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

/// Build a prompt asking for additional suggestions to reach the minimum
fn build_continuation_prompt(existing: &[Suggestion], needed: usize) -> String {
    let existing_summaries: Vec<String> = existing
        .iter()
        .enumerate()
        .map(|(i, s)| format!("{}. {} - {}", i + 1, s.file.display(), s.summary))
        .collect();

    format!(
        r#"You provided {} suggestions, but I need at least {} total (so {} more).

EXISTING SUGGESTIONS (do NOT repeat these):
{}

Find {} MORE unique suggestions. Look in different files or find different issues in the same files.
Focus on areas you haven't explored yet:
- Error handling gaps
- Missing input validation  
- Performance issues
- Security concerns
- Code that could fail silently

Use shell tools to explore and verify. Return ONLY the new suggestions as a JSON array."#,
        existing.len(),
        MIN_SUGGESTIONS,
        needed,
        existing_summaries.join("\n"),
        needed
    )
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
    let project_name = index
        .root
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("project");

    let mut sections = Vec::new();

    // ═══ TIER 0: THE GIST ═══
    let project_context = discover_project_context(index);
    sections.push(format!(
        "═══ THE GIST ═══\n{} ({} files, {} LOC)\n{}",
        project_name,
        stats.file_count,
        stats.total_loc,
        if !project_context.trim().is_empty() {
            truncate_str(&project_context, 300).to_string()
        } else {
            "A software project.".to_string()
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
            .take(6)
            .map(|(d, c)| format!("{}/ ({} files)", d, c))
            .collect();
        sections.push(format!("\n\n═══ KEY AREAS ═══\n{}", areas.join("\n")));
    }

    // ═══ TIER 2: PRIORITY FILES ═══
    let changed: HashSet<PathBuf> = context.all_changed_files().into_iter().cloned().collect();

    // Collect top priority files for code preview
    let mut priority_files: Vec<PathBuf> = changed.iter().take(2).cloned().collect();

    if !changed.is_empty() {
        let mut s = String::from("\n\n═══ PRIORITY FILES ═══\n[CHANGED] Read these first:");
        for path in changed.iter().take(6) {
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
        .take(4)
        .collect();

    // Add top complex file to priority list
    if let Some(h) = hot.first() {
        if priority_files.len() < 3 {
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

    // TODOs
    let todos: Vec<_> = index
        .patterns
        .iter()
        .filter(|p| matches!(p.kind, PatternKind::TodoMarker))
        .take(4)
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

    // ═══ CODE PREVIEW (first 35 lines of top priority files) ═══
    if !priority_files.is_empty() {
        let mut preview_section = String::from("\n\n═══ CODE PREVIEW ═══");
        for path in priority_files.iter().take(3) {
            let full_path = index.root.join(path);
            if let Ok(content) = std::fs::read_to_string(&full_path) {
                let lines: String = content
                    .lines()
                    .take(35)
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
        .take(35)
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
            let terms = g.to_prompt_context(6);
            if !terms.trim().is_empty() {
                sections.push(format!("\n\nTERMINOLOGY:\n{}", terms));
            }
        }
    }

    // ═══ INSTRUCTIONS ═══
    sections.push(String::from(
        "\n\n═══ YOUR TASK ═══
You MUST return EXACTLY 10 suggestions. Not 5, not 8, not 12. Exactly 10.

TIERED DISCOVERY:
1. THE GIST → understand project purpose
2. KEY AREAS → identify interesting modules
3. PRIORITY FILES → pick files to investigate
4. Use grep/head to find and read code

SURGICAL COMMANDS (save tokens!):
• grep -n 'fn foo' <file> → find line number
• sed -n '45,75p' <file> → read 30 lines around match
• head -50 <file> → read file start
• rg 'pattern' → search entire codebase

EXAMPLE WORKFLOW:
1. See 'handles API calls' in summary
2. grep -n 'async fn' src/api.rs → find functions
3. sed -n '120,160p' src/api.rs → read around interesting function
4. Find issue → record with evidence

RULES:
- Only suggest issues you've verified by reading actual code
- Return as JSON array
- COUNT YOUR SUGGESTIONS BEFORE RESPONDING: You must have exactly 10 items in the array",
    ));

    sections.join("")
}
