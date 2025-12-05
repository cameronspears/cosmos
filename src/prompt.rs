//! AI Prompt Builder - Generate contextual prompts for AI-assisted fixes
//!
//! This module creates rich, actionable prompts that can be copied to clipboard
//! and used with AI coding assistants to fix identified issues.
//!
//! Key principle: Give the AI EVERYTHING it needs to understand and fix the issue.
//! Abstract metrics are useless without the actual code and context.

use crate::analysis::{
    BusFactorRisk, ChurnEntry, DangerZone, DustyFile, FileComplexity, TestCoverage, TodoEntry,
};
use arboard::Clipboard;
use std::fs;
use std::path::Path;

/// The type of issue being addressed
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IssueType {
    DangerZone,
    HighChurn,
    DustyFile,
    TodoItem,
    BusFactorRisk,
    MissingTests,
}

impl IssueType {
    pub fn label(&self) -> &'static str {
        match self {
            IssueType::DangerZone => "DANGER ZONE",
            IssueType::HighChurn => "HIGH CHURN",
            IssueType::DustyFile => "STALE CODE",
            IssueType::TodoItem => "TODO/FIXME",
            IssueType::BusFactorRisk => "BUS FACTOR RISK",
            IssueType::MissingTests => "MISSING TESTS",
        }
    }

    pub fn emoji(&self) -> &'static str {
        match self {
            IssueType::DangerZone => "🔥",
            IssueType::HighChurn => "⚡",
            IssueType::DustyFile => "🕸️",
            IssueType::TodoItem => "📝",
            IssueType::BusFactorRisk => "🚌",
            IssueType::MissingTests => "🧪",
        }
    }
}

/// Context gathered about a file from various analyzers
#[derive(Debug, Clone, Default)]
pub struct FileContext {
    pub path: String,
    pub repo_root: Option<String>,
    pub issue_type: Option<IssueType>,
    
    // The actual file content!
    pub file_content: Option<String>,
    
    // From DangerZone
    pub danger_score: Option<f64>,
    pub change_count: Option<usize>,
    pub complexity_score: Option<f64>,
    pub danger_reason: Option<String>,
    
    // From Complexity analysis
    pub loc: Option<usize>,
    pub function_count: Option<usize>,
    pub max_function_length: Option<usize>,
    
    // From ChurnEntry
    pub days_active: Option<i64>,
    
    // From DustyFile
    pub days_since_change: Option<i64>,
    
    // From BusFactorRisk
    pub primary_author: Option<String>,
    pub primary_author_pct: Option<f64>,
    pub bus_risk_reason: Option<String>,
    
    // From TestCoverage
    pub has_tests: Option<bool>,
    pub test_files: Vec<String>,
    
    // All TODOs in this file
    pub todos_in_file: Vec<(usize, String, String)>, // (line, kind, text)
    
    // From TodoEntry (if specific)
    pub todo_text: Option<String>,
    pub todo_line: Option<usize>,
    
    // Recent git changes (commit messages)
    pub recent_commits: Vec<String>,
}

impl FileContext {
    pub fn new(path: &str) -> Self {
        Self {
            path: path.to_string(),
            ..Default::default()
        }
    }

    pub fn with_repo_root(mut self, root: &str) -> Self {
        self.repo_root = Some(root.to_string());
        self
    }

    /// Load the actual file content
    pub fn load_file_content(&mut self) {
        let full_path = if let Some(ref root) = self.repo_root {
            Path::new(root).join(&self.path)
        } else {
            Path::new(&self.path).to_path_buf()
        };

        if let Ok(content) = fs::read_to_string(&full_path) {
            // Truncate very large files but keep enough context
            if content.len() > 15000 {
                let truncated: String = content.chars().take(15000).collect();
                self.file_content = Some(format!(
                    "{}\n\n... [truncated - file has {} total characters] ...",
                    truncated,
                    content.len()
                ));
            } else {
                self.file_content = Some(content);
            }
        }
    }

    /// Collect all TODOs from this file
    pub fn with_todos_from_list(mut self, all_todos: &[TodoEntry]) -> Self {
        self.todos_in_file = all_todos
            .iter()
            .filter(|t| t.path == self.path)
            .map(|t| (t.line_number, t.kind.as_str().to_string(), t.text.clone()))
            .collect();
        self
    }

