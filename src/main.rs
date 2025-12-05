mod ai;
mod analysis;
mod config;
mod diff;
mod git_ops;
mod history;
mod mascot;
mod prompt;
mod score;
mod spinner;
mod testing;
mod ui;
mod workflow;

use analysis::{
    AuthorAnalyzer, ComplexityAnalyzer, GitAnalyzer, StalenessAnalyzer, TestAnalyzer, TodoScanner,
};
use prompt::PromptBuilder;
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
use ui::{App, Overlay};

#[derive(Parser, Debug)]
#[command(
    name = "codecosmos",
    about = "A terminal health dashboard for your codebase",
    long_about = "codecosmos - A sophisticated TUI for codebase health analysis.\n\n\
                  Analyze code complexity, churn, technical debt, test coverage,\n\
                  bus factor risk, and more. Get an instant health score (0-100)\n\
                  for any git repository.",
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

    /// Skip bus factor analysis (faster but less data)
    #[arg(long)]
    skip_authors: bool,

    /// Set up OpenRouter API key for AI features
    #[arg(long)]
    setup: bool,
}

/// JSON output structure for --json flag
#[derive(Serialize)]
struct JsonOutput {
    score: u8,
    grade: String,
    components: ComponentsOutput,
    metrics: MetricsOutput,
    danger_zones: Vec<DangerZoneOutput>,
    test_coverage: Option<TestCoverageOutput>,
    bus_factor: Option<BusFactorOutput>,
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

#[derive(Serialize)]
struct TestCoverageOutput {
    coverage_pct: f64,
    files_with_tests: usize,
    files_without_tests: usize,
    untested_danger_zones: Vec<String>,
}

#[derive(Serialize)]
struct BusFactorOutput {
    total_authors: usize,
    single_author_files: usize,
    avg_bus_factor: f64,
    high_risk_files: Vec<BusRiskOutput>,
}

#[derive(Serialize)]
struct BusRiskOutput {
    path: String,
    primary_author: String,
    primary_author_pct: f64,
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

    // Handle --setup flag
    if args.setup {
        match config::setup_api_key_interactive() {
            Ok(_) => {
                println!("  You can now use AI features! Press 'a' in the TUI.");
                return Ok(true);
            }
            Err(e) => {
                eprintln!("  Setup failed: {}", e);
                return Ok(false);
            }
        }
    }

    let path = args.path.canonicalize()?;

    // Analyze the repository with animated progress
    let git_analyzer = GitAnalyzer::new(&path)?;
    let staleness_analyzer = StalenessAnalyzer::new(&path)?;
    let todo_scanner = TodoScanner::new();
    let complexity_analyzer = ComplexityAnalyzer::new();
    let test_analyzer = TestAnalyzer::new();

    let repo_name = git_analyzer.repo_name();
    let branch_name = git_analyzer.current_branch()?;

    // Use animated spinner for analysis phase
    let use_spinner = !args.json && !args.check;
    let mut spin = if use_spinner {
        spinner::print_analysis_header(&repo_name);
        let s = spinner::Spinner::new(spinner::SpinnerStyle::Circle)
            .with_message("analyzing churn...");
        s.start();
        Some(s)
    } else {
        if !args.json {
            eprintln!(":: Analyzing repository...");
            eprintln!("   → churn");
        }
        None
    };

    let churn_entries = git_analyzer.analyze_churn(args.days)?;
    let commits_recent = git_analyzer.commit_count(args.days)?;

    if let Some(ref mut s) = spin {
        s.set_message("analyzing complexity...");
        s.tick();
    } else if !args.json {
        eprintln!("   → complexity");
    }
    let complexity_entries = complexity_analyzer.analyze(&path)?;
    let (total_loc, avg_complexity, max_complexity) =
        complexity_analyzer.aggregate_stats(&complexity_entries);

    if let Some(ref mut s) = spin {
        s.set_message("finding danger zones...");
        s.tick();
    } else if !args.json {
        eprintln!("   → danger zones");
    }
    let danger_zones =
        complexity_analyzer.find_danger_zones(&churn_entries, &complexity_entries, 20);

    if let Some(ref mut s) = spin {
        s.set_message("checking staleness...");
        s.tick();
    } else if !args.json {
        eprintln!("   → staleness");
    }
    let dusty_files = staleness_analyzer.find_dusty_files(args.stale_days)?;
    let total_files = staleness_analyzer.total_file_count()?;

