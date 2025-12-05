//! Cosmos UI - A contemplative dual-panel interface
//!
//! Layout:
//! ╔══════════════════════════════════════════════════════════════╗
//! ║                    ☽ C O S M O S ✦                           ║
//! ║          a contemplative companion for your codebase         ║
//! ╠═══════════════════════════╦══════════════════════════════════╣
//! ║  PROJECT                  ║  SUGGESTIONS                     ║
//! ║  ├── src/                 ║  ● Refactor: ai.rs has 715       ║
//! ║  │   ├── main.rs      ●   ║    lines - split into modules    ║
//! ║  │   ├── ui/              ║                                  ║
//! ║  │   └── index/           ║  ◐ Quality: Missing tests for    ║
//! ║  └── tests/               ║    public functions              ║
//! ╠═══════════════════════════╩══════════════════════════════════╣
//! ║  main ● 5 changed │ ? inquiry  ↵ view  a apply  q quit      ║
//! ╚══════════════════════════════════════════════════════════════╝

pub mod panels;
pub mod theme;

use crate::context::WorkContext;
use crate::index::{CodebaseIndex, FileIndex, FlatTreeEntry};
use crate::suggest::{Priority, Suggestion, SuggestionEngine};
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph, Wrap},
    Frame,
};
use std::path::PathBuf;
use std::time::Instant;
use theme::Theme;

/// Active panel
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ActivePanel {
    #[default]
    Project,
    Suggestions,
}

/// Overlay state
#[derive(Debug, Clone, PartialEq, Default)]
pub enum Overlay {
    #[default]
    None,
    Help,
    SuggestionDetail {
        suggestion_id: uuid::Uuid,
        scroll: usize,
    },
    Inquiry {
        response: String,
        scroll: usize,
    },
    ApplyConfirm {
        suggestion_id: uuid::Uuid,
        diff_preview: String,
        scroll: usize,
    },
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

/// Main application state for Cosmos
pub struct App {
    // Core data
    pub index: CodebaseIndex,
    pub suggestions: SuggestionEngine,
    pub context: WorkContext,
    
    // UI state
    pub active_panel: ActivePanel,
    pub project_scroll: usize,
    pub project_selected: usize,
    pub suggestion_scroll: usize,
    pub suggestion_selected: usize,
    pub overlay: Overlay,
    pub toast: Option<Toast>,
    pub should_quit: bool,
    
    // Cached data for display
    pub file_tree: Vec<FlatTreeEntry>,
    pub repo_path: PathBuf,
}

impl App {
    /// Create a new Cosmos app
    pub fn new(
        index: CodebaseIndex,
        suggestions: SuggestionEngine,
        context: WorkContext,
    ) -> Self {
        let file_tree = build_file_tree(&index);
        let repo_path = index.root.clone();
        
        Self {
            index,
            suggestions,
            context,
            active_panel: ActivePanel::default(),
            project_scroll: 0,
            project_selected: 0,
            suggestion_scroll: 0,
            suggestion_selected: 0,
            overlay: Overlay::None,
            toast: None,
            should_quit: false,
            file_tree,
            repo_path,
        }
    }

    /// Switch to the other panel
    pub fn toggle_panel(&mut self) {
        self.active_panel = match self.active_panel {
            ActivePanel::Project => ActivePanel::Suggestions,
            ActivePanel::Suggestions => ActivePanel::Project,
        };
    }

    /// Navigate down in the current panel
    pub fn navigate_down(&mut self) {
        match self.active_panel {
            ActivePanel::Project => {
                let max = self.file_tree.len().saturating_sub(1);
                self.project_selected = (self.project_selected + 1).min(max);
                self.ensure_project_visible();
            }
            ActivePanel::Suggestions => {
                let max = self.suggestions.active_suggestions().len().saturating_sub(1);
                self.suggestion_selected = (self.suggestion_selected + 1).min(max);
                self.ensure_suggestion_visible();
            }
        }
    }

    /// Navigate up in the current panel
    pub fn navigate_up(&mut self) {
        match self.active_panel {
            ActivePanel::Project => {
                self.project_selected = self.project_selected.saturating_sub(1);
                self.ensure_project_visible();
            }
            ActivePanel::Suggestions => {
                self.suggestion_selected = self.suggestion_selected.saturating_sub(1);
                self.ensure_suggestion_visible();
            }
        }
    }

