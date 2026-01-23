use crate::ui::theme::Theme;
use crate::ui::{Toast, ToastKind};
use ratatui::{
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Clear, Paragraph},
    Frame,
};

pub(super) fn render_toast(frame: &mut Frame, toast: &Toast) {
    let area = frame.area();

    // Use the ToastKind for consistent styling
    let (prefix, message, bg, text_style) = match toast.kind {
        ToastKind::Success => (
            "  + ",
            toast.message.trim_start_matches('+').trim_start(),
            Theme::GREEN,
            Style::default()
                .fg(Theme::WHITE)
                .add_modifier(Modifier::BOLD),
        ),
        ToastKind::Error => (
            "  x ",
            toast.message.as_str(),
            Theme::RED,
            Style::default().fg(Theme::WHITE),
        ),
        ToastKind::RateLimit => {
            // Rate limit toast with countdown - countdown shown in suffix
            (
                "  ~ ",
                toast.message.as_str(),
                Theme::YELLOW,
                Style::default().fg(Theme::GREY_900),
            )
        }
        ToastKind::Info => (
            "  › ",
            toast.message.as_str(),
            Theme::GREY_700,
            Style::default()
                .fg(Theme::GREY_100)
                .add_modifier(Modifier::ITALIC),
        ),
    };

    // For rate limits, add countdown hint
    let suffix = if toast.kind == ToastKind::RateLimit {
        let remaining = toast
            .kind
            .duration_secs()
            .saturating_sub(toast.created_at.elapsed().as_secs());
        format!(" ({}s) ", remaining)
    } else {
        String::from("  ")
    };

    let width = (prefix.len() + message.len() + suffix.len()) as u16;
    let height = 1u16;
    let toast_area = Rect {
        x: (area.width.saturating_sub(width)) / 2,
        y: area.height.saturating_sub(5),
        width: width.min(area.width),
        height,
    };

    frame.render_widget(Clear, toast_area);

    let content = Paragraph::new(Line::from(vec![
        Span::styled(prefix, Style::default().fg(Theme::WHITE)),
        Span::styled(message, text_style),
        Span::styled(&suffix, Style::default().fg(Theme::GREY_900)),
    ]))
    .style(Style::default().bg(bg));
    frame.render_widget(content, toast_area);
}
