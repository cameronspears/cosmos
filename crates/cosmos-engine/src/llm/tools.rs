//! Agentic tool definitions and execution for LLM-driven code exploration.
//!
//! Philosophy: Support top-down exploration that's naturally token-efficient.
//! Specialized tools enforce efficient patterns; shell is fallback for edge cases.

use cosmos_adapters::util::{resolve_repo_path_allow_new, run_command_with_timeout};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

/// Timeout for shell and search commands (prevents hangs)
const TOOL_TIMEOUT: Duration = Duration::from_secs(10);
const RELACE_PATH_GUIDANCE: &str =
    "Use repo-relative paths like `crates/...` or `.`; do not use absolute filesystem paths.";

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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub strict: Option<bool>,
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

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ReportBackPayload {
    pub explanation: ReportBackExplanation,
    pub files: HashMap<String, Vec<(i64, i64)>>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ReportBackExplanation {
    pub role: String,
    #[serde(default)]
    pub findings: Vec<ReportBackFinding>,
    #[serde(default)]
    pub verified_findings: Vec<ReportBackFinding>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ReportBackFinding {
    pub file: String,
    pub line: usize,
    pub category: String,
    pub criticality: String,
    pub summary: String,
    pub detail: String,
    pub evidence_quote: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
enum ReportBackExplanationWire {
    String(String),
    Object(ReportBackExplanation),
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReportBackPayloadWire {
    explanation: ReportBackExplanationWire,
    files: ReportBackFilesWire,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
enum ReportBackFilesWire {
    Map(HashMap<String, Vec<(i64, i64)>>),
    List(Vec<ReportBackFileEntryWire>),
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReportBackFileEntryWire {
    path: String,
    ranges: Vec<(i64, i64)>,
}

pub fn parse_report_back_payload(args_json: &str) -> Result<ReportBackPayload, String> {
    let parsed: ReportBackPayloadWire = serde_json::from_str(args_json)
        .map_err(|err| format!("Invalid report_back JSON: {err}"))?;

    let explanation = match parsed.explanation {
        ReportBackExplanationWire::String(text) => {
            serde_json::from_str::<ReportBackExplanation>(text.trim()).map_err(|err| {
                format!("report_back.explanation must be valid JSON object: {err}")
            })?
        }
        ReportBackExplanationWire::Object(value) => value,
    };

    let files = match parsed.files {
        ReportBackFilesWire::Map(files) => files,
        ReportBackFilesWire::List(entries) => {
            let mut files: HashMap<String, Vec<(i64, i64)>> = HashMap::new();
            for entry in entries {
                files
                    .entry(entry.path)
                    .or_default()
                    .extend(entry.ranges.into_iter());
            }
            files
        }
    };

    let parsed = ReportBackPayload { explanation, files };

    if parsed.explanation.role.trim().is_empty() {
        return Err("report_back.explanation.role must be non-empty".to_string());
    }

    for (file, ranges) in &parsed.files {
        if file.trim().is_empty() {
            return Err("report_back contains an empty file path".to_string());
        }
        for (start, end) in ranges {
            if *start < 1 {
                return Err(format!(
                    "Invalid report_back range for {file}: start must be >= 1 (got {start})"
                ));
            }
            if *end < *start {
                return Err(format!(
                    "Invalid report_back range for {file}: end ({end}) < start ({start})"
                ));
            }
        }
    }

    Ok(parsed)
}

/// Get all available tool definitions for top-down exploration
pub fn get_tool_definitions() -> Vec<ToolDefinition> {
    vec![
        // 1. TREE - Understand structure first
        ToolDefinition {
            tool_type: "function",
            function: FunctionDefinition {
                name: "tree",
                strict: None,
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
                strict: None,
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
                strict: None,
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
                strict: None,
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
                strict: None,
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

/// Relace-style fast-agentic-search tools used by suggestion generation.
pub fn get_relace_search_tool_definitions() -> Vec<ToolDefinition> {
    vec![
        ToolDefinition {
            tool_type: "function",
            function: FunctionDefinition {
                name: "view_file",
                strict: None,
                description: "Tool for viewing/exploring the contents of existing files\n\nLine numbers are included in the output, indexing at 1. If the output does not include the end of the file, it will be noted after the final output line.\n\nExample (viewing the first 2 lines of a file):\n1   def my_function():\n2       print(\"Hello, World!\")\n... rest of file truncated ...",
                parameters: serde_json::json!({
                    "type": "object",
                    "required": [],
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "Repository-relative path to a file (preferred), e.g. `crates/cosmos-engine/src/llm/agentic.rs`. `/repo/...` paths are also accepted for compatibility."
                        },
                        "file_path": {
                            "type": "string",
                            "description": "Legacy alias for path."
                        },
                        "view_range": {
                            "type": "array",
                            "items": { "type": "integer" },
                            "default": [1, 100],
                            "description": "Range of file lines to view. If not specified, the first 100 lines of the file are shown. If provided, the file will be shown in the indicated line number range, e.g. [11, 12] will show lines 11 and 12. Indexing at 1 to start. Setting `[start_line, -1]` shows all lines from `start_line` to the end of the file."
                        },
                        "range": {
                            "type": "array",
                            "items": { "type": "integer" }
                        },
                        "line": { "type": "integer" },
                        "start_line": { "type": "integer" },
                        "end_line": { "type": "integer" },
                        "line_start": { "type": "integer" },
                        "line_end": { "type": "integer" },
                        "start": { "type": "integer" },
                        "end": { "type": "integer" }
                    },
                    "additionalProperties": false
                }),
            },
        },
        ToolDefinition {
            tool_type: "function",
            function: FunctionDefinition {
                name: "view_directory",
                strict: None,
                description: "Tool for viewing the contents of a directory.\n\n* Lists contents recursively, relative to the input directory\n* Directories are suffixed with a trailing slash '/'\n* Depth might be limited by the tool implementation\n* Output is limited to the first 250 items\n\nExample output:\nfile1.txt\nfile2.txt\nsubdir1/\nsubdir1/file3.txt",
                parameters: serde_json::json!({
                    "type": "object",
                    "required": [],
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "Repository-relative path to a directory (preferred), e.g. `.` or `crates/`. `/repo/...` paths are also accepted for compatibility."
                        },
                        "root_path": {
                            "type": "string",
                            "description": "Legacy alias for path."
                        },
                        "dir": {
                            "type": "string",
                            "description": "Legacy alias for path."
                        },
                        "directory": {
                            "type": "string",
                            "description": "Legacy alias for path."
                        },
                        "include_hidden": {
                            "type": "boolean",
                            "default": false,
                            "description": "If true, include hidden files in the output (false by default)."
                        }
                    },
                    "additionalProperties": false
                }),
            },
        },
        ToolDefinition {
            tool_type: "function",
            function: FunctionDefinition {
                name: "grep_search",
                strict: None,
                description: "Fast text-based regex search that finds exact pattern matches within files or directories, utilizing the ripgrep command for efficient searching. Results will be formatted in the style of ripgrep and can be configured to include line numbers and content. To avoid overwhelming output, the results are capped at 50 matches. Use the include or exclude patterns to filter the search scope by file type or specific paths. This is best for finding exact text matches or regex patterns.",
                parameters: serde_json::json!({
                    "type": "object",
                    "required": ["query"],
                    "properties": {
                        "query": {
                            "type": "string",
                            "description": "The regex pattern to search for"
                        },
                        "case_sensitive": {
                            "type": "boolean",
                            "default": true,
                            "description": "Whether the search should be case sensitive"
                        },
                        "exclude_pattern": {
                            "type": ["string", "null"],
                            "description": "Glob pattern for files to exclude"
                        },
                        "include_pattern": {
                            "type": ["string", "null"],
                            "description": "Glob pattern for files to include (e.g. '*.ts' for TypeScript files)"
                        },
                        "path": {
                            "type": ["string", "null"],
                            "description": "Optional repository-relative path to narrow the search scope."
                        }
                    },
                    "additionalProperties": false
                }),
            },
        },
        ToolDefinition {
            tool_type: "function",
            function: FunctionDefinition {
                name: "search",
                strict: None,
                description: "Compatibility alias for grep_search. Supports either `query` or `pattern` and performs fast regex search within the repository.",
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "query": {
                            "type": "string",
                            "description": "Regex pattern to search for."
                        },
                        "pattern": {
                            "type": "string",
                            "description": "Regex pattern to search for (legacy alias for query)."
                        },
                        "case_sensitive": {
                            "type": "boolean",
                            "default": true
                        },
                        "exclude_pattern": {
                            "type": ["string", "null"]
                        },
                        "include_pattern": {
                            "type": ["string", "null"]
                        },
                        "path": {
                            "type": ["string", "null"]
                        },
                        "context": {
                            "type": "integer",
                            "minimum": 0
                        }
                    },
                    "additionalProperties": false
                }),
            },
        },
        ToolDefinition {
            tool_type: "function",
            function: FunctionDefinition {
                name: "repo_browser.search",
                strict: None,
                description:
                    "Compatibility alias for search/grep_search used by some providers.",
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "query": {
                            "type": "string",
                            "description": "Regex pattern to search for."
                        },
                        "pattern": {
                            "type": "string",
                            "description": "Regex pattern to search for (legacy alias for query)."
                        },
                        "case_sensitive": {
                            "type": "boolean",
                            "default": true
                        },
                        "exclude_pattern": {
                            "type": ["string", "null"]
                        },
                        "include_pattern": {
                            "type": ["string", "null"]
                        },
                        "path": {
                            "type": ["string", "null"]
                        },
                        "context": {
                            "type": "integer",
                            "minimum": 0
                        }
                    },
                    "additionalProperties": false
                }),
            },
        },
        ToolDefinition {
            tool_type: "function",
            function: FunctionDefinition {
                name: "open_file",
                strict: None,
                description: "Compatibility alias for view_file. Opens a file at an optional line range.",
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "path": { "type": "string" },
                        "file_path": { "type": "string" },
                        "view_range": {
                            "type": "array",
                            "items": { "type": "integer" }
                        },
                        "range": {
                            "type": "array",
                            "items": { "type": "integer" }
                        },
                        "line": { "type": "integer" },
                        "start_line": { "type": "integer" },
                        "end_line": { "type": "integer" },
                        "line_start": { "type": "integer" },
                        "line_end": { "type": "integer" },
                        "start": { "type": "integer" },
                        "end": { "type": "integer" }
                    },
                    "additionalProperties": false
                }),
            },
        },
        ToolDefinition {
            tool_type: "function",
            function: FunctionDefinition {
                name: "repo_browser.open_file",
                strict: None,
                description: "Compatibility alias for view_file used by some providers.",
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "path": { "type": "string" },
                        "file_path": { "type": "string" },
                        "view_range": {
                            "type": "array",
                            "items": { "type": "integer" }
                        },
                        "range": {
                            "type": "array",
                            "items": { "type": "integer" }
                        },
                        "line": { "type": "integer" },
                        "start_line": { "type": "integer" },
                        "end_line": { "type": "integer" },
                        "line_start": { "type": "integer" },
                        "line_end": { "type": "integer" },
                        "start": { "type": "integer" },
                        "end": { "type": "integer" }
                    },
                    "additionalProperties": false
                }),
            },
        },
        ToolDefinition {
            tool_type: "function",
            function: FunctionDefinition {
                name: "repo_browser.view_file",
                strict: None,
                description: "Compatibility alias for view_file used by some providers.",
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "path": { "type": "string" },
                        "file_path": { "type": "string" },
                        "view_range": {
                            "type": "array",
                            "items": { "type": "integer" }
                        },
                        "range": {
                            "type": "array",
                            "items": { "type": "integer" }
                        },
                        "line": { "type": "integer" },
                        "start_line": { "type": "integer" },
                        "end_line": { "type": "integer" },
                        "line_start": { "type": "integer" },
                        "line_end": { "type": "integer" },
                        "start": { "type": "integer" },
                        "end": { "type": "integer" }
                    },
                    "additionalProperties": false
                }),
            },
        },
        ToolDefinition {
            tool_type: "function",
            function: FunctionDefinition {
                name: "print_tree",
                strict: None,
                description: "Compatibility alias for view_directory.",
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "path": { "type": "string" },
                        "root_path": { "type": "string" },
                        "dir": { "type": "string" },
                        "directory": { "type": "string" },
                        "include_hidden": {
                            "type": "boolean",
                            "default": false
                        }
                    },
                    "additionalProperties": false
                }),
            },
        },
        ToolDefinition {
            tool_type: "function",
            function: FunctionDefinition {
                name: "repo_browser.view_directory",
                strict: None,
                description: "Compatibility alias for view_directory used by some providers.",
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "path": { "type": "string" },
                        "root_path": { "type": "string" },
                        "dir": { "type": "string" },
                        "directory": { "type": "string" },
                        "include_hidden": {
                            "type": "boolean",
                            "default": false
                        }
                    },
                    "additionalProperties": false
                }),
            },
        },
        ToolDefinition {
            tool_type: "function",
            function: FunctionDefinition {
                name: "repo_browser.print_tree",
                strict: None,
                description: "Compatibility alias for view_directory/print_tree used by some providers.",
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "path": { "type": "string" },
                        "root_path": { "type": "string" },
                        "dir": { "type": "string" },
                        "directory": { "type": "string" },
                        "include_hidden": {
                            "type": "boolean",
                            "default": false
                        }
                    },
                    "additionalProperties": false
                }),
            },
        },
        ToolDefinition {
            tool_type: "function",
            function: FunctionDefinition {
                name: "bash",
                strict: Some(true),
                description: "Tool for executing bash commands.\n\n* Avoid long running commands\n* Avoid dangerous/destructive commands\n* Prefer using other more specialized tools where possible",
                parameters: serde_json::json!({
                    "type": "object",
                    "required": ["command"],
                    "properties": {
                        "command": {
                            "type": "string",
                            "description": "Bash command to execute"
                        }
                    },
                    "additionalProperties": false
                }),
            },
        },
        ToolDefinition {
            tool_type: "function",
            function: FunctionDefinition {
                name: "repo_browser.exec",
                strict: None,
                description:
                    "Compatibility alias for bash/shell execution used by some providers.",
                parameters: serde_json::json!({
                    "type": "object",
                    "required": [],
                    "properties": {
                        "command": {
                            "type": "string",
                            "description": "Bash command to execute"
                        },
                        "cmd": {
                            "type": "string",
                            "description": "Legacy alias for command."
                        }
                    },
                    "additionalProperties": false
                }),
            },
        },
        ToolDefinition {
            tool_type: "function",
            function: FunctionDefinition {
                name: "report_back",
                strict: Some(true),
                description: "This is a tool to use when you feel like you have finished exploring the codebase and understanding the problem, and now would like to report back to the user.",
                parameters: serde_json::json!({
                    "type": "object",
                    "required": ["explanation", "files"],
                    "properties": {
                        "explanation": {
                            "type": "object",
                            "required": ["role", "findings", "verified_findings"],
                            "properties": {
                                "role": {
                                    "type": "string",
                                    "enum": ["bug_hunter", "security_reviewer", "final_reviewer"]
                                },
                                "findings": {
                                    "type": "array",
                                    "items": {
                                        "type": "object",
                                        "required": [
                                            "file",
                                            "line",
                                            "category",
                                            "criticality",
                                            "summary",
                                            "detail",
                                            "evidence_quote"
                                        ],
                                        "properties": {
                                            "file": { "type": "string" },
                                            "line": { "type": "integer", "minimum": 1 },
                                            "category": {
                                                "type": "string",
                                                "enum": ["bug", "security"]
                                            },
                                            "criticality": {
                                                "type": "string",
                                                "enum": ["critical", "high", "medium", "low"]
                                            },
                                            "summary": { "type": "string" },
                                            "detail": { "type": "string" },
                                            "evidence_quote": { "type": "string" }
                                        },
                                        "additionalProperties": false
                                    }
                                },
                                "verified_findings": {
                                    "type": "array",
                                    "items": {
                                        "type": "object",
                                        "required": [
                                            "file",
                                            "line",
                                            "category",
                                            "criticality",
                                            "summary",
                                            "detail",
                                            "evidence_quote"
                                        ],
                                        "properties": {
                                            "file": { "type": "string" },
                                            "line": { "type": "integer", "minimum": 1 },
                                            "category": {
                                                "type": "string",
                                                "enum": ["bug", "security"]
                                            },
                                            "criticality": {
                                                "type": "string",
                                                "enum": ["critical", "high", "medium", "low"]
                                            },
                                            "summary": { "type": "string" },
                                            "detail": { "type": "string" },
                                            "evidence_quote": { "type": "string" }
                                        },
                                        "additionalProperties": false
                                    }
                                }
                            },
                            "additionalProperties": false,
                            "description": "Structured findings payload. bug/security agents fill findings and leave verified_findings empty; final reviewer fills verified_findings and leaves findings empty."
                        },
                        "files": {
                            "type": "array",
                            "items": {
                                "type": "object",
                                "required": ["path", "ranges"],
                                "properties": {
                                    "path": {
                                        "type": "string",
                                        "description": "File path relative to repo root (without /repo prefix)."
                                    },
                                    "ranges": {
                                        "type": "array",
                                        "items": {
                                            "type": "array",
                                            "minItems": 2,
                                            "maxItems": 2,
                                            "prefixItems": [{ "type": "integer" }, { "type": "integer" }]
                                        },
                                        "description": "Line ranges relevant to the report as [start, end] tuples."
                                    }
                                },
                                "additionalProperties": false
                            },
                            "description": "A list of file entries containing path and relevant line ranges."
                        }
                    },
                    "additionalProperties": false
                }),
            },
        },
        ToolDefinition {
            tool_type: "function",
            function: FunctionDefinition {
                name: "repo_browser.report_back",
                strict: Some(true),
                description: "Compatibility alias for report_back used by some providers.",
                parameters: serde_json::json!({
                    "type": "object",
                    "required": ["explanation", "files"],
                    "properties": {
                        "explanation": {
                            "type": "object",
                            "required": ["role", "findings", "verified_findings"],
                            "properties": {
                                "role": {
                                    "type": "string",
                                    "enum": ["bug_hunter", "security_reviewer", "final_reviewer"]
                                },
                                "findings": {
                                    "type": "array",
                                    "items": {
                                        "type": "object",
                                        "required": [
                                            "file",
                                            "line",
                                            "category",
                                            "criticality",
                                            "summary",
                                            "detail",
                                            "evidence_quote"
                                        ],
                                        "properties": {
                                            "file": { "type": "string" },
                                            "line": { "type": "integer", "minimum": 1 },
                                            "category": {
                                                "type": "string",
                                                "enum": [
                                                    "bug",
                                                    "security"
                                                ]
                                            },
                                            "criticality": {
                                                "type": "string",
                                                "enum": [
                                                    "low",
                                                    "medium",
                                                    "high",
                                                    "critical"
                                                ]
                                            },
                                            "summary": { "type": "string" },
                                            "detail": { "type": "string" },
                                            "evidence_quote": { "type": "string" }
                                        },
                                        "additionalProperties": false
                                    }
                                },
                                "verified_findings": {
                                    "type": "array",
                                    "items": {
                                        "type": "object",
                                        "required": [
                                            "file",
                                            "line",
                                            "category",
                                            "criticality",
                                            "summary",
                                            "detail",
                                            "evidence_quote"
                                        ],
                                        "properties": {
                                            "file": { "type": "string" },
                                            "line": { "type": "integer", "minimum": 1 },
                                            "category": {
                                                "type": "string",
                                                "enum": [
                                                    "bug",
                                                    "security"
                                                ]
                                            },
                                            "criticality": {
                                                "type": "string",
                                                "enum": [
                                                    "low",
                                                    "medium",
                                                    "high",
                                                    "critical"
                                                ]
                                            },
                                            "summary": { "type": "string" },
                                            "detail": { "type": "string" },
                                            "evidence_quote": { "type": "string" }
                                        },
                                        "additionalProperties": false
                                    }
                                }
                            },
                            "additionalProperties": false
                        },
                        "files": {
                            "type": "array",
                            "items": {
                                "type": "object",
                                "required": ["path", "ranges"],
                                "properties": {
                                    "path": {
                                        "type": "string",
                                        "description": "File path relative to repo root (without /repo prefix)."
                                    },
                                    "ranges": {
                                        "type": "array",
                                        "items": {
                                            "type": "array",
                                            "minItems": 2,
                                            "maxItems": 2,
                                            "prefixItems": [{ "type": "integer" }, { "type": "integer" }]
                                        },
                                        "description": "Line ranges relevant to the report as [start, end] tuples."
                                    }
                                },
                                "additionalProperties": false
                            },
                            "description": "A list of file entries containing path and relevant line ranges."
                        }
                    },
                    "additionalProperties": false
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
        "search" | "repo_browser.search" => {
            execute_search_alias(root, &tool_call.function.arguments)
        }
        "read_range" => execute_read_range(root, &tool_call.function.arguments),
        "shell" => execute_shell(root, &tool_call.function.arguments),
        "view_file" | "open_file" | "repo_browser.open_file" | "repo_browser.view_file" => {
            execute_open_file_alias(root, &tool_call.function.arguments)
        }
        "view_directory"
        | "print_tree"
        | "repo_browser.view_directory"
        | "repo_browser.print_tree" => {
            execute_print_tree_alias(root, &tool_call.function.arguments)
        }
        "grep_search" => execute_grep_search(root, &tool_call.function.arguments),
        "bash" | "repo_browser.exec" => execute_bash(root, &tool_call.function.arguments),
        "report_back" | "repo_browser.report_back" => {
            execute_report_back(&tool_call.function.arguments)
        }
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

fn resolve_relace_path(root: &Path, raw: &str) -> Result<std::path::PathBuf, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(format!(
            "Invalid path '{}': path must not be empty. {}",
            raw, RELACE_PATH_GUIDANCE
        ));
    }

    let repo_relative = if trimmed == "/repo" || trimmed == "/repo/" {
        "."
    } else if let Some(stripped) = trimmed.strip_prefix("/repo/") {
        stripped
    } else {
        trimmed
    };

    if repo_relative == "." {
        return root
            .canonicalize()
            .map_err(|err| normalize_relace_path_error(raw, &err.to_string()));
    }

    resolve_repo_path_allow_new(root, std::path::Path::new(repo_relative))
        .map(|resolved| resolved.absolute)
        .map_err(|err| normalize_relace_path_error(raw, &err))
}

fn normalize_relace_path_error(raw: &str, err: &str) -> String {
    let trimmed_err = err.trim().trim_end_matches('.');
    format!(
        "Invalid path '{}': {}. {}",
        raw, trimmed_err, RELACE_PATH_GUIDANCE
    )
}

fn execute_view_file(root: &Path, args_json: &str) -> String {
    #[derive(Deserialize)]
    struct ViewFileArgs {
        path: String,
        view_range: Vec<i64>,
    }

    let args: ViewFileArgs = match serde_json::from_str(args_json) {
        Ok(value) => value,
        Err(err) => return format!("Invalid arguments: {err}"),
    };

    if args.view_range.len() != 2 {
        return "Invalid view_range: expected [start_line, end_line]".to_string();
    }

    let target = match resolve_relace_path(root, &args.path) {
        Ok(path) => path,
        Err(err) => return err,
    };

    if !target.exists() || !target.is_file() {
        return format!("File not found: {}", args.path);
    }

    let start_line = args.view_range[0].max(1) as usize;
    let raw_end = args.view_range[1];

    let content = match fs::read_to_string(&target) {
        Ok(value) => value,
        Err(err) => return format!("Failed to read file: {err}"),
    };

    let lines: Vec<&str> = content.lines().collect();
    let total_lines = lines.len();
    if total_lines == 0 {
        return String::new();
    }

    let start = start_line
        .saturating_sub(1)
        .min(total_lines.saturating_sub(1));
    let end = if raw_end == -1 {
        total_lines
    } else {
        raw_end.max(start_line as i64) as usize
    }
    .min(total_lines);

    let mut output = String::new();
    for (idx, line) in lines[start..end].iter().enumerate() {
        output.push_str(&format!("{}   {}\n", start + idx + 1, line));
    }
    if end < total_lines {
        output.push_str("... rest of file truncated ...");
    }
    output
}

fn execute_view_directory(root: &Path, args_json: &str) -> String {
    #[derive(Deserialize)]
    struct ViewDirectoryArgs {
        path: String,
        include_hidden: bool,
    }

    let args: ViewDirectoryArgs = match serde_json::from_str(args_json) {
        Ok(value) => value,
        Err(err) => return format!("Invalid arguments: {err}"),
    };

    let target = match resolve_relace_path(root, &args.path) {
        Ok(path) => path,
        Err(err) => return err,
    };
    if !target.exists() || !target.is_dir() {
        return format!("Directory not found: {}", args.path);
    }

    let mut entries = Vec::new();
    let mut stack = vec![target.clone()];

    while let Some(dir) = stack.pop() {
        let read_dir = match fs::read_dir(&dir) {
            Ok(value) => value,
            Err(_) => continue,
        };
        for entry in read_dir.filter_map(|item| item.ok()) {
            let path = entry.path();
            let rel = match path.strip_prefix(&target) {
                Ok(value) => value,
                Err(_) => continue,
            };
            let rel_display = rel.to_string_lossy().replace('\\', "/");
            if rel_display.is_empty() {
                continue;
            }
            if !args.include_hidden
                && rel
                    .components()
                    .any(|component| component.as_os_str().to_string_lossy().starts_with('.'))
            {
                continue;
            }

            if path.is_dir() {
                entries.push(format!("{}/", rel_display.trim_end_matches('/')));
                stack.push(path);
            } else {
                entries.push(rel_display);
            }
        }
    }

    entries.sort();
    entries.truncate(250);
    entries.join("\n")
}

fn execute_search_alias(root: &Path, args_json: &str) -> String {
    #[derive(Deserialize, Default)]
    struct SearchAliasArgs {
        query: Option<String>,
        pattern: Option<String>,
        path: Option<String>,
        case_sensitive: Option<bool>,
        exclude_pattern: Option<String>,
        include_pattern: Option<String>,
    }

    let args: SearchAliasArgs = match serde_json::from_str(args_json) {
        Ok(value) => value,
        Err(err) => return format!("Invalid arguments: {err}"),
    };

    let has_grep_specific_args = args.query.is_some()
        || args.case_sensitive.is_some()
        || args.exclude_pattern.is_some()
        || args.include_pattern.is_some();

    if has_grep_specific_args {
        let query = args
            .query
            .as_deref()
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .map(str::to_string)
            .or_else(|| {
                args.pattern
                    .as_deref()
                    .map(str::trim)
                    .filter(|text| !text.is_empty())
                    .map(str::to_string)
            });
        let Some(query) = query else {
            return "Invalid arguments: expected non-empty query or pattern".to_string();
        };
        let normalized = serde_json::json!({
            "query": query,
            "case_sensitive": args.case_sensitive.unwrap_or(true),
            "exclude_pattern": args.exclude_pattern,
            "include_pattern": args.include_pattern,
            "path": args.path,
        });
        return execute_grep_search(root, &normalized.to_string());
    }

    execute_search(root, args_json)
}

fn execute_open_file_alias(root: &Path, args_json: &str) -> String {
    #[derive(Deserialize, Default)]
    struct OpenFileAliasArgs {
        path: Option<String>,
        file_path: Option<String>,
        view_range: Option<Vec<i64>>,
        range: Option<Vec<i64>>,
        line: Option<i64>,
        start_line: Option<i64>,
        end_line: Option<i64>,
        line_start: Option<i64>,
        line_end: Option<i64>,
        start: Option<i64>,
        end: Option<i64>,
    }

    let args: OpenFileAliasArgs = match serde_json::from_str(args_json) {
        Ok(value) => value,
        Err(err) => return format!("Invalid arguments: {err}"),
    };

    let path = [args.path.as_deref(), args.file_path.as_deref()]
        .into_iter()
        .flatten()
        .map(str::trim)
        .find(|value| !value.is_empty())
        .map(str::to_string);
    let Some(path) = path else {
        return "Invalid arguments: expected a non-empty path".to_string();
    };

    let view_range = if let Some(range) = args.view_range.or(args.range) {
        if range.len() != 2 {
            return "Invalid arguments: view_range/range must contain exactly two integers"
                .to_string();
        }
        range
    } else if let Some(line) = args.line {
        vec![line, line]
    } else {
        let start = args
            .start_line
            .or(args.line_start)
            .or(args.start)
            .unwrap_or(1);
        let end = args
            .end_line
            .or(args.line_end)
            .or(args.end)
            .unwrap_or_else(|| start.saturating_add(99));
        vec![start, end]
    };

    let normalized = serde_json::json!({
        "path": path,
        "view_range": view_range,
    });
    execute_view_file(root, &normalized.to_string())
}

fn execute_print_tree_alias(root: &Path, args_json: &str) -> String {
    #[derive(Deserialize, Default)]
    struct PrintTreeAliasArgs {
        path: Option<String>,
        root_path: Option<String>,
        dir: Option<String>,
        directory: Option<String>,
        include_hidden: Option<bool>,
    }

    let args: PrintTreeAliasArgs = match serde_json::from_str(args_json) {
        Ok(value) => value,
        Err(err) => return format!("Invalid arguments: {err}"),
    };

    let path = [
        args.path.as_deref(),
        args.root_path.as_deref(),
        args.dir.as_deref(),
        args.directory.as_deref(),
    ]
    .into_iter()
    .flatten()
    .map(str::trim)
    .find(|value| !value.is_empty())
    .map(str::to_string)
    .unwrap_or_else(|| ".".to_string());

    let normalized = serde_json::json!({
        "path": path,
        "include_hidden": args.include_hidden.unwrap_or(false),
    });
    execute_view_directory(root, &normalized.to_string())
}

fn execute_grep_search(root: &Path, args_json: &str) -> String {
    #[derive(Deserialize)]
    struct GrepSearchArgs {
        query: String,
        case_sensitive: Option<bool>,
        exclude_pattern: Option<String>,
        include_pattern: Option<String>,
        path: Option<String>,
    }

    let args: GrepSearchArgs = match serde_json::from_str(args_json) {
        Ok(value) => value,
        Err(err) => return format!("Invalid arguments: {err}"),
    };

    let query = args.query.trim();
    if query.is_empty() {
        return "Invalid arguments: query must not be empty".to_string();
    }
    if let Err(err) = is_safe_regex_pattern(query) {
        return err;
    }
    let search_root = match args
        .path
        .as_deref()
        .map(str::trim)
        .filter(|path| !path.is_empty())
    {
        Some(path) => match resolve_relace_path(root, path) {
            Ok(value) => value,
            Err(err) => return err,
        },
        None => root.to_path_buf(),
    };

    let mut cmd = Command::new("rg");
    cmd.arg("--line-number")
        .arg("--no-heading")
        .arg("--color=never")
        .arg("--max-count=50");

    if !args.case_sensitive.unwrap_or(true) {
        cmd.arg("-i");
    }
    if let Some(pattern) = args.exclude_pattern.as_deref() {
        if !pattern.trim().is_empty() {
            cmd.arg("--glob").arg(format!("!{}", pattern.trim()));
        }
    }
    if let Some(pattern) = args.include_pattern.as_deref() {
        if !pattern.trim().is_empty() {
            cmd.arg("--glob").arg(pattern.trim());
        }
    }

    cmd.arg(query).arg(&search_root).current_dir(root);

    match run_command_with_timeout(&mut cmd, TOOL_TIMEOUT) {
        Ok(result) => {
            if result.timed_out {
                return "Search timed out after 10 seconds. Try a more specific pattern."
                    .to_string();
            }
            if result.stdout.trim().is_empty() {
                "No matches found".to_string()
            } else {
                truncate_output(result.stdout)
            }
        }
        Err(err) => format!("Search failed: {err}"),
    }
}

fn execute_bash(root: &Path, args_json: &str) -> String {
    execute_shell(root, args_json)
}

fn execute_report_back(args_json: &str) -> String {
    match parse_report_back_payload(args_json) {
        Ok(payload) => format!(
            "report_back accepted (role={}, findings={}, verified_findings={}, files={})",
            payload.explanation.role,
            payload.explanation.findings.len(),
            payload.explanation.verified_findings.len(),
            payload.files.len(),
        ),
        Err(err) => format!("Invalid report_back payload: {err}"),
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
    #[derive(Deserialize, Default)]
    struct ShellArgs {
        #[serde(default)]
        command: Option<String>,
        #[serde(default)]
        cmd: Option<String>,
    }

    let args: ShellArgs = match serde_json::from_str(args_json) {
        Ok(a) => a,
        Err(e) => return format!("Invalid arguments: {}", e),
    };

    let command = args
        .command
        .as_deref()
        .or(args.cmd.as_deref())
        .map(str::trim)
        .unwrap_or("");
    if command.is_empty() {
        return "Invalid arguments: missing command".to_string();
    }

    // Check for dangerous shell metacharacters that could bypass the allowlist
    if contains_dangerous_chars(command) {
        return "Command blocked: contains shell metacharacters that could bypass security. \
             Avoid using: backticks, $, ;, &, or newlines. \
             Use pipes (|) for chaining allowed commands."
            .to_string();
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
    fn test_relace_tool_definitions_match_expected_names() {
        let tools = get_relace_search_tool_definitions();
        let names = tools
            .iter()
            .map(|tool| tool.function.name)
            .collect::<Vec<_>>();
        assert_eq!(
            names,
            vec![
                "view_file",
                "view_directory",
                "grep_search",
                "search",
                "repo_browser.search",
                "open_file",
                "repo_browser.open_file",
                "repo_browser.view_file",
                "print_tree",
                "repo_browser.view_directory",
                "repo_browser.print_tree",
                "bash",
                "repo_browser.exec",
                "report_back",
                "repo_browser.report_back"
            ]
        );
        let strict_by_name = tools
            .iter()
            .map(|tool| (tool.function.name, tool.function.strict))
            .collect::<std::collections::HashMap<_, _>>();
        assert_eq!(strict_by_name.get("report_back"), Some(&Some(true)));
        assert_eq!(
            strict_by_name.get("repo_browser.report_back"),
            Some(&Some(true))
        );
        assert_eq!(strict_by_name.get("view_file"), Some(&None));
        assert_eq!(strict_by_name.get("view_directory"), Some(&None));
        assert_eq!(strict_by_name.get("repo_browser.exec"), Some(&None));
    }

    #[test]
    fn test_relace_tool_required_params_match_expected_schema() {
        let tools = get_relace_search_tool_definitions();
        let mut required_by_tool = std::collections::HashMap::new();
        for tool in tools {
            let required = tool
                .function
                .parameters
                .get("required")
                .and_then(|v| v.as_array())
                .map(|required| {
                    required
                        .iter()
                        .filter_map(|v| v.as_str().map(str::to_string))
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            required_by_tool.insert(tool.function.name, required);
        }

        assert_eq!(
            required_by_tool.get("view_file"),
            Some(&Vec::<String>::new())
        );
        assert_eq!(
            required_by_tool.get("view_directory"),
            Some(&Vec::<String>::new())
        );
        assert_eq!(
            required_by_tool.get("grep_search"),
            Some(&vec!["query".to_string()])
        );
        assert_eq!(required_by_tool.get("search"), Some(&Vec::<String>::new()));
        assert_eq!(
            required_by_tool.get("open_file"),
            Some(&Vec::<String>::new())
        );
        assert_eq!(
            required_by_tool.get("repo_browser.search"),
            Some(&Vec::<String>::new())
        );
        assert_eq!(
            required_by_tool.get("repo_browser.open_file"),
            Some(&Vec::<String>::new())
        );
        assert_eq!(
            required_by_tool.get("repo_browser.view_file"),
            Some(&Vec::<String>::new())
        );
        assert_eq!(
            required_by_tool.get("print_tree"),
            Some(&Vec::<String>::new())
        );
        assert_eq!(
            required_by_tool.get("repo_browser.view_directory"),
            Some(&Vec::<String>::new())
        );
        assert_eq!(
            required_by_tool.get("repo_browser.print_tree"),
            Some(&Vec::<String>::new())
        );
        assert_eq!(
            required_by_tool.get("bash"),
            Some(&vec!["command".to_string()])
        );
        assert_eq!(
            required_by_tool.get("repo_browser.exec"),
            Some(&Vec::<String>::new())
        );
        assert_eq!(
            required_by_tool.get("report_back"),
            Some(&vec!["explanation".to_string(), "files".to_string()])
        );
        assert_eq!(
            required_by_tool.get("repo_browser.report_back"),
            Some(&vec!["explanation".to_string(), "files".to_string()])
        );
    }

    #[test]
    fn test_report_back_explanation_schema_is_object() {
        let tools = get_relace_search_tool_definitions();
        let report_back = tools
            .into_iter()
            .find(|tool| tool.function.name == "report_back")
            .expect("report_back tool should exist");
        let explanation_type = report_back
            .function
            .parameters
            .get("properties")
            .and_then(|p| p.get("explanation"))
            .and_then(|e| e.get("type"))
            .and_then(|t| t.as_str())
            .expect("explanation type should be present");
        assert_eq!(explanation_type, "object");
    }

    #[test]
    fn test_parse_report_back_payload_strict_validation() {
        let ok = parse_report_back_payload(
            r#"{"explanation":"{\"role\":\"x\"}","files":{"src/lib.rs":[[10,20]]}}"#,
        )
        .expect("valid payload");
        assert_eq!(ok.files.len(), 1);
        assert_eq!(ok.explanation.role, "x");

        let ok_object_explanation = parse_report_back_payload(
            r#"{"explanation":{"role":"bug_hunter","findings":[],"verified_findings":[]},"files":{"src/lib.rs":[[10,20]]}}"#,
        )
        .expect("valid object explanation payload");
        assert_eq!(ok_object_explanation.explanation.role, "bug_hunter");

        let ok_list = parse_report_back_payload(
            r#"{"explanation":"{\"role\":\"x\"}","files":[{"path":"src/main.rs","ranges":[[4,7]]}]}"#,
        )
        .expect("valid list payload");
        assert_eq!(ok_list.files.len(), 1);
        assert!(ok_list.files.contains_key("src/main.rs"));

        let err =
            parse_report_back_payload(r#"{"explanation":" ","files":{"src/lib.rs":[[0,20]]}}"#)
                .expect_err("invalid explanation should fail");
        assert!(err.contains("valid JSON object"));

        let err_role = parse_report_back_payload(
            r#"{"explanation":{"role":" ","findings":[],"verified_findings":[]},"files":{"src/lib.rs":[[10,20]]}}"#,
        )
        .expect_err("empty role should fail");
        assert!(err_role.contains("role must be non-empty"));
    }

    #[test]
    fn test_resolve_relace_path_accepts_repo_relative_and_repo_alias_paths() {
        let dir = tempdir().expect("tempdir");
        fs::create_dir_all(dir.path().join("crates")).expect("create crates dir");

        let root = dir.path();
        let canonical_root = root.canonicalize().expect("canonicalize root");
        let resolved_dot = resolve_relace_path(root, ".").expect("dot path should resolve");
        assert_eq!(resolved_dot, canonical_root);

        let resolved_relative =
            resolve_relace_path(root, "crates/").expect("repo-relative path should resolve");
        assert_eq!(resolved_relative, canonical_root.join("crates"));

        let resolved_repo = resolve_relace_path(root, "/repo").expect("/repo should resolve");
        assert_eq!(resolved_repo, canonical_root);

        let resolved_repo_nested =
            resolve_relace_path(root, "/repo/crates/").expect("/repo/... should resolve");
        assert_eq!(resolved_repo_nested, canonical_root.join("crates"));
    }

    #[test]
    fn test_resolve_relace_path_error_includes_canonical_guidance() {
        let dir = tempdir().expect("tempdir");
        let err = resolve_relace_path(dir.path(), "/etc/passwd")
            .expect_err("absolute path outside /repo alias should fail");
        assert!(err.contains("Invalid path '/etc/passwd'"));
        assert!(err.contains("Use repo-relative paths like `crates/...` or `.`"));
    }

    #[test]
    fn test_search_alias_maps_query_to_grep_search() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("test.rs"), "fn hello_world() {}\n").unwrap();
        let call = ToolCall {
            id: "1".to_string(),
            function: FunctionCall {
                name: "search".to_string(),
                arguments: r#"{"query":"hello_world"}"#.to_string(),
            },
        };
        let result = execute_tool(dir.path(), &call);
        assert!(result.content.contains("hello_world"));
    }

    #[test]
    fn test_open_file_alias_maps_to_view_file() {
        let dir = tempdir().unwrap();
        fs::write(
            dir.path().join("test.rs"),
            "line1\nline2\nline3\nline4\nline5\n",
        )
        .unwrap();
        let call = ToolCall {
            id: "1".to_string(),
            function: FunctionCall {
                name: "open_file".to_string(),
                arguments: r#"{"path":"test.rs","start_line":2,"end_line":3}"#.to_string(),
            },
        };
        let result = execute_tool(dir.path(), &call);
        assert!(result.content.contains("2   line2"));
        assert!(result.content.contains("3   line3"));
    }

    #[test]
    fn test_repo_browser_open_file_alias_maps_to_view_file() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("test.rs"), "alpha\nbeta\ngamma\n").unwrap();
        let call = ToolCall {
            id: "1".to_string(),
            function: FunctionCall {
                name: "repo_browser.open_file".to_string(),
                arguments: r#"{"file_path":"test.rs","line":2}"#.to_string(),
            },
        };
        let result = execute_tool(dir.path(), &call);
        assert!(result.content.contains("2   beta"));
    }

    #[test]
    fn test_repo_browser_view_file_alias_maps_to_view_file() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("test.rs"), "alpha\nbeta\ngamma\n").unwrap();
        let call = ToolCall {
            id: "1".to_string(),
            function: FunctionCall {
                name: "repo_browser.view_file".to_string(),
                arguments: r#"{"path":"test.rs","line":3}"#.to_string(),
            },
        };
        let result = execute_tool(dir.path(), &call);
        assert!(result.content.contains("3   gamma"));
    }

    #[test]
    fn test_repo_browser_search_alias_maps_to_search() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("test.rs"), "fn hello_world() {}\n").unwrap();
        let call = ToolCall {
            id: "1".to_string(),
            function: FunctionCall {
                name: "repo_browser.search".to_string(),
                arguments: r#"{"query":"hello_world"}"#.to_string(),
            },
        };
        let result = execute_tool(dir.path(), &call);
        assert!(result.content.contains("hello_world"));
    }

    #[test]
    fn test_print_tree_alias_maps_to_view_directory() {
        let dir = tempdir().unwrap();
        fs::create_dir_all(dir.path().join("src/nested")).unwrap();
        fs::write(dir.path().join("src/lib.rs"), "pub fn x() {}\n").unwrap();
        let call = ToolCall {
            id: "1".to_string(),
            function: FunctionCall {
                name: "print_tree".to_string(),
                arguments: r#"{"path":"src"}"#.to_string(),
            },
        };
        let result = execute_tool(dir.path(), &call);
        assert!(result.content.contains("lib.rs"));
        assert!(result.content.contains("nested/"));
    }

    #[test]
    fn test_repo_browser_report_back_alias_maps_to_report_back() {
        let dir = tempdir().unwrap();
        let call = ToolCall {
            id: "1".to_string(),
            function: FunctionCall {
                name: "repo_browser.report_back".to_string(),
                arguments: r#"{"explanation":{"role":"bug_hunter","findings":[],"verified_findings":[]},"files":[]}"#
                    .to_string(),
            },
        };
        let result = execute_tool(dir.path(), &call);
        assert!(result.content.contains("report_back accepted"));
    }

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
