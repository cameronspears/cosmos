//! Self-update functionality for Cosmos
//!
//! Provides version checking against crates.io and self-updating via
//! cargo install from crates.io.

use anyhow::{Context, Result};
use serde::Deserialize;

/// Current version of Cosmos (from Cargo.toml)
pub const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Information about an available update
#[derive(Debug, Clone)]
pub struct UpdateInfo {
    pub latest_version: String,
}

/// Response from crates.io API
#[derive(Debug, Deserialize)]
struct CrateResponse {
    #[serde(rename = "crate")]
    krate: CrateInfo,
}

#[derive(Debug, Deserialize)]
struct CrateInfo {
    max_stable_version: String,
}

/// Check crates.io for the latest version
///
/// Returns `Some(UpdateInfo)` if a newer version is available, `None` if up to date.
pub async fn check_for_update() -> Result<Option<UpdateInfo>> {
    let client = reqwest::Client::builder()
        .user_agent(format!("cosmos-tui/{}", CURRENT_VERSION))
        .build()
        .context("Failed to create HTTP client")?;

    let url = "https://crates.io/api/v1/crates/cosmos-tui";
    let response: CrateResponse = client
        .get(url)
        .send()
        .await
        .context("Failed to fetch version info from crates.io")?
        .json()
        .await
        .context("Failed to parse crates.io response")?;

    let latest = &response.krate.max_stable_version;

    if is_newer_version(latest, CURRENT_VERSION) {
        Ok(Some(UpdateInfo {
            latest_version: latest.clone(),
        }))
    } else {
        Ok(None)
    }
}

/// Extract the most useful error message from cargo stderr output
///
/// Looks for lines starting with "error:" or "error[" first (actual error messages),
/// then falls back to the last non-empty line.
fn extract_cargo_error(stderr: &str) -> &str {
    stderr
        .lines()
        .find(|line| line.trim().starts_with("error:") || line.trim().starts_with("error["))
        .or_else(|| stderr.lines().rfind(|l| !l.trim().is_empty()))
        .map(|s| s.trim())
        .unwrap_or("unknown error")
}

/// Compare two semver version strings
/// Returns true if `latest` is newer than `current`
fn is_newer_version(latest: &str, current: &str) -> bool {
    let parse = |v: &str| -> Option<(u32, u32, u32)> {
        let parts: Vec<&str> = v.trim_start_matches('v').split('.').collect();
        if parts.len() >= 3 {
            Some((
                parts[0].parse().ok()?,
                parts[1].parse().ok()?,
                parts[2].split('-').next()?.parse().ok()?,
            ))
        } else {
            None
        }
    };

    match (parse(latest), parse(current)) {
        (Some((l_major, l_minor, l_patch)), Some((c_major, c_minor, c_patch))) => {
            (l_major, l_minor, l_patch) > (c_major, c_minor, c_patch)
        }
        _ => false,
    }
}

/// Install the latest version from crates.io using cargo
///
/// This function runs `cargo install cosmos-tui --force --profile release-dist`
/// to update to the latest version,
/// then exec()s into the new binary.
///
/// On success, this function does not return (the process is replaced).
pub fn run_update<F>(target_version: &str, on_progress: F) -> Result<()>
where
    F: Fn(u8) + Send + 'static,
{
    use std::process::Command;

    // Check if cargo is available first
    let cargo_check = Command::new("cargo").arg("--version").output();
    if cargo_check.is_err() || !cargo_check.as_ref().unwrap().status.success() {
        return Err(anyhow::anyhow!(
            "Cargo is not installed. Please install Rust from https://rustup.rs"
        ));
    }

    // Starting download/compile
    on_progress(10);

    // Run cargo install to update
    // Note: We don't use --locked because it requires exact Cargo.lock match,
    // which can fail if dependencies have been updated since publishing.
    let output = Command::new("cargo")
        .args([
            "install",
            "cosmos-tui",
            "--force",
            "--profile",
            "release-dist",
        ])
        .output()
        .context("Failed to run cargo install")?;

    // Update complete
    on_progress(100);

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let error_msg = extract_cargo_error(&stderr);
        return Err(anyhow::anyhow!(
            "{}. Try manually: cargo install cosmos-tui --force --profile release-dist",
            error_msg
        ));
    }

    // Binary was replaced, now exec into the new version
    exec_new_binary().map_err(|e| {
        anyhow::anyhow!(
            "Update installed (v{}) but failed to restart: {}",
            target_version,
            e
        )
    })
}

