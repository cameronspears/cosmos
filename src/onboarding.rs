//! First-run onboarding experience
//!
//! Runs the same OpenRouter setup flow as `cosmos --setup`.
//! This is required when no API key is configured.

use crate::config::Config;
use std::io::{self, IsTerminal, Write};

/// Check if this is a first run (no config exists)
pub fn is_first_run() -> bool {
    let config = Config::load();

    // First run if no API key
    !config.has_api_key()
}

/// Run the onboarding flow
pub fn run_onboarding() -> Result<(), String> {
    if !io::stdin().is_terminal() {
        return Err(
            "Setup requires an interactive terminal. Run `cosmos --setup` or set OPENROUTER_API_KEY."
                .to_string(),
        );
    }

    loop {
        match crate::config::setup_api_key_interactive() {
            Ok(_) => return Ok(()),
            Err(err) if err == "No API key provided" => {
                println!();
                println!("  An API key is required to continue.");
                print!("  Press Enter to try again, or Ctrl+C to exit...");
                io::stdout().flush().map_err(|e| e.to_string())?;
                let mut _input = String::new();
                let _ = io::stdin().read_line(&mut _input);
                println!();
            }
            Err(err) => return Err(err),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_first_run() {
        // This will depend on the actual config state, so just test it doesn't panic
        let _ = is_first_run();
    }
}
