//! Agentic tool definitions and execution for LLM-driven code exploration.
//!
//! Philosophy: Support top-down exploration that's naturally token-efficient.
//! Specialized tools enforce efficient patterns; shell is fallback for edge cases.

use crate::util::{resolve_repo_path_allow_new, run_command_with_timeout};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

/// Timeout for shell and search commands (prevents hangs)
const TOOL_TIMEOUT: Duration = Duration::from_secs(10);

// ═══════════════════════════════════════════════════════════════════════════
//  TOOL DEFINITIONS
// ═══════════════════════════════════════════════════════════════════════════

/// Tool definitions for the LLM
#[derive(Debug, Clone, Serialize)]
pub struct ToolDefinition {
    #[serde(rename = "type")]
    pub tool_type: &'static str,
    pub function: FunctionDefinition,
}

#[derive(Debug, Clone, Serialize)]
pub struct FunctionDefinition {
    pub name: &'static str,
    pub description: &'static str,
    pub parameters: serde_json::Value,
}

/// A tool call from the model
#[derive(Debug, Clone, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub function: FunctionCall,
}

#[derive(Debug, Clone, Deserialize)]
pub struct FunctionCall {
    pub name: String,
    pub arguments: String, // JSON string
}

/// Result of executing a tool
#[derive(Debug, Clone, Serialize)]
pub struct ToolResult {
    pub tool_call_id: String,
    pub content: String,
}

/// Get all available tool definitions for top-down exploration
pub fn get_tool_definitions() -> Vec<ToolDefinition> {
    vec![
        // 1. TREE - Understand structure first
        ToolDefinition {
            tool_type: "function",
            function: FunctionDefinition {
                name: "tree",
                description: "List directory structure. Start here to understand the codebase layout.",
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "Directory path (default: repo root)"
                        },
                        "depth": {
                            "type": "integer",
                            "description": "Max depth to traverse (default: 3)"
                        }
                    }
                }),
            },
        },
        // 2. HEAD - See file structure (imports, exports, top-level)
        ToolDefinition {
            tool_type: "function",
            function: FunctionDefinition {
                name: "head",
                description: "Read first N lines of a file. Use to see imports, exports, and structure before diving deeper.",
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "File path to read"
                        },
                        "lines": {
                            "type": "integer",
                            "description": "Number of lines (default: 50)"
                        }
                    },
                    "required": ["path"]
                }),
            },
        },
        // 3. SEARCH - Find patterns with line numbers
        ToolDefinition {
            tool_type: "function",
            function: FunctionDefinition {
                name: "search",
                description: "Search for pattern in files. Returns matches with line numbers. Use to find where to look before reading.",
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "pattern": {
                            "type": "string",
                            "description": "Regex pattern to search for"
                        },
                        "path": {
                            "type": "string",
                            "description": "File or directory to search (default: repo root)"
                        },
                        "context": {
                            "type": "integer",
                            "description": "Lines of context around matches (default: 2)"
                        }
                    },
                    "required": ["pattern"]
                }),
            },
        },
        // 4. READ_RANGE - Drill into specific sections
        ToolDefinition {
            tool_type: "function",
            function: FunctionDefinition {
                name: "read_range",
                description: "Read specific line range from a file. Use after search to examine specific sections.",
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "File path to read"
                        },
                        "start": {
                            "type": "integer",
                            "description": "Starting line number (1-indexed)"
                        },
                        "end": {
                            "type": "integer",
                            "description": "Ending line number (inclusive)"
                        }
                    },
                    "required": ["path", "start", "end"]
                }),
            },
        },
        // 5. SHELL - Fallback for edge cases
        ToolDefinition {
            tool_type: "function",
            function: FunctionDefinition {
                name: "shell",
                description: "Execute shell command. Use only when specialized tools don't fit. Output truncated at 4KB.",
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "command": {
                            "type": "string",
                            "description": "The shell command to execute"
                        }
                    },
                    "required": ["command"]
                }),
            },
        },
    ]
}

// ═══════════════════════════════════════════════════════════════════════════
//  TOOL EXECUTION
// ═══════════════════════════════════════════════════════════════════════════

/// Allowlist of safe commands for shell execution.
/// Only these base commands are permitted to prevent command injection.
const ALLOWED_COMMANDS: &[&str] = &[
    "ls", "cat", "head", "tail", "grep", "rg", "find", "wc", "sort", "uniq", "diff", "file",
    "stat", "du", "tree", "pwd", "echo", "printf", "test", "expr", "tr", "cut", "awk", "sed",
    "xargs", "tee", "less", "more", "strings", "hexdump", "xxd", "jq", "yq",
];

/// Shell metacharacters that could be used to bypass the allowlist or cause harm
const DANGEROUS_SHELL_CHARS: &[char] = &[
    '`',  // Command substitution
    '$',  // Variable expansion / command substitution
    ';',  // Command separator
    '&',  // Background / AND operator
    '\n', // Newline (command separator)
    '>',  // Output redirection (could overwrite files)
    '<',  // Input redirection
];

/// Maximum output size for tool results (4KB ≈ 1k tokens)
const MAX_OUTPUT_SIZE: usize = 4000;