    fn ensure_project_visible(&mut self) {
        if self.project_selected < self.project_scroll {
            self.project_scroll = self.project_selected;
        } else if self.project_selected >= self.project_scroll + 15 {
            self.project_scroll = self.project_selected.saturating_sub(14);
        }
    }

    fn ensure_suggestion_visible(&mut self) {
        if self.suggestion_selected < self.suggestion_scroll {
            self.suggestion_scroll = self.suggestion_selected;
        } else if self.suggestion_selected >= self.suggestion_scroll + 10 {
            self.suggestion_scroll = self.suggestion_selected.saturating_sub(9);
        }
    }

    /// Get currently selected file
    pub fn selected_file(&self) -> Option<&PathBuf> {
        self.file_tree.get(self.project_selected).map(|e| &e.path)
    }

    /// Get currently selected suggestion
    pub fn selected_suggestion(&self) -> Option<&Suggestion> {
        let suggestions = self.suggestions.active_suggestions();
        suggestions.get(self.suggestion_selected).copied()
    }

    /// Show suggestion detail
    pub fn show_suggestion_detail(&mut self) {
        if let Some(suggestion) = self.selected_suggestion() {
            self.overlay = Overlay::SuggestionDetail {
                suggestion_id: suggestion.id,
                scroll: 0,
            };
        }
    }

    /// Toggle help overlay
    pub fn toggle_help(&mut self) {
        self.overlay = match self.overlay {
            Overlay::Help => Overlay::None,
            _ => Overlay::Help,
        };
    }

    /// Close overlay
    pub fn close_overlay(&mut self) {
        self.overlay = Overlay::None;
    }

    /// Show inquiry response
    pub fn show_inquiry(&mut self, response: String) {
        self.overlay = Overlay::Inquiry { response, scroll: 0 };
    }

    /// Clear expired toast
    pub fn clear_expired_toast(&mut self) {
        if let Some(ref toast) = self.toast {
            if toast.is_expired() {
                self.toast = None;
            }
        }
    }

    /// Show a toast message
    pub fn show_toast(&mut self, message: &str) {
        self.toast = Some(Toast::new(message));
    }

    /// Scroll overlay down
    pub fn overlay_scroll_down(&mut self) {
        match &mut self.overlay {
            Overlay::SuggestionDetail { scroll, .. }
            | Overlay::Inquiry { scroll, .. }
            | Overlay::ApplyConfirm { scroll, .. } => {
                *scroll += 1;
            }
            _ => {}
        }
    }

    /// Scroll overlay up
    pub fn overlay_scroll_up(&mut self) {
        match &mut self.overlay {
            Overlay::SuggestionDetail { scroll, .. }
            | Overlay::Inquiry { scroll, .. }
            | Overlay::ApplyConfirm { scroll, .. } => {
                *scroll = scroll.saturating_sub(1);
            }
            _ => {}
        }
    }

    /// Dismiss the currently selected suggestion
    pub fn dismiss_selected(&mut self) {
        if let Some(suggestion) = self.selected_suggestion() {
            let id = suggestion.id;
            self.suggestions.dismiss(id);
            self.show_toast("Suggestion dismissed");
        }
    }
}

/// Build a flat file tree for display
fn build_file_tree(index: &CodebaseIndex) -> Vec<FlatTreeEntry> {
    let mut entries: Vec<_> = index.files.keys().cloned().collect();
    entries.sort();
    
    entries.into_iter().map(|path| {
        let file_index = index.files.get(&path);
        let priority = file_index.map(|f| f.priority_indicator()).unwrap_or(' ');
        let depth = path.components().count().saturating_sub(1);
        let name = path.file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("")
            .to_string();
        
        FlatTreeEntry {
            name,
            path,
            is_dir: false,
            depth,
            priority,
        }
    }).collect()
}

// ═══════════════════════════════════════════════════════════════════════════
//  RENDERING
// ═══════════════════════════════════════════════════════════════════════════

/// Main render function
pub fn render(frame: &mut Frame, app: &App) {
    let area = frame.area();
    
    // Clear with dark background
    frame.render_widget(Block::default().style(Style::default().bg(Theme::BG)), area);

    // Main layout - clean and minimal
    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(6),   // Header (logo + tagline)
            Constraint::Min(10),     // Main content
            Constraint::Length(3),   // Footer
        ])
        .split(area);

    render_header(frame, layout[0], app);
    render_main(frame, layout[1], app);
    render_footer(frame, layout[2], app);

    // Overlays
    match &app.overlay {
        Overlay::Help => render_help(frame),
        Overlay::SuggestionDetail { suggestion_id, scroll } => {
            if let Some(suggestion) = app.suggestions.suggestions.iter().find(|s| &s.id == suggestion_id) {
                render_suggestion_detail(frame, suggestion, *scroll);
            }
        }
        Overlay::Inquiry { response, scroll } => {
            render_inquiry(frame, response, *scroll);
        }
        Overlay::ApplyConfirm { diff_preview, scroll, .. } => {
            render_apply_confirm(frame, diff_preview, *scroll);
        }
        Overlay::None => {}
    }

    // Toast
    if let Some(toast) = &app.toast {
        render_toast(frame, toast);
    }
}

