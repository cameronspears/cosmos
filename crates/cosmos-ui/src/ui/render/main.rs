use crate::ui::helpers::{wrap_text, wrap_text_variable_width};
use crate::ui::markdown;
use crate::ui::theme::Theme;
use crate::ui::{
    ActivePanel, App, AskCosmosState, InputMode, LoadingState, ShipStep, ViewMode, WorkflowStep,
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
    project_panel: Rect,
    suggestions_panel: Rect,
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

pub(super) fn render_main(frame: &mut Frame, area: Rect, app: &App) {
    let (project_rect, suggestions_rect) = MAIN_LAYOUT_CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();

        // Reuse cached layout if area unchanged
        if let Some(cached) = cache.as_ref() {
            if cached.area == area {
                return (cached.project_panel, cached.suggestions_panel);
            }
        }

        // Recompute layout (only on resize)
        // Add horizontal padding for breathing room
        let padded = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Length(2), // Left padding
                Constraint::Min(10),   // Main content
                Constraint::Length(2), // Right padding
            ])
            .split(area);

        // Split into two panels with gap
        let panels = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Percentage(38), // Project tree
                Constraint::Length(2),      // Gap between panels
                Constraint::Percentage(62), // Suggestions (wider for wrapped text)
            ])
            .split(padded[1]);

        let project_panel = panels[0];
        let suggestions_panel = panels[2];

        // Cache the result
        *cache = Some(CachedMainLayout {
            area,
            project_panel,
            suggestions_panel,
        });

        (project_panel, suggestions_panel)
    });

    render_project_panel(frame, project_rect, app);
    render_suggestions_panel(frame, suggestions_rect, app);
}

fn render_project_panel(frame: &mut Frame, area: Rect, app: &App) {
    let is_active = app.active_panel == ActivePanel::Project;
    let is_searching = app.input_mode == InputMode::Search;

    let border_style = if is_searching {
        Style::default().fg(Theme::WHITE) // Bright border when searching
    } else if is_active {
        Style::default().fg(Theme::GREY_300) // Bright active border
    } else {
        Style::default().fg(Theme::GREY_600) // Visible inactive border
    };

    // Account for search bar if searching
    let search_height = if is_searching || !app.search_query.is_empty() {
        2
    } else {
        0
    };
    let visible_height = area.height.saturating_sub(4 + search_height as u16) as usize;

    let mut lines = vec![];

    // Search bar
    if is_searching || !app.search_query.is_empty() {
        let search_text = if is_searching {
            format!(" / {}_", app.search_query)
        } else {
            format!(" / {} (Esc to clear)", app.search_query)
        };
        lines.push(Line::from(vec![Span::styled(
            search_text,
            Style::default().fg(Theme::WHITE),
        )]));
        lines.push(Line::from(""));
    } else {
        // Top padding for breathing room
        lines.push(Line::from(""));
    }

    // Render based on view mode
    match app.view_mode {
        ViewMode::Flat => {
            render_flat_tree(&mut lines, app, is_active, visible_height);
        }
        ViewMode::Grouped => {
            render_grouped_tree(&mut lines, app, is_active, visible_height);
        }
    }

    // Build title with view/sort indicator
    let total_items = app.project_tree_len();
    let scroll_indicator = if total_items > visible_height {
        let current = app.project_scroll + 1;
        format!(" ↕ {}/{} ", current, total_items)
    } else {
        String::new()
    };

    let mode_indicator = format!(" [{}]", app.view_mode.label());
    let title = format!(
        " {}{}{}",
        Theme::SECTION_PROJECT,
        mode_indicator,
        scroll_indicator
    );

    let block = Block::default()
        .title(title)
        .title_style(Style::default().fg(Theme::GREY_200)) // Legible title
        .borders(Borders::ALL)
        .border_style(border_style)
        .style(Style::default().bg(Theme::GREY_800));

    let paragraph = Paragraph::new(lines).block(block);
    frame.render_widget(paragraph, area);
}

