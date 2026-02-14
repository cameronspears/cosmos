use crate::ui;
use cosmos_core::suggest;
use std::collections::HashMap;
use std::path::PathBuf;
use uuid::Uuid;

/// Messages from background tasks to the main UI thread
pub enum BackgroundMessage {
    /// Provisional grounded suggestions from fast pass (not yet actionable).
    SuggestionsReady {
        suggestions: Vec<suggest::Suggestion>,
        usage: Option<cosmos_engine::llm::Usage>,
        model: String,
        diagnostics: cosmos_engine::llm::SuggestionDiagnostics,
        duration_ms: u64,
    },
    /// Hidden refinement/gate progress update while attempts are running.
    SuggestionsRefinementProgress {
        attempt_index: usize,
        attempt_count: usize,
        gate: cosmos_engine::llm::SuggestionGateSnapshot,
        diagnostics: cosmos_engine::llm::SuggestionDiagnostics,
    },
    /// Refined suggestions after validation/regeneration (actionable list).
    SuggestionsRefined {
        suggestions: Vec<suggest::Suggestion>,
        usage: Option<cosmos_engine::llm::Usage>,
        diagnostics: cosmos_engine::llm::SuggestionDiagnostics,
        duration_ms: u64,
    },
    SuggestionsError(String),
    SummariesReady {
        summaries: HashMap<PathBuf, String>,
        usage: Option<cosmos_engine::llm::Usage>,
        failed_files: Vec<PathBuf>,
        duration_ms: u64,
    },
    /// Incremental summary progress update
    SummaryProgress {
        completed: usize,
        total: usize,
        summaries: HashMap<PathBuf, String>,
    },
    /// AI-assisted grouping update ready
    GroupingEnhanced {
        grouping: cosmos_core::grouping::CodebaseGrouping,
        updated_files: usize,
        usage: Option<cosmos_engine::llm::Usage>,
        model: String,
    },
    GroupingEnhanceError(String),
    /// Quick preview ready (Phase 1 - fast)
    PreviewReady {
        preview: cosmos_engine::llm::FixPreview,
        usage: Option<cosmos_engine::llm::Usage>,
        file_hashes: HashMap<PathBuf, String>,
        duration_ms: u64,
    },
    PreviewError(String),
    /// Progress updates from apply-time implementation harness.
    ApplyHarnessProgress {
        attempt_index: usize,
        attempt_count: usize,
        detail: String,
    },
    /// Detailed apply-harness failure payload.
    ApplyHarnessFailed {
        summary: String,
        fail_reasons: Vec<String>,
        report_path: Option<PathBuf>,
    },
    /// Apply succeeded, but at least one confidence-reducing condition occurred
    /// (for example, quick checks were unavailable).
    ApplyHarnessReducedConfidence {
        detail: String,
        report_path: Option<PathBuf>,
    },
    /// Direct fix applied (Smart preset generated + applied the change)
    /// Supports both single-file and multi-file changes
    DirectFixApplied {
        suggestion_id: Uuid,
        /// All file changes (path, diff)
        file_changes: Vec<(PathBuf, String)>,
        description: String,
        usage: Option<cosmos_engine::llm::Usage>,
        branch_name: String,
        /// Branch that was checked out before Cosmos created its fix branch.
        source_branch: String,
        /// Human-friendly title for PR (e.g., "Batch Processing")
        friendly_title: String,
        /// Behavior-focused problem description for non-technical readers
        problem_summary: String,
        /// What will be different after the fix
        outcome: String,
        /// Time spent generating + applying this fix
        duration_ms: u64,
    },
    DirectFixError(String),
    /// Ship workflow progress update
    ShipProgress(ui::ShipStep),
    /// Ship workflow completed successfully with PR URL
    ShipComplete(String),
    /// Ship workflow error
    ShipError(String),
    /// Cache reset completed
    ResetComplete {
        options: Vec<cosmos_adapters::cache::ResetOption>,
    },
    /// Git stash completed (save my work)
    StashComplete {
        message: String,
    },
    /// Discard changes completed
    DiscardComplete,
    /// Generic error (used for push/etc)
    Error(String),
    /// Response to a user question
    QuestionResponse {
        answer: String,
        usage: Option<cosmos_engine::llm::Usage>,
    },
    /// Response to a user question with cache metadata
    QuestionResponseWithCache {
        question: String,
        answer: String,
        usage: Option<cosmos_engine::llm::Usage>,
        context_hash: String,
    },
    /// Verification review completed (adversarial review of applied changes)
    VerificationComplete {
        findings: Vec<cosmos_engine::llm::ReviewFinding>,
        summary: String,
        usage: Option<cosmos_engine::llm::Usage>,
        duration_ms: u64,
    },
    /// Verification fix completed (Smart fixed the selected findings)
    VerificationFixComplete {
        file_changes: Vec<(PathBuf, String)>,
        description: String,
        usage: Option<cosmos_engine::llm::Usage>,
        duration_ms: u64,
    },
    /// New version available - show update panel
    UpdateAvailable {
        latest_version: String,
    },
    /// Update download progress (0-100)
    UpdateProgress {
        percent: u8,
    },
    /// Update failed
    UpdateError(String),
    /// Wallet balance updated
    WalletBalanceUpdated {
        balance: f64,
    },
}
