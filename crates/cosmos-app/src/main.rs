//! Cosmos - A contemplative vibe coding companion
//!
//! C O S M O S
//!
//! An AI-powered IDE in the terminal that uses codebase indexing
//! to suggest improvements, bug fixes, and optimizations.

use anyhow::Result;
use clap::Parser;
use cosmos_ui::context::WorkContext;
use cosmos_ui::index::CodebaseIndex;
use cosmos_ui::{app, cache, config, git_ops, github, keyring, onboarding, suggest};
use std::path::{Path, PathBuf};
use suggest::SuggestionEngine;

#[derive(Parser, Debug)]
#[command(
    name = "cosmos",
    about = "Terminal-based AI code reviewer",
    long_about = "C O S M O S\n\n\
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

    /// Authenticate with GitHub for PR creation
    #[arg(long)]
    github_login: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    // Handle --setup flag (BYOK mode)
    if args.setup {
        return setup_api_key();
    }

    // Handle --github-login flag
    if args.github_login {
        return github_login().await;
    }

    // Check if onboarding is needed (missing API key or GitHub auth)
    if onboarding::needs_onboarding() {
        onboarding::run_onboarding()
            .await
            .map_err(|e| anyhow::anyhow!("{}", e))?;

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

    let path = args.path.canonicalize()?;

    // Initialize cache
    let cache_manager = cache::Cache::new(&path);

    // Initialize index (fast, synchronous)
    let index = init_index(&path, &cache_manager)?;
    let context = init_context(&path)?;

    // Create suggestion engine (LLM suggestions generated on demand)
    let suggestions = SuggestionEngine::new(index.clone());

    // Run TUI with background LLM tasks
    app::run_tui(index, suggestions, context, cache_manager, path).await
}

/// Initialize the codebase index
fn init_index(path: &Path, cache_manager: &cache::Cache) -> Result<CodebaseIndex> {
    if let Some(index) = cache_manager.load_index_cache(path) {
        let stats = index.stats();
        eprintln!(
            "  Loaded index cache: {} files, {} symbols",
            stats.file_count, stats.symbol_count
        );
        if stats.skipped_files > 0 {
            eprintln!(
                "  Skipped {} files during last index build",
                stats.skipped_files
            );
            for err in index.index_errors.iter().take(3) {
                eprintln!("    - {}: {}", err.path.display(), err.reason);
            }
            if stats.skipped_files > 3 {
                eprintln!("    ({} more)", stats.skipped_files - 3);
            }
        }
        return Ok(index);
    }

    eprint!("  Indexing codebase...");

    let index = CodebaseIndex::new(path)?;
    let stats = index.stats();

    // Save index cache
    let _ = cache_manager.save_index_cache(&index);

    eprintln!(
        " {} files, {} symbols",
        stats.file_count, stats.symbol_count
    );
    if stats.skipped_files > 0 {
        eprintln!("  Skipped {} files during indexing", stats.skipped_files);
        for err in index.index_errors.iter().take(3) {
            eprintln!("    - {}: {}", err.path.display(), err.reason);
        }
        if stats.skipped_files > 3 {
            eprintln!("    ({} more)", stats.skipped_files - 3);
        }
    }

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
            eprintln!(
                "  ! This may be due to {} access issues.",
                keyring::credentials_store_label()
            );
            eprintln!();
            eprintln!("  Workaround: Set the OPENROUTER_API_KEY environment variable:");
            eprintln!("    export OPENROUTER_API_KEY=\"your-key-here\"");
            eprintln!();
            return Err(anyhow::anyhow!("API key verification failed"));
        }
    }

    Ok(())
}

/// Authenticate with GitHub using OAuth device flow
async fn github_login() -> Result<()> {
    use std::io::{self, Write};

    // Check if already authenticated
    if github::is_authenticated() {
        println!();
        println!("  Already authenticated with GitHub.");
        println!(
            "  To re-authenticate, unset GITHUB_TOKEN or clear credentials from {}.",
            keyring::credentials_store_label()
        );
        println!();
        return Ok(());
    }

    println!();
    println!("  GitHub Authentication");
    println!("  ─────────────────────");
    println!();
    println!("  Cosmos uses GitHub to create pull requests.");
    println!("  We'll open your browser to authenticate with GitHub.");
    println!();

    struct CliCallbacks {
        cancelled: bool,
    }

    impl github::DeviceFlowCallbacks for CliCallbacks {
        fn show_instructions(&mut self, instructions: &github::AuthInstructions) {
            println!("  To authenticate:");
            println!();
            println!("    1. Visit: {}", instructions.verification_uri);
            println!("    2. Enter code: {}", instructions.user_code);
            println!();
            print!("  Waiting for authorization...");
            let _ = io::stdout().flush();

            // Try to open the URL in the default browser
            let _ = git_ops::open_url(&instructions.verification_uri);
        }

        fn poll_status(&mut self) -> bool {
            print!(".");
            let _ = io::stdout().flush();
            !self.cancelled
        }

        fn on_success(&mut self, username: &str) {
            println!();
            println!();
            println!("  + Authenticated as @{}", username);
            println!("  + Token saved to {}", keyring::credentials_store_label());
            println!();
        }

        fn on_error(&mut self, error: &str) {
            println!();
            println!();
            eprintln!("  ! {}", error);
            println!();
        }
    }

    let mut callbacks = CliCallbacks { cancelled: false };
    github::run_device_flow(&mut callbacks).await?;

    Ok(())
}
