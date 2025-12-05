//! Cosmos - A contemplative vibe coding companion
//!
//! ☽ C O S M O S ✦
//!
//! An AI-powered IDE in the terminal that uses codebase indexing
//! to suggest improvements, bug fixes, and optimizations.

mod cache;
mod config;
mod context;
mod index;
mod suggest;
mod ui;

// Keep these for compatibility during transition
mod ai;
mod analysis;
mod diff;
mod git_ops;
mod history;
mod mascot;
mod prompt;
mod refactor;
mod score;
mod spinner;
mod testing;
mod workflow;

use anyhow::Result;
use clap::Parser;
use context::WorkContext;
use crossterm::{
    event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use index::CodebaseIndex;
use ratatui::prelude::*;
use std::collections::HashMap;
use std::io;
use std::path::PathBuf;
use std::sync::mpsc;
use std::time::Duration;
use suggest::SuggestionEngine;
use ui::{ActivePanel, App, InputMode, LoadingState, Overlay};

#[derive(Parser, Debug)]
#[command(
    name = "cosmos",
    about = "A contemplative vibe coding companion",
    long_about = "☽ C O S M O S ✦\n\n\
                  A contemplative companion for your codebase.\n\n\
                  Uses AST-based indexing and AI to suggest improvements,\n\
                  bug fixes, features, and optimizations.",
    version
)]
struct Args {
    /// Path to the repository (defaults to current directory)
    #[arg(default_value = ".")]
    path: PathBuf,

    /// Set up OpenRouter API key for AI features
    #[arg(long)]
    setup: bool,

    /// Show stats and exit (no TUI)
    #[arg(long)]
    stats: bool,
}

/// Messages from background tasks to the main UI thread
pub enum BackgroundMessage {
    SuggestionsReady {
        suggestions: Vec<suggest::Suggestion>,
        usage: Option<suggest::llm::Usage>,
        model: String,
    },
    SuggestionsError(String),
    SummariesReady {
        summaries: std::collections::HashMap<PathBuf, String>,
        usage: Option<suggest::llm::Usage>,
    },
    SummariesError(String),
    FixReady {
        suggestion_id: uuid::Uuid,
        diff_preview: String,
        file_path: PathBuf,
        summary: String,
        usage: Option<suggest::llm::Usage>,
    },
    FixError(String),
    RefinedFixReady {
        diff_preview: String,
        usage: Option<suggest::llm::Usage>,
    },
    RefinedFixError(String),
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    // Handle --setup flag
    if args.setup {
        return setup_api_key();
    }

    let path = args.path.canonicalize()?;

    // Show startup message
    eprintln!();
    eprintln!("  ☽ C O S M O S ✦");
    eprintln!("  a contemplative companion for your codebase");
    eprintln!();

    // Initialize cache
    let cache_manager = cache::Cache::new(&path);
    
    // Initialize index (fast, synchronous)
    let index = init_index(&path, &cache_manager)?;
    let context = init_context(&path)?;
    
    // Create empty suggestion engine (will be populated by LLM)
    let suggestions = SuggestionEngine::new_empty(index.clone());

    // Stats mode: print and exit
    if args.stats {
        print_stats(&index, &suggestions, &context);
        return Ok(());
    }

    // Run TUI with background LLM tasks
    run_tui(index, suggestions, context, cache_manager, path).await
}

/// Initialize the codebase index
fn init_index(path: &PathBuf, cache_manager: &cache::Cache) -> Result<CodebaseIndex> {
    eprint!("  Indexing codebase...");
    
    let index = CodebaseIndex::new(path)?;
    let stats = index.stats();
    
    // Save index cache
    let index_cache = cache::IndexCache::from_index(&index);
    let _ = cache_manager.save_index_cache(&index_cache);
    
    eprintln!(
        " {} files, {} symbols",
        stats.file_count,
        stats.symbol_count
    );
    
    Ok(index)
}

/// Initialize the work context
fn init_context(path: &PathBuf) -> Result<WorkContext> {
    eprint!("  Loading context...");
    
    let context = WorkContext::load(path)?;
    
    eprintln!(
        " {} on {}, {} changed",
        context.branch,
        if context.inferred_focus.is_some() { 
            context.inferred_focus.as_ref().unwrap() 
        } else { 
            "project" 
        },
        context.modified_count
    );
    
    Ok(context)
}

