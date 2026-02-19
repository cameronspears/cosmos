//! Configuration management for cosmos
//!
//! Stores settings in ~/.config/cosmos/config.json

use crate::keyring;
use crate::util::debug_stderr_enabled;
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::Write;
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {}

impl Default for Config {
    fn default() -> Self {
        Self {}
    }
}

impl Config {
    fn sanitize(&mut self) {}

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
                match serde_json::from_str::<Config>(&content) {
                    Ok(mut config) => {
                        config.sanitize();
                        return config;
                    }
                    Err(err) => {
                        preserve_corrupt_config(&path, &content);
                        if debug_stderr_enabled() {
                            eprintln!(
                                "  Warning: Config file was corrupted ({}). A backup was saved and defaults were loaded.",
                                err
                            );
                        }
                    }
                }
            }
        }
        Self::default()
    }

    /// Save config to disk
    pub fn save(&self) -> Result<(), String> {
        let mut sanitized = self.clone();
        sanitized.sanitize();
        let dir =
            Self::config_dir().ok_or_else(|| "Could not determine config directory".to_string())?;

        fs::create_dir_all(&dir)
            .map_err(|e| format!("Failed to create config directory: {}", e))?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if let Err(e) = fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)) {
                if debug_stderr_enabled() {
                    eprintln!(
                        "  Warning: Failed to set config directory permissions: {}",
                        e
                    );
                }
            }
        }

        let path = dir.join("config.json");
        let content = serde_json::to_string_pretty(&sanitized)
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

    /// Get the Groq API key (keyring first, environment fallback).
    pub fn get_api_key(&mut self) -> Option<String> {
        // Keyring/store has precedence for the default "just works" path.
        match keyring::get_api_key() {
            Ok(Some(key)) => return Some(key),
            Ok(None) => {} // No key stored, continue to env fallback.
            Err(err) => {
                keyring::warn_keychain_error_once("API key", &err);
            }
        }
        std::env::var("GROQ_API_KEY")
            .ok()
            .or_else(|| std::env::var("GROQ_API_TOKEN").ok())
            .or_else(|| std::env::var("OPENAI_API_KEY").ok())
    }

    /// Set and save the API key
    pub fn set_api_key(&mut self, key: &str) -> Result<(), String> {
        // Try to write to keychain
        keyring::set_api_key(key).map_err(|e| {
            format!(
                "Failed to store API key in {}: {}. \
                 You can set the GROQ_API_KEY environment variable instead.",
                keyring::credentials_store_label(),
                e
            )
        })?;

        // Verify the write succeeded by reading it back
        match keyring::get_api_key() {
            Ok(Some(stored_key)) if stored_key == key => self.save(),
            Ok(Some(_)) => Err(format!(
                "API key verification failed: stored key doesn't match in {}. \
                     You can set the GROQ_API_KEY environment variable instead.",
                keyring::credentials_store_label()
            )),
            Ok(None) => Err(format!(
                "API key verification failed: key was not persisted to {}. \
                     You can set the GROQ_API_KEY environment variable instead.",
                keyring::credentials_store_label()
            )),
            Err(read_err) => Err(format!(
                "API key verification failed: couldn't read back from {} ({}). \
                     You can set the GROQ_API_KEY environment variable instead.",
                keyring::credentials_store_label(),
                read_err
            )),
        }
    }

    /// Check if API key is configured
    pub fn has_api_key(&self) -> bool {
        match keyring::get_api_key() {
            Ok(Some(_)) => return true,
            Ok(None) => {} // No key stored
            Err(err) => {
                keyring::warn_keychain_error_once("API key", &err);
            }
        }
        std::env::var("GROQ_API_KEY").is_ok()
            || std::env::var("GROQ_API_TOKEN").is_ok()
            || std::env::var("OPENAI_API_KEY").is_ok()
    }

    /// Validate API key format.
    pub fn validate_api_key_format(key: &str) -> bool {
        let key = key.trim();
        !key.is_empty() && (key.starts_with("gsk_") || key.starts_with("sk-"))
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
    println!("  Cosmos uses Groq for AI-powered suggestions.");
    println!("  Quick setup takes about a minute.");
    println!();
    println!("  Steps:");
    println!("    1) Create a key at https://console.groq.com/keys");
    println!("    2) Ensure your Groq account is active");
    println!("    3) Paste the key below and press Enter");
    println!();
    println!("  Data use notice: Cosmos sends selected code snippets and file paths to Groq");
    println!("  to generate and validate suggestions. Local cache stays in .cosmos.");
    println!();
    println!(
        "  We'll store it in your {}.",
        keyring::credentials_store_label()
    );
    println!("  You can update it later with `cosmos --setup`.");
    println!("  Prefer env vars? Set GROQ_API_KEY and rerun.");
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
        println!("  Warning: Key doesn't look like a Groq key (usually starts with gsk_)");
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
        if debug_stderr_enabled() {
            eprintln!(
                "  Warning: Failed to set temp config file permissions: {}",
                e
            );
        }
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
        let encoded = serde_json::to_string(&config).unwrap();
        assert_eq!(encoded, "{}");
    }

    #[test]
    fn test_config_deserializes_legacy_shape_with_defaults() {
        let legacy = r#"{"openrouter_user_id":"anon-123","suggestions_profile":"strict","suggestions_display_cap":5}"#;
        let _parsed: Config = serde_json::from_str(legacy).unwrap();
    }

    #[test]
    fn test_config_round_trip() {
        let config = Config {};
        let encoded = serde_json::to_string(&config).unwrap();
        let _decoded: Config = serde_json::from_str(&encoded).unwrap();
    }
}
