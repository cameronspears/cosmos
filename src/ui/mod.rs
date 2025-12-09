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

#![allow(dead_code)]

pub mod markdown;
pub mod panels;
pub mod theme;

use crate::context::WorkContext;
use crate::index::{CodebaseIndex, FlatTreeEntry};
use crate::suggest::{Priority, Suggestion, SuggestionEngine};
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
    Flat,     // Traditional flat file list
    Grouped,  // Grouped by layer and feature
}

impl ViewMode {
    pub fn label(&self) -> &'static str {
        match self {
            ViewMode::Flat => "flat",
            ViewMode::Grouped => "grouped",
        }
    }

    pub fn toggle(&self) -> Self {
        match self {
            ViewMode::Flat => ViewMode::Grouped,
            ViewMode::Grouped => ViewMode::Flat,
        }
    }
}

/// Sort mode for file explorer
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SortMode {
    #[default]
    Name,
    Priority,
    Size,
    Modified,
    Complexity,
}

impl SortMode {
    pub fn label(&self) -> &'static str {
        match self {
            SortMode::Name => "name",
            SortMode::Priority => "priority",
            SortMode::Size => "size",
            SortMode::Modified => "modified",
            SortMode::Complexity => "complexity",
        }
    }
    
    pub fn next(&self) -> Self {
        match self {
            SortMode::Name => SortMode::Priority,
            SortMode::Priority => SortMode::Size,
            SortMode::Size => SortMode::Modified,
            SortMode::Modified => SortMode::Complexity,
            SortMode::Complexity => SortMode::Name,
        }
    }
}

/// Input mode
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum InputMode {
    #[default]
    Normal,
    Search,
    Question,  // Asking cosmos a question
}

/// Loading state for background tasks
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LoadingState {
    #[default]
    None,
    GeneratingSuggestions,
    GeneratingSummaries,
    GeneratingPreview,  // Fast preview generation (<1s)
    GeneratingFix,      // Full fix generation (slower)
    Answering,          // For question answering
}

impl LoadingState {
    pub fn message(&self) -> &'static str {
        match self {
            LoadingState::None => "",
            LoadingState::GeneratingSuggestions => "Generating suggestions",
            LoadingState::GeneratingSummaries => "Summarizing files",
            LoadingState::GeneratingPreview => "Previewing fix...",
            LoadingState::GeneratingFix => "Generating fix...",
            LoadingState::Answering => "Thinking...",
        }
    }
    
    pub fn is_loading(&self) -> bool {
        !matches!(self, LoadingState::None)
    }
}

/// Mode for the apply confirmation overlay
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum ApplyMode {
    #[default]
    View,   // Default: review diff, y/n to apply
    Edit,   // Inline editing of diff text
    Chat,   // Chat input to refine suggestion
}

/// Spinner animation frames (braille pattern)
pub const SPINNER_FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// Git file status for the status panel
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GitFileStatus {
    Staged,
    Modified,
    Untracked,
}

/// Overlay state
#[derive(Debug, Clone, PartialEq, Default)]
pub enum Overlay {
    #[default]
    None,
    Help,
    SuggestionDetail {
        suggestion_id: uuid::Uuid,
        scroll: usize,
    },
    Inquiry {
        response: String,
        scroll: usize,
    },
    ApplyConfirm {
        suggestion_id: uuid::Uuid,
        diff_preview: String,
        scroll: usize,
        mode: ApplyMode,
        edit_buffer: Option<String>,
        chat_input: String,
        file_path: PathBuf,
        summary: String,
    },
    /// Fast preview of what a fix will do - Phase 1 of two-phase fix flow
    FixPreview {
        suggestion_id: uuid::Uuid,
        file_path: PathBuf,
        summary: String,
        preview: crate::suggest::llm::FixPreview,
        modifier_input: String,
    },
    FileDetail {
        path: PathBuf,
        scroll: usize,
    },
    /// Branch creation dialog
    BranchCreate {
        branch_name: String,
        commit_message: String,
        pending_files: Vec<PathBuf>,
    },
    /// PR Review panel with AI code review
    PRReview {
        branch_name: String,
        files_changed: Vec<(PathBuf, String)>, // (path, diff)
        review_comments: Vec<PRReviewComment>,
        scroll: usize,
        reviewing: bool,
        pr_url: Option<String>,
    },
    /// Git status panel for viewing and managing changed files
    GitStatus {
        staged: Vec<String>,
        modified: Vec<String>,
        untracked: Vec<String>,
        selected: usize,
        scroll: usize,
        commit_input: Option<String>,
    },
}

/// A comment from AI code review
#[derive(Debug, Clone, PartialEq)]
pub struct PRReviewComment {
    pub file: PathBuf,
    pub comment: String,
    pub severity: ReviewSeverity,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewSeverity {
    Praise,    // Good stuff
    Info,      // FYI
    Suggest,   // Could improve
    Warning,   // Should fix
}

impl ReviewSeverity {
    pub fn icon(&self) -> &'static str {
        match self {
            ReviewSeverity::Praise => "✓",
            ReviewSeverity::Info => "○",
            ReviewSeverity::Suggest => "◐",
            ReviewSeverity::Warning => "●",
        }
    }
}

/// Toast notification
pub struct Toast {
    pub message: String,
    pub created_at: Instant,
}

impl Toast {
    pub fn new(message: &str) -> Self {
        Self {
            message: message.to_string(),
            created_at: Instant::now(),
        }
    }

    pub fn is_expired(&self) -> bool {
        self.created_at.elapsed().as_secs() >= 3
    }
}

/// A pending change that has been applied but not yet committed
#[derive(Debug, Clone)]
pub struct PendingChange {
    pub suggestion_id: uuid::Uuid,
    pub file_path: PathBuf,
    pub description: String,
    pub diff: String,
    pub applied_at: Instant,
}

impl PendingChange {
    pub fn new(suggestion_id: uuid::Uuid, file_path: PathBuf, description: String, diff: String) -> Self {
        Self {
            suggestion_id,
            file_path,
            description,
            diff,
            applied_at: Instant::now(),
        }
    }
}

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
    pub sort_mode: SortMode,
    pub view_mode: ViewMode,
    
    // Question input (ask cosmos)
    pub question_input: String,
    
    // Loading state for background tasks
    pub loading: LoadingState,
    pub loading_frame: usize,
    
    // LLM-generated file summaries
    pub llm_summaries: std::collections::HashMap<PathBuf, String>,
    
    // Cost tracking
    pub session_cost: f64,          // Total USD spent this session
    pub session_tokens: u32,        // Total tokens used this session
    pub active_model: Option<String>, // Current/last model used
    
    // Track if summaries need generation (to avoid showing loading state when all cached)
    pub needs_summary_generation: bool,
    
    // Summary generation progress (completed, total)
    pub summary_progress: Option<(usize, usize)>,
    
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
}

