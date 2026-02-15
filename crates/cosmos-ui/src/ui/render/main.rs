use crate::ui::helpers::{wrap_text, wrap_text_variable_width};
use crate::ui::markdown;
use crate::ui::theme::Theme;
use crate::ui::{
    ActivePanel, App, AskCosmosState, LoadingState, ShipStep, WorkflowStep, ASK_STARTER_QUESTIONS,
    SPINNER_FRAMES,
};
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph},
    Frame,
};
use std::cell::RefCell;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

/// Cached main layout to avoid recomputing on every frame
struct CachedMainLayout {
    area: Rect,
    suggestions_panel: Rect,
    ask_panel: Rect,
}

struct CachedAskMarkdown {
    response_hash: u64,
    width: usize,
    padded_lines: Vec<Line<'static>>,
}

thread_local! {
    static MAIN_LAYOUT_CACHE: RefCell<Option<CachedMainLayout>> = const { RefCell::new(None) };
    static ASK_MARKDOWN_CACHE: RefCell<Option<CachedAskMarkdown>> = const { RefCell::new(None) };
}

const ASK_TARGET_PERCENT: u16 = 30;
const ASK_MIN_COLS: u16 = 44;
const ASK_MAX_COLS: u16 = 58;
const SUGGESTIONS_MIN_COLS: u16 = 52;
const GAP_COLS: u16 = 2;
const ASK_HARD_MIN_COLS: u16 = 28;

fn compute_ask_panel_width(padded_width: u16) -> u16 {
    let available = padded_width.saturating_sub(GAP_COLS);
    if available == 0 {
        return 0;
    }

    let target = ((available as u32 * ASK_TARGET_PERCENT as u32) + 50) / 100;
    let bounded = (target as u16)
        .clamp(ASK_MIN_COLS, ASK_MAX_COLS)
        .min(available);
    let max_allowed = available
        .saturating_sub(SUGGESTIONS_MIN_COLS)
        .max(ASK_HARD_MIN_COLS)
        .min(available);

    bounded.min(max_allowed)
}

pub(super) fn render_main(frame: &mut Frame, area: Rect, app: &App) {
    let (suggestions_rect, ask_rect) = MAIN_LAYOUT_CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();

        // Reuse cached layout if area unchanged
        if let Some(cached) = cache.as_ref() {
            if cached.area == area {
                return (cached.suggestions_panel, cached.ask_panel);
            }
        }

        // Recompute layout (only on resize) with horizontal padding.
        let padded = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Length(2), // Left padding
                Constraint::Min(10),   // Main content
                Constraint::Length(2), // Right padding
            ])
            .split(area);

        let ask_width = compute_ask_panel_width(padded[1].width);
        let panels = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Min(SUGGESTIONS_MIN_COLS), // Main workflow/suggestions
                Constraint::Length(GAP_COLS),          // Gap
                Constraint::Length(ask_width),         // Ask panel (adaptive)
            ])
            .split(padded[1]);

        // Cache the result
        *cache = Some(CachedMainLayout {
            area,
            suggestions_panel: panels[0],
            ask_panel: panels[2],
        });

        (panels[0], panels[2])
    });

    render_suggestions_panel(frame, suggestions_rect, app);
    render_ask_panel(frame, ask_rect, app);
}

fn render_suggestions_panel(frame: &mut Frame, area: Rect, app: &App) {
    let is_active = app.active_panel == ActivePanel::Suggestions;
    let border_style = if is_active {
        Style::default().fg(Theme::GREY_300)
    } else {
        Style::default().fg(Theme::GREY_600)
    };

    // Reserve space for border (2 lines top/bottom)
    let content_height = area.height.saturating_sub(2) as usize;
    let inner_width = area.width.saturating_sub(4) as usize;

    let mut lines = vec![];

    // Render content based on workflow step
    match app.workflow_step {
        WorkflowStep::Suggestions => {
            render_suggestions_content(&mut lines, app, is_active, content_height, inner_width);
        }
        WorkflowStep::Review => {
            render_review_content(&mut lines, app, content_height, inner_width);
        }
        WorkflowStep::Ship => {
            render_ship_content(&mut lines, app, content_height, inner_width);
        }
    }

    // Build title with workflow breadcrumbs in the border.
    let title = render_workflow_title(app.workflow_step);

    let block = Block::default()
        .title(title)
        .title_style(Style::default().fg(Theme::GREY_200))
        .borders(Borders::ALL)
        .border_style(border_style)
        .style(Style::default().bg(Theme::GREY_800));

    let paragraph = Paragraph::new(lines).block(block);
    frame.render_widget(paragraph, area);
}

/// Build the workflow title for the border.
fn render_workflow_title(current: WorkflowStep) -> String {
    let steps = [
        (WorkflowStep::Suggestions, Theme::WORKFLOW_SUGGESTIONS),
        (WorkflowStep::Review, Theme::WORKFLOW_REVIEW),
        (WorkflowStep::Ship, Theme::WORKFLOW_SHIP),
    ];

    let mut parts = Vec::new();
    for (step, label) in steps.iter() {
        if *step == current {
            // Current step is shown (with underline effect via brackets)
            parts.push(format!("[{}]", label));
        } else if step.index() < current.index() {
            // Completed steps shown normally
            parts.push(label.to_string());
        } else {
            // Future steps shown dimmer (just show them)
            parts.push(label.to_string());
        }
    }

    format!(" {} ", parts.join(" › "))
}

