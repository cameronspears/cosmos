pub mod panels;
pub mod theme;

use crate::analysis::{
    AuthorStats, BusFactorRisk, ChurnEntry, DangerZone, DustyFile, FileComplexity, TestCoverage,
    TestSummary, TodoEntry,
};
use crate::history::HistoryEntry;
use crate::mascot::Mascot;
use crate::prompt::{FileContext, IssueType, PromptBuilder};
use crate::score::{HealthScore, RepoMetrics, Trend};
use panels::Panel;
use std::time::Instant;
use theme::Theme;
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect, Alignment},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph, Wrap},
    Frame,
};

/// The active panel in the UI
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ActivePanel {
    #[default]
    DangerZones,
    Hotspots,
    DustyFiles,
    Todos,
    BusFactor,
    Tests,
}

impl ActivePanel {
    pub fn index(&self) -> usize {
        match self {
            ActivePanel::DangerZones => 0,
            ActivePanel::Hotspots => 1,
            ActivePanel::DustyFiles => 2,
            ActivePanel::Todos => 3,
            ActivePanel::BusFactor => 4,
            ActivePanel::Tests => 5,
        }
    }

    pub fn from_index(index: usize) -> Self {
        match index {
            0 => ActivePanel::DangerZones,
            1 => ActivePanel::Hotspots,
            2 => ActivePanel::DustyFiles,
            3 => ActivePanel::Todos,
            4 => ActivePanel::BusFactor,
            5 => ActivePanel::Tests,
            _ => ActivePanel::DangerZones,
        }
    }

    pub fn name(&self) -> &'static str {
        match self {
            ActivePanel::DangerZones => "Danger Zones",
            ActivePanel::Hotspots => "Hotspots", 
            ActivePanel::DustyFiles => "Dusty Files",
            ActivePanel::Todos => "TODOs",
            ActivePanel::BusFactor => "Bus Factor",
            ActivePanel::Tests => "Tests",
        }
    }

    pub fn icon(&self) -> &'static str {
        match self {
            ActivePanel::DangerZones => "◆",
            ActivePanel::Hotspots => "●",
            ActivePanel::DustyFiles => "○",
            ActivePanel::Todos => "▸",
            ActivePanel::BusFactor => "◐",
            ActivePanel::Tests => "◇",
        }
    }

    pub fn count() -> usize {
        6
    }
}

/// UI overlay state
#[derive(Debug, Clone, PartialEq, Default)]
pub enum Overlay {
    #[default]
    None,
    Help,
    FileDetail,
    PromptCopied(String),
    AiChat { content: String, scroll: usize },
    DiffPreview {
        file_path: String,
        diff: crate::diff::UnifiedDiff,
        scroll: usize,
        /// Original file content for potential restore
        original_content: String,
    },
    TestResults {
        passed: bool,
        output: String,
        scroll: usize,
    },
    ReviewResults {
        result: crate::ai::ReviewResult,
        scroll: usize,
    },
    InputPrompt {
        title: String,
        prompt: String,
        input: String,
        action: InputAction,
    },
}

/// Actions that can be taken after an input prompt
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputAction {
    CreateBranch,
    CommitMessage,
}

/// Toast notification
pub struct Toast {
    pub message: String,
    pub created_at: Instant,
}

impl Toast {
    pub fn new(message: &str) -> Self {
        Self {
            message: message.to_string(),
            created_at: Instant::now(),
        }
    }

    pub fn is_expired(&self) -> bool {
        self.created_at.elapsed().as_secs() >= 3
    }
}

/// Main application state
pub struct App {
    pub score: HealthScore,
    pub metrics: RepoMetrics,
    pub repo_name: String,
    pub branch_name: String,
    pub repo_path: std::path::PathBuf,
    pub churn_entries: Vec<ChurnEntry>,
    pub dusty_files: Vec<DustyFile>,
    pub todo_entries: Vec<TodoEntry>,
    pub danger_zones: Vec<DangerZone>,
    pub bus_factor_risks: Vec<BusFactorRisk>,
    pub author_stats: Option<AuthorStats>,
    pub test_coverages: Vec<TestCoverage>,
    pub test_summary: Option<TestSummary>,
    pub complexity_entries: Vec<FileComplexity>,
    pub history_entries: Vec<HistoryEntry>,
    pub active_panel: ActivePanel,
    pub scroll_offset: usize,
    pub should_quit: bool,
    pub search_query: String,
    pub search_active: bool,
    pub overlay: Overlay,
    pub selected_index: usize,
    pub prompt_builder: Option<PromptBuilder>,
    pub toast: Option<Toast>,
    pub ai_loading: bool,
    /// Workflow state for fix-and-ship
    pub workflow: crate::workflow::Workflow,
}

