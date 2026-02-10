use crate::lab::sandbox::SandboxSession;
use crate::util::{run_command_with_timeout, truncate};
use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

const CLONE_TIMEOUT: Duration = Duration::from_secs(1_200);
const FETCH_TIMEOUT: Duration = Duration::from_secs(600);
const CHECKOUT_TIMEOUT: Duration = Duration::from_secs(120);
const REV_PARSE_TIMEOUT: Duration = Duration::from_secs(30);

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CorpusManifest {
    pub schema_version: u32,
    #[serde(default)]
    pub repo: Vec<CorpusRepoSpec>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CorpusRepoSpec {
    pub id: String,
    pub git_url: String,
    #[serde(rename = "ref")]
    pub git_ref: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub subdir: Option<String>,
    #[serde(default)]
    pub sample_size_override: Option<usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CorpusRepoCheckout {
    pub id: String,
    pub git_url: String,
    pub requested_ref: String,
    pub local_path: PathBuf,
    pub head_sha: String,
    pub subdir: Option<String>,
}

impl CorpusManifest {
    pub fn load(path: &Path) -> Result<Self> {
        let content = std::fs::read_to_string(path)
            .with_context(|| format!("Failed to read corpus manifest '{}'", path.display()))?;
        let manifest: CorpusManifest = toml::from_str(&content).with_context(|| {
            format!("Failed to parse corpus manifest TOML '{}'", path.display())
        })?;
        manifest.validate()?;
        Ok(manifest)
    }

    fn validate(&self) -> Result<()> {
        if self.schema_version != 1 {
            return Err(anyhow!(
                "Unsupported corpus manifest schema_version={} (expected 1)",
                self.schema_version
            ));
        }

        let mut seen: HashSet<String> = HashSet::new();
        for repo in &self.repo {
            if repo.id.trim().is_empty() {
                return Err(anyhow!("Corpus repo id must not be empty"));
            }
            let id_ok = repo
                .id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'));
            if !id_ok {
                return Err(anyhow!(
                    "Corpus repo id '{}' contains unsupported characters (allowed: a-zA-Z0-9_-)",
                    repo.id
                ));
            }
            if !seen.insert(repo.id.clone()) {
                return Err(anyhow!("Duplicate corpus repo id '{}'", repo.id));
            }
            if repo.git_url.trim().is_empty() {
                return Err(anyhow!(
                    "Corpus repo '{}' git_url must not be empty",
                    repo.id
                ));
            }
            if repo.git_ref.trim().is_empty() {
                return Err(anyhow!("Corpus repo '{}' ref must not be empty", repo.id));
            }

            if let Some(subdir) = repo.subdir.as_ref() {
                let candidate = PathBuf::from(subdir);
                if !is_safe_relative_path(&candidate) {
                    return Err(anyhow!(
                        "Corpus repo '{}' subdir '{}' is unsafe (must be relative, no traversal)",
                        repo.id,
                        subdir
                    ));
                }
            }
        }
        Ok(())
    }
}

fn is_safe_relative_path(path: &Path) -> bool {
    if path.as_os_str().is_empty() || path.is_absolute() {
        return false;
    }
    for component in path.components() {
        match component {
            std::path::Component::CurDir | std::path::Component::Normal(_) => {}
            _ => return false,
        }
    }
    true
}

pub fn sync_repo(
    spec: &CorpusRepoSpec,
    corpus_root: &Path,
    sync: bool,
) -> Result<CorpusRepoCheckout> {
    std::fs::create_dir_all(corpus_root).with_context(|| {
        format!(
            "Failed to create corpus root directory '{}'",
            corpus_root.display()
        )
    })?;
    let local_path = corpus_root.join(&spec.id);

    if !local_path.exists() {
        clone_repo(&spec.git_url, &local_path)?;
    } else if !local_path.join(".git").exists() {
        return Err(anyhow!(
            "Corpus path '{}' exists but is not a git repository (missing .git)",
            local_path.display()
        ));
    }

    if sync {
        fetch_repo(&local_path)?;
    }

    checkout_detached(&local_path, &spec.git_ref)?;
    // Make sure the working tree is clean for reproducible worktree creation.
    reset_clean(&local_path)?;
    let head_sha = rev_parse_head(&local_path)?;

    Ok(CorpusRepoCheckout {
        id: spec.id.clone(),
        git_url: spec.git_url.clone(),
        requested_ref: spec.git_ref.clone(),
        local_path,
        head_sha,
        subdir: spec.subdir.clone(),
    })
}

fn run_git(repo_dir: &Path, args: &[&str], timeout: Duration) -> Result<(bool, String, String)> {
    let mut cmd = Command::new("git");
    cmd.current_dir(repo_dir).args(args);
    for (k, v) in SandboxSession::env_overrides() {
        cmd.env(k, v);
    }
    let output = run_command_with_timeout(&mut cmd, timeout)
        .map_err(|e| anyhow!("Failed to start git command: {}", e))?;
    let success = !output.timed_out && output.status.map(|s| s.success()).unwrap_or(false);
    Ok((success, output.stdout, output.stderr))
}

fn clone_repo(git_url: &str, dest: &Path) -> Result<()> {
    let dest_str = dest.to_string_lossy().to_string();
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent).with_context(|| {
            format!("Failed to create corpus repo parent '{}'", parent.display())
        })?;
    }

    // Prefer a partial clone to reduce disk and network, but fall back to a normal clone
    // for older git versions.
    let attempts: Vec<Vec<&str>> = vec![
        vec![
            "clone",
            "--filter=blob:none",
            "--no-checkout",
            git_url,
            &dest_str,
        ],
        vec!["clone", "--no-checkout", git_url, &dest_str],
    ];
    for (idx, args) in attempts.iter().enumerate() {
        let mut cmd = Command::new("git");
        cmd.args(args);
        for (k, v) in SandboxSession::env_overrides() {
            cmd.env(k, v);
        }
        let output = run_command_with_timeout(&mut cmd, CLONE_TIMEOUT)
            .map_err(|e| anyhow!("Failed to start git clone: {}", e))?;
        if !output.timed_out && output.status.map(|s| s.success()).unwrap_or(false) {
            return Ok(());
        }

        let stderr_lower = output.stderr.to_ascii_lowercase();
        let unsupported = stderr_lower.contains("unknown option")
            || stderr_lower.contains("unrecognized option")
            || stderr_lower.contains("invalid option")
            || stderr_lower.contains("illegal option");
        if unsupported && idx + 1 < attempts.len() {
            continue;
        }

        return Err(anyhow!(
            "git clone failed for {}: {}",
            git_url,
            truncate(&output.stderr, 240)
        ));
    }

    Err(anyhow!("git clone failed for {}", git_url))
}

