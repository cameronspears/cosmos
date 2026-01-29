//! Cosmos UI module.
//!
//! Renders a dual-panel terminal interface with header, main content, and footer.
//! See `render/mod.rs` for the layout implementation.

pub mod helpers;
pub mod markdown;
pub mod theme;
pub mod types;

mod render;
mod tree;

pub use render::render;

// Re-export all types for backward compatibility
pub use types::{
    ActivePanel, AskCosmosState, FileChange, InputMode, LoadingState, Overlay, PendingChange,
    ReviewState, ShipState, ShipStep, Toast, ToastKind, VerifyState, ViewMode, WorkflowStep,
    SPINNER_FRAMES,
};

use crate::context::WorkContext;
use crate::index::{CodebaseIndex, FlatTreeEntry};
use crate::suggest::{Suggestion, SuggestionEngine};
use helpers::lowercase_first;
use std::path::{Path, PathBuf};
use std::time::Instant;
use tree::{build_file_tree, build_grouped_tree};

// ═══════════════════════════════════════════════════════════════════════════
//  APP STATE
// ═══════════════════════════════════════════════════════════════════════════

/// Main application state for Cosmos
pub struct App {
    // Core data
    pub index: CodebaseIndex,
    pub suggestions: SuggestionEngine,
    pub context: WorkContext,

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

    // Question answer cache
    pub question_cache: crate::cache::QuestionCache,

    // Cost tracking
    pub session_cost: f64,            // Total USD spent this session
    pub session_tokens: u32,          // Total tokens used this session
    pub active_model: Option<String>, // Current/last model used
    pub wallet_balance: Option<f64>,  // Remaining credits in OpenRouter account

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
    /// Diagnostics from the most recent suggestion run
    pub last_suggestion_diagnostics: Option<crate::suggest::llm::SuggestionDiagnostics>,
    /// Last suggestion error message (full, untruncated)
    pub last_suggestion_error: Option<String>,

    // Flag: generate suggestions once summaries complete (used at init and after reset)
    pub pending_suggestions_on_init: bool,