/// Maximum length for search patterns to prevent ReDoS
const MAX_PATTERN_LENGTH: usize = 500;

/// Patterns that could cause ReDoS (catastrophic backtracking)
const DANGEROUS_REGEX_PATTERNS: &[&str] = &[
    "(.*)+",    // Nested quantifiers with .
    "(.+)+",    // Nested quantifiers
    "(a+)+",    // Classic ReDoS pattern
    "([^a]+)+", // Negated class with nested quantifier
    "(\\s+)+",  // Whitespace with nested quantifier
    "(\\S+)+",  // Non-whitespace with nested quantifier
    "(\\w+)+",  // Word chars with nested quantifier
];

/// Validate a regex pattern for safety (length and ReDoS prevention)
fn is_safe_regex_pattern(pattern: &str) -> Result<(), String> {
    // Length limit
    if pattern.len() > MAX_PATTERN_LENGTH {
        return Err(format!(
            "Pattern too long ({} chars). Maximum is {} chars.",
            pattern.len(),
            MAX_PATTERN_LENGTH
        ));
    }

    // Check for known dangerous patterns
    let pattern_lower = pattern.to_lowercase();
    for dangerous in DANGEROUS_REGEX_PATTERNS {
        if pattern_lower.contains(dangerous) {
            return Err(format!(
                "Pattern contains potentially dangerous construct '{}' that could cause slow execution. \
                 Simplify the pattern to avoid nested quantifiers.",
                dangerous
            ));
        }
    }

    // Heuristic: reject patterns with multiple nested groups with quantifiers
    // Count occurrences of patterns like (...)+ or (...)*
    let mut nested_quantifier_count = 0;
    let mut in_group = 0;
    let mut prev_char = ' ';
    for c in pattern.chars() {
        match c {
            '(' => in_group += 1,
            ')' => {
                if in_group > 0 {
                    in_group -= 1;
                }
            }
            '+' | '*' if prev_char == ')' && in_group == 0 => {
                nested_quantifier_count += 1;
            }
            _ => {}
        }
        prev_char = c;
    }

    if nested_quantifier_count >= 2 {
        return Err(
            "Pattern has multiple groups with quantifiers which could cause slow execution. \
             Simplify the pattern."
                .to_string(),
        );
    }

    Ok(())
}

/// Execute a tool call and return the result
pub fn execute_tool(root: &Path, tool_call: &ToolCall) -> ToolResult {
    let content = match tool_call.function.name.as_str() {
        "tree" => execute_tree(root, &tool_call.function.arguments),
        "head" => execute_head(root, &tool_call.function.arguments),
        "search" => execute_search(root, &tool_call.function.arguments),
        "read_range" => execute_read_range(root, &tool_call.function.arguments),
        "shell" => execute_shell(root, &tool_call.function.arguments),
        _ => format!("Unknown tool: {}", tool_call.function.name),
    };

    ToolResult {
        tool_call_id: tool_call.id.clone(),
        content,
    }
}

// ═══════════════════════════════════════════════════════════════════════════
//  TREE - Directory structure
// ═══════════════════════════════════════════════════════════════════════════

fn execute_tree(root: &Path, args_json: &str) -> String {
    #[derive(Deserialize, Default)]
    struct TreeArgs {
        path: Option<String>,
        depth: Option<usize>,
    }

    let args: TreeArgs = serde_json::from_str(args_json).unwrap_or_default();
    let target = match &args.path {
        Some(p) => {
            // Validate path to prevent traversal attacks
            let path = std::path::Path::new(p);
            match resolve_repo_path_allow_new(root, path) {
                Ok(resolved) => resolved.absolute,
                Err(e) => return format!("Invalid path '{}': {}", p, e),
            }
        }
        None => root.to_path_buf(),
    };
    let max_depth = args.depth.unwrap_or(3);

    if !target.exists() {
        return format!("Path not found: {}", target.display());
    }

    let mut output = String::new();
    build_tree(&target, root, "", max_depth, 0, &mut output);
    truncate_output(output)
}

