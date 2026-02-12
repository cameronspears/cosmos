//! Git operations for the fix-and-ship workflow
//!
//! Provides branch, stage, commit, and push operations.

use crate::util::{resolve_repo_path_allow_new, run_command_with_timeout, CommandRunResult};
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BranchCreateOutcome {
    pub branch_name: String,
    pub created_new: bool,
}

fn open_repo_discover(repo_path: &Path) -> Result<Repository> {
    Repository::discover(repo_path).with_context(|| {
        format!(
            "Failed to open repository from path '{}'",
            repo_path.display()
        )
    })
}

/// Get the current git status
pub fn current_status(repo_path: &Path) -> Result<GitStatus> {
    let repo = open_repo_discover(repo_path)?;

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
    let repo = open_repo_discover(repo_path)?;

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
#[allow(dead_code)]
pub fn create_fix_branch_from_main(repo_path: &Path, branch_name: &str) -> Result<String> {
    let repo = open_repo_discover(repo_path)?;

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

    create_fix_branch_from_base_commit(
        repo_path,
        &repo,
        branch_name,
        &main_commit,
        &format!("main branch '{}'", main_branch_name),
    )
    .map(|outcome| outcome.branch_name)
}

/// Create a new branch from the current checkout/HEAD and check it out.
/// Used when the user chooses to continue from their current branch context.
pub fn create_fix_branch_from_current(repo_path: &Path, branch_name: &str) -> Result<String> {
    create_fix_branch_from_current_with_outcome(repo_path, branch_name)
        .map(|outcome| outcome.branch_name)
}

pub fn create_fix_branch_from_current_with_outcome(
    repo_path: &Path,
    branch_name: &str,
) -> Result<BranchCreateOutcome> {
    let repo = open_repo_discover(repo_path)?;
    let head = repo
        .head()
        .context("Failed to get HEAD for current branch")?;
    let head_label = head.shorthand().unwrap_or("detached HEAD").to_string();
    let head_commit = head
        .peel_to_commit()
        .context("Failed to get commit from current checkout")?;

    create_fix_branch_from_base_commit(
        repo_path,
        &repo,
        branch_name,
        &head_commit,
        &format!("current checkout '{}'", head_label),
    )
}

fn create_fix_branch_from_base_commit(
    repo_path: &Path,
    repo: &Repository,
    branch_name: &str,
    base_commit: &git2::Commit<'_>,
    base_label: &str,
) -> Result<BranchCreateOutcome> {
    // Check if branch already exists (avoid deleting user work)
    let mut final_name = branch_name.to_string();
    if let Ok(existing) = repo.find_branch(branch_name, git2::BranchType::Local) {
        let existing_commit = existing
            .get()
            .peel_to_commit()
            .context("Failed to get commit from existing branch")?;
        if existing_commit.id() == base_commit.id() {
            // Branch already points at desired base; just reuse it.
            checkout_branch(repo_path, branch_name)?;
            return Ok(BranchCreateOutcome {
                branch_name: branch_name.to_string(),
                created_new: false,
            });
        }

        final_name = unique_branch_name(repo, branch_name)?;
    }

    // Create the new branch from selected base commit.
    repo.branch(&final_name, base_commit, false)
        .context(format!(
            "Failed to create branch '{}' from {}",
            final_name, base_label
        ))?;

    // Checkout the new branch
    if let Err(error) = checkout_branch(repo_path, &final_name) {
        // Best-effort cleanup so branch creation is transactional.
        let cleanup_failed = repo
            .find_branch(&final_name, git2::BranchType::Local)
            .and_then(|mut b| b.delete())
            .is_err();
        if cleanup_failed {
            return Err(anyhow::anyhow!(
                "Failed to checkout newly created branch '{}' ({}). Cleanup also failed; you may need to delete the branch manually.",
                final_name,
                error
            ));
        }
        return Err(anyhow::anyhow!(
            "Failed to checkout newly created branch '{}': {}",
            final_name,
            error
        ));
    }

    Ok(BranchCreateOutcome {
        branch_name: final_name,
        created_new: true,
    })
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

/// Delete a local branch with safety checks.
pub fn delete_local_branch_safe(repo_path: &Path, branch_name: &str) -> Result<()> {
    let repo = open_repo_discover(repo_path)?;
    let head = repo.head().context("Failed to get HEAD")?;
    let current = head.shorthand().unwrap_or_default();
    if current == branch_name {
        return Err(anyhow::anyhow!(
            "Refusing to delete currently checked out branch '{}'",
            branch_name
        ));
    }

    let mut branch = repo
        .find_branch(branch_name, git2::BranchType::Local)
        .context(format!("Local branch '{}' not found", branch_name))?;

    if branch.upstream().is_ok() {
        return Err(anyhow::anyhow!(
            "Refusing to delete branch '{}' with upstream tracking",
            branch_name
        ));
    }

    branch
        .delete()
        .context(format!("Failed to delete local branch '{}'", branch_name))?;
    Ok(())
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

/// Maximum length for git ref names (prevent buffer overflow attacks)
const MAX_GIT_REF_LENGTH: usize = 255;

fn is_valid_git_ref(name: &str) -> bool {
    // Empty check
    if name.is_empty() {
        return false;
    }

    // Length limit to prevent abuse
    if name.len() > MAX_GIT_REF_LENGTH {
        return false;
    }

    // Reject refs starting with hyphen (could be interpreted as git flags)
    if name.starts_with('-') {
        return false;
    }

    // Reject common path component attacks
    if name.starts_with('.') || name.ends_with('.') || name.ends_with('/') {
        return false;
    }

    // Reject .lock suffix (git uses this internally)
    if name.ends_with(".lock") {
        return false;
    }

    // Reject path traversal and special git sequences
    if name.contains("..") || name.contains("@{") || name.contains("//") {
        return false;
    }

    // Reject shell/git dangerous characters
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
            || c == '\''
            || c == '"'
            || c == '`'
            || c == '$'
            || c == '!'
            || c == '&'
            || c == ';'
            || c == '|'
            || c == '<'
            || c == '>'
        {
            return false;
        }
    }
    true
}

/// Stage a specific file
pub fn stage_file(repo_path: &Path, file_path: &str) -> Result<()> {
    let repo = open_repo_discover(repo_path)?;
    let mut index = repo.index()?;

    index.add_path(Path::new(file_path))?;
    index.write()?;

    Ok(())
}

/// Commit staged changes
pub fn commit(repo_path: &Path, message: &str) -> Result<String> {
    let repo = open_repo_discover(repo_path)?;
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
    if push_disabled_by_env() {
        return Err(anyhow::anyhow!(
            "Push blocked: sandbox mode is active (COSMOS_DISABLE_PUSH=1). \
             Disable sandbox mode before pushing."
        ));
    }

    let repo = open_repo_discover(repo_path)?;
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

fn push_disabled_by_env() -> bool {
    std::env::var("COSMOS_DISABLE_PUSH")
        .ok()
        .map(|value| {
            matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
        .unwrap_or(false)
}

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
    let repo = open_repo_discover(repo_path)?;

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
    if !crate::github::is_authenticated() {
        return Err(anyhow::anyhow!(
            "Not authenticated with GitHub. Please authenticate first."
        ));
    }

    let (owner, repo) = crate::github::get_remote_info(repo_path)?;
    let base = get_main_branch_name(repo_path)?;
    let head = get_current_branch(repo_path)?;

    crate::github::create_pull_request(&owner, &repo, &base, &head, title, body).await
}

/// Get the current branch name.
fn get_current_branch(repo_path: &Path) -> Result<String> {
    let repo = open_repo_discover(repo_path)?;
    let head = repo.head().context("Failed to get HEAD")?;
    let branch = head
        .shorthand()
        .ok_or_else(|| anyhow::anyhow!("HEAD is not a branch"))?;
    Ok(branch.to_string())
}

/// Read file content from HEAD (without modifying the working directory).
/// Returns None if the file doesn't exist in HEAD (new file).
pub fn read_file_from_head(repo_path: &Path, file_path: &Path) -> Result<Option<String>> {
    let repo = open_repo_discover(repo_path)?;

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
    // Validate path to prevent traversal attacks
    let resolved = resolve_repo_path_allow_new(repo_path, file_path)
        .map_err(|e| anyhow::anyhow!("Invalid path '{}': {}", file_path.display(), e))?;

    let repo = open_repo_discover(repo_path)?;

    // Get HEAD commit
    let head = repo.head()?;
    let commit = head.peel_to_commit()?;
    let tree = commit.tree()?;

    // Try to find the file in HEAD (use relative path for git operations)
    match tree.get_path(&resolved.relative) {
        Ok(entry) => {
            // File exists in HEAD - restore it
            let blob = repo.find_blob(entry.id())?;
            let content = blob.content();
            std::fs::write(&resolved.absolute, content)
                .with_context(|| format!("Failed to restore {}", file_path.display()))?;

            // Unstage the file (reset index entry to HEAD)
            let mut index = repo.index()?;
            index.add_path(&resolved.relative)?;
            index.write()?;
        }
        Err(_) => {
            // File doesn't exist in HEAD - it's a new file, remove it
            if resolved.absolute.exists() {
                std::fs::remove_file(&resolved.absolute).with_context(|| {
                    format!("Failed to remove new file {}", file_path.display())
                })?;
            }
            // Remove from index if staged
            let mut index = repo.index()?;
            let _ = index.remove_path(&resolved.relative);
            index.write()?;
        }
    }

    Ok(())
}

/// Stash uncommitted changes with a descriptive message
/// Returns the stash message used (for display purposes)
pub fn stash_changes(repo_path: &Path) -> Result<String> {
    let timestamp = chrono::Local::now().format("%Y-%m-%d %H:%M");
    let message = format!("cosmos: saved work ({})", timestamp);

    let mut cmd = Command::new("git");
    cmd.current_dir(repo_path)
        .args(["stash", "push", "-m", &message, "--include-untracked"]);

    let output = run_command_with_timeout(&mut cmd, Duration::from_secs(30))
        .map_err(|e| anyhow::anyhow!("Failed to execute git stash: {}", e))?;

    if output.timed_out {
        return Err(anyhow::anyhow!("git stash timed out after 30s"));
    }

    if output
        .status
        .map(|s: std::process::ExitStatus| s.success())
        .unwrap_or(false)
    {
        Ok(message)
    } else {
        // Check if "No local changes to save" which is actually fine
        if output.stdout.contains("No local changes") || output.stderr.contains("No local changes")
        {
            Ok("No changes to stash".to_string())
        } else {
            Err(anyhow::anyhow!("git stash failed: {}", output.stderr))
        }
    }
}

/// Discard all uncommitted changes (both staged and unstaged)
/// This resets the working directory to HEAD
pub fn discard_all_changes(repo_path: &Path) -> Result<()> {
    // First, reset staged changes
    let mut reset_cmd = Command::new("git");
    reset_cmd.current_dir(repo_path).args(["reset", "HEAD"]);
    let reset_output = run_command_with_timeout(&mut reset_cmd, Duration::from_secs(30))
        .map_err(|e| anyhow::anyhow!("Failed to execute git reset: {}", e))?;

    if reset_output.timed_out {
        return Err(anyhow::anyhow!("git reset timed out"));
    }

    // Then, checkout all tracked files to discard modifications
    // Use "git checkout HEAD -- ." which handles empty repos better
    let mut checkout_cmd = Command::new("git");
    checkout_cmd
        .current_dir(repo_path)
        .args(["checkout", "HEAD", "--", "."]);
    let checkout_output = run_command_with_timeout(&mut checkout_cmd, Duration::from_secs(30))
        .map_err(|e| anyhow::anyhow!("Failed to execute git checkout: {}", e))?;

    if checkout_output.timed_out {
        return Err(anyhow::anyhow!("git checkout timed out"));
    }

    // Checkout can fail with "did not match any file(s)" if there are no tracked files
    // This is not an error - it just means there's nothing to checkout
    let checkout_ok = checkout_output
        .status
        .map(|s: std::process::ExitStatus| s.success())
        .unwrap_or(false);
    let checkout_no_files = checkout_output.stderr.contains("did not match any file");

    if !checkout_ok && !checkout_no_files {
        return Err(anyhow::anyhow!(
            "git checkout failed: {}",
            checkout_output.stderr
        ));
    }

    // Finally, clean untracked files
    let mut clean_cmd = Command::new("git");
    clean_cmd.current_dir(repo_path).args(["clean", "-fd"]); // -f force, -d directories
    let clean_output = run_command_with_timeout(&mut clean_cmd, Duration::from_secs(30))
        .map_err(|e| anyhow::anyhow!("Failed to execute git clean: {}", e))?;

    if clean_output.timed_out {
        return Err(anyhow::anyhow!("git clean timed out"));
    }

    if !clean_output
        .status
        .map(|s: std::process::ExitStatus| s.success())
        .unwrap_or(false)
    {
        return Err(anyhow::anyhow!("git clean failed: {}", clean_output.stderr));
    }

    Ok(())
}

/// Allowed URL schemes for security
const ALLOWED_URL_SCHEMES: &[&str] = &["https://", "http://"];

/// Open a URL in the default browser
/// Only allows http:// and https:// URLs for security
pub fn open_url(url: &str) -> Result<()> {
    // Validate URL scheme to prevent injection attacks
    let url_lower = url.to_lowercase();
    let is_safe = ALLOWED_URL_SCHEMES
        .iter()
        .any(|scheme| url_lower.starts_with(scheme));

    if !is_safe {
        return Err(anyhow::anyhow!(
            "URL scheme not allowed. Only http:// and https:// URLs are permitted."
        ));
    }

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
    use std::sync::Mutex;

    static PUSH_ENV_LOCK: Mutex<()> = Mutex::new(());

    // ========================================================================
    // Git Status Tests
    // ========================================================================

    #[test]
    fn test_current_status() {
        let (_temp_dir, repo_path) = create_temp_repo();
        let status = current_status(&repo_path);
        assert!(status.is_ok());
        assert!(!status.unwrap().branch.is_empty());
    }

    #[test]
    fn test_current_status_returns_git_status_struct() {
        let (_temp_dir, repo_path) = create_temp_repo();
        let status = current_status(&repo_path).unwrap();

        // Verify struct fields are accessible
        let _branch: &str = &status.branch;
        let _staged: &Vec<String> = &status.staged;
        let _modified: &Vec<String> = &status.modified;
        let _untracked: &Vec<String> = &status.untracked;
        let _ahead: usize = status.ahead;
        let _behind: usize = status.behind;
    }

    #[test]
    fn test_current_status_from_subdirectory_path() {
        let (_temp_dir, repo_path) = create_temp_repo();
        let nested = repo_path.join("src").join("nested");
        std::fs::create_dir_all(&nested).unwrap();

        let status = current_status(&nested).unwrap();
        assert!(!status.branch.is_empty());
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
        let (_temp_dir, repo_path) = create_temp_repo();
        let result = get_main_branch_name(&repo_path);
        assert!(result.is_ok());

        let branch = result.unwrap();
        // Should return a non-empty branch name. In CI environments with shallow
        // clones or feature branches, main/master may not exist locally, so the
        // function may fall back to HEAD or the current branch name.
        assert!(!branch.is_empty(), "Branch name should not be empty");
        assert!(
            is_valid_git_ref(&branch) || branch == "HEAD",
            "Branch name should be a valid git ref: {}",
            branch
        );
    }

    // ========================================================================
    // Current Branch Tests
    // ========================================================================

    #[test]
    fn test_get_current_branch() {
        let (_temp_dir, repo_path) = create_temp_repo();
        let result = get_current_branch(&repo_path);
        assert!(result.is_ok());
        assert!(!result.unwrap().is_empty());
    }

    #[test]
    fn test_get_current_branch_is_valid_ref() {
        let (_temp_dir, repo_path) = create_temp_repo();
        let branch = get_current_branch(&repo_path).unwrap();
        assert!(is_valid_git_ref(&branch));
    }

    #[test]
    fn test_create_fix_branch_from_current_uses_current_head() {
        let (_temp_dir, repo_path) = create_temp_repo();
        let repo = Repository::open(&repo_path).unwrap();

        let initial_commit = repo.head().unwrap().peel_to_commit().unwrap();
        repo.branch("feature/base", &initial_commit, false).unwrap();
        checkout_branch(&repo_path, "feature/base").unwrap();

        let feature_file = repo_path.join("feature.txt");
        std::fs::write(&feature_file, "feature work").unwrap();
        let mut index = repo.index().unwrap();
        index.add_path(Path::new("feature.txt")).unwrap();
        index.write().unwrap();
        let sig = Signature::now("Test User", "test@example.com").unwrap();
        let tree_id = index.write_tree().unwrap();
        let tree = repo.find_tree(tree_id).unwrap();
        let parent = repo.head().unwrap().peel_to_commit().unwrap();
        repo.commit(
            Some("HEAD"),
            &sig,
            &sig,
            "Feature commit",
            &tree,
            &[&parent],
        )
        .unwrap();

        let feature_head = repo.head().unwrap().target().unwrap();
        let created = create_fix_branch_from_current(&repo_path, "fix/from-current").unwrap();
        assert_eq!(created, "fix/from-current");
        assert_eq!(get_current_branch(&repo_path).unwrap(), created);

        let created_head = Repository::open(&repo_path)
            .unwrap()
            .head()
            .unwrap()
            .target()
            .unwrap();
        assert_eq!(created_head, feature_head);
    }

    #[test]
    fn test_create_fix_branch_from_current_with_outcome_marks_created_new() {
        let (_temp_dir, repo_path) = create_temp_repo();
        let outcome = create_fix_branch_from_current_with_outcome(&repo_path, "fix/new-outcome")
            .expect("branch should be created");
        assert_eq!(outcome.branch_name, "fix/new-outcome");
        assert!(outcome.created_new);
    }

    #[test]
    fn test_create_fix_branch_from_main_cleans_up_on_checkout_failure() {
        let (_temp_dir, repo_path) = create_temp_repo();
        let repo = Repository::open(&repo_path).unwrap();

        // Add a tracked file on the default branch.
        std::fs::write(repo_path.join("conflict.txt"), "tracked").unwrap();
        let mut index = repo.index().unwrap();
        index.add_path(Path::new("conflict.txt")).unwrap();
        index.write().unwrap();
        let sig = Signature::now("Test User", "test@example.com").unwrap();
        let tree_id = index.write_tree().unwrap();
        let tree = repo.find_tree(tree_id).unwrap();
        let parent = repo.head().unwrap().peel_to_commit().unwrap();
        repo.commit(
            Some("HEAD"),
            &sig,
            &sig,
            "Add tracked conflict file",
            &tree,
            &[&parent],
        )
        .unwrap();

        // Create a branch where the file is removed so we can later create it as untracked.
        let main_branch = get_main_branch_name(&repo_path).unwrap();
        let main_commit = repo.head().unwrap().peel_to_commit().unwrap();
        repo.branch("feature/without-conflict", &main_commit, false)
            .unwrap();
        checkout_branch(&repo_path, "feature/without-conflict").unwrap();

        std::fs::remove_file(repo_path.join("conflict.txt")).unwrap();
        let repo = Repository::open(&repo_path).unwrap();
        let mut index = repo.index().unwrap();
        index.remove_path(Path::new("conflict.txt")).unwrap();
        index.write().unwrap();
        let tree_id = index.write_tree().unwrap();
        let tree = repo.find_tree(tree_id).unwrap();
        let parent = repo.head().unwrap().peel_to_commit().unwrap();
        repo.commit(
            Some("HEAD"),
            &sig,
            &sig,
            "Remove conflict file",
            &tree,
            &[&parent],
        )
        .unwrap();

        // Create an untracked file that would be overwritten by checkout from main.
        std::fs::write(repo_path.join("conflict.txt"), "untracked").unwrap();
        let status = current_status(&repo_path).unwrap();
        assert!(status.untracked.iter().any(|p| p == "conflict.txt"));

        let branch_name = "fix/from-main";
        let err = create_fix_branch_from_main(&repo_path, branch_name).unwrap_err();
        assert!(err
            .to_string()
            .contains("Failed to checkout newly created branch"));

        // Ensure the failed attempt didn't leave the new branch behind.
        let repo = Repository::open(&repo_path).unwrap();
        assert!(repo
            .find_branch(branch_name, git2::BranchType::Local)
            .is_err());

        // Ensure we did not delete or corrupt the main branch reference.
        assert!(repo
            .find_branch(&main_branch, git2::BranchType::Local)
            .is_ok());
    }

    #[test]
    fn test_delete_local_branch_safe_deletes_non_tracking_branch() {
        let (_temp_dir, repo_path) = create_temp_repo();
        let source_branch = get_current_branch(&repo_path).unwrap();
        create_fix_branch_from_current(&repo_path, "fix/delete-me").unwrap();
        checkout_branch(&repo_path, &source_branch).unwrap();
        let deleted = delete_local_branch_safe(&repo_path, "fix/delete-me");
        assert!(deleted.is_ok());
        let repo = Repository::open(&repo_path).unwrap();
        assert!(repo
            .find_branch("fix/delete-me", git2::BranchType::Local)
            .is_err());
    }

    #[test]
    fn test_push_branch_blocked_when_sandbox_flag_is_set() {
        let _guard = PUSH_ENV_LOCK.lock().unwrap();
        std::env::set_var("COSMOS_DISABLE_PUSH", "1");

        let result = push_branch(Path::new("/this/path/does/not/matter"), "main");
        assert!(result.is_err());
        let message = result.unwrap_err().to_string();
        assert!(message.contains("Push blocked"));
        assert!(message.contains("COSMOS_DISABLE_PUSH=1"));

        std::env::remove_var("COSMOS_DISABLE_PUSH");
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

        let (_temp_dir, repo_path) = create_temp_repo();
        add_test_remote(
            &repo_path,
            "origin",
            "https://github.com/example/cosmos.git",
        );
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
        if let Some(val) = orig {
            std::env::set_var("GITHUB_TOKEN", val);
        }
    }

    #[tokio::test]
    async fn test_create_pr_is_async() {
        // This test just verifies create_pr is an async function
        // by using it in an async context
        let (_temp_dir, repo_path) = create_temp_repo();

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
        let (_temp_dir, repo_path) = create_temp_repo();
        commit_test_file(
            &repo_path,
            "Cargo.toml",
            "[package]\nname = \"cosmos\"\n",
            "add file",
        );

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
        let (_temp_dir, repo_path) = create_temp_repo();

        // Try reading a file that definitely doesn't exist
        let result = read_file_from_head(&repo_path, Path::new("definitely-not-a-real-file.xyz"));
        assert!(result.is_ok());
        assert!(result.unwrap().is_none());
    }

    // ========================================================================
    // Stash and Discard Tests
    // ========================================================================

    /// Helper to create a temporary git repo for testing
    fn create_temp_repo() -> (tempfile::TempDir, std::path::PathBuf) {
        let temp_dir = tempfile::tempdir().expect("Failed to create temp dir");
        let repo_path = temp_dir.path().to_path_buf();

        // Initialize git repo
        Repository::init(&repo_path).expect("Failed to init repo");

        // Configure git user (required for commits)
        let repo = Repository::open(&repo_path).unwrap();
        let mut config = repo.config().unwrap();
        config.set_str("user.name", "Test User").unwrap();
        config.set_str("user.email", "test@example.com").unwrap();

        // Create initial commit so HEAD exists
        let sig = Signature::now("Test User", "test@example.com").unwrap();
        let tree_id = repo.index().unwrap().write_tree().unwrap();
        let tree = repo.find_tree(tree_id).unwrap();
        repo.commit(Some("HEAD"), &sig, &sig, "Initial commit", &tree, &[])
            .unwrap();

        (temp_dir, repo_path)
    }

    fn commit_test_file(repo_path: &Path, rel_path: &str, content: &str, message: &str) {
        let full_path = repo_path.join(rel_path);
        if let Some(parent) = full_path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&full_path, content).unwrap();

        let repo = Repository::open(repo_path).unwrap();
        let mut index = repo.index().unwrap();
        index.add_path(Path::new(rel_path)).unwrap();
        index.write().unwrap();
        let tree_id = index.write_tree().unwrap();
        let tree = repo.find_tree(tree_id).unwrap();
        let parent = repo.head().unwrap().peel_to_commit().unwrap();
        let sig = Signature::now("Test User", "test@example.com").unwrap();
        repo.commit(Some("HEAD"), &sig, &sig, message, &tree, &[&parent])
            .unwrap();
    }

    fn add_test_remote(repo_path: &Path, name: &str, url: &str) {
        let repo = Repository::open(repo_path).unwrap();
        let _ = repo.remote(name, url).unwrap();
    }

    #[test]
    fn test_stash_changes_with_modifications() {
        let (_temp_dir, repo_path) = create_temp_repo();

        // Create a file and commit it
        let test_file = repo_path.join("test.txt");
        std::fs::write(&test_file, "original content").unwrap();

        let repo = Repository::open(&repo_path).unwrap();
        let mut index = repo.index().unwrap();
        index.add_path(Path::new("test.txt")).unwrap();
        index.write().unwrap();

        let sig = Signature::now("Test User", "test@example.com").unwrap();
        let tree_id = index.write_tree().unwrap();
        let tree = repo.find_tree(tree_id).unwrap();
        let parent = repo.head().unwrap().peel_to_commit().unwrap();
        repo.commit(Some("HEAD"), &sig, &sig, "Add test file", &tree, &[&parent])
            .unwrap();

        // Now modify the file
        std::fs::write(&test_file, "modified content").unwrap();

        // Stash the changes
        let result = stash_changes(&repo_path);
        assert!(result.is_ok(), "stash_changes failed: {:?}", result);

        let message = result.unwrap();
        assert!(
            message.contains("cosmos: saved work"),
            "Stash message should contain 'cosmos: saved work', got: {}",
            message
        );

        // Verify the file is back to original content
        let content = std::fs::read_to_string(&test_file).unwrap();
        assert_eq!(content, "original content");
    }

    #[test]
    fn test_stash_changes_no_changes() {
        let (_temp_dir, repo_path) = create_temp_repo();

        // Stash with no changes
        let result = stash_changes(&repo_path);
        assert!(result.is_ok());

        let message = result.unwrap();
        // Should indicate no changes to stash (or successfully stash nothing)
        assert!(
            message.contains("No changes") || message.contains("cosmos: saved work"),
            "Expected success message, got: {}",
            message
        );
    }

    #[test]
    fn test_discard_all_changes_modified_file() {
        let (_temp_dir, repo_path) = create_temp_repo();

        // Create a file and commit it
        let test_file = repo_path.join("test.txt");
        std::fs::write(&test_file, "original content").unwrap();

        let repo = Repository::open(&repo_path).unwrap();
        let mut index = repo.index().unwrap();
        index.add_path(Path::new("test.txt")).unwrap();
        index.write().unwrap();

        let sig = Signature::now("Test User", "test@example.com").unwrap();
        let tree_id = index.write_tree().unwrap();
        let tree = repo.find_tree(tree_id).unwrap();
        let parent = repo.head().unwrap().peel_to_commit().unwrap();
        repo.commit(Some("HEAD"), &sig, &sig, "Add test file", &tree, &[&parent])
            .unwrap();

        // Modify the file
        std::fs::write(&test_file, "modified content").unwrap();

        // Discard changes
        let result = discard_all_changes(&repo_path);
        assert!(result.is_ok(), "discard_all_changes failed: {:?}", result);

        // Verify the file is back to original content
        let content = std::fs::read_to_string(&test_file).unwrap();
        assert_eq!(content, "original content");
    }

    #[test]
    fn test_discard_all_changes_untracked_file() {
        let (_temp_dir, repo_path) = create_temp_repo();

        // Create an untracked file
        let untracked_file = repo_path.join("untracked.txt");
        std::fs::write(&untracked_file, "untracked content").unwrap();
        assert!(untracked_file.exists());

        // Discard changes (should clean untracked files)
        let result = discard_all_changes(&repo_path);
        assert!(result.is_ok(), "discard_all_changes failed: {:?}", result);

        // Verify the untracked file is removed
        assert!(!untracked_file.exists(), "Untracked file should be removed");
    }

    #[test]
    fn test_discard_all_changes_staged_file() {
        let (_temp_dir, repo_path) = create_temp_repo();

        // Create a file and commit it
        let test_file = repo_path.join("test.txt");
        std::fs::write(&test_file, "original content").unwrap();

        let repo = Repository::open(&repo_path).unwrap();
        let mut index = repo.index().unwrap();
        index.add_path(Path::new("test.txt")).unwrap();
        index.write().unwrap();

        let sig = Signature::now("Test User", "test@example.com").unwrap();
        let tree_id = index.write_tree().unwrap();
        let tree = repo.find_tree(tree_id).unwrap();
        let parent = repo.head().unwrap().peel_to_commit().unwrap();
        repo.commit(Some("HEAD"), &sig, &sig, "Add test file", &tree, &[&parent])
            .unwrap();

        // Modify and stage the file
        std::fs::write(&test_file, "staged content").unwrap();
        let mut index = repo.index().unwrap();
        index.add_path(Path::new("test.txt")).unwrap();
        index.write().unwrap();

        // Discard changes (should unstage and revert)
        let result = discard_all_changes(&repo_path);
        assert!(result.is_ok(), "discard_all_changes failed: {:?}", result);

        // Verify the file is back to original content
        let content = std::fs::read_to_string(&test_file).unwrap();
        assert_eq!(content, "original content");
    }
}
