//! Git operations for the fix-and-ship workflow
//!
//! Provides branch, stage, commit, and push operations.

use crate::util::{run_command_with_timeout, CommandRunResult};
use anyhow::{Context, Result};
use git2::{Repository, Signature, StatusOptions};
use std::path::Path;
use std::process::Command;
use std::time::Duration;

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
    let is_detached = !head.is_branch();
    let branch = head.shorthand().unwrap_or("detached").to_string();

    let mut status = GitStatus {
        branch,
        ..Default::default()
    };

    let mut opts = StatusOptions::new();
    opts.include_untracked(true);
    opts.recurse_untracked_dirs(true);
    opts.include_ignored(false);
    opts.include_unmodified(false);
    opts.exclude_submodules(true);

    let statuses = repo.statuses(Some(&mut opts))?;

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
    if is_detached {
        return Ok(status);
    }

    if let Some(local_oid) = head.target() {
        if let Ok(upstream) = repo
            .find_branch(&status.branch, git2::BranchType::Local)
            .and_then(|b| b.upstream())
        {
            if let Some(upstream_oid) = upstream.get().target() {
                let (ahead, behind) = repo.graph_ahead_behind(local_oid, upstream_oid)?;
                status.ahead = ahead;
                status.behind = behind;
            }
        }
    }

    Ok(status)
}

/// Checkout an existing branch
pub fn checkout_branch(repo_path: &Path, name: &str) -> Result<()> {
    let repo = Repository::open(repo_path)?;

    let (object, reference) = repo
        .revparse_ext(name)
        .context(format!("Branch '{}' not found", name))?;

    repo.checkout_tree(&object, None)?;

    match reference {
        Some(r) => repo.set_head(r.name().unwrap_or("HEAD"))?,
        None => repo.set_head_detached(object.id())?,
    }

    Ok(())
}

/// Create a new branch from main (or master) and check it out
/// Used for creating fix branches before applying changes
pub fn create_fix_branch_from_main(repo_path: &Path, branch_name: &str) -> Result<String> {
    let repo = Repository::open(repo_path)?;

    // Find the default branch (main/master/trunk/etc)
    let main_branch_name = get_main_branch_name(repo_path)?;
    let main_branch = repo
        .find_branch(&main_branch_name, git2::BranchType::Local)
        .or_else(|_| repo.find_branch(&main_branch_name, git2::BranchType::Remote))
        .context(format!(
            "Could not find '{}' branch locally or on remote",
            main_branch_name
        ))?;

    let main_commit = main_branch
        .get()
        .peel_to_commit()
        .context("Failed to get commit from main branch")?;

    // Check if branch already exists (avoid deleting user work)
    let mut final_name = branch_name.to_string();
    if let Ok(existing) = repo.find_branch(branch_name, git2::BranchType::Local) {
        let existing_commit = existing
            .get()
            .peel_to_commit()
            .context("Failed to get commit from existing branch")?;
        if existing_commit.id() == main_commit.id() {
            // Branch already points at main; just reuse it.
            checkout_branch(repo_path, branch_name)?;
            return Ok(branch_name.to_string());
        }

        final_name = unique_branch_name(&repo, branch_name)?;
    }

    // Create the new branch from main
    repo.branch(&final_name, &main_commit, false)
        .context(format!(
            "Failed to create branch '{}' from main",
            final_name
        ))?;

    // Checkout the new branch
    checkout_branch(repo_path, &final_name)?;

    Ok(final_name)
}

fn unique_branch_name(repo: &Repository, base: &str) -> Result<String> {
    for suffix in 2..100 {
        let candidate = format!("{}-{}", base, suffix);
        if repo
            .find_branch(&candidate, git2::BranchType::Local)
            .is_err()
        {
            return Ok(candidate);
        }
    }
    Err(anyhow::anyhow!(
        "Failed to find available branch name for '{}'",
        base
    ))
}

/// Generate a branch name from a suggestion summary
pub fn generate_fix_branch_name(suggestion_id: &str, summary: &str) -> String {
    // Take first 8 chars of UUID
    let short_id = &suggestion_id[..8.min(suggestion_id.len())];
    let fallback = format!("fix/{}", short_id);

    let slug = sanitize_branch_slug(summary);
    if slug.is_empty() {
        return fallback;
    }

    let candidate = format!("fix/{}-{}", short_id, slug);
    if is_valid_git_ref(&candidate) {
        candidate
    } else {
        fallback
    }
}

