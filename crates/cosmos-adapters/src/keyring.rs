//! Unified keyring storage for all cosmos credentials
//!
//! Stores all credentials in a single keychain entry to minimize
//! macOS password prompts. Credentials are stored as JSON.

use keyring::Entry;
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};

use crate::util::debug_stderr_enabled;

/// Single service name for all cosmos credentials.
/// Using one entry means only one macOS keychain prompt instead of two.
const KEYRING_SERVICE: &str = "cosmos-credentials";
const KEYRING_USERNAME: &str = "default";

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

    // Explicit override for bypassing keychain prompts.
    let disabled_by_env = matches!(
        std::env::var("COSMOS_DISABLE_KEYRING")
            .unwrap_or_default()
            .to_lowercase()
            .as_str(),
        "1" | "true" | "yes"
    );
    if disabled_by_env {
        return true;
    }

    // If the local credentials file exists and already contains usable credentials, prefer it
    // to avoid interactive system keychain prompts (common in CI/lab runs and some macOS setups).
    if let Ok(creds) = read_fallback_credentials() {
        if creds.openrouter_api_key.is_some() || creds.github_token.is_some() {
            return true;
        }
    }

    false
}

/// Human-friendly credential backend label used in CLI messages.
pub fn credentials_store_label() -> &'static str {
    if keyring_disabled() {
        "local credentials file"
    } else {
        "system keychain"
    }
}

fn keyring_entry() -> Result<Entry, keyring::Error> {
    Entry::new(KEYRING_SERVICE, KEYRING_USERNAME)
}

fn fallback_credentials_path() -> KeyringResult<PathBuf> {
    if let Ok(path) = std::env::var("COSMOS_CREDENTIALS_FILE") {
        let trimmed = path.trim();
        if !trimmed.is_empty() {
            return Ok(PathBuf::from(trimmed));
        }
    }

    if cfg!(test) {
        return Ok(std::env::temp_dir().join("cosmos-test-credentials.json"));
    }

    dirs::config_dir()
        .map(|p| p.join("cosmos").join("credentials.json"))
        .ok_or_else(|| "Could not determine credentials file path".to_string())
}

fn read_fallback_credentials() -> KeyringResult<StoredCredentials> {
    let path = fallback_credentials_path()?;
    if !path.exists() {
        return Ok(StoredCredentials::default());
    }
    let json = fs::read_to_string(&path).map_err(|e| {
        format!(
            "Failed to read credentials file '{}': {}",
            path.display(),
            e
        )
    })?;
    serde_json::from_str(&json).map_err(|e| {
        format!(
            "Failed to parse credentials file '{}': {}",
            path.display(),
            e
        )
    })
}

fn write_fallback_credentials(creds: &StoredCredentials) -> KeyringResult<()> {
    let path = fallback_credentials_path()?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| {
            format!(
                "Failed to create credentials directory '{}': {}",
                parent.display(),
                e
            )
        })?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(parent, fs::Permissions::from_mode(0o700));
        }
    }

    let content = serde_json::to_string(creds)
        .map_err(|e| format!("Failed to serialize credentials: {}", e))?;
    #[cfg(unix)]
    {
        let tmp_path = path.with_extension("json.tmp");
        let mut tmp_file = fs::File::create(&tmp_path).map_err(|e| {
            format!(
                "Failed to create temp credentials file '{}': {}",
                tmp_path.display(),
                e
            )
        })?;
        use std::os::unix::fs::PermissionsExt;
        let _ = tmp_file.set_permissions(fs::Permissions::from_mode(0o600));
        tmp_file.write_all(content.as_bytes()).map_err(|e| {
            format!(
                "Failed to write credentials file '{}': {}",
                tmp_path.display(),
                e
            )
        })?;
        fs::rename(&tmp_path, &path).map_err(|e| {
            format!(
                "Failed to finalize credentials file '{}': {}",
                path.display(),
                e
            )
        })?;
    }

    #[cfg(not(unix))]
    {
        fs::write(&path, content).map_err(|e| {
            format!(
                "Failed to write credentials file '{}': {}",
                path.display(),
                e
            )
        })?;
    }
    Ok(())
}

/// Warn about keychain errors only once per session
pub fn warn_keychain_error_once(context: &str, err: &str) {
    if !debug_stderr_enabled() || KEYRING_ERROR_WARNED.swap(true, Ordering::Relaxed) {
        return;
    }
    eprintln!(
        "  Warning: Couldn't access system keychain for {}: {}",
        context, err
    );
    eprintln!("  Tip: When macOS prompts, choose \"Always Allow\" for cosmos.");
    eprintln!("  Tip: To bypass keychain prompts: export COSMOS_DISABLE_KEYRING=1");
    eprintln!(
        "  Tip: You can also set OPENROUTER_API_KEY and GITHUB_TOKEN env vars to bypass keychain."
    );
}

/// Read credentials from the unified keychain entry
fn read_credentials_uncached() -> KeyringResult<StoredCredentials> {
    if keyring_disabled() {
        return read_fallback_credentials();
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
fn write_credentials(creds: &StoredCredentials) -> KeyringResult<()> {
    if keyring_disabled() {
        return write_fallback_credentials(creds);
    }
    let entry = keyring_entry().map_err(|e| e.to_string())?;
    let json = serde_json::to_string(creds).expect("Failed to serialize credentials");
    entry.set_password(&json).map_err(|e| e.to_string())?;
    Ok(())
}

/// Read credentials with caching
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

    let creds = read_credentials_uncached()?;

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

#[cfg(test)]
fn reset_for_tests() {
    let cache = credentials_cache();
    let mut guard = match cache.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    guard.cached = None;
    KEYRING_ERROR_WARNED.store(false, Ordering::Relaxed);
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
    use std::time::{SystemTime, UNIX_EPOCH};

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

    #[test]
    fn test_credentials_store_label_uses_file_backend_in_tests() {
        assert_eq!(credentials_store_label(), "local credentials file");
    }

    #[test]
    fn test_file_backend_round_trip_for_tokens() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("cosmos-keyring-test-{}.json", unique));
        std::env::set_var("COSMOS_CREDENTIALS_FILE", &path);
        let _ = std::fs::remove_file(&path);
        reset_for_tests();

        set_api_key("sk-test-key").unwrap();
        set_github_token("ghp-test-token").unwrap();
        assert_eq!(get_api_key().unwrap(), Some("sk-test-key".to_string()));
        assert_eq!(
            get_github_token().unwrap(),
            Some("ghp-test-token".to_string())
        );

        let _ = std::fs::remove_file(&path);
        std::env::remove_var("COSMOS_CREDENTIALS_FILE");
        reset_for_tests();
    }
}