impl App {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        score: HealthScore,
        metrics: RepoMetrics,
        repo_name: String,
        branch_name: String,
        repo_path: std::path::PathBuf,
        churn_entries: Vec<ChurnEntry>,
        dusty_files: Vec<DustyFile>,
        todo_entries: Vec<TodoEntry>,
        danger_zones: Vec<DangerZone>,
    ) -> Self {
        Self {
            score,
            metrics,
            repo_name,
            branch_name,
            repo_path,
            churn_entries,
            dusty_files,
            todo_entries,
            danger_zones,
            bus_factor_risks: Vec::new(),
            author_stats: None,
            test_coverages: Vec::new(),
            test_summary: None,
            complexity_entries: Vec::new(),
            history_entries: Vec::new(),
            active_panel: ActivePanel::default(),
            scroll_offset: 0,
            should_quit: false,
            search_query: String::new(),
            search_active: false,
            overlay: Overlay::None,
            selected_index: 0,
            prompt_builder: None,
            toast: None,
            ai_loading: false,
            workflow: crate::workflow::Workflow::new(),
        }
    }

    pub fn with_bus_factor(mut self, risks: Vec<BusFactorRisk>, stats: AuthorStats) -> Self {
        self.bus_factor_risks = risks;
        self.author_stats = Some(stats);
        self
    }

    pub fn with_tests(mut self, coverages: Vec<TestCoverage>, summary: TestSummary) -> Self {
        self.test_coverages = coverages;
        self.test_summary = Some(summary);
        self
    }

    pub fn with_history(mut self, entries: Vec<HistoryEntry>) -> Self {
        self.history_entries = entries;
        self
    }

    pub fn with_complexity(mut self, entries: Vec<FileComplexity>) -> Self {
        self.complexity_entries = entries;
        self
    }

    pub fn with_prompt_builder(mut self, builder: PromptBuilder) -> Self {
        self.prompt_builder = Some(builder);
        self
    }

    pub fn next_panel(&mut self) {
        self.active_panel = ActivePanel::from_index((self.active_panel.index() + 1) % ActivePanel::count());
        self.scroll_offset = 0;
        self.selected_index = 0;
    }

    pub fn prev_panel(&mut self) {
        self.active_panel = ActivePanel::from_index(
            (self.active_panel.index() + ActivePanel::count() - 1) % ActivePanel::count(),
        );
        self.scroll_offset = 0;
        self.selected_index = 0;
    }

    pub fn select_panel(&mut self, index: usize) {
        if index < ActivePanel::count() {
            self.active_panel = ActivePanel::from_index(index);
            self.scroll_offset = 0;
            self.selected_index = 0;
        }
    }

    fn current_panel_len(&self) -> usize {
        match self.active_panel {
            ActivePanel::DangerZones => self.danger_zones.len(),
            ActivePanel::Hotspots => self.churn_entries.len(),
            ActivePanel::DustyFiles => self.dusty_files.len(),
            ActivePanel::Todos => self.todo_entries.len(),
            ActivePanel::BusFactor => self.bus_factor_risks.len(),
            ActivePanel::Tests => self.test_coverages.iter().filter(|t| !t.has_tests).count(),
        }
    }

    pub fn scroll_down(&mut self) {
        let max = self.current_panel_len().saturating_sub(1);
        self.selected_index = (self.selected_index + 1).min(max);
        // Keep selection visible
        if self.selected_index >= self.scroll_offset + 10 {
            self.scroll_offset = self.selected_index.saturating_sub(9);
        }
    }

    pub fn scroll_up(&mut self) {
        self.selected_index = self.selected_index.saturating_sub(1);
        if self.selected_index < self.scroll_offset {
            self.scroll_offset = self.selected_index;
        }
    }

    pub fn toggle_help(&mut self) {
        self.overlay = match self.overlay {
            Overlay::Help => Overlay::None,
            _ => Overlay::Help,
        };
    }

    pub fn show_detail(&mut self) {
        self.overlay = Overlay::FileDetail;
    }

    pub fn close_overlay(&mut self) {
        self.overlay = Overlay::None;
    }

    /// Scroll overlay content down
    pub fn overlay_scroll_down(&mut self) {
        match &mut self.overlay {
            Overlay::AiChat { scroll, content } | Overlay::TestResults { scroll, output: content, .. } => {
                let line_count = content.lines().count();
                if *scroll + 1 < line_count {
                    *scroll += 1;
                }
            }
            Overlay::DiffPreview { scroll, diff, .. } => {
                let line_count = diff.hunks.iter().map(|h| h.lines.len() + 2).sum::<usize>();
                if *scroll + 1 < line_count {
                    *scroll += 1;
                }
            }
            Overlay::ReviewResults { scroll, result } => {
                let line_count = result.issues.len() + result.suggestions.len() + 5;
                if *scroll + 1 < line_count {
                    *scroll += 1;
                }
            }
            _ => {}
        }
    }

    /// Scroll overlay content up
    pub fn overlay_scroll_up(&mut self) {
        match &mut self.overlay {
            Overlay::AiChat { scroll, .. } 
            | Overlay::DiffPreview { scroll, .. }
            | Overlay::TestResults { scroll, .. }
            | Overlay::ReviewResults { scroll, .. } => {
                *scroll = scroll.saturating_sub(1);
            }
            _ => {}
        }
    }

    /// Page down in overlay
    pub fn overlay_page_down(&mut self) {
        match &mut self.overlay {
            Overlay::AiChat { scroll, content } | Overlay::TestResults { scroll, output: content, .. } => {
                let line_count = content.lines().count();
                *scroll = (*scroll + 20).min(line_count.saturating_sub(1));
            }
            Overlay::DiffPreview { scroll, diff, .. } => {
                let line_count = diff.hunks.iter().map(|h| h.lines.len() + 2).sum::<usize>();
                *scroll = (*scroll + 20).min(line_count.saturating_sub(1));
            }
            Overlay::ReviewResults { scroll, result } => {
                let line_count = result.issues.len() + result.suggestions.len() + 5;
                *scroll = (*scroll + 20).min(line_count.saturating_sub(1));
            }
            _ => {}
        }
    }

    /// Page up in overlay
    pub fn overlay_page_up(&mut self) {
        match &mut self.overlay {
            Overlay::AiChat { scroll, .. } 
            | Overlay::DiffPreview { scroll, .. }
            | Overlay::TestResults { scroll, .. }
            | Overlay::ReviewResults { scroll, .. } => {
                *scroll = scroll.saturating_sub(20);
            }
            _ => {}
        }
    }

    /// Handle input for InputPrompt overlay
    pub fn input_char(&mut self, c: char) {
        if let Overlay::InputPrompt { input, .. } = &mut self.overlay {
            input.push(c);
        }
    }

    /// Handle backspace for InputPrompt overlay
    pub fn input_backspace(&mut self) {
        if let Overlay::InputPrompt { input, .. } = &mut self.overlay {
            input.pop();
        }
    }

    /// Get the current input value
    pub fn get_input_value(&self) -> Option<(String, InputAction)> {
        if let Overlay::InputPrompt { input, action, .. } = &self.overlay {
            Some((input.clone(), action.clone()))
        } else {
            None
        }
    }

    pub fn start_search(&mut self) {
        self.search_active = true;
        self.search_query.clear();
    }

    pub fn end_search(&mut self) {
        self.search_active = false;
    }

    pub fn search_input(&mut self, c: char) {
        self.search_query.push(c);
    }

    pub fn search_backspace(&mut self) {
        self.search_query.pop();
    }

    pub fn clear_expired_toast(&mut self) {
        if let Some(ref toast) = self.toast {
            if toast.is_expired() {
                self.toast = None;
            }
        }
    }

    /// Get the currently selected file path
    pub fn selected_file_path(&self) -> Option<String> {
        match self.active_panel {
            ActivePanel::DangerZones => self.danger_zones.get(self.selected_index).map(|d| d.path.clone()),
            ActivePanel::Hotspots => self.churn_entries.get(self.selected_index).map(|c| c.path.clone()),
            ActivePanel::DustyFiles => self.dusty_files.get(self.selected_index).map(|d| d.path.clone()),
            ActivePanel::Todos => self.todo_entries.get(self.selected_index).map(|t| t.path.clone()),
            ActivePanel::BusFactor => self.bus_factor_risks.get(self.selected_index).map(|b| b.path.clone()),
            ActivePanel::Tests => {
                self.test_coverages.iter()
                    .filter(|t| !t.has_tests)
                    .nth(self.selected_index)
                    .map(|t| t.path.clone())
            }
        }
    }

    /// Build a FileContext for the currently selected file
    pub fn build_file_context(&self) -> Option<FileContext> {
        let path = self.selected_file_path()?;
        let mut ctx = FileContext::new(&path);
        
        // Set repo root so file content can be loaded
        ctx.repo_root = Some(self.repo_path.display().to_string());

        if let Some(dz) = self.danger_zones.iter().find(|d| d.path == path) {
            ctx = ctx.with_danger_zone(dz);
        }
        if let Some(churn) = self.churn_entries.iter().find(|c| c.path == path) {
            ctx = ctx.with_churn(churn);
        }
        if let Some(fc) = self.complexity_entries.iter().find(|c| c.path == path) {
            ctx = ctx.with_complexity(fc);
        }
        if let Some(df) = self.dusty_files.iter().find(|d| d.path == path) {
            ctx = ctx.with_dusty(df);
        }
        if let Some(bf) = self.bus_factor_risks.iter().find(|b| b.path == path) {
            ctx = ctx.with_bus_factor(bf);
        }
        if let Some(tc) = self.test_coverages.iter().find(|t| t.path == path) {
            ctx = ctx.with_test_coverage(tc);
        }
        ctx = ctx.with_todos_from_list(&self.todo_entries);

        if ctx.issue_type.is_none() {
            ctx.issue_type = Some(match self.active_panel {
                ActivePanel::DangerZones => IssueType::DangerZone,
                ActivePanel::Hotspots => IssueType::HighChurn,
                ActivePanel::DustyFiles => IssueType::DustyFile,
                ActivePanel::Todos => IssueType::TodoItem,
                ActivePanel::BusFactor => IssueType::BusFactorRisk,
                ActivePanel::Tests => IssueType::MissingTests,
            });
        }

        ctx.load_file_content();
        Some(ctx)
    }

    /// Generate AI prompt and copy to clipboard
    pub fn generate_prompt(&mut self) {
        if let Some(ctx) = self.build_file_context() {
            if let Some(ref mut builder) = self.prompt_builder {
                match builder.generate_and_copy(&ctx) {
                    Ok(prompt) => {
                        let preview: String = prompt.lines().take(5).collect::<Vec<_>>().join("\n");
                        self.overlay = Overlay::PromptCopied(preview);
                    }
                    Err(e) => {
                        self.toast = Some(Toast::new(&format!("Error: {}", e)));
                    }
                }
            }
        }
    }

    /// Copy file path to clipboard
    pub fn copy_path(&mut self) {
        if let Some(path) = self.selected_file_path() {
            if let Some(ref mut builder) = self.prompt_builder {
                match builder.copy_to_clipboard(&path) {
                    Ok(_) => self.toast = Some(Toast::new(&format!("Copied: {}", path))),
                    Err(e) => self.toast = Some(Toast::new(&format!("Error: {}", e))),
                }
            }
        }
    }
}

