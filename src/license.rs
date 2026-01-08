//! License management for Cosmos Pro
//!
//! Supports three tiers:
//! - Free: TUI + static analysis + BYOK mode
//! - Pro: Managed AI credits + history + analytics
//! - Team: Everything + sync + SSO (future)

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

/// License tier
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum Tier {
    #[default]
    Free,
    Pro,
    Team,
}

impl Tier {
    pub fn label(&self) -> &'static str {
        match self {
            Tier::Free => "free",
            Tier::Pro => "pro",
            Tier::Team => "team",
        }
    }

    pub fn badge(&self) -> &'static str {
        match self {
            Tier::Free => "",
            Tier::Pro => " PRO",
            Tier::Team => " TEAM",
        }
    }

    pub fn has_managed_ai(&self) -> bool {
        matches!(self, Tier::Pro | Tier::Team)
    }

    pub fn has_history(&self) -> bool {
        matches!(self, Tier::Pro | Tier::Team)
    }

    pub fn has_team_sync(&self) -> bool {
        matches!(self, Tier::Team)
    }

    /// Monthly token allowance
    pub fn token_allowance(&self) -> u64 {
        match self {
            Tier::Free => 0,        // BYOK only
            Tier::Pro => 50_000,    // 50K tokens/month
            Tier::Team => 200_000,  // 200K tokens/month per seat
        }
    }
}

/// License information
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct License {
    /// License key (COSMOS-XXXX-XXXX-XXXX)
    pub key: String,
    /// License tier
    pub tier: Tier,
    /// When the license was activated
    pub activated_at: DateTime<Utc>,
    /// When the license expires (None = lifetime)
    pub expires_at: Option<DateTime<Utc>>,
    /// Email associated with license
    pub email: Option<String>,
    /// Tokens used this billing period
    pub tokens_used: u64,
    /// When the billing period resets
    pub period_resets_at: DateTime<Utc>,
}

impl License {
    /// Create a new license from a key
    pub fn new(key: String, tier: Tier) -> Self {
        let now = Utc::now();
        // Period resets ~30 days from activation
        let period_resets_at = now + chrono::Duration::days(30);

        Self {
            key,
            tier,
            activated_at: now,
            expires_at: None,
            email: None,
            tokens_used: 0,
            period_resets_at,
        }
    }

    /// Check if license is valid (not expired)
    pub fn is_valid(&self) -> bool {
        match self.expires_at {
            Some(exp) => Utc::now() < exp,
            None => true,
        }
    }

    /// Check if we need to reset the billing period
    pub fn check_period_reset(&mut self) {
        if Utc::now() >= self.period_resets_at {
            self.tokens_used = 0;
            self.period_resets_at = Utc::now() + chrono::Duration::days(30);
        }
    }

    /// Tokens remaining this period (accounts for period reset)
    pub fn tokens_remaining(&self) -> u64 {
        let allowance = self.tier.token_allowance();
        // If period has expired, tokens_used would be reset to 0 on next mutation,
        // so we should report full allowance available
        if Utc::now() >= self.period_resets_at {
            return allowance;
        }
        allowance.saturating_sub(self.tokens_used)
    }

    /// Record token usage
    pub fn record_usage(&mut self, tokens: u64) {
        self.check_period_reset();
        self.tokens_used += tokens;
    }

    /// Check if we have enough tokens
    pub fn has_tokens(&self, needed: u64) -> bool {
        self.tokens_remaining() >= needed
    }
}

/// License manager - handles storage and validation
#[derive(Debug, Clone, Default)]
pub struct LicenseManager {
    license: Option<License>,
}

impl LicenseManager {
    /// Get license file path
    fn license_path() -> Option<PathBuf> {
        dirs::config_dir().map(|p| p.join("codecosmos").join("license.json"))
    }

    /// Load license from disk
    pub fn load() -> Self {
        let license = Self::license_path()
            .and_then(|path| fs::read_to_string(path).ok())
            .and_then(|content| serde_json::from_str(&content).ok());

        Self { license }
    }

    /// Save license to disk
    pub fn save(&self) -> Result<(), String> {
        let path = Self::license_path()
            .ok_or_else(|| "Could not determine config directory".to_string())?;

        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .map_err(|e| format!("Failed to create config directory: {}", e))?;
        }

        match &self.license {
            Some(license) => {
                let content = serde_json::to_string_pretty(license)
                    .map_err(|e| format!("Failed to serialize license: {}", e))?;
                fs::write(&path, content)
                    .map_err(|e| format!("Failed to write license: {}", e))?;
            }
            None => {
                // Remove license file if deactivating
                let _ = fs::remove_file(&path);
            }
        }

