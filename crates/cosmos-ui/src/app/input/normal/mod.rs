use crate::app::background;
use crate::app::messages::BackgroundMessage;
use crate::app::RuntimeContext;
use crate::ui::{ActivePanel, App, LoadingState, Overlay, ShipStep, WorkflowStep};
use anyhow::Result;
use cosmos_adapters::git_ops;
use cosmos_adapters::util::{hash_bytes, resolve_repo_path_allow_new};
use cosmos_core::suggest::Suggestion;
use cosmos_engine::llm::FixPreview;
use cosmos_engine::llm::{
    ImplementationAppliedFile, ImplementationFinalizationStatus, ImplementationHarnessRunContext,
};
use crossterm::event::{KeyCode, KeyEvent};
use std::collections::HashMap;
use std::path::PathBuf;

mod refresh;
use refresh::{llm_available_for_apply, prompt_api_key_setup, refresh_suggestions_now};

// =============================================================================
// Apply Fix Validation (Suggestions Enter key handling)
// =============================================================================

/// Errors that can occur when validating the apply fix action.
/// Each variant has a user-friendly message.
#[derive(Debug, Clone)]
enum ApplyError {
    /// Apply has not been armed by the first Enter press
    ApplyNotConfirmed,
    /// Fix is already being applied
    AlreadyApplying,
    /// The selected suggestion is no longer available
    SuggestionNotFound,
    /// Suggestion is not in the validated set
    SuggestionNotValidated,
    /// Suggestion is validated but marked as weakly grounded
    SuggestionWeakGrounding,
    /// Failed to check git status
    GitStatusFailed(String),
    /// Working tree has uncommitted changes
    DirtyWorkingTree,
    /// Files have changed since the preview was generated
    FilesChanged(Vec<PathBuf>),
    /// Path resolution failed (security check)
    UnsafePath(PathBuf, String),
    /// File read failed
    FileReadFailed(PathBuf, String),
}

impl ApplyError {
    /// Returns a user-friendly message for display in user-facing error UI
    fn user_message(&self) -> String {
        match self {
            Self::ApplyNotConfirmed => {
                "Apply pending: open the scope preview and confirm to apply this suggestion."
                    .into()
            }
            Self::AlreadyApplying => "Apply failed: already in progress...".into(),
            Self::SuggestionNotFound => {
                "Apply failed: suggestion no longer exists. Select another.".into()
            }
            Self::SuggestionNotValidated => {
                "Apply failed: suggestion is not in the validated set. Refresh suggestions and try again.".into()
            }
            Self::SuggestionWeakGrounding => {
                "Apply failed: suggestion grounding is too weak to apply safely. Refresh suggestions and pick a better-grounded item.".into()
            }
            Self::GitStatusFailed(e) => format!("Git error: {}. Check repo state.", e),
            Self::DirtyWorkingTree => {
                "Apply failed: working tree has changes. Commit or stash first.".into()
            }
            Self::FilesChanged(paths) => {
                let names: Vec<String> = paths
                    .iter()
                    .take(3)
                    .map(|p| p.display().to_string())
                    .collect();
                let more = paths.len().saturating_sub(3);
                let suffix = if more > 0 {
                    format!(" (+{} more)", more)
                } else {
                    String::new()
                };
                format!(
                    "Apply failed: files changed ({}{}). Refresh suggestions and try again.",
                    names.join(", "),
                    suffix
                )
            }
            Self::UnsafePath(path, e) => {
                format!("Apply failed: unsafe path {}: {}", path.display(), e)
            }
            Self::FileReadFailed(path, e) => {
                format!("Apply failed: couldn't read {}: {}", path.display(), e)
            }
        }
    }
}

/// Context needed to apply a fix, validated and ready to use
struct ApplyContext {
    preview: FixPreview,
    suggestion: Suggestion,
    repo_path: PathBuf,
    repo_memory_context: String,
}

fn suggestion_has_weak_grounding(suggestion: &Suggestion) -> bool {
    suggestion
        .implementation_risk_flags
        .iter()
        .any(|flag| flag == "claim_not_grounded_in_snippet")
}

