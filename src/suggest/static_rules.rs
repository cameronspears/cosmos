//! Static rule-based suggestions (fallback when LLM unavailable)
//!
//! These are MINIMAL rules for truly critical issues only.
//! The LLM handles the real suggestions - these are just a safety net.

use super::{Priority, Suggestion, SuggestionKind, SuggestionSource};
use crate::index::{FileIndex, PatternKind};
use std::path::PathBuf;

/// Analyze a file and generate static suggestions
/// 
/// Intentionally minimal - only flags FIXME/BUG markers and critical issues.
/// The LLM provides the real value; this is just fallback.
pub fn analyze_file(path: &PathBuf, file_index: &FileIndex) -> Vec<Suggestion> {
    let mut suggestions = Vec::new();

    // Only pattern-based suggestions for truly actionable items
    for pattern in &file_index.patterns {
        if let Some(suggestion) = pattern_to_suggestion(path, pattern) {
            suggestions.push(suggestion);
        }
    }

    suggestions
}

/// Convert a detected pattern to a suggestion
/// 
/// Very selective - only surfaces FIXME/BUG markers that developers explicitly left.
/// Everything else is left for the LLM to handle with proper context.
fn pattern_to_suggestion(path: &PathBuf, pattern: &crate::index::Pattern) -> Option<Suggestion> {
    match pattern.kind {
        // Only show FIXME and BUG markers - these are explicit developer flags
        PatternKind::TodoMarker => {
            let todo_text = &pattern.description;
            let upper = todo_text.to_uppercase();
            
            // Only surface explicit FIXME/BUG markers, not regular TODOs
            if upper.contains("FIXME") || upper.contains("BUG") {
                let summary = format!("Developer note: {}", truncate(todo_text, 60));
                let suggestion = Suggestion::new(
                    SuggestionKind::BugFix,
                    Priority::High,
                    path.clone(),
                    summary,
                    SuggestionSource::Static,
                )
                .with_line(pattern.line)
                .with_detail(todo_text.clone());
                
                Some(suggestion)
            } else {
                None
            }
        }
        
        // Let the LLM handle everything else with proper context
        _ => None,
    }
}

/// Truncate a string for display
fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        format!("{}...", &s[..max - 3])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_regular_todo_not_shown() {
        // Regular TODOs should not generate suggestions - let LLM handle context
        let pattern = crate::index::Pattern {
            kind: PatternKind::TodoMarker,
            file: PathBuf::from("test.rs"),
            line: 10,
            description: "TODO: implement this feature".to_string(),
        };

        let path = PathBuf::from("test.rs");
        let suggestion = pattern_to_suggestion(&path, &pattern);

        assert!(suggestion.is_none());
    }

    #[test]
    fn test_fixme_is_shown() {
        // FIXME markers are explicit developer flags - surface these
        let pattern = crate::index::Pattern {
            kind: PatternKind::TodoMarker,
            file: PathBuf::from("test.rs"),
            line: 10,
            description: "FIXME: this is broken".to_string(),
        };

        let path = PathBuf::from("test.rs");
        let suggestion = pattern_to_suggestion(&path, &pattern);

        assert!(suggestion.is_some());
        assert!(suggestion.unwrap().priority == Priority::High);
    }
    
    #[test]
    fn test_bug_marker_is_shown() {
        // BUG markers are explicit developer flags - surface these
        let pattern = crate::index::Pattern {
            kind: PatternKind::TodoMarker,
            file: PathBuf::from("test.rs"),
            line: 42,
            description: "BUG: race condition when multiple users".to_string(),
        };

        let path = PathBuf::from("test.rs");
        let suggestion = pattern_to_suggestion(&path, &pattern);

        assert!(suggestion.is_some());
        let s = suggestion.unwrap();
        assert!(s.priority == Priority::High);
        assert!(s.kind == SuggestionKind::BugFix);
    }
}
