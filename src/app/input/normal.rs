use crate::app::background;
use crate::app::messages::BackgroundMessage;
use crate::app::RuntimeContext;
use crate::cache;
use crate::git_ops;
use crate::index;
use crate::suggest;
use crate::ui::{ActivePanel, App, LoadingState, Overlay, ShipStep, WorkflowStep};
use crate::util::{hash_bytes, resolve_repo_path_allow_new};
use anyhow::Result;
use crossterm::event::{KeyCode, KeyEvent};
use std::collections::HashMap;
use std::path::PathBuf;

/// Handle key events in normal mode (no special input active)
pub(super) fn handle_normal_mode(
    app: &mut App,
    key: KeyEvent,
    ctx: &RuntimeContext,
) -> Result<()> {
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
                                        let additional_files_for_preview = additional_files.clone();
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
                                                let mem = if repo_memory_context.trim().is_empty() {
                                                    None
                                                } else {
                                                    Some(repo_memory_context)
                                                };
                                                let mut file_hashes = HashMap::new();
                                                let mut primary_content = String::new();
                                                let mut primary_rel = None;

                                                let mut all_files = Vec::new();
                                                all_files.push(file_path.clone());
                                                all_files.extend(additional_files_for_preview.clone());

                                                for target in &all_files {
                                                    let resolved = match resolve_repo_path_allow_new(
                                                        &repo_root,
                                                        target,
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

                                                    let bytes = match std::fs::read(&resolved.absolute)
                                                    {
                                                        Ok(content) => content,
                                                        Err(e)
                                                            if e.kind()
                                                                == std::io::ErrorKind::NotFound =>
                                                        {
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

                                                    if target == &file_path {
                                                        match String::from_utf8(bytes) {
                                                            Ok(content) => {
                                                                primary_content = content;
                                                                primary_rel =
                                                                    Some(resolved.relative.clone());
                                                            }
                                                            Err(_) => {
                                                                let _ = tx_preview.send(
                                                                    BackgroundMessage::PreviewError(
                                                                        format!(
                                                                            "Failed to read {}: file is not valid UTF-8",
                                                                            resolved.relative.display(),
                                                                        ),
                                                                    ),
                                                                );
                                                                return;
                                                            }
                                                        }
                                                    }
                                                }

                                                let resolved_rel =
                                                    primary_rel.unwrap_or_else(|| file_path.clone());
                                                match suggest::llm::generate_fix_preview(
                                                    &resolved_rel,
                                                    &primary_content,
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
                                            if let Ok(status) =
                                                git_ops::current_status(&app.repo_path)
                                            {
                                                let changed_count = status.staged.len()
                                                    + status.modified.len()
                                                    + status.untracked.len();
                                                if changed_count > 0 {
                                                    app.show_toast(
                                                        "Working tree has uncommitted changes. Commit or stash before applying a fix.",
                                                    );
                                                    return Ok(());
                                                }
                                            }

                                            let mut changed_files = Vec::new();
                                            let mut all_files = Vec::new();
                                            all_files.push(fp.clone());
                                            all_files.extend(
                                                app.verify_state.additional_files.clone(),
                                            );
                                            for target in &all_files {
                                                let resolved = match resolve_repo_path_allow_new(
                                                    &app.repo_path,
                                                    target,
                                                ) {
                                                    Ok(resolved) => resolved,
                                                    Err(e) => {
                                                        app.show_toast(&format!(
                                                            "Unsafe path {}: {}",
                                                            target.display(),
                                                            e
                                                        ));
                                                        return Ok(());
                                                    }
                                                };
                                                let bytes = match std::fs::read(&resolved.absolute)
                                                {
                                                    Ok(content) => content,
                                                    Err(e)
                                                        if e.kind()
                                                            == std::io::ErrorKind::NotFound =>
                                                    {
                                                        Vec::new()
                                                    }
                                                    Err(e) => {
                                                        app.show_toast(&format!(
                                                            "Failed to read {}: {}",
                                                            resolved.relative.display(),
                                                            e
                                                        ));
                                                        return Ok(());
                                                    }
                                                };
                                                let current_hash = hash_bytes(&bytes);
                                                match app
                                                    .verify_state
                                                    .preview_hashes
                                                    .get(&resolved.relative)
                                                {
                                                    Some(expected)
                                                        if expected == &current_hash => {}
                                                    _ => changed_files.push(
                                                        resolved.relative.clone(),
                                                    ),
                                                }
                                            }
                                            if !changed_files.is_empty() {
                                                let names: Vec<String> = changed_files
                                                    .iter()
                                                    .take(3)
                                                    .map(|p| p.display().to_string())
                                                    .collect();
                                                let more = changed_files.len().saturating_sub(3);
                                                let suffix = if more > 0 {
                                                    format!(" (and {} more)", more)
                                                } else {
                                                    String::new()
                                                };
                                                app.show_toast(&format!(
                                                    "Files changed since preview: {}{}",
                                                    names.join(", "),
                                                    suffix
                                                ));
                                                return Ok(());
                                            }

                                            app.loading = LoadingState::GeneratingFix;

                                            background::spawn_background(
                                                ctx.tx.clone(),
                                                "apply_fix",
                                                async move {
                                                    // Create branch from main
                                                    let branch_name =
                                                        git_ops::generate_fix_branch_name(
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
                                                        let mut file_inputs: Vec<suggest::llm::FileInput> =
                                                            Vec::new();
                                                        for file_path in &all_files {
                                                            let resolved =
                                                                match resolve_repo_path_allow_new(
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
                                                                // Backup all files first
                                                                let mut backups: Vec<(
                                                                    PathBuf,
                                                                    PathBuf,
                                                                    PathBuf,
                                                                    bool,
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
                                                                    let backup_path = full_path
                                                                        .with_extension("cosmos.bak");
                                                                    if let Some(parent) = full_path.parent() {
                                                                        let _ = std::fs::create_dir_all(parent);
                                                                    }
                                                                    let was_new_file = file_inputs
                                                                        .iter()
                                                                        .find(|f| f.path == resolved.relative)
                                                                        .map(|f| f.is_new)
                                                                        .unwrap_or(false);
                                                                    let backup_result = if was_new_file {
                                                                        std::fs::write(&backup_path, "")
                                                                    } else {
                                                                        std::fs::copy(&full_path, &backup_path)
                                                                            .map(|_| ())
                                                                    };
                                                                    if let Err(e) = backup_result {
                                                                        // Rollback any backups we made
                                                                        for (_, bp, _, _) in &backups {
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
                                                                        was_new_file,
                                                                    ));
                                                                }

                                                                // Apply all edits
                                                                let mut file_changes: Vec<(
                                                                    PathBuf,
                                                                    PathBuf,
                                                                    String,
                                                                    bool,
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
                                                                    let backup_path = full_path
                                                                        .with_extension("cosmos.bak");
                                                                    let was_new_file = backups
                                                                        .iter()
                                                                        .find(|(path, _, _, _)| path == &resolved.relative)
                                                                        .map(|(_, _, _, is_new)| *is_new)
                                                                        .unwrap_or(false);

                                                                    if let Some(parent) = full_path.parent() {
                                                                        let _ = std::fs::create_dir_all(parent);
                                                                    }
                                                                    match std::fs::write(
                                                                        &full_path,
                                                                        &file_edit.new_content,
                                                                    ) {
                                                                        Ok(_) => {
                                                                            // Stage the file
                                                                            let rel_path =
                                                                                resolved
                                                                                    .relative
                                                                                    .to_string_lossy()
                                                                                    .to_string();
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
                                                                                was_new_file,
                                                                            ));
                                                                        }
                                                                        Err(e) => {
                                                                            // Rollback all changes
                                                                            for (_path, full, backup, is_new) in &backups {
                                                                                if *is_new {
                                                                                    let _ = std::fs::remove_file(full);
                                                                                } else {
                                                                                    let _ =
                                                                                        std::fs::copy(
                                                                                            backup,
                                                                                            full,
                                                                                        );
                                                                                }
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
                                                            match resolve_repo_path_allow_new(&repo_path, &fp) {
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
                                                        let content =
                                                            match std::fs::read_to_string(&full_path) {
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
                                                                let backup_path = full_path
                                                                    .with_extension("cosmos.bak");
                                                                if let Some(parent) = full_path.parent() {
                                                                    let _ = std::fs::create_dir_all(parent);
                                                                }
                                                                let backup_result = if is_new_file {
                                                                    std::fs::write(&backup_path, "")
                                                                } else {
                                                                    std::fs::copy(&full_path, &backup_path)
                                                                        .map(|_| ())
                                                                };
                                                                if let Err(e) = backup_result {
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

                                                                if let Some(parent) = full_path.parent() {
                                                                    let _ = std::fs::create_dir_all(parent);
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
                                                                                file_changes: vec![(
                                                                                    rel_path.into(),
                                                                                    backup_path,
                                                                                    diff,
                                                                                    is_new_file,
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
                                                                        if is_new_file {
                                                                            let _ = std::fs::remove_file(&full_path);
                                                                        } else {
                                                                            let _ = std::fs::copy(
                                                                                &backup_path,
                                                                                &full_path,
                                                                            );
                                                                        }
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
                                                },
                                            );
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
                                                                BackgroundMessage::Error(e.to_string()),
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
                                        let commit_message =
                                            app.ship_state.commit_message.clone();
                                        let (pr_title, pr_body) = app.generate_pr_content();
                                        let tx_ship = ctx.tx.clone();

                                        app.set_ship_step(ShipStep::Committing);

                                        background::spawn_background(
                                            ctx.tx.clone(),
                                            "ship_confirm",
                                            async move {
                                                // Execute ship workflow
                                                let _ = tx_ship.send(BackgroundMessage::ShipProgress(
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

                                                let _ = tx_ship.send(BackgroundMessage::ShipProgress(
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

                                                let _ = tx_ship.send(BackgroundMessage::ShipProgress(
                                                    ShipStep::CreatingPR,
                                                ));

                                                // Create PR with human-friendly content
                                                match git_ops::create_pr(
                                                    &repo_path,
                                                    &pr_title,
                                                    &pr_body,
                                                ) {
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
