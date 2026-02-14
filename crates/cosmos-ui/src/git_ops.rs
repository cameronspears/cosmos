//! Minimal git operations for UI shell mode.

use anyhow::{Context, Result};
use git2::{Repository, Signature, StatusOptions};
use std::path::Path;
use std::process::Command;

#[derive(Debug, Clone, Default)]
pub struct GitStatus {
    pub branch: String,
    pub staged: Vec<String>,
    pub modified: Vec<String>,
    pub untracked: Vec<String>,
    pub ahead: usize,
    pub behind: usize,
}

fn open_repo(repo_path: &Path) -> Result<Repository> {
    Repository::discover(repo_path).with_context(|| {
        format!(
            "Failed to open repository from path '{}'",
            repo_path.display()
        )
    })
}

pub fn current_status(repo_path: &Path) -> Result<GitStatus> {
    let repo = open_repo(repo_path)?;
    let head = repo.head().context("Failed to read HEAD")?;
    let branch = head.shorthand().unwrap_or("detached").to_string();

    let mut status = GitStatus {
        branch,
        ..GitStatus::default()
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

        if s.is_index_new() || s.is_index_modified() || s.is_index_deleted() || s.is_index_renamed()
        {
            status.staged.push(path.clone());
        }
        if s.is_wt_modified() || s.is_wt_deleted() || s.is_wt_renamed() {
            status.modified.push(path.clone());
        }
        if s.is_wt_new() {
            status.untracked.push(path);
        }
    }

    Ok(status)
}

pub fn get_main_branch_name(repo_path: &Path) -> Result<String> {
    let repo = open_repo(repo_path)?;
    if repo.find_branch("main", git2::BranchType::Local).is_ok() {
        return Ok("main".to_string());
    }
    if repo.find_branch("master", git2::BranchType::Local).is_ok() {
        return Ok("master".to_string());
    }
    let head = repo.head().context("Failed to read HEAD")?;
    Ok(head.shorthand().unwrap_or("HEAD").to_string())
}

pub fn checkout_branch(repo_path: &Path, name: &str) -> Result<()> {
    let repo = open_repo(repo_path)?;
    let (object, reference) = repo
        .revparse_ext(name)
        .with_context(|| format!("Branch '{}' not found", name))?;

    repo.checkout_tree(&object, None)?;
    match reference {
        Some(r) => repo.set_head(r.name().unwrap_or("HEAD"))?,
        None => repo.set_head_detached(object.id())?,
    }

    Ok(())
}

pub fn restore_file(repo_path: &Path, path: &Path) -> Result<()> {
    let repo = open_repo(repo_path)?;
    let mut checkout = git2::build::CheckoutBuilder::new();
    checkout.force().path(path);
    repo.checkout_head(Some(&mut checkout))?;
    Ok(())
}

pub fn stash_changes(repo_path: &Path) -> Result<String> {
    let status = Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(repo_path)
        .output()
        .context("Failed to inspect git status")?;

    if !status.status.success() || status.stdout.is_empty() {
        return Ok("No changes to stash".to_string());
    }

    let output = Command::new("git")
        .args(["stash", "push", "-u", "-m", "cosmos: saved work"])
        .current_dir(repo_path)
        .output()
        .context("Failed to run git stash")?;

    if output.status.success() {
        Ok("cosmos: saved work".to_string())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        Err(anyhow::anyhow!(if stderr.is_empty() {
            "Failed to stash changes".to_string()
        } else {
            stderr
        }))
    }
}

pub fn discard_all_changes(repo_path: &Path) -> Result<()> {
    let reset = Command::new("git")
        .args(["reset", "--hard", "HEAD"])
        .current_dir(repo_path)
        .output()
        .context("Failed to run git reset --hard")?;

    if !reset.status.success() {
        let stderr = String::from_utf8_lossy(&reset.stderr).trim().to_string();
        return Err(anyhow::anyhow!(if stderr.is_empty() {
            "Failed to reset working tree".to_string()
        } else {
            stderr
        }));
    }

    let clean = Command::new("git")
        .args(["clean", "-fd"])
        .current_dir(repo_path)
        .output()
        .context("Failed to run git clean")?;

    if !clean.status.success() {
        let stderr = String::from_utf8_lossy(&clean.stderr).trim().to_string();
        return Err(anyhow::anyhow!(if stderr.is_empty() {
            "Failed to clean untracked files".to_string()
        } else {
            stderr
        }));
    }

    Ok(())
}

pub fn commit(repo_path: &Path, message: &str) -> Result<String> {
    let repo = open_repo(repo_path)?;

    let mut index = repo.index()?;
    index.add_all(["*"], git2::IndexAddOption::DEFAULT, None)?;
    index.write()?;

    let tree_id = index.write_tree()?;
    let tree = repo.find_tree(tree_id)?;

    let parent = repo.head().ok().and_then(|h| h.peel_to_commit().ok());

    let config = repo.config()?;
    let name = config
        .get_string("user.name")
        .unwrap_or_else(|_| "cosmos".to_string());
    let email = config
        .get_string("user.email")
        .unwrap_or_else(|_| "cosmos@local".to_string());
    let sig = Signature::now(&name, &email)?;

    let oid = if let Some(parent) = parent {
        repo.commit(Some("HEAD"), &sig, &sig, message, &tree, &[&parent])?
    } else {
        repo.commit(Some("HEAD"), &sig, &sig, message, &tree, &[])?
    };

    Ok(oid.to_string())
}

pub fn push_branch(repo_path: &Path, branch: &str) -> Result<String> {
    let output = Command::new("git")
        .args(["push", "origin", branch])
        .current_dir(repo_path)
        .output()
        .context("Failed to run git push")?;

    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        Err(anyhow::anyhow!(if stderr.is_empty() {
            "git push failed".to_string()
        } else {
            stderr
        }))
    }
}

pub async fn create_pr(_repo_path: &Path, _title: &str, _body: &str) -> Result<String> {
    Err(anyhow::anyhow!("PR creation is disabled in UI shell mode"))
}

pub fn open_url(url: &str) -> Result<()> {
    #[cfg(target_os = "macos")]
    {
        Command::new("open")
            .arg(url)
            .output()
            .context("Failed to launch open")?;
        return Ok(());
    }

    #[cfg(target_os = "linux")]
    {
        Command::new("xdg-open")
            .arg(url)
            .output()
            .context("Failed to launch xdg-open")?;
        return Ok(());
    }

    #[cfg(target_os = "windows")]
    {
        Command::new("cmd")
            .args(["/C", "start", "", url])
            .output()
            .context("Failed to launch start")?;
        return Ok(());
    }

    #[allow(unreachable_code)]
    Err(anyhow::anyhow!(
        "Opening URLs is not supported on this platform"
    ))
}
