//! Cache module for Cosmos
//!
//! Persists suggestions and index data to .cosmos/ directory
//! to avoid redundant LLM calls and speed up startup.

#![allow(dead_code)]

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

/// Cache validity duration (24 hours for index, 7 days for suggestions)
const INDEX_CACHE_HOURS: i64 = 24;
const SUGGESTIONS_CACHE_DAYS: i64 = 7;

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
        let file_hashes = index.files.iter()
            .map(|(path, file_index)| {
                let hash = format!(
                    "{}-{}-{}",
                    file_index.loc,
                    file_index.symbols.len(),
                    file_index.last_modified.timestamp()
                );
                (path.clone(), hash)
            })
            .collect();

        Self {
            root: index.root.clone(),
            file_count: index.files.len(),
            symbol_count: index.symbols.len(),
            cached_at: Utc::now(),
            file_hashes,
        }
    }

    /// Check if the cache is still valid
    pub fn is_valid(&self) -> bool {
        let age = Utc::now() - self.cached_at;
        age < Duration::hours(INDEX_CACHE_HOURS)
    }

    /// Check if a file has changed since caching
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
    pub fn is_valid(&self) -> bool {
        let age = Utc::now() - self.cached_at;
        age < Duration::days(SUGGESTIONS_CACHE_DAYS)
    }

    /// Filter out suggestions for files that have changed
    pub fn filter_unchanged(&self, changed_files: &[PathBuf]) -> Vec<Suggestion> {
        self.suggestions.iter()
            .filter(|s| !changed_files.contains(&s.file))
            .cloned()
            .collect()
    }
}

/// Cached file summaries (static analysis)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SummariesCache {
    pub summaries: HashMap<PathBuf, crate::index::FileSummary>,
    pub cached_at: DateTime<Utc>,
}

impl SummariesCache {
    /// Create from file index
    pub fn from_index(index: &CodebaseIndex) -> Self {
        let summaries = index.files.iter()
            .map(|(path, file_index)| (path.clone(), file_index.summary.clone()))
            .collect();
        
        Self {
            summaries,
            cached_at: Utc::now(),
        }
    }
    
    /// Check if the cache is still valid
    pub fn is_valid(&self) -> bool {
        let age = Utc::now() - self.cached_at;
        age < Duration::days(SUGGESTIONS_CACHE_DAYS)
    }
    
    /// Get summary for a file
    pub fn get(&self, path: &PathBuf) -> Option<&crate::index::FileSummary> {
        self.summaries.get(path)
    }
}

// ═══════════════════════════════════════════════════════════════════════════
//  LLM SUMMARY CACHE - Persistent storage for AI-generated summaries
// ═══════════════════════════════════════════════════════════════════════════

const LLM_SUMMARIES_CACHE_FILE: &str = "llm_summaries.json";
const LLM_SUMMARY_CACHE_DAYS: i64 = 30;

/// A single LLM-generated summary entry with hash for change detection
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmSummaryEntry {
    /// The AI-generated summary text
    pub summary: String,
    /// Hash of file content (LOC + symbols + mtime) for change detection
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

    /// Load cached suggestions
    pub fn load_suggestions_cache(&self) -> Option<SuggestionsCache> {
        let path = self.cache_dir.join(SUGGESTIONS_CACHE_FILE);
        if !path.exists() {
            return None;
        }

        let content = fs::read_to_string(&path).ok()?;
        let cache: SuggestionsCache = serde_json::from_str(&content).ok()?;
        
        if cache.is_valid() {
            Some(cache)
        } else {
            None
        }
    }

    /// Save suggestions cache
    pub fn save_suggestions_cache(&self, cache: &SuggestionsCache) -> anyhow::Result<()> {
        self.ensure_dir()?;
        let path = self.cache_dir.join(SUGGESTIONS_CACHE_FILE);
        let content = serde_json::to_string_pretty(cache)?;
        fs::write(path, content)?;
        Ok(())
    }
    
    /// Load cached file summaries
    pub fn load_summaries_cache(&self) -> Option<SummariesCache> {
        let path = self.cache_dir.join(SUMMARIES_CACHE_FILE);
        if !path.exists() {
            return None;
        }

        let content = fs::read_to_string(&path).ok()?;
        let cache: SummariesCache = serde_json::from_str(&content).ok()?;
        
        if cache.is_valid() {
            Some(cache)
        } else {
            None
        }
    }

    /// Save file summaries cache
    pub fn save_summaries_cache(&self, cache: &SummariesCache) -> anyhow::Result<()> {
        self.ensure_dir()?;
        let path = self.cache_dir.join(SUMMARIES_CACHE_FILE);
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
    pub fn save_settings(&self, settings: &Settings) -> anyhow::Result<()> {
        self.ensure_dir()?;
        let path = self.cache_dir.join(SETTINGS_FILE);
        let content = serde_json::to_string_pretty(settings)?;
        fs::write(path, content)?;
        Ok(())
    }

    /// Add a dismissed suggestion ID
    pub fn dismiss_suggestion(&self, id: uuid::Uuid) -> anyhow::Result<()> {
        let mut settings = self.load_settings();
        if !settings.dismissed.contains(&id) {
            settings.dismissed.push(id);
            self.save_settings(&settings)?;
        }
        Ok(())
    }

    /// Add an applied suggestion ID
    pub fn mark_applied(&self, id: uuid::Uuid) -> anyhow::Result<()> {
        let mut settings = self.load_settings();
        if !settings.applied.contains(&id) {
            settings.applied.push(id);
            self.save_settings(&settings)?;
        }
        Ok(())
    }

    /// Clear all caches
    pub fn clear(&self) -> anyhow::Result<()> {
        if self.cache_dir.exists() {
            fs::remove_dir_all(&self.cache_dir)?;
        }
        Ok(())
    }

    /// Get cache stats
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

#[derive(Debug, Clone, Default)]
pub struct CacheStats {
    pub exists: bool,
    pub file_count: usize,
    pub total_size: u64,
    pub has_index: bool,
    pub has_suggestions: bool,
}

impl CacheStats {
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
    fn test_dismiss_suggestion() {
        let dir = tempdir().unwrap();
        let cache = Cache::new(dir.path());
        
        let id = uuid::Uuid::new_v4();
        cache.dismiss_suggestion(id).unwrap();
        
        let settings = cache.load_settings();
        assert!(settings.dismissed.contains(&id));
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
