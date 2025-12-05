//! Static rule-based suggestions (no LLM cost)
//!
//! These rules analyze code patterns and generate suggestions
//! without any API calls, making them completely free.

use super::{Priority, Suggestion, SuggestionKind, SuggestionSource};
use crate::index::{FileIndex, PatternKind, Symbol, SymbolKind};
use std::path::PathBuf;

/// Analyze a file and generate static suggestions
pub fn analyze_file(path: &PathBuf, file_index: &FileIndex) -> Vec<Suggestion> {
    let mut suggestions = Vec::new();

    // File-level checks
    suggestions.extend(check_file_size(path, file_index));
    suggestions.extend(check_complexity(path, file_index));

    // Symbol-level checks
    for symbol in &file_index.symbols {
        suggestions.extend(check_function_length(path, symbol));
        suggestions.extend(check_function_complexity(path, symbol));
    }

    // Pattern-based suggestions
    for pattern in &file_index.patterns {
        if let Some(suggestion) = pattern_to_suggestion(path, pattern) {
            suggestions.push(suggestion);
        }
    }

    // Language-specific checks
    suggestions.extend(language_specific_checks(path, file_index));

    suggestions
}

/// Check for overly large files - only flag truly problematic files
fn check_file_size(path: &PathBuf, file_index: &FileIndex) -> Vec<Suggestion> {
    let mut suggestions = Vec::new();

    // Only flag files that are genuinely too large (2000+ lines)
    if file_index.loc > 2000 {
        suggestions.push(
            Suggestion::new(
                SuggestionKind::Improvement,
                Priority::High,
                path.clone(),
                format!("File has {} lines - split into modules", file_index.loc),
                SuggestionSource::Static,
            )
            .with_detail(format!(
                "This file has grown to {} lines. Consider extracting \
                 distinct functionality into separate modules for maintainability.",
                file_index.loc
            )),
        );
    }

    suggestions
}

/// Check overall file complexity - only flag severe cases
fn check_complexity(path: &PathBuf, file_index: &FileIndex) -> Vec<Suggestion> {
    let mut suggestions = Vec::new();

    let function_count = file_index
        .symbols
        .iter()
        .filter(|s| matches!(s.kind, SymbolKind::Function | SymbolKind::Method))
        .count();

    // Only flag very high average complexity (25+)
    if function_count > 0 {
        let avg_complexity = file_index.complexity / function_count as f64;

        if avg_complexity > 25.0 {
            suggestions.push(
                Suggestion::new(
                    SuggestionKind::Improvement,
                    Priority::High,
                    path.clone(),
                    format!(
                        "Very high complexity ({:.0}) - refactor needed",
                        avg_complexity
                    ),
                    SuggestionSource::Static,
                )
                .with_detail(
                    "This file has exceptionally high cyclomatic complexity. \
                     Break complex functions into smaller, focused units."
                        .to_string(),
                ),
            );
        }
    }

    // Only flag files with 50+ functions
    if function_count > 50 {
        suggestions.push(
            Suggestion::new(
                SuggestionKind::Improvement,
                Priority::High,
                path.clone(),
                format!("{} functions - split into modules", function_count),
                SuggestionSource::Static,
            )
            .with_detail(
                "This file has too many functions. Split into focused modules."
                    .to_string(),
            ),
        );
    }

    suggestions
}

/// Check individual function length - only flag very long functions
fn check_function_length(path: &PathBuf, symbol: &Symbol) -> Vec<Suggestion> {
    let mut suggestions = Vec::new();

    if !matches!(symbol.kind, SymbolKind::Function | SymbolKind::Method) {
        return suggestions;
    }

    let lines = symbol.line_count();

    // Only flag functions over 200 lines
    if lines > 200 {
        suggestions.push(
            Suggestion::new(
                SuggestionKind::Improvement,
                Priority::High,
                path.clone(),
                format!("`{}` is {} lines - needs refactoring", symbol.name, lines),
                SuggestionSource::Static,
            )
            .with_line(symbol.line)
            .with_detail(format!(
                "The function `{}` is {} lines. Extract logical sections \
                 into helper functions.",
                symbol.name, lines
            )),
        );
    }

    suggestions
}

