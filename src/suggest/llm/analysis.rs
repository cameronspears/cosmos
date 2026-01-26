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

/// Analyze codebase with compact context and minimal, surgical tool use
///
/// Strategy:
/// 1. Start with synthesized context (summaries, not full files)
/// 2. Model can make 1-3 targeted tool calls to verify specific issues
/// 3. Uses gpt-oss-120b for cost efficiency (~$0.02-0.05 total)
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
    let response = call_llm_agentic(
        ANALYZE_CODEBASE_AGENTIC_SYSTEM,
        &user_prompt,
        Model::Speed,
        repo_root,
        false,
    )
    .await?;

    let suggestions = parse_codebase_suggestions(&response.content)?;
    Ok((suggestions, None))
}

/// Build a lean prompt using summaries, not full file content
///
/// The model gets:
/// - Project context and purpose
/// - Compact file summaries (what each file does)
/// - List of changed files with their purposes
/// - Permission to read specific files IF needed for verification
///
/// This keeps the initial prompt small (~2-4K tokens) for fast first response.
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

    // Get project context from README/package files
    let project_context = discover_project_context(index);

    let mut sections = Vec::new();

    // Header
    sections.push(format!(
        "CODEBASE: {} ({} files, {} LOC) on branch '{}'",
        project_name, stats.file_count, stats.total_loc, context.branch,
    ));

    // Project context
    if !project_context.trim().is_empty() {
        sections.push(format!(
            "\n\nWHAT THIS PROJECT DOES:\n{}",
            truncate_str(&project_context, 600)
        ));
    }

    // Changed files with summaries (highest priority)
    let changed: HashSet<PathBuf> = context.all_changed_files().into_iter().cloned().collect();
    if !changed.is_empty() {
        let mut changed_section = String::from("\n\nCHANGED FILES (focus here):");
        for path in changed.iter().take(10) {
            let summary = index
                .files
                .get(path)
                .map(|f| f.summary.purpose.as_str())
                .unwrap_or("(new file)");
            changed_section.push_str(&format!(
                "\n• {} - {}",
                path.display(),
                truncate_str(summary, 80)
            ));
        }
        sections.push(changed_section);
    }

    // Hotspots with summaries
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
        .take(5)
        .collect();

    if !hot.is_empty() {
        let mut hot_section = String::from("\n\nCOMPLEX FILES (potential issues):");
        for file in hot {
            hot_section.push_str(&format!(
                "\n• {} ({} LOC) - {}",
                file.path.display(),
                file.loc,
                truncate_str(&file.summary.purpose, 60)
            ));
        }
        sections.push(hot_section);
    }

    // Key file summaries for broader context
    let other_summaries: Vec<_> = index
        .files
        .values()
        .filter(|f| !changed.contains(&f.path))
        .filter(|f| !f.summary.purpose.is_empty())
        .take(15)
        .map(|f| {
            format!(
                "• {}: {}",
                f.path.display(),
                truncate_str(&f.summary.purpose, 50)
            )
        })
        .collect();

    if !other_summaries.is_empty() {
        sections.push(format!(
            "\n\nOTHER KEY FILES:\n{}",
            other_summaries.join("\n")
        ));
    }

    // TODOs
    let todos: Vec<_> = index
        .patterns
        .iter()
        .filter(|p| matches!(p.kind, PatternKind::TodoMarker))
        .take(5)
        .collect();

    if !todos.is_empty() {
        let mut todos_section = String::from("\n\nTODO/FIXME:");
        for todo in &todos {
            todos_section.push_str(&format!(
                "\n• {}:{} - {}",
                todo.file.display(),
                todo.line,
                truncate_str(&todo.description, 50)
            ));
        }
        sections.push(todos_section);
    }

    // Repo memory
    let memory_section = format_repo_memory_section(repo_memory, "CONVENTIONS");
    if !memory_section.is_empty() {
        sections.push(memory_section);
    }

    // Glossary
    if let Some(glossary) = glossary {
        if !glossary.is_empty() {
            let terms = glossary.to_prompt_context(10);
            if !terms.trim().is_empty() {
                sections.push(format!("\n\nTERMINOLOGY:\n{}", terms));
            }
        }
    }

    // Instructions - encourage minimal tool use
    sections.push(String::from(
        "\n\nINSTRUCTIONS:
Based on the summaries above, identify 10-15 improvement opportunities.
You may use `head -100 <file>` to verify specific issues, but MINIMIZE tool calls.
Most suggestions should be derivable from the summaries + your expertise.
Only read files when you need to confirm a specific detail.
After gathering evidence, return your suggestions as JSON.",
    ));

    sections.join("")
}