/// Validates all preconditions for applying a fix from the Suggestions step.
/// Returns an ApplyContext if all conditions are met, or an ApplyError describing what failed.
fn validate_apply_fix(app: &App) -> std::result::Result<ApplyContext, ApplyError> {
    let suggestion_id = app
        .armed_suggestion_id
        .ok_or(ApplyError::ApplyNotConfirmed)?;
    if app.loading == LoadingState::GeneratingFix {
        return Err(ApplyError::AlreadyApplying);
    }

    let suggestion = app
        .suggestions
        .suggestions
        .iter()
        .find(|s| s.id == suggestion_id)
        .cloned()
        .ok_or(ApplyError::SuggestionNotFound)?;

    if suggestion.validation_state != cosmos_core::suggest::SuggestionValidationState::Validated {
        return Err(ApplyError::SuggestionNotValidated);
    }
    if suggestion_has_weak_grounding(&suggestion) {
        return Err(ApplyError::SuggestionWeakGrounding);
    }

    let current_hashes = snapshot_suggestion_file_hashes(app, &suggestion)?;
    let mut changed_files = Vec::new();
    for (path, current_hash) in &current_hashes {
        match app.armed_file_hashes.get(path) {
            Some(expected) if expected == current_hash => {}
            _ => changed_files.push(path.clone()),
        }
    }
    for path in app.armed_file_hashes.keys() {
        if !current_hashes.contains_key(path) {
            changed_files.push(path.clone());
        }
    }

    if !changed_files.is_empty() {
        return Err(ApplyError::FilesChanged(changed_files));
    }

    let status = git_ops::current_status(&app.repo_path)
        .map_err(|e| ApplyError::GitStatusFailed(e.to_string()))?;
    let changed_count = status.staged.len() + status.modified.len() + status.untracked.len();
    if changed_count > 0 {
        return Err(ApplyError::DirtyWorkingTree);
    }

    let preview = cosmos_engine::llm::build_fix_preview_from_validated_suggestion(&suggestion);
    Ok(ApplyContext {
        preview,
        suggestion,
        repo_path: app.repo_path.clone(),
        repo_memory_context: app.repo_memory.to_prompt_context(12, 900),
    })
}

fn snapshot_suggestion_file_hashes(
    app: &App,
    suggestion: &Suggestion,
) -> std::result::Result<HashMap<PathBuf, String>, ApplyError> {
    let mut hashes = HashMap::new();
    for target in suggestion.affected_files() {
        let resolved = resolve_repo_path_allow_new(&app.repo_path, target)
            .map_err(|e| ApplyError::UnsafePath(target.clone(), e.to_string()))?;
        let bytes = match std::fs::read(&resolved.absolute) {
            Ok(content) => content,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(e) => {
                return Err(ApplyError::FileReadFailed(
                    resolved.relative.clone(),
                    e.to_string(),
                ))
            }
        };
        hashes.insert(resolved.relative, hash_bytes(&bytes));
    }
    Ok(hashes)
}

fn append_apply_plan_audit(
    app: &App,
    suggestion: &Suggestion,
    preview: &FixPreview,
    affected_files: &[PathBuf],
    event: cosmos_adapters::cache::ApplyPlanAuditEvent,
) {
    let record = cosmos_adapters::cache::ApplyPlanAuditRecord {
        timestamp: chrono::Utc::now(),
        event,
        run_id: app.current_suggestion_run_id.clone(),
        suggestion_id: suggestion.id.to_string(),
        suggestion_summary: suggestion.summary.clone(),
        suggestion_file: suggestion.file.clone(),
        evidence_ids: suggestion
            .evidence_refs
            .iter()
            .map(|evidence| evidence.snippet_id)
            .collect(),
        affected_files: affected_files.to_vec(),
        preview_friendly_title: preview.friendly_title.clone(),
        preview_problem_summary: preview.problem_summary.clone(),
        preview_outcome: preview.outcome.clone(),
        preview_description: preview.description.clone(),
        preview_verification_note: preview.verification_note.clone(),
        preview_evidence_line: preview.evidence_line,
        preview_evidence_snippet: preview.evidence_snippet.clone(),
    };
    let cache = cosmos_adapters::cache::Cache::new(&app.repo_path);
    let _ = cache.append_apply_plan_audit(&record);
}

