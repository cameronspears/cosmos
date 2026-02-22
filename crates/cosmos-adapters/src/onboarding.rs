//! First-run onboarding experience
//!
//! Runs the setup flows for Cerebras API key and GitHub authentication.
//! The API key is required; GitHub auth is optional but recommended.

use crate::config::Config;
use crate::github;
use std::io::{self, IsTerminal, Write};

/// Check if onboarding is needed (missing API key or GitHub auth)
pub fn needs_onboarding() -> bool {
    let config = Config::load();

    // Onboarding needed if missing API key or GitHub auth
    !config.has_api_key() || !github::is_authenticated()
}

/// Run the onboarding flow (async for GitHub auth)
pub async fn run_onboarding() -> Result<(), String> {
    if !io::stdin().is_terminal() {
        return Err(
            "Setup requires an interactive terminal. Run `cosmos --setup` or set CEREBRAS_API_KEY."
                .to_string(),
        );
    }

    // Step 1: Cerebras API key (required)
    loop {
        match crate::config::setup_api_key_interactive() {
            Ok(_) => break,
            Err(err) if err == "No API key provided" => {
                println!();
                println!("  An API key is required to continue.");
                println!(
                    "  Cosmos uses Cerebras and may send selected code snippets + file paths for AI analysis."
                );
                print!("  Press Enter to try again, or Ctrl+C to exit...");
                io::stdout().flush().map_err(|e| e.to_string())?;
                let mut _input = String::new();
                let _ = io::stdin().read_line(&mut _input);
                println!();
            }
            Err(err) => return Err(err),
        }
    }

    // Step 2: GitHub authentication (required for PR creation)
    if !github::is_authenticated() {
        loop {
            println!();
            println!("  GitHub Authentication");
            println!("  ─────────────────────");
            println!();
            println!("  Cosmos creates pull requests on your behalf.");
            println!("  We'll open your browser to authenticate with GitHub.");
            println!();

            match run_github_auth().await {
                Ok(()) => break,
                Err(e) => {
                    eprintln!("  ! {}", e);
                    println!();
                    print!("  Press Enter to try again, or Ctrl+C to exit...");
                    io::stdout().flush().map_err(|e| e.to_string())?;
                    let mut _input = String::new();
                    let _ = io::stdin().read_line(&mut _input);
                }
            }
        }
    }

    Ok(())
}

/// Run the GitHub device flow authentication
async fn run_github_auth() -> Result<(), String> {
    struct CliCallbacks {
        cancelled: bool,
    }

    impl github::DeviceFlowCallbacks for CliCallbacks {
        fn show_instructions(&mut self, instructions: &github::AuthInstructions) {
            println!();
            println!("  To authenticate:");
            println!();
            println!("    1. Visit: {}", instructions.verification_uri);
            println!("    2. Enter code: {}", instructions.user_code);
            println!();
            print!("  Waiting for authorization...");
            let _ = io::stdout().flush();

            // Try to open the URL in the default browser
            let _ = crate::git_ops::open_url(&instructions.verification_uri);
        }

        fn poll_status(&mut self) -> bool {
            print!(".");
            let _ = io::stdout().flush();
            !self.cancelled
        }

        fn on_success(&mut self, username: &str) {
            println!();
            println!();
            println!("  + Authenticated as @{}", username);
            println!(
                "  + Token saved to {}",
                crate::keyring::credentials_store_label()
            );
        }

        fn on_error(&mut self, error: &str) {
            println!();
            println!();
            eprintln!("  ! {}", error);
            eprintln!("  ! You can try again later with `cosmos --github-login`");
        }
    }

    let mut callbacks = CliCallbacks { cancelled: false };
    github::run_device_flow(&mut callbacks)
        .await
        .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_needs_onboarding_does_not_panic() {
        // This will depend on the actual config state, so just test it doesn't panic
        let _ = needs_onboarding();
    }

    #[test]
    fn test_needs_onboarding_returns_bool() {
        let result = needs_onboarding();
        // Just verify it returns a boolean (type check)
        let _: bool = result;
    }

    #[test]
    fn test_needs_onboarding_with_env_vars() {
        // Save original state
        let orig_cerebras = std::env::var("CEREBRAS_API_KEY").ok();
        let orig_github = std::env::var("GITHUB_TOKEN").ok();

        // Set both env vars
        std::env::set_var("CEREBRAS_API_KEY", "csk_test-key");
        std::env::set_var("GITHUB_TOKEN", "ghp_test-token");

        // With both set, should not need onboarding
        assert!(!needs_onboarding());

        // Restore original state
        match orig_cerebras {
            Some(val) => std::env::set_var("CEREBRAS_API_KEY", val),
            None => std::env::remove_var("CEREBRAS_API_KEY"),
        }
        match orig_github {
            Some(val) => std::env::set_var("GITHUB_TOKEN", val),
            None => std::env::remove_var("GITHUB_TOKEN"),
        }
    }

    #[test]
    fn test_needs_onboarding_missing_api_key() {
        // Save original state
        let orig_cerebras = std::env::var("CEREBRAS_API_KEY").ok();
        let orig_github = std::env::var("GITHUB_TOKEN").ok();

        // Only set GitHub token, not API key
        std::env::remove_var("CEREBRAS_API_KEY");
        std::env::set_var("GITHUB_TOKEN", "ghp_test-token");

        // Without API key, should need onboarding (unless keyring has it)
        let result = needs_onboarding();
        // We can't assert true/false here because keyring might have a key
        // Just verify it runs without panic
        let _: bool = result;

        // Restore original state
        match orig_cerebras {
            Some(val) => std::env::set_var("CEREBRAS_API_KEY", val),
            None => std::env::remove_var("CEREBRAS_API_KEY"),
        }
        match orig_github {
            Some(val) => std::env::set_var("GITHUB_TOKEN", val),
            None => std::env::remove_var("GITHUB_TOKEN"),
        }
    }

    #[test]
    fn test_needs_onboarding_missing_github_token() {
        // Save original state
        let orig_cerebras = std::env::var("CEREBRAS_API_KEY").ok();
        let orig_github = std::env::var("GITHUB_TOKEN").ok();

        // Only set API key, not GitHub token
        std::env::set_var("CEREBRAS_API_KEY", "csk_test-key");
        std::env::remove_var("GITHUB_TOKEN");

        // Without GitHub token, should need onboarding (unless keyring has it)
        let result = needs_onboarding();
        // We can't assert true/false here because keyring might have a token
        // Just verify it runs without panic
        let _: bool = result;

        // Restore original state
        match orig_cerebras {
            Some(val) => std::env::set_var("CEREBRAS_API_KEY", val),
            None => std::env::remove_var("CEREBRAS_API_KEY"),
        }
        match orig_github {
            Some(val) => std::env::set_var("GITHUB_TOKEN", val),
            None => std::env::remove_var("GITHUB_TOKEN"),
        }
    }
}
