use crate::ui::theme::Theme;
use crate::ui::{ActivePanel, App, LoadingState, ShipStep, WorkflowStep};
use ratatui::{
    layout::Rect,
    style::Style,
    text::{Line, Span},
    widgets::Paragraph,
    Frame,
};
use std::cell::RefCell;

#[derive(Clone, PartialEq, Eq)]
struct FooterCacheKey {
    available_width: usize,
    project_name: String,
    branch_display: String,
    stale: bool,
    balance_text: String,
    cost_suffix: String,
    active_panel: ActivePanel,
    workflow_step: WorkflowStep,
    loading: LoadingState,
    verify_loading: bool,
    verify_has_preview: bool,
    verify_show_details: bool,
    review_passed: bool,
    review_verification_failed: bool,
    ship_step: ShipStep,
    has_pending_changes: bool,
    has_update_available: bool,
    ai_available: bool,
}

thread_local! {
    static FOOTER_SPANS_CACHE: RefCell<Option<(FooterCacheKey, Vec<Span<'static>>)>> = const { RefCell::new(None) };
}

/// A footer button with its key and label
struct FooterButton {
    key: &'static str,
    label: &'static str,
    key_fg: ratatui::style::Color,
    key_bg: ratatui::style::Color,
    label_fg: ratatui::style::Color,
}

impl FooterButton {
    fn new(
        key: &'static str,
        label: &'static str,
        key_fg: ratatui::style::Color,
        key_bg: ratatui::style::Color,
        label_fg: ratatui::style::Color,
    ) -> Self {
        Self {
            key,
            label,
            key_fg,
            key_bg,
            label_fg,
        }
    }

    fn width(&self) -> usize {
        // " key " + " label " + "  " (spacing between buttons)
        self.key.chars().count() + 2 + self.label.chars().count() + 3
    }

    fn to_spans(&self) -> Vec<Span<'static>> {
        vec![
            Span::styled(
                format!(" {} ", self.key),
                Style::default().fg(self.key_fg).bg(self.key_bg),
            ),
            Span::styled(
                format!(" {}  ", self.label),
                Style::default().fg(self.label_fg),
            ),
        ]
    }
}

/// Helper for building a primary action button (green background)
fn primary_button(key: &'static str, label: &'static str) -> FooterButton {
    FooterButton::new(key, label, Theme::GREY_900, Theme::GREEN, Theme::GREY_300)
}

/// Helper for building a secondary action button
fn secondary_button(key: &'static str, label: &'static str) -> FooterButton {
    FooterButton::new(
        key,
        label,
        Theme::GREY_900,
        Theme::GREY_600,
        Theme::GREY_600,
    )
}

/// Helper for building a normal hint button
fn hint_button(key: &'static str, label: &'static str) -> FooterButton {
    FooterButton::new(
        key,
        label,
        Theme::GREY_900,
        Theme::GREY_500,
        Theme::GREY_500,
    )
}