fn render_header(frame: &mut Frame, area: Rect, _app: &App) {
    let lines = vec![
        Line::from(""),
        Line::from(""),
        Line::from(vec![
            Span::styled(
                format!("   {}", Theme::COSMOS_LOGO),
                Style::default().fg(Theme::WHITE).add_modifier(Modifier::BOLD)
            ),
        ]),
        Line::from(vec![
            Span::styled(
                format!("   {}", Theme::COSMOS_TAGLINE),
                Style::default().fg(Theme::GREY_300)  // More legible tagline
            ),
        ]),
        Line::from(""),
    ];

    let header = Paragraph::new(lines).style(Style::default().bg(Theme::BG));
    frame.render_widget(header, area);
}

fn render_main(frame: &mut Frame, area: Rect, app: &App) {
    // Add horizontal padding for breathing room
    let padded = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Length(2),       // Left padding
            Constraint::Min(10),         // Main content
            Constraint::Length(2),       // Right padding
        ])
        .split(area);
    
    // Split into two panels with gap
    let panels = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(38),  // Project tree
            Constraint::Length(2),       // Gap between panels
            Constraint::Percentage(62),  // Suggestions (wider for wrapped text)
        ])
        .split(padded[1]);

    render_project_panel(frame, panels[0], app);
    render_suggestions_panel(frame, panels[2], app);
}

fn render_project_panel(frame: &mut Frame, area: Rect, app: &App) {
    let is_active = app.active_panel == ActivePanel::Project;
    let border_style = if is_active {
        Style::default().fg(Theme::GREY_300)  // Bright active border
    } else {
        Style::default().fg(Theme::GREY_600)  // Visible inactive border
    };

    let visible_height = area.height.saturating_sub(4) as usize; // Account for borders and padding
    
    let mut lines = vec![];
    
    // Top padding for breathing room
    lines.push(Line::from(""));
    
    let total_files = app.file_tree.len();
    let scroll_indicator = if total_files > visible_height {
        let current = app.project_scroll + 1;
        format!(" ↕ {}/{} ", current, total_files)
    } else {
        String::new()
    };
    
    for (i, entry) in app.file_tree.iter()
        .enumerate()
        .skip(app.project_scroll)
        .take(visible_height)
    {
        let is_selected = i == app.project_selected && is_active;
        let indent = "  ".repeat(entry.depth);
        
        let name_style = if is_selected {
            Style::default().fg(Theme::WHITE).add_modifier(Modifier::BOLD)
        } else if entry.priority == Theme::PRIORITY_HIGH {
            Style::default().fg(Theme::GREY_50)  // Bright for high priority
        } else {
            Style::default().fg(Theme::GREY_200)  // Legible for regular files
        };
        
        let cursor = if is_selected { " ›" } else { "  " };
        let priority_indicator = if entry.priority == Theme::PRIORITY_HIGH {
            "  ●"
        } else {
            ""
        };
        
        lines.push(Line::from(vec![
            Span::styled(cursor, Style::default().fg(Theme::GREY_100)),  // Bright cursor
            Span::styled(format!(" {}", indent), Style::default().fg(Theme::GREY_600)),
            Span::styled(&entry.name, name_style),
            Span::styled(priority_indicator, Style::default().fg(Theme::GREY_200)),  // Visible indicator
        ]));
    }

    let block = Block::default()
        .title(format!(" {} {}", Theme::SECTION_PROJECT, scroll_indicator))
        .title_style(Style::default().fg(Theme::GREY_200))  // Legible title
        .borders(Borders::ALL)
        .border_style(border_style)
        .style(Style::default().bg(Theme::GREY_800));

    let paragraph = Paragraph::new(lines).block(block);
    frame.render_widget(paragraph, area);
}

