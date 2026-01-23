//! Git-aware work context for Cosmos
//!
//! Tracks the current working state by inferring from git:
//! - Uncommitted changes
//! - Current branch
//! - Work-in-progress detection

use git2::{Repository, StatusOptions};
use std::path::{Path, PathBuf};

/// Current work context inferred from git state
#[derive(Debug, Clone)]
pub struct WorkContext {
    /// Current branch name
    pub branch: String,
    /// Files with uncommitted modifications
    pub uncommitted_files: Vec<PathBuf>,
    /// Files staged for commit
    pub staged_files: Vec<PathBuf>,
    /// Untracked files in the working tree
    pub untracked_files: Vec<PathBuf>,
    /// Inferred focus area (what the user seems to be working on)
    pub inferred_focus: Option<String>,
    /// Total number of modified files
    pub modified_count: usize,
    /// Repository root path
    pub repo_root: PathBuf,
}

impl WorkContext {
    /// Load work context from a git repository
    pub fn load(repo_path: &Path) -> anyhow::Result<Self> {
        let repo = Repository::discover(repo_path)?;
        let repo_root = repo
            .workdir()
            .ok_or_else(|| anyhow::anyhow!("No working directory"))?
            .to_path_buf();

        let branch = get_current_branch(&repo)?;
        let (uncommitted, staged, untracked) = get_file_statuses(&repo)?;
        let modified_count = uncommitted.len() + staged.len() + untracked.len();

        let inferred_focus = infer_focus(&uncommitted, &staged, &untracked);

        Ok(Self {
            branch,
            uncommitted_files: uncommitted,
            staged_files: staged,
            untracked_files: untracked,
            inferred_focus,
            modified_count,
            repo_root,
        })
    }

    /// Refresh the context (e.g., after a change)
    pub fn refresh(&mut self) -> anyhow::Result<()> {
        let new_context = Self::load(&self.repo_root)?;
        *self = new_context;
        Ok(())
    }

    /// Get all changed files (uncommitted + staged)
    pub fn all_changed_files(&self) -> Vec<&PathBuf> {
        self.uncommitted_files
            .iter()
            .chain(self.staged_files.iter())
            .chain(self.untracked_files.iter())
            .collect()
    }
}

/// Get the current branch name
fn get_current_branch(repo: &Repository) -> anyhow::Result<String> {
    let head = repo.head()?;
    let shorthand = head.shorthand().unwrap_or("HEAD");
    Ok(shorthand.to_string())
}

/// Get file statuses (uncommitted, staged)
fn get_file_statuses(
    repo: &Repository,
) -> anyhow::Result<(Vec<PathBuf>, Vec<PathBuf>, Vec<PathBuf>)> {
    let mut opts = StatusOptions::new();
    opts.include_untracked(true);
    opts.include_ignored(false);
    opts.include_unmodified(false);
    opts.recurse_untracked_dirs(true);
    opts.exclude_submodules(true);

    let statuses = repo.statuses(Some(&mut opts))?;

    let mut uncommitted = Vec::new();
    let mut staged = Vec::new();
    let mut untracked = Vec::new();

    for entry in statuses.iter() {
        let status = entry.status();
        let path = entry.path().map(PathBuf::from);

        if let Some(path) = path {
            // Working tree modifications (not yet staged)
            if status.is_wt_modified() || status.is_wt_deleted() || status.is_wt_renamed() {
                uncommitted.push(path.clone());
            }

            // Staged changes (in index)
            if status.is_index_new() || status.is_index_modified() 
                || status.is_index_deleted() || status.is_index_renamed() 
            {
                staged.push(path.clone());
            }

            if status.is_wt_new() {
                untracked.push(path.clone());
            }
        }
    }

    Ok((uncommitted, staged, untracked))
}

/// Get recent commits
/// Infer what the user is focused on based on changes and commits
fn infer_focus(
    uncommitted: &[PathBuf],
    staged: &[PathBuf],
    untracked: &[PathBuf],
) -> Option<String> {
    // Collect all changed files
    let all_files: Vec<&str> = uncommitted
        .iter()
        .chain(staged.iter())
        .chain(untracked.iter())
        .filter_map(|p| p.to_str())
        .collect();

    if all_files.is_empty() {
        return None;
    }

    // Find common patterns/directories
    let mut dir_counts: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
    let mut keyword_counts: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();

    let focus_keywords = [
        "auth", "api", "db", "database", "ui", "test", "config", "util",
        "model", "view", "controller", "service", "handler", "route",
        "component", "hook", "store", "action", "reducer",
    ];

    for file in &all_files {
        // Count directories
        let parts: Vec<&str> = file.split('/').collect();
        if parts.len() > 1 {
            *dir_counts.entry(parts[0]).or_insert(0) += 1;
        }

        // Count keywords
        let lower = file.to_lowercase();
        for keyword in &focus_keywords {
            if lower.contains(keyword) {
                *keyword_counts.entry(*keyword).or_insert(0) += 1;
            }
        }
    }

    // Find most common directory
    let top_dir = dir_counts.iter().max_by_key(|(_, c)| *c);

    // Find most common keyword
    let top_keyword = keyword_counts.iter().max_by_key(|(_, c)| *c);

    // Build focus string
    match (top_dir, top_keyword) {
        (Some((dir, _)), Some((keyword, _))) => Some(format!("{} ({})", keyword, dir)),
        (Some((dir, _)), None) => Some(format!("{}/", dir)),
        (None, Some((keyword, _))) => Some(keyword.to_string()),
        (None, None) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_infer_focus() {
        let uncommitted = vec![
            PathBuf::from("src/auth/login.rs"),
            PathBuf::from("src/auth/session.rs"),
        ];
        let staged = vec![];
        let untracked = vec![];

        let focus = infer_focus(&uncommitted, &staged, &untracked);
        assert!(focus.is_some());
        assert!(focus.unwrap().contains("auth"));
    }
}
