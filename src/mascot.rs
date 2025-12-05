//! ASCII mascot that reacts to codebase health
//!
//! Meet Cosmo - a little astronaut who explores your codebase!
//! Their expression changes based on the health score.

/// The mascot frames for different health states
pub struct Mascot;

impl Mascot {
    /// Excellent health (90-100) - Celebrating! 🎉
    pub const EXCELLENT: &'static str = r#"
    ╭─────╮
    │ ◠ ◠ │  ★
    │  ◡  │ /
    ╰──┬──╯
      ╱│╲  
     ╱ │ ╲ "#;

    /// Good health (75-89) - Happy and confident
    pub const GOOD: &'static str = r#"
    ╭─────╮
    │ ◠ ◠ │
    │  ◡  │
    ╰──┬──╯
       │   
      ╱ ╲  "#;

    /// Okay health (60-74) - Slight concern
    pub const OKAY: &'static str = r#"
    ╭─────╮
    │ • • │
    │  ─  │
    ╰──┬──╯
       │   
      ╱ ╲  "#;

    /// Poor health (40-59) - Worried
    pub const POOR: &'static str = r#"
    ╭─────╮
    │ • • │  ?
    │  ︵ │ /
    ╰──┬──╯
       │   
      ╱ ╲  "#;

    /// Critical health (0-39) - Panicked!
    pub const CRITICAL: &'static str = r#"
    ╭─────╮
    │ ◉ ◉ │ !
    │  ○  │/
    ╰──┬──╯
      \│/  
      ╱ ╲  "#;

    /// Get mascot for a given score
    pub fn for_score(score: u8) -> &'static str {
        match score {
            90..=100 => Self::EXCELLENT,
            75..=89 => Self::GOOD,
            60..=74 => Self::OKAY,
            40..=59 => Self::POOR,
            _ => Self::CRITICAL,
        }
    }

    /// Get a witty comment based on score
    pub fn comment(score: u8) -> &'static str {
        match score {
            95..=100 => "This codebase sparks joy! ✨",
            90..=94 => "Ship it! This code is ready.",
            85..=89 => "Looking good! Minor polish needed.",
            80..=84 => "Solid foundation here.",
            75..=79 => "Good shape, some rough edges.",
            70..=74 => "Needs some love, but not urgent.",
            65..=69 => "Technical debt is accumulating...",
            60..=64 => "Time for a refactoring sprint.",
            55..=59 => "Warning signs everywhere.",
            50..=54 => "This code has seen things.",
            45..=49 => "Here be dragons. 🐉",
            40..=44 => "Abandon hope, all ye who enter.",
            35..=39 => "It's not a bug, it's a haunting.",
            30..=34 => "The code is held together by prayers.",
            25..=29 => "Legacy code? This is archaeology.",
            20..=24 => "Who hurt this codebase?",
            15..=19 => "Send help. Immediately.",
            10..=14 => "This is a cry for help.",
            5..=9 => "rm -rf might be the answer.",
            _ => "How is this even running?!",
        }
    }

    /// Get a longer description for the grade
    pub fn grade_description(score: u8) -> &'static str {
        match score {
            90..=100 => "Exceptional codebase health. Your future self will thank you.",
            75..=89 => "Well-maintained code. Keep up the good work!",
            60..=74 => "Acceptable but showing wear. Schedule some maintenance.",
            40..=59 => "Significant issues detected. Prioritize improvements.",
            _ => "Critical state. Major intervention required.",
        }
    }

    /// Get an emoji for the score
    pub fn emoji(score: u8) -> &'static str {
        match score {
            90..=100 => "🌟",
            80..=89 => "✨",
            70..=79 => "👍",
            60..=69 => "😐",
            50..=59 => "😰",
            40..=49 => "😱",
            30..=39 => "💀",
            20..=29 => "🔥",
            10..=19 => "☠️",
            _ => "🆘",
        }
    }
}

/// Generate a mini heat map tree showing file health
pub fn heat_map_tree(
    danger_zones: &[(String, f64)],  // (path, danger_score)
    hotspots: &[(String, usize)],     // (path, change_count)
    max_items: usize,
) -> Vec<String> {
    let mut lines = Vec::new();
    lines.push("╭─ Danger Map ──────────────────────╮".to_string());
    
    // Combine and sort by danger
    let mut items: Vec<(&str, &str, f64)> = Vec::new();
    
    for (path, score) in danger_zones.iter().take(max_items) {
        let intensity = if *score >= 80.0 {
            "▓▓▓"
        } else if *score >= 60.0 {
            "▓▓░"
        } else if *score >= 40.0 {
            "▓░░"
        } else {
            "░░░"
        };
        items.push((path, intensity, *score));
    }
    
    for (path, intensity, _) in items.iter().take(5) {
        let display_path = if path.len() > 28 {
            format!("...{}", &path[path.len()-25..])
        } else {
            path.to_string()
        };
        lines.push(format!("│ {} {:28} │", intensity, display_path));
    }
    
    if items.is_empty() {
        lines.push("│  No danger zones! 🎉           │".to_string());
    }
    
    lines.push("╰───────────────────────────────────╯".to_string());
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_mascot_selection() {
        assert!(Mascot::for_score(95).contains("◠ ◠"));
        assert!(Mascot::for_score(80).contains("◠ ◠"));
        assert!(Mascot::for_score(65).contains("• •"));
        assert!(Mascot::for_score(45).contains("︵"));
        assert!(Mascot::for_score(20).contains("◉ ◉"));
    }

    #[test]
    fn test_comments_exist() {
        // Just verify we get non-empty strings for all ranges
        for score in (0..=100).step_by(5) {
            assert!(!Mascot::comment(score).is_empty());
        }
    }
}

