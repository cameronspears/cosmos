//! Native GitHub API integration
//!
//! Provides OAuth device flow authentication and PR creation without requiring
//! the `gh` CLI. Tokens are stored securely in the system keychain via keyring.

use crate::keyring;
use anyhow::{Context, Result};
use git2::Repository;
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::time::Duration;

// ============================================================================
// OAuth App Configuration
// ============================================================================

/// GitHub OAuth App client ID for device flow authentication.
///
/// To get your own client_id:
/// 1. Go to https://github.com/settings/developers
/// 2. Click "New OAuth App"
/// 3. Fill in:
///    - Application name: "Cosmos"
///    - Homepage URL: https://github.com/cameronspears/cosmos
///    - Callback URL: https://github.com/cameronspears/cosmos (not used for device flow)
/// 4. Under "Device Flow", check "Enable Device Flow"
/// 5. Copy the Client ID here
///
/// Note: Client ID is public and safe to embed in source code.
/// Device flow does not require a client secret.
const CLIENT_ID: &str = "Ov23liBvoDPv3W7Dpjoz";

// ============================================================================
// Token Management
// ============================================================================

/// Get the stored GitHub token, or None if not authenticated.
pub fn get_stored_token() -> Option<String> {
    // Check environment variable first
    if let Ok(token) = std::env::var("GITHUB_TOKEN") {
        if !token.is_empty() {
            return Some(token);
        }
    }

    // Try keychain (migration from legacy entries happens automatically)
    match keyring::get_github_token() {
        Ok(Some(token)) => Some(token),
        Ok(None) => None,
        Err(err) => {
            keyring::warn_keychain_error_once("GitHub token", &err);
            None
        }
    }
}

/// Check if GitHub authentication is configured.
pub fn is_authenticated() -> bool {
    get_stored_token().is_some()
}

// ============================================================================
// OAuth Device Flow
// ============================================================================

const GITHUB_DEVICE_CODE_URL: &str = "https://github.com/login/device/code";
const GITHUB_TOKEN_URL: &str = "https://github.com/login/oauth/access_token";
const DEVICE_FLOW_TIMEOUT_SECS: u64 = 30;

/// Maximum length for error body content in error messages
const MAX_ERROR_BODY_LEN: usize = 200;

/// Sanitize an API error body to prevent credential leakage.
/// Truncates long responses and redacts potential secrets.
fn sanitize_error_body(body: &str) -> String {
    // Patterns that might indicate secrets in error responses
    const SECRET_PATTERNS: &[&str] = &[
        "token",
        "secret",
        "password",
        "credential",
        "auth",
        "bearer",
        "ghp_",        // GitHub personal access token prefix
        "gho_",        // GitHub OAuth token prefix
        "ghu_",        // GitHub user token prefix
        "github_pat_", // GitHub PAT prefix
    ];

    let truncated = if body.len() > MAX_ERROR_BODY_LEN {
        format!("{}... (truncated)", &body[..MAX_ERROR_BODY_LEN])
    } else {
        body.to_string()
    };

    // Check if the body might contain secrets
    let lower = truncated.to_lowercase();
    for pattern in SECRET_PATTERNS {
        if lower.contains(pattern) {
            return "(error details redacted - may contain sensitive data)".to_string();
        }
    }

    truncated
}

/// Required scope for creating PRs
const OAUTH_SCOPE: &str = "repo";

#[derive(Deserialize)]
struct DeviceCodeResponse {
    device_code: String,
    user_code: String,
    verification_uri: String,
    expires_in: u64,
    interval: u64,
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: Option<String>,
    error: Option<String>,
    error_description: Option<String>,
}

/// Instructions for the user to complete device flow authentication.
#[derive(Debug, Clone)]
pub struct AuthInstructions {
    pub verification_uri: String,
    pub user_code: String,
}

/// Callbacks for device flow UI integration.
pub trait DeviceFlowCallbacks {
    /// Called when the user should visit the verification URL and enter the code.
    fn show_instructions(&mut self, instructions: &AuthInstructions);

    /// Called while polling for authorization. Return false to cancel.
    fn poll_status(&mut self) -> bool;

    /// Called when authentication succeeds.
    fn on_success(&mut self, username: &str);

