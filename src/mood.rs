use crate::analysis::{ChurnEntry, DustyFile, TodoEntry};

/// The overall mood/vibe of the repository
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mood {
    Calm,
    Chaotic,
    Stale,
    RefactorFrenzy,
}

impl Mood {
    pub fn symbol(&self) -> &'static str {
        match self {
            Mood::Calm => "◉",
            Mood::Chaotic => "⚡",
            Mood::Stale => "◎",
            Mood::RefactorFrenzy => "🔄",
        }
    }

    pub fn name(&self) -> &'static str {
        match self {
            Mood::Calm => "CALM",
            Mood::Chaotic => "CHAOTIC",
            Mood::Stale => "STALE",
            Mood::RefactorFrenzy => "REFACTOR FRENZY",
        }
    }

    pub fn tagline(&self) -> &'static str {
        match self {
            Mood::Calm => "Steady progress, no fires",
            Mood::Chaotic => "Lots of churn, stay focused",
            Mood::Stale => "Cobwebs gathering, time to revisit",
            Mood::RefactorFrenzy => "Heavy restructuring in progress",
        }
    }

    pub fn color(&self) -> ratatui::style::Color {
        use ratatui::style::Color;
        match self {
            Mood::Calm => Color::Rgb(134, 239, 172),      // Soft green
            Mood::Chaotic => Color::Rgb(251, 146, 60),    // Orange
            Mood::Stale => Color::Rgb(148, 163, 184),     // Slate gray
            Mood::RefactorFrenzy => Color::Rgb(251, 191, 36), // Amber
        }
    }
}

/// Metrics used to calculate mood
#[derive(Debug, Clone)]
pub struct RepoMetrics {
    pub total_files: usize,
    pub files_changed_recently: usize,
    pub total_commits_recent: usize,
    pub todo_count: usize,
    pub fixme_count: usize,
    pub hack_count: usize,
    pub dusty_file_count: usize,
    pub add_delete_ratio: f64,
    pub churn_concentration: f64, // How concentrated changes are (0-1)
}

impl RepoMetrics {
    pub fn from_analysis(
        total_files: usize,
        churn: &[ChurnEntry],
        todos: &[TodoEntry],
        dusty_files: &[DustyFile],
        commits_recent: usize,
        add_delete_ratio: f64,
    ) -> Self {
        let todo_count = todos.iter().filter(|t| t.kind == crate::analysis::scanner::TodoKind::Todo).count();
        let fixme_count = todos.iter().filter(|t| t.kind == crate::analysis::scanner::TodoKind::Fixme).count();
        let hack_count = todos.iter().filter(|t| t.kind == crate::analysis::scanner::TodoKind::Hack).count();

        // Calculate churn concentration (how much of the churn is in top files)
        let total_changes: usize = churn.iter().map(|c| c.change_count).sum();
        let top_5_changes: usize = churn.iter().take(5).map(|c| c.change_count).sum();
        let churn_concentration = if total_changes > 0 {
            top_5_changes as f64 / total_changes as f64
        } else {
            0.0
        };

        Self {
            total_files,
            files_changed_recently: churn.len(),
            total_commits_recent: commits_recent,
            todo_count,
            fixme_count,
            hack_count,
            dusty_file_count: dusty_files.len(),
            add_delete_ratio,
            churn_concentration,
        }
    }
}

/// Engine for calculating repository mood
pub struct MoodEngine;

impl MoodEngine {
    /// Calculate the mood based on repository metrics
    pub fn calculate(metrics: &RepoMetrics) -> Mood {
        let mut scores = MoodScores::default();

        // High churn suggests chaos
        let churn_ratio = if metrics.total_files > 0 {
            metrics.files_changed_recently as f64 / metrics.total_files as f64
        } else {
            0.0
        };

        if churn_ratio > 0.3 {
            scores.chaotic += 3;
        } else if churn_ratio > 0.15 {
            scores.chaotic += 1;
        }

        // Many TODOs and FIXMEs suggest chaos
        let debt_count = metrics.todo_count + metrics.fixme_count + metrics.hack_count;
        if debt_count > 20 {
            scores.chaotic += 2;
        } else if debt_count > 10 {
            scores.chaotic += 1;
        }

        // High concentration of changes in few files suggests refactor frenzy
        if metrics.churn_concentration > 0.7 && metrics.files_changed_recently >= 3 {
            scores.refactor_frenzy += 3;
        } else if metrics.churn_concentration > 0.5 {
            scores.refactor_frenzy += 1;
        }

        // High delete ratio suggests refactoring
        if metrics.add_delete_ratio > 0.8 {
            scores.refactor_frenzy += 2;
        } else if metrics.add_delete_ratio > 0.5 {
            scores.refactor_frenzy += 1;
        }

        // Many dusty files suggests staleness
        let dusty_ratio = if metrics.total_files > 0 {
            metrics.dusty_file_count as f64 / metrics.total_files as f64
        } else {
            0.0
        };

        if dusty_ratio > 0.5 {
            scores.stale += 3;
        } else if dusty_ratio > 0.25 {
            scores.stale += 2;
        }

        // Low recent activity suggests staleness
        if metrics.total_commits_recent < 3 && metrics.total_files > 10 {
            scores.stale += 2;
        }

        // Calm indicators
        if churn_ratio < 0.15 && churn_ratio > 0.02 {
            scores.calm += 1;
        }
        if debt_count < 10 {
            scores.calm += 1;
        }
        if metrics.churn_concentration < 0.5 {
            scores.calm += 1;
        }

        scores.determine_mood()
    }
}

#[derive(Default)]
struct MoodScores {
    calm: i32,
    chaotic: i32,
    stale: i32,
    refactor_frenzy: i32,
}

impl MoodScores {
    fn determine_mood(&self) -> Mood {
        let max_score = self.calm.max(self.chaotic).max(self.stale).max(self.refactor_frenzy);
        
        // If no clear signal, default to calm
        if max_score == 0 {
            return Mood::Calm;
        }

        // Pick the dominant mood
        if self.chaotic == max_score {
            Mood::Chaotic
        } else if self.refactor_frenzy == max_score {
            Mood::RefactorFrenzy
        } else if self.stale == max_score {
            Mood::Stale
        } else {
            Mood::Calm
        }
    }
}