fn render_suggestions_panel(frame: &mut Frame, area: Rect, app: &App) {
    let is_active = app.active_panel == ActivePanel::Suggestions;
    let border_style = if is_active {
        Style::default().fg(Theme::GREY_300)  // Bright active border
    } else {
        Style::default().fg(Theme::GREY_600)  // Visible inactive border
    };

    let visible_height = area.height.saturating_sub(4) as usize; // Account for borders and padding
    let inner_width = area.width.saturating_sub(6) as usize; // Account for borders and padding
    let suggestions = app.suggestions.active_suggestions();
    
    let mut lines = vec![];
    
    // Top padding for breathing room
    lines.push(Line::from(""));
    
    if suggestions.is_empty() {
        lines.push(Line::from(""));
        lines.push(Line::from(vec![
            Span::styled(
                "   ✧ 𝑛𝑜 𝑠𝑢𝑔𝑔𝑒𝑠𝑡𝑖𝑜𝑛𝑠 · 𝑐𝑜𝑑𝑒𝑏𝑎𝑠𝑒 𝑖𝑠 𝑠𝑒𝑟𝑒𝑛𝑒",
                Style::default().fg(Theme::GREY_300).add_modifier(Modifier::ITALIC)
            ),
        ]));
    } else {
        let mut line_count = 1; // Start at 1 for top padding
        
        for (i, suggestion) in suggestions.iter().enumerate().skip(app.suggestion_scroll) {
            if line_count >= visible_height {
                break;
            }
            
            let is_selected = i == app.suggestion_selected && is_active;
            
            let file_style = if is_selected {
                Style::default().fg(Theme::WHITE).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Theme::GREY_100)  // Bright file names
            };
            
            let text_style = if is_selected {
                Style::default().fg(Theme::GREY_100)  // Bright selected text
            } else {
                Style::default().fg(Theme::GREY_300)  // Legible suggestion text
            };
            
            let cursor = if is_selected { " ›" } else { "  " };
            let file_name = suggestion.file.file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("?");
            
            // File name line with cursor
            lines.push(Line::from(vec![
                Span::styled(cursor, Style::default().fg(Theme::GREY_100)),  // Bright cursor
                Span::styled(format!(" {}", file_name), file_style),
            ]));
            line_count += 1;
            
            // Wrap the summary text
            let summary = &suggestion.summary;
            let wrapped = wrap_text(summary, inner_width.saturating_sub(6));
            
            for (j, wrapped_line) in wrapped.iter().enumerate() {
                if line_count >= visible_height {
                    break;
                }
                
                let prefix = if j == 0 { "     " } else { "     " };
                let line_text = if j == 0 && wrapped.len() > 1 {
                    format!("{}{}", prefix, wrapped_line)
                } else if j == wrapped.len() - 1 && wrapped.len() > 1 {
                    format!("{}{}", prefix, wrapped_line)
                } else {
                    format!("{}{}", prefix, wrapped_line)
                };
                
                lines.push(Line::from(vec![
                    Span::styled(line_text, text_style),
                ]));
                line_count += 1;
            }
            
            // Add spacing between suggestions
            if line_count < visible_height {
                lines.push(Line::from(""));
                line_count += 1;
            }
        }
    }

    let counts = app.suggestions.counts();
    let scroll_indicator = if suggestions.len() > visible_height / 3 {
        let total = suggestions.len();
        let current = app.suggestion_scroll + 1;
        format!(" ↕ {}/{} ", current, total)
    } else {
        String::new()
    };
    
    let title = if counts.total > 0 {
        format!(" {} · {}{}", Theme::SECTION_SUGGESTIONS, counts.total, scroll_indicator)
    } else {
        format!(" {} ", Theme::SECTION_SUGGESTIONS)
    };

    let block = Block::default()
        .title(title)
        .title_style(Style::default().fg(Theme::GREY_200))  // Legible title
        .borders(Borders::ALL)
        .border_style(border_style)
        .style(Style::default().bg(Theme::GREY_800));

    let paragraph = Paragraph::new(lines)
        .block(block)
        .wrap(Wrap { trim: false });
    frame.render_widget(paragraph, area);
}

