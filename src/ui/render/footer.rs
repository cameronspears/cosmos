use crate::ui::theme::Theme;
use crate::ui::{ActivePanel, App, LoadingState, ShipStep, WorkflowStep};
use ratatui::{
    layout::Rect,
    style::Style,
    text::{Line, Span},
    widgets::Paragraph,
    Frame,
};

pub(super) fn render_footer(frame: &mut Frame, area: Rect, app: &App) {
    // Status and action buttons
    let mut spans = vec![Span::styled("  ", Style::default())];

    // Project name and branch with icon (truncate long branch names)
    let project_name = app
        .context
        .repo_root
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("unknown");
    spans.push(Span::styled(
        project_name,
        Style::default().fg(Theme::GREY_400),
    ));
    spans.push(Span::styled(" ⎇ ", Style::default().fg(Theme::GREY_500)));
    let branch_display = if app.context.branch.len() > 20 {
        format!("{}…", &app.context.branch[..19])
    } else {
        app.context.branch.clone()
    };
    let is_on_main = app.is_on_main_branch();
    spans.push(Span::styled(
        branch_display,
        Style::default().fg(if is_on_main {
            Theme::GREY_100
        } else {
            Theme::GREEN
        }),
    ));

    if app.git_refresh_error.is_some() {
        spans.push(Span::styled(
            "  status stale",
            Style::default().fg(Theme::YELLOW),
        ));
    }

    // Cost + budget indicators
    if app.session_cost > 0.0
        || app.config.max_session_cost_usd.is_some()
        || app.config.max_tokens_per_day.is_some()
    {
        spans.push(Span::styled("  ", Style::default()));

        if let Some(max) = app.config.max_session_cost_usd {
            spans.push(Span::styled(
                format!("${:.4}/${:.4}", app.session_cost, max),
                Style::default().fg(Theme::GREY_400),
            ));
        } else if app.session_cost > 0.0 {
            spans.push(Span::styled(
                format!("${:.4}", app.session_cost),
                Style::default().fg(Theme::GREY_400),
            ));
        }

        if let Some(max_tokens) = app.config.max_tokens_per_day {
            spans.push(Span::styled("  ", Style::default()));
            spans.push(Span::styled(
                format!("tok {}/{}", app.config.tokens_used_today, max_tokens),
                Style::default().fg(Theme::GREY_500),
            ));
        }
    }

    // Spacer before buttons
    let status_len: usize = spans.iter().map(|s| s.content.chars().count()).sum();
    let available = area.width as usize;
    // Panel-specific hints + help/quit buttons
    let button_area_approx = match app.active_panel {
        ActivePanel::Project => 55, // / search  g group  ␣ expand  ? help  q quit
        ActivePanel::Suggestions => match app.workflow_step {
            WorkflowStep::Suggestions => 38, // ↵ verify  ? help  q quit
            WorkflowStep::Verify => {
                if app.verify_state.loading || app.loading == LoadingState::GeneratingFix {
                    30 // Esc cancel  ? help  q quit
                } else if app.verify_state.preview.is_some() {
                    55 // ↵ apply  d details  Esc back  ? help  q quit
                } else {
                    30 // Esc back  ? help  q quit
                }
            }
            WorkflowStep::Review => {
                // Review passed (no findings) has shorter footer
                if app.review_state.findings.is_empty() && !app.review_state.summary.is_empty() {
                    38 // ↵ ship  Esc back  ? help  q quit
                } else {
                    50 // ␣ select  ↵ fix  Esc back  ? help  q quit
                }
            }
            WorkflowStep::Ship => match app.ship_state.step {
                ShipStep::Confirm => 45, // ↵ ship  Esc back  ? help  q quit
                ShipStep::Done => 50,    // ↵ open  Esc done  ? help  q quit
                _ => 25,                 // ? help  q quit (processing)
            },
        },
    };
    let spacer_len = available.saturating_sub(status_len + button_area_approx);
    if spacer_len > 0 {
        spans.push(Span::styled(" ".repeat(spacer_len), Style::default()));
    }

    // Panel-specific contextual hints
    match app.active_panel {
        ActivePanel::Project => {
            spans.push(Span::styled(
                " / ",
                Style::default().fg(Theme::GREY_900).bg(Theme::GREY_500),
            ));
            spans.push(Span::styled(
                " search ",
                Style::default().fg(Theme::GREY_500),
            ));
            spans.push(Span::styled(
                " g ",
                Style::default().fg(Theme::GREY_900).bg(Theme::GREY_500),
            ));
            spans.push(Span::styled(
                " group ",
                Style::default().fg(Theme::GREY_500),
            ));
            spans.push(Span::styled(
                " ↵ ",
                Style::default().fg(Theme::GREY_900).bg(Theme::GREY_500),
            ));
            spans.push(Span::styled(
                " expand ",
                Style::default().fg(Theme::GREY_500),
            ));
        }
        ActivePanel::Suggestions => match app.workflow_step {
            WorkflowStep::Suggestions => {
                spans.push(Span::styled(
                    " ↵ ",
                    Style::default().fg(Theme::GREY_900).bg(Theme::GREEN),
                ));
                spans.push(Span::styled(
                    " verify ",
                    Style::default().fg(Theme::GREY_300),
                ));
            }
            WorkflowStep::Verify => {
                if app.verify_state.loading || app.loading == LoadingState::GeneratingFix {
                    spans.push(Span::styled(
                        " Esc ",
                        Style::default().fg(Theme::GREY_900).bg(Theme::GREY_500),
                    ));
                    spans.push(Span::styled(
                        " cancel ",
                        Style::default().fg(Theme::GREY_500),
                    ));
                } else if app.verify_state.preview.is_some() {
                    spans.push(Span::styled(
                        " ↵ ",
                        Style::default().fg(Theme::GREY_900).bg(Theme::GREEN),
                    ));
                    spans.push(Span::styled(
                        " apply ",
                        Style::default().fg(Theme::GREY_300),
                    ));
                    spans.push(Span::styled(
                        " d ",
                        Style::default().fg(Theme::GREY_900).bg(Theme::GREY_600),
                    ));
                    spans.push(Span::styled(
                        if app.verify_state.show_technical_details {
                            " hide details "
                        } else {
                            " details "
                        },
                        Style::default().fg(Theme::GREY_500),
                    ));
                    spans.push(Span::styled(
                        " Esc ",
                        Style::default().fg(Theme::GREY_900).bg(Theme::GREY_600),
                    ));
                    spans.push(Span::styled(" back ", Style::default().fg(Theme::GREY_600)));
                } else {
                    spans.push(Span::styled(
                        " Esc ",
                        Style::default().fg(Theme::GREY_900).bg(Theme::GREY_500),
                    ));
                    spans.push(Span::styled(" back ", Style::default().fg(Theme::GREY_500)));
                }
            }
            WorkflowStep::Review => {
                // Check if review passed (no findings) - in this case, show "ship" instead of "fix"
                let review_passed =
                    app.review_state.findings.is_empty() && !app.review_state.summary.is_empty();

                if review_passed {
                    // Review passed - only action is to continue to ship
                    spans.push(Span::styled(
                        " ↵ ",
                        Style::default().fg(Theme::GREY_900).bg(Theme::GREEN),
                    ));
                    spans.push(Span::styled(" ship ", Style::default().fg(Theme::GREY_300)));
                    spans.push(Span::styled(
                        " Esc ",
                        Style::default().fg(Theme::GREY_900).bg(Theme::GREY_600),
                    ));
                    spans.push(Span::styled(" back ", Style::default().fg(Theme::GREY_600)));
                } else {
                    // Review has findings - show selection and fix options
                    spans.push(Span::styled(
                        " ␣ ",
                        Style::default().fg(Theme::GREY_900).bg(Theme::GREY_500),
                    ));
                    spans.push(Span::styled(
                        " select ",
                        Style::default().fg(Theme::GREY_500),
                    ));
                    spans.push(Span::styled(
                        " ↵ ",
                        Style::default().fg(Theme::GREY_900).bg(Theme::GREEN),
                    ));
                    spans.push(Span::styled(" fix ", Style::default().fg(Theme::GREY_300)));
                    spans.push(Span::styled(
                        " Esc ",
                        Style::default().fg(Theme::GREY_900).bg(Theme::GREY_600),
                    ));
                    spans.push(Span::styled(" back ", Style::default().fg(Theme::GREY_600)));
                }
            }
            WorkflowStep::Ship => match app.ship_state.step {
                ShipStep::Confirm => {
                    spans.push(Span::styled(
                        " ↵ ",
                        Style::default().fg(Theme::GREY_900).bg(Theme::GREEN),
                    ));
                    spans.push(Span::styled(" ship ", Style::default().fg(Theme::GREY_300)));
                    spans.push(Span::styled(
                        " Esc ",
                        Style::default().fg(Theme::GREY_900).bg(Theme::GREY_600),
                    ));
                    spans.push(Span::styled(" back ", Style::default().fg(Theme::GREY_600)));
                }
                ShipStep::Done => {
                    spans.push(Span::styled(
                        " ↵ ",
                        Style::default().fg(Theme::GREY_900).bg(Theme::GREEN),
                    ));
                    spans.push(Span::styled(
                        " open PR ",
                        Style::default().fg(Theme::GREY_300),
                    ));
                    spans.push(Span::styled(
                        " Esc ",
                        Style::default().fg(Theme::GREY_900).bg(Theme::GREY_600),
                    ));
                    spans.push(Span::styled(" done ", Style::default().fg(Theme::GREY_600)));
                }
                _ => {
                    // Processing states - no action buttons
                }
            },
        },
    }

    // Undo hint (shown when there are pending changes)
    if !app.pending_changes.is_empty() {
        spans.push(Span::styled(
            " u ",
            Style::default().fg(Theme::GREY_900).bg(Theme::YELLOW),
        ));
        spans.push(Span::styled(" undo ", Style::default().fg(Theme::GREY_400)));
    }

    // Help and quit (always shown)
    spans.push(Span::styled(
        " ? ",
        Style::default().fg(Theme::GREY_900).bg(Theme::GREY_500),
    ));
    spans.push(Span::styled(" help ", Style::default().fg(Theme::GREY_500)));

    spans.push(Span::styled(
        " q ",
        Style::default().fg(Theme::GREY_900).bg(Theme::GREY_600),
    ));
    spans.push(Span::styled(" quit ", Style::default().fg(Theme::GREY_600)));

    spans.push(Span::styled(" ", Style::default()));

    let footer_line = Line::from(spans);

    let footer = Paragraph::new(vec![Line::from(""), footer_line])
        .style(Style::default().bg(Theme::GREY_900));
    frame.render_widget(footer, area);
}