fn build_tree(
    path: &Path,
    root: &Path,
    prefix: &str,
    max_depth: usize,
    depth: usize,
    output: &mut String,
) {
    if depth > max_depth {
        return;
    }

    // Get relative path for display
    let display_name = path.strip_prefix(root).unwrap_or(path);
    let name = if depth == 0 {
        display_name.display().to_string()
    } else {
        path.file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| ".".to_string())
    };

    // Skip hidden files/dirs except at root
    if depth > 0 && name.starts_with('.') {
        return;
    }

    if path.is_dir() {
        output.push_str(&format!("{}{}/\n", prefix, name));

        // Read and sort entries
        let mut entries: Vec<_> = match fs::read_dir(path) {
            Ok(entries) => entries.filter_map(|e| e.ok()).collect(),
            Err(_) => return,
        };
        entries.sort_by_key(|e| e.path());

        // Filter out common noise
        let entries: Vec<_> = entries
            .into_iter()
            .filter(|e| {
                let name = e.file_name().to_string_lossy().to_string();
                !matches!(
                    name.as_str(),
                    "node_modules"
                        | "target"
                        | ".git"
                        | "__pycache__"
                        | ".venv"
                        | "venv"
                        | "dist"
                        | "build"
                        | ".next"
                )
            })
            .collect();

        let count = entries.len();
        for (i, entry) in entries.into_iter().enumerate() {
            let is_last = i == count - 1;
            let new_prefix = if depth == 0 {
                String::new()
            } else {
                format!("{}{}   ", prefix, if is_last { " " } else { "│" })
            };
            let connector = if is_last { "└── " } else { "├── " };
            let child_prefix = format!("{}{}", prefix, connector);

            if entry.path().is_dir() {
                build_tree(
                    &entry.path(),
                    root,
                    &new_prefix,
                    max_depth,
                    depth + 1,
                    output,
                );
            } else {
                let file_name = entry.file_name().to_string_lossy().to_string();
                if !file_name.starts_with('.') {
                    output.push_str(&format!("{}{}\n", child_prefix, file_name));
                }
            }
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════════
//  HEAD - First N lines
// ═══════════════════════════════════════════════════════════════════════════

fn execute_head(root: &Path, args_json: &str) -> String {
    #[derive(Deserialize)]
    struct HeadArgs {
        path: String,
        lines: Option<usize>,
    }

    let args: HeadArgs = match serde_json::from_str(args_json) {
        Ok(a) => a,
        Err(e) => return format!("Invalid arguments: {}", e),
    };

    // Validate path to prevent traversal attacks
    let target = match resolve_repo_path_allow_new(root, std::path::Path::new(&args.path)) {
        Ok(resolved) => resolved.absolute,
        Err(e) => return format!("Invalid path '{}': {}", args.path, e),
    };
    let num_lines = args.lines.unwrap_or(50);

    if !target.exists() {
        return format!("File not found: {}", args.path);
    }

    match fs::read_to_string(&target) {
        Ok(content) => {
            let lines: Vec<&str> = content.lines().take(num_lines).collect();
            let total_lines = content.lines().count();
            let mut output = String::new();

            for (i, line) in lines.iter().enumerate() {
                output.push_str(&format!("{:>4}│ {}\n", i + 1, line));
            }

            if total_lines > num_lines {
                output.push_str(&format!("\n... ({} more lines)\n", total_lines - num_lines));
            }

            output
        }
        Err(e) => format!("Failed to read file: {}", e),
    }
}

// ═══════════════════════════════════════════════════════════════════════════
//  SEARCH - Pattern matching with context
// ═══════════════════════════════════════════════════════════════════════════

fn execute_search(root: &Path, args_json: &str) -> String {
    #[derive(Deserialize)]
    struct SearchArgs {
        pattern: String,
        path: Option<String>,
        context: Option<usize>,
    }

    let args: SearchArgs = match serde_json::from_str(args_json) {
        Ok(a) => a,
        Err(e) => return format!("Invalid arguments: {}", e),
    };

    // Validate regex pattern for safety (prevent ReDoS)
    if let Err(e) = is_safe_regex_pattern(&args.pattern) {
        return e;
    }

    let target = match &args.path {
        Some(p) => {
            // Validate path to prevent traversal attacks
            match resolve_repo_path_allow_new(root, std::path::Path::new(p)) {
                Ok(resolved) => resolved.absolute,
                Err(e) => return format!("Invalid path '{}': {}", p, e),
            }
        }
        None => root.to_path_buf(),
    };
    let context = args.context.unwrap_or(2);

    // Use ripgrep for speed and smart defaults
    let mut cmd = Command::new("rg");
    cmd.arg("--line-number")
        .arg("--no-heading")
        .arg("--color=never")
        .arg("-C")
        .arg(context.to_string())
        .arg("--max-count=50") // Limit matches per file
        .arg(&args.pattern)
        .arg(&target)
        .current_dir(root);

    match run_command_with_timeout(&mut cmd, TOOL_TIMEOUT) {
        Ok(result) => {
            if result.timed_out {
                return "Search timed out after 10 seconds. Try a more specific pattern or path."
                    .to_string();
            }
            if result.stdout.is_empty() {
                format!("No matches found for pattern: {}", args.pattern)
            } else {
                truncate_output(result.stdout)
            }
        }
        Err(_) => {
            // Fallback to grep if rg not available
            let mut cmd = Command::new("grep");
            cmd.arg("-rn")
                .arg("-C")
                .arg(context.to_string())
                .arg(&args.pattern)
                .arg(&target)
                .current_dir(root);

            match run_command_with_timeout(&mut cmd, TOOL_TIMEOUT) {
                Ok(result) => {
                    if result.timed_out {
                        return "Search timed out after 10 seconds. Try a more specific pattern or path.".to_string();
                    }
                    if result.stdout.is_empty() {
                        format!("No matches found for pattern: {}", args.pattern)
                    } else {
                        truncate_output(result.stdout)
                    }
                }
                Err(e) => format!("Search failed: {}", e),
            }
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════════
//  READ_RANGE - Specific line range
// ═══════════════════════════════════════════════════════════════════════════

fn execute_read_range(root: &Path, args_json: &str) -> String {
    #[derive(Deserialize)]
    struct ReadRangeArgs {
        path: String,
        start: usize,
        end: usize,
    }

    let args: ReadRangeArgs = match serde_json::from_str(args_json) {
        Ok(a) => a,
        Err(e) => return format!("Invalid arguments: {}", e),
    };

    // Validate path to prevent traversal attacks
    let target = match resolve_repo_path_allow_new(root, std::path::Path::new(&args.path)) {
        Ok(resolved) => resolved.absolute,
        Err(e) => return format!("Invalid path '{}': {}", args.path, e),
    };

    if !target.exists() {
        return format!("File not found: {}", args.path);
    }

    // Validate range
    if args.start == 0 || args.end < args.start {
        return "Invalid range: start must be >= 1 and end must be >= start".to_string();
    }

    match fs::read_to_string(&target) {
        Ok(content) => {
            let lines: Vec<&str> = content.lines().collect();
            let total_lines = lines.len();

            // Clamp to actual file bounds
            let start = args.start.saturating_sub(1); // Convert to 0-indexed
            let end = args.end.min(total_lines);

            if start >= total_lines {
                return format!(
                    "Start line {} exceeds file length ({})",
                    args.start, total_lines
                );
            }

            let mut output = String::new();

            if start > 0 {
                output.push_str(&format!("... (lines 1-{} above)\n\n", start));
            }

            for (i, line) in lines[start..end].iter().enumerate() {
                output.push_str(&format!("{:>4}│ {}\n", start + i + 1, line));
            }

            if end < total_lines {
                output.push_str(&format!("\n... ({} more lines below)\n", total_lines - end));
            }

            output
        }
        Err(e) => format!("Failed to read file: {}", e),
    }
}

// ═══════════════════════════════════════════════════════════════════════════
//  SHELL - Fallback for edge cases
// ═══════════════════════════════════════════════════════════════════════════

/// Truncate output at line boundary
fn truncate_output(result: String) -> String {
    if result.len() > MAX_OUTPUT_SIZE {
        let truncate_at = result[..MAX_OUTPUT_SIZE]
            .rfind('\n')
            .unwrap_or(MAX_OUTPUT_SIZE);
        format!(
            "{}\n\n... (truncated - use read_range for specific sections)",
            &result[..truncate_at]
        )
    } else {
        result
    }
}

/// Extract the base command from a shell command string.
/// Returns the first word (the command name) for allowlist checking.
fn extract_base_command(command: &str) -> Option<&str> {
    command.split_whitespace().next()
}

/// Check if a command is in the allowlist.
/// Only the base command (first word) is checked.
fn is_command_allowed(command: &str) -> bool {
    if let Some(base) = extract_base_command(command) {
        ALLOWED_COMMANDS.contains(&base)
    } else {
        false
    }
}

/// Check if command contains dangerous shell metacharacters that could bypass allowlist.
fn contains_dangerous_chars(command: &str) -> bool {
    command.chars().any(|c| DANGEROUS_SHELL_CHARS.contains(&c))
}

/// Execute shell command with allowlist-based safety checks
fn execute_shell(root: &Path, args_json: &str) -> String {
    #[derive(Deserialize)]
    struct ShellArgs {
        command: String,
    }

    let args: ShellArgs = match serde_json::from_str(args_json) {
        Ok(a) => a,
        Err(e) => return format!("Invalid arguments: {}", e),
    };

    let command = args.command.trim();

    // Check for dangerous shell metacharacters that could bypass the allowlist
    if contains_dangerous_chars(command) {
        return format!(
            "Command blocked: contains shell metacharacters that could bypass security. \
             Avoid using: backticks, $, ;, &, or newlines. \
             Use pipes (|) for chaining allowed commands."
        );
    }

    // Split by pipe and validate each command in the pipeline
    for part in command.split('|') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        if !is_command_allowed(part) {
            let base = extract_base_command(part).unwrap_or("<empty>");
            return format!(
                "Command '{}' is not in the allowlist. Allowed commands: {}",
                base,
                ALLOWED_COMMANDS.join(", ")
            );
        }
    }

    // Ensure we're working within the repo by checking the path exists
    if !root.exists() {
        return format!("Repository root does not exist: {}", root.display());
    }

    // Execute the command with timeout
    let mut cmd = Command::new("sh");
    cmd.args(["-c", command]).current_dir(root);

    match run_command_with_timeout(&mut cmd, TOOL_TIMEOUT) {
        Ok(run_result) => {
            if run_result.timed_out {
                return "Command timed out after 10 seconds".to_string();
            }

            let exit_code = run_result
                .status
                .map(|s| s.code().unwrap_or(-1))
                .unwrap_or(-1);

            let mut result = String::new();

            if !run_result.stdout.is_empty() {
                result.push_str(&run_result.stdout);
            }

            if !run_result.stderr.is_empty() {
                if !result.is_empty() {
                    result.push_str("\n--- stderr ---\n");
                }
                result.push_str(&run_result.stderr);
            }

            if result.is_empty() {
                result = format!("Command completed with exit code {}", exit_code);
            } else if exit_code != 0 {
                result.push_str(&format!("\n[exit code: {}]", exit_code));
            }

            truncate_output(result)
        }
        Err(e) => format!("Failed to execute command: {}", e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn test_shell_echo() {
        let dir = tempdir().unwrap();

        let call = ToolCall {
            id: "1".to_string(),
            function: FunctionCall {
                name: "shell".to_string(),
                arguments: r#"{"command": "echo hello world"}"#.to_string(),
            },
        };

        let result = execute_tool(dir.path(), &call);
        assert!(result.content.contains("hello world"));
    }

    #[test]
    fn test_shell_grep() {
        let dir = tempdir().unwrap();
        let file = dir.path().join("test.rs");
        fs::write(&file, "fn hello_world() {\n    println!(\"hello\");\n}").unwrap();

        let call = ToolCall {
            id: "1".to_string(),
            function: FunctionCall {
                name: "shell".to_string(),
                arguments: r#"{"command": "grep hello_world test.rs"}"#.to_string(),
            },
        };

        let result = execute_tool(dir.path(), &call);
        assert!(
            result.content.contains("hello_world"),
            "Expected 'hello_world' in result: {}",
            result.content
        );
    }

    #[test]
    fn test_shell_cat() {
        let dir = tempdir().unwrap();
        let file = dir.path().join("test.txt");
        fs::write(&file, "line1\nline2\nline3").unwrap();

        let call = ToolCall {
            id: "1".to_string(),
            function: FunctionCall {
                name: "shell".to_string(),
                arguments: r#"{"command": "cat test.txt"}"#.to_string(),
            },
        };

        let result = execute_tool(dir.path(), &call);
        assert!(result.content.contains("line1"));
        assert!(result.content.contains("line2"));
        assert!(result.content.contains("line3"));
    }

    #[test]
    fn test_shell_ls() {
        let dir = tempdir().unwrap();
        fs::create_dir(dir.path().join("subdir")).unwrap();
        fs::write(dir.path().join("file.rs"), "content").unwrap();

        let call = ToolCall {
            id: "1".to_string(),
            function: FunctionCall {
                name: "shell".to_string(),
                arguments: r#"{"command": "ls"}"#.to_string(),
            },
        };

        let result = execute_tool(dir.path(), &call);
        assert!(result.content.contains("subdir"));
        assert!(result.content.contains("file.rs"));
    }

    #[test]
    fn test_shell_blocks_dangerous() {
        let dir = tempdir().unwrap();

        let call = ToolCall {
            id: "1".to_string(),
            function: FunctionCall {
                name: "shell".to_string(),
                arguments: r#"{"command": "sudo rm -rf /"}"#.to_string(),
            },
        };

        let result = execute_tool(dir.path(), &call);
        // sudo is not in the allowlist
        assert!(
            result.content.contains("not in the allowlist"),
            "Expected command to be blocked: {}",
            result.content
        );
    }

    #[test]
    fn test_shell_blocks_rm() {
        let dir = tempdir().unwrap();
        let file = dir.path().join("delete_me.txt");
        fs::write(&file, "temporary").unwrap();
        assert!(file.exists());

        let call = ToolCall {
            id: "1".to_string(),
            function: FunctionCall {
                name: "shell".to_string(),
                arguments: r#"{"command": "rm delete_me.txt"}"#.to_string(),
            },
        };

        let result = execute_tool(dir.path(), &call);
        // rm is not in the allowlist for safety
        assert!(
            result.content.contains("not in the allowlist"),
            "Expected rm to be blocked: {}",
            result.content
        );
        assert!(file.exists()); // File should NOT be deleted
    }

    #[test]
    fn test_shell_piping() {
        let dir = tempdir().unwrap();
        let file = dir.path().join("test.txt");
        fs::write(&file, "apple\nbanana\napricot\ncherry").unwrap();

        let call = ToolCall {
            id: "1".to_string(),
            function: FunctionCall {
                name: "shell".to_string(),
                arguments: r#"{"command": "grep ^a test.txt | wc -l"}"#.to_string(),
            },
        };

        let result = execute_tool(dir.path(), &call);
        // Should find 2 lines starting with 'a' (apple, apricot)
        assert!(
            result.content.trim().contains('2'),
            "Expected count of 2: {}",
            result.content
        );
    }

    #[test]
    fn test_shell_head_tail() {
        let dir = tempdir().unwrap();
        let file = dir.path().join("test.txt");
        fs::write(&file, "line1\nline2\nline3\nline4\nline5").unwrap();

        let call = ToolCall {
            id: "1".to_string(),
            function: FunctionCall {
                name: "shell".to_string(),
                arguments: r#"{"command": "head -2 test.txt"}"#.to_string(),
            },
        };

        let result = execute_tool(dir.path(), &call);
        assert!(result.content.contains("line1"));
        assert!(result.content.contains("line2"));
        assert!(!result.content.contains("line3"));
    }

    #[test]
    fn test_shell_find() {
        let dir = tempdir().unwrap();
        fs::create_dir(dir.path().join("subdir")).unwrap();
        fs::write(dir.path().join("file.rs"), "content").unwrap();
        fs::write(dir.path().join("subdir/nested.rs"), "content").unwrap();

        let call = ToolCall {
            id: "1".to_string(),
            function: FunctionCall {
                name: "shell".to_string(),
                arguments: r#"{"command": "find . -name '*.rs'"}"#.to_string(),
            },
        };

        let result = execute_tool(dir.path(), &call);
        assert!(result.content.contains("file.rs"));
        assert!(result.content.contains("nested.rs"));
    }

    #[test]
    fn test_shell_blocks_fork_bomb() {
        let dir = tempdir().unwrap();

        let call = ToolCall {
            id: "1".to_string(),
            function: FunctionCall {
                name: "shell".to_string(),
                arguments: r#"{"command": ":(){:|:&};:"}"#.to_string(),
            },
        };

        let result = execute_tool(dir.path(), &call);
        // Blocked due to & and ; being dangerous shell chars
        assert!(
            result.content.contains("blocked") || result.content.contains("metacharacters"),
            "Expected fork bomb to be blocked: {}",
            result.content
        );
    }

    #[test]
    fn test_shell_blocks_curl_pipe() {
        let dir = tempdir().unwrap();

        // curl is not in the allowlist
        let call = ToolCall {
            id: "1".to_string(),
            function: FunctionCall {
                name: "shell".to_string(),
                arguments: r#"{"command": "curl http://example.com"}"#.to_string(),
            },
        };

        let result = execute_tool(dir.path(), &call);
        assert!(
            result.content.contains("not in the allowlist"),
            "Expected curl to be blocked: {}",
            result.content
        );
    }

    #[test]
    fn test_shell_blocks_command_substitution() {
        let dir = tempdir().unwrap();

        let call = ToolCall {
            id: "1".to_string(),
            function: FunctionCall {
                name: "shell".to_string(),
                arguments: r#"{"command": "echo $(whoami)"}"#.to_string(),
            },
        };

        let result = execute_tool(dir.path(), &call);
        // Blocked due to $ being a dangerous shell char
        assert!(
            result.content.contains("metacharacters"),
            "Expected command substitution to be blocked: {}",
            result.content
        );
    }

    #[test]
    fn test_shell_allows_echo() {
        let dir = tempdir().unwrap();

        let call = ToolCall {
            id: "1".to_string(),
            function: FunctionCall {
                name: "shell".to_string(),
                arguments: r#"{"command": "echo 'hello world'"}"#.to_string(),
            },
        };

        let result = execute_tool(dir.path(), &call);
        assert!(
            result.content.contains("hello world"),
            "Expected echo to work: {}",
            result.content
        );
    }

    #[test]
    fn test_shell_blocks_file_write() {
        let dir = tempdir().unwrap();

        let call = ToolCall {
            id: "1".to_string(),
            function: FunctionCall {
                name: "shell".to_string(),
                arguments: r#"{"command": "echo 'new content' > newfile.txt"}"#.to_string(),
            },
        };

        let result = execute_tool(dir.path(), &call);
        // > is blocked as a dangerous shell char
        assert!(
            result.content.contains("metacharacters"),
            "Expected file redirection to be blocked: {}",
            result.content
        );

        // Verify file was NOT created
        let created = dir.path().join("newfile.txt");
        assert!(!created.exists());
    }

    #[test]
    fn test_shell_sed_read_only() {
        let dir = tempdir().unwrap();
        let file = dir.path().join("test.txt");
        fs::write(&file, "hello world").unwrap();

        // sed without -i is allowed (read-only mode)
        let call = ToolCall {
            id: "1".to_string(),
            function: FunctionCall {
                name: "shell".to_string(),
                arguments: r#"{"command": "sed 's/hello/goodbye/' test.txt"}"#.to_string(),
            },
        };

        let result = execute_tool(dir.path(), &call);
        assert!(
            result.content.contains("goodbye"),
            "Expected sed to transform output: {}",
            result.content
        );

        // Original file should be unchanged
        let content = fs::read_to_string(&file).unwrap();
        assert!(content.contains("hello"));
    }

    #[test]
    fn test_shell_exit_code() {
        let dir = tempdir().unwrap();

        // Use a command that returns non-zero exit code
        let call = ToolCall {
            id: "1".to_string(),
            function: FunctionCall {
                name: "shell".to_string(),
                arguments: r#"{"command": "test -f nonexistent_file_xyz"}"#.to_string(),
            },
        };

        let result = execute_tool(dir.path(), &call);
        // Should include exit code for failed commands
        assert!(
            result.content.contains("exit code"),
            "Expected exit code in output: {}",
            result.content
        );
    }

    #[test]
    fn test_shell_stderr_capture() {
        let dir = tempdir().unwrap();

        let call = ToolCall {
            id: "1".to_string(),
            function: FunctionCall {
                name: "shell".to_string(),
                arguments: r#"{"command": "ls nonexistent_file_xyz"}"#.to_string(),
            },
        };

        let result = execute_tool(dir.path(), &call);
        // Should capture stderr
        assert!(
            result.content.contains("No such file") || result.content.contains("cannot access"),
            "Expected error message: {}",
            result.content
        );
    }

    #[test]
    fn test_tool_definitions() {
        let tools = get_tool_definitions();
        assert_eq!(tools.len(), 5);

        // Verify tools are in the right order for top-down exploration
        let names: Vec<_> = tools.iter().map(|t| t.function.name).collect();
        assert_eq!(names, vec!["tree", "head", "search", "read_range", "shell"]);

        // All tools should be functions
        for tool in &tools {
            assert_eq!(tool.tool_type, "function");
        }
    }

    #[test]
    fn test_unknown_tool() {
        let dir = tempdir().unwrap();

        let call = ToolCall {
            id: "1".to_string(),
            function: FunctionCall {
                name: "unknown_tool".to_string(),
                arguments: r#"{}"#.to_string(),
            },
        };

        let result = execute_tool(dir.path(), &call);
        assert!(result.content.contains("Unknown tool"));
    }

    #[test]
    fn test_invalid_json_args() {
        let dir = tempdir().unwrap();

        let call = ToolCall {
            id: "1".to_string(),
            function: FunctionCall {
                name: "shell".to_string(),
                arguments: "not valid json".to_string(),
            },
        };

        let result = execute_tool(dir.path(), &call);
        assert!(result.content.contains("Invalid arguments"));
    }

    // ═══════════════════════════════════════════════════════════════════════
    //  TREE TOOL TESTS
    // ═══════════════════════════════════════════════════════════════════════

    #[test]
    fn test_tree_basic() {
        let dir = tempdir().unwrap();
        fs::create_dir(dir.path().join("src")).unwrap();
        fs::write(dir.path().join("src/main.rs"), "fn main() {}").unwrap();
        fs::write(dir.path().join("README.md"), "# Test").unwrap();

        let call = ToolCall {
            id: "1".to_string(),
            function: FunctionCall {
                name: "tree".to_string(),
                arguments: r#"{}"#.to_string(),
            },
        };

        let result = execute_tool(dir.path(), &call);
        assert!(result.content.contains("src/"));
        assert!(result.content.contains("main.rs"));
        assert!(result.content.contains("README.md"));
    }

    #[test]
    fn test_tree_with_depth() {
        let dir = tempdir().unwrap();
        fs::create_dir_all(dir.path().join("a/b/c/d")).unwrap();
        fs::write(dir.path().join("a/b/c/d/deep.txt"), "deep").unwrap();

        let call = ToolCall {
            id: "1".to_string(),
            function: FunctionCall {
                name: "tree".to_string(),
                arguments: r#"{"depth": 2}"#.to_string(),
            },
        };

        let result = execute_tool(dir.path(), &call);
        assert!(result.content.contains("a/"));
        assert!(result.content.contains("b/"));
        // Depth 2 shouldn't show c/ or d/
        assert!(!result.content.contains("deep.txt"));
    }

    // ═══════════════════════════════════════════════════════════════════════
    //  HEAD TOOL TESTS
    // ═══════════════════════════════════════════════════════════════════════

    #[test]
    fn test_head_basic() {
        let dir = tempdir().unwrap();
        let content = (1..=100)
            .map(|i| format!("line {}", i))
            .collect::<Vec<_>>()
            .join("\n");
        fs::write(dir.path().join("test.txt"), &content).unwrap();

        let call = ToolCall {
            id: "1".to_string(),
            function: FunctionCall {
                name: "head".to_string(),
                arguments: r#"{"path": "test.txt", "lines": 10}"#.to_string(),
            },
        };

        let result = execute_tool(dir.path(), &call);
        assert!(result.content.contains("line 1"));
        assert!(result.content.contains("line 10"));
        assert!(!result.content.contains("line 11"));
        assert!(result.content.contains("90 more lines"));
    }

    #[test]
    fn test_head_with_line_numbers() {
        let dir = tempdir().unwrap();
        fs::write(
            dir.path().join("test.rs"),
            "fn main() {\n    println!(\"hello\");\n}",
        )
        .unwrap();

        let call = ToolCall {
            id: "1".to_string(),
            function: FunctionCall {
                name: "head".to_string(),
                arguments: r#"{"path": "test.rs"}"#.to_string(),
            },
        };

        let result = execute_tool(dir.path(), &call);
        // Should include line numbers
        assert!(result.content.contains("1│"));
        assert!(result.content.contains("2│"));
    }

    // ═══════════════════════════════════════════════════════════════════════
    //  SEARCH TOOL TESTS
    // ═══════════════════════════════════════════════════════════════════════

    #[test]
    fn test_search_basic() {
        let dir = tempdir().unwrap();
        fs::write(
            dir.path().join("test.rs"),
            "fn hello_world() {\n    println!(\"hello\");\n}",
        )
        .unwrap();

        let call = ToolCall {
            id: "1".to_string(),
            function: FunctionCall {
                name: "search".to_string(),
                arguments: r#"{"pattern": "hello_world"}"#.to_string(),
            },
        };

        let result = execute_tool(dir.path(), &call);
        assert!(result.content.contains("hello_world"));
    }

    #[test]
    fn test_search_no_matches() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("test.rs"), "fn main() {}").unwrap();

        let call = ToolCall {
            id: "1".to_string(),
            function: FunctionCall {
                name: "search".to_string(),
                arguments: r#"{"pattern": "nonexistent_pattern_xyz"}"#.to_string(),
            },
        };

        let result = execute_tool(dir.path(), &call);
        assert!(result.content.contains("No matches"));
    }

    // ═══════════════════════════════════════════════════════════════════════
    //  READ_RANGE TOOL TESTS
    // ═══════════════════════════════════════════════════════════════════════

    #[test]
    fn test_read_range_basic() {
        let dir = tempdir().unwrap();
        let content = (1..=50)
            .map(|i| format!("line {}", i))
            .collect::<Vec<_>>()
            .join("\n");
        fs::write(dir.path().join("test.txt"), &content).unwrap();

        let call = ToolCall {
            id: "1".to_string(),
            function: FunctionCall {
                name: "read_range".to_string(),
                arguments: r#"{"path": "test.txt", "start": 10, "end": 15}"#.to_string(),
            },
        };

        let result = execute_tool(dir.path(), &call);
        assert!(result.content.contains("line 10"));
        assert!(result.content.contains("line 15"));
        assert!(!result.content.contains("line 9"));
        assert!(!result.content.contains("line 16"));
        assert!(result.content.contains("lines 1-9 above"));
        assert!(result.content.contains("more lines below"));
    }

    #[test]
    fn test_read_range_with_line_numbers() {
        let dir = tempdir().unwrap();
        let content = (1..=20)
            .map(|i| format!("content {}", i))
            .collect::<Vec<_>>()
            .join("\n");
        fs::write(dir.path().join("test.txt"), &content).unwrap();

        let call = ToolCall {
            id: "1".to_string(),
            function: FunctionCall {
                name: "read_range".to_string(),
                arguments: r#"{"path": "test.txt", "start": 5, "end": 7}"#.to_string(),
            },
        };

        let result = execute_tool(dir.path(), &call);
        // Should show line numbers in output
        assert!(result.content.contains("5│"));
        assert!(result.content.contains("6│"));
        assert!(result.content.contains("7│"));
    }

    #[test]
    fn test_read_range_invalid_range() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("test.txt"), "line 1\nline 2").unwrap();

        let call = ToolCall {
            id: "1".to_string(),
            function: FunctionCall {
                name: "read_range".to_string(),
                arguments: r#"{"path": "test.txt", "start": 10, "end": 5}"#.to_string(),
            },
        };

        let result = execute_tool(dir.path(), &call);
        assert!(result.content.contains("Invalid range"));
    }

    #[test]
    fn test_read_range_file_not_found() {
        let dir = tempdir().unwrap();

        let call = ToolCall {
            id: "1".to_string(),
            function: FunctionCall {
                name: "read_range".to_string(),
                arguments: r#"{"path": "nonexistent.txt", "start": 1, "end": 10}"#.to_string(),
            },
        };

        let result = execute_tool(dir.path(), &call);
        assert!(result.content.contains("File not found"));
    }

    // ═══════════════════════════════════════════════════════════════════════
    //  PATH TRAVERSAL SECURITY TESTS
    // ═══════════════════════════════════════════════════════════════════════

    #[test]
    fn test_tree_blocks_path_traversal() {
        let dir = tempdir().unwrap();

        let call = ToolCall {
            id: "1".to_string(),
            function: FunctionCall {
                name: "tree".to_string(),
                arguments: r#"{"path": "../../../etc"}"#.to_string(),
            },
        };

        let result = execute_tool(dir.path(), &call);
        assert!(
            result.content.contains("Invalid path"),
            "Expected path traversal to be blocked: {}",
            result.content
        );
    }

    #[test]
    fn test_head_blocks_path_traversal() {
        let dir = tempdir().unwrap();

        let call = ToolCall {
            id: "1".to_string(),
            function: FunctionCall {
                name: "head".to_string(),
                arguments: r#"{"path": "../../../etc/passwd"}"#.to_string(),
            },
        };

        let result = execute_tool(dir.path(), &call);
        assert!(
            result.content.contains("Invalid path"),
            "Expected path traversal to be blocked: {}",
            result.content
        );
    }

    #[test]
    fn test_search_blocks_path_traversal() {
        let dir = tempdir().unwrap();

        let call = ToolCall {
            id: "1".to_string(),
            function: FunctionCall {
                name: "search".to_string(),
                arguments: r#"{"pattern": "root", "path": "../../../etc"}"#.to_string(),
            },
        };

        let result = execute_tool(dir.path(), &call);
        assert!(
            result.content.contains("Invalid path"),
            "Expected path traversal to be blocked: {}",
            result.content
        );
    }

    #[test]
    fn test_read_range_blocks_path_traversal() {
        let dir = tempdir().unwrap();

        let call = ToolCall {
            id: "1".to_string(),
            function: FunctionCall {
                name: "read_range".to_string(),
                arguments: r#"{"path": "../../../etc/passwd", "start": 1, "end": 10}"#.to_string(),
            },
        };

        let result = execute_tool(dir.path(), &call);
        assert!(
            result.content.contains("Invalid path"),
            "Expected path traversal to be blocked: {}",
            result.content
        );
    }

    #[test]
    fn test_head_blocks_absolute_path() {
        let dir = tempdir().unwrap();

        let call = ToolCall {
            id: "1".to_string(),
            function: FunctionCall {
                name: "head".to_string(),
                arguments: r#"{"path": "/etc/passwd"}"#.to_string(),
            },
        };

        let result = execute_tool(dir.path(), &call);
        assert!(
            result.content.contains("Invalid path"),
            "Expected absolute path to be blocked: {}",
            result.content
        );
    }
}
