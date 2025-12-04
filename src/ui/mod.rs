pub mod panels;

use crate::analysis::{ChurnEntry, DangerZone, DustyFile, TodoEntry};
use crate::score::{HealthScore, RepoMetrics, Trend};
use panels::Panel;
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Tabs},
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
}

impl ActivePanel {
    pub fn index(&self) -> usize {
        match self {
            ActivePanel::DangerZones => 0,
            ActivePanel::Hotspots => 1,
            ActivePanel::DustyFiles => 2,
            ActivePanel::Todos => 3,
        }
    }

    pub fn from_index(index: usize) -> Self {
        match index {
            0 => ActivePanel::DangerZones,
            1 => ActivePanel::Hotspots,
            2 => ActivePanel::DustyFiles,
            3 => ActivePanel::Todos,
            _ => ActivePanel::DangerZones,
        }
    }
}

/// Main application state
pub struct App {
    pub score: HealthScore,
    pub metrics: RepoMetrics,
    pub repo_name: String,
    pub branch_name: String,
    pub churn_entries: Vec<ChurnEntry>,
    pub dusty_files: Vec<DustyFile>,
    pub todo_entries: Vec<TodoEntry>,
    pub danger_zones: Vec<DangerZone>,
    pub active_panel: ActivePanel,
    pub scroll_offset: usize,
    pub should_quit: bool,
}

impl App {
    pub fn new(
        score: HealthScore,
        metrics: RepoMetrics,
        repo_name: String,
        branch_name: String,
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
            churn_entries,
            dusty_files,
            todo_entries,
            danger_zones,
            active_panel: ActivePanel::default(),
            scroll_offset: 0,
            should_quit: false,
        }
    }

    pub fn next_panel(&mut self) {
        self.active_panel = ActivePanel::from_index((self.active_panel.index() + 1) % 4);
        self.scroll_offset = 0;
    }

    pub fn prev_panel(&mut self) {
        self.active_panel = ActivePanel::from_index((self.active_panel.index() + 3) % 4);
        self.scroll_offset = 0;
    }

    pub fn select_panel(&mut self, index: usize) {
        if index < 4 {
            self.active_panel = ActivePanel::from_index(index);
            self.scroll_offset = 0;
        }
    }

    pub fn scroll_down(&mut self) {
        let max_scroll = self.current_panel_len().saturating_sub(1);
        self.scroll_offset = (self.scroll_offset + 1).min(max_scroll);
    }

    pub fn scroll_up(&mut self) {
        self.scroll_offset = self.scroll_offset.saturating_sub(1);
    }

    fn current_panel_len(&self) -> usize {
        match self.active_panel {
            ActivePanel::DangerZones => self.danger_zones.len(),
            ActivePanel::Hotspots => self.churn_entries.len(),
            ActivePanel::DustyFiles => self.dusty_files.len(),
            ActivePanel::Todos => self.todo_entries.len(),
        }
    }
}

/// Render the entire UI
pub fn render(frame: &mut Frame, app: &App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(4),  // Header with score
            Constraint::Length(3),  // Stats bar
            Constraint::Length(3),  // Tab bar
            Constraint::Min(8),     // Main panel
            Constraint::Length(2),  // Help bar
        ])
        .split(frame.area());

    render_header(frame, chunks[0], app);
    render_stats(frame, chunks[1], app);
    render_tabs(frame, chunks[2], app);
    render_panel(frame, chunks[3], app);
    render_help(frame, chunks[4], app);
}

fn render_header(frame: &mut Frame, area: Rect, app: &App) {
    let score_color = app.score.color();
    
    // Trend indicator with color
    let trend_span = match app.score.trend {
        Trend::Improving => Span::styled(" ↑", Style::default().fg(Color::Rgb(134, 239, 172))),
        Trend::Declining => Span::styled(" ↓", Style::default().fg(Color::Rgb(248, 113, 113))),
        Trend::Stable => Span::styled(" →", Style::default().fg(Color::Rgb(148, 163, 184))),
        Trend::Unknown => Span::raw(""),
    };

    let header_content = vec![
        Line::from(vec![
            Span::styled(
                format!("  ◉ {}/100 ", app.score.value),
                Style::default().fg(score_color).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("({})", app.score.grade),
                Style::default().fg(score_color).add_modifier(Modifier::BOLD),
            ),
            trend_span,
            Span::raw("                              "),
            Span::styled(
                format!("{} @ {}", app.repo_name, app.branch_name),
                Style::default().fg(Color::Rgb(148, 163, 184)),
            ),
        ]),
        Line::from(vec![
            Span::raw("  "),
            Span::styled(
                format!("\"{}\"", app.score.grade.description()),
                Style::default().fg(Color::Rgb(148, 163, 184)).add_modifier(Modifier::ITALIC),
            ),
        ]),
    ];

    let header = Paragraph::new(header_content)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Rgb(51, 65, 85)))
                .style(Style::default().bg(Color::Rgb(15, 23, 42))),
        );

    frame.render_widget(header, area);
}

