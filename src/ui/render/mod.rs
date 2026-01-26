mod footer;
mod header;
mod main;
mod overlays;
mod toast;

use crate::ui::theme::Theme;
use crate::ui::{App, Overlay};
use ratatui::{
    layout::{Constraint, Direction, Layout},
    style::Style,
    widgets::Block,
    Frame,
};

use footer::render_footer;
use header::render_header;
use main::render_main;
use overlays::{
    render_file_detail, render_help, render_reset_overlay,
    render_startup_check,
};
use toast::render_toast;

/// Main render function
pub fn render(frame: &mut Frame, app: &App) {
    let area = frame.area();

    // Clear with dark background
    frame.render_widget(Block::default().style(Style::default().bg(Theme::BG)), area);

    // Main layout - clean and minimal
    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3), // Header (logo)
            Constraint::Min(10),   // Main content
            Constraint::Length(3), // Footer
        ])
        .split(area);

    render_header(frame, layout[0], app);
    render_main(frame, layout[1], app);
    render_footer(frame, layout[2], app);

    // Loading is shown inline in the footer status bar (non-blocking)

    // Overlays
    match &app.overlay {
        Overlay::Help { scroll } => render_help(frame, *scroll),
        Overlay::FileDetail { path, scroll } => {
            if let Some(file_index) = app.index.files.get(path) {
                render_file_detail(frame, path, file_index, app.get_llm_summary(path), *scroll);
            }
        }
        Overlay::Reset { options, selected } => {
            render_reset_overlay(frame, options, *selected);
        }
        Overlay::StartupCheck {
            changed_count,
            current_branch,
            main_branch,
            scroll,
            confirming_discard,
        } => {
            render_startup_check(
                frame,
                *changed_count,
                current_branch,
                main_branch,
                *scroll,
                *confirming_discard,
            );
        }
        Overlay::None => {}
    }

    // Toast
    if let Some(toast) = &app.toast {
        render_toast(frame, toast);
    }
}
