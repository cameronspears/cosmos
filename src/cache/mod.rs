//! Cache module for Cosmos
//!
//! Persists suggestions and index data to .cosmos/ directory
//! to avoid redundant LLM calls and speed up startup.

use crate::index::CodebaseIndex;
use crate::suggest::Suggestion;
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

const CACHE_DIR: &str = ".cosmos";
const INDEX_CACHE_FILE: &str = "index.json";
const SUGGESTIONS_CACHE_FILE: &str = "suggestions.json";
const SUMMARIES_CACHE_FILE: &str = "summaries.json";
const SETTINGS_FILE: &str = "settings.json";
const MEMORY_FILE: &str = "memory.json";
const GLOSSARY_FILE: &str = "glossary.json";
const HISTORY_DB_FILE: &str = "history.db";

/// Options for selective cache reset
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResetOption {
    /// Clear index.json - codebase structure, symbols, patterns
    Index,
    /// Clear suggestions.json - generated suggestions
    Suggestions,
    /// Clear summaries.json + llm_summaries.json - file summaries
    Summaries,
    /// Clear glossary.json - domain terminology
    Glossary,
    /// Clear history.db - suggestion history/analytics
    History,
    /// Clear settings.json - dismissed/applied suggestions
    Settings,
    /// Clear memory.json - repo decisions/conventions
    Memory,
}

impl ResetOption {
    /// Get human-readable label for the option
    pub fn label(&self) -> &'static str {
        match self {
            ResetOption::Index => "Index & Symbols",
            ResetOption::Suggestions => "Suggestions",
            ResetOption::Summaries => "File Summaries",
            ResetOption::Glossary => "Domain Glossary",
            ResetOption::History => "Suggestion History",
            ResetOption::Settings => "User Settings",
            ResetOption::Memory => "Repo Memory",
        }
    }

    /// Get description for the option
    pub fn description(&self) -> &'static str {
        match self {
            ResetOption::Index => "rebuild file tree",
            ResetOption::Suggestions => "regenerate with AI",
            ResetOption::Summaries => "regenerate with AI",
            ResetOption::Glossary => "extract terminology",
            ResetOption::History => "clear analytics",
            ResetOption::Settings => "dismissed/applied",
            ResetOption::Memory => "decisions/conventions",
        }
    }

    /// Get all options in display order
    pub fn all() -> Vec<ResetOption> {
        vec![
            ResetOption::Index,
            ResetOption::Suggestions,
            ResetOption::Summaries,
            ResetOption::Glossary,
            ResetOption::History,
            ResetOption::Settings,
            ResetOption::Memory,
        ]
    }

    /// Get default options (safe to reset without losing user data)
    pub fn defaults() -> Vec<ResetOption> {
        vec![
            ResetOption::Index,
            ResetOption::Suggestions,
            ResetOption::Summaries,
            ResetOption::Glossary,
        ]
    }
}

