use std::path::{Component, Path, PathBuf};

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

pub struct RepoPath {
    pub absolute: PathBuf,
    pub relative: PathBuf,
}

pub fn resolve_repo_path(repo_root: &Path, candidate: &Path) -> Result<RepoPath, String> {
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

    if !joined.exists() {
        return Err(format!("File does not exist: {}", candidate.display()));
    }

    let absolute = joined
        .canonicalize()
        .map_err(|e| format!("Failed to resolve path {}: {}", candidate.display(), e))?;

    if !absolute.starts_with(&root) {
        return Err(format!("Path escapes repository: {}", candidate.display()));
    }

    let relative = absolute
        .strip_prefix(&root)
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|_| candidate.to_path_buf());

    Ok(RepoPath { absolute, relative })
}

#[cfg(test)]
mod tests {
    use super::truncate;

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
}