        Ok(())
    }

    /// Get current tier
    pub fn tier(&self) -> Tier {
        self.license
            .as_ref()
            .filter(|l| l.is_valid())
            .map(|l| l.tier)
            .unwrap_or(Tier::Free)
    }

    /// Check if Pro or higher
    pub fn is_pro(&self) -> bool {
        matches!(self.tier(), Tier::Pro | Tier::Team)
    }

    /// Get license if valid
    pub fn get_license(&self) -> Option<&License> {
        self.license.as_ref().filter(|l| l.is_valid())
    }

    /// Get mutable license
    pub fn get_license_mut(&mut self) -> Option<&mut License> {
        self.license.as_mut().filter(|l| l.is_valid())
    }

    /// Record token usage
    pub fn record_usage(&mut self, tokens: u64) {
        if let Some(license) = self.license.as_mut() {
            license.record_usage(tokens);
            let _ = self.save();
        }
    }

    /// Get usage stats
    pub fn usage_stats(&self) -> UsageStats {
        match &self.license {
            Some(license) if license.is_valid() => UsageStats {
                tier: license.tier,
                tokens_used: license.tokens_used,
                tokens_remaining: license.tokens_remaining(),
                period_resets_at: Some(license.period_resets_at),
            },
            _ => UsageStats {
                tier: Tier::Free,
                tokens_used: 0,
                tokens_remaining: 0,
                period_resets_at: None,
            },
        }
    }

    /// Activate a license key
    pub fn activate(&mut self, key: &str) -> Result<Tier, String> {
        // Validate key format: COSMOS-XXXX-XXXX-XXXX
        let tier = validate_license_key(key)?;

        let license = License::new(key.to_string(), tier);
        self.license = Some(license);
        self.save()?;

        Ok(tier)
    }

    /// Deactivate current license
    pub fn deactivate(&mut self) -> Result<(), String> {
        self.license = None;
        self.save()
    }

    /// Get license file location for display
    pub fn license_location() -> String {
        Self::license_path()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "~/.config/codecosmos/license.json".to_string())
    }
}

/// Usage statistics
#[derive(Debug, Clone)]
pub struct UsageStats {
    pub tier: Tier,
    pub tokens_used: u64,
    pub tokens_remaining: u64,
    pub period_resets_at: Option<DateTime<Utc>>,
}

impl UsageStats {
    /// Format for display
    pub fn display(&self) -> String {
        match self.tier {
            Tier::Free => "Free tier (BYOK mode)".to_string(),
            Tier::Pro | Tier::Team => {
                let allowance = self.tier.token_allowance();
                let pct = (self.tokens_used as f64 / allowance as f64 * 100.0) as u32;
                format!(
                    "{}: {}K / {}K tokens ({}%)",
                    self.tier.label().to_uppercase(),
                    self.tokens_used / 1000,
                    allowance / 1000,
                    pct
                )
            }
        }
    }
}

/// Validate a license key and return its tier
///
/// Key format: COSMOS-TIER-XXXX-XXXX-CHECK
/// Where:
/// - TIER: PRO or TEAM
/// - XXXX: Random alphanumeric segments
/// - CHECK: Checksum for validation
fn validate_license_key(key: &str) -> Result<Tier, String> {
    let key = key.trim().to_uppercase();

    // Check format
    let parts: Vec<&str> = key.split('-').collect();
    if parts.len() != 5 {
        return Err("Invalid key format. Expected: COSMOS-TIER-XXXX-XXXX-CHECK".to_string());
    }

    if parts[0] != "COSMOS" {
        return Err("Invalid key: must start with COSMOS-".to_string());
    }

    // Determine tier
    let tier = match parts[1] {
        "PRO" => Tier::Pro,
        "TEAM" => Tier::Team,
        "FREE" => Tier::Free, // For testing
        _ => return Err("Invalid tier in key. Expected PRO or TEAM.".to_string()),
    };

    // Validate segments are alphanumeric
    for segment in &parts[2..4] {
        if segment.len() != 4 || !segment.chars().all(|c| c.is_alphanumeric()) {
            return Err("Invalid key segment format.".to_string());
        }
    }

    // Validate checksum (simple XOR-based for now)
    let expected_check = compute_checksum(&parts[0..4].join("-"));
    if parts[4] != expected_check {
        return Err("Invalid key: checksum mismatch.".to_string());
    }

    Ok(tier)
}

/// Compute a simple checksum for license validation
fn compute_checksum(data: &str) -> String {
    // Simple checksum: XOR all bytes, then format as 4-char hex
    let checksum: u32 = data.bytes().fold(0u32, |acc, b| acc.wrapping_add(b as u32));
    let checksum = checksum ^ 0xC05_0575; // Magic number: "COSMOS"
    format!("{:04X}", checksum & 0xFFFF)
}

/// Generate a license key (for admin/testing purposes)
pub fn generate_license_key(tier: Tier) -> String {
    use std::time::{SystemTime, UNIX_EPOCH};

    let tier_str = match tier {
        Tier::Free => "FREE",
        Tier::Pro => "PRO",
        Tier::Team => "TEAM",
    };

    // Generate pseudo-random segments based on time
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();

    let seg1 = format!("{:04X}", (now & 0xFFFF) as u16);
    let seg2 = format!("{:04X}", ((now >> 16) & 0xFFFF) as u16);

    let base = format!("COSMOS-{}-{}-{}", tier_str, seg1, seg2);
    let checksum = compute_checksum(&base);

    format!("{}-{}", base, checksum)
}