pub(super) fn render_footer(frame: &mut Frame, area: Rect, app: &App) {
    let available_width = area.width as usize;

    // Build status section (left side): project name, branch, cost
    let project_name = app
        .context
        .repo_root
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("unknown");

    let is_on_main = app.is_on_main_branch();
    let branch_color = if is_on_main {
        Theme::GREY_100
    } else {
        Theme::GREEN
    };

    // Calculate status width
    let branch_display = if app.context.branch.len() > 20 {
        format!("{}…", &app.context.branch[..19])
    } else {
        app.context.branch.clone()
    };

    let stale_text = if app.git_refresh_error.is_some() {
        "  status stale"
    } else {
        ""
    };

    // Build balance/cost display: "$12.48 (-$0.02)" format
    // Shows current wallet balance, then session cost in parentheses
    // Note: wallet_balance from API already reflects spending, so we don't subtract session_cost
    let (balance_text, cost_suffix) = match (app.session_cost > 0.0, app.wallet_balance) {
        (true, Some(balance)) => (
            format!("  ${:.2}", balance),
            format!(" (-${:.2})", app.session_cost),
        ),
        (true, None) => (format!("  -${:.2}", app.session_cost), String::new()),
        (false, Some(balance)) => (format!("  ${:.2}", balance), String::new()),
        (false, None) => (String::new(), String::new()),
    };
    let cost_text_len = balance_text.chars().count() + cost_suffix.chars().count();

    // Base status: "  project ⎇ branch"
    let base_status_width = 2 + project_name.chars().count() + 3 + branch_display.chars().count();

    let cache_key = FooterCacheKey {
        available_width,
        project_name: project_name.to_string(),
        branch_display: branch_display.clone(),
        stale: !stale_text.is_empty(),
        balance_text: balance_text.clone(),
        cost_suffix: cost_suffix.clone(),
        active_panel: app.active_panel,
        workflow_step: app.workflow_step,
        loading: app.loading,
        verify_loading: app.verify_state.loading,
        verify_has_preview: app.verify_state.preview.is_some(),
        verify_show_details: app.verify_state.show_technical_details,
        review_passed: app.review_passed(),
        review_verification_failed: app.review_state.verification_failed,
        ship_step: app.ship_state.step,
        has_pending_changes: !app.pending_changes.is_empty(),
        has_update_available: app.update_available.is_some(),
        ai_available: crate::suggest::llm::is_available(),
    };

    if let Some(cached_spans) = FOOTER_SPANS_CACHE.with(|cache| {
        let cache = cache.borrow();
        cache
            .as_ref()
            .and_then(|(cached_key, spans)| (cached_key == &cache_key).then(|| spans.clone()))
    }) {
        let footer_line = Line::from(cached_spans);
        let footer = Paragraph::new(vec![Line::from(""), footer_line])
            .style(Style::default().bg(Theme::GREY_900));
        frame.render_widget(footer, area);
        return;
    }

    // Build button lists by priority
    // Priority 1: Essential (always shown if possible)
    let quit_btn = FooterButton::new(
        "q",
        "quit",
        Theme::GREY_900,
        Theme::GREY_600,
        Theme::GREY_600,
    );
    let help_btn = hint_button("?", "help");

    // Priority 2: Primary action (very important)
    let primary_buttons = get_primary_buttons(app);

    // Priority 3: Secondary actions
    let secondary_buttons = get_secondary_buttons(app);

    // Priority 4: Contextual hints
    let hint_buttons = get_hint_buttons(app);

    // Priority 5: Optional indicators (undo, update)
    let optional_buttons = get_optional_buttons(app);

    // Calculate total button widths
    let essential_width = quit_btn.width() + help_btn.width() + 1; // +1 for trailing space
    let primary_width: usize = primary_buttons.iter().map(|b| b.width()).sum();
    let secondary_width: usize = secondary_buttons.iter().map(|b| b.width()).sum();
    let hint_width: usize = hint_buttons.iter().map(|b| b.width()).sum();
    let optional_width: usize = optional_buttons.iter().map(|b| b.width()).sum();

    // Determine what fits
    // Minimum: essential buttons only
    // Then progressively add: primary -> secondary -> hints -> optional -> status

    let mut buttons_to_show: Vec<&FooterButton> = Vec::new();
    let mut used_width = essential_width;

    // Always reserve space for essential buttons
    let remaining_for_content = available_width.saturating_sub(essential_width);

    // Try to fit primary buttons
    if primary_width <= remaining_for_content.saturating_sub(used_width - essential_width) {
        buttons_to_show.extend(primary_buttons.iter());
        used_width += primary_width;
    }

    // Try to fit secondary buttons
    if secondary_width > 0
        && used_width + secondary_width
            <= available_width.saturating_sub(essential_width) + essential_width
    {
        buttons_to_show.extend(secondary_buttons.iter());
        used_width += secondary_width;
    }

    // Try to fit hint buttons
    if hint_width > 0 && used_width + hint_width <= available_width {
        buttons_to_show.extend(hint_buttons.iter());
        used_width += hint_width;
    }

    // Try to fit optional buttons
    if optional_width > 0 && used_width + optional_width <= available_width {
        buttons_to_show.extend(optional_buttons.iter());
        used_width += optional_width;
    }

    // Calculate remaining space for status
    let space_for_status = available_width.saturating_sub(used_width + 2); // +2 for minimum spacing

    // Build the footer spans
    let mut spans: Vec<Span> = vec![Span::styled("  ", Style::default())];

    // Add status if it fits (progressively truncate)
    if space_for_status >= base_status_width {
        spans.push(Span::styled(
            project_name.to_string(),
            Style::default().fg(Theme::GREY_400),
        ));
        spans.push(Span::styled(" ⎇ ", Style::default().fg(Theme::GREY_500)));

        // Truncate branch name to fit
        let remaining_for_branch =
            space_for_status.saturating_sub(2 + project_name.chars().count() + 3);
        let truncated_branch = if branch_display.chars().count() > remaining_for_branch {
            if remaining_for_branch > 1 {
                format!(
                    "{}…",
                    branch_display
                        .chars()
                        .take(remaining_for_branch.saturating_sub(1))
                        .collect::<String>()
                )
            } else {
                String::new()
            }
        } else {
            branch_display.clone()
        };

        if !truncated_branch.is_empty() {
            spans.push(Span::styled(
                truncated_branch.clone(),
                Style::default().fg(branch_color),
            ));
        }

        // Add stale indicator if it fits
        let current_status_len: usize = spans.iter().map(|s| s.content.chars().count()).sum();
        if !stale_text.is_empty()
            && current_status_len + stale_text.chars().count() <= space_for_status
        {
            spans.push(Span::styled(
                stale_text.to_string(),
                Style::default().fg(Theme::YELLOW),
            ));
        }

        // Add balance/cost if it fits
        let current_status_len: usize = spans.iter().map(|s| s.content.chars().count()).sum();
        if !balance_text.is_empty() && current_status_len + cost_text_len <= space_for_status {
            // Remaining balance in standard color
            spans.push(Span::styled(
                balance_text.clone(),
                Style::default().fg(Theme::GREY_400),
            ));
            // Session cost in dimmer color (if present)
            if !cost_suffix.is_empty() {
                spans.push(Span::styled(
                    cost_suffix.clone(),
                    Style::default().fg(Theme::GREY_500),
                ));
            }
        }
    }

    // Add spacer
    let current_len: usize = spans.iter().map(|s| s.content.chars().count()).sum();
    let spacer_len = available_width.saturating_sub(current_len + used_width);
    if spacer_len > 0 {
        spans.push(Span::styled(" ".repeat(spacer_len), Style::default()));
    }

    // Add buttons in order
    for btn in buttons_to_show {
        spans.extend(btn.to_spans());
    }

    // Add essential buttons (help, quit)
    spans.extend(help_btn.to_spans());
    spans.extend(quit_btn.to_spans());
    spans.push(Span::styled(" ", Style::default()));

    FOOTER_SPANS_CACHE.with(|cache| {
        *cache.borrow_mut() = Some((cache_key, spans.clone()));
    });

    let footer_line = Line::from(spans);

    let footer = Paragraph::new(vec![Line::from(""), footer_line])
        .style(Style::default().bg(Theme::GREY_900));
    frame.render_widget(footer, area);
}