// ============================================================================
// RENDERING
// ============================================================================

/// Render the entire UI
pub fn render(frame: &mut Frame, app: &App) {
    let area = frame.area();
    
    // Clear with dark background
    frame.render_widget(Block::default().style(Style::default().bg(Theme::BG)), area);

    // Main layout: Header | Content | Footer
    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(7),   // Score header
            Constraint::Length(3),   // Nav tabs  
            Constraint::Min(10),     // Main content
            Constraint::Length(1),   // Footer
        ])
        .split(area);

    render_score_header(frame, layout[0], app);
    render_nav_tabs(frame, layout[1], app);
    render_main_content(frame, layout[2], app);
    render_footer(frame, layout[3], app);

    // Overlays
    match &app.overlay {
        Overlay::Help => render_help(frame),
        Overlay::FileDetail => render_file_detail(frame, app),
        Overlay::PromptCopied(preview) => render_prompt_copied(frame, preview),
        Overlay::AiChat { content, scroll } => render_ai_chat(frame, content, *scroll),
        Overlay::DiffPreview { file_path, diff, scroll, .. } => {
            render_diff_preview(frame, file_path, diff, *scroll)
        }
        Overlay::TestResults { passed, output, scroll } => {
            render_test_results(frame, *passed, output, *scroll)
        }
        Overlay::ReviewResults { result, scroll } => {
            render_review_results(frame, result, *scroll)
        }
        Overlay::InputPrompt { title, prompt, input, .. } => {
            render_input_prompt(frame, title, prompt, input)
        }
        Overlay::None => {}
    }

    // Toast
    if let Some(toast) = &app.toast {
        render_toast(frame, toast);
    }
}

fn render_score_header(frame: &mut Frame, area: Rect, app: &App) {
    let emoji = Mascot::emoji(app.score.value);
    let comment = Mascot::comment(app.score.value);
    
    // Score bar
    let bar_width = 30;
    let filled = (app.score.value as usize * bar_width) / 100;
    let bar: String = (0..bar_width)
        .map(|i| if i < filled { '█' } else { '░' })
        .collect();

    let trend = match app.score.trend {
        Trend::Improving => " ↑",
        Trend::Declining => " ↓",
        Trend::Stable => "",
        Trend::Unknown => "",
    };

    let content = vec![
        Line::from(""),
        Line::from(vec![
            Span::styled("   ", Style::default()),
            Span::styled(
                format!("{} ", app.score.value),
                Style::default().fg(Theme::score_color(app.score.value)).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("({}){}", app.score.grade, trend),
                Style::default().fg(Theme::GREY_300),
            ),
            Span::styled("   ", Style::default()),
            Span::styled(bar, Style::default().fg(Theme::score_color(app.score.value))),
        ]),
        Line::from(vec![
            Span::styled(format!("   \"{}\"", comment), Style::default().fg(Theme::GREY_500).add_modifier(Modifier::ITALIC)),
        ]),
        Line::from(""),
        Line::from(vec![
            Span::styled(format!("   {} @ {}  ", app.repo_name, app.branch_name), Style::default().fg(Theme::GREY_400)),
            Span::styled(format!("{}files  ", app.metrics.total_files), Style::default().fg(Theme::GREY_600)),
            Span::styled(format!("{}loc", app.metrics.total_loc), Style::default().fg(Theme::GREY_600)),
        ]),
    ];

    let block = Paragraph::new(content)
        .style(Style::default().bg(Theme::BG));
    
    frame.render_widget(block, area);
}