/// Interactive license activation
pub fn activate_interactive() -> Result<Tier, String> {
    use std::io::{self, Write};

    println!();
    println!("  ┌─────────────────────────────────────────────────────────┐");
    println!("  │  ✦ COSMOS PRO ACTIVATION                                │");
    println!("  └─────────────────────────────────────────────────────────┘");
    println!();
    println!("  Enter your license key to unlock:");
    println!("  • Managed AI (no API key setup)");
    println!("  • Suggestion history & analytics");
    println!("  • Priority support");
    println!();
    println!("  Get a license at: https://cosmos.dev/pro");
    println!();
    print!("  License Key: ");
    io::stdout().flush().map_err(|e| e.to_string())?;

    let mut key = String::new();
    io::stdin().read_line(&mut key).map_err(|e| e.to_string())?;
    let key = key.trim();

    if key.is_empty() {
        return Err("No license key provided".to_string());
    }

    let mut manager = LicenseManager::load();
    let tier = manager.activate(key)?;

    println!();
    println!("  ✓ License activated! You now have Cosmos {}.", tier.label().to_uppercase());
    println!("  ✓ Saved to {}", LicenseManager::license_location());
    println!();

    Ok(tier)
}

/// Deactivate license interactively
pub fn deactivate_interactive() -> Result<(), String> {
    use std::io::{self, Write};

    println!();
    print!("  Are you sure you want to deactivate your license? [y/N] ");
    io::stdout().flush().map_err(|e| e.to_string())?;

    let mut response = String::new();
    io::stdin().read_line(&mut response).map_err(|e| e.to_string())?;

    if response.trim().to_lowercase() == "y" {
        let mut manager = LicenseManager::load();
        manager.deactivate()?;
        println!("  ✓ License deactivated. You're now on the free tier.");
    } else {
        println!("  Cancelled.");
    }
    println!();

    Ok(())
}

/// Safely format a license key for display, masking the middle portion.
/// Returns the full key if it's too short to mask safely.
fn format_key_masked(key: &str) -> String {
    // Need at least 16 chars to safely show 12 prefix + 4 suffix with "..."
    if key.len() >= 16 {
        format!("{}...{}", &key[..12], &key[key.len() - 4..])
    } else if key.is_empty() {
        "(invalid)".to_string()
    } else {
        // Key is too short to mask; show it partially obscured
        format!("{}...", &key[..key.len().min(8)])
    }
}

/// Show current license status
pub fn show_status() {
    let manager = LicenseManager::load();
    let _stats = manager.usage_stats();

    println!();
    println!("  ┌─────────────────────────────────────────────────────────┐");
    println!("  │  ✦ COSMOS LICENSE STATUS                                │");
    println!("  └─────────────────────────────────────────────────────────┘");
    println!();

    match manager.get_license() {
        Some(license) => {
            println!("  Tier:      {}", license.tier.label().to_uppercase());
            println!("  Key:       {}", format_key_masked(&license.key));
            println!("  Activated: {}", license.activated_at.format("%Y-%m-%d"));

            if let Some(exp) = license.expires_at {
                println!("  Expires:   {}", exp.format("%Y-%m-%d"));
            } else {
                println!("  Expires:   Never");
            }

            if license.tier.has_managed_ai() {
                let allowance = license.tier.token_allowance();
                let pct = (license.tokens_used as f64 / allowance as f64 * 100.0) as u32;
                println!();
                println!("  Usage:     {}K / {}K tokens ({}%)", 
                    license.tokens_used / 1000, 
                    allowance / 1000,
                    pct
                );
                println!("  Resets:    {}", license.period_resets_at.format("%Y-%m-%d"));
            }
        }
        None => {
            println!("  Tier:      FREE");
            println!("  Mode:      BYOK (Bring Your Own Key)");
            println!();
            println!("  To unlock Cosmos Pro:");
            println!("    cosmos --activate <license-key>");
            println!();
            println!("  Get a license at: https://cosmos.dev/pro");
        }
    }

    println!();
    println!("  Config: {}", LicenseManager::license_location());
    println!();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_generate_and_validate_key() {
        let key = generate_license_key(Tier::Pro);
        println!("Generated key: {}", key);
        let tier = validate_license_key(&key).unwrap();
        assert_eq!(tier, Tier::Pro);
    }

    #[test]
    fn test_invalid_key() {
        assert!(validate_license_key("invalid-key").is_err());
        assert!(validate_license_key("COSMOS-PRO-XXXX-YYYY-ZZZZ").is_err());
    }

    #[test]
    fn test_tier_properties() {
        assert!(!Tier::Free.has_managed_ai());
        assert!(Tier::Pro.has_managed_ai());
        assert!(Tier::Team.has_team_sync());
        assert!(!Tier::Pro.has_team_sync());
    }
}


