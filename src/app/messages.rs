use crate::suggest;
use crate::ui;
use std::collections::HashMap;
use std::path::PathBuf;
use uuid::Uuid;

/// Messages from background tasks to the main UI thread
pub enum BackgroundMessage {
    SuggestionsReady {
        suggestions: Vec<suggest::Suggestion>,
        usage: Option<suggest::llm::Usage>,
        model: String,
    },
    SuggestionsError(String),
    SummariesReady {
        summaries: HashMap<PathBuf, String>,
        usage: Option<suggest::llm::Usage>,
        failed_files: Vec<PathBuf>,
    },
    /// Incremental summary progress update
    SummaryProgress {
        completed: usize,
        total: usize,
        summaries: HashMap<PathBuf, String>,
    },
    SummariesError(String),
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
        file_hashes: HashMap<PathBuf, String>,
    },
    PreviewError(String),
    /// Direct fix applied (Smart preset generated + applied the change)
    /// Supports both single-file and multi-file changes
    DirectFixApplied {
        suggestion_id: Uuid,
        /// All file changes (path, backup_path, diff, was_new_file)
        file_changes: Vec<(PathBuf, PathBuf, String, bool)>,
        description: String,
        usage: Option<suggest::llm::Usage>,
        branch_name: String,
        /// Human-friendly title for PR (e.g., "Batch Processing")
        friendly_title: String,
        /// Behavior-focused problem description for non-technical readers
        problem_summary: String,
        /// What will be different after the fix
        outcome: String,
    },
    DirectFixError(String),
    /// Ship workflow progress update
    ShipProgress(ui::ShipStep),
    /// Ship workflow completed successfully with PR URL
    ShipComplete(String),
    /// Ship workflow error
    ShipError(String),
    /// Generic error (used for push/etc)
    Error(String),
    /// Response to a user question
    QuestionResponse {
        answer: String,
        usage: Option<suggest::llm::Usage>,
    },
    /// Verification review completed (adversarial review of applied changes)
    VerificationComplete {
        findings: Vec<suggest::llm::ReviewFinding>,
        summary: String,
        usage: Option<suggest::llm::Usage>,
    },
    /// Verification fix completed (Smart fixed the selected findings)
    VerificationFixComplete {
        new_content: String,
        description: String,
        usage: Option<suggest::llm::Usage>,
    },
}
