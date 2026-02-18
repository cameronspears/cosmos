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
    ReviewFileContent, ReviewState, ShipState, ShipStep, StartupAction, StartupMode, VerifyState,
    ViewMode, WorkflowStep, SPINNER_FRAMES,
};

use cosmos_adapters::config::SuggestionsProfile;
use cosmos_core::context::WorkContext;
use cosmos_core::index::{CodebaseIndex, FlatTreeEntry};
use cosmos_core::suggest::{Suggestion, SuggestionEngine};
use helpers::lowercase_first;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::Instant;
use tree::{build_file_tree, build_grouped_tree};

pub fn openrouter_keys_shortcut_display() -> &'static str {
    if cfg!(target_os = "macos") {
        "control + k"
    } else {
        "Ctrl + k"
    }
}

pub fn openrouter_credits_shortcut_display() -> &'static str {
    if cfg!(target_os = "macos") {
        "control + b"
    } else {
        "Ctrl + b"
    }
}

pub fn openrouter_keys_shortcut_chip() -> &'static str {
    if cfg!(target_os = "macos") {
        " control+k "
    } else {
        " Ctrl+k "
    }
}

pub fn openrouter_credits_shortcut_chip() -> &'static str {
    if cfg!(target_os = "macos") {
        " control+b "
    } else {
        " Ctrl+b "
    }
}

pub(crate) const ASK_STARTER_QUESTIONS: [&str; 3] = [
    "What does this repo help users do today?",
    "Where are the biggest reliability risks for users right now?",
    "What are the top 3 improvements with the biggest user impact?",
];
const MAX_UI_SUGGESTIONS_DISPLAY_CAP: usize = 30;

// ═══════════════════════════════════════════════════════════════════════════
//  APP STATE
// ═══════════════════════════════════════════════════════════════════════════

#[derive(Debug, Clone)]
struct FlatSearchEntry {
    name_lower: String,
    path_lower: String,
}

#[derive(Debug, Clone)]
struct GroupedSearchEntry {
    name_lower: String,
    path_lower: Option<String>,
}

#[derive(Debug, Clone)]
struct GroupingSearchFile {
    layer: cosmos_core::grouping::Layer,
    name_lower: String,
    path_lower: String,
}

/// Main application state for Cosmos
pub struct App {
    // Core data
    pub index: CodebaseIndex,
    pub suggestions: SuggestionEngine,
    pub context: WorkContext,
    pub suggestions_profile: SuggestionsProfile,
    pub suggestions_display_cap: usize,

    // UI state
    pub active_panel: ActivePanel,
    pub project_scroll: usize,
    pub project_selected: usize,
    pub suggestion_scroll: usize,
    pub suggestion_selected: usize,
    pub overlay: Overlay,
    pub should_quit: bool,

    // Search and sort state
    pub input_mode: InputMode,
    pub search_query: String,
    pub view_mode: ViewMode,

    // Question input (ask cosmos)
    pub question_input: String,
    pub question_suggestion_selected: usize,
    pub ask_in_flight: bool,
    pub active_ask_request_id: Option<u64>,
    next_ask_request_id: u64,

    // Loading state for background tasks
    pub loading: LoadingState,
    pub loading_frame: usize,

    // Personal repo memory (local)
    pub repo_memory: cosmos_adapters::cache::RepoMemory,

    // Domain glossary (auto-extracted terminology)
    pub glossary: cosmos_adapters::cache::DomainGlossary,

    // Question answer cache
    pub question_cache: cosmos_adapters::cache::QuestionCache,

    // Cost tracking
    pub session_cost: f64,            // Total USD spent this session
    pub session_tokens: u32,          // Total tokens used this session
    pub active_model: Option<String>, // Current/last model used
    pub wallet_balance: Option<f64>,  // Remaining credits in OpenRouter account

    // Cached data for display
    pub file_tree: Vec<FlatTreeEntry>,
    pub filtered_tree_indices: Vec<usize>,
    flat_search_entries: Vec<FlatSearchEntry>,
    pub repo_path: PathBuf,

    // Grouped view data
    pub grouping: cosmos_core::grouping::CodebaseGrouping,
    pub grouped_tree: Vec<cosmos_core::grouping::GroupedTreeEntry>,
    pub filtered_grouped_indices: Vec<usize>,
    grouped_search_entries: Vec<GroupedSearchEntry>,
    grouping_search_files: Vec<GroupingSearchFile>,

    // Pending changes for batch commit workflow
    pub pending_changes: Vec<PendingChange>,
    pub cosmos_branch: Option<String>,
    /// Branch user was on before Cosmos created a working fix branch.
    pub cosmos_base_branch: Option<String>,

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
    /// Whether background refinement is still validating/regenerating suggestions.
    pub suggestion_refinement_in_progress: bool,
    /// Number of provisional suggestions from fast pass.
    pub suggestion_provisional_count: usize,
    /// Number of validated suggestions after refinement.
    pub suggestion_validated_count: usize,
    /// Number of rejected suggestions from refinement.
    pub suggestion_rejected_count: usize,
    /// Run identifier for the latest suggestion generation cycle.
    pub current_suggestion_run_id: Option<String>,
    /// Rolling precision from recent verify outcomes.
    pub rolling_verify_precision: Option<f64>,
    /// Armed suggestion id for two-step apply confirmation.
    pub armed_suggestion_id: Option<uuid::Uuid>,
    /// File hash snapshot captured when apply confirmation was armed.
    pub armed_file_hashes: HashMap<PathBuf, String>,

