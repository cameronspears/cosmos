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

use chrono::{DateTime, Duration, Utc};
use cosmos_core::index::CodebaseIndex;
use cosmos_core::suggest::Suggestion;
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::time::{Duration as StdDuration, Instant};

const CACHE_DIR: &str = ".cosmos";
const CACHE_LAYOUT_V2_DIR: &str = "v2";
const INDEX_CACHE_FILE: &str = "index.json";
const INDEX_META_FILE: &str = "index.meta.json";
const SUGGESTIONS_CACHE_FILE: &str = "suggestions.json";
const MEMORY_FILE: &str = "memory.json";
const GLOSSARY_FILE: &str = "glossary.json";
const GROUPING_AI_CACHE_FILE: &str = "grouping_ai.json";
const PIPELINE_METRICS_FILE: &str = "pipeline_metrics.jsonl";
const SUGGESTION_QUALITY_FILE: &str = "suggestion_quality.jsonl";
const IMPLEMENTATION_HARNESS_FILE: &str = "implementation_harness.jsonl";
const SUGGESTION_RUN_AUDIT_FILE: &str = "suggestion_runs.jsonl";
const APPLY_PLAN_AUDIT_FILE: &str = "apply_plan_audit.jsonl";
const SUGGESTION_COVERAGE_FILE: &str = "suggestion_coverage.json";
const CACHE_LOCK_TIMEOUT_SECS: u64 = 5;
const CACHE_LOCK_RETRY_MS: u64 = 50;

/// Options for selective cache reset
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResetOption {
    /// Clear index.json - codebase structure, symbols, patterns
    Index,
    /// Clear suggestions.json - generated suggestions
    Suggestions,
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
    /// Clear implementation_harness.jsonl - apply harness telemetry
    ImplementationHarness,
    /// Clear data_notice_seen - OpenRouter data use acknowledgement
    DataNotice,
}

impl ResetOption {
    /// Get human-readable label for the option
    pub fn label(&self) -> &'static str {
        match self {
            ResetOption::Index => "Index & Symbols",
            ResetOption::Suggestions => "Suggestions",
            ResetOption::Glossary => "Domain Glossary",
            ResetOption::Memory => "Repo Memory",
            ResetOption::GroupingAi => "Grouping AI",
            ResetOption::QuestionCache => "Question Cache",
            ResetOption::PipelineMetrics => "Pipeline Metrics",
            ResetOption::SuggestionQuality => "Suggestion Quality",
            ResetOption::ImplementationHarness => "Implementation Harness",
            ResetOption::DataNotice => "Data Notice Ack",
        }
    }

    /// Get description for the option
    pub fn description(&self) -> &'static str {
        match self {
            ResetOption::Index => "rebuild file tree",
            ResetOption::Suggestions => "regenerate with AI",
            ResetOption::Glossary => "extract terminology",
            ResetOption::Memory => "decisions/conventions",
            ResetOption::GroupingAi => "rebuild AI grouping",
            ResetOption::QuestionCache => "clear saved Q&A",
            ResetOption::PipelineMetrics => "clear latency/cost logs",
            ResetOption::SuggestionQuality => "clear validation telemetry",
            ResetOption::ImplementationHarness => "clear apply harness telemetry",
            ResetOption::DataNotice => "show data notice again",
        }
    }

    /// Get all options in display order
    pub fn all() -> Vec<ResetOption> {
        vec![
            ResetOption::Index,
            ResetOption::Suggestions,
            ResetOption::Glossary,
            ResetOption::Memory,
            ResetOption::GroupingAi,
            ResetOption::QuestionCache,
            ResetOption::PipelineMetrics,
            ResetOption::SuggestionQuality,
            ResetOption::ImplementationHarness,
            ResetOption::DataNotice,
        ]
    }

    /// Get default options (safe to reset without losing user data)
    pub fn defaults() -> Vec<ResetOption> {
        vec![
            ResetOption::Index,
            ResetOption::Suggestions,
            ResetOption::Glossary,
            ResetOption::GroupingAi,
        ]
    }
}

