//! Configuration management for codecosmos
//!
//! Stores settings in ~/.config/codecosmos/config.json

#![allow(dead_code)]

use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Config {
    pub openrouter_api_key: Option<String>,
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
        
        let path = dir.join("config.json");
        let content = serde_json::to_string_pretty(self)
            .map_err(|e| format!("Failed to serialize config: {}", e))?;
        
        fs::write(&path, content)
            .map_err(|e| format!("Failed to write config: {}", e))?;
        
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
    println!("  │  🔑 OPENROUTER SETUP                                    │");
    println!("  └─────────────────────────────────────────────────────────┘");
    println!();
    println!("  codecosmos uses OpenRouter for AI-powered fix suggestions.");
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

    // Validate key format (should start with sk-)
    if !key.starts_with("sk-") {
        println!();
        println!("  ⚠️  Key doesn't look like an OpenRouter key (should start with sk-)");
        println!("     Saving anyway...");
    }

    // Save the key
    let mut config = Config::load();
    config.set_api_key(&key)?;

    println!();
    println!("  ✓ API key saved to {}", Config::config_location());
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