impl App {
    /// Create a new Cosmos app
    pub fn new(
        index: CodebaseIndex,
        suggestions: SuggestionEngine,
        context: WorkContext,
    ) -> Self {
        let file_tree = build_file_tree(&index, SortMode::Name);
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
            sort_mode: SortMode::Name,
            view_mode: ViewMode::Grouped,  // Default to grouped view
            question_input: String::new(),
            loading: LoadingState::None,
            loading_frame: 0,
            llm_summaries: std::collections::HashMap::new(),
            session_cost: 0.0,
            session_tokens: 0,
            active_model: None,
            needs_summary_generation: false,
            summary_progress: None,
            file_tree,
            filtered_tree,
            repo_path,
            grouping,
            grouped_tree,
            filtered_grouped_tree,
            pending_changes: Vec::new(),
            cosmos_branch: None,
        }
    }
    
    /// Add a pending change from an applied fix
    pub fn add_pending_change(&mut self, suggestion_id: uuid::Uuid, file_path: PathBuf, description: String, diff: String) {
        self.pending_changes.push(PendingChange::new(suggestion_id, file_path, description, diff));
    }
    
    /// Get count of pending changes
    pub fn pending_change_count(&self) -> usize {
        self.pending_changes.len()
    }
    
    /// Clear all pending changes (after commit)
    pub fn clear_pending_changes(&mut self) {
        self.pending_changes.clear();
        self.cosmos_branch = None;
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
                    self.filtered_tree = self.file_tree.iter()
                        .filter(|entry| {
                            entry.name.to_lowercase().contains(&query) ||
                            entry.path.to_string_lossy().to_lowercase().contains(&query)
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
                    self.filtered_grouped_tree = self.grouped_tree.clone();
                } else {
                    let query = self.search_query.to_lowercase();
                    self.filtered_grouped_tree = self.grouped_tree.iter()
                        .filter(|entry| {
                            entry.name.to_lowercase().contains(&query) ||
                            entry.path.as_ref().map(|p| 
                                p.to_string_lossy().to_lowercase().contains(&query)
                            ).unwrap_or(false)
                        })
                        .cloned()
                        .collect();
                }
                
                // Reset selection if it's out of bounds
                if self.project_selected >= self.filtered_grouped_tree.len() {
                    self.project_selected = self.filtered_grouped_tree.len().saturating_sub(1);
                }
            }
        }
        self.project_scroll = 0;
    }
    
    /// Cycle to next sort mode
    pub fn cycle_sort(&mut self) {
        self.sort_mode = self.sort_mode.next();
        self.file_tree = build_file_tree(&self.index, self.sort_mode);
        self.apply_filter();
        self.show_toast(&format!("Sort: {}", self.sort_mode.label()));
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
    
    /// Collapse all layer groups
    pub fn collapse_all(&mut self) {
        if self.view_mode != ViewMode::Grouped {
            return;
        }
        
        for group in self.grouping.groups.values_mut() {
            group.expanded = false;
        }
        self.rebuild_grouped_tree();
        self.project_selected = 0;
        self.project_scroll = 0;
        self.show_toast("Collapsed all");
    }
    
    /// Expand all layer groups
    pub fn expand_all(&mut self) {
        if self.view_mode != ViewMode::Grouped {
            return;
        }
        
        for group in self.grouping.groups.values_mut() {
            group.expanded = true;
        }
        self.rebuild_grouped_tree();
        self.show_toast("Expanded all");
    }
    
    /// Jump to a specific layer by index (1-8 keys)
    pub fn jump_to_layer(&mut self, layer_index: usize) {
        if self.view_mode != ViewMode::Grouped {
            self.show_toast("Use 'g' for grouped view first");
            return;
        }
        
        if let Some(target_layer) = crate::grouping::Layer::from_index(layer_index) {
            // Find the position of this layer in the tree
            for (i, entry) in self.filtered_grouped_tree.iter().enumerate() {
                if let crate::grouping::GroupedEntryKind::Layer(layer) = &entry.kind {
                    if *layer == target_layer {
                        self.project_selected = i;
                        self.ensure_project_visible();
                        self.show_toast(&format!("Jumped to {}", target_layer.label()));
                        return;
                    }
                }
            }
            self.show_toast(&format!("No {} files", target_layer.label()));
        }
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
                let max = self.suggestions.active_suggestions().len().saturating_sub(1);
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

    /// Get currently selected file
    pub fn selected_file(&self) -> Option<&PathBuf> {
        match self.view_mode {
            ViewMode::Flat => self.filtered_tree.get(self.project_selected).map(|e| &e.path),
            ViewMode::Grouped => self.filtered_grouped_tree
                .get(self.project_selected)
                .and_then(|e| e.path.as_ref()),
        }
    }
    
    /// Get the FileIndex for the currently selected file
    pub fn selected_file_index(&self) -> Option<&crate::index::FileIndex> {
        self.selected_file().and_then(|path| self.index.files.get(path))
    }

    /// Get currently selected suggestion
    pub fn selected_suggestion(&self) -> Option<&Suggestion> {
        let suggestions = self.suggestions.active_suggestions();
        suggestions.get(self.suggestion_selected).copied()
    }

    /// Show suggestion detail
    pub fn show_suggestion_detail(&mut self) {
        if let Some(suggestion) = self.selected_suggestion() {
            self.overlay = Overlay::SuggestionDetail {
                suggestion_id: suggestion.id,
                scroll: 0,
            };
        }
    }

    /// Toggle help overlay
    pub fn toggle_help(&mut self) {
        self.overlay = match self.overlay {
            Overlay::Help => Overlay::None,
            _ => Overlay::Help,
        };
    }

    /// Close overlay
    pub fn close_overlay(&mut self) {
        self.overlay = Overlay::None;
    }

    /// Show inquiry response
    pub fn show_inquiry(&mut self, response: String) {
        self.overlay = Overlay::Inquiry { response, scroll: 0 };
    }

    /// Show apply confirmation overlay with generated fix
    pub fn show_apply_confirm(&mut self, suggestion_id: uuid::Uuid, diff_preview: String, file_path: PathBuf, summary: String) {
        self.overlay = Overlay::ApplyConfirm {
            suggestion_id,
            diff_preview,
            scroll: 0,
            mode: ApplyMode::View,
            edit_buffer: None,
            chat_input: String::new(),
            file_path,
            summary,
        };
    }

    /// Show fix preview overlay (Phase 1 - fast preview)
    pub fn show_fix_preview(&mut self, suggestion_id: uuid::Uuid, file_path: PathBuf, summary: String, preview: crate::suggest::llm::FixPreview) {
        self.overlay = Overlay::FixPreview {
            suggestion_id,
            file_path,
            summary,
            preview,
            modifier_input: String::new(),
        };
    }

    /// Push character to preview modifier input
    pub fn preview_modifier_push(&mut self, c: char) {
        if let Overlay::FixPreview { modifier_input, .. } = &mut self.overlay {
            modifier_input.push(c);
        }
    }

    /// Pop character from preview modifier input
    pub fn preview_modifier_pop(&mut self) {
        if let Overlay::FixPreview { modifier_input, .. } = &mut self.overlay {
            modifier_input.pop();
        }
    }

    /// Get the current preview modifier text
    pub fn get_preview_modifier(&self) -> Option<&str> {
        if let Overlay::FixPreview { modifier_input, .. } = &self.overlay {
            Some(modifier_input.as_str())
        } else {
            None
        }
    }

    /// Get mutable access to apply confirm edit buffer
    pub fn apply_edit_push(&mut self, c: char) {
        if let Overlay::ApplyConfirm { edit_buffer, .. } = &mut self.overlay {
            if let Some(buf) = edit_buffer {
                buf.push(c);
            }
        }
    }

    /// Remove character from apply edit buffer
    pub fn apply_edit_pop(&mut self) {
        if let Overlay::ApplyConfirm { edit_buffer, .. } = &mut self.overlay {
            if let Some(buf) = edit_buffer {
                buf.pop();
            }
        }
    }

    /// Push character to chat input
    pub fn apply_chat_push(&mut self, c: char) {
        if let Overlay::ApplyConfirm { chat_input, .. } = &mut self.overlay {
            chat_input.push(c);
        }
    }

    /// Pop character from chat input
    pub fn apply_chat_pop(&mut self) {
        if let Overlay::ApplyConfirm { chat_input, .. } = &mut self.overlay {
            chat_input.pop();
        }
    }

    /// Set apply mode
    pub fn set_apply_mode(&mut self, new_mode: ApplyMode) {
        if let Overlay::ApplyConfirm { mode, edit_buffer, diff_preview, .. } = &mut self.overlay {
            // When entering edit mode, populate the edit buffer with the current diff
            if new_mode == ApplyMode::Edit && edit_buffer.is_none() {
                *edit_buffer = Some(diff_preview.clone());
            }
            *mode = new_mode;
        }
    }

    /// Get current apply mode
    pub fn get_apply_mode(&self) -> Option<&ApplyMode> {
        if let Overlay::ApplyConfirm { mode, .. } = &self.overlay {
            Some(mode)
        } else {
            None
        }
    }

    /// Commit edit buffer back to diff preview
    pub fn commit_apply_edit(&mut self) {
        if let Overlay::ApplyConfirm { edit_buffer, diff_preview, mode, .. } = &mut self.overlay {
            if let Some(buf) = edit_buffer.take() {
                *diff_preview = buf;
            }
            *mode = ApplyMode::View;
        }
    }

    /// Discard edit buffer and return to view mode
    pub fn discard_apply_edit(&mut self) {
        if let Overlay::ApplyConfirm { edit_buffer, mode, .. } = &mut self.overlay {
            *edit_buffer = None;
            *mode = ApplyMode::View;
        }
    }

    /// Get chat input for refinement
    pub fn get_apply_chat_input(&self) -> Option<&str> {
        if let Overlay::ApplyConfirm { chat_input, .. } = &self.overlay {
            Some(chat_input.as_str())
        } else {
            None
        }
    }

    /// Update diff preview (after refinement)
    pub fn update_apply_diff(&mut self, new_diff: String) {
        if let Overlay::ApplyConfirm { diff_preview, mode, chat_input, .. } = &mut self.overlay {
            *diff_preview = new_diff;
            *mode = ApplyMode::View;
            chat_input.clear();
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

    /// Show a toast message
    pub fn show_toast(&mut self, message: &str) {
        self.toast = Some(Toast::new(message));
    }

    /// Scroll overlay down
    pub fn overlay_scroll_down(&mut self) {
        match &mut self.overlay {
            Overlay::SuggestionDetail { scroll, .. }
            | Overlay::Inquiry { scroll, .. }
            | Overlay::ApplyConfirm { scroll, .. } => {
                *scroll += 1;
            }
            _ => {}
        }
    }

    /// Scroll overlay up
    pub fn overlay_scroll_up(&mut self) {
        match &mut self.overlay {
            Overlay::SuggestionDetail { scroll, .. }
            | Overlay::Inquiry { scroll, .. }
            | Overlay::ApplyConfirm { scroll, .. } => {
                *scroll = scroll.saturating_sub(1);
            }
            _ => {}
        }
    }

    /// Dismiss the currently selected suggestion
    pub fn dismiss_selected(&mut self) {
        if let Some(suggestion) = self.selected_suggestion() {
            let id = suggestion.id;
            self.suggestions.dismiss(id);
            self.show_toast("Suggestion dismissed");
        }
    }
    
    /// Show the branch creation dialog
    pub fn show_branch_dialog(&mut self) {
        if self.pending_changes.is_empty() {
            self.show_toast("No pending changes to commit");
            return;
        }
        
        // Generate a branch name from pending changes
        let branch_name = self.generate_branch_name();
        let commit_message = self.generate_commit_message();
        let pending_files: Vec<PathBuf> = self.pending_changes.iter()
            .map(|c| c.file_path.clone())
            .collect();
        
        self.overlay = Overlay::BranchCreate {
            branch_name,
            commit_message,
            pending_files,
        };
    }
    
    /// Generate a descriptive branch name from pending changes
    fn generate_branch_name(&self) -> String {
        if self.pending_changes.is_empty() {
            return "cosmos/changes".to_string();
        }
        
        // Get the first change's description for the branch name
        let first_desc = &self.pending_changes[0].description;
        let words: Vec<&str> = first_desc.split_whitespace()
            .take(4)
            .collect();
        
        let slug = words.join("-")
            .to_lowercase()
            .chars()
            .filter(|c| c.is_alphanumeric() || *c == '-')
            .collect::<String>();
        
        format!("cosmos/{}", if slug.is_empty() { "fix" } else { &slug })
    }
    
    /// Generate a commit message from pending changes
    fn generate_commit_message(&self) -> String {
        if self.pending_changes.len() == 1 {
            self.pending_changes[0].description.clone()
        } else {
            let summaries: Vec<String> = self.pending_changes.iter()
                .map(|c| format!("- {}", c.description))
                .collect();
            format!("Cosmos fixes:\n\n{}", summaries.join("\n"))
        }
    }
    
    /// Show the PR review panel
    pub fn show_pr_review(&mut self) {
        if self.pending_changes.is_empty() {
            self.show_toast("No changes to review");
            return;
        }
        
        let branch_name = self.cosmos_branch.clone()
            .unwrap_or_else(|| self.generate_branch_name());
        
        let files_changed: Vec<(PathBuf, String)> = self.pending_changes.iter()
            .map(|c| (c.file_path.clone(), c.diff.clone()))
            .collect();
        
        self.overlay = Overlay::PRReview {
            branch_name,
            files_changed,
            review_comments: Vec::new(),
            scroll: 0,
            reviewing: false,
            pr_url: None,
        };
    }
    
    /// Update branch name in branch dialog
    pub fn update_branch_name(&mut self, name: String) {
        if let Overlay::BranchCreate { branch_name, .. } = &mut self.overlay {
            *branch_name = name;
        }
    }
    
    /// Update commit message in branch dialog
    pub fn update_commit_message(&mut self, msg: String) {
        if let Overlay::BranchCreate { commit_message, .. } = &mut self.overlay {
            *commit_message = msg;
        }
    }
    
    /// Set PR review comments from AI analysis
    pub fn set_review_comments(&mut self, comments: Vec<PRReviewComment>) {
        if let Overlay::PRReview { review_comments, reviewing, .. } = &mut self.overlay {
            *review_comments = comments;
            *reviewing = false;
        }
    }
    
    /// Set the PR URL after creation
    pub fn set_pr_url(&mut self, url: String) {
        if let Overlay::PRReview { pr_url, .. } = &mut self.overlay {
            *pr_url = Some(url);
        }
    }
    
    /// Show the git status panel with current changes
    pub fn show_git_status(&mut self) {
        use crate::git_ops;
        
        match git_ops::current_status(&self.repo_path) {
            Ok(status) => {
                self.overlay = Overlay::GitStatus {
                    staged: status.staged,
                    modified: status.modified,
                    untracked: status.untracked,
                    selected: 0,
                    scroll: 0,
                    commit_input: None,
                };
            }
            Err(e) => {
                self.show_toast(&format!("Git error: {}", e));
            }
        }
    }
    
    /// Refresh git status in the overlay
    pub fn refresh_git_status(&mut self) {
        use crate::git_ops;
        
        if let Overlay::GitStatus { staged, modified, untracked, selected, .. } = &mut self.overlay {
            if let Ok(status) = git_ops::current_status(&self.repo_path) {
                *staged = status.staged;
                *modified = status.modified;
                *untracked = status.untracked;
                // Clamp selection to valid range
                let total = staged.len() + modified.len() + untracked.len();
                if *selected >= total && total > 0 {
                    *selected = total - 1;
                }
            }
        }
    }
    
    /// Navigate in git status panel
    pub fn git_status_navigate(&mut self, delta: isize) {
        if let Overlay::GitStatus { staged, modified, untracked, selected, .. } = &mut self.overlay {
            let total = staged.len() + modified.len() + untracked.len();
            if total == 0 {
                return;
            }
            
            let new_sel = (*selected as isize + delta).clamp(0, (total as isize) - 1) as usize;
            *selected = new_sel;
        }
    }
    
    /// Get the selected file path in git status panel
    pub fn git_status_selected_file(&self) -> Option<(String, GitFileStatus)> {
        if let Overlay::GitStatus { staged, modified, untracked, selected, .. } = &self.overlay {
            let staged_len = staged.len();
            let modified_len = modified.len();
            
            if *selected < staged_len {
                return Some((staged[*selected].clone(), GitFileStatus::Staged));
            } else if *selected < staged_len + modified_len {
                return Some((modified[*selected - staged_len].clone(), GitFileStatus::Modified));
            } else if *selected < staged_len + modified_len + untracked.len() {
                return Some((untracked[*selected - staged_len - modified_len].clone(), GitFileStatus::Untracked));
            }
        }
        None
    }
    
    /// Stage the selected file
    pub fn git_stage_selected(&mut self) {
        use crate::git_ops;
        
        if let Some((path, status)) = self.git_status_selected_file() {
            match status {
                GitFileStatus::Modified | GitFileStatus::Untracked => {
                    if let Err(e) = git_ops::stage_file(&self.repo_path, &path) {
                        self.show_toast(&format!("Stage failed: {}", e));
                    } else {
                        self.show_toast(&format!("Staged: {}", path));
                        self.refresh_git_status();
                    }
                }
                GitFileStatus::Staged => {
                    self.show_toast("Already staged");
                }
            }
        }
    }
    
    /// Unstage the selected file
    pub fn git_unstage_selected(&mut self) {
        use std::process::Command;
        
        if let Some((path, status)) = self.git_status_selected_file() {
            if status == GitFileStatus::Staged {
                let output = Command::new("git")
                    .current_dir(&self.repo_path)
                    .args(["reset", "HEAD", "--", &path])
                    .output();
                    
                match output {
                    Ok(o) if o.status.success() => {
                        self.show_toast(&format!("Unstaged: {}", path));
                        self.refresh_git_status();
                    }
                    Ok(o) => {
                        self.show_toast(&format!("Unstage failed: {}", String::from_utf8_lossy(&o.stderr)));
                    }
                    Err(e) => {
                        self.show_toast(&format!("Unstage failed: {}", e));
                    }
                }
            } else {
                self.show_toast("Not staged");
            }
        }
    }
    
    /// Restore (discard changes) the selected file
    pub fn git_restore_selected(&mut self) {
        use crate::git_ops;
        
        if let Some((path, status)) = self.git_status_selected_file() {
            match status {
                GitFileStatus::Modified => {
                    if let Err(e) = git_ops::reset_file(&self.repo_path, &path) {
                        self.show_toast(&format!("Restore failed: {}", e));
                    } else {
                        self.show_toast(&format!("Restored: {}", path));
                        self.refresh_git_status();
                        // Also refresh context
                        let _ = self.context.refresh();
                    }
                }
                GitFileStatus::Staged => {
                    self.show_toast("Unstage first (u), then restore");
                }
                GitFileStatus::Untracked => {
                    self.show_toast("Untracked files can't be restored");
                }
            }
        }
    }
    
    /// Stage all modified files
    pub fn git_stage_all(&mut self) {
        use crate::git_ops;
        
        if let Err(e) = git_ops::stage_all(&self.repo_path) {
            self.show_toast(&format!("Stage all failed: {}", e));
        } else {
            self.show_toast("All files staged");
            self.refresh_git_status();
        }
    }
    
    /// Start commit input mode
    pub fn git_start_commit(&mut self) {
        if let Overlay::GitStatus { staged, commit_input, .. } = &mut self.overlay {
            if staged.is_empty() {
                self.show_toast("No staged files to commit");
                return;
            }
            *commit_input = Some(String::new());
        }
    }
    
    /// Cancel commit input
    pub fn git_cancel_commit(&mut self) {
        if let Overlay::GitStatus { commit_input, .. } = &mut self.overlay {
            *commit_input = None;
        }
    }
    
    /// Push character to commit message
    pub fn git_commit_push(&mut self, c: char) {
        if let Overlay::GitStatus { commit_input: Some(input), .. } = &mut self.overlay {
            input.push(c);
        }
    }
    
    /// Pop character from commit message
    pub fn git_commit_pop(&mut self) {
        if let Overlay::GitStatus { commit_input: Some(input), .. } = &mut self.overlay {
            input.pop();
        }
    }
    
    /// Execute the commit
    pub fn git_do_commit(&mut self) -> Result<String, String> {
        use crate::git_ops;
        
        if let Overlay::GitStatus { commit_input: Some(msg), .. } = &self.overlay {
            if msg.trim().is_empty() {
                return Err("Commit message cannot be empty".to_string());
            }
            
            match git_ops::commit(&self.repo_path, msg) {
                Ok(oid) => {
                    // Clear commit input and refresh
                    if let Overlay::GitStatus { commit_input, .. } = &mut self.overlay {
                        *commit_input = None;
                    }
                    self.refresh_git_status();
                    let _ = self.context.refresh();
                    Ok(oid)
                }
                Err(e) => Err(format!("Commit failed: {}", e))
            }
        } else {
            Err("No commit in progress".to_string())
        }
    }
    
    /// Push current branch
    pub fn git_push(&mut self) -> Result<String, String> {
        use crate::git_ops;
        
        let branch = self.context.branch.clone();
        match git_ops::push_branch(&self.repo_path, &branch) {
            Ok(output) => {
                let _ = self.context.refresh();
                Ok(output)
            }
            Err(e) => Err(format!("Push failed: {}", e))
        }
    }
    
    /// Check if we're in commit input mode
    pub fn is_git_commit_mode(&self) -> bool {
        matches!(&self.overlay, Overlay::GitStatus { commit_input: Some(_), .. })
    }
    
    /// Get the current commit message being typed
    pub fn get_git_commit_input(&self) -> Option<&str> {
        if let Overlay::GitStatus { commit_input: Some(input), .. } = &self.overlay {
            Some(input.as_str())
        } else {
            None
        }
    }
    
    /// Delete the selected untracked file
    pub fn git_delete_untracked(&mut self) {
        use std::fs;
        
        if let Some((path, status)) = self.git_status_selected_file() {
            if status == GitFileStatus::Untracked {
                let full_path = self.repo_path.join(&path);
                if full_path.is_dir() {
                    match fs::remove_dir_all(&full_path) {
                        Ok(_) => {
                            self.show_toast(&format!("Deleted: {}", path));
                            self.refresh_git_status();
                        }
                        Err(e) => {
                            self.show_toast(&format!("Delete failed: {}", e));
                        }
                    }
                } else {
                    match fs::remove_file(&full_path) {
                        Ok(_) => {
                            self.show_toast(&format!("Deleted: {}", path));
                            self.refresh_git_status();
                        }
                        Err(e) => {
                            self.show_toast(&format!("Delete failed: {}", e));
                        }
                    }
                }
            } else {
                self.show_toast("Use 'r' to restore tracked files");
            }
        }
    }
    
    /// Clean all untracked files (git clean -fd)
    pub fn git_clean_untracked(&mut self) -> Result<(), String> {
        use std::process::Command;
        
        let output = Command::new("git")
            .current_dir(&self.repo_path)
            .args(["clean", "-fd"])
            .output()
            .map_err(|e| format!("Failed to run git clean: {}", e))?;
        
        if output.status.success() {
            self.refresh_git_status();
            let _ = self.context.refresh();
            Ok(())
        } else {
            Err(format!("git clean failed: {}", String::from_utf8_lossy(&output.stderr)))
        }
    }
    
    /// Reset branch to clean state (discard all changes + remove untracked)
    pub fn git_reset_hard(&mut self) -> Result<(), String> {
        use std::process::Command;
        
        // First, reset all tracked changes
        let reset_output = Command::new("git")
            .current_dir(&self.repo_path)
            .args(["reset", "--hard", "HEAD"])
            .output()
            .map_err(|e| format!("Failed to run git reset: {}", e))?;
        
        if !reset_output.status.success() {
            return Err(format!("git reset failed: {}", String::from_utf8_lossy(&reset_output.stderr)));
        }
        
        // Then clean untracked files
        let clean_output = Command::new("git")
            .current_dir(&self.repo_path)
            .args(["clean", "-fd"])
            .output()
            .map_err(|e| format!("Failed to run git clean: {}", e))?;
        
        if clean_output.status.success() {
            self.refresh_git_status();
            let _ = self.context.refresh();
            Ok(())
        } else {
            Err(format!("git clean failed: {}", String::from_utf8_lossy(&clean_output.stderr)))
        }
    }
}

/// Build a flat file tree for display with sorting
fn build_file_tree(index: &CodebaseIndex, sort_mode: SortMode) -> Vec<FlatTreeEntry> {
    let mut entries: Vec<_> = index.files.iter().collect();
    
    // Sort based on mode
    match sort_mode {
        SortMode::Name => {
            entries.sort_by(|a, b| a.0.cmp(b.0));
        }
        SortMode::Priority => {
            entries.sort_by(|a, b| {
                b.1.suggestion_density()
                    .partial_cmp(&a.1.suggestion_density())
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
        }
        SortMode::Size => {
            entries.sort_by(|a, b| b.1.loc.cmp(&a.1.loc));
        }
        SortMode::Modified => {
            entries.sort_by(|a, b| b.1.last_modified.cmp(&a.1.last_modified));
        }
        SortMode::Complexity => {
            entries.sort_by(|a, b| {
                b.1.complexity
                    .partial_cmp(&a.1.complexity)
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
        }
    }
    
    entries.into_iter().map(|(path, file_index)| {
        let priority = file_index.priority_indicator();
        let depth = path.components().count().saturating_sub(1);
        let name = path.file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("")
            .to_string();
        
        FlatTreeEntry {
            name,
            path: path.clone(),
            is_dir: false,
            depth,
            priority,
        }
    }).collect()
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
            entries.push(GroupedTreeEntry::layer(*layer, group.file_count(), group.expanded));
            
            if group.expanded {
                // Add features first, sorted by file count (largest first)
                let mut sorted_features: Vec<_> = group.features.iter().collect();
                sorted_features.sort_by(|a, b| b.files.len().cmp(&a.files.len()));
                
                for feature in sorted_features {
                    if feature.files.is_empty() {
                        continue;
                    }

                    // Add feature header
                    entries.push(GroupedTreeEntry::feature(&feature.name, feature.files.len(), true));

                    // Sort files: priority files first, then alphabetically
                    let mut sorted_files: Vec<_> = feature.files.iter().collect();
                    sorted_files.sort_by(|a, b| {
                        let pri_a = index.files.get(*a).map(|f| f.priority_indicator()).unwrap_or(' ');
                        let pri_b = index.files.get(*b).map(|f| f.priority_indicator()).unwrap_or(' ');
                        // Priority files (●) come first
                        match (pri_a == '●' || pri_a == '\u{25CF}', pri_b == '●' || pri_b == '\u{25CF}') {
                            (true, false) => std::cmp::Ordering::Less,
                            (false, true) => std::cmp::Ordering::Greater,
                            _ => a.cmp(b),
                        }
                    });

                    // Add files in this feature with contextual names
                    for file_path in sorted_files {
                        let priority = index.files.get(file_path)
                            .map(|f| f.priority_indicator())
                            .unwrap_or(' ');

                        // Use contextual display name for generic files
                        let name = crate::grouping::display_name_with_context(file_path);

                        entries.push(GroupedTreeEntry::file(&name, file_path.clone(), priority, 2));
                    }
                }

                // Add ungrouped files with priority sorting
                let mut sorted_ungrouped: Vec<_> = group.ungrouped_files.iter().collect();
                sorted_ungrouped.sort_by(|a, b| {
                    let pri_a = index.files.get(*a).map(|f| f.priority_indicator()).unwrap_or(' ');
                    let pri_b = index.files.get(*b).map(|f| f.priority_indicator()).unwrap_or(' ');
                    match (pri_a == '●' || pri_a == '\u{25CF}', pri_b == '●' || pri_b == '\u{25CF}') {
                        (true, false) => std::cmp::Ordering::Less,
                        (false, true) => std::cmp::Ordering::Greater,
                        _ => a.cmp(b),
                    }
                });

                for file_path in sorted_ungrouped {
                    let priority = index.files.get(file_path)
                        .map(|f| f.priority_indicator())
                        .unwrap_or(' ');

                    // Use contextual display name
                    let name = crate::grouping::display_name_with_context(file_path);
                    
                    entries.push(GroupedTreeEntry::file(&name, file_path.clone(), priority, 1));
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
            Constraint::Length(3),   // Header (logo)
            Constraint::Min(10),     // Main content
            Constraint::Length(3),   // Footer
        ])
        .split(area);

    render_header(frame, layout[0], app);
    render_main(frame, layout[1], app);
    render_footer(frame, layout[2], app);

    // Loading is now shown inline in the footer status bar (non-blocking)
    // Only show loading overlay for critical operations that need user attention
    if matches!(app.loading, LoadingState::GeneratingFix) {
        render_loading_overlay(frame, &app.loading, app.loading_frame, app.summary_progress);
    }

    // Overlays
    match &app.overlay {
        Overlay::Help => render_help(frame),
        Overlay::SuggestionDetail { suggestion_id, scroll } => {
            if let Some(suggestion) = app.suggestions.suggestions.iter().find(|s| &s.id == suggestion_id) {
                let file_summary = app.get_llm_summary(&suggestion.file);
                let file_index = app.index.files.get(&suggestion.file);
                render_suggestion_detail(frame, suggestion, file_summary, file_index, *scroll);
            }
        }
        Overlay::Inquiry { response, scroll } => {
            render_inquiry(frame, response, *scroll);
        }
        Overlay::ApplyConfirm { diff_preview, scroll, mode, edit_buffer, chat_input, file_path, summary, .. } => {
            render_apply_confirm(frame, diff_preview, *scroll, mode, edit_buffer, chat_input, file_path, summary);
        }
        Overlay::FixPreview { file_path, summary, preview, modifier_input, .. } => {
            render_fix_preview(frame, file_path, summary, preview, modifier_input);
        }
        Overlay::FileDetail { path, scroll } => {
            if let Some(file_index) = app.index.files.get(path) {
                render_file_detail(frame, path, file_index, app.get_llm_summary(path), *scroll);
            }
        }
        Overlay::BranchCreate { branch_name, commit_message, pending_files } => {
            render_branch_dialog(frame, branch_name, commit_message, pending_files);
        }
        Overlay::PRReview { branch_name, files_changed, review_comments, scroll, reviewing, pr_url } => {
            render_pr_review(frame, branch_name, files_changed, review_comments, *scroll, *reviewing, pr_url);
        }
        Overlay::GitStatus { staged, modified, untracked, selected, scroll, commit_input } => {
            render_git_status(frame, staged, modified, untracked, *selected, *scroll, commit_input.as_deref());
        }
        Overlay::None => {}
    }

    // Toast
    if let Some(toast) = &app.toast {
        render_toast(frame, toast);
    }
}

fn render_header(frame: &mut Frame, area: Rect, _app: &App) {
    let lines = vec![
        Line::from(""),
        Line::from(vec![
            Span::styled(
                format!("   {}", Theme::COSMOS_LOGO),
                Style::default().fg(Theme::WHITE).add_modifier(Modifier::BOLD)
            ),
        ]),
    ];

    let header = Paragraph::new(lines).style(Style::default().bg(Theme::BG));
    frame.render_widget(header, area);
}

fn render_main(frame: &mut Frame, area: Rect, app: &App) {
    // Add horizontal padding for breathing room
    let padded = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Length(2),       // Left padding
            Constraint::Min(10),         // Main content
            Constraint::Length(2),       // Right padding
        ])
        .split(area);
    
    // Check if we're in question mode or have a question
    let show_question_box = app.input_mode == InputMode::Question || !app.question_input.is_empty();
    
    // Split vertically: optional question input + main panels
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints(if show_question_box {
            vec![
                Constraint::Length(3),   // Question input box
                Constraint::Min(10),     // Main panels
            ]
        } else {
            vec![
                Constraint::Length(0),   // Hidden question box
                Constraint::Min(10),     // Main panels
            ]
        })
        .split(padded[1]);
    
    // Render question input box when in question mode
    if show_question_box {
        render_question_input(frame, vertical[0], app);
    }
    
    // Split into two panels with gap
    let panels = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(38),  // Project tree
            Constraint::Length(2),       // Gap between panels
            Constraint::Percentage(62),  // Suggestions (wider for wrapped text)
        ])
        .split(vertical[1]);

    render_project_panel(frame, panels[0], app);
    render_suggestions_panel(frame, panels[2], app);
}

fn render_project_panel(frame: &mut Frame, area: Rect, app: &App) {
    let is_active = app.active_panel == ActivePanel::Project;
    let is_searching = app.input_mode == InputMode::Search;
    
    let border_style = if is_searching {
        Style::default().fg(Theme::WHITE)  // Bright border when searching
    } else if is_active {
        Style::default().fg(Theme::GREY_300)  // Bright active border
    } else {
        Style::default().fg(Theme::GREY_600)  // Visible inactive border
    };

    // Account for search bar if searching
    let search_height = if is_searching || !app.search_query.is_empty() { 2 } else { 0 };
    let visible_height = area.height.saturating_sub(4 + search_height as u16) as usize;
    
    let mut lines = vec![];
    
    // Search bar
    if is_searching || !app.search_query.is_empty() {
        let search_text = if is_searching {
            format!(" / {}_", app.search_query)
        } else {
            format!(" / {} (Esc to clear)", app.search_query)
        };
        lines.push(Line::from(vec![
            Span::styled(search_text, Style::default().fg(Theme::WHITE)),
        ]));
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
    let title = format!(" {}{}{}", Theme::SECTION_PROJECT, mode_indicator, scroll_indicator);

    let block = Block::default()
        .title(title)
        .title_style(Style::default().fg(Theme::GREY_200))  // Legible title
        .borders(Borders::ALL)
        .border_style(border_style)
        .style(Style::default().bg(Theme::GREY_800));

    let paragraph = Paragraph::new(lines).block(block);
    frame.render_widget(paragraph, area);
}

/// Render the flat file tree
fn render_flat_tree<'a>(lines: &mut Vec<Line<'a>>, app: &'a App, is_active: bool, visible_height: usize) {
    let tree = &app.filtered_tree;
    let total = tree.len();
    
    for (i, entry) in tree.iter()
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
                if has_more { "│ " } else { "  " }
            })
            .collect();
        
        let (file_icon_str, icon_color) = if entry.is_dir {
            ("▸", Theme::GREY_400)
        } else {
            file_icon(&entry.name)
        };
        
        let name_style = if is_selected {
            Style::default().fg(Theme::WHITE)
        } else if entry.is_dir {
            Style::default().fg(Theme::GREY_300)
        } else if entry.priority == Theme::PRIORITY_HIGH {
            Style::default().fg(Theme::GREY_200)
        } else {
            Style::default().fg(Theme::GREY_500)
        };
        
        let cursor = if is_selected { "›" } else { " " };
        let priority_indicator = if entry.priority == Theme::PRIORITY_HIGH {
            Span::styled(" ●", Style::default().fg(Theme::GREY_300))
        } else {
            Span::styled("", Style::default())
        };
        
        if entry.depth == 0 {
            // Root level - no connector
            lines.push(Line::from(vec![
                Span::styled(format!(" {} ", cursor), Style::default().fg(if is_selected { Theme::WHITE } else { Theme::GREY_600 })),
                Span::styled(format!("{} ", file_icon_str), Style::default().fg(icon_color)),
                Span::styled(entry.name.clone(), name_style),
                priority_indicator,
            ]));
        } else {
            lines.push(Line::from(vec![
                Span::styled(format!(" {} ", cursor), Style::default().fg(if is_selected { Theme::WHITE } else { Theme::GREY_600 })),
                Span::styled(format!("{}{}", indent_str, connector), Style::default().fg(Theme::GREY_700)),
                Span::styled(format!(" {} ", file_icon_str), Style::default().fg(icon_color)),
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
fn render_grouped_tree<'a>(lines: &mut Vec<Line<'a>>, app: &'a App, is_active: bool, visible_height: usize) {
    use crate::grouping::GroupedEntryKind;
    
    let tree = &app.filtered_grouped_tree;
    
    for (i, entry) in tree.iter()
        .enumerate()
        .skip(app.project_scroll)
        .take(visible_height)
    {
        let is_selected = i == app.project_selected && is_active;
        let cursor = if is_selected { "›" } else { " " };
        
        match &entry.kind {
            GroupedEntryKind::Layer(_layer) => {
                // Add spacing before layer (except first)
                if i > 0 && app.project_scroll == 0 || (i > app.project_scroll && app.project_scroll > 0) {
                    // Check if previous visible item was a file - add separator
                    if i > 0 {
                        if let Some(prev) = tree.get(i.saturating_sub(1)) {
                            if prev.kind == GroupedEntryKind::File {
                                lines.push(Line::from(""));
                            }
                        }
                    }
                }
                
                // Layer header - clean and minimal
                let expand_icon = if entry.expanded { "▾" } else { "▸" };
                let count_str = format!(" {}", entry.file_count);
                
                let (name_style, count_style) = if is_selected {
                    (
                        Style::default().fg(Theme::WHITE).add_modifier(Modifier::BOLD),
                        Style::default().fg(Theme::GREY_200),
                    )
                } else {
                    (
                        Style::default().fg(Theme::GREY_100),
                        Style::default().fg(Theme::GREY_600),
                    )
                };
                
                lines.push(Line::from(vec![
                    Span::styled(format!(" {} ", cursor), Style::default().fg(if is_selected { Theme::WHITE } else { Theme::GREY_600 })),
                    Span::styled(expand_icon.to_string(), Style::default().fg(Theme::GREY_500)),
                    Span::styled(format!(" {}", entry.name), name_style),
                    Span::styled(count_str, count_style),
                ]));
            }
            GroupedEntryKind::Feature => {
                // Feature header - subtle folder-like grouping
                let style = if is_selected {
                    Style::default().fg(Theme::WHITE)
                } else {
                    Style::default().fg(Theme::GREY_300)
                };
                
                let count_str = format!(" {}", entry.file_count);
                
                lines.push(Line::from(vec![
                    Span::styled(format!(" {} ", cursor), Style::default().fg(if is_selected { Theme::WHITE } else { Theme::GREY_700 })),
                    Span::styled("   ├─ ", Style::default().fg(Theme::GREY_700)),
                    Span::styled(entry.name.clone(), style),
                    Span::styled(count_str, Style::default().fg(Theme::GREY_600)),
                ]));
            }
            GroupedEntryKind::File => {
                // Simple clean file display with subtle guide
                let (file_icon_str, icon_color) = file_icon(&entry.name);
                
                let name_style = if is_selected {
                    Style::default().fg(Theme::WHITE)
                } else if entry.priority == Theme::PRIORITY_HIGH {
                    Style::default().fg(Theme::GREY_200)
                } else {
                    Style::default().fg(Theme::GREY_500)
                };
                
                let priority_indicator = if entry.priority == Theme::PRIORITY_HIGH {
                    Span::styled(" ●", Style::default().fg(Theme::GREY_400))
                } else {
                    Span::styled("", Style::default())
                };
                
                // Simple indentation with subtle vertical guide
                let indent = "     │  ";
                
                lines.push(Line::from(vec![
                    Span::styled(format!(" {} ", cursor), Style::default().fg(if is_selected { Theme::WHITE } else { Theme::GREY_700 })),
                    Span::styled(indent.to_string(), Style::default().fg(Theme::GREY_800)),
                    Span::styled(format!("{} ", file_icon_str), Style::default().fg(icon_color)),
                    Span::styled(entry.name.clone(), name_style),
                    priority_indicator,
                ]));
            }
        }
    }
}

fn render_suggestions_panel(frame: &mut Frame, area: Rect, app: &App) {
    let is_active = app.active_panel == ActivePanel::Suggestions;
    let border_style = if is_active {
        Style::default().fg(Theme::GREY_300)
    } else {
        Style::default().fg(Theme::GREY_600)
    };

    let visible_height = area.height.saturating_sub(4) as usize;
    let inner_width = area.width.saturating_sub(8) as usize;
    let suggestions = app.suggestions.active_suggestions();
    
    let mut lines = vec![];
    
    if suggestions.is_empty() {
        let is_loading = matches!(app.loading, LoadingState::GeneratingSuggestions);
        
        // Only show the empty state box when not loading
        // (loading status is already shown in the footer)
        if !is_loading {
            lines.push(Line::from(""));
            lines.push(Line::from(""));
            lines.push(Line::from(vec![
                Span::styled("    ╭", Style::default().fg(Theme::GREY_700)),
                Span::styled("──────────────────────────────────", Style::default().fg(Theme::GREY_700)),
                Span::styled("╮", Style::default().fg(Theme::GREY_700)),
            ]));
            lines.push(Line::from(vec![
                Span::styled("    │", Style::default().fg(Theme::GREY_700)),
                Span::styled("                                  ", Style::default()),
                Span::styled("│", Style::default().fg(Theme::GREY_700)),
            ]));
            lines.push(Line::from(vec![
                Span::styled("    │", Style::default().fg(Theme::GREY_700)),
                Span::styled("       ✓ ", Style::default().fg(Theme::GREEN)),
                Span::styled("No issues found", Style::default().fg(Theme::GREY_300)),
                Span::styled("          │", Style::default().fg(Theme::GREY_700)),
            ]));
            lines.push(Line::from(vec![
                Span::styled("    │", Style::default().fg(Theme::GREY_700)),
                Span::styled("         Nothing to suggest", Style::default().fg(Theme::GREY_500)),
                Span::styled("       │", Style::default().fg(Theme::GREY_700)),
            ]));
            lines.push(Line::from(vec![
                Span::styled("    │", Style::default().fg(Theme::GREY_700)),
                Span::styled("                                  ", Style::default()),
                Span::styled("│", Style::default().fg(Theme::GREY_700)),
            ]));
            lines.push(Line::from(vec![
                Span::styled("    ╰", Style::default().fg(Theme::GREY_700)),
                Span::styled("──────────────────────────────────", Style::default().fg(Theme::GREY_700)),
                Span::styled("╯", Style::default().fg(Theme::GREY_700)),
            ]));
            lines.push(Line::from(""));
            lines.push(Line::from(vec![
                Span::styled("    ", Style::default()),
                Span::styled(" r ", Style::default().fg(Theme::GREY_900).bg(Theme::GREY_400)),
                Span::styled(" refresh status", Style::default().fg(Theme::GREY_400)),
            ]));
        }
        // When loading, panel stays empty - footer shows "Generating suggestions"
    } else {
        let mut line_count = 0;
        
        for (i, suggestion) in suggestions.iter().enumerate().skip(app.suggestion_scroll) {
            if line_count >= visible_height.saturating_sub(2) {
                break;
            }
            
            let is_selected = i == app.suggestion_selected && is_active;
            let card_width = inner_width.saturating_sub(4);
            
            // Get badge color based on suggestion kind
            let badge_color = match suggestion.kind {
                crate::suggest::SuggestionKind::Improvement => Theme::BADGE_REFACTOR,
                crate::suggest::SuggestionKind::Quality => Theme::BADGE_QUALITY,
                crate::suggest::SuggestionKind::BugFix => Theme::BADGE_BUG,
                crate::suggest::SuggestionKind::Optimization => Theme::BADGE_PERF,
                crate::suggest::SuggestionKind::Documentation => Theme::BADGE_DOCS,
                crate::suggest::SuggestionKind::Feature => Theme::BADGE_QUALITY,
                crate::suggest::SuggestionKind::Testing => Theme::BADGE_QUALITY,
            };
            
            // Card top border (rounded)
            let card_border_color = if is_selected { Theme::GREY_400 } else { Theme::GREY_700 };
            let card_inner = "─".repeat(card_width);
            lines.push(Line::from(vec![
                Span::styled(if is_selected { " › " } else { "   " }, Style::default().fg(Theme::WHITE)),
                Span::styled("╭", Style::default().fg(card_border_color)),
                Span::styled(card_inner.clone(), Style::default().fg(card_border_color)),
                Span::styled("╮", Style::default().fg(card_border_color)),
            ]));
            line_count += 1;
            
            // File name with badge
            let file_name = suggestion.file.file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("?");
            let kind_label = suggestion.kind.label();
            let file_style = if is_selected {
                Style::default().fg(Theme::WHITE).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Theme::GREY_100)
            };
            
            lines.push(Line::from(vec![
                Span::styled("   │ ", Style::default().fg(card_border_color)),
                Span::styled(format!(" {} ", kind_label), Style::default().fg(Theme::GREY_900).bg(badge_color)),
                Span::styled(" ", Style::default()),
                Span::styled(file_name.to_string(), file_style),
                Span::styled(" ", Style::default()),
                Span::styled(format!("{}", panels::priority_badge(suggestion.priority.icon()).content), 
                    Style::default().fg(match suggestion.priority {
                        Priority::High => Theme::WHITE,
                        Priority::Medium => Theme::GREY_300,
                        Priority::Low => Theme::GREY_500,
                    })),
            ]));
            line_count += 1;
            
            // Summary text (wrapped)
            let summary = &suggestion.summary;
            let text_style = if is_selected {
                Style::default().fg(Theme::GREY_100)
            } else {
                Style::default().fg(Theme::GREY_300)
            };
            
            let wrapped = wrap_text(summary, card_width.saturating_sub(4));
            for wrapped_line in wrapped.iter().take(3) { // Limit to 3 lines for compactness
                if line_count >= visible_height.saturating_sub(2) {
                    break;
                }
                lines.push(Line::from(vec![
                    Span::styled("   │ ", Style::default().fg(card_border_color)),
                    Span::styled(format!(" {}", wrapped_line), text_style),
                ]));
                line_count += 1;
            }
            if wrapped.len() > 3 && line_count < visible_height.saturating_sub(2) {
                lines.push(Line::from(vec![
                    Span::styled("   │ ", Style::default().fg(card_border_color)),
                    Span::styled(" ...", Style::default().fg(Theme::GREY_500)),
                    Span::styled("  ↵ for more", Style::default().fg(Theme::GREY_600)),
                ]));
                line_count += 1;
            }
            
            // Action buttons (only for selected)
            if is_selected && line_count < visible_height.saturating_sub(2) {
                lines.push(Line::from(vec![
                    Span::styled("   │ ", Style::default().fg(card_border_color)),
                ]));
                line_count += 1;
                
                lines.push(Line::from(vec![
                    Span::styled("   │ ", Style::default().fg(card_border_color)),
                    Span::styled(" a ", Style::default().fg(Theme::GREY_900).bg(Theme::GREEN).add_modifier(Modifier::BOLD)),
                    Span::styled(" Fix ", Style::default().fg(Theme::GREEN)),
                    Span::styled("  ", Style::default()),
                    Span::styled(" ↵ ", Style::default().fg(Theme::GREY_900).bg(Theme::GREY_300)),
                    Span::styled(" Details ", Style::default().fg(Theme::GREY_300)),
                    Span::styled("  ", Style::default()),
                    Span::styled(" d ", Style::default().fg(Theme::GREY_900).bg(Theme::GREY_500)),
                    Span::styled(" Skip ", Style::default().fg(Theme::GREY_500)),
                ]));
                line_count += 1;
            }
            
            // Card bottom border
            lines.push(Line::from(vec![
                Span::styled("   ╰", Style::default().fg(card_border_color)),
                Span::styled(card_inner.clone(), Style::default().fg(card_border_color)),
                Span::styled("╯", Style::default().fg(card_border_color)),
            ]));
            line_count += 1;
            
            // Spacing between cards
            if line_count < visible_height.saturating_sub(2) {
                lines.push(Line::from(""));
                line_count += 1;
            }
        }
    }

    let counts = app.suggestions.counts();
    let scroll_indicator = if suggestions.len() > 3 {
        let total = suggestions.len();
        let current = app.suggestion_selected + 1;  // Show selected item, not scroll offset
        format!(" ↕ {}/{}", current, total)
    } else {
        String::new()
    };
    
    let title = if counts.total > 0 {
        format!(" {} · {}{} ", Theme::SECTION_SUGGESTIONS, counts.total, scroll_indicator)
    } else {
        format!(" {} ", Theme::SECTION_SUGGESTIONS)
    };

    let block = Block::default()
        .title(title)
        .title_style(Style::default().fg(Theme::GREY_200))
        .borders(Borders::ALL)
        .border_style(border_style)
        .style(Style::default().bg(Theme::GREY_800));

    let paragraph = Paragraph::new(lines)
        .block(block)
        .wrap(Wrap { trim: false });
    frame.render_widget(paragraph, area);
}

fn render_question_input(frame: &mut Frame, area: Rect, app: &App) {
    let is_active = app.input_mode == InputMode::Question;
    
    let border_style = if is_active {
        Style::default().fg(Theme::WHITE)
    } else {
        Style::default().fg(Theme::GREY_500)
    };
    
    // Build the input display
    let prompt = "> Ask cosmos: ";
    let cursor = if is_active { "█" } else { "" };
    
    let spans = vec![
        Span::styled(prompt, Style::default().fg(Theme::GREY_300)),
        Span::styled(&app.question_input, Style::default().fg(Theme::WHITE)),
        Span::styled(cursor, Style::default().fg(Theme::WHITE)),
    ];
    
    let hint = if is_active {
        if app.question_input.is_empty() {
            "  (Enter to ask, Esc to cancel)"
        } else {
            ""
        }
    } else {
        "  (press 'i' to ask a question)"
    };
    
    let mut full_spans = spans;
    full_spans.push(Span::styled(hint, Style::default().fg(Theme::GREY_500)));
    
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(border_style)
        .style(Style::default().bg(Theme::GREY_800));
    
    let paragraph = Paragraph::new(Line::from(full_spans))
        .block(block);
    
    frame.render_widget(paragraph, area);
}

fn render_footer(frame: &mut Frame, area: Rect, app: &App) {
    // Top line - subtle separator
    let separator = Line::from(vec![
        Span::styled(
            "─".repeat(area.width as usize),
            Style::default().fg(Theme::GREY_600)
        ),
    ]);

    // Bottom line - status and action buttons
    let mut spans = vec![
        Span::styled("  ", Style::default()),
    ];
    
    // Loading indicator (non-blocking, inline in status bar)
    if app.loading.is_loading() {
        let spinner = SPINNER_FRAMES[app.loading_frame % SPINNER_FRAMES.len()];
        spans.push(Span::styled(
            format!("{} ", spinner),
            Style::default().fg(Theme::WHITE).add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::styled(
            format!("{} ", app.loading.message()),
            Style::default().fg(Theme::GREY_200),
        ));
        spans.push(Span::styled("│ ", Style::default().fg(Theme::GREY_600)));
    }
    
    // Project name and branch with icon
    let project_name = app.context.repo_root
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("unknown");
    spans.push(Span::styled(project_name, Style::default().fg(Theme::GREY_400)));
    spans.push(Span::styled(" ⎇ ", Style::default().fg(Theme::GREY_500)));
    spans.push(Span::styled(&app.context.branch, Style::default().fg(Theme::GREY_100)));

    // Show pending changes count with prominent action hints
    let pending_count = app.pending_change_count();
    if pending_count > 0 {
        spans.push(Span::styled("  │  ", Style::default().fg(Theme::GREY_600)));
        spans.push(Span::styled(
            format!("● {} pending ", pending_count),
            Style::default().fg(Theme::GREEN).add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::styled(" b ", Style::default().fg(Theme::GREY_900).bg(Theme::GREY_300)));
        spans.push(Span::styled(" branch ", Style::default().fg(Theme::GREY_400)));
        spans.push(Span::styled(" p ", Style::default().fg(Theme::GREY_900).bg(Theme::GREY_300)));
        spans.push(Span::styled(" PR", Style::default().fg(Theme::GREY_400)));
    } else if app.context.has_changes() {
        spans.push(Span::styled("  │  ", Style::default().fg(Theme::GREY_600)));
        spans.push(Span::styled(" c ", Style::default().fg(Theme::GREY_900).bg(Theme::GREY_300)));
        spans.push(Span::styled(
            format!(" {} changed ", app.context.modified_count),
            Style::default().fg(Theme::GREY_200),
        ));
    }

    // Model indicator with icon
    if let Some(model) = &app.active_model {
        spans.push(Span::styled("  │  ", Style::default().fg(Theme::GREY_600)));
        spans.push(Span::styled("⚙ ", Style::default().fg(Theme::GREY_500)));
        spans.push(Span::styled(
            model.clone(),
            Style::default().fg(Theme::GREY_300),
        ));
    }

    // Cost meter (show if any cost has been incurred)
    if app.session_cost > 0.0 {
        spans.push(Span::styled("  ", Style::default()));
        spans.push(Span::styled(
            format!("${:.4}", app.session_cost),
            Style::default().fg(Theme::GREY_400),
        ));
    }

    // Spacer before buttons
    let status_len: usize = spans.iter().map(|s| s.content.chars().count()).sum();
    let available = area.width as usize;
    let button_area_approx = 50; // Approximate width needed for buttons
    let spacer_len = available.saturating_sub(status_len + button_area_approx);
    if spacer_len > 0 {
        spans.push(Span::styled(" ".repeat(spacer_len), Style::default()));
    }

    // Action buttons - badge style
    spans.push(Span::styled(" i ", Style::default().fg(Theme::GREY_900).bg(Theme::GREY_200)));
    spans.push(Span::styled(" ask ", Style::default().fg(Theme::GREY_300)));
    
    spans.push(Span::styled(" g ", Style::default().fg(Theme::GREY_900).bg(Theme::GREY_400)));
    spans.push(Span::styled(" group ", Style::default().fg(Theme::GREY_400)));
    
    spans.push(Span::styled(" / ", Style::default().fg(Theme::GREY_900).bg(Theme::GREY_400)));
    spans.push(Span::styled(" search ", Style::default().fg(Theme::GREY_400)));
    
    spans.push(Span::styled(" ? ", Style::default().fg(Theme::GREY_900).bg(Theme::GREY_400)));
    spans.push(Span::styled(" help ", Style::default().fg(Theme::GREY_400)));
    
    spans.push(Span::styled(" q ", Style::default().fg(Theme::GREY_900).bg(Theme::GREY_500)));
    spans.push(Span::styled(" quit ", Style::default().fg(Theme::GREY_500)));
    
    spans.push(Span::styled(" ", Style::default()));

    let footer_line = Line::from(spans);

    let footer = Paragraph::new(vec![separator, footer_line])
        .style(Style::default().bg(Theme::GREY_900));
    frame.render_widget(footer, area);
}

// ═══════════════════════════════════════════════════════════════════════════
//  OVERLAYS
// ═══════════════════════════════════════════════════════════════════════════

fn render_help(frame: &mut Frame) {
    let area = centered_rect(55, 80, frame.area());
    frame.render_widget(Clear, area);

    // Helper functions that return owned data
    fn section_start(title: &str) -> Vec<Line<'static>> {
        vec![
            Line::from(""),
            Line::from(vec![
                Span::styled("    ╭─ ".to_string(), Style::default().fg(Theme::GREY_600)),
                Span::styled(title.to_string(), Style::default().fg(Theme::WHITE).add_modifier(Modifier::BOLD)),
                Span::styled(" ─────────────────────────╮".to_string(), Style::default().fg(Theme::GREY_600)),
            ]),
        ]
    }
    
    fn key_row(key: &str, desc: &str) -> Line<'static> {
        Line::from(vec![
            Span::styled("    │  ".to_string(), Style::default().fg(Theme::GREY_600)),
            Span::styled(format!(" {} ", key), Style::default().fg(Theme::GREY_900).bg(Theme::GREY_300)),
            Span::styled(format!("  {}", desc), Style::default().fg(Theme::GREY_200)),
        ])
    }
    
    fn section_end() -> Line<'static> {
        Line::from(vec![
            Span::styled("    ╰─────────────────────────────────────╯".to_string(), Style::default().fg(Theme::GREY_600)),
        ])
    }
    
    fn section_spacer() -> Line<'static> {
        Line::from(vec![
            Span::styled("    │".to_string(), Style::default().fg(Theme::GREY_600)),
        ])
    }

    let mut help_text: Vec<Line<'static>> = vec![
        Line::from(""),
    ];
    
    // Navigation section
    help_text.extend(section_start("Navigation"));
    help_text.push(section_spacer());
    help_text.push(key_row("↑↓ / jk", "Move up/down"));
    help_text.push(key_row("Tab", "Switch between panels"));
    help_text.push(key_row("Enter", "View details / select"));
    help_text.push(key_row("PgUp/Dn", "Page scroll"));
    help_text.push(section_spacer());
    help_text.push(section_end());
    
    // File Explorer section
    help_text.extend(section_start("File Explorer"));
    help_text.push(section_spacer());
    help_text.push(key_row("/", "Search files"));
    help_text.push(key_row("g", "Toggle grouped/flat view"));
    help_text.push(key_row("Space", "Expand/collapse section"));
    help_text.push(key_row("C / E", "Collapse/Expand all"));
    help_text.push(key_row("1-8", "Jump to layer"));
    help_text.push(key_row("Esc", "Clear search"));
    help_text.push(section_spacer());
    help_text.push(section_end());
    
    // Actions section
    help_text.extend(section_start("Actions"));
    help_text.push(section_spacer());
    help_text.push(key_row("i", "Ask cosmos a question"));
    help_text.push(key_row("a", "Apply/fix suggestion"));
    help_text.push(key_row("d", "Dismiss suggestion"));
    help_text.push(key_row("r", "Refresh status"));
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
        Span::styled(" Esc ".to_string(), Style::default().fg(Theme::GREY_900).bg(Theme::GREY_400)),
        Span::styled(" close help".to_string(), Style::default().fg(Theme::GREY_400)),
    ]));
    help_text.push(Line::from(""));

    let block = Paragraph::new(help_text)
        .block(Block::default()
            .title(" › 𝘩𝘦𝘭𝘱 ")
            .title_style(Style::default().fg(Theme::GREY_100))
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Theme::GREY_400))
            .style(Style::default().bg(Theme::GREY_900)));
    
    frame.render_widget(block, area);
}

