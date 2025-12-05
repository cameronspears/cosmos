//! Author analysis for bus factor and knowledge distribution
//!
//! Identifies files with concentrated ownership (high bus factor risk)
//! and tracks who knows what across the codebase.

use anyhow::{Context, Result};
use chrono::{DateTime, Duration, TimeZone, Utc};
use git2::{BlameOptions, Repository, Sort};
use std::collections::HashMap;
use std::path::Path;
use walkdir::WalkDir;

/// Author statistics for a single file
#[derive(Debug, Clone)]
pub struct FileAuthorship {
    pub path: String,
    /// Total number of unique authors who have touched this file
    pub author_count: usize,
    /// Primary author (most lines attributed)
    pub primary_author: String,
    /// Percentage of code written by primary author
    pub primary_author_pct: f64,
    /// All authors with their line counts
    pub authors: Vec<(String, usize)>,
    /// Bus factor: 1 = single author, higher = better distributed
    pub bus_factor: usize,
    /// Last modified date
    pub last_modified: DateTime<Utc>,
    /// Line count
    pub line_count: usize,
}

/// A file identified as having high bus factor risk
#[derive(Debug, Clone)]
pub struct BusFactorRisk {
    pub path: String,
    pub primary_author: String,
    pub primary_author_pct: f64,
    pub line_count: usize,
    pub last_modified_days: i64,
    pub risk_reason: String,
}

/// Aggregated author statistics across the codebase
#[derive(Debug, Clone, Default)]
pub struct AuthorStats {
    /// Map of author name to their stats
    pub authors: HashMap<String, AuthorContribution>,
    /// Total unique authors
    pub total_authors: usize,
    /// Files with single author (bus factor = 1)
    pub single_author_files: usize,
    /// Average bus factor across all files
    pub avg_bus_factor: f64,
}

/// Contribution statistics for a single author
#[derive(Debug, Clone, Default)]
pub struct AuthorContribution {
    pub name: String,
    pub files_touched: usize,
    pub total_lines: usize,
    pub files_primary_author: usize,
    pub recent_commits: usize,
    pub last_active: Option<DateTime<Utc>>,
}

/// Analyzes author distribution and bus factor risk
pub struct AuthorAnalyzer {
    repo: Repository,
    ignore_dirs: Vec<String>,
}

impl AuthorAnalyzer {
    pub fn new(path: &Path) -> Result<Self> {
        let repo = Repository::discover(path).context("Failed to find git repository")?;
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
            ".codecosmos".to_string(),
        ];

