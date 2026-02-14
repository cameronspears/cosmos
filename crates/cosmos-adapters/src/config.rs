//! Configuration management for cosmos
//!
//! Stores settings in ~/.config/cosmos/config.json

use crate::keyring;
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::Write;
use std::path::PathBuf;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Config {
    /// Anonymous per-install identifier used for OpenRouter request stickiness.
    /// Cosmos may send selected code snippets + file paths to OpenRouter to generate/validate AI output.
    pub openrouter_user_id: Option<String>,
}

impl Config {
    /// Get the config directory path
    fn config_dir() -> Option<PathBuf> {
        dirs::config_dir().map(|p| p.join("cosmos"))
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
        let dir =
            Self::config_dir().ok_or_else(|| "Could not determine config directory".to_string())?;

        fs::create_dir_all(&dir)
            .map_err(|e| format!("Failed to create config directory: {}", e))?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if let Err(e) = fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)) {
                eprintln!(
                    "  Warning: Failed to set config directory permissions: {}",
                    e
                );
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
            fs::write(&path, content).map_err(|e| format!("Failed to write config: {}", e))?;
        }

        Ok(())
    }

    /// Get the OpenRouter API key (from environment or keychain)
    pub fn get_api_key(&mut self) -> Option<String> {
        // Environment variable takes precedence
        if let Ok(key) = std::env::var("OPENROUTER_API_KEY") {
            return Some(key);
        }

        // Try keychain (migration from legacy entries happens automatically)
        match keyring::get_api_key() {
            Ok(Some(key)) => return Some(key),
            Ok(None) => {} // No key stored, continue
            Err(err) => {
                keyring::warn_keychain_error_once("API key", &err);
            }
        }
        None
    }

    /// Set and save the API key
    pub fn set_api_key(&mut self, key: &str) -> Result<(), String> {
        // Try to write to keychain
        keyring::set_api_key(key).map_err(|e| {
            format!(
                "Failed to store API key in {}: {}. \
                 You can set the OPENROUTER_API_KEY environment variable instead.",
                keyring::credentials_store_label(),
                e
            )
        })?;

        // Verify the write succeeded by reading it back
        match keyring::get_api_key() {
            Ok(Some(stored_key)) if stored_key == key => self.save(),
            Ok(Some(_)) => Err(format!(
                "API key verification failed: stored key doesn't match in {}. \
                     You can set the OPENROUTER_API_KEY environment variable instead.",
                keyring::credentials_store_label()
            )),
            Ok(None) => Err(format!(
                "API key verification failed: key was not persisted to {}. \
                     You can set the OPENROUTER_API_KEY environment variable instead.",
                keyring::credentials_store_label()
            )),
            Err(read_err) => Err(format!(
                "API key verification failed: couldn't read back from {} ({}). \
                     You can set the OPENROUTER_API_KEY environment variable instead.",
                keyring::credentials_store_label(),
                read_err
            )),
        }
    }

    /// Check if API key is configured
    pub fn has_api_key(&self) -> bool {
        if std::env::var("OPENROUTER_API_KEY").is_ok() {
            return true;
        }
        match keyring::get_api_key() {
            Ok(Some(_)) => return true,
            Ok(None) => {} // No key stored
            Err(err) => {
                keyring::warn_keychain_error_once("API key", &err);
            }
        }
        false
    }

    /// Validate API key format (should start with sk-)
    pub fn validate_api_key_format(key: &str) -> bool {
        key.starts_with("sk-")
    }

    /// Get the config file location for display
    pub fn config_location() -> String {
        Self::config_path()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "~/.config/cosmos/config.json".to_string())
    }
}

/// Interactive prompt to set up API key
pub fn setup_api_key_interactive() -> Result<String, String> {
    use std::io::{self, Write};

    println!();
    println!("  ┌─────────────────────────────────────────────────────────┐");
    println!("  │  COSMOS SETUP                                           │");
    println!("  └─────────────────────────────────────────────────────────┘");
    println!();
    println!("  Cosmos uses OpenRouter for AI-powered suggestions.");
    println!("  Quick setup takes about a minute.");
    println!();
    println!("  Steps:");
    println!("    1) Create a key at https://openrouter.ai/keys");
    println!("    2) Add funds in OpenRouter (required to use Cosmos)");
    println!("    3) Paste the key below and press Enter");
    println!();
    println!("  Data use notice: Cosmos sends selected code snippets and file paths to OpenRouter");
    println!("  to generate and validate suggestions. Local cache stays in .cosmos.");
    println!();
    println!(
        "  We'll store it in your {}.",
        keyring::credentials_store_label()
    );
    println!("  You can update it later with `cosmos --setup`.");
    println!("  Prefer env vars? Set OPENROUTER_API_KEY and rerun.");
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
        eprintln!(
            "  Warning: Failed to set temp config file permissions: {}",
            e
        );
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
        assert!(config.openrouter_user_id.is_none());
    }
}
