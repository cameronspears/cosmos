//! Git operations for the fix-and-ship workflow
//!
//! Provides branch, stage, commit, and push operations.

use anyhow::{Context, Result};
use git2::{Repository, Signature, IndexAddOption};
use std::path::Path;
use std::process::Command;

/// Status of the working directory
#[derive(Debug, Clone, Default)]
pub struct GitStatus {
    pub branch: String,
    pub staged: Vec<String>,
    pub modified: Vec<String>,
    pub untracked: Vec<String>,
    pub ahead: usize,
    pub behind: usize,
}

/// Get the current git status
pub fn current_status(repo_path: &Path) -> Result<GitStatus> {
    let repo = Repository::open(repo_path)?;
    
    let head = repo.head().context("Failed to get HEAD")?;
    let branch = head.shorthand().unwrap_or("detached").to_string();
    
    let mut status = GitStatus {
        branch,
        ..Default::default()
    };
    
    let statuses = repo.statuses(None)?;
    
    for entry in statuses.iter() {
        let path = entry.path().unwrap_or("").to_string();
        let s = entry.status();
        
        if s.is_index_new() || s.is_index_modified() || s.is_index_deleted() {
            status.staged.push(path.clone());
        }
        if s.is_wt_modified() || s.is_wt_deleted() {
            status.modified.push(path.clone());
        }
        if s.is_wt_new() {
            status.untracked.push(path);
        }
    }
    
    // Count ahead/behind (simplified - just counts local commits)
    if let Ok(local_oid) = head.target().ok_or(()) {
        if let Ok(upstream) = repo.find_branch(&status.branch, git2::BranchType::Local)
            .and_then(|b| b.upstream()) {
            if let Some(upstream_oid) = upstream.get().target() {
                let (ahead, behind) = repo.graph_ahead_behind(local_oid, upstream_oid)?;
                status.ahead = ahead;
                status.behind = behind;
            }
        }
    }
    
    Ok(status)
}

/// Create a new branch from current HEAD
pub fn create_branch(repo_path: &Path, name: &str) -> Result<()> {
    let repo = Repository::open(repo_path)?;
    let head = repo.head()?;
    let commit = head.peel_to_commit()?;
    
    repo.branch(name, &commit, false)
        .context(format!("Failed to create branch '{}'", name))?;
    
    Ok(())
}

/// Checkout an existing branch
pub fn checkout_branch(repo_path: &Path, name: &str) -> Result<()> {
    let repo = Repository::open(repo_path)?;
    
    let (object, reference) = repo.revparse_ext(name)
        .context(format!("Branch '{}' not found", name))?;
    
    repo.checkout_tree(&object, None)?;
    
    match reference {
        Some(r) => repo.set_head(r.name().unwrap_or("HEAD"))?,
        None => repo.set_head_detached(object.id())?,
    }
    
    Ok(())
}

/// Create branch and checkout in one step
pub fn create_and_checkout_branch(repo_path: &Path, name: &str) -> Result<()> {
    create_branch(repo_path, name)?;
    checkout_branch(repo_path, name)?;
    Ok(())
}

/// Create a new branch from main (or master) and check it out
/// Used for creating fix branches before applying changes
pub fn create_fix_branch_from_main(repo_path: &Path, branch_name: &str) -> Result<String> {
    let repo = Repository::open(repo_path)?;
    
    // Try to find main or master branch
    let main_branch = repo.find_branch("main", git2::BranchType::Local)
        .or_else(|_| repo.find_branch("master", git2::BranchType::Local))
        .context("Could not find 'main' or 'master' branch")?;
    
    let main_commit = main_branch.get().peel_to_commit()
        .context("Failed to get commit from main branch")?;
    
    // Create the new branch from main
    repo.branch(branch_name, &main_commit, false)
        .context(format!("Failed to create branch '{}' from main", branch_name))?;
    
    // Checkout the new branch
    checkout_branch(repo_path, branch_name)?;
    
    Ok(branch_name.to_string())
}

/// Generate a branch name from a suggestion summary
pub fn generate_fix_branch_name(suggestion_id: &str, summary: &str) -> String {
    // Take first 8 chars of UUID
    let short_id = &suggestion_id[..8.min(suggestion_id.len())];
    
    // Slugify the summary: lowercase, replace spaces/special chars with dashes
    let slug: String = summary
        .to_lowercase()
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '-' })
        .collect::<String>()
        .split('-')
        .filter(|s| !s.is_empty())
        .take(5) // Limit to first 5 words
        .collect::<Vec<_>>()
        .join("-");
    
    // Truncate slug to reasonable length
    let slug = if slug.len() > 40 {
        slug[..40].trim_end_matches('-').to_string()
    } else {
        slug
    };
    
    format!("fix/{}-{}", short_id, slug)
}

