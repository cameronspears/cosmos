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
                format!(
                    "This file has grown pretty big - might be time to break it up ({} lines)",
                    file_index.loc
                ),
                SuggestionSource::Static,
            )
            .with_detail(format!(
                "At {} lines, this file is getting hard to navigate. \
                 Splitting related pieces into their own files will make it easier to work with.",
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
                        "This code is getting tangled - could use some untangling (complexity: {:.0})",
                        avg_complexity
                    ),
                    SuggestionSource::Static,
                )
                .with_detail(
                    "There's a lot of branching logic here, which can make it tricky to debug. \
                     Breaking things into smaller functions will help."
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
                format!(
                    "This file is doing a lot - might be time to split it up ({} functions)",
                    function_count
                ),
                SuggestionSource::Static,
            )
            .with_detail(
                "With this many functions in one place, it's getting hard to find things. \
                 Grouping related functions into separate files will help."
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
                format!(
                    "`{}` is doing a lot - consider breaking it into smaller pieces ({} lines)",
                    symbol.name, lines
                ),
                SuggestionSource::Static,
            )
            .with_line(symbol.line)
            .with_detail(format!(
                "At {} lines, `{}` is pretty long. Pulling out logical chunks \
                 into their own functions will make it easier to understand and test.",
                lines, symbol.name
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
                    "`{}` has gotten pretty tangled - could use some untangling (complexity: {:.0})",
                    symbol.name, symbol.complexity
                ),
                SuggestionSource::Static,
            )
            .with_line(symbol.line)
            .with_detail(format!(
                "`{}` has a lot of if/else branches and loops, making it tricky to follow. \
                 Try returning early or extracting some conditions into helper functions.",
                symbol.name
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
            "This code is nested pretty deep - early returns could help".to_string(),
            Some("Deeply nested code can be hard to follow. Try returning early for error cases, \
                  or pull some of the inner logic into its own function.".to_string()),
        ),
        
        // Skip many parameters - not critical
        PatternKind::ManyParameters => return None,
        
        // God module is important
        PatternKind::GodModule => (
            SuggestionKind::Improvement,
            Priority::High,
            format!("This module is doing a lot - {}", pattern.description),
            Some("When a module handles too many things, it gets hard to work with. \
                  Try splitting it into smaller, focused pieces.".to_string()),
        ),
        
        // Skip duplicate pattern - not critical enough
        PatternKind::DuplicatePattern => return None,
        
        // Missing error handling is important
        PatternKind::MissingErrorHandling => (
            SuggestionKind::BugFix,
            Priority::High,
            "This could blow up if something goes wrong - add error handling".to_string(),
            Some("Right now, if something unexpected happens here, it might crash or behave strangely. \
                  Adding some error handling will make it more robust.".to_string()),
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
                    format!("Someone flagged this as needing attention: {}", truncate(todo_text, 50)),
                    Some(format!("Found a note in the code: {}", todo_text)),
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
