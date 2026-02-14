//! Background task handling for the UI shell runtime.

use crate::app::messages::BackgroundMessage;
use crate::app::RuntimeContext;
use crate::ui;
use crate::ui::{App, LoadingState, WorkflowStep};
use crate::util::truncate;
use futures::FutureExt;
use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::path::PathBuf;
use std::sync::mpsc;

pub const SUGGESTION_ENGINE_ENABLED: bool = false;

pub fn suggestion_engine_enabled() -> bool {
    SUGGESTION_ENGINE_ENABLED
}

pub fn request_suggestions_refresh(
    app: &mut App,
    _tx: mpsc::Sender<BackgroundMessage>,
    _repo_root: PathBuf,
    _reason: &str,
) -> bool {
    app.show_toast("Suggestion engine is disabled in UI shell mode.");
    false
}

pub fn drain_messages(
    app: &mut App,
    rx: &mpsc::Receiver<BackgroundMessage>,
    _ctx: &RuntimeContext,
) -> bool {
    let mut changed = false;
    while let Ok(msg) = rx.try_recv() {
        changed = true;
        match msg {
            BackgroundMessage::ShipProgress(step) => {
                if app.workflow_step == WorkflowStep::Ship {
                    app.set_ship_step(step);
                } else {
                    app.ship_step = Some(step);
                }
            }
            BackgroundMessage::ShipComplete(url) => {
                if app.workflow_step == WorkflowStep::Ship {
                    app.set_ship_pr_url(url.clone());
                    app.show_toast("PR created!");
                } else {
                    app.ship_step = Some(ui::ShipStep::Done);
                    app.pr_url = Some(url.clone());
                    app.clear_pending_changes();
                }
            }
            BackgroundMessage::ShipError(e) => {
                app.ship_step = None;
                app.close_overlay();
                app.show_toast(&format!("Ship failed: {}", truncate(&e, 80)));
            }
            BackgroundMessage::ResetComplete { options } => {
                app.loading = LoadingState::None;
                if options.contains(&crate::cache::ResetOption::QuestionCache) {
                    app.question_cache = crate::cache::QuestionCache::default();
                }
                let labels: Vec<&str> = options.iter().map(|o| o.label()).collect();
                if labels.is_empty() {
                    app.show_toast("Reset complete");
                } else {
                    app.show_toast(&format!("Reset complete: {}", labels.join(", ")));
                }
            }
            BackgroundMessage::StashComplete { message } => {
                app.loading = LoadingState::None;
                app.show_toast(&format!("Changes saved: {}", message));
            }
            BackgroundMessage::DiscardComplete => {
                app.loading = LoadingState::None;
                app.show_toast("Changes discarded - starting fresh");
            }
            BackgroundMessage::Error(e) => {
                app.loading = LoadingState::None;
                app.show_toast(&truncate(&e, 100));
            }
            BackgroundMessage::UpdateAvailable { latest_version } => {
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
        }
    }

    if changed {
        app.needs_redraw = true;
    }
    changed
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
