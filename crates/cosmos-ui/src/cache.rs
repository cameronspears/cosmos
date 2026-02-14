//! Minimal cache layer for UI shell mode.

use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

const CACHE_DIR: &str = ".cosmos";
const WELCOME_SEEN_FILE: &str = "welcome_seen";
const REPO_MEMORY_FILE: &str = "memory.json";
const GLOSSARY_FILE: &str = "glossary.json";
const QUESTION_CACHE_FILE: &str = "question_cache.json";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResetOption {
    Index,
    Suggestions,
    Summaries,
    Glossary,
    Memory,
    GroupingAi,
    QuestionCache,
    PipelineMetrics,
    SuggestionQuality,
    ImplementationHarness,
    EvidencePack,
    DataNotice,
}

impl ResetOption {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Index => "Index & Symbols",
            Self::Suggestions => "Suggestions",
            Self::Summaries => "File Summaries",
            Self::Glossary => "Domain Glossary",
            Self::Memory => "Repo Memory",
            Self::GroupingAi => "Grouping AI",
            Self::QuestionCache => "Question Cache",
            Self::PipelineMetrics => "Pipeline Metrics",
            Self::SuggestionQuality => "Suggestion Quality",
            Self::ImplementationHarness => "Implementation Harness",
            Self::EvidencePack => "Evidence Pack",
            Self::DataNotice => "Data Notice Ack",
        }
    }

    pub fn description(&self) -> &'static str {
        match self {
            Self::Index => "clear cached index",
            Self::Suggestions => "clear cached suggestions",
            Self::Summaries => "clear cached summaries",
            Self::Glossary => "clear glossary",
            Self::Memory => "clear repo memory",
            Self::GroupingAi => "clear grouping cache",
            Self::QuestionCache => "clear question cache",
            Self::PipelineMetrics => "clear pipeline logs",
            Self::SuggestionQuality => "clear quality logs",
            Self::ImplementationHarness => "clear harness logs",
            Self::EvidencePack => "clear evidence cache",
            Self::DataNotice => "show data notice again",
        }
    }

    pub fn all() -> Vec<Self> {
        vec![
            Self::Index,
            Self::Suggestions,
            Self::Summaries,
            Self::Glossary,
            Self::Memory,
            Self::GroupingAi,
            Self::QuestionCache,
            Self::PipelineMetrics,
            Self::SuggestionQuality,
            Self::ImplementationHarness,
            Self::EvidencePack,
            Self::DataNotice,
        ]
    }

    pub fn defaults() -> Vec<Self> {
        vec![
            Self::Index,
            Self::Suggestions,
            Self::Summaries,
            Self::Glossary,
            Self::GroupingAi,
        ]
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RepoMemory {
    pub entries: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GlossaryEntry {
    pub name: String,
    pub definition: String,
    pub file: PathBuf,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DomainGlossary {
    pub terms: Vec<GlossaryEntry>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct QuestionCache {
    pub entries: std::collections::HashMap<String, String>,
}

#[derive(Debug, Clone)]
pub struct Cache {
    cache_dir: PathBuf,
}

impl Cache {
    pub fn new(project_root: &Path) -> Self {
        let cache_dir = project_root.join(CACHE_DIR);
        let _ = fs::create_dir_all(&cache_dir);
        Self { cache_dir }
    }

    pub fn load_repo_memory(&self) -> RepoMemory {
        self.load_json(REPO_MEMORY_FILE).unwrap_or_default()
    }

    pub fn load_glossary(&self) -> Option<DomainGlossary> {
        self.load_json(GLOSSARY_FILE)
    }

    pub fn load_question_cache(&self) -> Option<QuestionCache> {
        self.load_json(QUESTION_CACHE_FILE)
    }

    pub fn has_seen_welcome(&self) -> bool {
        self.cache_dir.join(WELCOME_SEEN_FILE).exists()
    }

    pub fn mark_welcome_seen(&self) -> anyhow::Result<()> {
        fs::create_dir_all(&self.cache_dir)?;
        fs::write(self.cache_dir.join(WELCOME_SEEN_FILE), b"seen")?;
        Ok(())
    }

    fn load_json<T>(&self, name: &str) -> Option<T>
    where
        T: for<'de> serde::Deserialize<'de>,
    {
        let path = self.cache_dir.join(name);
        let raw = fs::read_to_string(path).ok()?;
        serde_json::from_str(&raw).ok()
    }

    fn cache_file_for_option(&self, option: ResetOption) -> PathBuf {
        match option {
            ResetOption::Glossary => self.cache_dir.join(GLOSSARY_FILE),
            ResetOption::Memory => self.cache_dir.join(REPO_MEMORY_FILE),
            ResetOption::QuestionCache => self.cache_dir.join(QUESTION_CACHE_FILE),
            ResetOption::DataNotice => self.cache_dir.join("data_notice_seen"),
            ResetOption::Index => self.cache_dir.join("index.json"),
            ResetOption::Suggestions => self.cache_dir.join("suggestions.json"),
            ResetOption::Summaries => self.cache_dir.join("llm_summaries.json"),
            ResetOption::GroupingAi => self.cache_dir.join("grouping_ai.json"),
            ResetOption::PipelineMetrics => self.cache_dir.join("pipeline_metrics.jsonl"),
            ResetOption::SuggestionQuality => self.cache_dir.join("suggestion_quality.jsonl"),
            ResetOption::ImplementationHarness => {
                self.cache_dir.join("implementation_harness.jsonl")
            }
            ResetOption::EvidencePack => self.cache_dir.join("evidence_pack.json"),
        }
    }
}

pub async fn reset_cosmos(repo_path: &Path, options: &[ResetOption]) -> anyhow::Result<()> {
    let cache = Cache::new(repo_path);
    for option in options {
        let path = cache.cache_file_for_option(*option);
        if path.exists() {
            let _ = fs::remove_file(path);
        }
    }
    Ok(())
}
