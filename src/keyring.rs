//! Unified keyring storage for all cosmos credentials
//!
//! Stores all credentials in a single keychain entry to minimize
//! macOS password prompts. Credentials are stored as JSON.

use keyring::Entry;
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};

/// Single service name for all cosmos credentials.
/// Using one entry means only one macOS keychain prompt instead of two.
const KEYRING_SERVICE: &str = "cosmos-credentials";
const KEYRING_USERNAME: &str = "default";

/// Legacy service names for migration
const LEGACY_API_KEY_SERVICE: &str = "cosmos";
const LEGACY_API_KEY_USERNAME: &str = "openrouter_api_key";
const LEGACY_GITHUB_SERVICE: &str = "cosmos-github";
const LEGACY_GITHUB_USERNAME: &str = "github_token";

/// All credentials stored in a single keychain entry
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct StoredCredentials {
    #[serde(skip_serializing_if = "Option::is_none")]
    openrouter_api_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    github_token: Option<String>,
}

type KeyringResult<T> = Result<T, String>;

#[derive(Debug, Default)]
struct CredentialsCache {
    cached: Option<KeyringResult<StoredCredentials>>,
    migration_attempted: bool,
}

static CREDENTIALS_CACHE: OnceLock<Mutex<CredentialsCache>> = OnceLock::new();
static KEYRING_ERROR_WARNED: AtomicBool = AtomicBool::new(false);

fn credentials_cache() -> &'static Mutex<CredentialsCache> {
    CREDENTIALS_CACHE.get_or_init(|| Mutex::new(CredentialsCache::default()))
}

fn keyring_disabled() -> bool {
    if cfg!(test) {
        return true;
    }
    matches!(
        std::env::var("COSMOS_DISABLE_KEYRING")
            .unwrap_or_default()
            .to_lowercase()
            .as_str(),
        "1" | "true" | "yes"
    )
}

fn keyring_entry() -> Result<Entry, keyring::Error> {
    Entry::new(KEYRING_SERVICE, KEYRING_USERNAME)
}

fn legacy_api_key_entry() -> Result<Entry, keyring::Error> {
    Entry::new(LEGACY_API_KEY_SERVICE, LEGACY_API_KEY_USERNAME)
}

fn legacy_github_entry() -> Result<Entry, keyring::Error> {
    Entry::new(LEGACY_GITHUB_SERVICE, LEGACY_GITHUB_USERNAME)
}

/// Warn about keychain errors only once per session
pub fn warn_keychain_error_once(context: &str, err: &str) {
    if KEYRING_ERROR_WARNED.swap(true, Ordering::Relaxed) {
        return;
    }
    eprintln!(
        "  Warning: Couldn't access system keychain for {}: {}",
        context, err
    );
    eprintln!("  Tip: When macOS prompts, choose \"Always Allow\" for cosmos.");
    eprintln!(
        "  Tip: You can also set OPENROUTER_API_KEY and GITHUB_TOKEN env vars to bypass keychain."
    );
}

/// Read credentials from the unified keychain entry
fn read_credentials_uncached() -> KeyringResult<StoredCredentials> {
    if keyring_disabled() {
        return Ok(StoredCredentials::default());
    }
    let entry = keyring_entry().map_err(|e| e.to_string())?;
    match entry.get_password() {
        Ok(json) => {
            serde_json::from_str(&json).map_err(|e| format!("Failed to parse credentials: {}", e))
        }
        Err(keyring::Error::NoEntry) => Ok(StoredCredentials::default()),
        Err(err) => Err(err.to_string()),
    }
}

/// Write credentials to the unified keychain entry
fn write_credentials(creds: &StoredCredentials) -> Result<(), keyring::Error> {
    let entry = keyring_entry()?;
    let json = serde_json::to_string(creds).expect("Failed to serialize credentials");
    entry.set_password(&json)?;
    Ok(())
}

/// Attempt to migrate credentials from legacy separate keychain entries
fn migrate_legacy_credentials(creds: &mut StoredCredentials) -> bool {
    let mut migrated = false;

    // Migrate legacy API key if we don't have one
    if creds.openrouter_api_key.is_none() {
        if let Ok(entry) = legacy_api_key_entry() {
            if let Ok(key) = entry.get_password() {
                creds.openrouter_api_key = Some(key);
                migrated = true;
                // Delete legacy entry after successful read
                let _ = entry.delete_credential();
            }
        }
    }

    // Migrate legacy GitHub token if we don't have one
    if creds.github_token.is_none() {
        if let Ok(entry) = legacy_github_entry() {
            if let Ok(token) = entry.get_password() {
                creds.github_token = Some(token);
                migrated = true;
                // Delete legacy entry after successful read
                let _ = entry.delete_credential();
            }
        }
    }

    migrated
}