    pub fn with_danger_zone(mut self, dz: &DangerZone) -> Self {
        self.issue_type = Some(IssueType::DangerZone);
        self.danger_score = Some(dz.danger_score);
        self.change_count = Some(dz.change_count);
        self.complexity_score = Some(dz.complexity_score);
        self.danger_reason = Some(dz.reason.clone());
        self
    }

    pub fn with_churn(mut self, entry: &ChurnEntry) -> Self {
        if self.issue_type.is_none() {
            self.issue_type = Some(IssueType::HighChurn);
        }
        self.change_count = Some(entry.change_count);
        self.days_active = Some(entry.days_active);
        self
    }

    pub fn with_complexity(mut self, fc: &FileComplexity) -> Self {
        self.loc = Some(fc.loc);
        self.function_count = Some(fc.function_count);
        self.max_function_length = Some(fc.max_function_length);
        self.complexity_score = Some(fc.complexity_score);
        self
    }

    pub fn with_dusty(mut self, df: &DustyFile) -> Self {
        if self.issue_type.is_none() {
            self.issue_type = Some(IssueType::DustyFile);
        }
        self.days_since_change = Some(df.days_since_change);
        self.loc = Some(df.line_count);
        self
    }

    pub fn with_bus_factor(mut self, bf: &BusFactorRisk) -> Self {
        if self.issue_type.is_none() {
            self.issue_type = Some(IssueType::BusFactorRisk);
        }
        self.primary_author = Some(bf.primary_author.clone());
        self.primary_author_pct = Some(bf.primary_author_pct);
        self.bus_risk_reason = Some(bf.risk_reason.clone());
        self
    }

    pub fn with_test_coverage(mut self, tc: &TestCoverage) -> Self {
        if !tc.has_tests && self.issue_type.is_none() {
            self.issue_type = Some(IssueType::MissingTests);
        }
        self.has_tests = Some(tc.has_tests);
        self.test_files = tc.test_files.clone();
        self.loc = Some(tc.source_line_count);
        self
    }

    pub fn with_todo(mut self, todo: &TodoEntry) -> Self {
        self.issue_type = Some(IssueType::TodoItem);
        self.todo_text = Some(todo.text.clone());
        self.todo_line = Some(todo.line_number);
        self
    }

    /// Generate a summary of the issue for AI fix generation
    pub fn issue_summary(&self) -> String {
        let issue_type = self.issue_type.as_ref()
            .map(|t| format!("{:?}", t))
            .unwrap_or_else(|| "Unknown issue".to_string());
        
        let mut parts = vec![issue_type];
        
        if let Some(score) = self.danger_score {
            parts.push(format!("danger score: {:.0}", score));
        }
        if let Some(count) = self.change_count {
            parts.push(format!("changed {} times", count));
        }
        if let Some(complexity) = self.complexity_score {
            parts.push(format!("complexity: {:.1}", complexity));
        }
        if let Some(loc) = self.loc {
            parts.push(format!("{} lines", loc));
        }
        if let Some(ref reason) = self.danger_reason {
            parts.push(reason.clone());
        }
        if let Some(ref reason) = self.bus_risk_reason {
            parts.push(reason.clone());
        }
        if let Some(days) = self.days_since_change {
            parts.push(format!("not changed in {} days", days));
        }
        if let Some(ref text) = self.todo_text {
            parts.push(format!("TODO: {}", text));
        }
        
        parts.join(", ")
    }
}

/// Prompt builder that generates AI-ready prompts
pub struct PromptBuilder {
    clipboard: Option<Clipboard>,
}

impl PromptBuilder {
    pub fn new() -> Self {
        let clipboard = Clipboard::new().ok();
        Self { clipboard }
    }

