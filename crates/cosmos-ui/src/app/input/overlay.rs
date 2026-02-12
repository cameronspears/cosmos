use super::normal::confirm_apply_from_overlay;
use crate::app::background;
use crate::app::messages::BackgroundMessage;
use crate::app::RuntimeContext;
use crate::ui::{App, LoadingState, Overlay};
use anyhow::Result;
use crossterm::event::{KeyCode, KeyEvent};

/// Handle key events when an overlay is active
pub(super) fn handle_overlay_input(
    app: &mut App,
    key: KeyEvent,
    ctx: &RuntimeContext,
) -> Result<()> {
    // Handle overlay mode
    if app.overlay != Overlay::None {
        // Handle Apply Plan overlay
        if let Overlay::ApplyPlan { .. } = &app.overlay {
            match key.code {
                KeyCode::Esc | KeyCode::Char('q') => {
                    app.close_overlay();
                    app.clear_apply_confirm();
                    app.show_toast("Apply canceled.");
                }
                KeyCode::Down => {
                    app.apply_plan_scroll_down();
                }
                KeyCode::Up => {
                    app.apply_plan_scroll_up();
                }
                KeyCode::Char('t') => {
                    app.apply_plan_toggle_technical_details();
                }
                KeyCode::Char('y') | KeyCode::Enter => {
                    app.apply_plan_set_confirm(true);
                    let cache = crate::cache::Cache::new(&app.repo_path);
                    if !cache.has_seen_data_notice() {
                        let _ = cache.mark_data_notice_seen();
                    }
                    confirm_apply_from_overlay(app, ctx);
                }
                _ => {}
            }
            return Ok(());
        }

        // Handle Reset cosmos overlay
        if let Overlay::Reset { .. } = &app.overlay {
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
                    let selected = app.get_reset_selections();
                    if selected.is_empty() {
                        app.show_toast("Select at least one reset option");
                        return Ok(());
                    }

                    app.loading = LoadingState::Resetting;
                    app.close_overlay();

                    let tx_reset = ctx.tx.clone();
                    let repo_path = app.repo_path.clone();
                    background::spawn_background(ctx.tx.clone(), "reset_cosmos", async move {
                        match crate::cache::reset_cosmos(&repo_path, &selected).await {
                            Ok(_) => {
                                let _ = tx_reset
                                    .send(BackgroundMessage::ResetComplete { options: selected });
                            }
                            Err(e) => {
                                let _ = tx_reset.send(BackgroundMessage::Error(e.to_string()));
                            }
                        }
                    });
                }
                _ => {}
            }
            return Ok(());
        }

        // Handle Startup Check overlay
        if let Overlay::StartupCheck {
            confirming_discard, ..
        } = &app.overlay
        {
            let confirming = *confirming_discard;
            match key.code {
                KeyCode::Esc | KeyCode::Char('q') => {
                    app.close_overlay();
                }
                KeyCode::Down => {
                    app.overlay_scroll_down();
                }
                KeyCode::Up => {
                    app.overlay_scroll_up();
                }
                // === Confirmation mode handlers ===
                KeyCode::Char('y') if confirming => {
                    // Confirm discard - actually discard changes
                    app.loading = LoadingState::Discarding;
                    app.close_overlay();
                    let tx = ctx.tx.clone();
                    let repo_path = app.repo_path.clone();
                    background::spawn_background(ctx.tx.clone(), "discard_changes", async move {
                        match crate::git_ops::discard_all_changes(&repo_path) {
                            Ok(()) => {
                                let _ = tx.send(BackgroundMessage::DiscardComplete);
                            }
                            Err(e) => {
                                let _ = tx.send(BackgroundMessage::Error(e.to_string()));
                            }
                        }
                    });
                }
                KeyCode::Enter if confirming => {
                    // Confirm discard via Enter
                    app.loading = LoadingState::Discarding;
                    app.close_overlay();
                    let tx = ctx.tx.clone();
                    let repo_path = app.repo_path.clone();
                    background::spawn_background(ctx.tx.clone(), "discard_changes", async move {
                        match crate::git_ops::discard_all_changes(&repo_path) {
                            Ok(()) => {
                                let _ = tx.send(BackgroundMessage::DiscardComplete);
                            }
                            Err(e) => {
                                let _ = tx.send(BackgroundMessage::Error(e.to_string()));
                            }
                        }
                    });
                }
                KeyCode::Char('n') if confirming => {
                    // Cancel confirmation - go back to main menu
                    app.startup_check_confirm_discard(false);
                }
                KeyCode::Char('c') if confirming => {
                    // Cancel confirmation - go back to main menu
                    app.startup_check_confirm_discard(false);
                }
                // === Initial menu handlers ===
                KeyCode::Char('s') if !confirming => {
                    // Save my work and start fresh (git stash)
                    app.loading = LoadingState::Stashing;
                    app.close_overlay();
                    let tx = ctx.tx.clone();
                    let repo_path = app.repo_path.clone();
                    background::spawn_background(ctx.tx.clone(), "stash_changes", async move {
                        match crate::git_ops::stash_changes(&repo_path) {
                            Ok(message) => {
                                let _ = tx.send(BackgroundMessage::StashComplete { message });
                            }
                            Err(e) => {
                                let _ = tx.send(BackgroundMessage::Error(e.to_string()));
                            }
                        }
                    });
                }
                KeyCode::Char('d') if !confirming => {
                    // Discard and start fresh - show confirmation
                    app.startup_check_confirm_discard(true);
                }
                KeyCode::Char('c') if !confirming => {
                    // Continue as-is - just close the overlay
                    app.close_overlay();
                }
                _ => {}
            }
            return Ok(());
        }

        // Handle Update overlay
        if let Overlay::Update {
            target_version,
            progress,
            error,
            ..
        } = &app.overlay
        {
            let is_downloading = progress.is_some() && error.is_none();
            let has_error = error.is_some();
            let target = target_version.clone();

            match key.code {
                // Decline update: n, Esc, or q
                KeyCode::Char('n') | KeyCode::Esc | KeyCode::Char('q') => {
                    // Can dismiss if not currently downloading
                    if !is_downloading {
                        app.close_overlay();
                    }
                }
                // Accept update: y or Enter
                KeyCode::Char('y') | KeyCode::Enter => {
                    // Start download if not already downloading and no error
                    if !is_downloading && !has_error {
                        // Set initial progress
                        app.set_update_progress(0);
                        app.update_progress = Some(0);

                        let tx_update = ctx.tx.clone();
                        let tx_error = ctx.tx.clone();

                        // Run update in a blocking task since self_update is sync
                        background::spawn_background(ctx.tx.clone(), "run_update", async move {
                            // self_update is blocking, so we run it in spawn_blocking
                            let result = tokio::task::spawn_blocking(move || {
                                crate::update::run_update(&target, move |percent| {
                                    let _ = tx_update
                                        .send(BackgroundMessage::UpdateProgress { percent });
                                })
                            })
                            .await;

                            match result {
                                Ok(Ok(())) => {
                                    // This shouldn't happen since run_update execs
                                    // But if it does (already up to date), we're done
                                }
                                Ok(Err(e)) => {
                                    let _ = tx_error
                                        .send(BackgroundMessage::UpdateError(e.to_string()));
                                }
                                Err(e) => {
                                    let _ = tx_error.send(BackgroundMessage::UpdateError(format!(
                                        "Update task failed: {}",
                                        e
                                    )));
                                }
                            }
                        });
                    } else if has_error {
                        // Retry on error - reset and allow starting again
                        if let Overlay::Update {
                            progress, error, ..
                        } = &mut app.overlay
                        {
                            *progress = None;
                            *error = None;
                        }
                    }
                }
                _ => {}
            }
            return Ok(());
        }

        // Handle Welcome overlay - dismiss with Enter or Esc
        if let Overlay::Welcome = &app.overlay {
            match key.code {
                KeyCode::Enter | KeyCode::Esc | KeyCode::Char('q') => {
                    app.close_overlay();
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

    Ok(())
}
