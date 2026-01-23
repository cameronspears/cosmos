//! Cosmos UI - A contemplative dual-panel interface
//!
//! Layout:
//! ╔══════════════════════════════════════════════════════════════╗
//! ║                      C O S M O S                             ║
//! ║          a contemplative companion for your codebase         ║
//! ╠═══════════════════════════╦══════════════════════════════════╣
//! ║  PROJECT                  ║  SUGGESTIONS                     ║
//! ║  ├── src/                 ║  ● Refactor: ai.rs has 715       ║
//! ║  │   ├── main.rs      ●   ║    lines - split into modules    ║
//! ║  │   ├── ui/              ║                                  ║
//! ║  │   └── index/           ║  ◐ Quality: Missing tests for    ║
//! ║  └── tests/               ║    public functions              ║
//! ╠═══════════════════════════╩══════════════════════════════════╣
//! ║  main ● 5 changed │ ? inquiry  ↵ view  a apply  q quit      ║
//! ╚══════════════════════════════════════════════════════════════╝

pub mod markdown;
pub mod theme;

use crate::context::WorkContext;
use crate::index::{CodebaseIndex, FlatTreeEntry};
use crate::suggest::{Suggestion, SuggestionEngine};
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph, Wrap},
    Frame,
};
use std::path::PathBuf;
use std::time::Instant;
use theme::Theme;