    /// Generate a prompt for a single file issue
    pub fn generate(&self, ctx: &FileContext) -> String {
        let issue_type = ctx.issue_type.unwrap_or(IssueType::DangerZone);
        let mut prompt = String::new();

        // Header
        prompt.push_str(&format!(
            "# {} {} - `{}`\n\n",
            issue_type.emoji(),
            issue_type.label(),
            ctx.path
        ));

        // What this analysis found and WHY it matters
        prompt.push_str("## What Was Found\n\n");
        prompt.push_str(&self.generate_what_was_found(ctx));
        prompt.push_str("\n\n");

        // Show TODOs/FIXMEs in this file if any
        if !ctx.todos_in_file.is_empty() {
            prompt.push_str("## TODOs & FIXMEs in This File\n\n");
            prompt.push_str("The following markers were found that may indicate known issues:\n\n");
            for (line, kind, text) in &ctx.todos_in_file {
                prompt.push_str(&format!("- **Line {}** `{}`: {}\n", line, kind, text));
            }
            prompt.push_str("\n");
        }

        // The actual task
        prompt.push_str("## Your Task\n\n");
        prompt.push_str(&self.generate_task(ctx));
        prompt.push_str("\n\n");

        // THE ACTUAL FILE CONTENT - this is the key!
        prompt.push_str("## File Content\n\n");
        prompt.push_str(&format!("Here is the current content of `{}`:\n\n", ctx.path));
        
        if let Some(ref content) = ctx.file_content {
            // Detect language for syntax highlighting
            let lang = detect_language(&ctx.path);
            prompt.push_str(&format!("```{}\n{}\n```\n\n", lang, content));
        } else {
            prompt.push_str("*File content not available. Please provide the file content or ensure the file path is correct.*\n\n");
        }

        // Guidelines at the end
        prompt.push_str("## Guidelines\n\n");
        prompt.push_str(&self.generate_guidelines(ctx));

        prompt
    }


