//! Git-aware work context for Cosmos
//!
//! Tracks the current working state by inferring from git:
//! - Uncommitted changes
//! - Recent commits
//! - Current branch
//! - Work-in-progress detection

use chrono::{DateTime, Utc};
use git2::{Repository, StatusOptions};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Information about a git commit
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommitInfo {
    pub sha: String,
    pub short_sha: String,
    pub message: String,
    pub author: String,
    pub time: DateTime<Utc>,
    pub files_changed: Vec<String>,
}

/// Current work context inferred from git state
#[derive(Debug, Clone)]
pub struct WorkContext {
    /// Current branch name
    pub branch: String,
    /// Files with uncommitted modifications
    pub uncommitted_files: Vec<PathBuf>,
    /// Files staged for commit
    pub staged_files: Vec<PathBuf>,
    /// Untracked files (kept for potential future use)
    #[allow(dead_code)]
    pub untracked_files: Vec<PathBuf>,
    /// Recent commits (last 5)
    pub recent_commits: Vec<CommitInfo>,
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
        let recent_commits = get_recent_commits(&repo, 5)?;
        let modified_count = uncommitted.len() + staged.len();

        let inferred_focus = infer_focus(&uncommitted, &staged, &recent_commits);

        Ok(Self {
            branch,
            uncommitted_files: uncommitted,
            staged_files: staged,
            untracked_files: untracked,
            recent_commits,
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

    /// Check if there are any uncommitted changes
    #[allow(dead_code)]
    pub fn has_changes(&self) -> bool {
        !self.uncommitted_files.is_empty() || !self.staged_files.is_empty()
    }

    /// Get all changed files (uncommitted + staged)
    pub fn all_changed_files(&self) -> Vec<&PathBuf> {
        self.uncommitted_files
            .iter()
            .chain(self.staged_files.iter())
            .collect()
    }

    /// Get the most recently modified directories
    #[allow(dead_code)]
    pub fn active_directories(&self) -> Vec<String> {
        let mut dirs: std::collections::HashSet<String> = std::collections::HashSet::new();

        for file in self.all_changed_files() {
            if let Some(parent) = file.parent() {
                dirs.insert(parent.to_string_lossy().to_string());
            }
        }

        // Also include directories from recent commits
        for commit in &self.recent_commits {
            for file in &commit.files_changed {
                if let Some(parent) = Path::new(file).parent() {
                    dirs.insert(parent.to_string_lossy().to_string());
                }
            }
        }

        let mut dirs: Vec<_> = dirs.into_iter().collect();
        dirs.sort();
        dirs
    }

    /// Format status for display
    #[allow(dead_code)]
    pub fn status_line(&self) -> String {
        let mut parts = Vec::new();

        parts.push(self.branch.clone());

        if self.modified_count > 0 {
            parts.push(format!("{} changed", self.modified_count));
        }

        if !self.staged_files.is_empty() {
            parts.push(format!("{} staged", self.staged_files.len()));
        }

        parts.join(" | ")
    }

    /// Get a summary of the work context
    #[allow(dead_code)]
    pub fn summary(&self) -> String {
        let mut lines = Vec::new();

        lines.push(format!("Branch: {}", self.branch));

        if let Some(ref focus) = self.inferred_focus {
            lines.push(format!("Focus: {}", focus));
        }

        if self.has_changes() {
            lines.push(format!(
                "Changes: {} uncommitted, {} staged",
                self.uncommitted_files.len(),
                self.staged_files.len()
            ));
        }

        if !self.recent_commits.is_empty() {
            lines.push(format!("Recent: {}", self.recent_commits[0].message));
        }

        lines.join("\n")
    }
}

/// Get the current branch name
fn get_current_branch(repo: &Repository) -> anyhow::Result<String> {
    let head = repo.head()?;
    let shorthand = head.shorthand().unwrap_or("HEAD");
    Ok(shorthand.to_string())
}

/// Get file statuses (uncommitted, staged, untracked)
fn get_file_statuses(repo: &Repository) -> anyhow::Result<(Vec<PathBuf>, Vec<PathBuf>, Vec<PathBuf>)> {
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

            // Untracked files (new in working tree, but NOT staged)
            // A file that's been `git add`ed will have is_index_new() true,
            // so we exclude those from the untracked list.
            if status.is_wt_new() && !status.is_index_new() {
                untracked.push(path);
            }
        }
    }

    Ok((uncommitted, staged, untracked))
}

/// Get recent commits
fn get_recent_commits(repo: &Repository, count: usize) -> anyhow::Result<Vec<CommitInfo>> {
    let mut commits = Vec::new();

    let head = match repo.head() {
        Ok(h) => h,
        Err(_) => return Ok(commits), // Empty repo
    };

    let oid = match head.target() {
        Some(o) => o,
        None => return Ok(commits),
    };

    let mut revwalk = repo.revwalk()?;
    revwalk.push(oid)?;

    for (i, rev) in revwalk.enumerate() {
        if i >= count {
            break;
        }

        let oid = rev?;
        let commit = repo.find_commit(oid)?;

        let sha = oid.to_string();
        let short_sha = sha[..7.min(sha.len())].to_string();
        let message = commit
            .message()
            .unwrap_or("")
            .lines()
            .next()
            .unwrap_or("")
            .to_string();
        let author = commit.author().name().unwrap_or("Unknown").to_string();
        let time = DateTime::from_timestamp(commit.time().seconds(), 0)
            .unwrap_or_else(|| Utc::now());

        // Get files changed in this commit
        let files_changed = get_commit_files(repo, &commit)?;

        commits.push(CommitInfo {
            sha,
            short_sha,
            message,
            author,
            time,
            files_changed,
        });
    }

    Ok(commits)
}

/// Get files changed in a commit
fn get_commit_files(repo: &Repository, commit: &git2::Commit) -> anyhow::Result<Vec<String>> {
    let mut files = Vec::new();

    let tree = commit.tree()?;

    if let Ok(parent) = commit.parent(0) {
        let parent_tree = parent.tree()?;
        let diff = repo.diff_tree_to_tree(Some(&parent_tree), Some(&tree), None)?;

        diff.foreach(
            &mut |delta, _| {
                if let Some(path) = delta.new_file().path() {
                    files.push(path.to_string_lossy().to_string());
                }
                true
            },
            None,
            None,
            None,
        )?;
    }

    Ok(files)
}

/// Infer what the user is focused on based on changes and commits
fn infer_focus(
    uncommitted: &[PathBuf],
    staged: &[PathBuf],
    recent_commits: &[CommitInfo],
) -> Option<String> {
    // Collect all changed files
    let mut all_files: Vec<&str> = uncommitted
        .iter()
        .chain(staged.iter())
        .filter_map(|p| p.to_str())
        .collect();

    // Add files from recent commits
    for commit in recent_commits.iter().take(3) {
        for file in &commit.files_changed {
            all_files.push(file);
        }
    }

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
        let commits = vec![];

        let focus = infer_focus(&uncommitted, &staged, &commits);
        assert!(focus.is_some());
        assert!(focus.unwrap().contains("auth"));
    }
}
