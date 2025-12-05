//! Test coverage detection and correlation
//!
//! Identifies source files and their associated test files,
//! highlighting untested code especially in danger zones.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use walkdir::WalkDir;
use anyhow::Result;

/// Represents a source file and its test coverage status
#[derive(Debug, Clone)]
pub struct TestCoverage {
    pub path: String,
    pub has_tests: bool,
    pub test_files: Vec<String>,
    pub inline_tests: bool,
    pub test_line_count: usize,
    pub source_line_count: usize,
    /// Ratio of test lines to source lines
    pub test_ratio: f64,
}

/// Summary of test coverage across the codebase
#[derive(Debug, Clone, Default)]
pub struct TestSummary {
    pub total_source_files: usize,
    pub files_with_tests: usize,
    pub files_without_tests: usize,
    pub total_test_files: usize,
    pub coverage_pct: f64,
    /// Files that are both untested AND in danger zones
    pub untested_danger_zones: Vec<String>,
}

/// Analyzes test coverage across the codebase
pub struct TestAnalyzer {
    ignore_dirs: Vec<String>,
}

impl TestAnalyzer {
    pub fn new() -> Self {
        let ignore_dirs = vec![
            ".git".to_string(),
            "node_modules".to_string(),
            "target".to_string(),
            "vendor".to_string(),
            "dist".to_string(),
            "build".to_string(),
            ".next".to_string(),
            "__pycache__".to_string(),
            ".venv".to_string(),
            "venv".to_string(),
            ".codecosmos".to_string(),
        ];

        Self { ignore_dirs }
    }

    /// Analyze test coverage for all source files
    pub fn analyze(&self, root: &Path) -> Result<Vec<TestCoverage>> {
        // First pass: collect all files and categorize them
        let mut source_files: HashMap<String, (String, usize)> = HashMap::new(); // normalized name -> (path, lines)
        let mut test_files: HashMap<String, (String, usize)> = HashMap::new(); // normalized source name -> (test path, lines)
        let mut inline_test_files: HashSet<String> = HashSet::new();

        for entry in WalkDir::new(root)
            .follow_links(false)
            .into_iter()
            .filter_entry(|e| !self.should_ignore(e))
        {
            let entry = match entry {
                Ok(e) => e,
                Err(_) => continue,
            };

            if !entry.file_type().is_file() {
                continue;
            }

            let path = entry.path();
            let relative_path = path
                .strip_prefix(root)
                .unwrap_or(path)
                .to_string_lossy()
                .to_string();

            if !self.is_code_file(path) {
                continue;
            }

            let line_count = std::fs::read_to_string(path)
                .map(|c| c.lines().count())
                .unwrap_or(0);

            let file_name = path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("");

            let ext = path
                .extension()
                .and_then(|s| s.to_str())
                .unwrap_or("");

            // Detect test files by various conventions
            if self.is_test_file(&relative_path, file_name) {
                // Extract the source file name this test corresponds to
                let source_name = self.extract_source_name(file_name);
                test_files.insert(source_name, (relative_path.clone(), line_count));
            } else {
                // Check for inline tests (Rust #[cfg(test)], Python if __name__ == "__main__")
                let has_inline = self.has_inline_tests(path, ext);
                if has_inline {
                    inline_test_files.insert(relative_path.clone());
                }

                // Normalize the source file name for matching
                let normalized = self.normalize_source_name(file_name);
                source_files.insert(normalized, (relative_path, line_count));
            }
        }

        // Second pass: correlate source files with their tests
        let mut results: Vec<TestCoverage> = Vec::new();

        for (normalized_name, (source_path, source_lines)) in &source_files {
            let test_info = test_files.get(normalized_name);
            let has_inline = inline_test_files.contains(source_path);

            let (has_tests, test_file_list, test_lines) = if let Some((test_path, test_line_count)) = test_info {
                (true, vec![test_path.clone()], *test_line_count)
            } else if has_inline {
                (true, vec![], 0) // Inline tests, no separate file
            } else {
                (false, vec![], 0)
            };

            let test_ratio = if *source_lines > 0 && test_lines > 0 {
                test_lines as f64 / *source_lines as f64
            } else {
                0.0
            };

            results.push(TestCoverage {
                path: source_path.clone(),
                has_tests,
                test_files: test_file_list,
                inline_tests: has_inline,
                test_line_count: test_lines,
                source_line_count: *source_lines,
                test_ratio,
            });
        }

        // Sort: untested files first, then by source lines (largest untested first)
        results.sort_by(|a, b| {
            a.has_tests
                .cmp(&b.has_tests)
                .then_with(|| b.source_line_count.cmp(&a.source_line_count))
        });

        Ok(results)
    }