/// Active panel
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ActivePanel {
    #[default]
    Project,
    Suggestions,
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

/// Loading state for background tasks
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LoadingState {
    #[default]
    None,
    GeneratingSuggestions,
    GeneratingSummaries,
    GeneratingPreview,   // Fast preview generation (<1s)
    GeneratingFix,       // Full fix generation (slower)
    ReviewingChanges,    // Adversarial review or PR review
    ApplyingReviewFixes, // Applying fixes from review
    Answering,           // For question answering
}

impl LoadingState {
    pub fn is_loading(&self) -> bool {
        !matches!(self, LoadingState::None)
    }
}

/// Spinner animation frames (braille pattern)
pub const SPINNER_FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// Overlay state
#[derive(Debug, Clone, PartialEq, Default)]
pub enum Overlay {
    #[default]
    None,
    Help {
        scroll: usize,
    },
    /// Privacy preview for inquiry (what will be sent)
    InquiryPreview {
        question: String,
        preview: String,
        scroll: usize,
    },
    FileDetail {
        path: PathBuf,
        scroll: usize,
    },
    /// Reset cosmos - selective cache/data reset
    Reset {
        /// List of (option, is_selected) pairs
        options: Vec<(crate::cache::ResetOption, bool)>,
        /// Currently focused option index
        selected: usize,
    },
    /// Startup check - shown when cosmos starts with unsaved work
    StartupCheck {
        /// Number of files with uncommitted changes
        changed_count: usize,
        /// True when showing "are you sure?" confirmation for discard
        confirming_discard: bool,
    },
}

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

// ═══════════════════════════════════════════════════════════════════════════
//  WORKFLOW NAVIGATION (right panel flow)
// ═══════════════════════════════════════════════════════════════════════════

/// Main workflow steps for the right panel: Suggestions → Verify → Review → Ship
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum WorkflowStep {
    #[default]
    Suggestions, // Browse and select suggestions
    Verify, // Verify the fix, apply it
    Review, // Review applied changes, fix issues
    Ship,   // Commit, push, create PR
}

impl WorkflowStep {
    pub fn index(&self) -> usize {
        match self {
            WorkflowStep::Suggestions => 0,
            WorkflowStep::Verify => 1,
            WorkflowStep::Review => 2,
            WorkflowStep::Ship => 3,
        }
    }
}

/// State for the Verify step
#[derive(Debug, Clone, Default)]
pub struct VerifyState {
    pub suggestion_id: Option<uuid::Uuid>,
    pub file_path: Option<PathBuf>,
    /// Additional files for multi-file suggestions
    pub additional_files: Vec<PathBuf>,
    pub summary: String,
    pub preview: Option<crate::suggest::llm::FixPreview>,
    pub loading: bool,
    pub scroll: usize,
    /// Whether to show technical details (code evidence, affected areas, etc.)
    pub show_technical_details: bool,
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
pub struct ReviewState {
    pub file_path: Option<PathBuf>,
    pub original_content: String,
    pub new_content: String,
    pub findings: Vec<crate::suggest::llm::ReviewFinding>,
    pub selected: std::collections::HashSet<usize>,
    pub cursor: usize,
    pub summary: String,
    pub scroll: usize,
    pub reviewing: bool,
    pub fixing: bool,
    pub confirm_ship: bool,
    pub review_iteration: u32,
    pub fixed_titles: Vec<String>,
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

/// Toast notification kind - affects duration and styling
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ToastKind {
    #[default]
    Info,
    Success,
    Error,
    RateLimit,
}

impl ToastKind {
    /// Duration in seconds before toast expires
    pub fn duration_secs(&self) -> u64 {
        match self {
            ToastKind::Info => 3,
            ToastKind::Success => 3,
            ToastKind::Error => 10,     // Errors stay longer
            ToastKind::RateLimit => 15, // Rate limits stay even longer
        }
    }
}

/// Toast notification
pub struct Toast {
    pub message: String,
    pub created_at: Instant,
    pub kind: ToastKind,
}

impl Toast {
    pub fn new(message: &str) -> Self {
        // Auto-detect toast type - check success indicators BEFORE error keywords
        let kind = if message.contains("Rate limit") || message.contains("rate limited") {
            ToastKind::RateLimit
        } else if message.starts_with("Fixed:") || message.starts_with('+') {
            ToastKind::Success
        } else if message.contains("failed")
            || message.contains("error")
            || message.contains("Error")
        {
            ToastKind::Error
        } else {
            ToastKind::Info
        };

        Self {
            message: message.to_string(),
            created_at: Instant::now(),
            kind,
        }
    }

    pub fn is_expired(&self) -> bool {
        self.created_at.elapsed().as_secs() >= self.kind.duration_secs()
    }

    pub fn is_error(&self) -> bool {
        matches!(self.kind, ToastKind::Error | ToastKind::RateLimit)
    }
}

/// A single file change within a pending change (for multi-file support)
#[derive(Debug, Clone)]
pub struct FileChange {
    pub path: PathBuf,
    pub diff: String,
    pub backup_path: PathBuf,
}

impl FileChange {
    pub fn new(path: PathBuf, diff: String, backup_path: PathBuf) -> Self {
        Self {
            path,
            diff,
            backup_path,
        }
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

// ═══════════════════════════════════════════════════════════════════════════
//  RITUAL MODE (time-boxed queue)
// ═══════════════════════════════════════════════════════════════════════════

/// Main application state for Cosmos
pub struct App {
    // Core data
    pub index: CodebaseIndex,
    pub suggestions: SuggestionEngine,
    pub context: WorkContext,
    pub config: crate::config::Config,

    // UI state
    pub active_panel: ActivePanel,
    pub project_scroll: usize,
    pub project_selected: usize,
    pub suggestion_scroll: usize,
    pub suggestion_selected: usize,
    pub overlay: Overlay,
    pub toast: Option<Toast>,
    pub should_quit: bool,

    // Search and sort state
    pub input_mode: InputMode,
    pub search_query: String,
    pub view_mode: ViewMode,

    // Question input (ask cosmos)
    pub question_input: String,
    pub question_suggestions: Vec<String>,
    pub question_suggestion_selected: usize,

    // Loading state for background tasks
    pub loading: LoadingState,
    pub loading_frame: usize,

    // LLM-generated file summaries
    pub llm_summaries: std::collections::HashMap<PathBuf, String>,

    // Personal repo memory (local)
    pub repo_memory: crate::cache::RepoMemory,

    // Domain glossary (auto-extracted terminology)
    pub glossary: crate::cache::DomainGlossary,

    // Cost tracking
    pub session_cost: f64,            // Total USD spent this session
    pub session_tokens: u32,          // Total tokens used this session
    pub active_model: Option<String>, // Current/last model used

    // Track if summaries need generation (to avoid showing loading state when all cached)
    pub needs_summary_generation: bool,

    // Summary generation progress (completed, total)
    pub summary_progress: Option<(usize, usize)>,
    /// Files that failed summary generation (for retry visibility)
    pub summary_failed_files: Vec<PathBuf>,

    // Cached data for display
    pub file_tree: Vec<FlatTreeEntry>,
    pub filtered_tree: Vec<FlatTreeEntry>,
    pub repo_path: PathBuf,

    // Grouped view data
    pub grouping: crate::grouping::CodebaseGrouping,
    pub grouped_tree: Vec<crate::grouping::GroupedTreeEntry>,
    pub filtered_grouped_tree: Vec<crate::grouping::GroupedTreeEntry>,

    // Pending changes for batch commit workflow
    pub pending_changes: Vec<PendingChange>,
    pub cosmos_branch: Option<String>,

    // PR URL for "press Enter to open" flow
    pub pr_url: Option<String>,

    // Ship step for inline shipping from SafeApplyReport
    pub ship_step: Option<ShipStep>,

    // Workflow navigation (right panel)
    pub workflow_step: WorkflowStep,
    pub verify_state: VerifyState,
    pub review_state: ReviewState,
    pub ship_state: ShipState,
    pub ask_cosmos_state: Option<AskCosmosState>,

    /// Last git refresh error message (if any)
    pub git_refresh_error: Option<String>,
    /// Last time we surfaced a git refresh error
    pub git_refresh_error_at: Option<Instant>,

    // Flag: generate suggestions once summaries complete (used at init and after reset)
    pub pending_suggestions_on_init: bool,
}

impl App {
    /// Create a new Cosmos app
    pub fn new(index: CodebaseIndex, suggestions: SuggestionEngine, context: WorkContext) -> Self {
        let file_tree = build_file_tree(&index);
        let filtered_tree = file_tree.clone();
        let repo_path = index.root.clone();

        // Generate grouping for the codebase
        let grouping = index.generate_grouping();
        let grouped_tree = build_grouped_tree(&grouping, &index);
        let filtered_grouped_tree = grouped_tree.clone();

        Self {
            index,
            suggestions,
            context,
            config: crate::config::Config::load(),
            active_panel: ActivePanel::default(),
            project_scroll: 0,
            project_selected: 0,
            suggestion_scroll: 0,
            suggestion_selected: 0,
            overlay: Overlay::None,
            toast: None,
            should_quit: false,
            input_mode: InputMode::Normal,
            search_query: String::new(),
            view_mode: ViewMode::Grouped, // Default to grouped view
            question_input: String::new(),
            question_suggestions: Vec::new(),
            question_suggestion_selected: 0,
            loading: LoadingState::None,
            loading_frame: 0,
            llm_summaries: std::collections::HashMap::new(),
            repo_memory: crate::cache::RepoMemory::default(),
            glossary: crate::cache::DomainGlossary::default(),
            session_cost: 0.0,
            session_tokens: 0,
            active_model: None,
            needs_summary_generation: false,
            summary_progress: None,
            summary_failed_files: Vec::new(),
            file_tree,
            filtered_tree,
            repo_path,
            grouping,
            grouped_tree,
            filtered_grouped_tree,
            pending_changes: Vec::new(),
            cosmos_branch: None,
            pr_url: None,
            ship_step: None,
            workflow_step: WorkflowStep::default(),
            verify_state: VerifyState::default(),
            review_state: ReviewState::default(),
            ship_state: ShipState::default(),
            ask_cosmos_state: None,
            pending_suggestions_on_init: false,
            git_refresh_error: None,
            git_refresh_error_at: None,
        }
    }

    /// Apply a new grouping and rebuild grouped trees.
    pub fn apply_grouping_update(&mut self, grouping: crate::grouping::CodebaseGrouping) {
        self.index.apply_grouping(&grouping);
        self.grouping = grouping;
        self.grouped_tree = build_grouped_tree(&self.grouping, &self.index);
        self.filtered_grouped_tree = self.grouped_tree.clone();

        if self.project_selected >= self.filtered_grouped_tree.len() {
            self.project_selected = self.filtered_grouped_tree.len().saturating_sub(1);
        }
        self.project_scroll = 0;
    }

    /// Clear all pending changes (after commit)
    pub fn clear_pending_changes(&mut self) {
        for change in &self.pending_changes {
            for file_change in &change.files {
                let _ = std::fs::remove_file(&file_change.backup_path);
            }
        }
        self.pending_changes.clear();
        self.cosmos_branch = None;
    }

    /// Undo the most recent applied change by restoring all backup files.
    /// Supports multi-file changes - restores all files atomically.
    /// Removes it from the pending queue.
    pub fn undo_last_pending_change(&mut self) -> Result<(), String> {
        let change = self
            .pending_changes
            .pop()
            .ok_or_else(|| "No pending changes to undo".to_string())?;

        // Verify all backups exist before restoring any
        let missing_backup = change
            .files
            .iter()
            .find(|f| !f.backup_path.exists())
            .map(|f| f.path.display().to_string());

        if let Some(missing_file) = missing_backup {
            // Put the change back since we couldn't undo
            self.pending_changes.push(change);
            return Err(format!(
                "Backup file not found for {}: cannot undo",
                missing_file
            ));
        }

        // Restore all files from their backups
        for file_change in &change.files {
            let target = self.repo_path.join(&file_change.path);
            std::fs::copy(&file_change.backup_path, &target).map_err(|e| {
                format!(
                    "Failed to restore backup for {}: {}",
                    file_change.path.display(),
                    e
                )
            })?;
            let _ = std::fs::remove_file(&file_change.backup_path);
        }

        // Mark suggestion as not applied (so it can be re-applied if desired).
        self.suggestions.unmark_applied(change.suggestion_id);

        Ok(())
    }

    /// Tick the loading animation
    pub fn tick_loading(&mut self) {
        if self.loading.is_loading() {
            self.loading_frame = self.loading_frame.wrapping_add(1);
        }
    }

    /// Update file summaries from LLM (merges with existing, doesn't replace)
    pub fn update_summaries(&mut self, summaries: std::collections::HashMap<PathBuf, String>) {
        // IMPORTANT: Extend, don't replace! This preserves cached summaries
        self.llm_summaries.extend(summaries);
    }

    /// Get LLM summary for a file
    pub fn get_llm_summary(&self, path: &PathBuf) -> Option<&String> {
        self.llm_summaries.get(path)
    }

    /// Enter search mode
    pub fn start_search(&mut self) {
        self.input_mode = InputMode::Search;
        self.search_query.clear();
    }

    /// Exit search mode
    pub fn exit_search(&mut self) {
        self.input_mode = InputMode::Normal;
        self.search_query.clear();
        self.apply_filter();
    }

    /// Enter question mode
    pub fn start_question(&mut self) {
        self.input_mode = InputMode::Question;
        self.question_input.clear();
        self.question_suggestions = Self::generate_question_suggestions();
        self.question_suggestion_selected = 0;
    }

    /// Exit question mode
    pub fn exit_question(&mut self) {
        self.input_mode = InputMode::Normal;
        self.question_input.clear();
        self.question_suggestions.clear();
    }

    /// Add character to question input
    pub fn question_push(&mut self, c: char) {
        self.question_input.push(c);
    }

    /// Remove last character from question input
    pub fn question_pop(&mut self) {
        self.question_input.pop();
    }

    /// Get the current question and clear it
    pub fn take_question(&mut self) -> String {
        let q = self.question_input.clone();
        self.question_input.clear();
        self.question_suggestions.clear();
        self.input_mode = InputMode::Normal;
        q
    }

    /// Generate exploratory question suggestions for the ask feature
    fn generate_question_suggestions() -> Vec<String> {
        vec![
            // Understanding
            "What does this project do and who is it for?".into(),
            "What are the main parts and how do they work together?".into(),
            "What should I understand first if I'm new here?".into(),
            // Strategic
            "What areas would benefit most from improvement?".into(),
            "What are the risks or rough edges to know about?".into(),
            // Strengths
            "What does this project do really well?".into(),
        ]
    }

    /// Move selection up in question suggestions
    pub fn question_suggestion_up(&mut self) {
        if self.question_suggestion_selected > 0 {
            self.question_suggestion_selected -= 1;
        }
    }

    /// Move selection down in question suggestions
    pub fn question_suggestion_down(&mut self) {
        if self.question_suggestion_selected < self.question_suggestions.len().saturating_sub(1) {
            self.question_suggestion_selected += 1;
        }
    }

    /// Use the selected suggestion as the question input
    pub fn use_selected_suggestion(&mut self) {
        if let Some(q) = self
            .question_suggestions
            .get(self.question_suggestion_selected)
        {
            self.question_input = q.clone();
        }
    }

    /// Add character to search query
    pub fn search_push(&mut self, c: char) {
        self.search_query.push(c);
        self.apply_filter();
    }

    /// Remove last character from search query
    pub fn search_pop(&mut self) {
        self.search_query.pop();
        self.apply_filter();
    }

    /// Apply search filter to file tree
    fn apply_filter(&mut self) {
        match self.view_mode {
            ViewMode::Flat => {
                if self.search_query.is_empty() {
                    self.filtered_tree = self.file_tree.clone();
                } else {
                    let query = self.search_query.to_lowercase();
                    self.filtered_tree = self
                        .file_tree
                        .iter()
                        .filter(|entry| {
                            entry.name.to_lowercase().contains(&query)
                                || entry.path.to_string_lossy().to_lowercase().contains(&query)
                        })
                        .cloned()
                        .collect();
                }

                // Reset selection if it's out of bounds
                if self.project_selected >= self.filtered_tree.len() {
                    self.project_selected = self.filtered_tree.len().saturating_sub(1);
                }
            }
            ViewMode::Grouped => {
                if self.search_query.is_empty() {
                    // No search - restore original expand states and use cached tree
                    self.filtered_grouped_tree = self.grouped_tree.clone();
                } else {
                    let query = self.search_query.to_lowercase();

                    // Search through ALL files in the grouping (not just visible ones)
                    // and find which layers contain matching files
                    let mut matching_layers = std::collections::HashSet::new();

                    for (path, assignment) in &self.grouping.file_assignments {
                        let path_str = path.to_string_lossy().to_lowercase();
                        let name = path
                            .file_name()
                            .and_then(|n| n.to_str())
                            .unwrap_or("")
                            .to_lowercase();

                        if name.contains(&query) || path_str.contains(&query) {
                            matching_layers.insert(assignment.layer);
                        }
                    }

                    // Auto-expand layers that contain matching files
                    for layer in &matching_layers {
                        if let Some(group) = self.grouping.groups.get_mut(layer) {
                            group.expanded = true;
                        }
                    }

                    // Rebuild the grouped tree with expanded layers
                    self.grouped_tree = build_grouped_tree(&self.grouping, &self.index);

                    // Now filter the rebuilt tree to show only matching entries
                    self.filtered_grouped_tree = self
                        .grouped_tree
                        .iter()
                        .filter(|entry| {
                            use crate::grouping::GroupedEntryKind;
                            match &entry.kind {
                                // Always show layer headers that contain matches
                                GroupedEntryKind::Layer(layer) => matching_layers.contains(layer),
                                // Show features if they contain matching files
                                GroupedEntryKind::Feature => {
                                    // Feature names don't have paths, check if name matches
                                    // or if any child files match (they'll be shown separately)
                                    entry.name.to_lowercase().contains(&query) || true
                                }
                                // Show files that match the query
                                GroupedEntryKind::File => {
                                    entry.name.to_lowercase().contains(&query)
                                        || entry
                                            .path
                                            .as_ref()
                                            .map(|p| {
                                                p.to_string_lossy().to_lowercase().contains(&query)
                                            })
                                            .unwrap_or(false)
                                }
                            }
                        })
                        .cloned()
                        .collect();

                    // Remove features that have no matching files under them
                    // by checking if the next entries are files that match
                    self.filtered_grouped_tree = self.filter_empty_features(&query);
                }

                // Reset selection if it's out of bounds
                if self.project_selected >= self.filtered_grouped_tree.len() {
                    self.project_selected = self.filtered_grouped_tree.len().saturating_sub(1);
                }
            }
        }
        self.project_scroll = 0;
    }

    /// Filter out features that have no matching files
    fn filter_empty_features(&self, query: &str) -> Vec<crate::grouping::GroupedTreeEntry> {
        use crate::grouping::GroupedEntryKind;

        let mut result = Vec::new();
        let entries = &self.filtered_grouped_tree;
        let mut i = 0;

        while i < entries.len() {
            let entry = &entries[i];

            match &entry.kind {
                GroupedEntryKind::Layer(_) => {
                    // Always include layers (they were already filtered to have matches)
                    result.push(entry.clone());
                    i += 1;
                }
                GroupedEntryKind::Feature => {
                    // Check if this feature has any matching files after it
                    let mut has_matching_files = false;
                    let mut j = i + 1;

                    while j < entries.len() {
                        match &entries[j].kind {
                            GroupedEntryKind::File => {
                                // Check if this file matches the query
                                let name_matches = entries[j].name.to_lowercase().contains(query);
                                let path_matches = entries[j]
                                    .path
                                    .as_ref()
                                    .map(|p| p.to_string_lossy().to_lowercase().contains(query))
                                    .unwrap_or(false);

                                if name_matches || path_matches {
                                    has_matching_files = true;
                                    break;
                                }
                                j += 1;
                            }
                            // Stop when we hit another feature or layer
                            _ => break,
                        }
                    }

                    if has_matching_files || entry.name.to_lowercase().contains(query) {
                        result.push(entry.clone());
                    }
                    i += 1;
                }
                GroupedEntryKind::File => {
                    // Files were already filtered, include them
                    let name_matches = entry.name.to_lowercase().contains(query);
                    let path_matches = entry
                        .path
                        .as_ref()
                        .map(|p| p.to_string_lossy().to_lowercase().contains(query))
                        .unwrap_or(false);

                    if name_matches || path_matches {
                        result.push(entry.clone());
                    }
                    i += 1;
                }
            }
        }

        result
    }

    /// Toggle between flat and grouped view modes
    pub fn toggle_view_mode(&mut self) {
        self.view_mode = self.view_mode.toggle();
        self.project_selected = 0;
        self.project_scroll = 0;
        self.apply_filter();
        self.show_toast(&format!("View: {}", self.view_mode.label()));
    }

    /// Toggle expand/collapse of the selected group in grouped view
    pub fn toggle_group_expand(&mut self) {
        if self.view_mode != ViewMode::Grouped {
            return;
        }

        if let Some(entry) = self.filtered_grouped_tree.get(self.project_selected) {
            use crate::grouping::GroupedEntryKind;
            match &entry.kind {
                GroupedEntryKind::Layer(layer) => {
                    if let Some(group) = self.grouping.groups.get_mut(layer) {
                        group.expanded = !group.expanded;
                        self.rebuild_grouped_tree();
                    }
                }
                GroupedEntryKind::Feature => {
                    // For now, features are always expanded - could add feature collapse later
                }
                GroupedEntryKind::File => {
                    // Files can't be expanded - show details instead
                    self.show_file_detail();
                }
            }
        }
    }

    /// Rebuild the grouped tree after a toggle
    fn rebuild_grouped_tree(&mut self) {
        self.grouped_tree = build_grouped_tree(&self.grouping, &self.index);
        self.apply_filter();
    }

    /// Page down (jump 10 items)
    pub fn page_down(&mut self) {
        let max = self.project_tree_len().saturating_sub(1);
        self.project_selected = (self.project_selected + 10).min(max);
        self.ensure_project_visible();
    }

    /// Page up (jump 10 items)
    pub fn page_up(&mut self) {
        self.project_selected = self.project_selected.saturating_sub(10);
        self.ensure_project_visible();
    }

    /// Show file detail overlay for currently selected file
    pub fn show_file_detail(&mut self) {
        match self.view_mode {
            ViewMode::Flat => {
                if let Some(entry) = self.filtered_tree.get(self.project_selected) {
                    self.overlay = Overlay::FileDetail {
                        path: entry.path.clone(),
                        scroll: 0,
                    };
                }
            }
            ViewMode::Grouped => {
                if let Some(entry) = self.filtered_grouped_tree.get(self.project_selected) {
                    if let Some(path) = &entry.path {
                        self.overlay = Overlay::FileDetail {
                            path: path.clone(),
                            scroll: 0,
                        };
                    }
                }
            }
        }
    }

    /// Switch to the other panel
    pub fn toggle_panel(&mut self) {
        self.active_panel = match self.active_panel {
            ActivePanel::Project => ActivePanel::Suggestions,
            ActivePanel::Suggestions => ActivePanel::Project,
        };
    }

    /// Navigate down in the current panel
    pub fn navigate_down(&mut self) {
        match self.active_panel {
            ActivePanel::Project => {
                let max = self.project_tree_len().saturating_sub(1);
                self.project_selected = (self.project_selected + 1).min(max);
                self.ensure_project_visible();
            }
            ActivePanel::Suggestions => {
                let max = self
                    .suggestions
                    .active_suggestions()
                    .len()
                    .saturating_sub(1);
                self.suggestion_selected = (self.suggestion_selected + 1).min(max);
                self.ensure_suggestion_visible();
            }
        }
    }

    /// Navigate up in the current panel
    pub fn navigate_up(&mut self) {
        match self.active_panel {
            ActivePanel::Project => {
                self.project_selected = self.project_selected.saturating_sub(1);
                self.ensure_project_visible();
            }
            ActivePanel::Suggestions => {
                self.suggestion_selected = self.suggestion_selected.saturating_sub(1);
                self.ensure_suggestion_visible();
            }
        }
    }

    /// Get the length of the current project tree based on view mode
    fn project_tree_len(&self) -> usize {
        match self.view_mode {
            ViewMode::Flat => self.filtered_tree.len(),
            ViewMode::Grouped => self.filtered_grouped_tree.len(),
        }
    }

    fn ensure_project_visible(&mut self) {
        if self.project_selected < self.project_scroll {
            self.project_scroll = self.project_selected;
        } else if self.project_selected >= self.project_scroll + 15 {
            self.project_scroll = self.project_selected.saturating_sub(14);
        }
    }

    fn ensure_suggestion_visible(&mut self) {
        // Each suggestion card is ~7-8 lines tall, so only ~3-4 fit in view
        let visible_count = 3;
        if self.suggestion_selected < self.suggestion_scroll {
            self.suggestion_scroll = self.suggestion_selected;
        } else if self.suggestion_selected >= self.suggestion_scroll + visible_count {
            self.suggestion_scroll = self.suggestion_selected.saturating_sub(visible_count - 1);
        }
    }

    /// Get currently selected suggestion
    pub fn selected_suggestion(&self) -> Option<&Suggestion> {
        let suggestions = self.suggestions.active_suggestions();
        suggestions.get(self.suggestion_selected).copied()
    }

    /// Toggle help overlay
    pub fn toggle_help(&mut self) {
        self.overlay = match self.overlay {
            Overlay::Help { .. } => Overlay::None,
            _ => Overlay::Help { scroll: 0 },
        };
    }

    /// Close overlay
    pub fn close_overlay(&mut self) {
        self.overlay = Overlay::None;
    }

    /// Show inquiry response in the right panel (Ask Cosmos mode)
    pub fn show_inquiry(&mut self, response: String) {
        self.ask_cosmos_state = Some(AskCosmosState { response, scroll: 0 });
    }

    /// Exit ask cosmos mode and return to suggestions
    pub fn exit_ask_cosmos(&mut self) {
        self.ask_cosmos_state = None;
        self.workflow_step = WorkflowStep::Suggestions;
    }

    /// Check if in ask cosmos mode (showing response)
    pub fn is_ask_cosmos_mode(&self) -> bool {
        self.ask_cosmos_state.is_some()
    }

    /// Scroll ask cosmos response down
    pub fn ask_cosmos_scroll_down(&mut self) {
        if let Some(state) = &mut self.ask_cosmos_state {
            state.scroll = state.scroll.saturating_add(1);
        }
    }

    /// Scroll ask cosmos response up
    pub fn ask_cosmos_scroll_up(&mut self) {
        if let Some(state) = &mut self.ask_cosmos_state {
            state.scroll = state.scroll.saturating_sub(1);
        }
    }

    /// Show a privacy preview for an inquiry (what will be sent).
    pub fn show_inquiry_preview(&mut self, question: String) {
        let mut preview = String::new();
        preview.push_str("Cosmos will send:\\n");
        preview.push_str("- Repo stats (file count, LOC, symbol count)\\n");
        preview.push_str("- Up to 50 file paths (top-level key files)\\n");
        preview.push_str("- Up to 100 symbol names (functions/structs/enums)\\n");

        let changed: Vec<String> = self
            .context
            .all_changed_files()
            .into_iter()
            .take(10)
            .map(|p| p.display().to_string())
            .collect();
        if !changed.is_empty() {
            preview.push_str("\\nChanged files (sample):\\n");
            for f in changed {
                preview.push_str(&format!("- `{}`\\n", f));
            }
        }

        let mem = self.repo_memory.to_prompt_context(6, 500);
        if !mem.trim().is_empty() {
            preview.push_str("\\nRepo memory (sample):\\n");
            preview.push_str(&mem);
            preview.push_str("\\n");
        }

        self.overlay = Overlay::InquiryPreview {
            question,
            preview,
            scroll: 0,
        };
    }

    /// Clear expired toast
    pub fn clear_expired_toast(&mut self) {
        if let Some(ref toast) = self.toast {
            if toast.is_expired() {
                self.toast = None;
            }
        }
    }

    /// Show a toast message (errors, rate limits, and success messages are displayed)
    pub fn show_toast(&mut self, message: &str) {
        let toast = Toast::new(message);
        if toast.is_error() {
            self.toast = Some(toast);
        } else if matches!(toast.kind, ToastKind::Success) {
            self.toast = Some(toast);
        }
        // Info toasts are silently ignored
    }

    // ═══════════════════════════════════════════════════════════════════════════
    //  RESET COSMOS OVERLAY
    // ═══════════════════════════════════════════════════════════════════════════

    /// Open the reset cosmos overlay with default options selected
    pub fn open_reset_overlay(&mut self) {
        use crate::cache::ResetOption;

        let defaults = ResetOption::defaults();
        let options: Vec<(ResetOption, bool)> = ResetOption::all()
            .into_iter()
            .map(|opt| {
                let selected = defaults.contains(&opt);
                (opt, selected)
            })
            .collect();

        self.overlay = Overlay::Reset {
            options,
            selected: 0,
        };
    }

    /// Navigate in reset overlay
    pub fn reset_navigate(&mut self, delta: isize) {
        if let Overlay::Reset { options, selected } = &mut self.overlay {
            let len = options.len();
            if len == 0 {
                return;
            }
            *selected = if delta > 0 {
                (*selected + delta as usize) % len
            } else {
                (*selected + len - ((-delta) as usize % len)) % len
            };
        }
    }

    /// Toggle selection of the currently focused reset option
    pub fn reset_toggle_selected(&mut self) {
        if let Overlay::Reset { options, selected } = &mut self.overlay {
            if let Some((_, is_selected)) = options.get_mut(*selected) {
                *is_selected = !*is_selected;
            }
        }
    }

    /// Get the selected reset options (returns empty vec if not in reset overlay)
    pub fn get_reset_selections(&self) -> Vec<crate::cache::ResetOption> {
        if let Overlay::Reset { options, .. } = &self.overlay {
            options
                .iter()
                .filter(|(_, selected)| *selected)
                .map(|(opt, _)| *opt)
                .collect()
        } else {
            Vec::new()
        }
    }

    // ═══════════════════════════════════════════════════════════════════════════
    //  STARTUP CHECK OVERLAY
    // ═══════════════════════════════════════════════════════════════════════════

    /// Show the startup check overlay when there's unsaved work
    pub fn show_startup_check(&mut self, changed_count: usize) {
        self.overlay = Overlay::StartupCheck {
            changed_count,
            confirming_discard: false,
        };
    }

    /// Set confirming_discard state in startup check overlay
    pub fn startup_check_confirm_discard(&mut self, confirming: bool) {
        if let Overlay::StartupCheck {
            confirming_discard, ..
        } = &mut self.overlay
        {
            *confirming_discard = confirming;
        }
    }

    /// Scroll overlay down
    pub fn overlay_scroll_down(&mut self) {
        match &mut self.overlay {
            Overlay::Help { scroll }
            | Overlay::InquiryPreview { scroll, .. }
            | Overlay::FileDetail { scroll, .. } => {
                *scroll += 1;
            }
            _ => {}
        }
    }

    /// Scroll overlay up
    pub fn overlay_scroll_up(&mut self) {
        match &mut self.overlay {
            Overlay::Help { scroll }
            | Overlay::InquiryPreview { scroll, .. }
            | Overlay::FileDetail { scroll, .. } => {
                *scroll = scroll.saturating_sub(1);
            }
            _ => {}
        }
    }

    /// Generate a commit message from pending changes using conventional commit format
    pub fn generate_commit_message(&self) -> String {
        if self.pending_changes.is_empty() {
            return "chore: apply changes".to_string();
        }

        if self.pending_changes.len() == 1 {
            // Single change: use description as the commit message
            let desc = &self.pending_changes[0].description;
            // If it already looks like a conventional commit, use as-is
            if desc.contains(':')
                && desc
                    .split(':')
                    .next()
                    .map(|s| s.len() < 15)
                    .unwrap_or(false)
            {
                desc.clone()
            } else {
                format!("fix: {}", lowercase_first(desc))
            }
        } else {
            // Multiple changes: create a summary with bullet points
            let summaries: Vec<String> = self
                .pending_changes
                .iter()
                .map(|c| format!("- {}", c.description))
                .collect();
            format!(
                "fix: apply {} improvements\n\n{}",
                self.pending_changes.len(),
                summaries.join("\n")
            )
        }
    }

    /// Generate human-friendly PR title and body from pending changes
    ///
    /// Returns (title, body) tuple with content written for non-technical readers
    /// while still including technical details for developers who want them.
    pub fn generate_pr_content(&self) -> (String, String) {
        if self.pending_changes.is_empty() {
            return (
                "Improvements".to_string(),
                "## Summary\n\nNo changes to describe.\n\n---\n*Applied with Cosmos*".to_string(),
            );
        }

        if self.pending_changes.len() == 1 {
            // Single change: use the friendly context directly
            let change = &self.pending_changes[0];

            let title = change.friendly_title.clone().unwrap_or_else(|| {
                // Fallback: extract a friendly title from description
                let desc = &change.description;
                if desc.len() > 50 {
                    format!("{}...", &desc[..47])
                } else {
                    desc.clone()
                }
            });

            let mut body = String::from("## Summary\n\n");

            // Add the problem summary if available
            if let Some(problem) = &change.problem_summary {
                body.push_str(problem);
                body.push_str("\n\n");
            }

            // Add the outcome if available
            if let Some(outcome) = &change.outcome {
                body.push_str("**The fix:** ");
                body.push_str(outcome);
                body.push_str("\n\n");
            }

            // Add technical details section
            body.push_str("## Details\n\n");
            if change.is_multi_file() {
                body.push_str(&format!(
                    "- **Files:** {} files modified\n",
                    change.files.len()
                ));
                for file_change in &change.files {
                    body.push_str(&format!("  - `{}`\n", file_change.path.display()));
                }
            } else {
                body.push_str(&format!("- **File:** `{}`\n", change.file_path().display()));
                let diff = change.diff();
                if !diff.is_empty() && !diff.starts_with("Modified areas:") {
                    body.push_str(&format!("- **Changes:** {}\n", diff));
                } else if diff.starts_with("Modified areas:") {
                    body.push_str(&format!("- {}\n", diff));
                }
            }

            body.push_str("\n---\n*Applied with Cosmos*");

            (title, body)
        } else {
            // Multiple changes: summarize themes
            let title = self.generate_pr_title_for_multiple_changes();

            let mut body = String::from("## Summary\n\n");
            body.push_str(&format!(
                "This PR addresses {} issues:\n\n",
                self.pending_changes.len()
            ));

            for change in &self.pending_changes {
                let change_title = change.friendly_title.as_deref().unwrap_or("Improvement");

                body.push_str(&format!("- **{}**: ", change_title));

                if let Some(problem) = &change.problem_summary {
                    body.push_str(problem);
                    if let Some(outcome) = &change.outcome {
                        body.push_str(&format!(" {}", outcome));
                    }
                } else {
                    body.push_str(&change.description);
                }
                body.push_str("\n\n");
            }

            // Add files changed section
            body.push_str("## Files Changed\n\n");
            for change in &self.pending_changes {
                for file_change in &change.files {
                    body.push_str(&format!("- `{}`\n", file_change.path.display()));
                }
            }

            body.push_str("\n---\n*Applied with Cosmos*");

            (title, body)
        }
    }

    /// Generate a title for PRs with multiple changes by finding common themes
    fn generate_pr_title_for_multiple_changes(&self) -> String {
        // Collect all friendly titles
        let titles: Vec<&str> = self
            .pending_changes
            .iter()
            .filter_map(|c| c.friendly_title.as_deref())
            .collect();

        if titles.is_empty() {
            // Fallback: generic title with count
            return format!("{} improvements", self.pending_changes.len());
        }

        if titles.len() == 1 {
            return titles[0].to_string();
        }

        if titles.len() == 2 {
            return format!("{} and {}", titles[0], titles[1]);
        }

        // For 3+ changes, list the first two and add "and more"
        format!(
            "{}, {}, and {} more",
            titles[0],
            titles[1],
            titles.len() - 2
        )
    }

    // ═══════════════════════════════════════════════════════════════════════════
    //  WORKFLOW NAVIGATION (right panel flow)
    // ═══════════════════════════════════════════════════════════════════════════

    /// Go back to the previous workflow step
    pub fn workflow_back(&mut self) {
        self.workflow_step = match self.workflow_step {
            WorkflowStep::Suggestions => WorkflowStep::Suggestions,
            WorkflowStep::Verify => WorkflowStep::Suggestions,
            WorkflowStep::Review => WorkflowStep::Verify,
            WorkflowStep::Ship => WorkflowStep::Review,
        };
    }

    /// Move to the Verify step with a selected suggestion
    /// Move to the Verify step with a multi-file suggestion
    pub fn start_verify_multi(
        &mut self,
        suggestion_id: uuid::Uuid,
        file_path: PathBuf,
        additional_files: Vec<PathBuf>,
        summary: String,
    ) {
        self.verify_state = VerifyState {
            suggestion_id: Some(suggestion_id),
            file_path: Some(file_path),
            additional_files,
            summary,
            preview: None,
            loading: true,
            scroll: 0,
            show_technical_details: false,
        };
        self.workflow_step = WorkflowStep::Verify;
        self.loading = LoadingState::GeneratingPreview;
    }

    /// Set the preview result in the Verify step
    pub fn set_verify_preview(&mut self, preview: crate::suggest::llm::FixPreview) {
        self.verify_state.preview = Some(preview);
        self.verify_state.loading = false;
        self.loading = LoadingState::None;
    }

    /// Scroll verify panel down
    pub fn verify_scroll_down(&mut self) {
        self.verify_state.scroll += 1;
    }

    /// Scroll verify panel up
    pub fn verify_scroll_up(&mut self) {
        self.verify_state.scroll = self.verify_state.scroll.saturating_sub(1);
    }

    /// Toggle technical details visibility in verify panel
    pub fn verify_toggle_details(&mut self) {
        self.verify_state.show_technical_details = !self.verify_state.show_technical_details;
    }

    /// Scroll ship panel down
    pub fn ship_scroll_down(&mut self) {
        self.ship_state.scroll += 1;
    }

    /// Scroll ship panel up
    pub fn ship_scroll_up(&mut self) {
        self.ship_state.scroll = self.ship_state.scroll.saturating_sub(1);
    }

    /// Move to the Review step after applying a fix
    pub fn start_review(
        &mut self,
        file_path: PathBuf,
        original_content: String,
        new_content: String,
    ) {
        self.review_state = ReviewState {
            file_path: Some(file_path),
            original_content,
            new_content,
            findings: Vec::new(),
            selected: std::collections::HashSet::new(),
            cursor: 0,
            summary: String::new(),
            scroll: 0,
            reviewing: true,
            fixing: false,
            confirm_ship: false,
            review_iteration: 1,
            fixed_titles: Vec::new(),
        };
        self.workflow_step = WorkflowStep::Review;
        self.loading = LoadingState::ReviewingChanges;
    }

    /// Set review findings from the adversarial reviewer
    pub fn set_review_findings(
        &mut self,
        findings: Vec<crate::suggest::llm::ReviewFinding>,
        summary: String,
    ) {
        self.review_state.findings = findings.clone();
        self.review_state.summary = summary;
        self.review_state.reviewing = false;
        self.review_state.confirm_ship = false;
        // Pre-select recommended findings
        for (i, finding) in findings.iter().enumerate() {
            if finding.recommended {
                self.review_state.selected.insert(i);
            }
        }
        self.loading = LoadingState::None;
    }

    /// Toggle selection of finding at cursor in review
    pub fn review_toggle_finding(&mut self) {
        let cursor = self.review_state.cursor;
        if cursor < self.review_state.findings.len() {
            if self.review_state.selected.contains(&cursor) {
                self.review_state.selected.remove(&cursor);
            } else {
                self.review_state.selected.insert(cursor);
            }
        }
        self.review_state.confirm_ship = false;
    }

    /// Select all findings in review
    pub fn review_select_all(&mut self) {
        for i in 0..self.review_state.findings.len() {
            self.review_state.selected.insert(i);
        }
        self.review_state.confirm_ship = false;
    }

    /// Move cursor up in review
    pub fn review_cursor_up(&mut self) {
        self.review_state.cursor = self.review_state.cursor.saturating_sub(1);
        if self.review_state.cursor < self.review_state.scroll {
            self.review_state.scroll = self.review_state.cursor;
        }
        self.review_state.confirm_ship = false;
    }

    /// Move cursor down in review
    pub fn review_cursor_down(&mut self) {
        if self.review_state.cursor + 1 < self.review_state.findings.len() {
            self.review_state.cursor += 1;
            let visible = 6;
            if self.review_state.cursor >= self.review_state.scroll + visible {
                self.review_state.scroll = self.review_state.cursor.saturating_sub(visible - 1);
            }
        }
        self.review_state.confirm_ship = false;
    }

    /// Check if review passed (no recommended fixes remaining)
    pub fn review_passed(&self) -> bool {
        if self.review_state.reviewing {
            return false;
        }
        !self.review_state.findings.iter().any(|f| f.recommended)
    }

    /// Get selected findings for fixing
    pub fn get_selected_review_findings(&self) -> Vec<crate::suggest::llm::ReviewFinding> {
        self.review_state
            .findings
            .iter()
            .enumerate()
            .filter(|(i, _)| self.review_state.selected.contains(i))
            .map(|(_, f)| f.clone())
            .collect()
    }

    /// Set review fixing state
    pub fn set_review_fixing(&mut self, fixing: bool) {
        self.review_state.fixing = fixing;
        if fixing {
            self.loading = LoadingState::ApplyingReviewFixes;
        }
    }

    /// Update review with new content after a fix, trigger re-review
    pub fn review_fix_complete(&mut self, new_content: String) {
        // Add fixed finding titles for context in next review
        for i in &self.review_state.selected {
            if let Some(f) = self.review_state.findings.get(*i) {
                self.review_state.fixed_titles.push(f.title.clone());
            }
        }

        self.review_state.new_content = new_content;
        self.review_state.findings.clear();
        self.review_state.selected.clear();
        self.review_state.summary.clear();
        self.review_state.reviewing = false;
        self.review_state.fixing = false;
        self.review_state.confirm_ship = false;
        self.review_state.review_iteration += 1;
        self.loading = LoadingState::None;
    }

    /// Move to the Ship step
    pub fn start_ship(&mut self) {
        // Gather changed files from pending changes (all files from multi-file changes)
        let files: Vec<PathBuf> = self
            .pending_changes
            .iter()
            .flat_map(|c| c.files.iter().map(|f| f.path.clone()))
            .collect();

        // Generate commit message using the shared method
        let commit_message = self.generate_commit_message();

        // Use existing cosmos branch or create name for new one
        let branch_name = self.cosmos_branch.clone().unwrap_or_else(|| {
            format!("cosmos-fix-{}", chrono::Utc::now().format("%Y%m%d-%H%M%S"))
        });

        self.ship_state = ShipState {
            branch_name,
            commit_message,
            files,
            step: ShipStep::Confirm,
            scroll: 0,
            pr_url: None,
        };
        self.workflow_step = WorkflowStep::Ship;
    }

    /// Update ship step progress
    pub fn set_ship_step(&mut self, step: ShipStep) {
        self.ship_state.step = step;
    }

    /// Set ship PR URL on completion
    pub fn set_ship_pr_url(&mut self, url: String) {
        self.ship_state.pr_url = Some(url);
        self.ship_state.step = ShipStep::Done;
    }

    /// Reset workflow to suggestions after shipping
    pub fn workflow_complete(&mut self) {
        self.workflow_step = WorkflowStep::Suggestions;
        self.verify_state = VerifyState::default();
        self.review_state = ReviewState::default();
        self.ship_state = ShipState::default();
        self.pending_changes.clear();
        self.cosmos_branch = None;
    }

    /// Check if currently on main/master branch
    pub fn is_on_main_branch(&self) -> bool {
        self.context.branch == "main" || self.context.branch == "master"
    }
}

/// Convert first character to lowercase for commit message formatting
fn lowercase_first(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        None => String::new(),
        Some(c) => c.to_lowercase().chain(chars).collect(),
    }
}

/// Build a flat file tree for display with sorting
fn build_file_tree(index: &CodebaseIndex) -> Vec<FlatTreeEntry> {
    use std::collections::BTreeSet;

    // Collect all unique directories from file paths
    let mut directories: BTreeSet<PathBuf> = BTreeSet::new();
    for path in index.files.keys() {
        let mut current = PathBuf::new();
        for component in path.components() {
            current.push(component);
            // Only add parent directories (not the file itself)
            if current != *path {
                directories.insert(current.clone());
            }
        }
    }

    // Build combined list of directories and files
    let mut all_entries: Vec<FlatTreeEntry> = Vec::new();

    // Add directories
    for dir_path in &directories {
        let depth = dir_path.components().count().saturating_sub(1);
        let name = dir_path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("")
            .to_string();

        all_entries.push(FlatTreeEntry {
            name,
            path: dir_path.clone(),
            is_dir: true,
            depth,
            priority: ' ',
        });
    }

    // Add files
    for (path, file_index) in &index.files {
        let priority = file_index.priority_indicator();
        let depth = path.components().count().saturating_sub(1);
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("")
            .to_string();

        all_entries.push(FlatTreeEntry {
            name,
            path: path.clone(),
            is_dir: false,
            depth,
            priority,
        });
    }

    // Hierarchical sort: by path, with directories before files at each level
    all_entries.sort_by(|a, b| {
        // Compare by full path, but ensure directories come before their contents
        // by comparing component by component
        let a_components: Vec<_> = a.path.components().collect();
        let b_components: Vec<_> = b.path.components().collect();

        // Compare each component
        for i in 0..a_components.len().min(b_components.len()) {
            let a_comp = a_components[i].as_os_str().to_string_lossy().to_lowercase();
            let b_comp = b_components[i].as_os_str().to_string_lossy().to_lowercase();

            match a_comp.cmp(&b_comp) {
                std::cmp::Ordering::Equal => continue,
                other => return other,
            }
        }

        // If all compared components are equal, shorter path (directory) comes first
        // This ensures parent directories come before their contents
        a_components.len().cmp(&b_components.len())
    });

    all_entries
}

/// Build a grouped tree for display
fn build_grouped_tree(
    grouping: &crate::grouping::CodebaseGrouping,
    index: &CodebaseIndex,
) -> Vec<crate::grouping::GroupedTreeEntry> {
    use crate::grouping::{GroupedTreeEntry, Layer};

    let mut entries = Vec::new();

    // Add layers in order
    for layer in Layer::all() {
        if let Some(group) = grouping.groups.get(layer) {
            if group.file_count() == 0 {
                continue;
            }

            // Add layer header
            entries.push(GroupedTreeEntry::layer(
                *layer,
                group.file_count(),
                group.expanded,
            ));

            if group.expanded {
                // Add features first, sorted by file count (largest first)
                let mut sorted_features: Vec<_> = group.features.iter().collect();
                sorted_features.sort_by(|a, b| b.files.len().cmp(&a.files.len()));

                for feature in sorted_features {
                    if feature.files.is_empty() {
                        continue;
                    }

                    // Add feature header
                    entries.push(GroupedTreeEntry::feature(
                        &feature.name,
                        feature.files.len(),
                        true,
                    ));

                    // Sort files: priority files first, then alphabetically
                    let mut sorted_files: Vec<_> = feature.files.iter().collect();
                    sorted_files.sort_by(|a, b| {
                        let pri_a = index
                            .files
                            .get(*a)
                            .map(|f| f.priority_indicator())
                            .unwrap_or(' ');
                        let pri_b = index
                            .files
                            .get(*b)
                            .map(|f| f.priority_indicator())
                            .unwrap_or(' ');
                        // Priority files (●) come first
                        match (pri_a == '●', pri_b == '●') {
                            (true, false) => std::cmp::Ordering::Less,
                            (false, true) => std::cmp::Ordering::Greater,
                            _ => a.cmp(b),
                        }
                    });

                    // Add files in this feature with contextual names
                    for file_path in sorted_files {
                        let priority = index
                            .files
                            .get(file_path)
                            .map(|f| f.priority_indicator())
                            .unwrap_or(' ');

                        // Use contextual display name for generic files
                        let name = crate::grouping::display_name_with_context(file_path);

                        entries.push(GroupedTreeEntry::file(
                            &name,
                            file_path.clone(),
                            priority,
                        ));
                    }
                }

                // Add ungrouped files with priority sorting
                let mut sorted_ungrouped: Vec<_> = group.ungrouped_files.iter().collect();
                sorted_ungrouped.sort_by(|a, b| {
                    let pri_a = index
                        .files
                        .get(*a)
                        .map(|f| f.priority_indicator())
                        .unwrap_or(' ');
                    let pri_b = index
                        .files
                        .get(*b)
                        .map(|f| f.priority_indicator())
                        .unwrap_or(' ');
                    match (pri_a == '●', pri_b == '●') {
                        (true, false) => std::cmp::Ordering::Less,
                        (false, true) => std::cmp::Ordering::Greater,
                        _ => a.cmp(b),
                    }
                });

                for file_path in sorted_ungrouped {
                    let priority = index
                        .files
                        .get(file_path)
                        .map(|f| f.priority_indicator())
                        .unwrap_or(' ');

                    // Use contextual display name
                    let name = crate::grouping::display_name_with_context(file_path);

                    entries.push(GroupedTreeEntry::file(
                        &name,
                        file_path.clone(),
                        priority,
                    ));
                }
            }
        }
    }

    entries
}

// ═══════════════════════════════════════════════════════════════════════════
//  RENDERING
// ═══════════════════════════════════════════════════════════════════════════

/// Main render function
pub fn render(frame: &mut Frame, app: &App) {
    let area = frame.area();

    // Clear with dark background
    frame.render_widget(Block::default().style(Style::default().bg(Theme::BG)), area);

    // Main layout - clean and minimal
    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3), // Header (logo)
            Constraint::Min(10),   // Main content
            Constraint::Length(3), // Footer
        ])
        .split(area);

    render_header(frame, layout[0], app);
    render_main(frame, layout[1], app);
    render_footer(frame, layout[2], app);

    // Loading is shown inline in the footer status bar (non-blocking)

    // Overlays
    match &app.overlay {
        Overlay::Help { scroll } => render_help(frame, *scroll),
        Overlay::InquiryPreview {
            question,
            preview,
            scroll,
        } => {
            render_inquiry_preview(frame, question, preview, *scroll);
        }
        Overlay::FileDetail { path, scroll } => {
            if let Some(file_index) = app.index.files.get(path) {
                render_file_detail(frame, path, file_index, app.get_llm_summary(path), *scroll);
            }
        }
        Overlay::Reset { options, selected } => {
            render_reset_overlay(frame, options, *selected);
        }
        Overlay::StartupCheck {
            changed_count,
            confirming_discard,
        } => {
            render_startup_check(frame, *changed_count, *confirming_discard);
        }
        Overlay::None => {}
    }

    // Toast
    if let Some(toast) = &app.toast {
        render_toast(frame, toast);
    }
}

fn render_header(frame: &mut Frame, area: Rect, _app: &App) {
    // Build spans for the logo
    let spans = vec![Span::styled(
        format!("   {}", Theme::COSMOS_LOGO),
        Style::default()
            .fg(Theme::WHITE)
            .add_modifier(Modifier::BOLD),
    )];

    let lines = vec![Line::from(""), Line::from(spans)];

    let header = Paragraph::new(lines).style(Style::default().bg(Theme::BG));
    frame.render_widget(header, area);
}

fn render_main(frame: &mut Frame, area: Rect, app: &App) {
    // Add horizontal padding for breathing room
    let padded = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Length(2), // Left padding
            Constraint::Min(10),   // Main content
            Constraint::Length(2), // Right padding
        ])
        .split(area);

    // Split into two panels with gap
    let panels = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(38), // Project tree
            Constraint::Length(2),      // Gap between panels
            Constraint::Percentage(62), // Suggestions (wider for wrapped text)
        ])
        .split(padded[1]);

    render_project_panel(frame, panels[0], app);
    render_suggestions_panel(frame, panels[2], app);
}

