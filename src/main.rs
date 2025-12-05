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
        
        // Spawn background task for file summaries
        let index_clone2 = index.clone();
        let tx_summaries = tx;
        
        tokio::spawn(async move {
            match suggest::llm::generate_file_summaries(&index_clone2).await {
                Ok((summaries, usage)) => {
                    let _ = tx_summaries.send(BackgroundMessage::SummariesReady { summaries, usage });
                }
                Err(e) => {
                    let _ = tx_summaries.send(BackgroundMessage::SummariesError(e.to_string()));
                }
            }
        });
    }

    // Main loop with async event handling
    let result = run_loop(&mut terminal, &mut app, rx);

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
                    app.update_summaries(summaries);
                    
                    // Track cost
                    if let Some(u) = usage {
                        let cost = u.calculate_cost(suggest::llm::Model::Opus);
                        app.session_cost += cost;
                        app.session_tokens += u.total_tokens;
                    }
                    
                    app.loading = LoadingState::None;
                    app.show_toast(&format!("Summaries ready · ${:.4}", app.session_cost));
                }
                BackgroundMessage::SummariesError(e) => {
                    app.loading = LoadingState::None;
                    app.show_toast(&format!("Summary error: {}", truncate(&e, 30)));
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
                    match key.code {
                        KeyCode::Esc | KeyCode::Char('q') => app.close_overlay(),
                        KeyCode::Down | KeyCode::Char('j') => app.overlay_scroll_down(),
                        KeyCode::Up | KeyCode::Char('k') => app.overlay_scroll_up(),
                        KeyCode::Char('a') => {
                            if let Overlay::SuggestionDetail { .. } = &app.overlay {
                                app.show_toast("Apply coming soon...");
                                app.close_overlay();
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
                        KeyCode::Char('y') => {
                            if let Overlay::ApplyConfirm { suggestion_id, .. } = &app.overlay {
                                let id = *suggestion_id;
                                app.suggestions.mark_applied(id);
                                app.show_toast("Applied!");
                                app.close_overlay();
                            }
                        }
                        KeyCode::Char('n') => {
                            if matches!(app.overlay, Overlay::ApplyConfirm { .. }) {
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
                        if app.selected_suggestion().is_some() {
                            app.show_toast("Apply coming soon...");
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
