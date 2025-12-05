//! Animated spinner and progress indicators for the analysis phase
//!
//! Beautiful unicode animations to make the analysis feel polished.

use crossterm::{
    cursor::{Hide, MoveToColumn, Show},
    execute,
    style::{Color, Print, ResetColor, SetForegroundColor},
    terminal::{Clear, ClearType},
};
use std::io::{self, Write};
use std::time::{Duration, Instant};

/// Spinner animation frames - braille pattern spinner
pub const SPINNER_BRAILLE: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

/// Spinner animation frames - dots
pub const SPINNER_DOTS: [&str; 4] = ["⠋", "⠙", "⠹", "⠼"];

/// Spinner animation frames - bouncing bar
pub const SPINNER_BOUNCE: [&str; 6] = ["▐⠀⠀▌", "▐⠈⠀▌", "▐⠈⠁▌", "▐⠀⠁▌", "▐⠀⠀▌", "▐⠀⠀▌"];

/// Spinner animation frames - moon phases
pub const SPINNER_MOON: [char; 8] = ['🌑', '🌒', '🌓', '🌔', '🌕', '🌖', '🌗', '🌘'];

/// Spinner animation frames - circular
pub const SPINNER_CIRCLE: [char; 4] = ['◐', '◓', '◑', '◒'];

/// Spinner animation frames - elegant dots
pub const SPINNER_ELEGANT: [&str; 8] = ["·  ", "·· ", "···", " ··", "  ·", "   ", "   ", "·  "];

/// Progress bar characters
pub const PROGRESS_CHARS: [char; 9] = [' ', '▏', '▎', '▍', '▌', '▋', '▊', '▉', '█'];

/// A beautiful animated spinner for console output
pub struct Spinner {
    frames: Vec<String>,
    current_frame: usize,
    message: String,
    last_update: Instant,
    frame_duration: Duration,
    style: SpinnerStyle,
}

#[derive(Clone, Copy)]
pub enum SpinnerStyle {
    Braille,
    Circle,
    Moon,
    Elegant,
    Dots,
}

impl Spinner {
    pub fn new(style: SpinnerStyle) -> Self {
        let frames: Vec<String> = match style {
            SpinnerStyle::Braille => SPINNER_BRAILLE.iter().map(|c| c.to_string()).collect(),
            SpinnerStyle::Circle => SPINNER_CIRCLE.iter().map(|c| c.to_string()).collect(),
            SpinnerStyle::Moon => SPINNER_MOON.iter().map(|c| c.to_string()).collect(),
            SpinnerStyle::Elegant => SPINNER_ELEGANT.iter().map(|s| s.to_string()).collect(),
            SpinnerStyle::Dots => SPINNER_DOTS.iter().map(|s| s.to_string()).collect(),
        };

        Self {
            frames,
            current_frame: 0,
            message: String::new(),
            last_update: Instant::now(),
            frame_duration: Duration::from_millis(80),
            style,
        }
    }

    pub fn with_message(mut self, msg: &str) -> Self {
        self.message = msg.to_string();
        self
    }

    /// Start the spinner (hides cursor)
    pub fn start(&self) {
        let _ = execute!(io::stderr(), Hide);
    }

    /// Stop the spinner (shows cursor, clears line)
    pub fn stop(&self) {
        let _ = execute!(
            io::stderr(),
            MoveToColumn(0),
            Clear(ClearType::CurrentLine),
            Show
        );
    }

    /// Update the spinner animation (call in a loop)
    pub fn tick(&mut self) {
        if self.last_update.elapsed() >= self.frame_duration {
            self.current_frame = (self.current_frame + 1) % self.frames.len();
            self.last_update = Instant::now();
            self.render();
        }
    }

    /// Set a new message
    pub fn set_message(&mut self, msg: &str) {
        self.message = msg.to_string();
        self.render();
    }

    fn render(&self) {
        let frame = &self.frames[self.current_frame];
        let _ = execute!(
            io::stderr(),
            MoveToColumn(0),
            Clear(ClearType::CurrentLine),
            SetForegroundColor(Color::Rgb { r: 140, g: 140, b: 140 }),
            Print(format!("  {} ", frame)),
            SetForegroundColor(Color::Rgb { r: 180, g: 180, b: 180 }),
            Print(&self.message),
            ResetColor
        );
        let _ = io::stderr().flush();
    }

