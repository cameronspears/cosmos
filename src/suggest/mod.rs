//! Suggestion engine for Cosmos
//!
//! Tiered approach to minimize LLM spend:
//! - Layer 1: Static rules (FREE)
//! - Layer 2: Cached suggestions (ONE-TIME)
//! - Layer 3: Grok Fast for categorization (~$0.0001/call)
//! - Layer 4: LLM for deep analysis (Speed for analysis, Smart for code gen)

#![allow(dead_code)]

pub mod llm;
pub mod static_rules;

use crate::index::{CodebaseIndex, PatternSeverity};
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

impl SuggestionSource {
    pub fn icon(&self) -> &'static str {
        match self {
            SuggestionSource::Static => "  ",
            SuggestionSource::Cached => " ",
            SuggestionSource::LlmFast => " ",
            SuggestionSource::LlmDeep => " ",
        }
    }
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
}

impl SuggestionKind {
    pub fn icon(&self) -> char {
        match self {
            SuggestionKind::Improvement => '\u{2728}',  // 
            SuggestionKind::BugFix => '\u{1F41B}',      // 
            SuggestionKind::Feature => '\u{2795}',      // 
            SuggestionKind::Optimization => '\u{26A1}', // 
            SuggestionKind::Quality => '\u{2726}',      // 
            SuggestionKind::Documentation => '\u{1F4DD}', // 
            SuggestionKind::Testing => '\u{1F9EA}',     // 
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            SuggestionKind::Improvement => "Improve",
            SuggestionKind::BugFix => "Fix",
            SuggestionKind::Feature => "Feature",
            SuggestionKind::Optimization => "Optimize",
            SuggestionKind::Quality => "Quality",
            SuggestionKind::Documentation => "Docs",
            SuggestionKind::Testing => "Test",
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

impl Priority {
    pub fn icon(&self) -> char {
        match self {
            Priority::High => '\u{25CF}',   // 
            Priority::Medium => '\u{25D0}', // 
            Priority::Low => '\u{25CB}',    // 
        }
    }

    pub fn from_severity(severity: PatternSeverity) -> Self {
        match severity {
            PatternSeverity::High => Priority::High,
            PatternSeverity::Medium => Priority::Medium,
            PatternSeverity::Low | PatternSeverity::Info => Priority::Low,
        }
    }
}

/// A suggestion for improvement
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Suggestion {
    pub id: Uuid,
    pub kind: SuggestionKind,
    pub priority: Priority,
    pub file: PathBuf,
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

    /// Format for display in the suggestion list
    pub fn display_summary(&self) -> String {
        if let Some(line) = self.line {
            format!("{}:{} - {}", self.file.display(), line, self.summary)
        } else {
            format!("{} - {}", self.file.display(), self.summary)
        }
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
    /// By default, starts empty - LLM suggestions are generated separately.
    /// Static rules are available as fallback but not auto-generated.
    pub fn new(index: CodebaseIndex) -> Self {
        Self {
            suggestions: Vec::new(),
            index,
        }
    }
    
    /// Create an empty suggestion engine (populated by LLM later)
    pub fn new_empty(index: CodebaseIndex) -> Self {
        Self {
            suggestions: Vec::new(),
            index,
        }
    }
    
    /// Generate suggestions from static analysis (no LLM)
    /// 
    /// Only used as fallback when LLM is unavailable (no API key).
    /// These are intentionally minimal - we trust the LLM for real suggestions.
    #[allow(dead_code)]
    pub fn generate_static_suggestions(&mut self) {
        // Only generate static suggestions for truly critical issues
        for (path, file_index) in &self.index.files {
            let static_suggestions = static_rules::analyze_file(path, file_index);
            self.suggestions.extend(static_suggestions);
        }

        // Sort by priority (high first)
        self.suggestions.sort_by(|a, b| b.priority.cmp(&a.priority));
    }

    /// Get suggestions for a specific file
    pub fn suggestions_for_file(&self, path: &PathBuf) -> Vec<&Suggestion> {
        self.suggestions
            .iter()
            .filter(|s| &s.file == path && !s.dismissed && !s.applied)
            .collect()
    }

    /// Get all active suggestions (not dismissed/applied)
    pub fn active_suggestions(&self) -> Vec<&Suggestion> {
        self.suggestions
            .iter()
            .filter(|s| !s.dismissed && !s.applied)
            .collect()
    }

    /// Get suggestions grouped by file
    pub fn suggestions_by_file(&self) -> std::collections::HashMap<PathBuf, Vec<&Suggestion>> {
        let mut map: std::collections::HashMap<PathBuf, Vec<&Suggestion>> = std::collections::HashMap::new();
        
        for suggestion in self.active_suggestions() {
            map.entry(suggestion.file.clone())
                .or_default()
                .push(suggestion);
        }
        
        map
    }

    /// Get high priority suggestions
    pub fn high_priority_suggestions(&self) -> Vec<&Suggestion> {
        self.active_suggestions()
            .into_iter()
            .filter(|s| s.priority == Priority::High)
            .collect()
    }

    /// Dismiss a suggestion
    pub fn dismiss(&mut self, id: Uuid) {
        if let Some(s) = self.suggestions.iter_mut().find(|s| s.id == id) {
            s.dismissed = true;
        }
    }

    /// Mark a suggestion as applied
    pub fn mark_applied(&mut self, id: Uuid) {
        if let Some(s) = self.suggestions.iter_mut().find(|s| s.id == id) {
            s.applied = true;
        }
    }

    /// Add a suggestion from LLM
    pub fn add_llm_suggestion(&mut self, suggestion: Suggestion) {
        self.suggestions.push(suggestion);
        self.suggestions.sort_by(|a, b| b.priority.cmp(&a.priority));
    }

    /// Get suggestion count by priority
    pub fn counts(&self) -> SuggestionCounts {
        let active = self.active_suggestions();
        SuggestionCounts {
            total: active.len(),
            high: active.iter().filter(|s| s.priority == Priority::High).count(),
            medium: active.iter().filter(|s| s.priority == Priority::Medium).count(),
            low: active.iter().filter(|s| s.priority == Priority::Low).count(),
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct SuggestionCounts {
    pub total: usize,
    pub high: usize,
    pub medium: usize,
    pub low: usize,
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