/// Cache validity durations
const INDEX_CACHE_HOURS: i64 = 24;
const SUGGESTIONS_CACHE_DAYS: i64 = 7;
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
    #[allow(dead_code)]
    pub fn from_index(index: &CodebaseIndex) -> Self {
        Self {
            root: index.root.clone(),
            file_count: index.files.len(),
            symbol_count: index.symbols.len(),
            cached_at: Utc::now(),
            file_hashes: compute_file_hashes(index),
        }
    }

    /// Check if the cache is still valid
    #[allow(dead_code)]
    pub fn is_valid(&self) -> bool {
        let age = Utc::now() - self.cached_at;
        age < Duration::hours(INDEX_CACHE_HOURS)
    }

    /// Check if a file has changed since caching
    #[allow(dead_code)]
    pub fn file_changed(&self, path: &PathBuf, new_hash: &str) -> bool {
        self.file_hashes.get(path).map(|h| h != new_hash).unwrap_or(true)
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
    #[allow(dead_code)]
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

    /// Check if the cache is still valid
    #[allow(dead_code)]
    pub fn is_valid(&self) -> bool {
        let age = Utc::now() - self.cached_at;
        age < Duration::days(SUGGESTIONS_CACHE_DAYS)
    }

    /// Filter out suggestions for files that have changed
    #[allow(dead_code)]
    pub fn filter_unchanged(&self, changed_files: &[PathBuf]) -> Vec<Suggestion> {
        self.suggestions.iter()
            .filter(|s| !changed_files.contains(&s.file))
            .cloned()
            .collect()
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
    #[allow(dead_code)]
    pub fn get_valid_summary(&self, path: &PathBuf, current_hash: &str) -> Option<&str> {
        if self.is_file_valid(path, current_hash) {
            self.summaries.get(path).map(|e| e.summary.as_str())
        } else {
            None
        }
    }

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

    /// Count stats
    #[allow(dead_code)]
    pub fn stats(&self) -> (usize, usize) {
        let total = self.summaries.len();
        let valid = self.summaries.iter()
            .filter(|(_, entry)| {
                let age = Utc::now() - entry.generated_at;
                age < Duration::days(LLM_SUMMARY_CACHE_DAYS)
            })
            .count();
        (total, valid)
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

/// Compute file hashes for change detection
pub fn compute_file_hashes(index: &CodebaseIndex) -> HashMap<PathBuf, String> {
    index.files.iter()
        .map(|(path, file_index)| {
            // Use only content-based metrics, not mtime (which changes on git checkout, copy, etc.)
            let hash = format!(
                "{}-{}",
                file_index.loc,
                file_index.symbols.len()
            );
            (path.clone(), hash)
        })
        .collect()
}

/// User settings
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Settings {
    /// Dismissed suggestion IDs
    pub dismissed: Vec<uuid::Uuid>,
    /// Applied suggestion IDs
    pub applied: Vec<uuid::Uuid>,
    /// Custom ignore patterns
    pub ignore_patterns: Vec<String>,
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

    /// Load cached index metadata
    #[allow(dead_code)]
    pub fn load_index_cache(&self) -> Option<IndexCache> {
        let path = self.cache_dir.join(INDEX_CACHE_FILE);
        if !path.exists() {
            return None;
        }

        let content = fs::read_to_string(&path).ok()?;
        let cache: IndexCache = serde_json::from_str(&content).ok()?;
        
        if cache.is_valid() {
            Some(cache)
        } else {
            None
        }
    }

    /// Save index cache
    pub fn save_index_cache(&self, cache: &IndexCache) -> anyhow::Result<()> {
        self.ensure_dir()?;
        let path = self.cache_dir.join(INDEX_CACHE_FILE);
        let content = serde_json::to_string_pretty(cache)?;
        fs::write(path, content)?;
        Ok(())
    }

    /// Save suggestions cache
    pub fn save_suggestions_cache(&self, cache: &SuggestionsCache) -> anyhow::Result<()> {
        self.ensure_dir()?;
        let path = self.cache_dir.join(SUGGESTIONS_CACHE_FILE);
        let content = serde_json::to_string_pretty(cache)?;
        fs::write(path, content)?;
        Ok(())
    }

    /// Load LLM-generated summaries cache
    pub fn load_llm_summaries_cache(&self) -> Option<LlmSummaryCache> {
        let path = self.cache_dir.join(LLM_SUMMARIES_CACHE_FILE);
        if !path.exists() {
            return None;
        }

        let content = fs::read_to_string(&path).ok()?;
        serde_json::from_str(&content).ok()
    }

    /// Save LLM-generated summaries cache
    pub fn save_llm_summaries_cache(&self, cache: &LlmSummaryCache) -> anyhow::Result<()> {
        self.ensure_dir()?;
        let path = self.cache_dir.join(LLM_SUMMARIES_CACHE_FILE);
        let content = serde_json::to_string_pretty(cache)?;
        fs::write(path, content)?;
        Ok(())
    }

    /// Load settings
    #[allow(dead_code)]
    pub fn load_settings(&self) -> Settings {
        let path = self.cache_dir.join(SETTINGS_FILE);
        if !path.exists() {
            return Settings::default();
        }

        fs::read_to_string(&path)
            .ok()
            .and_then(|content| serde_json::from_str(&content).ok())
            .unwrap_or_default()
    }

    /// Save settings
    #[allow(dead_code)]
    pub fn save_settings(&self, settings: &Settings) -> anyhow::Result<()> {
        self.ensure_dir()?;
        let path = self.cache_dir.join(SETTINGS_FILE);
        let content = serde_json::to_string_pretty(settings)?;
        fs::write(path, content)?;
        Ok(())
    }

    /// Load repo memory (decisions/conventions) from `.cosmos/memory.json`
    pub fn load_repo_memory(&self) -> RepoMemory {
        let path = self.cache_dir.join(MEMORY_FILE);
        if !path.exists() {
            return RepoMemory::default();
        }
        fs::read_to_string(&path)
            .ok()
            .and_then(|content| serde_json::from_str(&content).ok())
            .unwrap_or_default()
    }

    /// Save repo memory to `.cosmos/memory.json`
    pub fn save_repo_memory(&self, memory: &RepoMemory) -> anyhow::Result<()> {
        self.ensure_dir()?;
        let path = self.cache_dir.join(MEMORY_FILE);
        let content = serde_json::to_string_pretty(memory)?;
        fs::write(path, content)?;
        Ok(())
    }

    /// Load domain glossary from `.cosmos/glossary.json`
    pub fn load_glossary(&self) -> Option<DomainGlossary> {
        let path = self.cache_dir.join(GLOSSARY_FILE);
        if !path.exists() {
            return None;
        }
        fs::read_to_string(&path)
            .ok()
            .and_then(|content| serde_json::from_str(&content).ok())
    }

    /// Save domain glossary to `.cosmos/glossary.json`
    pub fn save_glossary(&self, glossary: &DomainGlossary) -> anyhow::Result<()> {
        self.ensure_dir()?;
        let path = self.cache_dir.join(GLOSSARY_FILE);
        let content = serde_json::to_string_pretty(glossary)?;
        fs::write(path, content)?;
        Ok(())
    }

    /// Clear all caches
    #[allow(dead_code)]
    pub fn clear(&self) -> anyhow::Result<()> {
        if self.cache_dir.exists() {
            fs::remove_dir_all(&self.cache_dir)?;
        }
        Ok(())
    }

    /// Clear selected cache files only
    pub fn clear_selective(&self, options: &[ResetOption]) -> anyhow::Result<Vec<String>> {
        let mut cleared = Vec::new();

        for option in options {
            let files_to_remove: Vec<&str> = match option {
                ResetOption::Index => vec![INDEX_CACHE_FILE],
                ResetOption::Suggestions => vec![SUGGESTIONS_CACHE_FILE],
                ResetOption::Summaries => vec![SUMMARIES_CACHE_FILE, LLM_SUMMARIES_CACHE_FILE],
                ResetOption::Glossary => vec![GLOSSARY_FILE],
                ResetOption::History => vec![HISTORY_DB_FILE],
                ResetOption::Settings => vec![SETTINGS_FILE],
                ResetOption::Memory => vec![MEMORY_FILE],
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

    /// Get cache stats
    #[allow(dead_code)]
    pub fn stats(&self) -> CacheStats {
        let mut stats = CacheStats::default();

        if self.cache_dir.exists() {
            stats.exists = true;

            if let Ok(entries) = fs::read_dir(&self.cache_dir) {
                for entry in entries.flatten() {
                    if let Ok(metadata) = entry.metadata() {
                        stats.total_size += metadata.len();
                        stats.file_count += 1;
                    }
                }
            }

            stats.has_index = self.cache_dir.join(INDEX_CACHE_FILE).exists();
            stats.has_suggestions = self.cache_dir.join(SUGGESTIONS_CACHE_FILE).exists();
        }

        stats
    }
}

#[allow(dead_code)]
#[derive(Debug, Clone, Default)]
pub struct CacheStats {
    pub exists: bool,
    pub file_count: usize,
    pub total_size: u64,
    pub has_index: bool,
    pub has_suggestions: bool,
}

impl CacheStats {
    #[allow(dead_code)]
    pub fn size_human(&self) -> String {
        if self.total_size < 1024 {
            format!("{} B", self.total_size)
        } else if self.total_size < 1024 * 1024 {
            format!("{:.1} KB", self.total_size as f64 / 1024.0)
        } else {
            format!("{:.1} MB", self.total_size as f64 / (1024.0 * 1024.0))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn test_cache_creation() {
        let dir = tempdir().unwrap();
        let cache = Cache::new(dir.path());
        
        assert!(cache.load_settings().dismissed.is_empty());
    }

    #[test]
    fn test_cache_validity() {
        let cache = IndexCache {
            root: PathBuf::from("/test"),
            file_count: 10,
            symbol_count: 100,
            cached_at: Utc::now(),
            file_hashes: HashMap::new(),
        };
        
        assert!(cache.is_valid());
    }
}