fn render_project_panel(frame: &mut Frame, area: Rect, app: &App) {
    let is_active = app.active_panel == ActivePanel::Project;
    let is_searching = app.input_mode == InputMode::Search;

    let border_style = if is_searching {
        Style::default().fg(Theme::WHITE) // Bright border when searching
    } else if is_active {
        Style::default().fg(Theme::GREY_300) // Bright active border
    } else {
        Style::default().fg(Theme::GREY_600) // Visible inactive border
    };

    // Account for search bar if searching
    let search_height = if is_searching || !app.search_query.is_empty() {
        2
    } else {
        0
    };
    let visible_height = area.height.saturating_sub(4 + search_height as u16) as usize;

    let mut lines = vec![];

    // Search bar
    if is_searching || !app.search_query.is_empty() {
        let search_text = if is_searching {
            format!(" / {}_", app.search_query)
        } else {
            format!(" / {} (Esc to clear)", app.search_query)
        };
        lines.push(Line::from(vec![Span::styled(
            search_text,
            Style::default().fg(Theme::WHITE),
        )]));
        lines.push(Line::from(""));
    } else {
        // Top padding for breathing room
        lines.push(Line::from(""));
    }

    // Render based on view mode
    match app.view_mode {
        ViewMode::Flat => {
            render_flat_tree(&mut lines, app, is_active, visible_height);
        }
        ViewMode::Grouped => {
            render_grouped_tree(&mut lines, app, is_active, visible_height);
        }
    }

    // Build title with view/sort indicator
    let total_items = app.project_tree_len();
    let scroll_indicator = if total_items > visible_height {
        let current = app.project_scroll + 1;
        format!(" ↕ {}/{} ", current, total_items)
    } else {
        String::new()
    };

    let mode_indicator = format!(" [{}]", app.view_mode.label());
    let title = format!(
        " {}{}{}",
        Theme::SECTION_PROJECT,
        mode_indicator,
        scroll_indicator
    );

    let block = Block::default()
        .title(title)
        .title_style(Style::default().fg(Theme::GREY_200)) // Legible title
        .borders(Borders::ALL)
        .border_style(border_style)
        .style(Style::default().bg(Theme::GREY_800));

    let paragraph = Paragraph::new(lines).block(block);
    frame.render_widget(paragraph, area);
}

