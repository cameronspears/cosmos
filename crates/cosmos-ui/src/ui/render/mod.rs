mod footer;
mod header;
mod main;
mod overlays;

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
    render_alert, render_api_key_overlay, render_apply_plan, render_file_detail, render_help,
    render_reset_overlay, render_startup_check, render_suggestion_focus_overlay,
    render_update_overlay, render_welcome,
};

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
        Overlay::Alert {
            title,
            message,
            scroll,
        } => render_alert(frame, title, message, *scroll),
        Overlay::Help { scroll } => render_help(frame, *scroll),
        Overlay::FileDetail { path, scroll } => {
            if let Some(file_index) = app.index.files.get(path) {
                render_file_detail(frame, path, file_index, *scroll);
            }
        }
        Overlay::ApiKeySetup {
            input,
            error,
            save_armed,
        } => {
            render_api_key_overlay(frame, input, error.as_deref(), *save_armed);
        }
        Overlay::SuggestionFocus { selected } => {
            render_suggestion_focus_overlay(frame, *selected);
        }
        Overlay::ApplyPlan {
            preview,
            affected_files,
            confirm_apply,
            show_technical_details,
            show_data_notice,
            scroll,
            ..
        } => {
            render_apply_plan(
                frame,
                preview,
                affected_files,
                *confirm_apply,
                *show_technical_details,
                *show_data_notice,
                *scroll,
            );
        }
        Overlay::Reset {
            options,
            selected,
            error,
        } => {
            render_reset_overlay(frame, options, *selected, error.as_deref());
        }
        Overlay::StartupCheck {
            changed_count,
            current_branch,
            main_branch,
            mode,
            selected_action,
        } => {
            render_startup_check(
                frame,
                *changed_count,
                current_branch,
                main_branch,
                *mode,
                *selected_action,
            );
        }
        Overlay::Update {
            current_version,
            target_version,
            progress,
            error,
        } => {
            render_update_overlay(
                frame,
                current_version,
                target_version,
                *progress,
                error.as_deref(),
            );
        }
        Overlay::Welcome => {
            render_welcome(frame);
        }
        Overlay::None => {}
    }
}