/// Render the Suggestions step content
fn render_suggestions_content<'a>(
    lines: &mut Vec<Line<'a>>,
    app: &App,
    is_active: bool,
    visible_height: usize,
    inner_width: usize,
) {
    let suggestions = app.suggestions.active_suggestions();

    // Top padding for breathing room
    lines.push(Line::from(""));

    // Check for loading states relevant to suggestions panel
    let loading_message: Option<String> = match app.loading {
        LoadingState::GeneratingSuggestions => {
            if let Some((completed, total)) = app.summary_progress {
                Some(format!(
                    "Generating suggestions... (summaries: {}/{})",
                    completed, total
                ))
            } else {
                Some("Generating suggestions...".to_string())
            }
        }
        LoadingState::GeneratingSummaries => {
            if let Some((completed, total)) = app.summary_progress {
                Some(format!("Summarizing files... ({}/{})", completed, total))
            } else {
                Some("Summarizing files...".to_string())
            }
        }
        LoadingState::Resetting => Some("Resetting cache...".to_string()),
        LoadingState::SwitchingBranch => Some("Switching to main branch...".to_string()),
        LoadingState::None => None,
        _ => {
            // For other active loading states, show context if available
            if app.pending_suggestions_on_init {
                Some("Preparing suggestions...".to_string())
            } else {
                None
            }
        }
    };

    if let Some(message) = loading_message {
        let spinner = SPINNER_FRAMES[app.loading_frame % SPINNER_FRAMES.len()];
        lines.push(Line::from(vec![
            Span::styled("    ", Style::default()),
            Span::styled(format!("{} ", spinner), Style::default().fg(Theme::WHITE)),
            Span::styled(message, Style::default().fg(Theme::GREY_300)),
        ]));
        return;
    }

    if suggestions.is_empty() {
        let has_ai = cosmos_engine::llm::is_available();
        let summaries_incomplete =
            app.needs_summary_generation && !app.summary_failed_files.is_empty();

        let border_style = Style::default().fg(Theme::GREY_700);
        let card_width = inner_width.saturating_sub(12).clamp(26, 40);
        let rule_width = card_width + 2;
        let row_width = card_width;

        let center_row = |text: &str| -> String {
            let clipped: String = text.chars().take(row_width).collect();
            let len = clipped.chars().count();
            if len >= row_width {
                return clipped;
            }
            let left = (row_width - len) / 2;
            let right = row_width - len - left;
            format!("{}{}{}", " ".repeat(left), clipped, " ".repeat(right))
        };

        lines.push(Line::from(vec![
            Span::styled("    ╭", border_style),
            Span::styled("─".repeat(rule_width), border_style),
            Span::styled("╮", border_style),
        ]));
        lines.push(Line::from(vec![
            Span::styled("    │ ", border_style),
            Span::styled(" ".repeat(row_width), Style::default()),
            Span::styled(" │", border_style),
        ]));

        if summaries_incomplete {
            lines.push(Line::from(vec![
                Span::styled("    │ ", border_style),
                Span::styled(
                    center_row("Summaries incomplete"),
                    Style::default().fg(Theme::YELLOW),
                ),
                Span::styled(" │", border_style),
            ]));
            lines.push(Line::from(vec![
                Span::styled("    │ ", border_style),
                Span::styled(
                    center_row(&format!(
                        "{} file(s) failed",
                        app.summary_failed_files.len()
                    )),
                    Style::default().fg(Theme::GREY_400),
                ),
                Span::styled(" │", border_style),
            ]));
            lines.push(Line::from(vec![
                Span::styled("    │ ", border_style),
                Span::styled(
                    center_row("Press R to open Reset Cosmos"),
                    Style::default().fg(Theme::GREY_300),
                ),
                Span::styled(" │", border_style),
            ]));
            lines.push(Line::from(vec![
                Span::styled("    │ ", border_style),
                Span::styled(
                    center_row("Then restart to regenerate"),
                    Style::default().fg(Theme::GREY_500),
                ),
                Span::styled(" │", border_style),
            ]));
        } else if app.suggestion_refinement_in_progress {
            lines.push(Line::from(vec![
                Span::styled("    │ ", border_style),
                Span::styled(
                    center_row("Refining suggestions"),
                    Style::default().fg(Theme::ACCENT),
                ),
                Span::styled(" │", border_style),
            ]));
            lines.push(Line::from(vec![
                Span::styled("    │ ", border_style),
                Span::styled(
                    center_row(&format!(
                        "{} provisional suggestions in review",
                        app.suggestion_provisional_count
                    )),
                    Style::default().fg(Theme::GREY_500),
                ),
                Span::styled(" │", border_style),
            ]));
        } else if has_ai {
            lines.push(Line::from(vec![
                Span::styled("    │ ", border_style),
                Span::styled(
                    center_row("No issues found"),
                    Style::default().fg(Theme::GREY_300),
                ),
                Span::styled(" │", border_style),
            ]));
            lines.push(Line::from(vec![
                Span::styled("    │ ", border_style),
                Span::styled(
                    center_row("Nothing to suggest"),
                    Style::default().fg(Theme::GREY_500),
                ),
                Span::styled(" │", border_style),
            ]));
        } else {
            lines.push(Line::from(vec![
                Span::styled("    │ ", border_style),
                Span::styled(
                    center_row("AI not configured"),
                    Style::default().fg(Theme::GREY_200),
                ),
                Span::styled(" │", border_style),
            ]));
            lines.push(Line::from(vec![
                Span::styled("    │ ", border_style),
                Span::styled(" ".repeat(row_width), Style::default()),
                Span::styled(" │", border_style),
            ]));
            lines.push(Line::from(vec![
                Span::styled("    │ ", border_style),
                Span::styled(
                    center_row("Press k for setup guide"),
                    Style::default().fg(Theme::GREY_300),
                ),
                Span::styled(" │", border_style),
            ]));
            lines.push(Line::from(vec![
                Span::styled("    │ ", border_style),
                Span::styled(
                    center_row("Suggestions unlock after setup"),
                    Style::default().fg(Theme::GREY_500),
                ),
                Span::styled(" │", border_style),
            ]));
        }

        lines.push(Line::from(vec![
            Span::styled("    │ ", border_style),
            Span::styled(" ".repeat(row_width), Style::default()),
            Span::styled(" │", border_style),
        ]));
        lines.push(Line::from(vec![
            Span::styled("    ╰", border_style),
            Span::styled("─".repeat(rule_width), border_style),
            Span::styled("╯", border_style),
        ]));
        return;
    }

    let mut line_count = 0;
    // Use nearly full width - just leave small margin
    let text_width = inner_width.saturating_sub(4);

    for (i, suggestion) in suggestions.iter().enumerate().skip(app.suggestion_scroll) {
        if line_count >= visible_height.saturating_sub(4) {
            break;
        }

        let is_selected = i == app.suggestion_selected && is_active;

        // Kind label with subtle styling - brighter when selected
        let kind_label = suggestion.kind.label();
        let kind_style = if is_selected {
            Style::default().fg(Theme::GREY_100)
        } else {
            Style::default().fg(Theme::GREY_500)
        };

        // Multi-file indicator
        let multi_file_indicator = if suggestion.is_multi_file() {
            format!(" [{}]", suggestion.file_count())
        } else {
            String::new()
        };
        let multi_file_style = Style::default().fg(Theme::ACCENT);

        // Summary text style - selection via styling only (bold + bright)
        let summary_style = if is_selected {
            Style::default()
                .fg(Theme::WHITE)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Theme::GREY_300)
        };

        // First line has: padding (2) + kind + multi-file + ": "
        let first_prefix_len = 2 + kind_label.len() + multi_file_indicator.len() + 2;
        let first_line_width = text_width.saturating_sub(first_prefix_len);
        // Continuation lines just have small indent (5 chars)
        let cont_indent = "     ";
        let cont_line_width = text_width.saturating_sub(5);

        // Use variable width wrapping: first line is shorter due to prefix
        let wrapped =
            wrap_text_variable_width(&suggestion.summary, first_line_width, cont_line_width);

        // Render first line with kind and multi-file indicator
        if let Some(first_line) = wrapped.first() {
            let mut spans = vec![
                Span::styled("  ", Style::default()),
                Span::styled(kind_label, kind_style),
            ];
            if suggestion.is_multi_file() {
                spans.push(Span::styled(multi_file_indicator, multi_file_style));
            }
            spans.push(Span::styled(": ", kind_style));
            spans.push(Span::styled(first_line.clone(), summary_style));
            lines.push(Line::from(spans));
            line_count += 1;
        }

        // Render ALL continuation lines (no artificial limit)
        for wrapped_line in wrapped.iter().skip(1) {
            if line_count >= visible_height.saturating_sub(4) {
                break;
            }
            lines.push(Line::from(vec![
                Span::styled(cont_indent, Style::default()),
                Span::styled(wrapped_line.clone(), summary_style),
            ]));
            line_count += 1;
        }

        // Add empty line for spacing between suggestions
        if line_count < visible_height.saturating_sub(4) {
            lines.push(Line::from(""));
            line_count += 1;
        }
    }

    // Bottom hints
    let content_lines = lines.len();
    let available = visible_height;
    if content_lines < available {
        for _ in 0..(available - content_lines).saturating_sub(2) {
            lines.push(Line::from(""));
        }
    }

    // Show scroll indicator
    if suggestions.len() > 3 {
        lines.push(Line::from(vec![Span::styled(
            format!("  ↕ {}/{}", app.suggestion_selected + 1, suggestions.len()),
            Style::default().fg(Theme::GREY_500),
        )]));
    }
}