/// Replace the current process with the new binary
///
/// On Unix, uses exec() to replace the process in-place.
/// On Windows, spawns the new process and exits.
fn exec_new_binary() -> Result<()> {
    let exe_path = std::env::current_exe().context("Failed to get current executable path")?;

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;

        // Get the original arguments (skip the program name)
        let args: Vec<String> = std::env::args().skip(1).collect();

        // exec() replaces the current process - this never returns on success
        let err = std::process::Command::new(&exe_path).args(&args).exec();

        // If we get here, exec failed
        Err(anyhow::anyhow!("exec failed: {}", err))
    }

    #[cfg(windows)]
    {
        use std::process::Command;

        // Get the original arguments
        let args: Vec<String> = std::env::args().skip(1).collect();

        // Spawn the new process
        Command::new(&exe_path)
            .args(&args)
            .spawn()
            .context("Failed to spawn new process")?;

        // Exit the current process
        std::process::exit(0);
    }

    #[cfg(not(any(unix, windows)))]
    {
        Err(anyhow::anyhow!(
            "Self-update restart not supported on this platform"
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_version_comparison_basic() {
        // Newer versions
        assert!(is_newer_version("0.4.0", "0.3.0"));
        assert!(is_newer_version("1.0.0", "0.9.9"));
        assert!(is_newer_version("0.3.1", "0.3.0"));
        assert!(is_newer_version("0.3.10", "0.3.9"));
        assert!(is_newer_version("2.0.0", "1.99.99"));

        // Same version
        assert!(!is_newer_version("0.3.0", "0.3.0"));
        assert!(!is_newer_version("1.0.0", "1.0.0"));

        // Older versions
        assert!(!is_newer_version("0.2.0", "0.3.0"));
        assert!(!is_newer_version("0.2.9", "0.3.0"));
        assert!(!is_newer_version("0.3.0", "0.3.1"));
    }

    #[test]
    fn test_version_comparison_with_v_prefix() {
        // v prefix should be handled
        assert!(is_newer_version("v0.4.0", "0.3.0"));
        assert!(is_newer_version("0.4.0", "v0.3.0"));
        assert!(is_newer_version("v0.4.0", "v0.3.0"));
        assert!(!is_newer_version("v0.3.0", "v0.3.0"));
    }

    #[test]
    fn test_version_comparison_prerelease() {
        // Prerelease suffixes should be stripped for comparison
        assert!(is_newer_version("0.4.0", "0.3.0-beta"));
        assert!(is_newer_version("0.4.0-alpha", "0.3.0"));
        assert!(!is_newer_version("0.3.0-beta", "0.3.0"));
    }

    #[test]
    fn test_version_comparison_invalid() {
        // Invalid versions should return false (safe default)
        assert!(!is_newer_version("invalid", "0.3.0"));
        assert!(!is_newer_version("0.3.0", "invalid"));
        assert!(!is_newer_version("", "0.3.0"));
        assert!(!is_newer_version("0.3.0", ""));
        assert!(!is_newer_version("1.0", "0.3.0")); // Only 2 parts
        assert!(!is_newer_version("0.3.0", "1.0")); // Only 2 parts
    }

    #[test]
    fn test_current_version_is_valid() {
        // Ensure CURRENT_VERSION can be parsed
        let parts: Vec<&str> = CURRENT_VERSION.split('.').collect();
        assert_eq!(parts.len(), 3, "CURRENT_VERSION should have 3 parts");
        assert!(
            parts[0].parse::<u32>().is_ok(),
            "Major version should be numeric"
        );
        assert!(
            parts[1].parse::<u32>().is_ok(),
            "Minor version should be numeric"
        );
        // Patch may have prerelease suffix
        let patch = parts[2].split('-').next().unwrap();
        assert!(
            patch.parse::<u32>().is_ok(),
            "Patch version should be numeric"
        );
    }

    #[test]
    fn test_update_info_creation() {
        let info = UpdateInfo {
            latest_version: "0.4.0".to_string(),
        };
        assert_eq!(info.latest_version, "0.4.0");
    }

    #[test]
    fn test_version_comparison_major_bump() {
        // Major version bumps should always be newer
        assert!(is_newer_version("1.0.0", "0.99.99"));
        assert!(is_newer_version("2.0.0", "1.99.99"));
        assert!(is_newer_version("10.0.0", "9.99.99"));
    }

    #[test]
    fn test_version_comparison_minor_bump() {
        // Minor version bumps within same major
        assert!(is_newer_version("0.4.0", "0.3.99"));
        assert!(is_newer_version("1.2.0", "1.1.99"));
    }

    #[test]
    fn test_version_comparison_patch_bump() {
        // Patch version bumps within same minor
        assert!(is_newer_version("0.3.5", "0.3.4"));
        assert!(is_newer_version("0.3.100", "0.3.99"));
    }

    #[test]
    fn test_extract_cargo_error_finds_error_line() {
        // Should find the "error:" line
        let stderr = r#"
    Updating crates.io index
    Compiling some-crate v1.0.0
error: failed to download `some-crate v1.0.0`
    Caused by: network error
"#;
        assert_eq!(
            extract_cargo_error(stderr),
            "error: failed to download `some-crate v1.0.0`"
        );
    }

    #[test]
    fn test_extract_cargo_error_finds_error_code() {
        // Should find "error[E0123]:" style lines
        let stderr = r#"
   Compiling cosmos-tui v0.5.0
error[E0433]: failed to resolve: use of undeclared crate or module
   --> src/main.rs:1:5
"#;
        assert!(extract_cargo_error(stderr).starts_with("error[E0433]"));
    }

    #[test]
    fn test_extract_cargo_error_falls_back_to_last_line() {
        // When no "error:" line, use last non-empty line
        let stderr = r#"
warning: some warning
note: some note
The operation failed
"#;
        assert_eq!(extract_cargo_error(stderr), "The operation failed");
    }

    #[test]
    fn test_extract_cargo_error_empty_stderr() {
        // Empty stderr returns unknown error
        assert_eq!(extract_cargo_error(""), "unknown error");
        assert_eq!(extract_cargo_error("   \n  \n  "), "unknown error");
    }

    #[test]
    fn test_extract_cargo_error_trims_whitespace() {
        // Should trim leading/trailing whitespace from the error
        let stderr = "   error: some problem   \n";
        assert_eq!(extract_cargo_error(stderr), "error: some problem");
    }

    #[test]
    fn test_extract_cargo_error_prefers_first_error() {
        // When multiple error lines exist, find the first one
        let stderr = r#"
error: first error
error: second error
"#;
        assert_eq!(extract_cargo_error(stderr), "error: first error");
    }

    #[test]
    fn test_extract_cargo_error_real_cargo_output() {
        // Test with realistic cargo install failure output
        let stderr = r#"
    Updating crates.io index
error: could not find `nonexistent-crate` in registry `crates-io`
"#;
        assert_eq!(
            extract_cargo_error(stderr),
            "error: could not find `nonexistent-crate` in registry `crates-io`"
        );
    }

    #[test]
    fn test_extract_cargo_error_network_failure() {
        // Test network-related error
        let stderr = r#"
    Updating crates.io index
warning: spurious network error
error: failed to fetch `https://github.com/rust-lang/crates.io-index`
"#;
        assert!(extract_cargo_error(stderr).contains("failed to fetch"));
    }
}
