use super::agentic::call_llm_agentic;
use super::client::{call_llm_with_usage, truncate_str};
use super::models::{Model, Usage};
use super::parse::parse_codebase_suggestions;
use super::prompt_utils::format_repo_memory_section;
use super::prompts::{ANALYZE_CODEBASE_AGENTIC_SYSTEM, ASK_QUESTION_SYSTEM};
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
//  AGENTIC CODEBASE ANALYSIS
// ═══════════════════════════════════════════════════════════════════════════

/// Analyze codebase using agentic exploration for highest accuracy
///
/// Unlike `analyze_codebase`, this lets the model explore the codebase with shell
/// commands before generating suggestions. This eliminates hallucinations because
/// the model only suggests issues for code it has actually read.
///
/// Uses Model::Smart (Opus 4.5) for best reasoning during exploration.
pub async fn analyze_codebase_agentic(
    repo_root: &Path,
    index: &CodebaseIndex,
    context: &WorkContext,
    repo_memory: Option<String>,
    glossary: Option<&DomainGlossary>,
) -> anyhow::Result<(Vec<Suggestion>, Option<Usage>)> {
    let user_prompt =
        build_agentic_exploration_prompt(index, context, repo_memory.as_deref(), glossary);

    // Use Smart model (Opus 4.5) for best reasoning during exploration
    let response = call_llm_agentic(
        ANALYZE_CODEBASE_AGENTIC_SYSTEM,
        &user_prompt,
        Model::Smart,
        repo_root,
        false, // Not JSON mode - model explores first, then returns JSON
    )
    .await?;

    // Parse the final JSON response
    let suggestions = parse_codebase_suggestions(&response.content)?;

    // Note: Agentic calls don't currently return usage stats
    // TODO: Track usage across tool calls in agentic loop
    Ok((suggestions, None))
}

/// Build exploration prompt for agentic analysis
///
/// This prompt guides the model on where to focus its exploration,
/// but doesn't include actual code - the model reads it via shell.
fn build_agentic_exploration_prompt(
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

    // Header with overview
    sections.push(format!(
        "CODEBASE: {} ({} files, {} LOC)\nBRANCH: {}",
        project_name, stats.file_count, stats.total_loc, context.branch,
    ));

    // Focus areas - changed files are highest priority
    if !context.uncommitted_files.is_empty()
        || !context.staged_files.is_empty()
        || !context.untracked_files.is_empty()
    {
        let mut focus = String::from("\n\nFOCUS AREAS [CHANGED] - Read these files first:");
        for file in context
            .uncommitted_files
            .iter()
            .chain(context.staged_files.iter())
            .chain(context.untracked_files.iter())
            .take(10)
        {
            focus.push_str(&format!("\n- {}", file.display()));
        }
        sections.push(focus);
    }

    // Blast radius - related files to check
    if !context.all_changed_files().is_empty() {
        let changed: HashSet<PathBuf> = context.all_changed_files().into_iter().cloned().collect();
        let mut related: HashSet<PathBuf> = HashSet::new();

        for c in &changed {
            if let Some(file_index) = index.files.get(c) {
                for u in file_index.summary.used_by.iter().take(5) {
                    related.insert(u.clone());
                }
                for d in file_index.summary.depends_on.iter().take(5) {
                    related.insert(d.clone());
                }
            }
        }
        for c in &changed {
            related.remove(c);
        }

        if !related.is_empty() {
            let mut list: Vec<_> = related.into_iter().collect();
            list.sort();
            let mut blast = String::from("\n\nBLAST RADIUS (dependencies of changed files):");
            for path in list.into_iter().take(10) {
                blast.push_str(&format!("\n- {}", path.display()));
            }
            sections.push(blast);
        }
    }

    // Hotspots - complex files worth examining
    let mut hotspots = index.files.values().collect::<Vec<_>>();
    hotspots.sort_by(|a, b| {
        b.complexity
            .partial_cmp(&a.complexity)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let hot: Vec<_> = hotspots
        .iter()
        .filter(|f| f.complexity > HIGH_COMPLEXITY_THRESHOLD || f.loc > GOD_MODULE_LOC_THRESHOLD)
        .take(8)
        .collect();

    if !hot.is_empty() {
        let mut hot_section = String::from("\n\nHOTSPOTS (complex files worth examining):");
        for file in hot {
            hot_section.push_str(&format!("\n- {} ({} LOC)", file.path.display(), file.loc));
        }
        sections.push(hot_section);
    }

    // TODOs and FIXMEs
    let todos: Vec<_> = index
        .patterns
        .iter()
        .filter(|p| matches!(p.kind, PatternKind::TodoMarker))
        .take(5)
        .collect();

    if !todos.is_empty() {
        let mut todos_section = String::from("\n\nTODO/FIXME MARKERS:");
        for todo in &todos {
            todos_section.push_str(&format!(
                "\n- {}:{} - {}",
                todo.file.display(),
                todo.line,
                truncate_str(&todo.description, 60)
            ));
        }
        sections.push(todos_section);
    }

    // Repository memory / conventions
    let memory_section = format_repo_memory_section(repo_memory, "REPO CONVENTIONS");
    if !memory_section.is_empty() {
        sections.push(memory_section);
    }

    // Domain glossary
    if let Some(glossary) = glossary {
        if !glossary.is_empty() {
            let terms = glossary.to_prompt_context(15);
            if !terms.trim().is_empty() {
                sections.push(format!("\n\nDOMAIN TERMINOLOGY:\n{}", terms));
            }
        }
    }

    // Final instruction
    sections.push(String::from(
        "\n\nExplore the codebase using shell commands. Focus on [CHANGED] files first, \
         then check BLAST RADIUS and HOTSPOTS. Only suggest issues you've verified by reading the actual code.",
    ));

    sections.join("")
}