fn open_apply_plan_for_suggestion(
    app: &mut App,
    suggestion: &Suggestion,
) -> std::result::Result<(), ApplyError> {
    if suggestion.validation_state != cosmos_core::suggest::SuggestionValidationState::Validated {
        return Err(ApplyError::SuggestionNotValidated);
    }
    if suggestion_has_weak_grounding(suggestion) {
        return Err(ApplyError::SuggestionWeakGrounding);
    }

    let hashes = snapshot_suggestion_file_hashes(app, suggestion)?;
    let preview = cosmos_engine::llm::build_fix_preview_from_validated_suggestion(suggestion);
    let affected_files = suggestion
        .affected_files()
        .into_iter()
        .cloned()
        .collect::<Vec<_>>();
    append_apply_plan_audit(
        app,
        suggestion,
        &preview,
        &affected_files,
        cosmos_adapters::cache::ApplyPlanAuditEvent::Opened,
    );

    app.arm_apply_confirm(suggestion.id, hashes);

    let show_data_notice =
        !cosmos_adapters::cache::Cache::new(&app.repo_path).has_seen_data_notice();
    app.open_apply_plan_overlay(suggestion.id, preview, affected_files, show_data_notice);
    Ok(())
}

fn resolve_review_file_path(
    finding_file: &str,
    files: &[crate::ui::ReviewFileContent],
) -> Option<PathBuf> {
    let normalized = finding_file.replace('\\', "/");
    let candidate = PathBuf::from(&normalized);

    if let Some(found) = files.iter().find(|f| f.path == candidate) {
        return Some(found.path.clone());
    }

    for file in files {
        let file_str = file.path.to_string_lossy().replace('\\', "/");
        if normalized.ends_with(&file_str) {
            return Some(file.path.clone());
        }
    }

    let file_name = PathBuf::from(&normalized)
        .file_name()
        .and_then(|n| n.to_str())
        .map(str::to_string);
    if let Some(file_name) = file_name {
        let matches: Vec<_> = files
            .iter()
            .filter(|f| {
                f.path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .map(|n| n == file_name)
                    .unwrap_or(false)
            })
            .collect();
        if matches.len() == 1 {
            return Some(matches[0].path.clone());
        }
    }

    None
}

fn start_review_fix_for_selected_findings(app: &mut App, ctx: &RuntimeContext) {
    if app.review_state.selected.is_empty() || app.review_state.reviewing || app.review_state.fixing
    {
        return;
    }

    if app.session_cost >= 0.05 && !app.review_state.confirm_extra_review_budget {
        app.review_state.confirm_extra_review_budget = true;
        app.open_alert(
            "Budget guardrail",
            "This extra review-fix run is beyond the $0.05 session guardrail. Press f or Enter again to continue.",
        );
        return;
    }
    app.review_state.confirm_extra_review_budget = false;

    let selected_findings = app.get_selected_review_findings();
    let files = app.review_state.files.clone();
    let iter = app.review_state.review_iteration;
    let fixed = app.review_state.fixed_titles.clone();
    let repo_memory_context = app.repo_memory.to_prompt_context(12, 900);
    let memory = if repo_memory_context.trim().is_empty() {
        None
    } else {
        Some(repo_memory_context)
    };
    let tx_fix = ctx.tx.clone();

    app.set_review_fixing(true);

    background::spawn_background(ctx.tx.clone(), "verification_fix", async move {
        let stage_start = std::time::Instant::now();
        let mut findings_by_file: HashMap<PathBuf, Vec<cosmos_engine::llm::ReviewFinding>> =
            HashMap::new();

        for finding in selected_findings {
            let Some(path) = resolve_review_file_path(&finding.file, &files) else {
                let _ = tx_fix.send(BackgroundMessage::Error(format!(
                    "Review fix failed: finding file '{}' does not match changed files.",
                    finding.file
                )));
                return;
            };
            findings_by_file.entry(path).or_default().push(finding);
        }

        let mut file_changes: Vec<(PathBuf, String)> = Vec::new();
        let mut descriptions: Vec<String> = Vec::new();
        let mut total_usage = cosmos_engine::llm::Usage::default();
        let mut saw_usage = false;

        for (path, findings) in findings_by_file {
            let Some(file_state) = files.iter().find(|f| f.path == path) else {
                let _ = tx_fix.send(BackgroundMessage::Error(format!(
                    "Review fix failed: missing state for {}",
                    path.display()
                )));
                return;
            };

            let original_ref = if iter > 1 {
                Some(file_state.original_content.as_str())
            } else {
                None
            };

            match cosmos_engine::llm::fix_review_findings(
                &file_state.path,
                &file_state.new_content,
                original_ref,
                &findings,
                memory.clone(),
                iter,
                &fixed,
            )
            .await
            {
                Ok(fix) => {
                    descriptions.push(format!("{}: {}", path.display(), fix.description));
                    file_changes.push((path.clone(), fix.new_content));
                    if let Some(u) = fix.usage {
                        total_usage.prompt_tokens += u.prompt_tokens;
                        total_usage.completion_tokens += u.completion_tokens;
                        total_usage.total_tokens += u.total_tokens;
                        total_usage.cost = Some(total_usage.cost.unwrap_or(0.0) + u.cost());
                        saw_usage = true;
                    }
                }
                Err(e) => {
                    let _ = tx_fix.send(BackgroundMessage::Error(e.to_string()));
                    return;
                }
            }
        }

        let usage = if saw_usage { Some(total_usage) } else { None };
        let description = if descriptions.is_empty() {
            "Fixed selected review findings".to_string()
        } else {
            descriptions.join("; ")
        };

        let _ = tx_fix.send(BackgroundMessage::VerificationFixComplete {
            file_changes,
            description,
            usage,
            duration_ms: stage_start.elapsed().as_millis() as u64,
        });
    });
}