fn render_nav_tabs(frame: &mut Frame, area: Rect, app: &App) {
    let panels = [
        (ActivePanel::DangerZones, app.danger_zones.len()),
        (ActivePanel::Hotspots, app.churn_entries.len()),
        (ActivePanel::DustyFiles, app.dusty_files.len()),
        (ActivePanel::Todos, app.todo_entries.len()),
        (ActivePanel::BusFactor, app.bus_factor_risks.len()),
        (ActivePanel::Tests, app.test_coverages.iter().filter(|t| !t.has_tests).count()),
    ];

    let mut spans = vec![Span::styled("   ", Style::default())];
    
    for (i, (panel, count)) in panels.iter().enumerate() {
        let is_active = app.active_panel == *panel;
        let style = if is_active {
            Style::default().fg(Theme::WHITE).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Theme::GREY_500)
        };
        
        spans.push(Span::styled(format!("{}", i + 1), Style::default().fg(Theme::GREY_600)));
        spans.push(Span::styled(
            format!(" {} {} ", panel.icon(), panel.name()),
            style,
        ));
        spans.push(Span::styled(format!("{}", count), Style::default().fg(Theme::GREY_600)));
        spans.push(Span::styled("   ", Style::default()));
    }

    let tabs = Paragraph::new(Line::from(spans))
        .style(Style::default().bg(Theme::BG));
    
    frame.render_widget(tabs, area);
}

fn render_main_content(frame: &mut Frame, area: Rect, app: &App) {
    // Add padding
    let inner = Rect {
        x: area.x + 2,
        y: area.y,
        width: area.width.saturating_sub(4),
        height: area.height,
    };

    match app.active_panel {
        ActivePanel::DangerZones => render_danger_list(frame, inner, app),
        ActivePanel::Hotspots => render_hotspot_list(frame, inner, app),
        ActivePanel::DustyFiles => render_dusty_list(frame, inner, app),
        ActivePanel::Todos => render_todo_list(frame, inner, app),
        ActivePanel::BusFactor => render_bus_factor_list(frame, inner, app),
        ActivePanel::Tests => render_test_list(frame, inner, app),
    }
}

fn render_danger_list(frame: &mut Frame, area: Rect, app: &App) {
    let visible_count = area.height.saturating_sub(2) as usize;
    let items: Vec<Line> = app.danger_zones
        .iter()
        .enumerate()
        .skip(app.scroll_offset)
        .take(visible_count)
        .map(|(i, dz)| {
            let selected = i == app.selected_index;
            let intensity = if dz.danger_score >= 70.0 { "▓▓" } else if dz.danger_score >= 50.0 { "▓░" } else { "░░" };
            
            let path_style = if selected {
                Style::default().fg(Theme::WHITE).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Theme::GREY_200)
            };
            
            let cursor = if selected { "›" } else { " " };
            
            Line::from(vec![
                Span::styled(cursor, Style::default().fg(Theme::WHITE)),
                Span::styled(format!(" {} ", intensity), Style::default().fg(Theme::GREY_400)),
                Span::styled(truncate_path(&dz.path, 50), path_style),
                Span::styled(format!("  {}× ", dz.change_count), Style::default().fg(Theme::GREY_500)),
                Span::styled(format!("c:{:.1}", dz.complexity_score), Style::default().fg(Theme::GREY_600)),
            ])
        })
        .collect();

    let block = Paragraph::new(items).style(Style::default().bg(Theme::BG));
    frame.render_widget(block, area);
}

fn render_hotspot_list(frame: &mut Frame, area: Rect, app: &App) {
    let visible_count = area.height.saturating_sub(2) as usize;
    let max_changes = app.churn_entries.first().map(|c| c.change_count).unwrap_or(1);
    
    let items: Vec<Line> = app.churn_entries
        .iter()
        .enumerate()
        .skip(app.scroll_offset)
        .take(visible_count)
        .map(|(i, c)| {
            let selected = i == app.selected_index;
            let bar_width = 10;
            let filled = (c.change_count * bar_width) / max_changes.max(1);
            let bar: String = (0..bar_width)
                .map(|j| if j < filled { '█' } else { '░' })
                .collect();
            
            let path_style = if selected {
                Style::default().fg(Theme::WHITE).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Theme::GREY_200)
            };
            
            let cursor = if selected { "›" } else { " " };
            
            Line::from(vec![
                Span::styled(cursor, Style::default().fg(Theme::WHITE)),
                Span::styled(format!(" {:>3}× ", c.change_count), Style::default().fg(Theme::GREY_400)),
                Span::styled(bar, Style::default().fg(Theme::GREY_500)),
                Span::styled("  ", Style::default()),
                Span::styled(truncate_path(&c.path, 50), path_style),
            ])
        })
        .collect();

    let block = Paragraph::new(items).style(Style::default().bg(Theme::BG));
    frame.render_widget(block, area);
}

fn render_dusty_list(frame: &mut Frame, area: Rect, app: &App) {
    let visible_count = area.height.saturating_sub(2) as usize;
    
    let items: Vec<Line> = app.dusty_files
        .iter()
        .enumerate()
        .skip(app.scroll_offset)
        .take(visible_count)
        .map(|(i, df)| {
            let selected = i == app.selected_index;
            let dust = if df.days_since_change > 365 { "····" } 
                else if df.days_since_change > 180 { "···" }
                else if df.days_since_change > 90 { "··" }
                else { "·" };
            
            let path_style = if selected {
                Style::default().fg(Theme::WHITE).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Theme::GREY_200)
            };
            
            let cursor = if selected { "›" } else { " " };
            
            Line::from(vec![
                Span::styled(cursor, Style::default().fg(Theme::WHITE)),
                Span::styled(format!(" {:>4} ", dust), Style::default().fg(Theme::GREY_500)),
                Span::styled(truncate_path(&df.path, 50), path_style),
                Span::styled(format!("  {}d ago", df.days_since_change), Style::default().fg(Theme::GREY_600)),
            ])
        })
        .collect();

    let block = Paragraph::new(items).style(Style::default().bg(Theme::BG));
    frame.render_widget(block, area);
}

fn render_todo_list(frame: &mut Frame, area: Rect, app: &App) {
    let visible_count = area.height.saturating_sub(2) as usize;
    
    let items: Vec<Line> = app.todo_entries
        .iter()
        .enumerate()
        .skip(app.scroll_offset)
        .take(visible_count)
        .map(|(i, t)| {
            let selected = i == app.selected_index;
            let kind_style = match t.kind.as_str() {
                "FIXME" => Style::default().fg(Theme::WHITE),
                "HACK" => Style::default().fg(Theme::GREY_200),
                _ => Style::default().fg(Theme::GREY_400),
            };
            
            let path_style = if selected {
                Style::default().fg(Theme::WHITE).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Theme::GREY_300)
            };
            
            let cursor = if selected { "›" } else { " " };
            let text_preview = if t.text.len() > 40 {
                format!("{}...", &t.text[..37])
            } else {
                t.text.clone()
            };
            
            Line::from(vec![
                Span::styled(cursor, Style::default().fg(Theme::WHITE)),
                Span::styled(format!(" {:5} ", t.kind), kind_style),
                Span::styled(truncate_path(&t.path, 30), path_style),
                Span::styled(format!(":{} ", t.line_number), Style::default().fg(Theme::GREY_600)),
                Span::styled(text_preview, Style::default().fg(Theme::GREY_500)),
            ])
        })
        .collect();

    let block = Paragraph::new(items).style(Style::default().bg(Theme::BG));
    frame.render_widget(block, area);
}

