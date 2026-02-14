use super::ReviewFinding;
use cosmos_adapters::util::resolve_repo_path_allow_new;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

pub(super) fn build_files_with_content(
    sandbox_root: &Path,
    old_contents: &HashMap<PathBuf, String>,
    files: &[PathBuf],
) -> Result<Vec<(PathBuf, String, String)>, String> {
    files
        .iter()
        .map(|path| {
            let resolved = resolve_repo_path_allow_new(sandbox_root, path)
                .map_err(|e| format!("Unsafe path {}: {}", path.display(), e))?;
            let new_content = std::fs::read_to_string(&resolved.absolute).unwrap_or_default();
            let old_content = old_contents.get(path).cloned().unwrap_or_default();
            Ok((resolved.absolute, old_content, new_content))
        })
        .collect::<Result<Vec<_>, _>>()
}

pub(super) fn blocking_findings(
    findings: &[ReviewFinding],
    blocking_severities: &HashSet<String>,
) -> Vec<ReviewFinding> {
    findings
        .iter()
        .filter(|finding| {
            finding.recommended
                && blocking_severities.contains(&finding.severity.to_ascii_lowercase())
        })
        .cloned()
        .collect()
}

pub(super) fn is_probable_compile_error_false_positive(title: &str) -> bool {
    let lower = title.to_ascii_lowercase();
    if lower.contains("missing import")
        || lower.contains("not imported")
        || lower.contains("unresolved import")
    {
        return true;
    }

    // Titles often look like "`symbol` undefined". Avoid matching broader phrases like
    // "undefined behavior" by requiring backticks.
    if title.contains('`') && lower.contains("undefined") {
        return true;
    }

    false
}

pub(super) fn group_findings_by_file(
    findings: &[ReviewFinding],
    candidates: &[PathBuf],
) -> HashMap<PathBuf, Vec<ReviewFinding>> {
    let mut grouped: HashMap<PathBuf, Vec<ReviewFinding>> = HashMap::new();
    for finding in findings {
        if let Some(path) = resolve_finding_file_path(&finding.file, candidates) {
            grouped.entry(path).or_default().push(finding.clone());
        }
    }
    grouped
}

pub(super) fn resolve_finding_file_path(
    finding_file: &str,
    candidates: &[PathBuf],
) -> Option<PathBuf> {
    let normalized = finding_file.replace('\\', "/");
    let candidate = PathBuf::from(&normalized);
    if candidates.iter().any(|p| p == &candidate) {
        return Some(candidate);
    }

    for path in candidates {
        let p = path.to_string_lossy().replace('\\', "/");
        if normalized.ends_with(&p) {
            return Some(path.clone());
        }
    }

    let normalized_path = PathBuf::from(&normalized);
    let file_name = normalized_path.file_name().and_then(|name| name.to_str())?;
    let mut matches = candidates
        .iter()
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .map(|name| name == file_name)
                .unwrap_or(false)
        })
        .cloned()
        .collect::<Vec<_>>();
    if matches.len() == 1 {
        return matches.pop();
    }
    None
}