/// Render the Review step content  
fn render_review_content<'a>(
    lines: &mut Vec<Line<'a>>,
    app: &'a App,
    visible_height: usize,
    inner_width: usize,
) {
    let state = &app.review_state;

    if state.reviewing {
        let spinner = SPINNER_FRAMES[app.loading_frame % SPINNER_FRAMES.len()];
        lines.push(Line::from(""));
        lines.push(Line::from(vec![
            Span::styled("    ", Style::default()),
            Span::styled(format!("{} ", spinner), Style::default().fg(Theme::WHITE)),
            Span::styled(
                "Reviewing your changes...",
                Style::default().fg(Theme::GREY_300),
            ),
        ]));
        return;
    }

    if state.fixing {
        let spinner = SPINNER_FRAMES[app.loading_frame % SPINNER_FRAMES.len()];
        lines.push(Line::from(""));
        lines.push(Line::from(vec![
            Span::styled("    ", Style::default()),
            Span::styled(format!("{} ", spinner), Style::default().fg(Theme::WHITE)),
            Span::styled("Applying fixes...", Style::default().fg(Theme::GREY_300)),
        ]));
        return;
    }

    let file_name = state
        .files
        .first()
        .map(|f| f.path.as_path())
        .and_then(|p| p.file_name())
        .and_then(|n| n.to_str())
        .unwrap_or("file");

    // Header: file name with optional round indicator (no "Review" label - shown in workflow breadcrumb)
    lines.push(Line::from(vec![
        Span::styled(
            format!("  {}", file_name),
            Style::default()
                .fg(Theme::WHITE)
                .add_modifier(Modifier::BOLD),
        ),
        if state.files.len() > 1 {
            Span::styled(
                format!(" (+{} files)", state.files.len().saturating_sub(1)),
                Style::default().fg(Theme::GREY_400),
            )
        } else {
            Span::styled("", Style::default())
        },
        if state.review_iteration > 1 {
            Span::styled(
                format!(" (round {})", state.review_iteration),
                Style::default().fg(Theme::GREY_400),
            )
        } else {
            Span::styled("", Style::default())
        },
    ]));
    lines.push(Line::from(""));

    // Check if review reached a terminal state (pass or verification failure)
    if state.findings.is_empty() && !state.summary.is_empty() {
        if state.verification_failed {
            lines.push(Line::from(vec![
                Span::styled("  ! ", Style::default().fg(Theme::YELLOW)),
                Span::styled(
                    "Verification failed",
                    Style::default()
                        .fg(Theme::YELLOW)
                        .add_modifier(Modifier::BOLD),
                ),
            ]));
        } else {
            lines.push(Line::from(vec![
                Span::styled("  + ", Style::default().fg(Theme::GREEN)),
                Span::styled(
                    "No issues found!",
                    Style::default()
                        .fg(Theme::GREEN)
                        .add_modifier(Modifier::BOLD),
                ),
            ]));
        }
        lines.push(Line::from(""));

        let text_width = inner_width.saturating_sub(6);
        for line in wrap_text(&state.summary, text_width) {
            lines.push(Line::from(vec![Span::styled(
                format!("  {}", line),
                Style::default().fg(Theme::GREY_300),
            )]));
        }
        if state.verification_failed {
            if let Some(err) = state.verification_error.as_ref() {
                lines.push(Line::from(""));
                for line in wrap_text(&format!("  {}", err), text_width) {
                    lines.push(Line::from(vec![Span::styled(
                        line,
                        Style::default().fg(Theme::GREY_500),
                    )]));
                }
            }
        }

        // Action to continue to ship
        let content_lines = lines.len();
        if content_lines < visible_height {
            for _ in 0..(visible_height - content_lines).saturating_sub(3) {
                lines.push(Line::from(""));
            }
        }

        lines.push(Line::from(vec![Span::styled(
            "  ─────────────────────────────────",
            Style::default().fg(Theme::GREY_700),
        )]));
        lines.push(Line::from(""));
        lines.push(Line::from(vec![
            Span::styled("  ", Style::default()),
            Span::styled(
                " ↵ ",
                Style::default()
                    .fg(Theme::GREY_900)
                    .bg(if state.verification_failed {
                        Theme::YELLOW
                    } else {
                        Theme::GREEN
                    }),
            ),
            Span::styled(
                if state.verification_failed {
                    if state.confirm_ship {
                        " Ship with Override"
                    } else {
                        " Arm Ship Override"
                    }
                } else {
                    " Continue to Ship"
                },
                Style::default().fg(Theme::GREY_300),
            ),
        ]));
        return;
    }

    // Show findings with a clear, readable layout
    if !state.findings.is_empty() {
        let selected_count = state.selected.len();
        let total_findings = state.findings.len();
        let text_width = inner_width.saturating_sub(6);

        // Summary line
        lines.push(Line::from(vec![
            Span::styled(
                format!(
                    "  {} issue{} found",
                    total_findings,
                    if total_findings == 1 { "" } else { "s" }
                ),
                Style::default().fg(Theme::WHITE),
            ),
            if selected_count > 0 {
                Span::styled(
                    format!(" · {} to fix", selected_count),
                    Style::default().fg(Theme::GREEN),
                )
            } else {
                Span::styled("", Style::default())
            },
        ]));
        lines.push(Line::from(""));

        // Calculate how much space we have for the selected finding's details
        // Reserve: header (3 lines) + separator (1) + hint (1) + some findings list
        let min_list_height = 3.min(total_findings);
        let detail_budget = visible_height.saturating_sub(6 + min_list_height);

        // Show the currently selected finding in detail first (if any)
        if let Some(current_finding) = state.findings.get(state.cursor) {
            // Severity indicator
            let severity_color = match current_finding.severity.as_str() {
                "critical" => Theme::RED,
                "warning" => Theme::YELLOW,
                _ => Theme::GREY_400,
            };
            let severity_label = match current_finding.severity.as_str() {
                "critical" => "Critical",
                "warning" => "Warning",
                "suggestion" => "Suggestion",
                _ => "Note",
            };

            lines.push(Line::from(vec![
                Span::styled("  ", Style::default()),
                Span::styled(
                    format!(" {} ", severity_label),
                    Style::default().fg(Theme::GREY_900).bg(severity_color),
                ),
            ]));
            lines.push(Line::from(""));

            // Title - prominent and bold
            for title_line in wrap_text(&current_finding.title, text_width) {
                lines.push(Line::from(vec![Span::styled(
                    format!("  {}", title_line),
                    Style::default()
                        .fg(Theme::WHITE)
                        .add_modifier(Modifier::BOLD),
                )]));
            }
            lines.push(Line::from(""));

            // Description - the full explanation, clearly laid out
            if !current_finding.description.is_empty() {
                let desc_lines = wrap_text(&current_finding.description, text_width);
                // Show as many description lines as we have budget for
                let max_desc_lines = detail_budget.saturating_sub(4); // Reserve space for severity + title
                for desc_line in desc_lines.iter().take(max_desc_lines.max(6)) {
                    lines.push(Line::from(vec![Span::styled(
                        format!("  {}", desc_line),
                        Style::default().fg(Theme::GREY_200),
                    )]));
                }
                // If truncated, show indicator
                if desc_lines.len() > max_desc_lines.max(6) {
                    lines.push(Line::from(vec![Span::styled(
                        "  ...",
                        Style::default().fg(Theme::GREY_500),
                    )]));
                }
            }

            // Selection status
            lines.push(Line::from(""));
            let is_selected = state.selected.contains(&state.cursor);
            if is_selected {
                lines.push(Line::from(vec![
                    Span::styled("  ", Style::default()),
                    Span::styled("[×]", Style::default().fg(Theme::GREEN)),
                    Span::styled(" Selected for fixing", Style::default().fg(Theme::GREEN)),
                ]));
            } else {
                lines.push(Line::from(vec![
                    Span::styled("  ", Style::default()),
                    Span::styled("[ ]", Style::default().fg(Theme::GREY_500)),
                    Span::styled(" Not selected", Style::default().fg(Theme::GREY_500)),
                ]));
            }
        }

        // Separator
        lines.push(Line::from(""));
        lines.push(Line::from(vec![Span::styled(
            "  ─────────────────────────────────",
            Style::default().fg(Theme::GREY_700),
        )]));

        // Show list of all findings (compact) if there's more than one
        if total_findings > 1 {
            lines.push(Line::from(vec![Span::styled(
                format!("  All issues ({}/{}):", state.cursor + 1, total_findings),
                Style::default().fg(Theme::GREY_400),
            )]));

            // Show a compact list of all findings
            let remaining_height = visible_height.saturating_sub(lines.len() + 2);
            for (i, finding) in state.findings.iter().enumerate().take(remaining_height) {
                let is_cursor = i == state.cursor;
                let is_selected = state.selected.contains(&i);

                let indicator = if is_cursor { "›" } else { " " };
                let checkbox = if is_selected { "×" } else { " " };

                // Truncate title to fit on one line
                let max_title_len = text_width.saturating_sub(8);
                let title = if finding.title.len() > max_title_len {
                    format!("{}...", &finding.title[..max_title_len.saturating_sub(3)])
                } else {
                    finding.title.clone()
                };

                let title_style = if is_cursor {
                    Style::default().fg(Theme::WHITE)
                } else {
                    Style::default().fg(Theme::GREY_400)
                };

                lines.push(Line::from(vec![
                    Span::styled(
                        format!("  {}", indicator),
                        Style::default().fg(if is_cursor {
                            Theme::WHITE
                        } else {
                            Theme::GREY_700
                        }),
                    ),
                    Span::styled(
                        format!("[{}] ", checkbox),
                        Style::default().fg(if is_selected {
                            Theme::GREEN
                        } else {
                            Theme::GREY_600
                        }),
                    ),
                    Span::styled(title, title_style),
                ]));
            }

            // Scroll hint if needed
            if total_findings > remaining_height {
                lines.push(Line::from(vec![Span::styled(
                    format!(
                        "  ↕ Use ↑↓ to see more ({} hidden)",
                        total_findings - remaining_height
                    ),
                    Style::default().fg(Theme::GREY_500),
                )]));
            }
        }
    }
}