fn render_stats(frame: &mut Frame, area: Rect, app: &App) {
    let total_todos = app.metrics.todo_count + app.metrics.fixme_count + app.metrics.hack_count;
    
    let stats_line = Line::from(vec![
        Span::styled(" [", Style::default().fg(Color::Rgb(71, 85, 105))),
        Span::styled(
            format!("{}", app.metrics.total_files),
            Style::default().fg(Color::Rgb(96, 165, 250)),
        ),
        Span::styled(" files]", Style::default().fg(Color::Rgb(71, 85, 105))),
        Span::raw("  "),
        Span::styled("[", Style::default().fg(Color::Rgb(71, 85, 105))),
        Span::styled(
            format!("{}", app.metrics.danger_zone_count),
            Style::default().fg(Color::Rgb(248, 113, 113)),
        ),
        Span::styled(" danger]", Style::default().fg(Color::Rgb(71, 85, 105))),
        Span::raw("  "),
        Span::styled("[", Style::default().fg(Color::Rgb(71, 85, 105))),
        Span::styled(
            format!("{}", app.metrics.files_changed_recently),
            Style::default().fg(Color::Rgb(52, 211, 153)),
        ),
        Span::styled(" changed]", Style::default().fg(Color::Rgb(71, 85, 105))),
        Span::raw("  "),
        Span::styled("[", Style::default().fg(Color::Rgb(71, 85, 105))),
        Span::styled(
            format!("{}", total_todos),
            Style::default().fg(Color::Rgb(251, 146, 60)),
        ),
        Span::styled(" todos]", Style::default().fg(Color::Rgb(71, 85, 105))),
        Span::raw("  "),
        Span::styled("[", Style::default().fg(Color::Rgb(71, 85, 105))),
        Span::styled(
            format!("{}", app.metrics.dusty_file_count),
            Style::default().fg(Color::Rgb(148, 163, 184)),
        ),
        Span::styled(" dusty]", Style::default().fg(Color::Rgb(71, 85, 105))),
    ]);

    let stats = Paragraph::new(stats_line)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Rgb(51, 65, 85)))
                .style(Style::default().bg(Color::Rgb(15, 23, 42))),
        );

    frame.render_widget(stats, area);
}

fn render_tabs(frame: &mut Frame, area: Rect, app: &App) {
    let titles = vec![
        Line::from(" [1] Danger Zones "),
        Line::from(" [2] Hotspots "),
        Line::from(" [3] Dusty Files "),
        Line::from(" [4] TODOs "),
    ];

    let tabs = Tabs::new(titles)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Rgb(51, 65, 85)))
                .style(Style::default().bg(Color::Rgb(15, 23, 42))),
        )
        .select(app.active_panel.index())
        .style(Style::default().fg(Color::Rgb(148, 163, 184)))
        .highlight_style(
            Style::default()
                .fg(app.score.color())
                .add_modifier(Modifier::BOLD),
        );

    frame.render_widget(tabs, area);
}

fn render_panel(frame: &mut Frame, area: Rect, app: &App) {
    match app.active_panel {
        ActivePanel::DangerZones => {
            let panel = Panel::danger_zones(&app.danger_zones, app.scroll_offset, app.score.color());
            frame.render_widget(panel, area);
        }
        ActivePanel::Hotspots => {
            let panel = Panel::hotspots(&app.churn_entries, app.scroll_offset, app.score.color());
            frame.render_widget(panel, area);
        }
        ActivePanel::DustyFiles => {
            let panel = Panel::dusty_files(&app.dusty_files, app.scroll_offset, app.score.color());
            frame.render_widget(panel, area);
        }
        ActivePanel::Todos => {
            let panel = Panel::todos(&app.todo_entries, app.scroll_offset, app.score.color());
            frame.render_widget(panel, area);
        }
    }
}

fn render_help(frame: &mut Frame, area: Rect, app: &App) {
    let help_line = Line::from(vec![
        Span::styled(" [q] ", Style::default().fg(app.score.color())),
        Span::raw("quit   "),
        Span::styled("[1-4] ", Style::default().fg(app.score.color())),
        Span::raw("switch panel   "),
        Span::styled("[↑↓] ", Style::default().fg(app.score.color())),
        Span::raw("scroll   "),
        Span::styled("[Tab] ", Style::default().fg(app.score.color())),
        Span::raw("next panel"),
    ]);

    let help = Paragraph::new(help_line)
        .style(Style::default().bg(Color::Rgb(15, 23, 42)).fg(Color::Rgb(148, 163, 184)));

    frame.render_widget(help, area);
}