/// Render the flat file tree
fn render_flat_tree<'a>(
    lines: &mut Vec<Line<'a>>,
    app: &'a App,
    is_active: bool,
    visible_height: usize,
) {
    let tree = &app.file_tree;
    let indices = &app.filtered_tree_indices;
    let total = indices.len();

    for (i, entry_idx) in indices
        .iter()
        .enumerate()
        .skip(app.project_scroll)
        .take(visible_height)
    {
        let entry = &tree[*entry_idx];
        let is_selected = i == app.project_selected && is_active;

        // Calculate tree connectors
        let is_last = {
            if i + 1 >= total {
                true
            } else {
                let next_entry = &tree[indices[i + 1]];
                next_entry.depth <= entry.depth
            }
        };

        let connector = if is_last { "└" } else { "├" };
        let indent_str: String = (0..entry.depth.saturating_sub(1))
            .map(|d| {
                // Check if ancestor at this depth has more siblings
                let has_more = indices
                    .iter()
                    .skip(i + 1)
                    .any(|next_idx| tree[*next_idx].depth == d + 1);
                if has_more {
                    "│ "
                } else {
                    "  "
                }
            })
            .collect();

        let (file_icon_str, icon_color) = if entry.is_dir {
            ("▸", Theme::GREY_400)
        } else {
            file_icon(&entry.name)
        };

        // Selection indicated by styling only (no cursor)
        let name_style = if is_selected {
            Style::default()
                .fg(Theme::WHITE)
                .add_modifier(Modifier::BOLD)
        } else if entry.is_dir {
            Style::default().fg(Theme::GREY_300)
        } else if entry.priority == Theme::PRIORITY_HIGH {
            Style::default().fg(Theme::GREY_200)
        } else {
            Style::default().fg(Theme::GREY_500)
        };

        let priority_indicator = if entry.priority == Theme::PRIORITY_HIGH {
            Span::styled(" ●", Style::default().fg(Theme::GREY_300))
        } else {
            Span::styled("", Style::default())
        };

        // Icon styling also reflects selection
        let icon_style = if is_selected {
            Style::default().fg(Theme::WHITE)
        } else {
            Style::default().fg(icon_color)
        };

        if entry.depth == 0 {
            // Root level - no connector
            lines.push(Line::from(vec![
                Span::styled("   ", Style::default()),
                Span::styled(format!("{} ", file_icon_str), icon_style),
                Span::styled(entry.name.as_str(), name_style),
                priority_indicator,
            ]));
        } else {
            lines.push(Line::from(vec![
                Span::styled("   ", Style::default()),
                Span::styled(
                    format!("{}{}", indent_str, connector),
                    Style::default().fg(Theme::GREY_700),
                ),
                Span::styled(format!(" {} ", file_icon_str), icon_style),
                Span::styled(entry.name.as_str(), name_style),
                priority_indicator,
            ]));
        }
    }
}

/// Get file type icon based on extension - minimal and clean
fn file_icon(name: &str) -> (&'static str, ratatui::style::Color) {
    let ext = name.rsplit('.').next().unwrap_or("");
    match ext {
        // React/JSX - subtle blue tint
        "tsx" | "jsx" => ("›", Theme::BADGE_QUALITY),
        // TypeScript - subtle yellow
        "ts" => ("›", Theme::BADGE_DOCS),
        // JavaScript
        "js" | "mjs" | "cjs" => ("›", Theme::BADGE_DOCS),
        // Styles - purple
        "css" | "scss" | "sass" | "less" => ("◈", Theme::BADGE_REFACTOR),
        // Data files - muted
        "json" | "yaml" | "yml" | "toml" => ("○", Theme::GREY_600),
        // Rust - orange
        "rs" => ("●", Theme::BADGE_SECURITY),
        // Python - teal
        "py" => ("●", Theme::BADGE_PERF),
        // Go - blue
        "go" => ("●", Theme::BADGE_QUALITY),
        // Config - very muted
        "env" | "config" => ("○", Theme::GREY_700),
        // Markdown - muted
        "md" | "mdx" => ("○", Theme::GREY_600),
        // Tests - teal indicator
        _ if name.contains("test") || name.contains("spec") => ("◎", Theme::BADGE_PERF),
        // Default - minimal dot
        _ => ("·", Theme::GREY_600),
    }
}

