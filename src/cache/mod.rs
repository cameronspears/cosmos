//! Cache module for Cosmos
//!
//! Persists suggestions and index data to .cosmos/ directory
//! to avoid redundant LLM calls and speed up startup.

use crate::index::CodebaseIndex;
use crate::suggest::Suggestion;
use chrono::{DateTime, Duration, Utc};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::path::{Path, PathBuf};

const CACHE_DIR: &str = ".cosmos";
const INDEX_CACHE_FILE: &str = "index.json";
const SUGGESTIONS_CACHE_FILE: &str = "suggestions.json";
const MEMORY_FILE: &str = "memory.json";
const GLOSSARY_FILE: &str = "glossary.json";
const GROUPING_AI_CACHE_FILE: &str = "grouping_ai.json";

/// Options for selective cache reset
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResetOption {
    /// Clear index.json - codebase structure, symbols, patterns
    Index,
    /// Clear suggestions.json - generated suggestions
    Suggestions,
    /// Clear llm_summaries.json - file summaries
    Summaries,
    /// Clear glossary.json - domain terminology
    Glossary,
    /// Clear memory.json - repo decisions/conventions
    Memory,
    /// Clear grouping_ai.json - AI grouping cache
    GroupingAi,
}

impl ResetOption {
    /// Get human-readable label for the option
    pub fn label(&self) -> &'static str {
        match self {
            ResetOption::Index => "Index & Symbols",
            ResetOption::Suggestions => "Suggestions",
            ResetOption::Summaries => "File Summaries",
            ResetOption::Glossary => "Domain Glossary",
            ResetOption::Memory => "Repo Memory",
            ResetOption::GroupingAi => "Grouping AI",
        }
    }

    /// Get description for the option
    pub fn description(&self) -> &'static str {
        match self {
            ResetOption::Index => "rebuild file tree",
            ResetOption::Suggestions => "regenerate with AI",
            ResetOption::Summaries => "regenerate with AI",
            ResetOption::Glossary => "extract terminology",
            ResetOption::Memory => "decisions/conventions",
            ResetOption::GroupingAi => "rebuild AI grouping",
        }
    }

    /// Get all options in display order
    pub fn all() -> Vec<ResetOption> {
        vec![
            ResetOption::Index,
            ResetOption::Suggestions,
            ResetOption::Summaries,
            ResetOption::Glossary,
            ResetOption::Memory,
            ResetOption::GroupingAi,
        ]
    }

    /// Get default options (safe to reset without losing user data)
    pub fn defaults() -> Vec<ResetOption> {
        vec![
            ResetOption::Index,
            ResetOption::Suggestions,
            ResetOption::Summaries,
            ResetOption::Glossary,
            ResetOption::GroupingAi,
        ]
    }
}

/// Cache validity durations
const LLM_SUMMARY_CACHE_DAYS: i64 = 30;

/// Cached index metadata
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexCache {
    pub root: PathBuf,
    pub file_count: usize,
    pub symbol_count: usize,
    pub cached_at: DateTime<Utc>,
    pub file_hashes: HashMap<PathBuf, String>,
}

impl IndexCache {
    /// Create from a CodebaseIndex
    pub fn from_index(index: &CodebaseIndex) -> Self {
        Self {
            root: index.root.clone(),
            file_count: index.files.len(),
            symbol_count: index.symbols.len(),
            cached_at: Utc::now(),
            file_hashes: compute_file_hashes(index),
        }
    }

}

/// Cached suggestions
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SuggestionsCache {
    pub suggestions: Vec<Suggestion>,
    pub cached_at: DateTime<Utc>,
    /// File paths that these suggestions apply to
    pub files: Vec<PathBuf>,
}

impl SuggestionsCache {
    /// Create from a list of suggestions
    pub fn from_suggestions(suggestions: &[Suggestion]) -> Self {
        let files: Vec<PathBuf> = suggestions.iter()
            .map(|s| s.file.clone())
            .collect::<std::collections::HashSet<_>>()
            .into_iter()
            .collect();

        Self {
            suggestions: suggestions.to_vec(),
            cached_at: Utc::now(),
            files,
        }
    }

}

// ═══════════════════════════════════════════════════════════════════════════
//  LLM SUMMARY CACHE - Persistent storage for AI-generated summaries
// ═══════════════════════════════════════════════════════════════════════════

const LLM_SUMMARIES_CACHE_FILE: &str = "llm_summaries.json";

