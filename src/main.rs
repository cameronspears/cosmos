mod analysis;
mod history;
mod score;
mod ui;

use analysis::{ComplexityAnalyzer, GitAnalyzer, StalenessAnalyzer, TodoScanner};
use anyhow::Result;
use clap::Parser;
use crossterm::{
    event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use history::ScoreHistory;
use ratatui::prelude::*;
use score::{HealthScore, RepoMetrics};
use serde::Serialize;
use std::io;
use std::path::PathBuf;
use std::process::ExitCode;
use ui::App;

#[derive(Parser, Debug)]
#[command(
    name = "codecosmos",
    about = "A terminal health dashboard for your codebase",
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

    /// Print health summary and exit (no TUI)
    #[arg(short, long)]
    check: bool,

    /// Minimum health score threshold (exit code 1 if below)
    #[arg(short = 't', long)]
    threshold: Option<u8>,

    /// Output results as JSON
    #[arg(long)]
    json: bool,

    /// Save current score to history
    #[arg(long)]
    save: bool,
}

/// JSON output structure for --json flag
#[derive(Serialize)]
struct JsonOutput {
    score: u8,
    grade: String,
    components: ComponentsOutput,
    metrics: MetricsOutput,
    danger_zones: Vec<DangerZoneOutput>,
}

#[derive(Serialize)]
struct ComponentsOutput {
    churn: u8,
    complexity: u8,
    debt: u8,
    freshness: u8,
}

#[derive(Serialize)]
struct MetricsOutput {
    total_files: usize,
    total_loc: usize,
    files_changed_recently: usize,
    todo_count: usize,
    fixme_count: usize,
    hack_count: usize,
    dusty_file_count: usize,
    danger_zone_count: usize,
}

#[derive(Serialize)]
struct DangerZoneOutput {
    path: String,
    danger_score: f64,
    change_count: usize,
    complexity_score: f64,
}

fn main() -> ExitCode {
    match run() {
        Ok(passed) => {
            if passed {
                ExitCode::SUCCESS
            } else {
                ExitCode::from(1)
            }
        }
        Err(e) => {
            eprintln!("Error: {}", e);
            ExitCode::from(1)
        }
    }
}