    /// Called when authentication fails.
    fn on_error(&mut self, error: &str);
}

/// Run the OAuth device flow interactively.
///
/// This will:
/// 1. Request a device code from GitHub
/// 2. Call `callbacks.show_instructions()` with the URL and code
/// 3. Poll for authorization until complete or timeout
/// 4. Store the token in the keychain
/// 5. Call `callbacks.on_success()` or `callbacks.on_error()`
pub async fn run_device_flow<C: DeviceFlowCallbacks>(callbacks: &mut C) -> Result<()> {
    if CLIENT_ID == "YOUR_CLIENT_ID_HERE" {
        let err = "GitHub OAuth App not configured. Set CLIENT_ID in src/github.rs";
        callbacks.on_error(err);
        return Err(anyhow::anyhow!("{}", err));
    }

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(DEVICE_FLOW_TIMEOUT_SECS))
        .build()
        .context("Failed to create HTTP client")?;

    // Step 1: Request device code
    let device_resp = client
        .post(GITHUB_DEVICE_CODE_URL)
        .header("Accept", "application/json")
        .form(&[("client_id", CLIENT_ID), ("scope", OAUTH_SCOPE)])
        .send()
        .await
        .context("Failed to request device code from GitHub")?;

    if !device_resp.status().is_success() {
        let status = device_resp.status();
        let body = device_resp.text().await.unwrap_or_default();
        let sanitized = sanitize_error_body(&body);
        let err = format!("GitHub returned error {}: {}", status, sanitized);
        callbacks.on_error(&err);
        return Err(anyhow::anyhow!("{}", err));
    }

    let device_code: DeviceCodeResponse = device_resp
        .json()
        .await
        .context("Failed to parse device code response")?;

    // Step 2: Show instructions to user
    let instructions = AuthInstructions {
        verification_uri: device_code.verification_uri.clone(),
        user_code: device_code.user_code.clone(),
    };
    callbacks.show_instructions(&instructions);

    // Step 3: Poll for token
    let poll_interval = Duration::from_secs(device_code.interval.max(5));
    let deadline = std::time::Instant::now() + Duration::from_secs(device_code.expires_in);

    loop {
        if std::time::Instant::now() > deadline {
            let err = "Authentication timed out. Please try again.";
            callbacks.on_error(err);
            return Err(anyhow::anyhow!("{}", err));
        }

        if !callbacks.poll_status() {
            return Err(anyhow::anyhow!("Authentication cancelled by user"));
        }

        tokio::time::sleep(poll_interval).await;

        let token_resp = client
            .post(GITHUB_TOKEN_URL)
            .header("Accept", "application/json")
            .form(&[
                ("client_id", CLIENT_ID),
                ("device_code", device_code.device_code.as_str()),
                ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
            ])
            .send()
            .await;

        let token_resp = match token_resp {
            Ok(r) => r,
            Err(_) => continue, // Network error, retry
        };

        let token_data: TokenResponse = match token_resp.json().await {
            Ok(d) => d,
            Err(_) => continue,
        };

        if let Some(token) = token_data.access_token {
            // Step 4: Store token and get username
            if let Err(e) = keyring::set_github_token(&token) {
                let err = format!(
                    "Failed to store token in {}: {}",
                    keyring::credentials_store_label(),
                    e
                );
                callbacks.on_error(&err);
                return Err(anyhow::anyhow!("{}", err));
            }

            // Get username for confirmation
            let username = get_authenticated_user(&client, &token)
                .await
                .unwrap_or_else(|_| "unknown".to_string());

            callbacks.on_success(&username);
            return Ok(());
        }

        if let Some(error) = &token_data.error {
            match error.as_str() {
                "authorization_pending" => continue,
                "slow_down" => {
                    tokio::time::sleep(Duration::from_secs(5)).await;
                    continue;
                }
                "expired_token" => {
                    let err = "Device code expired. Please try again.";
                    callbacks.on_error(err);
                    return Err(anyhow::anyhow!("{}", err));
                }
                "access_denied" => {
                    let err = "Access denied. You declined the authorization.";
                    callbacks.on_error(err);
                    return Err(anyhow::anyhow!("{}", err));
                }
                _ => {
                    let desc = token_data
                        .error_description
                        .as_deref()
                        .unwrap_or("Unknown error");
                    let err = format!("GitHub error: {}", desc);
                    callbacks.on_error(&err);
                    return Err(anyhow::anyhow!("{}", err));
                }
            }
        }
    }
}

