//! Configuration management for codecosmos
//!
//! Stores settings in ~/.config/codecosmos/config.json

use keyring::Entry;
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::Write;
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

const KEYRING_SERVICE: &str = "codecosmos";
const KEYRING_USERNAME: &str = "openrouter_api_key";

fn keyring_entry() -> Result<Entry, keyring::Error> {
    Entry::new(KEYRING_SERVICE, KEYRING_USERNAME)
}

fn read_keyring_key() -> Result<Option<String>, keyring::Error> {
    let entry = keyring_entry()?;
    match entry.get_password() {
        Ok(key) => Ok(Some(key)),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(err) => Err(err),
    }
}

fn write_keyring_key(key: &str) -> Result<(), keyring::Error> {
    let entry = keyring_entry()?;
    entry.set_password(key)
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
        if let Some(path) = Self::config_path() {
            if let Ok(content) = fs::read_to_string(&path) {
                match serde_json::from_str(&content) {
                    Ok(config) => return config,
                    Err(err) => {
                        preserve_corrupt_config(&path, &content);
                        eprintln!(
                            "  Warning: Config file was corrupted ({}). A backup was saved and defaults were loaded.",
                            err
                        );
                    }
                }
            }
        }
        Self::default()
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

        #[cfg(unix)]
        {
            write_config_atomic(&path, &content)
                .map_err(|e| format!("Failed to write config: {}", e))?;
        }

        #[cfg(not(unix))]
        {
            fs::write(&path, content)
                .map_err(|e| format!("Failed to write config: {}", e))?;
        }
        
        Ok(())
    }

    /// Get the OpenRouter API key (from environment or keychain)
    pub fn get_api_key(&mut self) -> Option<String> {
        // Environment variable takes precedence
        if let Ok(key) = std::env::var("OPENROUTER_API_KEY") {
            return Some(key);
        }

        // Try keychain
        match read_keyring_key() {
            Ok(Some(key)) => return Some(key),
            Ok(None) => {} // No key stored, continue
            Err(err) => {
                eprintln!(
                    "  Warning: Failed to read API key from system keychain: {}",
                    err
                );
                eprintln!("  Tip: Set the OPENROUTER_API_KEY environment variable as a workaround.");
            }
        }

        // DEPRECATED: Legacy migration of plaintext API keys to system keychain.
        // This code path exists to migrate users who stored keys in config before
        // keychain support was added. Once migrated, the plaintext key is removed.
        // TODO: Remove this migration code after 2026-06-01 (6 months from keychain release)
        if let Some(key) = self.openrouter_api_key.clone() {
            eprintln!("  Migrating API key from config file to system keychain...");
            match write_keyring_key(&key) {
                Ok(()) => {
                    // Verify migration succeeded
                    if let Ok(Some(stored)) = read_keyring_key() {
                        if stored == key {
                            self.openrouter_api_key = None;
                            let _ = self.save();
                            eprintln!("  + API key migrated successfully.");
                        }
                    }
                }
                Err(err) => {
                    eprintln!("  Warning: Failed to migrate API key to keychain: {}", err);
                }
            }
            return Some(key);
        }

        None
    }

    /// Set and save the API key
    pub fn set_api_key(&mut self, key: &str) -> Result<(), String> {
        // Try to write to keychain
        if let Err(write_err) = write_keyring_key(key) {
            return Err(format!(
                "Failed to store API key in system keychain: {}. \
                 You can set the OPENROUTER_API_KEY environment variable instead.",
                write_err
            ));
        }

        // Verify the write succeeded by reading it back
        match read_keyring_key() {
            Ok(Some(stored_key)) if stored_key == key => {
                // Successfully verified - clear any legacy plaintext key from config
                self.openrouter_api_key = None;
                self.save()
            }
            Ok(Some(_)) => {
                Err(
                    "API key verification failed: stored key doesn't match. \
                     You can set the OPENROUTER_API_KEY environment variable instead."
                        .to_string(),
                )
            }
            Ok(None) => {
                Err(
                    "API key verification failed: key was not persisted to keychain. \
                     You can set the OPENROUTER_API_KEY environment variable instead."
                        .to_string(),
                )
            }
            Err(read_err) => {
                Err(format!(
                    "API key verification failed: couldn't read back from keychain ({}). \
                     You can set the OPENROUTER_API_KEY environment variable instead.",
                    read_err
                ))
            }
        }
    }

    /// Check if API key is configured
    pub fn has_api_key(&self) -> bool {
        if std::env::var("OPENROUTER_API_KEY").is_ok() {
            return true;
        }
        match read_keyring_key() {
            Ok(Some(_)) => return true,
            Ok(None) => {} // No key stored
            Err(err) => {
                eprintln!(
                    "  Warning: Failed to check system keychain for API key: {}",
                    err
                );
            }
        }
        // Legacy: check for plaintext key in config (will be migrated on get_api_key)
        self.openrouter_api_key.is_some()
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
    println!("  2. Paste it below (saved in your system keychain when available)");
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

fn preserve_corrupt_config(path: &std::path::Path, content: &str) {
    let corrupt_path = path.with_extension("json.corrupt");
    if fs::rename(path, &corrupt_path).is_err() {
        let _ = fs::write(&corrupt_path, content);
    }
}

#[cfg(unix)]
fn write_config_atomic(path: &std::path::Path, content: &str) -> Result<(), String> {
    use std::fs::OpenOptions;
    use std::os::unix::fs::PermissionsExt;

    let tmp_path = path.with_extension("tmp");
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(&tmp_path)
        .map_err(|e| e.to_string())?;

    if let Err(e) = file.set_permissions(fs::Permissions::from_mode(0o600)) {
        eprintln!("  Warning: Failed to set temp config file permissions: {}", e);
    }

    file.write_all(content.as_bytes())
        .map_err(|e| e.to_string())?;

    if let Err(err) = fs::rename(&tmp_path, path) {
        let _ = fs::remove_file(&tmp_path);
        return Err(err.to_string());
    }
    Ok(())
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