/// Render the flat file tree
fn render_flat_tree<'a>(
    lines: &mut Vec<Line<'a>>,
    app: &'a App,
    is_active: bool,
    visible_height: usize,
) {
    let tree = &app.filtered_tree;
    let total = tree.len();

    for (i, entry) in tree
        .iter()
        .enumerate()
        .skip(app.project_scroll)
        .take(visible_height)
    {
        let is_selected = i == app.project_selected && is_active;

        // Calculate tree connectors
        let is_last = {
            if i + 1 >= total {
                true
            } else {
                tree[i + 1].depth <= entry.depth
            }
        };

        let connector = if is_last { "└" } else { "├" };
        let indent_str: String = (0..entry.depth.saturating_sub(1))
            .map(|d| {
                // Check if ancestor at this depth has more siblings
                let has_more = tree.iter().skip(i + 1).any(|e| e.depth == d + 1);
                if has_more {
                    "│ "
                } else {
                    "  "
                }
            })
            .collect();

        let (file_icon_str, icon_color) = if entry.is_dir {
            ("▸", Theme::GREY_400)
        } else {
            file_icon(&entry.name)
        };

        // Selection indicated by styling only (no cursor)
        let name_style = if is_selected {
            Style::default()
                .fg(Theme::WHITE)
                .add_modifier(Modifier::BOLD)
        } else if entry.is_dir {
            Style::default().fg(Theme::GREY_300)
        } else if entry.priority == Theme::PRIORITY_HIGH {
            Style::default().fg(Theme::GREY_200)
        } else {
            Style::default().fg(Theme::GREY_500)
        };

        let priority_indicator = if entry.priority == Theme::PRIORITY_HIGH {
            Span::styled(" ●", Style::default().fg(Theme::GREY_300))
        } else {
            Span::styled("", Style::default())
        };

        // Icon styling also reflects selection
        let icon_style = if is_selected {
            Style::default().fg(Theme::WHITE)
        } else {
            Style::default().fg(icon_color)
        };

        if entry.depth == 0 {
            // Root level - no connector
            lines.push(Line::from(vec![
                Span::styled("   ", Style::default()),
                Span::styled(format!("{} ", file_icon_str), icon_style),
                Span::styled(entry.name.clone(), name_style),
                priority_indicator,
            ]));
        } else {
            lines.push(Line::from(vec![
                Span::styled("   ", Style::default()),
                Span::styled(
                    format!("{}{}", indent_str, connector),
                    Style::default().fg(Theme::GREY_700),
                ),
                Span::styled(format!(" {} ", file_icon_str), icon_style),
                Span::styled(entry.name.clone(), name_style),
                priority_indicator,
            ]));
        }
    }
}

/// Get file type icon based on extension - minimal and clean
fn file_icon(name: &str) -> (&'static str, ratatui::style::Color) {
    let ext = name.rsplit('.').next().unwrap_or("");
    match ext {
        // React/JSX - subtle blue tint
        "tsx" | "jsx" => ("›", Theme::BADGE_QUALITY),
        // TypeScript - subtle yellow
        "ts" => ("›", Theme::BADGE_DOCS),
        // JavaScript
        "js" | "mjs" | "cjs" => ("›", Theme::BADGE_DOCS),
        // Styles - purple
        "css" | "scss" | "sass" | "less" => ("◈", Theme::BADGE_REFACTOR),
        // Data files - muted
        "json" | "yaml" | "yml" | "toml" => ("○", Theme::GREY_600),
        // Rust - orange
        "rs" => ("●", Theme::BADGE_SECURITY),
        // Python - teal
        "py" => ("●", Theme::BADGE_PERF),
        // Go - blue
        "go" => ("●", Theme::BADGE_QUALITY),
        // Config - very muted
        "env" | "config" => ("○", Theme::GREY_700),
        // Markdown - muted
        "md" | "mdx" => ("○", Theme::GREY_600),
        // Tests - teal indicator
        _ if name.contains("test") || name.contains("spec") => ("◎", Theme::BADGE_PERF),
        // Default - minimal dot
        _ => ("·", Theme::GREY_600),
    }
}

/// Render the grouped file tree
fn render_grouped_tree<'a>(
    lines: &mut Vec<Line<'a>>,
    app: &'a App,
    is_active: bool,
    visible_height: usize,
) {
    use crate::grouping::GroupedEntryKind;

    let tree = &app.filtered_grouped_tree;

    for (i, entry) in tree
        .iter()
        .enumerate()
        .skip(app.project_scroll)
        .take(visible_height)
    {
        let is_selected = i == app.project_selected && is_active;

        match &entry.kind {
            GroupedEntryKind::Layer(_layer) => {
                // Add spacing before layer (except first)
                if i > 0 && app.project_scroll == 0
                    || (i > app.project_scroll && app.project_scroll > 0)
                {
                    // Check if previous visible item was a file - add separator
                    if i > 0 {
                        if let Some(prev) = tree.get(i.saturating_sub(1)) {
                            if prev.kind == GroupedEntryKind::File {
                                lines.push(Line::from(""));
                            }
                        }
                    }
                }

                // Layer header - selection via styling only, expand icon shows state
                let expand_icon = if entry.expanded { "▾" } else { "▸" };
                let count_str = format!(" {}", entry.file_count);

                let (expand_style, name_style, count_style) = if is_selected {
                    (
                        Style::default().fg(Theme::WHITE),
                        Style::default()
                            .fg(Theme::WHITE)
                            .add_modifier(Modifier::BOLD),
                        Style::default().fg(Theme::GREY_200),
                    )
                } else {
                    (
                        Style::default().fg(Theme::GREY_500),
                        Style::default().fg(Theme::GREY_100),
                        Style::default().fg(Theme::GREY_600),
                    )
                };

                lines.push(Line::from(vec![
                    Span::styled("   ", Style::default()),
                    Span::styled(expand_icon.to_string(), expand_style),
                    Span::styled(format!(" {}", entry.name), name_style),
                    Span::styled(count_str, count_style),
                ]));
            }
            GroupedEntryKind::Feature => {
                // Feature header - selection via styling only
                let style = if is_selected {
                    Style::default()
                        .fg(Theme::WHITE)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(Theme::GREY_300)
                };

                let count_str = format!(" {}", entry.file_count);

                lines.push(Line::from(vec![
                    Span::styled("   ", Style::default()),
                    Span::styled("   ├─ ", Style::default().fg(Theme::GREY_700)),
                    Span::styled(entry.name.clone(), style),
                    Span::styled(count_str, Style::default().fg(Theme::GREY_600)),
                ]));
            }
            GroupedEntryKind::File => {
                // File display - selection via styling only
                let (file_icon_str, icon_color) = file_icon(&entry.name);

                let name_style = if is_selected {
                    Style::default()
                        .fg(Theme::WHITE)
                        .add_modifier(Modifier::BOLD)
                } else if entry.priority == Theme::PRIORITY_HIGH {
                    Style::default().fg(Theme::GREY_200)
                } else {
                    Style::default().fg(Theme::GREY_500)
                };

                let icon_style = if is_selected {
                    Style::default().fg(Theme::WHITE)
                } else {
                    Style::default().fg(icon_color)
                };

                let priority_indicator = if entry.priority == Theme::PRIORITY_HIGH {
                    Span::styled(" ●", Style::default().fg(Theme::GREY_400))
                } else {
                    Span::styled("", Style::default())
                };

                // Simple indentation with subtle vertical guide
                let indent = "     │  ";

                lines.push(Line::from(vec![
                    Span::styled("   ", Style::default()),
                    Span::styled(indent.to_string(), Style::default().fg(Theme::GREY_800)),
                    Span::styled(format!("{} ", file_icon_str), icon_style),
                    Span::styled(entry.name.clone(), name_style),
                    priority_indicator,
                ]));
            }
        }
    }
}

fn render_suggestions_panel(frame: &mut Frame, area: Rect, app: &App) {
    let is_active = app.active_panel == ActivePanel::Suggestions;
    let is_question_mode = app.input_mode == InputMode::Question;

    let border_style = if is_question_mode {
        Style::default().fg(Theme::WHITE) // Bright border when in question mode
    } else if is_active {
        Style::default().fg(Theme::GREY_300)
    } else {
        Style::default().fg(Theme::GREY_600)
    };

    // Reserve space for border (2 lines top/bottom)
    let visible_height = area.height.saturating_sub(2) as usize;
    let inner_width = area.width.saturating_sub(4) as usize;

    let mut lines = vec![];

    // Question input mode takes highest priority
    if is_question_mode {
        render_question_mode_content(&mut lines, app, visible_height);
    } else if let Some(ask_state) = &app.ask_cosmos_state {
        // Ask cosmos response display
        render_ask_cosmos_content(&mut lines, ask_state, app, visible_height, inner_width);
    } else if app.loading == LoadingState::Answering {
        render_ask_cosmos_loading(&mut lines, app);
    } else {
        // Render content based on workflow step
        match app.workflow_step {
            WorkflowStep::Suggestions => {
                render_suggestions_content(&mut lines, app, is_active, visible_height, inner_width);
            }
            WorkflowStep::Verify => {
                render_verify_content(&mut lines, app, visible_height, inner_width);
            }
            WorkflowStep::Review => {
                render_review_content(&mut lines, app, visible_height, inner_width);
            }
            WorkflowStep::Ship => {
                render_ship_content(&mut lines, app, visible_height, inner_width);
            }
        }
    }

    // Build title with workflow breadcrumbs in the border (italic, lowercase like project panel)
    let ask_cosmos_active = is_question_mode
        || app.ask_cosmos_state.is_some()
        || app.loading == LoadingState::Answering;
    let title = render_workflow_title(app.workflow_step, ask_cosmos_active);

    let block = Block::default()
        .title(title)
        .title_style(Style::default().fg(Theme::GREY_200))
        .borders(Borders::ALL)
        .border_style(border_style)
        .style(Style::default().bg(Theme::GREY_800));

    let paragraph = Paragraph::new(lines).block(block);
    frame.render_widget(paragraph, area);
}

/// Build the workflow title for the border (italic, lowercase like project panel)
fn render_workflow_title(current: WorkflowStep, ask_cosmos_active: bool) -> String {
    // When in ask cosmos mode, show simple title (italicized like other panels)
    if ask_cosmos_active {
        return " 𝘢𝘴𝘬 𝘤𝘰𝘴𝘮𝘰𝘴 ".to_string();
    }

    let steps = [
        (WorkflowStep::Suggestions, Theme::WORKFLOW_SUGGESTIONS),
        (WorkflowStep::Verify, Theme::WORKFLOW_VERIFY),
        (WorkflowStep::Review, Theme::WORKFLOW_REVIEW),
        (WorkflowStep::Ship, Theme::WORKFLOW_SHIP),
    ];

    let mut parts = Vec::new();
    for (step, label) in steps.iter() {
        if *step == current {
            // Current step is shown (with underline effect via brackets)
            parts.push(format!("[{}]", label));
        } else if step.index() < current.index() {
            // Completed steps shown normally
            parts.push(label.to_string());
        } else {
            // Future steps shown dimmer (just show them)
            parts.push(label.to_string());
        }
    }

    format!(" {} ", parts.join(" › "))
}