fn render_footer(frame: &mut Frame, area: Rect, app: &App) {
    // Top line - subtle separator
    let separator = Line::from(vec![
        Span::styled(
            "─".repeat(area.width as usize),
            Style::default().fg(Theme::GREY_500)  // Visible separator
        ),
    ]);
    
    // Bottom line - status and hints
    let mut spans = vec![
        Span::styled("  ", Style::default()),
        Span::styled(&app.context.branch, Style::default().fg(Theme::GREY_100)),  // Bright branch
    ];

    if app.context.has_changes() {
        spans.push(Span::styled("  ·  ", Style::default().fg(Theme::GREY_500)));
        spans.push(Span::styled(
            format!("{} 𝘤𝘩𝘢𝘯𝘨𝘦𝘥", app.context.modified_count),
            Style::default().fg(Theme::GREY_200),  // Visible count
        ));
    }

    spans.push(Span::styled("  ·  ", Style::default().fg(Theme::GREY_500)));
    
    // Key hints with elegant styling - high contrast
    let hints = [
        ("?", "𝘩𝘦𝘭𝘱"),
        ("⇥", "𝘴𝘸𝘪𝘵𝘤𝘩"),
        ("↵", "𝘷𝘪𝘦𝘸"),
        ("𝘢", "𝘢𝘱𝘱𝘭𝘺"),
        ("𝘥", "𝘥𝘪𝘴𝘮𝘪𝘴𝘴"),
        ("𝘲", "𝘲𝘶𝘪𝘵"),
    ];
    
    for (key, action) in hints {
        spans.push(Span::styled(key, Style::default().fg(Theme::WHITE)));  // White keys
        spans.push(Span::styled(format!(" {} ", action), Style::default().fg(Theme::GREY_400)));  // Legible action
    }

    let footer_line = Line::from(spans);
    
    let footer = Paragraph::new(vec![separator, footer_line])
        .style(Style::default().bg(Theme::BG));
    frame.render_widget(footer, area);
}

// ═══════════════════════════════════════════════════════════════════════════
//  OVERLAYS
// ═══════════════════════════════════════════════════════════════════════════

fn render_help(frame: &mut Frame) {
    let area = centered_rect(45, 65, frame.area());
    frame.render_widget(Clear, area);

    let help_text = vec![
        Line::from(""),
        Line::from(""),
        Line::from(vec![
            Span::styled("     𝒏𝒂𝒗𝒊𝒈𝒂𝒕𝒊𝒐𝒏", Style::default().fg(Theme::WHITE).add_modifier(Modifier::BOLD))
        ]),
        Line::from(""),
        Line::from(vec![
            Span::styled("     ↑ ↓  𝘰𝘳  𝘬 𝘫", Style::default().fg(Theme::WHITE)),
            Span::styled("      navigate", Style::default().fg(Theme::GREY_300)),
        ]),
        Line::from(vec![
            Span::styled("     ⇥  Tab", Style::default().fg(Theme::WHITE)),
            Span::styled("           switch panels", Style::default().fg(Theme::GREY_300)),
        ]),
        Line::from(vec![
            Span::styled("     ↵  Enter", Style::default().fg(Theme::WHITE)),
            Span::styled("         view details", Style::default().fg(Theme::GREY_300)),
        ]),
        Line::from(""),
        Line::from(vec![
            Span::styled("     ─────────────────────────", Style::default().fg(Theme::GREY_500))
        ]),
        Line::from(""),
        Line::from(vec![
            Span::styled("     𝒂𝒄𝒕𝒊𝒐𝒏𝒔", Style::default().fg(Theme::WHITE).add_modifier(Modifier::BOLD))
        ]),
        Line::from(""),
        Line::from(vec![
            Span::styled("     ?", Style::default().fg(Theme::WHITE)),
            Span::styled("                 inquiry", Style::default().fg(Theme::GREY_300)),
        ]),
        Line::from(vec![
            Span::styled("     𝘢", Style::default().fg(Theme::WHITE)),
            Span::styled("                 apply suggestion", Style::default().fg(Theme::GREY_300)),
        ]),
        Line::from(vec![
            Span::styled("     𝘥", Style::default().fg(Theme::WHITE)),
            Span::styled("                 dismiss", Style::default().fg(Theme::GREY_300)),
        ]),
        Line::from(vec![
            Span::styled("     𝘳", Style::default().fg(Theme::WHITE)),
            Span::styled("                 refresh", Style::default().fg(Theme::GREY_300)),
        ]),
        Line::from(""),
        Line::from(vec![
            Span::styled("     ─────────────────────────", Style::default().fg(Theme::GREY_500))
        ]),
        Line::from(""),
        Line::from(vec![
            Span::styled("     Esc", Style::default().fg(Theme::WHITE)),
            Span::styled("               close", Style::default().fg(Theme::GREY_300)),
        ]),
        Line::from(vec![
            Span::styled("     𝘲", Style::default().fg(Theme::WHITE)),
            Span::styled("                 quit cosmos", Style::default().fg(Theme::GREY_300)),
        ]),
        Line::from(""),
        Line::from(""),
    ];

    let block = Paragraph::new(help_text)
        .block(Block::default()
            .title(" ✧ 𝘩𝘦𝘭𝘱 ")
            .title_style(Style::default().fg(Theme::GREY_100))
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Theme::GREY_400))
            .style(Style::default().bg(Theme::GREY_900)));
    
    frame.render_widget(block, area);
}

