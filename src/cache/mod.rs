//! Cache module for Cosmos
//!
//! Persists suggestions and index data to .cosmos/ directory
//! to avoid redundant LLM calls and speed up startup.
//!
//! # Error Handling
//!
//! Cache operations are designed to be best-effort. Callers typically use
//! `let _ = cache.save_*()` because:
//! - Cache failure is recoverable (data will be regenerated next time)
//! - We don't want to interrupt user workflows for cache issues
//! - The .cosmos/ directory might not exist or have permission issues
//!
//! For critical data, callers should explicitly handle errors.

use crate::index::CodebaseIndex;
use chrono::{DateTime, Duration, Utc};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::time::{Duration as StdDuration, Instant};

const CACHE_DIR: &str = ".cosmos";
const INDEX_CACHE_FILE: &str = "index.json";
const INDEX_META_FILE: &str = "index.meta.json";
const SUGGESTIONS_CACHE_FILE: &str = "suggestions.json";
const MEMORY_FILE: &str = "memory.json";
const GLOSSARY_FILE: &str = "glossary.json";
const GROUPING_AI_CACHE_FILE: &str = "grouping_ai.json";
const PIPELINE_METRICS_FILE: &str = "pipeline_metrics.jsonl";
const SUGGESTION_QUALITY_FILE: &str = "suggestion_quality.jsonl";
const SELF_ITERATION_RUNS_FILE: &str = "self_iteration_runs.jsonl";
const CACHE_LOCK_TIMEOUT_SECS: u64 = 5;
const CACHE_LOCK_RETRY_MS: u64 = 50;

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
    /// Clear question_cache.json - persisted question/answer history
    QuestionCache,
    /// Clear pipeline_metrics.jsonl - latency/cost telemetry rows
    PipelineMetrics,
    /// Clear suggestion_quality.jsonl - per-suggestion validation telemetry
    SuggestionQuality,
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
            ResetOption::QuestionCache => "Question Cache",
            ResetOption::PipelineMetrics => "Pipeline Metrics",
            ResetOption::SuggestionQuality => "Suggestion Quality",
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
            ResetOption::QuestionCache => "clear saved Q&A",
            ResetOption::PipelineMetrics => "clear latency/cost logs",
            ResetOption::SuggestionQuality => "clear validation telemetry",
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
            ResetOption::QuestionCache,
            ResetOption::PipelineMetrics,
            ResetOption::SuggestionQuality,
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

/// Flag file indicating user has seen the welcome overlay
const WELCOME_SEEN_FILE: &str = "welcome_seen";

/// Question answer cache file
const QUESTION_CACHE_FILE: &str = "question_cache.json";

/// Max age for question cache entries (in hours)
const QUESTION_CACHE_HOURS: i64 = 24;