/// Render the Suggestions step content
fn render_suggestions_content<'a>(
    lines: &mut Vec<Line<'a>>,
    app: &App,
    is_active: bool,
    visible_height: usize,
    inner_width: usize,
) {
    use crate::suggest::Priority;

    let suggestions = app.suggestions.active_suggestions();

    // Top padding for breathing room (matching project panel)
    lines.push(Line::from(""));

    // Check for loading states relevant to suggestions panel
    let loading_message: Option<String> = match app.loading {
        LoadingState::GeneratingSuggestions => {
            if let Some((completed, total)) = app.summary_progress {
                Some(format!(
                    "Generating suggestions... (summaries: {}/{})",
                    completed, total
                ))
            } else {
                Some("Generating suggestions...".to_string())
            }
        }
        LoadingState::GeneratingSummaries => {
            if let Some((completed, total)) = app.summary_progress {
                Some(format!("Summarizing files... ({}/{})", completed, total))
            } else {
                Some("Summarizing files...".to_string())
            }
        }
        LoadingState::Answering => Some("Thinking...".to_string()),
        _ => None,
    };

    if let Some(message) = loading_message {
        let spinner = SPINNER_FRAMES[app.loading_frame % SPINNER_FRAMES.len()];
        lines.push(Line::from(vec![
            Span::styled("    ", Style::default()),
            Span::styled(format!("{} ", spinner), Style::default().fg(Theme::WHITE)),
            Span::styled(message, Style::default().fg(Theme::GREY_300)),
        ]));
        return;
    }

    if suggestions.is_empty() {
        let has_ai = crate::suggest::llm::is_available();

        lines.push(Line::from(vec![
            Span::styled("    ╭", Style::default().fg(Theme::GREY_700)),
            Span::styled(
                "──────────────────────────────────",
                Style::default().fg(Theme::GREY_700),
            ),
            Span::styled("╮", Style::default().fg(Theme::GREY_700)),
        ]));
        lines.push(Line::from(vec![
            Span::styled("    │", Style::default().fg(Theme::GREY_700)),
            Span::styled("                                  ", Style::default()),
            Span::styled("│", Style::default().fg(Theme::GREY_700)),
        ]));

        if has_ai {
            lines.push(Line::from(vec![
                Span::styled("    │", Style::default().fg(Theme::GREY_700)),
                Span::styled("       + ", Style::default().fg(Theme::GREEN)),
                Span::styled("No issues found", Style::default().fg(Theme::GREY_300)),
                Span::styled("          │", Style::default().fg(Theme::GREY_700)),
            ]));
            lines.push(Line::from(vec![
                Span::styled("    │", Style::default().fg(Theme::GREY_700)),
                Span::styled(
                    "         Nothing to suggest",
                    Style::default().fg(Theme::GREY_500),
                ),
                Span::styled("       │", Style::default().fg(Theme::GREY_700)),
            ]));
        } else {
            lines.push(Line::from(vec![
                Span::styled("    │", Style::default().fg(Theme::GREY_700)),
                Span::styled("       ☽ ", Style::default().fg(Theme::GREY_400)),
                Span::styled("AI not configured", Style::default().fg(Theme::GREY_300)),
                Span::styled("        │", Style::default().fg(Theme::GREY_700)),
            ]));
            lines.push(Line::from(vec![
                Span::styled("    │", Style::default().fg(Theme::GREY_700)),
                Span::styled("                                  ", Style::default()),
                Span::styled("│", Style::default().fg(Theme::GREY_700)),
            ]));
            lines.push(Line::from(vec![
                Span::styled("    │", Style::default().fg(Theme::GREY_700)),
                Span::styled(
                    "   cosmos --setup    ",
                    Style::default().fg(Theme::GREY_500),
                ),
                Span::styled("(BYOK)", Style::default().fg(Theme::GREY_600)),
                Span::styled("   │", Style::default().fg(Theme::GREY_700)),
            ]));
        }

        lines.push(Line::from(vec![
            Span::styled("    │", Style::default().fg(Theme::GREY_700)),
            Span::styled("                                  ", Style::default()),
            Span::styled("│", Style::default().fg(Theme::GREY_700)),
        ]));
        lines.push(Line::from(vec![
            Span::styled("    ╰", Style::default().fg(Theme::GREY_700)),
            Span::styled(
                "──────────────────────────────────",
                Style::default().fg(Theme::GREY_700),
            ),
            Span::styled("╯", Style::default().fg(Theme::GREY_700)),
        ]));
        return;
    }

    let mut line_count = 0;
    // Use nearly full width - just leave small margin
    let text_width = inner_width.saturating_sub(4);

    for (i, suggestion) in suggestions.iter().enumerate().skip(app.suggestion_scroll) {
        if line_count >= visible_height.saturating_sub(4) {
            break;
        }

        let is_selected = i == app.suggestion_selected && is_active;

        // Build priority indicator with red exclamation points for critical items
        let priority_indicator = match suggestion.priority {
            Priority::High => Span::styled(
                "!! ",
                Style::default().fg(Theme::RED).add_modifier(Modifier::BOLD),
            ),
            Priority::Medium => Span::styled("!  ", Style::default().fg(Theme::YELLOW)),
            Priority::Low => Span::styled("   ", Style::default()),
        };

        // Kind label with subtle styling - brighter when selected
        let kind_label = suggestion.kind.label();
        let kind_style = if is_selected {
            Style::default().fg(Theme::GREY_100)
        } else {
            Style::default().fg(Theme::GREY_500)
        };

        // Multi-file indicator
        let multi_file_indicator = if suggestion.is_multi_file() {
            format!(" [{}]", suggestion.file_count())
        } else {
            String::new()
        };
        let multi_file_style = Style::default().fg(Theme::ACCENT);

        // Summary text style - selection via styling only (bold + bright)
        let summary_style = if is_selected {
            Style::default()
                .fg(Theme::WHITE)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Theme::GREY_300)
        };

        // First line has: padding (2) + priority (3) + kind + multi-file indicator + ": "
        let first_prefix_len = 2 + 3 + kind_label.len() + multi_file_indicator.len() + 2;
        let first_line_width = text_width.saturating_sub(first_prefix_len);
        // Continuation lines just have small indent (5 chars)
        let cont_indent = "     ";
        let cont_line_width = text_width.saturating_sub(5);

        // Use variable width wrapping: first line is shorter due to prefix
        let wrapped =
            wrap_text_variable_width(&suggestion.summary, first_line_width, cont_line_width);

        // Render first line with priority, kind, and multi-file indicator
        if let Some(first_line) = wrapped.first() {
            let mut spans = vec![
                Span::styled("  ", Style::default()),
                priority_indicator,
                Span::styled(kind_label, kind_style),
            ];
            if suggestion.is_multi_file() {
                spans.push(Span::styled(multi_file_indicator, multi_file_style));
            }
            spans.push(Span::styled(": ", kind_style));
            spans.push(Span::styled(first_line.clone(), summary_style));
            lines.push(Line::from(spans));
            line_count += 1;
        }

        // Render ALL continuation lines (no artificial limit)
        for wrapped_line in wrapped.iter().skip(1) {
            if line_count >= visible_height.saturating_sub(4) {
                break;
            }
            lines.push(Line::from(vec![
                Span::styled(cont_indent, Style::default()),
                Span::styled(wrapped_line.clone(), summary_style),
            ]));
            line_count += 1;
        }

        // Add empty line for spacing between suggestions
        if line_count < visible_height.saturating_sub(4) {
            lines.push(Line::from(""));
            line_count += 1;
        }
    }

    // Bottom hints
    let content_lines = lines.len();
    let available = visible_height;
    if content_lines < available {
        for _ in 0..(available - content_lines).saturating_sub(2) {
            lines.push(Line::from(""));
        }
    }

    // Show scroll indicator
    if suggestions.len() > 3 {
        lines.push(Line::from(vec![Span::styled(
            format!("  ↕ {}/{}", app.suggestion_selected + 1, suggestions.len()),
            Style::default().fg(Theme::GREY_500),
        )]));
    }
}

/// Render the Verify step content
fn render_verify_content<'a>(
    lines: &mut Vec<Line<'a>>,
    app: &App,
    visible_height: usize,
    inner_width: usize,
) {
    let state = &app.verify_state;

    if state.loading || app.loading == LoadingState::GeneratingFix {
        let spinner = SPINNER_FRAMES[app.loading_frame % SPINNER_FRAMES.len()];
        let message = if app.loading == LoadingState::GeneratingFix {
            "Applying fix..."
        } else {
            "Verifying issue..."
        };
        lines.push(Line::from(""));
        lines.push(Line::from(vec![
            Span::styled("    ", Style::default()),
            Span::styled(format!("{} ", spinner), Style::default().fg(Theme::WHITE)),
            Span::styled(message, Style::default().fg(Theme::GREY_300)),
        ]));
        return;
    }

    // Build all scrollable content first
    let mut content: Vec<Line<'a>> = Vec::new();
    let text_width = inner_width.saturating_sub(6);

    // Show preview content (non-technical, user-friendly)
    if let Some(preview) = &state.preview {
        // Friendly title header (no file names)
        content.push(Line::from(vec![Span::styled(
            format!("  {}", preview.friendly_title),
            Style::default()
                .fg(Theme::WHITE)
                .add_modifier(Modifier::BOLD),
        )]));
        content.push(Line::from(""));

        // Problem summary (behavior-focused)
        for line in wrap_text(&preview.problem_summary, text_width) {
            content.push(Line::from(vec![Span::styled(
                format!("  {}", line),
                Style::default().fg(Theme::GREY_200),
            )]));
        }
        content.push(Line::from(""));

        // Separator
        content.push(Line::from(vec![Span::styled(
            "  ─────────────────────────────────",
            Style::default().fg(Theme::GREY_700),
        )]));
        content.push(Line::from(""));

        // Simple verification status (no verbose explanation)
        let (status_icon, status_text, status_color) = if preview.verified {
            ("✓", "Confirmed", Theme::GREEN)
        } else {
            ("?", "Uncertain", Theme::BADGE_BUG)
        };
        content.push(Line::from(vec![
            Span::styled(
                format!("  {} ", status_icon),
                Style::default().fg(status_color),
            ),
            Span::styled(status_text, Style::default().fg(status_color)),
        ]));
        content.push(Line::from(""));

        // The fix (outcome-focused)
        content.push(Line::from(vec![Span::styled(
            "  The fix:",
            Style::default().fg(Theme::GREY_400),
        )]));
        for line in wrap_text(&preview.outcome, text_width) {
            content.push(Line::from(vec![Span::styled(
                format!("  {}", line),
                Style::default().fg(Theme::GREY_200),
            )]));
        }
        content.push(Line::from(""));

        // Show multi-file indicator if this affects multiple files
        if state.is_multi_file() {
            content.push(Line::from(vec![
                Span::styled("  Files affected:", Style::default().fg(Theme::GREY_400)),
                Span::styled(
                    format!(" {}", state.file_count()),
                    Style::default().fg(Theme::ACCENT),
                ),
            ]));
            // List all affected files
            if let Some(primary) = &state.file_path {
                let file_name = primary
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("file");
                content.push(Line::from(vec![Span::styled(
                    format!("    · {}", file_name),
                    Style::default().fg(Theme::GREY_300),
                )]));
            }
            for additional in &state.additional_files {
                let file_name = additional
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("file");
                content.push(Line::from(vec![Span::styled(
                    format!("    · {}", file_name),
                    Style::default().fg(Theme::GREY_300),
                )]));
            }
            content.push(Line::from(""));
        }

        // Technical details section (toggleable)
        if state.show_technical_details {
            content.push(Line::from(vec![Span::styled(
                "  ─────────────────────────────────",
                Style::default().fg(Theme::GREY_700),
            )]));
            content.push(Line::from(vec![Span::styled(
                "  Technical Details",
                Style::default()
                    .fg(Theme::GREY_300)
                    .add_modifier(Modifier::BOLD),
            )]));
            content.push(Line::from(""));

            // Verification note
            if !preview.verification_note.is_empty() {
                content.push(Line::from(vec![Span::styled(
                    "  Verification:",
                    Style::default().fg(Theme::GREY_400),
                )]));
                for line in wrap_text(&preview.verification_note, text_width) {
                    content.push(Line::from(vec![Span::styled(
                        format!("  {}", line),
                        Style::default().fg(Theme::GREY_300),
                    )]));
                }
                content.push(Line::from(""));
            }

            // Evidence snippet with line number
            if let Some(snippet) = &preview.evidence_snippet {
                content.push(Line::from(vec![
                    Span::styled("  Evidence", Style::default().fg(Theme::GREY_400)),
                    if let Some(line_num) = preview.evidence_line {
                        Span::styled(
                            format!(" (line {})", line_num),
                            Style::default().fg(Theme::GREY_500),
                        )
                    } else {
                        Span::styled("", Style::default())
                    },
                    Span::styled(":", Style::default().fg(Theme::GREY_400)),
                ]));
                // Show the code snippet in a monospace style
                for line in snippet.lines().take(8) {
                    let truncated = if line.len() > text_width.saturating_sub(4) {
                        format!("{}...", &line[..text_width.saturating_sub(7)])
                    } else {
                        line.to_string()
                    };
                    content.push(Line::from(vec![Span::styled(
                        format!("    {}", truncated),
                        Style::default().fg(Theme::GREY_400),
                    )]));
                }
                content.push(Line::from(""));
            }

            // Implementation description
            if !preview.description.is_empty() {
                content.push(Line::from(vec![Span::styled(
                    "  Implementation:",
                    Style::default().fg(Theme::GREY_400),
                )]));
                for line in wrap_text(&preview.description, text_width) {
                    content.push(Line::from(vec![Span::styled(
                        format!("  {}", line),
                        Style::default().fg(Theme::GREY_300),
                    )]));
                }
                content.push(Line::from(""));
            }

            // Affected areas
            if !preview.affected_areas.is_empty() {
                content.push(Line::from(vec![Span::styled(
                    "  Affected areas:",
                    Style::default().fg(Theme::GREY_400),
                )]));
                for area in &preview.affected_areas {
                    content.push(Line::from(vec![Span::styled(
                        format!("    · {}", area),
                        Style::default().fg(Theme::GREY_300),
                    )]));
                }
                content.push(Line::from(""));
            }

            // Scope indicator
            content.push(Line::from(vec![
                Span::styled("  Scope: ", Style::default().fg(Theme::GREY_400)),
                Span::styled(
                    preview.scope.label(),
                    Style::default().fg(match preview.scope {
                        crate::suggest::llm::FixScope::Small => Theme::GREEN,
                        crate::suggest::llm::FixScope::Medium => Theme::BADGE_PERF,
                        crate::suggest::llm::FixScope::Large => Theme::BADGE_BUG,
                    }),
                ),
            ]));
            content.push(Line::from(""));
        } else {
            // Hint to show technical details
            content.push(Line::from(vec![Span::styled(
                "  Press 'd' for technical details",
                Style::default().fg(Theme::GREY_600),
            )]));
            content.push(Line::from(""));
        }
    } else {
        // Fallback when no preview yet - show technical summary
        let file_name = state
            .file_path
            .as_ref()
            .and_then(|p| p.file_name())
            .and_then(|n| n.to_str())
            .unwrap_or("file");

        content.push(Line::from(vec![Span::styled(
            format!("  {}", file_name),
            Style::default()
                .fg(Theme::WHITE)
                .add_modifier(Modifier::BOLD),
        )]));
        content.push(Line::from(""));

        for line in wrap_text(&state.summary, text_width) {
            content.push(Line::from(vec![Span::styled(
                format!("  {}", line),
                Style::default().fg(Theme::GREY_200),
            )]));
        }
        content.push(Line::from(""));
    }

    // Use full visible height for scrollable content
    let scrollable_height = visible_height.saturating_sub(2); // Leave room for scroll indicator
    let total_content = content.len();
    let scroll = state.scroll.min(total_content.saturating_sub(1));

    // Apply scroll and take visible lines
    for line in content.into_iter().skip(scroll).take(scrollable_height) {
        lines.push(line);
    }

    // Scroll indicator if needed
    if total_content > scrollable_height {
        // Pad to bottom
        while lines.len() < scrollable_height {
            lines.push(Line::from(""));
        }
        lines.push(Line::from(vec![
            Span::styled(
                "  ─────────────────────────────────",
                Style::default().fg(Theme::GREY_700),
            ),
            Span::styled(
                format!(
                    " ↕ {}/{} ",
                    scroll + 1,
                    total_content.saturating_sub(scrollable_height) + 1
                ),
                Style::default().fg(Theme::GREY_500),
            ),
        ]));
    }
}