/// Flag file indicating user has seen the welcome overlay
const WELCOME_SEEN_FILE: &str = "welcome_seen";
/// Flag file indicating user has acknowledged the OpenRouter data-use notice.
const DATA_NOTICE_SEEN_FILE: &str = "data_notice_seen";

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

/// Normalize cache keys to repo-relative paths.
pub fn normalize_cache_path(path: &Path, root: &Path) -> PathBuf {
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

// ═══════════════════════════════════════════════════════════════════════════
//  GROUPING AI CACHE - AI-assisted layer classification hints
// ═══════════════════════════════════════════════════════════════════════════

const GROUPING_AI_CACHE_DAYS: i64 = 30;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GroupingAiEntry {
    pub layer: cosmos_core::grouping::Layer,
    pub confidence: f64,
    pub file_hash: String,
    pub generated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GroupingAiCache {
    pub entries: HashMap<PathBuf, GroupingAiEntry>,
    pub cached_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SuggestionCoverageCache {
    pub updated_at: DateTime<Utc>,
    pub recently_scanned: HashMap<PathBuf, DateTime<Utc>>,
}

impl SuggestionCoverageCache {
    pub fn new() -> Self {
        Self {
            updated_at: Utc::now(),
            recently_scanned: HashMap::new(),
        }
    }

    pub fn normalize_paths(&mut self, root: &Path) -> bool {
        if self.recently_scanned.is_empty() {
            return false;
        }
        let mut changed = false;
        let mut normalized = HashMap::with_capacity(self.recently_scanned.len());
        for (path, ts) in &self.recently_scanned {
            let key = normalize_cache_path(path, root);
            if &key != path {
                changed = true;
            }
            let replace = normalized
                .get(&key)
                .map(|existing: &DateTime<Utc>| ts > existing)
                .unwrap_or(true);
            if replace {
                normalized.insert(key, *ts);
            }
        }
        if changed {
            self.recently_scanned = normalized;
            self.updated_at = Utc::now();
        }
        changed
    }

    pub fn record_scan<I>(&mut self, files: I)
    where
        I: IntoIterator<Item = PathBuf>,
    {
        let now = Utc::now();
        for file in files {
            self.recently_scanned.insert(file, now);
        }
        self.updated_at = now;
    }

    pub fn scanned_at(&self, path: &Path) -> Option<DateTime<Utc>> {
        self.recently_scanned.get(path).copied()
    }

    pub fn prune(&mut self, keep_limit: usize) {
        if self.recently_scanned.len() <= keep_limit {
            return;
        }
        let mut by_time = self
            .recently_scanned
            .iter()
            .map(|(path, ts)| (path.clone(), *ts))
            .collect::<Vec<_>>();
        by_time.sort_by(|left, right| right.1.cmp(&left.1).then_with(|| left.0.cmp(&right.0)));
        by_time.truncate(keep_limit);
        self.recently_scanned = by_time.into_iter().collect();
        self.updated_at = Utc::now();
    }
}

impl Default for SuggestionCoverageCache {
    fn default() -> Self {
        Self::new()
    }
}

/// Lightweight pipeline metric row written as JSONL to `.cosmos/pipeline_metrics.jsonl`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PipelineMetricRecord {
    pub timestamp: DateTime<Utc>,
    pub stage: String,
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
    #[serde(default)]
    pub batch_missing_index_count: usize,
    #[serde(default)]
    pub batch_no_reason_count: usize,
    #[serde(default)]
    pub transport_retry_count: usize,
    #[serde(default)]
    pub transport_recovered_count: usize,
    #[serde(default)]
    pub rewrite_recovered_count: usize,
    #[serde(default)]
    pub prevalidation_contradiction_count: usize,
}

/// One finalized suggestion-run snapshot for post-run auditing.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SuggestionRunAuditRecord {
    pub timestamp: DateTime<Utc>,
    pub run_id: String,
    pub suggestion_count: usize,
    pub validated_count: usize,
    pub rejected_count: usize,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub parse_strategy: Option<String>,
    #[serde(default)]
    pub attempt_index: Option<usize>,
    #[serde(default)]
    pub attempt_count: Option<usize>,
    #[serde(default)]
    pub gate_passed: Option<bool>,
    #[serde(default)]
    pub gate_fail_reasons: Vec<String>,
    #[serde(default)]
    pub llm_ms: Option<u64>,
    #[serde(default)]
    pub tool_calls: Option<usize>,
    #[serde(default)]
    pub notes: Vec<String>,
    #[serde(default)]
    pub response_preview: Option<String>,
    pub suggestions: Vec<Suggestion>,
}

/// Apply-plan lifecycle event for post-run auditing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApplyPlanAuditEvent {
    Opened,
    Confirmed,
}