/// Print stats and exit
fn print_stats(index: &CodebaseIndex, suggestions: &SuggestionEngine, context: &WorkContext) {
    let stats = index.stats();
    let counts = suggestions.counts();

    println!();
    println!("  ╔══════════════════════════════════════════════════╗");
    println!("  ║           ☽ C O S M O S ✦ Stats                  ║");
    println!("  ╠══════════════════════════════════════════════════╣");
    println!("  ║                                                  ║");
    println!("  ║  Files:     {:>6}                               ║", stats.file_count);
    println!("  ║  LOC:       {:>6}                               ║", stats.total_loc);
    println!("  ║  Symbols:   {:>6}                               ║", stats.symbol_count);
    println!("  ║  Patterns:  {:>6}                               ║", stats.pattern_count);
    println!("  ║                                                  ║");
    println!("  ║  Suggestions:                                    ║");
    println!("  ║    High:    {:>6} ●                             ║", counts.high);
    println!("  ║    Medium:  {:>6} ◐                             ║", counts.medium);
    println!("  ║    Low:     {:>6} ○                             ║", counts.low);
    println!("  ║                                                  ║");
    println!("  ║  Context:                                        ║");
    println!("  ║    Branch:  {:>20}               ║", truncate(&context.branch, 20));
    println!("  ║    Changed: {:>6}                               ║", context.modified_count);
    println!("  ║                                                  ║");
    println!("  ╚══════════════════════════════════════════════════╝");
    println!();

    // Top suggestions
    let top = suggestions.high_priority_suggestions();
    if !top.is_empty() {
        println!("  Top suggestions:");
        println!();
        for (i, s) in top.iter().take(5).enumerate() {
            println!("    {}. {} {}: {}", 
                i + 1,
                s.priority.icon(),
                s.kind.label(),
                truncate(&s.summary, 50)
            );
        }
        println!();
    }
}

/// Set up the API key interactively
fn setup_api_key() -> Result<()> {
    config::setup_api_key_interactive()
        .map_err(|e| anyhow::anyhow!("{}", e))?;
    println!("  ✓ API key configured. You can now use AI features!");
    Ok(())
}