#[derive(Debug)]
struct ApplyFinalizationFailure {
    message: String,
    status: ImplementationFinalizationStatus,
    mutation_on_failure: bool,
}

fn has_repo_mutations(repo_path: &std::path::Path) -> bool {
    git_ops::current_status(repo_path)
        .map(|status| !(status.staged.is_empty() && status.modified.is_empty()))
        .unwrap_or(true)
}

fn apply_finalization_failure(
    message: String,
    status: ImplementationFinalizationStatus,
    mutation_on_failure: bool,
) -> ApplyFinalizationFailure {
    ApplyFinalizationFailure {
        message,
        status,
        mutation_on_failure,
    }
}

fn rollback_finalization_failure(
    repo_path: &std::path::Path,
    source_branch: &str,
    created_branch: &str,
    created_new_branch: bool,
    touched_files: &[PathBuf],
    message: String,
) -> ApplyFinalizationFailure {
    let rollback_detail = rollback_finalization(
        repo_path,
        source_branch,
        created_branch,
        created_new_branch,
        touched_files,
    );
    apply_finalization_failure(
        format!("{message} ({rollback_detail})"),
        ImplementationFinalizationStatus::RolledBack,
        has_repo_mutations(repo_path),
    )
}

fn validate_finalization_repo_state(
    repo_path: &std::path::Path,
    source_branch: &str,
) -> std::result::Result<(), ApplyFinalizationFailure> {
    let status = git_ops::current_status(repo_path).map_err(|error| {
        apply_finalization_failure(
            format!(
                "Finalization stopped because git status could not be read: {}",
                error
            ),
            ImplementationFinalizationStatus::FailedBeforeFinalize,
            true,
        )
    })?;

    if !source_branch.is_empty() && source_branch != "unknown" && status.branch != source_branch {
        return Err(apply_finalization_failure(
            format!(
                "Finalization stopped because the active branch changed from '{}' to '{}' while apply was running.",
                source_branch, status.branch
            ),
            ImplementationFinalizationStatus::FailedBeforeFinalize,
            false,
        ));
    }
    if !(status.staged.is_empty() && status.modified.is_empty()) {
        return Err(apply_finalization_failure(
            "Finalization stopped because repository state changed while preparing apply."
                .to_string(),
            ImplementationFinalizationStatus::FailedBeforeFinalize,
            true,
        ));
    }
    Ok(())
}

fn apply_finalized_file_on_branch(
    repo_path: &std::path::Path,
    source_branch: &str,
    branch_outcome: &git_ops::BranchCreateOutcome,
    touched_files: &mut Vec<PathBuf>,
    file: &ImplementationAppliedFile,
) -> std::result::Result<(PathBuf, String), ApplyFinalizationFailure> {
    let resolved = resolve_repo_path_allow_new(repo_path, &file.path).map_err(|error| {
        rollback_finalization_failure(
            repo_path,
            source_branch,
            &branch_outcome.branch_name,
            branch_outcome.created_new,
            touched_files,
            format!(
                "Finalization failed due to unsafe file path {}: {}",
                file.path.display(),
                error
            ),
        )
    })?;

    if let Some(parent) = resolved.absolute.parent() {
        std::fs::create_dir_all(parent).map_err(|error| {
            rollback_finalization_failure(
                repo_path,
                source_branch,
                &branch_outcome.branch_name,
                branch_outcome.created_new,
                touched_files,
                format!(
                    "Finalization failed while preparing {}: {}",
                    file.path.display(),
                    error
                ),
            )
        })?;
    }

    touched_files.push(resolved.relative.clone());

    std::fs::write(&resolved.absolute, &file.content).map_err(|error| {
        rollback_finalization_failure(
            repo_path,
            source_branch,
            &branch_outcome.branch_name,
            branch_outcome.created_new,
            touched_files,
            format!(
                "Finalization failed while writing {}: {}",
                file.path.display(),
                error
            ),
        )
    })?;

    git_ops::stage_file(repo_path, &resolved.relative.to_string_lossy()).map_err(|error| {
        rollback_finalization_failure(
            repo_path,
            source_branch,
            &branch_outcome.branch_name,
            branch_outcome.created_new,
            touched_files,
            format!(
                "Finalization failed while staging {}: {}",
                file.path.display(),
                error
            ),
        )
    })?;

    Ok((file.path.clone(), file.summary.clone()))
}

