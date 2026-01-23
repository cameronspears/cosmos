use crate::app::messages::BackgroundMessage;
use crate::app::RuntimeContext;
use crate::cache;
use crate::suggest;
use crate::ui::{App, LoadingState, WorkflowStep};
use crate::ui;
use crate::util::truncate;
use futures::FutureExt;
use std::path::PathBuf;
use std::sync::mpsc;
use std::future::Future;
use std::panic::AssertUnwindSafe;

pub fn drain_messages(
    app: &mut App,
    rx: &mpsc::Receiver<BackgroundMessage>,
    ctx: &RuntimeContext,
) {
    while let Ok(msg) = rx.try_recv() {
        match msg {
            BackgroundMessage::SuggestionsReady {
                suggestions,
                usage,
                model,
            } => {
                let count = suggestions.len();
                for s in suggestions {
                    app.suggestions.add_llm_suggestion(s);
                }

                // Diff-first ordering: changed files and their blast radius float to the top.
                app.suggestions.sort_with_context(&app.context);

                // Track cost (Smart model for suggestions)
                if let Some(u) = usage {
                    let cost = u.calculate_cost(suggest::llm::Model::Smart);
                    app.session_cost += cost;
                    app.session_tokens += u.total_tokens;
                    ctx.budget_guard.record_usage(cost, u.total_tokens);
                    let _ = app.config.record_tokens(u.total_tokens);
                    let _ = app
                        .config
                        .allow_ai(app.session_cost)
                        .map_err(|e| app.show_toast(&e));
                }

                // If summaries are still generating, switch to that loading state
                // Otherwise, clear loading
                if app.needs_summary_generation && app.summary_progress.is_some() {
                    app.loading = LoadingState::GeneratingSummaries;
                } else {
                    app.loading = LoadingState::None;
                }

                // More prominent toast for suggestions
                app.show_toast(&format!("{} suggestions ready ({})", count, &model));
                app.active_model = Some(model);
            }
            BackgroundMessage::SuggestionsError(e) => {
                // If summaries are still generating, switch to that loading state
                if app.needs_summary_generation && app.summary_progress.is_some() {
                    app.loading = LoadingState::GeneratingSummaries;
                } else {
                    app.loading = LoadingState::None;
                }
                app.show_toast(&format!("Error: {}", truncate(&e, 80)));
            }
            BackgroundMessage::SummariesReady {
                summaries,
                usage,
                failed_files,
            } => {
                let new_count = summaries.len();
                app.update_summaries(summaries);
                let failed_count = failed_files.len();
                app.summary_failed_files = failed_files;
                // Track cost (using Speed preset for summaries)
                if let Some(u) = usage {
                    let cost = u.calculate_cost(suggest::llm::Model::Speed);
                    app.session_cost += cost;
                    app.session_tokens += u.total_tokens;
                    ctx.budget_guard.record_usage(cost, u.total_tokens);
                    let _ = app.config.record_tokens(u.total_tokens);
                    let _ = app
                        .config
                        .allow_ai(app.session_cost)
                        .map_err(|e| app.show_toast(&e));
                }

                // Reload glossary from cache (it was built during summary generation)
                let cache = cache::Cache::new(ctx.repo_path);
                if let Some(new_glossary) = cache.load_glossary() {
                    app.glossary = new_glossary;
                }

                app.summary_progress = None;
                app.needs_summary_generation = failed_count > 0;

                // If we're waiting to generate suggestions after reset, do it now
                if app.pending_suggestions_on_init {
                    app.pending_suggestions_on_init = false;

                    // Check if AI is still available
                    let mut ai_enabled = suggest::llm::is_available();
                    if ai_enabled {
                        if let Err(e) = app.config.allow_ai(app.session_cost) {
                            app.show_toast(&e);
                            ai_enabled = false;
                        }
                    }

                    if ai_enabled {
                        let index_clone = app.index.clone();
                        let context_clone = app.context.clone();
                        let tx_suggestions = ctx.tx.clone();
                        let cache_clone_path = ctx.repo_path.clone();
                        let repo_memory_context = app.repo_memory.to_prompt_context(12, 900);
                        let glossary_clone = app.glossary.clone();
                        let budget_guard = ctx.budget_guard.clone();

                        app.loading = LoadingState::GeneratingSuggestions;
                        if failed_count == 0 {
                            app.show_toast(&format!(
                                "{} terms in glossary · generating suggestions...",
                                glossary_clone.len()
                            ));
                        }

                        spawn_background(ctx.tx.clone(), "suggestions_generation", async move {
                            let mut config = crate::config::Config::load();
                            if let Err(e) = budget_guard.allow_ai(&mut config) {
                                let _ = tx_suggestions
                                    .send(BackgroundMessage::SuggestionsError(e));
                                return;
                            }
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
                                    let cache_data =
                                        cache::SuggestionsCache::from_suggestions(&suggestions);
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
                    } else {
                        app.loading = LoadingState::None;
                        if failed_count > 0 {
                            let message = format!(
                                "Summaries incomplete: {} files failed. Press 'R' to retry.",
                                failed_count
                            );
                            app.show_toast(&message);
                        } else if new_count > 0 {
                            app.show_toast(&format!(
                                "{} summaries · {} glossary terms",
                                new_count,
                                app.glossary.len()
                            ));
                        }
                    }
                } else {
                    // Not waiting for suggestions, just finish up
                    if !matches!(app.loading, LoadingState::GeneratingSuggestions) {
                        app.loading = LoadingState::None;
                    }
                    if failed_count > 0 {
                        let message = format!(
                            "Summaries incomplete: {} files failed. Press 'R' to retry.",
                            failed_count
                        );
                        app.show_toast(&message);
                    } else if new_count > 0 {
                        app.show_toast(&format!(
                            "{} summaries · {} glossary terms",
                            new_count,
                            app.glossary.len()
                        ));
                    } else {
                        app.show_toast(&format!(
                            "Summaries ready · {} glossary terms",
                            app.glossary.len()
                        ));
                    }
                }
            }
            BackgroundMessage::SummaryProgress {
                completed,
                total,
                summaries,
            } => {
                // Merge new summaries as they arrive
                app.update_summaries(summaries);
                // Track progress for display
                app.summary_progress = Some((completed, total));
            }
            BackgroundMessage::SummariesError(e) => {
                // Only clear loading if we're not still waiting for suggestions
                if !matches!(app.loading, LoadingState::GeneratingSuggestions) {
                    app.loading = LoadingState::None;
                }
                app.summary_progress = None;
                app.show_toast(&format!("Summary error: {}", truncate(&e, 80)));
            }
            BackgroundMessage::GroupingEnhanced {
                grouping,
                updated_files,
                usage,
                model,
            } => {
                if updated_files > 0 {
                    app.apply_grouping_update(grouping);
                }

                if let Some(u) = usage {
                    let cost = u.calculate_cost(suggest::llm::Model::Balanced);
                    app.session_cost += cost;
                    app.session_tokens += u.total_tokens;
                    ctx.budget_guard.record_usage(cost, u.total_tokens);
                    let _ = app.config.record_tokens(u.total_tokens);
                    let _ = app
                        .config
                        .allow_ai(app.session_cost)
                        .map_err(|e| app.show_toast(&e));
                }

                if updated_files > 0 {
                    app.show_toast(&format!(
                        "Grouping updated for {} files ({})",
                        updated_files, model
                    ));
                    app.active_model = Some(model);
                }
            }
            BackgroundMessage::GroupingEnhanceError(e) => {
                app.show_toast(&format!("Grouping error: {}", truncate(&e, 80)));
            }
            BackgroundMessage::PreviewReady { preview, .. } => {
                app.loading = LoadingState::None;
                // Set the preview in the Verify workflow step
                app.set_verify_preview(preview);
            }
            BackgroundMessage::PreviewError(e) => {
                app.loading = LoadingState::None;
                // Reset workflow if we were in Verify step
                if app.workflow_step == WorkflowStep::Verify {
                    app.workflow_step = WorkflowStep::Suggestions;
                    app.verify_state = ui::VerifyState::default();
                }
                app.show_toast(&format!("Preview error: {}", truncate(&e, 80)));
            }
            BackgroundMessage::DirectFixApplied {
                suggestion_id,
                file_changes,
                description,
                usage,
                branch_name,
                friendly_title,
                problem_summary,
                outcome,
            } => {
                // Track cost
                if let Some(u) = usage {
                    let cost = u.calculate_cost(suggest::llm::Model::Smart);
                    app.session_cost += cost;
                    app.session_tokens += u.total_tokens;
                    ctx.budget_guard.record_usage(cost, u.total_tokens);
                    let _ = app.config.record_tokens(u.total_tokens);
                    let _ = app
                        .config
                        .allow_ai(app.session_cost)
                        .map_err(|e| app.show_toast(&e));
                }

                app.loading = LoadingState::None;
                app.suggestions.mark_applied(suggestion_id);

                // Store the cosmos branch name - this enables the Ship workflow
                app.cosmos_branch = Some(branch_name.clone());

                // Convert file_changes to FileChange structs for multi-file support
                let ui_file_changes: Vec<ui::FileChange> = file_changes
                    .iter()
                    .map(|(path, backup, diff)| {
                        ui::FileChange::new(path.clone(), diff.clone(), backup.clone())
                    })
                    .collect();

                // Track as pending change with multi-file support
                app.pending_changes
                    .push(ui::PendingChange::with_preview_context_multi(
                        suggestion_id,
                        ui_file_changes,
                        description.clone(),
                        friendly_title,
                        problem_summary.clone(),
                        outcome.clone(),
                    ));

                // Read original (backup) and new content for verification (all files)
                let files_with_content: Vec<(PathBuf, String, String)> = file_changes
                    .iter()
                    .map(|(path, backup, _diff)| {
                        let original = std::fs::read_to_string(backup).unwrap_or_default();
                        let full_path = app.repo_path.join(path);
                        let new_content =
                            std::fs::read_to_string(&full_path).unwrap_or_default();
                        (path.clone(), original, new_content)
                    })
                    .collect();

                // Transition to Review workflow step (use first file for display)
                let first_file = file_changes
                    .first()
                    .map(|(p, _, _)| p.clone())
                    .unwrap_or_default();
                let first_original = files_with_content
                    .first()
                    .map(|(_, o, _)| o.clone())
                    .unwrap_or_default();
                let first_new = files_with_content
                    .first()
                    .map(|(_, _, n)| n.clone())
                    .unwrap_or_default();
                app.start_review(first_file, first_original.clone(), first_new.clone());

                // Trigger verification in background (all files)
                if let Err(e) = app.config.allow_ai(app.session_cost) {
                    app.show_toast(&e);
                } else {
                    let tx_verify = ctx.tx.clone();
                    spawn_background(ctx.tx.clone(), "verification", async move {
                        match suggest::llm::verify_changes(&files_with_content, 1, &[]).await {
                            Ok(review) => {
                                let _ = tx_verify.send(BackgroundMessage::VerificationComplete {
                                    findings: review.findings,
                                    summary: review.summary,
                                    usage: review.usage,
                                });
                            }
                            Err(e) => {
                                let _ = tx_verify.send(BackgroundMessage::Error(format!(
                                    "Verification failed: {}",
                                    e
                                )));
                            }
                        }
                    });
                }
            }
            BackgroundMessage::DirectFixError(e) => {
                app.loading = LoadingState::None;
                // Reset workflow if we were in Verify step
                if app.workflow_step == WorkflowStep::Verify {
                    app.workflow_step = WorkflowStep::Suggestions;
                    app.verify_state = ui::VerifyState::default();
                }
                app.show_toast(&format!("Apply failed: {}", truncate(&e, 80)));
            }
            BackgroundMessage::ShipProgress(step) => {
                // Handle workflow mode
                if app.workflow_step == WorkflowStep::Ship {
                    app.set_ship_step(step);
                } else {
                    app.ship_step = Some(step);
                }
            }
            BackgroundMessage::ShipComplete(url) => {
                // Handle workflow mode
                if app.workflow_step == WorkflowStep::Ship {
                    app.set_ship_pr_url(url.clone());
                    app.show_toast("PR created!");
                } else {
                    app.ship_step = Some(ui::ShipStep::Done);
                    app.pr_url = Some(url.clone());
                    app.clear_pending_changes();
                }
            }
            BackgroundMessage::ShipError(e) => {
                app.ship_step = None;
                app.close_overlay();
                app.show_toast(&format!("Ship failed: {}", truncate(&e, 80)));
            }
            BackgroundMessage::Error(e) => {
                app.loading = LoadingState::None;
                app.show_toast(&truncate(&e, 100));
            }
            BackgroundMessage::QuestionResponse {
                answer,
                usage,
                ..
            } => {
                // Track cost (Balanced model for questions)
                if let Some(u) = usage {
                    let cost = u.calculate_cost(suggest::llm::Model::Balanced);
                    app.session_cost += cost;
                    app.session_tokens += u.total_tokens;
                    ctx.budget_guard.record_usage(cost, u.total_tokens);
                    let _ = app.config.record_tokens(u.total_tokens);
                    let _ = app
                        .config
                        .allow_ai(app.session_cost)
                        .map_err(|e| app.show_toast(&e));
                }

                app.loading = LoadingState::None;
                // Show the response in the ask cosmos panel
                app.show_inquiry(answer);
            }
            BackgroundMessage::VerificationComplete {
                findings,
                summary,
                usage,
            } => {
                // Track cost
                if let Some(u) = usage {
                    let cost = u.calculate_cost(suggest::llm::Model::Reviewer);
                    app.session_cost += cost;
                    app.session_tokens += u.total_tokens;
                    ctx.budget_guard.record_usage(cost, u.total_tokens);
                    let _ = app.config.record_tokens(u.total_tokens);
                }
                // Update the Review workflow step with findings
                app.set_review_findings(findings, summary);
            }
            BackgroundMessage::VerificationFixComplete {
                new_content,
                description,
                usage,
            } => {
                // Track cost
                if let Some(u) = usage {
                    let cost = u.calculate_cost(suggest::llm::Model::Smart);
                    app.session_cost += cost;
                    app.session_tokens += u.total_tokens;
                    ctx.budget_guard.record_usage(cost, u.total_tokens);
                    let _ = app.config.record_tokens(u.total_tokens);
                }

                app.show_toast(&format!("Fixed: {}", truncate(&description, 40)));

                // Update workflow review state
                let file_path = app.review_state.file_path.clone();
                let original_content = app.review_state.original_content.clone();
                let iteration = app.review_state.review_iteration + 1;
                let fixed_titles = app.review_state.fixed_titles.clone();

                app.review_fix_complete(new_content.clone());

                // Trigger re-review
                // Note: On re-reviews, we don't pass suggestion context because we're
                // verifying fixes to the reviewer's findings, not the original suggestion
                if let Some(fp) = file_path {
                    if let Err(e) = app.config.allow_ai(app.session_cost) {
                        app.show_toast(&e);
                    } else {
                        app.review_state.reviewing = true;
                        app.loading = LoadingState::ReviewingChanges;

                        let tx_verify = ctx.tx.clone();
                        spawn_background(ctx.tx.clone(), "re_verification", async move {
                            let files_with_content = vec![(fp, original_content, new_content)];
                            match suggest::llm::verify_changes(
                                &files_with_content,
                                iteration,
                                &fixed_titles,
                            )
                            .await
                            {
                                Ok(review) => {
                                    let _ =
                                        tx_verify.send(BackgroundMessage::VerificationComplete {
                                            findings: review.findings,
                                            summary: review.summary,
                                            usage: review.usage,
                                        });
                                }
                                Err(e) => {
                                    let _ = tx_verify.send(BackgroundMessage::Error(format!(
                                        "Re-verification failed: {}",
                                        e
                                    )));
                                }
                            }
                        });
                    }
                }
            }
        }
    }
}

pub fn spawn_background<F>(
    tx: mpsc::Sender<BackgroundMessage>,
    task_name: &'static str,
    fut: F,
) where
    F: Future<Output = ()> + Send + 'static,
{
    tokio::spawn(async move {
        if let Err(panic) = AssertUnwindSafe(fut).catch_unwind().await {
            let detail = if let Some(s) = panic.downcast_ref::<&str>() {
                s.to_string()
            } else if let Some(s) = panic.downcast_ref::<String>() {
                s.clone()
            } else {
                "unknown panic payload".to_string()
            };
            let _ = tx.send(BackgroundMessage::Error(format!(
                "Background task '{}' crashed unexpectedly: {}",
                task_name, detail
            )));
        }
    });
}
