//! Suggestion history persistence with SQLite
//!
//! Stores suggestion history in `.cosmos/history.db` for:
//! - Tracking suggestions across sessions
//! - Analytics on what gets applied vs dismissed
//! - "Previously seen" badges for recurring issues
//! - Learning from user preferences

use crate::suggest::{Priority, Suggestion, SuggestionKind, SuggestionSource};
use chrono::{DateTime, Utc};
use rusqlite::{params, Connection, Result as SqlResult};
use std::path::{Path, PathBuf};
use uuid::Uuid;

/// Outcome of a suggestion
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SuggestionOutcome {
    /// Suggestion is pending (no action taken)
    Pending,
    /// User applied the suggestion
    Applied,
    /// User dismissed the suggestion
    Dismissed,
    /// Suggestion expired (file changed significantly)
    Expired,
}

impl SuggestionOutcome {
    fn as_str(&self) -> &'static str {
        match self {
            SuggestionOutcome::Pending => "pending",
            SuggestionOutcome::Applied => "applied",
            SuggestionOutcome::Dismissed => "dismissed",
            SuggestionOutcome::Expired => "expired",
        }
    }

    fn from_str(s: &str) -> Self {
        match s {
            "applied" => SuggestionOutcome::Applied,
            "dismissed" => SuggestionOutcome::Dismissed,
            "expired" => SuggestionOutcome::Expired,
            _ => SuggestionOutcome::Pending,
        }
    }
}

/// A historical suggestion record
#[derive(Debug, Clone)]
pub struct HistoricalSuggestion {
    pub id: Uuid,
    pub kind: SuggestionKind,
    pub priority: Priority,
    pub file: PathBuf,
    pub line: Option<usize>,
    pub summary: String,
    pub detail: Option<String>,
    pub source: SuggestionSource,
    pub created_at: DateTime<Utc>,
    pub outcome: SuggestionOutcome,
    pub outcome_at: Option<DateTime<Utc>>,
    /// Hash of file content when suggestion was generated
    pub file_hash: Option<String>,
    /// Number of times this suggestion (or similar) has appeared
    pub occurrence_count: u32,
}

/// History database manager
pub struct HistoryDb {
    conn: Connection,
}

impl HistoryDb {
    /// Open or create the history database for a repository
    pub fn open(repo_path: &Path) -> SqlResult<Self> {
        let cosmos_dir = repo_path.join(".cosmos");
        std::fs::create_dir_all(&cosmos_dir).ok();
        
        let db_path = cosmos_dir.join("history.db");
        let conn = Connection::open(&db_path)?;
        
        // Initialize schema
        conn.execute_batch(include_str!("history_schema.sql"))?;
        
        Ok(Self { conn })
    }

