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
use crate::ui;
use crate::ui::{App, LoadingState, WorkflowStep};
use chrono::Utc;
use cosmos_adapters::cache;
use cosmos_adapters::util::truncate;
use futures::FutureExt;
use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::path::PathBuf;
use std::sync::mpsc;

fn is_api_key_error(message: &str) -> bool {
    let lowered = message.to_ascii_lowercase();
    lowered.contains("no api key configured")
        || lowered.contains("invalid api key")
        || (lowered.contains("openrouter") && lowered.contains("api key"))
        || lowered.contains("run 'cosmos --setup'")
}

fn maybe_prompt_api_key_overlay(app: &mut App, message: &str) -> bool {
    if !is_api_key_error(message) {
        return false;
    }
    let detail = if message.to_ascii_lowercase().contains("invalid api key") {
        "OpenRouter rejected this API key. Paste a valid key to continue."
    } else {
        "Cosmos needs an OpenRouter API key to run AI actions."
    };
    app.open_api_key_overlay(Some(detail.to_string()));
    true
}

fn spawn_suggestions_generation(
    tx: mpsc::Sender<BackgroundMessage>,
    repo_root: PathBuf,
    index: cosmos_core::index::CodebaseIndex,
    context: cosmos_core::context::WorkContext,
    suggestions_profile: cosmos_adapters::config::SuggestionsProfile,
    repo_memory_context: String,
    summaries_for_suggestions: std::collections::HashMap<PathBuf, String>,
) {
    let tx_suggestions = tx.clone();
    spawn_background(tx.clone(), "suggestions_generation", async move {
        let stage_start = std::time::Instant::now();
        let mem = if repo_memory_context.trim().is_empty() {
            None
        } else {
            Some(repo_memory_context)
        };
        let gate_config =
            cosmos_engine::llm::suggestion_gate_config_for_profile(suggestions_profile);
        let run = cosmos_engine::llm::run_fast_grounded_with_gate_with_progress(
            &repo_root,
            &index,
            &context,
            mem,
            Some(&summaries_for_suggestions),
            gate_config,
            |attempt_index, attempt_count, gate, diagnostics| {
                let _ = tx_suggestions.send(BackgroundMessage::SuggestionsRefinementProgress {
                    attempt_index,
                    attempt_count,
                    gate: gate.clone(),
                    diagnostics: diagnostics.clone(),
                });
            },
        )
        .await;

        match run {
            Ok(result) => {
                let _ = tx_suggestions.send(BackgroundMessage::SuggestionsRefined {
                    suggestions: result.suggestions,
                    usage: result.usage,
                    diagnostics: result.diagnostics,
                    duration_ms: stage_start.elapsed().as_millis() as u64,
                });
            }
            Err(e) => {
                let _ = tx_suggestions.send(BackgroundMessage::SuggestionsError(e.to_string()));
            }
        }
    });
}

pub fn request_suggestions_refresh(
    app: &mut App,
    tx: mpsc::Sender<BackgroundMessage>,
    repo_root: PathBuf,
    _reason: &str,
) -> bool {
    if !cosmos_engine::llm::is_available() {
        return false;
    }

    if app.loading == LoadingState::GeneratingSuggestions {
        return false;
    }

    let fresh_index = match cosmos_core::index::CodebaseIndex::new(&repo_root) {
        Ok(index) => index,
        Err(err) => {
            app.open_alert(
                "Refresh failed",
                format!(
                    "Couldn't refresh suggestions: {}",
                    truncate(&err.to_string(), 120)
                ),
            );
            return false;
        }
    };
    app.replace_index(fresh_index);

    if app.needs_summary_generation {
        if app.summary_progress.is_some() {
            app.pending_suggestions_on_init = true;
            return false;
        }
        if !app.summary_failed_files.is_empty() {
            return false;
        }
    }

    app.loading = LoadingState::GeneratingSuggestions;
    app.suggestion_refinement_in_progress = true;
    app.suggestion_provisional_count = 0;
    app.suggestion_validated_count = 0;
    app.suggestion_rejected_count = 0;
    app.clear_apply_confirm();

    let index = app.index.clone();
    let context = app.context.clone();
    let repo_memory_context = app.repo_memory.to_prompt_context(12, 900);
    let summaries_for_suggestions = app.llm_summaries.clone();
    spawn_suggestions_generation(
        tx,
        repo_root,
        index,
        context,
        app.suggestions_profile,
        repo_memory_context,
        summaries_for_suggestions,
    );
    true
}

