//! Unified diff parsing and application
//!
//! Handles parsing unified diff format and applying patches to files.

use std::fs;
use std::path::Path;

/// A single line in a diff hunk
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiffLine {
    Context(String),
    Add(String),
    Remove(String),
}

impl DiffLine {
    #[allow(dead_code)]
    pub fn content(&self) -> &str {
        match self {
            DiffLine::Context(s) => s,
            DiffLine::Add(s) => s,
            DiffLine::Remove(s) => s,
        }
    }
}

/// A hunk in a unified diff
#[derive(Debug, Clone, PartialEq)]
pub struct DiffHunk {
    pub old_start: usize,
    pub old_count: usize,
    pub new_start: usize,
    pub new_count: usize,
    pub lines: Vec<DiffLine>,
}

impl DiffHunk {
    /// Get a summary of changes in this hunk
    #[allow(dead_code)]
    pub fn summary(&self) -> (usize, usize) {
        let adds = self.lines.iter().filter(|l| matches!(l, DiffLine::Add(_))).count();
        let removes = self.lines.iter().filter(|l| matches!(l, DiffLine::Remove(_))).count();
        (adds, removes)
    }
}

/// A parsed unified diff
#[derive(Debug, Clone, PartialEq)]
pub struct UnifiedDiff {
    pub old_path: String,
    pub new_path: String,
    pub hunks: Vec<DiffHunk>,
}

impl UnifiedDiff {
    /// Get total additions and deletions
    #[allow(dead_code)]
    pub fn stats(&self) -> (usize, usize) {
        self.hunks.iter().fold((0, 0), |acc, h| {
            let (a, r) = h.summary();
            (acc.0 + a, acc.1 + r)
        })
    }
}

/// Parse a unified diff string into structured data
pub fn parse_unified_diff(diff: &str) -> Result<UnifiedDiff, String> {
    let lines: Vec<&str> = diff.lines().collect();
    
    if lines.len() < 3 {
        return Err("Diff too short".to_string());
    }

    // Find --- and +++ lines
    let mut old_path = String::new();
    let mut new_path = String::new();
    let mut start_idx = 0;
    
    for (i, line) in lines.iter().enumerate() {
        if line.starts_with("--- ") {
            old_path = line[4..].trim_start_matches("a/").to_string();
            // Handle timestamp suffix
            if let Some(tab_pos) = old_path.find('\t') {
                old_path = old_path[..tab_pos].to_string();
            }
        } else if line.starts_with("+++ ") {
            new_path = line[4..].trim_start_matches("b/").to_string();
            if let Some(tab_pos) = new_path.find('\t') {
                new_path = new_path[..tab_pos].to_string();
            }
            start_idx = i + 1;
            break;
        }
    }

    if old_path.is_empty() || new_path.is_empty() {
        return Err("Could not find file paths in diff".to_string());
    }

    let mut hunks = Vec::new();
    let mut i = start_idx;
    
    while i < lines.len() {
        let line = lines[i];
        
        // Parse hunk header: @@ -start,count +start,count @@
        if line.starts_with("@@ ") {
            let hunk = parse_hunk(&lines, &mut i)?;
            hunks.push(hunk);
        } else {
            i += 1;
        }
    }

    if hunks.is_empty() {
        return Err("No hunks found in diff".to_string());
    }

    Ok(UnifiedDiff {
        old_path,
        new_path,
        hunks,
    })
}

/// Parse a single hunk from the diff
fn parse_hunk(lines: &[&str], idx: &mut usize) -> Result<DiffHunk, String> {
    let header = lines[*idx];
    
    // Parse @@ -old_start,old_count +new_start,new_count @@
    let parts: Vec<&str> = header.split_whitespace().collect();
    if parts.len() < 4 || parts[0] != "@@" {
        return Err(format!("Invalid hunk header: {}", header));
    }

    let (old_start, old_count) = parse_range(parts[1].trim_start_matches('-'))?;
    let (new_start, new_count) = parse_range(parts[2].trim_start_matches('+'))?;

    *idx += 1;
    let mut diff_lines = Vec::new();

    while *idx < lines.len() {
        let line = lines[*idx];
        
        // Stop at next hunk or end
        if line.starts_with("@@ ") || line.starts_with("diff ") {
            break;
        }

        if line.starts_with('+') && !line.starts_with("+++") {
            diff_lines.push(DiffLine::Add(line[1..].to_string()));
        } else if line.starts_with('-') && !line.starts_with("---") {
            diff_lines.push(DiffLine::Remove(line[1..].to_string()));
        } else if line.starts_with(' ') || line.is_empty() {
            let content = if line.is_empty() { "" } else { &line[1..] };
            diff_lines.push(DiffLine::Context(content.to_string()));
        }
        // Skip other lines (like "\ No newline at end of file")

        *idx += 1;
    }

    Ok(DiffHunk {
        old_start,
        old_count,
        new_start,
        new_count,
        lines: diff_lines,
    })
}