    if let Some(ref mut s) = spin {
        s.set_message("scanning debt markers...");
        s.tick();
    } else if !args.json {
        eprintln!("   → debt markers");
    }
    let todo_entries = todo_scanner.scan(&path)?;

    if let Some(ref mut s) = spin {
        s.set_message("analyzing test coverage...");
        s.tick();
    } else if !args.json {
        eprintln!("   → test coverage");
    }
    let test_coverages = test_analyzer.analyze(&path)?;
    let danger_zone_paths: Vec<String> = danger_zones.iter().map(|d| d.path.clone()).collect();
    let test_summary = test_analyzer.summarize(&test_coverages, &danger_zone_paths);

    // Bus factor analysis (optional, can be slow on large repos)
    let (bus_factor_risks, author_stats) = if !args.skip_authors {
        if let Some(ref mut s) = spin {
            s.set_message("analyzing bus factor...");
            s.tick();
        } else if !args.json {
            eprintln!("   → bus factor");
        }
        let author_analyzer = AuthorAnalyzer::new(&path)?;
        let authorships = author_analyzer.analyze(&path, args.days)?;
        let risks = author_analyzer.find_high_risk_files(&authorships, 50);
        let stats = author_analyzer.aggregate_stats(&authorships, args.days)?;
        (risks, Some(stats))
    } else {
        (Vec::new(), None)
    };

    // Finish spinner
    if let Some(s) = spin {
        s.finish_with_message("analysis complete");
    }

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
    let history_entries = history.recent_entries(20).to_vec();

    let score = HealthScore::calculate(&metrics).with_trend(previous_score);

    // Save to history if requested
    if args.save {
        history.add_entry(&score, Some(branch_name.clone()));
        if let Err(e) = history.save(&path) {
            if !args.json {
                eprintln!("   !! Failed to save history: {}", e);
            }
        } else if !args.json {
            eprintln!("   → saved to history");
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
            test_coverage: Some(TestCoverageOutput {
                coverage_pct: test_summary.coverage_pct,
                files_with_tests: test_summary.files_with_tests,
                files_without_tests: test_summary.files_without_tests,
                untested_danger_zones: test_summary.untested_danger_zones.clone(),
            }),
            bus_factor: author_stats.as_ref().map(|s| BusFactorOutput {
                total_authors: s.total_authors,
                single_author_files: s.single_author_files,
                avg_bus_factor: s.avg_bus_factor,
                high_risk_files: bus_factor_risks
                    .iter()
                    .take(10)
                    .map(|r| BusRiskOutput {
                        path: r.path.clone(),
                        primary_author: r.primary_author.clone(),
                        primary_author_pct: r.primary_author_pct,
                    })
                    .collect(),
            }),
        };

        println!("{}", serde_json::to_string_pretty(&output)?);
        return Ok(passes_threshold);
    }

    // Check mode: print summary and exit
    if args.check {
        print_summary(
            &score,
            &metrics,
            &repo_name,
            &branch_name,
            &danger_zones,
            &test_summary,
            author_stats.as_ref(),
            args.threshold,
        );
        return Ok(passes_threshold);
    }

    // Set up terminal
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    // Create prompt builder for clipboard support
    let prompt_builder = PromptBuilder::new();

    // Create app with all data
    let mut app = App::new(
        score,
        metrics,
        repo_name,
        branch_name,
        path.clone(),
        churn_entries,
        dusty_files,
        todo_entries,
        danger_zones,
    )
    .with_tests(test_coverages, test_summary)
    .with_history(history_entries)
    .with_complexity(complexity_entries)
    .with_prompt_builder(prompt_builder);