fn render_suggestion_detail(
    frame: &mut Frame, 
    suggestion: &Suggestion, 
    file_summary: Option<&String>,
    file_index: Option<&crate::index::FileIndex>,
    scroll: usize
) {
    let area = centered_rect(75, 80, frame.area());
    frame.render_widget(Clear, area);

    let inner_width = area.width.saturating_sub(10) as usize;
    // Reserve space for: header (8 lines) + footer (3 lines) + borders (2 lines)
    let visible_height = area.height.saturating_sub(15) as usize;
    
    // Get badge color based on suggestion kind
    let badge_color = match suggestion.kind {
        crate::suggest::SuggestionKind::Improvement => Theme::BADGE_REFACTOR,
        crate::suggest::SuggestionKind::Quality => Theme::BADGE_QUALITY,
        crate::suggest::SuggestionKind::BugFix => Theme::BADGE_BUG,
        crate::suggest::SuggestionKind::Optimization => Theme::BADGE_PERF,
        crate::suggest::SuggestionKind::Documentation => Theme::BADGE_DOCS,
        crate::suggest::SuggestionKind::Feature => Theme::BADGE_QUALITY,
        crate::suggest::SuggestionKind::Testing => Theme::BADGE_QUALITY,
    };
    
    let priority_style = match suggestion.priority {
        Priority::High => Style::default().fg(Theme::WHITE).add_modifier(Modifier::BOLD),
        Priority::Medium => Style::default().fg(Theme::GREY_200),
        Priority::Low => Style::default().fg(Theme::GREY_400),
    };
    
    let file_name = suggestion.file.file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("?");
    
    // === FIXED HEADER ===
    let mut lines = vec![
        Line::from(""),
        // Header with badge and priority
        Line::from(vec![
            Span::styled("    ", Style::default()),
            Span::styled(format!(" {} ", suggestion.kind.label()), 
                Style::default().fg(Theme::GREY_900).bg(badge_color)),
            Span::styled("  ", Style::default()),
            Span::styled(format!("{} ", suggestion.priority.icon()), priority_style),
            Span::styled(
                match suggestion.priority {
                    Priority::High => "High Priority",
                    Priority::Medium => "Medium",
                    Priority::Low => "Low",
                },
                priority_style
            ),
        ]),
        Line::from(""),
        // File info
        Line::from(vec![
            Span::styled("    ", Style::default()),
            Span::styled(file_name.to_string(), Style::default().fg(Theme::WHITE).add_modifier(Modifier::BOLD)),
        ]),
        Line::from(vec![
            Span::styled(format!("       {}", suggestion.file.display()), 
                Style::default().fg(Theme::GREY_500)),
        ]),
    ];
    
    // File metrics bar (if we have file index data)
    if let Some(fi) = file_index {
        let func_count = fi.symbols.iter()
            .filter(|s| matches!(s.kind, crate::index::SymbolKind::Function | crate::index::SymbolKind::Method))
            .count();
        
        lines.push(Line::from(vec![
            Span::styled("       ", Style::default()),
            Span::styled(format!("{}", fi.loc), Style::default().fg(Theme::GREY_400)),
            Span::styled(" LOC  ", Style::default().fg(Theme::GREY_600)),
            Span::styled(format!("{}", func_count), Style::default().fg(Theme::GREY_400)),
            Span::styled(" funcs  ", Style::default().fg(Theme::GREY_600)),
            Span::styled(format!("{:.0}", fi.complexity), Style::default().fg(Theme::GREY_400)),
            Span::styled(" complexity", Style::default().fg(Theme::GREY_600)),
        ]));
    }
    
    lines.push(Line::from(""));
    
    // === BUILD SCROLLABLE CONTENT ===
    let mut scrollable_content: Vec<Line<'static>> = Vec::new();
    
    // File Summary card (what this file is about)
    scrollable_content.push(Line::from(vec![
        Span::styled("    ╭─ ", Style::default().fg(Theme::GREY_600)),
        Span::styled("File Summary", Style::default().fg(Theme::GREY_300)),
        Span::styled(" ─".to_string() + &"─".repeat(inner_width.saturating_sub(19)) + "╮", Style::default().fg(Theme::GREY_600)),
    ]));
    scrollable_content.push(Line::from(vec![
        Span::styled("    │", Style::default().fg(Theme::GREY_600)),
    ]));
    
    // Show file summary if available, otherwise show a placeholder
    let summary_text = if let Some(summary) = file_summary {
        summary.clone()
    } else if let Some(fi) = file_index {
        fi.summary.purpose.clone()
    } else {
        "No summary available for this file.".to_string()
    };
    
    // Full file summary - no truncation
    let summary_wrapped = wrap_text(&summary_text, inner_width.saturating_sub(6));
    for wrapped_line in &summary_wrapped {
        scrollable_content.push(Line::from(vec![
            Span::styled("    │  ", Style::default().fg(Theme::GREY_600)),
            Span::styled(wrapped_line.to_string(), Style::default().fg(Theme::GREY_200)),
        ]));
    }
    
    scrollable_content.push(Line::from(vec![
        Span::styled("    │", Style::default().fg(Theme::GREY_600)),
    ]));
    scrollable_content.push(Line::from(vec![
        Span::styled("    ╰".to_string() + &"─".repeat(inner_width.saturating_sub(4)) + "╯", Style::default().fg(Theme::GREY_600)),
    ]));
    
    // Line info if available
    if let Some(line) = suggestion.line {
        scrollable_content.push(Line::from(""));
        scrollable_content.push(Line::from(vec![
            Span::styled(format!("    Line {}", line), 
                Style::default().fg(Theme::GREY_300)),
        ]));
    }

    // Fix Details section - what the suggestion is about
    scrollable_content.push(Line::from(""));
    scrollable_content.push(Line::from(vec![
        Span::styled("    ╭─ ", Style::default().fg(Theme::GREY_600)),
        Span::styled("Fix Details", Style::default().fg(Theme::GREY_300)),
        Span::styled(" ─".to_string() + &"─".repeat(inner_width.saturating_sub(18)) + "╮", Style::default().fg(Theme::GREY_600)),
    ]));
    scrollable_content.push(Line::from(vec![
        Span::styled("    │", Style::default().fg(Theme::GREY_600)),
    ]));
    
    // Issue summary (what's wrong)
    let issue_wrapped = wrap_text(&suggestion.summary, inner_width.saturating_sub(6));
    for wrapped_line in &issue_wrapped {
        scrollable_content.push(Line::from(vec![
            Span::styled("    │  ", Style::default().fg(Theme::GREY_600)),
            Span::styled(wrapped_line.to_string(), Style::default().fg(Theme::GREY_50)),
        ]));
    }
    
    // Detail explanation (if any)
    if let Some(detail) = &suggestion.detail {
        scrollable_content.push(Line::from(vec![
            Span::styled("    │", Style::default().fg(Theme::GREY_600)),
        ]));

        // Parse markdown and render with styling
        let parsed_lines = markdown::parse_markdown(detail, inner_width.saturating_sub(8));
        
        // Add padding to each line
        for line in parsed_lines {
            let mut spans = vec![
                Span::styled("    │  ", Style::default().fg(Theme::GREY_600)),
            ];
            spans.extend(line.spans);
            scrollable_content.push(Line::from(spans));
        }
    }
    
    scrollable_content.push(Line::from(vec![
        Span::styled("    │", Style::default().fg(Theme::GREY_600)),
    ]));
    scrollable_content.push(Line::from(vec![
        Span::styled("    ╰".to_string() + &"─".repeat(inner_width.saturating_sub(4)) + "╯", Style::default().fg(Theme::GREY_600)),
    ]));
    
    // === APPLY SCROLL TO CONTENT ===
    let total_content_lines = scrollable_content.len();
    let needs_scroll = total_content_lines > visible_height;
    
    for line in scrollable_content.into_iter().skip(scroll).take(visible_height) {
        lines.push(line);
    }
    
    // Scroll indicator
    if needs_scroll {
        lines.push(Line::from(vec![
            Span::styled("    ", Style::default()),
            Span::styled(
                format!("↕ {}/{}", scroll + 1, total_content_lines.saturating_sub(visible_height) + 1),
                Style::default().fg(Theme::GREY_500)
            ),
        ]));
    } else {
        lines.push(Line::from(""));
    }

    lines.push(Line::from(""));
    
    // Action buttons
    lines.push(Line::from(vec![
        Span::styled("    ", Style::default()),
        Span::styled(" a ", Style::default().fg(Theme::GREY_900).bg(Theme::GREEN).add_modifier(Modifier::BOLD)),
        Span::styled(" Apply Fix ", Style::default().fg(Theme::GREEN)),
        Span::styled("  ", Style::default()),
        Span::styled(" d ", Style::default().fg(Theme::GREY_900).bg(Theme::GREY_400)),
        Span::styled(" Dismiss ", Style::default().fg(Theme::GREY_400)),
        Span::styled("  ", Style::default()),
        Span::styled(" Esc ", Style::default().fg(Theme::GREY_900).bg(Theme::GREY_500)),
        Span::styled(" Close ", Style::default().fg(Theme::GREY_500)),
    ]));
    lines.push(Line::from(""));

    let block = Paragraph::new(lines)
        .wrap(Wrap { trim: false })
        .block(Block::default()
            .title(" › 𝘥𝘦𝘵𝘢𝘪𝘭 ")
            .title_style(Style::default().fg(Theme::GREY_100))
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Theme::GREY_400))
            .style(Style::default().bg(Theme::GREY_900)));
    
    frame.render_widget(block, area);
}

