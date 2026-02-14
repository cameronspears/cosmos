//! Minimal config handling for UI shell mode.

use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Config {
    pub openrouter_api_key: Option<String>,
    pub openrouter_user_id: Option<String>,
}

impl Config {
    fn config_dir() -> Option<PathBuf> {
        dirs::config_dir().map(|p| p.join("cosmos"))
    }

    fn config_path() -> Option<PathBuf> {
        Self::config_dir().map(|p| p.join("config.json"))
    }

    pub fn load() -> Self {
        let Some(path) = Self::config_path() else {
            return Self::default();
        };

        match fs::read_to_string(path) {
            Ok(raw) => serde_json::from_str(&raw).unwrap_or_default(),
            Err(_) => Self::default(),
        }
    }

    pub fn save(&self) -> Result<(), String> {
        let dir = Self::config_dir().ok_or_else(|| "Could not determine config dir".to_string())?;
        fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        let path = dir.join("config.json");
        let raw = serde_json::to_string_pretty(self).map_err(|e| e.to_string())?;
        fs::write(path, raw).map_err(|e| e.to_string())
    }

    pub fn get_api_key(&mut self) -> Option<String> {
        if let Ok(key) = std::env::var("OPENROUTER_API_KEY") {
            if !key.trim().is_empty() {
                return Some(key);
            }
        }
        self.openrouter_api_key.clone()
    }

    pub fn set_api_key(&mut self, key: &str) -> Result<(), String> {
        self.openrouter_api_key = Some(key.trim().to_string());
        self.save()
    }

    pub fn has_api_key(&self) -> bool {
        std::env::var("OPENROUTER_API_KEY")
            .ok()
            .map(|v| !v.trim().is_empty())
            .unwrap_or(false)
            || self
                .openrouter_api_key
                .as_ref()
                .map(|v| !v.trim().is_empty())
                .unwrap_or(false)
    }

    pub fn validate_api_key_format(key: &str) -> bool {
        key.trim().starts_with("sk-")
    }

    pub fn config_location() -> String {
        Self::config_path()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "~/.config/cosmos/config.json".to_string())
    }
}
