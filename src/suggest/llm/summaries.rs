use super::agentic::{call_llm_agentic, schema_to_response_format};
use super::models::{Model, Usage};
use super::parse::{parse_summaries_and_terms_response, SummariesAndTerms};
use super::prompts::SUMMARY_BATCH_SYSTEM;
use crate::cache::DomainGlossary;
use crate::context::WorkContext;
use crate::index::{CodebaseIndex, SymbolKind};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

// ═══════════════════════════════════════════════════════════════════════════
//  BATCH PROCESSING CONSTANTS
// ═══════════════════════════════════════════════════════════════════════════

/// Number of files to process in a single LLM batch request.
/// Larger batches reduce API overhead but increase per-request latency.
pub const SUMMARY_BATCH_SIZE: usize = 16;

/// Number of concurrent batch requests for summary generation.
const SUMMARY_CONCURRENCY: usize = 4;

/// Result from a single batch of file summaries
pub struct SummaryBatchResult {
    pub summaries: HashMap<PathBuf, String>,
    /// Domain terms extracted from these files
    pub terms: HashMap<String, String>,
    /// Domain terms mapped to their source files
    pub terms_by_file: HashMap<PathBuf, HashMap<String, String>>,
    pub usage: Option<Usage>,
}

fn build_fallback_summary(index: &CodebaseIndex, path: &Path) -> Option<String> {
    let file_index = index.files.get(path)?;

    let mut parts: Vec<String> = Vec::new();

    let purpose = file_index.summary.purpose.trim();
    if !purpose.is_empty() {
        parts.push(purpose.trim_end_matches('.').to_string());
    } else {
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("this file");
        parts.push(format!("{} supports part of the project logic", name));
    }

    let function_count = file_index
        .symbols
        .iter()
        .filter(|s| matches!(s.kind, SymbolKind::Function | SymbolKind::Method))
        .count();
    let type_count = file_index
        .symbols
        .iter()
        .filter(|s| {
            matches!(
                s.kind,
                SymbolKind::Struct
                    | SymbolKind::Class
                    | SymbolKind::Interface
                    | SymbolKind::Trait
                    | SymbolKind::Enum
            )
        })
        .count();
    if function_count > 0 || type_count > 0 {
        parts.push(format!(
            "It contains {} function{} and {} type{}",
            function_count,
            if function_count == 1 { "" } else { "s" },
            type_count,
            if type_count == 1 { "" } else { "s" }
        ));
    }

    if !file_index.summary.depends_on.is_empty() {
        let deps = file_index
            .summary
            .depends_on
            .iter()
            .take(3)
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>();
        if !deps.is_empty() {
            parts.push(format!("It depends on {}", deps.join(", ")));
        }
    }

    if !file_index.summary.used_by.is_empty() {
        parts.push(format!(
            "It is used by {} other file{}",
            file_index.summary.used_by.len(),
            if file_index.summary.used_by.len() == 1 {
                ""
            } else {
                "s"
            }
        ));
    }

    if parts.is_empty() {
        return None;
    }

    Some(format!("{}.", parts.join(". ")))
}

fn summary_batch_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "summaries": {
                "type": "object",
                "additionalProperties": { "type": "string" }
            },
            "terms": {
                "type": "object",
                "additionalProperties": { "type": "string" }
            },
            "terms_by_file": {
                "type": "object",
                "additionalProperties": {
                    "type": "object",
                    "additionalProperties": { "type": "string" }
                }
            }
        },
        "required": ["summaries"]
    })
}

/// Discover project context from README and key files
pub fn discover_project_context(index: &CodebaseIndex) -> String {
    // Try README first
    if let Some(readme) = try_read_readme(&index.root) {
        return extract_readme_summary(&readme);
    }

    // Try package metadata (Cargo.toml, package.json, pyproject)
    if let Some(desc) = try_read_package_description(&index.root) {
        return desc;
    }

    // Fall back to analyzing directory structure
    analyze_project_structure(index)
}

fn try_read_readme(root: &Path) -> Option<String> {
    let candidates = ["README.md", "readme.md", "README.txt", "README"];
    for name in candidates {
        let path = root.join(name);
        if path.exists() {
            if let Ok(content) = std::fs::read_to_string(path) {
                return Some(content);
            }
        }
    }
    None
}

