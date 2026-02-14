//! TUI runtime for the UI-shell build.

use crate::app::messages::BackgroundMessage;
use crate::app::{background, input, RuntimeContext};
use crate::cache;
use crate::context::WorkContext;
use crate::git_ops;
use crate::index::CodebaseIndex;
use crate::suggest::SuggestionEngine;
use crate::ui;
use crate::ui::App;
use anyhow::Result;
use crossterm::{
    event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyEventKind},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::prelude::*;
use std::io;
use std::path::PathBuf;
use std::sync::mpsc;
use std::time::Duration;

/// Run the TUI application.
pub async fn run_tui(
    index: CodebaseIndex,
    suggestions: SuggestionEngine,
    context: WorkContext,
    cache_manager: cache::Cache,
    repo_path: PathBuf,
) -> Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut app = App::new(index.clone(), suggestions, context.clone());
    app.repo_memory = cache_manager.load_repo_memory();
    app.glossary = cache_manager.load_glossary().unwrap_or_default();
    app.question_cache = cache_manager.load_question_cache().unwrap_or_default();
    app.needs_summary_generation = false;
    app.pending_suggestions_on_init = false;

    if let Ok(status) = git_ops::current_status(&repo_path) {
        let main_branch =
            git_ops::get_main_branch_name(&repo_path).unwrap_or_else(|_| "main".to_string());
        let is_on_main = status.branch == main_branch;
        let changed_count = status.staged.len() + status.modified.len() + status.untracked.len();
        if !is_on_main || changed_count > 0 {
            app.show_startup_check(changed_count, status.branch.clone(), main_branch);
        }
    }

    if !cache_manager.has_seen_welcome() && app.overlay == ui::Overlay::None {
        app.overlay = ui::Overlay::Welcome;
        let _ = cache_manager.mark_welcome_seen();
    }

    let (tx, rx) = mpsc::channel::<BackgroundMessage>();

    {
        let tx_update = tx.clone();
        background::spawn_background(tx.clone(), "version_check", async move {
            if let Ok(Some(update_info)) = crate::update::check_for_update().await {
                let _ = tx_update.send(BackgroundMessage::UpdateAvailable {
                    latest_version: update_info.latest_version,
                });
            }
        });
    }

    let result = run_loop(&mut terminal, &mut app, rx, tx, repo_path, index);

    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    )?;
    terminal.show_cursor()?;

    result
}

fn run_loop<B: Backend>(
    terminal: &mut Terminal<B>,
    app: &mut App,
    rx: mpsc::Receiver<BackgroundMessage>,
    tx: mpsc::Sender<BackgroundMessage>,
    repo_path: PathBuf,
    index: CodebaseIndex,
) -> Result<()> {
    let mut last_git_refresh = std::time::Instant::now();
    let mut last_spinner_tick = std::time::Instant::now();
    let mut last_toast_check = std::time::Instant::now();
    let spinner_interval = Duration::from_millis(100);
    let toast_check_interval = Duration::from_millis(250);
    let idle_poll_cap = Duration::from_millis(500);

    let git_refresh_interval = if index.stats().file_count > 20000 {
        std::time::Duration::from_secs(10)
    } else if index.stats().file_count > 5000 {
        std::time::Duration::from_secs(5)
    } else {
        std::time::Duration::from_secs(2)
    };

    let ctx = RuntimeContext {
        index: &index,
        repo_path: &repo_path,
        tx: &tx,
    };
    let mut needs_redraw = app.needs_redraw;

    loop {
        if app.loading.is_loading() && last_spinner_tick.elapsed() >= spinner_interval {
            app.tick_loading();
            last_spinner_tick = std::time::Instant::now();
            needs_redraw = true;
        }

        if last_toast_check.elapsed() >= toast_check_interval {
            let had_toast = app.toast.is_some();
            app.clear_expired_toast();
            if had_toast && app.toast.is_none() {
                needs_redraw = true;
            }
            last_toast_check = std::time::Instant::now();
        }

        if last_git_refresh.elapsed() >= git_refresh_interval {
            match app.context.refresh() {
                Ok(_) => {
                    app.git_refresh_error = None;
                    app.git_refresh_error_at = None;
                    needs_redraw = true;
                }
                Err(e) => {
                    let message = format!("Git status refresh failed: {}", e);
                    let should_log = app
                        .git_refresh_error_at
                        .map(|t| t.elapsed() >= Duration::from_secs(30))
                        .unwrap_or(true);
                    if should_log {
                        app.show_toast(&message);
                        app.git_refresh_error_at = Some(std::time::Instant::now());
                        needs_redraw = true;
                    }
                    app.git_refresh_error = Some(message);
                }
            }
            last_git_refresh = std::time::Instant::now();
        }

        if background::drain_messages(app, &rx, &ctx) {
            needs_redraw = true;
        }
        if app.needs_redraw {
            needs_redraw = true;
        }

        if needs_redraw {
            terminal.draw(|f| ui::render(f, app))?;
            needs_redraw = false;
            app.needs_redraw = false;
        }

        let to_next_git = git_refresh_interval.saturating_sub(last_git_refresh.elapsed());
        let to_next_spinner = if app.loading.is_loading() {
            spinner_interval.saturating_sub(last_spinner_tick.elapsed())
        } else {
            idle_poll_cap
        };
        let to_next_toast = if app.toast.is_some() {
            toast_check_interval.saturating_sub(last_toast_check.elapsed())
        } else {
            idle_poll_cap
        };
        let poll_timeout = to_next_git
            .min(to_next_spinner)
            .min(to_next_toast)
            .min(idle_poll_cap);

        if event::poll(poll_timeout)? {
            if let Event::Key(key) = event::read()? {
                if key.kind != KeyEventKind::Press {
                    continue;
                }
                input::handle_key_event(app, key, &ctx)?;
                needs_redraw = true;
                app.needs_redraw = true;
            }
        }

        if app.should_quit {
            return Ok(());
        }
    }
}