/// Render the Review step content  
fn render_review_content<'a>(
    lines: &mut Vec<Line<'a>>,
    app: &'a App,
    visible_height: usize,
    inner_width: usize,
) {
    let state = &app.review_state;

    if state.reviewing {
        let spinner = SPINNER_FRAMES[app.loading_frame % SPINNER_FRAMES.len()];
        lines.push(Line::from(""));
        lines.push(Line::from(vec![
            Span::styled("    ", Style::default()),
            Span::styled(format!("{} ", spinner), Style::default().fg(Theme::WHITE)),
            Span::styled("Reviewing changes...", Style::default().fg(Theme::GREY_300)),
        ]));
        return;
    }

    if state.fixing {
        let spinner = SPINNER_FRAMES[app.loading_frame % SPINNER_FRAMES.len()];
        lines.push(Line::from(""));
        lines.push(Line::from(vec![
            Span::styled("    ", Style::default()),
            Span::styled(format!("{} ", spinner), Style::default().fg(Theme::WHITE)),
            Span::styled("Applying fixes...", Style::default().fg(Theme::GREY_300)),
        ]));
        return;
    }

    let file_name = state
        .file_path
        .as_ref()
        .and_then(|p| p.file_name())
        .and_then(|n| n.to_str())
        .unwrap_or("file");

    // Header: file name with optional round indicator (no "Review" label - shown in workflow breadcrumb)
    lines.push(Line::from(vec![
        Span::styled(
            format!("  {}", file_name),
            Style::default()
                .fg(Theme::WHITE)
                .add_modifier(Modifier::BOLD),
        ),
        if state.review_iteration > 1 {
            Span::styled(
                format!(" (round {})", state.review_iteration),
                Style::default().fg(Theme::GREY_400),
            )
        } else {
            Span::styled("", Style::default())
        },
    ]));
    lines.push(Line::from(""));

    // Check if review passed
    if state.findings.is_empty() && !state.summary.is_empty() {
        lines.push(Line::from(vec![
            Span::styled("  + ", Style::default().fg(Theme::GREEN)),
            Span::styled(
                "No issues found!",
                Style::default()
                    .fg(Theme::GREEN)
                    .add_modifier(Modifier::BOLD),
            ),
        ]));
        lines.push(Line::from(""));

        let text_width = inner_width.saturating_sub(6);
        for line in wrap_text(&state.summary, text_width) {
            lines.push(Line::from(vec![Span::styled(
                format!("  {}", line),
                Style::default().fg(Theme::GREY_300),
            )]));
        }

        // Action to continue to ship
        let content_lines = lines.len();
        if content_lines < visible_height {
            for _ in 0..(visible_height - content_lines).saturating_sub(3) {
                lines.push(Line::from(""));
            }
        }

        lines.push(Line::from(vec![Span::styled(
            "  ─────────────────────────────────",
            Style::default().fg(Theme::GREY_700),
        )]));
        lines.push(Line::from(""));
        lines.push(Line::from(vec![
            Span::styled("  ", Style::default()),
            Span::styled(" ↵ ", Style::default().fg(Theme::GREY_900).bg(Theme::GREEN)),
            Span::styled(" Continue to Ship  ", Style::default().fg(Theme::GREY_300)),
            Span::styled(
                " r ",
                Style::default().fg(Theme::GREY_900).bg(Theme::GREY_500),
            ),
            Span::styled(" Re-review", Style::default().fg(Theme::GREY_400)),
        ]));
        return;
    }

    // Show findings
    if !state.findings.is_empty() {
        let selected_count = state.selected.len();
        let total_findings = state.findings.len();

        // Reserve lines for header (file name, empty line, findings count, empty line = 4 lines)
        let visible_findings = visible_height.saturating_sub(4);

        lines.push(Line::from(vec![
            Span::styled(
                format!("  {} findings", total_findings),
                Style::default().fg(Theme::WHITE),
            ),
            Span::styled(" · ", Style::default().fg(Theme::GREY_600)),
            Span::styled(
                format!("{} selected", selected_count),
                Style::default().fg(if selected_count > 0 {
                    Theme::WHITE
                } else {
                    Theme::GREY_500
                }),
            ),
            if total_findings > visible_findings {
                Span::styled(
                    format!(
                        " · ↕ {}/{}",
                        state.scroll + 1,
                        total_findings.saturating_sub(visible_findings) + 1
                    ),
                    Style::default().fg(Theme::GREY_500),
                )
            } else {
                Span::styled("", Style::default())
            },
        ]));
        lines.push(Line::from(""));

        for (i, finding) in state
            .findings
            .iter()
            .enumerate()
            .skip(state.scroll)
            .take(visible_findings)
        {
            let is_selected = state.selected.contains(&i);
            let is_cursor = i == state.cursor;

            let checkbox = if is_selected { "[×]" } else { "[ ]" };
            let cursor_indicator = if is_cursor { "›" } else { " " };

            let title_style = if is_cursor {
                Style::default()
                    .fg(Theme::WHITE)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Theme::GREY_200)
            };

            // Wrap the title to use full available width (cursor + checkbox + padding = ~8 chars)
            let title_width = inner_width.saturating_sub(8);
            let wrapped_title = wrap_text(&finding.title, title_width);

            // First line with cursor and checkbox
            lines.push(Line::from(vec![
                Span::styled(
                    format!(" {} ", cursor_indicator),
                    Style::default().fg(if is_cursor {
                        Theme::WHITE
                    } else {
                        Theme::GREY_700
                    }),
                ),
                Span::styled(
                    format!("{} ", checkbox),
                    Style::default().fg(if is_selected {
                        Theme::WHITE
                    } else {
                        Theme::GREY_500
                    }),
                ),
                Span::styled(
                    wrapped_title.first().cloned().unwrap_or_default(),
                    title_style.clone(),
                ),
            ]));

            // Continue wrapped title lines (indented to align with title start)
            for title_line in wrapped_title.iter().skip(1) {
                lines.push(Line::from(vec![Span::styled(
                    format!("        {}", title_line),
                    title_style.clone(),
                )]));
            }

            // Show description for cursor item (more lines, better formatting)
            if is_cursor && !finding.description.is_empty() {
                let desc_width = inner_width.saturating_sub(10);
                for desc_line in wrap_text(&finding.description, desc_width).iter().take(4) {
                    lines.push(Line::from(vec![Span::styled(
                        format!("        {}", desc_line),
                        Style::default().fg(Theme::GREY_400),
                    )]));
                }
            }
        }
    }
}

/// Render the Ship step content
fn render_ship_content<'a>(
    lines: &mut Vec<Line<'a>>,
    app: &'a App,
    visible_height: usize,
    inner_width: usize,
) {
    let state = &app.ship_state;
    let text_width = inner_width.saturating_sub(6);

    match state.step {
        ShipStep::Done => {
            // Build scrollable content
            let mut content: Vec<Line<'a>> = Vec::new();

            content.push(Line::from(vec![
                Span::styled("  + ", Style::default().fg(Theme::GREEN)),
                Span::styled(
                    "Pull request created!",
                    Style::default()
                        .fg(Theme::GREEN)
                        .add_modifier(Modifier::BOLD),
                ),
            ]));
            content.push(Line::from(""));

            if let Some(url) = &state.pr_url {
                content.push(Line::from(vec![
                    Span::styled("  ", Style::default()),
                    Span::styled(url.clone(), Style::default().fg(Theme::GREY_300)),
                ]));
                content.push(Line::from(""));
                content.push(Line::from(vec![
                    Span::styled("  Press ", Style::default().fg(Theme::GREY_500)),
                    Span::styled("↵", Style::default().fg(Theme::WHITE)),
                    Span::styled(" to open in browser", Style::default().fg(Theme::GREY_500)),
                ]));
            }

            // Use full visible height for content
            let scrollable_height = visible_height;
            let total_content = content.len();
            let scroll = state.scroll.min(total_content.saturating_sub(1));

            for line in content.into_iter().skip(scroll).take(scrollable_height) {
                lines.push(line);
            }
        }
        ShipStep::Committing => {
            lines.push(Line::from(""));
            lines.push(Line::from(vec![
                Span::styled("  ⠋ ", Style::default().fg(Theme::WHITE)),
                Span::styled(
                    "Committing changes...",
                    Style::default().fg(Theme::GREY_300),
                ),
            ]));
        }
        ShipStep::Pushing => {
            lines.push(Line::from(""));
            lines.push(Line::from(vec![
                Span::styled("  + ", Style::default().fg(Theme::GREEN)),
                Span::styled("Committed", Style::default().fg(Theme::GREY_400)),
            ]));
            lines.push(Line::from(vec![
                Span::styled("  ⠋ ", Style::default().fg(Theme::WHITE)),
                Span::styled("Pushing to remote...", Style::default().fg(Theme::GREY_300)),
            ]));
        }
        ShipStep::CreatingPR => {
            lines.push(Line::from(""));
            lines.push(Line::from(vec![
                Span::styled("  + ", Style::default().fg(Theme::GREEN)),
                Span::styled("Committed", Style::default().fg(Theme::GREY_400)),
            ]));
            lines.push(Line::from(vec![
                Span::styled("  + ", Style::default().fg(Theme::GREEN)),
                Span::styled("Pushed", Style::default().fg(Theme::GREY_400)),
            ]));
            lines.push(Line::from(vec![
                Span::styled("  ⠋ ", Style::default().fg(Theme::WHITE)),
                Span::styled(
                    "Creating pull request...",
                    Style::default().fg(Theme::GREY_300),
                ),
            ]));
        }
        ShipStep::Confirm => {
            // Build scrollable content
            let mut content: Vec<Line<'a>> = Vec::new();

            // Branch
            content.push(Line::from(vec![
                Span::styled("  Branch: ", Style::default().fg(Theme::GREY_500)),
                Span::styled(state.branch_name.clone(), Style::default().fg(Theme::WHITE)),
            ]));
            content.push(Line::from(""));

            // Files - show all files for scrolling
            content.push(Line::from(vec![Span::styled(
                format!("  {} file(s) to commit:", state.files.len()),
                Style::default().fg(Theme::GREY_400),
            )]));
            for file in state.files.iter() {
                let name = file.file_name().and_then(|n| n.to_str()).unwrap_or("?");
                content.push(Line::from(vec![Span::styled(
                    format!("    • {}", name),
                    Style::default().fg(Theme::GREY_300),
                )]));
            }
            content.push(Line::from(""));

            // Commit message - show full message for scrolling
            content.push(Line::from(vec![Span::styled(
                "  Commit message:",
                Style::default().fg(Theme::GREY_400),
            )]));
            for line in wrap_text(&state.commit_message, text_width) {
                content.push(Line::from(vec![Span::styled(
                    format!("  {}", line),
                    Style::default().fg(Theme::WHITE),
                )]));
            }

            // Use full visible height for scrollable content
            let scrollable_height = visible_height.saturating_sub(2); // Leave room for scroll indicator
            let total_content = content.len();
            let scroll = state.scroll.min(total_content.saturating_sub(1));

            for line in content.into_iter().skip(scroll).take(scrollable_height) {
                lines.push(line);
            }

            // Scroll indicator if needed
            if total_content > scrollable_height {
                while lines.len() < scrollable_height {
                    lines.push(Line::from(""));
                }
                lines.push(Line::from(vec![
                    Span::styled(
                        "  ─────────────────────────────────",
                        Style::default().fg(Theme::GREY_700),
                    ),
                    Span::styled(
                        format!(
                            " ↕ {}/{} ",
                            scroll + 1,
                            total_content.saturating_sub(scrollable_height) + 1
                        ),
                        Style::default().fg(Theme::GREY_500),
                    ),
                ]));
            }
        }
    }
}

/// Render the question input mode content in the right panel
fn render_question_mode_content<'a>(lines: &mut Vec<Line<'a>>, app: &App, visible_height: usize) {
    // Top padding
    lines.push(Line::from(""));

    // Input line with cursor
    let cursor = "█";
    let input_line = if app.question_input.is_empty() {
        Line::from(vec![
            Span::styled("  ", Style::default()),
            Span::styled(cursor, Style::default().fg(Theme::WHITE)),
            Span::styled(
                " Type your question...",
                Style::default().fg(Theme::GREY_500),
            ),
        ])
    } else {
        Line::from(vec![
            Span::styled("  ", Style::default()),
            Span::styled(
                app.question_input.clone(),
                Style::default().fg(Theme::WHITE),
            ),
            Span::styled(cursor, Style::default().fg(Theme::WHITE)),
        ])
    };
    lines.push(input_line);

    lines.push(Line::from(""));

    // Show suggested questions when input is empty
    if app.question_input.is_empty() && !app.question_suggestions.is_empty() {
        lines.push(Line::from(vec![Span::styled(
            "  Suggested questions:",
            Style::default().fg(Theme::GREY_400),
        )]));
        lines.push(Line::from(""));

        for (i, suggestion) in app.question_suggestions.iter().enumerate() {
            let is_selected = i == app.question_suggestion_selected;

            let (prefix, style) = if is_selected {
                (" › ", Style::default().fg(Theme::WHITE))
            } else {
                ("   ", Style::default().fg(Theme::GREY_400))
            };

            lines.push(Line::from(vec![
                Span::styled(prefix, style),
                Span::styled(suggestion.clone(), style),
            ]));
        }
    }

    // Fill remaining space and add hints at bottom
    let used_lines = lines.len();
    let remaining = visible_height.saturating_sub(used_lines + 2);
    for _ in 0..remaining {
        lines.push(Line::from(""));
    }

    // Action hints
    let hint = if app.question_input.is_empty() {
        Line::from(vec![
            Span::styled("  ", Style::default()),
            Span::styled(
                " ↑↓ ",
                Style::default().fg(Theme::GREY_900).bg(Theme::GREY_400),
            ),
            Span::styled(" browse ", Style::default().fg(Theme::GREY_400)),
            Span::styled("   ", Style::default()),
            Span::styled(
                " ↵ ",
                Style::default().fg(Theme::GREY_900).bg(Theme::GREY_400),
            ),
            Span::styled(" ask ", Style::default().fg(Theme::GREY_400)),
            Span::styled("   ", Style::default()),
            Span::styled(
                " Esc ",
                Style::default().fg(Theme::GREY_900).bg(Theme::GREY_400),
            ),
            Span::styled(" cancel ", Style::default().fg(Theme::GREY_400)),
        ])
    } else {
        Line::from(vec![
            Span::styled("  ", Style::default()),
            Span::styled(
                " ↵ ",
                Style::default().fg(Theme::GREY_900).bg(Theme::GREY_400),
            ),
            Span::styled(" ask ", Style::default().fg(Theme::GREY_400)),
            Span::styled("   ", Style::default()),
            Span::styled(
                " Esc ",
                Style::default().fg(Theme::GREY_900).bg(Theme::GREY_400),
            ),
            Span::styled(" cancel ", Style::default().fg(Theme::GREY_400)),
        ])
    };
    lines.push(hint);
}

/// Render the loading state for Ask Cosmos
fn render_ask_cosmos_loading<'a>(lines: &mut Vec<Line<'a>>, app: &App) {
    lines.push(Line::from(""));

    let spinner = SPINNER_FRAMES[app.loading_frame % SPINNER_FRAMES.len()];
    lines.push(Line::from(vec![
        Span::styled("    ", Style::default()),
        Span::styled(format!("{} ", spinner), Style::default().fg(Theme::WHITE)),
        Span::styled("Thinking...", Style::default().fg(Theme::GREY_300)),
    ]));

    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        Span::styled("    ", Style::default()),
        Span::styled(
            "Esc",
            Style::default().fg(Theme::GREY_900).bg(Theme::GREY_500),
        ),
        Span::styled(" cancel", Style::default().fg(Theme::GREY_500)),
    ]));
}

/// Render the Ask Cosmos response content in the right panel
fn render_ask_cosmos_content<'a>(
    lines: &mut Vec<Line<'a>>,
    ask_state: &AskCosmosState,
    app: &App,
    visible_height: usize,
    inner_width: usize,
) {
    let _ = app; // silence unused warning

    // Top padding for breathing room (matching other panels)
    lines.push(Line::from(""));

    // Parse markdown and render with styling
    let text_width = inner_width.saturating_sub(6);
    let parsed_lines = markdown::parse_markdown(&ask_state.response, text_width);

    // Add simple left padding to each line (matching verify/suggestions pattern)
    let padded_lines: Vec<Line<'static>> = parsed_lines
        .into_iter()
        .map(|line| {
            let mut spans = vec![Span::styled("  ", Style::default())];
            spans.extend(line.spans);
            Line::from(spans)
        })
        .collect();

    // Calculate available height for content
    // Account for: 1 empty top + 1 scroll indicator + 1 empty + 1 hint = 4 lines overhead
    let content_height = visible_height.saturating_sub(4);
    let total_lines = padded_lines.len();
    let scroll = ask_state.scroll.min(total_lines.saturating_sub(1));

    // Render visible content
    for line in padded_lines.iter().skip(scroll).take(content_height) {
        lines.push(line.clone());
    }

    // Scroll indicator (if content exceeds visible area)
    if total_lines > content_height {
        lines.push(Line::from(vec![Span::styled(
            format!(
                "  ↕ {}/{}",
                scroll + 1,
                total_lines.saturating_sub(content_height) + 1
            ),
            Style::default().fg(Theme::GREY_500),
        )]));
    } else {
        lines.push(Line::from(""));
    }

    lines.push(Line::from(""));

    // Action hints at bottom
    lines.push(Line::from(vec![
        Span::styled("  ", Style::default()),
        Span::styled(
            " ↑↓ ",
            Style::default().fg(Theme::GREY_900).bg(Theme::GREY_400),
        ),
        Span::styled(" scroll ", Style::default().fg(Theme::GREY_400)),
        Span::styled("   ", Style::default()),
        Span::styled(
            " Esc ",
            Style::default().fg(Theme::GREY_900).bg(Theme::GREY_400),
        ),
        Span::styled(" back ", Style::default().fg(Theme::GREY_400)),
    ]));
}

