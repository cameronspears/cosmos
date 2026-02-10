use super::client::{call_llm_structured, StructuredResponse};
use super::models::{Model, Usage};
use super::prompts::GROUPING_CLASSIFY_SYSTEM;
use crate::cache::normalize_summary_path;
use crate::grouping::Layer;
use crate::index::{CodebaseIndex, Language};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub const GROUPING_AI_FILES_PER_REQUEST: usize = 3;
pub const GROUPING_AI_MAX_REQUESTS: usize = 3;
pub const GROUPING_AI_MIN_CONFIDENCE: f64 = 0.8;

#[derive(Debug, Clone)]
pub struct GroupingAiSuggestion {
    pub path: PathBuf,
    pub layer: Layer,
    pub confidence: f64,
}

#[derive(Serialize)]
struct FileContext {
    path: String,
    language: String,
    loc: usize,
    purpose: String,
    exports: Vec<String>,
    symbols: Vec<String>,
    external_imports: Vec<String>,
    internal_deps: Vec<String>,
}

#[derive(Deserialize)]
struct GroupingAiResponse {
    files: Vec<GroupingAiFile>,
}

#[derive(Deserialize)]
struct GroupingAiFile {
    path: String,
    layer: String,
    confidence: f64,
}

fn grouping_ai_response_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "files": {
                "type": "array",
                "items": {
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {
                        "path": { "type": "string" },
                        "layer": { "type": "string" },
                        "confidence": { "type": "number" }
                    },
                    "required": ["path", "layer", "confidence"]
                }
            }
        },
        "required": ["files"]
    })
}

pub async fn classify_grouping_candidates(
    index: &CodebaseIndex,
    candidates: &[PathBuf],
) -> anyhow::Result<(Vec<GroupingAiSuggestion>, Option<Usage>)> {
    if candidates.is_empty() {
        return Ok((Vec::new(), None));
    }

    let file_contexts: Vec<FileContext> = candidates
        .iter()
        .filter_map(|path| index.files.get(path).map(|f| build_file_context(path, f)))
        .collect();

    if file_contexts.is_empty() {
        return Ok((Vec::new(), None));
    }

    let user = format!(
        "Classify these files into architectural layers.\n\nFILES:\n{}",
        serde_json::to_string_pretty(&file_contexts)?
    );

    let StructuredResponse {
        data: parsed,
        usage,
        ..
    } = call_llm_structured::<GroupingAiResponse>(
        GROUPING_CLASSIFY_SYSTEM,
        &user,
        Model::Balanced,
        "grouping_classification",
        grouping_ai_response_schema(),
    )
    .await?;

    let mut suggestions = Vec::new();
    for file in parsed.files {
        let path = normalize_summary_path(Path::new(file.path.trim()), &index.root);
        if !index.files.contains_key(&path) {
            continue;
        }
        let layer = match Layer::parse(&file.layer) {
            Some(layer) => layer,
            None => continue,
        };
        let confidence = file.confidence.clamp(0.0, 1.0);
        suggestions.push(GroupingAiSuggestion {
            path,
            layer,
            confidence,
        });
    }

    Ok((suggestions, usage))
}

fn build_file_context(path: &std::path::Path, file: &crate::index::FileIndex) -> FileContext {
    let exports = file.summary.exports.iter().take(6).cloned().collect();
    let symbols = file
        .symbols
        .iter()
        .take(8)
        .map(|s| format!("{:?}: {}", s.kind, s.name))
        .collect();
    let external_imports = file
        .dependencies
        .iter()
        .filter(|d| d.is_external)
        .take(8)
        .map(|d| d.import_path.clone())
        .collect();
    let internal_deps = file
        .summary
        .depends_on
        .iter()
        .take(6)
        .map(|p| p.display().to_string())
        .collect();

    FileContext {
        path: path.display().to_string(),
        language: language_label(file.language).to_string(),
        loc: file.loc,
        purpose: file.summary.purpose.clone(),
        exports,
        symbols,
        external_imports,
        internal_deps,
    }
}

fn language_label(language: Language) -> &'static str {
    match language {
        Language::Rust => "rust",
        Language::JavaScript => "javascript",
        Language::TypeScript => "typescript",
        Language::Python => "python",
        Language::Go => "go",
        Language::Unknown => "unknown",
    }
}