fn extract_readme_summary(content: &str) -> String {
    // Take first 10-15 lines of README, skipping badges
    let lines: Vec<&str> = content
        .lines()
        .filter(|line| {
            let l = line.trim();
            !l.is_empty()
                && !l.starts_with("[!")
                && !l.contains("shields.io")
                && !l.starts_with("![](")
        })
        .take(15)
        .collect();
    lines.join("\n")
}

fn try_read_package_description(root: &Path) -> Option<String> {
    if let Some(desc) = extract_cargo_description(&root.join("Cargo.toml")) {
        return Some(desc);
    }
    if let Some(desc) = extract_package_json_description(&root.join("package.json")) {
        return Some(desc);
    }
    if let Some(desc) = extract_pyproject_description(&root.join("pyproject.toml")) {
        return Some(desc);
    }
    None
}

fn extract_cargo_description(path: &Path) -> Option<String> {
    if !path.exists() {
        return None;
    }
    let content = std::fs::read_to_string(path).ok()?;
    for line in content.lines() {
        if line.trim().starts_with("description =") {
            return Some(
                line.split('=')
                    .nth(1)
                    .unwrap_or("")
                    .trim()
                    .trim_matches('"')
                    .to_string(),
            );
        }
    }
    None
}

fn extract_package_json_description(path: &Path) -> Option<String> {
    if !path.exists() {
        return None;
    }
    let content = std::fs::read_to_string(path).ok()?;
    let json: serde_json::Value = serde_json::from_str(&content).ok()?;
    json.get("description")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}

fn extract_pyproject_description(path: &Path) -> Option<String> {
    if !path.exists() {
        return None;
    }
    let content = std::fs::read_to_string(path).ok()?;
    for line in content.lines() {
        if line.trim().starts_with("description =") {
            return Some(
                line.split('=')
                    .nth(1)
                    .unwrap_or("")
                    .trim()
                    .trim_matches('"')
                    .to_string(),
            );
        }
    }
    None
}

fn analyze_project_structure(index: &CodebaseIndex) -> String {
    let mut hints = Vec::new();

    // Basic file structure
    let mut key_dirs = std::collections::HashSet::new();
    for path in index.files.keys() {
        if let Some(parent) = path.parent() {
            if let Some(dir) = parent.file_name().and_then(|n| n.to_str()) {
                if ["src", "lib", "app", "cmd", "internal", "pkg"].contains(&dir) {
                    key_dirs.insert(dir.to_string());
                }
            }
        }
    }
    let mut key_dirs: Vec<_> = key_dirs.into_iter().collect();
    key_dirs.sort();

    if !key_dirs.is_empty() {
        hints.push(format!("Key directories: {}", key_dirs.join(", ")));
    }

    // Identify technologies
    let mut technologies = Vec::new();
    let files: Vec<_> = index.files.keys().collect();

    if files
        .iter()
        .any(|p| p.extension().map(|e| e == "rs").unwrap_or(false))
    {
        technologies.push("Rust");
    }
    if files.iter().any(|p| {
        p.extension()
            .map(|e| e == "ts" || e == "tsx")
            .unwrap_or(false)
    }) {
        technologies.push("TypeScript");
    }
    if files.iter().any(|p| {
        p.extension()
            .map(|e| e == "js" || e == "jsx")
            .unwrap_or(false)
    }) {
        technologies.push("JavaScript");
    }
    if files
        .iter()
        .any(|p| p.extension().map(|e| e == "py").unwrap_or(false))
    {
        technologies.push("Python");
    }
    if files
        .iter()
        .any(|p| p.extension().map(|e| e == "go").unwrap_or(false))
    {
        technologies.push("Go");
    }

    if !technologies.is_empty() {
        hints.push(format!("Technologies: {}", technologies.join(", ")));
    }

    // File count summary
    let stats = index.stats();
    hints.push(format!(
        "Total: {} files, {} symbols",
        stats.file_count, stats.symbol_count
    ));

    hints.join("\n")
}

// ═══════════════════════════════════════════════════════════════════════════
//  FILE SUMMARIES GENERATION
// ═══════════════════════════════════════════════════════════════════════════