    /// Generate explanation of what was found and WHY it matters
    fn generate_what_was_found(&self, ctx: &FileContext) -> String {
        let issue_type = ctx.issue_type.unwrap_or(IssueType::DangerZone);
        let mut explanation = String::new();

        match issue_type {
            IssueType::DangerZone => {
                explanation.push_str("This file was flagged as a **danger zone** by static analysis. Here's what that means:\n\n");
                
                explanation.push_str("**Why this file is risky:**\n");
                explanation.push_str("- It's frequently modified (high \"churn\") AND has high complexity\n");
                explanation.push_str("- Files with this combination are statistically more likely to contain bugs\n");
                explanation.push_str("- Each change risks introducing regressions due to the complexity\n\n");

                explanation.push_str("**The numbers:**\n");
                if let Some(changes) = ctx.change_count {
                    explanation.push_str(&format!("- **{} changes** in the analysis window - this file is a hotspot of activity\n", changes));
                }
                if let Some(complexity) = ctx.complexity_score {
                    let complexity_meaning = if complexity > 15.0 {
                        "very high - likely has deeply nested logic, long functions, or many branches"
                    } else if complexity > 8.0 {
                        "high - probably has some long functions or complex conditionals"
                    } else {
                        "moderate - but combined with high churn, still concerning"
                    };
                    explanation.push_str(&format!("- **Complexity score: {:.1}** - {}\n", complexity, complexity_meaning));
                }
                if let Some(max_fn) = ctx.max_function_length {
                    if max_fn > 50 {
                        explanation.push_str(&format!("- **Longest function: {} lines** - functions over 30-40 lines are hard to reason about\n", max_fn));
                    }
                }
                if let Some(loc) = ctx.loc {
                    explanation.push_str(&format!("- **{} lines of code** total\n", loc));
                }
                if ctx.has_tests == Some(false) {
                    explanation.push_str("- **⚠️ NO TEST COVERAGE** - changes can't be validated automatically\n");
                }
            }
            IssueType::HighChurn => {
                explanation.push_str("This file has **unusually high churn** (frequent changes). Here's what that means:\n\n");
                
                if let Some(changes) = ctx.change_count {
                    explanation.push_str(&format!("**{} changes** were made to this file recently.\n\n", changes));
                }
                
                explanation.push_str("**Why high churn matters:**\n");
                explanation.push_str("- Frequent changes often indicate the code is unstable or unclear\n");
                explanation.push_str("- It may be a \"catch-all\" file that's doing too much\n");
                explanation.push_str("- Could indicate requirements churn or poor initial design\n");
                explanation.push_str("- Each change is an opportunity for bugs\n");
            }
            IssueType::DustyFile => {
                explanation.push_str("This file is **dusty** - it hasn't been touched in a long time. Here's what that means:\n\n");
                
                if let Some(days) = ctx.days_since_change {
                    let time_desc = if days > 365 {
                        format!("over {} year(s)", days / 365)
                    } else if days > 90 {
                        format!("about {} months", days / 30)
                    } else {
                        format!("{} days", days)
                    };
                    explanation.push_str(&format!("**Last modified:** {} ago\n\n", time_desc));
                }
                
                explanation.push_str("**Why dusty files need attention:**\n");
                explanation.push_str("- May contain outdated patterns or deprecated APIs\n");
                explanation.push_str("- Could be dead code that's no longer used\n");
                explanation.push_str("- Might have implicit assumptions that no longer hold\n");
                explanation.push_str("- Documentation may be stale or missing\n");
            }
            IssueType::TodoItem => {
                explanation.push_str("This file contains **TODO/FIXME markers** that indicate known issues or incomplete work:\n\n");
                
                if let (Some(text), Some(line)) = (&ctx.todo_text, ctx.todo_line) {
                    explanation.push_str(&format!("**The specific marker (line {}):**\n", line));
                    explanation.push_str(&format!("> {}\n\n", text));
                }
            }
            IssueType::BusFactorRisk => {
                explanation.push_str("This file has **bus factor risk** - knowledge is concentrated in one person:\n\n");
                
                if let (Some(author), Some(pct)) = (&ctx.primary_author, ctx.primary_author_pct) {
                    explanation.push_str(&format!("**{}** wrote **{:.0}%** of this code.\n\n", author, pct));
                }
                
                explanation.push_str("**Why this is risky:**\n");
                explanation.push_str("- If that person leaves or is unavailable, knowledge is lost\n");
                explanation.push_str("- Code may have implicit knowledge not captured in docs\n");
                explanation.push_str("- Other team members may avoid modifying it\n");
            }
            IssueType::MissingTests => {
                explanation.push_str("This file has **no test coverage**:\n\n");
                
                if let Some(loc) = ctx.loc {
                    explanation.push_str(&format!("**{} lines of code** with no automated tests.\n\n", loc));
                }
                
                explanation.push_str("**Why this matters:**\n");
                explanation.push_str("- Bugs can be introduced without any safety net\n");
                explanation.push_str("- Refactoring is risky without tests to verify behavior\n");
                explanation.push_str("- The code's intended behavior isn't documented through tests\n");
            }
        }

        // Add author info if available and relevant
        if issue_type != IssueType::BusFactorRisk {
            if let (Some(author), Some(pct)) = (&ctx.primary_author, ctx.primary_author_pct) {
                if pct > 70.0 {
                    explanation.push_str(&format!("\n**Note:** {} owns {:.0}% of this code (bus factor risk)\n", author, pct));
                }
            }
        }

        explanation
    }

