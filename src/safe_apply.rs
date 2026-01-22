//! Safe Apply checks
//!
//! Runs a small set of fast, local checks after Cosmos applies a change.
//! The goal is confidence (and a clear report), not exhaustive CI.

use std::path::Path;
use std::process::Command;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckStatus {
    Pass,
    Fail,
    Skipped,
}

impl CheckStatus {
    pub fn icon(&self) -> &'static str {
        match self {
            CheckStatus::Pass => "+",
            CheckStatus::Fail => "●",
            CheckStatus::Skipped => "·",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckResult {
    pub name: String,
    pub status: CheckStatus,
    /// Combined stdout/stderr (truncated)
    pub output: String,
}

/// Run a pragmatic set of checks for the repo.
///
/// Always includes `git diff --check`.
/// Adds language-specific checks when the toolchain is detected.
pub fn run(repo_path: &Path) -> Vec<CheckResult> {
    let mut results = Vec::new();

    results.push(run_cmd(repo_path, "git diff --check", "git", &["diff", "--check"]));

    // Rust
    if repo_path.join("Cargo.toml").exists() {
        results.push(run_cmd(repo_path, "cargo fmt --check", "cargo", &["fmt", "--", "--check"]));
        results.push(run_cmd(repo_path, "cargo check", "cargo", &["check", "-q"]));
    }

    // Go
    if repo_path.join("go.mod").exists() {
        results.push(run_cmd(repo_path, "go test ./...", "go", &["test", "./..."]));
    }

    // Python (very lightweight)
    if repo_path.join("pyproject.toml").exists() || repo_path.join("setup.py").exists() {
        results.push(run_cmd(repo_path, "python -m compileall .", "python", &["-m", "compileall", "."]));
    }

    results
}

fn run_cmd(repo_path: &Path, name: &str, bin: &str, args: &[&str]) -> CheckResult {
    let output = Command::new(bin)
        .current_dir(repo_path)
        .args(args)
        .output();

    match output {
        Ok(out) => {
            let mut combined = String::new();
            if !out.stdout.is_empty() {
                combined.push_str(&String::from_utf8_lossy(&out.stdout));
            }
            if !out.stderr.is_empty() {
                if !combined.is_empty() {
                    combined.push_str("\n");
                }
                combined.push_str(&String::from_utf8_lossy(&out.stderr));
            }

            let status = if out.status.success() {
                CheckStatus::Pass
            } else {
                CheckStatus::Fail
            };

            CheckResult {
                name: name.to_string(),
                status,
                output: truncate_output(&combined, 1800),
            }
        }
        Err(e) => CheckResult {
            name: name.to_string(),
            status: CheckStatus::Skipped,
            output: format!("Skipped: {}", e),
        },
    }
}

fn truncate_output(s: &str, max: usize) -> String {
    let trimmed = s.trim();
    let char_count = trimmed.chars().count();
    if char_count <= max {
        trimmed.to_string()
    } else {
        let snippet: String = trimmed.chars().take(max).collect();
        format!("{}\n… (truncated)", snippet)
    }
}

#[cfg(test)]
mod tests {
    use super::truncate_output;

    #[test]
    fn test_truncate_output_unicode_safe() {
        let input = "错误: 失败 😊";
        let out = truncate_output(input, 5);
        assert_eq!(out, "错误: 失\n… (truncated)");
    }

    #[test]
    fn test_truncate_output_no_truncation() {
        let input = "ok";
        assert_eq!(truncate_output(input, 10), "ok");
    }
}

