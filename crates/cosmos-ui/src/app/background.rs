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
) {
    let tx_suggestions = tx.clone();
    spawn_background(tx.clone(), "suggestions_generation", async move {
        let stage_start = std::time::Instant::now();
        let mem = if repo_memory_context.trim().is_empty() {
            None
        } else {
            Some(repo_memory_context)
        };
        let generation_target =
            cosmos_engine::llm::suggestion_gate_config_for_profile(suggestions_profile)
                .max_final_count
                .max(1);
        let run = cosmos_engine::llm::analyze_codebase_fast_grounded(
            &repo_root,
            &index,
            &context,
            mem,
            cosmos_engine::llm::models::Model::Smart,
            generation_target,
            None,
        )
        .await;

        match run {
            Ok((suggestions, usage, mut diagnostics)) => {
                diagnostics.refinement_complete = true;
                diagnostics.provisional_count = suggestions.len();
                diagnostics.validated_count = suggestions.len();
                diagnostics.rejected_count = 0;
                diagnostics.final_count = suggestions.len();
                let model = diagnostics.model.clone();

                let _ = tx_suggestions.send(BackgroundMessage::SuggestionsReady {
                    suggestions,
                    usage,
                    model,
                    diagnostics,
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

    app.loading = LoadingState::GeneratingSuggestions;
    app.clear_apply_confirm();

    let index = app.index.clone();
    let context = app.context.clone();
    let repo_memory_context = app.repo_memory.to_prompt_context(12, 900);
    spawn_suggestions_generation(
        tx,
        repo_root,
        index,
        context,
        app.suggestions_profile,
        repo_memory_context,
    );
    true
}

fn restore_loading_after_suggestion_stage(app: &mut App) {
    app.loading = LoadingState::None;
}

fn handle_suggestions_ready_message(
    app: &mut App,
    suggestions: Vec<cosmos_core::suggest::Suggestion>,
    usage: Option<cosmos_engine::llm::Usage>,
    model: String,
    diagnostics: cosmos_engine::llm::SuggestionDiagnostics,
    duration_ms: u64,
    ctx: &RuntimeContext,
) {
    let run_id = diagnostics.run_id.clone();
    let validated_count = suggestions
        .iter()
        .filter(|s| {
            s.validation_state == cosmos_core::suggest::SuggestionValidationState::Validated
        })
        .count();
    let cache = cache::Cache::new(&app.repo_path);
    let run_audit = cache::SuggestionRunAuditRecord {
        timestamp: Utc::now(),
        run_id: run_id.clone(),
        suggestion_count: suggestions.len(),
        validated_count,
        rejected_count: diagnostics.rejected_count,
        suggestions: suggestions.clone(),
    };
    let _ = cache.append_suggestion_run_audit(&run_audit);
    let contradiction_counts = cache
        .recent_contradicted_evidence_counts(300)
        .unwrap_or_default();
    app.suggestions.replace_llm_suggestions(suggestions);
    app.suggestions
        .sort_with_context(&app.context, Some(&contradiction_counts));

    let (tokens, cost) = track_usage(app, usage.as_ref(), ctx);
    record_pipeline_metric(
        app,
        "suggest",
        duration_ms,
        tokens,
        cost,
        "suggestions",
        true,
    );

    restore_loading_after_suggestion_stage(app);
    app.active_model = Some(model);
    app.clear_apply_confirm();
    app.current_suggestion_run_id = Some(run_id);
}

fn build_files_with_content_for_review(
    repo_path: &std::path::Path,
    file_changes: &[(PathBuf, String)],
) -> Vec<(PathBuf, String, String)> {
    file_changes
        .iter()
        .map(|(path, _diff)| {
            let original = cosmos_adapters::git_ops::read_file_from_head(repo_path, path)
                .unwrap_or(None)
                .unwrap_or_default();
            let full_path = repo_path.join(path);
            let new_content = std::fs::read_to_string(&full_path).unwrap_or_default();
            (path.clone(), original, new_content)
        })
        .collect()
}

fn spawn_verification_after_direct_fix(
    tx: mpsc::Sender<BackgroundMessage>,
    files_with_content: Vec<(PathBuf, String, String)>,
    problem_summary: String,
    outcome: String,
    description: String,
) {
    let fix_context = cosmos_engine::llm::FixContext {
        problem_summary,
        outcome,
        description,
        modified_areas: Vec::new(),
    };

    spawn_background(tx.clone(), "verification", async move {
        let review_start = std::time::Instant::now();
        match cosmos_engine::llm::verify_changes(&files_with_content, 1, &[], Some(&fix_context))
            .await
        {
            Ok(review) => {
                let _ = tx.send(BackgroundMessage::VerificationComplete {
                    findings: review.findings,
                    summary: review.summary,
                    usage: review.usage,
                    duration_ms: review_start.elapsed().as_millis() as u64,
                });
            }
            Err(e) => {
                let _ = tx.send(BackgroundMessage::Error(format!(
                    "Verification failed: {}",
                    e
                )));
            }
        }
    });
}

#[allow(clippy::too_many_arguments)]
fn handle_direct_fix_applied_message(
    app: &mut App,
    suggestion_id: uuid::Uuid,
    file_changes: Vec<(PathBuf, String)>,
    description: String,
    usage: Option<cosmos_engine::llm::Usage>,
    branch_name: String,
    source_branch: String,
    friendly_title: String,
    problem_summary: String,
    outcome: String,
    duration_ms: u64,
    ctx: &RuntimeContext,
) {
    let (tokens, cost) = track_usage(app, usage.as_ref(), ctx);
    record_pipeline_metric(app, "apply", duration_ms, tokens, cost, "apply_fix", true);

    app.loading = LoadingState::None;
    app.suggestions.mark_applied(suggestion_id);
    app.cosmos_branch = Some(branch_name);
    app.cosmos_base_branch = Some(source_branch);

    let ui_file_changes: Vec<ui::FileChange> = file_changes
        .iter()
        .map(|(path, diff)| ui::FileChange::new(path.clone(), diff.clone()))
        .collect();
    app.pending_changes
        .push(ui::PendingChange::with_preview_context_multi(
            suggestion_id,
            ui_file_changes,
            description.clone(),
            friendly_title,
            problem_summary.clone(),
            outcome.clone(),
        ));

    let files_with_content = build_files_with_content_for_review(&app.repo_path, &file_changes);
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

    spawn_verification_after_direct_fix(
        ctx.tx.clone(),
        files_with_content,
        problem_summary,
        outcome,
        description,
    );
}

fn apply_review_fix_file_changes(
    app: &mut App,
    file_changes: &[(PathBuf, String)],
) -> std::result::Result<Vec<ui::ReviewFileContent>, String> {
    for (path, new_content) in file_changes {
        let full_path = app.repo_path.join(path);
        if let Some(parent) = full_path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("Couldn't create {} ({})", parent.display(), e))?;
        }
        std::fs::write(&full_path, new_content)
            .map_err(|e| format!("Couldn't write {} ({})", path.display(), e))?;
        let rel_path = path.to_string_lossy().to_string();
        cosmos_adapters::git_ops::stage_file(&app.repo_path, &rel_path)
            .map_err(|e| format!("Couldn't stage {} ({})", path.display(), e))?;
    }

    let mut updated_files = app.review_state.files.clone();
    for (path, new_content) in file_changes {
        if let Some(file) = updated_files.iter_mut().find(|f| f.path == *path) {
            file.new_content = new_content.clone();
        }
    }
    Ok(updated_files)
}

fn spawn_reverification(
    tx: mpsc::Sender<BackgroundMessage>,
    files_with_content: Vec<(PathBuf, String, String)>,
    iteration: u32,
    fixed_titles: Vec<String>,
) {
    spawn_background(tx.clone(), "re_verification", async move {
        let review_start = std::time::Instant::now();
        match cosmos_engine::llm::verify_changes(
            &files_with_content,
            iteration,
            &fixed_titles,
            None,
        )
        .await
        {
            Ok(review) => {
                let _ = tx.send(BackgroundMessage::VerificationComplete {
                    findings: review.findings,
                    summary: review.summary,
                    usage: review.usage,
                    duration_ms: review_start.elapsed().as_millis() as u64,
                });
            }
            Err(e) => {
                let _ = tx.send(BackgroundMessage::Error(format!(
                    "Re-verification failed: {}",
                    e
                )));
            }
        }
    });
}

fn handle_verification_fix_complete_message(
    app: &mut App,
    file_changes: Vec<(PathBuf, String)>,
    usage: Option<cosmos_engine::llm::Usage>,
    duration_ms: u64,
    ctx: &RuntimeContext,
) {
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

    let updated_files = match apply_review_fix_file_changes(app, &file_changes) {
        Ok(files) => files,
        Err(error) => {
            app.review_state.fixing = false;
            app.loading = LoadingState::None;
            app.open_alert("Review fix failed", error);
            return;
        }
    };

    let iteration = app.review_state.review_iteration + 1;
    let fixed_titles = app.review_state.fixed_titles.clone();
    app.review_fix_complete(file_changes.clone());

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
    spawn_reverification(ctx.tx.clone(), files_with_content, iteration, fixed_titles);
}

fn handle_background_error_message(app: &mut App, error: String) {
    if error.contains("ask_question") {
        if let Some(request_id) = app.active_ask_request_id {
            let _ = app.complete_ask_request(request_id);
        }
    }
    app.loading = LoadingState::None;
    if app.review_state.fixing {
        app.review_state.fixing = false;
    }

    if maybe_prompt_api_key_overlay(app, &error) {
        return;
    }
    if error.contains("verification failed") || error.contains("Re-verification failed") {
        app.review_state.reviewing = false;
        app.review_state.verification_failed = true;
        app.review_state.verification_error = Some(truncate(&error, 200).to_string());
        app.review_state.confirm_ship = false;
        if app.review_state.summary.is_empty() {
            app.review_state.summary =
                "Verification failed before completion. Review manually before shipping."
                    .to_string();
        }
        return;
    }
    app.open_alert(
        "Operation failed",
        format!("Cosmos hit an error: {}", truncate(&error, 120)),
    );
}

fn handle_suggestions_error_message(app: &mut App, error: String) {
    restore_loading_after_suggestion_stage(app);
    if !maybe_prompt_api_key_overlay(app, &error) {
        app.open_alert(
            "Suggestions failed",
            format!("Couldn't generate suggestions: {}", truncate(&error, 420)),
        );
    }
    app.clear_apply_confirm();
}

fn handle_grouping_enhanced_message(
    app: &mut App,
    grouping: cosmos_core::grouping::CodebaseGrouping,
    updated_files: usize,
    usage: Option<cosmos_engine::llm::Usage>,
    model: String,
    ctx: &RuntimeContext,
) {
    if updated_files > 0 {
        app.apply_grouping_update(grouping);
    }
    let _ = track_usage(app, usage.as_ref(), ctx);
    if updated_files > 0 {
        app.active_model = Some(model);
    }
}

fn handle_preview_ready_message(
    app: &mut App,
    preview: cosmos_engine::llm::FixPreview,
    usage: Option<cosmos_engine::llm::Usage>,
    file_hashes: std::collections::HashMap<PathBuf, String>,
    duration_ms: u64,
    ctx: &RuntimeContext,
) {
    app.loading = LoadingState::None;
    let (tokens, cost) = track_usage(app, usage.as_ref(), ctx);
    let gate = match preview.verification_state {
        cosmos_core::suggest::VerificationState::Verified => "verified",
        cosmos_core::suggest::VerificationState::Contradicted => "contradicted",
        cosmos_core::suggest::VerificationState::InsufficientEvidence => "insufficient_evidence",
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
    app.set_verify_preview(preview, file_hashes);
}

fn handle_preview_error_message(app: &mut App, error: String) {
    app.loading = LoadingState::None;
    app.workflow_step = WorkflowStep::Suggestions;
    app.verify_state = ui::VerifyState::default();
    if !maybe_prompt_api_key_overlay(app, &error) {
        app.open_alert(
            "Preview failed",
            format!("Couldn't prepare a safe preview: {}", truncate(&error, 120)),
        );
    }
}

fn handle_apply_harness_progress_message(app: &mut App) {
    app.loading = LoadingState::GeneratingFix;
}

fn handle_apply_harness_failed_message(
    app: &mut App,
    summary: String,
    fail_reasons: Vec<String>,
    report_path: Option<PathBuf>,
) {
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

fn handle_direct_fix_error_message(app: &mut App, error: String) {
    app.loading = LoadingState::None;
    app.workflow_step = WorkflowStep::Suggestions;
    app.verify_state = ui::VerifyState::default();
    app.clear_apply_confirm();
    if !maybe_prompt_api_key_overlay(app, &error) {
        app.open_alert(
            "Apply failed",
            format!(
                "Couldn't apply this change safely: {}",
                truncate(&error, 120)
            ),
        );
    }
}

fn handle_ship_progress_message(app: &mut App, step: ui::ShipStep) {
    if app.workflow_step == WorkflowStep::Ship {
        app.set_ship_step(step);
    } else {
        app.ship_step = Some(step);
    }
}

fn handle_ship_complete_message(app: &mut App, url: String) {
    if app.workflow_step == WorkflowStep::Ship {
        app.set_ship_pr_url(url);
    } else {
        app.ship_step = Some(ui::ShipStep::Done);
        app.pr_url = Some(url);
        app.clear_pending_changes();
    }
}

fn handle_ship_error_message(app: &mut App, error: String) {
    app.ship_step = None;
    app.close_overlay();
    app.open_alert(
        "Ship failed",
        format!(
            "Couldn't complete the shipping steps: {}",
            truncate(&error, 120)
        ),
    );
}

fn handle_question_error_message(app: &mut App, request_id: u64, error: String) {
    let is_active = app.complete_ask_request(request_id);
    if !is_active {
        return;
    }
    if !maybe_prompt_api_key_overlay(app, &error) {
        app.show_inquiry(format!(
            "Couldn't answer that right now.\n\n{}",
            truncate(&error, 180)
        ));
    }
}

fn handle_question_response_with_cache_message(
    app: &mut App,
    request_id: u64,
    question: String,
    answer: String,
    usage: Option<cosmos_engine::llm::Usage>,
    context_hash: String,
    ctx: &RuntimeContext,
) {
    let _ = track_usage_for_ask(app, usage.as_ref(), ctx);
    app.question_cache
        .set(question, answer.clone(), context_hash);
    let cache = cache::Cache::new(&app.repo_path);
    let _ = cache.save_question_cache(&app.question_cache);

    if !app.complete_ask_request(request_id) {
        return;
    }
    app.show_inquiry(answer);
}

fn handle_question_response_message(
    app: &mut App,
    request_id: u64,
    answer: String,
    usage: Option<cosmos_engine::llm::Usage>,
    ctx: &RuntimeContext,
) {
    let _ = track_usage_for_ask(app, usage.as_ref(), ctx);
    if !app.complete_ask_request(request_id) {
        return;
    }
    app.show_inquiry(answer);
}

fn handle_verification_complete_message(
    app: &mut App,
    findings: Vec<cosmos_engine::llm::ReviewFinding>,
    summary: String,
    usage: Option<cosmos_engine::llm::Usage>,
    duration_ms: u64,
    ctx: &RuntimeContext,
) {
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
    app.set_review_findings(findings, summary);
}

fn handle_update_progress_message(app: &mut App, percent: u8) {
    app.update_progress = Some(percent);
    app.set_update_progress(percent);
}

fn handle_update_error_message(app: &mut App, error: String) {
    app.update_progress = None;
    app.set_update_error(error);
}

fn handle_generation_messages(
    app: &mut App,
    msg: BackgroundMessage,
    ctx: &RuntimeContext,
) -> Option<BackgroundMessage> {
    match msg {
        BackgroundMessage::SuggestionsReady {
            suggestions,
            usage,
            model,
            diagnostics,
            duration_ms,
        } => {
            handle_suggestions_ready_message(
                app,
                suggestions,
                usage,
                model,
                diagnostics,
                duration_ms,
                ctx,
            );
            None
        }
        BackgroundMessage::SuggestionsError(error) => {
            handle_suggestions_error_message(app, error);
            None
        }
        BackgroundMessage::GroupingEnhanced {
            grouping,
            updated_files,
            usage,
            model,
        } => {
            handle_grouping_enhanced_message(app, grouping, updated_files, usage, model, ctx);
            None
        }
        BackgroundMessage::GroupingEnhanceError(_error) => None,
        other => Some(other),
    }
}

fn handle_preview_messages(
    app: &mut App,
    msg: BackgroundMessage,
    ctx: &RuntimeContext,
) -> Option<BackgroundMessage> {
    match msg {
        BackgroundMessage::PreviewReady {
            preview,
            usage,
            file_hashes,
            duration_ms,
        } => {
            handle_preview_ready_message(app, preview, usage, file_hashes, duration_ms, ctx);
            None
        }
        BackgroundMessage::PreviewError(error) => {
            handle_preview_error_message(app, error);
            None
        }
        other => Some(other),
    }
}

fn handle_apply_messages(
    app: &mut App,
    msg: BackgroundMessage,
    ctx: &RuntimeContext,
) -> Option<BackgroundMessage> {
    match msg {
        BackgroundMessage::ApplyHarnessProgress {
            attempt_index: _,
            attempt_count: _,
            detail: _,
        } => {
            handle_apply_harness_progress_message(app);
            None
        }
        BackgroundMessage::ApplyHarnessFailed {
            summary,
            fail_reasons,
            report_path,
        } => {
            handle_apply_harness_failed_message(app, summary, fail_reasons, report_path);
            None
        }
        BackgroundMessage::ApplyHarnessReducedConfidence {
            detail: _,
            report_path: _,
        } => None,
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
            handle_direct_fix_applied_message(
                app,
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
                ctx,
            );
            None
        }
        BackgroundMessage::DirectFixError(error) => {
            handle_direct_fix_error_message(app, error);
            None
        }
        other => Some(other),
    }
}

fn handle_ship_repo_messages(app: &mut App, msg: BackgroundMessage) -> Option<BackgroundMessage> {
    match msg {
        BackgroundMessage::ShipProgress(step) => {
            handle_ship_progress_message(app, step);
            None
        }
        BackgroundMessage::ShipComplete(url) => {
            handle_ship_complete_message(app, url);
            None
        }
        BackgroundMessage::ShipError(error) => {
            handle_ship_error_message(app, error);
            None
        }
        BackgroundMessage::ResetComplete { options } => {
            app.loading = LoadingState::None;
            if options.contains(&cosmos_adapters::cache::ResetOption::QuestionCache) {
                app.question_cache = cosmos_adapters::cache::QuestionCache::default();
            }
            None
        }
        BackgroundMessage::StashComplete { message: _ } => {
            app.loading = LoadingState::None;
            None
        }
        BackgroundMessage::DiscardComplete => {
            app.loading = LoadingState::None;
            None
        }
        BackgroundMessage::StartupSwitchedToMain { branch: _ } => {
            app.loading = LoadingState::None;
            None
        }
        other => Some(other),
    }
}

fn handle_misc_messages(app: &mut App, msg: BackgroundMessage, ctx: &RuntimeContext) {
    match msg {
        BackgroundMessage::Error(error) => {
            handle_background_error_message(app, error);
        }
        BackgroundMessage::QuestionError { request_id, error } => {
            handle_question_error_message(app, request_id, error);
        }
        BackgroundMessage::QuestionResponseWithCache {
            request_id,
            question,
            answer,
            usage,
            context_hash,
        } => {
            handle_question_response_with_cache_message(
                app,
                request_id,
                question,
                answer,
                usage,
                context_hash,
                ctx,
            );
        }
        BackgroundMessage::QuestionResponse {
            request_id,
            answer,
            usage,
        } => {
            handle_question_response_message(app, request_id, answer, usage, ctx);
        }
        BackgroundMessage::VerificationComplete {
            findings,
            summary,
            usage,
            duration_ms,
        } => {
            handle_verification_complete_message(app, findings, summary, usage, duration_ms, ctx);
        }
        BackgroundMessage::VerificationFixComplete {
            file_changes,
            description: _,
            usage,
            duration_ms,
        } => {
            handle_verification_fix_complete_message(app, file_changes, usage, duration_ms, ctx);
        }
        BackgroundMessage::UpdateAvailable { latest_version } => {
            app.update_available = Some(latest_version);
        }
        BackgroundMessage::UpdateProgress { percent } => {
            handle_update_progress_message(app, percent);
        }
        BackgroundMessage::UpdateError(error) => {
            handle_update_error_message(app, error);
        }
        BackgroundMessage::WalletBalanceUpdated { balance } => {
            app.wallet_balance = Some(balance);
        }
        BackgroundMessage::SuggestionsReady { .. }
        | BackgroundMessage::SuggestionsError(_)
        | BackgroundMessage::GroupingEnhanced { .. }
        | BackgroundMessage::GroupingEnhanceError(_)
        | BackgroundMessage::PreviewReady { .. }
        | BackgroundMessage::PreviewError(_)
        | BackgroundMessage::ApplyHarnessProgress { .. }
        | BackgroundMessage::ApplyHarnessFailed { .. }
        | BackgroundMessage::ApplyHarnessReducedConfidence { .. }
        | BackgroundMessage::DirectFixApplied { .. }
        | BackgroundMessage::DirectFixError(_)
        | BackgroundMessage::ShipProgress(_)
        | BackgroundMessage::ShipComplete(_)
        | BackgroundMessage::ShipError(_)
        | BackgroundMessage::ResetComplete { .. }
        | BackgroundMessage::StashComplete { .. }
        | BackgroundMessage::DiscardComplete
        | BackgroundMessage::StartupSwitchedToMain { .. } => unreachable!(),
    }
}

fn handle_background_message(app: &mut App, msg: BackgroundMessage, ctx: &RuntimeContext) {
    let Some(msg) = handle_generation_messages(app, msg, ctx) else {
        return;
    };
    let Some(msg) = handle_preview_messages(app, msg, ctx) else {
        return;
    };
    let Some(msg) = handle_apply_messages(app, msg, ctx) else {
        return;
    };
    let Some(msg) = handle_ship_repo_messages(app, msg) else {
        return;
    };
    handle_misc_messages(app, msg, ctx);
}

pub fn drain_messages(
    app: &mut App,
    rx: &mpsc::Receiver<BackgroundMessage>,
    ctx: &RuntimeContext,
) -> bool {
    let mut changed = false;
    while let Ok(msg) = rx.try_recv() {
        changed = true;
        handle_background_message(app, msg, ctx);
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
        "suggest" => metric.suggest_ms = Some(duration_ms),
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
