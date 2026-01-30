/// Format optional repo memory into a prompt section.
pub(crate) fn format_repo_memory_section(repo_memory: Option<&str>, heading: &str) -> String {
    repo_memory
        .filter(|m| !m.trim().is_empty())
        .map(|m| format!("\n\n{}:\n{}", heading, m))
        .unwrap_or_default()
}

// ═══════════════════════════════════════════════════════════════════════════
//  TOKEN ESTIMATION AND CONTEXT BUDGETING
// ═══════════════════════════════════════════════════════════════════════════

/// Estimate the number of tokens in a text string.
///
/// This uses a simple heuristic that works well for code:
/// - Split by whitespace to count words
/// - Add punctuation count (each punctuation is roughly a token)
/// - This is accurate to within ~10-15% of actual tokenization
///
/// For more precise counting, a tokenizer like tiktoken would be needed,
/// but this heuristic is sufficient for context budgeting.
pub fn estimate_tokens(text: &str) -> usize {
    if text.is_empty() {
        return 0;
    }

    // Count whitespace-separated tokens
    let word_count = text.split_whitespace().count();

    // Count punctuation as additional tokens (common in code)
    let punct_count = text.chars().filter(|c| c.is_ascii_punctuation()).count();

    // Rough estimate: words + punctuation/2 (punctuation often merges with words)
    word_count + punct_count / 2
}

/// Model context window sizes
pub const CLAUDE_CONTEXT_TOKENS: usize = 200_000;
pub const RESERVED_OUTPUT_TOKENS: usize = 8_000;
pub const AVAILABLE_CONTEXT_TOKENS: usize = CLAUDE_CONTEXT_TOKENS - RESERVED_OUTPUT_TOKENS;

/// Context budget for building prompts
#[derive(Debug, Clone)]
pub struct ContextBudget {
    pub total_tokens: usize,
    /// Tokens for file structure (paths, tree)
    pub structure_tokens: usize,
    /// Tokens for symbol listings
    pub symbol_tokens: usize,
    /// Tokens for file summaries
    pub summary_tokens: usize,
    /// Tokens for code previews
    pub code_tokens: usize,
    /// Tokens for metadata (glossary, memory, etc.)
    pub metadata_tokens: usize,
}

impl ContextBudget {
    /// Create a context budget appropriate for the codebase size.
    ///
    /// Smaller codebases get more detail per file.
    /// Larger codebases get broader coverage with less depth.
    pub fn for_codebase(file_count: usize, _total_loc: usize) -> Self {
        let total = AVAILABLE_CONTEXT_TOKENS;

        // Adjust allocations based on codebase size
        let (structure_pct, symbol_pct, summary_pct, code_pct, metadata_pct) = if file_count < 50 {
            // Small codebase: more detail
            (10, 15, 35, 30, 10)
        } else if file_count < 200 {
            // Medium codebase: balanced
            (15, 15, 30, 30, 10)
        } else if file_count < 500 {
            // Large codebase: broader coverage
            (20, 10, 30, 30, 10)
        } else {
            // Very large codebase: focus on structure and summaries
            (25, 10, 35, 20, 10)
        };

        Self {
            total_tokens: total,
            structure_tokens: total * structure_pct / 100,
            symbol_tokens: total * symbol_pct / 100,
            summary_tokens: total * summary_pct / 100,
            code_tokens: total * code_pct / 100,
            metadata_tokens: total * metadata_pct / 100,
        }
    }

    /// Calculate how many items can fit in a token budget given average tokens per item
    pub fn items_for_budget(budget_tokens: usize, avg_tokens_per_item: usize) -> usize {
        if avg_tokens_per_item == 0 {
            return 0;
        }
        budget_tokens / avg_tokens_per_item
    }
}

impl Default for ContextBudget {
    fn default() -> Self {
        Self::for_codebase(100, 10000) // Medium codebase defaults
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_estimate_tokens() {
        // Empty string
        assert_eq!(estimate_tokens(""), 0);

        // Simple text
        let simple = "hello world";
        assert!(estimate_tokens(simple) >= 2);

        // Code with punctuation
        let code = "fn main() { println!(\"hello\"); }";
        let tokens = estimate_tokens(code);
        assert!(tokens > 5); // Should count words + some punctuation
    }

    #[test]
    fn test_context_budget_scaling() {
        let small = ContextBudget::for_codebase(20, 1000);
        let large = ContextBudget::for_codebase(600, 100000);

        // Large codebases should have more structure tokens (percentage-wise)
        let small_structure_pct = small.structure_tokens * 100 / small.total_tokens;
        let large_structure_pct = large.structure_tokens * 100 / large.total_tokens;
        assert!(large_structure_pct >= small_structure_pct);
    }
}
