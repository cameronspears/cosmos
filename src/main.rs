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
mod safe_apply;
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
use std::path::PathBuf;
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
fn init_index(path: &PathBuf, cache_manager: &cache::Cache) -> Result<CodebaseIndex> {
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
fn init_context(path: &PathBuf) -> Result<WorkContext> {
    eprint!("  Loading context...");

    let context = WorkContext::load(path)?;

    eprintln!(
        " {} on {}, {} changed",
        context.branch,
        if context.inferred_focus.is_some() {
            context.inferred_focus.as_ref().unwrap()
        } else {
            "project"
        },
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
    println!("  + API key configured. You can now use AI features!");
    Ok(())
}
