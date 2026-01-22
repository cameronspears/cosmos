//! Configuration management for codecosmos
//!
//! Stores settings in ~/.config/codecosmos/config.json

use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Config {
    pub openrouter_api_key: Option<String>,
    /// Optional max USD spend per Cosmos session (best-effort; enforced before new AI actions)
    pub max_session_cost_usd: Option<f64>,
    /// Optional max tokens per day (local tracking; best-effort)
    pub max_tokens_per_day: Option<u32>,
    /// Tokens used today (local tracking)
    pub tokens_used_today: u32,
    /// Date string (YYYY-MM-DD) for tokens_used_today
    pub tokens_used_date: Option<String>,
    /// If true, only generate LLM summaries for changed files (and not the whole repo)
    pub summarize_changed_only: bool,
    /// If true, show a preview of what will be sent before inquiry actions
    #[serde(default = "default_privacy_preview")]
    pub privacy_preview: bool,
}

fn default_privacy_preview() -> bool {
    true
}

impl Config {
    /// Get the config directory path
    fn config_dir() -> Option<PathBuf> {
        dirs::config_dir().map(|p| p.join("codecosmos"))
    }

    /// Get the config file path
    fn config_path() -> Option<PathBuf> {
        Self::config_dir().map(|p| p.join("config.json"))
    }

    /// Load config from disk, or return default
    pub fn load() -> Self {
        Self::config_path()
            .and_then(|path| fs::read_to_string(path).ok())
            .and_then(|content| serde_json::from_str(&content).ok())
            .unwrap_or_default()
    }

    /// Save config to disk
    pub fn save(&self) -> Result<(), String> {
        let dir = Self::config_dir()
            .ok_or_else(|| "Could not determine config directory".to_string())?;
        
        fs::create_dir_all(&dir)
            .map_err(|e| format!("Failed to create config directory: {}", e))?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if let Err(e) = fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)) {
                eprintln!("  Warning: Failed to set config directory permissions: {}", e);
            }
        }
        
        let path = dir.join("config.json");
        let content = serde_json::to_string_pretty(self)
            .map_err(|e| format!("Failed to serialize config: {}", e))?;
        
        fs::write(&path, content)
            .map_err(|e| format!("Failed to write config: {}", e))?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if let Err(e) = fs::set_permissions(&path, fs::Permissions::from_mode(0o600)) {
                eprintln!("  Warning: Failed to set config file permissions: {}", e);
            }
        }
        
        Ok(())
    }

    /// Get the OpenRouter API key (from config or environment)
    pub fn get_api_key(&self) -> Option<String> {
        // Environment variable takes precedence
        std::env::var("OPENROUTER_API_KEY").ok()
            .or_else(|| self.openrouter_api_key.clone())
    }

    /// Set and save the API key
    pub fn set_api_key(&mut self, key: &str) -> Result<(), String> {
        self.openrouter_api_key = Some(key.to_string());
        self.save()
    }

    /// Check if API key is configured
    pub fn has_api_key(&self) -> bool {
        self.get_api_key().is_some()
    }

    /// Validate API key format (should start with sk-)
    pub fn validate_api_key_format(key: &str) -> bool {
        key.starts_with("sk-")
    }

    /// Refresh daily token counter if the day changed.
    pub fn ensure_daily_rollover(&mut self) {
        let today = chrono::Utc::now().date_naive().to_string();
        match self.tokens_used_date.as_deref() {
            Some(d) if d == today => {}
            _ => {
                self.tokens_used_today = 0;
                self.tokens_used_date = Some(today);
            }
        }
    }

    /// Record token usage for daily budgeting (best-effort).
    pub fn record_tokens(&mut self, tokens: u32) -> Result<(), String> {
        self.ensure_daily_rollover();
        self.tokens_used_today = self.tokens_used_today.saturating_add(tokens);
        self.save()
    }

    /// Check whether AI actions are allowed given current session cost and daily token budget.
    pub fn allow_ai(&mut self, session_cost: f64) -> Result<(), String> {
        // Session cost budget
        if let Some(max) = self.max_session_cost_usd {
            if max >= 0.0 && session_cost >= max {
                return Err(format!("Session budget reached (${:.4}/${:.4})", session_cost, max));
            }
        }

        // Daily token budget
        self.ensure_daily_rollover();
        if let Some(max_tokens) = self.max_tokens_per_day {
            if self.tokens_used_today >= max_tokens {
                return Err(format!("Daily token budget reached ({} / {})", self.tokens_used_today, max_tokens));
            }
        }

        Ok(())
    }

    /// Get the config file location for display
    pub fn config_location() -> String {
        Self::config_path()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "~/.config/codecosmos/config.json".to_string())
    }
}

/// Interactive prompt to set up API key
pub fn setup_api_key_interactive() -> Result<String, String> {
    use std::io::{self, Write};

    println!();
    println!("  ┌─────────────────────────────────────────────────────────┐");
    println!("  │  OPENROUTER SETUP                                       │");
    println!("  └─────────────────────────────────────────────────────────┘");
    println!();
    println!("  codecosmos uses OpenRouter for AI-powered suggestions.");
    println!("  Uses a 4-tier model system optimized for cost and quality.");
    println!();
    println!("  1. Get a free API key at: https://openrouter.ai/keys");
    println!("  2. Paste it below (it will be saved locally)");
    println!();
    print!("  API Key: ");
    io::stdout().flush().map_err(|e| e.to_string())?;

    let mut key = String::new();
    io::stdin().read_line(&mut key).map_err(|e| e.to_string())?;
    let key = key.trim().to_string();

    if key.is_empty() {
        return Err("No API key provided".to_string());
    }

    // Validate key format
    if !Config::validate_api_key_format(&key) {
        println!();
        println!("  Warning: Key doesn't look like an OpenRouter key (should start with sk-)");
        println!("     Saving anyway...");
    }

    // Save the key
    let mut config = Config::load();
    config.set_api_key(&key)?;

    println!();
    println!("  + API key saved to {}", Config::config_location());
    println!();

    Ok(key)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_config_default() {
        let config = Config::default();
        assert!(config.openrouter_api_key.is_none());
    }
}