/// Render the Ship step content
fn render_ship_content<'a>(
    lines: &mut Vec<Line<'a>>,
    app: &'a App,
    visible_height: usize,
    inner_width: usize,
) {
    let state = &app.ship_state;
    let text_width = inner_width.saturating_sub(6);

    match state.step {
        ShipStep::Done => {
            // Build scrollable content
            let mut content: Vec<Line<'a>> = Vec::new();

            content.push(Line::from(vec![
                Span::styled("  + ", Style::default().fg(Theme::GREEN)),
                Span::styled(
                    "Pull request created!",
                    Style::default()
                        .fg(Theme::GREEN)
                        .add_modifier(Modifier::BOLD),
                ),
            ]));
            content.push(Line::from(""));

            if let Some(url) = &state.pr_url {
                content.push(Line::from(vec![
                    Span::styled("  ", Style::default()),
                    Span::styled(url.clone(), Style::default().fg(Theme::GREY_300)),
                ]));
                content.push(Line::from(""));
                content.push(Line::from(vec![
                    Span::styled("  Press ", Style::default().fg(Theme::GREY_500)),
                    Span::styled("↵", Style::default().fg(Theme::WHITE)),
                    Span::styled(" to open in browser", Style::default().fg(Theme::GREY_500)),
                ]));
            }

            // Use full visible height for content
            let scrollable_height = visible_height;
            let total_content = content.len();
            let scroll = state.scroll.min(total_content.saturating_sub(1));

            for line in content.into_iter().skip(scroll).take(scrollable_height) {
                lines.push(line);
            }
        }
        ShipStep::Committing => {
            lines.push(Line::from(""));
            lines.push(Line::from(vec![
                Span::styled("  ⠋ ", Style::default().fg(Theme::WHITE)),
                Span::styled(
                    "Committing changes...",
                    Style::default().fg(Theme::GREY_300),
                ),
            ]));
        }
        ShipStep::Pushing => {
            lines.push(Line::from(""));
            lines.push(Line::from(vec![
                Span::styled("  + ", Style::default().fg(Theme::GREEN)),
                Span::styled("Committed", Style::default().fg(Theme::GREY_400)),
            ]));
            lines.push(Line::from(vec![
                Span::styled("  ⠋ ", Style::default().fg(Theme::WHITE)),
                Span::styled("Pushing to remote...", Style::default().fg(Theme::GREY_300)),
            ]));
        }
        ShipStep::CreatingPR => {
            lines.push(Line::from(""));
            lines.push(Line::from(vec![
                Span::styled("  + ", Style::default().fg(Theme::GREEN)),
                Span::styled("Committed", Style::default().fg(Theme::GREY_400)),
            ]));
            lines.push(Line::from(vec![
                Span::styled("  + ", Style::default().fg(Theme::GREEN)),
                Span::styled("Pushed", Style::default().fg(Theme::GREY_400)),
            ]));
            lines.push(Line::from(vec![
                Span::styled("  ⠋ ", Style::default().fg(Theme::WHITE)),
                Span::styled(
                    "Creating pull request...",
                    Style::default().fg(Theme::GREY_300),
                ),
            ]));
        }
        ShipStep::Confirm => {
            // Build scrollable content
            let mut content: Vec<Line<'a>> = Vec::new();

            // Branch
            content.push(Line::from(vec![
                Span::styled("  Branch: ", Style::default().fg(Theme::GREY_500)),
                Span::styled(state.branch_name.clone(), Style::default().fg(Theme::WHITE)),
            ]));
            content.push(Line::from(""));

            // Files - show all files for scrolling
            content.push(Line::from(vec![Span::styled(
                format!("  {} file(s) to commit:", state.files.len()),
                Style::default().fg(Theme::GREY_400),
            )]));
            for file in state.files.iter() {
                let name = file.file_name().and_then(|n| n.to_str()).unwrap_or("?");
                content.push(Line::from(vec![Span::styled(
                    format!("    • {}", name),
                    Style::default().fg(Theme::GREY_300),
                )]));
            }
            content.push(Line::from(""));

            // Commit message - show full message for scrolling
            content.push(Line::from(vec![Span::styled(
                "  Commit message:",
                Style::default().fg(Theme::GREY_400),
            )]));
            for line in wrap_text(&state.commit_message, text_width) {
                content.push(Line::from(vec![Span::styled(
                    format!("  {}", line),
                    Style::default().fg(Theme::WHITE),
                )]));
            }

            // Use full visible height for scrollable content
            let scrollable_height = visible_height.saturating_sub(2); // Leave room for scroll indicator
            let total_content = content.len();
            let scroll = state.scroll.min(total_content.saturating_sub(1));

            for line in content.into_iter().skip(scroll).take(scrollable_height) {
                lines.push(line);
            }

            // Scroll indicator if needed
            if total_content > scrollable_height {
                while lines.len() < scrollable_height {
                    lines.push(Line::from(""));
                }
                lines.push(Line::from(vec![
                    Span::styled(
                        "  ─────────────────────────────────",
                        Style::default().fg(Theme::GREY_700),
                    ),
                    Span::styled(
                        format!(
                            " ↕ {}/{} ",
                            scroll + 1,
                            total_content.saturating_sub(scrollable_height) + 1
                        ),
                        Style::default().fg(Theme::GREY_500),
                    ),
                ]));
            }
        }
    }
}