    /// Finish with a success message
    pub fn finish_with_message(&self, msg: &str) {
        let _ = execute!(
            io::stderr(),
            MoveToColumn(0),
            Clear(ClearType::CurrentLine),
            SetForegroundColor(Color::Rgb { r: 140, g: 140, b: 140 }),
            Print("  ✓ "),
            SetForegroundColor(Color::Rgb { r: 180, g: 180, b: 180 }),
            Print(msg),
            ResetColor,
            Print("\n")
        );
    }
}

/// A progress bar for showing analysis progress
pub struct ProgressBar {
    total: usize,
    current: usize,
    width: usize,
    message: String,
    last_render: Instant,
}

impl ProgressBar {
    pub fn new(total: usize) -> Self {
        Self {
            total,
            current: 0,
            width: 30,
            message: String::new(),
            last_render: Instant::now(),
        }
    }

    pub fn with_message(mut self, msg: &str) -> Self {
        self.message = msg.to_string();
        self
    }

    /// Start the progress bar
    pub fn start(&self) {
        let _ = execute!(io::stderr(), Hide);
    }

    /// Increment progress
    pub fn inc(&mut self) {
        self.current += 1;
        // Throttle rendering to avoid flicker
        if self.last_render.elapsed() >= Duration::from_millis(50) {
            self.render();
            self.last_render = Instant::now();
        }
    }

    /// Set current progress
    pub fn set(&mut self, current: usize) {
        self.current = current;
        self.render();
    }

    /// Set message
    pub fn set_message(&mut self, msg: &str) {
        self.message = msg.to_string();
        self.render();
    }

    fn render(&self) {
        let pct = if self.total > 0 {
            (self.current as f64 / self.total as f64).min(1.0)
        } else {
            0.0
        };

        let filled_width = (pct * self.width as f64) as usize;
        let partial_idx = ((pct * self.width as f64).fract() * 8.0) as usize;

        let mut bar = String::new();
        for i in 0..self.width {
            if i < filled_width {
                bar.push('█');
            } else if i == filled_width && partial_idx > 0 {
                bar.push(PROGRESS_CHARS[partial_idx]);
            } else {
                bar.push('░');
            }
        }

        let pct_str = format!("{:3.0}%", pct * 100.0);

        let _ = execute!(
            io::stderr(),
            MoveToColumn(0),
            Clear(ClearType::CurrentLine),
            SetForegroundColor(Color::Rgb { r: 100, g: 100, b: 100 }),
            Print("  "),
            SetForegroundColor(Color::Rgb { r: 140, g: 140, b: 140 }),
            Print(&bar),
            Print(" "),
            SetForegroundColor(Color::Rgb { r: 180, g: 180, b: 180 }),
            Print(&pct_str),
            Print("  "),
            SetForegroundColor(Color::Rgb { r: 120, g: 120, b: 120 }),
            Print(&self.message),
            ResetColor
        );
        let _ = io::stderr().flush();
    }

    /// Finish the progress bar
    pub fn finish(&self) {
        let _ = execute!(
            io::stderr(),
            MoveToColumn(0),
            Clear(ClearType::CurrentLine),
            Show
        );
    }

    /// Finish with a message
    pub fn finish_with_message(&self, msg: &str) {
        let _ = execute!(
            io::stderr(),
            MoveToColumn(0),
            Clear(ClearType::CurrentLine),
            SetForegroundColor(Color::Rgb { r: 140, g: 140, b: 140 }),
            Print("  ✓ "),
            SetForegroundColor(Color::Rgb { r: 180, g: 180, b: 180 }),
            Print(msg),
            ResetColor,
            Print("\n"),
            Show
        );
    }
}

/// Animated score reveal - counts up to the final score
pub struct ScoreReveal {
    target: u8,
    current: f64,
    duration: Duration,
    start_time: Option<Instant>,
}

impl ScoreReveal {
    pub fn new(target: u8, duration_ms: u64) -> Self {
        Self {
            target,
            current: 0.0,
            duration: Duration::from_millis(duration_ms),
            start_time: None,
        }
    }

    /// Start the animation
    pub fn start(&mut self) {
        self.start_time = Some(Instant::now());
    }

