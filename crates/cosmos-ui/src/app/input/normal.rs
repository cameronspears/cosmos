use crate::app::background;
use crate::app::messages::BackgroundMessage;
use crate::app::RuntimeContext;
use crate::git_ops;
use crate::ui::{App, Overlay, ShipStep, WorkflowStep};
use anyhow::Result;
use crossterm::event::{KeyCode, KeyEvent};

fn refresh_suggestions_now(app: &mut App, _ctx: &RuntimeContext, _reason: &str) {
    app.show_toast("Suggestion engine is disabled. Suggestions panel kept for rebuild.");
}

fn start_ship_from_confirm(app: &mut App, ctx: &RuntimeContext) {
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

fn handle_suggestions_enter(app: &mut App) {
    if app.suggestion_refinement_in_progress {
        app.show_toast("Suggestions are still refining. Wait for refined results.");
        return;
    }

    app.clear_apply_confirm();
    app.show_toast(
        "Apply/review workflow is removed in clean-slate mode. UI components are retained only.",
    );
}

pub(super) fn handle_normal_mode(app: &mut App, key: KeyEvent, ctx: &RuntimeContext) -> Result<()> {
    match key.code {
        KeyCode::Char('q') => app.should_quit = true,
        KeyCode::Tab => {
            app.toggle_panel();
        }
        KeyCode::Down => {
            if app.is_ask_cosmos_mode() {
                app.ask_cosmos_scroll_down();
            } else {
                match app.workflow_step {
                    WorkflowStep::Ship => app.ship_scroll_down(),
                    _ => app.navigate_down(),
                }
            }
        }
        KeyCode::Up => {
            if app.is_ask_cosmos_mode() {
                app.ask_cosmos_scroll_up();
            } else {
                match app.workflow_step {
                    WorkflowStep::Ship => app.ship_scroll_up(),
                    _ => app.navigate_up(),
                }
            }
        }
        KeyCode::Enter => {
            if let Some(url) = app.pr_url.take() {
                let _ = git_ops::open_url(&url);
            } else {
                match app.workflow_step {
                    WorkflowStep::Suggestions => handle_suggestions_enter(app),
                    WorkflowStep::Review => {
                        app.show_toast(
                            "Review stage is disabled in clean-slate mode. Return to Suggestions to continue.",
                        );
                    }
                    WorkflowStep::Ship => match app.ship_state.step {
                        ShipStep::Confirm => start_ship_from_confirm(app, ctx),
                        ShipStep::Done => {
                            if let Some(url) = &app.ship_state.pr_url {
                                let _ = git_ops::open_url(url);
                            }
                            app.workflow_complete();
                        }
                        _ => {}
                    },
                }
            }
        }
        KeyCode::Esc => {
            if app.is_ask_cosmos_mode() {
                app.exit_ask_cosmos();
            } else if app.workflow_step != WorkflowStep::Suggestions {
                app.workflow_back();
            } else if app.overlay != Overlay::None {
                app.close_overlay();
                app.clear_apply_confirm();
            }
        }
        KeyCode::Char('?') => app.toggle_help(),
        KeyCode::Char('d') => {
            if app.workflow_step == WorkflowStep::Suggestions {
                app.show_toast(
                    "Suggestion diagnostics are unavailable while suggestion engine is disabled.",
                );
            }
        }
        KeyCode::Char('x') => {
            if app.workflow_step == WorkflowStep::Suggestions {
                app.show_toast("Suggestion engine is disabled. Nothing to dismiss.");
            }
        }
        KeyCode::Char('i') => {
            if app.workflow_step == WorkflowStep::Suggestions {
                app.show_toast("Ask Cosmos is disabled in UI shell mode.");
            }
        }
        KeyCode::Char('k') => {
            app.open_api_key_overlay(None);
        }
        KeyCode::Char('u') => match app.undo_last_pending_change() {
            Ok(()) => app.show_toast("Change undone"),
            Err(e) => app.show_toast(&e),
        },
        KeyCode::Char('r') => {
            if app.workflow_step == WorkflowStep::Suggestions {
                refresh_suggestions_now(app, ctx, "Manual refresh");
            }
        }
        KeyCode::Char('R') => {
            app.open_reset_overlay();
        }
        KeyCode::Char('U') => {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::WorkContext;
    use crate::index::CodebaseIndex;
    use crate::suggest::SuggestionEngine;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use std::collections::HashMap;
    use std::sync::mpsc;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn make_app_root(prefix: &str) -> std::path::PathBuf {
        let mut root = std::env::temp_dir();
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        root.push(format!("{}_{}", prefix, nanos));
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    fn make_app(root: &std::path::Path) -> (App, CodebaseIndex) {
        let index = CodebaseIndex {
            root: root.to_path_buf(),
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
            repo_root: root.to_path_buf(),
        };
        let mut app = App::new(index.clone(), suggestions, context);
        app.workflow_step = WorkflowStep::Suggestions;
        (app, index)
    }

    #[test]
    fn k_opens_api_key_overlay() {
        let root = make_app_root("cosmos_api_key_overlay_test");
        let (mut app, index) = make_app(&root);

        let (tx, _rx) = mpsc::channel();
        let ctx = crate::app::RuntimeContext {
            index: &index,
            repo_path: &root,
            tx: &tx,
        };

        handle_normal_mode(
            &mut app,
            KeyEvent::new(KeyCode::Char('k'), KeyModifiers::NONE),
            &ctx,
        )
        .unwrap();

        assert!(matches!(app.overlay, Overlay::ApiKeySetup { .. }));

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn enter_on_suggestion_reports_disabled_apply_pipeline() {
        let root = make_app_root("cosmos_suggestion_enter_test");
        let (mut app, index) = make_app(&root);

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

        let toast = app
            .toast
            .as_ref()
            .map(|t| t.message.clone())
            .unwrap_or_default();
        assert!(toast.contains("removed in clean-slate mode"));

        let _ = std::fs::remove_dir_all(root);
    }
}
