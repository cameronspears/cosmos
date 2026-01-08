//! First-run onboarding experience
//!
//! Shows a welcome screen on first run with options to:
//! - Try free with BYOK (Bring Your Own Key)
//! - Activate a Pro license
//! - Skip setup (limited functionality)

use crate::config::Config;
use crate::license::LicenseManager;
use std::io::{self, Write};

/// Check if this is a first run (no config or license exists)
pub fn is_first_run() -> bool {
    let config = Config::load();
    let license = LicenseManager::load();
    
    // First run if no API key AND no license
    !config.has_api_key() && license.tier() == crate::license::Tier::Free
}

/// Run the onboarding flow
/// Returns true if setup was completed, false if skipped
pub fn run_onboarding() -> Result<bool, String> {
    clear_screen();
    print_welcome();
    
    let choice = prompt_choice()?;
    
    match choice {
        OnboardingChoice::Byok => {
            setup_byok()?;
            Ok(true)
        }
        OnboardingChoice::Pro => {
            setup_pro()?;
            Ok(true)
        }
        OnboardingChoice::Skip => {
            print_skip_message();
            Ok(false)
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum OnboardingChoice {
    Byok,
    Pro,
    Skip,
}

fn clear_screen() {
    print!("\x1B[2J\x1B[1;1H");
    let _ = io::stdout().flush();
}

fn print_welcome() {
    println!();
    println!("  ╔════════════════════════════════════════════════════════════════╗");
    println!("  ║                                                                ║");
    println!("  ║     ☽ C O S M O S ✦                                           ║");
    println!("  ║                                                                ║");
    println!("  ║     A terminal-first steward for your codebase                ║");
    println!("  ║                                                                ║");
    println!("  ╚════════════════════════════════════════════════════════════════╝");
    println!();
    println!("  Welcome! Cosmos sits *outside* your editor and helps you ship:");
    println!("  it reads git context, suggests high-leverage improvements,");
    println!("  and can turn fixes into clean branches/PRs.");
    println!();
    println!("  (Keep Cursor as your editor. Let Cosmos be your steward.)");
    println!();
    println!("  ─────────────────────────────────────────────────────────────────");
    println!();
    println!("  How would you like to use Cosmos?");
    println!();
    println!("    [1] Free (BYOK) - Bring your own OpenRouter API key");
    println!("        You pay OpenRouter directly. Typically <$0.10/session.");
    println!();
    println!("    [2] Pro - $12/month with managed AI credits");
    println!("        No API key needed. 50K tokens/month included.");
    println!("        Get a license at: https://cosmos.dev/pro");
    println!();
    println!("    [3] Skip - Continue without AI features");
    println!("        You can set up later with --setup or --activate.");
    println!();
}

fn prompt_choice() -> Result<OnboardingChoice, String> {
    loop {
        print!("  Enter your choice [1/2/3]: ");
        io::stdout().flush().map_err(|e| e.to_string())?;
        
        let mut input = String::new();
        io::stdin().read_line(&mut input).map_err(|e| e.to_string())?;
        
        match input.trim() {
            "1" => return Ok(OnboardingChoice::Byok),
            "2" => return Ok(OnboardingChoice::Pro),
            "3" | "q" | "" => return Ok(OnboardingChoice::Skip),
            _ => {
                println!("  Please enter 1, 2, or 3.");
                continue;
            }
        }
    }
}

fn setup_byok() -> Result<(), String> {
    println!();
    println!("  ─────────────────────────────────────────────────────────────────");
    println!("  Setting up BYOK (Bring Your Own Key) mode");
    println!("  ─────────────────────────────────────────────────────────────────");
    println!();
    println!("  1. Go to: https://openrouter.ai/keys");
    println!("  2. Sign up or log in");
    println!("  3. Create a new API key");
    println!("  4. Copy and paste it below");
    println!();
    println!("  Tip: OpenRouter offers $5 free credit for new accounts!");
    println!();
    
    print!("  API Key (starts with sk-): ");
    io::stdout().flush().map_err(|e| e.to_string())?;
    
    let mut key = String::new();
    io::stdin().read_line(&mut key).map_err(|e| e.to_string())?;
    let key = key.trim();
    
    if key.is_empty() {
        println!();
        println!("  No key entered. You can set up later with: cosmos --setup");
        return Ok(());
    }
    
    // Validate format
    if !key.starts_with("sk-") {
        println!();
        println!("  Warning: Key doesn't look like an OpenRouter key (should start with sk-)");
        print!("  Save anyway? [y/N]: ");
        io::stdout().flush().map_err(|e| e.to_string())?;
        
        let mut confirm = String::new();
        io::stdin().read_line(&mut confirm).map_err(|e| e.to_string())?;
        
        if confirm.trim().to_lowercase() != "y" {
            println!("  Cancelled. Run 'cosmos --setup' to try again.");
            return Ok(());
        }
    }
    
    // Save the key
    let mut config = Config::load();
    config.set_api_key(key)?;
    
    println!();
    println!("  ✓ API key saved!");
    println!("  ✓ Config location: {}", Config::config_location());
    println!();
    println!("  You're all set! Cosmos will now analyze your codebase.");
    println!();
    print!("  Press Enter to continue...");
    io::stdout().flush().map_err(|e| e.to_string())?;
    let mut _input = String::new();
    let _ = io::stdin().read_line(&mut _input);
    
    Ok(())
}

fn setup_pro() -> Result<(), String> {
    println!();
    println!("  ─────────────────────────────────────────────────────────────────");
    println!("  Activating Cosmos Pro");
    println!("  ─────────────────────────────────────────────────────────────────");
    println!();
    println!("  If you have a license key, enter it below.");
    println!("  Otherwise, get one at: https://cosmos.dev/pro");
    println!();
    
    print!("  License Key (COSMOS-...): ");
    io::stdout().flush().map_err(|e| e.to_string())?;
    
    let mut key = String::new();
    io::stdin().read_line(&mut key).map_err(|e| e.to_string())?;
    let key = key.trim();
    
    if key.is_empty() {
        println!();
        println!("  No key entered. You can activate later with: cosmos --activate <key>");
        return Ok(());
    }
    
    // Try to activate
    let mut manager = LicenseManager::load();
    match manager.activate(key) {
        Ok(tier) => {
            println!();
            println!("  ✓ License activated!");
            println!("  ✓ Tier: {}", tier.label().to_uppercase());
            println!();
            println!("  You're all set! Cosmos Pro features are now unlocked.");
            println!();
            print!("  Press Enter to continue...");
            io::stdout().flush().map_err(|e| e.to_string())?;
            let mut _input = String::new();
            let _ = io::stdin().read_line(&mut _input);
            Ok(())
        }
        Err(e) => {
            println!();
            println!("  ✗ Activation failed: {}", e);
            println!();
            println!("  You can try again with: cosmos --activate <key>");
            println!("  Or set up BYOK mode with: cosmos --setup");
            Ok(())
        }
    }
}

fn print_skip_message() {
    println!();
    println!("  ─────────────────────────────────────────────────────────────────");
    println!("  Continuing without AI features");
    println!("  ─────────────────────────────────────────────────────────────────");
    println!();
    println!("  Cosmos will still show your project structure and file details,");
    println!("  but AI-powered suggestions won't be available.");
    println!();
    println!("  To enable AI features later:");
    println!("    cosmos --setup     (BYOK mode - bring your own API key)");
    println!("    cosmos --activate  (Pro mode - managed AI credits)");
    println!();
    print!("  Press Enter to continue...");
    let _ = io::stdout().flush();
    let mut _input = String::new();
    let _ = io::stdin().read_line(&mut _input);
}

/// Quick setup wizard for API key (non-interactive, for CI/scripts)
pub fn quick_setup(api_key: &str) -> Result<(), String> {
    if api_key.is_empty() {
        return Err("API key cannot be empty".to_string());
    }
    
    let mut config = Config::load();
    config.set_api_key(api_key)?;
    
    Ok(())
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


