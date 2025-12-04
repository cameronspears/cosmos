use crate::analysis::{ChurnEntry, DustyFile, TodoEntry};
use serde::{Deserialize, Serialize};

/// Letter grade for the health score
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Grade {
    A,
    B,
    C,
    D,
    F,
}

impl Grade {
    pub fn from_score(score: u8) -> Self {
        match score {
            90..=100 => Grade::A,
            75..=89 => Grade::B,
            60..=74 => Grade::C,
            40..=59 => Grade::D,
            _ => Grade::F,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Grade::A => "A",
            Grade::B => "B",
            Grade::C => "C",
            Grade::D => "D",
            Grade::F => "F",
        }
    }

    pub fn description(&self) -> &'static str {
        match self {
            Grade::A => "Excellent health",
            Grade::B => "Good shape",
            Grade::C => "Needs attention",
            Grade::D => "Significant issues",
            Grade::F => "Critical state",
        }
    }
}

impl std::fmt::Display for Grade {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

/// Trend direction compared to previous score
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Trend {
    Improving,
    Declining,
    Stable,
    Unknown,
}

impl Trend {
    #[allow(dead_code)]
    pub fn symbol(&self) -> &'static str {
        match self {
            Trend::Improving => "↑",
            Trend::Declining => "↓",
            Trend::Stable => "→",
            Trend::Unknown => "",
        }
    }

    pub fn from_delta(current: u8, previous: u8) -> Self {
        let diff = current as i16 - previous as i16;
        if diff > 2 {
            Trend::Improving
        } else if diff < -2 {
            Trend::Declining
        } else {
            Trend::Stable
        }
    }
}

/// Individual component scores that make up the overall health
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ComponentScores {
    /// Score based on file churn ratio (0-100)
    pub churn: u8,
    /// Score based on code complexity (0-100)
    pub complexity: u8,
    /// Score based on TODO/FIXME/HACK count (0-100)
    pub debt: u8,
    /// Score based on dusty file ratio (0-100)
    pub freshness: u8,
}

impl ComponentScores {
    pub fn calculate(metrics: &RepoMetrics) -> Self {
        Self {
            churn: Self::calculate_churn_score(metrics),
            complexity: Self::calculate_complexity_score(metrics),
            debt: Self::calculate_debt_score(metrics),
            freshness: Self::calculate_freshness_score(metrics),
        }
    }

    fn calculate_churn_score(metrics: &RepoMetrics) -> u8 {
        // Penalize high churn ratio (many files changed recently)
        // Ideal: <10% files changed, Bad: >40% files changed
        let churn_ratio = if metrics.total_files > 0 {
            metrics.files_changed_recently as f64 / metrics.total_files as f64
        } else {
            0.0
        };

        let score = if churn_ratio <= 0.05 {
            100.0
        } else if churn_ratio <= 0.10 {
            90.0 - (churn_ratio - 0.05) * 200.0
        } else if churn_ratio <= 0.20 {
            80.0 - (churn_ratio - 0.10) * 150.0
        } else if churn_ratio <= 0.40 {
            65.0 - (churn_ratio - 0.20) * 150.0
        } else {
            35.0 - (churn_ratio - 0.40) * 50.0
        };

        score.clamp(0.0, 100.0) as u8
    }

    fn calculate_complexity_score(metrics: &RepoMetrics) -> u8 {
        // Score based on average complexity
        // Ideal: avg complexity < 5, Bad: avg complexity > 20
        let avg_complexity = metrics.avg_complexity;

        let score = if avg_complexity <= 3.0 {
            100.0
        } else if avg_complexity <= 5.0 {
            95.0 - (avg_complexity - 3.0) * 5.0
        } else if avg_complexity <= 10.0 {
            85.0 - (avg_complexity - 5.0) * 4.0
        } else if avg_complexity <= 20.0 {
            65.0 - (avg_complexity - 10.0) * 3.0
        } else {
            35.0 - (avg_complexity - 20.0) * 1.0
        };

        score.clamp(0.0, 100.0) as u8
    }

    fn calculate_debt_score(metrics: &RepoMetrics) -> u8 {
        // Penalize TODOs/FIXMEs per 1000 lines of code
        // Ideal: <1 per 1000 LOC, Bad: >10 per 1000 LOC
        let total_debt = metrics.todo_count + metrics.fixme_count + metrics.hack_count;
        let debt_per_kloc = if metrics.total_loc > 0 {
            (total_debt as f64 / metrics.total_loc as f64) * 1000.0
        } else {
            0.0
        };

        let score = if debt_per_kloc <= 0.5 {
            100.0
        } else if debt_per_kloc <= 2.0 {
            95.0 - (debt_per_kloc - 0.5) * 10.0
        } else if debt_per_kloc <= 5.0 {
            80.0 - (debt_per_kloc - 2.0) * 8.0
        } else if debt_per_kloc <= 10.0 {
            56.0 - (debt_per_kloc - 5.0) * 6.0
        } else {
            26.0 - (debt_per_kloc - 10.0) * 2.0
        };

        score.clamp(0.0, 100.0) as u8
    }

