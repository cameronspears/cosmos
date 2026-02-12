use crate::ui::theme::Theme;
use crate::ui::App;
use ratatui::{
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
    Frame,
};

pub(super) fn render_header(frame: &mut Frame, area: Rect, _app: &App) {
    // Build spans for the logo
    let spans = vec![Span::styled(
        format!("   {}", Theme::COSMOS_LOGO),
        Style::default()
            .fg(Theme::WHITE)
            .add_modifier(Modifier::BOLD),
    )];

    let lines = vec![Line::from(""), Line::from(spans)];

    let header = Paragraph::new(lines).style(Style::default().bg(Theme::BG));
    frame.render_widget(header, area);
}