/// Check function complexity - only flag very complex functions
fn check_function_complexity(path: &PathBuf, symbol: &Symbol) -> Vec<Suggestion> {
    let mut suggestions = Vec::new();

    if !matches!(symbol.kind, SymbolKind::Function | SymbolKind::Method) {
        return suggestions;
    }

    // Only flag very high complexity (30+)
    if symbol.complexity > 30.0 {
        suggestions.push(
            Suggestion::new(
                SuggestionKind::Improvement,
                Priority::High,
                path.clone(),
                format!(
                    "`{}` complexity is {:.0} - simplify",
                    symbol.name, symbol.complexity
                ),
                SuggestionSource::Static,
            )
            .with_line(symbol.line)
            .with_detail(format!(
                "The function `{}` has cyclomatic complexity of {:.0}. \
                 Use early returns and extract conditions.",
                symbol.name, symbol.complexity
            )),
        );
    }

    suggestions
}

/// Convert a detected pattern to a suggestion - only high priority items
fn pattern_to_suggestion(path: &PathBuf, pattern: &crate::index::Pattern) -> Option<Suggestion> {
    let (kind, priority, summary, detail) = match pattern.kind {
        // Skip long function - handled by check_function_length
        PatternKind::LongFunction => return None,
        
        // Deep nesting is important
        PatternKind::DeepNesting => (
            SuggestionKind::Improvement,
            Priority::High,
            "Deep nesting - flatten with early returns".to_string(),
            Some("Use early returns or extract nested logic.".to_string()),
        ),
        
        // Skip many parameters - not critical
        PatternKind::ManyParameters => return None,
        
        // God module is important
        PatternKind::GodModule => (
            SuggestionKind::Improvement,
            Priority::High,
            format!("Large module - {}", pattern.description),
            Some("Split into focused sub-modules.".to_string()),
        ),
        
        // Skip duplicate pattern - not critical enough
        PatternKind::DuplicatePattern => return None,
        
        // Missing error handling is important
        PatternKind::MissingErrorHandling => (
            SuggestionKind::BugFix,
            Priority::High,
            "Missing error handling".to_string(),
            Some("Add error handling to prevent runtime failures.".to_string()),
        ),
        
        // Skip unused imports - not critical
        PatternKind::UnusedImport => return None,
        
        // Only show FIXME/BUG markers
        PatternKind::TodoMarker => {
            let todo_text = &pattern.description;
            let upper = todo_text.to_uppercase();
            
            // Only show FIXME and BUG markers
            if upper.contains("FIXME") || upper.contains("BUG") {
                (
                    SuggestionKind::BugFix,
                    Priority::High,
                    format!("FIXME: {}", truncate(todo_text, 50)),
                    Some(format!("Found: {}", todo_text)),
                )
            } else {
                return None;
            }
        }
    };

    let mut suggestion = Suggestion::new(kind, priority, path.clone(), summary, SuggestionSource::Static)
        .with_line(pattern.line);

    if let Some(d) = detail {
        suggestion = suggestion.with_detail(d);
    }

    Some(suggestion)
}

/// Language-specific static checks - currently minimal, reserved for critical issues
fn language_specific_checks(_path: &PathBuf, _file_index: &FileIndex) -> Vec<Suggestion> {
    // Reserved for critical language-specific issues only
    // We don't surface low-priority items like missing tests
    Vec::new()
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
    use crate::index::{Symbol, SymbolKind, Visibility};

    #[test]
    fn test_function_length_check() {
        // Only functions over 200 lines are flagged now
        let symbol = Symbol {
            name: "very_long_function".to_string(),
            kind: SymbolKind::Function,
            file: PathBuf::from("test.rs"),
            line: 1,
            end_line: 250,
            complexity: 5.0,
            visibility: Visibility::Public,
        };

        let path = PathBuf::from("test.rs");
        let suggestions = check_function_length(&path, &symbol);

        assert!(!suggestions.is_empty());
        assert!(suggestions[0].priority == Priority::High);
    }

    #[test]
    fn test_moderate_function_not_flagged() {
        // Functions under 200 lines should not be flagged
        let symbol = Symbol {
            name: "moderate_function".to_string(),
            kind: SymbolKind::Function,
            file: PathBuf::from("test.rs"),
            line: 1,
            end_line: 100,
            complexity: 5.0,
            visibility: Visibility::Public,
        };

        let path = PathBuf::from("test.rs");
        let suggestions = check_function_length(&path, &symbol);

        assert!(suggestions.is_empty());
    }

    #[test]
    fn test_todo_not_shown() {
        // Regular TODOs should not be shown
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
}