/// Generate summaries for a specific list of files with project context
/// Uses aggressive parallel batch processing for speed
/// Also extracts domain terminology for the glossary
pub async fn generate_summaries_for_files(
    index: &CodebaseIndex,
    files: &[PathBuf],
    project_context: &str,
) -> anyhow::Result<(
    HashMap<PathBuf, String>,
    DomainGlossary,
    Option<Usage>,
    Vec<PathBuf>,
)> {
    let batch_size = SUMMARY_BATCH_SIZE;
    let concurrency = SUMMARY_CONCURRENCY;

    let batches: Vec<_> = files.chunks(batch_size).collect();

    let mut all_summaries = HashMap::new();
    let mut glossary = DomainGlossary::new();
    let mut total_usage = Usage::default();
    let mut failed_files: HashSet<PathBuf> = HashSet::new();

    // Process batches with limited concurrency
    for batch_group in batches.chunks(concurrency) {
        // Run concurrent batches
        let futures: Vec<_> = batch_group
            .iter()
            .map(|batch| {
                let batch_files: Vec<PathBuf> = batch.to_vec();
                async move {
                    let result = generate_summary_batch(index, &batch_files, project_context).await;
                    (batch_files, result)
                }
            })
            .collect();

        let results = futures::future::join_all(futures).await;

        for (batch_files, result) in results {
            match result {
                Ok(batch_result) => {
                    let missing_files: Vec<PathBuf> = batch_files
                        .iter()
                        .filter(|p| !batch_result.summaries.contains_key(*p))
                        .cloned()
                        .collect();

                    // Collect summaries
                    all_summaries.extend(batch_result.summaries.clone());

                    // Collect terms into glossary
                    if !batch_result.terms_by_file.is_empty() {
                        for (file, terms) in batch_result.terms_by_file {
                            for (term, definition) in terms {
                                glossary.add_term(term, definition, file.clone());
                            }
                        }
                    } else {
                        for (term, definition) in batch_result.terms {
                            // Backward compatibility: associate term with files from this batch
                            for file in batch_result.summaries.keys() {
                                glossary.add_term(term.clone(), definition.clone(), file.clone());
                            }
                        }
                    }

                    if let Some(usage) = batch_result.usage {
                        total_usage.prompt_tokens += usage.prompt_tokens;
                        total_usage.completion_tokens += usage.completion_tokens;
                        total_usage.total_tokens += usage.total_tokens;
                    }

                    if !missing_files.is_empty() {
                        // Try deterministic fallback summaries so one malformed model response
                        // does not block the whole pipeline.
                        for missing in missing_files {
                            if let Some(fallback) = build_fallback_summary(index, &missing) {
                                all_summaries.insert(missing.clone(), fallback);
                                failed_files.remove(&missing);
                            } else {
                                failed_files.insert(missing);
                            }
                        }
                    }
                }
                Err(_e) => {
                    // Batch failed. Use deterministic fallback summaries where possible
                    // to keep startup resilient and summaries-first gate satisfied.
                    for file in batch_files {
                        if let Some(fallback) = build_fallback_summary(index, &file) {
                            all_summaries.insert(file.clone(), fallback);
                            failed_files.remove(&file);
                        } else {
                            failed_files.insert(file);
                        }
                    }
                }
            }
        }
    }

    let final_usage = if total_usage.total_tokens > 0 {
        Some(total_usage)
    } else {
        None
    };

    Ok((
        all_summaries,
        glossary,
        final_usage,
        failed_files.into_iter().collect(),
    ))
}