        Ok(Self { repo, ignore_dirs })
    }

    /// Analyze authorship for all code files
    pub fn analyze(&self, root: &Path, days: i64) -> Result<Vec<FileAuthorship>> {
        let mut results = Vec::new();
        let cutoff = Utc::now() - Duration::days(days * 4); // Look back further for authorship

        for entry in WalkDir::new(root)
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
            if !self.is_code_file(path) {
                continue;
            }

            if let Ok(authorship) = self.analyze_file(path, root, &cutoff) {
                if authorship.line_count > 0 {
                    results.push(authorship);
                }
            }
        }

        // Sort by bus factor risk (lowest bus factor first)
        results.sort_by(|a, b| {
            a.bus_factor
                .cmp(&b.bus_factor)
                .then_with(|| b.primary_author_pct.partial_cmp(&a.primary_author_pct).unwrap())
        });

        Ok(results)
    }

    /// Analyze authorship for a single file using git blame
    fn analyze_file(
        &self,
        path: &Path,
        root: &Path,
        _cutoff: &DateTime<Utc>,
    ) -> Result<FileAuthorship> {
        let relative_path = path
            .strip_prefix(root)
            .unwrap_or(path)
            .to_string_lossy()
            .to_string();

        let mut opts = BlameOptions::new();
        opts.track_copies_same_file(true);

        let blame = self
            .repo
            .blame_file(Path::new(&relative_path), Some(&mut opts))
            .context("Failed to get blame info")?;

        let mut author_lines: HashMap<String, usize> = HashMap::new();
        let mut last_modified = DateTime::<Utc>::MIN_UTC;
        let mut total_lines = 0;

        for hunk in blame.iter() {
            let sig = hunk.final_signature();
            let author = sig
                .name()
                .unwrap_or("Unknown")
                .to_string();

            let lines_in_hunk = hunk.lines_in_hunk();
            *author_lines.entry(author).or_insert(0) += lines_in_hunk;
            total_lines += lines_in_hunk;

            // Track last modification time
            let commit_time = Utc
                .timestamp_opt(hunk.final_commit_id().is_zero().then_some(0).unwrap_or(
                    self.repo
                        .find_commit(hunk.final_commit_id())
                        .map(|c| c.time().seconds())
                        .unwrap_or(0),
                ), 0)
                .single()
                .unwrap_or_else(Utc::now);

            if commit_time > last_modified {
                last_modified = commit_time;
            }
        }

        // Sort authors by contribution
        let mut authors: Vec<(String, usize)> = author_lines.into_iter().collect();
        authors.sort_by(|a, b| b.1.cmp(&a.1));

        let (primary_author, primary_lines) = authors
            .first()
            .map(|(a, l)| (a.clone(), *l))
            .unwrap_or(("Unknown".to_string(), 0));

        let primary_author_pct = if total_lines > 0 {
            (primary_lines as f64 / total_lines as f64) * 100.0
        } else {
            0.0
        };

        // Calculate bus factor: number of authors needed to cover 80% of code
        let bus_factor = self.calculate_bus_factor(&authors, total_lines);

        Ok(FileAuthorship {
            path: relative_path,
            author_count: authors.len(),
            primary_author,
            primary_author_pct,
            authors,
            bus_factor,
            last_modified,
            line_count: total_lines,
        })
    }

    /// Calculate bus factor: minimum authors needed to cover 80% of the code
    fn calculate_bus_factor(&self, authors: &[(String, usize)], total_lines: usize) -> usize {
        if total_lines == 0 || authors.is_empty() {
            return 0;
        }

        let threshold = (total_lines as f64 * 0.8) as usize;
        let mut covered = 0usize;
        let mut count = 0usize;

        for (_, lines) in authors {
            covered += lines;
            count += 1;
            if covered >= threshold {
                break;
            }
        }

        count
    }

    /// Find files with high bus factor risk
    pub fn find_high_risk_files(
        &self,
        authorships: &[FileAuthorship],
        min_lines: usize,
    ) -> Vec<BusFactorRisk> {
        let now = Utc::now();

        authorships
            .iter()
            .filter(|a| {
                // High risk: bus factor of 1, significant file, one author owns >80%
                a.line_count >= min_lines
                    && (a.bus_factor == 1 || a.primary_author_pct >= 80.0)
            })
            .map(|a| {
                let days_ago = (now - a.last_modified).num_days();
                let risk_reason = if a.author_count == 1 {
                    "single author".to_string()
                } else if a.primary_author_pct >= 90.0 {
                    format!("{}% by one author", a.primary_author_pct.round())
                } else {
                    format!("bus factor {}", a.bus_factor)
                };

                BusFactorRisk {
                    path: a.path.clone(),
                    primary_author: a.primary_author.clone(),
                    primary_author_pct: a.primary_author_pct,
                    line_count: a.line_count,
                    last_modified_days: days_ago,
                    risk_reason,
                }
            })
            .collect()
    }

    /// Get aggregated author statistics
    pub fn aggregate_stats(&self, authorships: &[FileAuthorship], days: i64) -> Result<AuthorStats> {
        let mut stats = AuthorStats::default();
        let mut author_data: HashMap<String, AuthorContribution> = HashMap::new();

        // Count recent commits per author
        let recent_commits = self.count_recent_commits(days)?;

        for authorship in authorships {
            if authorship.bus_factor == 1 {
                stats.single_author_files += 1;
            }

            for (author, lines) in &authorship.authors {
                let entry = author_data.entry(author.clone()).or_insert_with(|| {
                    AuthorContribution {
                        name: author.clone(),
                        ..Default::default()
                    }
                });

                entry.files_touched += 1;
                entry.total_lines += lines;

                if author == &authorship.primary_author {
                    entry.files_primary_author += 1;
                }

                // Update last active time
                if entry.last_active.map_or(true, |t| authorship.last_modified > t) {
                    entry.last_active = Some(authorship.last_modified);
                }
            }
        }

        // Add recent commit counts
        for (author, count) in recent_commits {
            if let Some(entry) = author_data.get_mut(&author) {
                entry.recent_commits = count;
            }
        }

        stats.total_authors = author_data.len();
        stats.avg_bus_factor = if !authorships.is_empty() {
            authorships.iter().map(|a| a.bus_factor).sum::<usize>() as f64
                / authorships.len() as f64
        } else {
            0.0
        };

        // Sort by files touched
        let mut authors: Vec<_> = author_data.into_iter().collect();
        authors.sort_by(|a, b| b.1.files_touched.cmp(&a.1.files_touched));
        stats.authors = authors.into_iter().collect();

        Ok(stats)
    }

    /// Count commits per author in recent days
    fn count_recent_commits(&self, days: i64) -> Result<HashMap<String, usize>> {
        let mut counts: HashMap<String, usize> = HashMap::new();
        let cutoff = Utc::now() - Duration::days(days);

        let mut revwalk = self.repo.revwalk()?;
        revwalk.set_sorting(Sort::TIME)?;
        revwalk.push_head()?;

        for oid in revwalk {
            let oid = oid?;
            let commit = self.repo.find_commit(oid)?;

            let commit_time = Utc
                .timestamp_opt(commit.time().seconds(), 0)
                .single()
                .unwrap_or_else(Utc::now);

            if commit_time < cutoff {
                break;
            }

            let author = commit
                .author()
                .name()
                .unwrap_or("Unknown")
                .to_string();

            *counts.entry(author).or_insert(0) += 1;
        }

        Ok(counts)
    }

    fn should_ignore(&self, entry: &walkdir::DirEntry) -> bool {
        entry
            .file_name()
            .to_str()
            .map(|name| self.ignore_dirs.contains(&name.to_string()) || name.starts_with('.'))
            .unwrap_or(false)
    }

    fn is_code_file(&self, path: &Path) -> bool {
        let code_extensions = [
            "rs", "js", "ts", "tsx", "jsx", "py", "rb", "go", "java", "kt", "scala", "c", "cpp",
            "h", "hpp", "cs", "swift", "m", "mm", "php", "pl", "pm", "sh", "bash", "ex", "exs",
            "hs", "elm", "clj", "lua", "r", "jl", "nim", "zig", "vue", "svelte",
        ];

        path.extension()
            .and_then(|e| e.to_str())
            .map(|ext| code_extensions.contains(&ext.to_lowercase().as_str()))
            .unwrap_or(false)
    }
}

