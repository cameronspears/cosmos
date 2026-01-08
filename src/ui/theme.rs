//! Cosmos Theme - Monochromatic with cosmic elegance
//!
//! A contemplative, high-contrast palette with celestial motifs.
//! "Where code meets the cosmos"

#![allow(dead_code)]

use ratatui::style::{Color, Modifier, Style};

/// The Cosmos theme - monochromatic with meaning
pub struct Theme;

impl Theme {
    // ═══════════════════════════════════════════════════════════════════════
    //  CORE PALETTE - High contrast cosmic greys
    // ═══════════════════════════════════════════════════════════════════════

    /// Starlight white - maximum emphasis, celestial highlights
    pub const WHITE: Color = Color::Rgb(255, 255, 255);

    /// Moonlight - headers, selected items, primary focus
    pub const GREY_50: Color = Color::Rgb(252, 252, 252);

    /// Dawn grey - primary text, important content
    pub const GREY_100: Color = Color::Rgb(240, 240, 240);

    /// Twilight - secondary text, active elements
    pub const GREY_200: Color = Color::Rgb(220, 220, 220);

    /// Dusk - muted text, less important info
    pub const GREY_300: Color = Color::Rgb(190, 190, 190);

    /// Evening - subtle elements, inactive tabs
    pub const GREY_400: Color = Color::Rgb(155, 155, 155);

    /// Night - borders, separators
    pub const GREY_500: Color = Color::Rgb(120, 120, 120);

    /// Deep night - panel backgrounds, dimmer borders
    pub const GREY_600: Color = Color::Rgb(70, 70, 70);

    /// Void - overlay backgrounds
    pub const GREY_700: Color = Color::Rgb(45, 45, 45);

    /// Abyss - panel background
    pub const GREY_800: Color = Color::Rgb(28, 28, 28);

    /// Deep space - deepest background
    pub const GREY_900: Color = Color::Rgb(16, 16, 16);

    /// Background color alias
    pub const BG: Color = Self::GREY_900;

    // ─────────────────────────────────────────────────────────────────────
    // Semantic colors (still greyscale, but with meaning)
    // ─────────────────────────────────────────────────────────────────────

    /// Critical/danger indicator - pure white for maximum contrast
    pub const CRITICAL: Color = Self::WHITE;

    /// Warning indicator - bright
    pub const WARNING: Color = Self::GREY_100;

    /// Success/good indicator - medium bright
    pub const SUCCESS: Color = Self::GREY_200;

    /// Info/neutral - standard
    pub const INFO: Color = Self::GREY_300;

    // ─────────────────────────────────────────────────────────────────────
    // Accent colors for diffs and special UI
    // ─────────────────────────────────────────────────────────────────────

    /// Green for additions - brighter for contrast
    pub const GREEN: Color = Color::Rgb(130, 220, 130);

    /// Red for removals - brighter for contrast  
    pub const RED: Color = Color::Rgb(230, 120, 120);

    // ─────────────────────────────────────────────────────────────────────
    // Badge colors for categorization
    // ─────────────────────────────────────────────────────────────────────

    /// Refactor badge color
    pub const BADGE_REFACTOR: Color = Color::Rgb(180, 140, 255);  // Soft purple
    
    /// Quality badge color
    pub const BADGE_QUALITY: Color = Color::Rgb(100, 180, 255);   // Soft blue
    
    /// Security badge color
    pub const BADGE_SECURITY: Color = Color::Rgb(255, 160, 100);  // Soft orange
    
    /// Performance badge color
    pub const BADGE_PERF: Color = Color::Rgb(130, 220, 180);      // Soft teal
    
    /// Documentation badge color
    pub const BADGE_DOCS: Color = Color::Rgb(255, 200, 100);      // Soft yellow
    
    /// Bug badge color
    pub const BADGE_BUG: Color = Color::Rgb(255, 130, 130);       // Soft red

    // ─────────────────────────────────────────────────────────────────────
    // Pre-built styles for common UI elements
    // ─────────────────────────────────────────────────────────────────────

    /// Main background style
    pub fn bg() -> Style {
        Style::default().bg(Self::GREY_900)
    }

    /// Panel background style
    pub fn panel_bg() -> Style {
        Style::default().bg(Self::GREY_800)
    }

    /// Primary text style - bright for readability
    pub fn text() -> Style {
        Style::default().fg(Self::GREY_50)
    }

    /// Secondary/muted text - still readable
    pub fn text_muted() -> Style {
        Style::default().fg(Self::GREY_200)
    }

    /// Dimmed text for less important items - now more legible
    pub fn text_dim() -> Style {
        Style::default().fg(Self::GREY_300)
    }