/// Run the TUI application with background LLM tasks
async fn run_tui(
    index: CodebaseIndex,
    suggestions: SuggestionEngine,
    context: WorkContext,
    cache_manager: cache::Cache,
    repo_path: PathBuf,
) -> Result<()> {
    // Set up terminal
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    // Create app with loading state
    let mut app = App::new(index.clone(), suggestions, context.clone());
    
    // Check if we have API key
    let has_api_key = suggest::llm::is_available();
    
    // ═══════════════════════════════════════════════════════════════════════
    //  SMART SUMMARY CACHING
    // ═══════════════════════════════════════════════════════════════════════
    
    // Compute file hashes for change detection
    let file_hashes = cache::compute_file_hashes(&index);
    
    // Load cached LLM summaries and apply immediately
    let mut llm_cache = cache_manager.load_llm_summaries_cache()
        .unwrap_or_else(cache::LlmSummaryCache::new);
    
    // Get all valid cached summaries and load them immediately (instant startup!)
    let cached_summaries = llm_cache.get_all_valid_summaries(&file_hashes);
    let cached_count = cached_summaries.len();
    if !cached_summaries.is_empty() {
        app.update_summaries(cached_summaries);
    }
    
    // Discover project context (for better quality summaries)
    let project_context = suggest::llm::discover_project_context(&index);
    llm_cache.set_project_context(project_context.clone());
    
    // Find files that need new/updated summaries
    let files_needing_summary = llm_cache.get_files_needing_summary(&file_hashes);
    let needs_summary_count = files_needing_summary.len();
    
    if has_api_key {
        app.loading = LoadingState::GeneratingSuggestions;
    }

    // Create channel for background tasks
    let (tx, rx) = mpsc::channel::<BackgroundMessage>();

    // Spawn background task for suggestions if API key available
    if has_api_key {
        let index_clone = index.clone();
        let context_clone = context.clone();
        let tx_suggestions = tx.clone();
        let cache_clone_path = repo_path.clone();
        
        tokio::spawn(async move {
            match suggest::llm::analyze_codebase(&index_clone, &context_clone).await {
                Ok((suggestions, usage)) => {
                    // Cache the suggestions
                    let cache = cache::Cache::new(&cache_clone_path);
                    let cache_data = cache::SuggestionsCache::from_suggestions(&suggestions);
                    let _ = cache.save_suggestions_cache(&cache_data);
                    
                    let _ = tx_suggestions.send(BackgroundMessage::SuggestionsReady {
                        suggestions,
                        usage,
                        model: "opus-4.5".to_string(),
                    });
                }
                Err(e) => {
                    let _ = tx_suggestions.send(BackgroundMessage::SuggestionsError(e.to_string()));
                }
            }
        });
        
        // Only generate summaries for files that need them (smart caching!)
        if !files_needing_summary.is_empty() {
            let index_clone2 = index.clone();
            let context_clone2 = context.clone();
            let tx_summaries = tx.clone();
            let cache_path = repo_path.clone();
            let file_hashes_clone = file_hashes.clone();
            
            // Prioritize files for generation
            let (high_priority, medium_priority, low_priority) = 
                suggest::llm::prioritize_files_for_summary(&index_clone2, &context_clone2, &files_needing_summary);
            
            tokio::spawn(async move {
                let cache = cache::Cache::new(&cache_path);
                
                // Load existing cache to update incrementally
                let mut llm_cache = cache.load_llm_summaries_cache()
                    .unwrap_or_else(cache::LlmSummaryCache::new);
                
                let mut all_summaries = HashMap::new();
                let mut total_usage = suggest::llm::Usage::default();
                
                // Process high priority files first
                if !high_priority.is_empty() {
                    if let Ok((summaries, usage)) = suggest::llm::generate_summaries_for_files(
                        &index_clone2, &high_priority, &project_context
                    ).await {
                        // Update cache with new summaries
                        for (path, summary) in &summaries {
                            if let Some(hash) = file_hashes_clone.get(path) {
                                llm_cache.set_summary(path.clone(), summary.clone(), hash.clone());
                            }
                        }
                        // Save cache incrementally
                        let _ = cache.save_llm_summaries_cache(&llm_cache);
                        
                        all_summaries.extend(summaries);
                        if let Some(u) = usage {
                            total_usage.prompt_tokens += u.prompt_tokens;
                            total_usage.completion_tokens += u.completion_tokens;
                            total_usage.total_tokens += u.total_tokens;
                        }
                    }
                }
                
                // Process medium priority files
                if !medium_priority.is_empty() {
                    if let Ok((summaries, usage)) = suggest::llm::generate_summaries_for_files(
                        &index_clone2, &medium_priority, &project_context
                    ).await {
                        for (path, summary) in &summaries {
                            if let Some(hash) = file_hashes_clone.get(path) {
                                llm_cache.set_summary(path.clone(), summary.clone(), hash.clone());
                            }
                        }
                        let _ = cache.save_llm_summaries_cache(&llm_cache);
                        all_summaries.extend(summaries);
                        if let Some(u) = usage {
                            total_usage.prompt_tokens += u.prompt_tokens;
                            total_usage.completion_tokens += u.completion_tokens;
                            total_usage.total_tokens += u.total_tokens;
                        }
                    }
                }
                
                // Process low priority files (background)
                if !low_priority.is_empty() {
                    if let Ok((summaries, usage)) = suggest::llm::generate_summaries_for_files(
                        &index_clone2, &low_priority, &project_context
                    ).await {
                        for (path, summary) in &summaries {
                            if let Some(hash) = file_hashes_clone.get(path) {
                                llm_cache.set_summary(path.clone(), summary.clone(), hash.clone());
                            }
                        }
                        let _ = cache.save_llm_summaries_cache(&llm_cache);
                        all_summaries.extend(summaries);
                        if let Some(u) = usage {
                            total_usage.prompt_tokens += u.prompt_tokens;
                            total_usage.completion_tokens += u.completion_tokens;
                            total_usage.total_tokens += u.total_tokens;
                        }
                    }
                }
                
                let final_usage = if total_usage.total_tokens > 0 {
                    Some(total_usage)
                } else {
                    None
                };
                
                let _ = tx_summaries.send(BackgroundMessage::SummariesReady { 
                    summaries: all_summaries, 
                    usage: final_usage 
                });
            });
        } else {
            // All summaries are cached, skip generation
            if cached_count > 0 {
                // Show toast that cached summaries were loaded
                app.show_toast(&format!("{} summaries loaded from cache", cached_count));
            }
        }
    }

    // Main loop with async event handling
    let result = run_loop(&mut terminal, &mut app, rx, tx, repo_path, index);

    // Restore terminal
    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    )?;
    terminal.show_cursor()?;

    result
}