fn render_ask_panel(frame: &mut Frame, area: Rect, app: &App) {
    let is_active = app.active_panel == ActivePanel::Ask;

    let border_style = if is_active {
        Style::default().fg(Theme::GREY_300)
    } else {
        Style::default().fg(Theme::GREY_600)
    };

    let content_height = area.height.saturating_sub(2) as usize;
    let inner_width = area.width.saturating_sub(4) as usize;
    let mut lines = vec![];

    if let Some(ask_state) = &app.ask_cosmos_state {
        render_ask_cosmos_content(&mut lines, ask_state, app, content_height, inner_width);
    } else {
        // Always show input + suggested questions by default (no Enter gate/idle state).
        render_question_mode_content(&mut lines, app, content_height, inner_width, is_active);
    }

    let block = Block::default()
        .title(" 𝘢𝘴𝘬 𝘤𝘰𝘴𝘮𝘰𝘴 ")
        .title_style(Style::default().fg(Theme::GREY_200))
        .borders(Borders::ALL)
        .border_style(border_style)
        .style(Style::default().bg(Theme::GREY_800));

    let paragraph = Paragraph::new(lines).block(block);
    frame.render_widget(paragraph, area);
}

/// Render the question input mode content in the right panel
fn render_question_mode_content<'a>(
    lines: &mut Vec<Line<'a>>,
    app: &App,
    visible_height: usize,
    inner_width: usize,
    show_action_hints: bool,
) {
    let text_width = inner_width.saturating_sub(6).max(10);

    lines.push(Line::from(""));

    let cursor = "█";
    let input_line = if app.question_input.is_empty() {
        Line::from(vec![
            Span::styled("  ", Style::default()),
            Span::styled(cursor, Style::default().fg(Theme::WHITE)),
            Span::styled(
                format!(
                    " {}",
                    truncate_with_ellipsis("Type your question...", text_width)
                ),
                Style::default().fg(Theme::GREY_500),
            ),
        ])
    } else {
        let shown = truncate_with_ellipsis(&app.question_input, text_width.saturating_sub(1));
        Line::from(vec![
            Span::styled("  ", Style::default()),
            Span::styled(shown, Style::default().fg(Theme::WHITE)),
            Span::styled(cursor, Style::default().fg(Theme::WHITE)),
        ])
    };
    lines.push(input_line);
    lines.push(Line::from(""));

    if app.ask_in_flight {
        let spinner = SPINNER_FRAMES[app.loading_frame % SPINNER_FRAMES.len()];
        lines.push(Line::from(vec![
            Span::styled("  ", Style::default()),
            Span::styled(format!("{} ", spinner), Style::default().fg(Theme::WHITE)),
            Span::styled("Thinking...", Style::default().fg(Theme::GREY_300)),
        ]));
        lines.push(Line::from(""));
    }

    if app.question_input.is_empty() {
        lines.push(Line::from(vec![Span::styled(
            "  Suggested questions:",
            Style::default().fg(Theme::GREY_400),
        )]));
        lines.push(Line::from(""));

        let total = ASK_STARTER_QUESTIONS.len();
        let selected = app
            .question_suggestion_selected
            .min(total.saturating_sub(1));
        let list_budget = visible_height.saturating_sub(lines.len() + 2);
        let max_items_by_budget = list_budget.saturating_div(2).max(1);
        let (start, end) = suggestion_window(total, selected, max_items_by_budget);
        let mut consumed = 0usize;
        let mut rendered = 0usize;

        for i in start..end {
            let is_selected = i == selected;
            let style = if is_selected {
                Style::default().fg(Theme::WHITE)
            } else {
                Style::default().fg(Theme::GREY_400)
            };
            let wrapped = wrap_text(
                ASK_STARTER_QUESTIONS[i],
                text_width.saturating_sub(4).max(1),
            );
            let needed = wrapped.len().saturating_add(1); // +1 vertical spacer between questions

            if consumed + needed > list_budget {
                break;
            }

            for (line_idx, segment) in wrapped.into_iter().enumerate() {
                let prefix = if line_idx == 0 {
                    if is_selected {
                        " › "
                    } else {
                        "   "
                    }
                } else {
                    "   "
                };
                lines.push(Line::from(vec![
                    Span::styled(prefix, style),
                    Span::styled(segment, style),
                ]));
            }

            lines.push(Line::from(""));
            consumed += needed;
            rendered += 1;
        }

        let hidden_count = total.saturating_sub(start + rendered);
        if hidden_count > 0 && consumed < list_budget {
            lines.push(Line::from(vec![Span::styled(
                format!("   +{} more", hidden_count),
                Style::default().fg(Theme::GREY_500),
            )]));
        } else if matches!(lines.last(), Some(last) if last.spans.is_empty()) {
            lines.pop();
        }
    }

    let footer_lines = usize::from(show_action_hints);
    let remaining = visible_height.saturating_sub(lines.len() + footer_lines);
    for _ in 0..remaining {
        lines.push(Line::from(""));
    }

    if show_action_hints {
        let hint = if app.question_input.is_empty() {
            Line::from(vec![
                Span::styled("  ", Style::default()),
                Span::styled(
                    " ↑↓ ",
                    Style::default().fg(Theme::GREY_900).bg(Theme::GREY_400),
                ),
                Span::styled(" choose ", Style::default().fg(Theme::GREY_400)),
                Span::styled(" ", Style::default()),
                Span::styled(
                    " ↵ ",
                    Style::default().fg(Theme::GREY_900).bg(Theme::GREY_400),
                ),
                Span::styled(" ask ", Style::default().fg(Theme::GREY_400)),
                Span::styled(" ", Style::default()),
                Span::styled(
                    " Esc ",
                    Style::default().fg(Theme::GREY_900).bg(Theme::GREY_400),
                ),
                Span::styled(" cancel ", Style::default().fg(Theme::GREY_400)),
            ])
        } else {
            Line::from(vec![
                Span::styled("  ", Style::default()),
                Span::styled(
                    " ↵ ",
                    Style::default().fg(Theme::GREY_900).bg(Theme::GREY_400),
                ),
                Span::styled(" ask ", Style::default().fg(Theme::GREY_400)),
                Span::styled(" ", Style::default()),
                Span::styled(
                    " Esc ",
                    Style::default().fg(Theme::GREY_900).bg(Theme::GREY_400),
                ),
                Span::styled(" cancel ", Style::default().fg(Theme::GREY_400)),
            ])
        };
        lines.push(hint);
    }
}