    /// Record a new suggestion
    pub fn record_suggestion(&self, suggestion: &Suggestion, file_hash: Option<&str>) -> SqlResult<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO suggestions (
                id, kind, priority, file, line, summary, detail, source, 
                created_at, outcome, file_hash
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                suggestion.id.to_string(),
                kind_to_str(suggestion.kind),
                priority_to_str(suggestion.priority),
                suggestion.file.to_string_lossy(),
                suggestion.line.map(|l| l as i64),
                suggestion.summary,
                suggestion.detail,
                source_to_str(suggestion.source),
                suggestion.created_at.to_rfc3339(),
                "pending",
                file_hash,
            ],
        )?;
        
        // Update occurrence tracking
        self.update_occurrence_count(&suggestion.summary, &suggestion.file)?;
        
        Ok(())
    }

    /// Mark a suggestion as applied
    pub fn mark_applied(&self, id: Uuid) -> SqlResult<()> {
        self.conn.execute(
            "UPDATE suggestions SET outcome = 'applied', outcome_at = ?1 WHERE id = ?2",
            params![Utc::now().to_rfc3339(), id.to_string()],
        )?;
        Ok(())
    }

    /// Mark a suggestion as dismissed
    pub fn mark_dismissed(&self, id: Uuid) -> SqlResult<()> {
        self.conn.execute(
            "UPDATE suggestions SET outcome = 'dismissed', outcome_at = ?1 WHERE id = ?2",
            params![Utc::now().to_rfc3339(), id.to_string()],
        )?;
        Ok(())
    }

    /// Check if a similar suggestion has been seen before
    pub fn is_recurring(&self, summary: &str, file: &Path) -> SqlResult<bool> {
        let count: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM suggestions WHERE summary = ?1 AND file = ?2",
            params![summary, file.to_string_lossy()],
            |row| row.get(0),
        )?;
        Ok(count > 1)
    }

    /// Get occurrence count for a suggestion pattern
    pub fn get_occurrence_count(&self, summary: &str, file: &Path) -> SqlResult<u32> {
        let count: i64 = self.conn.query_row(
            "SELECT COALESCE(count, 0) FROM occurrence_counts WHERE summary_hash = ?1 AND file = ?2",
            params![hash_summary(summary), file.to_string_lossy()],
            |row| row.get(0),
        ).unwrap_or(0);
        Ok(count as u32)
    }

    /// Update occurrence count for a suggestion pattern
    fn update_occurrence_count(&self, summary: &str, file: &Path) -> SqlResult<()> {
        self.conn.execute(
            "INSERT INTO occurrence_counts (summary_hash, file, count, last_seen)
             VALUES (?1, ?2, 1, ?3)
             ON CONFLICT(summary_hash, file) DO UPDATE SET 
                count = count + 1,
                last_seen = ?3",
            params![
                hash_summary(summary),
                file.to_string_lossy(),
                Utc::now().to_rfc3339(),
            ],
        )?;
        Ok(())
    }

    /// Get recent suggestions for a file
    pub fn get_file_history(&self, file: &Path, limit: usize) -> SqlResult<Vec<HistoricalSuggestion>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, kind, priority, file, line, summary, detail, source, 
                    created_at, outcome, outcome_at, file_hash
             FROM suggestions 
             WHERE file = ?1 
             ORDER BY created_at DESC 
             LIMIT ?2"
        )?;
        
        let rows = stmt.query_map(params![file.to_string_lossy(), limit as i64], |row| {
            Ok(HistoricalSuggestion {
                id: Uuid::parse_str(&row.get::<_, String>(0)?).unwrap_or_default(),
                kind: str_to_kind(&row.get::<_, String>(1)?),
                priority: str_to_priority(&row.get::<_, String>(2)?),
                file: PathBuf::from(row.get::<_, String>(3)?),
                line: row.get::<_, Option<i64>>(4)?.map(|l| l as usize),
                summary: row.get(5)?,
                detail: row.get(6)?,
                source: str_to_source(&row.get::<_, String>(7)?),
                created_at: DateTime::parse_from_rfc3339(&row.get::<_, String>(8)?)
                    .map(|dt| dt.with_timezone(&Utc))
                    .unwrap_or_else(|_| Utc::now()),
                outcome: SuggestionOutcome::from_str(&row.get::<_, String>(9)?),
                outcome_at: row.get::<_, Option<String>>(10)?
                    .and_then(|s| DateTime::parse_from_rfc3339(&s).ok())
                    .map(|dt| dt.with_timezone(&Utc)),
                file_hash: row.get(11)?,
                occurrence_count: 1, // Will be updated separately if needed
            })
        })?;
        
        rows.collect()
    }

    /// Get analytics summary
    pub fn get_analytics(&self) -> SqlResult<HistoryAnalytics> {
        let total: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM suggestions",
            [],
            |row| row.get(0),
        )?;
        
        let applied: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM suggestions WHERE outcome = 'applied'",
            [],
            |row| row.get(0),
        )?;
        
        let dismissed: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM suggestions WHERE outcome = 'dismissed'",
            [],
            |row| row.get(0),
        )?;
        
        let pending: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM suggestions WHERE outcome = 'pending'",
            [],
            |row| row.get(0),
        )?;
        
        // Most common suggestion types
        let mut stmt = self.conn.prepare(
            "SELECT kind, COUNT(*) as cnt FROM suggestions 
             GROUP BY kind ORDER BY cnt DESC LIMIT 5"
        )?;
        let top_kinds: Vec<(String, i64)> = stmt.query_map([], |row| {
            Ok((row.get(0)?, row.get(1)?))
        })?.filter_map(|r| r.ok()).collect();
        
        // Most suggested files
        let mut stmt = self.conn.prepare(
            "SELECT file, COUNT(*) as cnt FROM suggestions 
             GROUP BY file ORDER BY cnt DESC LIMIT 5"
        )?;
        let top_files: Vec<(String, i64)> = stmt.query_map([], |row| {
            Ok((row.get(0)?, row.get(1)?))
        })?.filter_map(|r| r.ok()).collect();
        
        Ok(HistoryAnalytics {
            total_suggestions: total as u32,
            applied_count: applied as u32,
            dismissed_count: dismissed as u32,
            pending_count: pending as u32,
            apply_rate: if total > 0 { applied as f64 / total as f64 } else { 0.0 },
            top_kinds,
            top_files,
        })
    }

    /// Clean up old suggestions (older than 90 days)
    pub fn cleanup_old(&self, days: i64) -> SqlResult<usize> {
        let cutoff = Utc::now() - chrono::Duration::days(days);
        let deleted = self.conn.execute(
            "DELETE FROM suggestions WHERE created_at < ?1 AND outcome != 'pending'",
            params![cutoff.to_rfc3339()],
        )?;
        Ok(deleted)
    }
}