    /// Get current value (call in render loop)
    pub fn value(&mut self) -> u8 {
        match self.start_time {
            Some(start) => {
                let elapsed = start.elapsed();
                if elapsed >= self.duration {
                    self.target
                } else {
                    let progress = elapsed.as_secs_f64() / self.duration.as_secs_f64();
                    // Ease-out cubic for satisfying feel
                    let eased = 1.0 - (1.0 - progress).powi(3);
                    (eased * self.target as f64) as u8
                }
            }
            None => 0,
        }
    }

    /// Check if animation is complete
    pub fn is_complete(&self) -> bool {
        match self.start_time {
            Some(start) => start.elapsed() >= self.duration,
            None => false,
        }
    }
}

/// Print a styled header for the analysis phase
pub fn print_analysis_header(repo_name: &str) {
    // Calculate width based on content - minimum 30, adapts to repo name
    let title = "codecosmos";
    let scanning_prefix = "scanning: ";
    let display_name = truncate_str(repo_name, 40);
    let content_width = (title.len()).max(scanning_prefix.len() + display_name.len()) + 4; // +4 for padding
    let box_width = content_width.max(30);
    
    // Build the box dynamically
    let top_border = format!("  ╭{}╮\n", "─".repeat(box_width));
    let bottom_border = format!("  ╰{}╯\n", "─".repeat(box_width));
    
    // Title line: "  codecosmos" padded to box width
    let title_padding = box_width - title.len() - 2; // -2 for "  " prefix
    let title_line = format!("  {}{}", title, " ".repeat(title_padding));
    
    // Scanning line: "  scanning: repo-name" padded to box width  
    let scan_content = format!("  {}{}", scanning_prefix, display_name);
    let scan_padding = box_width - scan_content.len();
    let scan_line = format!("{}{}", scan_content, " ".repeat(scan_padding));

    let _ = execute!(
        io::stderr(),
        Print("\n"),
        SetForegroundColor(Color::Rgb { r: 100, g: 100, b: 100 }),
        Print(&top_border),
        Print("  │"),
        SetForegroundColor(Color::Rgb { r: 180, g: 180, b: 180 }),
        Print(&title_line),
        SetForegroundColor(Color::Rgb { r: 100, g: 100, b: 100 }),
        Print("│\n"),
        Print("  │"),
        SetForegroundColor(Color::Rgb { r: 140, g: 140, b: 140 }),
        Print(&scan_line),
        SetForegroundColor(Color::Rgb { r: 100, g: 100, b: 100 }),
        Print("│\n"),
        Print(&bottom_border),
        ResetColor,
        Print("\n")
    );
}

/// Print a completion message with the score
pub fn print_analysis_complete(score: u8, grade: &str) {
    let score_color = match score {
        90..=100 => Color::Rgb { r: 255, g: 255, b: 255 },
        75..=89 => Color::Rgb { r: 220, g: 220, b: 220 },
        60..=74 => Color::Rgb { r: 180, g: 180, b: 180 },
        _ => Color::Rgb { r: 140, g: 140, b: 140 },
    };

    let _ = execute!(
        io::stderr(),
        Print("\n"),
        SetForegroundColor(Color::Rgb { r: 100, g: 100, b: 100 }),
        Print("  ╭─────────────────────────────────────────╮\n"),
        Print("  │"),
        SetForegroundColor(score_color),
        Print(format!("       {} ", score)),
        SetForegroundColor(Color::Rgb { r: 140, g: 140, b: 140 }),
        Print(format!("({})                        ", grade)),
        SetForegroundColor(Color::Rgb { r: 100, g: 100, b: 100 }),
        Print("│\n"),
        Print("  ╰─────────────────────────────────────────╯\n"),
        ResetColor,
        Print("\n")
    );
}

fn truncate_str(s: &str, max_len: usize) -> String {
    if s.len() <= max_len {
        s.to_string()
    } else {
        format!("{}...", &s[..max_len.saturating_sub(3)])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_spinner_frames() {
        let spinner = Spinner::new(SpinnerStyle::Braille);
        assert!(!spinner.frames.is_empty());
    }

    #[test]
    fn test_progress_bar() {
        let mut pb = ProgressBar::new(100);
        pb.inc();
        assert_eq!(pb.current, 1);
    }

    #[test]
    fn test_score_reveal() {
        let mut reveal = ScoreReveal::new(80, 100);
        assert_eq!(reveal.value(), 0);
        reveal.start();
        // After start, value should eventually reach target
    }
}