fn truncate_with_ellipsis(text: &str, max_chars: usize) -> String {
    if max_chars == 0 {
        return String::new();
    }
    let len = text.chars().count();
    if len <= max_chars {
        return text.to_string();
    }
    if max_chars == 1 {
        return "…".to_string();
    }
    let head: String = text.chars().take(max_chars.saturating_sub(1)).collect();
    format!("{head}…")
}

fn suggestion_window(total: usize, selected: usize, list_height: usize) -> (usize, usize) {
    if total == 0 {
        return (0, 0);
    }
    let clamped_selected = selected.min(total.saturating_sub(1));
    let height = list_height.max(1);
    let start = clamped_selected.saturating_sub(height.saturating_sub(1));
    let end = (start + height).min(total);
    (start, end)
}

/// Render the Ask Cosmos response content in the right panel
fn render_ask_cosmos_content<'a>(
    lines: &mut Vec<Line<'a>>,
    ask_state: &AskCosmosState,
    app: &App,
    visible_height: usize,
    inner_width: usize,
) {
    let _ = app; // silence unused warning

    // Top padding for breathing room (matching other panels)
    lines.push(Line::from(""));

    let text_width = inner_width.saturating_sub(6);
    let response_hash = stable_hash(&ask_state.response);

    let padded_lines = ASK_MARKDOWN_CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        let needs_reparse = cache
            .as_ref()
            .map(|cached| cached.response_hash != response_hash || cached.width != text_width)
            .unwrap_or(true);

        if needs_reparse {
            let parsed_lines = markdown::parse_markdown(&ask_state.response, text_width);
            let mut with_padding = Vec::with_capacity(parsed_lines.len());
            for line in parsed_lines {
                let mut spans = vec![Span::styled("  ", Style::default())];
                spans.extend(line.spans);
                with_padding.push(Line::from(spans));
            }
            *cache = Some(CachedAskMarkdown {
                response_hash,
                width: text_width,
                padded_lines: with_padding,
            });
        }

        cache
            .as_ref()
            .map(|cached| cached.padded_lines.clone())
            .unwrap_or_default()
    });

    // Calculate available height for content
    // Account for: 1 empty top + 1 scroll indicator + 1 empty + 1 hint = 4 lines overhead
    let content_height = visible_height.saturating_sub(4);
    let total_lines = padded_lines.len();
    let scroll = ask_state.scroll.min(total_lines.saturating_sub(1));

    // Render visible content
    for line in padded_lines.iter().skip(scroll).take(content_height) {
        lines.push(line.clone());
    }

    // Scroll indicator (if content exceeds visible area)
    if total_lines > content_height {
        lines.push(Line::from(vec![Span::styled(
            format!(
                "  ↕ {}/{}",
                scroll + 1,
                total_lines.saturating_sub(content_height) + 1
            ),
            Style::default().fg(Theme::GREY_500),
        )]));
    } else {
        lines.push(Line::from(""));
    }

    lines.push(Line::from(""));

    // Action hints at bottom
    lines.push(Line::from(vec![
        Span::styled("  ", Style::default()),
        Span::styled(
            " ↑↓ ",
            Style::default().fg(Theme::GREY_900).bg(Theme::GREY_400),
        ),
        Span::styled(" scroll ", Style::default().fg(Theme::GREY_400)),
        Span::styled("   ", Style::default()),
        Span::styled(
            " Esc ",
            Style::default().fg(Theme::GREY_900).bg(Theme::GREY_400),
        ),
        Span::styled(" back ", Style::default().fg(Theme::GREY_400)),
    ]));
}