fn sanitize_branch_slug(summary: &str) -> String {
    // Slugify the summary: lowercase, replace spaces/special chars with dashes
    let slug: String = summary
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect::<String>()
        .split('-')
        .filter(|s| !s.is_empty())
        .take(5) // Limit to first 5 words
        .collect::<Vec<_>>()
        .join("-");

    // Truncate slug to reasonable length
    let slug = if slug.chars().count() > 40 {
        slug.chars()
            .take(40)
            .collect::<String>()
            .trim_end_matches('-')
            .to_string()
    } else {
        slug
    };

    slug.trim_matches('-').to_string()
}

fn is_valid_git_ref(name: &str) -> bool {
    if name.is_empty() {
        return false;
    }
    if name.starts_with('.') || name.ends_with('.') || name.ends_with('/') {
        return false;
    }
    if name.ends_with(".lock") {
        return false;
    }
    if name.contains("..") || name.contains("@{") || name.contains("//") {
        return false;
    }
    for c in name.chars() {
        if c.is_control()
            || c == ' '
            || c == '~'
            || c == '^'
            || c == ':'
            || c == '?'
            || c == '*'
            || c == '['
            || c == '\\'
        {
            return false;
        }
    }
    true
}

/// Stage a specific file
pub fn stage_file(repo_path: &Path, file_path: &str) -> Result<()> {
    let repo = Repository::open(repo_path)?;
    let mut index = repo.index()?;

    index.add_path(Path::new(file_path))?;
    index.write()?;

    Ok(())
}