    /// Bold emphasis
    pub fn bold() -> Style {
        Style::default()
            .fg(Self::GREY_50)
            .add_modifier(Modifier::BOLD)
    }

    /// Selected/highlighted item
    pub fn selected() -> Style {
        Style::default()
            .fg(Self::WHITE)
            .add_modifier(Modifier::BOLD)
    }

    /// Border style for panels - visible but subtle
    pub fn border() -> Style {
        Style::default().fg(Self::GREY_400)
    }

    /// Active border (focused panel) - prominent
    pub fn border_active() -> Style {
        Style::default().fg(Self::GREY_200)
    }

    /// Title style
    pub fn title() -> Style {
        Style::default()
            .fg(Self::GREY_50)
            .add_modifier(Modifier::BOLD)
    }

    /// Keybinding highlight
    pub fn key() -> Style {
        Style::default()
            .fg(Self::WHITE)
            .add_modifier(Modifier::BOLD)
    }

    /// Score color based on grade
    pub fn score_color(score: u8) -> Color {
        match score {
            90..=100 => Self::WHITE,        // Excellent - brightest
            75..=89 => Self::GREY_100,      // Good - bright
            60..=74 => Self::GREY_200,      // Okay - medium
            40..=59 => Self::GREY_300,      // Poor - dim
            _ => Self::GREY_400,            // Critical - dimmest (inverse logic: bad = less visible)
        }
    }

    /// Danger level indicators
    pub fn danger_critical() -> Style {
        Style::default()
            .fg(Self::WHITE)
            .add_modifier(Modifier::BOLD)
    }

    pub fn danger_high() -> Style {
        Style::default().fg(Self::GREY_100)
    }

    pub fn danger_medium() -> Style {
        Style::default().fg(Self::GREY_200)
    }

    /// Progress bar characters
    pub const BAR_FILLED: char = '█';
    pub const BAR_PARTIAL: char = '▓';
    pub const BAR_EMPTY: char = '░';