fn render_bus_factor_list(frame: &mut Frame, area: Rect, app: &App) {
    let visible_count = area.height.saturating_sub(2) as usize;
    
    let items: Vec<Line> = app.bus_factor_risks
        .iter()
        .enumerate()
        .skip(app.scroll_offset)
        .take(visible_count)
        .map(|(i, bf)| {
            let selected = i == app.selected_index;
            
            let path_style = if selected {
                Style::default().fg(Theme::WHITE).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Theme::GREY_200)
            };
            
            let cursor = if selected { "›" } else { " " };
            
            Line::from(vec![
                Span::styled(cursor, Style::default().fg(Theme::WHITE)),
                Span::styled(format!(" {:>3.0}% ", bf.primary_author_pct), Style::default().fg(Theme::GREY_400)),
                Span::styled(truncate_path(&bf.path, 40), path_style),
                Span::styled(format!("  by {}", truncate_str(&bf.primary_author, 15)), Style::default().fg(Theme::GREY_500)),
            ])
        })
        .collect();

    let block = Paragraph::new(items).style(Style::default().bg(Theme::BG));
    frame.render_widget(block, area);
}

fn render_test_list(frame: &mut Frame, area: Rect, app: &App) {
    let visible_count = area.height.saturating_sub(2) as usize;
    let untested: Vec<_> = app.test_coverages.iter().filter(|t| !t.has_tests).collect();
    
    let items: Vec<Line> = untested
        .iter()
        .enumerate()
        .skip(app.scroll_offset)
        .take(visible_count)
        .map(|(i, tc)| {
            let selected = i == app.selected_index;
            
            let path_style = if selected {
                Style::default().fg(Theme::WHITE).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Theme::GREY_200)
            };
            
            let cursor = if selected { "›" } else { " " };
            
            Line::from(vec![
                Span::styled(cursor, Style::default().fg(Theme::WHITE)),
                Span::styled(" ○ ", Style::default().fg(Theme::GREY_500)),
                Span::styled(truncate_path(&tc.path, 55), path_style),
                Span::styled(format!("  {}loc", tc.source_line_count), Style::default().fg(Theme::GREY_600)),
            ])
        })
        .collect();

    let block = Paragraph::new(items).style(Style::default().bg(Theme::BG));
    frame.render_widget(block, area);
}

fn render_footer(frame: &mut Frame, area: Rect, app: &App) {
    let content = if app.search_active {
        Line::from(vec![
            Span::styled(" /", Style::default().fg(Theme::WHITE)),
            Span::styled(&app.search_query, Style::default().fg(Theme::GREY_200)),
            Span::styled("█", Style::default().fg(Theme::GREY_400)),
        ])
    } else if app.workflow.state.is_active() {
        // Show workflow status
        let status = app.workflow.state.status_text();
        Line::from(vec![
            Span::styled(" 🔧 ", Style::default().fg(Theme::WHITE)),
            Span::styled(status, Style::default().fg(Theme::GREY_300)),
            Span::styled("  ", Style::default()),
            Span::styled("t", Style::default().fg(Theme::GREY_500)),
            Span::styled(" test  ", Style::default().fg(Theme::GREY_600)),
            Span::styled("C", Style::default().fg(Theme::GREY_500)),
            Span::styled(" commit  ", Style::default().fg(Theme::GREY_600)),
            Span::styled("P", Style::default().fg(Theme::GREY_500)),
            Span::styled(" push+PR  ", Style::default().fg(Theme::GREY_600)),
            Span::styled("?", Style::default().fg(Theme::GREY_500)),
            Span::styled(" help", Style::default().fg(Theme::GREY_600)),
        ])
    } else {
        Line::from(vec![
            Span::styled(" a", Style::default().fg(Theme::GREY_500)),
            Span::styled(" fix  ", Style::default().fg(Theme::GREY_600)),
            Span::styled("t", Style::default().fg(Theme::GREY_500)),
            Span::styled(" test  ", Style::default().fg(Theme::GREY_600)),
            Span::styled("d", Style::default().fg(Theme::GREY_500)),
            Span::styled(" diff  ", Style::default().fg(Theme::GREY_600)),
            Span::styled("z", Style::default().fg(Theme::GREY_500)),
            Span::styled(" undo  ", Style::default().fg(Theme::GREY_600)),
            Span::styled("C", Style::default().fg(Theme::GREY_500)),
            Span::styled(" commit  ", Style::default().fg(Theme::GREY_600)),
            Span::styled("P", Style::default().fg(Theme::GREY_500)),
            Span::styled(" PR  ", Style::default().fg(Theme::GREY_600)),
            Span::styled("?", Style::default().fg(Theme::GREY_500)),
            Span::styled(" help", Style::default().fg(Theme::GREY_600)),
        ])
    };

    let footer = Paragraph::new(content).style(Style::default().bg(Theme::BG));
    frame.render_widget(footer, area);
}

// ============================================================================
// OVERLAYS
// ============================================================================

