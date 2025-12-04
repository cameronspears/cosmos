mod analysis;
mod mood;
mod ui;

use analysis::{GitAnalyzer, StalenessAnalyzer, TodoScanner};
use anyhow::Result;
use clap::Parser;
use crossterm::{
    event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use mood::{MoodEngine, RepoMetrics};
use ratatui::prelude::*;
use std::io;
use std::path::PathBuf;
use ui::App;

#[derive(Parser, Debug)]
#[command(
    name = "codecosmos",
    about = "A terminal mood dashboard for your codebase",
    version
)]
struct Args {
    /// Path to the repository (defaults to current directory)
    #[arg(default_value = ".")]
    path: PathBuf,

    /// Number of days to analyze for churn (default: 14)
    #[arg(short, long, default_value = "14")]
    days: i64,

    /// Minimum days for a file to be considered dusty (default: 90)
    #[arg(short = 's', long, default_value = "90")]
    stale_days: i64,

    /// Print mood summary and exit (no TUI)
    #[arg(short, long)]
    check: bool,
}

fn main() -> Result<()> {
    let args = Args::parse();
    let path = args.path.canonicalize()?;

    // Analyze the repository
    eprintln!("🔍 Analyzing repository...");
    
    let git_analyzer = GitAnalyzer::new(&path)?;
    let staleness_analyzer = StalenessAnalyzer::new(&path)?;
    let todo_scanner = TodoScanner::new();

    let repo_name = git_analyzer.repo_name();
    let branch_name = git_analyzer.current_branch()?;
    
    eprintln!("  📊 Analyzing churn...");
    let churn_entries = git_analyzer.analyze_churn(args.days)?;
    let commits_recent = git_analyzer.commit_count(args.days)?;
    let add_delete_ratio = git_analyzer.add_delete_ratio(args.days)?;
    
    eprintln!("  🕸️  Finding dusty files...");
    let dusty_files = staleness_analyzer.find_dusty_files(args.stale_days)?;
    let total_files = staleness_analyzer.total_file_count()?;
    
    eprintln!("  📝 Scanning for TODOs...");
    let todo_entries = todo_scanner.scan(&path)?;

    // Calculate metrics and mood
    let metrics = RepoMetrics::from_analysis(
        total_files,
        &churn_entries,
        &todo_entries,
        &dusty_files,
        commits_recent,
        add_delete_ratio,
    );
    let mood = MoodEngine::calculate(&metrics);

    // Check mode: print summary and exit
    if args.check {
        print_summary(&mood, &metrics, &repo_name, &branch_name, &todo_entries);
        return Ok(());
    }

    eprintln!("  ✨ Done! Launching dashboard...\n");

    // Set up terminal
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    // Create app and run
    let mut app = App::new(
        mood,
        metrics,
        repo_name,
        branch_name,
        churn_entries,
        dusty_files,
        todo_entries,
    );

    let result = run_app(&mut terminal, &mut app);

    // Restore terminal
    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    )?;
    terminal.show_cursor()?;

    if let Err(err) = result {
        eprintln!("Error: {}", err);
    }

    Ok(())
}

fn run_app<B: Backend>(terminal: &mut Terminal<B>, app: &mut App) -> Result<()> {
    loop {
        terminal.draw(|f| ui::render(f, app))?;

        if let Event::Key(key) = event::read()? {
            if key.kind != KeyEventKind::Press {
                continue;
            }

            match key.code {
                KeyCode::Char('q') | KeyCode::Esc => {
                    app.should_quit = true;
                }
                KeyCode::Char('1') => app.select_panel(0),
                KeyCode::Char('2') => app.select_panel(1),
                KeyCode::Char('3') => app.select_panel(2),
                KeyCode::Tab => app.next_panel(),
                KeyCode::BackTab => app.prev_panel(),
                KeyCode::Down | KeyCode::Char('j') => app.scroll_down(),
                KeyCode::Up | KeyCode::Char('k') => app.scroll_up(),
                KeyCode::PageDown => {
                    for _ in 0..10 {
                        app.scroll_down();
                    }
                }
                KeyCode::PageUp => {
                    for _ in 0..10 {
                        app.scroll_up();
                    }
                }
                KeyCode::Home => app.scroll_offset = 0,
                KeyCode::End => {
                    app.scroll_offset = app.churn_entries.len().saturating_sub(1);
                }
                _ => {}
            }
        }

        if app.should_quit {
            return Ok(());
        }
    }
}

fn print_summary(
    mood: &mood::Mood,
    metrics: &mood::RepoMetrics,
    repo_name: &str,
    branch_name: &str,
    todos: &[analysis::TodoEntry],
) {
    let total_todos = metrics.todo_count + metrics.fixme_count + metrics.hack_count;
    
    println!();
    println!("┌─────────────────────────────────────────────────────────┐");
    println!("│  {} {}                                              │", mood.symbol(), mood.name());
    println!("│  \"{}\"{}│", mood.tagline(), " ".repeat(33 - mood.tagline().len()));
    println!("│                                                         │");
    println!("│  {} @ {}{}│", repo_name, branch_name, " ".repeat(46 - repo_name.len() - branch_name.len()));
    println!("├─────────────────────────────────────────────────────────┤");
    println!("│  📁 {} files  │  📝 {} changed  │  📌 {} TODOs  │  🕸️  {} dusty │",
        metrics.total_files,
        metrics.files_changed_recently,
        total_todos,
        metrics.dusty_file_count
    );
    println!("└─────────────────────────────────────────────────────────┘");
    
    if !todos.is_empty() {
        println!();
        println!("Top TODOs/FIXMEs:");
        for (i, todo) in todos.iter().take(5).enumerate() {
            println!("  {}. [{}] {}:{} - {}", 
                i + 1, 
                todo.kind.as_str(), 
                todo.path, 
                todo.line_number,
                if todo.text.len() > 50 { 
                    format!("{}...", &todo.text[..47]) 
                } else { 
                    todo.text.clone() 
                }
            );
        }
    }
    println!();
}

