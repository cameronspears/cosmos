use super::normal::confirm_apply_from_overlay;
use crate::app::background;
use crate::app::messages::BackgroundMessage;
use crate::app::RuntimeContext;
use crate::ui::{App, LoadingState, Overlay, StartupAction, StartupMode};
use anyhow::Result;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

const GROQ_KEYS_URL: &str = "https://console.groq.com/keys";

fn normalize_api_key_input(raw: &str) -> String {
    let trimmed = raw.trim();
    let unquoted = trimmed.trim_matches(|c| c == '"' || c == '\'' || c == '`');
    unquoted.chars().filter(|c| !c.is_whitespace()).collect()
}

fn open_provider_link(app: &mut App, url: &str, label: &str) {
    match cosmos_adapters::git_ops::open_url(url) {
        Ok(()) => {}
        Err(_) => {
            let message = format!(
                "Couldn't open browser. Visit {} to open Groq {} page.",
                url, label
            );
            if let Overlay::ApiKeySetup { error, .. } = &mut app.overlay {
                *error = Some(message);
            } else {
                app.open_alert("Couldn't open browser", message);
            }
        }
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
fn handle_alert_overlay_input(app: &mut App, key: &KeyEvent) {
    match key.code {
        KeyCode::Enter | KeyCode::Esc | KeyCode::Char('q') => app.close_overlay(),
        KeyCode::Down | KeyCode::Char('j') => app.overlay_scroll_down(),
        KeyCode::Up | KeyCode::Char('k') => app.overlay_scroll_up(),
        KeyCode::PageDown => {
            for _ in 0..6 {
                app.overlay_scroll_down();
            }
        }
        KeyCode::PageUp => {
            for _ in 0..6 {
                app.overlay_scroll_up();
            }
        }
        _ => {}
    }
}

fn handle_api_key_overlay_input(app: &mut App, key: &KeyEvent, ctx: &RuntimeContext) {
    match key.code {
        KeyCode::Esc | KeyCode::Char('q') => {
            app.close_overlay();
        }
        KeyCode::Char('k') if has_control_or_command(key.modifiers) => {
            open_provider_link(app, GROQ_KEYS_URL, "keys");
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
                    *error = Some("Paste a Groq API key to continue.".to_string());
                }
                return;
            }

            if !save_armed && !cosmos_adapters::config::Config::validate_api_key_format(&candidate)
            {
                if let Overlay::ApiKeySetup {
                    error, save_armed, ..
                } = &mut app.overlay
                {
                    *save_armed = true;
                    *error = Some(
                        "This key does not start with gsk_ or sk-. Press Enter again to save anyway, or keep editing."
                            .to_string(),
                    );
                }
                return;
            }

            let mut cfg = cosmos_adapters::config::Config::load();
            match cfg.set_api_key(&candidate) {
                Ok(()) => {
                    app.close_overlay();
                    let _ = crate::app::background::request_suggestions_refresh(
                        app,
                        ctx.tx.clone(),
                        ctx.repo_path.clone(),
                        "API key saved",
                    );
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
}

fn handle_apply_plan_overlay_input(app: &mut App, key: &KeyEvent, ctx: &RuntimeContext) {
    match key.code {
        KeyCode::Esc | KeyCode::Char('q') => {
            app.close_overlay();
            app.clear_apply_confirm();
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
}

fn handle_reset_overlay_input(app: &mut App, key: &KeyEvent, ctx: &RuntimeContext) {
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
                app.set_reset_overlay_error("Select at least one reset option".to_string());
                return;
            }

            app.loading = LoadingState::Resetting;
            app.close_overlay();

            let tx_reset = ctx.tx.clone();
            let repo_path = app.repo_path.clone();
            background::spawn_background(ctx.tx.clone(), "reset_cosmos", async move {
                match cosmos_adapters::cache::reset_cosmos(&repo_path, &selected).await {
                    Ok(_) => {
                        let _ =
                            tx_reset.send(BackgroundMessage::ResetComplete { options: selected });
                    }
                    Err(e) => {
                        let _ = tx_reset.send(BackgroundMessage::Error(e.to_string()));
                    }
                }
            });
        }
        _ => {}
    }
}

fn handle_startup_check_overlay_input(app: &mut App, key: &KeyEvent, ctx: &RuntimeContext) {
    let (
        changed_count,
        current_branch,
        main_branch,
        startup_mode,
        available_actions,
        main_branch_name,
    ) = match &app.overlay {
        Overlay::StartupCheck {
            changed_count,
            current_branch,
            main_branch,
            mode,
            ..
        } => (
            *changed_count,
            current_branch.clone(),
            main_branch.clone(),
            *mode,
            App::startup_actions_for_context(*changed_count, current_branch, main_branch),
            main_branch.clone(),
        ),
        _ => return,
    };

    let _ = (changed_count, current_branch, main_branch);
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
            KeyCode::Char('s') if available_actions.contains(&StartupAction::SaveStartFresh) => {
                execute_startup_stash(app, ctx);
            }
            KeyCode::Char('d') if available_actions.contains(&StartupAction::DiscardStartFresh) => {
                app.startup_check_set_mode(StartupMode::ConfirmDiscard);
            }
            KeyCode::Char('m') if available_actions.contains(&StartupAction::SwitchToMain) => {
                execute_startup_switch_to_main(app, ctx, main_branch_name);
            }
            KeyCode::Char('c') if available_actions.contains(&StartupAction::ContinueAsIs) => {
                app.close_overlay();
            }
            _ => {}
        },
    }
}

fn handle_update_overlay_input(
    app: &mut App,
    key: &KeyEvent,
    ctx: &RuntimeContext,
    target_version: String,
    progress: Option<u8>,
    has_error: bool,
) {
    let is_downloading = progress.is_some() && !has_error;
    match key.code {
        KeyCode::Char('n') | KeyCode::Esc | KeyCode::Char('q') => {
            if !is_downloading {
                app.close_overlay();
            }
        }
        KeyCode::Char('y') | KeyCode::Enter => {
            if is_downloading {
                return;
            }
            if has_error {
                if let Overlay::Update {
                    progress, error, ..
                } = &mut app.overlay
                {
                    *progress = None;
                    *error = None;
                }
            }
            app.set_update_progress(0);
            app.update_progress = Some(0);

            let tx_update = ctx.tx.clone();
            let tx_error = ctx.tx.clone();
            background::spawn_background(ctx.tx.clone(), "run_update", async move {
                let result = tokio::task::spawn_blocking(move || {
                    cosmos_adapters::update::run_update(&target_version, move |percent| {
                        let _ = tx_update.send(BackgroundMessage::UpdateProgress { percent });
                    })
                })
                .await;

                match result {
                    Ok(Ok(())) => {}
                    Ok(Err(e)) => {
                        let _ = tx_error.send(BackgroundMessage::UpdateError(e.to_string()));
                    }
                    Err(e) => {
                        let _ = tx_error.send(BackgroundMessage::UpdateError(format!(
                            "Update task failed: {}",
                            e
                        )));
                    }
                }
            });
        }
        _ => {}
    }
}

fn handle_welcome_overlay_input(app: &mut App, key: &KeyEvent) {
    if matches!(key.code, KeyCode::Enter | KeyCode::Esc | KeyCode::Char('q')) {
        app.close_overlay();
    }
}

fn handle_generic_overlay_input(app: &mut App, key: &KeyEvent) {
    match key.code {
        KeyCode::Esc | KeyCode::Char('q') => app.close_overlay(),
        KeyCode::Down => app.overlay_scroll_down(),
        KeyCode::Up => app.overlay_scroll_up(),
        _ => {}
    }
}

pub(super) fn handle_overlay_input(
    app: &mut App,
    key: KeyEvent,
    ctx: &RuntimeContext,
) -> Result<()> {
    let overlay = app.overlay.clone();
    match overlay {
        Overlay::None => {}
        Overlay::Alert { .. } => handle_alert_overlay_input(app, &key),
        Overlay::ApiKeySetup { .. } => handle_api_key_overlay_input(app, &key, ctx),
        Overlay::ApplyPlan { .. } => handle_apply_plan_overlay_input(app, &key, ctx),
        Overlay::Reset { .. } => handle_reset_overlay_input(app, &key, ctx),
        Overlay::StartupCheck { .. } => handle_startup_check_overlay_input(app, &key, ctx),
        Overlay::Update {
            target_version,
            progress,
            error,
            ..
        } => handle_update_overlay_input(app, &key, ctx, target_version, progress, error.is_some()),
        Overlay::Welcome => handle_welcome_overlay_input(app, &key),
        _ => handle_generic_overlay_input(app, &key),
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