fn run() -> Result<bool> {
    let args = Args::parse();
    let path = args.path.canonicalize()?;

    // Analyze the repository
    if !args.json {
        eprintln!(":: Analyzing repository...");
    }

    let git_analyzer = GitAnalyzer::new(&path)?;
    let staleness_analyzer = StalenessAnalyzer::new(&path)?;
    let todo_scanner = TodoScanner::new();
    let complexity_analyzer = ComplexityAnalyzer::new();

    let repo_name = git_analyzer.repo_name();
    let branch_name = git_analyzer.current_branch()?;

    if !args.json {
        eprintln!("   -> churn");
    }
    let churn_entries = git_analyzer.analyze_churn(args.days)?;
    let commits_recent = git_analyzer.commit_count(args.days)?;

    if !args.json {
        eprintln!("   -> complexity");
    }
    let complexity_entries = complexity_analyzer.analyze(&path)?;
    let (total_loc, avg_complexity, max_complexity) =
        complexity_analyzer.aggregate_stats(&complexity_entries);

    if !args.json {
        eprintln!("   -> danger zones");
    }
    let danger_zones = complexity_analyzer.find_danger_zones(&churn_entries, &complexity_entries, 20);

    if !args.json {
        eprintln!("   -> staleness");
    }
    let dusty_files = staleness_analyzer.find_dusty_files(args.stale_days)?;
    let total_files = staleness_analyzer.total_file_count()?;

    if !args.json {
        eprintln!("   -> debt markers");
    }
    let todo_entries = todo_scanner.scan(&path)?;

    // Calculate metrics and score
    let metrics = RepoMetrics::from_analysis(
        total_files,
        total_loc,
        &churn_entries,
        &todo_entries,
        &dusty_files,
        commits_recent,
        avg_complexity,
        max_complexity,
        danger_zones.len(),
    );

    // Load history and calculate trend
    let mut history = ScoreHistory::load(&path).unwrap_or_default();
    let previous_score = history.latest_score();

    let score = HealthScore::calculate(&metrics).with_trend(previous_score);

    // Save to history if requested
    if args.save {
        history.add_entry(&score, Some(branch_name.clone()));
        if let Err(e) = history.save(&path) {
            if !args.json {
                eprintln!("   !! Failed to save history: {}", e);
            }
        } else if !args.json {
            eprintln!("   -> saved to history");
        }
    }

    // Check threshold
    let passes_threshold = args.threshold.map_or(true, |t| score.value >= t);

    // JSON output mode
    if args.json {
        let output = JsonOutput {
            score: score.value,
            grade: score.grade.to_string(),
            components: ComponentsOutput {
                churn: score.components.churn,
                complexity: score.components.complexity,
                debt: score.components.debt,
                freshness: score.components.freshness,
            },
            metrics: MetricsOutput {
                total_files: metrics.total_files,
                total_loc: metrics.total_loc,
                files_changed_recently: metrics.files_changed_recently,
                todo_count: metrics.todo_count,
                fixme_count: metrics.fixme_count,
                hack_count: metrics.hack_count,
                dusty_file_count: metrics.dusty_file_count,
                danger_zone_count: metrics.danger_zone_count,
            },
            danger_zones: danger_zones
                .iter()
                .map(|dz| DangerZoneOutput {
                    path: dz.path.clone(),
                    danger_score: dz.danger_score,
                    change_count: dz.change_count,
                    complexity_score: dz.complexity_score,
                })
                .collect(),
        };

        println!("{}", serde_json::to_string_pretty(&output)?);
        return Ok(passes_threshold);
    }

    // Check mode: print summary and exit
    if args.check {
        print_summary(&score, &metrics, &repo_name, &branch_name, &danger_zones, args.threshold);
        return Ok(passes_threshold);
    }

    if !args.json {
        eprintln!("   -> done\n");
    }

    // Set up terminal
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    // Create app and run
    let mut app = App::new(
        score,
        metrics,
        repo_name,
        branch_name,
        churn_entries,
        dusty_files,
        todo_entries,
        danger_zones,
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

    Ok(passes_threshold)
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
                KeyCode::Char('4') => app.select_panel(3),
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
                    let len = match app.active_panel {
                        ui::ActivePanel::DangerZones => app.danger_zones.len(),
                        ui::ActivePanel::Hotspots => app.churn_entries.len(),
                        ui::ActivePanel::DustyFiles => app.dusty_files.len(),
                        ui::ActivePanel::Todos => app.todo_entries.len(),
                    };
                    app.scroll_offset = len.saturating_sub(1);
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
    score: &HealthScore,
    metrics: &RepoMetrics,
    repo_name: &str,
    branch_name: &str,
    danger_zones: &[analysis::DangerZone],
    threshold: Option<u8>,
) {
    let total_todos = metrics.todo_count + metrics.fixme_count + metrics.hack_count;

    // Determine color codes for terminal
    let (score_color, reset) = if score.value >= 75 {
        ("\x1b[32m", "\x1b[0m") // Green
    } else if score.value >= 60 {
        ("\x1b[33m", "\x1b[0m") // Yellow
    } else {
        ("\x1b[31m", "\x1b[0m") // Red
    };

    let trend_str = match score.trend {
        score::Trend::Improving => " [+]",
        score::Trend::Declining => " [-]",
        score::Trend::Stable => " [=]",
        score::Trend::Unknown => "",
    };

    println!();
    println!("┌───────────────────────────────────────────────────────────────┐");
    println!(
        "│  {}{}/100 ({}){}{}                                              │",
        score_color, score.value, score.grade, trend_str, reset
    );
    println!(
        "│  \"{}\"{}│",
        score.grade.description(),
        " ".repeat(43 - score.grade.description().len())
    );
    println!("│                                                               │");
    println!(
        "│  {} @ {}{}│",
        repo_name,
        branch_name,
        " ".repeat(50 - repo_name.len() - branch_name.len())
    );
    println!("├───────────────────────────────────────────────────────────────┤");
    println!(
        "│  files: {:4}   danger: {:3}   todos: {:3}   dusty: {:3}        │",
        metrics.total_files, metrics.danger_zone_count, total_todos, metrics.dusty_file_count
    );
    println!("├───────────────────────────────────────────────────────────────┤");
    println!("│  Components:                                                  │");
    println!(
        "│    churn: {:3}   complexity: {:3}   debt: {:3}   freshness: {:3} │",
        score.components.churn,
        score.components.complexity,
        score.components.debt,
        score.components.freshness
    );
    println!("└───────────────────────────────────────────────────────────────┘");

    if !danger_zones.is_empty() {
        println!();
        println!("DANGER ZONES - files that are both complex AND frequently changed:");
        println!("(These are high-risk for bugs. Consider refactoring or adding tests.)");
        println!();
        for (i, dz) in danger_zones.iter().take(5).enumerate() {
            let risk_label = if dz.danger_score >= 70.0 {
                "CRITICAL"
            } else if dz.danger_score >= 50.0 {
                "HIGH    "
            } else {
                "MEDIUM  "
            };
            println!(
                "  {}. [{}] {}",
                i + 1,
                risk_label,
                dz.path
            );
            println!(
                "     ^ {} changes in window, complexity score {:.1}",
                dz.change_count,
                dz.complexity_score
            );
            // Actionable advice
            if dz.complexity_score > 10.0 {
                println!("     > Consider breaking this file into smaller modules");
            } else if dz.change_count > 10 {
                println!("     > High churn suggests instability - add test coverage");
            } else {
                println!("     > Review for opportunities to simplify");
            }
            println!();
        }
    }

    if let Some(t) = threshold {
        println!();
        if score.value >= t {
            println!("[PASS] Score {} meets threshold {}", score.value, t);
        } else {
            println!(
                "[FAIL] Score {} is below threshold {} (need +{})",
                score.value,
                t,
                t - score.value
            );
        }
    }

    println!();
}

#[allow(dead_code)]
fn truncate_path(path: &str, max_len: usize) -> String {
    if path.len() <= max_len {
        path.to_string()
    } else {
        let start = path.len() - max_len + 3;
        format!("...{}", &path[start..])
    }
}
