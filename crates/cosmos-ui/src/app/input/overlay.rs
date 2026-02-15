use super::normal::confirm_apply_from_overlay;
use crate::app::background;
use crate::app::messages::BackgroundMessage;
use crate::app::RuntimeContext;
use crate::ui::{App, LoadingState, Overlay, StartupAction, StartupMode};
use anyhow::Result;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

const OPENROUTER_KEYS_URL: &str = "https://openrouter.ai/settings/keys";
const OPENROUTER_CREDITS_URL: &str = "https://openrouter.ai/settings/credits";

fn normalize_api_key_input(raw: &str) -> String {
    let trimmed = raw.trim();
    let unquoted = trimmed.trim_matches(|c| c == '"' || c == '\'' || c == '`');
    unquoted.chars().filter(|c| !c.is_whitespace()).collect()
}

fn open_openrouter_link(app: &mut App, url: &str, label: &str) {
    match cosmos_adapters::git_ops::open_url(url) {
        Ok(()) => app.show_toast(&format!(
            "Opened OpenRouter {} page in your browser.",
            label
        )),
        Err(_) => app.show_toast(&format!(
            "Couldn't open browser. Visit {} to open OpenRouter {} page.",
            url, label
        )),
    }
}

fn has_control_or_command(modifiers: KeyModifiers) -> bool {
    modifiers.contains(KeyModifiers::CONTROL) || modifiers.contains(KeyModifiers::SUPER)
}

fn execute_startup_stash(app: &mut App, ctx: &RuntimeContext) {
    app.loading = LoadingState::Stashing;
    app.close_overlay();
    let tx = ctx.tx.clone();
    let repo_path = app.repo_path.clone();
    background::spawn_background(ctx.tx.clone(), "stash_changes", async move {
        match cosmos_adapters::git_ops::stash_changes(&repo_path) {
            Ok(message) => {
                let _ = tx.send(BackgroundMessage::StashComplete { message });
            }
            Err(e) => {
                let _ = tx.send(BackgroundMessage::Error(e.to_string()));
            }
        }
    });
}

fn execute_startup_discard(app: &mut App, ctx: &RuntimeContext) {
    app.loading = LoadingState::Discarding;
    app.close_overlay();
    let tx = ctx.tx.clone();
    let repo_path = app.repo_path.clone();
    background::spawn_background(ctx.tx.clone(), "discard_changes", async move {
        match cosmos_adapters::git_ops::discard_all_changes(&repo_path) {
            Ok(()) => {
                let _ = tx.send(BackgroundMessage::DiscardComplete);
            }
            Err(e) => {
                let _ = tx.send(BackgroundMessage::Error(e.to_string()));
            }
        }
    });
}

fn execute_startup_switch_to_main(app: &mut App, ctx: &RuntimeContext, main_branch: String) {
    app.loading = LoadingState::SwitchingBranch;
    app.close_overlay();
    let tx = ctx.tx.clone();
    let repo_path = app.repo_path.clone();
    background::spawn_background(ctx.tx.clone(), "switch_to_main_branch", async move {
        match cosmos_adapters::git_ops::checkout_branch(&repo_path, &main_branch) {
            Ok(()) => {
                let _ = tx.send(BackgroundMessage::StartupSwitchedToMain {
                    branch: main_branch,
                });
            }
            Err(e) => {
                let _ = tx.send(BackgroundMessage::Error(e.to_string()));
            }
        }
    });
}