    fn calculate_freshness_score(metrics: &RepoMetrics) -> u8 {
        // Penalize high ratio of dusty files
        // Ideal: <5% dusty, Bad: >40% dusty
        let dusty_ratio = if metrics.total_files > 0 {
            metrics.dusty_file_count as f64 / metrics.total_files as f64
        } else {
            0.0
        };

        let score = if dusty_ratio <= 0.05 {
            100.0
        } else if dusty_ratio <= 0.15 {
            95.0 - (dusty_ratio - 0.05) * 150.0
        } else if dusty_ratio <= 0.30 {
            80.0 - (dusty_ratio - 0.15) * 133.0
        } else if dusty_ratio <= 0.50 {
            60.0 - (dusty_ratio - 0.30) * 125.0
        } else {
            35.0 - (dusty_ratio - 0.50) * 50.0
        };

        score.clamp(0.0, 100.0) as u8
    }
}

/// The overall health score for a repository
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealthScore {
    /// Overall score 0-100
    pub value: u8,
    /// Letter grade
    pub grade: Grade,
    /// Individual component scores
    pub components: ComponentScores,
    /// Trend compared to previous score
    #[serde(default)]
    pub trend: Trend,
}

impl Default for Trend {
    fn default() -> Self {
        Trend::Unknown
    }
}

impl HealthScore {
    /// Calculate health score from repository metrics
    pub fn calculate(metrics: &RepoMetrics) -> Self {
        let components = ComponentScores::calculate(metrics);

        // Weighted average: churn 30%, complexity 30%, debt 20%, freshness 20%
        let weighted_score = (components.churn as f64 * 0.30)
            + (components.complexity as f64 * 0.30)
            + (components.debt as f64 * 0.20)
            + (components.freshness as f64 * 0.20);

        let value = weighted_score.round() as u8;
        let grade = Grade::from_score(value);

        Self {
            value,
            grade,
            components,
            trend: Trend::Unknown,
        }
    }

    /// Set the trend based on a previous score
    pub fn with_trend(mut self, previous_score: Option<u8>) -> Self {
        self.trend = match previous_score {
            Some(prev) => Trend::from_delta(self.value, prev),
            None => Trend::Unknown,
        };
        self
    }

    /// Get color based on score value
    pub fn color(&self) -> ratatui::style::Color {
        use ratatui::style::Color;
        match self.value {
            90..=100 => Color::Rgb(134, 239, 172), // Green
            75..=89 => Color::Rgb(163, 230, 53),   // Lime
            60..=74 => Color::Rgb(250, 204, 21),   // Yellow
            40..=59 => Color::Rgb(251, 146, 60),   // Orange
            _ => Color::Rgb(248, 113, 113),        // Red
        }
    }

    /// Format as display string like "78/100 (B)"
    #[allow(dead_code)]
    pub fn display(&self) -> String {
        let trend_str = if self.trend != Trend::Unknown {
            format!(" {}", self.trend.symbol())
        } else {
            String::new()
        };
        format!("{}/100 ({}){}", self.value, self.grade, trend_str)
    }
}

/// Metrics used to calculate the health score
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepoMetrics {
    pub total_files: usize,
    pub total_loc: usize,
    pub files_changed_recently: usize,
    pub total_commits_recent: usize,
    pub todo_count: usize,
    pub fixme_count: usize,
    pub hack_count: usize,
    pub dusty_file_count: usize,
    pub avg_complexity: f64,
    pub max_complexity: f64,
    pub danger_zone_count: usize,
}

impl RepoMetrics {
    pub fn from_analysis(
        total_files: usize,
        total_loc: usize,
        churn: &[ChurnEntry],
        todos: &[TodoEntry],
        dusty_files: &[DustyFile],
        commits_recent: usize,
        avg_complexity: f64,
        max_complexity: f64,
        danger_zone_count: usize,
    ) -> Self {
        let todo_count = todos
            .iter()
            .filter(|t| t.kind == crate::analysis::scanner::TodoKind::Todo)
            .count();
        let fixme_count = todos
            .iter()
            .filter(|t| t.kind == crate::analysis::scanner::TodoKind::Fixme)
            .count();
        let hack_count = todos
            .iter()
            .filter(|t| {
                t.kind == crate::analysis::scanner::TodoKind::Hack
                    || t.kind == crate::analysis::scanner::TodoKind::Xxx
            })
            .count();

        Self {
            total_files,
            total_loc,
            files_changed_recently: churn.len(),
            total_commits_recent: commits_recent,
            todo_count,
            fixme_count,
            hack_count,
            dusty_file_count: dusty_files.len(),
            avg_complexity,
            max_complexity,
            danger_zone_count,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_grade_from_score() {
        assert_eq!(Grade::from_score(95), Grade::A);
        assert_eq!(Grade::from_score(90), Grade::A);
        assert_eq!(Grade::from_score(89), Grade::B);
        assert_eq!(Grade::from_score(75), Grade::B);
        assert_eq!(Grade::from_score(74), Grade::C);
        assert_eq!(Grade::from_score(60), Grade::C);
        assert_eq!(Grade::from_score(59), Grade::D);
        assert_eq!(Grade::from_score(40), Grade::D);
        assert_eq!(Grade::from_score(39), Grade::F);
        assert_eq!(Grade::from_score(0), Grade::F);
    }

    #[test]
    fn test_trend_from_delta() {
        assert_eq!(Trend::from_delta(80, 70), Trend::Improving);
        assert_eq!(Trend::from_delta(70, 80), Trend::Declining);
        assert_eq!(Trend::from_delta(75, 74), Trend::Stable);
        assert_eq!(Trend::from_delta(75, 76), Trend::Stable);
    }
}

