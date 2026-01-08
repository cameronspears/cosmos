//! Cosmos - A contemplative vibe coding companion
//!
//! C O S M O S
//!
//! An AI-powered IDE in the terminal that uses codebase indexing
//! to suggest improvements, bug fixes, and optimizations.

mod cache;
mod config;
mod context;
mod grouping;
mod history;
mod index;
mod license;
mod onboarding;
mod safe_apply;
mod suggest;
mod ui;

// Keep these for compatibility during transition
mod git_ops;

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
    long_about = "C O S M O S\n\n\
                  A contemplative companion for your codebase.\n\n\
                  Uses AST-based indexing and AI to suggest improvements,\n\
                  bug fixes, features, and optimizations.",
    version
)]
struct Args {
    /// Path to the repository (defaults to current directory)
    #[arg(default_value = ".")]
    path: PathBuf,

    /// Set up OpenRouter API key for AI features (BYOK mode)
    #[arg(long)]
    setup: bool,

    /// Show stats and exit (no TUI)
    #[arg(long)]
    stats: bool,

    /// Activate a Cosmos Pro license
    #[arg(long, value_name = "LICENSE_KEY")]
    activate: Option<String>,

    /// Deactivate current license
    #[arg(long)]
    deactivate: bool,

    /// Show license and usage status
    #[arg(long)]
    status: bool,

    /// Show token usage statistics
    #[arg(long)]
    usage: bool,

    /// Generate a test license key (dev only)
    #[arg(long, hide = true)]
    generate_key: Option<String>,
}

/// Ritual mode arguments (invoked as: `cosmos ritual [PATH] --minutes N`)
#[derive(Parser, Debug)]
struct RitualArgs {
    /// Path to the repository (defaults to current directory)
    #[arg(default_value = ".")]
    path: PathBuf,

