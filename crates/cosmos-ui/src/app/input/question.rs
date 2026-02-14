use crate::app::RuntimeContext;
use crate::ui::App;
use anyhow::Result;
use crossterm::event::{KeyCode, KeyEvent};

/// Handle key events in question (ask cosmos) mode.
pub(super) fn handle_question_input(
    app: &mut App,
    key: KeyEvent,
    _ctx: &RuntimeContext,
) -> Result<()> {
    match key.code {
        KeyCode::Esc => app.exit_question(),
        KeyCode::Up if app.question_input.is_empty() => app.question_suggestion_up(),
        KeyCode::Down if app.question_input.is_empty() => app.question_suggestion_down(),
        KeyCode::Tab => app.use_selected_suggestion(),
        KeyCode::Enter => {
            app.exit_question();
            app.show_toast("Ask Cosmos is disabled in UI shell mode.");
        }
        KeyCode::Backspace => app.question_pop(),
        KeyCode::Char(c) => app.question_push(c),
        _ => {}
    }
    Ok(())
}
