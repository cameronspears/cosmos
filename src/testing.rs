//! Test runner detection and execution
//!
//! Detects project type and runs appropriate tests.

use std::path::Path;
use std::process::{Command, Output};
use std::fs;

/// Result of running tests
#[derive(Debug, Clone)]
pub struct TestResult {
    pub passed: bool,
    pub output: String,
    pub duration_ms: u64,
    pub test_count: Option<usize>,
    pub failed_count: Option<usize>,
}

impl TestResult {
    pub fn success(output: String, duration_ms: u64) -> Self {
        Self {
            passed: true,
            output,
            duration_ms,
            test_count: None,
            failed_count: None,
        }
    }
    
    pub fn failure(output: String, duration_ms: u64) -> Self {
        Self {
            passed: false,
            output,
            duration_ms,
            test_count: None,
            failed_count: None,
        }
    }
}

/// Detected project type
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProjectType {
    Rust,
    Node,
    Python,
    Go,
    Unknown,
}

impl ProjectType {
    pub fn name(&self) -> &'static str {
        match self {
            ProjectType::Rust => "Rust",
            ProjectType::Node => "Node.js",
            ProjectType::Python => "Python",
            ProjectType::Go => "Go",
            ProjectType::Unknown => "Unknown",
        }
    }
}

/// Detect project type from files in directory
pub fn detect_project_type(repo_path: &Path) -> ProjectType {
    if repo_path.join("Cargo.toml").exists() {
        ProjectType::Rust
    } else if repo_path.join("package.json").exists() {
        ProjectType::Node
    } else if repo_path.join("pyproject.toml").exists() 
        || repo_path.join("setup.py").exists()
        || repo_path.join("requirements.txt").exists() {
        ProjectType::Python
    } else if repo_path.join("go.mod").exists() {
        ProjectType::Go
    } else {
        ProjectType::Unknown
    }
}

/// Get the test command for a project type
pub fn test_command(project_type: ProjectType) -> Option<(&'static str, Vec<&'static str>)> {
    match project_type {
        ProjectType::Rust => Some(("cargo", vec!["test"])),
        ProjectType::Node => Some(("npm", vec!["test"])),
        ProjectType::Python => Some(("pytest", vec![])),
        ProjectType::Go => Some(("go", vec!["test", "./..."])),
        ProjectType::Unknown => None,
    }
}

/// Detect test command for a specific file
pub fn test_command_for_file(repo_path: &Path, file_path: &str) -> Option<(String, Vec<String>)> {
    let project_type = detect_project_type(repo_path);
    
    match project_type {
        ProjectType::Rust => {
            // For Rust, we can run specific tests
            // Try to find the test module name from the file
            if file_path.ends_with(".rs") {
                let module = file_path
                    .trim_end_matches(".rs")
                    .replace('/', "::")
                    .replace("src::", "");
                Some(("cargo".to_string(), vec!["test".to_string(), module]))
            } else {
                Some(("cargo".to_string(), vec!["test".to_string()]))
            }
        }
        ProjectType::Node => {
            // Check for different test runners
            if let Ok(pkg_json) = fs::read_to_string(repo_path.join("package.json")) {
                if pkg_json.contains("vitest") {
                    return Some(("npx".to_string(), vec!["vitest".to_string(), "run".to_string(), file_path.to_string()]));
                } else if pkg_json.contains("jest") {
                    return Some(("npx".to_string(), vec!["jest".to_string(), file_path.to_string()]));
                }
            }
            Some(("npm".to_string(), vec!["test".to_string()]))
        }
        ProjectType::Python => {
            Some(("pytest".to_string(), vec![file_path.to_string(), "-v".to_string()]))
        }
        ProjectType::Go => {
            let dir = Path::new(file_path).parent()
                .map(|p| format!("./{}", p.display()))
                .unwrap_or_else(|| "./...".to_string());
            Some(("go".to_string(), vec!["test".to_string(), "-v".to_string(), dir]))
        }
        ProjectType::Unknown => None,
    }
}

/// Run tests for the project
pub fn run_tests(repo_path: &Path) -> TestResult {
    let project_type = detect_project_type(repo_path);
    
    if let Some((cmd, args)) = test_command(project_type) {
        run_test_command(repo_path, cmd, &args)
    } else {
        TestResult::failure("No test runner detected".to_string(), 0)
    }
}

/// Run tests for a specific file
pub fn run_tests_for_file(repo_path: &Path, file_path: &str) -> TestResult {
    if let Some((cmd, args)) = test_command_for_file(repo_path, file_path) {
        let args_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
        run_test_command(repo_path, &cmd, &args_refs)
    } else {
        TestResult::failure("No test runner detected".to_string(), 0)
    }
}

/// Run a test command and capture output
fn run_test_command(repo_path: &Path, cmd: &str, args: &[&str]) -> TestResult {
    let start = std::time::Instant::now();
    
    let output = Command::new(cmd)
        .current_dir(repo_path)
        .args(args)
        .output();
    
    let duration_ms = start.elapsed().as_millis() as u64;
    
    match output {
        Ok(out) => {
            let stdout = String::from_utf8_lossy(&out.stdout);
            let stderr = String::from_utf8_lossy(&out.stderr);
            let combined = format!("{}\n{}", stdout, stderr);
            
            if out.status.success() {
                TestResult::success(combined, duration_ms)
            } else {
                TestResult::failure(combined, duration_ms)
            }
        }
        Err(e) => TestResult::failure(format!("Failed to run {}: {}", cmd, e), duration_ms),
    }
}

/// Check if tests exist for a file
pub fn has_tests_for_file(repo_path: &Path, file_path: &str) -> bool {
    let project_type = detect_project_type(repo_path);
    
    match project_type {
        ProjectType::Rust => {
            // Check if file has #[test] annotations
            if let Ok(content) = fs::read_to_string(repo_path.join(file_path)) {
                content.contains("#[test]") || content.contains("#[cfg(test)]")
            } else {
                false
            }
        }
        ProjectType::Node => {
            // Check for corresponding .test.ts or .spec.ts
            let test_patterns = [
                file_path.replace(".ts", ".test.ts"),
                file_path.replace(".ts", ".spec.ts"),
                file_path.replace(".js", ".test.js"),
                file_path.replace(".js", ".spec.js"),
            ];
            test_patterns.iter().any(|p| repo_path.join(p).exists())
        }
        ProjectType::Python => {
            // Check for test_ prefix or _test suffix
            let test_path = format!("test_{}", file_path);
            let path_test = file_path.replace(".py", "_test.py");
            repo_path.join(&test_path).exists() || repo_path.join(&path_test).exists()
        }
        ProjectType::Go => {
            // Check for _test.go file
            let test_path = file_path.replace(".go", "_test.go");
            repo_path.join(&test_path).exists()
        }
        ProjectType::Unknown => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::env;

    #[test]
    fn test_detect_rust_project() {
        let repo_path = env::current_dir().unwrap();
        assert_eq!(detect_project_type(&repo_path), ProjectType::Rust);
    }

    #[test]
    fn test_rust_test_command() {
        let (cmd, args) = test_command(ProjectType::Rust).unwrap();
        assert_eq!(cmd, "cargo");
        assert_eq!(args, vec!["test"]);
    }
}