/// Main event loop with background message handling
fn run_loop<B: Backend>(
    terminal: &mut Terminal<B>,
    app: &mut App,
    rx: mpsc::Receiver<BackgroundMessage>,
    tx: mpsc::Sender<BackgroundMessage>,
    repo_path: PathBuf,
    index: CodebaseIndex,
) -> Result<()> {
    loop {
        // Clear expired toasts
        app.clear_expired_toast();
        
        // Advance spinner animation
        app.tick_loading();

        // Check for background messages (non-blocking)
        while let Ok(msg) = rx.try_recv() {
            match msg {
                BackgroundMessage::SuggestionsReady { suggestions, usage, model } => {
                    for s in suggestions {
                        app.suggestions.add_llm_suggestion(s);
                    }
                    app.active_model = Some(model);
                    
                    // Track cost
                    if let Some(u) = usage {
                        let cost = u.calculate_cost(suggest::llm::Model::Opus);
                        app.session_cost += cost;
                        app.session_tokens += u.total_tokens;
                    }
                    
                    app.loading = LoadingState::GeneratingSummaries;
                    app.show_toast(&format!("{} suggestions from Opus 4.5", app.suggestions.counts().total));
                }
                BackgroundMessage::SuggestionsError(e) => {
                    app.loading = LoadingState::GeneratingSummaries; // Still try summaries
                    app.show_toast(&format!("Error: {}", truncate(&e, 40)));
                }
                BackgroundMessage::SummariesReady { summaries, usage } => {
                    let new_count = summaries.len();
                    app.update_summaries(summaries);
                    
                    // Track cost
                    if let Some(u) = usage {
                        let cost = u.calculate_cost(suggest::llm::Model::Opus);
                        app.session_cost += cost;
                        app.session_tokens += u.total_tokens;
                    }
                    
                    app.loading = LoadingState::None;
                    if new_count > 0 {
                        app.show_toast(&format!("{} new summaries · ${:.4}", new_count, app.session_cost));
                    } else {
                        app.show_toast("All summaries loaded from cache");
                    }
                }
                BackgroundMessage::SummariesError(e) => {
                    app.loading = LoadingState::None;
                    app.show_toast(&format!("Summary error: {}", truncate(&e, 30)));
                }
                BackgroundMessage::FixReady { suggestion_id, diff_preview, file_path, summary, usage } => {
                    // Track cost
                    if let Some(u) = usage {
                        let cost = u.calculate_cost(suggest::llm::Model::Opus);
                        app.session_cost += cost;
                        app.session_tokens += u.total_tokens;
                    }
                    
                    app.loading = LoadingState::None;
                    app.show_apply_confirm(suggestion_id, diff_preview, file_path, summary);
                }
                BackgroundMessage::FixError(e) => {
                    app.loading = LoadingState::None;
                    app.show_toast(&format!("Fix error: {}", truncate(&e, 40)));
                }
                BackgroundMessage::RefinedFixReady { diff_preview, usage } => {
                    // Track cost
                    if let Some(u) = usage {
                        let cost = u.calculate_cost(suggest::llm::Model::Opus);
                        app.session_cost += cost;
                        app.session_tokens += u.total_tokens;
                    }
                    
                    app.loading = LoadingState::None;
                    app.update_apply_diff(diff_preview);
                    app.show_toast("Fix refined");
                }
                BackgroundMessage::RefinedFixError(e) => {
                    app.loading = LoadingState::None;
                    app.show_toast(&format!("Refine error: {}", truncate(&e, 35)));
                }
            }
        }

        // Render
        terminal.draw(|f| ui::render(f, app))?;

        // Poll for events with timeout (for animation)
        if event::poll(Duration::from_millis(100))? {
            if let Event::Key(key) = event::read()? {
                if key.kind != KeyEventKind::Press {
                    continue;
                }

                // Handle search input mode
                if app.input_mode == InputMode::Search {
                    match key.code {
                        KeyCode::Esc => app.exit_search(),
                        KeyCode::Enter => {
                            app.input_mode = InputMode::Normal;
                        }
                        KeyCode::Backspace => app.search_pop(),
                        KeyCode::Char(c) => app.search_push(c),
                        _ => {}
                    }
                    continue;
                }

                // Handle overlay mode
                if app.overlay != Overlay::None {
                    // Handle ApplyConfirm overlay with its different modes
                    if let Overlay::ApplyConfirm { mode, suggestion_id, diff_preview, file_path, .. } = &app.overlay {
                        let mode = mode.clone();
                        let suggestion_id = *suggestion_id;
                        let diff_preview = diff_preview.clone();
                        let file_path_clone = file_path.clone();
                        
                        match mode {
                            ui::ApplyMode::View => {
                                match key.code {
                                    KeyCode::Esc | KeyCode::Char('n') => app.close_overlay(),
                                    KeyCode::Down | KeyCode::Char('j') => app.overlay_scroll_down(),
                                    KeyCode::Up | KeyCode::Char('k') => app.overlay_scroll_up(),
                                    KeyCode::Char('y') => {
                                        // Apply the fix
                                        let full_path = repo_path.join(&file_path_clone);
                                        match apply_fix(&full_path, &diff_preview) {
                                            Ok(_) => {
                                                app.suggestions.mark_applied(suggestion_id);
                                                app.show_toast("Changes applied successfully!");
                                                app.close_overlay();
                                            }
                                            Err(e) => {
                                                app.show_toast(&format!("Apply failed: {}", truncate(&e, 40)));
                                            }
                                        }
                                    }
                                    KeyCode::Char('e') => {
                                        app.set_apply_mode(ui::ApplyMode::Edit);
                                    }
                                    KeyCode::Char('c') => {
                                        app.set_apply_mode(ui::ApplyMode::Chat);
                                    }
                                    _ => {}
                                }
                            }
                            ui::ApplyMode::Edit => {
                                match key.code {
                                    KeyCode::Esc => {
                                        // Discard edits and return to view mode
                                        app.discard_apply_edit();
                                        app.show_toast("Edit cancelled");
                                    }
                                    KeyCode::F(2) => {
                                        // F2 to save and return to view mode
                                        app.commit_apply_edit();
                                        app.show_toast("Edit saved");
                                    }
                                    KeyCode::Enter => {
                                        // Enter adds a newline in edit mode
                                        app.apply_edit_push('\n');
                                    }
                                    KeyCode::Backspace => app.apply_edit_pop(),
                                    KeyCode::Tab => {
                                        // Tab inserts spaces for indentation
                                        app.apply_edit_push(' ');
                                        app.apply_edit_push(' ');
                                        app.apply_edit_push(' ');
                                        app.apply_edit_push(' ');
                                    }
                                    KeyCode::Char(c) => app.apply_edit_push(c),
                                    KeyCode::Down => app.overlay_scroll_down(),
                                    KeyCode::Up => app.overlay_scroll_up(),
                                    _ => {}
                                }
                            }
                            ui::ApplyMode::Chat => {
                                match key.code {
                                    KeyCode::Esc => {
                                        app.set_apply_mode(ui::ApplyMode::View);
                                    }
                                    KeyCode::Enter => {
                                        // Get chat input and spawn refinement
                                        if let Some(chat_text) = app.get_apply_chat_input() {
                                            if !chat_text.is_empty() {
                                                let chat_text = chat_text.to_string();
                                                let repo_path_clone = repo_path.clone();
                                                let tx_refine = tx.clone();
                                                
                                                // Find the suggestion to get more context
                                                if let Some(suggestion) = app.suggestions.suggestions.iter().find(|s| s.id == suggestion_id) {
                                                    let suggestion_clone = suggestion.clone();
                                                    let current_diff = diff_preview.clone();
                                                    
                                                    app.loading = LoadingState::GeneratingFix;
                                                    
                                                    tokio::spawn(async move {
                                                        let full_path = repo_path_clone.join(&suggestion_clone.file);
                                                        let content = match std::fs::read_to_string(&full_path) {
                                                            Ok(c) => c,
                                                            Err(e) => {
                                                                let _ = tx_refine.send(BackgroundMessage::RefinedFixError(format!("Failed to read file: {}", e)));
                                                                return;
                                                            }
                                                        };
                                                        
                                                        match suggest::llm::refine_fix(&suggestion_clone.file, &content, &suggestion_clone, &current_diff, &chat_text).await {
                                                            Ok(new_diff) => {
                                                                let _ = tx_refine.send(BackgroundMessage::RefinedFixReady {
                                                                    diff_preview: new_diff,
                                                                    usage: None,
                                                                });
                                                            }
                                                            Err(e) => {
                                                                let _ = tx_refine.send(BackgroundMessage::RefinedFixError(e.to_string()));
                                                            }
                                                        }
                                                    });
                                                }
                                            }
                                        }
                                    }
                                    KeyCode::Backspace => app.apply_chat_pop(),
                                    KeyCode::Char(c) => app.apply_chat_push(c),
                                    _ => {}
                                }
                            }
                        }
                        continue;
                    }
                    
                    // Handle other overlays
                    match key.code {
                        KeyCode::Esc | KeyCode::Char('q') => app.close_overlay(),
                        KeyCode::Down | KeyCode::Char('j') => app.overlay_scroll_down(),
                        KeyCode::Up | KeyCode::Char('k') => app.overlay_scroll_up(),
                        KeyCode::Char('a') => {
                            if let Overlay::SuggestionDetail { suggestion_id, .. } = &app.overlay {
                                let suggestion_id = *suggestion_id;
                                if let Some(suggestion) = app.suggestions.suggestions.iter().find(|s| s.id == suggestion_id) {
                                    if !suggest::llm::is_available() {
                                        app.show_toast("Run: cosmos --setup");
                                    } else {
                                        // Spawn fix generation
                                        let file_path = suggestion.file.clone();
                                        let summary = suggestion.summary.clone();
                                        let suggestion_clone = suggestion.clone();
                                        let repo_path_clone = repo_path.clone();
                                        let tx_fix = tx.clone();
                                        
                                        app.loading = LoadingState::GeneratingFix;
                                        app.close_overlay();
                                        
                                        tokio::spawn(async move {
                                            // Read file content
                                            let full_path = repo_path_clone.join(&file_path);
                                            let content = match std::fs::read_to_string(&full_path) {
                                                Ok(c) => c,
                                                Err(e) => {
                                                    let _ = tx_fix.send(BackgroundMessage::FixError(format!("Failed to read file: {}", e)));
                                                    return;
                                                }
                                            };
                                            
                                            match suggest::llm::generate_fix(&file_path, &content, &suggestion_clone).await {
                                                Ok(diff_preview) => {
                                                    let _ = tx_fix.send(BackgroundMessage::FixReady {
                                                        suggestion_id,
                                                        diff_preview,
                                                        file_path,
                                                        summary,
                                                        usage: None,
                                                    });
                                                }
                                                Err(e) => {
                                                    let _ = tx_fix.send(BackgroundMessage::FixError(e.to_string()));
                                                }
                                            }
                                        });
                                    }
                                }
                            }
                        }
                        KeyCode::Char('d') => {
                            if let Overlay::SuggestionDetail { suggestion_id, .. } = &app.overlay {
                                let id = *suggestion_id;
                                app.suggestions.dismiss(id);
                                app.show_toast("Dismissed");
                                app.close_overlay();
                            }
                        }
                        _ => {}
                    }
                    continue;
                }

                // Normal mode
                match key.code {
                    KeyCode::Char('q') => app.should_quit = true,
                    KeyCode::Esc => {
                        if !app.search_query.is_empty() {
                            app.exit_search();
                        } else if app.overlay != Overlay::None {
                            app.close_overlay();
                        }
                    }
                    KeyCode::Tab => app.toggle_panel(),
                    KeyCode::Down | KeyCode::Char('j') => app.navigate_down(),
                    KeyCode::Up | KeyCode::Char('k') => app.navigate_up(),
                    KeyCode::Enter => {
                        match app.active_panel {
                            ActivePanel::Project => app.show_file_detail(),
                            ActivePanel::Suggestions => app.show_suggestion_detail(),
                        }
                    }
                    KeyCode::Char('/') => app.start_search(),
                    KeyCode::Char('s') => app.cycle_sort(),
                    KeyCode::Char('?') => app.toggle_help(),
                    KeyCode::Char('d') => app.dismiss_selected(),
                    KeyCode::Char('a') => {
                        if let Some(suggestion) = app.selected_suggestion() {
                            if !suggest::llm::is_available() {
                                app.show_toast("Run: cosmos --setup");
                            } else {
                                // Spawn fix generation
                                let suggestion_id = suggestion.id;
                                let file_path = suggestion.file.clone();
                                let summary = suggestion.summary.clone();
                                let suggestion_clone = suggestion.clone();
                                let repo_path_clone = repo_path.clone();
                                let index_clone = index.clone();
                                let tx_fix = tx.clone();
                                
                                app.loading = LoadingState::GeneratingFix;
                                
                                tokio::spawn(async move {
                                    // Read file content
                                    let full_path = repo_path_clone.join(&file_path);
                                    let content = match std::fs::read_to_string(&full_path) {
                                        Ok(c) => c,
                                        Err(e) => {
                                            let _ = tx_fix.send(BackgroundMessage::FixError(format!("Failed to read file: {}", e)));
                                            return;
                                        }
                                    };
                                    
                                    match suggest::llm::generate_fix(&file_path, &content, &suggestion_clone).await {
                                        Ok(diff_preview) => {
                                            let _ = tx_fix.send(BackgroundMessage::FixReady {
                                                suggestion_id,
                                                diff_preview,
                                                file_path,
                                                summary,
                                                usage: None,
                                            });
                                        }
                                        Err(e) => {
                                            let _ = tx_fix.send(BackgroundMessage::FixError(e.to_string()));
                                        }
                                    }
                                });
                            }
                        }
                    }
                    KeyCode::Char('i') => {
                        if !suggest::llm::is_available() {
                            app.show_toast("Run: cosmos --setup");
                        } else {
                            app.show_toast("Inquiry coming soon...");
                        }
                    }
                    KeyCode::Char('r') => {
                        if let Err(e) = app.context.refresh() {
                            app.show_toast(&format!("Refresh failed: {}", e));
                        } else {
                            app.show_toast("Refreshed");
                        }
                    }
                    _ => {}
                }
            }
        }

        if app.should_quit {
            return Ok(());
        }
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        format!("{}...", &s[..max - 3])
    }
}