    fn generate_task(&self, ctx: &FileContext) -> String {
        let issue_type = ctx.issue_type.unwrap_or(IssueType::DangerZone);
        
        match issue_type {
            IssueType::DangerZone => {
                let mut task = String::from("**Refactor this file to reduce complexity and risk.**\n\n");
                task.push_str("Please analyze the code above and:\n\n");
                
                if ctx.max_function_length.map_or(false, |l| l > 50) {
                    task.push_str(&format!(
                        "1. **Split the long function(s)** - There's at least one function that's {} lines. Break it into smaller, single-purpose functions of 20-30 lines each.\n",
                        ctx.max_function_length.unwrap()
                    ));
                } else {
                    task.push_str("1. **Review function sizes** - Look for functions that do too much and could be split.\n");
                }
                
                task.push_str("2. **Reduce nesting and complexity** - Look for:\n");
                task.push_str("   - Deeply nested if/else chains that could be early returns\n");
                task.push_str("   - Complex boolean conditions that could be extracted to named functions\n");
                task.push_str("   - Repeated patterns that could be abstracted\n");
                
                if ctx.has_tests == Some(false) {
                    task.push_str("3. **Add test coverage** - This file has NO tests. Before or after refactoring, add tests for:\n");
                    task.push_str("   - The main happy path\n");
                    task.push_str("   - Error conditions and edge cases\n");
                    task.push_str("   - Any complex business logic\n");
                }
                
                task.push_str("\n**Show me the refactored code with explanations for each change.**\n");
                task
            }
            IssueType::HighChurn => {
                let mut task = String::from("**Analyze why this file changes so frequently and stabilize it.**\n\n");
                
                if let Some(changes) = ctx.change_count {
                    task.push_str(&format!("This file has been modified {} times recently. Please:\n\n", changes));
                } else {
                    task.push_str("Please:\n\n");
                }
                
                task.push_str("1. **Identify the volatility** - Look at the code and hypothesize why it might need frequent changes:\n");
                task.push_str("   - Is it a \"god file\" doing too many things?\n");
                task.push_str("   - Are there hardcoded values that should be configuration?\n");
                task.push_str("   - Is the interface unclear, causing repeated fixes?\n\n");
                
                task.push_str("2. **Propose stabilization** - Suggest specific refactors to reduce future churn:\n");
                task.push_str("   - Extract configuration from code\n");
                task.push_str("   - Split into focused modules with clear responsibilities\n");
                task.push_str("   - Define clearer interfaces/APIs\n\n");
                
                task.push_str("3. **Add tests** to catch regressions when changes are made\n");
                task
            }
            IssueType::DustyFile => {
                let days_str = ctx.days_since_change
                    .map(|d| format!("{} days", d))
                    .unwrap_or_else(|| "a long time".to_string());
                    
                let mut task = format!("**Review this neglected code** (untouched for {}).\n\n", days_str);
                
                task.push_str("Please analyze the code above and:\n\n");
                task.push_str("1. **Check for dead code** - Are there functions, variables, or imports that aren't used anywhere?\n\n");
                task.push_str("2. **Identify outdated patterns** - Is the code using deprecated APIs, old syntax, or patterns that have better modern alternatives?\n\n");
                task.push_str("3. **Verify it still works** - Based on the code, does it look like it would still function correctly with current dependencies/APIs?\n\n");
                task.push_str("4. **Add documentation** - If this code is still needed, add comments explaining:\n");
                task.push_str("   - What this file/module does\n");
                task.push_str("   - Why it exists\n");
                task.push_str("   - Any non-obvious behavior\n\n");
                task.push_str("5. **Recommend: keep, update, or delete?** - Give your assessment of whether this code should be kept as-is, modernized, or removed.\n");
                task
            }
            IssueType::TodoItem => {
                let mut task = String::from("**Address the TODO/FIXME marker(s) in this file.**\n\n");
                
                if let (Some(text), Some(line)) = (&ctx.todo_text, ctx.todo_line) {
                    task.push_str(&format!("The specific item to address (line {}):\n", line));
                    task.push_str(&format!("> {}\n\n", text));
                }
                
                if !ctx.todos_in_file.is_empty() && ctx.todos_in_file.len() > 1 {
                    task.push_str("There are multiple markers in this file - address all of them if possible.\n\n");
                }
                
                task.push_str("Please:\n");
                task.push_str("1. **Understand the intent** - What was the original developer trying to flag?\n");
                task.push_str("2. **Implement the fix** - Make the necessary code changes\n");
                task.push_str("3. **Add tests** if the change affects behavior\n");
                task.push_str("4. **Remove the marker** once the issue is resolved\n");
                task
            }
            IssueType::BusFactorRisk => {
                let author_str = ctx.primary_author.as_deref().unwrap_or("one developer");
                let pct = ctx.primary_author_pct.unwrap_or(100.0);
                
                let mut task = format!(
                    "**Document this code to reduce bus factor risk.**\n\n\
                    {} wrote {:.0}% of this code. Help spread the knowledge:\n\n",
                    author_str, pct
                );
                
                task.push_str("1. **Add inline documentation** - Comment on:\n");
                task.push_str("   - Non-obvious business logic\n");
                task.push_str("   - Why certain approaches were chosen\n");
                task.push_str("   - Edge cases and gotchas\n\n");
                
                task.push_str("2. **Simplify complex sections** - Look for code that's hard to understand and:\n");
                task.push_str("   - Break into smaller functions with clear names\n");
                task.push_str("   - Add type annotations if missing\n");
                task.push_str("   - Replace clever code with obvious code\n\n");
                
                task.push_str("3. **Add tests as documentation** - Write tests that demonstrate:\n");
                task.push_str("   - How to use the main functions\n");
                task.push_str("   - Expected behavior in various scenarios\n");
                task
            }
            IssueType::MissingTests => {
                let loc = ctx.loc.unwrap_or(0);
                
                let mut task = format!(
                    "**Write tests for this untested {} lines of code.**\n\n",
                    loc
                );
                
                task.push_str("Analyze the code above and create comprehensive tests:\n\n");
                
                task.push_str("1. **Identify testable units** - What functions/methods should be tested?\n\n");
                
                task.push_str("2. **Write tests for the happy path** - Test that the main functionality works correctly\n\n");
                
                task.push_str("3. **Test edge cases and error conditions**:\n");
                task.push_str("   - Empty inputs\n");
                task.push_str("   - Null/undefined values\n");
                task.push_str("   - Boundary conditions\n");
                task.push_str("   - Error scenarios\n\n");
                
                task.push_str("4. **Use descriptive test names** that document the expected behavior\n\n");
                
                task.push_str("Please provide the complete test file code.\n");
                task
            }
        }
    }