/// Analytics summary from history
#[derive(Debug, Clone)]
pub struct HistoryAnalytics {
    pub total_suggestions: u32,
    pub applied_count: u32,
    pub dismissed_count: u32,
    pub pending_count: u32,
    pub apply_rate: f64,
    pub top_kinds: Vec<(String, i64)>,
    pub top_files: Vec<(String, i64)>,
}

impl HistoryAnalytics {
    /// Format for display
    pub fn display(&self) -> String {
        let mut output = String::new();
        output.push_str(&format!("Total suggestions: {}\n", self.total_suggestions));
        output.push_str(&format!("Applied: {} ({:.1}%)\n", 
            self.applied_count, self.apply_rate * 100.0));
        output.push_str(&format!("Dismissed: {}\n", self.dismissed_count));
        output.push_str(&format!("Pending: {}\n", self.pending_count));
        
        if !self.top_kinds.is_empty() {
            output.push_str("\nTop categories:\n");
            for (kind, count) in &self.top_kinds {
                output.push_str(&format!("  {}: {}\n", kind, count));
            }
        }
        
        if !self.top_files.is_empty() {
            output.push_str("\nMost suggested files:\n");
            for (file, count) in &self.top_files {
                // Truncate long paths
                let display_file = if file.len() > 40 {
                    format!("...{}", &file[file.len()-37..])
                } else {
                    file.clone()
                };
                output.push_str(&format!("  {}: {}\n", display_file, count));
            }
        }
        
        output
    }
}

// Helper functions for enum conversion

fn kind_to_str(kind: SuggestionKind) -> &'static str {
    match kind {
        SuggestionKind::Improvement => "improvement",
        SuggestionKind::BugFix => "bugfix",
        SuggestionKind::Feature => "feature",
        SuggestionKind::Optimization => "optimization",
        SuggestionKind::Quality => "quality",
        SuggestionKind::Documentation => "documentation",
        SuggestionKind::Testing => "testing",
    }
}

fn str_to_kind(s: &str) -> SuggestionKind {
    match s {
        "bugfix" => SuggestionKind::BugFix,
        "feature" => SuggestionKind::Feature,
        "optimization" => SuggestionKind::Optimization,
        "quality" => SuggestionKind::Quality,
        "documentation" => SuggestionKind::Documentation,
        "testing" => SuggestionKind::Testing,
        _ => SuggestionKind::Improvement,
    }
}

