use crate::ui::{App, InputMode};
use anyhow::Result;
use crossterm::event::{KeyCode, KeyEvent};

/// Handle key events in search mode
pub(super) fn handle_search_input(app: &mut App, key: KeyEvent) -> Result<()> {
    match key.code {
        KeyCode::Esc => app.exit_search(),
        KeyCode::Enter => app.input_mode = InputMode::Normal,
        KeyCode::Backspace => app.search_pop(),
        KeyCode::Char(c) => app.search_push(c),
        _ => {}
    }
    Ok(())
}
