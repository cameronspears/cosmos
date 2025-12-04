use anyhow::{Context, Result};
use chrono::{DateTime, Duration, TimeZone, Utc};
use git2::{Repository, Sort};
use std::collections::HashMap;
use std::path::Path;

/// Represents a file's churn statistics
#[derive(Debug, Clone)]
pub struct ChurnEntry {
    pub path: String,
    pub change_count: usize,
    pub last_changed: DateTime<Utc>,
    pub days_active: i64,
}

/// Analyzes git history for churn and activity patterns
pub struct GitAnalyzer {
    repo: Repository,
}

impl GitAnalyzer {
    pub fn new(path: &Path) -> Result<Self> {
        let repo = Repository::discover(path).context("Failed to find git repository")?;
        Ok(Self { repo })
    }

    /// Get the current branch name
    pub fn current_branch(&self) -> Result<String> {
        let head = self.repo.head().context("Failed to get HEAD")?;
        Ok(head
            .shorthand()
            .unwrap_or("detached")
            .to_string())
    }

    /// Get the repository name from the path
    pub fn repo_name(&self) -> String {
        self.repo
            .workdir()
            .and_then(|p| p.file_name())
            .and_then(|n| n.to_str())
            .unwrap_or("unknown")
            .to_string()
    }

    /// Analyze file churn over the specified number of days
    pub fn analyze_churn(&self, days: i64) -> Result<Vec<ChurnEntry>> {
        let mut file_changes: HashMap<String, (usize, DateTime<Utc>)> = HashMap::new();
        let cutoff = Utc::now() - Duration::days(days);

        let mut revwalk = self.repo.revwalk()?;
        revwalk.set_sorting(Sort::TIME)?;
        revwalk.push_head()?;

        for oid in revwalk {
            let oid = oid?;
            let commit = self.repo.find_commit(oid)?;
            
            let commit_time = Utc.timestamp_opt(commit.time().seconds(), 0)
                .single()
                .unwrap_or_else(Utc::now);

            if commit_time < cutoff {
                break;
            }

            // Get the diff for this commit
            let tree = commit.tree()?;
            let parent_tree = commit.parent(0).ok().and_then(|p| p.tree().ok());

            let diff = self.repo.diff_tree_to_tree(
                parent_tree.as_ref(),
                Some(&tree),
                None,
            )?;

            diff.foreach(
                &mut |delta, _| {
                    if let Some(path) = delta.new_file().path().and_then(|p| p.to_str()) {
                        let entry = file_changes
                            .entry(path.to_string())
                            .or_insert((0, commit_time));
                        entry.0 += 1;
                        if commit_time > entry.1 {
                            entry.1 = commit_time;
                        }
                    }
                    true
                },
                None,
                None,
                None,
            )?;
        }

        let now = Utc::now();
        let mut entries: Vec<ChurnEntry> = file_changes
            .into_iter()
            .map(|(path, (count, last_changed))| {
                let days_active = (now - last_changed).num_days();
                ChurnEntry {
                    path,
                    change_count: count,
                    last_changed,
                    days_active,
                }
            })
            .collect();

        // Sort by change count descending
        entries.sort_by(|a, b| b.change_count.cmp(&a.change_count));

        Ok(entries)
    }

    /// Get total commit count in the specified time period
    pub fn commit_count(&self, days: i64) -> Result<usize> {
        let cutoff = Utc::now() - Duration::days(days);
        let mut count = 0;

        let mut revwalk = self.repo.revwalk()?;
        revwalk.set_sorting(Sort::TIME)?;
        revwalk.push_head()?;

        for oid in revwalk {
            let oid = oid?;
            let commit = self.repo.find_commit(oid)?;
            
            let commit_time = Utc.timestamp_opt(commit.time().seconds(), 0)
                .single()
                .unwrap_or_else(Utc::now);

            if commit_time < cutoff {
                break;
            }
            count += 1;
        }

        Ok(count)
    }

    /// Calculate the add/delete ratio for recent commits
    pub fn add_delete_ratio(&self, days: i64) -> Result<f64> {
        let cutoff = Utc::now() - Duration::days(days);
        let mut total_additions = 0usize;
        let mut total_deletions = 0usize;

        let mut revwalk = self.repo.revwalk()?;
        revwalk.set_sorting(Sort::TIME)?;
        revwalk.push_head()?;

        for oid in revwalk {
            let oid = oid?;
            let commit = self.repo.find_commit(oid)?;
            
            let commit_time = Utc.timestamp_opt(commit.time().seconds(), 0)
                .single()
                .unwrap_or_else(Utc::now);

            if commit_time < cutoff {
                break;
            }

            let tree = commit.tree()?;
            let parent_tree = commit.parent(0).ok().and_then(|p| p.tree().ok());

            let diff = self.repo.diff_tree_to_tree(
                parent_tree.as_ref(),
                Some(&tree),
                None,
            )?;

            let stats = diff.stats()?;
            total_additions += stats.insertions();
            total_deletions += stats.deletions();
        }

        if total_additions == 0 {
            return Ok(0.0);
        }

        Ok(total_deletions as f64 / total_additions as f64)
    }

    /// Get the number of unique files changed in recent commits
    pub fn files_changed_count(&self, days: i64) -> Result<usize> {
        let churn = self.analyze_churn(days)?;
        Ok(churn.len())
    }
}