/// Cached index metadata
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexCache {
    pub root: PathBuf,
    pub file_count: usize,
    pub symbol_count: usize,
    pub cached_at: DateTime<Utc>,
    pub file_hashes: HashMap<PathBuf, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct IndexMeta {
    root: PathBuf,
    git_head: Option<String>,
    file_count: usize,
    symbol_count: usize,
    cached_at: DateTime<Utc>,
}

// Note: Suggestions are generated fresh each session (not cached across restarts)
// to ensure users always see new insights from the AI exploration.

// ═══════════════════════════════════════════════════════════════════════════
//  QUESTION ANSWER CACHE - Persistent storage for AI-generated answers
// ═══════════════════════════════════════════════════════════════════════════

/// A cached question answer entry
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QuestionCacheEntry {
    /// The question that was asked
    pub question: String,
    /// The AI-generated answer
    pub answer: String,
    /// When this answer was generated
    pub generated_at: DateTime<Utc>,
    /// Hash of relevant context (for invalidation)
    pub context_hash: String,
}

/// Cached question answers
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct QuestionCache {
    /// Map of question hash to answer entry
    pub entries: HashMap<String, QuestionCacheEntry>,
}

impl QuestionCache {
    /// Get a cached answer if still valid
    pub fn get(&self, question: &str, current_context_hash: &str) -> Option<&str> {
        let key = hash_question(question);
        self.entries.get(&key).and_then(|entry| {
            // Check time-based expiration
            let age = Utc::now().signed_duration_since(entry.generated_at);
            if age.num_hours() > QUESTION_CACHE_HOURS {
                return None;
            }
            // Check context hash
            if entry.context_hash != current_context_hash {
                return None;
            }
            Some(entry.answer.as_str())
        })
    }

    /// Store an answer in the cache
    pub fn set(&mut self, question: String, answer: String, context_hash: String) {
        let key = hash_question(&question);
        self.entries.insert(
            key,
            QuestionCacheEntry {
                question,
                answer,
                generated_at: Utc::now(),
                context_hash,
            },
        );
    }

    /// Clean up expired entries
    pub fn cleanup(&mut self) {
        let now = Utc::now();
        self.entries.retain(|_, entry| {
            let age = now.signed_duration_since(entry.generated_at);
            age.num_hours() <= QUESTION_CACHE_HOURS
        });
    }
}

/// Hash a question for cache key
fn hash_question(question: &str) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut hasher = DefaultHasher::new();
    question.to_lowercase().trim().hash(&mut hasher);
    format!("{:016x}", hasher.finish())
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
        self.summaries.insert(
            path,
            LlmSummaryEntry {
                summary,
                file_hash,
                generated_at: Utc::now(),
            },
        );
        self.cached_at = Utc::now();
    }

    /// Set the project context
    pub fn set_project_context(&mut self, context: String) {
        self.project_context = Some(context);
    }

    /// Get all valid summaries as a HashMap for the UI
    pub fn get_all_valid_summaries(
        &self,
        file_hashes: &HashMap<PathBuf, String>,
    ) -> HashMap<PathBuf, String> {
        self.summaries
            .iter()
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
    pub fn get_files_needing_summary(
        &self,
        file_hashes: &HashMap<PathBuf, String>,
    ) -> Vec<PathBuf> {
        file_hashes
            .iter()
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

/// Lightweight pipeline metric row written as JSONL to `.cosmos/pipeline_metrics.jsonl`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PipelineMetricRecord {
    pub timestamp: DateTime<Utc>,
    pub stage: String,
    pub summary_ms: Option<u64>,
    pub suggest_ms: Option<u64>,
    pub verify_ms: Option<u64>,
    pub apply_ms: Option<u64>,
    pub review_ms: Option<u64>,
    pub tokens: u32,
    pub cost: f64,
    pub gate: String,
    pub passed: bool,
}

/// Per-suggestion telemetry row written as JSONL to `.cosmos/suggestion_quality.jsonl`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SuggestionQualityRecord {
    pub timestamp: DateTime<Utc>,
    /// Correlates all suggestions produced in one generation/refinement cycle.
    pub run_id: String,
    pub suggestion_id: String,
    pub evidence_ids: Vec<usize>,
    /// One of: pending, validated, rejected.
    pub validation_outcome: String,
    /// Optional detail from validator (if available).
    pub validation_reason: Option<String>,
    /// One of: verified, contradicted, insufficient_evidence (set later by user verify).
    pub user_verify_outcome: Option<String>,
}

/// Command result row for self-iteration validation runs.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SelfIterationCommandOutcome {
    pub name: String,
    pub command: String,
    pub cwd: PathBuf,
    pub duration_ms: u64,
    pub success: bool,
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    pub stdout_tail: String,
    pub stderr_tail: String,
    #[serde(default)]
    pub note: Option<String>,
}

/// Aggregated suggestion quality metrics from sandbox self-iteration runs.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SelfIterationSuggestionMetrics {
    #[serde(default)]
    pub trials: usize,
    #[serde(default)]
    pub provisional_count: usize,
    #[serde(default)]
    pub final_count: usize,
    #[serde(default)]
    pub validated_count: usize,
    #[serde(default)]
    pub pending_count: usize,
    #[serde(default)]
    pub rejected_count: usize,
    #[serde(default)]
    pub displayed_valid_ratio: f64,
    #[serde(default)]
    pub validated_ratio: f64,
    #[serde(default)]
    pub rejected_ratio: f64,
    #[serde(default)]
    pub preview_sampled: usize,
    #[serde(default)]
    pub preview_verified_count: usize,
    #[serde(default)]
    pub preview_contradicted_count: usize,
    #[serde(default)]
    pub preview_insufficient_count: usize,
    #[serde(default)]
    pub preview_error_count: usize,
    #[serde(default)]
    pub preview_precision: Option<f64>,
    #[serde(default)]
    pub evidence_line1_ratio: f64,
    #[serde(default)]
    pub evidence_source_mix: HashMap<String, usize>,
    #[serde(default)]
    pub suggest_total_tokens: u32,
    #[serde(default)]
    pub suggest_total_cost_usd: f64,
    #[serde(default)]
    pub suggest_total_ms: u64,
}

