//! Cosmos Theme - Monochromatic with cosmic elegance
//!
//! A contemplative, high-contrast palette with celestial motifs.
//! "Where code meets the cosmos"

use ratatui::style::Color;

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
    // Accent colors for diffs and special UI
    // ─────────────────────────────────────────────────────────────────────

    /// Green for additions - brighter for contrast
    pub const GREEN: Color = Color::Rgb(130, 220, 130);

    /// Red for removals - brighter for contrast  
    pub const RED: Color = Color::Rgb(230, 120, 120);

    /// Yellow/orange for warnings - visible but not alarming
    pub const YELLOW: Color = Color::Rgb(255, 200, 100);

    /// Accent color for highlighting selections
    pub const ACCENT: Color = Color::Rgb(140, 180, 255);

    // ─────────────────────────────────────────────────────────────────────
    // Badge colors for categorization (monochromatic)
    // ─────────────────────────────────────────────────────────────────────

    /// Refactor badge color
    pub const BADGE_REFACTOR: Color = Self::GREY_200;

    /// Quality badge color
    pub const BADGE_QUALITY: Color = Self::GREY_300;

    /// Security badge color
    pub const BADGE_SECURITY: Color = Self::GREY_100;

    /// Performance badge color
    pub const BADGE_PERF: Color = Self::GREY_300;

    /// Documentation badge color
    pub const BADGE_DOCS: Color = Self::GREY_400;

    /// Bug badge color
    pub const BADGE_BUG: Color = Self::GREY_100;

    // ═══════════════════════════════════════════════════════════════════════
    //  COSMIC MOTIFS - Celestial symbols for Cosmos branding
    // ═══════════════════════════════════════════════════════════════════════

    /// Priority indicators (cosmic)
    pub const PRIORITY_HIGH: char = '●'; // Full moon - attention

    // ═══════════════════════════════════════════════════════════════════════
    //  COSMOS UI STRINGS
    // ═══════════════════════════════════════════════════════════════════════

    /// The Cosmos header/branding - elegant italic
    pub const COSMOS_LOGO: &'static str = "𝘤 𝘰 𝘴 𝘮 𝘰 𝘴";

    /// Workflow step labels - italic style
    pub const WORKFLOW_SUGGESTIONS: &'static str = "𝘴𝘶𝘨𝘨𝘦𝘴𝘵𝘪𝘰𝘯𝘴";
    pub const WORKFLOW_REVIEW: &'static str = "𝘳𝘦𝘷𝘪𝘦𝘸";
    pub const WORKFLOW_SHIP: &'static str = "𝘴𝘩𝘪𝘱";
}