/// A single LLM-generated summary entry with hash for change detection
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmSummaryEntry {
    /// The AI-generated summary text
    pub summary: String,
    /// Hash of file content (LOC + symbols) for change detection
    pub file_hash: String,
    /// When this summary was generated
    pub generated_at: DateTime<Utc>,
}

/// Cached LLM-generated file summaries
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmSummaryCache {
    /// Map of file paths to their summary entries
    pub summaries: HashMap<PathBuf, LlmSummaryEntry>,
    /// Cached project context (from README, etc.)
    pub project_context: Option<String>,
    /// When the cache was last updated
    pub cached_at: DateTime<Utc>,
}

/// Normalize summary cache keys to repo-relative paths.
pub fn normalize_summary_path(path: &Path, root: &Path) -> PathBuf {
    let raw = path.to_string_lossy();
    let mut cleaned = raw.replace('\\', "/");
    while cleaned.starts_with("./") {
        cleaned = cleaned.trim_start_matches("./").to_string();
    }

    let mut out = PathBuf::from(cleaned);
    if out.is_absolute() {
        if let Ok(stripped) = out.strip_prefix(root) {
            if !stripped.as_os_str().is_empty() {
                out = stripped.to_path_buf();
            }
        }
    }

    out
}

impl LlmSummaryCache {
    /// Create a new empty cache
    pub fn new() -> Self {
        Self {
            summaries: HashMap::new(),
            project_context: None,
            cached_at: Utc::now(),
        }
    }

    /// Check if a file's summary is still valid (not changed and not too old)
    pub fn is_file_valid(&self, path: &PathBuf, current_hash: &str) -> bool {
        if let Some(entry) = self.summaries.get(path) {
            // Check if hash matches (file unchanged)
            if entry.file_hash != current_hash {
                return false;
            }
            // Check if not too old
            let age = Utc::now() - entry.generated_at;
            age < Duration::days(LLM_SUMMARY_CACHE_DAYS)
        } else {
            false
        }
    }

    /// Get summary for a file if valid
    /// Update or insert a summary
    pub fn set_summary(&mut self, path: PathBuf, summary: String, file_hash: String) {
        self.summaries.insert(path, LlmSummaryEntry {
            summary,
            file_hash,
            generated_at: Utc::now(),
        });
        self.cached_at = Utc::now();
    }

    /// Set the project context
    pub fn set_project_context(&mut self, context: String) {
        self.project_context = Some(context);
    }

    /// Get all valid summaries as a HashMap for the UI
    pub fn get_all_valid_summaries(&self, file_hashes: &HashMap<PathBuf, String>) -> HashMap<PathBuf, String> {
        self.summaries.iter()
            .filter(|(path, _)| {
                if let Some(current_hash) = file_hashes.get(*path) {
                    self.is_file_valid(path, current_hash)
                } else {
                    false
                }
            })
            .map(|(path, entry)| (path.clone(), entry.summary.clone()))
            .collect()
    }

    /// Get files that need regeneration (changed or missing)
    pub fn get_files_needing_summary(&self, file_hashes: &HashMap<PathBuf, String>) -> Vec<PathBuf> {
        file_hashes.iter()
            .filter(|(path, hash)| !self.is_file_valid(path, hash))
            .map(|(path, _)| path.clone())
            .collect()
    }

    /// Normalize cached paths to match current index keys.
    pub fn normalize_paths(&mut self, root: &Path) -> bool {
        if self.summaries.is_empty() {
            return false;
        }

        let mut normalized: HashMap<PathBuf, LlmSummaryEntry> = HashMap::new();
        let mut changed = false;

        for (path, entry) in self.summaries.iter() {
            let key = normalize_summary_path(path, root);
            if &key != path {
                changed = true;
            }

            match normalized.get(&key) {
                Some(existing) if existing.generated_at >= entry.generated_at => {}
                _ => {
                    normalized.insert(key, entry.clone());
                }
            }
        }

        if changed {
            self.summaries = normalized;
            self.cached_at = Utc::now();
        }

        changed
    }
}

impl Default for LlmSummaryCache {
    fn default() -> Self {
        Self::new()
    }
}

// ═══════════════════════════════════════════════════════════════════════════
//  GROUPING AI CACHE - AI-assisted layer classification hints
// ═══════════════════════════════════════════════════════════════════════════