fn render_inquiry(frame: &mut Frame, response: &str, scroll: usize) {
    let area = centered_rect(80, 85, frame.area());
    frame.render_widget(Clear, area);

    let visible_height = area.height.saturating_sub(12) as usize;
    let inner_width = area.width.saturating_sub(12) as usize;

    let mut lines = vec![
        Line::from(""),
        Line::from(vec![
            Span::styled("    ", Style::default()),
            Span::styled(" › ", Style::default().fg(Theme::GREY_900).bg(Theme::WHITE).add_modifier(Modifier::BOLD)),
            Span::styled("  𝘤𝘰𝘴𝘮𝘰𝘴 𝘳𝘦𝘴𝘱𝘰𝘯𝘥𝘴...", Style::default().fg(Theme::GREY_200).add_modifier(Modifier::ITALIC)),
        ]),
        Line::from(""),
    ];
    
    // Response card
    lines.push(Line::from(vec![
        Span::styled("    ╭", Style::default().fg(Theme::GREY_600)),
        Span::styled("─".repeat(inner_width.saturating_sub(2)), Style::default().fg(Theme::GREY_600)),
        Span::styled("╮", Style::default().fg(Theme::GREY_600)),
    ]));
    lines.push(Line::from(vec![
        Span::styled("    │", Style::default().fg(Theme::GREY_600)),
    ]));

    // Parse markdown and render with styling
    let parsed_lines = markdown::parse_markdown(response, inner_width.saturating_sub(6));
    
    // Add padding to each line
    let padded_lines: Vec<Line<'static>> = parsed_lines.into_iter()
        .map(|line| {
            let mut spans = vec![
                Span::styled("    │  ", Style::default().fg(Theme::GREY_600)),
            ];
            spans.extend(line.spans);
            Line::from(spans)
        })
        .collect();

    for line in padded_lines.iter().skip(scroll).take(visible_height) {
        lines.push(line.clone());
    }
    
    let total_lines = padded_lines.len();
    
    // Scroll indicator
    if total_lines > visible_height {
        lines.push(Line::from(vec![
            Span::styled("    │", Style::default().fg(Theme::GREY_600)),
        ]));
        lines.push(Line::from(vec![
            Span::styled("    │  ", Style::default().fg(Theme::GREY_600)),
            Span::styled(
                format!("↕ {}/{}", scroll + 1, total_lines.saturating_sub(visible_height) + 1),
                Style::default().fg(Theme::GREY_500)
            ),
        ]));
    }
    
    lines.push(Line::from(vec![
        Span::styled("    │", Style::default().fg(Theme::GREY_600)),
    ]));
    lines.push(Line::from(vec![
        Span::styled("    ╰", Style::default().fg(Theme::GREY_600)),
        Span::styled("─".repeat(inner_width.saturating_sub(2)), Style::default().fg(Theme::GREY_600)),
        Span::styled("╯", Style::default().fg(Theme::GREY_600)),
    ]));

    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        Span::styled("    ", Style::default()),
        Span::styled(" ↑↓ ", Style::default().fg(Theme::GREY_900).bg(Theme::GREY_400)),
        Span::styled(" scroll ", Style::default().fg(Theme::GREY_400)),
        Span::styled("  ", Style::default()),
        Span::styled(" i ", Style::default().fg(Theme::GREY_900).bg(Theme::GREY_300)),
        Span::styled(" ask another ", Style::default().fg(Theme::GREY_300)),
        Span::styled("  ", Style::default()),
        Span::styled(" Esc ", Style::default().fg(Theme::GREY_900).bg(Theme::GREY_500)),
        Span::styled(" close ", Style::default().fg(Theme::GREY_500)),
    ]));
    lines.push(Line::from(""));

    let block = Paragraph::new(lines)
        .wrap(Wrap { trim: false })
        .block(Block::default()
            .title(" › 𝘪𝘯𝘲𝘶𝘪𝘳𝘺 ")
            .title_style(Style::default().fg(Theme::GREY_100))
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Theme::GREY_400))
            .style(Style::default().bg(Theme::GREY_900)));
    
    frame.render_widget(block, area);
}