/// Stage a specific file
pub fn stage_file(repo_path: &Path, file_path: &str) -> Result<()> {
    let repo = Repository::open(repo_path)?;
    let mut index = repo.index()?;
    
    index.add_path(Path::new(file_path))?;
    index.write()?;
    
    Ok(())
}

/// Stage all modified files
pub fn stage_all(repo_path: &Path) -> Result<()> {
    let repo = Repository::open(repo_path)?;
    let mut index = repo.index()?;
    
    index.add_all(["*"].iter(), IndexAddOption::DEFAULT, None)?;
    index.write()?;
    
    Ok(())
}

/// Commit staged changes
pub fn commit(repo_path: &Path, message: &str) -> Result<String> {
    let repo = Repository::open(repo_path)?;
    let mut index = repo.index()?;
    
    let tree_id = index.write_tree()?;
    let tree = repo.find_tree(tree_id)?;
    
    let head = repo.head()?;
    let parent = head.peel_to_commit()?;
    
    // Get author info from git config
    let config = repo.config()?;
    let name = config.get_string("user.name").unwrap_or_else(|_| "codecosmos".to_string());
    let email = config.get_string("user.email").unwrap_or_else(|_| "codecosmos@local".to_string());
    
    let sig = Signature::now(&name, &email)?;
    
    let oid = repo.commit(
        Some("HEAD"),
        &sig,
        &sig,
        message,
        &tree,
        &[&parent],
    )?;
    
    Ok(oid.to_string())
}

/// Push current branch to remote (shells out to git)
pub fn push_branch(repo_path: &Path, branch: &str) -> Result<String> {
    let output = Command::new("git")
        .current_dir(repo_path)
        .args(["push", "-u", "origin", branch])
        .output()
        .context("Failed to execute git push")?;
    
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    } else {
        Err(anyhow::anyhow!(
            "git push failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ))
    }
}

/// Reset a file to HEAD (discard changes)
pub fn reset_file(repo_path: &Path, file_path: &str) -> Result<()> {
    let output = Command::new("git")
        .current_dir(repo_path)
        .args(["checkout", "HEAD", "--", file_path])
        .output()
        .context("Failed to reset file")?;
    
    if output.status.success() {
        Ok(())
    } else {
        Err(anyhow::anyhow!(
            "git checkout failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ))
    }
}

// ============================================================================
// GitHub CLI (gh) Integration
// ============================================================================

/// Check if gh CLI is available
pub fn gh_available() -> bool {
    Command::new("gh")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Check if gh is authenticated
pub fn gh_authenticated() -> bool {
    Command::new("gh")
        .args(["auth", "status"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Create a pull request using gh CLI
pub fn create_pr(repo_path: &Path, title: &str, body: &str) -> Result<String> {
    if !gh_available() {
        return Err(anyhow::anyhow!("gh CLI not installed. Install from https://cli.github.com"));
    }
    
    if !gh_authenticated() {
        return Err(anyhow::anyhow!("gh CLI not authenticated. Run 'gh auth login' first"));
    }
    
    let output = Command::new("gh")
        .current_dir(repo_path)
        .args(["pr", "create", "--title", title, "--body", body])
        .output()
        .context("Failed to create PR")?;
    
    if output.status.success() {
        // gh pr create outputs the PR URL
        let url = String::from_utf8_lossy(&output.stdout).trim().to_string();
        Ok(url)
    } else {
        Err(anyhow::anyhow!(
            "Failed to create PR: {}",
            String::from_utf8_lossy(&output.stderr)
        ))
    }
}

/// Open a URL in the default browser
pub fn open_url(url: &str) -> Result<()> {
    #[cfg(target_os = "macos")]
    {
        Command::new("open")
            .arg(url)
            .spawn()
            .context("Failed to open URL")?;
    }
    
    #[cfg(target_os = "linux")]
    {
        Command::new("xdg-open")
            .arg(url)
            .spawn()
            .context("Failed to open URL")?;
    }
    
    #[cfg(target_os = "windows")]
    {
        Command::new("cmd")
            .args(["/C", "start", url])
            .spawn()
            .context("Failed to open URL")?;
    }
    
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::env;

    #[test]
    fn test_current_status() {
        // Test on the codecosmos repo itself
        let repo_path = env::current_dir().unwrap();
        let status = current_status(&repo_path);
        assert!(status.is_ok());
        assert!(!status.unwrap().branch.is_empty());
    }
}

