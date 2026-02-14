/// Adaptive limits for context building based on codebase size.
pub(super) struct AdaptiveLimits {
    /// Max files to list in ask_question
    pub(super) file_list_limit: usize,
    /// Max symbols to include
    pub(super) symbol_limit: usize,
}

impl AdaptiveLimits {
    pub(super) fn for_codebase(file_count: usize, _total_loc: usize) -> Self {
        // Scale limits based on codebase size
        // Smaller codebases: more detail per file
        // Larger codebases: broader coverage
        if file_count < 50 {
            // Small codebase: show more detail
            Self {
                file_list_limit: file_count.min(50),
                symbol_limit: 150,
            }
        } else if file_count < 200 {
            // Medium codebase: balanced
            Self {
                file_list_limit: 50,
                symbol_limit: 100,
            }
        } else if file_count < 500 {
            // Large codebase: prioritize structure
            Self {
                file_list_limit: 40,
                symbol_limit: 80,
            }
        } else {
            // Very large codebase: focus on key areas
            Self {
                file_list_limit: 30,
                symbol_limit: 60,
            }
        }
    }
}