/// Read credentials with caching and automatic migration
fn read_credentials_cached() -> KeyringResult<StoredCredentials> {
    let cache = credentials_cache();
    let mut guard = match cache.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };

    // Return cached value if available
    if let Some(ref result) = guard.cached {
        return result.clone();
    }

    // Read from keychain
    let mut creds = read_credentials_uncached()?;

    // Attempt migration from legacy entries (only once)
    if !guard.migration_attempted {
        guard.migration_attempted = true;
        if migrate_legacy_credentials(&mut creds) {
            // Save migrated credentials
            if let Err(e) = write_credentials(&creds) {
                eprintln!("  Warning: Failed to save migrated credentials: {}", e);
            } else {
                eprintln!("  + Migrated credentials to unified keychain entry");
            }
        }
    }

    guard.cached = Some(Ok(creds.clone()));
    Ok(creds)
}

/// Update the cache after a write operation
fn update_cache(creds: StoredCredentials) {
    let cache = credentials_cache();
    let mut guard = match cache.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    guard.cached = Some(Ok(creds));
}

// ============================================================================
// Public API
// ============================================================================

/// Get the OpenRouter API key from the keychain
pub fn get_api_key() -> KeyringResult<Option<String>> {
    let creds = read_credentials_cached()?;
    Ok(creds.openrouter_api_key)
}

/// Set the OpenRouter API key in the keychain
pub fn set_api_key(key: &str) -> Result<(), String> {
    let mut creds = read_credentials_cached().unwrap_or_default();
    creds.openrouter_api_key = Some(key.to_string());
    write_credentials(&creds).map_err(|e| e.to_string())?;
    update_cache(creds);
    Ok(())
}

/// Get the GitHub token from the keychain
pub fn get_github_token() -> KeyringResult<Option<String>> {
    let creds = read_credentials_cached()?;
    Ok(creds.github_token)
}

/// Set the GitHub token in the keychain
pub fn set_github_token(token: &str) -> Result<(), String> {
    let mut creds = read_credentials_cached().unwrap_or_default();
    creds.github_token = Some(token.to_string());
    write_credentials(&creds).map_err(|e| e.to_string())?;
    update_cache(creds);
    Ok(())
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_stored_credentials_default() {
        let creds = StoredCredentials::default();
        assert!(creds.openrouter_api_key.is_none());
        assert!(creds.github_token.is_none());
    }

    #[test]
    fn test_stored_credentials_serialization() {
        let creds = StoredCredentials {
            openrouter_api_key: Some("sk-test".to_string()),
            github_token: Some("ghp_test".to_string()),
        };
        let json = serde_json::to_string(&creds).unwrap();
        assert!(json.contains("sk-test"));
        assert!(json.contains("ghp_test"));

        let parsed: StoredCredentials = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.openrouter_api_key, Some("sk-test".to_string()));
        assert_eq!(parsed.github_token, Some("ghp_test".to_string()));
    }

    #[test]
    fn test_stored_credentials_partial_serialization() {
        // Only API key set
        let creds = StoredCredentials {
            openrouter_api_key: Some("sk-test".to_string()),
            github_token: None,
        };
        let json = serde_json::to_string(&creds).unwrap();
        assert!(json.contains("sk-test"));
        assert!(!json.contains("github_token")); // None fields should be omitted
    }

    #[test]
    fn test_stored_credentials_deserialize_partial() {
        // JSON with only one field should parse correctly
        let json = r#"{"openrouter_api_key": "sk-test"}"#;
        let parsed: StoredCredentials = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.openrouter_api_key, Some("sk-test".to_string()));
        assert!(parsed.github_token.is_none());
    }

    #[test]
    fn test_stored_credentials_deserialize_empty() {
        let json = "{}";
        let parsed: StoredCredentials = serde_json::from_str(json).unwrap();
        assert!(parsed.openrouter_api_key.is_none());
        assert!(parsed.github_token.is_none());
    }
}