const GROUPING_AI_CACHE_DAYS: i64 = 30;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GroupingAiEntry {
    pub layer: crate::grouping::Layer,
    pub confidence: f64,
    pub file_hash: String,
    pub generated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GroupingAiCache {
    pub entries: HashMap<PathBuf, GroupingAiEntry>,
    pub cached_at: DateTime<Utc>,
}

impl GroupingAiCache {
    pub fn new() -> Self {
        Self {
            entries: HashMap::new(),
            cached_at: Utc::now(),
        }
    }

    pub fn is_file_valid(&self, path: &PathBuf, current_hash: &str) -> bool {
        if let Some(entry) = self.entries.get(path) {
            if entry.file_hash != current_hash {
                return false;
            }
            let age = Utc::now() - entry.generated_at;
            age < Duration::days(GROUPING_AI_CACHE_DAYS)
        } else {
            false
        }
    }

    pub fn set_entry(&mut self, path: PathBuf, entry: GroupingAiEntry) {
        self.entries.insert(path, entry);
        self.cached_at = Utc::now();
    }

    pub fn normalize_paths(&mut self, root: &Path) -> bool {
        if self.entries.is_empty() {
            return false;
        }

        let mut normalized: HashMap<PathBuf, GroupingAiEntry> = HashMap::new();
        let mut changed = false;

        for (path, entry) in self.entries.iter() {
            let key = normalize_summary_path(path, root);
            if &key != path {
                changed = true;
            }

            match normalized.get(&key) {
                Some(existing) if existing.generated_at >= entry.generated_at => {}
                _ => {
                    normalized.insert(key, entry.clone());
                }
            }
        }

        if changed {
            self.entries = normalized;
            self.cached_at = Utc::now();
        }

        changed
    }
}

impl Default for GroupingAiCache {
    fn default() -> Self {
        Self::new()
    }
}

/// Compute file hashes for change detection
pub fn compute_file_hashes(index: &CodebaseIndex) -> HashMap<PathBuf, String> {
    index.files.iter()
        .map(|(path, file_index)| {
            // Use a stable content hash when available; fall back for older data.
            let hash = if !file_index.content_hash.is_empty() {
                file_index.content_hash.clone()
            } else {
                format!("{}-{}", file_index.loc, file_index.symbols.len())
            };
            (path.clone(), hash)
        })
        .collect()
}

/// Local “repo memory” entries (decisions, conventions, reminders).
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct RepoMemory {
    pub entries: Vec<MemoryEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MemoryEntry {
    pub id: uuid::Uuid,
    pub text: String,
    pub created_at: DateTime<Utc>,
}

impl RepoMemory {
    pub fn add(&mut self, text: String) -> uuid::Uuid {
        let id = uuid::Uuid::new_v4();
        self.entries.push(MemoryEntry {
            id,
            text,
            created_at: Utc::now(),
        });
        id
    }

    /// Render a concise memory context for LLM prompts.
    pub fn to_prompt_context(&self, max_entries: usize, max_chars: usize) -> String {
        let mut entries = self.entries.clone();
        entries.sort_by(|a, b| b.created_at.cmp(&a.created_at)); // newest first

        let mut out = String::new();
        for e in entries.into_iter().take(max_entries) {
            let line = format!("- {}\n", e.text.trim());
            if out.len() + line.len() > max_chars {
                break;
            }
            out.push_str(&line);
        }
        out.trim().to_string()
    }
}

// ═══════════════════════════════════════════════════════════════════════════
//  DOMAIN GLOSSARY - Auto-extracted terminology from codebase
// ═══════════════════════════════════════════════════════════════════════════

/// A single domain term with its definition and source files
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GlossaryEntry {
    /// Plain-language definition of this term
    pub definition: String,
    /// Files where this term is used/defined
    pub files: Vec<PathBuf>,
}

/// Auto-generated domain glossary extracted during summarization
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DomainGlossary {
    /// Map of term name to its entry
    pub terms: HashMap<String, GlossaryEntry>,
    /// When this glossary was generated
    pub generated_at: DateTime<Utc>,
}

impl DomainGlossary {
    /// Create a new empty glossary
    pub fn new() -> Self {
        Self {
            terms: HashMap::new(),
            generated_at: Utc::now(),
        }
    }

    /// Add or update a term
    pub fn add_term(&mut self, name: String, definition: String, file: PathBuf) {
        if let Some(entry) = self.terms.get_mut(&name) {
            // Update existing: add file if not already present
            if !entry.files.contains(&file) {
                entry.files.push(file);
            }
        } else {
            // New term
            self.terms.insert(name, GlossaryEntry {
                definition,
                files: vec![file],
            });
        }
        self.generated_at = Utc::now();
    }

    /// Merge terms from another glossary (used when processing batches)
    pub fn merge(&mut self, other: &DomainGlossary) {
        for (name, entry) in &other.terms {
            if let Some(existing) = self.terms.get_mut(name) {
                // Merge files
                for file in &entry.files {
                    if !existing.files.contains(file) {
                        existing.files.push(file.clone());
                    }
                }
            } else {
                self.terms.insert(name.clone(), entry.clone());
            }
        }
        self.generated_at = Utc::now();
    }

    /// Format glossary for inclusion in LLM prompts
    pub fn to_prompt_context(&self, max_terms: usize) -> String {
        if self.terms.is_empty() {
            return String::new();
        }

        let mut lines = Vec::new();
        
        // Sort by number of files (most used terms first)
        let mut sorted: Vec<_> = self.terms.iter().collect();
        sorted.sort_by(|a, b| b.1.files.len().cmp(&a.1.files.len()));

        for (name, entry) in sorted.into_iter().take(max_terms) {
            lines.push(format!("- {}: {}", name, entry.definition));
        }

        if lines.is_empty() {
            String::new()
        } else {
            format!("DOMAIN TERMINOLOGY (use these terms, not generic descriptions):\n{}", lines.join("\n"))
        }
    }

    /// Check if glossary is empty
    pub fn is_empty(&self) -> bool {
        self.terms.is_empty()
    }

    /// Get term count
    pub fn len(&self) -> usize {
        self.terms.len()
    }
}

impl Default for DomainGlossary {
    fn default() -> Self {
        Self::new()
    }
}

/// The cache manager
pub struct Cache {
    cache_dir: PathBuf,
}

struct CacheLock {
    file: std::fs::File,
}

impl Drop for CacheLock {
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}

impl Cache {
    /// Create a new cache manager for a project
    pub fn new(project_root: &Path) -> Self {
        let cache_dir = project_root.join(CACHE_DIR);
        Self { cache_dir }
    }

    /// Ensure the cache directory exists
    fn ensure_dir(&self) -> anyhow::Result<()> {
        if !self.cache_dir.exists() {
            fs::create_dir_all(&self.cache_dir)?;
            
            // Add to .gitignore if it exists
            let gitignore = self.cache_dir.parent()
                .map(|p| p.join(".gitignore"))
                .filter(|p| p.exists());
            
            if let Some(gitignore_path) = gitignore {
                let content = fs::read_to_string(&gitignore_path)?;
                if !content.contains(".cosmos") {
                    let mut file = fs::OpenOptions::new()
                        .append(true)
                        .open(&gitignore_path)?;
                    use std::io::Write;
                    writeln!(file, "\n# Cosmos cache\n.cosmos/")?;
                }
            }
        }
        Ok(())
    }

    fn lock(&self, exclusive: bool) -> anyhow::Result<CacheLock> {
        if exclusive {
            self.ensure_dir()?;
        } else if !self.cache_dir.exists() {
            return Err(anyhow::anyhow!("Cache directory missing"));
        }

        let lock_path = self.cache_dir.join(".lock");
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .open(&lock_path)?;

        if exclusive {
            file.lock_exclusive()?;
        } else {
            file.lock_shared()?;
        }

        Ok(CacheLock { file })
    }

    /// Save index cache
    pub fn save_index_cache(&self, cache: &IndexCache) -> anyhow::Result<()> {
        let _lock = self.lock(true)?;
        let path = self.cache_dir.join(INDEX_CACHE_FILE);
        let content = serde_json::to_string_pretty(cache)?;
        write_atomic(&path, &content)?;
        Ok(())
    }

    /// Save suggestions cache
    pub fn save_suggestions_cache(&self, cache: &SuggestionsCache) -> anyhow::Result<()> {
        let _lock = self.lock(true)?;
        let path = self.cache_dir.join(SUGGESTIONS_CACHE_FILE);
        let content = serde_json::to_string_pretty(cache)?;
        write_atomic(&path, &content)?;
        Ok(())
    }

    /// Load LLM-generated summaries cache
    pub fn load_llm_summaries_cache(&self) -> Option<LlmSummaryCache> {
        let path = self.cache_dir.join(LLM_SUMMARIES_CACHE_FILE);
        if !path.exists() {
            return None;
        }

        let _lock = self.lock(false).ok()?;
        let content = fs::read_to_string(&path).ok()?;
        serde_json::from_str(&content).ok()
    }

    /// Save LLM-generated summaries cache
    pub fn save_llm_summaries_cache(&self, cache: &LlmSummaryCache) -> anyhow::Result<()> {
        let _lock = self.lock(true)?;
        let path = self.cache_dir.join(LLM_SUMMARIES_CACHE_FILE);
        let content = serde_json::to_string_pretty(cache)?;
        write_atomic(&path, &content)?;
        Ok(())
    }

    /// Load grouping AI cache
    pub fn load_grouping_ai_cache(&self) -> Option<GroupingAiCache> {
        let path = self.cache_dir.join(GROUPING_AI_CACHE_FILE);
        if !path.exists() {
            return None;
        }
        let _lock = self.lock(false).ok()?;
        let content = fs::read_to_string(&path).ok()?;
        serde_json::from_str(&content).ok()
    }

    /// Save grouping AI cache
    pub fn save_grouping_ai_cache(&self, cache: &GroupingAiCache) -> anyhow::Result<()> {
        let _lock = self.lock(true)?;
        let path = self.cache_dir.join(GROUPING_AI_CACHE_FILE);
        let content = serde_json::to_string_pretty(cache)?;
        write_atomic(&path, &content)?;
        Ok(())
    }

    /// Load repo memory (decisions/conventions) from `.cosmos/memory.json`
    pub fn load_repo_memory(&self) -> RepoMemory {
        let path = self.cache_dir.join(MEMORY_FILE);
        if !path.exists() {
            return RepoMemory::default();
        }
        let _lock = match self.lock(false) {
            Ok(lock) => lock,
            Err(_) => return RepoMemory::default(),
        };
        fs::read_to_string(&path)
            .ok()
            .and_then(|content| serde_json::from_str(&content).ok())
            .unwrap_or_default()
    }

    /// Save repo memory to `.cosmos/memory.json`
    pub fn save_repo_memory(&self, memory: &RepoMemory) -> anyhow::Result<()> {
        let _lock = self.lock(true)?;
        let path = self.cache_dir.join(MEMORY_FILE);
        let content = serde_json::to_string_pretty(memory)?;
        write_atomic(&path, &content)?;
        Ok(())
    }

    /// Load domain glossary from `.cosmos/glossary.json`
    pub fn load_glossary(&self) -> Option<DomainGlossary> {
        let path = self.cache_dir.join(GLOSSARY_FILE);
        if !path.exists() {
            return None;
        }
        let _lock = self.lock(false).ok()?;
        fs::read_to_string(&path)
            .ok()
            .and_then(|content| serde_json::from_str(&content).ok())
    }

    /// Save domain glossary to `.cosmos/glossary.json`
    pub fn save_glossary(&self, glossary: &DomainGlossary) -> anyhow::Result<()> {
        let _lock = self.lock(true)?;
        let path = self.cache_dir.join(GLOSSARY_FILE);
        let content = serde_json::to_string_pretty(glossary)?;
        write_atomic(&path, &content)?;
        Ok(())
    }

    /// Clear selected cache files only
    pub fn clear_selective(&self, options: &[ResetOption]) -> anyhow::Result<Vec<String>> {
        let _lock = self.lock(true)?;
        let mut cleared = Vec::new();

        for option in options {
            let files_to_remove: Vec<&str> = match option {
                ResetOption::Index => vec![INDEX_CACHE_FILE],
                ResetOption::Suggestions => vec![SUGGESTIONS_CACHE_FILE],
                ResetOption::Summaries => vec![LLM_SUMMARIES_CACHE_FILE],
                ResetOption::Glossary => vec![GLOSSARY_FILE],
                ResetOption::Memory => vec![MEMORY_FILE],
                ResetOption::GroupingAi => vec![GROUPING_AI_CACHE_FILE],
            };

            for file in files_to_remove {
                let path = self.cache_dir.join(file);
                if path.exists() {
                    fs::remove_file(&path)?;
                    cleared.push(file.to_string());
                }
            }
        }

        Ok(cleared)
    }

}

fn write_atomic(path: &Path, content: &str) -> anyhow::Result<()> {
    let tmp_path = path.with_extension("tmp");
    fs::write(&tmp_path, content)?;
    if let Err(err) = fs::rename(&tmp_path, path) {
        let _ = fs::remove_file(&tmp_path);
        return Err(err.into());
    }
    Ok(())
}


