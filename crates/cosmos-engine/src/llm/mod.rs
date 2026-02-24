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
pub mod tools;

pub use agentic::AgenticStreamKind;
pub use analysis::{
    analyze_codebase_fast_grounded, analyze_codebase_single_agent_reviewed, ask_question,
    run_fast_grounded_with_gate, run_fast_grounded_with_gate_with_progress,
    run_fast_grounded_with_gate_with_progress_and_stream, GatedSuggestionRunResult,
    SuggestionDiagnostics, SuggestionGateSnapshot, SuggestionQualityGateConfig,
    SuggestionReviewFocus, SuggestionStreamSink,
};
pub use client::is_available;
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
    ImplementationGateSnapshot, ImplementationHarnessConfig, ImplementationHarnessRunContext,
    ImplementationQuickCheckStatus, ImplementationReviewModel, ImplementationRunDiagnostics,
    ImplementationRunResult,
};
pub use models::Usage;
pub use review::{
    fix_review_findings, fix_review_findings_with_model, verify_changes,
    verify_changes_bounded_with_model, FixContext, ReviewFinding,
};