    /// Get summary statistics
    pub fn summarize(&self, coverages: &[TestCoverage], danger_zone_paths: &[String]) -> TestSummary {
        let total_source_files = coverages.len();
        let files_with_tests = coverages.iter().filter(|c| c.has_tests).count();
        let files_without_tests = total_source_files - files_with_tests;

        let coverage_pct = if total_source_files > 0 {
            (files_with_tests as f64 / total_source_files as f64) * 100.0
        } else {
            0.0
        };

        // Find untested files that are also in danger zones
        let danger_set: HashSet<&str> = danger_zone_paths.iter().map(|s| s.as_str()).collect();
        let untested_danger_zones: Vec<String> = coverages
            .iter()
            .filter(|c| !c.has_tests && danger_set.contains(c.path.as_str()))
            .map(|c| c.path.clone())
            .collect();

        // Count unique test files
        let total_test_files: HashSet<&str> = coverages
            .iter()
            .flat_map(|c| c.test_files.iter().map(|s| s.as_str()))
            .collect();

        TestSummary {
            total_source_files,
            files_with_tests,
            files_without_tests,
            total_test_files: total_test_files.len(),
            coverage_pct,
            untested_danger_zones,
        }
    }

    /// Check if a file is a test file based on naming conventions
    fn is_test_file(&self, path: &str, file_name: &str) -> bool {
        let path_lower = path.to_lowercase();
        let name_lower = file_name.to_lowercase();

        // Directory-based conventions (check both /dir/ and start with dir/)
        if path_lower.contains("/test/")
            || path_lower.contains("/tests/")
            || path_lower.contains("/__tests__/")
            || path_lower.contains("/spec/")
            || path_lower.contains("/__mocks__/")
            || path_lower.starts_with("test/")
            || path_lower.starts_with("tests/")
            || path_lower.starts_with("spec/")
        {
            return true;
        }

        // File naming conventions
        name_lower.starts_with("test_")           // Python: test_foo.py
            || name_lower.ends_with("_test")      // Go: foo_test.go, Rust: foo_test.rs
            || name_lower.ends_with(".test")      // JS: foo.test.js
            || name_lower.ends_with(".spec")      // JS: foo.spec.ts
            || name_lower.ends_with("_spec")      // Ruby: foo_spec.rb
            || name_lower.starts_with("spec_")    // Ruby: spec_foo.rb
            || name_lower.ends_with("tests")      // Rust: some_tests.rs
    }

    /// Extract the source file name from a test file name
    fn extract_source_name(&self, test_name: &str) -> String {
        let name = test_name.to_lowercase();

        // Remove common test prefixes/suffixes
        let stripped = name
            .strip_prefix("test_")
            .or_else(|| name.strip_suffix("_test"))
            .or_else(|| name.strip_suffix(".test"))
            .or_else(|| name.strip_suffix(".spec"))
            .or_else(|| name.strip_suffix("_spec"))
            .or_else(|| name.strip_prefix("spec_"))
            .or_else(|| name.strip_suffix("tests"))
            .unwrap_or(&name);

        stripped.to_string()
    }

    /// Normalize a source file name for matching with tests
    fn normalize_source_name(&self, name: &str) -> String {
        name.to_lowercase()
    }

    /// Check if a file has inline tests
    fn has_inline_tests(&self, path: &Path, ext: &str) -> bool {
        let content = match std::fs::read_to_string(path) {
            Ok(c) => c,
            Err(_) => return false,
        };

        match ext {
            "rs" => content.contains("#[cfg(test)]") || content.contains("#[test]"),
            "py" => content.contains("if __name__") || content.contains("unittest") || content.contains("pytest"),
            "go" => content.contains("func Test"),
            "ex" | "exs" => content.contains("defmodule") && content.contains("test \""),
            _ => false,
        }
    }

    fn should_ignore(&self, entry: &walkdir::DirEntry) -> bool {
        entry
            .file_name()
            .to_str()
            .map(|name| self.ignore_dirs.contains(&name.to_string()) || name.starts_with('.'))
            .unwrap_or(false)
    }

    fn is_code_file(&self, path: &Path) -> bool {
        let code_extensions = [
            "rs", "js", "ts", "tsx", "jsx", "py", "rb", "go", "java", "kt", "scala", "c", "cpp",
            "h", "hpp", "cs", "swift", "m", "mm", "php", "pl", "pm", "ex", "exs", "hs", "elm",
            "clj", "lua", "r", "jl", "nim", "zig", "vue", "svelte",
        ];

        path.extension()
            .and_then(|e| e.to_str())
            .map(|ext| code_extensions.contains(&ext.to_lowercase().as_str()))
            .unwrap_or(false)
    }
}

impl Default for TestAnalyzer {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_test_file() {
        let analyzer = TestAnalyzer::new();

        assert!(analyzer.is_test_file("src/test_foo.py", "test_foo"));
        assert!(analyzer.is_test_file("src/foo_test.go", "foo_test"));
        assert!(analyzer.is_test_file("src/foo.test.js", "foo.test"));
        assert!(analyzer.is_test_file("src/__tests__/foo.js", "foo"));
        assert!(analyzer.is_test_file("tests/foo.rs", "foo")); // tests/ directory at root

        assert!(!analyzer.is_test_file("src/foo.rs", "foo"));
        assert!(!analyzer.is_test_file("src/testing.rs", "testing"));
    }

    #[test]
    fn test_extract_source_name() {
        let analyzer = TestAnalyzer::new();

        assert_eq!(analyzer.extract_source_name("test_foo"), "foo");
        assert_eq!(analyzer.extract_source_name("foo_test"), "foo");
        assert_eq!(analyzer.extract_source_name("foo.test"), "foo");
        assert_eq!(analyzer.extract_source_name("foo.spec"), "foo");
    }
}