fn render_help(frame: &mut Frame) {
    let area = centered_rect(55, 80, frame.area());
    frame.render_widget(Clear, area);

    let help = vec![
        Line::from(""),
        Line::from(vec![Span::styled("  NAVIGATION", Style::default().fg(Theme::WHITE).add_modifier(Modifier::BOLD))]),
        Line::from(""),
        Line::from(vec![
            Span::styled("  ↑/k ↓/j   ", Style::default().fg(Theme::GREY_300)),
            Span::styled("Move up/down", Style::default().fg(Theme::GREY_500)),
        ]),
        Line::from(vec![
            Span::styled("  1-6       ", Style::default().fg(Theme::GREY_300)),
            Span::styled("Switch panels", Style::default().fg(Theme::GREY_500)),
        ]),
        Line::from(vec![
            Span::styled("  Tab       ", Style::default().fg(Theme::GREY_300)),
            Span::styled("Next panel", Style::default().fg(Theme::GREY_500)),
        ]),
        Line::from(vec![
            Span::styled("  /         ", Style::default().fg(Theme::GREY_300)),
            Span::styled("Search", Style::default().fg(Theme::GREY_500)),
        ]),
        Line::from(""),
        Line::from(vec![Span::styled("  FIX WORKFLOW", Style::default().fg(Theme::WHITE).add_modifier(Modifier::BOLD))]),
        Line::from(""),
        Line::from(vec![
            Span::styled("  a         ", Style::default().fg(Theme::GREY_300)),
            Span::styled("AI fix (generates diff patch)", Style::default().fg(Theme::GREY_500)),
        ]),
        Line::from(vec![
            Span::styled("  t         ", Style::default().fg(Theme::GREY_300)),
            Span::styled("Run tests", Style::default().fg(Theme::GREY_500)),
        ]),
        Line::from(vec![
            Span::styled("  d         ", Style::default().fg(Theme::GREY_300)),
            Span::styled("View git diff", Style::default().fg(Theme::GREY_500)),
        ]),
        Line::from(vec![
            Span::styled("  z         ", Style::default().fg(Theme::GREY_300)),
            Span::styled("Undo/revert file changes", Style::default().fg(Theme::GREY_500)),
        ]),
        Line::from(vec![
            Span::styled("  r         ", Style::default().fg(Theme::GREY_300)),
            Span::styled("AI review (DeepSeek)", Style::default().fg(Theme::GREY_500)),
        ]),
        Line::from(""),
        Line::from(vec![Span::styled("  GIT WORKFLOW", Style::default().fg(Theme::WHITE).add_modifier(Modifier::BOLD))]),
        Line::from(""),
        Line::from(vec![
            Span::styled("  b         ", Style::default().fg(Theme::GREY_300)),
            Span::styled("Create branch", Style::default().fg(Theme::GREY_500)),
        ]),
        Line::from(vec![
            Span::styled("  C         ", Style::default().fg(Theme::GREY_300)),
            Span::styled("Commit changes", Style::default().fg(Theme::GREY_500)),
        ]),
        Line::from(vec![
            Span::styled("  P         ", Style::default().fg(Theme::GREY_300)),
            Span::styled("Push & create PR", Style::default().fg(Theme::GREY_500)),
        ]),
        Line::from(""),
        Line::from(vec![Span::styled("  OTHER", Style::default().fg(Theme::WHITE).add_modifier(Modifier::BOLD))]),
        Line::from(""),
        Line::from(vec![
            Span::styled("  Enter     ", Style::default().fg(Theme::GREY_300)),
            Span::styled("View file details", Style::default().fg(Theme::GREY_500)),
        ]),
        Line::from(vec![
            Span::styled("  p         ", Style::default().fg(Theme::GREY_300)),
            Span::styled("Copy AI prompt to clipboard", Style::default().fg(Theme::GREY_500)),
        ]),
        Line::from(vec![
            Span::styled("  c         ", Style::default().fg(Theme::GREY_300)),
            Span::styled("Copy file path", Style::default().fg(Theme::GREY_500)),
        ]),
        Line::from(vec![
            Span::styled("  Esc       ", Style::default().fg(Theme::GREY_300)),
            Span::styled("Close / Back", Style::default().fg(Theme::GREY_500)),
        ]),
        Line::from(vec![
            Span::styled("  q         ", Style::default().fg(Theme::GREY_300)),
            Span::styled("Quit", Style::default().fg(Theme::GREY_500)),
        ]),
        Line::from(""),
    ];

    let block = Paragraph::new(help)
        .block(Block::default()
            .title(" Help ")
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Theme::GREY_600))
            .style(Style::default().bg(Theme::GREY_800)));
    
    frame.render_widget(block, area);
}

fn render_file_detail(frame: &mut Frame, app: &App) {
    let area = centered_rect(70, 70, frame.area());
    frame.render_widget(Clear, area);

    let path = app.selected_file_path().unwrap_or_else(|| "No file selected".to_string());
    
    let mut lines = vec![
        Line::from(""),
        Line::from(vec![Span::styled(format!("  {}", path), Style::default().fg(Theme::WHITE).add_modifier(Modifier::BOLD))]),
        Line::from(""),
    ];

    // Add metrics based on what data we have
    if let Some(dz) = app.danger_zones.iter().find(|d| d.path == path) {
        lines.push(Line::from(vec![
            Span::styled("  Danger Score: ", Style::default().fg(Theme::GREY_400)),
            Span::styled(format!("{:.0}/100", dz.danger_score), Style::default().fg(Theme::WHITE)),
        ]));
    }
    
    if let Some(c) = app.churn_entries.iter().find(|x| x.path == path) {
        lines.push(Line::from(vec![
            Span::styled("  Changes: ", Style::default().fg(Theme::GREY_400)),
            Span::styled(format!("{}×", c.change_count), Style::default().fg(Theme::GREY_200)),
        ]));
    }

    if let Some(fc) = app.complexity_entries.iter().find(|x| x.path == path) {
        lines.push(Line::from(vec![
            Span::styled("  Lines: ", Style::default().fg(Theme::GREY_400)),
            Span::styled(format!("{}", fc.loc), Style::default().fg(Theme::GREY_200)),
            Span::styled("  Functions: ", Style::default().fg(Theme::GREY_400)),
            Span::styled(format!("{}", fc.function_count), Style::default().fg(Theme::GREY_200)),
        ]));
    }

    if let Some(bf) = app.bus_factor_risks.iter().find(|x| x.path == path) {
        lines.push(Line::from(vec![
            Span::styled("  Primary Author: ", Style::default().fg(Theme::GREY_400)),
            Span::styled(format!("{} ({:.0}%)", bf.primary_author, bf.primary_author_pct), Style::default().fg(Theme::GREY_200)),
        ]));
    }

    if let Some(tc) = app.test_coverages.iter().find(|x| x.path == path) {
        let status = if tc.has_tests { "✓ Has tests" } else { "○ No tests" };
        lines.push(Line::from(vec![
            Span::styled("  Tests: ", Style::default().fg(Theme::GREY_400)),
            Span::styled(status, Style::default().fg(if tc.has_tests { Theme::GREY_300 } else { Theme::GREY_500 })),
        ]));
    }

    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        Span::styled("  Press ", Style::default().fg(Theme::GREY_600)),
        Span::styled("p", Style::default().fg(Theme::GREY_400)),
        Span::styled(" to copy AI prompt  ", Style::default().fg(Theme::GREY_600)),
        Span::styled("a", Style::default().fg(Theme::GREY_400)),
        Span::styled(" to ask AI for fix", Style::default().fg(Theme::GREY_600)),
    ]));
    lines.push(Line::from(""));

    let block = Paragraph::new(lines)
        .block(Block::default()
            .title(" File Details ")
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Theme::GREY_600))
            .style(Style::default().bg(Theme::GREY_800)));
    
    frame.render_widget(block, area);
}