/// One apply-plan snapshot row capturing exactly what was shown to the user.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApplyPlanAuditRecord {
    pub timestamp: DateTime<Utc>,
    pub event: ApplyPlanAuditEvent,
    pub run_id: Option<String>,
    pub suggestion_id: String,
    pub suggestion_summary: String,
    pub suggestion_file: PathBuf,
    pub evidence_ids: Vec<usize>,
    pub affected_files: Vec<PathBuf>,
    pub preview_friendly_title: String,
    pub preview_problem_summary: String,
    pub preview_outcome: String,
    pub preview_description: String,
    pub preview_verification_note: String,
    pub preview_evidence_line: Option<u32>,
    pub preview_evidence_snippet: Option<String>,
}

/// One apply-harness execution summary row written as JSONL.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImplementationHarnessRecord {
    #[serde(default = "implementation_harness_schema_version_default")]
    pub schema_version: u32,
    pub timestamp: DateTime<Utc>,
    pub run_id: String,
    pub suggestion_id: String,
    pub passed: bool,
    pub attempt_count: usize,
    pub total_ms: u64,
    pub total_cost_usd: f64,
    pub changed_file_count: usize,
    pub quick_check_status: String,
    #[serde(default)]
    pub finalization_status: String,
    #[serde(default)]
    pub mutation_on_failure: Option<bool>,
    #[serde(default = "implementation_harness_run_context_default")]
    pub run_context: String,
    #[serde(default)]
    pub independent_review_executed: bool,
    #[serde(default)]
    pub schema_fallback_count: usize,
    #[serde(default)]
    pub smart_escalation_count: usize,
    #[serde(default)]
    pub baseline_quick_check_failfast_count: usize,
    #[serde(default)]
    pub fail_reasons: Vec<String>,
    #[serde(default)]
    pub report_path: Option<PathBuf>,
}

fn implementation_harness_schema_version_default() -> u32 {
    4
}

