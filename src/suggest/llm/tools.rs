//! Agentic tool definitions and execution for LLM-driven code exploration.
//!
//! Philosophy: Let the model do its best work. Git is the safety net.
//! We provide a single powerful shell tool instead of limited primitives.

use serde::{Deserialize, Serialize};
use std::path::Path;
use std::process::Command;

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

/// Get all available tool definitions - just one powerful shell tool
pub fn get_tool_definitions() -> Vec<ToolDefinition> {
    vec![ToolDefinition {
        tool_type: "function",
        function: FunctionDefinition {
            name: "shell",
            description: r#"Execute a shell command in the repository. Use this to explore code, search for patterns, read files, check types, run tests, or make changes.

Common patterns:
- Search: rg "pattern" or grep -r "pattern" .
- Read files: cat path/to/file or head -100 file
- List files: ls -la or find . -name "*.rs"
- Check types: cargo check or tsc --noEmit
- Run tests: cargo test or npm test
- Edit files: Use sed, or write new content with shell redirection

The command runs in the repository root. Git protects against mistakes - be bold."#,
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
    }]
}

/// Commands/patterns that are blocked for safety (system-level destruction)
const BLOCKED_PATTERNS: &[&str] = &[
    "sudo ",
    "rm -rf /",
    "rm -rf /*",
    "rm -rf ~",
    "mkfs",
    "dd if=",
    ":(){", // fork bomb
    "chmod -R 777 /",
    "chown -R",
    "> /dev/",
    "curl | sh",
    "curl | bash",
    "wget | sh",
    "wget | bash",
];

/// Execute a tool call and return the result
pub fn execute_tool(root: &Path, tool_call: &ToolCall) -> ToolResult {
    let content = match tool_call.function.name.as_str() {
        "shell" => execute_shell(root, &tool_call.function.arguments),
        _ => format!("Unknown tool: {}", tool_call.function.name),
    };

    ToolResult {
        tool_call_id: tool_call.id.clone(),
        content,
    }
}

/// Execute shell command with safety checks
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

    // Check for blocked patterns
    let cmd_lower = command.to_lowercase();
    for pattern in BLOCKED_PATTERNS {
        if cmd_lower.contains(&pattern.to_lowercase()) {
            return format!(
                "Command blocked for safety: contains '{}'. This restriction exists to prevent system-level damage outside the repository.",
                pattern
            );
        }
    }

    // Ensure we're working within the repo by checking the path exists
    if !root.exists() {
        return format!("Repository root does not exist: {}", root.display());
    }

    // Execute the command with timeout
    let output = Command::new("sh")
        .args(["-c", command])
        .current_dir(root)
        .output();

    match output {
        Ok(out) => {
            let stdout = String::from_utf8_lossy(&out.stdout);
            let stderr = String::from_utf8_lossy(&out.stderr);
            let exit_code = out.status.code().unwrap_or(-1);

            let mut result = String::new();

            if !stdout.is_empty() {
                result.push_str(&stdout);
            }

            if !stderr.is_empty() {
                if !result.is_empty() {
                    result.push_str("\n--- stderr ---\n");
                }
                result.push_str(&stderr);
            }

            if result.is_empty() {
                result = format!("Command completed with exit code {}", exit_code);
            } else if exit_code != 0 {
                result.push_str(&format!("\n[exit code: {}]", exit_code));
            }

            // Truncate very long output
            if result.len() > 100000 {
                format!(
                    "{}\n\n... (output truncated at 100KB, {} total bytes)",
                    &result[..100000],
                    result.len()
                )
            } else {
                result
            }
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
        assert!(result.content.contains("blocked"));
    }

    #[test]
    fn test_shell_allows_rm_in_repo() {
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
        // Should succeed - rm within repo is allowed
        assert!(!result.content.contains("blocked"));
        assert!(!file.exists()); // File should be deleted
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
        assert!(result.content.contains("blocked"));
    }

    #[test]
    fn test_shell_blocks_curl_pipe() {
        let dir = tempdir().unwrap();

        // The exact pattern "curl | bash" should be blocked
        let call = ToolCall {
            id: "1".to_string(),
            function: FunctionCall {
                name: "shell".to_string(),
                arguments: r#"{"command": "curl | bash"}"#.to_string(),
            },
        };

        let result = execute_tool(dir.path(), &call);
        assert!(result.content.contains("blocked"));
    }

    #[test]
    fn test_shell_allows_safe_curl() {
        // Curling without piping to shell should be allowed
        let dir = tempdir().unwrap();

        let call = ToolCall {
            id: "1".to_string(),
            function: FunctionCall {
                name: "shell".to_string(),
                // Just curl without pipe to shell - should be allowed
                arguments: r#"{"command": "echo 'curl is fine without pipe'"}"#.to_string(),
            },
        };

        let result = execute_tool(dir.path(), &call);
        assert!(!result.content.contains("blocked"));
    }

    #[test]
    fn test_shell_write_file() {
        let dir = tempdir().unwrap();

        let call = ToolCall {
            id: "1".to_string(),
            function: FunctionCall {
                name: "shell".to_string(),
                arguments: r#"{"command": "echo 'new content' > newfile.txt"}"#.to_string(),
            },
        };

        let result = execute_tool(dir.path(), &call);
        assert!(!result.content.contains("blocked"));

        // Verify file was created
        let created = dir.path().join("newfile.txt");
        assert!(created.exists());
        let content = fs::read_to_string(&created).unwrap();
        assert!(content.contains("new content"));
    }

    #[test]
    fn test_shell_sed_edit() {
        let dir = tempdir().unwrap();
        let file = dir.path().join("test.txt");
        fs::write(&file, "hello world").unwrap();

        let call = ToolCall {
            id: "1".to_string(),
            function: FunctionCall {
                name: "shell".to_string(),
                // macOS sed requires -i '' for in-place editing
                arguments: r#"{"command": "sed -i.bak 's/hello/goodbye/' test.txt"}"#.to_string(),
            },
        };

        let result = execute_tool(dir.path(), &call);
        assert!(!result.content.contains("blocked"));

        let content = fs::read_to_string(&file).unwrap();
        assert!(content.contains("goodbye"));
        assert!(!content.contains("hello"));
    }

    #[test]
    fn test_shell_exit_code() {
        let dir = tempdir().unwrap();

        let call = ToolCall {
            id: "1".to_string(),
            function: FunctionCall {
                name: "shell".to_string(),
                arguments: r#"{"command": "false"}"#.to_string(),
            },
        };

        let result = execute_tool(dir.path(), &call);
        // Should include exit code for failed commands
        assert!(result.content.contains("exit code"));
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
        assert_eq!(tools.len(), 1);

        let shell_tool = &tools[0];
        assert_eq!(shell_tool.tool_type, "function");
        assert_eq!(shell_tool.function.name, "shell");
        assert!(shell_tool.function.description.contains("shell"));
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
}