fn render_prompt_copied(frame: &mut Frame, preview: &str) {
    let area = centered_rect(60, 40, frame.area());
    frame.render_widget(Clear, area);

    let mut lines = vec![
        Line::from(""),
        Line::from(vec![Span::styled("  ✓ Prompt copied to clipboard", Style::default().fg(Theme::WHITE).add_modifier(Modifier::BOLD))]),
        Line::from(""),
        Line::from(vec![Span::styled("  Preview:", Style::default().fg(Theme::GREY_400))]),
        Line::from(""),
    ];

    for line in preview.lines().take(5) {
        let truncated = if line.len() > 50 { format!("{}...", &line[..47]) } else { line.to_string() };
        lines.push(Line::from(vec![Span::styled(format!("  {}", truncated), Style::default().fg(Theme::GREY_500))]));
    }

    lines.push(Line::from(""));
    lines.push(Line::from(vec![Span::styled("  Press Esc to close", Style::default().fg(Theme::GREY_600))]));

    let block = Paragraph::new(lines)
        .block(Block::default()
            .title(" Copied ")
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Theme::WHITE))
            .style(Style::default().bg(Theme::GREY_800)));
    
    frame.render_widget(block, area);
}

fn render_ai_chat(frame: &mut Frame, content: &str, scroll: usize) {
    let area = centered_rect(80, 80, frame.area());
    frame.render_widget(Clear, area);

    // Calculate visible area (minus borders and header/footer)
    let visible_height = area.height.saturating_sub(6) as usize;
    let total_lines = content.lines().count();
    
    let mut lines = vec![
        Line::from(""),
        Line::from(vec![Span::styled("  🤖 AI Analysis (Claude)", Style::default().fg(Theme::WHITE).add_modifier(Modifier::BOLD))]),
        Line::from(""),
    ];

    // Show content with scrolling
    for line in content.lines().skip(scroll).take(visible_height) {
        lines.push(Line::from(vec![Span::styled(format!("  {}", line), Style::default().fg(Theme::GREY_300))]));
    }

    // Pad remaining space
    let shown_lines = content.lines().skip(scroll).take(visible_height).count();
    for _ in shown_lines..visible_height {
        lines.push(Line::from(""));
    }

    lines.push(Line::from(""));
    
    // Scroll indicator
    let scroll_info = if total_lines > visible_height {
        format!("  ↑↓/j/k scroll  line {}-{}/{}", 
            scroll + 1, 
            (scroll + visible_height).min(total_lines),
            total_lines)
    } else {
        String::new()
    };
    
    lines.push(Line::from(vec![
        Span::styled("  Esc", Style::default().fg(Theme::GREY_500)),
        Span::styled(" close  ", Style::default().fg(Theme::GREY_600)),
        Span::styled(scroll_info, Style::default().fg(Theme::GREY_500)),
    ]));

    let block = Paragraph::new(lines)
        .block(Block::default()
            .title(" AI Response ")
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Theme::GREY_500))
            .style(Style::default().bg(Theme::GREY_800)));
    
    frame.render_widget(block, area);
}

fn render_diff_preview(frame: &mut Frame, file_path: &str, diff: &crate::diff::UnifiedDiff, scroll: usize) {
    use crate::diff::DiffLine;
    
    let area = centered_rect(85, 85, frame.area());
    frame.render_widget(Clear, area);

    let visible_height = area.height.saturating_sub(8) as usize;
    let (adds, removes) = diff.stats();
    
    // Build all diff lines first
    let mut all_lines: Vec<Line> = Vec::new();
    
    for (hunk_idx, hunk) in diff.hunks.iter().enumerate() {
        // Hunk header
        all_lines.push(Line::from(vec![
            Span::styled(
                format!("  @@ Hunk {} of {} (lines {}-{}) @@", 
                    hunk_idx + 1, 
                    diff.hunks.len(),
                    hunk.old_start,
                    hunk.old_start + hunk.old_count.saturating_sub(1)
                ),
                Style::default().fg(Theme::GREY_400)
            ),
        ]));
        
        for diff_line in &hunk.lines {
            let (prefix, content, style) = match diff_line {
                DiffLine::Add(s) => ("+", s.as_str(), Style::default().fg(Theme::GREEN).bg(Color::Rgb(20, 40, 20))),
                DiffLine::Remove(s) => ("-", s.as_str(), Style::default().fg(Theme::RED).bg(Color::Rgb(40, 20, 20))),
                DiffLine::Context(s) => (" ", s.as_str(), Style::default().fg(Theme::GREY_400)),
            };
            
            all_lines.push(Line::from(vec![
                Span::styled(format!("  {}", prefix), style),
                Span::styled(truncate_str(content, 75), style),
            ]));
        }
        
        all_lines.push(Line::from(""));
    }

    let total_lines = all_lines.len();
    
    // Build display
    let mut lines = vec![
        Line::from(""),
        Line::from(vec![
            Span::styled("  📝 ", Style::default()),
            Span::styled(file_path, Style::default().fg(Theme::WHITE).add_modifier(Modifier::BOLD)),
        ]),
        Line::from(vec![
            Span::styled(format!("  +{} ", adds), Style::default().fg(Theme::GREEN)),
            Span::styled(format!("-{} ", removes), Style::default().fg(Theme::RED)),
            Span::styled(format!("({} hunks)", diff.hunks.len()), Style::default().fg(Theme::GREY_500)),
        ]),
        Line::from(""),
    ];

    // Add scrolled diff content
    for line in all_lines.iter().skip(scroll).take(visible_height) {
        lines.push(line.clone());
    }

    // Pad remaining space
    let shown = all_lines.iter().skip(scroll).take(visible_height).count();
    for _ in shown..visible_height {
        lines.push(Line::from(""));
    }

    lines.push(Line::from(""));
    
    let scroll_info = if total_lines > visible_height {
        format!("line {}-{}/{}", scroll + 1, (scroll + visible_height).min(total_lines), total_lines)
    } else {
        String::new()
    };
    
    lines.push(Line::from(vec![
        Span::styled("  Enter", Style::default().fg(Theme::GREEN)),
        Span::styled(" apply  ", Style::default().fg(Theme::GREY_600)),
        Span::styled("Esc", Style::default().fg(Theme::RED)),
        Span::styled(" cancel  ", Style::default().fg(Theme::GREY_600)),
        Span::styled("↑↓", Style::default().fg(Theme::GREY_500)),
        Span::styled(" scroll  ", Style::default().fg(Theme::GREY_600)),
        Span::styled(scroll_info, Style::default().fg(Theme::GREY_500)),
    ]));

    let block = Paragraph::new(lines)
        .block(Block::default()
            .title(" Diff Preview ")
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Theme::WHITE))
            .style(Style::default().bg(Theme::GREY_800)));
    
    frame.render_widget(block, area);
}