    // Self-update state
    /// Available update version (None if up to date or not yet checked)
    pub update_available: Option<String>,
    /// Update download progress (0-100), None if not downloading
    pub update_progress: Option<u8>,
    /// Soft budget warning already shown ($0.04)
    pub budget_warned_soft: bool,
    /// Hard budget warning already shown ($0.05)
    pub budget_warned_hard: bool,
    /// Runtime redraw hint for dirty-frame rendering.
    pub needs_redraw: bool,
}

impl App {
    /// Create a new Cosmos app
    pub fn new(index: CodebaseIndex, suggestions: SuggestionEngine, context: WorkContext) -> Self {
        Self::new_with_suggestion_preferences(
            index,
            suggestions,
            context,
            SuggestionsProfile::BalancedHighVolume,
            MAX_UI_SUGGESTIONS_DISPLAY_CAP,
        )
    }

    pub fn new_with_suggestion_preferences(
        index: CodebaseIndex,
        suggestions: SuggestionEngine,
        context: WorkContext,
        suggestions_profile: SuggestionsProfile,
        suggestions_display_cap: usize,
    ) -> Self {
        let file_tree = build_file_tree(&index);
        let flat_search_entries = build_flat_search_entries(&file_tree);
        let filtered_tree_indices = (0..file_tree.len()).collect();
        let repo_path = index.root.clone();

        // Generate grouping for the codebase
        let grouping = index.generate_grouping();
        let grouped_tree = build_grouped_tree(&grouping, &index);
        let grouped_search_entries = build_grouped_search_entries(&grouped_tree);
        let filtered_grouped_indices = (0..grouped_tree.len()).collect();
        let grouping_search_files = build_grouping_search_files(&grouping);

        Self {
            index,
            suggestions,
            context,
            suggestions_profile,
            suggestions_display_cap: suggestions_display_cap
                .clamp(1, MAX_UI_SUGGESTIONS_DISPLAY_CAP),
            active_panel: ActivePanel::Suggestions,
            project_scroll: 0,
            project_selected: 0,
            suggestion_scroll: 0,
            suggestion_selected: 0,
            overlay: Overlay::None,
            should_quit: false,
            input_mode: InputMode::Normal,
            search_query: String::new(),
            view_mode: ViewMode::Grouped, // Default to grouped view
            question_input: String::new(),
            question_suggestion_selected: 0,
            ask_in_flight: false,
            active_ask_request_id: None,
            next_ask_request_id: 1,
            loading: LoadingState::None,
            loading_frame: 0,
            repo_memory: cosmos_adapters::cache::RepoMemory::default(),
            glossary: cosmos_adapters::cache::DomainGlossary::default(),
            question_cache: cosmos_adapters::cache::QuestionCache::default(),
            session_cost: 0.0,
            session_tokens: 0,
            active_model: None,
            wallet_balance: None,
            file_tree,
            filtered_tree_indices,
            flat_search_entries,
            repo_path,
            grouping,
            grouped_tree,
            filtered_grouped_indices,
            grouped_search_entries,
            grouping_search_files,
            pending_changes: Vec::new(),
            cosmos_branch: None,
            cosmos_base_branch: None,
            pr_url: None,
            ship_step: None,
            workflow_step: WorkflowStep::default(),
            verify_state: VerifyState::default(),
            review_state: ReviewState::default(),
            ship_state: ShipState::default(),
            ask_cosmos_state: None,
            git_refresh_error: None,
            git_refresh_error_at: None,
            suggestion_refinement_in_progress: false,
            suggestion_provisional_count: 0,
            suggestion_validated_count: 0,
            suggestion_rejected_count: 0,
            current_suggestion_run_id: None,
            rolling_verify_precision: None,
            armed_suggestion_id: None,
            armed_file_hashes: HashMap::new(),
            update_available: None,
            update_progress: None,
            budget_warned_soft: false,
            budget_warned_hard: false,
            needs_redraw: true,
        }
    }

    fn active_suggestions_for_display(&self) -> Vec<&Suggestion> {
        self.suggestions
            .active_suggestions_with_limit(self.suggestions_display_cap)
    }

    /// Apply a new grouping and rebuild grouped trees.
    pub fn apply_grouping_update(&mut self, grouping: cosmos_core::grouping::CodebaseGrouping) {
        self.index.apply_grouping(&grouping);
        self.grouping = grouping;
        self.grouped_tree = build_grouped_tree(&self.grouping, &self.index);
        self.grouped_search_entries = build_grouped_search_entries(&self.grouped_tree);
        self.grouping_search_files = build_grouping_search_files(&self.grouping);
        self.filtered_grouped_indices = (0..self.grouped_tree.len()).collect();

        if self.project_selected >= self.filtered_grouped_indices.len() {
            self.project_selected = self.filtered_grouped_indices.len().saturating_sub(1);
        }
        self.project_scroll = 0;
        self.needs_redraw = true;
    }

