pub(super) fn default_max_auto_syntax_fix_loops() -> usize {
    1
}

pub(super) fn default_max_smart_escalations_per_attempt() -> usize {
    2
}

pub(super) fn default_reserve_independent_review_ms() -> u64 {
    8_000
}

pub(super) fn default_reserve_independent_review_cost_usd() -> f64 {
    0.0015
}

pub(super) fn default_enable_quick_check_baseline() -> bool {
    false
}

pub(super) fn default_require_independent_review_on_pass() -> bool {
    true
}
