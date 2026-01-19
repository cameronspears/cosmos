//! Git operations for the fix-and-ship workflow
//!
//! Provides branch, stage, commit, and push operations.

use anyhow::{Context, Result};
use git2::{IndexAddOption, Repository, Signature};
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
    
    // Check if branch already exists (from a previous failed attempt)
    if let Ok(mut existing) = repo.find_branch(branch_name, git2::BranchType::Local) {
        // We need to checkout main first - can't delete the currently checked out branch
        let main_ref = main_branch.get().name()
            .context("Failed to get main branch ref name")?;
        let main_object = main_branch.get().peel(git2::ObjectType::Any)
            .context("Failed to peel main branch")?;
        repo.checkout_tree(&main_object, None)
            .context("Failed to checkout main branch tree")?;
        repo.set_head(main_ref)
            .context("Failed to set HEAD to main branch")?;
        
        // Now we can safely delete the existing branch
        existing.delete()
            .context(format!("Failed to delete existing branch '{}'", branch_name))?;
    }
    
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
    
    // Truncate slug to reasonable length (character-safe for Unicode)
    let slug = if slug.chars().count() > 40 {
        slug.chars().take(40).collect::<String>().trim_end_matches('-').to_string()
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
    let repo = Repository::open(repo_path)?;
    ensure_local_branch(&repo, branch)?;
    let remote = resolve_push_remote(&repo, branch).unwrap_or_else(|_| "origin".to_string());
    let needs_upstream = !has_upstream(&repo, branch);

    let output = run_git_push(repo_path, &remote, branch, needs_upstream)
        .context("Failed to execute git push")?;

    if output.status.success() {
        return Ok(String::from_utf8_lossy(&output.stdout).to_string());
    }

    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    // Only retry with -u if the first attempt didn't include it.
    // If needs_upstream was already true, retrying with -u is pointless.
    if !needs_upstream
        && (stderr.contains("no upstream")
            || stderr.contains("set-upstream")
            || stderr.contains("set upstream"))
    {
        let retry = run_git_push(repo_path, &remote, branch, true)
            .context("Failed to retry git push with upstream")?;
        if retry.status.success() {
            return Ok(String::from_utf8_lossy(&retry.stdout).to_string());
        }
        let retry_err = String::from_utf8_lossy(&retry.stderr);
        return Err(anyhow::anyhow!(
            "git push failed after retrying with upstream (remote: {}, branch: {}): {}",
            remote,
            branch,
            retry_err
        ));
    }

    Err(anyhow::anyhow!(
        "git push failed (remote: {}, branch: {}): {}",
        remote,
        branch,
        stderr
    ))
}

fn run_git_push(
    repo_path: &Path,
    remote: &str,
    branch: &str,
    set_upstream: bool,
) -> Result<std::process::Output> {
    let mut args = vec!["push".to_string()];
    if set_upstream {
        args.push("-u".to_string());
    }
    args.push(remote.to_string());
    args.push(branch.to_string());

    Command::new("git")
        .current_dir(repo_path)
        .args(args)
        .output()
        .context("Failed to run git push command")
}

fn has_upstream(repo: &Repository, branch: &str) -> bool {
    repo.find_branch(branch, git2::BranchType::Local)
        .and_then(|b| b.upstream())
        .is_ok()
}

fn ensure_local_branch(repo: &Repository, branch: &str) -> Result<()> {
    if repo
        .find_branch(branch, git2::BranchType::Local)
        .is_ok()
    {
        return Ok(());
    }

    let head = repo.head().context("Failed to read HEAD")?;
    let commit = head
        .peel_to_commit()
        .context("Failed to resolve HEAD commit")?;
    repo.branch(branch, &commit, false)
        .context(format!("Failed to create local branch '{}'", branch))?;
    Ok(())
}

fn resolve_push_remote(repo: &Repository, branch: &str) -> Result<String> {
    let config = repo.config()?;
    if let Ok(remote) = config.get_string(&format!("branch.{}.remote", branch)) {
        if !remote.trim().is_empty() {
            return Ok(remote);
        }
    }
    if let Ok(remote) = config.get_string("remote.pushDefault") {
        if !remote.trim().is_empty() {
            return Ok(remote);
        }
    }

    let remotes = repo.remotes()?;
    if remotes.len() == 1 {
        if let Some(name) = remotes.get(0) {
            return Ok(name.to_string());
        }
    }

    Ok("origin".to_string())
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
// Clean Main State Operations
// ============================================================================

/// Get the name of the main branch (main or master)
pub fn get_main_branch_name(repo_path: &Path) -> Result<String> {
    let repo = Repository::open(repo_path)?;
    
    if repo.find_branch("main", git2::BranchType::Local).is_ok() {
        Ok("main".to_string())
    } else if repo.find_branch("master", git2::BranchType::Local).is_ok() {
        Ok("master".to_string())
    } else {
        Err(anyhow::anyhow!("Could not find 'main' or 'master' branch"))
    }
}

/// Stash all changes (staged + unstaged) with an optional message
/// Uses git CLI for reliable stash behavior
pub fn stash_changes(repo_path: &Path, message: Option<&str>) -> Result<()> {
    let mut args = vec!["stash", "push", "--include-untracked"];
    
    if let Some(msg) = message {
        args.push("-m");
        args.push(msg);
    }
    
    let output = Command::new("git")
        .current_dir(repo_path)
        .args(&args)
        .output()
        .context("Failed to run git stash")?;
    
    if output.status.success() {
        Ok(())
    } else {
        Err(anyhow::anyhow!(
            "git stash failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ))
    }
}

/// Hard reset the working directory and switch to main branch
/// WARNING: This discards all uncommitted changes permanently
pub fn reset_to_main(repo_path: &Path) -> Result<()> {
    let main_branch = get_main_branch_name(repo_path)?;
    
    // First, reset any staged changes
    let reset_output = Command::new("git")
        .current_dir(repo_path)
        .args(["reset", "--hard", "HEAD"])
        .output()
        .context("Failed to reset working directory")?;
    
    if !reset_output.status.success() {
        return Err(anyhow::anyhow!(
            "git reset failed: {}",
            String::from_utf8_lossy(&reset_output.stderr)
        ));
    }
    
    // Clean untracked files
    let clean_output = Command::new("git")
        .current_dir(repo_path)
        .args(["clean", "-fd"])
        .output()
        .context("Failed to clean untracked files")?;
    
    if !clean_output.status.success() {
        return Err(anyhow::anyhow!(
            "git clean failed: {}",
            String::from_utf8_lossy(&clean_output.stderr)
        ));
    }
    
    // Checkout main branch
    let checkout_output = Command::new("git")
        .current_dir(repo_path)
        .args(["checkout", &main_branch])
        .output()
        .context("Failed to checkout main branch")?;
    
    if checkout_output.status.success() {
        Ok(())
    } else {
        Err(anyhow::anyhow!(
            "git checkout {} failed: {}",
            main_branch,
            String::from_utf8_lossy(&checkout_output.stderr)
        ))
    }
}

/// Stash changes and switch to main branch
pub fn stash_and_switch_to_main(repo_path: &Path) -> Result<()> {
    let main_branch = get_main_branch_name(repo_path)?;
    
    // Stash with a descriptive message
    stash_changes(repo_path, Some("cosmos: auto-stash before switching to main"))?;
    
    // Checkout main branch
    checkout_branch(repo_path, &main_branch)?;
    
    Ok(())
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