fn stable_hash(input: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    input.hash(&mut hasher);
    hasher.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use cosmos_core::context::WorkContext;
    use cosmos_core::index::CodebaseIndex;
    use cosmos_core::suggest::SuggestionEngine;
    use std::collections::HashMap;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn make_test_app() -> App {
        let mut root = std::env::temp_dir();
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        root.push(format!("cosmos_render_main_test_{}", nanos));
        std::fs::create_dir_all(&root).unwrap();

        let index = CodebaseIndex {
            root: root.clone(),
            files: HashMap::new(),
            index_errors: Vec::new(),
            git_head: Some("deadbeef".to_string()),
        };
        let suggestions = SuggestionEngine::new(index.clone());
        let context = WorkContext {
            branch: "main".to_string(),
            uncommitted_files: Vec::new(),
            staged_files: Vec::new(),
            untracked_files: Vec::new(),
            inferred_focus: None,
            modified_count: 0,
            repo_root: root,
        };
        App::new(index, suggestions, context)
    }

    #[test]
    fn ask_width_wide_view_respects_max_cap() {
        let ask_width = compute_ask_panel_width(240);
        assert_eq!(ask_width, ASK_MAX_COLS);
    }

    #[test]
    fn ask_width_medium_view_follows_target_and_min() {
        let ask_width = compute_ask_panel_width(150);
        assert_eq!(ask_width, ASK_MIN_COLS);
    }

    #[test]
    fn ask_width_narrow_view_preserves_suggestions_space_when_possible() {
        let padded_width = 90;
        let ask_width = compute_ask_panel_width(padded_width);
        let available = padded_width.saturating_sub(GAP_COLS);
        let suggestions_width = available.saturating_sub(ask_width);

        assert_eq!(ask_width, 36);
        assert_eq!(suggestions_width, SUGGESTIONS_MIN_COLS);
    }

    #[test]
    fn ask_hints_hidden_when_panel_is_not_active() {
        let app = make_test_app();
        let mut lines = Vec::new();

        render_question_mode_content(&mut lines, &app, 20, 60, false);

        let rendered = lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert!(!rendered.contains(" choose "));
        assert!(!rendered.contains(" ask "));
        assert!(!rendered.contains(" cancel "));
    }

    #[test]
    fn ask_hints_visible_when_panel_is_active() {
        let app = make_test_app();
        let mut lines = Vec::new();

        render_question_mode_content(&mut lines, &app, 20, 60, true);

        let rendered = lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert!(rendered.contains(" choose "));
        assert!(rendered.contains(" ask "));
        assert!(rendered.contains(" cancel "));
    }
}