/// High-level record for one `cosmos-lab` run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SelfIterationRunRecord {
    pub timestamp: DateTime<Utc>,
    pub run_id: String,
    pub mode: String,
    pub cosmos_repo: PathBuf,
    pub target_repo: PathBuf,
    pub passed: bool,
    #[serde(default)]
    pub command_outcomes: Vec<SelfIterationCommandOutcome>,
    #[serde(default)]
    pub reliability_metrics: Option<SelfIterationSuggestionMetrics>,
    #[serde(default)]
    pub report_path: Option<PathBuf>,
    #[serde(default)]
    pub notes: Vec<String>,
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
    index
        .files
        .iter()
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
            self.terms.insert(
                name,
                GlossaryEntry {
                    definition,
                    files: vec![file],
                },
            );
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
        let _ = FileExt::unlock(&self.file);
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
        }
        self.ensure_cosmos_ignored()?;
        Ok(())
    }

    fn ensure_cosmos_ignored(&self) -> anyhow::Result<()> {
        let Some(repo_root) = self.cache_dir.parent() else {
            return Ok(());
        };

        let gitignore_path = repo_root.join(".gitignore");
        if gitignore_path.exists() {
            append_ignore_entry(&gitignore_path, ".cosmos/")?;
            return Ok(());
        }

        let git_dir = repo_root.join(".git");
        if git_dir.is_dir() {
            let info_exclude_path = git_dir.join("info").join("exclude");
            if let Some(parent) = info_exclude_path.parent() {
                if fs::create_dir_all(parent).is_ok() {
                    if append_ignore_entry(&info_exclude_path, ".cosmos/").is_ok() {
                        return Ok(());
                    }
                }
            }
        }

        append_ignore_entry(&gitignore_path, ".cosmos/")?;
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
            .truncate(false) // Lock file content doesn't matter, just the lock
            .open(&lock_path)?;

        let start = Instant::now();
        loop {
            let result = if exclusive {
                FileExt::try_lock_exclusive(&file)
            } else {
                FileExt::try_lock_shared(&file)
            };
            match result {
                Ok(()) => break,
                Err(err) => {
                    if err.kind() != ErrorKind::WouldBlock {
                        return Err(err.into());
                    }
                    if start.elapsed() >= StdDuration::from_secs(CACHE_LOCK_TIMEOUT_SECS) {
                        return Err(anyhow::anyhow!(
                            "Timed out waiting for cache lock ({}s)",
                            CACHE_LOCK_TIMEOUT_SECS
                        ));
                    }
                    std::thread::sleep(StdDuration::from_millis(CACHE_LOCK_RETRY_MS));
                }
            }
        }

        Ok(CacheLock { file })
    }

    /// Save full index cache (CodebaseIndex)
    pub fn save_index_cache(&self, index: &CodebaseIndex) -> anyhow::Result<()> {
        let _lock = self.lock(true)?;
        let index_path = self.cache_dir.join(INDEX_CACHE_FILE);
        let meta_path = self.cache_dir.join(INDEX_META_FILE);

        let index_content = serde_json::to_string(index)?;
        write_atomic(&index_path, &index_content)?;

        let stats = index.stats();
        let meta = IndexMeta {
            root: index.root.clone(),
            git_head: index.git_head.clone(),
            file_count: stats.file_count,
            symbol_count: stats.symbol_count,
            cached_at: Utc::now(),
        };
        let meta_content = serde_json::to_string(&meta)?;
        write_atomic(&meta_path, &meta_content)?;
        Ok(())
    }

    /// Load index cache if valid for current repo state
    pub fn load_index_cache(&self, root: &Path) -> Option<CodebaseIndex> {
        let index_path = self.cache_dir.join(INDEX_CACHE_FILE);
        if !index_path.exists() {
            return None;
        }

        let _lock = self.lock(false).ok()?;
        let meta_path = self.cache_dir.join(INDEX_META_FILE);
        if meta_path.exists() {
            let meta_content = fs::read_to_string(&meta_path).ok()?;
            let meta: IndexMeta = serde_json::from_str(&meta_content).ok()?;
            if meta.root != root {
                return None;
            }
            if is_index_meta_valid(root, &meta) {
                let content = fs::read_to_string(&index_path).ok()?;
                let index: CodebaseIndex = serde_json::from_str(&content).ok()?;
                if index.root == root {
                    return Some(index);
                }
                return None;
            }
        }

        let content = fs::read_to_string(&index_path).ok()?;

        // Try to parse as full CodebaseIndex (current format)
        if let Ok(index) = serde_json::from_str::<CodebaseIndex>(&content) {
            if index.root != root {
                return None;
            }
            if is_index_cache_valid(root, &index) {
                return Some(index);
            }
            return None;
        }

        // Legacy format (IndexCache metadata only) - treat as miss
        let _legacy: IndexCache = serde_json::from_str(&content).ok()?;
        None
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
        let content = serde_json::to_string(cache)?;
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
        let content = serde_json::to_string(cache)?;
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
        let content = serde_json::to_string(glossary)?;
        write_atomic(&path, &content)?;
        Ok(())
    }

    /// Check if user has seen the welcome overlay
    pub fn has_seen_welcome(&self) -> bool {
        self.cache_dir.join(WELCOME_SEEN_FILE).exists()
    }

    /// Mark that user has seen the welcome overlay
    pub fn mark_welcome_seen(&self) -> anyhow::Result<()> {
        self.ensure_dir()?;
        let path = self.cache_dir.join(WELCOME_SEEN_FILE);
        fs::write(&path, "")?;
        Ok(())
    }

    /// Load question answer cache
    pub fn load_question_cache(&self) -> Option<QuestionCache> {
        let path = self.cache_dir.join(QUESTION_CACHE_FILE);
        if !path.exists() {
            return None;
        }
        let _lock = self.lock(false).ok()?;
        let content = fs::read_to_string(&path).ok()?;
        let mut cache: QuestionCache = serde_json::from_str(&content).ok()?;
        cache.cleanup();
        Some(cache)
    }

    /// Save question answer cache
    pub fn save_question_cache(&self, cache: &QuestionCache) -> anyhow::Result<()> {
        let _lock = self.lock(true)?;
        let path = self.cache_dir.join(QUESTION_CACHE_FILE);
        let content = serde_json::to_string(cache)?;
        write_atomic(&path, &content)?;
        Ok(())
    }

    /// Append a pipeline metric record (JSONL) for latency/cost tracking.
    pub fn append_pipeline_metric(&self, record: &PipelineMetricRecord) -> anyhow::Result<()> {
        let _lock = self.lock(true)?;
        let path = self.cache_dir.join(PIPELINE_METRICS_FILE);
        let mut file = OpenOptions::new().create(true).append(true).open(&path)?;
        let row = serde_json::to_string(record)?;
        use std::io::Write;
        writeln!(file, "{}", row)?;
        Ok(())
    }

    /// Append a per-suggestion quality record (JSONL).
    pub fn append_suggestion_quality(
        &self,
        record: &SuggestionQualityRecord,
    ) -> anyhow::Result<()> {
        let _lock = self.lock(true)?;
        let path = self.cache_dir.join(SUGGESTION_QUALITY_FILE);
        let mut file = OpenOptions::new().create(true).append(true).open(&path)?;
        let row = serde_json::to_string(record)?;
        use std::io::Write;
        writeln!(file, "{}", row)?;
        Ok(())
    }

    /// Append one self-iteration run record (JSONL).
    pub fn append_self_iteration_run(&self, record: &SelfIterationRunRecord) -> anyhow::Result<()> {
        let _lock = self.lock(true)?;
        let path = self.cache_dir.join(SELF_ITERATION_RUNS_FILE);
        let mut file = OpenOptions::new().create(true).append(true).open(&path)?;
        let row = serde_json::to_string(record)?;
        use std::io::Write;
        writeln!(file, "{}", row)?;
        Ok(())
    }

    /// Load up to `limit` latest suggestion-quality records (newest last).
    pub fn load_recent_suggestion_quality(
        &self,
        limit: usize,
    ) -> anyhow::Result<Vec<SuggestionQualityRecord>> {
        let path = self.cache_dir.join(SUGGESTION_QUALITY_FILE);
        if !path.exists() || limit == 0 {
            return Ok(Vec::new());
        }
        let _lock = self.lock(false)?;
        let content = fs::read_to_string(&path)?;
        let mut records: Vec<SuggestionQualityRecord> = content
            .lines()
            .filter_map(|line| serde_json::from_str::<SuggestionQualityRecord>(line).ok())
            .collect();
        if records.len() > limit {
            let split = records.len() - limit;
            records.drain(0..split);
        }
        Ok(records)
    }

    /// Load up to `limit` latest self-iteration run records (newest last).
    pub fn load_recent_self_iteration_runs(
        &self,
        limit: usize,
    ) -> anyhow::Result<Vec<SelfIterationRunRecord>> {
        let path = self.cache_dir.join(SELF_ITERATION_RUNS_FILE);
        if !path.exists() || limit == 0 {
            return Ok(Vec::new());
        }
        let _lock = self.lock(false)?;
        let content = fs::read_to_string(&path)?;
        let mut records: Vec<SelfIterationRunRecord> = content
            .lines()
            .filter_map(|line| serde_json::from_str::<SelfIterationRunRecord>(line).ok())
            .collect();
        if records.len() > limit {
            let split = records.len() - limit;
            records.drain(0..split);
        }
        Ok(records)
    }

    /// Compute rolling verify precision from suggestion-quality telemetry.
    ///
    /// Precision = verified / (verified + contradicted)
    pub fn rolling_verify_precision(&self, window: usize) -> Option<f64> {
        let records = self
            .load_recent_suggestion_quality(window.saturating_mul(3))
            .ok()?;
        let mut verified = 0usize;
        let mut contradicted = 0usize;
        for record in records.iter().rev() {
            match record.user_verify_outcome.as_deref() {
                Some("verified") => verified += 1,
                Some("contradicted") => contradicted += 1,
                _ => {}
            }
            if verified + contradicted >= window {
                break;
            }
        }
        let total = verified + contradicted;
        if total == 0 {
            None
        } else {
            Some(verified as f64 / total as f64)
        }
    }

    /// Aggregate recent contradicted verify outcomes by evidence id.
    ///
    /// Returns a map keyed by `SuggestionEvidenceRef.snippet_id` with the number of
    /// recent contradicted verify outcomes tied to that evidence id.
    pub fn recent_contradicted_evidence_counts(
        &self,
        window_rows: usize,
    ) -> anyhow::Result<HashMap<usize, usize>> {
        if window_rows == 0 {
            return Ok(HashMap::new());
        }
        let records = self.load_recent_suggestion_quality(window_rows)?;
        let mut counts: HashMap<usize, usize> = HashMap::new();
        for record in records {
            if record.validation_outcome != "verify_result"
                || record.user_verify_outcome.as_deref() != Some("contradicted")
            {
                continue;
            }
            for evidence_id in record.evidence_ids {
                *counts.entry(evidence_id).or_insert(0) += 1;
            }
        }
        Ok(counts)
    }

    /// Clear selected cache files only
    pub fn clear_selective(&self, options: &[ResetOption]) -> anyhow::Result<Vec<String>> {
        let _lock = self.lock(true)?;
        let mut cleared = Vec::new();

        for option in options {
            let files_to_remove: Vec<&str> = match option {
                ResetOption::Index => vec![INDEX_CACHE_FILE, INDEX_META_FILE],
                ResetOption::Suggestions => vec![SUGGESTIONS_CACHE_FILE],
                ResetOption::Summaries => vec![LLM_SUMMARIES_CACHE_FILE],
                ResetOption::Glossary => vec![GLOSSARY_FILE],
                ResetOption::Memory => vec![MEMORY_FILE],
                ResetOption::GroupingAi => vec![GROUPING_AI_CACHE_FILE],
                ResetOption::QuestionCache => vec![QUESTION_CACHE_FILE],
                ResetOption::PipelineMetrics => vec![PIPELINE_METRICS_FILE],
                ResetOption::SuggestionQuality => vec![SUGGESTION_QUALITY_FILE],
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

fn append_ignore_entry(path: &Path, entry: &str) -> anyhow::Result<()> {
    let content = fs::read_to_string(path).unwrap_or_default();
    let already_present = content.lines().any(|line| {
        let trimmed = line.trim();
        trimmed == entry || trimmed == ".cosmos"
    });
    if already_present {
        return Ok(());
    }

    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    use std::io::Write;
    if !content.trim().is_empty() && !content.ends_with('\n') {
        writeln!(file)?;
    }
    writeln!(file, "# Cosmos cache")?;
    writeln!(file, "{}", entry)?;
    Ok(())
}

/// Reset selected Cosmos cache files for the given repository.
pub async fn reset_cosmos(
    repo_root: &Path,
    options: &[ResetOption],
) -> anyhow::Result<Vec<String>> {
    let cache = Cache::new(repo_root);
    cache.clear_selective(options)
}

fn is_index_cache_valid(root: &Path, index: &CodebaseIndex) -> bool {
    // Fast path: check git HEAD and uncommitted changes
    // This avoids a full filesystem walk when the repo hasn't changed
    if let Some(cached_head) = &index.git_head {
        if let Some(current_head) = get_current_git_head(root) {
            if cached_head == &current_head && !crate::index::has_uncommitted_changes(root) {
                // Git HEAD matches and no uncommitted changes - cache is valid
                return true;
            }
        }
    }

    // Full hash fallback is expensive on large repos. For bigger indexes,
    // invalidate and rebuild instead of walking every file on startup.
    if index.files.len() > 2_000 {
        return false;
    }

    // Fall back to full hash comparison only for smaller repositories.
    is_index_cache_valid_full(root, index)
}

fn is_index_meta_valid(root: &Path, meta: &IndexMeta) -> bool {
    if let Some(cached_head) = &meta.git_head {
        if let Some(current_head) = get_current_git_head(root) {
            return cached_head == &current_head && !crate::index::has_uncommitted_changes(root);
        }
    }
    false
}

/// Get current git HEAD commit hash
fn get_current_git_head(root: &Path) -> Option<String> {
    use std::process::Command;

    let output = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(root)
        .output()
        .ok()?;

    if output.status.success() {
        let head = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if !head.is_empty() {
            return Some(head);
        }
    }
    None
}

/// Full cache validation by comparing file hashes
fn is_index_cache_valid_full(root: &Path, index: &CodebaseIndex) -> bool {
    let cached_hashes = compute_file_hashes(index);
    let current_hashes = match compute_current_hashes(root) {
        Ok(map) => map,
        Err(_) => return false,
    };
    cached_hashes == current_hashes
}

fn compute_current_hashes(root: &Path) -> anyhow::Result<HashMap<PathBuf, String>> {
    let mut hashes = HashMap::new();
    for entry in walkdir::WalkDir::new(root)
        .into_iter()
        .filter_entry(|e| !is_ignored_path(e.path()))
        .filter_map(|e| e.ok())
    {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }

        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
        let language = crate::index::Language::from_extension(ext);
        if language == crate::index::Language::Unknown {
            continue;
        }

        let metadata = match fs::metadata(path) {
            Ok(m) => m,
            Err(_) => continue,
        };
        if metadata.len() > crate::index::MAX_INDEX_FILE_BYTES {
            continue;
        }

        let bytes = match fs::read(path) {
            Ok(b) => b,
            Err(_) => continue,
        };
        if bytes.len() as u64 > crate::index::MAX_INDEX_FILE_BYTES {
            continue;
        }

        if std::str::from_utf8(&bytes).is_err() {
            continue;
        }

        let rel_path = path.strip_prefix(root).unwrap_or(path).to_path_buf();
        let hash = crate::util::hash_bytes(&bytes);
        hashes.insert(rel_path, hash);
    }
    Ok(hashes)
}

fn is_ignored_path(path: &Path) -> bool {
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
    let ignored = [
        "target",
        "node_modules",
        ".git",
        ".svn",
        ".hg",
        "dist",
        "build",
        "__pycache__",
        ".pytest_cache",
        "vendor",
        ".idea",
        ".vscode",
        ".cosmos",
    ];

    ignored.contains(&name) || name.starts_with('.')
}

/// Write content atomically by writing to a temp file first, then renaming.
///
/// # Platform Notes
/// - **Unix**: Uses atomic `rename()` which is guaranteed to be atomic by POSIX.
/// - **Windows**: Uses a backup-and-restore pattern since `rename()` can fail if the
///   destination exists. This is NOT truly atomic - if the process crashes between
///   the backup rename and the final rename, the file may be left in an inconsistent
///   state. The backup file (.bak) can be used for recovery. For cache files, this
///   trade-off is acceptable as the cache can be regenerated.
fn write_atomic(path: &Path, content: &str) -> anyhow::Result<()> {
    let tmp_path = path.with_extension("tmp");
    fs::write(&tmp_path, content)?;

    // Set restrictive permissions on Unix before renaming
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(0o600); // Owner read/write only
        let _ = std::fs::set_permissions(&tmp_path, perms);
    }

    #[cfg(windows)]
    {
        let backup_path = path.with_extension("bak");
        // Clean up any stale backup from a previous crash
        if backup_path.exists() {
            let _ = fs::remove_file(&backup_path);
        }
        if path.exists() {
            if let Err(err) = fs::rename(path, &backup_path) {
                let _ = fs::remove_file(&tmp_path);
                return Err(err.into());
            }
        }
        if let Err(err) = fs::rename(&tmp_path, path) {
            // Attempt rollback on failure
            if backup_path.exists() {
                let _ = fs::rename(&backup_path, path);
            }
            let _ = fs::remove_file(&tmp_path);
            return Err(err.into());
        }
        // Clean up backup on success
        if backup_path.exists() {
            let _ = fs::remove_file(&backup_path);
        }
        return Ok(());
    }

    #[cfg(not(windows))]
    {
        if let Err(err) = fs::rename(&tmp_path, path) {
            let _ = fs::remove_file(&tmp_path);
            return Err(err.into());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn test_index_cache_round_trip_and_invalidation() {
        let mut root = std::env::temp_dir();
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        root.push(format!("cosmos_index_cache_test_{}", nanos));
        let src_dir = root.join("src");
        fs::create_dir_all(&src_dir).unwrap();
        let file_path = src_dir.join("lib.rs");
        fs::write(&file_path, "pub fn hello() {}").unwrap();

        let index = CodebaseIndex::new(&root).unwrap();
        let cache = Cache::new(&root);
        cache.save_index_cache(&index).unwrap();

        let loaded = cache.load_index_cache(&root);
        assert!(loaded.is_some());

        fs::write(&file_path, "pub fn hello() { println!(\"hi\"); }").unwrap();
        let invalidated = cache.load_index_cache(&root);
        assert!(invalidated.is_none());

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn test_index_cache_meta_fast_path() {
        let mut root = std::env::temp_dir();
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        root.push(format!("cosmos_index_meta_test_{}", nanos));
        let src_dir = root.join("src");
        fs::create_dir_all(&src_dir).unwrap();
        let file_path = src_dir.join("lib.rs");
        fs::write(&file_path, "pub fn hello() -> i32 { 1 }").unwrap();

        std::process::Command::new("git")
            .args(["init"])
            .current_dir(&root)
            .output()
            .expect("git init");
        std::process::Command::new("git")
            .args(["add", "."])
            .current_dir(&root)
            .output()
            .expect("git add");
        std::process::Command::new("git")
            .args([
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.com",
                "commit",
                "-m",
                "initial",
            ])
            .current_dir(&root)
            .output()
            .expect("git commit");

        let index = CodebaseIndex::new(&root).unwrap();
        let cache = Cache::new(&root);
        cache.save_index_cache(&index).unwrap();

        assert!(root.join(CACHE_DIR).join(INDEX_META_FILE).exists());
        assert!(cache.load_index_cache(&root).is_some());

        fs::write(&file_path, "pub fn hello() -> i32 { 2 }").unwrap();
        assert!(cache.load_index_cache(&root).is_none());

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn reset_options_include_question_cache_and_pipeline_metrics() {
        let options = ResetOption::all();
        assert!(options.contains(&ResetOption::QuestionCache));
        assert!(options.contains(&ResetOption::PipelineMetrics));
        assert!(options.contains(&ResetOption::SuggestionQuality));
        assert!(!ResetOption::defaults().contains(&ResetOption::QuestionCache));
        assert!(!ResetOption::defaults().contains(&ResetOption::PipelineMetrics));
        assert!(!ResetOption::defaults().contains(&ResetOption::SuggestionQuality));
    }

    #[test]
    fn clear_selective_removes_question_cache_and_pipeline_metrics() {
        let mut root = std::env::temp_dir();
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        root.push(format!("cosmos_reset_cache_test_{}", nanos));
        fs::create_dir_all(&root).unwrap();

        // Make this a git repo so ensure_dir can write to .git/info/exclude.
        std::process::Command::new("git")
            .args(["init"])
            .current_dir(&root)
            .output()
            .expect("git init should run");

        let cache = Cache::new(&root);

        let mut question_cache = QuestionCache::default();
        question_cache.set(
            "What does this do?".to_string(),
            "It processes requests.".to_string(),
            "ctx".to_string(),
        );
        cache.save_question_cache(&question_cache).unwrap();

        let metric = PipelineMetricRecord {
            timestamp: Utc::now(),
            stage: "summary".to_string(),
            summary_ms: Some(10),
            suggest_ms: None,
            verify_ms: None,
            apply_ms: None,
            review_ms: None,
            tokens: 123,
            cost: 0.01,
            gate: "ok".to_string(),
            passed: true,
        };
        cache.append_pipeline_metric(&metric).unwrap();
        let quality = SuggestionQualityRecord {
            timestamp: Utc::now(),
            run_id: "run-1".to_string(),
            suggestion_id: "suggestion-1".to_string(),
            evidence_ids: vec![1, 2],
            validation_outcome: "validated".to_string(),
            validation_reason: Some("Looks good".to_string()),
            user_verify_outcome: None,
        };
        cache.append_suggestion_quality(&quality).unwrap();

        let cache_dir = root.join(CACHE_DIR);
        assert!(cache_dir.join(QUESTION_CACHE_FILE).exists());
        assert!(cache_dir.join(PIPELINE_METRICS_FILE).exists());
        assert!(cache_dir.join(SUGGESTION_QUALITY_FILE).exists());

        cache
            .clear_selective(&[
                ResetOption::QuestionCache,
                ResetOption::PipelineMetrics,
                ResetOption::SuggestionQuality,
            ])
            .unwrap();

        assert!(!cache_dir.join(QUESTION_CACHE_FILE).exists());
        assert!(!cache_dir.join(PIPELINE_METRICS_FILE).exists());
        assert!(!cache_dir.join(SUGGESTION_QUALITY_FILE).exists());

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn suggestion_quality_deserializes_without_optional_fields() {
        let row = serde_json::json!({
            "timestamp": Utc::now(),
            "run_id": "run-1",
            "suggestion_id": "sid-1",
            "evidence_ids": [0],
            "validation_outcome": "pending"
        });
        let parsed: SuggestionQualityRecord = serde_json::from_value(row).unwrap();
        assert_eq!(parsed.validation_reason, None);
        assert_eq!(parsed.user_verify_outcome, None);
    }

    #[test]
    fn self_iteration_record_round_trip_and_load_recent() {
        let mut root = std::env::temp_dir();
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        root.push(format!("cosmos_self_iteration_test_{}", nanos));
        fs::create_dir_all(&root).unwrap();

        // Make this a git repo so ensure_dir can write to .git/info/exclude.
        std::process::Command::new("git")
            .args(["init"])
            .current_dir(&root)
            .output()
            .expect("git init should run");

        let cache = Cache::new(&root);
        let record = SelfIterationRunRecord {
            timestamp: Utc::now(),
            run_id: "run-abc".to_string(),
            mode: "fast".to_string(),
            cosmos_repo: root.clone(),
            target_repo: root.clone(),
            passed: true,
            command_outcomes: vec![SelfIterationCommandOutcome {
                name: "cargo test".to_string(),
                command: "cargo test --locked".to_string(),
                cwd: root.clone(),
                duration_ms: 1000,
                success: true,
                exit_code: Some(0),
                timed_out: false,
                stdout_tail: "ok".to_string(),
                stderr_tail: String::new(),
                note: None,
            }],
            reliability_metrics: Some(SelfIterationSuggestionMetrics {
                trials: 1,
                provisional_count: 8,
                final_count: 8,
                validated_count: 7,
                pending_count: 0,
                rejected_count: 1,
                displayed_valid_ratio: 0.875,
                validated_ratio: 0.875,
                rejected_ratio: 0.125,
                preview_sampled: 4,
                preview_verified_count: 3,
                preview_contradicted_count: 1,
                preview_insufficient_count: 0,
                preview_error_count: 0,
                preview_precision: Some(0.75),
                evidence_line1_ratio: 0.2,
                evidence_source_mix: HashMap::from([
                    ("pattern".to_string(), 12usize),
                    ("hotspot".to_string(), 7usize),
                    ("core".to_string(), 6usize),
                ]),
                suggest_total_tokens: 2000,
                suggest_total_cost_usd: 0.002,
                suggest_total_ms: 1500,
            }),
            report_path: None,
            notes: vec!["note".to_string()],
        };
        cache.append_self_iteration_run(&record).unwrap();

        let loaded = cache.load_recent_self_iteration_runs(10).unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].run_id, "run-abc");
        assert!(loaded[0].reliability_metrics.is_some());

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn self_iteration_record_deserializes_without_optional_fields() {
        let row = serde_json::json!({
            "timestamp": Utc::now(),
            "run_id": "run-legacy",
            "mode": "fast",
            "cosmos_repo": ".",
            "target_repo": ".",
            "passed": true
        });
        let parsed: SelfIterationRunRecord = serde_json::from_value(row).unwrap();
        assert!(parsed.command_outcomes.is_empty());
        assert!(parsed.reliability_metrics.is_none());
        assert!(parsed.notes.is_empty());
        assert!(parsed.report_path.is_none());
    }
}