fn finalize_harness_result_on_branch(
    repo_path: &std::path::Path,
    source_branch: &str,
    suggestion: &Suggestion,
    files: &[ImplementationAppliedFile],
) -> std::result::Result<(String, Vec<(PathBuf, String)>), ApplyFinalizationFailure> {
    validate_finalization_repo_state(repo_path, source_branch)?;

    let branch_name =
        git_ops::generate_fix_branch_name(&suggestion.id.to_string(), &suggestion.summary);
    let branch_outcome =
        git_ops::create_fix_branch_from_current_with_outcome(repo_path, &branch_name).map_err(
            |error| {
                apply_finalization_failure(
                    format!("Could not create fix branch: {}", error),
                    ImplementationFinalizationStatus::FailedBeforeFinalize,
                    false,
                )
            },
        )?;

    let mut touched_files = Vec::new();
    let mut final_file_changes = Vec::new();
    for file in files {
        final_file_changes.push(apply_finalized_file_on_branch(
            repo_path,
            source_branch,
            &branch_outcome,
            &mut touched_files,
            file,
        )?);
    }
    Ok((branch_outcome.branch_name, final_file_changes))
}

fn rollback_finalization(
    repo_path: &std::path::Path,
    source_branch: &str,
    created_branch: &str,
    created_new_branch: bool,
    touched_files: &[PathBuf],
) -> String {
    let mut rollback_errors = Vec::new();

    for path in touched_files {
        if let Err(error) = git_ops::restore_file(repo_path, path) {
            rollback_errors.push(format!("restore {}: {}", path.display(), error));
        }
    }
    if let Err(error) = git_ops::checkout_branch(repo_path, source_branch) {
        rollback_errors.push(format!("checkout {}: {}", source_branch, error));
    }
    if created_new_branch {
        if let Err(error) = git_ops::delete_local_branch_safe(repo_path, created_branch) {
            rollback_errors.push(format!("delete branch {}: {}", created_branch, error));
        }
    }

    if rollback_errors.is_empty() {
        "rollback completed successfully".to_string()
    } else {
        format!("rollback had issues: {}", rollback_errors.join("; "))
    }
}

fn optional_repo_memory_context(repo_memory_context: String) -> Option<String> {
    if repo_memory_context.trim().is_empty() {
        None
    } else {
        Some(repo_memory_context)
    }
}

fn apply_harness_progress_detail(
    diagnostics: &cosmos_engine::llm::ImplementationAttemptDiagnostics,
) -> String {
    if diagnostics.passed {
        return "attempt passed all gates".to_string();
    }
    if diagnostics.fail_reasons.is_empty() {
        return "attempt completed".to_string();
    }
    format!(
        "{} gate miss(es): {}",
        diagnostics.fail_reasons.len(),
        diagnostics
            .fail_reasons
            .iter()
            .take(2)
            .cloned()
            .collect::<Vec<_>>()
            .join("; ")
    )
}

fn record_interactive_finalization_outcome(
    repo_path: &std::path::Path,
    diagnostics: &mut cosmos_engine::llm::ImplementationRunDiagnostics,
    status: ImplementationFinalizationStatus,
    detail: Option<String>,
    mutation_on_failure: bool,
) {
    let _ = cosmos_engine::llm::record_harness_finalization_outcome(
        repo_path,
        diagnostics,
        status,
        detail,
        Some(mutation_on_failure),
        ImplementationHarnessRunContext::Interactive,
        None,
    );
}

fn send_apply_harness_failed(
    tx_apply: &std::sync::mpsc::Sender<BackgroundMessage>,
    summary: String,
    fail_reasons: Vec<String>,
    report_path: Option<PathBuf>,
) {
    let _ = tx_apply.send(BackgroundMessage::ApplyHarnessFailed {
        summary,
        fail_reasons,
        report_path,
    });
}