fn render_test_results(frame: &mut Frame, passed: bool, output: &str, scroll: usize) {
    let area = centered_rect(80, 80, frame.area());
    frame.render_widget(Clear, area);

    let visible_height = area.height.saturating_sub(6) as usize;
    let total_lines = output.lines().count();
    
    let status_icon = if passed { "✓" } else { "✗" };
    let status_text = if passed { "Tests Passed" } else { "Tests Failed" };
    let status_color = if passed { Theme::GREEN } else { Theme::RED };
    
    let mut lines = vec![
        Line::from(""),
        Line::from(vec![
            Span::styled(format!("  {} {}", status_icon, status_text), 
                Style::default().fg(status_color).add_modifier(Modifier::BOLD)),
        ]),
        Line::from(""),
    ];

    for line in output.lines().skip(scroll).take(visible_height) {
        let style = if line.contains("FAIL") || line.contains("error") || line.contains("Error") {
            Style::default().fg(Theme::RED)
        } else if line.contains("PASS") || line.contains("ok") {
            Style::default().fg(Theme::GREEN)
        } else {
            Style::default().fg(Theme::GREY_300)
        };
        lines.push(Line::from(vec![Span::styled(format!("  {}", line), style)]));
    }

    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        Span::styled("  Esc", Style::default().fg(Theme::GREY_500)),
        Span::styled(" close  ", Style::default().fg(Theme::GREY_600)),
        Span::styled("↑↓", Style::default().fg(Theme::GREY_500)),
        Span::styled(" scroll", Style::default().fg(Theme::GREY_600)),
    ]));

    let block = Paragraph::new(lines)
        .block(Block::default()
            .title(" Test Results ")
            .borders(Borders::ALL)
            .border_style(Style::default().fg(status_color))
            .style(Style::default().bg(Theme::GREY_800)));
    
    frame.render_widget(block, area);
}

fn render_review_results(frame: &mut Frame, result: &crate::ai::ReviewResult, _scroll: usize) {
    let area = centered_rect(70, 70, frame.area());
    frame.render_widget(Clear, area);

    let status_icon = if result.approved { "✓" } else { "⚠" };
    let status_text = if result.approved { "Approved" } else { "Needs Changes" };
    let status_color = if result.approved { Theme::GREEN } else { Theme::RED };
    
    let mut lines = vec![
        Line::from(""),
        Line::from(vec![
            Span::styled(format!("  {} {}", status_icon, status_text), 
                Style::default().fg(status_color).add_modifier(Modifier::BOLD)),
        ]),
        Line::from(""),
        Line::from(vec![
            Span::styled(format!("  {}", result.summary), Style::default().fg(Theme::GREY_300)),
        ]),
        Line::from(""),
    ];

    if !result.issues.is_empty() {
        lines.push(Line::from(vec![
            Span::styled("  Issues:", Style::default().fg(Theme::WHITE).add_modifier(Modifier::BOLD)),
        ]));
        for issue in &result.issues {
            lines.push(Line::from(vec![
                Span::styled(format!("  {} {}", issue.severity.emoji(), issue.description), 
                    Style::default().fg(Theme::GREY_300)),
            ]));
        }
        lines.push(Line::from(""));
    }

    if !result.suggestions.is_empty() {
        lines.push(Line::from(vec![
            Span::styled("  Suggestions:", Style::default().fg(Theme::WHITE).add_modifier(Modifier::BOLD)),
        ]));
        for suggestion in &result.suggestions {
            lines.push(Line::from(vec![
                Span::styled(format!("  • {}", suggestion), Style::default().fg(Theme::GREY_400)),
            ]));
        }
    }

    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        Span::styled("  Esc", Style::default().fg(Theme::GREY_500)),
        Span::styled(" close", Style::default().fg(Theme::GREY_600)),
    ]));

    let block = Paragraph::new(lines)
        .block(Block::default()
            .title(" AI Review ")
            .borders(Borders::ALL)
            .border_style(Style::default().fg(status_color))
            .style(Style::default().bg(Theme::GREY_800)));
    
    frame.render_widget(block, area);
}

fn render_input_prompt(frame: &mut Frame, title: &str, prompt: &str, input: &str) {
    let area = centered_rect(50, 20, frame.area());
    frame.render_widget(Clear, area);

    let lines = vec![
        Line::from(""),
        Line::from(vec![
            Span::styled(format!("  {}", prompt), Style::default().fg(Theme::GREY_300)),
        ]),
        Line::from(""),
        Line::from(vec![
            Span::styled("  > ", Style::default().fg(Theme::WHITE)),
            Span::styled(input, Style::default().fg(Theme::WHITE)),
            Span::styled("█", Style::default().fg(Theme::GREY_400)),
        ]),
        Line::from(""),
        Line::from(vec![
            Span::styled("  Enter", Style::default().fg(Theme::GREY_500)),
            Span::styled(" confirm  ", Style::default().fg(Theme::GREY_600)),
            Span::styled("Esc", Style::default().fg(Theme::GREY_500)),
            Span::styled(" cancel", Style::default().fg(Theme::GREY_600)),
        ]),
    ];

    let block = Paragraph::new(lines)
        .block(Block::default()
            .title(format!(" {} ", title))
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Theme::WHITE))
            .style(Style::default().bg(Theme::GREY_800)));
    
    frame.render_widget(block, area);
}

fn render_toast(frame: &mut Frame, toast: &Toast) {
    let area = frame.area();
    let width = (toast.message.len() + 6) as u16;
    let toast_area = Rect {
        x: (area.width.saturating_sub(width)) / 2,
        y: area.height.saturating_sub(3),
        width: width.min(area.width),
        height: 1,
    };

    let content = Paragraph::new(Line::from(vec![
        Span::styled(" ✓ ", Style::default().fg(Theme::WHITE)),
        Span::styled(&toast.message, Style::default().fg(Theme::GREY_200)),
        Span::styled(" ", Style::default()),
    ]))
    .style(Style::default().bg(Theme::GREY_700));

    frame.render_widget(content, toast_area);
}

// ============================================================================
// HELPERS
// ============================================================================

fn centered_rect(percent_x: u16, percent_y: u16, r: Rect) -> Rect {
    let popup_layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(r);

    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(popup_layout[1])[1]
}

fn truncate_path(path: &str, max_len: usize) -> String {
    if path.len() <= max_len {
        path.to_string()
    } else {
        format!("...{}", &path[path.len() - max_len + 3..])
    }
}

fn truncate_str(s: &str, max_len: usize) -> String {
    if s.len() <= max_len {
        s.to_string()
    } else {
        format!("{}...", &s[..max_len - 3])
    }
}