fn render_fix_preview(
    frame: &mut Frame,
    file_path: &PathBuf,
    _summary: &str,
    preview: &crate::suggest::llm::FixPreview,
    modifier_input: &str,
) {
    let area = centered_rect(60, 50, frame.area());
    frame.render_widget(Clear, area);

    let inner_width = area.width.saturating_sub(12) as usize;
    
    let file_name = file_path.file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("unknown");

    let mut lines = vec![
        Line::from(""),
        Line::from(vec![
            Span::styled("     › ", Style::default().fg(Theme::WHITE)),
            Span::styled("Quick Preview", Style::default().fg(Theme::WHITE).add_modifier(Modifier::BOLD)),
        ]),
        Line::from(""),
        Line::from(vec![
            Span::styled(format!("     {} ", file_name), Style::default().fg(Theme::GREY_100)),
            Span::styled(format!("{}  {}", preview.scope.icon(), preview.scope.label()), 
                Style::default().fg(Theme::GREY_400)),
        ]),
        Line::from(""),
        Line::from(vec![
            Span::styled("     ─────────────────────────────────────────", Style::default().fg(Theme::GREY_600))
        ]),
        Line::from(""),
    ];

    // Wrap the description
    let desc_wrapped = wrap_text(&preview.description, inner_width.saturating_sub(10));
    for wrapped_line in &desc_wrapped {
        lines.push(Line::from(vec![
            Span::styled(format!("     {}", wrapped_line), Style::default().fg(Theme::GREY_50)),
        ]));
    }

    // Affected areas
    if !preview.affected_areas.is_empty() {
        lines.push(Line::from(""));
        let areas_str = preview.affected_areas.join(", ");
        let areas_wrapped = wrap_text(&format!("Affects: {}", areas_str), inner_width.saturating_sub(10));
        for wrapped_line in &areas_wrapped {
            lines.push(Line::from(vec![
                Span::styled(format!("     {}", wrapped_line), Style::default().fg(Theme::GREY_300)),
            ]));
        }
    }

    // Modifier input (if user is typing)
    if !modifier_input.is_empty() {
        lines.push(Line::from(""));
        lines.push(Line::from(vec![
            Span::styled("     ─────────────────────────────────────────", Style::default().fg(Theme::GREY_600))
        ]));
        lines.push(Line::from(""));
        lines.push(Line::from(vec![
            Span::styled("     Your request: ", Style::default().fg(Theme::GREY_400)),
            Span::styled(modifier_input, Style::default().fg(Theme::WHITE)),
            Span::styled("_", Style::default().fg(Theme::WHITE).add_modifier(Modifier::SLOW_BLINK)),
        ]));
    }

    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        Span::styled("     ─────────────────────────────────────────", Style::default().fg(Theme::GREY_600))
    ]));
    
    // Key hints - make them IMPOSSIBLE TO MISS
    if modifier_input.is_empty() {
        lines.push(Line::from(""));
        lines.push(Line::from(vec![
            Span::styled("     ▶ ", Style::default().fg(Theme::WHITE).add_modifier(Modifier::BOLD)),
            Span::styled("Press ", Style::default().fg(Theme::WHITE)),
            Span::styled(" Y ", Style::default().fg(Theme::GREY_900).bg(Theme::GREEN).add_modifier(Modifier::BOLD)),
            Span::styled(" to apply this fix now", Style::default().fg(Theme::WHITE).add_modifier(Modifier::BOLD)),
        ]));
        lines.push(Line::from(""));
        lines.push(Line::from(vec![
            Span::styled("       ", Style::default()),
            Span::styled("d", Style::default().fg(Theme::GREY_300)),
            Span::styled(" diff    ", Style::default().fg(Theme::GREY_500)),
            Span::styled("m", Style::default().fg(Theme::GREY_300)),
            Span::styled(" tweak    ", Style::default().fg(Theme::GREY_500)),
            Span::styled("Esc", Style::default().fg(Theme::GREY_300)),
            Span::styled(" cancel", Style::default().fg(Theme::GREY_500)),
        ]));
    } else {
        lines.push(Line::from(""));
        lines.push(Line::from(vec![
            Span::styled("     ▶ ", Style::default().fg(Theme::WHITE).add_modifier(Modifier::BOLD)),
            Span::styled("Press ", Style::default().fg(Theme::WHITE)),
            Span::styled(" Enter ", Style::default().fg(Theme::GREY_900).bg(Theme::GREEN).add_modifier(Modifier::BOLD)),
            Span::styled(" to regenerate", Style::default().fg(Theme::WHITE).add_modifier(Modifier::BOLD)),
        ]));
        lines.push(Line::from(""));
        lines.push(Line::from(vec![
            Span::styled("       Esc", Style::default().fg(Theme::GREY_300)),
            Span::styled(" cancel", Style::default().fg(Theme::GREY_500)),
        ]));
    }
    lines.push(Line::from(""));

    let block = Paragraph::new(lines)
        .wrap(Wrap { trim: false })
        .block(Block::default()
            .title(" › 𝘱𝘳𝘦𝘷𝘪𝘦𝘸 ")
            .title_style(Style::default().fg(Theme::GREY_100))
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Theme::GREY_400))
            .style(Style::default().bg(Theme::GREY_900)));

    frame.render_widget(block, area);
}