fn send_apply_harness_reduced_confidence(
    tx_apply: &std::sync::mpsc::Sender<BackgroundMessage>,
    report_path: Option<PathBuf>,
) {
    let _ = tx_apply.send(BackgroundMessage::ApplyHarnessReducedConfidence {
        detail: "Quick checks were unavailable, so Cosmos could not automatically run your project checks. Treat this apply as lower confidence and consider running your tests before shipping.".to_string(),
        report_path,
    });
}

fn handle_non_passing_harness_result(
    tx_apply: &std::sync::mpsc::Sender<BackgroundMessage>,
    repo_path: &std::path::Path,
    result: &mut cosmos_engine::llm::ImplementationRunResult,
) {
    record_interactive_finalization_outcome(
        repo_path,
        &mut result.diagnostics,
        ImplementationFinalizationStatus::FailedBeforeFinalize,
        Some("Harness did not produce a passing attempt".to_string()),
        false,
    );
    send_apply_harness_failed(
        tx_apply,
        result.description.clone(),
        result.diagnostics.fail_reasons.clone(),
        result.diagnostics.report_path.clone(),
    );
}

fn handle_passing_harness_result(
    tx_apply: &std::sync::mpsc::Sender<BackgroundMessage>,
    repo_path: &std::path::Path,
    source_branch: &str,
    suggestion: &Suggestion,
    preview: &FixPreview,
    stage_start: std::time::Instant,
    result: &mut cosmos_engine::llm::ImplementationRunResult,
) {
    match finalize_harness_result_on_branch(
        repo_path,
        source_branch,
        suggestion,
        &result.file_changes,
    ) {
        Ok((created_branch, file_changes)) => {
            record_interactive_finalization_outcome(
                repo_path,
                &mut result.diagnostics,
                ImplementationFinalizationStatus::Applied,
                Some("Applied passing harness result on fix branch".to_string()),
                false,
            );
            if result.diagnostics.reduced_confidence {
                send_apply_harness_reduced_confidence(
                    tx_apply,
                    result.diagnostics.report_path.clone(),
                );
            }
            let _ = tx_apply.send(BackgroundMessage::DirectFixApplied {
                suggestion_id: suggestion.id,
                file_changes,
                description: result.description.clone(),
                usage: result.usage.clone(),
                branch_name: created_branch,
                source_branch: source_branch.to_string(),
                friendly_title: preview.friendly_title.clone(),
                problem_summary: preview.problem_summary.clone(),
                outcome: preview.outcome.clone(),
                duration_ms: stage_start.elapsed().as_millis() as u64,
            });
        }
        Err(finalize_error) => {
            record_interactive_finalization_outcome(
                repo_path,
                &mut result.diagnostics,
                finalize_error.status,
                Some(finalize_error.message.clone()),
                finalize_error.mutation_on_failure,
            );
            send_apply_harness_failed(
                tx_apply,
                "Harness found a safe fix but finalization could not complete.".to_string(),
                vec![finalize_error.message],
                result.diagnostics.report_path.clone(),
            );
        }
    }
}

fn start_apply_for_context(app: &mut App, ctx: &RuntimeContext, apply_ctx: ApplyContext) {
    app.loading = LoadingState::GeneratingFix;
    app.clear_apply_confirm();

    let tx_apply = ctx.tx.clone();
    let repo_path = apply_ctx.repo_path;
    let preview = apply_ctx.preview;
    let suggestion = apply_ctx.suggestion;
    let repo_memory_context = apply_ctx.repo_memory_context;

    background::spawn_background(ctx.tx.clone(), "apply_fix", async move {
        let stage_start = std::time::Instant::now();
        let source_branch = git_ops::current_status(&repo_path)
            .map(|s| s.branch)
            .unwrap_or_else(|_| "unknown".to_string());
        let mem = optional_repo_memory_context(repo_memory_context);

        let config = cosmos_engine::llm::ImplementationHarnessConfig::interactive_strict();
        let _ = tx_apply.send(BackgroundMessage::ApplyHarnessProgress {
            attempt_index: 1,
            attempt_count: config.max_attempts,
            detail: "starting strict implementation harness".to_string(),
        });
        let tx_progress = tx_apply.clone();

        match cosmos_engine::llm::implement_validated_suggestion_with_harness_with_progress(
            &repo_path,
            &suggestion,
            &preview,
            mem,
            config,
            |attempt_index, attempt_count, diagnostics| {
                let _ = tx_progress.send(BackgroundMessage::ApplyHarnessProgress {
                    attempt_index,
                    attempt_count,
                    detail: apply_harness_progress_detail(diagnostics),
                });
            },
        )
        .await
        {
            Ok(mut result) => {
                if !result.diagnostics.passed {
                    handle_non_passing_harness_result(&tx_apply, &repo_path, &mut result);
                    return;
                }
                handle_passing_harness_result(
                    &tx_apply,
                    &repo_path,
                    &source_branch,
                    &suggestion,
                    &preview,
                    stage_start,
                    &mut result,
                );
            }
            Err(e) => {
                let _ = tx_apply.send(BackgroundMessage::DirectFixError(e.to_string()));
            }
        }
    });
}

