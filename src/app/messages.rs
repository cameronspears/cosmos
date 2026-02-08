use crate::suggest;
use crate::ui;
use std::collections::HashMap;
use std::path::PathBuf;
use uuid::Uuid;

/// Messages from background tasks to the main UI thread
pub enum BackgroundMessage {
    /// Provisional grounded suggestions from fast pass (not yet actionable).
    SuggestionsReady {
        suggestions: Vec<suggest::Suggestion>,
        usage: Option<suggest::llm::Usage>,
        model: String,
        diagnostics: suggest::llm::SuggestionDiagnostics,
        duration_ms: u64,
    },
    /// Refined suggestions after validation/regeneration (actionable list).
    SuggestionsRefined {
        suggestions: Vec<suggest::Suggestion>,
        usage: Option<suggest::llm::Usage>,
        diagnostics: suggest::llm::SuggestionDiagnostics,
        duration_ms: u64,
    },
    SuggestionsError(String),
    SummariesReady {
        summaries: HashMap<PathBuf, String>,
        usage: Option<suggest::llm::Usage>,
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
        grouping: crate::grouping::CodebaseGrouping,
        updated_files: usize,
        usage: Option<suggest::llm::Usage>,
        model: String,
    },
    GroupingEnhanceError(String),
    /// Quick preview ready (Phase 1 - fast)
    PreviewReady {
        preview: suggest::llm::FixPreview,
        usage: Option<suggest::llm::Usage>,
        file_hashes: HashMap<PathBuf, String>,
        duration_ms: u64,
    },
    PreviewError(String),
    /// Direct fix applied (Smart preset generated + applied the change)
    /// Supports both single-file and multi-file changes
    DirectFixApplied {
        suggestion_id: Uuid,
        /// All file changes (path, diff)
        file_changes: Vec<(PathBuf, String)>,
        description: String,
        usage: Option<suggest::llm::Usage>,
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
        options: Vec<crate::cache::ResetOption>,
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
        usage: Option<suggest::llm::Usage>,
    },
    /// Response to a user question with cache metadata
    QuestionResponseWithCache {
        question: String,
        answer: String,
        usage: Option<suggest::llm::Usage>,
        context_hash: String,
    },
    /// Verification review completed (adversarial review of applied changes)
    VerificationComplete {
        findings: Vec<suggest::llm::ReviewFinding>,
        summary: String,
        usage: Option<suggest::llm::Usage>,
        duration_ms: u64,
    },
    /// Verification fix completed (Smart fixed the selected findings)
    VerificationFixComplete {
        file_changes: Vec<(PathBuf, String)>,
        description: String,
        usage: Option<suggest::llm::Usage>,
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