/// Handle key events when an overlay is active
pub(super) fn handle_overlay_input(
    app: &mut App,
    key: KeyEvent,
    ctx: &RuntimeContext,
) -> Result<()> {
    // Handle overlay mode
    if app.overlay != Overlay::None {
        // Handle API key setup overlay
        if let Overlay::ApiKeySetup { .. } = &app.overlay {
            match key.code {
                KeyCode::Esc | KeyCode::Char('q') => {
                    app.close_overlay();
                    app.show_toast("API key setup canceled.");
                }
                KeyCode::Char('k') if has_control_or_command(key.modifiers) => {
                    open_openrouter_link(app, OPENROUTER_KEYS_URL, "keys");
                }
                KeyCode::Char('b') if has_control_or_command(key.modifiers) => {
                    open_openrouter_link(app, OPENROUTER_CREDITS_URL, "credits");
                }
                KeyCode::Backspace => {
                    if let Overlay::ApiKeySetup {
                        input,
                        error,
                        save_armed,
                    } = &mut app.overlay
                    {
                        input.pop();
                        *error = None;
                        *save_armed = false;
                    }
                }
                KeyCode::Char(c) if !c.is_control() => {
                    if let Overlay::ApiKeySetup {
                        input,
                        error,
                        save_armed,
                    } = &mut app.overlay
                    {
                        input.push(c);
                        *error = None;
                        *save_armed = false;
                    }
                }
                KeyCode::Enter => {
                    let (candidate, save_armed) = match &app.overlay {
                        Overlay::ApiKeySetup {
                            input, save_armed, ..
                        } => (normalize_api_key_input(input), *save_armed),
                        _ => (String::new(), false),
                    };

                    if candidate.is_empty() {
                        if let Overlay::ApiKeySetup { error, .. } = &mut app.overlay {
                            *error = Some("Paste an OpenRouter API key to continue.".to_string());
                        }
                        return Ok(());
                    }

                    if !save_armed
                        && !cosmos_adapters::config::Config::validate_api_key_format(&candidate)
                    {
                        if let Overlay::ApiKeySetup {
                            error, save_armed, ..
                        } = &mut app.overlay
                        {
                            *save_armed = true;
                            *error = Some(
                                "This key does not start with sk-. Press Enter again to save anyway, or keep editing."
                                    .to_string(),
                            );
                        }
                        return Ok(());
                    }

                    let mut cfg = cosmos_adapters::config::Config::load();
                    match cfg.set_api_key(&candidate) {
                        Ok(()) => {
                            app.close_overlay();
                            crate::app::background::spawn_balance_refresh(ctx.tx.clone());
                            let refreshed = crate::app::background::request_suggestions_refresh(
                                app,
                                ctx.tx.clone(),
                                ctx.repo_path.clone(),
                                "API key saved",
                            );
                            if !refreshed {
                                app.show_toast(
                                    "API key saved. Press r in Suggestions to refresh analysis.",
                                );
                            }
                        }
                        Err(e) => {
                            if let Overlay::ApiKeySetup {
                                error, save_armed, ..
                            } = &mut app.overlay
                            {
                                *error = Some(e);
                                *save_armed = false;
                            }
                        }
                    }
                }
                _ => {}
            }
            return Ok(());
        }

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
                    let cache = cosmos_adapters::cache::Cache::new(&app.repo_path);
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
                        match cosmos_adapters::cache::reset_cosmos(&repo_path, &selected).await {
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
            changed_count,
            current_branch,
            main_branch,
            mode,
            ..
        } = &app.overlay
        {
            let startup_mode = *mode;
            let available_actions =
                App::startup_actions_for_context(*changed_count, current_branch, main_branch);
            let main_branch_name = main_branch.clone();

            match startup_mode {
                StartupMode::ConfirmDiscard => match key.code {
                    KeyCode::Char('y') | KeyCode::Enter => execute_startup_discard(app, ctx),
                    KeyCode::Char('n') | KeyCode::Esc | KeyCode::Char('q') => {
                        app.startup_check_set_mode(StartupMode::Choose);
                    }
                    _ => {}
                },
                StartupMode::Choose => match key.code {
                    KeyCode::Esc | KeyCode::Char('q') => app.close_overlay(),
                    KeyCode::Down => app.startup_check_navigate(1),
                    KeyCode::Up => app.startup_check_navigate(-1),
                    KeyCode::Enter => match app.startup_check_selected_action() {
                        Some(StartupAction::SaveStartFresh) => execute_startup_stash(app, ctx),
                        Some(StartupAction::DiscardStartFresh) => {
                            app.startup_check_set_mode(StartupMode::ConfirmDiscard)
                        }
                        Some(StartupAction::SwitchToMain) => {
                            execute_startup_switch_to_main(app, ctx, main_branch_name)
                        }
                        Some(StartupAction::ContinueAsIs) | None => app.close_overlay(),
                    },
                    KeyCode::Char('s')
                        if available_actions.contains(&StartupAction::SaveStartFresh) =>
                    {
                        execute_startup_stash(app, ctx);
                    }
                    KeyCode::Char('d')
                        if available_actions.contains(&StartupAction::DiscardStartFresh) =>
                    {
                        app.startup_check_set_mode(StartupMode::ConfirmDiscard);
                    }
                    KeyCode::Char('m')
                        if available_actions.contains(&StartupAction::SwitchToMain) =>
                    {
                        execute_startup_switch_to_main(app, ctx, main_branch_name);
                    }
                    KeyCode::Char('c')
                        if available_actions.contains(&StartupAction::ContinueAsIs) =>
                    {
                        app.close_overlay();
                    }
                    _ => {}
                },
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
                                cosmos_adapters::update::run_update(&target, move |percent| {
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

#[cfg(test)]
mod tests {
    use super::normalize_api_key_input;

    #[test]
    fn normalize_api_key_input_trims_quotes_and_whitespace() {
        let input = "  \" sk-or-v1-abcd1234 \n\"  ";
        assert_eq!(normalize_api_key_input(input), "sk-or-v1-abcd1234");
    }

    #[test]
    fn normalize_api_key_input_leaves_clean_key_unchanged() {
        let input = "sk-or-v1-clean-key";
        assert_eq!(normalize_api_key_input(input), "sk-or-v1-clean-key");
    }
}