    // Add bus factor data if available
    if let Some(stats) = author_stats {
        app = app.with_bus_factor(bus_factor_risks, stats);
    }

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

fn run_app<B: Backend + std::io::Write>(terminal: &mut Terminal<B>, app: &mut App) -> Result<()> {
    loop {
        // Clear expired toasts
        app.clear_expired_toast();
        
        terminal.draw(|f| ui::render(f, app))?;

        if let Event::Key(key) = event::read()? {
            if key.kind != KeyEventKind::Press {
                continue;
            }

            // Handle search input mode
            if app.search_active {
                match key.code {
                    KeyCode::Esc => app.end_search(),
                    KeyCode::Enter => app.end_search(),
                    KeyCode::Backspace => app.search_backspace(),
                    KeyCode::Char(c) => app.search_input(c),
                    _ => {}
                }
                continue;
            }

            // Handle input prompt mode
            if let Overlay::InputPrompt { .. } = &app.overlay {
                match key.code {
                    KeyCode::Esc => app.close_overlay(),
                    KeyCode::Enter => {
                        if let Some((input, action)) = app.get_input_value() {
                            app.close_overlay();
                            match action {
                                ui::InputAction::CreateBranch => {
                                    if !input.is_empty() {
                                        match git_ops::create_and_checkout_branch(&app.repo_path, &input) {
                                            Ok(_) => {
                                                app.branch_name = input.clone();
                                                app.toast = Some(ui::Toast::new(&format!("Created branch: {}", input)));
                                            }
                                            Err(e) => {
                                                app.toast = Some(ui::Toast::new(&format!("Error: {}", e)));
                                            }
                                        }
                                    }
                                }
                                ui::InputAction::CommitMessage => {
                                    if !input.is_empty() {
                                        match git_ops::commit(&app.repo_path, &input) {
                                            Ok(sha) => {
                                                let short_sha = &sha[..7.min(sha.len())];
                                                app.toast = Some(ui::Toast::new(&format!("Committed: {}", short_sha)));
                                                app.workflow.ready_to_push(app.branch_name.clone(), sha);
                                            }
                                            Err(e) => {
                                                app.toast = Some(ui::Toast::new(&format!("Error: {}", e)));
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                    KeyCode::Backspace => app.input_backspace(),
                    KeyCode::Char(c) => app.input_char(c),
                    _ => {}
                }
                continue;
            }

            // Handle overlay mode
            if app.overlay != Overlay::None {
                match key.code {
                    KeyCode::Esc | KeyCode::Char('q') => app.close_overlay(),
                    // Scroll in overlays
                    KeyCode::Down | KeyCode::Char('j') => app.overlay_scroll_down(),
                    KeyCode::Up | KeyCode::Char('k') => app.overlay_scroll_up(),
                    KeyCode::PageDown | KeyCode::Char('d') => app.overlay_page_down(),
                    KeyCode::PageUp | KeyCode::Char('u') => app.overlay_page_up(),
                    // Apply diff in DiffPreview
                    KeyCode::Enter | KeyCode::Char('y') => {
                        if let Overlay::DiffPreview { file_path, diff, original_content, .. } = &app.overlay {
                            let file_path = file_path.clone();
                            let diff = diff.clone();
                            let original_content = original_content.clone();
                            
                            let full_path = app.repo_path.join(&file_path);
                            match diff::apply_diff_to_file(&full_path, &diff) {
                                Ok(_) => {
                                    app.toast = Some(ui::Toast::new(&format!("Applied fix to {}", file_path)));
                                    app.workflow.mark_applied(file_path.clone(), original_content);
                                    app.workflow.record_fix(file_path);
                                    app.close_overlay();
                                }
                                Err(e) => {
                                    app.toast = Some(ui::Toast::new(&format!("Error: {}", e)));
                                }
                            }
                        } else {
                            app.close_overlay();
                        }
                    }
                    KeyCode::Char('p') => {
                        app.close_overlay();
                        app.generate_prompt();
                    }
                    KeyCode::Char('c') => {
                        app.close_overlay();
                        app.copy_path();
                    }
                    _ => {}
                }
                continue;
            }

            // Normal mode
            match key.code {
                KeyCode::Char('q') => app.should_quit = true,
                KeyCode::Esc => {
                    if app.overlay != Overlay::None {
                        app.close_overlay();
                    }
                }
                KeyCode::Char('1') => app.select_panel(0),
                KeyCode::Char('2') => app.select_panel(1),
                KeyCode::Char('3') => app.select_panel(2),
                KeyCode::Char('4') => app.select_panel(3),
                KeyCode::Char('5') => app.select_panel(4),
                KeyCode::Char('6') => app.select_panel(5),
                KeyCode::Tab => app.next_panel(),
                KeyCode::BackTab => app.prev_panel(),
                KeyCode::Down | KeyCode::Char('j') => app.scroll_down(),
                KeyCode::Up | KeyCode::Char('k') => app.scroll_up(),
                KeyCode::Char('/') => app.start_search(),
                KeyCode::Char('?') => app.toggle_help(),
                KeyCode::Enter => app.show_detail(),
                KeyCode::Char('p') => app.generate_prompt(),
                KeyCode::Char('c') => app.copy_path(),
                // AI Fix - generate diff patch
                KeyCode::Char('a') => {
                    if !ai::is_available() {
                        app.toast = Some(ui::Toast::new("Run: codecosmos --setup"));
                    } else if let Some(ctx) = app.build_file_context() {
                        let path = ctx.path.clone();
                        let content = ctx.file_content.clone().unwrap_or_default();
                        let issue = ctx.issue_summary();
                        
                        // Properly exit TUI mode
                        execute!(
                            terminal.backend_mut(),
                            LeaveAlternateScreen,
                            DisableMouseCapture
                        )?;
                        disable_raw_mode()?;
                        
                        println!();
                        println!("  🤖 Generating fix for {}...", path);
                        println!();
                        // Flush to ensure output is visible
                        use std::io::Write;
                        let _ = std::io::stdout().flush();
                        
                        let rt = tokio::runtime::Runtime::new().unwrap();
                        let result = rt.block_on(ai::generate_fix(&path, &content, &issue));
                        
                        // Re-enter TUI mode
                        enable_raw_mode()?;
                        execute!(
                            terminal.backend_mut(),
                            EnterAlternateScreen,
                            EnableMouseCapture
                        )?;
                        terminal.clear()?;
                        
                        match result {
                            Ok(diff_text) => {
                                match diff::parse_unified_diff(&diff_text) {
                                    Ok(parsed_diff) => {
                                        app.overlay = ui::Overlay::DiffPreview {
                                            file_path: path,
                                            diff: parsed_diff,
                                            scroll: 0,
                                            original_content: content,
                                        };
                                    }
                                    Err(_) => {
                                        // Fallback to showing raw AI response
                                        app.overlay = ui::Overlay::AiChat { 
                                            content: diff_text, 
                                            scroll: 0 
                                        };
                                    }
                                }
                            }
                            Err(e) => {
                                app.toast = Some(ui::Toast::new(&format!("AI error: {}", e)));
                            }
                        }
                    }
                }
                // Run tests
                KeyCode::Char('t') => {
                    if let Some(file_path) = app.selected_file_path() {
                        execute!(
                            terminal.backend_mut(),
                            LeaveAlternateScreen,
                            DisableMouseCapture
                        )?;
                        disable_raw_mode()?;
                        
                        println!();
                        println!("  🧪 Running tests...");
                        println!();
                        use std::io::Write;
                        let _ = std::io::stdout().flush();
                        
                        let result = testing::run_tests_for_file(&app.repo_path, &file_path);
                        
                        enable_raw_mode()?;
                        execute!(
                            terminal.backend_mut(),
                            EnterAlternateScreen,
                            EnableMouseCapture
                        )?;
                        terminal.clear()?;
                        
                        app.overlay = ui::Overlay::TestResults {
                            passed: result.passed,
                            output: result.output,
                            scroll: 0,
                        };
                    }
                }
                // AI Review
                KeyCode::Char('r') => {
                    if !ai::is_available() {
                        app.toast = Some(ui::Toast::new("Run: codecosmos --setup"));
                    } else if let Some(ctx) = app.build_file_context() {
                        let path = ctx.path.clone();
                        let content = ctx.file_content.clone().unwrap_or_default();
                        
                        // For review, we need original vs current
                        // For now, just review the current content
                        execute!(
                            terminal.backend_mut(),
                            LeaveAlternateScreen,
                            DisableMouseCapture
                        )?;
                        disable_raw_mode()?;
                        
                        println!();
                        println!("  🔍 AI reviewing {}...", path);
                        println!();
                        use std::io::Write;
                        let _ = std::io::stdout().flush();
                        
                        let rt = tokio::runtime::Runtime::new().unwrap();
                        // Use the content as both original and modified for now
                        // In a real workflow, we'd compare against the git HEAD version
                        let result = rt.block_on(ai::review_changes(&content, &content, &path));
                        
                        enable_raw_mode()?;
                        execute!(
                            terminal.backend_mut(),
                            EnterAlternateScreen,
                            EnableMouseCapture
                        )?;
                        terminal.clear()?;
                        
                        match result {
                            Ok(review) => {
                                app.overlay = ui::Overlay::ReviewResults {
                                    result: review,
                                    scroll: 0,
                                };
                            }
                            Err(e) => {
                                app.toast = Some(ui::Toast::new(&format!("Review error: {}", e)));
                            }
                        }
                    }
                }
                // View git diff of current changes
                KeyCode::Char('d') => {
                    execute!(
                        terminal.backend_mut(),
                        LeaveAlternateScreen,
                        DisableMouseCapture
                    )?;
                    disable_raw_mode()?;
                    
                    // Show git diff
                    let output = std::process::Command::new("git")
                        .current_dir(&app.repo_path)
                        .args(["diff", "--color=always"])
                        .output();
                    
                    println!();
                    if let Ok(out) = output {
                        println!("{}", String::from_utf8_lossy(&out.stdout));
                    } else {
                        println!("  No changes or git error");
                    }
                    println!();
                    println!("  Press any key to continue...");
                    let _ = event::read();
                    
                    enable_raw_mode()?;
                    execute!(
                        terminal.backend_mut(),
                        EnterAlternateScreen,
                        EnableMouseCapture
                    )?;
                    terminal.clear()?;
                }
                // Undo/revert changes to selected file
                KeyCode::Char('z') => {
                    if let Some(file_path) = app.selected_file_path() {
                        match git_ops::reset_file(&app.repo_path, &file_path) {
                            Ok(_) => {
                                app.toast = Some(ui::Toast::new(&format!("Reverted: {}", file_path)));
                            }
                            Err(e) => {
                                app.toast = Some(ui::Toast::new(&format!("Error: {}", e)));
                            }
                        }
                    }
                }
                // Create branch
                KeyCode::Char('b') => {
                    app.overlay = ui::Overlay::InputPrompt {
                        title: "Create Branch".to_string(),
                        prompt: "Branch name:".to_string(),
                        input: String::new(),
                        action: ui::InputAction::CreateBranch,
                    };
                }
                // Commit (capital C)
                KeyCode::Char('C') => {
                    match git_ops::current_status(&app.repo_path) {
                        Ok(status) => {
                            if status.modified.is_empty() && status.staged.is_empty() {
                                app.toast = Some(ui::Toast::new("No changes to commit"));
                            } else {
                                // Stage all modified files first
                                let _ = git_ops::stage_all(&app.repo_path);
                                
                                app.overlay = ui::Overlay::InputPrompt {
                                    title: "Commit".to_string(),
                                    prompt: "Commit message:".to_string(),
                                    input: String::new(),
                                    action: ui::InputAction::CommitMessage,
                                };
                            }
                        }
                        Err(e) => {
                            app.toast = Some(ui::Toast::new(&format!("Git error: {}", e)));
                        }
                    }
                }
                // Push and create PR (capital P)
                KeyCode::Char('P') => {
                    if !git_ops::gh_available() {
                        app.toast = Some(ui::Toast::new("gh CLI not installed"));
                    } else if !git_ops::gh_authenticated() {
                        app.toast = Some(ui::Toast::new("Run: gh auth login"));
                    } else {
                        execute!(
                            terminal.backend_mut(),
                            LeaveAlternateScreen,
                            DisableMouseCapture
                        )?;
                        disable_raw_mode()?;
                        
                        println!();
                        println!("  📤 Pushing {} and creating PR...", app.branch_name);
                        println!();
                        use std::io::Write;
                        let _ = std::io::stdout().flush();
                        
                        // Push first
                        match git_ops::push_branch(&app.repo_path, &app.branch_name) {
                            Ok(_) => {
                                // Then create PR
                                let title = format!("fix: {}", app.branch_name);
                                let body = format!("## Summary\n\nAutomated fix by codecosmos\n\n## Files Changed\n\n{}", 
                                    app.workflow.fixed_files.join("\n- "));
                                
                                match git_ops::create_pr(&app.repo_path, &title, &body) {
                                    Ok(url) => {
                                        println!("  ✓ PR created: {}", url);
                                        let _ = git_ops::open_url(&url);
                                        app.workflow.complete(url);
                                    }
                                    Err(e) => {
                                        println!("  ✗ PR creation failed: {}", e);
                                    }
                                }
                            }
                            Err(e) => {
                                println!("  ✗ Push failed: {}", e);
                            }
                        }
                        
                        println!();
                        println!("  Press any key to continue...");
                        let _ = event::read();
                        
                        enable_raw_mode()?;
                        execute!(
                            terminal.backend_mut(),
                            EnterAlternateScreen,
                            EnableMouseCapture
                        )?;
                        terminal.clear()?;
                    }
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
    test_summary: &analysis::TestSummary,
    author_stats: Option<&analysis::AuthorStats>,
    threshold: Option<u8>,
) {
    use mascot::Mascot;
    
    let total_todos = metrics.todo_count + metrics.fixme_count + metrics.hack_count;
    let comment = Mascot::comment(score.value);
    let emoji = Mascot::emoji(score.value);

    // Build visual score bar
    let bar_width = 25;
    let filled = (score.value as usize * bar_width) / 100;
    let score_bar: String = (0..bar_width)
        .map(|i| if i < filled { '█' } else { '░' })
        .collect();

    // Trend indicator
    let trend_str = match score.trend {
        score::Trend::Improving => " ↑",
        score::Trend::Declining => " ↓", 
        score::Trend::Stable => "",
        score::Trend::Unknown => "",
    };

    println!();
    println!("  ┌────────────────────────────────────────────────────┐");
    println!("  │  CODECOSMOS                                        │");
    println!("  │  {} @ {}{}│", 
        repo_name, branch_name, 
        " ".repeat(40usize.saturating_sub(repo_name.len() + branch_name.len() + 3)));
    println!("  └────────────────────────────────────────────────────┘");
    println!();

    // Big score display
    println!("       {} {:>3}  {}", emoji, score.value, score_bar);
    println!("          ({}){}", score.grade, trend_str);
    println!();
    println!("       \"{}\"", comment);
    println!();

    // Component bars - compact
    let make_bar = |value: u8, label: &str| {
        let w = 12;
        let f = (value as usize * w) / 100;
        let bar: String = (0..w).map(|i| if i < f { '█' } else { '░' }).collect();
        format!("       {:11} {} {:>3}", label, bar, value)
    };
    
    println!("{}", make_bar(score.components.churn, "churn"));
    println!("{}", make_bar(score.components.complexity, "complexity"));
    println!("{}", make_bar(score.components.debt, "debt"));
    println!("{}", make_bar(score.components.freshness, "freshness"));
    println!();

    // Stats line
    println!("  ┌────────────────────────────────────────────────────┐");
    println!("  │  {:>5} files   {:>3} danger   {:>3} todos   {:.0}% tested │", 
        metrics.total_files, metrics.danger_zone_count, total_todos, test_summary.coverage_pct);
    if let Some(stats) = author_stats {
        println!("  │  {:>5} LOC     {:>3} dusty    {:>3} authors  {:.1} bus    │",
            metrics.total_loc, metrics.dusty_file_count, stats.total_authors, stats.avg_bus_factor);
    } else {
        println!("  │  {:>5} LOC     {:>3} dusty                          │",
            metrics.total_loc, metrics.dusty_file_count);
    }
    println!("  └────────────────────────────────────────────────────┘");

    // Danger zones section
    if !danger_zones.is_empty() {
        println!();
        println!("  🔥 DANGER ZONES");
        println!();
        
        for (i, dz) in danger_zones.iter().take(5).enumerate() {
            let intensity = if dz.danger_score >= 70.0 { "▓▓" } 
                else if dz.danger_score >= 50.0 { "▓░" } 
                else { "░░" };
            
            let path_display = if dz.path.len() > 40 {
                format!("...{}", &dz.path[dz.path.len()-37..])
            } else {
                dz.path.clone()
            };
            
            println!("     {}. {} {}  {:>2}%", i + 1, intensity, path_display, dz.danger_score as u8);
        }
    }

    // Untested danger zones warning
    if !test_summary.untested_danger_zones.is_empty() {
        println!();
        println!("  ⚠️  UNTESTED DANGER ZONES");
        for path in test_summary.untested_danger_zones.iter().take(3) {
            let path_display = if path.len() > 50 {
                format!("...{}", &path[path.len()-47..])
            } else {
                path.clone()
            };
            println!("     ○ {}", path_display);
        }
    }

    // Threshold check
    if let Some(t) = threshold {
        println!();
        if score.value >= t {
            println!("  ✅ PASS (score {} ≥ threshold {})", score.value, t);
        } else {
            println!("  ❌ FAIL (score {} < threshold {}, need +{})", score.value, t, t - score.value);
        }
    }

    // Footer
    println!();
    println!("  ─────────────────────────────────────────────────────");
    println!("  codecosmos • github.com/yourusername/codecosmos");
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