fn render_apply_confirm(
    frame: &mut Frame, 
    diff_preview: &str, 
    scroll: usize,
    mode: &ApplyMode,
    edit_buffer: &Option<String>,
    chat_input: &str,
    file_path: &PathBuf,
    summary: &str,
) {
    let area = centered_rect(85, 85, frame.area());
    frame.render_widget(Clear, area);

    let visible_height = area.height.saturating_sub(16) as usize;
    let inner_width = area.width.saturating_sub(12) as usize;
    
    // File info header
    let file_name = file_path.file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("unknown");
    
    let mut lines = vec![
        Line::from(""),
        Line::from(""),
        Line::from(vec![
            Span::styled("     › ", Style::default().fg(Theme::WHITE)),
            Span::styled(file_name, Style::default().fg(Theme::WHITE).add_modifier(Modifier::BOLD)),
        ]),
        Line::from(vec![
            Span::styled(format!("     {}", file_path.display()), Style::default().fg(Theme::GREY_400)),
        ]),
        Line::from(""),
    ];
    
    // Summary - wrapped
    let summary_wrapped = wrap_text(summary, inner_width.saturating_sub(10));
    for wrapped_line in &summary_wrapped {
        lines.push(Line::from(vec![
            Span::styled(format!("     {}", wrapped_line), Style::default().fg(Theme::GREY_200).add_modifier(Modifier::ITALIC)),
        ]));
    }
    
    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        Span::styled("     ─────────────────────────────────────────────────────", Style::default().fg(Theme::GREY_600))
    ]));
    lines.push(Line::from(""));

    // Determine what to display based on mode
    let display_content = match mode {
        ApplyMode::Edit => edit_buffer.as_deref().unwrap_or(diff_preview),
        _ => diff_preview,
    };

    let diff_lines: Vec<&str> = display_content.lines().collect();
    
    for line in diff_lines.iter().skip(scroll).take(visible_height) {
        let style = if line.starts_with('+') && !line.starts_with("+++") {
            Style::default().fg(Theme::GREEN)
        } else if line.starts_with('-') && !line.starts_with("---") {
            Style::default().fg(Theme::RED)
        } else if line.starts_with("@@") {
            Style::default().fg(Theme::GREY_400).add_modifier(Modifier::ITALIC)
        } else if line.starts_with("+++") || line.starts_with("---") {
            Style::default().fg(Theme::GREY_300)
        } else {
            Style::default().fg(Theme::GREY_200)
        };
        
        lines.push(Line::from(vec![
            Span::styled(format!("     {}", line), style),
        ]));
    }
    
    // Scroll indicator
    if diff_lines.len() > visible_height {
        lines.push(Line::from(""));
        lines.push(Line::from(vec![
            Span::styled(
                format!("     ↕ {}/{} ", scroll + 1, diff_lines.len().saturating_sub(visible_height) + 1), 
                Style::default().fg(Theme::GREY_400)
            ),
        ]));
    }

    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        Span::styled("     ─────────────────────────────────────────────────────", Style::default().fg(Theme::GREY_600))
    ]));
    lines.push(Line::from(""));
    
    // Mode-specific footer
    match mode {
        ApplyMode::View => {
            lines.push(Line::from(vec![
                Span::styled("     𝘺", Style::default().fg(Theme::WHITE)),
                Span::styled(" apply   ", Style::default().fg(Theme::GREY_400)),
                Span::styled("𝘦", Style::default().fg(Theme::WHITE)),
                Span::styled(" edit   ", Style::default().fg(Theme::GREY_400)),
                Span::styled("𝘤", Style::default().fg(Theme::WHITE)),
                Span::styled(" chat   ", Style::default().fg(Theme::GREY_400)),
                Span::styled("Esc", Style::default().fg(Theme::WHITE)),
                Span::styled(" cancel   ", Style::default().fg(Theme::GREY_400)),
                Span::styled("↑↓", Style::default().fg(Theme::WHITE)),
                Span::styled(" scroll", Style::default().fg(Theme::GREY_400)),
            ]));
        }
        ApplyMode::Edit => {
            lines.push(Line::from(vec![
                Span::styled("     [EDIT MODE] ", Style::default().fg(Theme::WHITE).add_modifier(Modifier::BOLD)),
                Span::styled("type to modify · ", Style::default().fg(Theme::GREY_300)),
                Span::styled("F2", Style::default().fg(Theme::WHITE)),
                Span::styled(" save   ", Style::default().fg(Theme::GREY_400)),
                Span::styled("Esc", Style::default().fg(Theme::WHITE)),
                Span::styled(" cancel   ", Style::default().fg(Theme::GREY_400)),
                Span::styled("↑↓", Style::default().fg(Theme::WHITE)),
                Span::styled(" scroll", Style::default().fg(Theme::GREY_400)),
            ]));
        }
        ApplyMode::Chat => {
            // Show chat input field
            lines.push(Line::from(vec![
                Span::styled("     › ", Style::default().fg(Theme::WHITE)),
                Span::styled(chat_input, Style::default().fg(Theme::GREY_100)),
                Span::styled("_", Style::default().fg(Theme::WHITE).add_modifier(Modifier::SLOW_BLINK)),
            ]));
            lines.push(Line::from(""));
            lines.push(Line::from(vec![
                Span::styled("     [CHAT] ", Style::default().fg(Theme::WHITE).add_modifier(Modifier::BOLD)),
                Span::styled("Enter", Style::default().fg(Theme::WHITE)),
                Span::styled(" send   ", Style::default().fg(Theme::GREY_400)),
                Span::styled("Esc", Style::default().fg(Theme::WHITE)),
                Span::styled(" cancel", Style::default().fg(Theme::GREY_400)),
            ]));
        }
    }
    lines.push(Line::from(""));

    let title = match mode {
        ApplyMode::View => " › 𝘢𝘱𝘱𝘭𝘺 𝘤𝘩𝘢𝘯𝘨𝘦𝘴 ",
        ApplyMode::Edit => " › 𝘦𝘥𝘪𝘵 𝘥𝘪𝘧𝘧 ",
        ApplyMode::Chat => " › 𝘳𝘦𝘧𝘪𝘯𝘦 𝘧𝘪𝘹 ",
    };

    let block = Paragraph::new(lines)
        .block(Block::default()
            .title(title)
            .title_style(Style::default().fg(Theme::GREY_100))
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Theme::GREY_400))
            .style(Style::default().bg(Theme::GREY_900)));
    
    frame.render_widget(block, area);
}