/// Get primary action buttons based on current state
fn get_primary_buttons(app: &App) -> Vec<FooterButton> {
    match app.active_panel {
        ActivePanel::Project => {
            vec![hint_button("↵", "expand")]
        }
        ActivePanel::Suggestions => match app.workflow_step {
            WorkflowStep::Suggestions => {
                if app.suggestion_refinement_in_progress
                    || app.loading == LoadingState::GeneratingFix
                {
                    vec![]
                } else {
                    vec![primary_button("↵", "preview")]
                }
            }
            WorkflowStep::Review => {
                if app.review_passed() {
                    vec![primary_button("↵", "ship")]
                } else if app.review_state.verification_failed {
                    vec![primary_button("↵", "override")]
                } else {
                    vec![primary_button("↵", "fix")]
                }
            }
            WorkflowStep::Ship => match app.ship_state.step {
                ShipStep::Confirm => vec![primary_button("↵", "ship")],
                ShipStep::Done => vec![primary_button("↵", "open PR")],
                _ => vec![],
            },
        },
    }
}

/// Get secondary action buttons based on current state
fn get_secondary_buttons(app: &App) -> Vec<FooterButton> {
    match app.active_panel {
        ActivePanel::Project => vec![],
        ActivePanel::Suggestions => match app.workflow_step {
            WorkflowStep::Suggestions => vec![],
            WorkflowStep::Review => {
                if app.review_passed() || app.review_state.verification_failed {
                    vec![secondary_button("Esc", "back")]
                } else {
                    vec![hint_button("␣", "select"), secondary_button("Esc", "back")]
                }
            }
            WorkflowStep::Ship => match app.ship_state.step {
                ShipStep::Confirm => vec![secondary_button("Esc", "back")],
                ShipStep::Done => vec![secondary_button("Esc", "done")],
                _ => vec![],
            },
        },
    }
}

/// Get hint buttons based on current state (lowest priority contextual hints)
fn get_hint_buttons(app: &App) -> Vec<FooterButton> {
    let mut hints = match app.active_panel {
        ActivePanel::Project => {
            vec![hint_button("/", "search"), hint_button("g", "group")]
        }
        ActivePanel::Suggestions => match app.workflow_step {
            WorkflowStep::Suggestions => {
                let mut hints = vec![
                    hint_button("i", "ask"),
                    hint_button("r", "refresh"),
                    hint_button("x", "dismiss"),
                    hint_button("d", "diag"),
                ];
                if !crate::suggest::llm::is_available() {
                    hints.push(hint_button("k", "API key"));
                }
                hints
            }
            _ => vec![],
        },
    };

    // Always show Tab hint for panel switching (helps new users discover navigation)
    hints.push(hint_button("Tab", "panel"));

    hints
}

/// Get optional indicator buttons (undo, update)
fn get_optional_buttons(app: &App) -> Vec<FooterButton> {
    let mut buttons = Vec::new();

    if !app.pending_changes.is_empty() {
        buttons.push(FooterButton::new(
            "u",
            "undo",
            Theme::GREY_900,
            Theme::YELLOW,
            Theme::GREY_400,
        ));
    }

    if app.update_available.is_some() {
        buttons.push(FooterButton::new(
            "U",
            "update",
            Theme::GREY_900,
            Theme::GREEN,
            Theme::GREEN,
        ));
    }

    buttons
}
