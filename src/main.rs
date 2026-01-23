//! Cosmos - A contemplative vibe coding companion
//!
//! C O S M O S
//!
//! An AI-powered IDE in the terminal that uses codebase indexing
//! to suggest improvements, bug fixes, and optimizations.

mod cache;
mod config;
mod context;
mod grouping;
mod index;
mod onboarding;
mod suggest;
mod ui;
mod app;
mod util;

// Keep these for compatibility during transition
mod git_ops;

use anyhow::Result;
use clap::Parser;
use context::WorkContext;
use index::CodebaseIndex;
use std::path::{Path, PathBuf};
use suggest::SuggestionEngine;
use util::truncate;

#[derive(Parser, Debug)]
#[command(
    name = "cosmos",
    about = "A contemplative vibe coding companion",
    long_about = "C O S M O S\n\n\
                  A contemplative companion for your codebase.\n\n\
                  Uses AST-based indexing and AI to suggest improvements,\n\
                  bug fixes, features, and optimizations.",
    version
)]
struct Args {
    /// Path to the repository (defaults to current directory)
    #[arg(default_value = ".")]
    path: PathBuf,

    /// Set up OpenRouter API key for AI features (BYOK mode)
    #[arg(long)]
    setup: bool,

    /// Show stats and exit (no TUI)
    #[arg(long)]
    stats: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    // Handle --setup flag (BYOK mode)
    if args.setup {
        return setup_api_key();
    }

    // Check for first run and show onboarding
    if onboarding::is_first_run() {
        match onboarding::run_onboarding() {
            Ok(true) => {
                // Setup completed, verify API key is accessible
                let mut config = config::Config::load();
                match config.get_api_key() {
                    Some(_) => {
                        eprintln!("  + API key verified and ready to use");
                        eprintln!();
                    }
                    None => {
                        eprintln!("  ! Warning: API key was saved but cannot be read back.");
                        eprintln!("  ! This may be due to keychain access issues.");
                        eprintln!("  ! Workaround: Set OPENROUTER_API_KEY environment variable.");
                        eprintln!();
                        eprintln!("  Press Enter to continue...");
                        let mut _input = String::new();
                        let _ = std::io::stdin().read_line(&mut _input);
                    }
                }
            }
            Ok(false) => {
                // Setup skipped, continue to TUI
                eprintln!();
            }
            Err(e) => {
                eprintln!("  Onboarding error: {}", e);
                eprintln!("  Continuing without setup...");
            }
        }
    }

    let path = args.path.canonicalize()?;

    // Initialize cache
    let cache_manager = cache::Cache::new(&path);

    // Initialize index (fast, synchronous)
    let index = init_index(&path, &cache_manager)?;
    let context = init_context(&path)?;

    // Create suggestion engine (LLM suggestions generated on demand)
    let suggestions = SuggestionEngine::new(index.clone());

    // Stats mode: print and exit
    if args.stats {
        print_stats(&index, &suggestions, &context);
        return Ok(());
    }

    // Run TUI with background LLM tasks
    app::run_tui(index, suggestions, context, cache_manager, path).await
}

/// Initialize the codebase index
fn init_index(path: &Path, cache_manager: &cache::Cache) -> Result<CodebaseIndex> {
    eprint!("  Indexing codebase...");

    let index = CodebaseIndex::new(path)?;
    let stats = index.stats();

    // Save index cache
    let index_cache = cache::IndexCache::from_index(&index);
    let _ = cache_manager.save_index_cache(&index_cache);

    eprintln!(
        " {} files, {} symbols",
        stats.file_count, stats.symbol_count
    );

    Ok(index)
}

/// Initialize the work context
fn init_context(path: &Path) -> Result<WorkContext> {
    eprint!("  Loading context...");

    let context = WorkContext::load(path)?;

    eprintln!(
        " {} on {}, {} changed",
        context.branch,
        context.inferred_focus.as_deref().unwrap_or("project"),
        context.modified_count
    );

    Ok(context)
}

/// Print stats and exit
fn print_stats(index: &CodebaseIndex, suggestions: &SuggestionEngine, context: &WorkContext) {
    let stats = index.stats();
    let counts = suggestions.counts();

    println!();
    println!("  ╔══════════════════════════════════════════════════╗");
    println!("  ║             C O S M O S   Stats                  ║");
    println!("  ╠══════════════════════════════════════════════════╣");
    println!("  ║                                                  ║");
    println!(
        "  ║  Files:     {:>6}                               ║",
        stats.file_count
    );
    println!(
        "  ║  LOC:       {:>6}                               ║",
        stats.total_loc
    );
    println!(
        "  ║  Symbols:   {:>6}                               ║",
        stats.symbol_count
    );
    println!(
        "  ║  Patterns:  {:>6}                               ║",
        stats.pattern_count
    );
    println!("  ║                                                  ║");
    println!("  ║  Suggestions:                                    ║");
    println!(
        "  ║    High:    {:>6} ●                             ║",
        counts.high
    );
    println!(
        "  ║    Medium:  {:>6} ◐                             ║",
        counts.medium
    );
    println!(
        "  ║    Low:     {:>6} ○                             ║",
        counts.low
    );
    println!("  ║                                                  ║");
    println!("  ║  Context:                                        ║");
    println!(
        "  ║    Branch:  {:>20}               ║",
        truncate(&context.branch, 20)
    );
    println!(
        "  ║    Changed: {:>6}                               ║",
        context.modified_count
    );
    println!("  ║                                                  ║");
    println!("  ╚══════════════════════════════════════════════════╝");
    println!();

    // Top suggestions
    let top = suggestions.high_priority_suggestions();
    if !top.is_empty() {
        println!("  Top suggestions:");
        println!();
        for (i, s) in top.iter().take(5).enumerate() {
            println!(
                "    {}. {} {}: {}",
                i + 1,
                s.priority.icon(),
                s.kind.label(),
                truncate(&s.summary, 50)
            );
        }
        println!();
    }
}

/// Set up the API key interactively
fn setup_api_key() -> Result<()> {
    config::setup_api_key_interactive().map_err(|e| anyhow::anyhow!("{}", e))?;
    
    // Verify the key is readable
    let mut config = config::Config::load();
    match config.get_api_key() {
        Some(_) => {
            println!("  + API key verified and ready to use!");
        }
        None => {
            eprintln!();
            eprintln!("  ! Warning: API key was saved but cannot be read back.");
            eprintln!("  ! This may be due to system keychain access issues.");
            eprintln!();
            eprintln!("  Workaround: Set the OPENROUTER_API_KEY environment variable:");
            eprintln!("    export OPENROUTER_API_KEY=\"your-key-here\"");
            eprintln!();
            return Err(anyhow::anyhow!("API key verification failed"));
        }
    }
    
    Ok(())
}
