//! Suggestion engine for Cosmos
//!
//! LLM-driven suggestions with a hard cap to avoid overwhelming users.
//! Suggestions are generated on-demand via `analyze_codebase()`.

pub mod llm;

/// Maximum suggestions to display to avoid overwhelming users
const MAX_SUGGESTIONS: usize = 15;

use crate::index::CodebaseIndex;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use uuid::Uuid;

/// Source of a suggestion
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SuggestionSource {
    /// Pattern matching, no LLM cost
    Static,
    /// Previously generated, loaded from cache
    Cached,
    /// Grok Fast for quick categorization
    LlmFast,
    /// LLM for detailed analysis
    LlmDeep,
}

/// Kind of suggestion
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SuggestionKind {
    /// Code improvement/refactoring
    Improvement,
    /// Potential bug fix
    BugFix,
    /// New feature suggestion
    Feature,
    /// Performance optimization
    Optimization,
    /// Code quality/maintainability
    Quality,
    /// Documentation improvement
    Documentation,
    /// Test coverage
    Testing,
    /// Code refactoring (extract, rename, restructure)
    Refactoring,
}

impl SuggestionKind {
    pub fn label(&self) -> &'static str {
        match self {
            SuggestionKind::Improvement => "Improve",
            SuggestionKind::BugFix => "Fix",
            SuggestionKind::Feature => "Feature",
            SuggestionKind::Optimization => "Optimize",
            SuggestionKind::Quality => "Quality",
            SuggestionKind::Documentation => "Docs",
            SuggestionKind::Testing => "Test",
            SuggestionKind::Refactoring => "Refactor",
        }
    }
}

/// Priority level
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Priority {
    Low,
    Medium,
    High,
}

/// A suggestion for improvement
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Suggestion {
    pub id: Uuid,
    pub kind: SuggestionKind,
    pub priority: Priority,
    /// Primary file (used for display/grouping)
    pub file: PathBuf,
    /// Additional files affected by this suggestion (for multi-file refactors)
    #[serde(default)]
    pub additional_files: Vec<PathBuf>,
    pub line: Option<usize>,
    pub summary: String,
    pub detail: Option<String>,
    pub source: SuggestionSource,
    pub created_at: DateTime<Utc>,
    /// Whether the user has dismissed this suggestion
    pub dismissed: bool,
    /// Whether the suggestion has been applied
    pub applied: bool,
}

impl Suggestion {
    pub fn new(
        kind: SuggestionKind,
        priority: Priority,
        file: PathBuf,
        summary: String,
        source: SuggestionSource,
    ) -> Self {
        Self {
            id: Uuid::new_v4(),
            kind,
            priority,
            file,
            additional_files: Vec::new(),
            line: None,
            summary,
            detail: None,
            source,
            created_at: Utc::now(),
            dismissed: false,
            applied: false,
        }
    }

    pub fn with_line(mut self, line: usize) -> Self {
        self.line = Some(line);
        self
    }

    pub fn with_detail(mut self, detail: String) -> Self {
        self.detail = Some(detail);
        self
    }

    pub fn with_additional_files(mut self, files: Vec<PathBuf>) -> Self {
        self.additional_files = files;
        self
    }

    /// Get all files affected by this suggestion (primary + additional)
    pub fn affected_files(&self) -> Vec<&PathBuf> {
        std::iter::once(&self.file)
            .chain(self.additional_files.iter())
            .collect()
    }

    /// Check if this is a multi-file suggestion
    pub fn is_multi_file(&self) -> bool {
        !self.additional_files.is_empty()
    }

    /// Get the total number of files affected
    pub fn file_count(&self) -> usize {
        1 + self.additional_files.len()
    }
}

/// The suggestion engine
pub struct SuggestionEngine {
    pub suggestions: Vec<Suggestion>,
    pub index: CodebaseIndex,
}

impl SuggestionEngine {
    /// Create a new suggestion engine from a codebase index
    ///
    /// Starts empty - LLM suggestions are generated separately.
    pub fn new(index: CodebaseIndex) -> Self {
        Self {
            suggestions: Vec::new(),
            index,
        }
    }

    /// Get all active suggestions (not dismissed/applied), capped at MAX_SUGGESTIONS
    pub fn active_suggestions(&self) -> Vec<&Suggestion> {
        self.suggestions
            .iter()
            .filter(|s| !s.dismissed && !s.applied)
            .take(MAX_SUGGESTIONS)
            .collect()
    }

    /// Mark a suggestion as applied
    pub fn mark_applied(&mut self, id: Uuid) {
        if let Some(s) = self.suggestions.iter_mut().find(|s| s.id == id) {
            s.applied = true;
        }
    }

    /// Mark a suggestion as not applied (used for undo).
    pub fn unmark_applied(&mut self, id: Uuid) {
        if let Some(s) = self.suggestions.iter_mut().find(|s| s.id == id) {
            s.applied = false;
        }
    }

    /// Add a suggestion from LLM
    pub fn add_llm_suggestion(&mut self, suggestion: Suggestion) {
        self.suggestions.push(suggestion);
        self.suggestions.sort_by(|a, b| b.priority.cmp(&a.priority));
    }

    /// Sort suggestions with git context: changed files first, then blast radius, then priority.
    pub fn sort_with_context(&mut self, context: &crate::context::WorkContext) {
        let changed: std::collections::HashSet<PathBuf> =
            context.all_changed_files().into_iter().cloned().collect();

        // “Blast radius” = files that import changed files (and direct deps of changed files).
        let mut blast: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();
        for path in &changed {
            if let Some(file_index) = self.index.files.get(path) {
                for u in &file_index.summary.used_by {
                    blast.insert(u.clone());
                }
                for d in &file_index.summary.depends_on {
                    blast.insert(d.clone());
                }
            }
        }
        for c in &changed {
            blast.remove(c);
        }

        let kind_weight = |k: SuggestionKind| -> i64 {
            match k {
                SuggestionKind::BugFix => 40,
                SuggestionKind::Refactoring => 30,
                SuggestionKind::Optimization => 25,
                SuggestionKind::Testing => 20,
                SuggestionKind::Quality => 15,
                SuggestionKind::Documentation => 10,
                SuggestionKind::Improvement => 10,
                SuggestionKind::Feature => 0,
            }
        };

        self.suggestions.sort_by(|a, b| {
            let a_changed = changed.contains(&a.file);
            let b_changed = changed.contains(&b.file);
            if a_changed != b_changed {
                return b_changed.cmp(&a_changed);
            }

            let a_blast = blast.contains(&a.file);
            let b_blast = blast.contains(&b.file);
            if a_blast != b_blast {
                return b_blast.cmp(&a_blast);
            }

            // Higher priority first
            let pri = b.priority.cmp(&a.priority);
            if pri != std::cmp::Ordering::Equal {
                return pri;
            }

            // Then kind weight
            let kw = kind_weight(b.kind).cmp(&kind_weight(a.kind));
            if kw != std::cmp::Ordering::Equal {
                return kw;
            }

            // Finally: newest first
            b.created_at.cmp(&a.created_at)
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_priority_ordering() {
        assert!(Priority::High > Priority::Medium);
        assert!(Priority::Medium > Priority::Low);
    }

    #[test]
    fn test_suggestion_creation() {
        let suggestion = Suggestion::new(
            SuggestionKind::Improvement,
            Priority::High,
            PathBuf::from("test.rs"),
            "Test suggestion".to_string(),
            SuggestionSource::Static,
        );

        assert!(!suggestion.dismissed);
        assert!(!suggestion.applied);
    }
}