fn render_suggestion_detail(frame: &mut Frame, suggestion: &Suggestion, scroll: usize) {
    let area = centered_rect(75, 80, frame.area());
    frame.render_widget(Clear, area);

    let visible_height = area.height.saturating_sub(12) as usize;
    let inner_width = area.width.saturating_sub(8) as usize;
    
    let mut lines = vec![
        Line::from(""),
        Line::from(""),
        Line::from(vec![
            Span::styled(format!("     {} ", suggestion.priority.icon()), 
                Style::default().fg(Theme::WHITE)),
            Span::styled(suggestion.kind.label(), 
                Style::default().fg(Theme::GREY_200).add_modifier(Modifier::ITALIC)),
        ]),
        Line::from(""),
    ];
    
    // Wrap the summary
    let summary_wrapped = wrap_text(&suggestion.summary, inner_width.saturating_sub(10));
    for wrapped_line in &summary_wrapped {
        lines.push(Line::from(vec![
            Span::styled(format!("     {}", wrapped_line), 
                Style::default().fg(Theme::GREY_50)),
        ]));
    }
    
    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        Span::styled("     ─────────────────────────────────────────", Style::default().fg(Theme::GREY_600))
    ]));
    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        Span::styled(format!("     𝘧𝘪𝘭𝘦   {}", suggestion.file.display()), 
            Style::default().fg(Theme::GREY_300)),
    ]));

    if let Some(line) = suggestion.line {
        lines.push(Line::from(vec![
            Span::styled(format!("     𝘭𝘪𝘯𝘦   {}", line), 
                Style::default().fg(Theme::GREY_300)),
        ]));
    }

    lines.push(Line::from(""));

    if let Some(detail) = &suggestion.detail {
        lines.push(Line::from(vec![
            Span::styled("     ─────────────────────────────────────────", Style::default().fg(Theme::GREY_600))
        ]));
        lines.push(Line::from(""));
        
        // Wrap each line of detail text
        let detail_lines: Vec<&str> = detail.lines().collect();
        let mut wrapped_detail_lines = Vec::new();
        
        for line in &detail_lines {
            let wrapped = wrap_text(line, inner_width.saturating_sub(10));
            for w in wrapped {
                wrapped_detail_lines.push(w);
            }
        }
        
        for line in wrapped_detail_lines.iter().skip(scroll).take(visible_height) {
            lines.push(Line::from(vec![
                Span::styled(format!("     {}", line), Style::default().fg(Theme::GREY_100)),
            ]));
        }
        
        // Scroll indicator
        if wrapped_detail_lines.len() > visible_height {
            lines.push(Line::from(""));
            lines.push(Line::from(vec![
                Span::styled(
                    format!("     ↕ {}/{} ", scroll + 1, wrapped_detail_lines.len().saturating_sub(visible_height) + 1), 
                    Style::default().fg(Theme::GREY_400)
                ),
            ]));
        }
    }

    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        Span::styled("     ─────────────────────────────────────────", Style::default().fg(Theme::GREY_600))
    ]));
    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        Span::styled("     𝘢", Style::default().fg(Theme::WHITE)),
        Span::styled(" apply   ", Style::default().fg(Theme::GREY_400)),
        Span::styled("𝘥", Style::default().fg(Theme::WHITE)),
        Span::styled(" dismiss   ", Style::default().fg(Theme::GREY_400)),
        Span::styled("Esc", Style::default().fg(Theme::WHITE)),
        Span::styled(" close", Style::default().fg(Theme::GREY_400)),
    ]));
    lines.push(Line::from(""));

    let block = Paragraph::new(lines)
        .wrap(Wrap { trim: false })
        .block(Block::default()
            .title(" ✧ 𝘥𝘦𝘵𝘢𝘪𝘭 ")
            .title_style(Style::default().fg(Theme::GREY_100))
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Theme::GREY_400))
            .style(Style::default().bg(Theme::GREY_900)));
    
    frame.render_widget(block, area);
}