    // Self-update state
    /// Available update version (None if up to date or not yet checked)
    pub update_available: Option<String>,
    /// Update download progress (0-100), None if not downloading
    pub update_progress: Option<u8>,
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
            question_cache: crate::cache::QuestionCache::default(),
            session_cost: 0.0,
            session_tokens: 0,
            active_model: None,
            wallet_balance: None,
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
            last_suggestion_diagnostics: None,
            last_suggestion_error: None,
            update_available: None,
            update_progress: None,
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
        self.pending_changes.clear();
        self.cosmos_branch = None;
    }

    /// Undo the most recent applied change by restoring files from git.
    /// Supports multi-file changes - restores all files atomically.
    /// Removes it from the pending queue.
    /// If this was the last pending change, returns to main branch and suggestions step.
    pub fn undo_last_pending_change(&mut self) -> Result<(), String> {
        let change = self
            .pending_changes
            .pop()
            .ok_or_else(|| "No pending changes to undo".to_string())?;

        // Collect paths to restore (to avoid borrow issues)
        let files_to_restore: Vec<_> = change.files.iter().map(|f| f.path.clone()).collect();

        // Restore all files from git HEAD
        for path in &files_to_restore {
            if let Err(e) = crate::git_ops::restore_file(&self.repo_path, path) {
                // Put the change back since we couldn't fully undo
                self.pending_changes.push(change);
                return Err(format!("Failed to restore {}: {}", path.display(), e));
            }
        }

        // Mark suggestion as not applied (so it can be re-applied if desired).
        self.suggestions.unmark_applied(change.suggestion_id);

        // If no more pending changes, reset to main branch and suggestions step
        if self.pending_changes.is_empty() {
            // Switch back to main branch
            if let Ok(main_name) = crate::git_ops::get_main_branch_name(&self.repo_path) {
                let _ = crate::git_ops::checkout_branch(&self.repo_path, &main_name);
            }

            // Clear cosmos branch tracking
            self.cosmos_branch = None;

            // Return to suggestions workflow step
            self.workflow_step = WorkflowStep::Suggestions;
            self.verify_state = VerifyState::default();
            self.review_state = ReviewState::default();
            self.ship_state = ShipState::default();
        }

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
    pub fn get_llm_summary(&self, path: &Path) -> Option<&String> {
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
                                // Include all features initially; filter_empty_features()
                                // removes features without matching child files
                                GroupedEntryKind::Feature => true,
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
        self.ask_cosmos_state = Some(AskCosmosState {
            response,
            scroll: 0,
        });
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
        // Display error and success toasts; info toasts are silently ignored
        if toast.is_error() || matches!(toast.kind, ToastKind::Success) {
            self.toast = Some(toast);
        }
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
    pub fn show_startup_check(
        &mut self,
        changed_count: usize,
        current_branch: String,
        main_branch: String,
    ) {
        self.overlay = Overlay::StartupCheck {
            changed_count,
            current_branch,
            main_branch,
            scroll: 0,
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

    // ═══════════════════════════════════════════════════════════════════════════
    //  UPDATE OVERLAY
    // ═══════════════════════════════════════════════════════════════════════════

    /// Show the update available overlay
    pub fn show_update_overlay(&mut self, current_version: String, target_version: String) {
        self.overlay = Overlay::Update {
            current_version,
            target_version,
            progress: None,
            error: None,
        };
    }

    /// Set update download progress
    pub fn set_update_progress(&mut self, percent: u8) {
        if let Overlay::Update { progress, .. } = &mut self.overlay {
            *progress = Some(percent);
        }
    }

    /// Set update error
    pub fn set_update_error(&mut self, message: String) {
        if let Overlay::Update {
            error, progress, ..
        } = &mut self.overlay
        {
            *error = Some(message);
            *progress = None; // Clear progress to ensure clean error state
        }
    }

    /// Scroll overlay down
    pub fn overlay_scroll_down(&mut self) {
        match &mut self.overlay {
            Overlay::Help { scroll }
            | Overlay::FileDetail { scroll, .. }
            | Overlay::StartupCheck { scroll, .. } => {
                *scroll += 1;
            }
            _ => {}
        }
    }

    /// Scroll overlay up
    pub fn overlay_scroll_up(&mut self) {
        match &mut self.overlay {
            Overlay::Help { scroll }
            | Overlay::FileDetail { scroll, .. }
            | Overlay::StartupCheck { scroll, .. } => {
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
            preview_hashes: std::collections::HashMap::new(),
        };
        self.workflow_step = WorkflowStep::Verify;
        self.loading = LoadingState::GeneratingPreview;
    }

    /// Set the preview result in the Verify step
    pub fn set_verify_preview(
        &mut self,
        preview: crate::suggest::llm::FixPreview,
        file_hashes: std::collections::HashMap<PathBuf, String>,
    ) {
        self.verify_state.preview = Some(preview);
        self.verify_state.loading = false;
        self.verify_state.preview_hashes = file_hashes;
        self.loading = LoadingState::None;
    }

    /// Use cached verification result (transitions to Verify step without regenerating preview)
    pub fn use_cached_verify(&mut self) {
        self.verify_state.loading = false;
        self.verify_state.scroll = 0;
        self.workflow_step = WorkflowStep::Verify;
        self.loading = LoadingState::None;
    }

    /// Check if we have a valid cached preview for the given suggestion and files.
    /// Returns true if cache is valid and can be reused.
    pub fn has_valid_cached_preview(
        &self,
        suggestion_id: uuid::Uuid,
        file_path: &std::path::Path,
        additional_files: &[PathBuf],
        repo_path: &std::path::Path,
    ) -> bool {
        // Must match the same suggestion
        if self.verify_state.suggestion_id != Some(suggestion_id) {
            return false;
        }

        // Must have an existing preview
        if self.verify_state.preview.is_none() {
            return false;
        }

        // Must have cached hashes to compare
        if self.verify_state.preview_hashes.is_empty() {
            return false;
        }

        // Collect all files that need hash validation
        let mut all_files = vec![file_path.to_path_buf()];
        all_files.extend(additional_files.iter().cloned());

        // Check that all file hashes match
        for target in &all_files {
            let resolved = match crate::util::resolve_repo_path_allow_new(repo_path, target) {
                Ok(r) => r,
                Err(_) => return false,
            };

            let bytes = match std::fs::read(&resolved.absolute) {
                Ok(content) => content,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
                Err(_) => return false,
            };

            let current_hash = crate::util::hash_bytes(&bytes);

            match self.verify_state.preview_hashes.get(&resolved.relative) {
                Some(cached_hash) if cached_hash == &current_hash => continue,
                _ => return false,
            }
        }

        true
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
