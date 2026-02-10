use crate::app::background;
use crate::app::messages::BackgroundMessage;
use crate::app::RuntimeContext;
use crate::git_ops;
use crate::suggest;
use crate::suggest::llm::FixPreview;
use crate::suggest::llm::{ImplementationAppliedFile, ImplementationFinalizationStatus};
use crate::suggest::Suggestion;
use crate::ui::{ActivePanel, App, LoadingState, Overlay, ShipStep, WorkflowStep};
use crate::util::{hash_bytes, resolve_repo_path_allow_new};
use anyhow::Result;
use crossterm::event::{KeyCode, KeyEvent};
use std::collections::HashMap;
use std::path::PathBuf;
use uuid::Uuid;

// =============================================================================
// Apply Fix Validation (Suggestions Enter key handling)
// =============================================================================

/// Errors that can occur when validating the apply fix action.
/// Each variant has a user-friendly message.
#[derive(Debug, Clone)]
pub enum ApplyError {
    /// Apply has not been armed by the first Enter press
    ApplyNotConfirmed,
    /// Fix is already being applied
    AlreadyApplying,
    /// The selected suggestion is no longer available
    SuggestionNotFound,
    /// Suggestion is not in the refined validated set
    SuggestionNotValidated,
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
    /// Returns a user-friendly message for display in toasts
    pub fn user_message(&self) -> String {
        match self {
            Self::ApplyNotConfirmed => {
                "Apply pending: press Enter again to confirm applying this suggestion.".into()
            }
            Self::AlreadyApplying => "Apply failed: already in progress...".into(),
            Self::SuggestionNotFound => {
                "Apply failed: suggestion no longer exists. Select another.".into()
            }
            Self::SuggestionNotValidated => {
                "Apply failed: suggestion is not in the validated set. Regenerate suggestions and try again.".into()
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
pub struct ApplyContext {
    pub preview: FixPreview,
    pub suggestion: Suggestion,
    pub suggestion_id: Uuid,
    pub repo_path: PathBuf,
    pub repo_memory_context: String,
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

    if suggestion.validation_state != crate::suggest::SuggestionValidationState::Validated {
        return Err(ApplyError::SuggestionNotValidated);
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

    let preview = suggest::llm::build_fix_preview_from_validated_suggestion(&suggestion);
    Ok(ApplyContext {
        preview,
        suggestion_id: suggestion.id,
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
        app.show_toast(
            "Budget guardrail: press f/Enter again to run another review-fix cycle beyond $0.05.",
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
        let mut findings_by_file: HashMap<PathBuf, Vec<suggest::llm::ReviewFinding>> =
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
        let mut total_usage = suggest::llm::Usage::default();
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

            match suggest::llm::fix_review_findings(
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

fn finalize_harness_result_on_branch(
    repo_path: &std::path::Path,
    source_branch: &str,
    suggestion: &Suggestion,
    files: &[ImplementationAppliedFile],
) -> std::result::Result<(String, Vec<(PathBuf, String)>), ApplyFinalizationFailure> {
    let status = match git_ops::current_status(repo_path) {
        Ok(status) => status,
        Err(error) => {
            return Err(ApplyFinalizationFailure {
                message: format!(
                    "Finalization stopped because git status could not be read: {}",
                    error
                ),
                status: ImplementationFinalizationStatus::FailedBeforeFinalize,
                mutation_on_failure: true,
            });
        }
    };

    if !source_branch.is_empty() && source_branch != "unknown" && status.branch != source_branch {
        return Err(ApplyFinalizationFailure {
            message: format!(
                "Finalization stopped because the active branch changed from '{}' to '{}' while apply was running.",
                source_branch, status.branch
            ),
            status: ImplementationFinalizationStatus::FailedBeforeFinalize,
            mutation_on_failure: false,
        });
    }

    if !(status.staged.is_empty() && status.modified.is_empty()) {
        return Err(ApplyFinalizationFailure {
            message: "Finalization stopped because repository state changed while preparing apply."
                .to_string(),
            status: ImplementationFinalizationStatus::FailedBeforeFinalize,
            mutation_on_failure: true,
        });
    }

    let branch_name =
        git_ops::generate_fix_branch_name(&suggestion.id.to_string(), &suggestion.summary);
    let branch_outcome =
        match git_ops::create_fix_branch_from_current_with_outcome(repo_path, &branch_name) {
            Ok(outcome) => outcome,
            Err(error) => {
                return Err(ApplyFinalizationFailure {
                    message: format!("Could not create fix branch: {}", error),
                    status: ImplementationFinalizationStatus::FailedBeforeFinalize,
                    mutation_on_failure: false,
                });
            }
        };

    let mut touched_files = Vec::new();
    let mut final_file_changes = Vec::new();
    for file in files {
        let resolved = match resolve_repo_path_allow_new(repo_path, &file.path) {
            Ok(path) => path,
            Err(error) => {
                let rollback_detail = rollback_finalization(
                    repo_path,
                    source_branch,
                    &branch_outcome.branch_name,
                    branch_outcome.created_new,
                    &touched_files,
                );
                return Err(ApplyFinalizationFailure {
                    message: format!(
                        "Finalization failed due to unsafe file path {}: {} ({})",
                        file.path.display(),
                        error,
                        rollback_detail
                    ),
                    status: ImplementationFinalizationStatus::RolledBack,
                    mutation_on_failure: has_repo_mutations(repo_path),
                });
            }
        };

        if let Some(parent) = resolved.absolute.parent() {
            if let Err(error) = std::fs::create_dir_all(parent) {
                let rollback_detail = rollback_finalization(
                    repo_path,
                    source_branch,
                    &branch_outcome.branch_name,
                    branch_outcome.created_new,
                    &touched_files,
                );
                return Err(ApplyFinalizationFailure {
                    message: format!(
                        "Finalization failed while preparing {}: {} ({})",
                        file.path.display(),
                        error,
                        rollback_detail
                    ),
                    status: ImplementationFinalizationStatus::RolledBack,
                    mutation_on_failure: has_repo_mutations(repo_path),
                });
            }
        }

        touched_files.push(resolved.relative.clone());

        if let Err(error) = std::fs::write(&resolved.absolute, &file.content) {
            let rollback_detail = rollback_finalization(
                repo_path,
                source_branch,
                &branch_outcome.branch_name,
                branch_outcome.created_new,
                &touched_files,
            );
            return Err(ApplyFinalizationFailure {
                message: format!(
                    "Finalization failed while writing {}: {} ({})",
                    file.path.display(),
                    error,
                    rollback_detail
                ),
                status: ImplementationFinalizationStatus::RolledBack,
                mutation_on_failure: has_repo_mutations(repo_path),
            });
        }

        if let Err(error) = git_ops::stage_file(repo_path, &resolved.relative.to_string_lossy()) {
            let rollback_detail = rollback_finalization(
                repo_path,
                source_branch,
                &branch_outcome.branch_name,
                branch_outcome.created_new,
                &touched_files,
            );
            return Err(ApplyFinalizationFailure {
                message: format!(
                    "Finalization failed while staging {}: {} ({})",
                    file.path.display(),
                    error,
                    rollback_detail
                ),
                status: ImplementationFinalizationStatus::RolledBack,
                mutation_on_failure: has_repo_mutations(repo_path),
            });
        }

        final_file_changes.push((file.path.clone(), file.summary.clone()));
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

fn start_apply_for_context(app: &mut App, ctx: &RuntimeContext, apply_ctx: ApplyContext) {
    app.loading = LoadingState::GeneratingFix;
    app.clear_apply_confirm();

    let tx_apply = ctx.tx.clone();
    let repo_path = apply_ctx.repo_path;
    let preview = apply_ctx.preview;
    let suggestion = apply_ctx.suggestion;
    let sid = apply_ctx.suggestion_id;
    let repo_memory_context = apply_ctx.repo_memory_context;

    background::spawn_background(ctx.tx.clone(), "apply_fix", async move {
        let stage_start = std::time::Instant::now();
        let source_branch = git_ops::current_status(&repo_path)
            .map(|s| s.branch)
            .unwrap_or_else(|_| "unknown".to_string());

        let mem = if repo_memory_context.trim().is_empty() {
            None
        } else {
            Some(repo_memory_context)
        };

        let config = suggest::llm::ImplementationHarnessConfig::interactive_strict();
        let _ = tx_apply.send(BackgroundMessage::ApplyHarnessProgress {
            attempt_index: 1,
            attempt_count: config.max_attempts,
            detail: "starting strict implementation harness".to_string(),
        });
        let tx_progress = tx_apply.clone();

        match suggest::llm::implement_validated_suggestion_with_harness_with_progress(
            &repo_path,
            &suggestion,
            &preview,
            mem,
            config,
            |attempt_index, attempt_count, diagnostics| {
                let detail = if diagnostics.passed {
                    "attempt passed all gates".to_string()
                } else if diagnostics.fail_reasons.is_empty() {
                    "attempt completed".to_string()
                } else {
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
                };
                let _ = tx_progress.send(BackgroundMessage::ApplyHarnessProgress {
                    attempt_index,
                    attempt_count,
                    detail,
                });
            },
        )
        .await
        {
            Ok(mut result) => {
                if result.diagnostics.passed {
                    match finalize_harness_result_on_branch(
                        &repo_path,
                        &source_branch,
                        &suggestion,
                        &result.file_changes,
                    ) {
                        Ok((created_branch, file_changes)) => {
                            let _ = suggest::llm::record_harness_finalization_outcome(
                                &repo_path,
                                &mut result.diagnostics,
                                ImplementationFinalizationStatus::Applied,
                                Some("Applied passing harness result on fix branch".to_string()),
                                Some(false),
                            );
                            if result.diagnostics.reduced_confidence {
                                let _ = tx_apply.send(BackgroundMessage::ApplyHarnessReducedConfidence {
                                    detail: "Quick checks were unavailable, so Cosmos could not automatically run your project checks. Treat this apply as lower confidence and consider running your tests before shipping.".to_string(),
                                    report_path: result.diagnostics.report_path.clone(),
                                });
                            }
                            let _ = tx_apply.send(BackgroundMessage::DirectFixApplied {
                                suggestion_id: sid,
                                file_changes,
                                description: result.description,
                                usage: result.usage,
                                branch_name: created_branch,
                                source_branch: source_branch.clone(),
                                friendly_title: preview.friendly_title.clone(),
                                problem_summary: preview.problem_summary.clone(),
                                outcome: preview.outcome.clone(),
                                duration_ms: stage_start.elapsed().as_millis() as u64,
                            });
                        }
                        Err(finalize_error) => {
                            let _ = suggest::llm::record_harness_finalization_outcome(
                                &repo_path,
                                &mut result.diagnostics,
                                finalize_error.status,
                                Some(finalize_error.message.clone()),
                                Some(finalize_error.mutation_on_failure),
                            );
                            let _ = tx_apply.send(BackgroundMessage::ApplyHarnessFailed {
                                summary:
                                    "Harness found a safe fix but finalization could not complete."
                                        .to_string(),
                                fail_reasons: vec![finalize_error.message],
                                report_path: result.diagnostics.report_path.clone(),
                            });
                        }
                    }
                } else {
                    let _ = suggest::llm::record_harness_finalization_outcome(
                        &repo_path,
                        &mut result.diagnostics,
                        ImplementationFinalizationStatus::FailedBeforeFinalize,
                        Some("Harness did not produce a passing attempt".to_string()),
                        Some(false),
                    );
                    let _ = tx_apply.send(BackgroundMessage::ApplyHarnessFailed {
                        summary: result.description,
                        fail_reasons: result.diagnostics.fail_reasons,
                        report_path: result.diagnostics.report_path.clone(),
                    });
                }
            }
            Err(e) => {
                let _ = tx_apply.send(BackgroundMessage::DirectFixError(e.to_string()));
            }
        }
    });
}

fn llm_available_for_apply() -> bool {
    #[cfg(test)]
    {
        true
    }
    #[cfg(not(test))]
    {
        suggest::llm::is_available()
    }
}

/// Handle key events in normal mode (no special input active)
pub(super) fn handle_normal_mode(app: &mut App, key: KeyEvent, ctx: &RuntimeContext) -> Result<()> {
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
                start_review_fix_for_selected_findings(app, ctx);
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
                                if app.suggestion_refinement_in_progress {
                                    app.show_toast(
                                        "Suggestions are still refining. Wait for refined results before applying.",
                                    );
                                    return Ok(());
                                }
                                let suggestion = app.selected_suggestion().cloned();
                                if let Some(suggestion) = suggestion {
                                    if !llm_available_for_apply() {
                                        app.show_toast("Run: cosmos --setup");
                                    } else if app.armed_suggestion_id != Some(suggestion.id) {
                                        match snapshot_suggestion_file_hashes(app, &suggestion) {
                                            Ok(hashes) => {
                                                app.arm_apply_confirm(suggestion.id, hashes);
                                                app.show_toast(
                                                    "Press Enter again to apply this validated suggestion.",
                                                );
                                            }
                                            Err(e) => app.show_toast(&e.user_message()),
                                        }
                                    } else {
                                        match validate_apply_fix(app) {
                                            Ok(apply_ctx) => {
                                                start_apply_for_context(app, ctx, apply_ctx);
                                            }
                                            Err(e) => {
                                                app.clear_apply_confirm();
                                                app.show_toast(&e.user_message());
                                            }
                                        }
                                    }
                                }
                            }
                            WorkflowStep::Review => {
                                if !app.review_state.reviewing && !app.review_state.fixing {
                                    if !app.review_state.selected.is_empty() {
                                        // Fix selected findings (same as 'f' key)
                                        start_review_fix_for_selected_findings(app, ctx);
                                    } else if app.review_passed() {
                                        // Review passed - move to Ship
                                        app.start_ship();
                                    } else if app.review_state.verification_failed {
                                        if app.review_state.confirm_ship {
                                            app.review_state.confirm_ship = false;
                                            app.start_ship();
                                        } else {
                                            app.review_state.confirm_ship = true;
                                            app.show_toast(
                                                "Verification failed. Press Enter again to ship with manual override.",
                                            );
                                        }
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
                                    ShipStep::Confirm => {
                                        // Start the ship process
                                        let repo_path = app.repo_path.clone();
                                        let branch_name = app.ship_state.branch_name.clone();
                                        let commit_message = app.ship_state.commit_message.clone();
                                        let (pr_title, pr_body) = app.generate_pr_content();
                                        let tx_ship = ctx.tx.clone();

                                        app.set_ship_step(ShipStep::Committing);

                                        background::spawn_background(
                                            ctx.tx.clone(),
                                            "ship_confirm",
                                            async move {
                                                // Execute ship workflow
                                                let _ =
                                                    tx_ship.send(BackgroundMessage::ShipProgress(
                                                        ShipStep::Committing,
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

                                                let _ =
                                                    tx_ship.send(BackgroundMessage::ShipProgress(
                                                        ShipStep::Pushing,
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

                                                let _ =
                                                    tx_ship.send(BackgroundMessage::ShipProgress(
                                                        ShipStep::CreatingPR,
                                                    ));

                                                // Create PR with human-friendly content
                                                match git_ops::create_pr(
                                                    &repo_path, &pr_title, &pr_body,
                                                )
                                                .await
                                                {
                                                    Ok(url) => {
                                                        let _ = tx_ship.send(
                                                            BackgroundMessage::ShipComplete(url),
                                                        );
                                                    }
                                                    Err(e) => {
                                                        let _ = tx_ship.send(
                                                            BackgroundMessage::ShipError(
                                                                e.to_string(),
                                                            ),
                                                        );
                                                    }
                                                }
                                            },
                                        );
                                    }
                                    ShipStep::Done => {
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
                && app.workflow_step == WorkflowStep::Suggestions
                && app.armed_suggestion_id.is_some()
            {
                app.clear_apply_confirm();
                app.show_toast("Apply canceled.");
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
            // Ask Cosmos - only available from home (Suggestions step, not in workflow)
            if app.workflow_step != WorkflowStep::Suggestions {
                // Silently ignore during workflow
            } else if !suggest::llm::is_available() {
                app.show_toast("Run: cosmos --setup");
            } else {
                app.start_question();
            }
        }
        KeyCode::Char('u') => {
            // Undo the last applied change (restore from git)
            match app.undo_last_pending_change() {
                Ok(()) => app.show_toast("Change undone"),
                Err(e) => app.show_toast(&e),
            }
        }
        KeyCode::Char('R') => {
            // Open reset cosmos overlay
            app.open_reset_overlay();
        }
        KeyCode::Char('U') => {
            // Show update overlay if update is available
            if let Some(target_version) = app.update_available.clone() {
                app.show_update_overlay(crate::update::CURRENT_VERSION.to_string(), target_version);
            } else {
                app.show_toast(&format!(
                    "Already running latest version (v{})",
                    crate::update::CURRENT_VERSION
                ));
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
mod tests {
    use super::*;
    use crate::context::WorkContext;
    use crate::index::CodebaseIndex;
    use crate::suggest::SuggestionEngine;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use git2::{Repository, Signature};
    use std::collections::HashMap;
    use std::sync::mpsc;
    use std::time::{SystemTime, UNIX_EPOCH};
    use tempfile::tempdir;

    // ========================================================================
    // ApplyError User Message Tests
    // ========================================================================

    #[test]
    fn test_apply_error_apply_not_confirmed() {
        let err = ApplyError::ApplyNotConfirmed;
        let msg = err.user_message();
        assert!(msg.contains("press Enter again"));
    }

    #[test]
    fn test_apply_error_already_applying() {
        let err = ApplyError::AlreadyApplying;
        let msg = err.user_message();
        assert!(msg.contains("in progress"));
        assert!(msg.contains("failed")); // Must contain for toast visibility
    }

    #[test]
    fn test_apply_error_suggestion_not_validated() {
        let err = ApplyError::SuggestionNotValidated;
        let msg = err.user_message();
        assert!(msg.contains("validated"));
        assert!(msg.contains("failed"));
    }

    #[test]
    fn test_apply_error_suggestion_not_found() {
        let err = ApplyError::SuggestionNotFound;
        let msg = err.user_message();
        assert!(msg.contains("no longer exists"));
        assert!(msg.contains("failed")); // Must contain for toast visibility
    }

    #[test]
    fn test_apply_error_git_status_failed() {
        let err = ApplyError::GitStatusFailed("repository not found".into());
        let msg = err.user_message();
        assert!(msg.contains("Git error"));
        assert!(msg.contains("repository not found"));
    }

    #[test]
    fn test_apply_error_dirty_working_tree() {
        let err = ApplyError::DirtyWorkingTree;
        let msg = err.user_message();
        assert!(msg.contains("changes"));
        assert!(msg.contains("stash"));
        assert!(msg.contains("failed")); // Must contain for toast visibility
    }

    #[test]
    fn test_apply_error_files_changed_single() {
        let err = ApplyError::FilesChanged(vec![PathBuf::from("src/main.rs")]);
        let msg = err.user_message();
        assert!(msg.contains("files changed"));
        assert!(msg.contains("src/main.rs"));
        assert!(msg.contains("Refresh suggestions"));
        assert!(msg.contains("failed")); // Must contain for toast visibility
    }

    #[test]
    fn test_apply_error_files_changed_multiple() {
        let err = ApplyError::FilesChanged(vec![
            PathBuf::from("a.rs"),
            PathBuf::from("b.rs"),
            PathBuf::from("c.rs"),
            PathBuf::from("d.rs"),
        ]);
        let msg = err.user_message();
        assert!(msg.contains("a.rs"));
        assert!(msg.contains("b.rs"));
        assert!(msg.contains("c.rs"));
        assert!(msg.contains("+1 more"));
        assert!(msg.contains("failed")); // Must contain for toast visibility
    }

    #[test]
    fn test_apply_error_unsafe_path() {
        let err = ApplyError::UnsafePath(PathBuf::from("../evil"), "path traversal".into());
        let msg = err.user_message();
        assert!(msg.contains("unsafe path"));
        assert!(msg.contains("../evil"));
        assert!(msg.contains("failed")); // Must contain for toast visibility
    }

    #[test]
    fn test_apply_error_file_read_failed() {
        let err = ApplyError::FileReadFailed(PathBuf::from("missing.rs"), "not found".into());
        let msg = err.user_message();
        assert!(msg.contains("couldn't read"));
        assert!(msg.contains("missing.rs"));
        assert!(msg.contains("failed")); // Must contain for toast visibility
    }

    // ========================================================================
    // ApplyError Debug Trait Tests
    // ========================================================================

    #[test]
    fn test_apply_error_is_debug() {
        // Ensure Debug trait is implemented for logging
        let err = ApplyError::ApplyNotConfirmed;
        let debug_str = format!("{:?}", err);
        assert!(debug_str.contains("ApplyNotConfirmed"));
    }

    #[test]
    fn test_apply_error_is_clone() {
        // Ensure Clone trait is implemented
        let err = ApplyError::DirtyWorkingTree;
        let cloned = err.clone();
        assert_eq!(err.user_message(), cloned.user_message());
    }

    #[test]
    fn enter_is_blocked_while_suggestion_refinement_in_progress() {
        let mut root = std::env::temp_dir();
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        root.push(format!("cosmos_normal_mode_test_{}", nanos));
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
            repo_root: root.clone(),
        };
        let mut app = App::new(index.clone(), suggestions, context);
        app.active_panel = ActivePanel::Suggestions;
        app.workflow_step = WorkflowStep::Suggestions;
        app.suggestion_refinement_in_progress = true;

        let (tx, _rx) = mpsc::channel();
        let ctx = crate::app::RuntimeContext {
            index: &index,
            repo_path: &root,
            tx: &tx,
        };

        let key = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        handle_normal_mode(&mut app, key, &ctx).unwrap();

        let toast = app
            .toast
            .as_ref()
            .map(|t| t.message.clone())
            .unwrap_or_default();
        assert!(toast.contains("still refining"));
        assert_eq!(app.workflow_step, WorkflowStep::Suggestions);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn enter_arms_apply_confirmation_on_first_press() {
        std::env::set_var("OPENROUTER_API_KEY", "test-key");
        let mut root = std::env::temp_dir();
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        root.push(format!("cosmos_apply_arm_test_{}", nanos));
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/lib.rs"), "fn demo() {}\n").unwrap();

        let index = CodebaseIndex {
            root: root.clone(),
            files: HashMap::new(),
            index_errors: Vec::new(),
            git_head: Some("deadbeef".to_string()),
        };
        let mut suggestions = SuggestionEngine::new(index.clone());
        let suggestion = crate::suggest::Suggestion::new(
            crate::suggest::SuggestionKind::Improvement,
            crate::suggest::Priority::High,
            PathBuf::from("src/lib.rs"),
            "Improve demo".to_string(),
            crate::suggest::SuggestionSource::LlmDeep,
        )
        .with_validation_state(crate::suggest::SuggestionValidationState::Validated)
        .with_line(1);
        let suggestion_id = suggestion.id;
        suggestions.suggestions.push(suggestion);

        let context = WorkContext {
            branch: "main".to_string(),
            uncommitted_files: Vec::new(),
            staged_files: Vec::new(),
            untracked_files: Vec::new(),
            inferred_focus: None,
            modified_count: 0,
            repo_root: root.clone(),
        };
        let mut app = App::new(index.clone(), suggestions, context);
        app.active_panel = ActivePanel::Suggestions;
        app.workflow_step = WorkflowStep::Suggestions;

        let (tx, _rx) = mpsc::channel();
        let ctx = crate::app::RuntimeContext {
            index: &index,
            repo_path: &root,
            tx: &tx,
        };

        let key = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        handle_normal_mode(&mut app, key, &ctx).unwrap();

        assert_eq!(app.armed_suggestion_id, Some(suggestion_id));
        assert!(!app.armed_file_hashes.is_empty());
        let toast = app
            .toast
            .as_ref()
            .map(|t| t.message.clone())
            .unwrap_or_default();
        assert!(toast.contains("Press Enter again"));

        std::env::remove_var("OPENROUTER_API_KEY");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn apply_arm_resets_on_selection_change() {
        std::env::set_var("OPENROUTER_API_KEY", "test-key");
        let mut root = std::env::temp_dir();
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        root.push(format!("cosmos_apply_arm_reset_test_{}", nanos));
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/a.rs"), "fn a() {}\n").unwrap();
        std::fs::write(root.join("src/b.rs"), "fn b() {}\n").unwrap();

        let index = CodebaseIndex {
            root: root.clone(),
            files: HashMap::new(),
            index_errors: Vec::new(),
            git_head: Some("deadbeef".to_string()),
        };
        let mut suggestions = SuggestionEngine::new(index.clone());
        suggestions.suggestions.push(
            crate::suggest::Suggestion::new(
                crate::suggest::SuggestionKind::Improvement,
                crate::suggest::Priority::High,
                PathBuf::from("src/a.rs"),
                "Improve A".to_string(),
                crate::suggest::SuggestionSource::LlmDeep,
            )
            .with_validation_state(crate::suggest::SuggestionValidationState::Validated)
            .with_line(1),
        );
        suggestions.suggestions.push(
            crate::suggest::Suggestion::new(
                crate::suggest::SuggestionKind::Improvement,
                crate::suggest::Priority::High,
                PathBuf::from("src/b.rs"),
                "Improve B".to_string(),
                crate::suggest::SuggestionSource::LlmDeep,
            )
            .with_validation_state(crate::suggest::SuggestionValidationState::Validated)
            .with_line(1),
        );

        let context = WorkContext {
            branch: "main".to_string(),
            uncommitted_files: Vec::new(),
            staged_files: Vec::new(),
            untracked_files: Vec::new(),
            inferred_focus: None,
            modified_count: 0,
            repo_root: root.clone(),
        };
        let mut app = App::new(index.clone(), suggestions, context);
        app.active_panel = ActivePanel::Suggestions;
        app.workflow_step = WorkflowStep::Suggestions;

        let (tx, _rx) = mpsc::channel();
        let ctx = crate::app::RuntimeContext {
            index: &index,
            repo_path: &root,
            tx: &tx,
        };

        handle_normal_mode(
            &mut app,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            &ctx,
        )
        .unwrap();
        assert!(app.armed_suggestion_id.is_some());

        app.navigate_down();
        assert!(app.armed_suggestion_id.is_none());
        assert!(app.armed_file_hashes.is_empty());

        std::env::remove_var("OPENROUTER_API_KEY");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn esc_clears_apply_confirmation() {
        std::env::set_var("OPENROUTER_API_KEY", "test-key");
        let mut root = std::env::temp_dir();
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        root.push(format!("cosmos_apply_arm_esc_test_{}", nanos));
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/lib.rs"), "fn demo() {}\n").unwrap();

        let index = CodebaseIndex {
            root: root.clone(),
            files: HashMap::new(),
            index_errors: Vec::new(),
            git_head: Some("deadbeef".to_string()),
        };
        let mut suggestions = SuggestionEngine::new(index.clone());
        suggestions.suggestions.push(
            crate::suggest::Suggestion::new(
                crate::suggest::SuggestionKind::Improvement,
                crate::suggest::Priority::High,
                PathBuf::from("src/lib.rs"),
                "Improve demo".to_string(),
                crate::suggest::SuggestionSource::LlmDeep,
            )
            .with_validation_state(crate::suggest::SuggestionValidationState::Validated)
            .with_line(1),
        );
        let context = WorkContext {
            branch: "main".to_string(),
            uncommitted_files: Vec::new(),
            staged_files: Vec::new(),
            untracked_files: Vec::new(),
            inferred_focus: None,
            modified_count: 0,
            repo_root: root.clone(),
        };
        let mut app = App::new(index.clone(), suggestions, context);
        app.active_panel = ActivePanel::Suggestions;
        app.workflow_step = WorkflowStep::Suggestions;

        let (tx, _rx) = mpsc::channel();
        let ctx = crate::app::RuntimeContext {
            index: &index,
            repo_path: &root,
            tx: &tx,
        };

        handle_normal_mode(
            &mut app,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            &ctx,
        )
        .unwrap();
        assert!(app.armed_suggestion_id.is_some());

        handle_normal_mode(
            &mut app,
            KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
            &ctx,
        )
        .unwrap();
        assert!(app.armed_suggestion_id.is_none());
        assert!(app.armed_file_hashes.is_empty());

        std::env::remove_var("OPENROUTER_API_KEY");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn second_enter_reports_files_changed_since_arming() {
        std::env::set_var("OPENROUTER_API_KEY", "test-key");
        let mut root = std::env::temp_dir();
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        root.push(format!("cosmos_apply_arm_changed_test_{}", nanos));
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/lib.rs"), "fn demo() {}\n").unwrap();

        let index = CodebaseIndex {
            root: root.clone(),
            files: HashMap::new(),
            index_errors: Vec::new(),
            git_head: Some("deadbeef".to_string()),
        };
        let mut suggestions = SuggestionEngine::new(index.clone());
        suggestions.suggestions.push(
            crate::suggest::Suggestion::new(
                crate::suggest::SuggestionKind::Improvement,
                crate::suggest::Priority::High,
                PathBuf::from("src/lib.rs"),
                "Improve demo".to_string(),
                crate::suggest::SuggestionSource::LlmDeep,
            )
            .with_validation_state(crate::suggest::SuggestionValidationState::Validated)
            .with_line(1),
        );
        let context = WorkContext {
            branch: "main".to_string(),
            uncommitted_files: Vec::new(),
            staged_files: Vec::new(),
            untracked_files: Vec::new(),
            inferred_focus: None,
            modified_count: 0,
            repo_root: root.clone(),
        };
        let mut app = App::new(index.clone(), suggestions, context);
        app.active_panel = ActivePanel::Suggestions;
        app.workflow_step = WorkflowStep::Suggestions;

        let (tx, _rx) = mpsc::channel();
        let ctx = crate::app::RuntimeContext {
            index: &index,
            repo_path: &root,
            tx: &tx,
        };

        handle_normal_mode(
            &mut app,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            &ctx,
        )
        .unwrap();
        std::fs::write(root.join("src/lib.rs"), "fn demo() { println!(\"x\"); }\n").unwrap();
        handle_normal_mode(
            &mut app,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            &ctx,
        )
        .unwrap();

        let toast = app
            .toast
            .as_ref()
            .map(|t| t.message.clone())
            .unwrap_or_default();
        assert!(toast.contains("files changed"));
        assert!(app.armed_suggestion_id.is_none());

        std::env::remove_var("OPENROUTER_API_KEY");
        let _ = std::fs::remove_dir_all(root);
    }

    fn init_temp_git_repo_with_file() -> (tempfile::TempDir, PathBuf) {
        let dir = tempdir().unwrap();
        let repo_path = dir.path().to_path_buf();
        let repo = Repository::init(&repo_path).unwrap();
        let mut config = repo.config().unwrap();
        config.set_str("user.name", "Test User").unwrap();
        config.set_str("user.email", "test@example.com").unwrap();

        std::fs::create_dir_all(repo_path.join("src")).unwrap();
        std::fs::write(repo_path.join("src/lib.rs"), "fn demo() {}\n").unwrap();

        let sig = Signature::now("Test User", "test@example.com").unwrap();
        let mut index = repo.index().unwrap();
        index.add_path(std::path::Path::new("src/lib.rs")).unwrap();
        index.write().unwrap();
        let tree_id = index.write_tree().unwrap();
        let tree = repo.find_tree(tree_id).unwrap();
        repo.commit(Some("HEAD"), &sig, &sig, "Initial commit", &tree, &[])
            .unwrap();
        (dir, repo_path)
    }

    #[test]
    fn finalization_refuses_if_branch_changed_during_apply() {
        let (_dir, repo_path) = init_temp_git_repo_with_file();
        let source_branch = git_ops::current_status(&repo_path).unwrap().branch;

        let repo = Repository::open(&repo_path).unwrap();
        let head_commit = repo.head().unwrap().peel_to_commit().unwrap();
        repo.branch("feature/other", &head_commit, false).unwrap();
        git_ops::checkout_branch(&repo_path, "feature/other").unwrap();

        let suggestion = crate::suggest::Suggestion::new(
            crate::suggest::SuggestionKind::Improvement,
            crate::suggest::Priority::High,
            PathBuf::from("src/lib.rs"),
            "Improve demo".to_string(),
            crate::suggest::SuggestionSource::LlmDeep,
        );
        let branch_name =
            git_ops::generate_fix_branch_name(&suggestion.id.to_string(), &suggestion.summary);

        let result = finalize_harness_result_on_branch(
            &repo_path,
            &source_branch,
            &suggestion,
            &[ImplementationAppliedFile {
                path: PathBuf::from("src/lib.rs"),
                summary: "Modified".to_string(),
                content: "fn demo() { println!(\"x\"); }\n".to_string(),
            }],
        );
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(
            err.status,
            ImplementationFinalizationStatus::FailedBeforeFinalize
        );
        assert!(err.message.contains("active branch changed"));

        // Ensure finalization did not create/switch to a fix branch.
        let status = git_ops::current_status(&repo_path).unwrap();
        assert_eq!(status.branch, "feature/other");
        assert!(Repository::open(&repo_path)
            .unwrap()
            .find_branch(&branch_name, git2::BranchType::Local)
            .is_err());
    }

    #[test]
    fn finalization_rolls_back_on_unsafe_path_and_deletes_branch() {
        let (_dir, repo_path) = init_temp_git_repo_with_file();
        let source_branch = git_ops::current_status(&repo_path).unwrap().branch;

        let suggestion = crate::suggest::Suggestion::new(
            crate::suggest::SuggestionKind::Improvement,
            crate::suggest::Priority::High,
            PathBuf::from("src/lib.rs"),
            "Improve demo".to_string(),
            crate::suggest::SuggestionSource::LlmDeep,
        );
        let branch_name =
            git_ops::generate_fix_branch_name(&suggestion.id.to_string(), &suggestion.summary);

        let result = finalize_harness_result_on_branch(
            &repo_path,
            &source_branch,
            &suggestion,
            &[ImplementationAppliedFile {
                path: PathBuf::from("../evil"),
                summary: "Nope".to_string(),
                content: "bad".to_string(),
            }],
        );
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.status, ImplementationFinalizationStatus::RolledBack);
        assert!(!err.mutation_on_failure);

        // After rollback, we should be back on the source branch with a clean working tree.
        let status = git_ops::current_status(&repo_path).unwrap();
        assert_eq!(status.branch, source_branch);
        assert!(status.staged.is_empty());
        assert!(status.modified.is_empty());

        // Branch should be cleaned up.
        let repo = Repository::open(&repo_path).unwrap();
        assert!(repo
            .find_branch(&branch_name, git2::BranchType::Local)
            .is_err());
    }

    #[test]
    fn finalization_success_stages_only_payload_files() {
        let (_dir, repo_path) = init_temp_git_repo_with_file();
        let source_branch = git_ops::current_status(&repo_path).unwrap().branch;

        let suggestion = crate::suggest::Suggestion::new(
            crate::suggest::SuggestionKind::Improvement,
            crate::suggest::Priority::High,
            PathBuf::from("src/lib.rs"),
            "Improve demo".to_string(),
            crate::suggest::SuggestionSource::LlmDeep,
        );

        let (branch, changes) = finalize_harness_result_on_branch(
            &repo_path,
            &source_branch,
            &suggestion,
            &[ImplementationAppliedFile {
                path: PathBuf::from("src/lib.rs"),
                summary: "Modified: demo".to_string(),
                content: "fn demo() { println!(\"x\"); }\n".to_string(),
            }],
        )
        .unwrap();

        // Should have created/switch to a fix branch and stage only the approved file.
        assert_eq!(git_ops::current_status(&repo_path).unwrap().branch, branch);
        let status = git_ops::current_status(&repo_path).unwrap();
        assert_eq!(status.staged, vec!["src/lib.rs".to_string()]);
        assert!(status.modified.is_empty());
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].0, PathBuf::from("src/lib.rs"));

        let content = std::fs::read_to_string(repo_path.join("src/lib.rs")).unwrap();
        assert!(content.contains("println!"));
    }
}