    fn generate_guidelines(&self, ctx: &FileContext) -> String {
        let mut guidelines = String::from(
            "- Maintain backward compatibility where possible\n\
             - Follow existing code style and patterns in the project\n\
             - Keep changes focused and minimal\n"
        );

        if ctx.has_tests == Some(false) {
            guidelines.push_str("- Add tests for any new or modified functionality\n");
        }
        
        if ctx.primary_author_pct.map_or(false, |p| p > 80.0) {
            guidelines.push_str("- Add extra documentation since knowledge is concentrated\n");
        }

        guidelines
    }

    /// Copy the prompt to clipboard
    pub fn copy_to_clipboard(&mut self, prompt: &str) -> Result<(), String> {
        match &mut self.clipboard {
            Some(cb) => cb
                .set_text(prompt.to_string())
                .map_err(|e| format!("Failed to copy to clipboard: {}", e)),
            None => Err("Clipboard not available".to_string()),
        }
    }

    /// Generate and copy prompt for a file
    pub fn generate_and_copy(&mut self, ctx: &FileContext) -> Result<String, String> {
        let prompt = self.generate(ctx);
        self.copy_to_clipboard(&prompt)?;
        Ok(prompt)
    }
}

impl Default for PromptBuilder {
    fn default() -> Self {
        Self::new()
    }
}

