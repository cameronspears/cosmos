//! UI type definitions for Cosmos
//!
//! Contains enums, structs, and their implementations for UI state management.

use cosmos_engine::llm::FixPreview;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use super::theme::Theme;

// ═══════════════════════════════════════════════════════════════════════════
//  PANEL AND VIEW STATE
// ═══════════════════════════════════════════════════════════════════════════

/// Active panel
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ActivePanel {
    #[default]
    Suggestions,
    Ask,
}

/// View mode for file explorer
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ViewMode {
    #[default]
    Flat, // Traditional flat file list
    Grouped, // Grouped by layer and feature
}

impl ViewMode {
    pub fn label(&self) -> &'static str {
        match self {
            ViewMode::Flat => Theme::VIEW_FLAT,
            ViewMode::Grouped => Theme::VIEW_GROUPED,
        }
    }

    pub fn toggle(&self) -> Self {
        match self {
            ViewMode::Flat => ViewMode::Grouped,
            ViewMode::Grouped => ViewMode::Flat,
        }
    }
}

/// Input mode
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum InputMode {
    #[default]
    Normal,
    Search,
    Question, // Asking cosmos a question
}

// ═══════════════════════════════════════════════════════════════════════════
//  LOADING AND ANIMATION
// ═══════════════════════════════════════════════════════════════════════════

/// Loading state for background tasks
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LoadingState {
    #[default]
    None,
    GeneratingSuggestions,
    GeneratingPreview,   // Fast preview generation (<1s)
    GeneratingFix,       // Full fix generation (slower)
    ReviewingChanges,    // Adversarial review or PR review
    ApplyingReviewFixes, // Applying fixes from review
    Resetting,           // Clearing cache/data
    Stashing,            // Saving work via git stash
    Discarding,          // Discarding uncommitted changes
    SwitchingBranch,     // Switching to main branch from startup check
}

impl LoadingState {
    pub fn is_loading(&self) -> bool {
        !matches!(self, LoadingState::None)
    }
}

/// Spinner animation frames (braille pattern)
pub const SPINNER_FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

// ═══════════════════════════════════════════════════════════════════════════
//  OVERLAY STATE
// ═══════════════════════════════════════════════════════════════════════════

