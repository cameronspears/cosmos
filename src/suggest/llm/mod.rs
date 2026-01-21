pub mod analysis;
pub mod client;
pub mod fix;
pub mod models;
pub mod parse;
pub mod prompts;
pub mod review;
pub mod summaries;

pub use analysis::{analyze_codebase, ask_question};
pub use client::is_available;
#[allow(unused_imports)]
pub use fix::{
    generate_fix_content,
    generate_fix_preview,
    generate_multi_file_fix,
    AppliedFix,
    FixPreview,
    FixScope,
    MultiFileAppliedFix,
};
pub use models::{Model, Usage};
#[allow(unused_imports)]
pub use review::{fix_review_findings, verify_changes, ReviewFinding, VerificationReview};
#[allow(unused_imports)]
pub use summaries::{
    discover_project_context,
    generate_file_summaries,
    generate_summaries_for_files,
    prioritize_files_for_summary,
};