/// Generate a batch prompt for multiple files (with file content!)
pub fn generate_batch_prompt(contexts: &[FileContext], panel_name: &str) -> String {
    let file_count = contexts.len().min(5); // Limit to 5 for batch to keep prompt size manageable
    
    let mut prompt = format!(
        "# Codebase Health Report - {}\n\n\
         I'm using a codebase health analyzer and it identified **{} files** that need attention.\n\
         Here are the top {} with their full source code for you to analyze.\n\n",
        panel_name, contexts.len(), file_count
    );

    prompt.push_str("---\n\n");

    for (i, ctx) in contexts.iter().enumerate().take(file_count) {
        let issue = ctx.issue_type.unwrap_or(IssueType::DangerZone);
        prompt.push_str(&format!(
            "## {}. `{}` - {}\n\n",
            i + 1,
            ctx.path,
            issue.label()
        ));

        // Why this file was flagged
        prompt.push_str("**Why it was flagged:**\n");
        if let Some(score) = ctx.danger_score {
            prompt.push_str(&format!("- Danger score: {:.0}/100 (high churn + high complexity)\n", score));
        }
        if let Some(changes) = ctx.change_count {
            prompt.push_str(&format!("- Changed {} times recently\n", changes));
        }
        if let Some(complexity) = ctx.complexity_score {
            let level = if complexity > 15.0 { "very high" } else if complexity > 8.0 { "high" } else { "moderate" };
            prompt.push_str(&format!("- Complexity: {:.1} ({})\n", complexity, level));
        }
        if let Some(max_fn) = ctx.max_function_length {
            if max_fn > 40 {
                prompt.push_str(&format!("- Has a function that's {} lines long\n", max_fn));
            }
        }
        if ctx.has_tests == Some(false) {
            prompt.push_str("- ⚠️ No test coverage\n");
        }
        if let Some(days) = ctx.days_since_change {
            prompt.push_str(&format!("- Untouched for {} days\n", days));
        }
        if let (Some(author), Some(pct)) = (&ctx.primary_author, ctx.primary_author_pct) {
            if pct > 80.0 {
                prompt.push_str(&format!("- {} wrote {:.0}% (bus factor risk)\n", author, pct));
            }
        }
        prompt.push('\n');

        // TODOs in this file
        if !ctx.todos_in_file.is_empty() {
            prompt.push_str("**TODOs in this file:**\n");
            for (line, kind, text) in &ctx.todos_in_file {
                prompt.push_str(&format!("- Line {}: [{}] {}\n", line, kind, text));
            }
            prompt.push('\n');
        }

        // The actual file content
        if let Some(ref content) = ctx.file_content {
            let lang = detect_language(&ctx.path);
            prompt.push_str("**File content:**\n\n");
            prompt.push_str(&format!("```{}\n{}\n```\n\n", lang, content));
        }

        prompt.push_str("---\n\n");
    }

    // Summary task
    prompt.push_str("## Your Task\n\n");
    prompt.push_str("Please analyze these files and provide:\n\n");
    prompt.push_str("1. **Priority ranking** - Which file should be fixed first and why?\n\n");
    prompt.push_str("2. **Common patterns** - Do you see any anti-patterns or issues that appear across multiple files?\n\n");
    prompt.push_str("3. **Specific fixes for each file** - For each file, provide:\n");
    prompt.push_str("   - What the main problems are\n");
    prompt.push_str("   - Concrete refactoring suggestions with code examples\n");
    prompt.push_str("   - Whether tests should be added first or after refactoring\n\n");
    prompt.push_str("4. **Quick wins** - Are there any simple fixes that would have high impact?\n");

    prompt
}

/// Detect programming language from file extension
pub fn detect_language(path: &str) -> &'static str {
    let ext = path.rsplit('.').next().unwrap_or("");
    match ext {
        "rs" => "rust",
        "js" => "javascript",
        "ts" => "typescript",
        "tsx" => "tsx",
        "jsx" => "jsx",
        "py" => "python",
        "go" => "go",
        "java" => "java",
        "rb" => "ruby",
        "php" => "php",
        "c" | "h" => "c",
        "cpp" | "hpp" | "cc" => "cpp",
        "cs" => "csharp",
        "swift" => "swift",
        "kt" => "kotlin",
        "scala" => "scala",
        "sh" | "bash" => "bash",
        "sql" => "sql",
        "html" => "html",
        "css" => "css",
        "scss" | "sass" => "scss",
        "json" => "json",
        "yaml" | "yml" => "yaml",
        "md" => "markdown",
        "vue" => "vue",
        "svelte" => "svelte",
        _ => "",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_file_context_builder() {
        let ctx = FileContext::new("src/test.rs")
            .with_danger_zone(&DangerZone {
                path: "src/test.rs".to_string(),
                churn_rank: 1,
                complexity_rank: 2,
                change_count: 15,
                complexity_score: 12.5,
                danger_score: 85.0,
                reason: "high churn + complex".to_string(),
            });

        assert_eq!(ctx.issue_type, Some(IssueType::DangerZone));
        assert_eq!(ctx.danger_score, Some(85.0));
    }

    #[test]
    fn test_prompt_generation() {
        let ctx = FileContext {
            path: "src/test.rs".to_string(),
            issue_type: Some(IssueType::DangerZone),
            danger_score: Some(85.0),
            change_count: Some(15),
            complexity_score: Some(12.5),
            loc: Some(500),
            function_count: Some(10),
            max_function_length: Some(80),
            has_tests: Some(false),
            ..Default::default()
        };

        let builder = PromptBuilder::new();
        let prompt = builder.generate(&ctx);

        assert!(prompt.contains("DANGER ZONE"));
        assert!(prompt.contains("src/test.rs"));
        assert!(prompt.contains("500"));
    }
}
