use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

use crate::score::{ComponentScores, HealthScore, Trend};

/// A single historical score entry
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryEntry {
    pub timestamp: DateTime<Utc>,
    pub score: u8,
    pub components: ComponentScores,
    #[serde(default)]
    pub branch: Option<String>,
}

/// History of health scores for a repository
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ScoreHistory {
    pub entries: Vec<HistoryEntry>,
}

#[allow(dead_code)]
impl ScoreHistory {
    /// Load history from the repository's .codecosmos directory
    pub fn load(repo_path: &Path) -> Result<Self> {
        let history_path = Self::history_file_path(repo_path);

        if !history_path.exists() {
            return Ok(Self::default());
        }

        let content =
            fs::read_to_string(&history_path).context("Failed to read history file")?;

        let history: Self =
            serde_json::from_str(&content).context("Failed to parse history file")?;

        Ok(history)
    }

    /// Save history to the repository's .codecosmos directory
    pub fn save(&self, repo_path: &Path) -> Result<()> {
        let history_path = Self::history_file_path(repo_path);

        // Create .codecosmos directory if it doesn't exist
        if let Some(parent) = history_path.parent() {
            fs::create_dir_all(parent).context("Failed to create .codecosmos directory")?;
        }

        let content =
            serde_json::to_string_pretty(&self).context("Failed to serialize history")?;

        fs::write(&history_path, content).context("Failed to write history file")?;

        Ok(())
    }

    /// Add a new score entry to history
    pub fn add_entry(&mut self, score: &HealthScore, branch: Option<String>) {
        let entry = HistoryEntry {
            timestamp: Utc::now(),
            score: score.value,
            components: score.components.clone(),
            branch,
        };

        self.entries.push(entry);

        // Keep only the last 100 entries to avoid unbounded growth
        if self.entries.len() > 100 {
            self.entries.drain(0..self.entries.len() - 100);
        }
    }

    /// Get the most recent score (if any)
    pub fn latest_score(&self) -> Option<u8> {
        self.entries.last().map(|e| e.score)
    }

    /// Get the previous score (second to last, if any)
    pub fn previous_score(&self) -> Option<u8> {
        if self.entries.len() >= 2 {
            Some(self.entries[self.entries.len() - 2].score)
        } else {
            None
        }
    }

    /// Calculate trend based on recent scores
    pub fn calculate_trend(&self) -> Trend {
        if self.entries.len() < 2 {
            return Trend::Unknown;
        }

        let current = self.entries.last().unwrap().score;
        let previous = self.entries[self.entries.len() - 2].score;

        Trend::from_delta(current, previous)
    }

    /// Get average score over the last N entries
    pub fn average_score(&self, last_n: usize) -> Option<f64> {
        if self.entries.is_empty() {
            return None;
        }

        let count = self.entries.len().min(last_n);
        let sum: u32 = self.entries[self.entries.len() - count..]
            .iter()
            .map(|e| e.score as u32)
            .sum();

        Some(sum as f64 / count as f64)
    }

    /// Get score change from N entries ago
    pub fn score_change(&self, entries_ago: usize) -> Option<i16> {
        if self.entries.len() <= entries_ago {
            return None;
        }

        let current = self.entries.last()?.score as i16;
        let past = self.entries[self.entries.len() - 1 - entries_ago].score as i16;

        Some(current - past)
    }

    /// Get recent entries (up to last_n)
    pub fn recent_entries(&self, last_n: usize) -> &[HistoryEntry] {
        let start = self.entries.len().saturating_sub(last_n);
        &self.entries[start..]
    }

    fn history_file_path(repo_path: &Path) -> PathBuf {
        repo_path.join(".codecosmos").join("history.json")
    }
}

/// Ensures .codecosmos is in .gitignore (optional convenience)
#[allow(dead_code)]
pub fn ensure_gitignore(repo_path: &Path) -> Result<()> {
    let gitignore_path = repo_path.join(".gitignore");
    let codecosmos_entry = ".codecosmos/";

    if gitignore_path.exists() {
        let content = fs::read_to_string(&gitignore_path)?;
        if content.contains(codecosmos_entry) || content.contains(".codecosmos") {
            return Ok(()); // Already ignored
        }

        // Append to existing .gitignore
        let mut new_content = content;
        if !new_content.ends_with('\n') {
            new_content.push('\n');
        }
        new_content.push_str("\n# codecosmos history\n");
        new_content.push_str(codecosmos_entry);
        new_content.push('\n');

        fs::write(&gitignore_path, new_content)?;
    } else {
        // Create new .gitignore
        let content = format!("# codecosmos history\n{}\n", codecosmos_entry);
        fs::write(&gitignore_path, content)?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::score::Grade;

    fn make_test_score(value: u8) -> HealthScore {
        HealthScore {
            value,
            grade: Grade::from_score(value),
            components: ComponentScores {
                churn: 80,
                complexity: 75,
                debt: 90,
                freshness: 85,
            },
            trend: Trend::Unknown,
        }
    }

    #[test]
    fn test_trend_calculation() {
        let mut history = ScoreHistory::default();

        // No entries = unknown trend
        assert_eq!(history.calculate_trend(), Trend::Unknown);

        // One entry = unknown trend
        history.add_entry(&make_test_score(75), None);
        assert_eq!(history.calculate_trend(), Trend::Unknown);

        // Two entries, improving
        history.add_entry(&make_test_score(85), None);
        assert_eq!(history.calculate_trend(), Trend::Improving);

        // Add declining entry
        history.add_entry(&make_test_score(70), None);
        assert_eq!(history.calculate_trend(), Trend::Declining);

        // Add stable entry
        history.add_entry(&make_test_score(71), None);
        assert_eq!(history.calculate_trend(), Trend::Stable);
    }

    #[test]
    fn test_average_score() {
        let mut history = ScoreHistory::default();

        assert_eq!(history.average_score(5), None);

        history.add_entry(&make_test_score(80), None);
        history.add_entry(&make_test_score(70), None);
        history.add_entry(&make_test_score(90), None);

        assert_eq!(history.average_score(3), Some(80.0));
        assert_eq!(history.average_score(2), Some(80.0)); // (70+90)/2
    }
}


