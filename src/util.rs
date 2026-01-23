use std::io::{BufReader, Read};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};

pub fn truncate(s: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }

    let char_count = s.chars().count();
    if char_count <= max {
        return s.to_string();
    }

    if max <= 3 {
        return s.chars().take(max).collect();
    }

    let truncated: String = s.chars().take(max - 3).collect();
    format!("{}...", truncated)
}

#[derive(Debug)]
pub struct CommandRunResult {
    pub status: Option<ExitStatus>,
    pub stdout: String,
    pub stderr: String,
    pub timed_out: bool,
}

pub fn run_command_with_timeout(
    command: &mut Command,
    timeout: Duration,
) -> Result<CommandRunResult, String> {
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("Failed to start command: {}", e))?;

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "Failed to capture stdout".to_string())?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| "Failed to capture stderr".to_string())?;

    let stdout_handle = thread::spawn(move || {
        let mut buf = Vec::new();
        let mut reader = BufReader::new(stdout);
        let _ = reader.read_to_end(&mut buf);
        buf
    });
    let stderr_handle = thread::spawn(move || {
        let mut buf = Vec::new();
        let mut reader = BufReader::new(stderr);
        let _ = reader.read_to_end(&mut buf);
        buf
    });

    let start = Instant::now();
    let mut timed_out = false;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) => {
                if start.elapsed() >= timeout {
                    timed_out = true;
                    let _ = child.kill();
                    match child.wait() {
                        Ok(status) => break Some(status),
                        Err(_) => break None,
                    }
                }
                thread::sleep(Duration::from_millis(50));
            }
            Err(e) => return Err(format!("Failed to wait for command: {}", e)),
        }
    };

    let stdout_bytes = stdout_handle.join().unwrap_or_default();
    let stderr_bytes = stderr_handle.join().unwrap_or_default();

    Ok(CommandRunResult {
        status,
        stdout: String::from_utf8_lossy(&stdout_bytes).to_string(),
        stderr: String::from_utf8_lossy(&stderr_bytes).to_string(),
        timed_out,
    })
}

pub struct RepoPath {
    pub absolute: PathBuf,
    pub relative: PathBuf,
}

pub fn resolve_repo_path_allow_new(repo_root: &Path, candidate: &Path) -> Result<RepoPath, String> {
    if candidate.as_os_str().is_empty() {
        return Err("Path is empty".to_string());
    }
    if candidate.is_absolute() {
        return Err(format!(
            "Absolute paths are not allowed: {}",
            candidate.display()
        ));
    }
    if candidate
        .components()
        .any(|c| matches!(c, Component::ParentDir))
    {
        return Err(format!(
            "Parent traversal is not allowed: {}",
            candidate.display()
        ));
    }

    let root = repo_root
        .canonicalize()
        .map_err(|e| format!("Failed to resolve repo root: {}", e))?;
    let joined = root.join(candidate);
    let parent = joined
        .parent()
        .ok_or_else(|| format!("Invalid path: {}", candidate.display()))?;
    let parent_canon = canonicalize_existing_parent(parent)?;

    if !parent_canon.starts_with(&root) {
        return Err(format!("Path escapes repository: {}", candidate.display()));
    }

    let relative = joined
        .strip_prefix(&root)
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|_| candidate.to_path_buf());

    Ok(RepoPath {
        absolute: joined,
        relative,
    })
}

fn canonicalize_existing_parent(path: &Path) -> Result<PathBuf, String> {
    let mut current = path.to_path_buf();
    while !current.exists() {
        if !current.pop() {
            return Err("Path has no existing parent".to_string());
        }
    }
    current
        .canonicalize()
        .map_err(|e| format!("Failed to resolve path {}: {}", current.display(), e))
}

/// Compute a stable hash of file contents (FNV-1a 64-bit).
pub fn hash_bytes(content: &[u8]) -> String {
    const FNV_OFFSET_BASIS: u64 = 0xcbf29ce484222325;
    const FNV_PRIME: u64 = 0x100000001b3;

    let mut hash = FNV_OFFSET_BASIS;
    for byte in content {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(FNV_PRIME);
    }

    format!("{:016x}", hash)
}

pub fn hash_str(content: &str) -> String {
    hash_bytes(content.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::{hash_str, resolve_repo_path_allow_new, truncate};
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn test_truncate_unicode_safe() {
        let input = "ééééé";
        assert_eq!(truncate(input, 4), "é...");
    }

    #[test]
    fn test_truncate_small_max() {
        let input = "こんにちは";
        assert_eq!(truncate(input, 3), "こんに");
        assert_eq!(truncate(input, 0), "");
    }

    #[test]
    fn test_hash_str_is_stable() {
        let a = hash_str("hello");
        let b = hash_str("hello");
        let c = hash_str("world");
        assert_eq!(a, b);
        assert_ne!(a, c);
    }

    #[test]
    fn test_resolve_repo_path_allow_new_accepts_missing_file() {
        let mut root = std::env::temp_dir();
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        root.push(format!("cosmos_repo_path_test_{}", nanos));
        fs::create_dir_all(&root).unwrap();

        let candidate = PathBuf::from("new_dir/new_file.rs");
        let resolved = resolve_repo_path_allow_new(&root, &candidate).unwrap();
        assert_eq!(resolved.relative, candidate);
        assert!(resolved.absolute.ends_with("new_dir/new_file.rs"));

        let _ = fs::remove_dir_all(&root);
    }
}
