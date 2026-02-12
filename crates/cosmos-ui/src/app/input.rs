//! Input handling for Cosmos TUI
//!
//! # Error Handling
//!
//! Background tasks use `let _ =` for channel sends. See `background.rs` module
//! docs for the rationale: channel sends can fail if receiver is dropped during
//! shutdown, which is expected and safe to ignore.

use crate::app::RuntimeContext;
use crate::ui::{App, InputMode, Overlay};
use anyhow::Result;
use crossterm::event::KeyEvent;

mod normal;
mod overlay;
mod question;
mod search;

use normal::handle_normal_mode;
use overlay::handle_overlay_input;
use question::handle_question_input;
use search::handle_search_input;

// ═══════════════════════════════════════════════════════════════════════════
//  MAIN INPUT DISPATCHER
// ═══════════════════════════════════════════════════════════════════════════

/// Main key event handler - dispatches to mode-specific handlers
pub fn handle_key_event(app: &mut App, key: KeyEvent, ctx: &RuntimeContext) -> Result<()> {
    // Dispatch based on current input mode
    match app.input_mode {
        InputMode::Search => return handle_search_input(app, key),
        InputMode::Question => return handle_question_input(app, key, ctx),
        InputMode::Normal => {}
    }

    // Handle overlay mode if active
    if app.overlay != Overlay::None {
        return handle_overlay_input(app, key, ctx);
    }

    // Normal mode handling
    handle_normal_mode(app, key, ctx)
}
