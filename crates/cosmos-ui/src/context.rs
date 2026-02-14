//! Minimal git-aware work context for UI shell mode.

use git2::{Repository, StatusOptions};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Default)]
pub struct WorkContext {
    pub branch: String,
    pub uncommitted_files: Vec<PathBuf>,
    pub staged_files: Vec<PathBuf>,
    pub untracked_files: Vec<PathBuf>,
    pub inferred_focus: Option<String>,
    pub modified_count: usize,
    pub repo_root: PathBuf,
}

impl WorkContext {
    pub fn load(repo_path: &Path) -> anyhow::Result<Self> {
        let repo = Repository::discover(repo_path)?;
        let repo_root = repo
            .workdir()
            .ok_or_else(|| anyhow::anyhow!("No working directory"))?
            .to_path_buf();

        let mut ctx = Self {
            branch: current_branch(&repo)?,
            repo_root,
            ..Self::default()
        };
        ctx.refresh()?;
        Ok(ctx)
    }

    pub fn refresh(&mut self) -> anyhow::Result<()> {
        let repo = Repository::open(&self.repo_root)?;
        self.branch = current_branch(&repo)?;

        let mut opts = StatusOptions::new();
        opts.include_untracked(true);
        opts.include_ignored(false);
        opts.include_unmodified(false);
        opts.recurse_untracked_dirs(true);
        opts.exclude_submodules(true);

        let statuses = repo.statuses(Some(&mut opts))?;
        self.uncommitted_files.clear();
        self.staged_files.clear();
        self.untracked_files.clear();

        for entry in statuses.iter() {
            let status = entry.status();
            let path = match entry.path() {
                Some(p) => PathBuf::from(p),
                None => continue,
            };

            if status.is_wt_modified() || status.is_wt_deleted() || status.is_wt_renamed() {
                self.uncommitted_files.push(path.clone());
            }
            if status.is_index_new()
                || status.is_index_modified()
                || status.is_index_deleted()
                || status.is_index_renamed()
            {
                self.staged_files.push(path.clone());
            }
            if status.is_wt_new() {
                self.untracked_files.push(path);
            }
        }

        self.modified_count =
            self.uncommitted_files.len() + self.staged_files.len() + self.untracked_files.len();
        self.inferred_focus = None;

        Ok(())
    }

    pub fn all_changed_files(&self) -> Vec<&PathBuf> {
        self.uncommitted_files
            .iter()
            .chain(self.staged_files.iter())
            .chain(self.untracked_files.iter())
            .collect()
    }
}

fn current_branch(repo: &Repository) -> anyhow::Result<String> {
    let head = repo.head()?;
    Ok(head.shorthand().unwrap_or("HEAD").to_string())
}
