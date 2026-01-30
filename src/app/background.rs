//! Background task handling for Cosmos
//!
//! # Error Handling Patterns
//!
//! This module uses `let _ =` in several places. Here's why:
//!
//! - **Channel sends** (`tx.send(...)`): If the receiver is dropped (e.g., the app
//!   is shutting down), the send fails. This is expected and safe to ignore since
//!   no one is listening for the result anyway.
//!
//! - **Cache saves** (`cache.save_*()`): These are best-effort operations. Failure
//!   means we'll regenerate the data next time. Not ideal but not catastrophic.
//!

use crate::app::messages::BackgroundMessage;
use crate::app::RuntimeContext;
use crate::cache;
use crate::suggest;
use crate::ui;
use crate::ui::{App, LoadingState, WorkflowStep};
use crate::util::truncate;
use futures::FutureExt;
use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::path::PathBuf;
use std::sync::mpsc;

pub fn drain_messages(app: &mut App, rx: &mpsc::Receiver<BackgroundMessage>, ctx: &RuntimeContext) {
    while let Ok(msg) = rx.try_recv() {
        match msg {
            BackgroundMessage::SuggestionsReady {
                suggestions,
                usage,
                model,
                diagnostics,
            } => {
                let count = suggestions.len();
                for s in suggestions {
                    app.suggestions.add_llm_suggestion(s);
                }

                // Diff-first ordering: changed files and their blast radius float to the top.
                app.suggestions.sort_with_context(&app.context);

                // Track session cost for display
                if let Some(u) = usage {
                    let cost = u.cost();
                    app.session_cost += cost;
                    app.session_tokens += u.total_tokens;
                    // Refresh wallet balance after spending
                    spawn_balance_refresh(ctx.tx.clone());
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
                app.last_suggestion_diagnostics = Some(diagnostics);
                app.last_suggestion_error = None;
            }
            BackgroundMessage::SuggestionsError(e) => {
                // If summaries are still generating, switch to that loading state
                if app.needs_summary_generation && app.summary_progress.is_some() {
                    app.loading = LoadingState::GeneratingSummaries;
                } else {
                    app.loading = LoadingState::None;
                }
                app.show_toast(&format!("Suggestions error: {}", truncate(&e, 80)));
                app.last_suggestion_error = Some(e);
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
                // Track session cost for display
                if let Some(u) = usage {
                    let cost = u.cost();
                    app.session_cost += cost;
                    app.session_tokens += u.total_tokens;
                    // Refresh wallet balance after spending
                    spawn_balance_refresh(ctx.tx.clone());
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
                    let ai_enabled = suggest::llm::is_available();

                    if ai_enabled {
                        let index_clone = app.index.clone();
                        let context_clone = app.context.clone();
                        let tx_suggestions = ctx.tx.clone();
                        let cache_clone_path = ctx.repo_path.clone();
                        let repo_memory_context = app.repo_memory.to_prompt_context(12, 900);
                        let glossary_clone = app.glossary.clone();

                        app.loading = LoadingState::GeneratingSuggestions;
                        if failed_count == 0 {
                            app.show_toast(&format!(
                                "{} terms in glossary · generating suggestions...",
                                glossary_clone.len()
                            ));
                        }

                        let repo_root = cache_clone_path.clone();
                        spawn_background(ctx.tx.clone(), "suggestions_generation", async move {
                            let mem = if repo_memory_context.trim().is_empty() {
                                None
                            } else {
                                Some(repo_memory_context)
                            };
                            // Fast grounded suggestions: one LLM call, no tools, strict latency budget.
                            match suggest::llm::analyze_codebase_fast_grounded(
                                &repo_root,
                                &index_clone,
                                &context_clone,
                                mem,
                            )
                            .await
                            {
                                Ok((suggestions, usage, diagnostics)) => {
                                    let _ =
                                        tx_suggestions.send(BackgroundMessage::SuggestionsReady {
                                            suggestions,
                                            usage,
                                            model: "fast-grounded".to_string(),
                                            diagnostics,
                                        });
                                }
                                Err(e) => {
                                    let _ = tx_suggestions
                                        .send(BackgroundMessage::SuggestionsError(e.to_string()));
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
                    let cost = u.cost();
                    app.session_cost += cost;
                    app.session_tokens += u.total_tokens;
                    // Refresh wallet balance after spending
                    spawn_balance_refresh(ctx.tx.clone());
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
            BackgroundMessage::PreviewReady {
                preview,
                file_hashes,
            } => {
                app.loading = LoadingState::None;
                // Set the preview in the Verify workflow step
                app.set_verify_preview(preview, file_hashes);
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
                // Track session cost for display
                if let Some(u) = usage {
                    let cost = u.cost();
                    app.session_cost += cost;
                    app.session_tokens += u.total_tokens;
                    // Refresh wallet balance after spending
                    spawn_balance_refresh(ctx.tx.clone());
                }

                app.loading = LoadingState::None;
                app.suggestions.mark_applied(suggestion_id);

                // Store the cosmos branch name - this enables the Ship workflow
                app.cosmos_branch = Some(branch_name.clone());

                // Convert file_changes to FileChange structs for multi-file support
                let ui_file_changes: Vec<ui::FileChange> = file_changes
                    .iter()
                    .map(|(path, diff)| ui::FileChange::new(path.clone(), diff.clone()))
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

                // Read original (from git HEAD) and new content for verification (all files)
                let files_with_content: Vec<(PathBuf, String, String)> = file_changes
                    .iter()
                    .map(|(path, _diff)| {
                        // Get original from git HEAD (empty string for new files)
                        let original = crate::git_ops::read_file_from_head(&app.repo_path, path)
                            .unwrap_or(None)
                            .unwrap_or_default();
                        let full_path = app.repo_path.join(path);
                        let new_content = std::fs::read_to_string(&full_path).unwrap_or_default();
                        (path.clone(), original, new_content)
                    })
                    .collect();

                // Transition to Review workflow step (use first file for display)
                let first_file = file_changes
                    .first()
                    .map(|(p, _)| p.clone())
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
                {
                    let tx_verify = ctx.tx.clone();

                    // Build fix context so the reviewer knows what the fix was supposed to do
                    let fix_context = suggest::llm::FixContext {
                        problem_summary: problem_summary.clone(),
                        outcome: outcome.clone(),
                        description: description.clone(),
                        modified_areas: Vec::new(), // Could be extracted from fix response if needed
                    };

                    spawn_background(ctx.tx.clone(), "verification", async move {
                        match suggest::llm::verify_changes(
                            &files_with_content,
                            1,
                            &[],
                            Some(&fix_context),
                        )
                        .await
                        {
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
            BackgroundMessage::ResetComplete { options } => {
                app.loading = LoadingState::None;
                let labels: Vec<&str> = options.iter().map(|o| o.label()).collect();
                if labels.is_empty() {
                    app.show_toast("Reset complete");
                } else {
                    app.show_toast(&format!("Reset complete: {}", labels.join(", ")));
                }
            }
            BackgroundMessage::StashComplete { message } => {
                app.loading = LoadingState::None;
                app.show_toast(&format!("Changes saved: {}", message));
            }
            BackgroundMessage::DiscardComplete => {
                app.loading = LoadingState::None;
                app.show_toast("Changes discarded - starting fresh");
            }
            BackgroundMessage::Error(e) => {
                app.loading = LoadingState::None;
                // Reset review fixing state if we were applying review fixes
                if app.review_state.fixing {
                    app.review_state.fixing = false;
                }

                // Check if this is a verification error - allow user to proceed anyway
                if e.contains("verification failed") || e.contains("Re-verification failed") {
                    app.review_state.reviewing = false;
                    app.review_state.verification_failed = true;
                    app.review_state.verification_error = Some(truncate(&e, 200).to_string());
                    // Set a summary indicating verification was skipped
                    if app.review_state.summary.is_empty() {
                        app.review_state.summary =
                            "Verification unavailable - you can still proceed to ship".to_string();
                    }
                    app.show_toast(
                        "Verification failed - press Enter to ship anyway or fix issues manually",
                    );
                } else {
                    app.show_toast(&truncate(&e, 100));
                }
            }
            BackgroundMessage::QuestionResponse { answer, usage, .. } => {
                // Track session cost for display
                if let Some(u) = usage {
                    let cost = u.cost();
                    app.session_cost += cost;
                    app.session_tokens += u.total_tokens;
                    // Refresh wallet balance after spending
                    spawn_balance_refresh(ctx.tx.clone());
                }

                app.loading = LoadingState::None;
                // Show the response in the ask cosmos panel
                app.show_inquiry(answer);
            }
            BackgroundMessage::QuestionResponseWithCache {
                question,
                answer,
                usage,
                context_hash,
            } => {
                // Track session cost for display
                if let Some(u) = &usage {
                    let cost = u.cost();
                    app.session_cost += cost;
                    app.session_tokens += u.total_tokens;
                    // Refresh wallet balance after spending
                    spawn_balance_refresh(ctx.tx.clone());
                }

                // Store answer in cache
                app.question_cache
                    .set(question, answer.clone(), context_hash);
                // Save cache to disk
                let cache = cache::Cache::new(&app.repo_path);
                let _ = cache.save_question_cache(&app.question_cache);

                app.loading = LoadingState::None;
                // Show the response in the ask cosmos panel
                app.show_inquiry(answer);
            }
            BackgroundMessage::VerificationComplete {
                findings,
                summary,
                usage,
            } => {
                // Track session cost for display
                if let Some(u) = usage {
                    let cost = u.cost();
                    app.session_cost += cost;
                    app.session_tokens += u.total_tokens;
                    // Refresh wallet balance after spending
                    spawn_balance_refresh(ctx.tx.clone());
                }
                // Update the Review workflow step with findings
                app.set_review_findings(findings, summary);
            }
            BackgroundMessage::VerificationFixComplete {
                new_content,
                description,
                usage,
            } => {
                // Track session cost for display
                if let Some(u) = usage {
                    let cost = u.cost();
                    app.session_cost += cost;
                    app.session_tokens += u.total_tokens;
                    // Refresh wallet balance after spending
                    spawn_balance_refresh(ctx.tx.clone());
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
                    app.review_state.reviewing = true;
                    app.loading = LoadingState::ReviewingChanges;

                    let tx_verify = ctx.tx.clone();
                    spawn_background(ctx.tx.clone(), "re_verification", async move {
                        let files_with_content = vec![(fp, original_content, new_content)];
                        // For re-reviews, we pass None for fix_context since we're now
                        // verifying the fix to the reviewer's findings, not the original fix
                        match suggest::llm::verify_changes(
                            &files_with_content,
                            iteration,
                            &fixed_titles,
                            None,
                        )
                        .await
                        {
                            Ok(review) => {
                                let _ = tx_verify.send(BackgroundMessage::VerificationComplete {
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
            BackgroundMessage::UpdateAvailable { latest_version } => {
                // Store the available version - don't show overlay automatically
                // Users can press U to see the update panel when ready
                app.update_available = Some(latest_version);
            }
            BackgroundMessage::UpdateProgress { percent } => {
                app.update_progress = Some(percent);
                app.set_update_progress(percent);
            }
            BackgroundMessage::UpdateError(e) => {
                app.update_progress = None;
                app.set_update_error(e);
            }
            BackgroundMessage::WalletBalanceUpdated { balance } => {
                app.wallet_balance = Some(balance);
            }
        }
    }
}

/// Spawn a background task to fetch the wallet balance
pub fn spawn_balance_refresh(tx: mpsc::Sender<BackgroundMessage>) {
    spawn_background(tx.clone(), "balance_fetch", async move {
        if let Ok(balance) = suggest::llm::fetch_account_balance().await {
            let _ = tx.send(BackgroundMessage::WalletBalanceUpdated { balance });
        }
        // Silently ignore errors - balance display is optional
    });
}

pub fn spawn_background<F>(tx: mpsc::Sender<BackgroundMessage>, task_name: &'static str, fut: F)
where
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