    /// Sparkline characters (bottom to top)
    pub const SPARK_CHARS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];

    /// Box drawing characters
    pub const BOX_HORIZONTAL: char = '─';
    pub const BOX_VERTICAL: char = '│';
    pub const BOX_TOP_LEFT: char = '┌';
    pub const BOX_TOP_RIGHT: char = '┐';
    pub const BOX_BOTTOM_LEFT: char = '└';
    pub const BOX_BOTTOM_RIGHT: char = '┘';
    pub const BOX_T_DOWN: char = '┬';
    pub const BOX_T_UP: char = '┴';
    pub const BOX_T_RIGHT: char = '├';
    pub const BOX_T_LEFT: char = '┤';
    pub const BOX_CROSS: char = '┼';

    /// Bullet/indicator characters
    pub const BULLET_FILLED: char = '●';
    pub const BULLET_EMPTY: char = '○';
    pub const BULLET_HALF: char = '◐';
    pub const ARROW_RIGHT: char = '▸';
    pub const ARROW_DOWN: char = '▾';
    pub const DOT_SEPARATOR: char = '·';

    /// Risk indicators
    pub const RISK_CRITICAL: &'static str = "▓▓";
    pub const RISK_HIGH: &'static str = "▓░";
    pub const RISK_MEDIUM: &'static str = "░░";
    pub const RISK_LOW: &'static str = "  ";

    // ─────────────────────────────────────────────────────────────────────
    // Animation characters
    // ─────────────────────────────────────────────────────────────────────

    /// Spinner frames - braille pattern (smooth)
    pub const SPINNER_BRAILLE: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

    /// Spinner frames - circular (elegant)
    pub const SPINNER_CIRCLE: [char; 4] = ['◐', '◓', '◑', '◒'];

    /// Spinner frames - dots growing
    pub const SPINNER_DOTS: [&'static str; 4] = ["·  ", "·· ", "···", "   "];

    /// Pulsing indicator frames
    pub const PULSE_FRAMES: [&'static str; 4] = ["◉ ", "◎ ", "○ ", "◎ "];

    /// Progress fill characters (fine-grained)
    pub const PROGRESS_FINE: [char; 9] = [' ', '▏', '▎', '▍', '▌', '▋', '▊', '▉', '█'];

    /// Block elements for animations
    pub const BLOCKS: [char; 4] = ['░', '▒', '▓', '█'];

    /// Success/status indicators
    pub const CHECK_MARK: char = '✓';
    pub const CROSS_MARK: char = '✗';
    pub const WARNING_MARK: char = '⚠';
    pub const INFO_MARK: char = 'ℹ';

    /// Action key hints
    pub const KEY_PROMPT: char = '▸';
    pub const KEY_HINT_OPEN: char = '⌜';
    pub const KEY_HINT_CLOSE: char = '⌟';

    /// Box drawing - rounded corners (softer look)
    pub const BOX_ROUND_TL: char = '╭';
    pub const BOX_ROUND_TR: char = '╮';
    pub const BOX_ROUND_BL: char = '╰';
    pub const BOX_ROUND_BR: char = '╯';

    /// Decorative separators
    pub const SEPARATOR_THIN: &'static str = "─";
    pub const SEPARATOR_THICK: &'static str = "━";
    pub const SEPARATOR_DOUBLE: &'static str = "═";
    pub const SEPARATOR_DOTTED: &'static str = "┄";

    /// Status badges
    pub const BADGE_OPEN: &'static str = "⟨";
    pub const BADGE_CLOSE: &'static str = "⟩";

    // ═══════════════════════════════════════════════════════════════════════
    //  COSMIC MOTIFS - Celestial symbols for Cosmos branding
    // ═══════════════════════════════════════════════════════════════════════

    /// Moon phases - for progress/state indication
    pub const MOON_NEW: char = '●';        // New moon (filled circle)
    pub const MOON_WAXING: char = '◐';     // Waxing moon
    pub const MOON_FULL: char = '○';       // Full moon (empty circle)
    pub const MOON_WANING: char = '◑';     // Waning moon
    pub const MOON_CRESCENT: char = '☽';   // Crescent moon (decorative)

    /// Cosmic decorations
    pub const CONSTELLATION: &'static str = "· · ·";
    pub const ORBIT: &'static str = "◌";

    /// Priority indicators (cosmic)
    pub const PRIORITY_HIGH: char = '●';   // Full moon - attention
    pub const PRIORITY_MED: char = '◐';    // Half moon - moderate
    pub const PRIORITY_LOW: char = '○';    // New moon - low
    pub const PRIORITY_INFO: char = '·';   // Dot - informational

    // ═══════════════════════════════════════════════════════════════════════
    //  ELEGANT BOX DRAWING - Serif-inspired borders
    // ═══════════════════════════════════════════════════════════════════════

    /// Double-line box (for headers)
    pub const DOUBLE_HORIZONTAL: char = '═';
    pub const DOUBLE_VERTICAL: char = '║';
    pub const DOUBLE_TL: char = '╔';
    pub const DOUBLE_TR: char = '╗';
    pub const DOUBLE_BL: char = '╚';
    pub const DOUBLE_BR: char = '╝';

    /// Mixed corners (elegant transition)
    pub const DOUBLE_SINGLE_TL: char = '╒';
    pub const DOUBLE_SINGLE_TR: char = '╕';
    pub const DOUBLE_SINGLE_BL: char = '╘';
    pub const DOUBLE_SINGLE_BR: char = '╛';

    // ═══════════════════════════════════════════════════════════════════════
    //  COSMOS UI STRINGS
    // ═══════════════════════════════════════════════════════════════════════

    /// The Cosmos header/branding - elegant italic
    pub const COSMOS_LOGO: &'static str = "𝘤 𝘰 𝘴 𝘮 𝘰 𝘴";
    
    pub const COSMOS_TAGLINE: &'static str = "a contemplative companion for your codebase";

    /// Section headers - elegant serif style
    pub const SECTION_PROJECT: &'static str = "𝘱𝘳𝘰𝘫𝘦𝘤𝘵";
    pub const SECTION_SUGGESTIONS: &'static str = "𝘴𝘶𝘨𝘨𝘦𝘴𝘵𝘪𝘰𝘯𝘴";
    pub const SECTION_CONTEXT: &'static str = "𝘤𝘰𝘯𝘵𝘦𝘹𝘵";

    /// Tree drawing characters
    pub const TREE_BRANCH: &'static str = "├── ";
    pub const TREE_LAST: &'static str = "└── ";
    pub const TREE_PIPE: &'static str = "│   ";
    pub const TREE_SPACE: &'static str = "    ";
    pub const TREE_FOLDER_OPEN: char = '▾';
    pub const TREE_FOLDER_CLOSED: char = '▸';
    pub const TREE_FILE: char = '·';

    // ═══════════════════════════════════════════════════════════════════════
    //  STYLE BUILDERS
    // ═══════════════════════════════════════════════════════════════════════

    /// Style for the cosmos header
    pub fn cosmos_header() -> Style {
        Style::default()
            .fg(Self::WHITE)
            .add_modifier(Modifier::BOLD)
    }

    /// Style for suggestions based on priority
    pub fn suggestion_style(priority: char) -> Style {
        match priority {
            '●' => Style::default().fg(Self::WHITE).add_modifier(Modifier::BOLD),
            '◐' => Style::default().fg(Self::GREY_200),
            '○' => Style::default().fg(Self::GREY_400),
            _ => Style::default().fg(Self::GREY_500),
        }
    }

    /// Style for file tree items
    pub fn tree_item(is_dir: bool, has_suggestions: bool) -> Style {
        if has_suggestions {
            Style::default().fg(Self::GREY_100)
        } else if is_dir {
            Style::default().fg(Self::GREY_300)
        } else {
            Style::default().fg(Self::GREY_400)
        }
    }

    /// Style for selected tree item
    pub fn tree_selected() -> Style {
        Style::default()
            .fg(Self::WHITE)
            .add_modifier(Modifier::BOLD)
    }

    /// Style for the status bar
    pub fn status_bar() -> Style {
        Style::default()
            .fg(Self::GREY_400)
            .bg(Self::GREY_800)
    }

    /// Style for key hints - visible enough to read
    pub fn key_hint() -> Style {
        Style::default().fg(Self::GREY_300)
    }

    /// Style for key highlight - prominent
    pub fn key_highlight() -> Style {
        Style::default().fg(Self::GREY_100)
    }
}

// ═══════════════════════════════════════════════════════════════════════════
//  COSMIC UTILITIES
// ═══════════════════════════════════════════════════════════════════════════

/// Generate a constellation line (decorative separator)
pub fn constellation_line(width: usize) -> String {
    let pattern = "· · · ";
    let repeat = width / pattern.len() + 1;
    pattern.repeat(repeat)[..width].to_string()
}

/// Generate dot rating (e.g., ●●●○○)
pub fn dot_rating(filled: usize, total: usize) -> String {
    let mut result = String::new();
    for i in 0..total {
        if i < filled {
            result.push(Theme::BULLET_FILLED);
        } else {
            result.push(Theme::BULLET_EMPTY);
        }
    }
    result
}

/// Moon phase based on progress (0.0 to 1.0)
pub fn moon_phase(progress: f64) -> char {
    match (progress * 4.0) as usize {
        0 => Theme::MOON_NEW,
        1 => Theme::MOON_WAXING,
        2 => Theme::MOON_FULL,
        3 => Theme::MOON_WANING,
        _ => Theme::MOON_FULL,
    }
}

/// Generate a sparkline string from a series of values
pub fn sparkline(values: &[u8], width: usize) -> String {
    if values.is_empty() {
        return " ".repeat(width);
    }

    let min = *values.iter().min().unwrap_or(&0) as f64;
    let max = *values.iter().max().unwrap_or(&100) as f64;
    let range = (max - min).max(1.0);

    // Take the last `width` values, or pad with spaces if fewer
    let start = values.len().saturating_sub(width);
    let relevant = &values[start..];

    let mut result = String::new();

    // Pad with spaces if we don't have enough values
    for _ in 0..(width.saturating_sub(relevant.len())) {
        result.push(' ');
    }

    for &val in relevant {
        let normalized = ((val as f64 - min) / range * 7.0).round() as usize;
        let idx = normalized.min(7);
        result.push(Theme::SPARK_CHARS[idx]);
    }

    result
}

/// Generate a horizontal bar gauge
pub fn bar_gauge(value: u8, width: usize) -> String {
    let filled = (value as usize * width) / 100;
    let mut result = String::new();

    for i in 0..width {
        if i < filled {
            result.push(Theme::BAR_FILLED);
        } else {
            result.push(Theme::BAR_EMPTY);
        }
    }

    result
}

/// Generate a dot gauge (●○○○○)
pub fn dot_gauge(value: u8, max_dots: usize) -> String {
    let filled = ((value as usize * max_dots) + 50) / 100; // Round to nearest
    let mut result = String::new();

    for i in 0..max_dots {
        if i < filled {
            result.push(Theme::BULLET_FILLED);
        } else {
            result.push(Theme::BULLET_EMPTY);
        }
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sparkline() {
        let values = vec![50, 60, 70, 80, 90, 100, 90, 80];
        let spark = sparkline(&values, 8);
        assert_eq!(spark.chars().count(), 8);
    }

    #[test]
    fn test_bar_gauge() {
        let bar = bar_gauge(50, 10);
        assert_eq!(bar.chars().count(), 10);
    }

    #[test]
    fn test_dot_gauge() {
        let dots = dot_gauge(80, 5);
        assert_eq!(dots.chars().count(), 5);
    }
}
