use crate::util::run_command_with_timeout;
use anyhow::{anyhow, Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

const GIT_WORKTREE_TIMEOUT: Duration = Duration::from_secs(60);
const GIT_SWITCH_TIMEOUT: Duration = Duration::from_secs(30);

const SANDBOX_ROOT_DIR: &str = "cosmos-sandbox";

/// Isolated worktree session used for safe validation loops.
#[derive(Debug, Clone)]
pub struct SandboxSession {
    source_repo: PathBuf,
    run_root: PathBuf,
    worktree_path: PathBuf,
    branch_name: Option<String>,
}

impl SandboxSession {
    /// Create a detached git worktree in `$TMPDIR/cosmos-sandbox/<run_id>/<label>`.
    pub fn create(
        source_repo: &Path,
        run_id: &str,
        label: &str,
        create_branch: bool,
    ) -> Result<Self> {
        let source_repo = source_repo.canonicalize().with_context(|| {
            format!("Failed to resolve source repo '{}'", source_repo.display())
        })?;
        let safe_label = sanitize_component(label);
        let run_root = std::env::temp_dir()
            .join(SANDBOX_ROOT_DIR)
            .join(sanitize_component(run_id));
        let worktree_path = run_root.join(safe_label);

        std::fs::create_dir_all(&run_root).with_context(|| {
            format!(
                "Failed to create sandbox run directory '{}'",
                run_root.display()
            )
        })?;

        if worktree_path.exists() {
            std::fs::remove_dir_all(&worktree_path).with_context(|| {
                format!(
                    "Failed to clear existing sandbox worktree '{}'",
                    worktree_path.display()
                )
            })?;
        }

        run_git(
            &source_repo,
            &[
                "worktree",
                "add",
                "--detach",
                &worktree_path.to_string_lossy(),
            ],
            GIT_WORKTREE_TIMEOUT,
        )
        .with_context(|| {
            format!(
                "Failed to create detached worktree '{}' from '{}'",
                worktree_path.display(),
                source_repo.display()
            )
        })?;

        let mut session = Self {
            source_repo,
            run_root,
            worktree_path,
            branch_name: None,
        };

        if create_branch {
            session.create_local_branch()?;
        }

        Ok(session)
    }

    pub fn path(&self) -> &Path {
        &self.worktree_path
    }

    pub fn source_repo(&self) -> &Path {
        &self.source_repo
    }

    pub fn branch_name(&self) -> Option<&str> {
        self.branch_name.as_deref()
    }

    pub fn run_root(&self) -> &Path {
        &self.run_root
    }

    pub fn cleanup(&self) -> Result<()> {
        if self.worktree_path.exists() {
            run_git(
                &self.source_repo,
                &[
                    "worktree",
                    "remove",
                    "--force",
                    &self.worktree_path.to_string_lossy(),
                ],
                GIT_WORKTREE_TIMEOUT,
            )
            .with_context(|| {
                format!(
                    "Failed to remove sandbox worktree '{}'",
                    self.worktree_path.display()
                )
            })?;
        }

        let _ = std::fs::remove_dir(&self.run_root);
        Ok(())
    }

    pub fn env_overrides() -> Vec<(String, String)> {
        vec![
            ("GIT_TERMINAL_PROMPT".to_string(), "0".to_string()),
            ("GIT_ASKPASS".to_string(), "/bin/true".to_string()),
            ("COSMOS_DISABLE_PUSH".to_string(), "1".to_string()),
        ]
    }

    fn create_local_branch(&mut self) -> Result<()> {
        let run_id = self
            .run_root
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("run");
        let label = self
            .worktree_path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("worktree");
        let fragment = format!("{}-{}", run_id, label);
        let branch = format!("codex/self-iterate-{}", sanitize_branch_fragment(&fragment));

        let switch_result = run_git(
            &self.worktree_path,
            &["switch", "-c", &branch],
            GIT_SWITCH_TIMEOUT,
        );
        if switch_result.is_err() {
            run_git(
                &self.worktree_path,
                &["checkout", "-b", &branch],
                GIT_SWITCH_TIMEOUT,
            )
            .with_context(|| {
                format!(
                    "Failed to create sandbox branch '{}' in '{}'",
                    branch,
                    self.worktree_path.display()
                )
            })?;
        }

        self.branch_name = Some(branch);
        Ok(())
    }
}

fn run_git(repo_dir: &Path, args: &[&str], timeout: Duration) -> Result<()> {
    let mut cmd = Command::new("git");
    cmd.current_dir(repo_dir).args(args);
    for (k, v) in SandboxSession::env_overrides() {
        cmd.env(k, v);
    }
    let output = run_command_with_timeout(&mut cmd, timeout)
        .map_err(|e| anyhow!("Failed to run git command: {}", e))?;

    if output.timed_out {
        return Err(anyhow!(
            "git command timed out after {}s: git {}",
            timeout.as_secs(),
            args.join(" ")
        ));
    }

    if output.status.map(|s| s.success()).unwrap_or(false) {
        return Ok(());
    }

    Err(anyhow!(
        "git {} failed:\nstdout:\n{}\nstderr:\n{}",
        args.join(" "),
        output.stdout,
        output.stderr
    ))
}

fn sanitize_component(input: &str) -> String {
    let cleaned = input
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
        .collect::<String>();
    if cleaned.is_empty() {
        "run".to_string()
    } else {
        cleaned
    }
}

fn sanitize_branch_fragment(input: &str) -> String {
    let mut out = input
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect::<String>();
    while out.contains("--") {
        out = out.replace("--", "-");
    }
    out.trim_matches('-').chars().take(80).collect::<String>()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn setup_repo() -> (tempfile::TempDir, PathBuf) {
        let root = tempdir().unwrap();
        let repo = root.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();

        run_git_for_test(&repo, &["init"]).unwrap();
        run_git_for_test(&repo, &["config", "user.name", "Cosmos Test"]).unwrap();
        run_git_for_test(&repo, &["config", "user.email", "cosmos@test.local"]).unwrap();
        std::fs::write(repo.join("README.md"), "hello\n").unwrap();
        run_git_for_test(&repo, &["add", "."]).unwrap();
        run_git_for_test(&repo, &["commit", "-m", "init"]).unwrap();

        (root, repo)
    }

    fn run_git_for_test(repo: &Path, args: &[&str]) -> Result<()> {
        let mut cmd = Command::new("git");
        cmd.current_dir(repo).args(args);
        let out = run_command_with_timeout(&mut cmd, Duration::from_secs(20))
            .map_err(|e| anyhow!("{}", e))?;
        if out.status.map(|s| s.success()).unwrap_or(false) {
            Ok(())
        } else {
            Err(anyhow!(
                "git {} failed:\nstdout:{}\nstderr:{}",
                args.join(" "),
                out.stdout,
                out.stderr
            ))
        }
    }

    #[test]
    fn sandbox_lifecycle_creates_and_cleans_worktree() {
        let (_tmp, repo) = setup_repo();
        let session = SandboxSession::create(&repo, "run-001", "target", true).unwrap();
        assert!(session.path().exists());
        assert!(session.branch_name().is_some());

        std::fs::write(session.path().join("sandbox-only.txt"), "tmp").unwrap();
        assert!(!repo.join("sandbox-only.txt").exists());

        session.cleanup().unwrap();
        assert!(!session.path().exists());
    }

    #[test]
    fn sandbox_environment_has_no_prompt_and_no_push_flags() {
        let env = SandboxSession::env_overrides();
        assert!(env
            .iter()
            .any(|(k, v)| k == "GIT_TERMINAL_PROMPT" && v == "0"));
        assert!(env
            .iter()
            .any(|(k, v)| k == "COSMOS_DISABLE_PUSH" && v == "1"));
    }
}