fn render_footer(frame: &mut Frame, area: Rect, app: &App) {
    // Status and action buttons
    let mut spans = vec![Span::styled("  ", Style::default())];

    // Project name and branch with icon (truncate long branch names)
    let project_name = app
        .context
        .repo_root
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("unknown");
    spans.push(Span::styled(
        project_name,
        Style::default().fg(Theme::GREY_400),
    ));
    spans.push(Span::styled(" ⎇ ", Style::default().fg(Theme::GREY_500)));
    let branch_display = if app.context.branch.len() > 20 {
        format!("{}…", &app.context.branch[..19])
    } else {
        app.context.branch.clone()
    };
    let is_on_main = app.is_on_main_branch();
    spans.push(Span::styled(
        branch_display,
        Style::default().fg(if is_on_main {
            Theme::GREY_100
        } else {
            Theme::GREEN
        }),
    ));

    if app.git_refresh_error.is_some() {
        spans.push(Span::styled("  status stale", Style::default().fg(Theme::YELLOW)));
    }

    // Cost + budget indicators
    if app.session_cost > 0.0
        || app.config.max_session_cost_usd.is_some()
        || app.config.max_tokens_per_day.is_some()
    {
        spans.push(Span::styled("  ", Style::default()));

        if let Some(max) = app.config.max_session_cost_usd {
            spans.push(Span::styled(
                format!("${:.4}/${:.4}", app.session_cost, max),
                Style::default().fg(Theme::GREY_400),
            ));
        } else if app.session_cost > 0.0 {
            spans.push(Span::styled(
                format!("${:.4}", app.session_cost),
                Style::default().fg(Theme::GREY_400),
            ));
        }

        if let Some(max_tokens) = app.config.max_tokens_per_day {
            spans.push(Span::styled("  ", Style::default()));
            spans.push(Span::styled(
                format!("tok {}/{}", app.config.tokens_used_today, max_tokens),
                Style::default().fg(Theme::GREY_500),
            ));
        }
    }

    // Spacer before buttons
    let status_len: usize = spans.iter().map(|s| s.content.chars().count()).sum();
    let available = area.width as usize;
    // Panel-specific hints + help/quit buttons
    let button_area_approx = match app.active_panel {
        ActivePanel::Project => 55, // / search  g group  ␣ expand  ? help  q quit
        ActivePanel::Suggestions => match app.workflow_step {
            WorkflowStep::Suggestions => 38, // ↵ verify  ? help  q quit
            WorkflowStep::Verify => {
                if app.verify_state.loading || app.loading == LoadingState::GeneratingFix {
                    30 // Esc cancel  ? help  q quit
                } else if app.verify_state.preview.is_some() {
                    55 // ↵ apply  d details  Esc back  ? help  q quit
                } else {
                    30 // Esc back  ? help  q quit
                }
            }
            WorkflowStep::Review => 50, // ␣ select  ↵ fix  Esc back  ? help  q quit
            WorkflowStep::Ship => match app.ship_state.step {
                ShipStep::Confirm => 45, // ↵ ship  Esc back  ? help  q quit
                ShipStep::Done => 50,    // ↵ open  Esc done  ? help  q quit
                _ => 25,                 // ? help  q quit (processing)
            },
        },
    };
    let spacer_len = available.saturating_sub(status_len + button_area_approx);
    if spacer_len > 0 {
        spans.push(Span::styled(" ".repeat(spacer_len), Style::default()));
    }

    // Panel-specific contextual hints
    match app.active_panel {
        ActivePanel::Project => {
            spans.push(Span::styled(
                " / ",
                Style::default().fg(Theme::GREY_900).bg(Theme::GREY_500),
            ));
            spans.push(Span::styled(
                " search ",
                Style::default().fg(Theme::GREY_500),
            ));
            spans.push(Span::styled(
                " g ",
                Style::default().fg(Theme::GREY_900).bg(Theme::GREY_500),
            ));
            spans.push(Span::styled(
                " group ",
                Style::default().fg(Theme::GREY_500),
            ));
            spans.push(Span::styled(
                " ↵ ",
                Style::default().fg(Theme::GREY_900).bg(Theme::GREY_500),
            ));
            spans.push(Span::styled(
                " expand ",
                Style::default().fg(Theme::GREY_500),
            ));
        }
        ActivePanel::Suggestions => match app.workflow_step {
            WorkflowStep::Suggestions => {
                spans.push(Span::styled(
                    " ↵ ",
                    Style::default().fg(Theme::GREY_900).bg(Theme::GREEN),
                ));
                spans.push(Span::styled(
                    " verify ",
                    Style::default().fg(Theme::GREY_300),
                ));
            }
            WorkflowStep::Verify => {
                if app.verify_state.loading || app.loading == LoadingState::GeneratingFix {
                    spans.push(Span::styled(
                        " Esc ",
                        Style::default().fg(Theme::GREY_900).bg(Theme::GREY_500),
                    ));
                    spans.push(Span::styled(
                        " cancel ",
                        Style::default().fg(Theme::GREY_500),
                    ));
                } else if app.verify_state.preview.is_some() {
                    spans.push(Span::styled(
                        " ↵ ",
                        Style::default().fg(Theme::GREY_900).bg(Theme::GREEN),
                    ));
                    spans.push(Span::styled(
                        " apply ",
                        Style::default().fg(Theme::GREY_300),
                    ));
                    spans.push(Span::styled(
                        " d ",
                        Style::default().fg(Theme::GREY_900).bg(Theme::GREY_600),
                    ));
                    spans.push(Span::styled(
                        if app.verify_state.show_technical_details {
                            " hide details "
                        } else {
                            " details "
                        },
                        Style::default().fg(Theme::GREY_500),
                    ));
                    spans.push(Span::styled(
                        " Esc ",
                        Style::default().fg(Theme::GREY_900).bg(Theme::GREY_600),
                    ));
                    spans.push(Span::styled(" back ", Style::default().fg(Theme::GREY_600)));
                } else {
                    spans.push(Span::styled(
                        " Esc ",
                        Style::default().fg(Theme::GREY_900).bg(Theme::GREY_500),
                    ));
                    spans.push(Span::styled(" back ", Style::default().fg(Theme::GREY_500)));
                }
            }
            WorkflowStep::Review => {
                spans.push(Span::styled(
                    " ␣ ",
                    Style::default().fg(Theme::GREY_900).bg(Theme::GREY_500),
                ));
                spans.push(Span::styled(
                    " select ",
                    Style::default().fg(Theme::GREY_500),
                ));
                spans.push(Span::styled(
                    " ↵ ",
                    Style::default().fg(Theme::GREY_900).bg(Theme::GREEN),
                ));
                spans.push(Span::styled(" fix ", Style::default().fg(Theme::GREY_300)));
                spans.push(Span::styled(
                    " Esc ",
                    Style::default().fg(Theme::GREY_900).bg(Theme::GREY_600),
                ));
                spans.push(Span::styled(" back ", Style::default().fg(Theme::GREY_600)));
            }
            WorkflowStep::Ship => match app.ship_state.step {
                ShipStep::Confirm => {
                    spans.push(Span::styled(
                        " ↵ ",
                        Style::default().fg(Theme::GREY_900).bg(Theme::GREEN),
                    ));
                    spans.push(Span::styled(" ship ", Style::default().fg(Theme::GREY_300)));
                    spans.push(Span::styled(
                        " Esc ",
                        Style::default().fg(Theme::GREY_900).bg(Theme::GREY_600),
                    ));
                    spans.push(Span::styled(" back ", Style::default().fg(Theme::GREY_600)));
                }
                ShipStep::Done => {
                    spans.push(Span::styled(
                        " ↵ ",
                        Style::default().fg(Theme::GREY_900).bg(Theme::GREEN),
                    ));
                    spans.push(Span::styled(
                        " open PR ",
                        Style::default().fg(Theme::GREY_300),
                    ));
                    spans.push(Span::styled(
                        " Esc ",
                        Style::default().fg(Theme::GREY_900).bg(Theme::GREY_600),
                    ));
                    spans.push(Span::styled(" done ", Style::default().fg(Theme::GREY_600)));
                }
                _ => {
                    // Processing states - no action buttons
                }
            },
        },
    }

    // Help and quit (always shown)
    spans.push(Span::styled(
        " ? ",
        Style::default().fg(Theme::GREY_900).bg(Theme::GREY_500),
    ));
    spans.push(Span::styled(" help ", Style::default().fg(Theme::GREY_500)));

    spans.push(Span::styled(
        " q ",
        Style::default().fg(Theme::GREY_900).bg(Theme::GREY_600),
    ));
    spans.push(Span::styled(" quit ", Style::default().fg(Theme::GREY_600)));

    spans.push(Span::styled(" ", Style::default()));

    let footer_line = Line::from(spans);

    let footer = Paragraph::new(vec![Line::from(""), footer_line])
        .style(Style::default().bg(Theme::GREY_900));
    frame.render_widget(footer, area);
}

// ═══════════════════════════════════════════════════════════════════════════
//  OVERLAYS
// ═══════════════════════════════════════════════════════════════════════════

fn render_help(frame: &mut Frame, scroll: usize) {
    let area = centered_rect(55, 80, frame.area());
    frame.render_widget(Clear, area);

    // Helper functions that return owned data
    fn section_start(title: &str) -> Vec<Line<'static>> {
        vec![
            Line::from(""),
            Line::from(vec![
                Span::styled("    ╭─ ".to_string(), Style::default().fg(Theme::GREY_600)),
                Span::styled(
                    title.to_string(),
                    Style::default()
                        .fg(Theme::WHITE)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    " ─────────────────────────╮".to_string(),
                    Style::default().fg(Theme::GREY_600),
                ),
            ]),
        ]
    }

    fn key_row(key: &str, desc: &str) -> Line<'static> {
        Line::from(vec![
            Span::styled("    │  ".to_string(), Style::default().fg(Theme::GREY_600)),
            Span::styled(
                format!(" {} ", key),
                Style::default().fg(Theme::GREY_900).bg(Theme::GREY_300),
            ),
            Span::styled(format!("  {}", desc), Style::default().fg(Theme::GREY_200)),
        ])
    }

    fn section_end() -> Line<'static> {
        Line::from(vec![Span::styled(
            "    ╰─────────────────────────────────────╯".to_string(),
            Style::default().fg(Theme::GREY_600),
        )])
    }

    fn section_spacer() -> Line<'static> {
        Line::from(vec![Span::styled(
            "    │".to_string(),
            Style::default().fg(Theme::GREY_600),
        )])
    }

    let mut help_text: Vec<Line<'static>> = vec![Line::from("")];

    // Navigation section
    help_text.extend(section_start("Navigation"));
    help_text.push(section_spacer());
    help_text.push(key_row("↑↓", "Move up/down"));
    help_text.push(key_row("PgUp/Dn", "Page scroll"));
    help_text.push(key_row("Tab", "Switch between panels"));
    help_text.push(key_row("↵", "Expand/collapse or view details"));
    help_text.push(key_row("Esc", "Go back / cancel"));
    help_text.push(section_spacer());
    help_text.push(section_end());

    // File Explorer section
    help_text.extend(section_start("File Explorer"));
    help_text.push(section_spacer());
    help_text.push(key_row("/", "Search files"));
    help_text.push(key_row("g", "Toggle grouped/flat view"));
    help_text.push(section_spacer());
    help_text.push(section_end());

    // Actions section
    help_text.extend(section_start("Actions"));
    help_text.push(section_spacer());
    help_text.push(key_row("a", "View suggestion detail"));
    help_text.push(key_row("␣", "Toggle selection (Review)"));
    help_text.push(key_row("u", "Undo last applied fix"));
    help_text.push(key_row("i", "Ask cosmos a question"));
    help_text.push(key_row("R", "Start fresh (reset)"));
    help_text.push(section_spacer());
    help_text.push(section_end());

    // General section
    help_text.extend(section_start("General"));
    help_text.push(section_spacer());
    help_text.push(key_row("?", "Toggle this help"));
    help_text.push(key_row("q", "Quit cosmos"));
    help_text.push(section_spacer());
    help_text.push(section_end());

    help_text.push(Line::from(""));
    help_text.push(Line::from(vec![
        Span::styled("    ".to_string(), Style::default()),
        Span::styled(
            " Esc ".to_string(),
            Style::default().fg(Theme::GREY_900).bg(Theme::GREY_400),
        ),
        Span::styled(
            " close help".to_string(),
            Style::default().fg(Theme::GREY_400),
        ),
    ]));
    help_text.push(Line::from(""));

    // Calculate visible height (area minus borders)
    let visible_height = area.height.saturating_sub(2) as usize;
    let total_lines = help_text.len();
    let needs_scroll = total_lines > visible_height;

    // Add scroll indicator if needed
    if needs_scroll {
        let max_scroll = total_lines.saturating_sub(visible_height);
        let effective_scroll = scroll.min(max_scroll);
        help_text.push(Line::from(vec![Span::styled(
            format!("    ↕ {}/{} ", effective_scroll + 1, max_scroll + 1),
            Style::default().fg(Theme::GREY_500),
        )]));
    }

    let block = Paragraph::new(help_text)
        .block(
            Block::default()
                .title(" 𝘩𝘦𝘭𝘱 ")
                .title_style(Style::default().fg(Theme::GREY_100))
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Theme::GREY_400))
                .style(Style::default().bg(Theme::GREY_900)),
        )
        .scroll((scroll as u16, 0));

    frame.render_widget(block, area);
}

fn render_inquiry_preview(frame: &mut Frame, question: &str, preview: &str, scroll: usize) {
    let area = centered_rect(80, 80, frame.area());
    frame.render_widget(Clear, area);

    let inner_width = area.width.saturating_sub(10) as usize;
    let visible_height = area.height.saturating_sub(12) as usize;

    let header = vec![
        Line::from(""),
        Line::from(vec![
            Span::styled("   ", Style::default()),
            Span::styled(
                " › ",
                Style::default()
                    .fg(Theme::GREY_900)
                    .bg(Theme::WHITE)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                " inquiry preview ",
                Style::default()
                    .fg(Theme::GREY_200)
                    .add_modifier(Modifier::ITALIC),
            ),
        ]),
        Line::from(""),
        Line::from(vec![
            Span::styled("   Q: ", Style::default().fg(Theme::GREY_500)),
            Span::styled(
                truncate(question, inner_width.saturating_sub(6).max(8)),
                Style::default().fg(Theme::WHITE),
            ),
        ]),
        Line::from(""),
    ];

    let parsed = markdown::parse_markdown(preview, inner_width.saturating_sub(4));
    let total = parsed.len();
    let mut body: Vec<Line<'static>> = Vec::new();
    for line in parsed.into_iter().skip(scroll).take(visible_height) {
        body.push(line);
    }

    let mut lines = header;
    lines.extend(body);

    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        Span::styled("   ", Style::default()),
        Span::styled(
            " ↵ ",
            Style::default()
                .fg(Theme::GREY_900)
                .bg(Theme::GREEN)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(" send ", Style::default().fg(Theme::GREEN)),
        Span::styled("  ", Style::default()),
        Span::styled(
            " Esc ",
            Style::default().fg(Theme::GREY_900).bg(Theme::GREY_500),
        ),
        Span::styled(" cancel ", Style::default().fg(Theme::GREY_500)),
        Span::styled("  ", Style::default()),
        Span::styled(
            " ↑↓ ",
            Style::default().fg(Theme::GREY_900).bg(Theme::GREY_400),
        ),
        Span::styled(" scroll ", Style::default().fg(Theme::GREY_400)),
    ]));
    if total > visible_height {
        lines.push(Line::from(vec![
            Span::styled("   ", Style::default()),
            Span::styled(
                format!(
                    "↕ {}/{}",
                    scroll + 1,
                    total.saturating_sub(visible_height) + 1
                ),
                Style::default().fg(Theme::GREY_600),
            ),
        ]));
    }

    let block = Paragraph::new(lines).wrap(Wrap { trim: false }).block(
        Block::default()
            .title(" › inquiry preview ")
            .title_style(Style::default().fg(Theme::GREY_100))
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Theme::GREY_400))
            .style(Style::default().bg(Theme::GREY_900)),
    );

    frame.render_widget(block, area);
}

