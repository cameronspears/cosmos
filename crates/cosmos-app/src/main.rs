//! Cosmos - A contemplative vibe coding companion
//!
//! C O S M O S
//!
//! An AI-powered IDE in the terminal that uses codebase indexing
//! to suggest improvements, bug fixes, and optimizations.

use anyhow::Result;
use clap::{Parser, ValueEnum};
use cosmos_adapters::{cache, config, git_ops, github, keyring};
use cosmos_core::context::WorkContext;
use cosmos_core::index::CodebaseIndex;
use cosmos_core::suggest::SuggestionEngine;
use cosmos_engine::llm;
use cosmos_ui::app;
use std::path::{Path, PathBuf};

#[derive(Copy, Clone, Debug, ValueEnum)]
enum SuggestProfileArg {
    Strict,
    Balanced,
    Max,
}

impl SuggestProfileArg {
    fn as_profile(self) -> config::SuggestionsProfile {
        match self {
            SuggestProfileArg::Strict => config::SuggestionsProfile::Strict,
            SuggestProfileArg::Balanced => config::SuggestionsProfile::BalancedHighVolume,
            SuggestProfileArg::Max => config::SuggestionsProfile::MaxVolume,
        }
    }
}

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

    /// Run suggestions in non-interactive mode and print quality/gate results
    #[arg(long)]
    suggest_audit: bool,

    /// Number of full suggestion runs to execute in audit mode
    #[arg(long, default_value_t = 1, requires = "suggest_audit")]
    suggest_runs: usize,

    /// Suggestions profile to use in audit mode
    #[arg(long, value_enum, default_value_t = SuggestProfileArg::Balanced, requires = "suggest_audit")]
    suggest_profile: SuggestProfileArg,

    /// Print accepted suggestions in audit mode
    #[arg(long, requires = "suggest_audit")]
    suggest_print: bool,
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

    let path = args.path.canonicalize()?;

    // Initialize cache
    let cache_manager = cache::Cache::new(&path);

    // Initialize index (fast, synchronous)
    let index = init_index(&path, &cache_manager)?;
    let context = init_context(&path)?;

    if args.suggest_audit {
        return run_suggestion_audit(
            &path,
            &index,
            &context,
            args.suggest_runs.max(1),
            args.suggest_profile,
            args.suggest_print,
        )
        .await;
    }

    // Create suggestion engine (LLM suggestions generated on demand)
    let suggestions = SuggestionEngine::new(index.clone());

    // Run TUI with background LLM tasks
    app::run_tui(index, suggestions, context, cache_manager, path).await
}

async fn run_suggestion_audit(
    path: &Path,
    index: &CodebaseIndex,
    context: &WorkContext,
    runs: usize,
    profile_arg: SuggestProfileArg,
    print_suggestions: bool,
) -> Result<()> {
    if !llm::is_available() {
        return Err(anyhow::anyhow!(
            "AI is unavailable. Configure an API key with `cosmos --setup` first."
        ));
    }

    let profile = profile_arg.as_profile();
    let mut gate_config = llm::suggestion_gate_config_for_profile(profile);
    gate_config.max_attempts = gate_config.max_attempts.max(4);

    let mut best_result: Option<llm::GatedSuggestionRunResult> = None;
    let mut best_key: Option<(usize, usize, usize)> = None; // (ethos_actionable_count, final_count, validated_count)
    let mut last_error: Option<String> = None;

    println!(
        "Running suggestion audit: runs={}, profile={:?}, target={}..{}, gates=disabled",
        runs, profile, gate_config.min_final_count, gate_config.max_final_count
    );

    for run_index in 1..=runs {
        println!("Run {}/{}...", run_index, runs);
        let run_timeout_ms = gate_config.max_suggest_ms.saturating_add(30_000);
        let run_result = tokio::time::timeout(
            std::time::Duration::from_millis(run_timeout_ms),
            llm::run_fast_grounded_with_gate_with_progress(
                path,
                index,
                context,
                None,
                gate_config.clone(),
                |attempt_index, attempt_count, gate, diagnostics| {
                    let prevalidation = diagnostics
                        .validation_rejection_histogram
                        .get("prevalidation")
                        .copied()
                        .unwrap_or(0);
                    let insufficient = diagnostics
                        .validation_rejection_histogram
                        .get("validator_insufficient_evidence")
                        .copied()
                        .unwrap_or(0);
                    println!(
                        "    attempt {}/{} final_count={} ethos_actionable_count={} pending={} provisional={} validated={} rejected={} prevalidation={} insufficient={} readiness_filtered={} semantic_dropped={} file_dropped={} strategy={}",
                        attempt_index,
                        attempt_count,
                        gate.final_count,
                        gate.ethos_actionable_count,
                        gate.pending_count,
                        diagnostics.provisional_count,
                        diagnostics.validated_count,
                        diagnostics.rejected_count,
                        prevalidation,
                        insufficient,
                        diagnostics.readiness_filtered_count,
                        diagnostics.semantic_dedup_dropped_count,
                        diagnostics.file_balance_dropped_count,
                        diagnostics.parse_strategy
                    );
                },
            ),
        )
        .await;

        match run_result {
            Ok(Ok(result)) => {
                let validated_count = result
                    .suggestions
                    .iter()
                    .filter(|s| {
                        s.validation_state
                            == cosmos_core::suggest::SuggestionValidationState::Validated
                    })
                    .count();
                println!(
                    "  PASS final_count={} validated_count={} ethos_actionable_count={} attempts={} cost=${:.4}",
                    result.gate.final_count,
                    validated_count,
                    result.gate.ethos_actionable_count,
                    result.diagnostics.attempt_index,
                    result.usage.as_ref().map(|u| u.cost()).unwrap_or(0.0)
                );

                let candidate_key = (
                    result.gate.ethos_actionable_count,
                    result.gate.final_count,
                    validated_count,
                );
                let is_better = best_key
                    .map(|current| candidate_key > current)
                    .unwrap_or(true);
                if is_better {
                    best_key = Some(candidate_key);
                    best_result = Some(result);
                }
            }
            Ok(Err(err)) => {
                let text = err.to_string();
                println!("  FAIL {}", text);
                last_error = Some(text);
            }
            Err(_) => {
                let text = format!("run timed out after {}ms", run_timeout_ms);
                println!("  FAIL {}", text);
                last_error = Some(text);
            }
        }
    }

    let Some(best) = best_result else {
        return Err(anyhow::anyhow!(
            "Suggestion audit did not pass in {} run(s). Last error: {}",
            runs,
            last_error.unwrap_or_else(|| "unknown".to_string())
        ));
    };

    println!(
        "Best run: final_count={} validated_count={} ethos_actionable_count={} fail_reasons={}",
        best.gate.final_count,
        best.suggestions
            .iter()
            .filter(|s| s.validation_state
                == cosmos_core::suggest::SuggestionValidationState::Validated)
            .count(),
        best.gate.ethos_actionable_count,
        if best.gate.fail_reasons.is_empty() {
            "none".to_string()
        } else {
            best.gate.fail_reasons.join("; ")
        }
    );

    if print_suggestions {
        println!("\nAccepted suggestions:");
        for (idx, suggestion) in best.suggestions.iter().enumerate() {
            let detail = suggestion.detail.as_deref().unwrap_or("");
            println!(
                "{}. [{:?}] {} ({}:{})",
                idx + 1,
                suggestion.priority,
                suggestion.summary,
                suggestion.file.display(),
                suggestion.line.unwrap_or(1)
            );
            println!("   {}", detail);
        }
    }

    Ok(())
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