fn render_inquiry(frame: &mut Frame, response: &str, scroll: usize) {
    let area = centered_rect(80, 85, frame.area());
    frame.render_widget(Clear, area);

    let visible_height = area.height.saturating_sub(10) as usize;
    let inner_width = area.width.saturating_sub(10) as usize;
    
    let mut lines = vec![
        Line::from(""),
        Line::from(""),
        Line::from(vec![
            Span::styled("     ✧ ", Style::default().fg(Theme::WHITE)),
            Span::styled("𝘤𝘰𝘴𝘮𝘰𝘴 𝘴𝘶𝘨𝘨𝘦𝘴𝘵𝘴...", Style::default().fg(Theme::GREY_200).add_modifier(Modifier::ITALIC)),
        ]),
        Line::from(""),
        Line::from(vec![
            Span::styled("     ─────────────────────────────────────────────────", Style::default().fg(Theme::GREY_600))
        ]),
        Line::from(""),
    ];

    // Wrap each line of the response
    let response_lines: Vec<&str> = response.lines().collect();
    let mut wrapped_lines = Vec::new();
    
    for line in &response_lines {
        if line.is_empty() {
            wrapped_lines.push(String::new());
        } else {
            let wrapped = wrap_text(line, inner_width.saturating_sub(10));
            for w in wrapped {
                wrapped_lines.push(w);
            }
        }
    }
    
    for line in wrapped_lines.iter().skip(scroll).take(visible_height) {
        lines.push(Line::from(vec![
            Span::styled(format!("     {}", line), Style::default().fg(Theme::GREY_100)),
        ]));
    }
    
    // Scroll indicator
    if wrapped_lines.len() > visible_height {
        lines.push(Line::from(""));
        lines.push(Line::from(vec![
            Span::styled(
                format!("     ↕ {}/{} ", scroll + 1, wrapped_lines.len().saturating_sub(visible_height) + 1), 
                Style::default().fg(Theme::GREY_400)
            ),
        ]));
    }

    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        Span::styled("     ─────────────────────────────────────────────────", Style::default().fg(Theme::GREY_600))
    ]));
    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        Span::styled("     ↑↓", Style::default().fg(Theme::WHITE)),
        Span::styled(" 𝘴𝘤𝘳𝘰𝘭𝘭   ", Style::default().fg(Theme::GREY_400)),
        Span::styled("Esc", Style::default().fg(Theme::WHITE)),
        Span::styled(" 𝘤𝘭𝘰𝘴𝘦", Style::default().fg(Theme::GREY_400)),
    ]));
    lines.push(Line::from(""));

    let block = Paragraph::new(lines)
        .wrap(Wrap { trim: false })
        .block(Block::default()
            .title(" ✧ 𝘪𝘯𝘲𝘶𝘪𝘳𝘺 ")
            .title_style(Style::default().fg(Theme::GREY_100))
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Theme::GREY_400))
            .style(Style::default().bg(Theme::GREY_900)));
    
    frame.render_widget(block, area);
}

