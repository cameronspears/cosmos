use anyhow::Result;
use regex::Regex;
use std::fs;
use std::path::Path;
use walkdir::WalkDir;

use super::ChurnEntry;

/// Complexity metrics for a single file
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct FileComplexity {
    pub path: String,
    pub loc: usize,
    pub function_count: usize,
    pub max_function_length: usize,
    pub avg_function_length: f64,
    /// Simple complexity score: weighted combination of metrics
    pub complexity_score: f64,
}

/// A file that is both high-churn AND high-complexity (danger zone)
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct DangerZone {
    pub path: String,
    pub churn_rank: usize,
    pub complexity_rank: usize,
    pub change_count: usize,
    pub complexity_score: f64,
    /// Combined danger score (lower rank = higher danger)
    pub danger_score: f64,
    /// Human-readable reason why this is dangerous
    pub reason: String,
}

/// Analyzes code complexity across the repository
pub struct ComplexityAnalyzer {
    ignore_dirs: Vec<String>,
    /// Regex patterns for function detection in various languages
    function_patterns: Vec<Regex>,
}

impl ComplexityAnalyzer {
    pub fn new() -> Self {
        // Patterns to detect function/method definitions
        let function_patterns = vec![
            // Rust: fn name(
            Regex::new(r"^\s*(?:pub\s+)?(?:async\s+)?fn\s+\w+").unwrap(),
            // JavaScript/TypeScript: function name(, const name = (, =>
            Regex::new(r"^\s*(?:export\s+)?(?:async\s+)?function\s+\w+").unwrap(),
            Regex::new(r"^\s*(?:export\s+)?(?:const|let|var)\s+\w+\s*=\s*(?:async\s+)?\(").unwrap(),
            Regex::new(r"^\s*(?:export\s+)?(?:const|let|var)\s+\w+\s*=\s*(?:async\s+)?\w+\s*=>")
                .unwrap(),
            // Python: def name(
            Regex::new(r"^\s*(?:async\s+)?def\s+\w+").unwrap(),
            // Go: func name(, func (receiver) name(
            Regex::new(r"^\s*func\s+(?:\(\w+\s+\*?\w+\)\s+)?\w+").unwrap(),
            // Java/C#/Kotlin: public/private/protected type name(
            Regex::new(r"^\s*(?:public|private|protected|internal)?\s*(?:static\s+)?(?:async\s+)?(?:override\s+)?(?:virtual\s+)?(?:\w+\s+)+\w+\s*\(").unwrap(),
            // Ruby: def name
            Regex::new(r"^\s*def\s+\w+").unwrap(),
            // PHP: function name(
            Regex::new(r"^\s*(?:public|private|protected)?\s*(?:static\s+)?function\s+\w+").unwrap(),
            // C/C++: type name( - simplified
            Regex::new(r"^\s*(?:\w+\s+)+\w+\s*\([^;]*\)\s*\{?\s*$").unwrap(),
        ];

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

        Self {
            ignore_dirs,
            function_patterns,
        }
    }

    /// Analyze complexity for all code files in the repository
    pub fn analyze(&self, root: &Path) -> Result<Vec<FileComplexity>> {
        let mut results = Vec::new();

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
            if !self.is_code_file(path) {
                continue;
            }

            if let Ok(complexity) = self.analyze_file(path, root) {
                results.push(complexity);
            }
        }