fn priority_to_str(priority: Priority) -> &'static str {
    match priority {
        Priority::High => "high",
        Priority::Medium => "medium",
        Priority::Low => "low",
    }
}

fn str_to_priority(s: &str) -> Priority {
    match s {
        "high" => Priority::High,
        "low" => Priority::Low,
        _ => Priority::Medium,
    }
}

fn source_to_str(source: SuggestionSource) -> &'static str {
    match source {
        SuggestionSource::Static => "static",
        SuggestionSource::Cached => "cached",
        SuggestionSource::LlmFast => "llm_fast",
        SuggestionSource::LlmDeep => "llm_deep",
    }
}

fn str_to_source(s: &str) -> SuggestionSource {
    match s {
        "static" => SuggestionSource::Static,
        "cached" => SuggestionSource::Cached,
        "llm_fast" => SuggestionSource::LlmFast,
        _ => SuggestionSource::LlmDeep,
    }
}

/// Simple hash for grouping similar summaries
fn hash_summary(summary: &str) -> String {
    // Use first 100 chars lowercased as a simple grouping key
    // Must use char iteration to avoid panicking on multi-byte UTF-8 boundaries
    summary
        .to_lowercase()
        .chars()
        .take(100)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn create_test_suggestion() -> Suggestion {
        Suggestion::new(
            SuggestionKind::Improvement,
            Priority::Medium,
            PathBuf::from("src/main.rs"),
            "Test suggestion".to_string(),
            SuggestionSource::Static,
        )
    }

    #[test]
    fn test_history_db_basic() {
        let tmp = TempDir::new().unwrap();
        let db = HistoryDb::open(tmp.path()).unwrap();
        
        let suggestion = create_test_suggestion();
        db.record_suggestion(&suggestion, Some("abc123")).unwrap();
        
        let history = db.get_file_history(&PathBuf::from("src/main.rs"), 10).unwrap();
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].summary, "Test suggestion");
    }

    #[test]
    fn test_outcome_tracking() {
        let tmp = TempDir::new().unwrap();
        let db = HistoryDb::open(tmp.path()).unwrap();
        
        let suggestion = create_test_suggestion();
        let id = suggestion.id;
        db.record_suggestion(&suggestion, None).unwrap();
        
        db.mark_applied(id).unwrap();
        
        let history = db.get_file_history(&PathBuf::from("src/main.rs"), 10).unwrap();
        assert_eq!(history[0].outcome, SuggestionOutcome::Applied);
    }

    #[test]
    fn test_analytics() {
        let tmp = TempDir::new().unwrap();
        let db = HistoryDb::open(tmp.path()).unwrap();
        
        for _ in 0..5 {
            db.record_suggestion(&create_test_suggestion(), None).unwrap();
        }
        
        let analytics = db.get_analytics().unwrap();
        assert_eq!(analytics.total_suggestions, 5);
    }

    #[test]
    fn test_hash_summary_multibyte_utf8() {
        // This would panic before the fix if slicing at byte 100 fell within a multi-byte char
        let summary_with_emoji = "🎉".repeat(50); // 50 emojis = 200 bytes, 50 chars
        let hash = hash_summary(&summary_with_emoji);
        assert_eq!(hash.chars().count(), 50); // Should truncate to 100 chars, but only 50 available
        
        // Mix of ASCII and multi-byte to hit exactly the boundary
        let mixed = format!("{}{}", "a".repeat(99), "é"); // 99 ASCII + 1 two-byte char = 101 bytes
        let hash = hash_summary(&mixed);
        assert_eq!(hash.chars().count(), 100); // Should get exactly 100 chars
        
        // Long string with Chinese characters (3 bytes each)
        let chinese = "中".repeat(50); // 50 chars = 150 bytes
        let hash = hash_summary(&chinese);
        assert_eq!(hash.chars().count(), 50); // Only 50 chars available
    }
}


