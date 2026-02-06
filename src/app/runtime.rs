//! TUI runtime for Cosmos
//!
//! # Error Handling
//!
//! Background tasks use `let _ =` for channel sends and cache operations.
//! See `background.rs` module docs for the rationale. In short:
//! - Channel sends can fail if receiver is dropped (shutdown) - safe to ignore
//! - Cache saves are best-effort - failure means regeneration next time

use crate::app::messages::BackgroundMessage;
use crate::app::{background, input, RuntimeContext};
use crate::cache;
use crate::context::WorkContext;
use crate::git_ops;
use crate::grouping::{Confidence, Layer, LayerOverride};
use crate::index::CodebaseIndex;
use crate::suggest;
use crate::suggest::llm::grouping as grouping_llm;
use crate::suggest::SuggestionEngine;
use crate::ui;
use crate::ui::{App, LoadingState};
use anyhow::Result;
use crossterm::{
    event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyEventKind},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::prelude::*;
use std::collections::HashMap;
use std::io;
use std::path::PathBuf;
use std::sync::mpsc;
use std::time::Duration;

/// Run the TUI application with background LLM tasks
pub async fn run_tui(
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
    // Load repo-local “memory” (decisions/conventions) from .cosmos/
    app.repo_memory = cache_manager.load_repo_memory();
    // Load cached domain glossary (auto-extracted terminology)
    app.glossary = cache_manager.load_glossary().unwrap_or_default();
    // Load cached question answers
    app.question_cache = cache_manager.load_question_cache().unwrap_or_default();

    // Check for unsaved work and show startup overlay if needed
    if let Ok(status) = git_ops::current_status(&repo_path) {
        let main_branch =
            git_ops::get_main_branch_name(&repo_path).unwrap_or_else(|_| "main".to_string());
        let is_on_main = status.branch == main_branch;
        let changed_count = status.staged.len() + status.modified.len() + status.untracked.len();

        // Show overlay if not on main or has uncommitted changes
        if !is_on_main || changed_count > 0 {
            app.show_startup_check(changed_count, status.branch.clone(), main_branch);
        }
    }

    // Show welcome overlay on first run (only if no other overlay is showing)
    if !cache_manager.has_seen_welcome() && app.overlay == ui::Overlay::None {
        app.overlay = ui::Overlay::Welcome;
        // Mark as seen so it doesn't show again
        let _ = cache_manager.mark_welcome_seen();
    }

    // Check if we have API access
    let ai_enabled = suggest::llm::is_available();

    // ═══════════════════════════════════════════════════════════════════════
    //  SMART SUMMARY CACHING
    // ═══════════════════════════════════════════════════════════════════════

    // Compute file hashes for change detection
    let file_hashes = cache::compute_file_hashes(&index);

    // Optional AI-assisted grouping (safe fallback, low-confidence only)
    let grouping_ai_enabled = true;
    let mut grouping_ai_cache = cache_manager.load_grouping_ai_cache().unwrap_or_default();
    if grouping_ai_cache.normalize_paths(&index.root) {
        let _ = cache_manager.save_grouping_ai_cache(&grouping_ai_cache);
    }
    if grouping_ai_enabled {
        let overrides = cached_grouping_overrides(&app.grouping, &grouping_ai_cache, &file_hashes);
        if !overrides.is_empty() {
            let grouping = crate::grouping::generate_grouping_with_overrides(&index, &overrides);
            app.apply_grouping_update(grouping);
        }
    }

    // Load cached LLM summaries and apply immediately
    let mut llm_cache = cache_manager.load_llm_summaries_cache().unwrap_or_default();
    if llm_cache.normalize_paths(&index.root) {
        let _ = cache_manager.save_llm_summaries_cache(&llm_cache);
    }

    // Get all valid cached summaries and load them immediately (instant startup!)
    let cached_summaries = llm_cache.get_all_valid_summaries(&file_hashes);
    let cached_count = cached_summaries.len();
    let total_files = file_hashes.len();

    if !cached_summaries.is_empty() {
        app.update_summaries(cached_summaries);
        eprintln!(
            "  Loaded {} cached summaries ({} files total)",
            cached_count, total_files
        );
    }

    // Discover project context (for better quality summaries)
    let project_context = suggest::llm::discover_project_context(&index);
    llm_cache.set_project_context(project_context.clone());

    // Find files that need new/updated summaries
    let files_needing_summary = llm_cache.get_files_needing_summary(&file_hashes);
    let needs_summary_count = files_needing_summary.len();

    // Track if we need to generate summaries (used to control loading state)
    app.needs_summary_generation = needs_summary_count > 0;

    if needs_summary_count > 0 {
        eprintln!("  {} files need summary generation", needs_summary_count);
    } else if cached_count > 0 {
        eprintln!("  All {} summaries loaded from cache", cached_count);
    }

    eprintln!();

    // Create channel for background tasks
    let (tx, rx) = mpsc::channel::<BackgroundMessage>();

    // ═══════════════════════════════════════════════════════════════════════
    //  BACKGROUND VERSION CHECK
    // ═══════════════════════════════════════════════════════════════════════
    {
        let tx_update = tx.clone();
        background::spawn_background(tx.clone(), "version_check", async move {
            // Check for updates (silently fail if network unavailable)
            if let Ok(Some(update_info)) = crate::update::check_for_update().await {
                let _ = tx_update.send(BackgroundMessage::UpdateAvailable {
                    latest_version: update_info.latest_version,
                });
            }
        });
    }

    // ═══════════════════════════════════════════════════════════════════════
    //  FETCH INITIAL WALLET BALANCE
    // ═══════════════════════════════════════════════════════════════════════
    if ai_enabled {
        background::spawn_balance_refresh(tx.clone());
    }

    // AI grouping enhancement: low-confidence files only, capped for safety
    if grouping_ai_enabled && ai_enabled {
        let max_files =
            grouping_llm::GROUPING_AI_FILES_PER_REQUEST * grouping_llm::GROUPING_AI_MAX_REQUESTS;
        let candidates = select_grouping_ai_candidates(
            &app.grouping,
            &grouping_ai_cache,
            &file_hashes,
            max_files,
        );

        if !candidates.is_empty() {
            let index_clone = index.clone();
            let baseline_grouping = app.grouping.clone();
            let file_hashes_clone = file_hashes.clone();
            let tx_grouping = tx.clone();
            let cache_path = repo_path.clone();

            // Process chunks sequentially in a single task to avoid cache races
            background::spawn_background(tx.clone(), "grouping_ai", async move {
                let cache = cache::Cache::new(&cache_path);
                let mut grouping_cache = cache.load_grouping_ai_cache().unwrap_or_default();
                let _ = grouping_cache.normalize_paths(&index_clone.root);

                let mut total_usage = suggest::llm::Usage::default();
                let mut saw_usage = false;

                for chunk in candidates
                    .chunks(grouping_llm::GROUPING_AI_FILES_PER_REQUEST)
                    .take(grouping_llm::GROUPING_AI_MAX_REQUESTS)
                {
                    match grouping_llm::classify_grouping_candidates(&index_clone, chunk).await {
                        Ok((suggestions, usage)) => {
                            for suggestion in suggestions {
                                if let Some(hash) = file_hashes_clone.get(&suggestion.path) {
                                    grouping_cache.set_entry(
                                        suggestion.path.clone(),
                                        cache::GroupingAiEntry {
                                            layer: suggestion.layer,
                                            confidence: suggestion.confidence,
                                            file_hash: hash.clone(),
                                            generated_at: chrono::Utc::now(),
                                        },
                                    );
                                }
                            }

                            if let Some(u) = usage {
                                total_usage.prompt_tokens += u.prompt_tokens;
                                total_usage.completion_tokens += u.completion_tokens;
                                total_usage.total_tokens += u.total_tokens;
                                saw_usage = true;
                            }
                        }
                        Err(e) => {
                            let _ = tx_grouping
                                .send(BackgroundMessage::GroupingEnhanceError(e.to_string()));
                        }
                    }
                }

                let _ = cache.save_grouping_ai_cache(&grouping_cache);

                let overrides = cached_grouping_overrides(
                    &baseline_grouping,
                    &grouping_cache,
                    &file_hashes_clone,
                );
                let usage = if saw_usage { Some(total_usage) } else { None };

                if !overrides.is_empty() {
                    let grouping =
                        crate::grouping::generate_grouping_with_overrides(&index_clone, &overrides);
                    let _ = tx_grouping.send(BackgroundMessage::GroupingEnhanced {
                        grouping,
                        updated_files: overrides.len(),
                        usage,
                        model: "balanced".to_string(),
                    });
                } else if usage.is_some() {
                    let _ = tx_grouping.send(BackgroundMessage::GroupingEnhanced {
                        grouping: baseline_grouping.clone(),
                        updated_files: 0,
                        usage,
                        model: "balanced".to_string(),
                    });
                }
            });
        }
    }

    // ═══════════════════════════════════════════════════════════════════════
    //  SUGGESTION GENERATION: Always generate fresh suggestions on startup
    // ═══════════════════════════════════════════════════════════════════════
    //
    // Unlike summaries (which are expensive and file-content-dependent), suggestions
    // benefit from fresh analysis each session. The model may find different issues
    // based on its exploration path, and users expect new insights on restart.

    let suggestions_from_cache = false;

    // ═══════════════════════════════════════════════════════════════════════
    //  SEQUENTIAL INIT: Summaries first (builds glossary), then suggestions
    // ═══════════════════════════════════════════════════════════════════════

    if ai_enabled && !suggestions_from_cache {
        if !files_needing_summary.is_empty() {
            // Phase 1: Summaries needed - generate them first, suggestions come after
            app.loading = LoadingState::GeneratingSummaries;
            app.pending_suggestions_on_init = true;
            app.summary_progress = Some((0, needs_summary_count));

            let index_clone2 = index.clone();
            let context_clone2 = context.clone();
            let tx_summaries = tx.clone();
            let cache_path = repo_path.clone();
            let file_hashes_clone = file_hashes.clone();

            // Prioritize files for generation
            let (high_priority, medium_priority, low_priority) =
                suggest::llm::prioritize_files_for_summary(
                    &index_clone2,
                    &context_clone2,
                    &files_needing_summary,
                );

            // Show initial cached count
            if cached_count > 0 {
                app.show_toast(&format!(
                    "{}/{} cached · summarizing {}",
                    cached_count, total_files, needs_summary_count
                ));
            }

            // Calculate total file count for progress
            let total_to_process = high_priority.len() + medium_priority.len() + low_priority.len();

            background::spawn_background(tx.clone(), "summary_generation", async move {
                let stage_start = std::time::Instant::now();
                let cache = cache::Cache::new(&cache_path);

                // Load existing cache to update incrementally
                let mut llm_cache = cache.load_llm_summaries_cache().unwrap_or_default();

                // Load existing glossary to merge new terms into
                let mut glossary = cache.load_glossary().unwrap_or_default();

                let mut all_summaries = HashMap::new();
                let mut total_usage = suggest::llm::Usage::default();
                let mut completed_count = 0usize;
                let mut failed_files: Vec<PathBuf> = Vec::new();

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

                    let batch_size = suggest::llm::SUMMARY_BATCH_SIZE;
                    let batches: Vec<_> = files.chunks(batch_size).collect();

                    // Process batches sequentially (llm.rs handles internal parallelism)
                    for batch in batches {
                        let batch_files: Vec<PathBuf> = batch.to_vec();
                        match suggest::llm::generate_summaries_for_files(
                            &index_clone2,
                            batch,
                            &project_context,
                        )
                        .await
                        {
                            Ok((summaries, batch_glossary, usage, batch_failed)) => {
                                // Update cache with new summaries
                                for (path, summary) in &summaries {
                                    if let Some(hash) = file_hashes_clone.get(path) {
                                        llm_cache.set_summary(
                                            path.clone(),
                                            summary.clone(),
                                            hash.clone(),
                                        );
                                    }
                                }
                                // Merge new terms into glossary
                                glossary.merge(&batch_glossary);

                                // Save cache incrementally after each batch
                                let _ = cache.save_llm_summaries_cache(&llm_cache);
                                let _ = cache.save_glossary(&glossary);

                                completed_count += summaries.len() + batch_failed.len();
                                failed_files.extend(batch_failed);

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
                            Err(_e) => {
                                // Batch failed - track failed files for retry
                                // Error details are logged internally; user sees retry prompt
                                completed_count += batch_files.len();
                                failed_files.extend(batch_files);
                                let _ = tx_summaries.send(BackgroundMessage::SummaryProgress {
                                    completed: completed_count,
                                    total: total_to_process,
                                    summaries: HashMap::new(),
                                });
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
                    usage: final_usage,
                    failed_files,
                    duration_ms: stage_start.elapsed().as_millis() as u64,
                });
            });
        } else {
            // Phase 2 only: All summaries cached - generate suggestions directly with cached glossary
            app.loading = LoadingState::GeneratingSuggestions;

            let index_clone = index.clone();
            let context_clone = context.clone();
            let tx_suggestions = tx.clone();
            let cache_clone_path = repo_path.clone();
            let repo_memory_context = app.repo_memory.to_prompt_context(12, 900);
            let summaries_for_suggestions = app.llm_summaries.clone();
            let glossary_clone = app.glossary.clone();

            if !glossary_clone.is_empty() {
                app.show_toast(&format!(
                    "{} glossary terms · generating suggestions",
                    glossary_clone.len()
                ));
            }

            let repo_root = cache_clone_path.clone();
            background::spawn_background(tx.clone(), "suggestions_generation", async move {
                let stage_start = std::time::Instant::now();
                let mem = if repo_memory_context.trim().is_empty() {
                    None
                } else {
                    Some(repo_memory_context)
                };
                // Fast grounded suggestions: one LLM call, no tools, strict latency budget.
                // Agentic deep scan remains available elsewhere but should not block the user.
                match suggest::llm::analyze_codebase_fast_grounded(
                    &repo_root,
                    &index_clone,
                    &context_clone,
                    mem,
                    Some(&summaries_for_suggestions),
                )
                .await
                {
                    Ok((suggestions, usage, diagnostics)) => {
                        let _ = tx_suggestions.send(BackgroundMessage::SuggestionsReady {
                            suggestions,
                            usage,
                            model: "fast-grounded".to_string(),
                            diagnostics,
                            duration_ms: stage_start.elapsed().as_millis() as u64,
                        });
                    }
                    Err(e) => {
                        let _ =
                            tx_suggestions.send(BackgroundMessage::SuggestionsError(e.to_string()));
                    }
                }
            });
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
    let git_refresh_interval = if index.stats().file_count > 20000 {
        std::time::Duration::from_secs(10)
    } else if index.stats().file_count > 5000 {
        std::time::Duration::from_secs(5)
    } else {
        std::time::Duration::from_secs(2)
    };

    let ctx = RuntimeContext {
        index: &index,
        repo_path: &repo_path,
        tx: &tx,
    };

    loop {
        // Clear expired toasts
        app.clear_expired_toast();

        // Advance spinner animation
        app.tick_loading();

        // Periodically refresh git status (every 2 seconds)
        if last_git_refresh.elapsed() >= git_refresh_interval {
            match app.context.refresh() {
                Ok(_) => {
                    app.git_refresh_error = None;
                    app.git_refresh_error_at = None;
                }
                Err(e) => {
                    let message = format!("Git status refresh failed: {}", e);
                    let should_log = app
                        .git_refresh_error_at
                        .map(|t| t.elapsed() >= Duration::from_secs(30))
                        .unwrap_or(true);
                    if should_log {
                        app.show_toast(&message);
                        app.git_refresh_error_at = Some(std::time::Instant::now());
                    }
                    app.git_refresh_error = Some(message);
                }
            }
            last_git_refresh = std::time::Instant::now();
        }

        // Check for background messages (non-blocking)
        background::drain_messages(app, &rx, &ctx);

        // Render
        terminal.draw(|f| ui::render(f, app))?;

        // Poll for events with fast timeout (snappy animations)
        if event::poll(Duration::from_millis(50))? {
            if let Event::Key(key) = event::read()? {
                if key.kind != KeyEventKind::Press {
                    continue;
                }
                input::handle_key_event(app, key, &ctx)?;
            }
        }

        if app.should_quit {
            return Ok(());
        }
    }
}

fn cached_grouping_overrides(
    grouping: &crate::grouping::CodebaseGrouping,
    cache: &cache::GroupingAiCache,
    file_hashes: &HashMap<PathBuf, String>,
) -> HashMap<PathBuf, LayerOverride> {
    let mut overrides = HashMap::new();

    for (path, entry) in &cache.entries {
        let Some(hash) = file_hashes.get(path) else {
            continue;
        };
        if !cache.is_file_valid(path, hash) {
            continue;
        }
        if entry.confidence < grouping_llm::GROUPING_AI_MIN_CONFIDENCE {
            continue;
        }
        let Some(assignment) = grouping.file_assignments.get(path) else {
            continue;
        };
        if assignment.confidence != Confidence::Low {
            continue;
        }
        if !matches!(assignment.layer, Layer::Unknown | Layer::Shared) {
            continue;
        }
        if assignment.layer == entry.layer {
            continue;
        }
        overrides.insert(
            path.clone(),
            LayerOverride {
                layer: entry.layer,
                confidence: Confidence::from_score(entry.confidence),
            },
        );
    }

    overrides
}

fn select_grouping_ai_candidates(
    grouping: &crate::grouping::CodebaseGrouping,
    cache: &cache::GroupingAiCache,
    file_hashes: &HashMap<PathBuf, String>,
    max_files: usize,
) -> Vec<PathBuf> {
    let mut candidates: Vec<PathBuf> = grouping
        .file_assignments
        .iter()
        .filter(|(_, assignment)| assignment.confidence == Confidence::Low)
        .filter(|(_, assignment)| matches!(assignment.layer, Layer::Unknown | Layer::Shared))
        .filter(|(path, _)| {
            if let Some(hash) = file_hashes.get(*path) {
                !cache.is_file_valid(path, hash)
            } else {
                false
            }
        })
        .map(|(path, _)| path.clone())
        .collect();

    candidates.sort();
    candidates.truncate(max_files);
    candidates
}