fn render_apply_confirm(frame: &mut Frame, diff_preview: &str, scroll: usize) {
    let area = centered_rect(85, 85, frame.area());
    frame.render_widget(Clear, area);

    let visible_height = area.height.saturating_sub(12) as usize;
    
    let mut lines = vec![
        Line::from(""),
        Line::from(""),
        Line::from(vec![
            Span::styled("     𝘢𝘱𝘱𝘭𝘺 𝘵𝘩𝘦𝘴𝘦 𝘤𝘩𝘢𝘯𝘨𝘦𝘴?", Style::default().fg(Theme::WHITE).add_modifier(Modifier::BOLD)),
        ]),
        Line::from(""),
        Line::from(vec![
            Span::styled("     ─────────────────────────────────────────────────────", Style::default().fg(Theme::GREY_600))
        ]),
        Line::from(""),
    ];

    let diff_lines: Vec<&str> = diff_preview.lines().collect();
    
    for line in diff_lines.iter().skip(scroll).take(visible_height) {
        let style = if line.starts_with('+') && !line.starts_with("+++") {
            Style::default().fg(Theme::GREEN)
        } else if line.starts_with('-') && !line.starts_with("---") {
            Style::default().fg(Theme::RED)
        } else if line.starts_with("@@") {
            Style::default().fg(Theme::GREY_400).add_modifier(Modifier::ITALIC)
        } else if line.starts_with("+++") || line.starts_with("---") {
            Style::default().fg(Theme::GREY_300)
        } else {
            Style::default().fg(Theme::GREY_200)
        };
        
        lines.push(Line::from(vec![
            Span::styled(format!("     {}", line), style),
        ]));
    }
    
    // Scroll indicator
    if diff_lines.len() > visible_height {
        lines.push(Line::from(""));
        lines.push(Line::from(vec![
            Span::styled(
                format!("     ↕ {}/{} ", scroll + 1, diff_lines.len().saturating_sub(visible_height) + 1), 
                Style::default().fg(Theme::GREY_400)
            ),
        ]));
    }

    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        Span::styled("     ─────────────────────────────────────────────────────", Style::default().fg(Theme::GREY_600))
    ]));
    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        Span::styled("     𝘺", Style::default().fg(Theme::WHITE)),
        Span::styled(" apply   ", Style::default().fg(Theme::GREY_400)),
        Span::styled("𝘯", Style::default().fg(Theme::WHITE)),
        Span::styled(" cancel   ", Style::default().fg(Theme::GREY_400)),
        Span::styled("↑↓", Style::default().fg(Theme::WHITE)),
        Span::styled(" scroll", Style::default().fg(Theme::GREY_400)),
    ]));
    lines.push(Line::from(""));

    let block = Paragraph::new(lines)
        .block(Block::default()
            .title(" ✧ 𝘤𝘰𝘯𝘧𝘪𝘳𝘮 ")
            .title_style(Style::default().fg(Theme::GREY_100))
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Theme::GREY_400))
            .style(Style::default().bg(Theme::GREY_900)));
    
    frame.render_widget(block, area);
}

fn render_toast(frame: &mut Frame, toast: &Toast) {
    let area = frame.area();
    let width = (toast.message.len() + 10) as u16;
    let toast_area = Rect {
        x: (area.width.saturating_sub(width)) / 2,
        y: area.height.saturating_sub(5),
        width: width.min(area.width),
        height: 1,
    };

    let content = Paragraph::new(Line::from(vec![
        Span::styled("  ✧ ", Style::default().fg(Theme::WHITE)),
        Span::styled(&toast.message, Style::default().fg(Theme::GREY_100).add_modifier(Modifier::ITALIC)),
        Span::styled("  ", Style::default()),
    ]))
    .style(Style::default().bg(Theme::GREY_700));

    frame.render_widget(content, toast_area);
}

// ═══════════════════════════════════════════════════════════════════════════
//  UTILITIES
// ═══════════════════════════════════════════════════════════════════════════

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

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        format!("{}...", &s[..max - 3])
    }
}

/// Wrap text to fit within a given width
fn wrap_text(text: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return vec![text.to_string()];
    }
    
    let mut lines = Vec::new();
    let mut current_line = String::new();
    
    for word in text.split_whitespace() {
        if current_line.is_empty() {
            if word.len() > width {
                // Word is longer than width, force break it
                let mut remaining = word;
                while remaining.len() > width {
                    lines.push(remaining[..width].to_string());
                    remaining = &remaining[width..];
                }
                current_line = remaining.to_string();
            } else {
                current_line = word.to_string();
            }
        } else if current_line.len() + 1 + word.len() <= width {
            current_line.push(' ');
            current_line.push_str(word);
        } else {
            lines.push(current_line);
            if word.len() > width {
                let mut remaining = word;
                while remaining.len() > width {
                    lines.push(remaining[..width].to_string());
                    remaining = &remaining[width..];
                }
                current_line = remaining.to_string();
            } else {
                current_line = word.to_string();
            }
        }
    }
    
    if !current_line.is_empty() {
        lines.push(current_line);
    }
    
    if lines.is_empty() {
        lines.push(String::new());
    }
    
    lines
}