/// Categorize files by priority for smart summarization
pub fn prioritize_files_for_summary(
    index: &CodebaseIndex,
    context: &WorkContext,
    files_needing_summary: &[PathBuf],
) -> (Vec<PathBuf>, Vec<PathBuf>, Vec<PathBuf>) {
    let mut high_priority = Vec::new();
    let mut medium_priority = Vec::new();
    let mut low_priority = Vec::new();

    let changed_files: std::collections::HashSet<_> =
        context.all_changed_files().into_iter().collect();

    for path in files_needing_summary {
        // Check if file is in the index
        let file_index = match index.files.get(path) {
            Some(fi) => fi,
            None => {
                low_priority.push(path.clone());
                continue;
            }
        };

        // Tier 1: Changed files or high complexity
        if changed_files.contains(path) || file_index.complexity > 20.0 || file_index.loc > 500 {
            high_priority.push(path.clone());
            continue;
        }

        // Tier 2: Recent modification or in focus area
        let is_recent = file_index.last_modified.timestamp()
            > (chrono::Utc::now() - chrono::Duration::days(7)).timestamp();
        let in_focus = context
            .inferred_focus
            .as_ref()
            .map(|focus| path.to_string_lossy().contains(focus))
            .unwrap_or(false);

        if is_recent || in_focus {
            medium_priority.push(path.clone());
            continue;
        }

        // Tier 3: Everything else
        low_priority.push(path.clone());
    }

    (high_priority, medium_priority, low_priority)
}

/// Generate summaries for a single batch of files
/// Also extracts domain-specific terminology for the glossary
async fn generate_summary_batch(
    index: &CodebaseIndex,
    files: &[PathBuf],
    project_context: &str,
) -> anyhow::Result<SummaryBatchResult> {
    let user_prompt = build_batch_context(index, files, project_context);

    let response_format = schema_to_response_format("summary_batch", summary_batch_schema());
    let response = call_llm_agentic(
        SUMMARY_BATCH_SYSTEM,
        &user_prompt,
        Model::Speed,
        &index.root,
        false,
        4,
        Some(response_format),
    )
    .await?;

    let SummariesAndTerms {
        summaries,
        terms,
        terms_by_file,
    } = parse_summaries_and_terms_response(&response.content, &index.root)?;

    Ok(SummaryBatchResult {
        summaries,
        terms,
        terms_by_file,
        usage: response.usage,
    })
}

/// Build context for a batch of files
fn build_batch_context(index: &CodebaseIndex, files: &[PathBuf], project_context: &str) -> String {
    let project_name = index
        .root
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("project");

    let mut sections = Vec::new();

    // Include project context at the top
    sections.push(format!(
        "PROJECT: {}\n\n=== PROJECT CONTEXT (use this to understand file purposes) ===\n{}\n=== END PROJECT CONTEXT ===\n\nFILES TO SUMMARIZE:",
        project_name, project_context
    ));

    for path in files {
        if let Some(file_index) = index.files.get(path) {
            let func_count = file_index
                .symbols
                .iter()
                .filter(|s| matches!(s.kind, SymbolKind::Function | SymbolKind::Method))
                .count();

            let struct_count = file_index
                .symbols
                .iter()
                .filter(|s| {
                    matches!(
                        s.kind,
                        SymbolKind::Struct
                            | SymbolKind::Class
                            | SymbolKind::Interface
                            | SymbolKind::Trait
                    )
                })
                .count();

            // Get public exports
            let exports: Vec<_> = file_index
                .symbols
                .iter()
                .filter(|s| s.visibility == crate::index::Visibility::Public)
                .take(10)
                .map(|s| s.name.as_str())
                .collect();

            let exports_str = if exports.is_empty() {
                "none".to_string()
            } else {
                exports.join(", ")
            };

            let deps: Vec<_> = file_index
                .dependencies
                .iter()
                .filter(|d| !d.is_external)
                .take(5)
                .map(|d| d.import_path.as_str())
                .collect();

            let deps_str = if deps.is_empty() {
                "none".to_string()
            } else {
                deps.join(", ")
            };

            sections.push(format!(
                "\n---\nFILE: {}\n{} LOC | {} functions | {} structs\nExports: {}\nImports: {}",
                path.display(),
                file_index.loc,
                func_count,
                struct_count,
                exports_str,
                deps_str
            ));

            // Add doc comments if available
            if let Ok(content) = std::fs::read_to_string(index.root.join(path)) {
                let doc_lines: Vec<_> = content
                    .lines()
                    .take(10)
                    .filter(|l| {
                        l.starts_with("//!")
                            || l.starts_with("///")
                            || l.starts_with("#")
                            || l.starts_with("\"\"\"")
                    })
                    .take(2)
                    .collect();

                if !doc_lines.is_empty() {
                    sections.push(format!("Doc: {}", doc_lines.join(" ")));
                }
            }
        }
    }

    sections.join("")
}