    /// Replace index-backed UI data after a refresh.
    pub fn replace_index(&mut self, index: CodebaseIndex) {
        self.index = index;
        self.suggestions.index = self.index.clone();
        self.file_tree = build_file_tree(&self.index);
        self.flat_search_entries = build_flat_search_entries(&self.file_tree);
        self.filtered_tree_indices = (0..self.file_tree.len()).collect();
        let grouping = self.index.generate_grouping();
        self.apply_grouping_update(grouping);
    }

    /// Clear all pending changes (after commit)
    pub fn clear_pending_changes(&mut self) {
        self.pending_changes.clear();
        self.cosmos_branch = None;
        self.cosmos_base_branch = None;
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
            if let Err(e) = cosmos_adapters::git_ops::restore_file(&self.repo_path, path) {
                // Put the change back since we couldn't fully undo
                self.pending_changes.push(change);
                return Err(format!("Failed to restore {}: {}", path.display(), e));
            }
        }

        // Mark suggestion as not applied (so it can be re-applied if desired).
        self.suggestions.unmark_applied(change.suggestion_id);

        // If no more pending changes, return to original branch and suggestions step
        if self.pending_changes.is_empty() {
            if let Some(base_branch) = self.cosmos_base_branch.as_deref() {
                let _ = cosmos_adapters::git_ops::checkout_branch(&self.repo_path, base_branch);
            } else if let Ok(main_name) =
                cosmos_adapters::git_ops::get_main_branch_name(&self.repo_path)
            {
                // Fallback for older pending state that predates base-branch tracking.
                let _ = cosmos_adapters::git_ops::checkout_branch(&self.repo_path, &main_name);
            }

            // Clear cosmos branch tracking
            self.cosmos_branch = None;
            self.cosmos_base_branch = None;

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
        self.question_suggestion_selected = 0;
    }

    /// Exit question mode
    pub fn exit_question(&mut self) {
        self.input_mode = InputMode::Normal;
        self.question_input.clear();
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
        self.input_mode = InputMode::Normal;
        q
    }

    /// Move selection up in question suggestions
    pub fn question_suggestion_up(&mut self) {
        if self.question_suggestion_selected > 0 {
            self.question_suggestion_selected -= 1;
        }
    }

    /// Move selection down in question suggestions
    pub fn question_suggestion_down(&mut self) {
        if self.question_suggestion_selected < ASK_STARTER_QUESTIONS.len().saturating_sub(1) {
            self.question_suggestion_selected += 1;
        }
    }

    /// Use the selected suggestion as the question input
    pub fn use_selected_suggestion(&mut self) {
        if let Some(question) = ASK_STARTER_QUESTIONS.get(self.question_suggestion_selected) {
            self.question_input = (*question).to_string();
        }
    }

    /// Begin a new ask request and return the request id.
    pub fn begin_ask_request(&mut self) -> u64 {
        let request_id = self.next_ask_request_id;
        self.next_ask_request_id = self.next_ask_request_id.saturating_add(1);
        self.ask_in_flight = true;
        self.active_ask_request_id = Some(request_id);
        request_id
    }

