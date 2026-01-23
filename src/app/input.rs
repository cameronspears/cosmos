use crate::app::background;
use crate::app::messages::BackgroundMessage;
use crate::app::RuntimeContext;
use crate::cache;
use crate::git_ops;
use crate::index;
use crate::suggest;
use crate::ui::{ActivePanel, App, InputMode, LoadingState, Overlay, WorkflowStep};
use crate::ui;
use crate::util::resolve_repo_path;
use anyhow::Result;
use crossterm::event::{KeyCode, KeyEvent};
use std::collections::HashMap;
use std::path::PathBuf;

pub fn handle_key_event(app: &mut App, key: KeyEvent, ctx: &RuntimeContext) -> Result<()> {
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
        return Ok(());
    }

    // Handle question input mode
    if app.input_mode == InputMode::Question {
        match key.code {
            KeyCode::Esc => app.exit_question(),
            KeyCode::Up => {
                // Navigate suggestions when input is empty
                if app.question_input.is_empty() {
                    app.question_suggestion_up();
                }
            }
            KeyCode::Down => {
                // Navigate suggestions when input is empty
                if app.question_input.is_empty() {
                    app.question_suggestion_down();
                }
            }
            KeyCode::Tab => {
                // Use selected suggestion
                app.use_selected_suggestion();
            }
            KeyCode::Enter => {
                // If input is empty, use the selected suggestion first
                if app.question_input.is_empty() && !app.question_suggestions.is_empty() {
                    app.use_selected_suggestion();
                }
                let question = app.take_question();
                if !question.is_empty() {
                    // Privacy preview (what will be sent) before the network call
                    if app.config.privacy_preview {
                        app.show_inquiry_preview(question);
                    } else {
                        if let Err(e) = app.config.allow_ai(app.session_cost) {
                            app.show_toast(&e);
                            return Ok(());
                        }
                        // Send question to LLM
                        let index_clone = ctx.index.clone();
                        let context_clone = app.context.clone();
                        let tx_question = ctx.tx.clone();
                        let repo_memory_context = app.repo_memory.to_prompt_context(12, 900);

                        app.loading = LoadingState::Answering;

                        background::spawn_background(ctx.tx.clone(), "ask_question", async move {
                            let mem = if repo_memory_context.trim().is_empty() {
                                None
                            } else {
                                Some(repo_memory_context)
                            };
                            match suggest::llm::ask_question(
                                &index_clone,
                                &context_clone,
                                &question,
                                mem,
                            )
                            .await
                            {
                                Ok((answer, usage)) => {
                                    let _ = tx_question.send(BackgroundMessage::QuestionResponse {
                                        answer,
                                        usage,
                                    });
                                }
                                Err(e) => {
                                    let _ = tx_question
                                        .send(BackgroundMessage::Error(e.to_string()));
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
        return Ok(());
    }

    // Handle overlay mode
    if app.overlay != Overlay::None {
        // Inquiry privacy preview overlay
        if let Overlay::InquiryPreview { question, .. } = &app.overlay {
            match key.code {
                KeyCode::Esc | KeyCode::Char('q') => app.close_overlay(),
                KeyCode::Down => app.overlay_scroll_down(),
                KeyCode::Up => app.overlay_scroll_up(),
                KeyCode::Enter => {
                    if let Err(e) = app.config.allow_ai(app.session_cost) {
                        app.show_toast(&e);
                        return Ok(());
                    }
                    let question = question.clone();
                    let index_clone = ctx.index.clone();
                    let context_clone = app.context.clone();
                    let tx_question = ctx.tx.clone();
                    let repo_memory_context = app.repo_memory.to_prompt_context(12, 900);
                    app.loading = LoadingState::Answering;
                    app.close_overlay();
                    background::spawn_background(ctx.tx.clone(), "ask_question_preview", async move {
                        let mem = if repo_memory_context.trim().is_empty() {
                            None
                        } else {
                            Some(repo_memory_context)
                        };
                        match suggest::llm::ask_question(
                            &index_clone,
                            &context_clone,
                            &question,
                            mem,
                        )
                        .await
                        {
                            Ok((answer, usage)) => {
                                let _ = tx_question.send(BackgroundMessage::QuestionResponse {
                                    answer,
                                    usage,
                                });
                            }
                            Err(e) => {
                                let _ = tx_question
                                    .send(BackgroundMessage::Error(e.to_string()));
                            }
                        }
                    });
                }
                _ => {}
            }
            return Ok(());
        }

        // Handle Reset cosmos overlay
        if let ui::Overlay::Reset { .. } = &app.overlay {
            match key.code {
                KeyCode::Esc | KeyCode::Char('q') => {
                    app.close_overlay();
                }
                KeyCode::Down => {
                    app.reset_navigate(1);
                }
                KeyCode::Up => {
                    app.reset_navigate(-1);
                }
                KeyCode::Char(' ') => {
                    app.reset_toggle_selected();
                }
                KeyCode::Enter => {
                    // Execute reset with selected options
                    let selections = app.get_reset_selections();
                    if selections.is_empty() {
                        app.show_toast("No options selected");
                    } else {
                        // Clear selected caches
                        let cache = crate::cache::Cache::new(&app.repo_path);
                        match cache.clear_selective(&selections) {
                            Ok(cleared) => {
                                app.close_overlay();

                                // Check if we need to regenerate things
                                let needs_reindex =
                                    selections.contains(&crate::cache::ResetOption::Index);
                                let needs_suggestions =
                                    selections.contains(&crate::cache::ResetOption::Suggestions);
                                let needs_summaries =
                                    selections.contains(&crate::cache::ResetOption::Summaries);
                                let needs_glossary =
                                    selections.contains(&crate::cache::ResetOption::Glossary);
                                let needs_grouping_ai =
                                    selections.contains(&crate::cache::ResetOption::GroupingAi);

                                // Perform reindex if needed
                                if needs_reindex {
                                    match index::CodebaseIndex::new(&app.repo_path) {
                                        Ok(new_index) => {
                                            let idx = new_index;
                                            let grouping = idx.generate_grouping();
                                            app.index = idx;
                                            app.apply_grouping_update(grouping);
                                        }
                                        Err(e) => {
                                            app.show_toast(&format!("Reindex failed: {}", e));
                                        }
                                    }
                                } else if needs_grouping_ai {
                                    let grouping = app.index.generate_grouping();
                                    app.apply_grouping_update(grouping);
                                }

                                // Clear in-memory suggestions if needed
                                if needs_suggestions {
                                    app.suggestions =
                                        suggest::SuggestionEngine::new(app.index.clone());
                                }

                                // Clear in-memory summaries if needed
                                if needs_summaries {
                                    app.llm_summaries.clear();
                                    app.needs_summary_generation = true;
                                    app.summary_progress = None;
                                    app.summary_failed_files.clear();
                                }

                                // Clear in-memory glossary if needed
                                if needs_glossary {
                                    app.glossary = crate::cache::DomainGlossary::default();
                                }

                                // Refresh context
                                let _ = app.context.refresh();

                                // Check if AI is available for regeneration
                                let mut ai_enabled = suggest::llm::is_available();
                                if ai_enabled {
                                    if let Err(e) = app.config.allow_ai(app.session_cost) {
                                        app.show_toast(&e);
                                        ai_enabled = false;
                                    }
                                }

                                // IMPORTANT: Summaries must generate FIRST (they build the glossary),
                                // THEN suggestions can use the rebuilt glossary.
                                // We track pending_suggestions_on_init to trigger suggestions after summaries complete.

                                // Trigger regeneration of summaries first (builds glossary)
                                if needs_summaries && ai_enabled {
                                    let index_clone2 = app.index.clone();
                                    let context_clone2 = app.context.clone();
                                    let tx_summaries = ctx.tx.clone();
                                    let cache_path = ctx.repo_path.clone();

                                    // Compute file hashes for change detection
                                    let file_hashes =
                                        cache::compute_file_hashes(&index_clone2);
                                    let file_hashes_clone = file_hashes.clone();

                                    // All files need summaries after reset
                                    let files_needing_summary: Vec<PathBuf> =
                                        file_hashes.keys().cloned().collect();

                                    // Discover project context
                                    let project_context =
                                        suggest::llm::discover_project_context(&index_clone2);

                                    // Prioritize files for generation
                                    let (high_priority, medium_priority, low_priority) =
                                        suggest::llm::prioritize_files_for_summary(
                                            &index_clone2,
                                            &context_clone2,
                                            &files_needing_summary,
                                        );

                                    let total_to_process =
                                        high_priority.len() + medium_priority.len() + low_priority.len();

                                    if total_to_process > 0 {
                                        app.loading = LoadingState::GeneratingSummaries;
                                        app.summary_progress = Some((0, total_to_process));

                                        // Flag that suggestions should generate after summaries complete
                                        if needs_suggestions {
                                            app.pending_suggestions_on_init = true;
                                        }

                                        background::spawn_background(ctx.tx.clone(), "reset_summary_generation", async move {
                                            let cache = cache::Cache::new(&cache_path);

                                            // Start with fresh cache after reset
                                            let mut llm_cache = cache::LlmSummaryCache::new();
                                            let mut glossary = cache::DomainGlossary::new();

                                            let mut all_summaries = HashMap::new();
                                            let mut total_usage = suggest::llm::Usage::default();
                                            let mut completed_count = 0usize;
                                            let mut failed_files: Vec<PathBuf> = Vec::new();

                                            let priority_tiers = [
                                                ("high", high_priority),
                                                ("medium", medium_priority),
                                                ("low", low_priority),
                                            ];

                                            for (_tier_name, files) in priority_tiers {
                                                if files.is_empty() {
                                                    continue;
                                                }

                                                let batch_size = 16;
                                                let batches: Vec<_> =
                                                    files.chunks(batch_size).collect();

                                                for batch in batches {
                                                    let batch_files: Vec<PathBuf> =
                                                        batch.iter().cloned().collect();
                                                    match suggest::llm::generate_summaries_for_files(
                                                        &index_clone2,
                                                        batch,
                                                        &project_context,
                                                    )
                                                    .await
                                                    {
                                                        Ok((
                                                            summaries,
                                                            batch_glossary,
                                                            usage,
                                                            batch_failed,
                                                        )) => {
                                                            for (path, summary) in &summaries {
                                                                if let Some(hash) =
                                                                    file_hashes_clone.get(path)
                                                                {
                                                                    llm_cache.set_summary(
                                                                        path.clone(),
                                                                        summary.clone(),
                                                                        hash.clone(),
                                                                    );
                                                                }
                                                            }
                                                            glossary.merge(&batch_glossary);

                                                            let _ =
                                                                cache.save_llm_summaries_cache(
                                                                    &llm_cache,
                                                                );
                                                            let _ = cache.save_glossary(&glossary);

                                                            completed_count +=
                                                                summaries.len() + batch_failed.len();
                                                            failed_files.extend(batch_failed);

                                                            let _ = tx_summaries.send(
                                                                BackgroundMessage::SummaryProgress {
                                                                    completed: completed_count,
                                                                    total: total_to_process,
                                                                    summaries: summaries.clone(),
                                                                },
                                                            );

                                                            all_summaries.extend(summaries);
                                                            if let Some(u) = usage {
                                                                total_usage.prompt_tokens +=
                                                                    u.prompt_tokens;
                                                                total_usage.completion_tokens +=
                                                                    u.completion_tokens;
                                                                total_usage.total_tokens +=
                                                                    u.total_tokens;
                                                            }
                                                        }
                                                        Err(e) => {
                                                            completed_count += batch_files.len();
                                                            failed_files.extend(batch_files);
                                                            let _ = tx_summaries.send(
                                                                BackgroundMessage::SummaryProgress {
                                                                    completed: completed_count,
                                                                    total: total_to_process,
                                                                    summaries: HashMap::new(),
                                                                },
                                                            );
                                                            eprintln!(
                                                                "Warning: Failed to generate summaries for batch: {}",
                                                                e
                                                            );
                                                        }
                                                    }
                                                }
                                            }

                                            let final_usage = if total_usage.total_tokens > 0 {
                                                Some(total_usage)
                                            } else {
                                                None
                                            };

                                            let _ = tx_summaries.send(
                                                BackgroundMessage::SummariesReady {
                                                    summaries: HashMap::new(),
                                                    usage: final_usage,
                                                    failed_files,
                                                },
                                            );
                                        });
                                    }
                                } else if needs_suggestions && ai_enabled {
                                    // No summaries to generate, so generate suggestions directly
                                    let index_clone = app.index.clone();
                                    let context_clone = app.context.clone();
                                    let tx_suggestions = ctx.tx.clone();
                                    let cache_clone_path = ctx.repo_path.clone();
                                    let repo_memory_context = app.repo_memory.to_prompt_context(12, 900);
                                    let glossary_clone = app.glossary.clone();

                                    app.loading = LoadingState::GeneratingSuggestions;

                                    background::spawn_background(ctx.tx.clone(), "reset_suggestions_generation", async move {
                                        let mem = if repo_memory_context.trim().is_empty() {
                                            None
                                        } else {
                                            Some(repo_memory_context)
                                        };
                                        let glossary_ref = if glossary_clone.is_empty() {
                                            None
                                        } else {
                                            Some(&glossary_clone)
                                        };
                                        match suggest::llm::analyze_codebase(
                                            &index_clone,
                                            &context_clone,
                                            mem,
                                            glossary_ref,
                                        )
                                        .await
                                        {
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
                                                let _ = tx_suggestions.send(
                                                    BackgroundMessage::SuggestionsError(e.to_string()),
                                                );
                                            }
                                        }
                                    });
                                }

                                // Show what was cleared
                                let count = cleared.len();
                                if count > 0 {
                                    if !needs_suggestions && !needs_summaries {
                                        app.show_toast(&format!(
                                            "Reset complete: {} files cleared",
                                            count
                                        ));
                                    }
                                    // If regenerating, toast was already shown above
                                } else {
                                    app.show_toast("Reset complete (caches were already empty)");
                                }
                            }
                            Err(e) => {
                                app.show_toast(&format!("Reset failed: {}", e));
                            }
                        }
                    }
                }
                _ => {}
            }
            return Ok(());
        }

        // Handle Startup Check overlay
        if let ui::Overlay::StartupCheck {
            confirming_discard, ..
        } = &app.overlay
        {
            let confirming = *confirming_discard;
            match key.code {
                KeyCode::Esc => {
                    if confirming {
                        // Cancel confirmation, go back to main options
                        app.startup_check_confirm_discard(false);
                    } else {
                        // Quit cosmos
                        app.should_quit = true;
                    }
                }
                KeyCode::Char('s') if !confirming => {
                    // Save (stash) and start fresh
                    match git_ops::stash_and_switch_to_main(&app.repo_path) {
                        Ok(_) => {
                            app.close_overlay();
                            app.show_toast("Work saved! Restore with 'git stash pop'");
                            // Refresh context after switching branches
                            let _ = app.context.refresh();
                        }
                        Err(e) => {
                            app.show_toast(&format!("Failed to save: {}", e));
                        }
                    }
                }
                KeyCode::Char('d') if !confirming => {
                    // Show discard confirmation
                    app.startup_check_confirm_discard(true);
                }
                KeyCode::Char('c') if !confirming => {
                    // Continue as-is
                    app.close_overlay();
                }
                KeyCode::Char('y') if confirming => {
                    // Confirm discard
                    match git_ops::reset_to_main(&app.repo_path) {
                        Ok(_) => {
                            app.close_overlay();
                            app.show_toast("Started fresh");
                            // Refresh context after resetting
                            let _ = app.context.refresh();
                        }
                        Err(e) => {
                            app.show_toast(&format!("Failed to reset: {}", e));
                        }
                    }
                }
                KeyCode::Char('n') if confirming => {
                    // Cancel confirmation
                    app.startup_check_confirm_discard(false);
                }
                _ => {}
            }
            return Ok(());
        }

        // Handle other overlays (generic scroll/close)
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => app.close_overlay(),
            KeyCode::Down => app.overlay_scroll_down(),
            KeyCode::Up => app.overlay_scroll_up(),
            _ => {}
        }
        return Ok(());
    }

    // Normal mode
    match key.code {
        KeyCode::Char('q') => app.should_quit = true,
        KeyCode::Tab => app.toggle_panel(),
        KeyCode::Down => {
            // Handle ask cosmos scroll first
            if app.is_ask_cosmos_mode() {
                app.ask_cosmos_scroll_down();
            } else if app.active_panel == ActivePanel::Suggestions {
                // Handle navigation based on workflow step
                match app.workflow_step {
                    WorkflowStep::Review
                        if !app.review_state.reviewing && !app.review_state.fixing =>
                    {
                        app.review_cursor_down();
                    }
                    WorkflowStep::Verify if !app.verify_state.loading => {
                        app.verify_scroll_down();
                    }
                    WorkflowStep::Ship => {
                        app.ship_scroll_down();
                    }
                    WorkflowStep::Suggestions => app.navigate_down(),
                    _ => {}
                }
            } else {
                app.navigate_down();
            }
        }
        KeyCode::Up => {
            // Handle ask cosmos scroll first
            if app.is_ask_cosmos_mode() {
                app.ask_cosmos_scroll_up();
            } else if app.active_panel == ActivePanel::Suggestions {
                // Handle navigation based on workflow step
                match app.workflow_step {
                    WorkflowStep::Review
                        if !app.review_state.reviewing && !app.review_state.fixing =>
                    {
                        app.review_cursor_up();
                    }
                    WorkflowStep::Verify if !app.verify_state.loading => {
                        app.verify_scroll_up();
                    }
                    WorkflowStep::Ship => {
                        app.ship_scroll_up();
                    }
                    WorkflowStep::Suggestions => app.navigate_up(),
                    _ => {}
                }
            } else {
                app.navigate_up();
            }
        }
        KeyCode::Char(' ') => {
            // Space toggles finding selection in Review step
            if app.active_panel == ActivePanel::Suggestions
                && app.workflow_step == WorkflowStep::Review
                && !app.review_state.reviewing
                && !app.review_state.fixing
            {
                app.review_toggle_finding();
            }
        }
        KeyCode::Char('f') => {
            // Fix selected findings in Review step
            if app.active_panel == ActivePanel::Suggestions
                && app.workflow_step == WorkflowStep::Review
                && !app.review_state.reviewing
                && !app.review_state.fixing
                && !app.review_state.selected.is_empty()
            {
                if let Err(e) = app.config.allow_ai(app.session_cost) {
                    app.show_toast(&e);
                    return Ok(());
                }
                let selected_findings = app.get_selected_review_findings();
                let file = app.review_state.file_path.clone();
                let content = app.review_state.new_content.clone();
                let original = app.review_state.original_content.clone();
                let iter = app.review_state.review_iteration;
                let fixed = app.review_state.fixed_titles.clone();
                let repo_memory_context = app.repo_memory.to_prompt_context(12, 900);
                let memory = if repo_memory_context.trim().is_empty() {
                    None
                } else {
                    Some(repo_memory_context)
                };
                let tx_fix = ctx.tx.clone();

                if let Some(file_path) = file {
                    app.set_review_fixing(true);

                    background::spawn_background(ctx.tx.clone(), "verification_fix", async move {
                        let orig_ref = if iter > 1 {
                            Some(original.as_str())
                        } else {
                            None
                        };
                        match suggest::llm::fix_review_findings(
                            &file_path,
                            &content,
                            orig_ref,
                            &selected_findings,
                            memory,
                            iter,
                            &fixed,
                        )
                        .await
                        {
                            Ok(fix) => {
                                let _ = tx_fix.send(BackgroundMessage::VerificationFixComplete {
                                    new_content: fix.new_content,
                                    description: fix.description,
                                    usage: fix.usage,
                                });
                            }
                            Err(e) => {
                                let _ = tx_fix.send(BackgroundMessage::Error(e.to_string()));
                            }
                        }
                    });
                }
            }
        }
        KeyCode::Char('d') => {
            // Toggle technical details in Verify step
            if app.active_panel == ActivePanel::Suggestions
                && app.workflow_step == WorkflowStep::Verify
                && !app.verify_state.loading
                && app.verify_state.preview.is_some()
            {
                app.verify_toggle_details();
            }
        }
        KeyCode::Enter => {
            // If PR URL is pending, open it in browser
            if let Some(url) = app.pr_url.take() {
                let _ = git_ops::open_url(&url);
            } else {
                match app.active_panel {
                    ActivePanel::Project => app.toggle_group_expand(),
                    ActivePanel::Suggestions => {
                        // Handle based on workflow step
                        match app.workflow_step {
                            WorkflowStep::Suggestions => {
                                // Start verify step with selected suggestion
                                let suggestion = app.selected_suggestion().cloned();
                                if let Some(suggestion) = suggestion {
                                    if !suggest::llm::is_available() {
                                        app.show_toast("Run: cosmos --setup");
                                    } else {
                                        if let Err(e) = app.config.allow_ai(app.session_cost) {
                                            app.show_toast(&e);
                                            return Ok(());
                                        }
                                        let suggestion_id = suggestion.id;
                                        let file_path = suggestion.file.clone();
                                        let additional_files = suggestion.additional_files.clone();
                                        let summary = suggestion.summary.clone();
                                        let suggestion_clone = suggestion.clone();
                                        let tx_preview = ctx.tx.clone();
                                        let repo_root = app.repo_path.clone();
                                        let repo_memory_context =
                                            app.repo_memory.to_prompt_context(12, 900);

                                        // Move to Verify step (with multi-file support)
                                        app.start_verify_multi(
                                            suggestion_id,
                                            file_path.clone(),
                                            additional_files,
                                            summary.clone(),
                                        );

                                        background::spawn_background(ctx.tx.clone(), "preview_generation", async move {
                                            let mem = if repo_memory_context.trim().is_empty() {
                                                None
                                            } else {
                                                Some(repo_memory_context)
                                            };
                                            let resolved =
                                                match resolve_repo_path(&repo_root, &file_path) {
                                                    Ok(resolved) => resolved,
                                                    Err(e) => {
                                                        let _ = tx_preview.send(
                                                            BackgroundMessage::PreviewError(
                                                                format!(
                                                                    "Unsafe path {}: {}",
                                                                    file_path.display(),
                                                                    e
                                                                ),
                                                            ),
                                                        );
                                                        return;
                                                    }
                                                };
                                            let content = match std::fs::read_to_string(
                                                &resolved.absolute,
                                            ) {
                                                Ok(content) => content,
                                                Err(e) => {
                                                    let _ = tx_preview.send(
                                                        BackgroundMessage::PreviewError(format!(
                                                            "Failed to read {}: {}",
                                                            resolved.relative.display(),
                                                            e
                                                        )),
                                                    );
                                                    return;
                                                }
                                            };
                                            match suggest::llm::generate_fix_preview(
                                                &resolved.relative,
                                                &content,
                                                &suggestion_clone,
                                                None,
                                                mem,
                                            )
                                            .await
                                            {
                                                Ok(preview) => {
                                                    let _ = tx_preview
                                                        .send(BackgroundMessage::PreviewReady {
                                                            preview,
                                                        });
                                                }
                                                Err(e) => {
                                                    let _ = tx_preview.send(
                                                        BackgroundMessage::PreviewError(
                                                            e.to_string(),
                                                        ),
                                                    );
                                                }
                                            }
                                        });
                                    }
                                }
                            }
                            WorkflowStep::Verify => {
                                // Apply the fix and move to Review
                                if let Some(preview) = app.verify_state.preview.clone() {
                                    let state = &app.verify_state;
                                    let suggestion_id = state.suggestion_id;
                                    let file_path = state.file_path.clone();
                                    let tx_apply = ctx.tx.clone();
                                    let repo_path = app.repo_path.clone();
                                    let repo_memory_context =
                                        app.repo_memory.to_prompt_context(12, 900);

                                    if let (Some(sid), Some(fp)) = (suggestion_id, file_path.clone())
                                    {
                                        if let Some(suggestion) = app
                                            .suggestions
                                            .suggestions
                                            .iter()
                                            .find(|s| s.id == sid)
                                            .cloned()
                                        {
                                            if let Err(e) = app.config.allow_ai(app.session_cost) {
                                                app.show_toast(&e);
                                                return Ok(());
                                            }
                                            app.loading = LoadingState::GeneratingFix;

                                            background::spawn_background(ctx.tx.clone(), "apply_fix", async move {
                                                // Create branch from main
                                                let branch_name = git_ops::generate_fix_branch_name(
                                                    &suggestion.id.to_string(),
                                                    &suggestion.summary,
                                                );

                                                let created_branch = match git_ops::create_fix_branch_from_main(
                                                    &repo_path,
                                                    &branch_name,
                                                ) {
                                                    Ok(name) => name,
                                                    Err(e) => {
                                                        let _ = tx_apply.send(
                                                            BackgroundMessage::DirectFixError(
                                                                format!(
                                                                    "Failed to create fix branch: {}",
                                                                    e
                                                                ),
                                                            ),
                                                        );
                                                        return;
                                                    }
                                                };

                                                let mem = if repo_memory_context.trim().is_empty() {
                                                    None
                                                } else {
                                                    Some(repo_memory_context)
                                                };

                                                // Check if this is a multi-file suggestion
                                                if suggestion.is_multi_file() {
                                                    // Multi-file fix
                                                    let all_files = suggestion.affected_files();

                                                    // Read all file contents
                                                    let mut file_contents: Vec<(PathBuf, String)> =
                                                        Vec::new();
                                                    for file_path in &all_files {
                                                        let resolved = match resolve_repo_path(
                                                            &repo_path,
                                                            file_path,
                                                        ) {
                                                            Ok(resolved) => resolved,
                                                            Err(e) => {
                                                                let _ = tx_apply.send(
                                                                    BackgroundMessage::DirectFixError(
                                                                        format!(
                                                                            "Unsafe path {}: {}",
                                                                            file_path.display(),
                                                                            e
                                                                        ),
                                                                    ),
                                                                );
                                                                return;
                                                            }
                                                        };
                                                        match std::fs::read_to_string(
                                                            &resolved.absolute,
                                                        ) {
                                                            Ok(content) => {
                                                                file_contents.push((
                                                                    resolved.relative,
                                                                    content,
                                                                ))
                                                            }
                                                            Err(e) => {
                                                                let _ = tx_apply.send(
                                                                    BackgroundMessage::DirectFixError(
                                                                        format!(
                                                                            "Failed to read {}: {}",
                                                                            file_path.display(),
                                                                            e
                                                                        ),
                                                                    ),
                                                                );
                                                                return;
                                                            }
                                                        }
                                                    }

                                                    // Generate multi-file fix
                                                    match suggest::llm::generate_multi_file_fix(
                                                        &file_contents,
                                                        &suggestion,
                                                        &preview,
                                                        mem,
                                                    )
                                                    .await
                                                    {
                                                        Ok(multi_fix) => {
                                                            // Backup all files first
                                                            let mut backups: Vec<(
                                                                PathBuf,
                                                                PathBuf,
                                                                PathBuf,
                                                            )> = Vec::new();
                                                            for file_edit in &multi_fix.file_edits {
                                                                let resolved = match resolve_repo_path(
                                                                    &repo_path,
                                                                    &file_edit.path,
                                                                ) {
                                                                    Ok(resolved) => resolved,
                                                                    Err(e) => {
                                                                        let _ = tx_apply.send(
                                                                            BackgroundMessage::DirectFixError(
                                                                                format!(
                                                                                    "Unsafe path {}: {}",
                                                                                    file_edit
                                                                                        .path
                                                                                        .display(),
                                                                                    e
                                                                                ),
                                                                            ),
                                                                        );
                                                                        return;
                                                                    }
                                                                };
                                                                let full_path = resolved.absolute;
                                                                let backup_path = full_path
                                                                    .with_extension("cosmos.bak");
                                                                if let Err(e) =
                                                                    std::fs::copy(&full_path, &backup_path)
                                                                {
                                                                    // Rollback any backups we made
                                                                    for (_, bp, _) in &backups {
                                                                        let _ =
                                                                            std::fs::remove_file(bp);
                                                                    }
                                                                    let _ = tx_apply.send(
                                                                        BackgroundMessage::DirectFixError(
                                                                            format!(
                                                                                "Failed to backup {}: {}",
                                                                                file_edit.path.display(),
                                                                                e
                                                                            ),
                                                                        ),
                                                                    );
                                                                    return;
                                                                }
                                                                backups.push((
                                                                    resolved.relative,
                                                                    full_path,
                                                                    backup_path,
                                                                ));
                                                            }

                                                            // Apply all edits
                                                            let mut file_changes: Vec<(
                                                                PathBuf,
                                                                PathBuf,
                                                                String,
                                                            )> = Vec::new();
                                                            for file_edit in &multi_fix.file_edits {
                                                                let resolved = match resolve_repo_path(
                                                                    &repo_path,
                                                                    &file_edit.path,
                                                                ) {
                                                                    Ok(resolved) => resolved,
                                                                    Err(e) => {
                                                                        let _ = tx_apply.send(
                                                                            BackgroundMessage::DirectFixError(
                                                                                format!(
                                                                                    "Unsafe path {}: {}",
                                                                                    file_edit
                                                                                        .path
                                                                                        .display(),
                                                                                    e
                                                                                ),
                                                                            ),
                                                                        );
                                                                        return;
                                                                    }
                                                                };
                                                                let full_path = resolved.absolute;
                                                                let backup_path = full_path
                                                                    .with_extension("cosmos.bak");

                                                                match std::fs::write(
                                                                    &full_path,
                                                                    &file_edit.new_content,
                                                                ) {
                                                                    Ok(_) => {
                                                                        // Stage the file
                                                                        let rel_path =
                                                                            resolved.relative.to_string_lossy().to_string();
                                                                        let _ = git_ops::stage_file(
                                                                            &repo_path,
                                                                            &rel_path,
                                                                        );

                                                                        let diff = format!(
                                                                            "Modified: {}",
                                                                            file_edit
                                                                                .modified_areas
                                                                                .join(", ")
                                                                        );
                                                                        file_changes.push((
                                                                            resolved.relative,
                                                                            backup_path,
                                                                            diff,
                                                                        ));
                                                                    }
                                                                    Err(e) => {
                                                                        // Rollback all changes
                                                                        for (_path, full, backup) in &backups {
                                                                            let _ = std::fs::copy(
                                                                                backup,
                                                                                full,
                                                                            );
                                                                            let _ =
                                                                                std::fs::remove_file(backup);
                                                                        }
                                                                        let _ = tx_apply.send(
                                                                            BackgroundMessage::DirectFixError(
                                                                                format!(
                                                                                    "Failed to write {}: {}",
                                                                                    file_edit.path.display(),
                                                                                    e
                                                                                ),
                                                                            ),
                                                                        );
                                                                        return;
                                                                    }
                                                                }
                                                            }

                                                            let _ = tx_apply.send(
                                                                BackgroundMessage::DirectFixApplied {
                                                                    suggestion_id: sid,
                                                                    file_changes,
                                                                    description: multi_fix
                                                                        .description,
                                                                    usage: multi_fix.usage,
                                                                    branch_name: created_branch,
                                                                    friendly_title: preview
                                                                        .friendly_title
                                                                        .clone(),
                                                                    problem_summary: preview
                                                                        .problem_summary
                                                                        .clone(),
                                                                    outcome: preview.outcome.clone(),
                                                                },
                                                            );
                                                        }
                                                        Err(e) => {
                                                            let _ = tx_apply.send(
                                                                BackgroundMessage::DirectFixError(
                                                                    e.to_string(),
                                                                ),
                                                            );
                                                        }
                                                    }
                                                } else {
                                                    // Single-file fix (original logic)
                                                    let resolved =
                                                        match resolve_repo_path(&repo_path, &fp) {
                                                            Ok(resolved) => resolved,
                                                            Err(e) => {
                                                                let _ = tx_apply.send(
                                                                    BackgroundMessage::DirectFixError(
                                                                        format!(
                                                                            "Unsafe path {}: {}",
                                                                            fp.display(),
                                                                            e
                                                                        ),
                                                                    ),
                                                                );
                                                                return;
                                                            }
                                                        };
                                                    let full_path = resolved.absolute;
                                                    let rel_path = resolved.relative;
                                                    let content =
                                                        match std::fs::read_to_string(&full_path) {
                                                        Ok(c) => c,
                                                        Err(e) => {
                                                            let _ = tx_apply.send(
                                                                BackgroundMessage::DirectFixError(
                                                                    format!(
                                                                        "Failed to read file: {}",
                                                                        e
                                                                    ),
                                                                ),
                                                            );
                                                            return;
                                                        }
                                                    };

                                                    match suggest::llm::generate_fix_content(
                                                        &rel_path,
                                                        &content,
                                                        &suggestion,
                                                        &preview,
                                                        mem,
                                                    )
                                                    .await
                                                    {
                                                        Ok(applied_fix) => {
                                                            let backup_path = full_path
                                                                .with_extension("cosmos.bak");
                                                            if let Err(e) = std::fs::copy(
                                                                &full_path,
                                                                &backup_path,
                                                            ) {
                                                                let _ = tx_apply.send(
                                                                    BackgroundMessage::DirectFixError(
                                                                        format!(
                                                                            "Failed to create backup: {}",
                                                                            e
                                                                        ),
                                                                    ),
                                                                );
                                                                return;
                                                            }

                                                            match std::fs::write(
                                                                &full_path,
                                                                &applied_fix.new_content,
                                                            ) {
                                                                Ok(_) => {
                                                                    let rel_path = rel_path
                                                                        .to_string_lossy()
                                                                        .to_string();
                                                                    let _ = git_ops::stage_file(
                                                                        &repo_path,
                                                                        &rel_path,
                                                                    );

                                                                    let diff = format!(
                                                                        "Modified: {}",
                                                                        applied_fix
                                                                            .modified_areas
                                                                            .join(", ")
                                                                    );

                                                                    let _ = tx_apply.send(
                                                                        BackgroundMessage::DirectFixApplied {
                                                                            suggestion_id: sid,
                                                                            file_changes: vec![(rel_path.into(), backup_path, diff)],
                                                                            description: applied_fix.description,
                                                                            usage: applied_fix.usage,
                                                                            branch_name: created_branch,
                                                                            friendly_title: preview.friendly_title.clone(),
                                                                            problem_summary: preview.problem_summary.clone(),
                                                                            outcome: preview.outcome.clone(),
                                                                        },
                                                                    );
                                                                }
                                                                Err(e) => {
                                                                    let _ = std::fs::copy(
                                                                        &backup_path,
                                                                        &full_path,
                                                                    );
                                                                    let _ = std::fs::remove_file(
                                                                        &backup_path,
                                                                    );
                                                                    let _ = tx_apply.send(
                                                                        BackgroundMessage::DirectFixError(
                                                                            format!(
                                                                                "Failed to write fix: {}",
                                                                                e
                                                                            ),
                                                                        ),
                                                                    );
                                                                }
                                                            }
                                                        }
                                                        Err(e) => {
                                                            let _ = tx_apply.send(
                                                                BackgroundMessage::DirectFixError(
                                                                    e.to_string(),
                                                                ),
                                                            );
                                                        }
                                                    }
                                                }
                                            });
                                        }
                                    }
                                }
                            }
                            WorkflowStep::Review => {
                                if !app.review_state.reviewing && !app.review_state.fixing {
                                    if !app.review_state.selected.is_empty() {
                                        // Fix selected findings (same as 'f' key)
                                        if let Err(e) = app.config.allow_ai(app.session_cost) {
                                            app.show_toast(&e);
                                            return Ok(());
                                        }
                                        let selected_findings = app.get_selected_review_findings();
                                        let file = app.review_state.file_path.clone();
                                        let content = app.review_state.new_content.clone();
                                        let original = app.review_state.original_content.clone();
                                        let iter = app.review_state.review_iteration;
                                        let fixed = app.review_state.fixed_titles.clone();
                                        let repo_memory_context =
                                            app.repo_memory.to_prompt_context(12, 900);
                                        let memory = if repo_memory_context.trim().is_empty() {
                                            None
                                        } else {
                                            Some(repo_memory_context)
                                        };
                                        let tx_fix = ctx.tx.clone();

                                        if let Some(file_path) = file {
                                            app.set_review_fixing(true);

                                            background::spawn_background(ctx.tx.clone(), "verification_fix", async move {
                                                let orig_ref = if iter > 1 {
                                                    Some(original.as_str())
                                                } else {
                                                    None
                                                };
                                                match suggest::llm::fix_review_findings(
                                                    &file_path,
                                                    &content,
                                                    orig_ref,
                                                    &selected_findings,
                                                    memory,
                                                    iter,
                                                    &fixed,
                                                )
                                                .await
                                                {
                                                    Ok(fix) => {
                                                        let _ = tx_fix.send(
                                                            BackgroundMessage::VerificationFixComplete {
                                                                new_content: fix.new_content,
                                                                description: fix.description,
                                                                usage: fix.usage,
                                                            },
                                                        );
                                                    }
                                                    Err(e) => {
                                                        let _ = tx_fix.send(
                                                            BackgroundMessage::Error(e.to_string()),
                                                        );
                                                    }
                                                }
                                            });
                                        }
                                    } else if app.review_passed() {
                                        // Review passed - move to Ship
                                        app.start_ship();
                                    } else if app.review_state.confirm_ship {
                                        app.review_state.confirm_ship = false;
                                        app.start_ship();
                                    } else {
                                        app.review_state.confirm_ship = true;
                                        app.show_toast(
                                            "Review has findings. Select items to fix or press Enter again to ship anyway.",
                                        );
                                    }
                                }
                            }
                            WorkflowStep::Ship => {
                                // Execute ship based on current step
                                match app.ship_state.step {
                                    ui::ShipStep::Confirm => {
                                        // Start the ship process
                                        let repo_path = app.repo_path.clone();
                                        let branch_name = app.ship_state.branch_name.clone();
                                        let commit_message =
                                            app.ship_state.commit_message.clone();
                                        let (pr_title, pr_body) = app.generate_pr_content();
                                        let tx_ship = ctx.tx.clone();

                                        app.set_ship_step(ui::ShipStep::Committing);

                                        background::spawn_background(ctx.tx.clone(), "ship_confirm", async move {
                                            // Execute ship workflow
                                            let _ = tx_ship.send(BackgroundMessage::ShipProgress(
                                                ui::ShipStep::Committing,
                                            ));

                                            // Commit (files are already staged)
                                            if let Err(e) =
                                                git_ops::commit(&repo_path, &commit_message)
                                            {
                                                let _ = tx_ship.send(
                                                    BackgroundMessage::ShipError(e.to_string()),
                                                );
                                                return;
                                            }

                                            let _ = tx_ship.send(BackgroundMessage::ShipProgress(
                                                ui::ShipStep::Pushing,
                                            ));

                                            // Push
                                            if let Err(e) =
                                                git_ops::push_branch(&repo_path, &branch_name)
                                            {
                                                let _ = tx_ship.send(
                                                    BackgroundMessage::ShipError(e.to_string()),
                                                );
                                                return;
                                            }

                                            let _ = tx_ship.send(BackgroundMessage::ShipProgress(
                                                ui::ShipStep::CreatingPR,
                                            ));

                                            // Create PR with human-friendly content
                                            match git_ops::create_pr(&repo_path, &pr_title, &pr_body) {
                                                Ok(url) => {
                                                    let _ = tx_ship.send(
                                                        BackgroundMessage::ShipComplete(url),
                                                    );
                                                }
                                                Err(e) => {
                                                    let _ = tx_ship.send(
                                                        BackgroundMessage::ShipError(e.to_string()),
                                                    );
                                                }
                                            }
                                        });
                                    }
                                    ui::ShipStep::Done => {
                                        // Open PR in browser and complete workflow
                                        if let Some(url) = &app.ship_state.pr_url {
                                            let _ = git_ops::open_url(url);
                                        }
                                        app.workflow_complete();
                                    }
                                    _ => {}
                                }
                            }
                        }
                    }
                }
            }
        }
        KeyCode::Esc => {
            // Handle ask cosmos mode exit first
            if app.is_ask_cosmos_mode() {
                app.exit_ask_cosmos();
            } else if app.active_panel == ActivePanel::Suggestions
                && app.workflow_step != WorkflowStep::Suggestions
            {
                // Handle workflow back navigation
                app.workflow_back();
            } else if !app.search_query.is_empty() {
                app.exit_search();
            } else if app.overlay != Overlay::None {
                app.close_overlay();
            }
        }
        KeyCode::Char('/') => app.start_search(),
        KeyCode::Char('g') => app.toggle_view_mode(),
        KeyCode::PageDown => app.page_down(),
        KeyCode::PageUp => app.page_up(),
        KeyCode::Char('?') => app.toggle_help(),
        KeyCode::Char('a') => {
            // Select all findings in Review step
            if app.active_panel == ActivePanel::Suggestions
                && app.workflow_step == WorkflowStep::Review
                && !app.review_state.reviewing
                && !app.review_state.fixing
            {
                app.review_select_all();
            }
        }
        KeyCode::Char('i') => {
            if !suggest::llm::is_available() {
                app.show_toast("Run: cosmos --setup");
            } else {
                app.start_question();
            }
        }
        KeyCode::Char('u') => {
            // Undo the last applied change (restore backup)
            match app.undo_last_pending_change() {
                Ok(()) => app.show_toast("Undone (restored backup)"),
                Err(e) => app.show_toast(&e),
            }
        }
        KeyCode::Char('R') => {
            // Open reset cosmos overlay
            app.open_reset_overlay();
        }
        _ => {}
    }

    Ok(())
}