fn fetch_repo(repo_dir: &Path) -> Result<()> {
    let (ok, _stdout, stderr) = run_git(
        repo_dir,
        &["fetch", "--all", "--tags", "--prune"],
        FETCH_TIMEOUT,
    )?;
    if ok {
        return Ok(());
    }
    Err(anyhow!(
        "git fetch failed in {}: {}",
        repo_dir.display(),
        truncate(&stderr, 240)
    ))
}

fn checkout_detached(repo_dir: &Path, git_ref: &str) -> Result<()> {
    // Ensure the ref is present. This makes commit-sha refs work even after a shallow clone.
    let _ = run_git(repo_dir, &["fetch", "origin", git_ref], FETCH_TIMEOUT);

    let (ok, _stdout, stderr) = run_git(
        repo_dir,
        &["checkout", "--detach", git_ref],
        CHECKOUT_TIMEOUT,
    )?;
    if ok {
        return Ok(());
    }
    Err(anyhow!(
        "git checkout --detach {} failed in {}: {}",
        git_ref,
        repo_dir.display(),
        truncate(&stderr, 240)
    ))
}

fn reset_clean(repo_dir: &Path) -> Result<()> {
    let _ = run_git(repo_dir, &["reset", "--hard"], CHECKOUT_TIMEOUT);
    let _ = run_git(repo_dir, &["clean", "-fd"], CHECKOUT_TIMEOUT);
    Ok(())
}

fn rev_parse_head(repo_dir: &Path) -> Result<String> {
    let (ok, stdout, stderr) = run_git(repo_dir, &["rev-parse", "HEAD"], REV_PARSE_TIMEOUT)?;
    if !ok {
        return Err(anyhow!(
            "git rev-parse HEAD failed in {}: {}",
            repo_dir.display(),
            truncate(&stderr, 240)
        ));
    }
    Ok(stdout.trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;
    use tempfile::tempdir;

    fn git(repo: &Path, args: &[&str]) {
        let status = Command::new("git")
            .current_dir(repo)
            .args(args)
            .status()
            .expect("git command failed to start");
        assert!(status.success(), "git {:?} failed", args);
    }

    #[test]
    fn sync_repo_checks_out_requested_ref_and_records_head_sha() {
        let source = tempdir().unwrap();
        git(source.path(), &["init"]);
        git(source.path(), &["config", "user.email", "test@example.com"]);
        git(source.path(), &["config", "user.name", "Test"]);
        std::fs::write(source.path().join("README.md"), "hello").unwrap();
        git(source.path(), &["add", "."]);
        git(source.path(), &["commit", "-m", "init"]);
        let sha = Command::new("git")
            .current_dir(source.path())
            .args(["rev-parse", "HEAD"])
            .output()
            .unwrap();
        let sha = String::from_utf8_lossy(&sha.stdout).trim().to_string();

        let corpus_root = tempdir().unwrap();
        let spec = CorpusRepoSpec {
            id: "repo1".to_string(),
            git_url: source.path().to_string_lossy().to_string(),
            git_ref: sha.clone(),
            enabled: true,
            subdir: None,
            sample_size_override: None,
        };
        let checkout = sync_repo(&spec, corpus_root.path(), true).unwrap();
        assert_eq!(checkout.head_sha, sha);
        assert!(checkout.local_path.join(".git").exists());
    }
}
