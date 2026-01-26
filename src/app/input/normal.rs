use crate::app::background;
use crate::app::messages::BackgroundMessage;
use crate::app::RuntimeContext;
use crate::git_ops;
use crate::suggest;
use crate::suggest::llm::FixPreview;
use crate::suggest::Suggestion;
use crate::ui::{ActivePanel, App, LoadingState, Overlay, ShipStep, WorkflowStep};
use crate::util::{hash_bytes, resolve_repo_path_allow_new};
use anyhow::Result;
use crossterm::event::{KeyCode, KeyEvent};
use std::collections::HashMap;
use std::path::PathBuf;
use uuid::Uuid;

// =============================================================================
// Apply Fix Validation (WorkflowStep::Verify Enter key handling)
// =============================================================================

/// Errors that can occur when validating the apply fix action.
/// Each variant has a user-friendly message.
#[derive(Debug, Clone)]
pub enum ApplyError {
    /// Preview is still loading or hasn't been generated yet
    PreviewNotReady,
    /// Fix is already being applied
    AlreadyApplying,
    /// Internal state is missing (should never happen in normal use)
    MissingState(&'static str),
    /// The suggestion was removed from the list (rare edge case)
    SuggestionNotFound,
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
            Self::PreviewNotReady => "Preview not ready. Please wait for verification.".into(),
            Self::AlreadyApplying => "Already applying fix...".into(),
            Self::MissingState(what) => format!("Internal error: missing {}. Try again.", what),
            Self::SuggestionNotFound => "Suggestion no longer exists. Select another.".into(),
            Self::GitStatusFailed(e) => format!("Git error: {}. Check repo state.", e),
            Self::DirtyWorkingTree => {
                "Working tree has changes. Commit or stash before applying.".into()
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
                    "Files changed: {}{}. Re-verify first.",
                    names.join(", "),
                    suffix
                )
            }
            Self::UnsafePath(path, e) => format!("Unsafe path {}: {}", path.display(), e),
            Self::FileReadFailed(path, e) => format!("Failed to read {}: {}", path.display(), e),
        }
    }
}

/// Context needed to apply a fix, validated and ready to use
pub struct ApplyContext {
    pub preview: FixPreview,
    pub suggestion: Suggestion,
    pub suggestion_id: Uuid,
    pub file_path: PathBuf,
    pub repo_path: PathBuf,
    pub repo_memory_context: String,
}