fn render_file_detail(frame: &mut Frame, path: &PathBuf, file_index: &crate::index::FileIndex, llm_summary: Option<&String>, _scroll: usize) {
    let area = centered_rect(70, 75, frame.area());
    frame.render_widget(Clear, area);

    let inner_width = area.width.saturating_sub(12) as usize;
    
    // File name header
    let filename = path.file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("unknown");
    
    let mut lines = vec![
        Line::from(""),
        Line::from(vec![
            Span::styled("    ", Style::default()),
            Span::styled(format!(" {} ", file_index.language.icon()), 
                Style::default().fg(Theme::GREY_900).bg(Theme::GREY_300)),
            Span::styled(format!("  {}", filename), 
                Style::default().fg(Theme::WHITE).add_modifier(Modifier::BOLD)),
        ]),
        Line::from(vec![
            Span::styled(format!("       {}", path.display()), 
                Style::default().fg(Theme::GREY_500)),
        ]),
        Line::from(""),
    ];
    
    // Summary card
    lines.push(Line::from(vec![
        Span::styled("    ╭─ ", Style::default().fg(Theme::GREY_600)),
        Span::styled("Summary", Style::default().fg(Theme::GREY_300)),
        Span::styled(" ─".to_string() + &"─".repeat(inner_width.saturating_sub(15)) + "╮", Style::default().fg(Theme::GREY_600)),
    ]));
    lines.push(Line::from(vec![
        Span::styled("    │", Style::default().fg(Theme::GREY_600)),
    ]));
    
    if let Some(summary) = llm_summary {
        let wrapped = wrap_text(summary, inner_width.saturating_sub(6));
        for line in wrapped.iter().take(5) {
            lines.push(Line::from(vec![
                Span::styled("    │  ", Style::default().fg(Theme::GREY_600)),
                Span::styled(line.to_string(), Style::default().fg(Theme::GREY_50)),
            ]));
        }
        if wrapped.len() > 5 {
            lines.push(Line::from(vec![
                Span::styled("    │  ", Style::default().fg(Theme::GREY_600)),
                Span::styled("...", Style::default().fg(Theme::GREY_500)),
            ]));
        }
    } else {
        lines.push(Line::from(vec![
            Span::styled("    │  ", Style::default().fg(Theme::GREY_600)),
            Span::styled(&file_index.summary.purpose, Style::default().fg(Theme::GREY_100)),
        ]));
    }
    
    lines.push(Line::from(vec![
        Span::styled("    │", Style::default().fg(Theme::GREY_600)),
    ]));
    lines.push(Line::from(vec![
        Span::styled("    ╰".to_string() + &"─".repeat(inner_width.saturating_sub(4)) + "╯", Style::default().fg(Theme::GREY_600)),
    ]));
    lines.push(Line::from(""));
    
    // Metrics bar
    let func_count = file_index.symbols.iter()
        .filter(|s| matches!(s.kind, crate::index::SymbolKind::Function | crate::index::SymbolKind::Method))
        .count();
    let struct_count = file_index.symbols.iter()
        .filter(|s| matches!(s.kind, crate::index::SymbolKind::Struct | crate::index::SymbolKind::Class))
        .count();
    
    lines.push(Line::from(vec![
        Span::styled("    ", Style::default()),
        Span::styled(format!(" {} ", file_index.loc), Style::default().fg(Theme::GREY_900).bg(Theme::GREY_500)),
        Span::styled(" LOC  ", Style::default().fg(Theme::GREY_400)),
        Span::styled(format!(" {} ", func_count), Style::default().fg(Theme::GREY_900).bg(Theme::GREY_500)),
        Span::styled(" funcs  ", Style::default().fg(Theme::GREY_400)),
        Span::styled(format!(" {} ", struct_count), Style::default().fg(Theme::GREY_900).bg(Theme::GREY_500)),
        Span::styled(" structs", Style::default().fg(Theme::GREY_400)),
    ]));
    lines.push(Line::from(""));
    
    // Dependencies section
    if !file_index.summary.exports.is_empty() || !file_index.summary.used_by.is_empty() || !file_index.summary.depends_on.is_empty() {
        lines.push(Line::from(vec![
            Span::styled("    ╭─ ", Style::default().fg(Theme::GREY_600)),
            Span::styled("Dependencies", Style::default().fg(Theme::GREY_300)),
            Span::styled(" ─".to_string() + &"─".repeat(inner_width.saturating_sub(19)) + "╮", Style::default().fg(Theme::GREY_600)),
        ]));
        
        // Exports
        if !file_index.summary.exports.is_empty() {
            let exports_str = if file_index.summary.exports.len() > 5 {
                format!("{}, +{}", file_index.summary.exports[..5].join(", "), file_index.summary.exports.len() - 5)
            } else {
                file_index.summary.exports.join(", ")
            };
            lines.push(Line::from(vec![
                Span::styled("    │  ", Style::default().fg(Theme::GREY_600)),
                Span::styled("↗ Exports: ", Style::default().fg(Theme::GREY_400)),
                Span::styled(exports_str, Style::default().fg(Theme::GREY_200)),
            ]));
        }
        
        // Used by
        if !file_index.summary.used_by.is_empty() {
            let used_by_str: Vec<_> = file_index.summary.used_by.iter()
                .take(4)
                .filter_map(|p| p.file_name().and_then(|n| n.to_str()))
                .collect();
            let suffix = if file_index.summary.used_by.len() > 4 {
                format!(", +{}", file_index.summary.used_by.len() - 4)
            } else {
                String::new()
            };
            lines.push(Line::from(vec![
                Span::styled("    │  ", Style::default().fg(Theme::GREY_600)),
                Span::styled("← Used by: ", Style::default().fg(Theme::GREY_400)),
                Span::styled(format!("{}{}", used_by_str.join(", "), suffix), Style::default().fg(Theme::GREY_200)),
            ]));
        }
        
        // Depends on
        if !file_index.summary.depends_on.is_empty() {
            let deps_str: Vec<_> = file_index.summary.depends_on.iter()
                .take(4)
                .filter_map(|p| p.file_name().and_then(|n| n.to_str()))
                .collect();
            let suffix = if file_index.summary.depends_on.len() > 4 {
                format!(", +{}", file_index.summary.depends_on.len() - 4)
            } else {
                String::new()
            };
            lines.push(Line::from(vec![
                Span::styled("    │  ", Style::default().fg(Theme::GREY_600)),
                Span::styled("→ Depends: ", Style::default().fg(Theme::GREY_400)),
                Span::styled(format!("{}{}", deps_str.join(", "), suffix), Style::default().fg(Theme::GREY_200)),
            ]));
        }
        
        lines.push(Line::from(vec![
            Span::styled("    ╰".to_string() + &"─".repeat(inner_width.saturating_sub(4)) + "╯", Style::default().fg(Theme::GREY_600)),
        ]));
    }
    
    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        Span::styled("    ", Style::default()),
        Span::styled(" Esc ", Style::default().fg(Theme::GREY_900).bg(Theme::GREY_400)),
        Span::styled(" close", Style::default().fg(Theme::GREY_400)),
    ]));
    lines.push(Line::from(""));

    let block = Paragraph::new(lines)
        .wrap(Wrap { trim: false })
        .block(Block::default()
            .title(" › 𝘧𝘪𝘭𝘦 𝘥𝘦𝘵𝘢𝘪𝘭 ")
            .title_style(Style::default().fg(Theme::GREY_100))
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Theme::GREY_400))
            .style(Style::default().bg(Theme::GREY_900)));
    
    frame.render_widget(block, area);
}

fn render_git_status(
    frame: &mut Frame,
    staged: &[String],
    modified: &[String],
    untracked: &[String],
    selected: usize,
    _scroll: usize,
    commit_input: Option<&str>,
) {
    let area = centered_rect(70, 80, frame.area());
    frame.render_widget(Clear, area);

    let total_files = staged.len() + modified.len() + untracked.len();
    let mut current_idx = 0usize;
    
    let mut lines = vec![
        Line::from(""),
        Line::from(vec![
            Span::styled("     › ", Style::default().fg(Theme::WHITE)),
            Span::styled("Git Status", Style::default().fg(Theme::WHITE).add_modifier(Modifier::BOLD)),
        ]),
        Line::from(""),
    ];
    
    // Helper to render a file with selection indicator
    let render_file = |path: &str, icon: &str, icon_color: ratatui::style::Color, idx: usize, selected: usize| -> Line<'static> {
        let is_selected = idx == selected;
        let cursor = if is_selected { " › " } else { "   " };
        
        Line::from(vec![
            Span::styled(cursor.to_string(), Style::default().fg(Theme::WHITE)),
            Span::styled(format!("  {} ", icon), Style::default().fg(icon_color)),
            Span::styled(
                path.to_string(),
                if is_selected {
                    Style::default().fg(Theme::WHITE).add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(Theme::GREY_200)
                }
            ),
        ])
    };
    
    // Staged files section
    if !staged.is_empty() {
        lines.push(Line::from(vec![
            Span::styled("     ─── ", Style::default().fg(Theme::GREY_600)),
            Span::styled("Staged", Style::default().fg(Theme::GREEN).add_modifier(Modifier::BOLD)),
            Span::styled(format!(" ({}) ", staged.len()), Style::default().fg(Theme::GREY_400)),
            Span::styled("────────────────────────────", Style::default().fg(Theme::GREY_600)),
        ]));
        lines.push(Line::from(""));
        
        for file in staged.iter() {
            lines.push(render_file(file, "✓", Theme::GREEN, current_idx, selected));
            current_idx += 1;
        }
        lines.push(Line::from(""));
    }
    
    // Modified files section
    if !modified.is_empty() {
        lines.push(Line::from(vec![
            Span::styled("     ─── ", Style::default().fg(Theme::GREY_600)),
            Span::styled("Modified", Style::default().fg(Theme::BADGE_DOCS).add_modifier(Modifier::BOLD)),
            Span::styled(format!(" ({}) ", modified.len()), Style::default().fg(Theme::GREY_400)),
            Span::styled("──────────────────────────", Style::default().fg(Theme::GREY_600)),
        ]));
        lines.push(Line::from(""));
        
        for file in modified.iter() {
            lines.push(render_file(file, "●", Theme::BADGE_DOCS, current_idx, selected));
            current_idx += 1;
        }
        lines.push(Line::from(""));
    }
    
    // Untracked files section
    if !untracked.is_empty() {
        lines.push(Line::from(vec![
            Span::styled("     ─── ", Style::default().fg(Theme::GREY_600)),
            Span::styled("Untracked", Style::default().fg(Theme::GREY_300).add_modifier(Modifier::BOLD)),
            Span::styled(format!(" ({}) ", untracked.len()), Style::default().fg(Theme::GREY_400)),
            Span::styled("─────────────────────────", Style::default().fg(Theme::GREY_600)),
        ]));
        lines.push(Line::from(""));
        
        for file in untracked.iter() {
            lines.push(render_file(file, "?", Theme::GREY_400, current_idx, selected));
            current_idx += 1;
        }
        lines.push(Line::from(""));
    }
    
    // Empty state
    if total_files == 0 {
        lines.push(Line::from(""));
        lines.push(Line::from(vec![
            Span::styled("     ✓ Working tree clean", Style::default().fg(Theme::GREEN)),
        ]));
        lines.push(Line::from(""));
    }
    
    // Separator before actions
    lines.push(Line::from(vec![
        Span::styled("     ─────────────────────────────────────────────", Style::default().fg(Theme::GREY_600))
    ]));
    lines.push(Line::from(""));
    
    // Commit input mode
    if let Some(input) = commit_input {
        lines.push(Line::from(vec![
            Span::styled("     Commit message: ", Style::default().fg(Theme::GREY_400)),
        ]));
        lines.push(Line::from(vec![
            Span::styled(format!("     │ {}_", input), Style::default().fg(Theme::WHITE)),
        ]));
        lines.push(Line::from(""));
        lines.push(Line::from(vec![
            Span::styled("     ", Style::default()),
            Span::styled(" Enter ", Style::default().fg(Theme::GREY_900).bg(Theme::GREEN)),
            Span::styled(" commit   ", Style::default().fg(Theme::GREY_400)),
            Span::styled(" Esc ", Style::default().fg(Theme::GREY_900).bg(Theme::GREY_400)),
            Span::styled(" cancel", Style::default().fg(Theme::GREY_400)),
        ]));
    } else {
        // Action hints - Row 1: File operations
        lines.push(Line::from(vec![
            Span::styled("     ", Style::default()),
            Span::styled(" s ", Style::default().fg(Theme::GREY_900).bg(Theme::GREY_300)),
            Span::styled(" stage  ", Style::default().fg(Theme::GREY_400)),
            Span::styled(" u ", Style::default().fg(Theme::GREY_900).bg(Theme::GREY_300)),
            Span::styled(" unstage  ", Style::default().fg(Theme::GREY_400)),
            Span::styled(" r ", Style::default().fg(Theme::GREY_900).bg(Theme::GREY_300)),
            Span::styled(" restore  ", Style::default().fg(Theme::GREY_400)),
            Span::styled(" d ", Style::default().fg(Theme::GREY_900).bg(Theme::GREY_300)),
            Span::styled(" delete", Style::default().fg(Theme::GREY_400)),
        ]));
        // Row 2: Batch operations
        lines.push(Line::from(vec![
            Span::styled("     ", Style::default()),
            Span::styled(" S ", Style::default().fg(Theme::GREY_900).bg(Theme::GREY_300)),
            Span::styled(" stage all  ", Style::default().fg(Theme::GREY_400)),
            Span::styled(" D ", Style::default().fg(Theme::GREY_900).bg(Theme::GREY_300)),
            Span::styled(" clean untracked  ", Style::default().fg(Theme::GREY_400)),
            Span::styled(" X ", Style::default().fg(Theme::GREY_900).bg(Theme::RED)),
            Span::styled(" reset all", Style::default().fg(Theme::GREY_400)),
        ]));
        // Row 3: Git operations
        lines.push(Line::from(vec![
            Span::styled("     ", Style::default()),
            Span::styled(" c ", Style::default().fg(Theme::GREY_900).bg(Theme::GREEN)),
            Span::styled(" commit  ", Style::default().fg(Theme::GREY_400)),
            Span::styled(" P ", Style::default().fg(Theme::GREY_900).bg(Theme::GREY_300)),
            Span::styled(" push  ", Style::default().fg(Theme::GREY_400)),
            Span::styled(" R ", Style::default().fg(Theme::GREY_900).bg(Theme::GREY_400)),
            Span::styled(" refresh  ", Style::default().fg(Theme::GREY_400)),
            Span::styled(" q ", Style::default().fg(Theme::GREY_900).bg(Theme::GREY_500)),
            Span::styled(" close", Style::default().fg(Theme::GREY_500)),
        ]));
    }
    lines.push(Line::from(""));

    let block = Paragraph::new(lines)
        .wrap(Wrap { trim: false })
        .block(Block::default()
            .title(" › 𝘨𝘪𝘵 ")
            .title_style(Style::default().fg(Theme::GREY_100))
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Theme::GREY_400))
            .style(Style::default().bg(Theme::GREY_900)));

    frame.render_widget(block, area);
}