/// Render the grouped file tree
fn render_grouped_tree<'a>(
    lines: &mut Vec<Line<'a>>,
    app: &'a App,
    is_active: bool,
    visible_height: usize,
) {
    use crate::grouping::GroupedEntryKind;

    let tree = &app.grouped_tree;
    let indices = &app.filtered_grouped_indices;

    for (i, entry_idx) in indices
        .iter()
        .enumerate()
        .skip(app.project_scroll)
        .take(visible_height)
    {
        let entry = &tree[*entry_idx];
        let is_selected = i == app.project_selected && is_active;

        match &entry.kind {
            GroupedEntryKind::Layer(_layer) => {
                // Add spacing before layer (except first)
                if i > 0 && app.project_scroll == 0
                    || (i > app.project_scroll && app.project_scroll > 0)
                {
                    // Check if previous visible item was a file - add separator
                    if i > 0 {
                        if let Some(prev_idx) = indices.get(i.saturating_sub(1)) {
                            let prev = &tree[*prev_idx];
                            if prev.kind == GroupedEntryKind::File {
                                lines.push(Line::from(""));
                            }
                        }
                    }
                }

                // Layer header - selection via styling only, expand icon shows state
                let expand_icon = if entry.expanded { "▾" } else { "▸" };
                let count_str = format!(" {}", entry.file_count);

                let (expand_style, name_style, count_style) = if is_selected {
                    (
                        Style::default().fg(Theme::WHITE),
                        Style::default()
                            .fg(Theme::WHITE)
                            .add_modifier(Modifier::BOLD),
                        Style::default().fg(Theme::GREY_200),
                    )
                } else {
                    (
                        Style::default().fg(Theme::GREY_500),
                        Style::default().fg(Theme::GREY_100),
                        Style::default().fg(Theme::GREY_600),
                    )
                };

                lines.push(Line::from(vec![
                    Span::styled("   ", Style::default()),
                    Span::styled(expand_icon.to_string(), expand_style),
                    Span::styled(format!(" {}", entry.name), name_style),
                    Span::styled(count_str, count_style),
                ]));
            }
            GroupedEntryKind::Feature => {
                // Feature header - selection via styling only
                let style = if is_selected {
                    Style::default()
                        .fg(Theme::WHITE)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(Theme::GREY_300)
                };

                let count_str = format!(" {}", entry.file_count);

                lines.push(Line::from(vec![
                    Span::styled("   ", Style::default()),
                    Span::styled("   ├─ ", Style::default().fg(Theme::GREY_700)),
                    Span::styled(entry.name.as_str(), style),
                    Span::styled(count_str, Style::default().fg(Theme::GREY_600)),
                ]));
            }
            GroupedEntryKind::File => {
                // File display - selection via styling only
                let (file_icon_str, icon_color) = file_icon(&entry.name);

                let name_style = if is_selected {
                    Style::default()
                        .fg(Theme::WHITE)
                        .add_modifier(Modifier::BOLD)
                } else if entry.priority == Theme::PRIORITY_HIGH {
                    Style::default().fg(Theme::GREY_200)
                } else {
                    Style::default().fg(Theme::GREY_500)
                };

                let icon_style = if is_selected {
                    Style::default().fg(Theme::WHITE)
                } else {
                    Style::default().fg(icon_color)
                };

                let priority_indicator = if entry.priority == Theme::PRIORITY_HIGH {
                    Span::styled(" ●", Style::default().fg(Theme::GREY_400))
                } else {
                    Span::styled("", Style::default())
                };

                // Simple indentation with subtle vertical guide
                let indent = "     │  ";

                lines.push(Line::from(vec![
                    Span::styled("   ", Style::default()),
                    Span::styled(indent.to_string(), Style::default().fg(Theme::GREY_800)),
                    Span::styled(format!("{} ", file_icon_str), icon_style),
                    Span::styled(entry.name.as_str(), name_style),
                    priority_indicator,
                ]));
            }
        }
    }
}

fn render_suggestions_panel(frame: &mut Frame, area: Rect, app: &App) {
    let is_active = app.active_panel == ActivePanel::Suggestions;
    let is_question_mode = app.input_mode == InputMode::Question;

    let border_style = if is_question_mode {
        Style::default().fg(Theme::WHITE) // Bright border when in question mode
    } else if is_active {
        Style::default().fg(Theme::GREY_300)
    } else {
        Style::default().fg(Theme::GREY_600)
    };

    // Reserve space for border (2 lines top/bottom)
    let visible_height = area.height.saturating_sub(2) as usize;
    let inner_width = area.width.saturating_sub(4) as usize;

    let mut lines = vec![];

    // Question input mode takes highest priority
    if is_question_mode {
        render_question_mode_content(&mut lines, app, visible_height);
    } else if let Some(ask_state) = &app.ask_cosmos_state {
        // Ask cosmos response display
        render_ask_cosmos_content(&mut lines, ask_state, app, visible_height, inner_width);
    } else if app.loading == LoadingState::Answering {
        render_ask_cosmos_loading(&mut lines, app);
    } else {
        // Render content based on workflow step
        match app.workflow_step {
            WorkflowStep::Suggestions => {
                render_suggestions_content(&mut lines, app, is_active, visible_height, inner_width);
            }
            WorkflowStep::Review => {
                render_review_content(&mut lines, app, visible_height, inner_width);
            }
            WorkflowStep::Ship => {
                render_ship_content(&mut lines, app, visible_height, inner_width);
            }
        }
    }

    // Build title with workflow breadcrumbs in the border (italic, lowercase like project panel)
    let ask_cosmos_active = is_question_mode
        || app.ask_cosmos_state.is_some()
        || app.loading == LoadingState::Answering;
    let title = render_workflow_title(app.workflow_step, ask_cosmos_active);

    let block = Block::default()
        .title(title)
        .title_style(Style::default().fg(Theme::GREY_200))
        .borders(Borders::ALL)
        .border_style(border_style)
        .style(Style::default().bg(Theme::GREY_800));

    let paragraph = Paragraph::new(lines).block(block);
    frame.render_widget(paragraph, area);
}