/// Parse a range like "10,5" or "10" into (start, count)
fn parse_range(s: &str) -> Result<(usize, usize), String> {
    if let Some(comma) = s.find(',') {
        let start: usize = s[..comma].parse().map_err(|_| format!("Invalid start: {}", s))?;
        let count: usize = s[comma + 1..].parse().map_err(|_| format!("Invalid count: {}", s))?;
        Ok((start, count))
    } else {
        let start: usize = s.parse().map_err(|_| format!("Invalid line number: {}", s))?;
        Ok((start, 1))
    }
}

/// Apply a unified diff to the original content
pub fn apply_diff(original: &str, diff: &UnifiedDiff) -> Result<String, String> {
    let mut lines: Vec<String> = original.lines().map(|s| s.to_string()).collect();
    
    // Apply hunks in reverse order so line numbers don't shift
    for hunk in diff.hunks.iter().rev() {
        lines = apply_hunk(lines, hunk)?;
    }

    Ok(lines.join("\n"))
}

/// Apply a single hunk to the lines
fn apply_hunk(mut lines: Vec<String>, hunk: &DiffHunk) -> Result<Vec<String>, String> {
    let start = hunk.old_start.saturating_sub(1); // Convert to 0-indexed
    
    // Collect new lines for this section
    let mut new_section = Vec::new();
    
    for diff_line in &hunk.lines {
        match diff_line {
            DiffLine::Context(s) | DiffLine::Add(s) => {
                new_section.push(s.clone());
            }
            DiffLine::Remove(_) => {
                // Skip removed lines
            }
        }
    }

    // Calculate how many original lines to remove
    let remove_count = hunk.lines.iter()
        .filter(|l| matches!(l, DiffLine::Context(_) | DiffLine::Remove(_)))
        .count();

    // Replace the section
    let end = (start + remove_count).min(lines.len());
    lines.splice(start..end, new_section);

    Ok(lines)
}

/// Apply a diff to a file on disk
pub fn apply_diff_to_file(file_path: &Path, diff: &UnifiedDiff) -> Result<(), String> {
    let original = fs::read_to_string(file_path)
        .map_err(|e| format!("Failed to read file: {}", e))?;
    
    let patched = apply_diff(&original, diff)?;
    
    fs::write(file_path, patched)
        .map_err(|e| format!("Failed to write file: {}", e))?;
    
    Ok(())
}

/// Create a backup of a file before patching
pub fn backup_file(file_path: &Path) -> Result<std::path::PathBuf, String> {
    let backup_path = file_path.with_extension("orig");
    fs::copy(file_path, &backup_path)
        .map_err(|e| format!("Failed to create backup: {}", e))?;
    Ok(backup_path)
}

/// Restore a file from backup
pub fn restore_backup(file_path: &Path) -> Result<(), String> {
    let backup_path = file_path.with_extension("orig");
    if backup_path.exists() {
        fs::copy(&backup_path, file_path)
            .map_err(|e| format!("Failed to restore backup: {}", e))?;
        fs::remove_file(&backup_path)
            .map_err(|e| format!("Failed to remove backup: {}", e))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_simple_diff() {
        let diff = r#"--- a/src/example.ts
+++ b/src/example.ts
@@ -1,5 +1,6 @@
 function hello() {
-  console.log("old");
+  console.log("new");
+  console.log("extra");
   return true;
 }
"#;
        let parsed = parse_unified_diff(diff).unwrap();
        assert_eq!(parsed.old_path, "src/example.ts");
        assert_eq!(parsed.hunks.len(), 1);
        assert_eq!(parsed.stats(), (2, 1)); // 2 adds, 1 remove
    }

    #[test]
    fn test_apply_diff() {
        let original = r#"function hello() {
  console.log("old");
  return true;
}"#;
        let diff = r#"--- a/test.ts
+++ b/test.ts
@@ -1,4 +1,5 @@
 function hello() {
-  console.log("old");
+  console.log("new");
+  console.log("extra");
   return true;
 }
"#;
        let parsed = parse_unified_diff(diff).unwrap();
        let result = apply_diff(original, &parsed).unwrap();
        
        assert!(result.contains("console.log(\"new\")"));
        assert!(result.contains("console.log(\"extra\")"));
        assert!(!result.contains("console.log(\"old\")"));
    }
}