pub(super) fn confirm_apply_from_overlay(app: &mut App, ctx: &RuntimeContext) {
    match validate_apply_fix(app) {
        Ok(apply_ctx) => {
            let affected_files = apply_ctx
                .suggestion
                .affected_files()
                .into_iter()
                .cloned()
                .collect::<Vec<_>>();
            append_apply_plan_audit(
                app,
                &apply_ctx.suggestion,
                &apply_ctx.preview,
                &affected_files,
                cosmos_adapters::cache::ApplyPlanAuditEvent::Confirmed,
            );
            app.close_overlay();
            start_apply_for_context(app, ctx, apply_ctx);
        }
        Err(e) => {
            app.close_overlay();
            app.clear_apply_confirm();
            app.open_alert("Couldn't apply", e.user_message());
        }
    }
}

fn review_interaction_ready(app: &App) -> bool {
    app.workflow_step == WorkflowStep::Review
        && !app.review_state.reviewing
        && !app.review_state.fixing
}

fn handle_down_key(app: &mut App) {
    if app.active_panel == ActivePanel::Ask {
        if app.is_ask_cosmos_mode() {
            app.ask_cosmos_scroll_down();
        }
        return;
    }
    match app.workflow_step {
        WorkflowStep::Review if review_interaction_ready(app) => app.review_cursor_down(),
        WorkflowStep::Ship => app.ship_scroll_down(),
        WorkflowStep::Suggestions => app.navigate_down(),
        _ => {}
    }
}

fn handle_up_key(app: &mut App) {
    if app.active_panel == ActivePanel::Ask {
        if app.is_ask_cosmos_mode() {
            app.ask_cosmos_scroll_up();
        }
        return;
    }
    match app.workflow_step {
        WorkflowStep::Review if review_interaction_ready(app) => app.review_cursor_up(),
        WorkflowStep::Ship => app.ship_scroll_up(),
        WorkflowStep::Suggestions => app.navigate_up(),
        _ => {}
    }
}

fn handle_enter_in_ask_panel(app: &mut App) -> bool {
    if app.active_panel != ActivePanel::Ask {
        return false;
    }
    if app.workflow_step != WorkflowStep::Suggestions {
        return true;
    }
    if !cosmos_engine::llm::is_available() {
        prompt_api_key_setup(
            app,
            "No API key configured yet. Add your OpenRouter key to ask questions.",
        );
    } else {
        app.start_question();
    }
    true
}

fn handle_enter_suggestions(app: &mut App) {
    let suggestion = app.selected_suggestion().cloned();
    if let Some(suggestion) = suggestion {
        if !llm_available_for_apply() {
            prompt_api_key_setup(
                app,
                "No API key configured yet. Add your OpenRouter key to continue.",
            );
        } else if let Err(e) = open_apply_plan_for_suggestion(app, &suggestion) {
            app.open_alert("Couldn't open preview", e.user_message());
        }
    }
}

fn handle_enter_review(app: &mut App, ctx: &RuntimeContext) {
    if !review_interaction_ready(app) {
        return;
    }
    if !app.review_state.selected.is_empty() {
        start_review_fix_for_selected_findings(app, ctx);
        return;
    }
    if app.review_passed() {
        app.start_ship();
        return;
    }
    if app.review_state.confirm_ship {
        app.review_state.confirm_ship = false;
        app.start_ship();
        return;
    }
    app.review_state.confirm_ship = true;
}