/// Validates all preconditions for applying a fix from the Verify step.
/// Returns an ApplyContext if all conditions are met, or an ApplyError describing what failed.
fn validate_apply_fix(app: &App) -> std::result::Result<ApplyContext, ApplyError> {
    // Guard 1: Check if already loading/applying (align with footer UI)
    if app.loading == LoadingState::GeneratingFix {
        return Err(ApplyError::AlreadyApplying);
    }

    // Guard 2: Check if preview is still loading
    if app.verify_state.loading {
        return Err(ApplyError::PreviewNotReady);
    }

    // Get preview
    let preview = app
        .verify_state
        .preview
        .clone()
        .ok_or(ApplyError::PreviewNotReady)?;

    // Get suggestion_id
    let suggestion_id = app
        .verify_state
        .suggestion_id
        .ok_or(ApplyError::MissingState("suggestion_id"))?;

    // Get file_path
    let file_path = app
        .verify_state
        .file_path
        .clone()
        .ok_or(ApplyError::MissingState("file_path"))?;

    // Get additional files
    let additional_files = app.verify_state.additional_files.clone();

    // Find suggestion in list
    let suggestion = app
        .suggestions
        .suggestions
        .iter()
        .find(|s| s.id == suggestion_id)
        .cloned()
        .ok_or(ApplyError::SuggestionNotFound)?;

    // Check git status
    let status = git_ops::current_status(&app.repo_path)
        .map_err(|e| ApplyError::GitStatusFailed(e.to_string()))?;

    let changed_count = status.staged.len() + status.modified.len() + status.untracked.len();
    if changed_count > 0 {
        return Err(ApplyError::DirtyWorkingTree);
    }

    // Validate file hashes haven't changed since preview
    let mut all_files = vec![file_path.clone()];
    all_files.extend(additional_files.clone());

    let mut changed_files = Vec::new();
    for target in &all_files {
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

        let current_hash = hash_bytes(&bytes);
        match app.verify_state.preview_hashes.get(&resolved.relative) {
            Some(expected) if expected == &current_hash => {}
            _ => changed_files.push(resolved.relative.clone()),
        }
    }

    if !changed_files.is_empty() {
        return Err(ApplyError::FilesChanged(changed_files));
    }

    // All validations passed
    Ok(ApplyContext {
        preview,
        suggestion,
        suggestion_id,
        file_path,
        repo_path: app.repo_path.clone(),
        repo_memory_context: app.repo_memory.to_prompt_context(12, 900),
    })
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
                                        let suggestion_id = suggestion.id;
                                        let file_path = suggestion.file.clone();
                                        let additional_files = suggestion.additional_files.clone();

                                        // Check if we have a valid cached preview for this suggestion
                                        if app.has_valid_cached_preview(
                                            suggestion_id,
                                            &file_path,
                                            &additional_files,
                                            &app.repo_path,
                                        ) {
                                            // Use cached result - instant transition
                                            app.use_cached_verify();
                                        } else {
                                            // Generate new preview
                                            let additional_files_for_preview =
                                                additional_files.clone();
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

                                            background::spawn_background(
                                                ctx.tx.clone(),
                                                "preview_generation",
                                                async move {
                                                    let mem =
                                                        if repo_memory_context.trim().is_empty() {
                                                            None
                                                        } else {
                                                            Some(repo_memory_context)
                                                        };

                                                    // Build file hashes for change detection
                                                    let mut file_hashes = HashMap::new();
                                                    let mut all_files = Vec::new();
                                                    all_files.push(file_path.clone());
                                                    all_files.extend(
                                                        additional_files_for_preview.clone(),
                                                    );

                                                    for target in &all_files {
                                                        let resolved =
                                                            match resolve_repo_path_allow_new(
                                                                &repo_root, target,
                                                            ) {
                                                                Ok(resolved) => resolved,
                                                                Err(e) => {
                                                                    let _ = tx_preview.send(
                                                                BackgroundMessage::PreviewError(
                                                                    format!(
                                                                        "Unsafe path {}: {}",
                                                                        target.display(),
                                                                        e
                                                                    ),
                                                                ),
                                                            );
                                                                    return;
                                                                }
                                                            };

                                                        let bytes = match std::fs::read(&resolved.absolute) {
                                                        Ok(content) => content,
                                                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                                                            Vec::new()
                                                        }
                                                        Err(e) => {
                                                            let _ = tx_preview.send(
                                                                BackgroundMessage::PreviewError(
                                                                    format!(
                                                                        "Failed to read {}: {}",
                                                                        resolved.relative.display(),
                                                                        e
                                                                    ),
                                                                ),
                                                            );
                                                            return;
                                                        }
                                                    };

                                                        file_hashes.insert(
                                                            resolved.relative.clone(),
                                                            hash_bytes(&bytes),
                                                        );
                                                    }
                                                    // Use agentic verification - model explores with shell
                                                    match suggest::llm::generate_fix_preview_agentic(
                                                    &repo_root,
                                                    &suggestion_clone,
                                                    None,
                                                    mem,
                                                )
                                                .await
                                                {
                                                    Ok(preview) => {
                                                        let _ = tx_preview.send(
                                                            BackgroundMessage::PreviewReady {
                                                                preview,
                                                                file_hashes,
                                                            },
                                                        );
                                                    }
                                                    Err(e) => {
                                                        let _ = tx_preview.send(
                                                            BackgroundMessage::PreviewError(
                                                                e.to_string(),
                                                            ),
                                                        );
                                                    }
                                                }
                                                },
                                            );
                                        }
                                    }
                                }
                            }
                            WorkflowStep::Verify => {
                                // Apply the fix and move to Review
                                // Use validate_apply_fix to check all preconditions
                                match validate_apply_fix(app) {
                                    Ok(apply_ctx) => {
                                        // All validations passed - start applying
                                        app.loading = LoadingState::GeneratingFix;

                                        let tx_apply = ctx.tx.clone();
                                        let repo_path = apply_ctx.repo_path;
                                        let preview = apply_ctx.preview;
                                        let suggestion = apply_ctx.suggestion;
                                        let sid = apply_ctx.suggestion_id;
                                        let fp = apply_ctx.file_path;
                                        let repo_memory_context = apply_ctx.repo_memory_context;

                                        background::spawn_background(
                                            ctx.tx.clone(),
                                            "apply_fix",
                                            async move {
                                                // Create branch from main
                                                let branch_name = git_ops::generate_fix_branch_name(
                                                    &suggestion.id.to_string(),
                                                    &suggestion.summary,
                                                );

                                                let created_branch =
                                                    match git_ops::create_fix_branch_from_main(
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
                                                    let mut file_inputs: Vec<
                                                        suggest::llm::FileInput,
                                                    > = Vec::new();
                                                    for file_path in &all_files {
                                                        let resolved =
                                                            match resolve_repo_path_allow_new(
                                                                &repo_path, file_path,
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
                                                        let is_new = !resolved.absolute.exists();
                                                        let content = match std::fs::read_to_string(
                                                                &resolved.absolute,
                                                            ) {
                                                                Ok(content) => content,
                                                                Err(e)
                                                                    if e.kind()
                                                                        == std::io::ErrorKind::NotFound =>
                                                                {
                                                                    String::new()
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
                                                            };
                                                        file_inputs.push(suggest::llm::FileInput {
                                                            path: resolved.relative,
                                                            content,
                                                            is_new,
                                                        });
                                                    }

                                                    // Generate multi-file fix
                                                    match suggest::llm::generate_multi_file_fix(
                                                        &file_inputs,
                                                        &suggestion,
                                                        &preview,
                                                        mem,
                                                    )
                                                    .await
                                                    {
                                                        Ok(multi_fix) => {
                                                            // Apply all edits
                                                            let mut file_changes: Vec<(
                                                                PathBuf,
                                                                String,
                                                            )> = Vec::new();
                                                            for file_edit in &multi_fix.file_edits {
                                                                let resolved = match resolve_repo_path_allow_new(
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

                                                                if let Some(parent) =
                                                                    full_path.parent()
                                                                {
                                                                    let _ = std::fs::create_dir_all(
                                                                        parent,
                                                                    );
                                                                }
                                                                match std::fs::write(
                                                                    &full_path,
                                                                    &file_edit.new_content,
                                                                ) {
                                                                    Ok(_) => {
                                                                        // Stage the file
                                                                        let rel_path = resolved
                                                                            .relative
                                                                            .to_string_lossy()
                                                                            .to_string();
                                                                        let _ = git_ops::stage_file(
                                                                            &repo_path, &rel_path,
                                                                        );

                                                                        let diff = format!(
                                                                            "Modified: {}",
                                                                            file_edit
                                                                                .modified_areas
                                                                                .join(", ")
                                                                        );
                                                                        file_changes.push((
                                                                            resolved.relative,
                                                                            diff,
                                                                        ));
                                                                    }
                                                                    Err(e) => {
                                                                        // Rollback via git restore
                                                                        for (path, _) in
                                                                            &file_changes
                                                                        {
                                                                            let _ = git_ops::restore_file(&repo_path, path);
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
                                                    let resolved = match resolve_repo_path_allow_new(
                                                        &repo_path, &fp,
                                                    ) {
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
                                                    let is_new_file = !full_path.exists();
                                                    let content = match std::fs::read_to_string(
                                                        &full_path,
                                                    ) {
                                                        Ok(c) => c,
                                                        Err(e)
                                                            if e.kind()
                                                                == std::io::ErrorKind::NotFound =>
                                                        {
                                                            String::new()
                                                        }
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
                                                        is_new_file,
                                                    )
                                                    .await
                                                    {
                                                        Ok(applied_fix) => {
                                                            if let Some(parent) = full_path.parent()
                                                            {
                                                                let _ =
                                                                    std::fs::create_dir_all(parent);
                                                            }
                                                            match std::fs::write(
                                                                &full_path,
                                                                &applied_fix.new_content,
                                                            ) {
                                                                Ok(_) => {
                                                                    let rel_path_str = rel_path
                                                                        .to_string_lossy()
                                                                        .to_string();
                                                                    let _ = git_ops::stage_file(
                                                                        &repo_path,
                                                                        &rel_path_str,
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
                                                                                file_changes: vec![(
                                                                                    rel_path, diff,
                                                                                )],
                                                                                description: applied_fix.description,
                                                                                usage: applied_fix.usage,
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
                                                                    // Rollback via git restore
                                                                    let _ = git_ops::restore_file(
                                                                        &repo_path, &rel_path,
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
                                            },
                                        );
                                    }
                                    Err(e) => {
                                        // Show user-friendly error message
                                        app.show_toast(&e.user_message());
                                    }
                                }
                            }
                            WorkflowStep::Review => {
                                if !app.review_state.reviewing && !app.review_state.fixing {
                                    if !app.review_state.selected.is_empty() {
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

                                            background::spawn_background(
                                                ctx.tx.clone(),
                                                "verification_fix",
                                                async move {
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
                                                                BackgroundMessage::Error(
                                                                    e.to_string(),
                                                                ),
                                                            );
                                                        }
                                                    }
                                                },
                                            );
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

    // ========================================================================
    // ApplyError User Message Tests
    // ========================================================================

    #[test]
    fn test_apply_error_preview_not_ready() {
        let err = ApplyError::PreviewNotReady;
        let msg = err.user_message();
        assert!(msg.contains("Preview not ready"));
        assert!(msg.contains("wait"));
    }

    #[test]
    fn test_apply_error_already_applying() {
        let err = ApplyError::AlreadyApplying;
        let msg = err.user_message();
        assert!(msg.contains("Already applying"));
    }

    #[test]
    fn test_apply_error_missing_state() {
        let err = ApplyError::MissingState("suggestion_id");
        let msg = err.user_message();
        assert!(msg.contains("Internal error"));
        assert!(msg.contains("suggestion_id"));
    }

    #[test]
    fn test_apply_error_suggestion_not_found() {
        let err = ApplyError::SuggestionNotFound;
        let msg = err.user_message();
        assert!(msg.contains("no longer exists"));
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
        assert!(msg.contains("Commit") || msg.contains("stash"));
    }

    #[test]
    fn test_apply_error_files_changed_single() {
        let err = ApplyError::FilesChanged(vec![PathBuf::from("src/main.rs")]);
        let msg = err.user_message();
        assert!(msg.contains("Files changed"));
        assert!(msg.contains("src/main.rs"));
        assert!(msg.contains("Re-verify"));
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
    }

    #[test]
    fn test_apply_error_unsafe_path() {
        let err = ApplyError::UnsafePath(PathBuf::from("../evil"), "path traversal".into());
        let msg = err.user_message();
        assert!(msg.contains("Unsafe path"));
        assert!(msg.contains("../evil"));
    }

    #[test]
    fn test_apply_error_file_read_failed() {
        let err = ApplyError::FileReadFailed(PathBuf::from("missing.rs"), "not found".into());
        let msg = err.user_message();
        assert!(msg.contains("Failed to read"));
        assert!(msg.contains("missing.rs"));
    }

    // ========================================================================
    // ApplyError Debug Trait Tests
    // ========================================================================

    #[test]
    fn test_apply_error_is_debug() {
        // Ensure Debug trait is implemented for logging
        let err = ApplyError::PreviewNotReady;
        let debug_str = format!("{:?}", err);
        assert!(debug_str.contains("PreviewNotReady"));
    }

    #[test]
    fn test_apply_error_is_clone() {
        // Ensure Clone trait is implemented
        let err = ApplyError::DirtyWorkingTree;
        let cloned = err.clone();
        assert_eq!(err.user_message(), cloned.user_message());
    }
}