pub fn drain_messages(
    app: &mut App,
    rx: &mpsc::Receiver<BackgroundMessage>,
    ctx: &RuntimeContext,
) -> bool {
    let mut changed = false;
    while let Ok(msg) = rx.try_recv() {
        changed = true;
        match msg {
            BackgroundMessage::SuggestionsReady {
                suggestions,
                usage,
                model,
                diagnostics,
                duration_ms,
            } => {
                let run_id = diagnostics.run_id.clone();
                let count = suggestions.len();

                let (tokens, cost) = track_usage(app, usage.as_ref(), ctx);

                // If summaries are still generating, switch to that loading state
                // Otherwise, clear loading
                if app.needs_summary_generation && app.summary_progress.is_some() {
                    app.loading = LoadingState::GeneratingSummaries;
                } else {
                    app.loading = LoadingState::None;
                }

                // Provisional stage only: don't surface actionable suggestions yet.
                app.active_model = Some(model);
                app.suggestion_refinement_in_progress = true;
                app.suggestion_provisional_count = count;
                app.suggestion_validated_count = 0;
                app.suggestion_rejected_count = 0;
                app.clear_apply_confirm();
                app.current_suggestion_run_id = Some(run_id);
                record_pipeline_metric(
                    app,
                    "suggest",
                    duration_ms,
                    tokens,
                    cost,
                    "suggestions",
                    true,
                );
            }
            BackgroundMessage::SuggestionsRefinementProgress {
                attempt_index: _,
                attempt_count: _,
                gate,
                diagnostics: _diagnostics,
            } => {
                app.loading = LoadingState::GeneratingSuggestions;
                app.suggestion_refinement_in_progress = true;
                app.suggestion_provisional_count = 0;
                app.suggestion_validated_count =
                    gate.final_count.saturating_sub(gate.pending_count);
                app.suggestion_rejected_count = 0;
            }
            BackgroundMessage::SuggestionsRefined {
                suggestions,
                usage,
                diagnostics,
                duration_ms,
            } => {
                let run_id = diagnostics.run_id.clone();
                let validated_count = suggestions
                    .iter()
                    .filter(|s| {
                        s.validation_state
                            == cosmos_core::suggest::SuggestionValidationState::Validated
                    })
                    .count();
                let contradiction_counts = cache::Cache::new(&app.repo_path)
                    .recent_contradicted_evidence_counts(300)
                    .unwrap_or_default();
                app.suggestions.replace_llm_suggestions(suggestions);
                app.suggestions
                    .sort_with_context(&app.context, Some(&contradiction_counts));

                let (tokens, cost) = track_usage(app, usage.as_ref(), ctx);
                record_pipeline_metric(
                    app,
                    "suggest_refine",
                    duration_ms,
                    tokens,
                    cost,
                    "suggestions_refined",
                    true,
                );

                if app.needs_summary_generation && app.summary_progress.is_some() {
                    app.loading = LoadingState::GeneratingSummaries;
                } else {
                    app.loading = LoadingState::None;
                }
                app.suggestion_refinement_in_progress = false;
                app.suggestion_provisional_count = diagnostics.provisional_count;
                app.suggestion_validated_count = validated_count;
                app.suggestion_rejected_count = diagnostics.rejected_count;
                app.clear_apply_confirm();
                app.current_suggestion_run_id = Some(run_id);
            }
            BackgroundMessage::SuggestionsError(e) => {
                // If summaries are still generating, switch to that loading state
                if app.needs_summary_generation && app.summary_progress.is_some() {
                    app.loading = LoadingState::GeneratingSummaries;
                } else {
                    app.loading = LoadingState::None;
                }
                if !maybe_prompt_api_key_overlay(app, &e) {
                    app.open_alert(
                        "Suggestions failed",
                        format!("Couldn't generate suggestions: {}", truncate(&e, 120)),
                    );
                }
                app.suggestion_refinement_in_progress = false;
                app.clear_apply_confirm();
            }
            BackgroundMessage::SummariesReady {
                summaries,
                usage,
                failed_files,
                duration_ms,
            } => {
                app.update_summaries(summaries);
                let failed_count = failed_files.len();
                app.summary_failed_files = failed_files;
                let (tokens, cost) = track_usage(app, usage.as_ref(), ctx);
                record_pipeline_metric(
                    app,
                    "summary",
                    duration_ms,
                    tokens,
                    cost,
                    "summaries_complete",
                    failed_count == 0,
                );

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
                    let ai_enabled = cosmos_engine::llm::is_available();

                    // Strict summary gate: do not generate suggestions until summaries are complete.
                    if failed_count > 0 {
                        app.loading = LoadingState::None;
                    } else if ai_enabled {
                        let _ = request_suggestions_refresh(
                            app,
                            ctx.tx.clone(),
                            ctx.repo_path.clone(),
                            "Summaries ready",
                        );
                    } else {
                        app.loading = LoadingState::None;
                    }
                } else {
                    // Not waiting for suggestions, just finish up
                    if !matches!(app.loading, LoadingState::GeneratingSuggestions) {
                        app.loading = LoadingState::None;
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

                let _ = track_usage(app, usage.as_ref(), ctx);

                if updated_files > 0 {
                    app.active_model = Some(model);
                }
            }
            BackgroundMessage::GroupingEnhanceError(_e) => {}
            BackgroundMessage::PreviewReady {
                preview,
                usage,
                file_hashes,
                duration_ms,
            } => {
                app.loading = LoadingState::None;
                let (tokens, cost) = track_usage(app, usage.as_ref(), ctx);
                let gate = match preview.verification_state {
                    cosmos_core::suggest::VerificationState::Verified => "verified",
                    cosmos_core::suggest::VerificationState::Contradicted => "contradicted",
                    cosmos_core::suggest::VerificationState::InsufficientEvidence => {
                        "insufficient_evidence"
                    }
                    cosmos_core::suggest::VerificationState::Unverified => "unverified",
                };
                record_pipeline_metric(
                    app,
                    "verify",
                    duration_ms,
                    tokens,
                    cost,
                    gate,
                    preview.verification_state == cosmos_core::suggest::VerificationState::Verified,
                );
                if let (Some(run_id), Some(suggestion_id)) = (
                    app.current_suggestion_run_id.clone(),
                    app.verify_state.suggestion_id,
                ) {
                    let evidence_ids = app
                        .suggestions
                        .suggestions
                        .iter()
                        .find(|s| s.id == suggestion_id)
                        .map(|s| {
                            s.evidence_refs
                                .iter()
                                .map(|r| r.snippet_id)
                                .collect::<Vec<_>>()
                        })
                        .unwrap_or_default();
                    let quality = cache::SuggestionQualityRecord {
                        timestamp: Utc::now(),
                        run_id,
                        suggestion_id: suggestion_id.to_string(),
                        evidence_ids,
                        validation_outcome: "verify_result".to_string(),
                        validation_reason: None,
                        user_verify_outcome: Some(gate.to_string()),
                        batch_missing_index_count: 0,
                        batch_no_reason_count: 0,
                        transport_retry_count: 0,
                        transport_recovered_count: 0,
                        rewrite_recovered_count: 0,
                        prevalidation_contradiction_count: 0,
                    };
                    let cache = cache::Cache::new(&app.repo_path);
                    let _ = cache.append_suggestion_quality(&quality);
                }
                let cache = cache::Cache::new(&app.repo_path);
                app.rolling_verify_precision = cache.rolling_verify_precision(50);
                // Set the preview in the Verify workflow step
                app.set_verify_preview(preview, file_hashes);
            }
            BackgroundMessage::PreviewError(e) => {
                app.loading = LoadingState::None;
                app.workflow_step = WorkflowStep::Suggestions;
                app.verify_state = ui::VerifyState::default();
                if !maybe_prompt_api_key_overlay(app, &e) {
                    app.open_alert(
                        "Preview failed",
                        format!("Couldn't prepare a safe preview: {}", truncate(&e, 120)),
                    );
                }
            }
            BackgroundMessage::ApplyHarnessProgress {
                attempt_index: _,
                attempt_count: _,
                detail: _,
            } => {
                app.loading = LoadingState::GeneratingFix;
            }
            BackgroundMessage::ApplyHarnessFailed {
                summary,
                fail_reasons,
                report_path,
            } => {
                app.loading = LoadingState::None;
                app.workflow_step = WorkflowStep::Suggestions;
                app.verify_state = ui::VerifyState::default();
                app.clear_apply_confirm();
                let mut detail = summary;
                if !fail_reasons.is_empty() {
                    let joined = fail_reasons
                        .iter()
                        .take(2)
                        .cloned()
                        .collect::<Vec<_>>()
                        .join("; ");
                    if !joined.is_empty() {
                        detail = format!("{} ({})", detail, joined);
                    }
                }
                if let Some(path) = report_path {
                    detail = format!("{}. See report at {}", detail, path.display());
                }
                app.open_alert(
                    "Apply failed",
                    format!(
                        "Couldn't apply this change safely: {}",
                        truncate(&detail, 140)
                    ),
                );
            }
            BackgroundMessage::ApplyHarnessReducedConfidence {
                detail: _,
                report_path: _,
            } => {}
            BackgroundMessage::DirectFixApplied {
                suggestion_id,
                file_changes,
                description,
                usage,
                branch_name,
                source_branch,
                friendly_title,
                problem_summary,
                outcome,
                duration_ms,
            } => {
                let (tokens, cost) = track_usage(app, usage.as_ref(), ctx);
                record_pipeline_metric(app, "apply", duration_ms, tokens, cost, "apply_fix", true);

                app.loading = LoadingState::None;
                app.suggestions.mark_applied(suggestion_id);

                // Store the cosmos branch name - this enables the Ship workflow
                app.cosmos_branch = Some(branch_name.clone());
                app.cosmos_base_branch = Some(source_branch);

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
                        let original =
                            cosmos_adapters::git_ops::read_file_from_head(&app.repo_path, path)
                                .unwrap_or(None)
                                .unwrap_or_default();
                        let full_path = app.repo_path.join(path);
                        let new_content = std::fs::read_to_string(&full_path).unwrap_or_default();
                        (path.clone(), original, new_content)
                    })
                    .collect();

                // Transition to Review workflow step (multi-file aware)
                let review_files = files_with_content
                    .iter()
                    .map(|(path, original, new_content)| ui::ReviewFileContent {
                        path: path.clone(),
                        original_content: original.clone(),
                        new_content: new_content.clone(),
                    })
                    .collect();
                app.clear_apply_confirm();
                app.start_review(review_files);

                // Trigger verification in background (all files)
                {
                    let tx_verify = ctx.tx.clone();

                    // Build fix context so the reviewer knows what the fix was supposed to do
                    let fix_context = cosmos_engine::llm::FixContext {
                        problem_summary: problem_summary.clone(),
                        outcome: outcome.clone(),
                        description: description.clone(),
                        modified_areas: Vec::new(), // Could be extracted from fix response if needed
                    };

                    spawn_background(ctx.tx.clone(), "verification", async move {
                        let review_start = std::time::Instant::now();
                        match cosmos_engine::llm::verify_changes(
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
                                    duration_ms: review_start.elapsed().as_millis() as u64,
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
                app.workflow_step = WorkflowStep::Suggestions;
                app.verify_state = ui::VerifyState::default();
                app.clear_apply_confirm();
                if !maybe_prompt_api_key_overlay(app, &e) {
                    app.open_alert(
                        "Apply failed",
                        format!("Couldn't apply this change safely: {}", truncate(&e, 120)),
                    );
                }
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
                } else {
                    app.ship_step = Some(ui::ShipStep::Done);
                    app.pr_url = Some(url.clone());
                    app.clear_pending_changes();
                }
            }
            BackgroundMessage::ShipError(e) => {
                app.ship_step = None;
                app.close_overlay();
                app.open_alert(
                    "Ship failed",
                    format!(
                        "Couldn't complete the shipping steps: {}",
                        truncate(&e, 120)
                    ),
                );
            }
            BackgroundMessage::ResetComplete { options } => {
                app.loading = LoadingState::None;
                if options.contains(&cosmos_adapters::cache::ResetOption::QuestionCache) {
                    app.question_cache = cosmos_adapters::cache::QuestionCache::default();
                }
            }
            BackgroundMessage::StashComplete { message: _ } => {
                app.loading = LoadingState::None;
            }
            BackgroundMessage::DiscardComplete => {
                app.loading = LoadingState::None;
            }
            BackgroundMessage::StartupSwitchedToMain { branch: _ } => {
                app.loading = LoadingState::None;
            }
            BackgroundMessage::Error(e) => {
                if e.contains("ask_question") {
                    if let Some(request_id) = app.active_ask_request_id {
                        let _ = app.complete_ask_request(request_id);
                    }
                }
                app.loading = LoadingState::None;
                // Reset review fixing state if we were applying review fixes
                if app.review_state.fixing {
                    app.review_state.fixing = false;
                }

                if maybe_prompt_api_key_overlay(app, &e) {
                    // Key prompt handles the user-visible guidance.
                } else if e.contains("verification failed") || e.contains("Re-verification failed")
                {
                    // Verification failures are explicit and require manual override in Review.
                    app.review_state.reviewing = false;
                    app.review_state.verification_failed = true;
                    app.review_state.verification_error = Some(truncate(&e, 200).to_string());
                    app.review_state.confirm_ship = false;
                    // Set a summary indicating verification did not complete.
                    if app.review_state.summary.is_empty() {
                        app.review_state.summary =
                            "Verification failed before completion. Review manually before shipping."
                                .to_string();
                    }
                } else {
                    app.open_alert(
                        "Operation failed",
                        format!("Cosmos hit an error: {}", truncate(&e, 120)),
                    );
                }
            }
            BackgroundMessage::QuestionError { request_id, error } => {
                let is_active = app.complete_ask_request(request_id);
                if !is_active {
                    continue;
                }
                if !maybe_prompt_api_key_overlay(app, &error) {
                    app.show_inquiry(format!(
                        "Couldn't answer that right now.\n\n{}",
                        truncate(&error, 180)
                    ));
                }
            }
            BackgroundMessage::QuestionResponseWithCache {
                request_id,
                question,
                answer,
                usage,
                context_hash,
            } => {
                let _ = track_usage_for_ask(app, usage.as_ref(), ctx);

                // Store answer in cache
                app.question_cache
                    .set(question, answer.clone(), context_hash);
                // Save cache to disk
                let cache = cache::Cache::new(&app.repo_path);
                let _ = cache.save_question_cache(&app.question_cache);

                if !app.complete_ask_request(request_id) {
                    continue;
                }
                // Show the response in the ask cosmos panel
                app.show_inquiry(answer);
            }
            BackgroundMessage::QuestionResponse {
                request_id,
                answer,
                usage,
            } => {
                let _ = track_usage_for_ask(app, usage.as_ref(), ctx);
                if !app.complete_ask_request(request_id) {
                    continue;
                }
                // Show the response in the ask cosmos panel
                app.show_inquiry(answer);
            }
            BackgroundMessage::VerificationComplete {
                findings,
                summary,
                usage,
                duration_ms,
            } => {
                let (tokens, cost) = track_usage(app, usage.as_ref(), ctx);
                record_pipeline_metric(
                    app,
                    "review",
                    duration_ms,
                    tokens,
                    cost,
                    "review_complete",
                    true,
                );
                // Update the Review workflow step with findings
                app.set_review_findings(findings, summary);
            }
            BackgroundMessage::VerificationFixComplete {
                file_changes,
                description,
                usage,
                duration_ms,
            } => {
                let (tokens, cost) = track_usage(app, usage.as_ref(), ctx);
                record_pipeline_metric(
                    app,
                    "review",
                    duration_ms,
                    tokens,
                    cost,
                    "review_fix_applied",
                    true,
                );

                let _ = description;

                // Apply file updates to disk and stage them.
                let mut apply_failed = false;
                for (path, new_content) in &file_changes {
                    let full_path = app.repo_path.join(path);
                    if let Some(parent) = full_path.parent() {
                        if let Err(e) = std::fs::create_dir_all(parent) {
                            app.review_state.fixing = false;
                            app.loading = LoadingState::None;
                            app.open_alert(
                                "Review fix failed",
                                format!("Couldn't create {} ({})", parent.display(), e),
                            );
                            apply_failed = true;
                            break;
                        }
                    }

                    if let Err(e) = std::fs::write(&full_path, new_content) {
                        app.review_state.fixing = false;
                        app.loading = LoadingState::None;
                        app.open_alert(
                            "Review fix failed",
                            format!("Couldn't write {} ({})", path.display(), e),
                        );
                        apply_failed = true;
                        break;
                    }

                    let rel_path = path.to_string_lossy().to_string();
                    if let Err(e) = cosmos_adapters::git_ops::stage_file(&app.repo_path, &rel_path)
                    {
                        app.review_state.fixing = false;
                        app.loading = LoadingState::None;
                        app.open_alert(
                            "Review fix failed",
                            format!("Couldn't stage {} ({})", path.display(), e),
                        );
                        apply_failed = true;
                        break;
                    }
                }
                if apply_failed {
                    continue;
                }

                let mut updated_files = app.review_state.files.clone();
                for (path, new_content) in &file_changes {
                    if let Some(file) = updated_files.iter_mut().find(|f| f.path == *path) {
                        file.new_content = new_content.clone();
                    }
                }

                let iteration = app.review_state.review_iteration + 1;
                let fixed_titles = app.review_state.fixed_titles.clone();
                app.review_fix_complete(file_changes.clone());

                // Trigger re-review
                // Note: On re-reviews, we don't pass suggestion context because we're
                // verifying fixes to the reviewer's findings, not the original suggestion
                let files_with_content: Vec<(PathBuf, String, String)> = updated_files
                    .iter()
                    .map(|f| {
                        (
                            f.path.clone(),
                            f.original_content.clone(),
                            f.new_content.clone(),
                        )
                    })
                    .collect();

                app.review_state.reviewing = true;
                app.loading = LoadingState::ReviewingChanges;

                let tx_verify = ctx.tx.clone();
                spawn_background(ctx.tx.clone(), "re_verification", async move {
                    let review_start = std::time::Instant::now();
                    // For re-reviews, we pass None for fix_context since we're now
                    // verifying the fix to the reviewer's findings, not the original fix
                    match cosmos_engine::llm::verify_changes(
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
                                duration_ms: review_start.elapsed().as_millis() as u64,
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
    if changed {
        app.needs_redraw = true;
    }
    changed
}

fn track_usage(
    app: &mut App,
    usage: Option<&cosmos_engine::llm::Usage>,
    ctx: &RuntimeContext,
) -> (u32, f64) {
    track_usage_internal(app, usage, ctx, true)
}

fn track_usage_for_ask(
    app: &mut App,
    usage: Option<&cosmos_engine::llm::Usage>,
    ctx: &RuntimeContext,
) -> (u32, f64) {
    track_usage_internal(app, usage, ctx, false)
}

fn track_usage_internal(
    app: &mut App,
    usage: Option<&cosmos_engine::llm::Usage>,
    ctx: &RuntimeContext,
    show_budget_guardrails: bool,
) -> (u32, f64) {
    let Some(usage) = usage else {
        return (0, 0.0);
    };

    let cost = usage.cost();
    app.session_cost += cost;
    app.session_tokens += usage.total_tokens;
    spawn_balance_refresh(ctx.tx.clone());
    if show_budget_guardrails {
        maybe_show_budget_guardrails(app);
    }

    (usage.total_tokens, cost)
}

fn maybe_show_budget_guardrails(app: &mut App) {
    if app.session_cost >= 0.04 && !app.budget_warned_soft {
        app.budget_warned_soft = true;
    }
    if app.session_cost >= 0.05 && !app.budget_warned_hard {
        app.budget_warned_hard = true;
    }
}

fn record_pipeline_metric(
    app: &App,
    stage: &str,
    duration_ms: u64,
    tokens: u32,
    cost: f64,
    gate: &str,
    passed: bool,
) {
    let cache = cache::Cache::new(&app.repo_path);
    let mut metric = cache::PipelineMetricRecord {
        timestamp: Utc::now(),
        stage: stage.to_string(),
        summary_ms: None,
        suggest_ms: None,
        verify_ms: None,
        apply_ms: None,
        review_ms: None,
        tokens,
        cost,
        gate: gate.to_string(),
        passed,
    };

    match stage {
        "summary" => metric.summary_ms = Some(duration_ms),
        "suggest" => metric.suggest_ms = Some(duration_ms),
        "suggest_refine" => metric.suggest_ms = Some(duration_ms),
        "verify" => metric.verify_ms = Some(duration_ms),
        "apply" => metric.apply_ms = Some(duration_ms),
        "review" => metric.review_ms = Some(duration_ms),
        _ => {}
    }

    let _ = cache.append_pipeline_metric(&metric);
}

/// Spawn a background task to fetch the wallet balance
pub fn spawn_balance_refresh(tx: mpsc::Sender<BackgroundMessage>) {
    spawn_background(tx.clone(), "balance_fetch", async move {
        if let Ok(balance) = cosmos_engine::llm::fetch_account_balance().await {
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
        root.push(format!("cosmos_background_test_{}", nanos));
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
    fn stale_question_response_does_not_overwrite_active_request() {
        let mut app = make_test_app();
        let stale_id = app.begin_ask_request();
        let active_id = app.begin_ask_request();

        let (tx, rx) = mpsc::channel();
        tx.send(BackgroundMessage::QuestionResponse {
            request_id: stale_id,
            answer: "stale".to_string(),
            usage: None,
        })
        .unwrap();
        tx.send(BackgroundMessage::QuestionResponse {
            request_id: active_id,
            answer: "active".to_string(),
            usage: None,
        })
        .unwrap();

        let index = app.index.clone();
        let repo_path = app.repo_path.clone();
        let ctx = RuntimeContext {
            index: &index,
            repo_path: &repo_path,
            tx: &tx,
        };
        drain_messages(&mut app, &rx, &ctx);

        assert!(!app.ask_in_flight);
        assert_eq!(
            app.ask_cosmos_state.as_ref().map(|s| s.response.as_str()),
            Some("active")
        );
    }

    #[test]
    fn suggestions_messages_do_not_clear_ask_request_state() {
        let mut app = make_test_app();
        let request_id = app.begin_ask_request();

        let (tx, rx) = mpsc::channel();
        tx.send(BackgroundMessage::SuggestionsError(
            "transient suggest failure".to_string(),
        ))
        .unwrap();

        let index = app.index.clone();
        let repo_path = app.repo_path.clone();
        let ctx = RuntimeContext {
            index: &index,
            repo_path: &repo_path,
            tx: &tx,
        };
        drain_messages(&mut app, &rx, &ctx);

        assert!(app.ask_in_flight);
        assert_eq!(app.active_ask_request_id, Some(request_id));
        assert!(app.ask_cosmos_state.is_none());
    }
}