/// Overlay state
#[derive(Debug, Clone, PartialEq, Default)]
pub enum Overlay {
    #[default]
    None,
    /// Generic blocking message panel for important errors
    Alert {
        title: String,
        message: String,
        scroll: usize,
    },
    Help {
        scroll: usize,
    },
    FileDetail {
        path: PathBuf,
        scroll: usize,
    },
    /// API key entry overlay (in-TUI BYOK setup)
    ApiKeySetup {
        input: String,
        error: Option<String>,
        save_armed: bool,
    },
    /// Suggestions review focus selector (bug hunt vs security review)
    SuggestionFocus {
        selected: cosmos_engine::llm::SuggestionReviewFocus,
    },
    /// Apply plan preview - explicit scope/intent gate before mutation
    ApplyPlan {
        suggestion_id: uuid::Uuid,
        preview: FixPreview,
        affected_files: Vec<PathBuf>,
        confirm_apply: bool,
        show_technical_details: bool,
        show_data_notice: bool,
        scroll: usize,
    },
    /// Reset cosmos - selective cache/data reset
    Reset {
        /// List of (option, is_selected) pairs
        options: Vec<(cosmos_adapters::cache::ResetOption, bool)>,
        /// Currently focused option index
        selected: usize,
        /// Inline overlay error message
        error: Option<String>,
    },
    /// Startup action choices shown in Startup Check
    StartupCheck {
        /// Number of files with uncommitted changes
        changed_count: usize,
        /// Current git branch name (or "detached")
        current_branch: String,
        /// Default/main branch name
        main_branch: String,
        /// Current interaction mode for startup check
        mode: StartupMode,
        /// Currently focused action in choose mode
        selected_action: StartupAction,
    },
    /// Update available panel - shown when a new version is detected
    Update {
        /// Currently installed version
        current_version: String,
        /// New version available
        target_version: String,
        /// Download progress (None = not started, Some(0-100) = downloading)
        progress: Option<u8>,
        /// Error message if update failed
        error: Option<String>,
    },
    /// Welcome overlay - shown on first run to explain the basics
    Welcome,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartupAction {
    SaveStartFresh,
    DiscardStartFresh,
    ContinueAsIs,
    SwitchToMain,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartupMode {
    Choose,
    ConfirmDiscard,
}

// ═══════════════════════════════════════════════════════════════════════════
//  WORKFLOW NAVIGATION
// ═══════════════════════════════════════════════════════════════════════════

/// Steps in the Ship workflow
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ShipStep {
    #[default]
    Confirm, // Show what will happen
    Committing, // Committing changes
    Pushing,    // Pushing to remote
    CreatingPR, // Creating pull request
    Done,       // PR created successfully
}

/// Main workflow steps for the right panel: Suggestions → Review → Ship
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum WorkflowStep {
    #[default]
    Suggestions, // Browse and select suggestions
    Review, // Review applied changes, fix issues
    Ship,   // Commit, push, create PR
}

impl WorkflowStep {
    pub fn index(&self) -> usize {
        match self {
            WorkflowStep::Suggestions => 0,
            WorkflowStep::Review => 1,
            WorkflowStep::Ship => 2,
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════════
//  WORKFLOW STATE STRUCTS
// ═══════════════════════════════════════════════════════════════════════════

/// State for the Verify step
#[derive(Debug, Clone, Default)]
pub struct VerifyState {
    pub suggestion_id: Option<uuid::Uuid>,
    pub file_path: Option<PathBuf>,
    /// Additional files for multi-file suggestions
    pub additional_files: Vec<PathBuf>,
    pub summary: String,
    pub preview: Option<FixPreview>,
    pub loading: bool,
    pub scroll: usize,
    /// Whether to show technical details (code evidence, affected areas, etc.)
    pub show_technical_details: bool,
    /// File content hashes captured during preview (for change detection)
    pub preview_hashes: HashMap<PathBuf, String>,
}

impl VerifyState {
    /// Check if this is a multi-file suggestion
    pub fn is_multi_file(&self) -> bool {
        !self.additional_files.is_empty()
    }

    /// Get total file count
    pub fn file_count(&self) -> usize {
        if self.file_path.is_some() {
            1 + self.additional_files.len()
        } else {
            self.additional_files.len()
        }
    }
}

/// State for the Review step
#[derive(Debug, Clone, Default)]
pub struct ReviewFileContent {
    pub path: PathBuf,
    pub original_content: String,
    pub new_content: String,
}

/// State for the Review step
#[derive(Debug, Clone, Default)]
pub struct ReviewState {
    /// All files involved in this review cycle (multi-file aware)
    pub files: Vec<ReviewFileContent>,
    pub findings: Vec<cosmos_engine::llm::ReviewFinding>,
    pub selected: HashSet<usize>,
    pub cursor: usize,
    pub summary: String,
    pub scroll: usize,
    pub reviewing: bool,
    pub fixing: bool,
    pub confirm_ship: bool,
    pub review_iteration: u32,
    pub fixed_titles: Vec<String>,
    /// Explicit user confirmation needed before spending beyond hard budget guardrail.
    pub confirm_extra_review_budget: bool,
    /// Set when verification fails - allows user to proceed anyway with a warning
    pub verification_failed: bool,
    /// Error message from failed verification (for display)
    pub verification_error: Option<String>,
}

/// State for the Ship step
#[derive(Debug, Clone, Default)]
pub struct ShipState {
    pub branch_name: String,
    pub commit_message: String,
    pub files: Vec<PathBuf>,
    pub step: ShipStep,
    pub scroll: usize,
    pub pr_url: Option<String>,
}

/// State for the Ask Cosmos panel mode
#[derive(Debug, Clone, Default)]
pub struct AskCosmosState {
    pub response: String,
    pub scroll: usize,
}

// ═══════════════════════════════════════════════════════════════════════════
//  PENDING CHANGES
// ═══════════════════════════════════════════════════════════════════════════

/// A single file change within a pending change (for multi-file support)
#[derive(Debug, Clone)]
pub struct FileChange {
    pub path: PathBuf,
    pub diff: String,
}

impl FileChange {
    pub fn new(path: PathBuf, diff: String) -> Self {
        Self { path, diff }
    }
}

/// A pending change that has been applied but not yet committed
#[derive(Debug, Clone)]
pub struct PendingChange {
    pub suggestion_id: uuid::Uuid,
    /// All file changes in this pending change (supports multi-file refactors)
    pub files: Vec<FileChange>,
    pub description: String,
    /// Human-friendly title (e.g., "Batch Processing", "Error Handling")
    pub friendly_title: Option<String>,
    /// Behavior-focused problem description for non-technical readers
    pub problem_summary: Option<String>,
    /// What will be different after the fix (outcome-focused)
    pub outcome: Option<String>,
}

impl PendingChange {
    /// Create a multi-file pending change with human-friendly context
    pub fn with_preview_context_multi(
        suggestion_id: uuid::Uuid,
        files: Vec<FileChange>,
        description: String,
        friendly_title: String,
        problem_summary: String,
        outcome: String,
    ) -> Self {
        Self {
            suggestion_id,
            files,
            description,
            friendly_title: Some(friendly_title),
            problem_summary: Some(problem_summary),
            outcome: Some(outcome),
        }
    }

    /// Get the primary file path (first file, for backward compatibility)
    pub fn file_path(&self) -> &PathBuf {
        &self.files[0].path
    }

    /// Get the primary diff (first file, for backward compatibility)
    pub fn diff(&self) -> &str {
        &self.files[0].diff
    }

    /// Check if this is a multi-file change
    pub fn is_multi_file(&self) -> bool {
        self.files.len() > 1
    }
}