fn render_branch_dialog(
    frame: &mut Frame,
    branch_name: &str,
    commit_message: &str,
    pending_files: &[PathBuf],
) {
    let area = centered_rect(60, 60, frame.area());
    frame.render_widget(Clear, area);

    let inner_width = area.width.saturating_sub(10) as usize;
    
    let mut lines = vec![
        Line::from(""),
        Line::from(vec![
            Span::styled("     › ", Style::default().fg(Theme::WHITE)),
            Span::styled("Create Branch & Commit", Style::default().fg(Theme::WHITE).add_modifier(Modifier::BOLD)),
        ]),
        Line::from(""),
        Line::from(vec![
            Span::styled("     ─────────────────────────────────────────", Style::default().fg(Theme::GREY_600))
        ]),
        Line::from(""),
        Line::from(vec![
            Span::styled("     Branch: ", Style::default().fg(Theme::GREY_400)),
            Span::styled(branch_name, Style::default().fg(Theme::WHITE)),
        ]),
        Line::from(""),
        Line::from(vec![
            Span::styled("     Message:", Style::default().fg(Theme::GREY_400)),
        ]),
    ];
    
    // Show commit message (wrapped)
    let msg_wrapped = wrap_text(commit_message, inner_width.saturating_sub(10));
    for line in msg_wrapped {
        lines.push(Line::from(vec![
            Span::styled(format!("       {}", line), Style::default().fg(Theme::GREY_100)),
        ]));
    }
    
    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        Span::styled("     ─────────────────────────────────────────", Style::default().fg(Theme::GREY_600))
    ]));
    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        Span::styled(format!("     {} files to commit:", pending_files.len()), Style::default().fg(Theme::GREY_300)),
    ]));
    
    // Show files (limited)
    for file in pending_files.iter().take(5) {
        let name = file.file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("?");
        lines.push(Line::from(vec![
            Span::styled(format!("       • {}", name), Style::default().fg(Theme::GREY_200)),
        ]));
    }
    if pending_files.len() > 5 {
        lines.push(Line::from(vec![
            Span::styled(format!("       ...and {} more", pending_files.len() - 5), Style::default().fg(Theme::GREY_400)),
        ]));
    }
    
    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        Span::styled("     ─────────────────────────────────────────", Style::default().fg(Theme::GREY_600))
    ]));
    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        Span::styled("     𝘺", Style::default().fg(Theme::WHITE)),
        Span::styled(" create & push   ", Style::default().fg(Theme::GREY_400)),
        Span::styled("Esc", Style::default().fg(Theme::WHITE)),
        Span::styled(" cancel", Style::default().fg(Theme::GREY_400)),
    ]));
    lines.push(Line::from(""));

    let block = Paragraph::new(lines)
        .wrap(Wrap { trim: false })
        .block(Block::default()
            .title(" › 𝘣𝘳𝘢𝘯𝘤𝘩 ")
            .title_style(Style::default().fg(Theme::GREY_100))
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Theme::GREY_400))
            .style(Style::default().bg(Theme::GREY_900)));

    frame.render_widget(block, area);
}

fn render_pr_review(
    frame: &mut Frame,
    branch_name: &str,
    files_changed: &[(PathBuf, String)],
    review_comments: &[PRReviewComment],
    scroll: usize,
    reviewing: bool,
    pr_url: &Option<String>,
) {
    let area = centered_rect(80, 85, frame.area());
    frame.render_widget(Clear, area);

    let visible_height = area.height.saturating_sub(14) as usize;
    
    let mut lines = vec![
        Line::from(""),
        Line::from(vec![
            Span::styled("     › ", Style::default().fg(Theme::WHITE)),
            Span::styled("PR Review", Style::default().fg(Theme::WHITE).add_modifier(Modifier::BOLD)),
        ]),
        Line::from(""),
        Line::from(vec![
            Span::styled(format!("     Branch: {}", branch_name), Style::default().fg(Theme::GREY_200)),
        ]),
        Line::from(vec![
            Span::styled(format!("     {} files changed", files_changed.len()), Style::default().fg(Theme::GREY_300)),
        ]),
    ];
    
    if let Some(url) = pr_url {
        lines.push(Line::from(vec![
            Span::styled(format!("     PR: {}", url), Style::default().fg(Theme::GREY_200)),
        ]));
    }
    
    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        Span::styled("     ─────────────────────────────────────────────────────", Style::default().fg(Theme::GREY_600))
    ]));
    lines.push(Line::from(""));
    
    if reviewing {
        lines.push(Line::from(vec![
            Span::styled("     Reviewing with Sonnet 4...", Style::default().fg(Theme::WHITE).add_modifier(Modifier::ITALIC)),
        ]));
    } else if review_comments.is_empty() {
        lines.push(Line::from(vec![
            Span::styled("     Press 'r' to get AI code review", Style::default().fg(Theme::GREY_300)),
        ]));
    } else {
        // Show review comments
        for (i, comment) in review_comments.iter().skip(scroll).take(visible_height).enumerate() {
            let file_name = comment.file.file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("?");
            
            lines.push(Line::from(vec![
                Span::styled(format!("     {} ", comment.severity.icon()), 
                    Style::default().fg(match comment.severity {
                        ReviewSeverity::Praise => Theme::GREEN,
                        ReviewSeverity::Info => Theme::GREY_300,
                        ReviewSeverity::Suggest => Theme::WHITE,
                        ReviewSeverity::Warning => Theme::RED,
                    })),
                Span::styled(file_name, Style::default().fg(Theme::GREY_100).add_modifier(Modifier::BOLD)),
            ]));
            
            // Wrap comment
            let wrapped = wrap_text(&comment.comment, 55);
            for line in wrapped {
                lines.push(Line::from(vec![
                    Span::styled(format!("       {}", line), Style::default().fg(Theme::GREY_200)),
                ]));
            }
            
            if i < review_comments.len().saturating_sub(scroll).min(visible_height) - 1 {
                lines.push(Line::from(""));
            }
        }
    }
    
    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        Span::styled("     ─────────────────────────────────────────────────────", Style::default().fg(Theme::GREY_600))
    ]));
    lines.push(Line::from(""));
    
    // Action hints
    if pr_url.is_some() {
        lines.push(Line::from(vec![
            Span::styled("     𝘰", Style::default().fg(Theme::WHITE)),
            Span::styled(" open in browser   ", Style::default().fg(Theme::GREY_400)),
            Span::styled("𝘳", Style::default().fg(Theme::WHITE)),
            Span::styled(" review again   ", Style::default().fg(Theme::GREY_400)),
            Span::styled("Esc", Style::default().fg(Theme::WHITE)),
            Span::styled(" close", Style::default().fg(Theme::GREY_400)),
        ]));
    } else {
        lines.push(Line::from(vec![
            Span::styled("     𝘳", Style::default().fg(Theme::WHITE)),
            Span::styled(" review   ", Style::default().fg(Theme::GREY_400)),
            Span::styled("𝘤", Style::default().fg(Theme::WHITE)),
            Span::styled(" create PR   ", Style::default().fg(Theme::GREY_400)),
            Span::styled("Esc", Style::default().fg(Theme::WHITE)),
            Span::styled(" close", Style::default().fg(Theme::GREY_400)),
        ]));
    }
    lines.push(Line::from(""));

    let block = Paragraph::new(lines)
        .wrap(Wrap { trim: false })
        .block(Block::default()
            .title(" › 𝘱𝘳 𝘳𝘦𝘷𝘪𝘦𝘸 ")
            .title_style(Style::default().fg(Theme::GREY_100))
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Theme::GREY_400))
            .style(Style::default().bg(Theme::GREY_900)));

    frame.render_widget(block, area);
}

fn render_loading_overlay(frame: &mut Frame, state: &LoadingState, anim_frame: usize, _summary_progress: Option<(usize, usize)>) {
    let area = frame.area();
    
    // Simple message - summaries are silent background now
    let message = state.message().to_string();
    
    // Calculate overlay dimensions
    let width = (message.len() + 12) as u16;
    let height = 5u16;
    
    let overlay_area = Rect {
        x: (area.width.saturating_sub(width)) / 2,
        y: (area.height.saturating_sub(height)) / 2,
        width: width.min(area.width),
        height,
    };
    
    frame.render_widget(Clear, overlay_area);
    
    // Get spinner frame
    let spinner = SPINNER_FRAMES[anim_frame % SPINNER_FRAMES.len()];
    
    let lines = vec![
        Line::from(""),
        Line::from(vec![
            Span::styled(format!("   {} ", spinner), Style::default().fg(Theme::WHITE).add_modifier(Modifier::BOLD)),
            Span::styled(&message, Style::default().fg(Theme::GREY_100)),
            Span::styled("   ", Style::default()),
        ]),
        Line::from(""),
    ];
    
    let block = Paragraph::new(lines)
        .block(Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Theme::GREY_400))
            .style(Style::default().bg(Theme::GREY_800)));
    
    frame.render_widget(block, overlay_area);
}

fn render_toast(frame: &mut Frame, toast: &Toast) {
    let area = frame.area();
    let is_success = toast.message.starts_with('✓');
    let is_error = toast.message.contains("failed") || toast.message.contains("error") || toast.message.contains("Error");
    
    let width = (toast.message.len() + 10) as u16;
    let height = if is_success { 3u16 } else { 1u16 };
    let toast_area = Rect {
        x: (area.width.saturating_sub(width)) / 2,
        y: (area.height.saturating_sub(height)) / 2, // Center for success, otherwise bottom
        width: width.min(area.width),
        height,
    };
    
    // Success toasts get special treatment - centered and green
    if is_success {
        frame.render_widget(Clear, toast_area);
        let lines = vec![
            Line::from(""),
            Line::from(vec![
                Span::styled("  ", Style::default()),
                Span::styled(&toast.message, Style::default().fg(Theme::WHITE).add_modifier(Modifier::BOLD)),
                Span::styled("  ", Style::default()),
            ]),
            Line::from(""),
        ];
        let content = Paragraph::new(lines)
            .style(Style::default().bg(Theme::GREEN).fg(Theme::GREY_900));
        frame.render_widget(content, toast_area);
    } else if is_error {
        let toast_area = Rect {
            x: (area.width.saturating_sub(width)) / 2,
            y: area.height.saturating_sub(5),
            width: width.min(area.width),
            height: 1,
        };
        let content = Paragraph::new(Line::from(vec![
            Span::styled("  ✗ ", Style::default().fg(Theme::WHITE)),
            Span::styled(&toast.message, Style::default().fg(Theme::WHITE)),
            Span::styled("  ", Style::default()),
        ]))
        .style(Style::default().bg(Theme::RED));
        frame.render_widget(content, toast_area);
    } else {
        let toast_area = Rect {
            x: (area.width.saturating_sub(width)) / 2,
            y: area.height.saturating_sub(5),
            width: width.min(area.width),
            height: 1,
        };
        let content = Paragraph::new(Line::from(vec![
            Span::styled("  › ", Style::default().fg(Theme::WHITE)),
            Span::styled(&toast.message, Style::default().fg(Theme::GREY_100).add_modifier(Modifier::ITALIC)),
            Span::styled("  ", Style::default()),
        ]))
        .style(Style::default().bg(Theme::GREY_700));
        frame.render_widget(content, toast_area);
    }
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
    if width == 0 {
        return vec![text.to_string()];
    }
    
    let mut lines = Vec::new();
    let mut current_line = String::new();
    
    for word in text.split_whitespace() {
        if current_line.is_empty() {
            if word.len() > width {
                // Word is longer than width, force break it
                let mut remaining = word;
                while remaining.len() > width {
                    lines.push(remaining[..width].to_string());
                    remaining = &remaining[width..];
                }
                current_line = remaining.to_string();
            } else {
                current_line = word.to_string();
            }
        } else if current_line.len() + 1 + word.len() <= width {
            current_line.push(' ');
            current_line.push_str(word);
        } else {
            lines.push(current_line);
            if word.len() > width {
                let mut remaining = word;
                while remaining.len() > width {
                    lines.push(remaining[..width].to_string());
                    remaining = &remaining[width..];
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