    /// Complete the active ask request if the request id matches.
    pub fn complete_ask_request(&mut self, request_id: u64) -> bool {
        if self.active_ask_request_id != Some(request_id) {
            return false;
        }
        self.ask_in_flight = false;
        self.active_ask_request_id = None;
        true
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

    /// Set search query and re-apply filtering in one pass.
    pub fn set_search_query(&mut self, query: &str) {
        self.search_query.clear();
        self.search_query.push_str(query);
        self.apply_filter();
    }

    /// Apply search filter to file tree
    fn apply_filter(&mut self) {
        match self.view_mode {
            ViewMode::Flat => self.apply_flat_filter(),
            ViewMode::Grouped => self.apply_grouped_filter(),
        }
        self.project_scroll = 0;
        self.needs_redraw = true;
    }

    fn apply_flat_filter(&mut self) {
        if self.search_query.is_empty() {
            self.filtered_tree_indices = (0..self.file_tree.len()).collect();
        } else {
            let query = self.search_query.to_lowercase();
            self.filtered_tree_indices = self
                .flat_search_entries
                .iter()
                .enumerate()
                .filter(|(_, entry)| {
                    entry.name_lower.contains(&query) || entry.path_lower.contains(&query)
                })
                .map(|(idx, _)| idx)
                .collect();
        }

        if self.project_selected >= self.filtered_tree_indices.len() {
            self.project_selected = self.filtered_tree_indices.len().saturating_sub(1);
        }
    }

    fn apply_grouped_filter(&mut self) {
        if self.search_query.is_empty() {
            self.filtered_grouped_indices = (0..self.grouped_tree.len()).collect();
            if self.project_selected >= self.filtered_grouped_indices.len() {
                self.project_selected = self.filtered_grouped_indices.len().saturating_sub(1);
            }
            return;
        }

        let query = self.search_query.to_lowercase();
        let mut matching_layers: HashSet<cosmos_core::grouping::Layer> = HashSet::new();

        for entry in &self.grouping_search_files {
            if entry.name_lower.contains(&query) || entry.path_lower.contains(&query) {
                matching_layers.insert(entry.layer);
            }
        }

        for layer in &matching_layers {
            if let Some(group) = self.grouping.groups.get_mut(layer) {
                group.expanded = true;
            }
        }

        self.rebuild_grouped_tree_cache();
        self.filtered_grouped_indices = self.filter_grouped_indices(&query, &matching_layers);

        if self.project_selected >= self.filtered_grouped_indices.len() {
            self.project_selected = self.filtered_grouped_indices.len().saturating_sub(1);
        }
    }

    /// Filter out grouped entries in a single pass.
    fn filter_grouped_indices(
        &self,
        query: &str,
        matching_layers: &HashSet<cosmos_core::grouping::Layer>,
    ) -> Vec<usize> {
        use cosmos_core::grouping::GroupedEntryKind;

        let mut result = Vec::new();
        let mut current_layer_matches = false;
        let mut current_feature_idx: Option<usize> = None;
        let mut current_feature_name_match = false;
        let mut current_feature_emitted = false;

        for (idx, entry) in self.grouped_tree.iter().enumerate() {
            match &entry.kind {
                GroupedEntryKind::Layer(layer) => {
                    if let Some(feature_idx) = current_feature_idx.take() {
                        if current_feature_name_match && !current_feature_emitted {
                            result.push(feature_idx);
                        }
                    }
                    current_layer_matches = matching_layers.contains(layer);
                    current_feature_name_match = false;
                    current_feature_emitted = false;

                    if current_layer_matches {
                        result.push(idx);
                    }
                }
                GroupedEntryKind::Feature => {
                    if let Some(feature_idx) = current_feature_idx.take() {
                        if current_feature_name_match && !current_feature_emitted {
                            result.push(feature_idx);
                        }
                    }
                    if !current_layer_matches {
                        current_feature_name_match = false;
                        current_feature_emitted = false;
                        continue;
                    }
                    current_feature_idx = Some(idx);
                    current_feature_name_match =
                        self.grouped_search_entries[idx].name_lower.contains(query);
                    current_feature_emitted = false;
                }
                GroupedEntryKind::File => {
                    if !current_layer_matches {
                        continue;
                    }

                    let search = &self.grouped_search_entries[idx];
                    let name_matches = search.name_lower.contains(query);
                    let path_matches = search
                        .path_lower
                        .as_ref()
                        .map(|p| p.contains(query))
                        .unwrap_or(false);

                    if name_matches || path_matches {
                        if let Some(feature_idx) = current_feature_idx {
                            if !current_feature_emitted {
                                result.push(feature_idx);
                                current_feature_emitted = true;
                            }
                        }
                        result.push(idx);
                    }
                }
            }
        }

        if let Some(feature_idx) = current_feature_idx {
            if current_feature_name_match && !current_feature_emitted {
                result.push(feature_idx);
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
    }

    /// Toggle expand/collapse of the selected group in grouped view
    pub fn toggle_group_expand(&mut self) {
        if self.view_mode != ViewMode::Grouped {
            return;
        }

        let selected_kind = self.current_grouped_entry().map(|entry| entry.kind.clone());
        if let Some(kind) = selected_kind {
            use cosmos_core::grouping::GroupedEntryKind;
            match kind {
                GroupedEntryKind::Layer(layer) => {
                    if let Some(group) = self.grouping.groups.get_mut(&layer) {
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
        self.rebuild_grouped_tree_cache();
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
                if let Some(entry) = self.current_flat_entry() {
                    self.overlay = Overlay::FileDetail {
                        path: entry.path.clone(),
                        scroll: 0,
                    };
                }
            }
            ViewMode::Grouped => {
                if let Some(entry) = self.current_grouped_entry() {
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
            ActivePanel::Ask => ActivePanel::Suggestions,
            ActivePanel::Suggestions => ActivePanel::Ask,
        };

        if self.active_panel == ActivePanel::Ask
            && self.workflow_step == WorkflowStep::Suggestions
            && self.ask_cosmos_state.is_none()
        {
            self.start_question();
        }
    }

    /// Navigate down in the current panel
    pub fn navigate_down(&mut self) {
        match self.active_panel {
            ActivePanel::Ask => {}
            ActivePanel::Suggestions => {
                let previous = self.suggestion_selected;
                let max = self
                    .active_suggestions_for_display()
                    .len()
                    .saturating_sub(1);
                self.suggestion_selected = (self.suggestion_selected + 1).min(max);
                if self.workflow_step == WorkflowStep::Suggestions
                    && self.suggestion_selected != previous
                {
                    self.clear_apply_confirm();
                }
                self.ensure_suggestion_visible();
            }
        }
    }

    /// Navigate up in the current panel
    pub fn navigate_up(&mut self) {
        match self.active_panel {
            ActivePanel::Ask => {}
            ActivePanel::Suggestions => {
                let previous = self.suggestion_selected;
                self.suggestion_selected = self.suggestion_selected.saturating_sub(1);
                if self.workflow_step == WorkflowStep::Suggestions
                    && self.suggestion_selected != previous
                {
                    self.clear_apply_confirm();
                }
                self.ensure_suggestion_visible();
            }
        }
    }

    /// Get the length of the current project tree based on view mode
    fn project_tree_len(&self) -> usize {
        match self.view_mode {
            ViewMode::Flat => self.filtered_tree_indices.len(),
            ViewMode::Grouped => self.filtered_grouped_indices.len(),
        }
    }

    fn current_flat_entry(&self) -> Option<&FlatTreeEntry> {
        let idx = *self.filtered_tree_indices.get(self.project_selected)?;
        self.file_tree.get(idx)
    }

    fn current_grouped_entry(&self) -> Option<&cosmos_core::grouping::GroupedTreeEntry> {
        let idx = *self.filtered_grouped_indices.get(self.project_selected)?;
        self.grouped_tree.get(idx)
    }

    fn rebuild_grouped_tree_cache(&mut self) {
        self.grouped_tree = build_grouped_tree(&self.grouping, &self.index);
        self.grouped_search_entries = build_grouped_search_entries(&self.grouped_tree);
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
        let suggestions = self.active_suggestions_for_display();
        suggestions.get(self.suggestion_selected).copied()
    }

    /// Arm two-step apply confirmation for the currently selected suggestion.
    pub fn arm_apply_confirm(
        &mut self,
        suggestion_id: uuid::Uuid,
        file_hashes: HashMap<PathBuf, String>,
    ) {
        self.armed_suggestion_id = Some(suggestion_id);
        self.armed_file_hashes = file_hashes;
    }

    /// Clear two-step apply confirmation state.
    pub fn clear_apply_confirm(&mut self) {
        self.armed_suggestion_id = None;
        self.armed_file_hashes.clear();
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

    /// Open in-TUI API key setup overlay.
    pub fn open_api_key_overlay(&mut self, error: Option<String>) {
        self.overlay = Overlay::ApiKeySetup {
            input: String::new(),
            error,
            save_armed: false,
        };
    }

    /// Open pre-apply plan overlay with explicit scope and intent.
    pub fn open_apply_plan_overlay(
        &mut self,
        suggestion_id: uuid::Uuid,
        preview: cosmos_engine::llm::FixPreview,
        affected_files: Vec<PathBuf>,
        show_data_notice: bool,
    ) {
        self.overlay = Overlay::ApplyPlan {
            suggestion_id,
            preview,
            affected_files,
            confirm_apply: false,
            show_technical_details: false,
            show_data_notice,
            scroll: 0,
        };
    }

    pub fn apply_plan_scroll_down(&mut self) {
        if let Overlay::ApplyPlan { scroll, .. } = &mut self.overlay {
            *scroll += 1;
        }
    }

    pub fn apply_plan_scroll_up(&mut self) {
        if let Overlay::ApplyPlan { scroll, .. } = &mut self.overlay {
            *scroll = scroll.saturating_sub(1);
        }
    }

    pub fn apply_plan_toggle_technical_details(&mut self) {
        if let Overlay::ApplyPlan {
            show_technical_details,
            ..
        } = &mut self.overlay
        {
            *show_technical_details = !*show_technical_details;
        }
    }

    pub fn apply_plan_set_confirm(&mut self, confirm: bool) {
        if let Overlay::ApplyPlan { confirm_apply, .. } = &mut self.overlay {
            *confirm_apply = confirm;
        }
    }

    pub fn apply_plan_confirmed(&self) -> bool {
        matches!(
            self.overlay,
            Overlay::ApplyPlan {
                confirm_apply: true,
                ..
            }
        )
    }

    pub fn apply_plan_suggestion_id(&self) -> Option<uuid::Uuid> {
        match &self.overlay {
            Overlay::ApplyPlan { suggestion_id, .. } => Some(*suggestion_id),
            _ => None,
        }
    }

    /// Show inquiry response in the right panel (Ask Cosmos mode)
    pub fn show_inquiry(&mut self, response: String) {
        self.input_mode = InputMode::Normal;
        self.ask_in_flight = false;
        self.active_ask_request_id = None;
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

    /// Show a blocking message overlay for important failures.
    pub fn open_alert<T: Into<String>, U: Into<String>>(&mut self, title: T, message: U) {
        self.overlay = Overlay::Alert {
            title: title.into(),
            message: message.into(),
        };
        self.needs_redraw = true;
    }

    // ═══════════════════════════════════════════════════════════════════════════
    //  RESET COSMOS OVERLAY
    // ═══════════════════════════════════════════════════════════════════════════

    /// Open the reset cosmos overlay with default options selected
    pub fn open_reset_overlay(&mut self) {
        use cosmos_adapters::cache::ResetOption;

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
            error: None,
        };
    }

    /// Navigate in reset overlay
    pub fn reset_navigate(&mut self, delta: isize) {
        if let Overlay::Reset {
            options,
            selected,
            error,
        } = &mut self.overlay
        {
            let len = options.len();
            if len == 0 {
                return;
            }
            *selected = if delta > 0 {
                (*selected + delta as usize) % len
            } else {
                (*selected + len - ((-delta) as usize % len)) % len
            };
            *error = None;
        }
    }

    /// Toggle selection of the currently focused reset option
    pub fn reset_toggle_selected(&mut self) {
        if let Overlay::Reset {
            options,
            selected,
            error,
        } = &mut self.overlay
        {
            if let Some((_, is_selected)) = options.get_mut(*selected) {
                *is_selected = !*is_selected;
            }
            *error = None;
        }
    }

    /// Get the selected reset options (returns empty vec if not in reset overlay)
    pub fn get_reset_selections(&self) -> Vec<cosmos_adapters::cache::ResetOption> {
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

    pub fn set_reset_overlay_error(&mut self, message: String) {
        if let Overlay::Reset { error, .. } = &mut self.overlay {
            *error = Some(message);
        } else {
            self.open_alert("Reset failed", message);
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
        let actions =
            Self::startup_actions_for_context(changed_count, &current_branch, &main_branch);
        let selected_action = actions
            .first()
            .copied()
            .unwrap_or(StartupAction::ContinueAsIs);
        self.overlay = Overlay::StartupCheck {
            changed_count,
            current_branch,
            main_branch,
            mode: StartupMode::Choose,
            selected_action,
        };
    }

    /// Startup check actions shown for the current git context.
    pub fn startup_actions_for_context(
        changed_count: usize,
        current_branch: &str,
        main_branch: &str,
    ) -> Vec<StartupAction> {
        if changed_count > 0 {
            vec![
                StartupAction::SaveStartFresh,
                StartupAction::DiscardStartFresh,
                StartupAction::ContinueAsIs,
            ]
        } else if current_branch != main_branch {
            vec![StartupAction::SwitchToMain, StartupAction::ContinueAsIs]
        } else {
            vec![StartupAction::ContinueAsIs]
        }
    }

    /// Move focus in startup check action list.
    pub fn startup_check_navigate(&mut self, delta: isize) {
        if let Overlay::StartupCheck {
            changed_count,
            current_branch,
            main_branch,
            selected_action,
            mode,
        } = &mut self.overlay
        {
            if *mode != StartupMode::Choose {
                return;
            }

            let actions =
                Self::startup_actions_for_context(*changed_count, current_branch, main_branch);
            let len = actions.len();
            if len == 0 {
                return;
            }

            let current_idx = actions
                .iter()
                .position(|action| action == selected_action)
                .unwrap_or(0);
            let new_idx = if delta > 0 {
                (current_idx + delta as usize) % len
            } else {
                (current_idx + len - ((-delta) as usize % len)) % len
            };
            *selected_action = actions[new_idx];
        }
    }

    /// Set selected startup action when valid for current context.
    pub fn startup_check_set_selected(&mut self, action: StartupAction) {
        if let Overlay::StartupCheck {
            changed_count,
            current_branch,
            main_branch,
            selected_action,
            ..
        } = &mut self.overlay
        {
            let actions =
                Self::startup_actions_for_context(*changed_count, current_branch, main_branch);
            if actions.contains(&action) {
                *selected_action = action;
            }
        }
    }

    /// Set startup check mode (choose vs confirm discard).
    pub fn startup_check_set_mode(&mut self, mode: StartupMode) {
        if let Overlay::StartupCheck {
            changed_count,
            current_branch,
            main_branch,
            selected_action,
            mode: startup_mode,
        } = &mut self.overlay
        {
            *startup_mode = mode;
            if mode == StartupMode::Choose {
                let actions =
                    Self::startup_actions_for_context(*changed_count, current_branch, main_branch);
                if !actions.contains(selected_action) {
                    *selected_action = actions
                        .first()
                        .copied()
                        .unwrap_or(StartupAction::ContinueAsIs);
                }
            }
        }
    }

    /// Get currently selected startup action.
    pub fn startup_check_selected_action(&self) -> Option<StartupAction> {
        if let Overlay::StartupCheck {
            selected_action, ..
        } = &self.overlay
        {
            Some(*selected_action)
        } else {
            None
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
            Overlay::Help { scroll } | Overlay::FileDetail { scroll, .. } => {
                *scroll += 1;
            }
            _ => {}
        }
    }

    /// Scroll overlay up
    pub fn overlay_scroll_up(&mut self) {
        match &mut self.overlay {
            Overlay::Help { scroll } | Overlay::FileDetail { scroll, .. } => {
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
            WorkflowStep::Review => WorkflowStep::Suggestions,
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
        self.workflow_step = WorkflowStep::Suggestions;
        self.loading = LoadingState::None;
    }

    /// Set the preview result in the Verify step
    pub fn set_verify_preview(
        &mut self,
        preview: cosmos_engine::llm::FixPreview,
        file_hashes: std::collections::HashMap<PathBuf, String>,
    ) {
        if let Some(suggestion_id) = self.verify_state.suggestion_id {
            if let Some(suggestion) = self
                .suggestions
                .suggestions
                .iter_mut()
                .find(|s| s.id == suggestion_id)
            {
                suggestion.verification_state = preview.verification_state;
            }
        }
        self.verify_state.preview = Some(preview);
        self.verify_state.loading = false;
        self.verify_state.preview_hashes = file_hashes;
        self.loading = LoadingState::None;
    }

    /// Use cached verification result (transitions to Verify step without regenerating preview)
    pub fn use_cached_verify(&mut self) {
        self.verify_state.loading = false;
        self.verify_state.scroll = 0;
        self.workflow_step = WorkflowStep::Suggestions;
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
            let resolved =
                match cosmos_adapters::util::resolve_repo_path_allow_new(repo_path, target) {
                    Ok(r) => r,
                    Err(_) => return false,
                };

            let bytes = match std::fs::read(&resolved.absolute) {
                Ok(content) => content,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
                Err(_) => return false,
            };

            let current_hash = cosmos_adapters::util::hash_bytes(&bytes);

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

    /// Move to the Review step after applying a fix.
    pub fn start_review(&mut self, files: Vec<ReviewFileContent>) {
        self.review_state = ReviewState {
            files,
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
            confirm_extra_review_budget: false,
            verification_failed: false,
            verification_error: None,
        };
        self.workflow_step = WorkflowStep::Review;
        self.loading = LoadingState::ReviewingChanges;
    }

    /// Set review findings from the adversarial reviewer
    pub fn set_review_findings(
        &mut self,
        findings: Vec<cosmos_engine::llm::ReviewFinding>,
        summary: String,
    ) {
        self.review_state.findings = findings.clone();
        self.review_state.summary = summary;
        self.review_state.reviewing = false;
        self.review_state.confirm_ship = false;
        self.review_state.confirm_extra_review_budget = false;
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
        self.review_state.confirm_extra_review_budget = false;
    }

    /// Select all findings in review
    pub fn review_select_all(&mut self) {
        for i in 0..self.review_state.findings.len() {
            self.review_state.selected.insert(i);
        }
        self.review_state.confirm_ship = false;
        self.review_state.confirm_extra_review_budget = false;
    }

    /// Move cursor up in review
    pub fn review_cursor_up(&mut self) {
        self.review_state.cursor = self.review_state.cursor.saturating_sub(1);
        if self.review_state.cursor < self.review_state.scroll {
            self.review_state.scroll = self.review_state.cursor;
        }
        self.review_state.confirm_ship = false;
        self.review_state.confirm_extra_review_budget = false;
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
        self.review_state.confirm_extra_review_budget = false;
    }

    /// Check if review passed with completed verification and no remaining findings.
    pub fn review_passed(&self) -> bool {
        if self.review_state.reviewing {
            return false;
        }
        if self.review_state.verification_failed {
            return false;
        }
        self.review_state.findings.is_empty() && !self.review_state.summary.trim().is_empty()
    }

    /// Get selected findings for fixing
    pub fn get_selected_review_findings(&self) -> Vec<cosmos_engine::llm::ReviewFinding> {
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

    /// Update review with new file contents after applying selected fixes.
    pub fn review_fix_complete(&mut self, file_updates: Vec<(PathBuf, String)>) {
        // Add fixed finding titles for context in next review
        for i in &self.review_state.selected {
            if let Some(f) = self.review_state.findings.get(*i) {
                self.review_state.fixed_titles.push(f.title.clone());
            }
        }

        for (path, new_content) in file_updates {
            if let Some(file) = self.review_state.files.iter_mut().find(|f| f.path == path) {
                file.new_content = new_content;
            }
        }
        self.review_state.findings.clear();
        self.review_state.selected.clear();
        self.review_state.summary.clear();
        self.review_state.reviewing = false;
        self.review_state.fixing = false;
        self.review_state.confirm_ship = false;
        self.review_state.confirm_extra_review_budget = false;
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
        self.cosmos_base_branch = None;
    }

    /// Check if currently on main/master branch
    pub fn is_on_main_branch(&self) -> bool {
        self.context.branch == "main" || self.context.branch == "master"
    }
}

fn build_flat_search_entries(tree: &[FlatTreeEntry]) -> Vec<FlatSearchEntry> {
    tree.iter()
        .map(|entry| FlatSearchEntry {
            name_lower: entry.name.to_lowercase(),
            path_lower: entry.path.to_string_lossy().to_lowercase(),
        })
        .collect()
}

fn build_grouped_search_entries(
    tree: &[cosmos_core::grouping::GroupedTreeEntry],
) -> Vec<GroupedSearchEntry> {
    tree.iter()
        .map(|entry| GroupedSearchEntry {
            name_lower: entry.name.to_lowercase(),
            path_lower: entry
                .path
                .as_ref()
                .map(|path| path.to_string_lossy().to_lowercase()),
        })
        .collect()
}

fn build_grouping_search_files(
    grouping: &cosmos_core::grouping::CodebaseGrouping,
) -> Vec<GroupingSearchFile> {
    grouping
        .file_assignments
        .iter()
        .map(|(path, assignment)| GroupingSearchFile {
            layer: assignment.layer,
            name_lower: path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("")
                .to_lowercase(),
            path_lower: path.to_string_lossy().to_lowercase(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use cosmos_core::context::WorkContext;
    use cosmos_core::index::CodebaseIndex;
    use cosmos_core::suggest::SuggestionEngine;
    use std::collections::HashMap;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn make_test_app() -> App {
        let mut root = std::env::temp_dir();
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        root.push(format!("cosmos_ui_test_{}", nanos));
        std::fs::create_dir_all(&root).unwrap();

        let index = CodebaseIndex {
            root: root.clone(),
            files: HashMap::new(),
            index_errors: Vec::new(),
            git_head: Some("deadbeef".to_string()),
        };
        let suggestions = SuggestionEngine::new(index.clone());
        let context = WorkContext {
            branch: "main".to_string(),
            uncommitted_files: Vec::new(),
            staged_files: Vec::new(),
            untracked_files: Vec::new(),
            inferred_focus: None,
            modified_count: 0,
            repo_root: root,
        };

        App::new(index, suggestions, context)
    }

    #[test]
    fn review_passed_is_false_when_verification_failed() {
        let mut app = make_test_app();
        app.review_state.reviewing = false;
        app.review_state.findings.clear();
        app.review_state.summary = "Looks good".to_string();
        app.review_state.verification_failed = true;

        assert!(!app.review_passed());
    }

    #[test]
    fn start_question_resets_input_and_selection() {
        let mut app = make_test_app();
        app.question_input = "existing".to_string();
        app.question_suggestion_selected = ASK_STARTER_QUESTIONS.len().saturating_sub(1);

        app.start_question();

        assert_eq!(app.input_mode, InputMode::Question);
        assert!(app.question_input.is_empty());
        assert_eq!(app.question_suggestion_selected, 0);
    }

    #[test]
    fn question_suggestion_navigation_stays_in_bounds() {
        let mut app = make_test_app();
        app.start_question();

        for _ in 0..(ASK_STARTER_QUESTIONS.len() * 2) {
            app.question_suggestion_down();
        }
        assert_eq!(
            app.question_suggestion_selected,
            ASK_STARTER_QUESTIONS.len().saturating_sub(1)
        );

        for _ in 0..(ASK_STARTER_QUESTIONS.len() * 2) {
            app.question_suggestion_up();
        }
        assert_eq!(app.question_suggestion_selected, 0);
    }

    #[test]
    fn use_selected_suggestion_copies_question() {
        let mut app = make_test_app();
        app.start_question();
        app.question_suggestion_selected = 2;

        app.use_selected_suggestion();

        assert_eq!(app.question_input, ASK_STARTER_QUESTIONS[2]);
    }

    #[test]
    fn toggle_panel_to_ask_starts_question_mode() {
        let mut app = make_test_app();
        assert_eq!(app.active_panel, ActivePanel::Suggestions);
        assert_eq!(app.input_mode, InputMode::Normal);

        app.toggle_panel();

        assert_eq!(app.active_panel, ActivePanel::Ask);
        assert_eq!(app.input_mode, InputMode::Question);
        assert!(app.question_input.is_empty());
    }

    #[test]
    fn ask_request_tracking_ignores_stale_ids() {
        let mut app = make_test_app();
        let first = app.begin_ask_request();
        let second = app.begin_ask_request();

        assert_ne!(first, second);
        assert!(app.ask_in_flight);
        assert!(!app.complete_ask_request(first));
        assert!(app.ask_in_flight);
        assert!(app.complete_ask_request(second));
        assert!(!app.ask_in_flight);
        assert!(app.active_ask_request_id.is_none());
    }

    #[test]
    fn startup_actions_for_changed_context() {
        let actions = App::startup_actions_for_context(2, "feature/work", "main");
        assert_eq!(
            actions,
            vec![
                StartupAction::SaveStartFresh,
                StartupAction::DiscardStartFresh,
                StartupAction::ContinueAsIs
            ]
        );
    }

    #[test]
    fn startup_actions_for_branch_only_context() {
        let actions = App::startup_actions_for_context(0, "feature/work", "main");
        assert_eq!(
            actions,
            vec![StartupAction::SwitchToMain, StartupAction::ContinueAsIs]
        );
    }

    #[test]
    fn show_startup_check_sets_default_selection_for_changed_context() {
        let mut app = make_test_app();
        app.show_startup_check(3, "feature/work".to_string(), "main".to_string());

        if let Overlay::StartupCheck {
            mode,
            selected_action,
            ..
        } = app.overlay
        {
            assert_eq!(mode, StartupMode::Choose);
            assert_eq!(selected_action, StartupAction::SaveStartFresh);
        } else {
            panic!("expected startup check overlay");
        }
    }

    #[test]
    fn show_startup_check_sets_default_selection_for_branch_only_context() {
        let mut app = make_test_app();
        app.show_startup_check(0, "feature/work".to_string(), "main".to_string());

        if let Overlay::StartupCheck {
            mode,
            selected_action,
            ..
        } = app.overlay
        {
            assert_eq!(mode, StartupMode::Choose);
            assert_eq!(selected_action, StartupAction::SwitchToMain);
        } else {
            panic!("expected startup check overlay");
        }
    }

    #[test]
    fn startup_check_navigation_wraps() {
        let mut app = make_test_app();
        app.show_startup_check(1, "feature/work".to_string(), "main".to_string());

        app.startup_check_navigate(-1);
        assert_eq!(
            app.startup_check_selected_action(),
            Some(StartupAction::ContinueAsIs)
        );

        app.startup_check_navigate(1);
        assert_eq!(
            app.startup_check_selected_action(),
            Some(StartupAction::SaveStartFresh)
        );
    }
}
