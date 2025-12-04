use anyhow::Result;
use chrono::{DateTime, TimeZone, Utc};
use git2::Repository;
use std::collections::HashMap;
use std::path::Path;
use walkdir::WalkDir;

/// Represents a file that hasn't been touched in a while
#[derive(Debug, Clone)]
pub struct DustyFile {
    pub path: String,
    pub last_modified: DateTime<Utc>,
    pub days_since_change: i64,
    pub line_count: usize,
}

/// Analyzes file staleness based on git history
pub struct StalenessAnalyzer {
    repo: Repository,
    ignore_dirs: Vec<String>,
}

impl StalenessAnalyzer {
    pub fn new(path: &Path) -> Result<Self> {
        let repo = Repository::discover(path)?;
        let ignore_dirs = vec![
            ".git".to_string(),
            "node_modules".to_string(),
            "target".to_string(),
            "vendor".to_string(),
            "dist".to_string(),
            "build".to_string(),
            ".next".to_string(),
            "__pycache__".to_string(),
            ".venv".to_string(),
            "venv".to_string(),
        ];
        Ok(Self { repo, ignore_dirs })
    }

    /// Find files that haven't been modified in at least `min_days` days
    pub fn find_dusty_files(&self, min_days: i64) -> Result<Vec<DustyFile>> {
        let workdir = self.repo.workdir().unwrap_or(Path::new("."));
        
        // Build a map of file -> last commit time
        let mut file_times: HashMap<String, DateTime<Utc>> = HashMap::new();
        
        let mut revwalk = self.repo.revwalk()?;
        revwalk.push_head()?;

        for oid in revwalk {
            let oid = oid?;
            let commit = self.repo.find_commit(oid)?;
            
            let commit_time = Utc.timestamp_opt(commit.time().seconds(), 0)
                .single()
                .unwrap_or_else(Utc::now);

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
                        file_times
                            .entry(path.to_string())
                            .or_insert(commit_time);
                    }
                    true
                },
                None,
                None,
                None,
            )?;
        }

        let now = Utc::now();
        let mut dusty_files = Vec::new();

        // Walk the current file tree and check against our map
        for entry in WalkDir::new(workdir)
            .follow_links(false)
            .into_iter()
            .filter_entry(|e| !self.should_ignore(e))
        {
            let entry = match entry {
                Ok(e) => e,
                Err(_) => continue,
            };

            if !entry.file_type().is_file() {
                continue;
            }

            let path = entry.path();
            let relative_path = path
                .strip_prefix(workdir)
                .unwrap_or(path)
                .to_string_lossy()
                .to_string();

            // Skip non-code files
            if !self.is_code_file(path) {
                continue;
            }

            if let Some(&last_modified) = file_times.get(&relative_path) {
                let days_since = (now - last_modified).num_days();
                
                if days_since >= min_days {
                    let line_count = std::fs::read_to_string(path)
                        .map(|c| c.lines().count())
                        .unwrap_or(0);

                    dusty_files.push(DustyFile {
                        path: relative_path,
                        last_modified,
                        days_since_change: days_since,
                        line_count,
                    });
                }
            }
        }

        // Sort by days since change descending (dustiest first)
        dusty_files.sort_by(|a, b| b.days_since_change.cmp(&a.days_since_change));

        Ok(dusty_files)
    }

    /// Get total file count in the repository
    pub fn total_file_count(&self) -> Result<usize> {
        let workdir = self.repo.workdir().unwrap_or(Path::new("."));
        let mut count = 0;

        for entry in WalkDir::new(workdir)
            .follow_links(false)
            .into_iter()
            .filter_entry(|e| !self.should_ignore(e))
        {
            if let Ok(e) = entry {
                if e.file_type().is_file() && self.is_code_file(e.path()) {
                    count += 1;
                }
            }
        }

        Ok(count)
    }

    fn should_ignore(&self, entry: &walkdir::DirEntry) -> bool {
        entry
            .file_name()
            .to_str()
            .map(|name| {
                self.ignore_dirs.contains(&name.to_string())
                    || name.starts_with('.')
            })
            .unwrap_or(false)
    }

    fn is_code_file(&self, path: &Path) -> bool {
        let code_extensions = [
            "rs", "js", "ts", "tsx", "jsx", "py", "rb", "go", "java", "kt", "scala",
            "c", "cpp", "h", "hpp", "cs", "swift", "m", "mm", "php", "pl", "pm",
            "sh", "bash", "zsh", "html", "htm", "css", "scss", "sass", "less",
            "json", "yaml", "yml", "toml", "xml", "sql", "graphql",
            "vue", "svelte", "astro", "ex", "exs", "hs", "elm", "clj",
            "lua", "r", "jl", "nim", "zig",
        ];

        path.extension()
            .and_then(|e| e.to_str())
            .map(|ext| code_extensions.contains(&ext.to_lowercase().as_str()))
            .unwrap_or(false)
    }
}


