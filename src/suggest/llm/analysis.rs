use super::client::{call_llm_with_usage, truncate_str};
use super::models::{Model, Usage};
use super::parse::parse_codebase_suggestions;
use super::prompts::{ANALYZE_CODEBASE_SYSTEM, ASK_QUESTION_SYSTEM};
use crate::cache::DomainGlossary;
use crate::context::WorkContext;
use crate::index::{CodebaseIndex, PatternKind, SymbolKind};
use crate::suggest::Suggestion;
use std::collections::HashSet;
use std::path::PathBuf;

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
        .filter(|s| matches!(s.kind, SymbolKind::Function | SymbolKind::Struct | SymbolKind::Enum))
        .take(100)
        .map(|s| format!("{:?}: {}", s.kind, s.name))
        .collect();

    let memory_section = repo_memory
        .as_deref()
        .filter(|m| !m.trim().is_empty())
        .map(|m| format!("\n\nPROJECT NOTES:\n{}", m))
        .unwrap_or_default();

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

    let response =
        call_llm_with_usage(ASK_QUESTION_SYSTEM, &user, Model::Balanced, false).await?;
    Ok((response.content, response.usage))
}

// ═══════════════════════════════════════════════════════════════════════════
//  UNIFIED CODEBASE ANALYSIS
// ═══════════════════════════════════════════════════════════════════════════

/// Analyze entire codebase with @preset/smart for quality suggestions
///
/// This is the main entry point for generating high-quality suggestions.
/// Uses smart context building to pack maximum insight into the prompt.
/// Returns suggestions and usage stats for cost tracking.
///
/// The optional `glossary` provides domain-specific terminology to help
/// the LLM use the correct terms in suggestion summaries.
pub async fn analyze_codebase(
    index: &CodebaseIndex,
    context: &WorkContext,
    repo_memory: Option<String>,
    glossary: Option<&DomainGlossary>,
) -> anyhow::Result<(Vec<Suggestion>, Option<Usage>)> {
    let user_prompt = build_codebase_context(index, context, repo_memory.as_deref(), glossary);

    // Use Smart preset for quality reasoning on suggestions
    let response =
        call_llm_with_usage(ANALYZE_CODEBASE_SYSTEM, &user_prompt, Model::Smart, true).await?;

    let suggestions = parse_codebase_suggestions(&response.content)?;
    Ok((suggestions, response.usage))
}

/// Build rich context from codebase index for the LLM prompt
fn build_codebase_context(
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
        "CODEBASE: {} ({} files, {} LOC)\nBRANCH: {} | FOCUS: {}",
        project_name,
        stats.file_count,
        stats.total_loc,
        context.branch,
        context.inferred_focus.as_deref().unwrap_or("general"),
    ));

    // Uncommitted changes FIRST (highest priority)
    if !context.uncommitted_files.is_empty() || !context.staged_files.is_empty() {
        let mut changes_section = String::from("\n\nACTIVELY WORKING ON [CHANGED]:");
        for file in context
            .uncommitted_files
            .iter()
            .chain(context.staged_files.iter())
            .take(15)
        {
            // Include file details if we have them
            if let Some(file_index) = index.files.get(file) {
                let exports: Vec<_> = file_index
                    .symbols
                    .iter()
                    .filter(|s| s.visibility == crate::index::Visibility::Public)
                    .take(5)
                    .map(|s| s.name.as_str())
                    .collect();
                let exports_str = if exports.is_empty() {
                    String::new()
                } else {
                    format!(" exports: {}", exports.join(", "))
                };
                changes_section.push_str(&format!(
                    "\n- {} ({} LOC){}",
                    file.display(),
                    file_index.loc,
                    exports_str
                ));
            } else {
                changes_section.push_str(&format!("\n- {}", file.display()));
            }
        }
        sections.push(changes_section);
    }

    // Blast radius: files affected by the current changes (direct importers + direct deps)
    if !context.all_changed_files().is_empty() {
        let changed: HashSet<PathBuf> =
            context.all_changed_files().into_iter().cloned().collect();
        let mut related: HashSet<PathBuf> = HashSet::new();

        for c in &changed {
            if let Some(file_index) = index.files.get(c) {
                // Who imports this file?
                for u in file_index.summary.used_by.iter().take(10) {
                    related.insert(u.clone());
                }
                // What does this file depend on?
                for d in file_index.summary.depends_on.iter().take(10) {
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
            let mut blast = String::from("\n\nBLAST RADIUS (related to [CHANGED]):");
            for path in list.into_iter().take(15) {
                blast.push_str(&format!("\n- {}", path.display()));
            }
            sections.push(blast);
        }
    }

    // Repository memory / conventions
    if let Some(mem) = repo_memory {
        if !mem.trim().is_empty() {
            sections.push(format!("\n\nREPO MEMORY (CONVENTIONS/DECISIONS):\n{}", mem));
        }
    }

    // Domain glossary
    if let Some(glossary) = glossary {
        if !glossary.is_empty() {
            let terms = glossary.to_prompt_context(20);
            if !terms.trim().is_empty() {
                sections.push(format!("\n\nDOMAIN TERMINOLOGY (use these terms):\n{}", terms));
            }
        }
    }

    // Codebase summary stats
    sections.push(format!(
        "\n\nCODEBASE STRUCTURE:\n- Root: {}\n- Files: {}\n- Symbols: {}\n- Patterns: {}",
        project_name, stats.file_count, stats.symbol_count, stats.pattern_count
    ));

    // File hotspots (largest/most complex)
    let mut hotspots = index.files.values().collect::<Vec<_>>();
    hotspots.sort_by(|a, b| b.complexity.partial_cmp(&a.complexity).unwrap());
    let hot: Vec<_> = hotspots
        .iter()
        .filter(|f| f.complexity > 20.0 || f.loc > 500)
        .take(10)
        .collect();

    if !hot.is_empty() {
        let mut hot_section = String::from("\n\nHOTSPOTS (complex or large files):");
        for file in hot {
            let rel = &file.path;
            hot_section.push_str(&format!(
                "\n- {} ({} LOC, complexity {:.0})",
                rel.display(),
                file.loc,
                file.complexity
            ));
        }
        sections.push(hot_section);
    }

    // TODOs and FIXMEs found in code (actionable items from the developer)
    let todos: Vec<_> = index
        .patterns
        .iter()
        .filter(|p| matches!(p.kind, PatternKind::TodoMarker))
        .take(10)
        .collect();

    if !todos.is_empty() {
        let mut todos_section = String::from("\n\nTODO/FIXME MARKERS IN CODE:");
        for todo in &todos {
            todos_section.push_str(&format!(
                "\n- {}:{} - {}",
                todo.file.file_name().and_then(|n| n.to_str()).unwrap_or("?"),
                todo.line,
                truncate_str(&todo.description, 70)
            ));
        }
        sections.push(todos_section);
    }

    // Final instruction - open-ended
    sections.push(String::from(
        "\n\nLook for bugs, security issues, performance problems, missing error handling, \
         UX improvements, and feature opportunities. Prioritize the [CHANGED] files (and BLAST RADIUS). \
         Give me varied, specific suggestions - not just code organization advice.",
    ));

    sections.join("")
}