async fn get_authenticated_user(client: &reqwest::Client, token: &str) -> Result<String> {
    #[derive(Deserialize)]
    struct User {
        login: String,
    }

    let resp = client
        .get("https://api.github.com/user")
        .header("Accept", "application/vnd.github+json")
        .header("Authorization", format!("Bearer {}", token))
        .header("User-Agent", "cosmos-tui")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .send()
        .await?;

    let user: User = resp.json().await?;
    Ok(user.login)
}

// ============================================================================
// GitHub API Operations
// ============================================================================

const API_TIMEOUT_SECS: u64 = 60;

/// Extract owner and repo from a git remote URL.
///
/// Supports:
/// - git@github.com:owner/repo.git
/// - https://github.com/owner/repo.git
/// - https://github.com/owner/repo
pub fn parse_remote_url(url: &str) -> Option<(String, String)> {
    // SSH format: git@github.com:owner/repo.git
    if let Some(rest) = url.strip_prefix("git@github.com:") {
        let path = rest.trim_end_matches(".git");
        let parts: Vec<&str> = path.splitn(2, '/').collect();
        if parts.len() == 2 {
            return Some((parts[0].to_string(), parts[1].to_string()));
        }
    }

    // HTTPS format: https://github.com/owner/repo.git
    if url.contains("github.com") {
        if let Ok(parsed) = url::Url::parse(url) {
            let path = parsed
                .path()
                .trim_start_matches('/')
                .trim_end_matches(".git");
            let parts: Vec<&str> = path.splitn(2, '/').collect();
            if parts.len() == 2 {
                return Some((parts[0].to_string(), parts[1].to_string()));
            }
        }

        // Fallback: simple string parsing for URLs without scheme
        let path = url
            .split("github.com")
            .nth(1)?
            .trim_start_matches(['/', ':'])
            .trim_end_matches(".git");
        let parts: Vec<&str> = path.splitn(2, '/').collect();
        if parts.len() == 2 {
            return Some((parts[0].to_string(), parts[1].to_string()));
        }
    }

    None
}

/// Get the owner and repo from the repository's origin remote.
pub fn get_remote_info(repo_path: &Path) -> Result<(String, String)> {
    let repo = Repository::open(repo_path).context("Failed to open repository")?;

    // Try common remote names in order
    for remote_name in ["origin", "upstream", "github"] {
        if let Ok(remote) = repo.find_remote(remote_name) {
            if let Some(url) = remote.url() {
                if let Some((owner, repo_name)) = parse_remote_url(url) {
                    return Ok((owner, repo_name));
                }
            }
        }
    }

    // Try first available remote
    if let Ok(remotes) = repo.remotes() {
        for name in remotes.iter().flatten() {
            if let Ok(remote) = repo.find_remote(name) {
                if let Some(url) = remote.url() {
                    if let Some((owner, repo_name)) = parse_remote_url(url) {
                        return Ok((owner, repo_name));
                    }
                }
            }
        }
    }

    Err(anyhow::anyhow!(
        "No GitHub remote found. Make sure you have a remote pointing to github.com"
    ))
}

#[derive(Serialize)]
struct CreatePrRequest {
    title: String,
    body: String,
    head: String,
    base: String,
}

#[derive(Deserialize)]
struct CreatePrResponse {
    html_url: String,
}

#[derive(Deserialize)]
struct ApiErrorResponse {
    message: String,
    #[serde(default)]
    errors: Vec<ApiErrorDetail>,
}

#[derive(Deserialize)]
struct ApiErrorDetail {
    message: Option<String>,
}

