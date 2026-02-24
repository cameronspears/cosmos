/// Adaptive limits for Ask context building based on codebase size and question complexity.
pub(super) struct AdaptiveLimits {
    /// Max files to list in ask_question.
    pub(super) file_list_limit: usize,
    /// Max symbols to include.
    pub(super) symbol_limit: usize,
}

impl AdaptiveLimits {
    pub(super) fn for_codebase(file_count: usize, _total_loc: usize) -> Self {
        // Scale limits based on codebase size.
        if file_count < 50 {
            Self {
                file_list_limit: file_count.min(32),
                symbol_limit: 96,
            }
        } else if file_count < 200 {
            Self {
                file_list_limit: 28,
                symbol_limit: 72,
            }
        } else if file_count < 500 {
            Self {
                file_list_limit: 24,
                symbol_limit: 56,
            }
        } else {
            Self {
                file_list_limit: 20,
                symbol_limit: 44,
            }
        }
    }

    pub(super) fn for_codebase_and_question(
        file_count: usize,
        total_loc: usize,
        question: &str,
    ) -> Self {
        let mut limits = Self::for_codebase(file_count, total_loc);
        if is_complex_question(question) {
            limits.file_list_limit = limits.file_list_limit.saturating_add(8).min(44);
            limits.symbol_limit = limits.symbol_limit.saturating_add(24).min(140);
        }
        limits
    }
}

fn is_complex_question(question: &str) -> bool {
    let trimmed = question.trim();
    if trimmed.is_empty() {
        return false;
    }

    let lower = trimmed.to_ascii_lowercase();
    let token_count = trimmed.split_whitespace().count();

    let has_complex_keyword = [
        "architecture",
        "tradeoff",
        "trade-off",
        "data flow",
        "reliability",
        "scal",
        "concurrency",
        "migration",
        "security",
        "failure mode",
        "end to end",
        "end-to-end",
        "bottleneck",
    ]
    .iter()
    .any(|needle| lower.contains(needle));

    token_count >= 14 || has_complex_keyword
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn complex_questions_expand_limits() {
        let base = AdaptiveLimits::for_codebase_and_question(180, 50_000, "what is this");
        let complex = AdaptiveLimits::for_codebase_and_question(
            180,
            50_000,
            "Can you explain the end-to-end architecture and the biggest reliability tradeoffs?",
        );

        assert!(complex.file_list_limit > base.file_list_limit);
        assert!(complex.symbol_limit > base.symbol_limit);
    }

    #[test]
    fn limits_are_capped_for_huge_repos() {
        let limits = AdaptiveLimits::for_codebase_and_question(
            5_000,
            3_000_000,
            "Deep architecture and security tradeoffs across the whole stack",
        );

        assert!(limits.file_list_limit <= 44);
        assert!(limits.symbol_limit <= 140);
    }
}