fn render_file_detail(
    frame: &mut Frame,
    path: &PathBuf,
    file_index: &crate::index::FileIndex,
    llm_summary: Option<&String>,
    _scroll: usize,
) {
    let area = centered_rect(70, 75, frame.area());
    frame.render_widget(Clear, area);

    let inner_width = area.width.saturating_sub(12) as usize;

    // File name header
    let filename = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("unknown");

    let mut lines = vec![
        Line::from(""),
        Line::from(vec![
            Span::styled("    ", Style::default()),
            Span::styled(
                format!(" {} ", file_index.language.icon()),
                Style::default().fg(Theme::GREY_900).bg(Theme::GREY_300),
            ),
            Span::styled(
                format!("  {}", filename),
                Style::default()
                    .fg(Theme::WHITE)
                    .add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(vec![Span::styled(
            format!("       {}", path.display()),
            Style::default().fg(Theme::GREY_500),
        )]),
        Line::from(""),
    ];

    // Summary card
    lines.push(Line::from(vec![
        Span::styled("    ╭─ ", Style::default().fg(Theme::GREY_600)),
        Span::styled("Summary", Style::default().fg(Theme::GREY_300)),
        Span::styled(
            " ─".to_string() + &"─".repeat(inner_width.saturating_sub(15)) + "╮",
            Style::default().fg(Theme::GREY_600),
        ),
    ]));
    lines.push(Line::from(vec![Span::styled(
        "    │",
        Style::default().fg(Theme::GREY_600),
    )]));

    if let Some(summary) = llm_summary {
        let wrapped = wrap_text(summary, inner_width.saturating_sub(6));
        for line in &wrapped {
            lines.push(Line::from(vec![
                Span::styled("    │  ", Style::default().fg(Theme::GREY_600)),
                Span::styled(line.to_string(), Style::default().fg(Theme::GREY_50)),
            ]));
        }
    } else {
        let wrapped = wrap_text(&file_index.summary.purpose, inner_width.saturating_sub(6));
        for line in &wrapped {
            lines.push(Line::from(vec![
                Span::styled("    │  ", Style::default().fg(Theme::GREY_600)),
                Span::styled(line.to_string(), Style::default().fg(Theme::GREY_100)),
            ]));
        }
    }

    lines.push(Line::from(vec![Span::styled(
        "    │",
        Style::default().fg(Theme::GREY_600),
    )]));
    lines.push(Line::from(vec![Span::styled(
        "    ╰".to_string() + &"─".repeat(inner_width.saturating_sub(4)) + "╯",
        Style::default().fg(Theme::GREY_600),
    )]));
    lines.push(Line::from(""));

    // Metrics bar
    let func_count = file_index
        .symbols
        .iter()
        .filter(|s| {
            matches!(
                s.kind,
                crate::index::SymbolKind::Function | crate::index::SymbolKind::Method
            )
        })
        .count();
    let struct_count = file_index
        .symbols
        .iter()
        .filter(|s| {
            matches!(
                s.kind,
                crate::index::SymbolKind::Struct | crate::index::SymbolKind::Class
            )
        })
        .count();

    lines.push(Line::from(vec![
        Span::styled("    ", Style::default()),
        Span::styled(
            format!(" {} ", file_index.loc),
            Style::default().fg(Theme::GREY_900).bg(Theme::GREY_500),
        ),
        Span::styled(" LOC  ", Style::default().fg(Theme::GREY_400)),
        Span::styled(
            format!(" {} ", func_count),
            Style::default().fg(Theme::GREY_900).bg(Theme::GREY_500),
        ),
        Span::styled(" funcs  ", Style::default().fg(Theme::GREY_400)),
        Span::styled(
            format!(" {} ", struct_count),
            Style::default().fg(Theme::GREY_900).bg(Theme::GREY_500),
        ),
        Span::styled(" structs", Style::default().fg(Theme::GREY_400)),
    ]));
    lines.push(Line::from(""));

    // Dependencies section
    if !file_index.summary.exports.is_empty()
        || !file_index.summary.used_by.is_empty()
        || !file_index.summary.depends_on.is_empty()
    {
        lines.push(Line::from(vec![
            Span::styled("    ╭─ ", Style::default().fg(Theme::GREY_600)),
            Span::styled("Dependencies", Style::default().fg(Theme::GREY_300)),
            Span::styled(
                " ─".to_string() + &"─".repeat(inner_width.saturating_sub(19)) + "╮",
                Style::default().fg(Theme::GREY_600),
            ),
        ]));

        // Exports
        if !file_index.summary.exports.is_empty() {
            let exports_str = file_index.summary.exports.join(", ");
            let label = "↗ Exports: ";
            let label_width = label.chars().count();
            let content_width = inner_width.saturating_sub(6 + label_width);
            let wrapped = wrap_text(&exports_str, content_width);

            for (i, line) in wrapped.iter().enumerate() {
                if i == 0 {
                    lines.push(Line::from(vec![
                        Span::styled("    │  ", Style::default().fg(Theme::GREY_600)),
                        Span::styled(label, Style::default().fg(Theme::GREY_400)),
                        Span::styled(line.to_string(), Style::default().fg(Theme::GREY_200)),
                    ]));
                } else {
                    lines.push(Line::from(vec![
                        Span::styled("    │  ", Style::default().fg(Theme::GREY_600)),
                        Span::styled(" ".repeat(label_width), Style::default()),
                        Span::styled(line.to_string(), Style::default().fg(Theme::GREY_200)),
                    ]));
                }
            }
        }

        // Used by
        if !file_index.summary.used_by.is_empty() {
            let used_by_str: Vec<_> = file_index
                .summary
                .used_by
                .iter()
                .filter_map(|p| {
                    p.file_name()
                        .and_then(|n| n.to_str())
                        .map(|s| s.to_string())
                })
                .collect();
            let used_by_full = used_by_str.join(", ");
            let label = "← Used by: ";
            let label_width = label.chars().count();
            let content_width = inner_width.saturating_sub(6 + label_width);
            let wrapped = wrap_text(&used_by_full, content_width);

            for (i, line) in wrapped.iter().enumerate() {
                if i == 0 {
                    lines.push(Line::from(vec![
                        Span::styled("    │  ", Style::default().fg(Theme::GREY_600)),
                        Span::styled(label, Style::default().fg(Theme::GREY_400)),
                        Span::styled(line.to_string(), Style::default().fg(Theme::GREY_200)),
                    ]));
                } else {
                    lines.push(Line::from(vec![
                        Span::styled("    │  ", Style::default().fg(Theme::GREY_600)),
                        Span::styled(" ".repeat(label_width), Style::default()),
                        Span::styled(line.to_string(), Style::default().fg(Theme::GREY_200)),
                    ]));
                }
            }
        }

        // Depends on
        if !file_index.summary.depends_on.is_empty() {
            let deps_str: Vec<_> = file_index
                .summary
                .depends_on
                .iter()
                .filter_map(|p| {
                    p.file_name()
                        .and_then(|n| n.to_str())
                        .map(|s| s.to_string())
                })
                .collect();
            let deps_full = deps_str.join(", ");
            let label = "→ Depends: ";
            let label_width = label.chars().count();
            let content_width = inner_width.saturating_sub(6 + label_width);
            let wrapped = wrap_text(&deps_full, content_width);

            for (i, line) in wrapped.iter().enumerate() {
                if i == 0 {
                    lines.push(Line::from(vec![
                        Span::styled("    │  ", Style::default().fg(Theme::GREY_600)),
                        Span::styled(label, Style::default().fg(Theme::GREY_400)),
                        Span::styled(line.to_string(), Style::default().fg(Theme::GREY_200)),
                    ]));
                } else {
                    lines.push(Line::from(vec![
                        Span::styled("    │  ", Style::default().fg(Theme::GREY_600)),
                        Span::styled(" ".repeat(label_width), Style::default()),
                        Span::styled(line.to_string(), Style::default().fg(Theme::GREY_200)),
                    ]));
                }
            }
        }

        lines.push(Line::from(vec![Span::styled(
            "    ╰".to_string() + &"─".repeat(inner_width.saturating_sub(4)) + "╯",
            Style::default().fg(Theme::GREY_600),
        )]));
    }

    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        Span::styled("    ", Style::default()),
        Span::styled(
            " Esc ",
            Style::default().fg(Theme::GREY_900).bg(Theme::GREY_400),
        ),
        Span::styled(" close", Style::default().fg(Theme::GREY_400)),
    ]));
    lines.push(Line::from(""));

    let block = Paragraph::new(lines).wrap(Wrap { trim: false }).block(
        Block::default()
            .title(" › 𝘧𝘪𝘭𝘦 𝘥𝘦𝘵𝘢𝘪𝘭 ")
            .title_style(Style::default().fg(Theme::GREY_100))
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Theme::GREY_400))
            .style(Style::default().bg(Theme::GREY_900)),
    );

    frame.render_widget(block, area);
}

fn render_reset_overlay(
    frame: &mut Frame,
    options: &[(crate::cache::ResetOption, bool)],
    selected: usize,
) {
    let area = centered_rect(55, 50, frame.area());
    frame.render_widget(Clear, area);

    let mut lines: Vec<Line> = Vec::new();

    // Header
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "  Select what to reset and regenerate:",
        Style::default().fg(Theme::GREY_300),
    )));
    lines.push(Line::from(""));

    // Options list
    for (i, (option, is_selected)) in options.iter().enumerate() {
        let is_focused = i == selected;

        // Checkbox
        let checkbox = if *is_selected { "[x]" } else { "[ ]" };
        let checkbox_color = if *is_selected {
            Theme::GREEN
        } else {
            Theme::GREY_500
        };

        // Selection indicator
        let indicator = if is_focused { "▸ " } else { "  " };

        // Format: "▸ [x] Label                (description)"
        let label = option.label();
        let desc = option.description();

        // Calculate padding for alignment
        let label_width = 22;
        let padded_label = format!("{:<width$}", label, width = label_width);

        let line_style = if is_focused {
            Style::default().bg(Theme::GREY_700)
        } else {
            Style::default()
        };

        lines.push(
            Line::from(vec![
                Span::styled(
                    format!("  {}", indicator),
                    Style::default().fg(Theme::ACCENT),
                ),
                Span::styled(
                    format!("{} ", checkbox),
                    Style::default().fg(checkbox_color),
                ),
                Span::styled(padded_label, Style::default().fg(Theme::GREY_100)),
                Span::styled(format!("({})", desc), Style::default().fg(Theme::GREY_500)),
            ])
            .style(line_style),
        );
    }

    // Separator and help
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "  ─────────────────────────────────────────────────",
        Style::default().fg(Theme::GREY_600),
    )));
    lines.push(Line::from(vec![
        Span::styled("   ", Style::default()),
        Span::styled(
            " Space ",
            Style::default().fg(Theme::GREY_900).bg(Theme::GREY_400),
        ),
        Span::styled(" toggle  ", Style::default().fg(Theme::GREY_400)),
        Span::styled(
            " ↵ ",
            Style::default().fg(Theme::GREY_900).bg(Theme::GREY_400),
        ),
        Span::styled(" reset  ", Style::default().fg(Theme::GREY_400)),
        Span::styled(
            " Esc ",
            Style::default().fg(Theme::GREY_900).bg(Theme::GREY_400),
        ),
        Span::styled(" cancel", Style::default().fg(Theme::GREY_400)),
    ]));
    lines.push(Line::from(""));

    let block = Block::default()
        .title(" Reset Cosmos ")
        .title_style(Style::default().fg(Theme::GREY_100))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Theme::ACCENT))
        .style(Style::default().bg(Theme::GREY_800));

    let paragraph = Paragraph::new(lines)
        .block(block)
        .wrap(Wrap { trim: false });

    frame.render_widget(paragraph, area);
}

fn render_startup_check(frame: &mut Frame, changed_count: usize, confirming_discard: bool) {
    let area = centered_rect(55, 45, frame.area());
    frame.render_widget(Clear, area);

    let mut lines: Vec<Line> = Vec::new();

    if confirming_discard {
        // Confirmation dialog for discard
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "  Are you sure?",
            Style::default()
                .fg(Theme::WHITE)
                .add_modifier(Modifier::BOLD),
        )));
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "  This will permanently remove your uncommitted changes.",
            Style::default().fg(Theme::GREY_300),
        )));
        lines.push(Line::from(""));
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "  ─────────────────────────────────────────────────",
            Style::default().fg(Theme::GREY_600),
        )));
        lines.push(Line::from(vec![
            Span::styled("   ", Style::default()),
            Span::styled(" y ", Style::default().fg(Theme::GREY_900).bg(Theme::RED)),
            Span::styled(" yes, discard  ", Style::default().fg(Theme::GREY_400)),
            Span::styled(
                " n ",
                Style::default().fg(Theme::GREY_900).bg(Theme::GREY_400),
            ),
            Span::styled(" cancel", Style::default().fg(Theme::GREY_400)),
        ]));
        lines.push(Line::from(""));
    } else {
        // Main startup check dialog
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "  You have unsaved work",
            Style::default()
                .fg(Theme::WHITE)
                .add_modifier(Modifier::BOLD),
        )));
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "  Cosmos works best from a fresh starting point.",
            Style::default().fg(Theme::GREY_300),
        )));
        lines.push(Line::from(Span::styled(
            format!(
                "  You have {} file{} with changes.",
                changed_count,
                if changed_count == 1 { "" } else { "s" }
            ),
            Style::default().fg(Theme::GREY_300),
        )));
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "  ─────────────────────────────────────────────────",
            Style::default().fg(Theme::GREY_600),
        )));
        lines.push(Line::from(""));

        // Option: Save
        lines.push(Line::from(vec![
            Span::styled("   ", Style::default()),
            Span::styled(" s ", Style::default().fg(Theme::GREY_900).bg(Theme::GREEN)),
            Span::styled(
                "  Save my work and start fresh",
                Style::default().fg(Theme::GREY_100),
            ),
        ]));
        lines.push(Line::from(Span::styled(
            "      Your changes are safely stored.",
            Style::default().fg(Theme::GREY_500),
        )));
        lines.push(Line::from(""));

        // Option: Discard
        lines.push(Line::from(vec![
            Span::styled("   ", Style::default()),
            Span::styled(
                " d ",
                Style::default().fg(Theme::GREY_900).bg(Theme::GREY_500),
            ),
            Span::styled(
                "  Discard and start fresh",
                Style::default().fg(Theme::GREY_100),
            ),
        ]));
        lines.push(Line::from(Span::styled(
            "      Remove all changes and start clean.",
            Style::default().fg(Theme::GREY_500),
        )));
        lines.push(Line::from(""));

        // Option: Continue
        lines.push(Line::from(vec![
            Span::styled("   ", Style::default()),
            Span::styled(
                " c ",
                Style::default().fg(Theme::GREY_900).bg(Theme::GREY_500),
            ),
            Span::styled("  Continue as-is", Style::default().fg(Theme::GREY_100)),
        ]));
        lines.push(Line::from(""));

        // Footer
        lines.push(Line::from(Span::styled(
            "  ─────────────────────────────────────────────────",
            Style::default().fg(Theme::GREY_600),
        )));
        lines.push(Line::from(vec![
            Span::styled("   ", Style::default()),
            Span::styled(
                " s ",
                Style::default().fg(Theme::GREY_900).bg(Theme::GREY_400),
            ),
            Span::styled(" save  ", Style::default().fg(Theme::GREY_400)),
            Span::styled(
                " d ",
                Style::default().fg(Theme::GREY_900).bg(Theme::GREY_400),
            ),
            Span::styled(" discard  ", Style::default().fg(Theme::GREY_400)),
            Span::styled(
                " c ",
                Style::default().fg(Theme::GREY_900).bg(Theme::GREY_400),
            ),
            Span::styled(" continue  ", Style::default().fg(Theme::GREY_400)),
            Span::styled(
                " Esc ",
                Style::default().fg(Theme::GREY_900).bg(Theme::GREY_400),
            ),
            Span::styled(" quit", Style::default().fg(Theme::GREY_400)),
        ]));
        lines.push(Line::from(""));
    }

    let title = if confirming_discard {
        " Confirm "
    } else {
        " Startup Check "
    };
    let block = Block::default()
        .title(title)
        .title_style(Style::default().fg(Theme::GREY_100))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Theme::ACCENT))
        .style(Style::default().bg(Theme::GREY_800));

    let paragraph = Paragraph::new(lines)
        .block(block)
        .wrap(Wrap { trim: false });

    frame.render_widget(paragraph, area);
}

fn render_toast(frame: &mut Frame, toast: &Toast) {
    let area = frame.area();

    // Use the ToastKind for consistent styling
    let (prefix, message, bg, text_style) = match toast.kind {
        ToastKind::Success => (
            "  + ",
            toast.message.trim_start_matches('+').trim_start(),
            Theme::GREEN,
            Style::default()
                .fg(Theme::WHITE)
                .add_modifier(Modifier::BOLD),
        ),
        ToastKind::Error => (
            "  x ",
            toast.message.as_str(),
            Theme::RED,
            Style::default().fg(Theme::WHITE),
        ),
        ToastKind::RateLimit => {
            // Rate limit toast with countdown - countdown shown in suffix
            (
                "  ~ ",
                toast.message.as_str(),
                Theme::YELLOW,
                Style::default().fg(Theme::GREY_900),
            )
        }
        ToastKind::Info => (
            "  › ",
            toast.message.as_str(),
            Theme::GREY_700,
            Style::default()
                .fg(Theme::GREY_100)
                .add_modifier(Modifier::ITALIC),
        ),
    };

    // For rate limits, add countdown hint
    let suffix = if toast.kind == ToastKind::RateLimit {
        let remaining = toast
            .kind
            .duration_secs()
            .saturating_sub(toast.created_at.elapsed().as_secs());
        format!(" ({}s) ", remaining)
    } else {
        String::from("  ")
    };

    let width = (prefix.len() + message.len() + suffix.len()) as u16;
    let height = 1u16;
    let toast_area = Rect {
        x: (area.width.saturating_sub(width)) / 2,
        y: area.height.saturating_sub(5),
        width: width.min(area.width),
        height,
    };

    frame.render_widget(Clear, toast_area);

    let content = Paragraph::new(Line::from(vec![
        Span::styled(prefix, Style::default().fg(Theme::WHITE)),
        Span::styled(message, text_style),
        Span::styled(&suffix, Style::default().fg(Theme::GREY_900)),
    ]))
    .style(Style::default().bg(bg));
    frame.render_widget(content, toast_area);
}

// ═══════════════════════════════════════════════════════════════════════════
//  UTILITIES
// ═══════════════════════════════════════════════════════════════════════════

fn centered_rect(percent_x: u16, percent_y: u16, r: Rect) -> Rect {
    let popup_layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(r);

    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(popup_layout[1])[1]
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        format!("{}...", &s[..max - 3])
    }
}

/// Wrap text to fit within a given width
fn wrap_text(text: &str, width: usize) -> Vec<String> {
    wrap_text_variable_width(text, width, width)
}

/// Wrap text with different widths for first line vs continuation lines
/// This is useful when the first line has a prefix (like "Fix: ") that takes up space
fn wrap_text_variable_width(
    text: &str,
    first_line_width: usize,
    continuation_width: usize,
) -> Vec<String> {
    if first_line_width == 0 || continuation_width == 0 {
        return vec![text.to_string()];
    }

    let mut lines = Vec::new();
    let mut current_line = String::new();

    for word in text.split_whitespace() {
        // Use first_line_width for the first line, continuation_width for others
        let current_width = if lines.is_empty() {
            first_line_width
        } else {
            continuation_width
        };

        if current_line.is_empty() {
            if word.len() > current_width {
                // Word is longer than width, force break it
                let mut remaining = word;
                while remaining.len() > current_width {
                    lines.push(remaining[..current_width].to_string());
                    remaining = &remaining[current_width..];
                }
                current_line = remaining.to_string();
            } else {
                current_line = word.to_string();
            }
        } else if current_line.len() + 1 + word.len() <= current_width {
            current_line.push(' ');
            current_line.push_str(word);
        } else {
            lines.push(current_line);
            // After pushing, we're now on a continuation line
            let next_width = continuation_width;
            if word.len() > next_width {
                let mut remaining = word;
                while remaining.len() > next_width {
                    lines.push(remaining[..next_width].to_string());
                    remaining = &remaining[next_width..];
                }
                current_line = remaining.to_string();
            } else {
                current_line = word.to_string();
            }
        }
    }

    if !current_line.is_empty() {
        lines.push(current_line);
    }

    if lines.is_empty() {
        lines.push(String::new());
    }

    lines
}
