pub mod agentic;
pub mod analysis;
pub mod client;
pub mod fix;
pub mod grouping;
pub mod implementation;
pub mod models;
pub mod parse;
pub mod prompt_utils;
pub mod prompts;
pub mod review;
pub mod summaries;
pub mod tools;

pub use analysis::{
    analyze_codebase_fast_grounded, ask_question, refine_grounded_suggestions,
    run_fast_grounded_with_gate, run_fast_grounded_with_gate_with_progress,
    GatedSuggestionRunResult, SuggestionDiagnostics, SuggestionGateSnapshot,
    SuggestionQualityGateConfig,
};
pub use client::{fetch_account_balance, is_available};
pub use fix::{
    build_fix_preview_from_validated_suggestion, generate_fix_content,
    generate_fix_content_with_model, generate_fix_preview_agentic, generate_multi_file_fix,
    generate_multi_file_fix_with_model, FileInput, FixPreview, FixScope,
};
pub use implementation::{
    implement_validated_suggestion_with_harness,
    implement_validated_suggestion_with_harness_with_progress, record_harness_finalization_outcome,
    ImplementationAppliedFile, ImplementationAttemptDiagnostics,
    ImplementationFinalizationDiagnostics, ImplementationFinalizationStatus,
    ImplementationGateSnapshot, ImplementationHarnessConfig, ImplementationQuickCheckStatus,
    ImplementationRunDiagnostics, ImplementationRunResult,
};
pub use models::Usage;
pub use review::{
    fix_review_findings, fix_review_findings_with_model, verify_changes, FixContext, ReviewFinding,
};
pub use summaries::{
    discover_project_context, generate_summaries_for_files, prioritize_files_for_summary,
    SUMMARY_BATCH_SIZE,
};