fn start_ship_confirm(app: &mut App, ctx: &RuntimeContext) {
    let repo_path = app.repo_path.clone();
    let branch_name = app.ship_state.branch_name.clone();
    let commit_message = app.ship_state.commit_message.clone();
    let (pr_title, pr_body) = app.generate_pr_content();
    let tx_ship = ctx.tx.clone();

    app.set_ship_step(ShipStep::Committing);

    background::spawn_background(ctx.tx.clone(), "ship_confirm", async move {
        let _ = tx_ship.send(BackgroundMessage::ShipProgress(ShipStep::Committing));
        if let Err(e) = git_ops::commit(&repo_path, &commit_message) {
            let _ = tx_ship.send(BackgroundMessage::ShipError(e.to_string()));
            return;
        }

        let _ = tx_ship.send(BackgroundMessage::ShipProgress(ShipStep::Pushing));
        if let Err(e) = git_ops::push_branch(&repo_path, &branch_name) {
            let _ = tx_ship.send(BackgroundMessage::ShipError(e.to_string()));
            return;
        }

        let _ = tx_ship.send(BackgroundMessage::ShipProgress(ShipStep::CreatingPR));
        match git_ops::create_pr(&repo_path, &pr_title, &pr_body).await {
            Ok(url) => {
                let _ = tx_ship.send(BackgroundMessage::ShipComplete(url));
            }
            Err(e) => {
                let _ = tx_ship.send(BackgroundMessage::ShipError(e.to_string()));
            }
        }
    });
}

fn handle_enter_ship(app: &mut App, ctx: &RuntimeContext) {
    match app.ship_state.step {
        ShipStep::Confirm => start_ship_confirm(app, ctx),
        ShipStep::Done => {
            if let Some(url) = &app.ship_state.pr_url {
                let _ = git_ops::open_url(url);
            }
            app.workflow_complete();
        }
        _ => {}
    }
}

fn handle_enter_key(app: &mut App, ctx: &RuntimeContext) {
    if handle_enter_in_ask_panel(app) {
        return;
    }
    if let Some(url) = app.pr_url.take() {
        let _ = git_ops::open_url(&url);
        return;
    }
    match app.workflow_step {
        WorkflowStep::Suggestions => handle_enter_suggestions(app),
        WorkflowStep::Review => handle_enter_review(app, ctx),
        WorkflowStep::Ship => handle_enter_ship(app, ctx),
    }
}

fn handle_escape_key(app: &mut App) {
    if app.active_panel == ActivePanel::Ask && app.is_ask_cosmos_mode() {
        app.exit_ask_cosmos();
    } else if app.workflow_step == WorkflowStep::Suggestions && app.armed_suggestion_id.is_some() {
        app.clear_apply_confirm();
    } else if app.workflow_step != WorkflowStep::Suggestions {
        app.workflow_back();
    } else if !app.search_query.is_empty() {
        app.exit_search();
    } else if app.overlay != Overlay::None {
        app.close_overlay();
    }
}

/// Handle key events in normal mode (no special input active)
pub(super) fn handle_normal_mode(app: &mut App, key: KeyEvent, ctx: &RuntimeContext) -> Result<()> {
    match key.code {
        KeyCode::Char('q') => app.should_quit = true,
        KeyCode::Down => handle_down_key(app),
        KeyCode::Up => handle_up_key(app),
        KeyCode::Char(' ') => {
            if review_interaction_ready(app) {
                app.review_toggle_finding();
            }
        }
        KeyCode::Char('f') => {
            if review_interaction_ready(app) && !app.review_state.selected.is_empty() {
                start_review_fix_for_selected_findings(app, ctx);
            }
        }
        KeyCode::Enter => handle_enter_key(app, ctx),
        KeyCode::Esc => handle_escape_key(app),
        KeyCode::Char('?') => app.toggle_help(),
        KeyCode::Char('a') => {
            if app.active_panel == ActivePanel::Suggestions && review_interaction_ready(app) {
                app.review_select_all();
            }
        }
        KeyCode::Char('k') => app.open_api_key_overlay(None),
        KeyCode::Char('u') => {
            if let Err(e) = app.undo_last_pending_change() {
                app.open_alert("Couldn't undo", e);
            }
        }
        KeyCode::Char('r') => {
            if app.active_panel == ActivePanel::Suggestions
                && app.workflow_step == WorkflowStep::Suggestions
            {
                refresh_suggestions_now(app, ctx, "Manual refresh");
            }
        }
        KeyCode::Char('R') => app.open_reset_overlay(),
        KeyCode::Char('U') => {
            if let Some(target_version) = app.update_available.clone() {
                app.show_update_overlay(
                    cosmos_adapters::update::CURRENT_VERSION.to_string(),
                    target_version,
                );
            }
        }
        _ => {}
    }
    Ok(())
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests;