/// Commit staged changes
pub fn commit(repo_path: &Path, message: &str) -> Result<String> {
    let repo = Repository::open(repo_path)?;
    let mut index = repo.index()?;

    let tree_id = index.write_tree()?;
    let tree = repo.find_tree(tree_id)?;

    let parent = match repo.head() {
        Ok(head) => match head.peel_to_commit() {
            Ok(commit) => Some(commit),
            Err(err)
                if matches!(
                    err.code(),
                    git2::ErrorCode::UnbornBranch | git2::ErrorCode::NotFound
                ) =>
            {
                None
            }
            Err(err) => return Err(err.into()),
        },
        Err(err)
            if matches!(
                err.code(),
                git2::ErrorCode::UnbornBranch | git2::ErrorCode::NotFound
            ) =>
        {
            None
        }
        Err(err) => return Err(err.into()),
    };

    // Get author info from git config
    let config = repo.config()?;
    let name = config
        .get_string("user.name")
        .unwrap_or_else(|_| "cosmos".to_string());
    let email = config
        .get_string("user.email")
        .unwrap_or_else(|_| "cosmos@local".to_string());

    let sig = Signature::now(&name, &email)?;

    let oid = match parent {
        Some(ref parent) => repo.commit(Some("HEAD"), &sig, &sig, message, &tree, &[parent])?,
        None => repo.commit(Some("HEAD"), &sig, &sig, message, &tree, &[])?,
    };

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

    if output.timed_out {
        return Err(anyhow::anyhow!(
            "git push timed out after {}s (remote: {}, branch: {})",
            GIT_PUSH_TIMEOUT_SECS,
            remote,
            branch
        ));
    }

    if output.status.map(|s| s.success()).unwrap_or(false) {
        return Ok(output.stdout);
    }

    let stderr = output.stderr;
    // Only retry with -u if the first attempt didn't include it.
    // If needs_upstream was already true, retrying with -u is pointless.
    if !needs_upstream
        && (stderr.contains("no upstream")
            || stderr.contains("set-upstream")
            || stderr.contains("set upstream"))
    {
        let retry = run_git_push(repo_path, &remote, branch, true)
            .context("Failed to retry git push with upstream")?;
        if retry.timed_out {
            return Err(anyhow::anyhow!(
                "git push timed out after {}s (remote: {}, branch: {})",
                GIT_PUSH_TIMEOUT_SECS,
                remote,
                branch
            ));
        }
        if retry.status.map(|s| s.success()).unwrap_or(false) {
            return Ok(retry.stdout);
        }
        let retry_err = retry.stderr;
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

const GIT_PUSH_TIMEOUT_SECS: u64 = 180;

fn run_git_push(
    repo_path: &Path,
    remote: &str,
    branch: &str,
    set_upstream: bool,
) -> Result<CommandRunResult> {
    let mut args = vec!["push".to_string()];
    if set_upstream {
        args.push("-u".to_string());
    }
    args.push(remote.to_string());
    args.push(branch.to_string());

    let mut cmd = Command::new("git");
    cmd.current_dir(repo_path)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0");

    run_command_with_timeout(&mut cmd, Duration::from_secs(GIT_PUSH_TIMEOUT_SECS))
        .map_err(|e| anyhow::anyhow!("Failed to run git push command: {}", e))
}

fn has_upstream(repo: &Repository, branch: &str) -> bool {
    repo.find_branch(branch, git2::BranchType::Local)
        .and_then(|b| b.upstream())
        .is_ok()
}

fn ensure_local_branch(repo: &Repository, branch: &str) -> Result<()> {
    if repo.find_branch(branch, git2::BranchType::Local).is_ok() {
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

// ============================================================================
// Clean Main State Operations
// ============================================================================

/// Get the name of the main branch (main or master)
pub fn get_main_branch_name(repo_path: &Path) -> Result<String> {
    let repo = Repository::open(repo_path)?;

    if let Some(remote_default) = resolve_remote_head_branch(&repo) {
        if repo
            .find_branch(&remote_default, git2::BranchType::Local)
            .is_ok()
        {
            return Ok(remote_default);
        }
    }

    if let Ok(config) = repo.config() {
        if let Ok(name) = config.get_string("init.defaultBranch") {
            let name = name.trim();
            if !name.is_empty() && repo.find_branch(name, git2::BranchType::Local).is_ok() {
                return Ok(name.to_string());
            }
        }
    }

    if repo.find_branch("main", git2::BranchType::Local).is_ok() {
        Ok("main".to_string())
    } else if repo.find_branch("master", git2::BranchType::Local).is_ok() {
        Ok("master".to_string())
    } else {
        let head = repo.head().context("Failed to get HEAD")?;
        let branch = head.shorthand().unwrap_or("HEAD").to_string();
        Ok(branch)
    }
}

fn resolve_remote_head_branch(repo: &Repository) -> Option<String> {
    let remotes = repo.remotes().ok()?;
    for name in remotes.iter().flatten() {
        let head_ref = format!("refs/remotes/{}/HEAD", name);
        if let Ok(reference) = repo.find_reference(&head_ref) {
            if let Some(target) = reference.symbolic_target() {
                let prefix = format!("refs/remotes/{}/", name);
                if let Some(branch) = target.strip_prefix(&prefix) {
                    return Some(branch.to_string());
                }
            }
        }
    }
    None
}

// ============================================================================
// GitHub Integration (via native API)
// ============================================================================

/// Create a pull request using the GitHub API.
///
/// Returns the URL of the created PR.
pub async fn create_pr(repo_path: &Path, title: &str, body: &str) -> Result<String> {
    let (owner, repo) = crate::github::get_remote_info(repo_path)?;
    let base = get_main_branch_name(repo_path)?;
    let head = get_current_branch(repo_path)?;

    crate::github::create_pull_request(&owner, &repo, &base, &head, title, body).await
}

/// Get the current branch name.
fn get_current_branch(repo_path: &Path) -> Result<String> {
    let repo = Repository::open(repo_path)?;
    let head = repo.head().context("Failed to get HEAD")?;
    let branch = head
        .shorthand()
        .ok_or_else(|| anyhow::anyhow!("HEAD is not a branch"))?;
    Ok(branch.to_string())
}

/// Read file content from HEAD (without modifying the working directory).
/// Returns None if the file doesn't exist in HEAD (new file).
pub fn read_file_from_head(repo_path: &Path, file_path: &Path) -> Result<Option<String>> {
    let repo = Repository::open(repo_path)?;

    // Get HEAD commit
    let head = repo.head()?;
    let commit = head.peel_to_commit()?;
    let tree = commit.tree()?;

    // Try to find the file in HEAD
    match tree.get_path(file_path) {
        Ok(entry) => {
            let blob = repo.find_blob(entry.id())?;
            let content = blob.content();
            // Convert to string (assuming UTF-8)
            let text = String::from_utf8_lossy(content).to_string();
            Ok(Some(text))
        }
        Err(_) => {
            // File doesn't exist in HEAD - it's a new file
            Ok(None)
        }
    }
}

/// Restore a file to its state at HEAD (undo uncommitted changes)
/// For new files that don't exist in HEAD, this will remove the file.
pub fn restore_file(repo_path: &Path, file_path: &Path) -> Result<()> {
    let repo = Repository::open(repo_path)?;

    // Get HEAD commit
    let head = repo.head()?;
    let commit = head.peel_to_commit()?;
    let tree = commit.tree()?;

    // Try to find the file in HEAD
    match tree.get_path(file_path) {
        Ok(entry) => {
            // File exists in HEAD - restore it
            let blob = repo.find_blob(entry.id())?;
            let content = blob.content();
            let full_path = repo_path.join(file_path);
            std::fs::write(&full_path, content)
                .with_context(|| format!("Failed to restore {}", file_path.display()))?;

            // Unstage the file (reset index entry to HEAD)
            let mut index = repo.index()?;
            index.add_path(file_path)?;
            index.write()?;
        }
        Err(_) => {
            // File doesn't exist in HEAD - it's a new file, remove it
            let full_path = repo_path.join(file_path);
            if full_path.exists() {
                std::fs::remove_file(&full_path).with_context(|| {
                    format!("Failed to remove new file {}", file_path.display())
                })?;
            }
            // Remove from index if staged
            let mut index = repo.index()?;
            let _ = index.remove_path(file_path);
            index.write()?;
        }
    }

    Ok(())
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

    // ========================================================================
    // Git Status Tests
    // ========================================================================

    #[test]
    fn test_current_status() {
        // Test on the cosmos repo itself
        let repo_path = env::current_dir().unwrap();
        let status = current_status(&repo_path);
        assert!(status.is_ok());
        assert!(!status.unwrap().branch.is_empty());
    }

    #[test]
    fn test_current_status_returns_git_status_struct() {
        let repo_path = env::current_dir().unwrap();
        let status = current_status(&repo_path).unwrap();

        // Verify struct fields are accessible
        let _branch: &str = &status.branch;
        let _staged: &Vec<String> = &status.staged;
        let _modified: &Vec<String> = &status.modified;
        let _untracked: &Vec<String> = &status.untracked;
        let _ahead: usize = status.ahead;
        let _behind: usize = status.behind;
    }

    // ========================================================================
    // Branch Name Generation Tests
    // ========================================================================

    #[test]
    fn test_branch_name_sanitization() {
        let name = generate_fix_branch_name("12345678", "Fix: user/login (v2)!!!");
        assert!(name.starts_with("fix/12345678-"));
        assert!(is_valid_git_ref(&name));
    }

    #[test]
    fn test_branch_name_fallback_on_empty_slug() {
        let name = generate_fix_branch_name("12345678", "!!!");
        assert_eq!(name, "fix/12345678");
    }

    #[test]
    fn test_branch_name_with_long_summary() {
        let long_summary = "This is a very long summary that should be truncated to a reasonable length for the branch name";
        let name = generate_fix_branch_name("abcd1234", long_summary);
        assert!(name.starts_with("fix/abcd1234-"));
        assert!(is_valid_git_ref(&name));
        // Should be truncated
        assert!(name.len() < 60);
    }

    #[test]
    fn test_branch_name_unicode_handling() {
        // Unicode should be converted to dashes
        let name = generate_fix_branch_name("12345678", "Fix émoji 🚀 issue");
        assert!(is_valid_git_ref(&name));
        // Should not contain emoji or accented chars
        assert!(!name.contains("é"));
        assert!(!name.contains("🚀"));
    }

    #[test]
    fn test_branch_name_consecutive_special_chars() {
        let name = generate_fix_branch_name("12345678", "Fix---multiple///slashes");
        assert!(is_valid_git_ref(&name));
        // Should not have consecutive dashes (they get collapsed)
        assert!(!name.contains("--"));
    }

    // ========================================================================
    // Git Ref Validation Tests
    // ========================================================================

    #[test]
    fn test_invalid_git_ref_rejected() {
        assert!(!is_valid_git_ref("bad..name"));
        assert!(!is_valid_git_ref("bad@{name"));
        assert!(!is_valid_git_ref("bad name"));
        assert!(!is_valid_git_ref("bad:ref"));
        assert!(!is_valid_git_ref("bad.lock"));
    }

    #[test]
    fn test_valid_git_refs_accepted() {
        assert!(is_valid_git_ref("main"));
        assert!(is_valid_git_ref("feature/new-thing"));
        assert!(is_valid_git_ref("fix/issue-123"));
        assert!(is_valid_git_ref("release-v1.2.3"));
    }

    #[test]
    fn test_git_ref_edge_cases() {
        // Empty string
        assert!(!is_valid_git_ref(""));

        // Starting/ending with dot
        assert!(!is_valid_git_ref(".hidden"));
        assert!(!is_valid_git_ref("name."));

        // Ending with slash
        assert!(!is_valid_git_ref("path/"));

        // Control characters (tab)
        assert!(!is_valid_git_ref("name\twith\ttabs"));
    }

    // ========================================================================
    // Main Branch Detection Tests
    // ========================================================================

    #[test]
    fn test_get_main_branch_name() {
        let repo_path = env::current_dir().unwrap();
        let result = get_main_branch_name(&repo_path);
        assert!(result.is_ok());

        let branch = result.unwrap();
        // Should be one of the common default branch names
        assert!(
            branch == "main" || branch == "master" || branch == "trunk" || branch == "develop",
            "Unexpected default branch: {}",
            branch
        );
    }

    // ========================================================================
    // Current Branch Tests
    // ========================================================================

    #[test]
    fn test_get_current_branch() {
        let repo_path = env::current_dir().unwrap();
        let result = get_current_branch(&repo_path);
        assert!(result.is_ok());
        assert!(!result.unwrap().is_empty());
    }

    #[test]
    fn test_get_current_branch_is_valid_ref() {
        let repo_path = env::current_dir().unwrap();
        let branch = get_current_branch(&repo_path).unwrap();
        assert!(is_valid_git_ref(&branch));
    }

    // ========================================================================
    // PR Creation Tests (signature validation)
    // ========================================================================

    #[tokio::test]
    async fn test_create_pr_requires_github_auth() {
        // Save original env state
        let orig = std::env::var("GITHUB_TOKEN").ok();

        // Remove GitHub token to ensure not authenticated
        std::env::remove_var("GITHUB_TOKEN");

        let repo_path = env::current_dir().unwrap();
        let result = create_pr(&repo_path, "Test PR", "Test body").await;

        // Should fail because not authenticated
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("authenticated") || err.contains("token") || err.contains("GitHub"),
            "Error should mention authentication: {}",
            err
        );

        // Restore original state
        match orig {
            Some(val) => std::env::set_var("GITHUB_TOKEN", val),
            None => {}
        }
    }

    #[tokio::test]
    async fn test_create_pr_is_async() {
        // This test just verifies create_pr is an async function
        // by using it in an async context
        let repo_path = env::current_dir().unwrap();

        // We don't actually want to create a PR, just verify it compiles as async
        let future = create_pr(&repo_path, "title", "body");

        // Verify it's a future (can be awaited)
        // We'll cancel it immediately by dropping
        drop(future);
    }

    // ========================================================================
    // File Operations Tests
    // ========================================================================

    #[test]
    fn test_read_file_from_head_returns_option() {
        let repo_path = env::current_dir().unwrap();

        // Try reading a file that exists
        let result = read_file_from_head(&repo_path, Path::new("Cargo.toml"));
        assert!(result.is_ok());

        if let Ok(Some(content)) = result {
            // Cargo.toml should contain the package name
            assert!(content.contains("cosmos"));
        }
    }

    #[test]
    fn test_read_file_from_head_returns_none_for_new_file() {
        let repo_path = env::current_dir().unwrap();

        // Try reading a file that definitely doesn't exist
        let result = read_file_from_head(&repo_path, Path::new("definitely-not-a-real-file.xyz"));
        assert!(result.is_ok());
        assert!(result.unwrap().is_none());
    }
}