/// Apply a fix (diff) to a file
fn apply_fix(file_path: &PathBuf, diff_text: &str) -> Result<(), String> {
    use std::fs;
    
    // Try to parse as unified diff
    if let Ok(diff) = diff::parse_unified_diff(diff_text) {
        // Create backup
        let backup_path = diff::backup_file(file_path)?;
        
        // Apply the diff
        match diff::apply_diff_to_file(file_path, &diff) {
            Ok(_) => {
                // Remove backup on success
                let _ = fs::remove_file(&backup_path);
                Ok(())
            }
            Err(e) => {
                // Restore from backup on failure
                let _ = diff::restore_backup(file_path);
                Err(e)
            }
        }
    } else {
        // Try to extract code from the response and apply directly
        // Look for code blocks in the diff text
        if let Some(new_content) = extract_code_from_response(diff_text) {
            // Create backup
            let backup_path = file_path.with_extension("orig");
            fs::copy(file_path, &backup_path)
                .map_err(|e| format!("Failed to create backup: {}", e))?;
            
            // Write new content
            match fs::write(file_path, new_content) {
                Ok(_) => {
                    let _ = fs::remove_file(&backup_path);
                    Ok(())
                }
                Err(e) => {
                    // Restore backup
                    let _ = fs::copy(&backup_path, file_path);
                    let _ = fs::remove_file(&backup_path);
                    Err(format!("Failed to write file: {}", e))
                }
            }
        } else {
            Err("Could not parse diff format".to_string())
        }
    }
}

/// Extract code from LLM response that might contain markdown code blocks
fn extract_code_from_response(response: &str) -> Option<String> {
    // Look for code blocks with ```
    let lines: Vec<&str> = response.lines().collect();
    let mut in_code_block = false;
    let mut code_lines = Vec::new();
    
    for line in lines {
        if line.starts_with("```") {
            if in_code_block {
                // End of code block
                if !code_lines.is_empty() {
                    return Some(code_lines.join("\n"));
                }
            } else {
                // Start of code block
                in_code_block = true;
                code_lines.clear();
            }
        } else if in_code_block {
            code_lines.push(line);
        }
    }
    
    None
}
