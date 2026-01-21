use crate::app::bootstrap;
use crate::app::messages::BackgroundMessage;
use crate::app::{background, input, RuntimeContext};
use crate::cache;
use crate::context::WorkContext;
use crate::git_ops;
use crate::index::CodebaseIndex;
use crate::suggest;
use crate::suggest::SuggestionEngine;
use crate::ui::App;
use crate::ui;
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

/// Run the TUI application with background LLM tasks
pub async fn run_tui(
    index: CodebaseIndex,
    suggestions: SuggestionEngine,
    context: WorkContext,
    cache_manager: cache::Cache,
    repo_path: PathBuf,
) -> Result<()> {
    // Set up terminal
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    // Create app with loading state
    let mut app = App::new(index.clone(), suggestions, context.clone());
    // Load repo-local “memory” (decisions/conventions) from .cosmos/
    app.repo_memory = cache_manager.load_repo_memory();
    // Load cached domain glossary (auto-extracted terminology)
    app.glossary = cache_manager.load_glossary().unwrap_or_default();

    let needs_onboarding = !suggest::llm::is_available();

    // Check for unsaved work and show startup overlay if needed
    if !needs_onboarding {
        if let Ok(status) = git_ops::current_status(&repo_path) {
            let main_branch =
                git_ops::get_main_branch_name(&repo_path).unwrap_or_else(|_| "main".to_string());
            let is_on_main = status.branch == main_branch;
            let changed_count = status.staged.len() + status.modified.len();

            // Show overlay if not on main or has uncommitted changes
            if !is_on_main || changed_count > 0 {
                app.show_startup_check(changed_count);
            }
        }
    }

    // Create channel for background tasks
    let (tx, rx) = mpsc::channel::<BackgroundMessage>();

    if needs_onboarding {
        app.show_onboarding();
    } else {
        bootstrap::init_ai_pipeline(&mut app, tx.clone());
    }

    // Main loop with async event handling
    let result = run_loop(&mut terminal, &mut app, rx, tx, repo_path, index);

    // Restore terminal
    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    )?;
    terminal.show_cursor()?;

    result
}

/// Main event loop with background message handling
fn run_loop<B: Backend>(
    terminal: &mut Terminal<B>,
    app: &mut App,
    rx: mpsc::Receiver<BackgroundMessage>,
    tx: mpsc::Sender<BackgroundMessage>,
    repo_path: PathBuf,
    index: CodebaseIndex,
) -> Result<()> {
    // Track last git status refresh time
    let mut last_git_refresh = std::time::Instant::now();
    const GIT_REFRESH_INTERVAL: std::time::Duration = std::time::Duration::from_secs(2);

    let ctx = RuntimeContext {
        index: &index,
        repo_path: &repo_path,
        tx: &tx,
    };

    loop {
        // Clear expired toasts
        app.clear_expired_toast();

        // Advance spinner animation
        app.tick_loading();

        // Periodically refresh git status (every 2 seconds)
        if last_git_refresh.elapsed() >= GIT_REFRESH_INTERVAL {
            let _ = app.context.refresh();
            last_git_refresh = std::time::Instant::now();
        }

        // Check for background messages (non-blocking)
        background::drain_messages(app, &rx, &ctx);

        // Render
        terminal.draw(|f| ui::render(f, app))?;

        // Poll for events with fast timeout (snappy animations)
        if event::poll(Duration::from_millis(50))? {
            if let Event::Key(key) = event::read()? {
                if key.kind != KeyEventKind::Press {
                    continue;
                }
                input::handle_key_event(app, key, &ctx)?;
            }
        }

        if app.should_quit {
            return Ok(());
        }
    }
}
