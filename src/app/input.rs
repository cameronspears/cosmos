use crate::app::messages::BackgroundMessage;
use crate::app::RuntimeContext;
use crate::cache;
use crate::git_ops;
use crate::index;
use crate::suggest;
use crate::ui::{ActivePanel, App, InputMode, LoadingState, Overlay, WorkflowStep};
use crate::ui;
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

                        tokio::spawn(async move {
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
                                        question,
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
                    tokio::spawn(async move {
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
                                    question,
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
                    KeyCode::Down => app.memory_move(1),
                    KeyCode::Up => app.memory_move(-1),
                    KeyCode::Char('a') => app.memory_start_add(),
                    _ => {}
                },
            }
            return Ok(());
        }

        // Safe Apply report overlay - now doubles as ship confirmation
        if let Overlay::SafeApplyReport { branch_name, .. } = &app.overlay {
            // Check if we're in shipping state
            if let Some(ship_step) = app.ship_step {
                match ship_step {
                    ui::ShipStep::Done => {
                        if key.code == KeyCode::Enter || key.code == KeyCode::Esc {
                            app.ship_step = None;
                            app.clear_pending_changes();
                            app.close_overlay();
                        }
                    }
                    _ => {
                        // During shipping, only allow Esc to cancel view
                        if key.code == KeyCode::Esc {
                            app.close_overlay();
                            app.ship_step = None;
                        }
                    }
                }
                return Ok(());
            }

            match key.code {
                KeyCode::Esc | KeyCode::Char('q') => app.close_overlay(),
                KeyCode::Down => app.overlay_scroll_down(),
                KeyCode::Up => app.overlay_scroll_up(),
                KeyCode::Char('u') => match app.undo_last_pending_change() {
                    Ok(()) => {
                        app.show_toast("Undone (restored backup)");
                        app.close_overlay();
                    }
                    Err(e) => app.show_toast(&e),
                },
                KeyCode::Char('y') => {
                    // Ship inline: stage → commit → push → PR
                    let repo_path = app.repo_path.clone();
                    let branch = branch_name.clone();
                    let commit_message = app.generate_commit_message();
                    let (pr_title, pr_body) = app.generate_pr_content();
                    let files: Vec<PathBuf> = app
                        .pending_changes
                        .iter()
                        .flat_map(|c| c.files.iter().map(|f| f.path.clone()))
                        .collect();
                    let tx_ship = ctx.tx.clone();

                    app.ship_step = Some(ui::ShipStep::Committing);

                    tokio::spawn(async move {
                        // Stage all files (handle both absolute and relative paths)
                        for file in &files {
                            let rel_path = if file.is_absolute() {
                                file.strip_prefix(&repo_path)
                                    .ok()
                                    .map(|p| p.to_path_buf())
                            } else {
                                Some(file.clone())
                            };

                            if let Some(path) = rel_path {
                                if let Err(e) =
                                    git_ops::stage_file(&repo_path, path.to_str().unwrap_or_default())
                                {
                                    let _ = tx_ship.send(BackgroundMessage::ShipError(format!(
                                        "Stage failed: {}",
                                        e
                                    )));
                                    return;
                                }
                            }
                        }

                        // Validate staging
                        if let Ok(status) = git_ops::current_status(&repo_path) {
                            if status.staged.is_empty() {
                                let _ = tx_ship.send(BackgroundMessage::ShipError(
                                    "No files staged".to_string(),
                                ));
                                return;
                            }
                        }

                        // Commit
                        if let Err(e) = git_ops::commit(&repo_path, &commit_message) {
                            let _ = tx_ship.send(BackgroundMessage::ShipError(format!(
                                "Commit failed: {}",
                                e
                            )));
                            return;
                        }
                        let _ =
                            tx_ship.send(BackgroundMessage::ShipProgress(ui::ShipStep::Pushing));

                        // Push
                        if let Err(e) = git_ops::push_branch(&repo_path, &branch) {
                            let _ = tx_ship.send(BackgroundMessage::ShipError(format!(
                                "Push failed: {}",
                                e
                            )));
                            return;
                        }
                        let _ = tx_ship.send(BackgroundMessage::ShipProgress(
                            ui::ShipStep::CreatingPR,
                        ));

                        // Create PR with human-friendly content
                        match git_ops::create_pr(&repo_path, &pr_title, &pr_body) {
                            Ok(url) => {
                                let _ = tx_ship.send(BackgroundMessage::ShipComplete(url));
                            }
                            Err(e) => {
                                let _ = tx_ship.send(BackgroundMessage::ShipError(format!(
                                    "Pushed, but PR creation failed: {}. Create PR manually.",
                                    e
                                )));
                            }
                        }
                    });
                }
                _ => {}
            }
            return Ok(());
        }

        // Handle BranchCreate overlay
        if let Overlay::BranchCreate {
            branch_name,
            commit_message,
            pending_files,
        } = &app.overlay
        {
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
                                if let Some(rel_path) = file
                                    .strip_prefix(&repo_path)
                                    .ok()
                                    .and_then(|p| p.to_str())
                                {
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
                                    let tx_push = ctx.tx.clone();
                                    tokio::spawn(async move {
                                        match git_ops::push_branch(&repo_for_push, &branch_for_push)
                                        {
                                            Ok(_) => {
                                                let _ = tx_push.send(BackgroundMessage::Error(
                                                    "Pushed! Press 'p' for PR".to_string(),
                                                ));
                                            }
                                            Err(e) => {
                                                let _ = tx_push.send(BackgroundMessage::Error(
                                                    format!("Push failed: {}", e),
                                                ));
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
            return Ok(());
        }

        // Handle ShipDialog overlay - streamlined commit + push + PR flow
        if let Overlay::ShipDialog {
            branch_name,
            commit_message,
            files,
            step,
            ..
        } = &app.overlay
        {
            let step = *step;
            match key.code {
                KeyCode::Esc | KeyCode::Char('q') => {
                    if step == ui::ShipStep::Confirm || step == ui::ShipStep::Done {
                        app.close_overlay();
                    }
                    // Don't allow cancel during in-progress steps
                }
                KeyCode::Down => app.overlay_scroll_down(),
                KeyCode::Up => app.overlay_scroll_up(),
                KeyCode::Char('y') if step == ui::ShipStep::Confirm => {
                    // Execute the full ship workflow: stage → commit → push → PR
                    let repo_path = app.repo_path.clone();
                    let branch = branch_name.clone();
                    let message = commit_message.clone();
                    let (pr_title, pr_body) = app.generate_pr_content();
                    let files = files.clone();
                    let tx_ship = ctx.tx.clone();

                    app.update_ship_step(ui::ShipStep::Committing);

                    tokio::spawn(async move {
                        // Step 1: Stage all files (handle both absolute and relative paths)
                        for file in &files {
                            let rel_path = if file.is_absolute() {
                                file.strip_prefix(&repo_path)
                                    .ok()
                                    .map(|p| p.to_path_buf())
                            } else {
                                Some(file.clone())
                            };

                            if let Some(path) = rel_path {
                                if let Err(e) =
                                    git_ops::stage_file(&repo_path, path.to_str().unwrap_or_default())
                                {
                                    let _ = tx_ship.send(BackgroundMessage::ShipError(format!(
                                        "Stage failed: {}",
                                        e
                                    )));
                                    return;
                                }
                            }
                        }

                        // Validate: ensure something is staged before committing
                        if let Ok(status) = git_ops::current_status(&repo_path) {
                            if status.staged.is_empty() {
                                let _ = tx_ship.send(BackgroundMessage::ShipError(
                                    "No files staged - nothing to commit".to_string(),
                                ));
                                return;
                            }
                        }

                        // Step 2: Commit
                        if let Err(e) = git_ops::commit(&repo_path, &message) {
                            let _ = tx_ship.send(BackgroundMessage::ShipError(format!(
                                "Commit failed: {}",
                                e
                            )));
                            return;
                        }
                        let _ =
                            tx_ship.send(BackgroundMessage::ShipProgress(ui::ShipStep::Pushing));

                        // Step 3: Push
                        if let Err(e) = git_ops::push_branch(&repo_path, &branch) {
                            let _ = tx_ship.send(BackgroundMessage::ShipError(format!(
                                "Push failed: {}",
                                e
                            )));
                            return;
                        }
                        let _ = tx_ship.send(BackgroundMessage::ShipProgress(
                            ui::ShipStep::CreatingPR,
                        ));

                        // Step 4: Create PR with human-friendly content
                        match git_ops::create_pr(&repo_path, &pr_title, &pr_body) {
                            Ok(url) => {
                                let _ = tx_ship.send(BackgroundMessage::ShipComplete(url));
                            }
                            Err(e) => {
                                // PR creation failed but commit/push succeeded
                                let _ = tx_ship.send(BackgroundMessage::ShipError(format!(
                                    "Pushed, but PR creation failed: {}. Create PR manually.",
                                    e
                                )));
                            }
                        }
                    });
                }
                KeyCode::Enter if step == ui::ShipStep::Done => {
                    // Open the PR URL in browser
                    if let Some(url) = &app.pr_url {
                        let _ = git_ops::open_url(url);
                    }
                    app.clear_pending_changes();
                    app.close_overlay();
                }
                _ => {}
            }
            return Ok(());
        }

        // Handle GitStatus overlay - simplified interface
        if let Overlay::GitStatus { commit_input, .. } = &app.overlay {
            // Check if we're in commit input mode
            if commit_input.is_some() {
                match key.code {
                    KeyCode::Esc => {
                        app.git_cancel_commit();
                    }
                    KeyCode::Enter => match app.git_do_commit() {
                        Ok(_oid) => {
                            app.show_toast("+ Committed - Press 's' to Ship");
                            app.close_overlay();
                        }
                        Err(e) => {
                            app.show_toast(&e);
                        }
                    },
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
                    KeyCode::Down => {
                        app.git_status_navigate(1);
                    }
                    KeyCode::Up => {
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
                    KeyCode::Char('r') => {
                        // Restore selected file (discard changes)
                        app.git_restore_selected();
                    }
                    KeyCode::Char('S') => {
                        // Stage all files
                        app.git_stage_all();
                    }
                    KeyCode::Char('X') => {
                        // Hard reset - discard all changes
                        match app.git_reset_hard() {
                            Ok(_) => {
                                app.show_toast("Reset complete - all changes discarded");
                                app.close_overlay();
                            }
                            Err(e) => {
                                app.show_toast(&format!("Reset failed: {}", e));
                            }
                        }
                    }
                    KeyCode::Char('m') => {
                        // Switch to main branch
                        match app.git_switch_to_main() {
                            Ok(_) => {
                                app.show_toast("Switched to main branch");
                                app.refresh_git_status();
                            }
                            Err(e) => {
                                app.show_toast(&format!("Switch failed: {}", e));
                            }
                        }
                    }
                    KeyCode::Char('P') => {
                        // Push current branch
                        let branch = app.context.branch.clone();
                        match git_ops::push_branch(&app.repo_path, &branch) {
                            Ok(_) => {
                                app.show_toast(&format!("Pushed {}", branch));
                                app.refresh_git_status();
                            }
                            Err(e) => {
                                app.show_toast(&format!("Push failed: {}", e));
                            }
                        }
                    }
                    KeyCode::Char('d') => {
                        // Delete untracked file
                        app.git_delete_untracked();
                    }
                    _ => {}
                }
            }
            return Ok(());
        }

        // Handle ErrorLog overlay
        if let Overlay::ErrorLog { .. } = &app.overlay {
            match key.code {
                KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('e') => app.close_overlay(),
                KeyCode::Down => {
                    // Navigate down in error log
                    let max = app.error_log.len().saturating_sub(1);
                    if let Overlay::ErrorLog { selected, scroll } = &mut app.overlay {
                        if *selected < max {
                            *selected += 1;
                        }
                        // Keep selected in view
                        let visible = 10;
                        if *selected >= *scroll + visible {
                            *scroll = selected.saturating_sub(visible - 1);
                        }
                    }
                }
                KeyCode::Up => {
                    // Navigate up in error log
                    if let Overlay::ErrorLog { selected, scroll } = &mut app.overlay {
                        *selected = selected.saturating_sub(1);
                        if *selected < *scroll {
                            *scroll = *selected;
                        }
                    }
                }
                KeyCode::Char('c') => {
                    // Clear error log
                    app.clear_error_log();
                    app.close_overlay();
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
                                }

                                // Clear in-memory glossary if needed
                                if needs_glossary {
                                    app.glossary = crate::cache::DomainGlossary::default();
                                }

                                // Refresh context
                                let _ = app.context.refresh();

                                // Check if AI is available for regeneration
                                let ai_enabled = suggest::llm::is_available()
                                    && app.config.allow_ai(app.session_cost).is_ok();

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

                                        tokio::spawn(async move {
                                            let cache = cache::Cache::new(&cache_path);

                                            // Start with fresh cache after reset
                                            let mut llm_cache = cache::LlmSummaryCache::new();
                                            let mut glossary = cache::DomainGlossary::new();

                                            let mut all_summaries = HashMap::new();
                                            let mut total_usage = suggest::llm::Usage::default();
                                            let mut completed_count = 0usize;

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
                                                    if let Ok((summaries, batch_glossary, usage)) = suggest::llm::generate_summaries_for_files(
                                                        &index_clone2, batch, &project_context
                                                    ).await {
                                                        for (path, summary) in &summaries {
                                                            if let Some(hash) = file_hashes_clone.get(path) {
                                                                llm_cache.set_summary(path.clone(), summary.clone(), hash.clone());
                                                            }
                                                        }
                                                        glossary.merge(&batch_glossary);
                                                        
                                                        let _ = cache.save_llm_summaries_cache(&llm_cache);
                                                        let _ = cache.save_glossary(&glossary);
                                                        
                                                        completed_count += summaries.len();
                                                        
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

                                            let _ = tx_summaries.send(BackgroundMessage::SummariesReady {
                                                summaries: HashMap::new(),
                                                usage: final_usage,
                                            });
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

                                    tokio::spawn(async move {
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
            if app.active_panel == ActivePanel::Suggestions && app.workflow_step == WorkflowStep::Review
            {
                if !app.review_state.reviewing && !app.review_state.fixing {
                    app.review_toggle_finding();
                }
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

                    tokio::spawn(async move {
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
                                        let repo_memory_context =
                                            app.repo_memory.to_prompt_context(12, 900);

                                        // Move to Verify step (with multi-file support)
                                        app.start_verify_multi(
                                            suggestion_id,
                                            file_path.clone(),
                                            additional_files,
                                            summary.clone(),
                                        );

                                        tokio::spawn(async move {
                                            let mem = if repo_memory_context.trim().is_empty() {
                                                None
                                            } else {
                                                Some(repo_memory_context)
                                            };
                                            match suggest::llm::generate_fix_preview(
                                                &file_path,
                                                &suggestion_clone,
                                                None,
                                                mem,
                                            )
                                            .await
                                            {
                                                Ok(preview) => {
                                                    let _ = tx_preview
                                                        .send(BackgroundMessage::PreviewReady {
                                                            suggestion_id,
                                                            file_path,
                                                            summary,
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
                                            app.loading = LoadingState::GeneratingFix;

                                            tokio::spawn(async move {
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
                                                        let full_path = repo_path.join(file_path);
                                                        match std::fs::read_to_string(&full_path) {
                                                            Ok(content) => {
                                                                file_contents.push((
                                                                    (*file_path).clone(),
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
                                                            let mut backups: Vec<(PathBuf, PathBuf)> =
                                                                Vec::new();
                                                            for file_edit in &multi_fix.file_edits {
                                                                let full_path =
                                                                    repo_path.join(&file_edit.path);
                                                                let backup_path = full_path
                                                                    .with_extension("cosmos.bak");
                                                                if let Err(e) =
                                                                    std::fs::copy(&full_path, &backup_path)
                                                                {
                                                                    // Rollback any backups we made
                                                                    for (_, bp) in &backups {
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
                                                                    file_edit.path.clone(),
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
                                                                let full_path =
                                                                    repo_path.join(&file_edit.path);
                                                                let backup_path = full_path
                                                                    .with_extension("cosmos.bak");

                                                                match std::fs::write(
                                                                    &full_path,
                                                                    &file_edit.new_content,
                                                                ) {
                                                                    Ok(_) => {
                                                                        // Stage the file
                                                                        let rel_path = full_path
                                                                            .strip_prefix(&repo_path)
                                                                            .map(|p| {
                                                                                p.to_string_lossy()
                                                                                    .to_string()
                                                                            })
                                                                            .unwrap_or_else(|_| {
                                                                                file_edit
                                                                                    .path
                                                                                    .to_string_lossy()
                                                                                    .to_string()
                                                                            });
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
                                                                            file_edit.path.clone(),
                                                                            backup_path,
                                                                            diff,
                                                                        ));
                                                                    }
                                                                    Err(e) => {
                                                                        // Rollback all changes
                                                                        for (path, backup) in &backups {
                                                                            let full =
                                                                                repo_path.join(path);
                                                                            let _ = std::fs::copy(
                                                                                backup,
                                                                                &full,
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

                                                            let safety_checks =
                                                                crate::safe_apply::run(&repo_path);

                                                            let _ = tx_apply.send(
                                                                BackgroundMessage::DirectFixApplied {
                                                                    suggestion_id: sid,
                                                                    file_changes,
                                                                    description: multi_fix
                                                                        .description,
                                                                    safety_checks,
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
                                                    let full_path = repo_path.join(&fp);
                                                    let content = match std::fs::read_to_string(
                                                        &full_path,
                                                    ) {
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
                                                        &fp,
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
                                                                    let rel_path = full_path
                                                                        .strip_prefix(&repo_path)
                                                                        .map(|p| {
                                                                            p.to_string_lossy()
                                                                                .to_string()
                                                                        })
                                                                        .unwrap_or_else(|_| {
                                                                            fp.to_string_lossy()
                                                                                .to_string()
                                                                        });
                                                                    let _ = git_ops::stage_file(
                                                                        &repo_path,
                                                                        &rel_path,
                                                                    );

                                                                    let safety_checks =
                                                                        crate::safe_apply::run(
                                                                            &repo_path,
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
                                                                            file_changes: vec![(fp, backup_path, diff)],
                                                                            description: applied_fix.description,
                                                                            safety_checks,
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
                                // If findings are selected, fix them; otherwise move to Ship
                                if !app.review_state.reviewing
                                    && !app.review_state.fixing
                                    && !app.review_state.selected.is_empty()
                                {
                                    // Fix selected findings (same as 'f' key)
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

                                        tokio::spawn(async move {
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
                                } else if app.review_passed() || app.review_state.selected.is_empty()
                                {
                                    // No selections or review passed - move to Ship
                                    app.start_ship();
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

                                        tokio::spawn(async move {
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
            if app.active_panel == ActivePanel::Suggestions && app.workflow_step == WorkflowStep::Review
            {
                if !app.review_state.reviewing && !app.review_state.fixing {
                    app.review_select_all();
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