        // Sort by complexity score descending
        results.sort_by(|a, b| {
            b.complexity_score
                .partial_cmp(&a.complexity_score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        Ok(results)
    }

    /// Analyze a single file for complexity metrics
    fn analyze_file(&self, path: &Path, root: &Path) -> Result<FileComplexity> {
        let content = fs::read_to_string(path)?;
        let relative_path = path
            .strip_prefix(root)
            .unwrap_or(path)
            .to_string_lossy()
            .to_string();

        let lines: Vec<&str> = content.lines().collect();
        let loc = lines
            .iter()
            .filter(|line| {
                let trimmed = line.trim();
                !trimmed.is_empty() && !trimmed.starts_with("//") && !trimmed.starts_with('#')
            })
            .count();

        // Find functions and their lengths
        let mut function_lengths = Vec::new();
        let mut in_function = false;
        let mut current_function_start = 0;
        let mut brace_depth = 0;

        for (i, line) in lines.iter().enumerate() {
            // Check for function start
            if !in_function && self.is_function_start(line) {
                in_function = true;
                current_function_start = i;
                brace_depth = 0;
            }

            if in_function {
                // Count braces (simplified - doesn't handle strings/comments perfectly)
                for ch in line.chars() {
                    match ch {
                        '{' => brace_depth += 1,
                        '}' => {
                            brace_depth -= 1;
                            if brace_depth <= 0 {
                                function_lengths.push(i - current_function_start + 1);
                                in_function = false;
                                break;
                            }
                        }
                        _ => {}
                    }
                }
            }
        }

        // For Python/Ruby (indentation-based), use a simpler heuristic
        if function_lengths.is_empty() && (path.extension().map_or(false, |e| e == "py" || e == "rb"))
        {
            function_lengths = self.estimate_function_lengths_by_indent(&lines);
        }

        let function_count = function_lengths.len().max(1);
        let max_function_length = function_lengths.iter().copied().max().unwrap_or(0);
        let avg_function_length = if function_lengths.is_empty() {
            0.0
        } else {
            function_lengths.iter().sum::<usize>() as f64 / function_lengths.len() as f64
        };

        // Calculate complexity score
        // Factors: LOC, max function length, number of functions
        let complexity_score = self.calculate_complexity_score(loc, max_function_length, function_count);

        Ok(FileComplexity {
            path: relative_path,
            loc,
            function_count,
            max_function_length,
            avg_function_length,
            complexity_score,
        })
    }

    fn is_function_start(&self, line: &str) -> bool {
        self.function_patterns.iter().any(|p| p.is_match(line))
    }

    fn estimate_function_lengths_by_indent(&self, lines: &[&str]) -> Vec<usize> {
        let mut function_lengths = Vec::new();
        let mut current_function_start: Option<usize> = None;
        let def_pattern = Regex::new(r"^\s*(?:async\s+)?def\s+\w+").unwrap();

        for (i, line) in lines.iter().enumerate() {
            if def_pattern.is_match(line) {
                // End previous function
                if let Some(start) = current_function_start {
                    function_lengths.push(i - start);
                }
                current_function_start = Some(i);
            }
        }

        // End last function
        if let Some(start) = current_function_start {
            function_lengths.push(lines.len() - start);
        }

        function_lengths
    }

    fn calculate_complexity_score(&self, loc: usize, max_function_length: usize, function_count: usize) -> f64 {
        // Scoring formula:
        // - Base: LOC / 100 (larger files = higher complexity)
        // - Max function penalty: max_function_length / 50 (long functions = bad)
        // - Function density bonus/penalty: (loc / function_count) / 100 if >50 lines per function
        let base_score = loc as f64 / 100.0;
        let function_penalty = max_function_length as f64 / 50.0;
        
        let density_factor = if function_count > 0 {
            let lines_per_function = loc as f64 / function_count as f64;
            if lines_per_function > 50.0 {
                (lines_per_function - 50.0) / 100.0
            } else {
                0.0
            }
        } else {
            loc as f64 / 200.0 // No functions detected = assume it's one big block
        };

        base_score + function_penalty + density_factor
    }

    /// Find danger zones: files that are both high-churn AND high-complexity
    pub fn find_danger_zones(
        &self,
        churn_entries: &[ChurnEntry],
        complexity_entries: &[FileComplexity],
        top_n: usize,
    ) -> Vec<DangerZone> {
        // Create lookup maps for rankings
        let churn_ranks: std::collections::HashMap<&str, (usize, usize)> = churn_entries
            .iter()
            .enumerate()
            .map(|(rank, entry)| (entry.path.as_str(), (rank + 1, entry.change_count)))
            .collect();

        let complexity_ranks: std::collections::HashMap<&str, (usize, f64)> = complexity_entries
            .iter()
            .enumerate()
            .map(|(rank, entry)| (entry.path.as_str(), (rank + 1, entry.complexity_score)))
            .collect();

        // Find files that appear in both lists
        let mut danger_zones: Vec<DangerZone> = churn_entries
            .iter()
            .filter_map(|churn| {
                let complexity_info = complexity_ranks.get(churn.path.as_str())?;
                let churn_info = churn_ranks.get(churn.path.as_str())?;

                // Only consider files in top 50% of both lists
                let max_churn_rank = churn_entries.len() / 2 + 1;
                let max_complexity_rank = complexity_entries.len() / 2 + 1;

                if churn_info.0 > max_churn_rank || complexity_info.0 > max_complexity_rank {
                    return None;
                }

                // Danger score: product of inverse ranks (lower = more dangerous)
                // Normalized to 0-100 scale
                let danger_score = 100.0
                    - ((churn_info.0 as f64 * complexity_info.0 as f64).sqrt()
                        / (max_churn_rank as f64 * max_complexity_rank as f64).sqrt()
                        * 100.0);

                // Generate actionable reason
                let reason = Self::generate_reason(churn_info.1, complexity_info.1);

                Some(DangerZone {
                    path: churn.path.clone(),
                    churn_rank: churn_info.0,
                    complexity_rank: complexity_info.0,
                    change_count: churn_info.1,
                    complexity_score: complexity_info.1,
                    danger_score,
                    reason,
                })
            })
            .collect();

        // Sort by danger score descending (highest danger first)
        danger_zones.sort_by(|a, b| {
            b.danger_score
                .partial_cmp(&a.danger_score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        danger_zones.truncate(top_n);
        danger_zones
    }

    /// Generate a human-readable reason for why a file is in a danger zone
    fn generate_reason(change_count: usize, complexity_score: f64) -> String {
        let churn_level = if change_count >= 15 {
            "very high churn"
        } else if change_count >= 8 {
            "high churn"
        } else {
            "moderate churn"
        };

        let complexity_level = if complexity_score >= 15.0 {
            "very complex"
        } else if complexity_score >= 8.0 {
            "complex"
        } else {
            "moderately complex"
        };

        let advice = if complexity_score >= 15.0 && change_count >= 10 {
            "split into smaller modules"
        } else if complexity_score >= 10.0 {
            "refactor long functions"
        } else if change_count >= 12 {
            "add test coverage"
        } else {
            "review and simplify"
        };

        format!("{} + {} -> {}", churn_level, complexity_level, advice)
    }

    /// Get aggregate complexity stats
    pub fn aggregate_stats(&self, entries: &[FileComplexity]) -> (usize, f64, f64) {
        if entries.is_empty() {
            return (0, 0.0, 0.0);
        }

        let total_loc: usize = entries.iter().map(|e| e.loc).sum();
        let avg_complexity =
            entries.iter().map(|e| e.complexity_score).sum::<f64>() / entries.len() as f64;
        let max_complexity = entries
            .iter()
            .map(|e| e.complexity_score)
            .fold(0.0, f64::max);

        (total_loc, avg_complexity, max_complexity)
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
            "h", "hpp", "cs", "swift", "m", "mm", "php", "pl", "pm", "sh", "bash", "ex", "exs",
            "hs", "elm", "clj", "lua", "r", "jl", "nim", "zig", "vue", "svelte",
        ];

        path.extension()
            .and_then(|e| e.to_str())
            .map(|ext| code_extensions.contains(&ext.to_lowercase().as_str()))
            .unwrap_or(false)
    }
}

impl Default for ComplexityAnalyzer {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_complexity_score_calculation() {
        let analyzer = ComplexityAnalyzer::new();

        // Small file, small functions
        let score1 = analyzer.calculate_complexity_score(100, 20, 5);
        // Large file, large functions
        let score2 = analyzer.calculate_complexity_score(1000, 200, 10);

        assert!(score2 > score1, "Larger file should have higher complexity");
    }
}