/// Build the workflow title for the border (italic, lowercase like project panel)
fn render_workflow_title(current: WorkflowStep, ask_cosmos_active: bool) -> String {
    // When in ask cosmos mode, show simple title (italicized like other panels)
    if ask_cosmos_active {
        return " 𝘢𝘴𝘬 𝘤𝘰𝘴𝘮𝘰𝘴 ".to_string();
    }

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
    use crate::suggest::{Confidence, Priority};

    let suggestions = app.suggestions.active_suggestions();

    // Top padding for breathing room (matching project panel)
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
        LoadingState::Answering => Some("Thinking...".to_string()),
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
        let has_ai = crate::suggest::llm::is_available();
        let summaries_incomplete =
            app.needs_summary_generation && !app.summary_failed_files.is_empty();

        lines.push(Line::from(vec![
            Span::styled("    ╭", Style::default().fg(Theme::GREY_700)),
            Span::styled(
                "──────────────────────────────────",
                Style::default().fg(Theme::GREY_700),
            ),
            Span::styled("╮", Style::default().fg(Theme::GREY_700)),
        ]));
        lines.push(Line::from(vec![
            Span::styled("    │", Style::default().fg(Theme::GREY_700)),
            Span::styled("                                  ", Style::default()),
            Span::styled("│", Style::default().fg(Theme::GREY_700)),
        ]));

        if summaries_incomplete {
            lines.push(Line::from(vec![
                Span::styled("    │", Style::default().fg(Theme::GREY_700)),
                Span::styled("       ! ", Style::default().fg(Theme::YELLOW)),
                Span::styled("Summaries incomplete", Style::default().fg(Theme::GREY_300)),
                Span::styled("      │", Style::default().fg(Theme::GREY_700)),
            ]));
            lines.push(Line::from(vec![
                Span::styled("    │", Style::default().fg(Theme::GREY_700)),
                Span::styled(
                    format!("         {} file(s) failed", app.summary_failed_files.len()),
                    Style::default().fg(Theme::GREY_500),
                ),
                Span::styled("      │", Style::default().fg(Theme::GREY_700)),
            ]));
            lines.push(Line::from(vec![
                Span::styled("    │", Style::default().fg(Theme::GREY_700)),
                Span::styled(
                    "   Press R to open Reset Cosmos",
                    Style::default().fg(Theme::GREY_500),
                ),
                Span::styled("   │", Style::default().fg(Theme::GREY_700)),
            ]));
            lines.push(Line::from(vec![
                Span::styled("    │", Style::default().fg(Theme::GREY_700)),
                Span::styled(
                    "   then restart to regenerate",
                    Style::default().fg(Theme::GREY_500),
                ),
                Span::styled("       │", Style::default().fg(Theme::GREY_700)),
            ]));
        } else if app.suggestion_refinement_in_progress {
            lines.push(Line::from(vec![
                Span::styled("    │", Style::default().fg(Theme::GREY_700)),
                Span::styled("       ↻ ", Style::default().fg(Theme::ACCENT)),
                Span::styled("Refining suggestions", Style::default().fg(Theme::GREY_300)),
                Span::styled("      │", Style::default().fg(Theme::GREY_700)),
            ]));
            lines.push(Line::from(vec![
                Span::styled("    │", Style::default().fg(Theme::GREY_700)),
                Span::styled(
                    format!(
                        "         {} provisional suggestions in review",
                        app.suggestion_provisional_count
                    ),
                    Style::default().fg(Theme::GREY_500),
                ),
                Span::styled(" │", Style::default().fg(Theme::GREY_700)),
            ]));
        } else if has_ai {
            lines.push(Line::from(vec![
                Span::styled("    │", Style::default().fg(Theme::GREY_700)),
                Span::styled("       + ", Style::default().fg(Theme::GREEN)),
                Span::styled("No issues found", Style::default().fg(Theme::GREY_300)),
                Span::styled("          │", Style::default().fg(Theme::GREY_700)),
            ]));
            lines.push(Line::from(vec![
                Span::styled("    │", Style::default().fg(Theme::GREY_700)),
                Span::styled(
                    "         Nothing to suggest",
                    Style::default().fg(Theme::GREY_500),
                ),
                Span::styled("       │", Style::default().fg(Theme::GREY_700)),
            ]));
        } else {
            lines.push(Line::from(vec![
                Span::styled("    │", Style::default().fg(Theme::GREY_700)),
                Span::styled("       ☽ ", Style::default().fg(Theme::GREY_400)),
                Span::styled("AI not configured", Style::default().fg(Theme::GREY_300)),
                Span::styled("        │", Style::default().fg(Theme::GREY_700)),
            ]));
            lines.push(Line::from(vec![
                Span::styled("    │", Style::default().fg(Theme::GREY_700)),
                Span::styled("                                  ", Style::default()),
                Span::styled("│", Style::default().fg(Theme::GREY_700)),
            ]));
            lines.push(Line::from(vec![
                Span::styled("    │", Style::default().fg(Theme::GREY_700)),
                Span::styled(
                    "   cosmos --setup    ",
                    Style::default().fg(Theme::GREY_500),
                ),
                Span::styled("(BYOK)", Style::default().fg(Theme::GREY_600)),
                Span::styled("   │", Style::default().fg(Theme::GREY_700)),
            ]));
        }

        lines.push(Line::from(vec![
            Span::styled("    │", Style::default().fg(Theme::GREY_700)),
            Span::styled("                                  ", Style::default()),
            Span::styled("│", Style::default().fg(Theme::GREY_700)),
        ]));
        lines.push(Line::from(vec![
            Span::styled("    ╰", Style::default().fg(Theme::GREY_700)),
            Span::styled(
                "──────────────────────────────────",
                Style::default().fg(Theme::GREY_700),
            ),
            Span::styled("╯", Style::default().fg(Theme::GREY_700)),
        ]));

        render_suggestion_diagnostics(lines, app, inner_width);
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

        // Build priority indicator with red exclamation points for critical items
        let priority_indicator = match suggestion.priority {
            Priority::High => Span::styled(
                "!! ",
                Style::default().fg(Theme::RED).add_modifier(Modifier::BOLD),
            ),
            Priority::Medium => Span::styled("!  ", Style::default().fg(Theme::YELLOW)),
            Priority::Low => Span::styled("   ", Style::default()),
        };

        // Kind label with subtle styling - brighter when selected
        let kind_label = suggestion.kind.label();
        let kind_style = if is_selected {
            Style::default().fg(Theme::GREY_100)
        } else {
            Style::default().fg(Theme::GREY_500)
        };

        let (confidence_label, confidence_style) = match suggestion.confidence {
            Confidence::High => ("verified", Style::default().fg(Theme::GREEN)),
            Confidence::Medium => ("likely", Style::default().fg(Theme::YELLOW)),
            Confidence::Low => ("uncertain", Style::default().fg(Theme::GREY_500)),
        };
        let confidence_prefix = format!(" [{}]", confidence_label);

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

        // First line has: padding (2) + priority (3) + kind + multi-file + confidence + ": "
        let first_prefix_len =
            2 + 3 + kind_label.len() + multi_file_indicator.len() + confidence_prefix.len() + 2;
        let first_line_width = text_width.saturating_sub(first_prefix_len);
        // Continuation lines just have small indent (5 chars)
        let cont_indent = "     ";
        let cont_line_width = text_width.saturating_sub(5);

        // Use variable width wrapping: first line is shorter due to prefix
        let wrapped =
            wrap_text_variable_width(&suggestion.summary, first_line_width, cont_line_width);

        // Render first line with priority, kind, and multi-file indicator
        if let Some(first_line) = wrapped.first() {
            let mut spans = vec![
                Span::styled("  ", Style::default()),
                priority_indicator,
                Span::styled(kind_label, kind_style),
            ];
            if suggestion.is_multi_file() {
                spans.push(Span::styled(multi_file_indicator, multi_file_style));
            }
            spans.push(Span::styled(confidence_prefix, confidence_style));
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

fn render_suggestion_diagnostics<'a>(lines: &mut Vec<Line<'a>>, app: &App, inner_width: usize) {
    let diagnostics = app.last_suggestion_diagnostics.as_ref();
    let last_error = app.last_suggestion_error.as_ref();
    let has_any =
        diagnostics.is_some() || last_error.is_some() || !app.suggestions.suggestions.is_empty();
    if !has_any {
        return;
    }

    let indent = "    ";
    let text_width = inner_width.saturating_sub(6);

    lines.push(Line::from(""));
    lines.push(Line::from(vec![Span::styled(
        format!("{indent}Diagnostics"),
        Style::default().fg(Theme::GREY_500),
    )]));

    if let Some(error) = last_error {
        for line in wrap_text(&format!("Last error: {}", error), text_width) {
            lines.push(Line::from(vec![Span::styled(
                format!("{indent}{line}"),
                Style::default().fg(Theme::BADGE_BUG),
            )]));
        }
    }

    let total = app.suggestions.suggestions.len();
    if total > 0 {
        let dismissed = app
            .suggestions
            .suggestions
            .iter()
            .filter(|s| s.dismissed)
            .count();
        let applied = app
            .suggestions
            .suggestions
            .iter()
            .filter(|s| s.applied)
            .count();
        let active = total.saturating_sub(dismissed + applied);
        let line = format!(
            "Stored suggestions: total {} · active {} · dismissed {} · applied {}",
            total, active, dismissed, applied
        );
        for line in wrap_text(&line, text_width) {
            lines.push(Line::from(vec![Span::styled(
                format!("{indent}{line}"),
                Style::default().fg(Theme::GREY_400),
            )]));
        }
    }

    if !app.show_suggestion_diagnostics {
        if let Some(diag) = diagnostics {
            let compact = format!(
                "Latest run: {} · gate {} · final {} suggestions · {}ms",
                diag.model,
                if diag.gate_passed { "passed" } else { "missed" },
                diag.final_count,
                diag.attempt_ms
            );
            for line in wrap_text(&compact, text_width) {
                lines.push(Line::from(vec![Span::styled(
                    format!("{indent}{line}"),
                    Style::default().fg(Theme::GREY_500),
                )]));
            }
        }
        lines.push(Line::from(vec![Span::styled(
            format!("{indent}Press d to show technical diagnostics."),
            Style::default().fg(Theme::GREY_600),
        )]));
        return;
    }

    if let Some(diag) = diagnostics {
        let refinement_line = if app.suggestion_refinement_in_progress {
            "Refinement: in progress"
        } else if diag.refinement_complete {
            "Refinement: complete"
        } else {
            "Refinement: pending"
        };
        for line in wrap_text(refinement_line, text_width) {
            lines.push(Line::from(vec![Span::styled(
                format!("{indent}{line}"),
                Style::default().fg(Theme::GREY_400),
            )]));
        }

        let attempts_line = format!(
            "Gate: {} · attempt {}/{} · cost ${:.4} · {}ms",
            if diag.gate_passed { "passed" } else { "missed" },
            diag.attempt_index.max(1),
            diag.attempt_count.max(1),
            diag.attempt_cost_usd,
            diag.attempt_ms
        );
        for line in wrap_text(&attempts_line, text_width) {
            lines.push(Line::from(vec![Span::styled(
                format!("{indent}{line}"),
                Style::default().fg(Theme::GREY_500),
            )]));
        }

        if !diag.gate_fail_reasons.is_empty() {
            let fail_reasons = format!("Gate reasons: {}", diag.gate_fail_reasons.join("; "));
            for line in wrap_text(&fail_reasons, text_width) {
                lines.push(Line::from(vec![Span::styled(
                    format!("{indent}{line}"),
                    Style::default().fg(Theme::YELLOW),
                )]));
            }
        }

        let tool_names = if diag.tool_names.is_empty() {
            "none".to_string()
        } else {
            diag.tool_names.join(", ")
        };

        let meta = format!(
            "Model: {} · {}ms · iterations {} · tool calls {} ({})",
            diag.model, diag.llm_ms, diag.iterations, diag.tool_calls, tool_names
        );
        for line in wrap_text(&meta, text_width) {
            lines.push(Line::from(vec![Span::styled(
                format!("{indent}{line}"),
                Style::default().fg(Theme::GREY_400),
            )]));
        }

        let timing = format!(
            "Timing: evidence pack {}ms · tools {}ms · batch verify {}ms",
            diag.evidence_pack_ms, diag.tool_exec_ms, diag.batch_verify_ms
        );
        for line in wrap_text(&timing, text_width) {
            lines.push(Line::from(vec![Span::styled(
                format!("{indent}{line}"),
                Style::default().fg(Theme::GREY_500),
            )]));
        }

        let path = format!(
            "Forced final: {} · formatting pass: {} · structured output: {} · response healing: {}",
            if diag.forced_final { "yes" } else { "no" },
            if diag.formatting_pass { "yes" } else { "no" },
            if diag.response_format { "yes" } else { "no" },
            if diag.response_healing { "yes" } else { "no" }
        );
        for line in wrap_text(&path, text_width) {
            lines.push(Line::from(vec![Span::styled(
                format!("{indent}{line}"),
                Style::default().fg(Theme::GREY_400),
            )]));
        }

        let counts = format!(
            "Parsed: {} · deduped: {} · grounded removed: {} · low confidence removed: {} · truncated: {} · final: {}",
            diag.raw_count,
            diag.deduped_count,
            diag.grounding_filtered,
            diag.low_confidence_filtered,
            diag.truncated_count,
            diag.final_count
        );
        for line in wrap_text(&counts, text_width) {
            lines.push(Line::from(vec![Span::styled(
                format!("{indent}{line}"),
                Style::default().fg(Theme::GREY_400),
            )]));
        }

        let validation = format!(
            "Validation: provisional {} · validated {} · rejected {} · regen attempts {}",
            diag.provisional_count,
            diag.validated_count,
            diag.rejected_count,
            diag.regeneration_attempts
        );
        for line in wrap_text(&validation, text_width) {
            lines.push(Line::from(vec![Span::styled(
                format!("{indent}{line}"),
                Style::default().fg(Theme::GREY_500),
            )]));
        }

        let pack_mix = format!(
            "Evidence pack: patterns {} · hotspots {} · core {} · line1 {:.0}%",
            diag.pack_pattern_count,
            diag.pack_hotspot_count,
            diag.pack_core_count,
            diag.pack_line1_ratio * 100.0
        );
        for line in wrap_text(&pack_mix, text_width) {
            lines.push(Line::from(vec![Span::styled(
                format!("{indent}{line}"),
                Style::default().fg(Theme::GREY_500),
            )]));
        }

        let sent = format!(
            "Sent to model: {} snippets · ~{} bytes",
            diag.sent_snippet_count, diag.sent_bytes
        );
        for line in wrap_text(&sent, text_width) {
            lines.push(Line::from(vec![Span::styled(
                format!("{indent}{line}"),
                Style::default().fg(Theme::GREY_500),
            )]));
        }

        if diag.batch_verify_attempted > 0 {
            let batch_verify = format!(
                "Batch verify: attempted {} · confirmed {} · not found {} · errors {}",
                diag.batch_verify_attempted,
                diag.batch_verify_verified,
                diag.batch_verify_not_found,
                diag.batch_verify_errors
            );
            for line in wrap_text(&batch_verify, text_width) {
                lines.push(Line::from(vec![Span::styled(
                    format!("{indent}{line}"),
                    Style::default().fg(Theme::GREY_500),
                )]));
            }
        }

        let parse = format!(
            "Parse: {} · markdown stripped: {} · sanitized: {} · json fix: {} · individual parse: {}",
            if diag.parse_strategy.is_empty() {
                "unknown"
            } else {
                diag.parse_strategy.as_str()
            },
            if diag.parse_stripped_markdown { "yes" } else { "no" },
            if diag.parse_used_sanitized_fix { "yes" } else { "no" },
            if diag.parse_used_json_fix { "yes" } else { "no" },
            if diag.parse_used_individual_parse { "yes" } else { "no" }
        );
        for line in wrap_text(&parse, text_width) {
            lines.push(Line::from(vec![Span::styled(
                format!("{indent}{line}"),
                Style::default().fg(Theme::GREY_500),
            )]));
        }

        let response_info = format!("Response chars: {}", diag.response_chars);
        for line in wrap_text(&response_info, text_width) {
            lines.push(Line::from(vec![Span::styled(
                format!("{indent}{line}"),
                Style::default().fg(Theme::GREY_500),
            )]));
        }

        if let Some(precision) = app.rolling_verify_precision {
            let precision_line = format!(
                "Rolling verify precision (last 50): {:.0}%",
                precision * 100.0
            );
            for line in wrap_text(&precision_line, text_width) {
                lines.push(Line::from(vec![Span::styled(
                    format!("{indent}{line}"),
                    Style::default().fg(Theme::GREY_400),
                )]));
            }
        }

        if diag.final_count == 0 && !diag.response_preview.is_empty() {
            let preview = format!("Response preview: {}", diag.response_preview);
            for line in wrap_text(&preview, text_width) {
                lines.push(Line::from(vec![Span::styled(
                    format!("{indent}{line}"),
                    Style::default().fg(Theme::GREY_600),
                )]));
            }
        }
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

/// Render the question input mode content in the right panel
fn render_question_mode_content<'a>(lines: &mut Vec<Line<'a>>, app: &App, visible_height: usize) {
    // Top padding
    lines.push(Line::from(""));

    // Input line with cursor
    let cursor = "█";
    let input_line = if app.question_input.is_empty() {
        Line::from(vec![
            Span::styled("  ", Style::default()),
            Span::styled(cursor, Style::default().fg(Theme::WHITE)),
            Span::styled(
                " Type your question...",
                Style::default().fg(Theme::GREY_500),
            ),
        ])
    } else {
        Line::from(vec![
            Span::styled("  ", Style::default()),
            Span::styled(
                app.question_input.clone(),
                Style::default().fg(Theme::WHITE),
            ),
            Span::styled(cursor, Style::default().fg(Theme::WHITE)),
        ])
    };
    lines.push(input_line);

    lines.push(Line::from(""));

    // Show suggested questions when input is empty
    if app.question_input.is_empty() && !app.question_suggestions.is_empty() {
        lines.push(Line::from(vec![Span::styled(
            "  Suggested questions:",
            Style::default().fg(Theme::GREY_400),
        )]));
        lines.push(Line::from(""));

        for (i, suggestion) in app.question_suggestions.iter().enumerate() {
            let is_selected = i == app.question_suggestion_selected;

            let (prefix, style) = if is_selected {
                (" › ", Style::default().fg(Theme::WHITE))
            } else {
                ("   ", Style::default().fg(Theme::GREY_400))
            };

            lines.push(Line::from(vec![
                Span::styled(prefix, style),
                Span::styled(suggestion.clone(), style),
            ]));
        }
    }

    // Fill remaining space and add hints at bottom
    let used_lines = lines.len();
    let remaining = visible_height.saturating_sub(used_lines + 2);
    for _ in 0..remaining {
        lines.push(Line::from(""));
    }

    // Action hints
    let hint = if app.question_input.is_empty() {
        Line::from(vec![
            Span::styled("  ", Style::default()),
            Span::styled(
                " ↑↓ ",
                Style::default().fg(Theme::GREY_900).bg(Theme::GREY_400),
            ),
            Span::styled(" browse ", Style::default().fg(Theme::GREY_400)),
            Span::styled("   ", Style::default()),
            Span::styled(
                " ↵ ",
                Style::default().fg(Theme::GREY_900).bg(Theme::GREY_400),
            ),
            Span::styled(" ask ", Style::default().fg(Theme::GREY_400)),
            Span::styled("   ", Style::default()),
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
            Span::styled("   ", Style::default()),
            Span::styled(
                " Esc ",
                Style::default().fg(Theme::GREY_900).bg(Theme::GREY_400),
            ),
            Span::styled(" cancel ", Style::default().fg(Theme::GREY_400)),
        ])
    };
    lines.push(hint);
}

/// Render the loading state for Ask Cosmos
fn render_ask_cosmos_loading<'a>(lines: &mut Vec<Line<'a>>, app: &App) {
    lines.push(Line::from(""));

    let spinner = SPINNER_FRAMES[app.loading_frame % SPINNER_FRAMES.len()];
    lines.push(Line::from(vec![
        Span::styled("    ", Style::default()),
        Span::styled(format!("{} ", spinner), Style::default().fg(Theme::WHITE)),
        Span::styled("Thinking...", Style::default().fg(Theme::GREY_300)),
    ]));

    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        Span::styled("    ", Style::default()),
        Span::styled(
            "Esc",
            Style::default().fg(Theme::GREY_900).bg(Theme::GREY_500),
        ),
        Span::styled(" cancel", Style::default().fg(Theme::GREY_500)),
    ]));
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