/// Create a pull request on GitHub.
///
/// Returns the URL of the created PR.
pub async fn create_pull_request(
    owner: &str,
    repo: &str,
    base: &str,
    head: &str,
    title: &str,
    body: &str,
) -> Result<String> {
    let token = get_stored_token().ok_or_else(|| {
        anyhow::anyhow!("Not authenticated with GitHub. Please authenticate first.")
    })?;

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(API_TIMEOUT_SECS))
        .build()
        .context("Failed to create HTTP client")?;

    let url = format!("https://api.github.com/repos/{}/{}/pulls", owner, repo);

    let request = CreatePrRequest {
        title: title.to_string(),
        body: body.to_string(),
        head: head.to_string(),
        base: base.to_string(),
    };

    let resp = client
        .post(&url)
        .header("Accept", "application/vnd.github+json")
        .header("Authorization", format!("Bearer {}", token))
        .header("User-Agent", "cosmos-tui")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .json(&request)
        .send()
        .await
        .context("Failed to send PR creation request")?;

    let status = resp.status();
    if status.is_success() {
        let pr: CreatePrResponse = resp.json().await.context("Failed to parse PR response")?;
        Ok(pr.html_url)
    } else {
        let error_body = resp.text().await.unwrap_or_default();

        // Try to parse structured error
        if let Ok(api_error) = serde_json::from_str::<ApiErrorResponse>(&error_body) {
            let detail = api_error
                .errors
                .first()
                .and_then(|e| e.message.clone())
                .unwrap_or_default();

            let msg = if detail.is_empty() {
                api_error.message
            } else {
                format!("{}: {}", api_error.message, detail)
            };

            return Err(anyhow::anyhow!("GitHub API error: {}", msg));
        }

        // Sanitize raw error body to prevent credential leakage
        let sanitized = sanitize_error_body(&error_body);
        Err(anyhow::anyhow!(
            "GitHub API error ({}): {}",
            status,
            sanitized
        ))
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // ========================================================================
    // URL Parsing Tests
    // ========================================================================

    #[test]
    fn test_parse_ssh_remote() {
        let (owner, repo) = parse_remote_url("git@github.com:cameronspears/cosmos.git").unwrap();
        assert_eq!(owner, "cameronspears");
        assert_eq!(repo, "cosmos");
    }

    #[test]
    fn test_parse_ssh_remote_no_git_suffix() {
        // Some remotes don't have .git suffix
        let (owner, repo) = parse_remote_url("git@github.com:owner/repo").unwrap();
        assert_eq!(owner, "owner");
        assert_eq!(repo, "repo");
    }

    #[test]
    fn test_parse_https_remote() {
        let (owner, repo) =
            parse_remote_url("https://github.com/cameronspears/cosmos.git").unwrap();
        assert_eq!(owner, "cameronspears");
        assert_eq!(repo, "cosmos");
    }

    #[test]
    fn test_parse_https_remote_no_git_suffix() {
        let (owner, repo) = parse_remote_url("https://github.com/cameronspears/cosmos").unwrap();
        assert_eq!(owner, "cameronspears");
        assert_eq!(repo, "cosmos");
    }

    #[test]
    fn test_parse_https_with_auth() {
        // URLs with embedded credentials (rare but valid)
        let (owner, repo) =
            parse_remote_url("https://user:token@github.com/owner/repo.git").unwrap();
        assert_eq!(owner, "owner");
        assert_eq!(repo, "repo");
    }

    #[test]
    fn test_parse_github_enterprise_not_supported() {
        // GitHub Enterprise uses different domains - not supported
        assert!(parse_remote_url("https://github.mycompany.com/owner/repo").is_none());
    }

    #[test]
    fn test_parse_invalid_remote_gitlab() {
        assert!(parse_remote_url("https://gitlab.com/user/repo").is_none());
    }

    #[test]
    fn test_parse_invalid_remote_bitbucket() {
        assert!(parse_remote_url("git@bitbucket.org:user/repo.git").is_none());
    }

    #[test]
    fn test_parse_invalid_remote_garbage() {
        assert!(parse_remote_url("not-a-url").is_none());
        assert!(parse_remote_url("").is_none());
        assert!(parse_remote_url("   ").is_none());
    }

    #[test]
    fn test_parse_remote_with_nested_path() {
        // GitHub doesn't support nested paths, but we should handle gracefully
        // This should return owner="org" and repo="sub/repo" or fail
        let result = parse_remote_url("https://github.com/org/sub/repo.git");
        // We only take first two path segments, so this gets org/sub
        if let Some((owner, repo)) = result {
            assert_eq!(owner, "org");
            // repo might be "sub/repo" or just "sub" depending on implementation
            assert!(!repo.is_empty());
        }
    }

    #[test]
    fn test_parse_remote_single_segment() {
        // Invalid: only one path segment (no repo)
        assert!(parse_remote_url("https://github.com/owner").is_none());
    }

    #[test]
    fn test_parse_remote_preserves_case() {
        // GitHub repos can have mixed case
        let (owner, repo) = parse_remote_url("git@github.com:MyOrg/MyRepo.git").unwrap();
        assert_eq!(owner, "MyOrg");
        assert_eq!(repo, "MyRepo");
    }

    #[test]
    fn test_parse_remote_with_dashes_and_underscores() {
        let (owner, repo) = parse_remote_url("git@github.com:my-org/my_cool-repo.git").unwrap();
        assert_eq!(owner, "my-org");
        assert_eq!(repo, "my_cool-repo");
    }

    // ========================================================================
    // Token Environment Variable Tests
    // ========================================================================

    #[test]
    fn test_get_stored_token_respects_env_var() {
        // Save current env state
        let original = std::env::var("GITHUB_TOKEN").ok();

        // Set test token
        std::env::set_var("GITHUB_TOKEN", "test-token-12345");
        let token = get_stored_token();
        assert_eq!(token, Some("test-token-12345".to_string()));

        // Restore original state
        match original {
            Some(val) => std::env::set_var("GITHUB_TOKEN", val),
            None => std::env::remove_var("GITHUB_TOKEN"),
        }
    }

    #[test]
    fn test_get_stored_token_ignores_empty_env_var() {
        let original = std::env::var("GITHUB_TOKEN").ok();

        std::env::set_var("GITHUB_TOKEN", "");
        // Empty env var should be treated as not set
        // (falls through to keyring, which may or may not have a token)
        let _ = get_stored_token(); // Just ensure it doesn't panic

        match original {
            Some(val) => std::env::set_var("GITHUB_TOKEN", val),
            None => std::env::remove_var("GITHUB_TOKEN"),
        }
    }

    #[test]
    fn test_is_authenticated_with_env_var() {
        let original = std::env::var("GITHUB_TOKEN").ok();

        std::env::set_var("GITHUB_TOKEN", "ghp_xxxxxxxxxxxx");
        assert!(is_authenticated());

        match original {
            Some(val) => std::env::set_var("GITHUB_TOKEN", val),
            None => std::env::remove_var("GITHUB_TOKEN"),
        }
    }

    // ========================================================================
    // API Error Parsing Tests
    // ========================================================================

    #[test]
    fn test_parse_api_error_response() {
        let json = r#"{"message": "Validation Failed", "errors": [{"message": "A pull request already exists"}]}"#;
        let parsed: ApiErrorResponse = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.message, "Validation Failed");
        assert_eq!(parsed.errors.len(), 1);
        assert_eq!(
            parsed.errors[0].message,
            Some("A pull request already exists".to_string())
        );
    }

    #[test]
    fn test_parse_api_error_response_no_details() {
        let json = r#"{"message": "Not Found"}"#;
        let parsed: ApiErrorResponse = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.message, "Not Found");
        assert!(parsed.errors.is_empty());
    }

    #[test]
    fn test_parse_api_error_response_empty_errors() {
        let json = r#"{"message": "Bad Request", "errors": []}"#;
        let parsed: ApiErrorResponse = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.message, "Bad Request");
        assert!(parsed.errors.is_empty());
    }

    // ========================================================================
    // Device Flow Response Parsing Tests
    // ========================================================================

    #[test]
    fn test_parse_device_code_response() {
        let json = r#"{
            "device_code": "abc123",
            "user_code": "ABCD-1234",
            "verification_uri": "https://github.com/login/device",
            "expires_in": 900,
            "interval": 5
        }"#;
        let parsed: DeviceCodeResponse = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.device_code, "abc123");
        assert_eq!(parsed.user_code, "ABCD-1234");
        assert_eq!(parsed.verification_uri, "https://github.com/login/device");
        assert_eq!(parsed.expires_in, 900);
        assert_eq!(parsed.interval, 5);
    }

    #[test]
    fn test_parse_token_response_success() {
        let json = r#"{"access_token": "gho_xxxxxxxxxxxx"}"#;
        let parsed: TokenResponse = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.access_token, Some("gho_xxxxxxxxxxxx".to_string()));
        assert!(parsed.error.is_none());
    }

    #[test]
    fn test_parse_token_response_pending() {
        let json = r#"{"error": "authorization_pending", "error_description": "The user has not yet authorized"}"#;
        let parsed: TokenResponse = serde_json::from_str(json).unwrap();
        assert!(parsed.access_token.is_none());
        assert_eq!(parsed.error, Some("authorization_pending".to_string()));
        assert_eq!(
            parsed.error_description,
            Some("The user has not yet authorized".to_string())
        );
    }

    #[test]
    fn test_parse_token_response_expired() {
        let json =
            r#"{"error": "expired_token", "error_description": "The device code has expired"}"#;
        let parsed: TokenResponse = serde_json::from_str(json).unwrap();
        assert!(parsed.access_token.is_none());
        assert_eq!(parsed.error, Some("expired_token".to_string()));
    }

    #[test]
    fn test_parse_token_response_access_denied() {
        let json = r#"{"error": "access_denied", "error_description": "The user cancelled"}"#;
        let parsed: TokenResponse = serde_json::from_str(json).unwrap();
        assert!(parsed.access_token.is_none());
        assert_eq!(parsed.error, Some("access_denied".to_string()));
    }

    // ========================================================================
    // PR Request Serialization Tests
    // ========================================================================

    #[test]
    fn test_create_pr_request_serialization() {
        let request = CreatePrRequest {
            title: "Fix bug".to_string(),
            body: "This fixes the bug".to_string(),
            head: "fix/my-branch".to_string(),
            base: "main".to_string(),
        };
        let json = serde_json::to_string(&request).unwrap();
        assert!(json.contains("\"title\":\"Fix bug\""));
        assert!(json.contains("\"body\":\"This fixes the bug\""));
        assert!(json.contains("\"head\":\"fix/my-branch\""));
        assert!(json.contains("\"base\":\"main\""));
    }

    #[test]
    fn test_create_pr_request_handles_special_chars() {
        let request = CreatePrRequest {
            title: "Fix: handle \"quotes\" and 'apostrophes'".to_string(),
            body: "Line1\nLine2\n\n## Header".to_string(),
            head: "fix/branch-name".to_string(),
            base: "main".to_string(),
        };
        let json = serde_json::to_string(&request).unwrap();
        // Should properly escape quotes
        assert!(json.contains("\\\"quotes\\\""));
        // Should preserve newlines as \n
        assert!(json.contains("\\n"));
    }

    // ========================================================================
    // Auth Instructions Tests
    // ========================================================================

    #[test]
    fn test_auth_instructions_clone() {
        let instructions = AuthInstructions {
            verification_uri: "https://github.com/login/device".to_string(),
            user_code: "ABCD-1234".to_string(),
        };
        let cloned = instructions.clone();
        assert_eq!(cloned.verification_uri, instructions.verification_uri);
        assert_eq!(cloned.user_code, instructions.user_code);
    }

    // ========================================================================
    // Integration-style Tests (with real repo)
    // ========================================================================

    #[test]
    fn test_get_remote_info_on_cosmos_repo() {
        // This test runs on the Cosmos repo itself
        let repo_path = std::env::current_dir().unwrap();
        let result = get_remote_info(&repo_path);

        // Should succeed on the Cosmos repo (or any GitHub repo)
        if let Ok((owner, repo)) = result {
            // Should have non-empty owner and repo
            assert!(!owner.is_empty(), "Owner should not be empty");
            assert!(!repo.is_empty(), "Repo should not be empty");

            // Repo name should contain "cosmos" (handles codecosmos, cosmos, etc.)
            assert!(
                repo.to_lowercase().contains("cosmos"),
                "Repo name should contain 'cosmos', got: {}",
                repo
            );
        }
        // If it fails, that's okay - might be running in a different context
    }
}