    /// Ritual length in minutes (default: 10)
    #[arg(long, default_value_t = 10)]
    minutes: u32,
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
    /// Incremental summary progress update
    SummaryProgress {
        completed: usize,
        total: usize,
        summaries: std::collections::HashMap<PathBuf, String>,
    },
    SummariesError(String),
    /// Quick preview ready (Phase 1 - fast)
    PreviewReady {
        suggestion_id: uuid::Uuid,
        file_path: PathBuf,
        summary: String,
        preview: suggest::llm::FixPreview,
    },
    PreviewError(String),
    /// Direct fix applied (Smart preset generated + applied the change)
    DirectFixApplied {
        suggestion_id: uuid::Uuid,
        file_path: PathBuf,
        description: String,
        modified_areas: Vec<String>,
        backup_path: PathBuf,
        safety_checks: Vec<safe_apply::CheckResult>,
        usage: Option<suggest::llm::Usage>,
        branch_name: String,
    },
    DirectFixError(String),
    /// Ship workflow progress update
    ShipProgress(ui::ShipStep),
    /// Ship workflow completed successfully with PR URL
    ShipComplete(String),
    /// Ship workflow error
    ShipError(String),
    /// AI code review completed
    ReviewReady(Vec<ui::PRReviewComment>),
    /// PR created successfully
    PRCreated(String), // PR URL
    /// Generic error (used for push/etc)
    Error(String),
    /// Response to a user question
    QuestionResponse {
        question: String,
        answer: String,
        usage: Option<suggest::llm::Usage>,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    // Lightweight “subcommand” without restructuring the whole CLI:
    // `cosmos ritual [PATH] --minutes N`
    let raw_args: Vec<String> = std::env::args().collect();
    if raw_args.get(1).map(|s| s == "ritual").unwrap_or(false) {
        let ritual_argv: Vec<String> = std::iter::once(raw_args[0].clone())
            .chain(raw_args.iter().skip(2).cloned())
            .collect();
        let ritual_args = RitualArgs::parse_from(ritual_argv);

        // Check for first run and show onboarding (same behavior as normal mode)
        if onboarding::is_first_run() {
            match onboarding::run_onboarding() {
                Ok(_) => eprintln!(),
                Err(e) => {
                    eprintln!("  Onboarding error: {}", e);
                    eprintln!("  Continuing without setup...");
                }
            }
        }

        let path = ritual_args.path.canonicalize()?;
        let cache_manager = cache::Cache::new(&path);
        let index = init_index(&path, &cache_manager)?;
        let context = init_context(&path)?;
        let suggestions = SuggestionEngine::new_empty(index.clone());

        return run_tui(index, suggestions, context, cache_manager, path, Some(ritual_args.minutes)).await;
    }

    let args = Args::parse();

    // Handle --setup flag (BYOK mode)
    if args.setup {
        return setup_api_key();
    }

    // Handle --activate flag
    if let Some(key) = args.activate {
        return activate_license(&key);
    }

    // Handle --deactivate flag
    if args.deactivate {
        return deactivate_license();
    }

    // Handle --status flag
    if args.status {
        license::show_status();
        return Ok(());
    }

    // Handle --usage flag
    if args.usage {
        return show_usage();
    }

    // Handle --generate-key flag (dev only)
    if let Some(tier) = args.generate_key {
        return generate_test_key(&tier);
    }

    // Check for first run and show onboarding
    if onboarding::is_first_run() {
        match onboarding::run_onboarding() {
            Ok(true) => {
                // Setup completed, continue to TUI
                eprintln!();
            }
            Ok(false) => {
                // Setup skipped, continue to TUI
                eprintln!();
            }
            Err(e) => {
                eprintln!("  Onboarding error: {}", e);
                eprintln!("  Continuing without setup...");
            }
        }
    }

    let path = args.path.canonicalize()?;

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
    run_tui(index, suggestions, context, cache_manager, path, None).await
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
    println!("  ║             C O S M O S   Stats                  ║");
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
    ritual_minutes: Option<u32>,
) -> Result<()> {
    // Set up terminal
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    // Create app with loading state
    let mut app = App::new(index.clone(), suggestions, context.clone());
    if let Some(mins) = ritual_minutes {
        app.start_ritual(mins);
    }
    // Load repo-local “memory” (decisions/conventions) from .cosmos/
    app.repo_memory = cache_manager.load_repo_memory();
    
    // Check if we have API access (and budgets allow it)
    let mut ai_enabled = suggest::llm::is_available();
    if ai_enabled {
        if let Err(e) = app.config.allow_ai(0.0) {
            ai_enabled = false;
            app.show_toast(&e);
        }
    }
    
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
    let total_files = file_hashes.len();
    
    if !cached_summaries.is_empty() {
        app.update_summaries(cached_summaries);
        eprintln!("  Loaded {} cached summaries ({} files total)", cached_count, total_files);
    }
    
    // Discover project context (for better quality summaries)
    let project_context = suggest::llm::discover_project_context(&index);
    llm_cache.set_project_context(project_context.clone());
    
    // Find files that need new/updated summaries
    let mut files_needing_summary = llm_cache.get_files_needing_summary(&file_hashes);

    // Optional privacy/cost control: only summarize changed files (and their immediate blast radius)
    if app.config.summarize_changed_only {
        let changed: std::collections::HashSet<PathBuf> = context
            .all_changed_files()
            .into_iter()
            .cloned()
            .collect();
        let mut wanted = changed.clone();
        for c in &changed {
            if let Some(file_index) = index.files.get(c) {
                for u in &file_index.summary.used_by {
                    wanted.insert(u.clone());
                }
                for d in &file_index.summary.depends_on {
                    wanted.insert(d.clone());
                }
            }
        }
        files_needing_summary.retain(|p| wanted.contains(p));
    }
    let needs_summary_count = files_needing_summary.len();
    
    // Track if we need to generate summaries (used to control loading state)
    app.needs_summary_generation = needs_summary_count > 0;
    
    if needs_summary_count > 0 {
        eprintln!("  {} files need summary generation", needs_summary_count);
    } else if cached_count > 0 {
        eprintln!("  All {} summaries loaded from cache ✓", cached_count);
    }
    
    if ai_enabled {
        app.loading = LoadingState::GeneratingSuggestions;
    }
    
    eprintln!();

    // Create channel for background tasks
    let (tx, rx) = mpsc::channel::<BackgroundMessage>();

    // Spawn background task for suggestions if AI enabled
    if ai_enabled {
        let index_clone = index.clone();
        let context_clone = context.clone();
        let tx_suggestions = tx.clone();
        let cache_clone_path = repo_path.clone();
        let repo_memory_context = app.repo_memory.to_prompt_context(12, 900);
        
        tokio::spawn(async move {
            let mem = if repo_memory_context.trim().is_empty() {
                None
            } else {
                Some(repo_memory_context)
            };
            match suggest::llm::analyze_codebase(&index_clone, &context_clone, mem).await {
                Ok((suggestions, usage)) => {
                    // Cache the suggestions
                    let cache = cache::Cache::new(&cache_clone_path);
                    let cache_data = cache::SuggestionsCache::from_suggestions(&suggestions);
                    let _ = cache.save_suggestions_cache(&cache_data);
                    
                    let _ = tx_suggestions.send(BackgroundMessage::SuggestionsReady {
                        suggestions,
                        usage,
                        model: "smart".to_string(),
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
            
            // Show initial cached count
            if cached_count > 0 {
                app.show_toast(&format!("{}/{} cached · generating {}", cached_count, total_files, needs_summary_count));
            }
            
            // Calculate total file count for progress
            let total_to_process = high_priority.len() + medium_priority.len() + low_priority.len();
            
            tokio::spawn(async move {
                let cache = cache::Cache::new(&cache_path);
                
                // Load existing cache to update incrementally
                let mut llm_cache = cache.load_llm_summaries_cache()
                    .unwrap_or_else(cache::LlmSummaryCache::new);
                
                let mut all_summaries = HashMap::new();
                let mut total_usage = suggest::llm::Usage::default();
                let mut completed_count = 0usize;
                
                // Process all priority tiers with parallel batching within each tier
                let priority_tiers = [
                    ("high", high_priority),
                    ("medium", medium_priority), 
                    ("low", low_priority),
                ];
                
                for (_tier_name, files) in priority_tiers {
                    if files.is_empty() {
                        continue;
                    }
                    
                    // Use large batch size (16 files) for faster processing
                    let batch_size = 16;
                    let batches: Vec<_> = files.chunks(batch_size).collect();
                    
                    // Process batches sequentially (llm.rs handles internal parallelism)
                    for batch in batches {
                        if let Ok((summaries, usage)) = suggest::llm::generate_summaries_for_files(
                            &index_clone2, batch, &project_context
                        ).await {
                            // Update cache with new summaries
                            for (path, summary) in &summaries {
                                if let Some(hash) = file_hashes_clone.get(path) {
                                    llm_cache.set_summary(path.clone(), summary.clone(), hash.clone());
                                }
                            }
                            // Save cache incrementally after each batch
                            let _ = cache.save_llm_summaries_cache(&llm_cache);
                            
                            completed_count += summaries.len();
                            
                            // Send progress update with new summaries
                            let _ = tx_summaries.send(BackgroundMessage::SummaryProgress {
                                completed: completed_count,
                                total: total_to_process,
                                summaries: summaries.clone(),
                            });
                            
                            all_summaries.extend(summaries);
                            if let Some(u) = usage {
                                total_usage.prompt_tokens += u.prompt_tokens;
                                total_usage.completion_tokens += u.completion_tokens;
                                total_usage.total_tokens += u.total_tokens;
                            }
                        }
                    }
                }
                
                let final_usage = if total_usage.total_tokens > 0 {
                    Some(total_usage)
                } else {
                    None
                };
                
                // Send final message (summaries already sent via progress, so send empty)
                let _ = tx_summaries.send(BackgroundMessage::SummariesReady { 
                    summaries: HashMap::new(), 
                    usage: final_usage 
                });
            });
        } else {
            // All summaries are cached, no need to generate anything
            // Note: loading state will be set to None when suggestions finish
            // since needs_summary_generation is false
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
    // Track last git status refresh time
    let mut last_git_refresh = std::time::Instant::now();
    const GIT_REFRESH_INTERVAL: std::time::Duration = std::time::Duration::from_secs(2);
    
    loop {
        // Clear expired toasts
        app.clear_expired_toast();

        // Advance spinner animation
        app.tick_loading();
        
        // Periodically refresh git status (every 2 seconds)
        if last_git_refresh.elapsed() >= GIT_REFRESH_INTERVAL {
            let _ = app.context.refresh();
            last_git_refresh = std::time::Instant::now();
        }

        // Check for background messages (non-blocking)
        while let Ok(msg) = rx.try_recv() {
            match msg {
                BackgroundMessage::SuggestionsReady { suggestions, usage, model } => {
                    let count = suggestions.len();
                    for s in suggestions {
                        app.suggestions.add_llm_suggestion(s);
                    }

                    // Diff-first ordering: changed files and their blast radius float to the top.
                    app.suggestions.sort_with_context(&app.context);

                    // If ritual mode is active and empty, populate its queue now.
                    app.populate_ritual_items_if_possible();
                    
                    // Track cost (Smart preset for suggestions)
                    if let Some(u) = usage {
                        let cost = u.calculate_cost(suggest::llm::Model::Smart);
                        app.session_cost += cost;
                        app.session_tokens += u.total_tokens;
                        let _ = app.config.record_tokens(u.total_tokens);
                        let _ = app.config.allow_ai(app.session_cost).map_err(|e| app.show_toast(&e));
                    }
                    
                    // Summaries generate silently in background - no blocking overlay
                    app.loading = LoadingState::None;
                    
                    // More prominent toast for suggestions
                    app.show_toast(&format!("{} suggestions ready ({})", count, &model));
                    app.active_model = Some(model);
                }
                BackgroundMessage::SuggestionsError(e) => {
                    // Summaries generate silently in background - no blocking overlay
                    app.loading = LoadingState::None;
                    app.show_toast(&format!("Error: {}", truncate(&e, 40)));
                }
                BackgroundMessage::SummariesReady { summaries, usage } => {
                    let new_count = summaries.len();
                    app.update_summaries(summaries);

                    // Track cost (using Speed preset for summaries)
                    if let Some(u) = usage {
                        let cost = u.calculate_cost(suggest::llm::Model::Speed);
                        app.session_cost += cost;
                        app.session_tokens += u.total_tokens;
                        let _ = app.config.record_tokens(u.total_tokens);
                        let _ = app.config.allow_ai(app.session_cost).map_err(|e| app.show_toast(&e));
                    }
                    
                    app.loading = LoadingState::None;
                    app.summary_progress = None;
                    if new_count > 0 {
                        app.show_toast(&format!("{} new summaries · ${:.4}", new_count, app.session_cost));
                    } else {
                        app.show_toast("All summaries loaded from cache");
                    }
                }
                BackgroundMessage::SummaryProgress { completed, total: _, summaries } => {
                    // Silently merge new summaries as they arrive (no progress UI)
                    app.update_summaries(summaries);
                    // Track progress internally but don't display it
                    app.summary_progress = Some((completed, 0)); // Keep for internal tracking only
                }
                BackgroundMessage::SummariesError(e) => {
                    app.loading = LoadingState::None;
                    app.summary_progress = None;
                    app.show_toast(&format!("Summary error: {}", truncate(&e, 30)));
                }
                BackgroundMessage::PreviewReady { suggestion_id, file_path, summary, preview } => {
                    app.loading = LoadingState::None;
                    app.show_fix_preview(suggestion_id, file_path, summary, preview);
                }
                BackgroundMessage::PreviewError(e) => {
                    app.loading = LoadingState::None;
                    app.show_toast(&format!("Preview error: {}", truncate(&e, 40)));
                }
                BackgroundMessage::DirectFixApplied { suggestion_id, file_path, description, modified_areas, backup_path, safety_checks, usage, branch_name } => {
                    // Track cost
                    if let Some(u) = usage {
                        let cost = u.calculate_cost(suggest::llm::Model::Speed);
                        app.session_cost += cost;
                        app.session_tokens += u.total_tokens;
                        let _ = app.config.record_tokens(u.total_tokens);
                        let _ = app.config.allow_ai(app.session_cost).map_err(|e| app.show_toast(&e));
                    }
                    
                    app.loading = LoadingState::None;
                    app.suggestions.mark_applied(suggestion_id);
                    
                    // Store the cosmos branch name - this enables the Ship workflow
                    app.cosmos_branch = Some(branch_name.clone());
                    
                    // Track as pending change for batch commit
                    // Generate a simple diff description
                    let diff = format!("Modified areas: {}", modified_areas.join(", "));
                    app.add_pending_change(suggestion_id, file_path.clone(), description.clone(), diff, backup_path.clone());
                    
                    // Show a calm confidence report (and how to undo)
                    app.show_safe_apply_report(description.clone(), file_path.clone(), branch_name.clone(), backup_path, safety_checks);
                    
                    // Show success with branch name and hint about Ship
                    let pending = app.pending_change_count();
                    let short_branch = branch_name.split('/').last().unwrap_or(&branch_name);
                    if pending > 1 {
                        app.show_toast(&format!("✓ {} on {} · {} pending · 's' to Ship", truncate(&description, 20), short_branch, pending));
                    } else {
                        app.show_toast(&format!("✓ {} on {} · 's' to Ship", truncate(&description, 25), short_branch));
                    }
                }
                BackgroundMessage::DirectFixError(e) => {
                    app.loading = LoadingState::None;
                    app.show_toast(&format!("Apply failed: {}", truncate(&e, 40)));
                }
                BackgroundMessage::ShipProgress(step) => {
                    app.update_ship_step(step);
                }
                BackgroundMessage::ShipComplete(url) => {
                    app.update_ship_step(ui::ShipStep::Done);
                    app.show_toast(&format!("✓ PR created: {}", truncate(&url, 35)));
                    // Store URL for opening
                    if let Overlay::ShipDialog { .. } = &app.overlay {
                        // The PR URL is in the toast; user can press Enter to close
                    }
                    // Clear pending changes since they're now shipped
                    app.clear_pending_changes();
                }
                BackgroundMessage::ShipError(e) => {
                    app.close_overlay();
                    app.show_toast(&format!("Ship failed: {}", truncate(&e, 40)));
                }
                BackgroundMessage::ReviewReady(comments) => {
                    app.set_review_comments(comments);
                }
                BackgroundMessage::PRCreated(url) => {
                    app.set_pr_url(url.clone());
                    app.show_toast(&format!("PR created: {}", truncate(&url, 35)));
                }
                BackgroundMessage::Error(e) => {
                    app.show_toast(&truncate(&e, 50));
                }
                BackgroundMessage::QuestionResponse { question, answer, usage } => {
                    // Track cost
                    if let Some(u) = usage {
                        let cost = u.calculate_cost(suggest::llm::Model::Speed);
                        app.session_cost += cost;
                        app.session_tokens += u.total_tokens;
                        let _ = app.config.record_tokens(u.total_tokens);
                        let _ = app.config.allow_ai(app.session_cost).map_err(|e| app.show_toast(&e));
                    }
                    
                    app.loading = LoadingState::None;
                    // Show the response in the inquiry overlay
                    let response = format!("Q: {}\n\n{}", question, answer);
                    app.show_inquiry(response);
                }
            }
        }

        // Render
        terminal.draw(|f| ui::render(f, app))?;

        // Poll for events with fast timeout (snappy animations)
        if event::poll(Duration::from_millis(50))? {
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
                
                // Handle question input mode
                if app.input_mode == InputMode::Question {
                    match key.code {
                        KeyCode::Esc => app.exit_question(),
                        KeyCode::Enter => {
                            let question = app.take_question();
                            if !question.is_empty() {
                                // Privacy preview (what will be sent) before the network call
                                if app.config.privacy_preview {
                                    app.show_inquiry_preview(question);
                                } else {
                                    if let Err(e) = app.config.allow_ai(app.session_cost) {
                                        app.show_toast(&e);
                                        continue;
                                    }
                                    // Send question to LLM
                                    let index_clone = index.clone();
                                    let context_clone = app.context.clone();
                                    let tx_question = tx.clone();
                                    let repo_memory_context = app.repo_memory.to_prompt_context(12, 900);
                                    
                                    app.loading = LoadingState::Answering;
                                    
                                    tokio::spawn(async move {
                                        let mem = if repo_memory_context.trim().is_empty() {
                                            None
                                        } else {
                                            Some(repo_memory_context)
                                        };
                                        match suggest::llm::ask_question(&index_clone, &context_clone, &question, mem).await {
                                            Ok((answer, usage)) => {
                                                let _ = tx_question.send(BackgroundMessage::QuestionResponse {
                                                    question,
                                                    answer,
                                                    usage,
                                                });
                                            }
                                            Err(e) => {
                                                let _ = tx_question.send(BackgroundMessage::Error(e.to_string()));
                                            }
                                        }
                                    });
                                }
                            }
                        }
                        KeyCode::Backspace => app.question_pop(),
                        KeyCode::Char(c) => app.question_push(c),
                        _ => {}
                    }
                    continue;
                }

                // Handle overlay mode
                if app.overlay != Overlay::None {
                    // Inquiry privacy preview overlay
                    if let Overlay::InquiryPreview { question, .. } = &app.overlay {
                        match key.code {
                            KeyCode::Esc | KeyCode::Char('q') => app.close_overlay(),
                            KeyCode::Down | KeyCode::Char('j') => app.overlay_scroll_down(),
                            KeyCode::Up | KeyCode::Char('k') => app.overlay_scroll_up(),
                            KeyCode::Enter => {
                                if let Err(e) = app.config.allow_ai(app.session_cost) {
                                    app.show_toast(&e);
                                    continue;
                                }
                                let question = question.clone();
                                let index_clone = index.clone();
                                let context_clone = app.context.clone();
                                let tx_question = tx.clone();
                                let repo_memory_context = app.repo_memory.to_prompt_context(12, 900);
                                app.loading = LoadingState::Answering;
                                app.close_overlay();
                                tokio::spawn(async move {
                                    let mem = if repo_memory_context.trim().is_empty() {
                                        None
                                    } else {
                                        Some(repo_memory_context)
                                    };
                                    match suggest::llm::ask_question(&index_clone, &context_clone, &question, mem).await {
                                        Ok((answer, usage)) => {
                                            let _ = tx_question.send(BackgroundMessage::QuestionResponse {
                                                question,
                                                answer,
                                                usage,
                                            });
                                        }
                                        Err(e) => {
                                            let _ = tx_question.send(BackgroundMessage::Error(e.to_string()));
                                        }
                                    }
                                });
                            }
                            _ => {}
                        }
                        continue;
                    }

                    // Repo memory overlay
                    if let Overlay::RepoMemory { mode, .. } = &app.overlay {
                        match *mode {
                            ui::RepoMemoryMode::Add => match key.code {
                                KeyCode::Esc => app.memory_cancel_add(),
                                KeyCode::Enter => match app.memory_commit_add() {
                                    Ok(()) => app.show_toast("Saved to repo memory"),
                                    Err(e) => app.show_toast(&e),
                                },
                                KeyCode::Backspace => app.memory_input_pop(),
                                KeyCode::Char(c) => app.memory_input_push(c),
                                _ => {}
                            },
                            ui::RepoMemoryMode::View => match key.code {
                                KeyCode::Esc | KeyCode::Char('q') => app.close_overlay(),
                                KeyCode::Down | KeyCode::Char('j') => app.memory_move(1),
                                KeyCode::Up | KeyCode::Char('k') => app.memory_move(-1),
                                KeyCode::Char('a') => app.memory_start_add(),
                                _ => {}
                            },
                        }
                        continue;
                    }

                    // Safe Apply report overlay
                    if let Overlay::SafeApplyReport { .. } = &app.overlay {
                        match key.code {
                            KeyCode::Esc | KeyCode::Char('q') => app.close_overlay(),
                            KeyCode::Down | KeyCode::Char('j') => app.overlay_scroll_down(),
                            KeyCode::Up | KeyCode::Char('k') => app.overlay_scroll_up(),
                            KeyCode::Char('u') => {
                                match app.undo_last_pending_change() {
                                    Ok(()) => {
                                        app.show_toast("Undone (restored backup)");
                                        app.close_overlay();
                                    }
                                    Err(e) => app.show_toast(&e),
                                }
                            }
                            _ => {}
                        }
                        continue;
                    }

                    // Ritual overlay
                    if let Overlay::Ritual { .. } = &app.overlay {
                        match key.code {
                            KeyCode::Esc | KeyCode::Char('q') => {
                                app.close_ritual_overlay();
                            }
                            KeyCode::Down | KeyCode::Char('j') => app.ritual_move(1),
                            KeyCode::Up | KeyCode::Char('k') => app.ritual_move(-1),
                            KeyCode::Char('x') => {
                                app.ritual_set_selected_status(ui::RitualItemStatus::Done);
                                app.show_toast("Marked done");
                            }
                            KeyCode::Char('s') => {
                                app.ritual_set_selected_status(ui::RitualItemStatus::Skipped);
                                app.show_toast("Skipped");
                            }
                            KeyCode::Char('d') => {
                                if let Some(s) = app.selected_ritual_suggestion() {
                                    let id = s.id;
                                    app.suggestions.dismiss(id);
                                    app.ritual_set_selected_status(ui::RitualItemStatus::Dismissed);
                                    app.show_toast("Dismissed");
                                }
                            }
                            KeyCode::Enter => {
                                if let Some(s) = app.selected_ritual_suggestion() {
                                    let id = s.id;
                                    app.overlay = Overlay::SuggestionDetail { suggestion_id: id, scroll: 0 };
                                }
                            }
                            KeyCode::Char('a') => {
                                let suggestion = app.selected_ritual_suggestion().cloned();
                                if let Some(suggestion) = suggestion {
                                    if !suggest::llm::is_available() {
                                        app.show_toast("Run: cosmos --setup");
                                    } else {
                                        if let Err(e) = app.config.allow_ai(app.session_cost) {
                                            app.show_toast(&e);
                                            continue;
                                        }
                                        let suggestion_id = suggestion.id;
                                        let file_path = suggestion.file.clone();
                                        let summary = suggestion.summary.clone();
                                        let suggestion_clone = suggestion.clone();
                                        let tx_preview = tx.clone();
                                        let repo_memory_context = app.repo_memory.to_prompt_context(12, 900);
                                        app.loading = LoadingState::GeneratingPreview;
                                        app.close_overlay();
                                        tokio::spawn(async move {
                                            let mem = if repo_memory_context.trim().is_empty() {
                                                None
                                            } else {
                                                Some(repo_memory_context)
                                            };
                                            match suggest::llm::generate_fix_preview(&file_path, &suggestion_clone, None, mem).await {
                                                Ok(preview) => {
                                                    let _ = tx_preview.send(BackgroundMessage::PreviewReady {
                                                        suggestion_id,
                                                        file_path,
                                                        summary,
                                                        preview,
                                                    });
                                                }
                                                Err(e) => {
                                                    let _ = tx_preview.send(BackgroundMessage::PreviewError(e.to_string()));
                                                }
                                            }
                                        });
                                    }
                                }
                            }
                            _ => {}
                        }
                        continue;
                    }

                    // Handle FixPreview overlay (Phase 1 - fast preview)
                    if let Overlay::FixPreview { suggestion_id, file_path, modifier_input, .. } = &app.overlay {
                        let suggestion_id = *suggestion_id;
                        let file_path = file_path.clone();
                        let modifier_input = modifier_input.clone();
                        let is_typing_modifier = !modifier_input.is_empty();
                        
                        match key.code {
                            KeyCode::Esc => {
                                if is_typing_modifier {
                                    // Clear modifier and stay in preview
                                    if let Overlay::FixPreview { modifier_input, .. } = &mut app.overlay {
                                        modifier_input.clear();
                                    }
                                } else {
                                    app.close_overlay();
                                }
                            }
                            KeyCode::Char('n') if !is_typing_modifier => {
                                app.close_overlay();
                            }
                            KeyCode::Char('y') if !is_typing_modifier => {
                                if let Err(e) = app.config.allow_ai(app.session_cost) {
                                    app.show_toast(&e);
                                    continue;
                                }
                                // Phase 2: Generate and apply fix with Smart preset
                                let preview = if let Overlay::FixPreview { preview, .. } = &app.overlay {
                                    preview.clone()
                                } else {
                                    continue;
                                };
                                
                                // Show warning toast if proceeding with unverified fix
                                if !preview.verified {
                                    app.show_toast("Proceeding with unverified fix...");
                                }
                                
                                if let Some(suggestion) = app.suggestions.suggestions.iter().find(|s| s.id == suggestion_id) {
                                    
                                    let suggestion_clone = suggestion.clone();
                                    let repo_path_clone = repo_path.clone();
                                    let tx_fix = tx.clone();
                                    let file_path_clone = file_path.clone();
                                    
                                    app.loading = LoadingState::GeneratingFix;
                                    app.close_overlay();
                                    
                                    tokio::spawn(async move {
                                        // Create a new branch from main before applying the fix
                                        let branch_name = git_ops::generate_fix_branch_name(
                                            &suggestion_clone.id.to_string(),
                                            &suggestion_clone.summary
                                        );
                                        
                                        // Try to create and checkout the fix branch from main
                                        let created_branch = match git_ops::create_fix_branch_from_main(&repo_path_clone, &branch_name) {
                                            Ok(name) => name,
                                            Err(e) => {
                                                let _ = tx_fix.send(BackgroundMessage::DirectFixError(
                                                    format!("Failed to create fix branch: {}. Please ensure you have no uncommitted changes and 'main' or 'master' branch exists.", e)
                                                ));
                                                return;
                                            }
                                        };
                                        
                                        let full_path = repo_path_clone.join(&file_path_clone);
                                        let content = match std::fs::read_to_string(&full_path) {
                                            Ok(c) => c,
                                            Err(e) => {
                                                let _ = tx_fix.send(BackgroundMessage::DirectFixError(format!("Failed to read file: {}", e)));
                                                return;
                                            }
                                        };
                                        
                                        // Generate the fix content using the human plan (+ repo memory)
                                        let mem = crate::cache::Cache::new(&repo_path_clone)
                                            .load_repo_memory()
                                            .to_prompt_context(12, 900);
                                        let mem = if mem.trim().is_empty() { None } else { Some(mem) };
                                        match suggest::llm::generate_fix_content(&file_path_clone, &content, &suggestion_clone, &preview, mem).await {
                                            Ok(applied_fix) => {
                                                // Create backup before applying
                                                let backup_path = full_path.with_extension("cosmos.bak");
                                                if let Err(e) = std::fs::copy(&full_path, &backup_path) {
                                                    let _ = tx_fix.send(BackgroundMessage::DirectFixError(format!("Failed to create backup: {}", e)));
                                                    return;
                                                }
                                                
                                                // Write the new content
                                                match std::fs::write(&full_path, &applied_fix.new_content) {
                                                    Ok(_) => {
                                                        // Run fast local checks for confidence (best-effort)
                                                        let safety_checks = crate::safe_apply::run(&repo_path_clone);

                                                        let _ = tx_fix.send(BackgroundMessage::DirectFixApplied {
                                                            suggestion_id,
                                                            file_path: file_path_clone,
                                                            description: applied_fix.description,
                                                            modified_areas: applied_fix.modified_areas,
                                                            backup_path,
                                                            safety_checks,
                                                            usage: applied_fix.usage,
                                                            branch_name: created_branch,
                                                        });
                                                    }
                                                    Err(e) => {
                                                        // Restore backup on failure
                                                        let _ = std::fs::copy(&backup_path, &full_path);
                                                        let _ = std::fs::remove_file(&backup_path);
                                                        let _ = tx_fix.send(BackgroundMessage::DirectFixError(format!("Failed to write fix: {}", e)));
                                                    }
                                                }
                                            }
                                            Err(e) => {
                                                let _ = tx_fix.send(BackgroundMessage::DirectFixError(e.to_string()));
                                            }
                                        }
                                    });
                                }
                            }
                            KeyCode::Char('m') if !is_typing_modifier => {
                                // Start typing modifier
                                app.preview_modifier_push(' ');  // Add space to trigger typing mode
                                if let Overlay::FixPreview { modifier_input, .. } = &mut app.overlay {
                                    modifier_input.clear();  // Then clear it
                                }
                            }
                            KeyCode::Enter if is_typing_modifier => {
                                // Regenerate preview with modifier
                                if let Some(suggestion) = app.suggestions.suggestions.iter().find(|s| s.id == suggestion_id) {
                                    if let Err(e) = app.config.allow_ai(app.session_cost) {
                                        app.show_toast(&e);
                                        continue;
                                    }
                                    let suggestion_clone = suggestion.clone();
                                    let summary = suggestion.summary.clone();
                                    let file_path_clone = file_path.clone();
                                    let modifier = if modifier_input.is_empty() { None } else { Some(modifier_input.clone()) };
                                    let tx_preview = tx.clone();
                                    let repo_memory_context = app.repo_memory.to_prompt_context(12, 900);
                                    
                                    app.loading = LoadingState::GeneratingPreview;
                                    app.close_overlay();
                                    
                                    tokio::spawn(async move {
                                        let mem = if repo_memory_context.trim().is_empty() {
                                            None
                                        } else {
                                            Some(repo_memory_context)
                                        };
                                        match suggest::llm::generate_fix_preview(&file_path_clone, &suggestion_clone, modifier.as_deref(), mem).await {
                                            Ok(preview) => {
                                                let _ = tx_preview.send(BackgroundMessage::PreviewReady {
                                                    suggestion_id,
                                                    file_path: file_path_clone,
                                                    summary,
                                                    preview,
                                                });
                                            }
                                            Err(e) => {
                                                let _ = tx_preview.send(BackgroundMessage::PreviewError(e.to_string()));
                                            }
                                        }
                                    });
                                }
                            }
                            KeyCode::Char('d') if !is_typing_modifier => {
                                // Dismiss the suggestion (especially useful when not verified)
                                app.suggestions.dismiss(suggestion_id);
                                app.show_toast("Dismissed");
                                app.close_overlay();
                            }
                            KeyCode::Backspace if is_typing_modifier => {
                                app.preview_modifier_pop();
                            }
                            KeyCode::Char(c) if is_typing_modifier => {
                                app.preview_modifier_push(c);
                            }
                            _ => {}
                        }
                        continue;
                    }
                    
                    // Handle BranchCreate overlay
                    if let Overlay::BranchCreate { branch_name, commit_message, pending_files } = &app.overlay {
                        match key.code {
                            KeyCode::Esc | KeyCode::Char('q') => app.close_overlay(),
                            KeyCode::Char('y') => {
                                // Execute branch creation and commit
                                let repo_path = app.repo_path.clone();
                                let branch = branch_name.clone();
                                let message = commit_message.clone();
                                let files = pending_files.clone();
                                
                                app.close_overlay();
                                app.show_toast("Creating branch...");
                                
                                // Create branch, stage files, commit, and push
                                match git_ops::create_and_checkout_branch(&repo_path, &branch) {
                                    Ok(()) => {
                                        // Stage all pending files
                                        for file in &files {
                                            if let Some(rel_path) = file.strip_prefix(&repo_path).ok().and_then(|p| p.to_str()) {
                                                let _ = git_ops::stage_file(&repo_path, rel_path);
                                            }
                                        }
                                        
                                        // Commit
                                        match git_ops::commit(&repo_path, &message) {
                                            Ok(_) => {
                                                app.cosmos_branch = Some(branch.clone());
                                                
                                                // Try to push (non-blocking)
                                                let repo_for_push = repo_path.clone();
                                                let branch_for_push = branch.clone();
                                                let tx_push = tx.clone();
                                                tokio::spawn(async move {
                                                    match git_ops::push_branch(&repo_for_push, &branch_for_push) {
                                                        Ok(_) => {
                                                            let _ = tx_push.send(BackgroundMessage::Error("Pushed! Press 'p' for PR".to_string()));
                                                        }
                                                        Err(e) => {
                                                            let _ = tx_push.send(BackgroundMessage::Error(format!("Push failed: {}", e)));
                                                        }
                                                    }
                                                });
                                                
                                                app.show_toast("Branch created and committed");
                                            }
                                            Err(e) => {
                                                app.show_toast(&format!("Commit failed: {}", e));
                                            }
                                        }
                                    }
                                    Err(e) => {
                                        app.show_toast(&format!("Branch failed: {}", e));
                                    }
                                }
                            }
                            _ => {}
                        }
                        continue;
                    }
                    
                    // Handle ShipDialog overlay - streamlined commit + push + PR flow
                    if let Overlay::ShipDialog { branch_name, commit_message, files, step } = &app.overlay {
                        let step = *step;
                        match key.code {
                            KeyCode::Esc | KeyCode::Char('q') => {
                                if step == ui::ShipStep::Confirm || step == ui::ShipStep::Done {
                                    app.close_overlay();
                                }
                                // Don't allow cancel during in-progress steps
                            }
                            KeyCode::Char('y') if step == ui::ShipStep::Confirm => {
                                // Execute the full ship workflow: stage → commit → push → PR
                                let repo_path = app.repo_path.clone();
                                let branch = branch_name.clone();
                                let message = commit_message.clone();
                                let files = files.clone();
                                let tx_ship = tx.clone();
                                
                                app.update_ship_step(ui::ShipStep::Committing);
                                
                                tokio::spawn(async move {
                                    // Step 1: Stage all files
                                    for file in &files {
                                        if let Some(rel_path) = file.strip_prefix(&repo_path).ok().and_then(|p| p.to_str()) {
                                            let _ = git_ops::stage_file(&repo_path, rel_path);
                                        }
                                    }
                                    
                                    // Step 2: Commit
                                    if let Err(e) = git_ops::commit(&repo_path, &message) {
                                        let _ = tx_ship.send(BackgroundMessage::ShipError(format!("Commit failed: {}", e)));
                                        return;
                                    }
                                    let _ = tx_ship.send(BackgroundMessage::ShipProgress(ui::ShipStep::Pushing));
                                    
                                    // Step 3: Push
                                    if let Err(e) = git_ops::push_branch(&repo_path, &branch) {
                                        let _ = tx_ship.send(BackgroundMessage::ShipError(format!("Push failed: {}", e)));
                                        return;
                                    }
                                    let _ = tx_ship.send(BackgroundMessage::ShipProgress(ui::ShipStep::CreatingPR));
                                    
                                    // Step 4: Create PR
                                    let pr_title = message.lines().next().unwrap_or("Cosmos fix").to_string();
                                    let pr_body = format!(
                                        "## Summary\n\nAutomated fix applied by Cosmos.\n\n{}\n\n---\n*Created with [Cosmos](https://cosmos.dev)*",
                                        message
                                    );
                                    
                                    match git_ops::create_pr(&repo_path, &pr_title, &pr_body) {
                                        Ok(url) => {
                                            let _ = tx_ship.send(BackgroundMessage::ShipComplete(url));
                                        }
                                        Err(e) => {
                                            // PR creation failed but commit/push succeeded
                                            let _ = tx_ship.send(BackgroundMessage::ShipError(
                                                format!("Pushed, but PR creation failed: {}. Create PR manually.", e)
                                            ));
                                        }
                                    }
                                });
                            }
                            KeyCode::Enter if step == ui::ShipStep::Done => {
                                // Open the PR URL (stored in a toast or elsewhere)
                                // For now, just close
                                app.clear_pending_changes();
                                app.close_overlay();
                            }
                            _ => {}
                        }
                        continue;
                    }
                    
                    // Handle PRReview overlay
                    if let Overlay::PRReview { branch_name, files_changed, reviewing, pr_url, .. } = &app.overlay {
                        match key.code {
                            KeyCode::Esc | KeyCode::Char('q') => app.close_overlay(),
                            KeyCode::Down | KeyCode::Char('j') => app.overlay_scroll_down(),
                            KeyCode::Up | KeyCode::Char('k') => app.overlay_scroll_up(),
                            KeyCode::Char('r') => {
                                if !*reviewing {
                                    // Start AI review
                                    let files = files_changed.clone();
                                    let tx_review = tx.clone();
                                    
                                    // Set reviewing state
                                    if let Overlay::PRReview { reviewing, .. } = &mut app.overlay {
                                        *reviewing = true;
                                    }
                                    
                                    tokio::spawn(async move {
                                        match suggest::llm::review_changes(&files).await {
                                            Ok((comments, _usage)) => {
                                                let _ = tx_review.send(BackgroundMessage::ReviewReady(comments));
                                            }
                                            Err(e) => {
                                                let _ = tx_review.send(BackgroundMessage::Error(e.to_string()));
                                            }
                                        }
                                    });
                                }
                            }
                            KeyCode::Char('c') => {
                                // Create PR
                                if pr_url.is_none() {
                                    let repo_path = app.repo_path.clone();
                                    let branch = branch_name.clone();
                                    let tx_pr = tx.clone();
                                    
                                    app.show_toast("Creating PR...");
                                    
                                    tokio::spawn(async move {
                                        let title = format!("Cosmos: {}", branch);
                                        let body = "Changes applied via Cosmos code companion.";
                                        match git_ops::create_pr(&repo_path, &title, body) {
                                            Ok(url) => {
                                                let _ = tx_pr.send(BackgroundMessage::PRCreated(url));
                                            }
                                            Err(e) => {
                                                let _ = tx_pr.send(BackgroundMessage::Error(e.to_string()));
                                            }
                                        }
                                    });
                                }
                            }
                            KeyCode::Char('o') => {
                                // Open PR in browser
                                if let Some(url) = pr_url {
                                    let _ = git_ops::open_url(url);
                                }
                            }
                            _ => {}
                        }
                        continue;
                    }
                    
                    // Handle GitStatus overlay - simplified interface
                    if let Overlay::GitStatus { commit_input, .. } = &app.overlay {
                        // Check if we're in commit input mode
                        if commit_input.is_some() {
                            match key.code {
                                KeyCode::Esc => {
                                    app.git_cancel_commit();
                                }
                                KeyCode::Enter => {
                                    match app.git_do_commit() {
                                        Ok(_oid) => {
                                            app.show_toast("✓ Committed · Press 's' to Ship");
                                            app.close_overlay();
                                        }
                                        Err(e) => {
                                            app.show_toast(&e);
                                        }
                                    }
                                }
                                KeyCode::Backspace => {
                                    app.git_commit_pop();
                                }
                                KeyCode::Char(c) => {
                                    app.git_commit_push(c);
                                }
                                _ => {}
                            }
                        } else {
                            // Clean, simple navigation and actions
                            match key.code {
                                KeyCode::Esc | KeyCode::Char('q') => {
                                    app.close_overlay();
                                }
                                KeyCode::Down | KeyCode::Char('j') => {
                                    app.git_status_navigate(1);
                                }
                                KeyCode::Up | KeyCode::Char('k') => {
                                    app.git_status_navigate(-1);
                                }
                                KeyCode::Char('s') | KeyCode::Enter => {
                                    // Stage selected file (Enter as alias for convenience)
                                    app.git_stage_selected();
                                }
                                KeyCode::Char('u') => {
                                    // Unstage selected file
                                    app.git_unstage_selected();
                                }
                                KeyCode::Char('c') => {
                                    // Start commit
                                    app.git_start_commit();
                                }
                                // Legacy/advanced keys (still work but not shown)
                                KeyCode::Char('S') => {
                                    app.git_stage_all();
                                }
                                KeyCode::Char('r') => {
                                    app.git_restore_selected();
                                }
                                KeyCode::Char('R') => {
                                    app.refresh_git_status();
                                }
                                KeyCode::Char('P') => {
                                    match app.git_push() {
                                        Ok(_) => app.show_toast("Pushed successfully"),
                                        Err(e) => app.show_toast(&e),
                                    }
                                }
                                KeyCode::Char('m') => {
                                    // Switch to main branch
                                    if app.is_on_main_branch() {
                                        app.show_toast("Already on main");
                                    } else {
                                        match app.git_switch_to_main() {
                                            Ok(_) => {
                                                app.show_toast("Switched to main");
                                                app.close_overlay();
                                            }
                                            Err(e) => app.show_toast(&e),
                                        }
                                    }
                                }
                                KeyCode::Char('X') => {
                                    match app.git_reset_hard() {
                                        Ok(_) => app.show_toast("Branch reset to clean state"),
                                        Err(e) => {
                                            app.show_toast(&e);
                                        }
                                    }
                                }
                                _ => {}
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
                                        if let Err(e) = app.config.allow_ai(app.session_cost) {
                                            app.show_toast(&e);
                                            continue;
                                        }
                                        // Phase 1: Generate quick preview (fast)
                                        let file_path = suggestion.file.clone();
                                        let summary = suggestion.summary.clone();
                                        let suggestion_clone = suggestion.clone();
                                        let tx_preview = tx.clone();
                                        let repo_memory_context = app.repo_memory.to_prompt_context(12, 900);
                                        
                                        app.loading = LoadingState::GeneratingPreview;
                                        app.close_overlay();
                                        
                                        tokio::spawn(async move {
                                            let mem = if repo_memory_context.trim().is_empty() {
                                                None
                                            } else {
                                                Some(repo_memory_context)
                                            };
                                            match suggest::llm::generate_fix_preview(&file_path, &suggestion_clone, None, mem).await {
                                                Ok(preview) => {
                                                    let _ = tx_preview.send(BackgroundMessage::PreviewReady {
                                                        suggestion_id,
                                                        file_path,
                                                        summary,
                                                        preview,
                                                    });
                                                }
                                                Err(e) => {
                                                    let _ = tx_preview.send(BackgroundMessage::PreviewError(e.to_string()));
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
                    KeyCode::Char('S') => app.cycle_sort(),  // Shift+S to cycle sort (flat view)
                    KeyCode::Char('g') => app.toggle_view_mode(),
                    KeyCode::Char(' ') => app.toggle_group_expand(),  // Space to expand/collapse
                    KeyCode::Char('C') => app.collapse_all(),  // Shift+C to collapse all
                    KeyCode::Char('E') => app.expand_all(),    // Shift+E to expand all
                    // Number keys 1-8 jump to layers
                    KeyCode::Char('1') => app.jump_to_layer(1),
                    KeyCode::Char('2') => app.jump_to_layer(2),
                    KeyCode::Char('3') => app.jump_to_layer(3),
                    KeyCode::Char('4') => app.jump_to_layer(4),
                    KeyCode::Char('5') => app.jump_to_layer(5),
                    KeyCode::Char('6') => app.jump_to_layer(6),
                    KeyCode::Char('7') => app.jump_to_layer(7),
                    KeyCode::Char('8') => app.jump_to_layer(8),
                    KeyCode::PageDown => app.page_down(),
                    KeyCode::PageUp => app.page_up(),
                    KeyCode::Char('?') => app.toggle_help(),
                    KeyCode::Char('d') => app.dismiss_selected(),
                    KeyCode::Char('a') => {
                        let suggestion = app.selected_suggestion().cloned();
                        if let Some(suggestion) = suggestion {
                            if !suggest::llm::is_available() {
                                app.show_toast("Run: cosmos --setup");
                            } else {
                                if let Err(e) = app.config.allow_ai(app.session_cost) {
                                    app.show_toast(&e);
                                    continue;
                                }
                                // Phase 1: Generate quick preview (fast)
                                let suggestion_id = suggestion.id;
                                let file_path = suggestion.file.clone();
                                let summary = suggestion.summary.clone();
                                let suggestion_clone = suggestion.clone();
                                let tx_preview = tx.clone();
                                let repo_memory_context = app.repo_memory.to_prompt_context(12, 900);
                                
                                app.loading = LoadingState::GeneratingPreview;
                                
                                tokio::spawn(async move {
                                    let mem = if repo_memory_context.trim().is_empty() {
                                        None
                                    } else {
                                        Some(repo_memory_context)
                                    };
                                    match suggest::llm::generate_fix_preview(&file_path, &suggestion_clone, None, mem).await {
                                        Ok(preview) => {
                                            let _ = tx_preview.send(BackgroundMessage::PreviewReady {
                                                suggestion_id,
                                                file_path,
                                                summary,
                                                preview,
                                            });
                                        }
                                        Err(e) => {
                                            let _ = tx_preview.send(BackgroundMessage::PreviewError(e.to_string()));
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
                            app.start_question();
                        }
                    }
                    KeyCode::Char('R') => {
                        // Ritual mode - a time-boxed, curated queue of improvements
                        app.start_ritual(10);
                    }
                    KeyCode::Char('b') => {
                        // Branch workflow - create branch and commit pending changes
                        // If already on a cosmos branch, this shows ship dialog
                        app.show_branch_dialog();
                    }
                    KeyCode::Char('s') => {
                        // Ship workflow - streamlined commit + push + PR
                        if app.is_ready_to_ship() {
                            app.show_ship_dialog();
                        } else if app.pending_change_count() > 0 {
                            // Has pending changes but not on cosmos branch - guide user
                            app.show_toast("First apply a fix (creates branch), then ship");
                        } else {
                            app.show_toast("No changes to ship");
                        }
                    }
                    KeyCode::Char('u') => {
                        // Undo the last applied change (restore backup)
                        match app.undo_last_pending_change() {
                            Ok(()) => app.show_toast("Undone (restored backup)"),
                            Err(e) => app.show_toast(&e),
                        }
                    }
                    KeyCode::Char('p') => {
                        // PR workflow - show PR review panel (legacy)
                        if app.is_ready_to_ship() {
                            // Redirect to ship dialog for better UX
                            app.show_ship_dialog();
                        } else {
                            app.show_pr_review();
                        }
                    }
                    KeyCode::Char('c') => {
                        // Git status - view and manage changed files
                        app.show_git_status();
                    }
                    KeyCode::Char('m') => {
                        // Switch to main branch (quick escape from fix branches)
                        if app.is_on_main_branch() {
                            app.show_toast("Already on main");
                        } else {
                            match app.git_switch_to_main() {
                                Ok(_) => {
                                    app.show_toast("Switched to main");
                                    let _ = app.context.refresh();
                                }
                                Err(e) => app.show_toast(&e),
                            }
                        }
                    }
                    KeyCode::Char('M') => {
                        // Repo memory (decisions/conventions)
                        app.show_repo_memory();
                    }
                    KeyCode::Char('P') => {
                        // Toggle inquiry privacy preview
                        app.config.privacy_preview = !app.config.privacy_preview;
                        let _ = app.config.save();
                        app.show_toast(if app.config.privacy_preview { "Privacy preview: on" } else { "Privacy preview: off" });
                    }
                    KeyCode::Char('T') => {
                        // Toggle summarize-changed-only (next startup / future sessions)
                        app.config.summarize_changed_only = !app.config.summarize_changed_only;
                        let _ = app.config.save();
                        app.show_toast(if app.config.summarize_changed_only { "Summaries: changed files only" } else { "Summaries: full repo" });
                    }
                    KeyCode::Char('O') => {
                        // Show config location (for budgets, toggles, etc.)
                        app.show_toast(&format!("Config: {}", crate::config::Config::config_location()));
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

/// Activate a Cosmos Pro license
fn activate_license(key: &str) -> Result<()> {
    let mut manager = license::LicenseManager::load();
    match manager.activate(key) {
        Ok(tier) => {
            println!();
            println!("  ✓ License activated! You now have Cosmos {}.", tier.label().to_uppercase());
            println!("  ✓ Saved to {}", license::LicenseManager::license_location());
            println!();
            Ok(())
        }
        Err(e) => {
            println!();
            println!("  ✗ Activation failed: {}", e);
            println!();
            Err(anyhow::anyhow!(e))
        }
    }
}

/// Deactivate the current license
fn deactivate_license() -> Result<()> {
    license::deactivate_interactive()
        .map_err(|e| anyhow::anyhow!(e))
}

/// Show usage statistics
fn show_usage() -> Result<()> {
    let manager = license::LicenseManager::load();
    let stats = manager.usage_stats();

    println!();
    println!("  ┌─────────────────────────────────────────────────────────┐");
    println!("  │  ✦ COSMOS USAGE                                         │");
    println!("  └─────────────────────────────────────────────────────────┘");
    println!();

    match stats.tier {
        license::Tier::Free => {
            println!("  Tier:   FREE (BYOK mode)");
            println!();
            println!("  In BYOK mode, usage is billed directly to your");
            println!("  OpenRouter account. Check your dashboard at:");
            println!("  https://openrouter.ai/activity");
        }
        license::Tier::Pro | license::Tier::Team => {
            let allowance = stats.tier.token_allowance();
            let pct = if allowance > 0 {
                (stats.tokens_used as f64 / allowance as f64 * 100.0) as u32
            } else {
                0
            };

            println!("  Tier:      {}", stats.tier.label().to_uppercase());
            println!("  Tokens:    {} / {} ({:.1}%)", 
                format_tokens(stats.tokens_used),
                format_tokens(allowance),
                pct
            );
            println!("  Remaining: {}", format_tokens(stats.tokens_remaining));

            if let Some(reset) = stats.period_resets_at {
                let days_until = (reset - chrono::Utc::now()).num_days();
                println!("  Resets in: {} days ({})", days_until, reset.format("%Y-%m-%d"));
            }

            // Usage bar
            println!();
            print!("  [");
            let bar_width = 40;
            let filled = (pct as usize * bar_width / 100).min(bar_width);
            for i in 0..bar_width {
                if i < filled {
                    print!("█");
                } else {
                    print!("░");
                }
            }
            println!("] {}%", pct);
        }
    }

    println!();
    Ok(())
}

/// Format token count for display
fn format_tokens(tokens: u64) -> String {
    if tokens >= 1_000_000 {
        format!("{:.1}M", tokens as f64 / 1_000_000.0)
    } else if tokens >= 1_000 {
        format!("{:.1}K", tokens as f64 / 1_000.0)
    } else {
        format!("{}", tokens)
    }
}

/// Generate a test license key (dev only)
fn generate_test_key(tier: &str) -> Result<()> {
    let tier = match tier.to_lowercase().as_str() {
        "pro" => license::Tier::Pro,
        "team" => license::Tier::Team,
        "free" => license::Tier::Free,
        _ => {
            println!("  Invalid tier. Use: pro, team, or free");
            return Ok(());
        }
    };

    let key = license::generate_license_key(tier);
    println!();
    println!("  Generated {} license key:", tier.label().to_uppercase());
    println!("  {}", key);
    println!();
    println!("  Activate with: cosmos --activate {}", key);
    println!();

    Ok(())
}