fn implementation_harness_run_context_default() -> String {
    "interactive".to_string()
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
            let key = normalize_cache_path(path, root);
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
    cache_root: PathBuf,
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
        let cache_root = project_root.join(CACHE_DIR);
        let cache_dir = cache_root.join(CACHE_LAYOUT_V2_DIR);
        Self {
            cache_root,
            cache_dir,
        }
    }

    /// Ensure the cache directory exists
    fn ensure_dir(&self) -> anyhow::Result<()> {
        if !self.cache_root.exists() {
            fs::create_dir_all(&self.cache_root)?;
        }
        if !self.cache_dir.exists() {
            fs::create_dir_all(&self.cache_dir)?;
        }
        self.ensure_cosmos_ignored()?;
        Ok(())
    }

    fn ensure_cosmos_ignored(&self) -> anyhow::Result<()> {
        let Some(repo_root) = self.cache_root.parent() else {
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
                let ready = fs::create_dir_all(parent).is_ok();
                if ready && append_ignore_entry(&info_exclude_path, ".cosmos/").is_ok() {
                    return Ok(());
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

    /// Check if user has acknowledged the OpenRouter data-use notice.
    pub fn has_seen_data_notice(&self) -> bool {
        self.cache_dir.join(DATA_NOTICE_SEEN_FILE).exists()
    }

    /// Mark that user has acknowledged the OpenRouter data-use notice.
    pub fn mark_data_notice_seen(&self) -> anyhow::Result<()> {
        self.ensure_dir()?;
        let path = self.cache_dir.join(DATA_NOTICE_SEEN_FILE);
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

    pub fn load_suggestion_coverage_cache(&self) -> Option<SuggestionCoverageCache> {
        let path = self.cache_dir.join(SUGGESTION_COVERAGE_FILE);
        if !path.exists() {
            return None;
        }
        let _lock = self.lock(false).ok()?;
        let content = fs::read_to_string(&path).ok()?;
        let mut cache: SuggestionCoverageCache = serde_json::from_str(&content).ok()?;
        let root = self
            .cache_root
            .parent()
            .unwrap_or(self.cache_root.as_path());
        let _ = cache.normalize_paths(root);
        Some(cache)
    }

    pub fn save_suggestion_coverage_cache(
        &self,
        coverage: &SuggestionCoverageCache,
    ) -> anyhow::Result<()> {
        let _lock = self.lock(true)?;
        let path = self.cache_dir.join(SUGGESTION_COVERAGE_FILE);
        let content = serde_json::to_string(coverage)?;
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

    /// Append one implementation-harness telemetry row (JSONL).
    pub fn append_implementation_harness(
        &self,
        record: &ImplementationHarnessRecord,
    ) -> anyhow::Result<()> {
        let _lock = self.lock(true)?;
        let path = self.cache_dir.join(IMPLEMENTATION_HARNESS_FILE);
        let mut file = OpenOptions::new().create(true).append(true).open(&path)?;
        let row = serde_json::to_string(record)?;
        use std::io::Write;
        writeln!(file, "{}", row)?;
        Ok(())
    }

    /// Append one finalized suggestion-run snapshot row (JSONL).
    pub fn append_suggestion_run_audit(
        &self,
        record: &SuggestionRunAuditRecord,
    ) -> anyhow::Result<()> {
        let _lock = self.lock(true)?;
        let path = self.cache_dir.join(SUGGESTION_RUN_AUDIT_FILE);
        let mut file = OpenOptions::new().create(true).append(true).open(&path)?;
        let row = serde_json::to_string(record)?;
        use std::io::Write;
        writeln!(file, "{}", row)?;
        Ok(())
    }

    /// Append one apply-plan snapshot row (JSONL).
    pub fn append_apply_plan_audit(&self, record: &ApplyPlanAuditRecord) -> anyhow::Result<()> {
        let _lock = self.lock(true)?;
        let path = self.cache_dir.join(APPLY_PLAN_AUDIT_FILE);
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

    /// Load up to `limit` latest implementation-harness telemetry records (newest last).
    pub fn load_recent_implementation_harness(
        &self,
        limit: usize,
    ) -> anyhow::Result<Vec<ImplementationHarnessRecord>> {
        let path = self.cache_dir.join(IMPLEMENTATION_HARNESS_FILE);
        if !path.exists() || limit == 0 {
            return Ok(Vec::new());
        }
        let _lock = self.lock(false)?;
        let content = fs::read_to_string(&path)?;
        let mut records: Vec<ImplementationHarnessRecord> = content
            .lines()
            .filter_map(|line| serde_json::from_str::<ImplementationHarnessRecord>(line).ok())
            .collect();
        if records.len() > limit {
            let split = records.len() - limit;
            records.drain(0..split);
        }
        Ok(records)
    }

    /// Load up to `limit` latest suggestion-run audit rows (newest last).
    pub fn load_recent_suggestion_run_audit(
        &self,
        limit: usize,
    ) -> anyhow::Result<Vec<SuggestionRunAuditRecord>> {
        let path = self.cache_dir.join(SUGGESTION_RUN_AUDIT_FILE);
        if !path.exists() || limit == 0 {
            return Ok(Vec::new());
        }
        let _lock = self.lock(false)?;
        let content = fs::read_to_string(&path)?;
        let mut records: Vec<SuggestionRunAuditRecord> = content
            .lines()
            .filter_map(|line| serde_json::from_str::<SuggestionRunAuditRecord>(line).ok())
            .collect();
        if records.len() > limit {
            let split = records.len() - limit;
            records.drain(0..split);
        }
        Ok(records)
    }

    /// Load up to `limit` latest apply-plan audit rows (newest last).
    pub fn load_recent_apply_plan_audit(
        &self,
        limit: usize,
    ) -> anyhow::Result<Vec<ApplyPlanAuditRecord>> {
        let path = self.cache_dir.join(APPLY_PLAN_AUDIT_FILE);
        if !path.exists() || limit == 0 {
            return Ok(Vec::new());
        }
        let _lock = self.lock(false)?;
        let content = fs::read_to_string(&path)?;
        let mut records: Vec<ApplyPlanAuditRecord> = content
            .lines()
            .filter_map(|line| serde_json::from_str::<ApplyPlanAuditRecord>(line).ok())
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
                ResetOption::Suggestions => vec![
                    SUGGESTIONS_CACHE_FILE,
                    SUGGESTION_RUN_AUDIT_FILE,
                    APPLY_PLAN_AUDIT_FILE,
                    SUGGESTION_COVERAGE_FILE,
                ],
                ResetOption::Glossary => vec![GLOSSARY_FILE],
                ResetOption::Memory => vec![MEMORY_FILE],
                ResetOption::GroupingAi => vec![GROUPING_AI_CACHE_FILE],
                ResetOption::QuestionCache => vec![QUESTION_CACHE_FILE],
                ResetOption::PipelineMetrics => vec![PIPELINE_METRICS_FILE],
                ResetOption::SuggestionQuality => vec![SUGGESTION_QUALITY_FILE],
                ResetOption::ImplementationHarness => vec![IMPLEMENTATION_HARNESS_FILE],
                ResetOption::DataNotice => vec![DATA_NOTICE_SEEN_FILE],
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
            if cached_head == &current_head && !cosmos_core::index::has_uncommitted_changes(root) {
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
            return cached_head == &current_head
                && !cosmos_core::index::has_uncommitted_changes(root);
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
        let language = cosmos_core::index::Language::from_extension(ext);
        if language == cosmos_core::index::Language::Unknown {
            continue;
        }

        let metadata = match fs::metadata(path) {
            Ok(m) => m,
            Err(_) => continue,
        };
        if metadata.len() > cosmos_core::index::MAX_INDEX_FILE_BYTES {
            continue;
        }

        let bytes = match fs::read(path) {
            Ok(b) => b,
            Err(_) => continue,
        };
        if bytes.len() as u64 > cosmos_core::index::MAX_INDEX_FILE_BYTES {
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

        assert!(root
            .join(CACHE_DIR)
            .join(CACHE_LAYOUT_V2_DIR)
            .join(INDEX_META_FILE)
            .exists());
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
        assert!(options.contains(&ResetOption::ImplementationHarness));
        assert!(options.contains(&ResetOption::DataNotice));
        assert!(!ResetOption::defaults().contains(&ResetOption::QuestionCache));
        assert!(!ResetOption::defaults().contains(&ResetOption::PipelineMetrics));
        assert!(!ResetOption::defaults().contains(&ResetOption::SuggestionQuality));
        assert!(!ResetOption::defaults().contains(&ResetOption::ImplementationHarness));
        assert!(!ResetOption::defaults().contains(&ResetOption::DataNotice));
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
            stage: "suggest".to_string(),
            suggest_ms: Some(10),
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
            batch_missing_index_count: 0,
            batch_no_reason_count: 0,
            transport_retry_count: 0,
            transport_recovered_count: 0,
            rewrite_recovered_count: 0,
            prevalidation_contradiction_count: 0,
        };
        cache.append_suggestion_quality(&quality).unwrap();
        let harness = ImplementationHarnessRecord {
            schema_version: 4,
            timestamp: Utc::now(),
            run_id: "run-1".to_string(),
            suggestion_id: "suggestion-1".to_string(),
            passed: true,
            attempt_count: 1,
            total_ms: 2000,
            total_cost_usd: 0.002,
            changed_file_count: 1,
            quick_check_status: "passed".to_string(),
            finalization_status: "applied".to_string(),
            mutation_on_failure: Some(false),
            run_context: "interactive".to_string(),
            independent_review_executed: false,
            schema_fallback_count: 0,
            smart_escalation_count: 0,
            baseline_quick_check_failfast_count: 0,
            fail_reasons: Vec::new(),
            report_path: None,
        };
        cache.append_implementation_harness(&harness).unwrap();
        cache.mark_data_notice_seen().unwrap();

        let cache_dir = root.join(CACHE_DIR).join(CACHE_LAYOUT_V2_DIR);
        assert!(cache_dir.join(QUESTION_CACHE_FILE).exists());
        assert!(cache_dir.join(PIPELINE_METRICS_FILE).exists());
        assert!(cache_dir.join(SUGGESTION_QUALITY_FILE).exists());
        assert!(cache_dir.join(IMPLEMENTATION_HARNESS_FILE).exists());
        assert!(cache_dir.join(DATA_NOTICE_SEEN_FILE).exists());

        cache
            .clear_selective(&[
                ResetOption::QuestionCache,
                ResetOption::PipelineMetrics,
                ResetOption::SuggestionQuality,
                ResetOption::ImplementationHarness,
                ResetOption::DataNotice,
            ])
            .unwrap();

        assert!(!cache_dir.join(QUESTION_CACHE_FILE).exists());
        assert!(!cache_dir.join(PIPELINE_METRICS_FILE).exists());
        assert!(!cache_dir.join(SUGGESTION_QUALITY_FILE).exists());
        assert!(!cache_dir.join(IMPLEMENTATION_HARNESS_FILE).exists());
        assert!(!cache_dir.join(DATA_NOTICE_SEEN_FILE).exists());

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn data_notice_seen_persists_and_can_be_cleared() {
        let mut root = std::env::temp_dir();
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        root.push(format!("cosmos_data_notice_test_{}", nanos));
        fs::create_dir_all(&root).unwrap();

        std::process::Command::new("git")
            .args(["init"])
            .current_dir(&root)
            .output()
            .expect("git init should run");

        let cache = Cache::new(&root);
        assert!(!cache.has_seen_data_notice());
        cache.mark_data_notice_seen().unwrap();
        assert!(cache.has_seen_data_notice());

        cache.clear_selective(&[ResetOption::DataNotice]).unwrap();
        assert!(!cache.has_seen_data_notice());

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
        assert_eq!(parsed.batch_missing_index_count, 0);
        assert_eq!(parsed.batch_no_reason_count, 0);
        assert_eq!(parsed.transport_retry_count, 0);
        assert_eq!(parsed.transport_recovered_count, 0);
        assert_eq!(parsed.rewrite_recovered_count, 0);
        assert_eq!(parsed.prevalidation_contradiction_count, 0);
    }

    #[test]
    fn implementation_harness_record_deserializes_legacy_shape() {
        let row = serde_json::json!({
            "timestamp": Utc::now(),
            "run_id": "run-legacy",
            "suggestion_id": "s-1",
            "passed": false,
            "attempt_count": 2,
            "total_ms": 3000,
            "total_cost_usd": 0.004,
            "changed_file_count": 0,
            "quick_check_status": "unavailable"
        });
        let parsed: ImplementationHarnessRecord = serde_json::from_value(row).unwrap();
        assert_eq!(parsed.schema_version, 4);
        assert_eq!(parsed.finalization_status, "");
        assert!(parsed.mutation_on_failure.is_none());
        assert_eq!(parsed.run_context, "interactive");
        assert!(!parsed.independent_review_executed);
        assert_eq!(parsed.schema_fallback_count, 0);
        assert_eq!(parsed.smart_escalation_count, 0);
        assert_eq!(parsed.baseline_quick_check_failfast_count, 0);
        assert!(parsed.report_path.is_none());
    }

    #[test]
    fn implementation_harness_round_trip_and_load_recent() {
        let mut root = std::env::temp_dir();
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        root.push(format!("cosmos_impl_harness_recent_test_{}", nanos));
        fs::create_dir_all(&root).unwrap();

        std::process::Command::new("git")
            .args(["init"])
            .current_dir(&root)
            .output()
            .expect("git init should run");

        let cache = Cache::new(&root);
        for idx in 0..3 {
            let row = ImplementationHarnessRecord {
                schema_version: 4,
                timestamp: Utc::now(),
                run_id: format!("run-{}", idx),
                suggestion_id: format!("s-{}", idx),
                passed: idx % 2 == 0,
                attempt_count: 1,
                total_ms: 1_000 + idx as u64,
                total_cost_usd: 0.001 + idx as f64 * 0.0001,
                changed_file_count: idx + 1,
                quick_check_status: "passed".to_string(),
                finalization_status: "failed_before_finalize".to_string(),
                mutation_on_failure: Some(false),
                run_context: "lab".to_string(),
                independent_review_executed: idx % 2 == 0,
                schema_fallback_count: idx,
                smart_escalation_count: idx.saturating_sub(1),
                baseline_quick_check_failfast_count: 0,
                fail_reasons: Vec::new(),
                report_path: None,
            };
            cache.append_implementation_harness(&row).unwrap();
        }

        let recent = cache.load_recent_implementation_harness(2).unwrap();
        assert_eq!(recent.len(), 2);
        assert_eq!(recent[0].run_id, "run-1");
        assert_eq!(recent[1].run_id, "run-2");
        assert_eq!(recent[0].run_context, "lab");
        assert_eq!(recent[1].schema_version, 4);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn suggestion_and_apply_plan_audit_round_trip_and_reset() {
        let mut root = std::env::temp_dir();
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        root.push(format!("cosmos_suggestion_audit_test_{}", nanos));
        fs::create_dir_all(&root).unwrap();

        std::process::Command::new("git")
            .args(["init"])
            .current_dir(&root)
            .output()
            .expect("git init should run");

        let cache = Cache::new(&root);
        for idx in 0..3usize {
            let suggestion = Suggestion::new(
                cosmos_core::suggest::SuggestionKind::BugFix,
                cosmos_core::suggest::Priority::High,
                std::path::PathBuf::from(format!("src/lib_{}.rs", idx)),
                format!("Fix issue {}", idx),
                cosmos_core::suggest::SuggestionSource::LlmDeep,
            )
            .with_validation_state(cosmos_core::suggest::SuggestionValidationState::Validated);
            let run_row = SuggestionRunAuditRecord {
                timestamp: Utc::now(),
                run_id: format!("run-{}", idx),
                suggestion_count: 1,
                validated_count: 1,
                rejected_count: 0,
                model: None,
                parse_strategy: None,
                attempt_index: None,
                attempt_count: None,
                gate_passed: None,
                gate_fail_reasons: Vec::new(),
                llm_ms: None,
                tool_calls: None,
                notes: Vec::new(),
                response_preview: None,
                suggestions: vec![suggestion.clone()],
            };
            cache.append_suggestion_run_audit(&run_row).unwrap();

            let apply_row = ApplyPlanAuditRecord {
                timestamp: Utc::now(),
                event: if idx % 2 == 0 {
                    ApplyPlanAuditEvent::Opened
                } else {
                    ApplyPlanAuditEvent::Confirmed
                },
                run_id: Some(format!("run-{}", idx)),
                suggestion_id: suggestion.id.to_string(),
                suggestion_summary: suggestion.summary.clone(),
                suggestion_file: suggestion.file.clone(),
                evidence_ids: vec![idx + 1],
                affected_files: vec![suggestion.file.clone()],
                preview_friendly_title: "Fix".to_string(),
                preview_problem_summary: format!("Problem {}", idx),
                preview_outcome: format!("Outcome {}", idx),
                preview_description: format!("Description {}", idx),
                preview_verification_note: "Using pre-validated suggestion evidence.".to_string(),
                preview_evidence_line: Some(100 + idx as u32),
                preview_evidence_snippet: Some(format!("snippet {}", idx)),
            };
            cache.append_apply_plan_audit(&apply_row).unwrap();
        }

        let recent_runs = cache.load_recent_suggestion_run_audit(2).unwrap();
        assert_eq!(recent_runs.len(), 2);
        assert_eq!(recent_runs[0].run_id, "run-1");
        assert_eq!(recent_runs[1].run_id, "run-2");
        assert_eq!(recent_runs[1].suggestion_count, 1);

        let recent_apply = cache.load_recent_apply_plan_audit(2).unwrap();
        assert_eq!(recent_apply.len(), 2);
        assert_eq!(recent_apply[0].run_id.as_deref(), Some("run-1"));
        assert_eq!(recent_apply[1].run_id.as_deref(), Some("run-2"));
        assert_eq!(recent_apply[1].event, ApplyPlanAuditEvent::Opened);

        let cache_dir = root.join(CACHE_DIR).join(CACHE_LAYOUT_V2_DIR);
        assert!(cache_dir.join(SUGGESTION_RUN_AUDIT_FILE).exists());
        assert!(cache_dir.join(APPLY_PLAN_AUDIT_FILE).exists());

        cache.clear_selective(&[ResetOption::Suggestions]).unwrap();
        assert!(!cache_dir.join(SUGGESTION_RUN_AUDIT_FILE).exists());
        assert!(!cache_dir.join(APPLY_PLAN_AUDIT_FILE).exists());

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn suggestion_coverage_round_trip_and_suggestions_reset() {
        let mut root = std::env::temp_dir();
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        root.push(format!("cosmos_suggestion_coverage_test_{}", nanos));
        fs::create_dir_all(&root).unwrap();

        std::process::Command::new("git")
            .args(["init"])
            .current_dir(&root)
            .output()
            .expect("git init should run");

        let cache = Cache::new(&root);
        let mut coverage = SuggestionCoverageCache::new();
        coverage.record_scan(vec![
            PathBuf::from("src/a.rs"),
            PathBuf::from("src/b.rs"),
            PathBuf::from("src/c.rs"),
        ]);
        cache.save_suggestion_coverage_cache(&coverage).unwrap();

        let loaded = cache
            .load_suggestion_coverage_cache()
            .expect("coverage should load");
        assert_eq!(loaded.recently_scanned.len(), 3);
        assert!(loaded
            .recently_scanned
            .contains_key(&PathBuf::from("src/a.rs")));

        let cache_dir = root.join(CACHE_DIR).join(CACHE_LAYOUT_V2_DIR);
        assert!(cache_dir.join(SUGGESTION_COVERAGE_FILE).exists());
        cache.clear_selective(&[ResetOption::Suggestions]).unwrap();
        assert!(!cache_dir.join(SUGGESTION_COVERAGE_FILE).exists());

        let _ = fs::remove_dir_all(&root);
    }
}
