//! Cosmos UI shell entrypoint.

use anyhow::Result;
use clap::Parser;
use cosmos_ui::context::WorkContext;
use cosmos_ui::index::CodebaseIndex;
use cosmos_ui::{app, cache, suggest};
use std::collections::HashMap;
use std::path::PathBuf;
use suggest::SuggestionEngine;

#[derive(Parser, Debug)]
#[command(name = "cosmos", about = "Terminal UI shell", version)]
struct Args {
    /// Path to the repository (defaults to current directory)
    #[arg(default_value = ".")]
    path: PathBuf,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let path = args.path.canonicalize()?;
    let cache_manager = cache::Cache::new(&path);

    // UI shell mode: no indexing or code analysis.
    let index = CodebaseIndex {
        root: path.clone(),
        files: HashMap::new(),
        index_errors: Vec::new(),
        git_head: None,
    };

    let context = WorkContext::load(&path).unwrap_or(WorkContext {
        branch: "main".to_string(),
        uncommitted_files: Vec::new(),
        staged_files: Vec::new(),
        untracked_files: Vec::new(),
        inferred_focus: None,
        modified_count: 0,
        repo_root: path.clone(),
    });

    let suggestions = SuggestionEngine::new(index.clone());
    app::run_tui(index, suggestions, context, cache_manager, path).await
}
