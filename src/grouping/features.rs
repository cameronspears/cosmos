//! Feature clustering for grouping related files
//!
//! Uses dependency graph analysis, directory structure, naming patterns,
//! and file purpose to identify feature clusters within each architectural layer.

use super::{CodebaseGrouping, Feature, Layer};
use crate::index::CodebaseIndex;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

/// Detect and assign features within a codebase grouping
pub fn detect_features(grouping: &mut CodebaseGrouping, index: &CodebaseIndex) {
    // Process each layer independently
    for layer in Layer::all() {
        if let Some(group) = grouping.groups.get_mut(layer) {
            let files: Vec<PathBuf> = group.ungrouped_files.clone();
            if files.is_empty() {
                continue;
            }

            // Detect features using multiple strategies
            let features = detect_layer_features(&files, index, *layer);
            
            // Clear ungrouped and reassign to features
            group.ungrouped_files.clear();
            
            for feature in features {
                for file in &feature.files {
                    // Update the file assignment
                    if let Some(assignment) = grouping.file_assignments.get_mut(file) {
                        assignment.feature = Some(feature.name.clone());
                    }
                }
                group.features.push(feature);
            }
        }
    }
}

/// Maximum files in a misc/other group before we try harder to split it
const MAX_MISC_FILES: usize = 8;

/// Minimum files needed to form a group for generic files
const MIN_GROUP_SIZE_GENERIC: usize = 2;

/// Generic filenames that shouldn't form single-file features
const GENERIC_FILES: &[&str] = &[
    "index", "mod", "lib", "main", "app", "page", "layout",
    "route", "server", "+page", "+layout", "+server",
    "__init__", "views", "models", "urls",
];

/// Check if a file has a meaningful name (not generic)
fn is_named_file(path: &Path) -> bool {
    let stem = path.file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_lowercase();
    
    // Remove common suffixes
    let clean_stem = stem
        .trim_end_matches(".test")
        .trim_end_matches(".spec")
        .trim_end_matches(".service")
        .trim_end_matches(".controller")
        .trim_end_matches(".model")
        .trim_end_matches(".component");
    
    !GENERIC_FILES.iter().any(|g| clean_stem == *g)
}

/// Extract a feature name from a file path
fn extract_feature_name(path: &Path, index: &CodebaseIndex) -> Option<String> {
    // First try: use the file's purpose if available and meaningful
    if let Some(file_index) = index.files.get(path) {
        let purpose = &file_index.summary.purpose;
        // Extract key words from purpose
        let purpose_lower = purpose.to_lowercase();
        
        // Skip generic purposes
        let generic_purposes = ["module", "file", "code", "script", "utility"];
        if !generic_purposes.iter().any(|g| purpose_lower.contains(g)) {
            // Try to extract a meaningful name from purpose
            if let Some(name) = extract_purpose_keyword(&purpose_lower) {
                return Some(name);
            }
        }
    }
    
    // Second try: use filename stem if meaningful
    let stem = path.file_stem()?.to_str()?;
    let clean_name = stem
        .trim_end_matches(".test")
        .trim_end_matches(".spec")
        .trim_end_matches(".service")
        .trim_end_matches(".controller")
        .trim_end_matches(".model")
        .trim_end_matches(".component")
        .trim_end_matches(".hook");
    
    // Split by separators and get first meaningful part
    let parts: Vec<&str> = clean_name
        .split(|c| c == '-' || c == '_' || c == '.')
        .filter(|p| !p.is_empty())
        .collect();
    
    if parts.is_empty() {
        return None;
    }
    
    // Use first meaningful part
    let first = parts[0].to_lowercase();
    if GENERIC_FILES.iter().any(|g| first == *g) || first.len() < 3 {
        if parts.len() > 1 {
            return Some(parts[1].to_lowercase());
        }
        return None;
    }
    
    Some(first)
}

/// Extract a keyword from a purpose string
fn extract_purpose_keyword(purpose: &str) -> Option<String> {
    // Look for common patterns like "handles X", "manages Y", "X service"
    let words: Vec<&str> = purpose.split_whitespace().collect();
    
    for word in words.iter() {
        let w = word.trim_matches(|c: char| !c.is_alphanumeric());
        
        // Skip common verbs and articles
        let skip_words = [
            "the", "a", "an", "is", "are", "was", "were", "be", "been",
            "handles", "manages", "provides", "contains", "defines",
            "implements", "exports", "imports", "for", "with", "and", "or",
        ];
        
        if skip_words.contains(&w) || w.len() < 3 {
            continue;
        }
        
        // Found a good word
        return Some(w.to_string());
    }
    
    None
}

/// Detect features within a single layer
fn detect_layer_features(files: &[PathBuf], index: &CodebaseIndex, layer: Layer) -> Vec<Feature> {
    let mut features: Vec<Feature> = Vec::new();
    let mut assigned: HashSet<PathBuf> = HashSet::new();

    // Strategy 1: Detect features from explicit feature directories
    let dir_features = detect_directory_features(files, layer);
    for feature in dir_features {
        for file in &feature.files {
            assigned.insert(file.clone());
        }
        features.push(feature);
    }

    // Strategy 2: Group by file purpose (semantic grouping)
    let remaining: Vec<_> = files.iter()
        .filter(|f| !assigned.contains(*f))
        .cloned()
        .collect();
    
    let purpose_features = group_by_purpose(&remaining, index);
    for feature in purpose_features {
        for file in &feature.files {
            assigned.insert(file.clone());
        }
        features.push(feature);
    }

    // Strategy 3: Cluster by dependency relationships
    let remaining: Vec<_> = files.iter()
        .filter(|f| !assigned.contains(*f))
        .cloned()
        .collect();
    
    let dep_features = detect_dependency_clusters(&remaining, index);
    for feature in dep_features {
        for file in &feature.files {
            assigned.insert(file.clone());
        }
        features.push(feature);
    }

    // Strategy 4: Group by immediate parent directory
    let remaining: Vec<_> = files.iter()
        .filter(|f| !assigned.contains(*f))
        .cloned()
        .collect();
    
    let parent_features = group_by_parent_directory(&remaining);
    for feature in parent_features {
        for file in &feature.files {
            assigned.insert(file.clone());
        }
        features.push(feature);
    }

    // Strategy 5: Cluster by naming patterns
    let remaining: Vec<_> = files.iter()
        .filter(|f| !assigned.contains(*f))
        .cloned()
        .collect();
    
    let name_features = detect_naming_clusters(&remaining, index);
    for feature in name_features {
        for file in &feature.files {
            assigned.insert(file.clone());
        }
        features.push(feature);
    }

    // Strategy 6: Create single-file features for named files
    let remaining: Vec<_> = files.iter()
        .filter(|f| !assigned.contains(*f))
        .cloned()
        .collect();
    
    let single_features = create_single_file_features(&remaining, index);
    for feature in single_features {
        for file in &feature.files {
            assigned.insert(file.clone());
        }
        features.push(feature);
    }

    // Handle remaining files - only create misc if small enough
    let still_remaining: Vec<_> = files.iter()
        .filter(|f| !assigned.contains(*f))
        .cloned()
        .collect();
    
    if !still_remaining.is_empty() {
        if still_remaining.len() <= MAX_MISC_FILES {
            let other_name = get_misc_name(layer);
            features.push(Feature::new(other_name).with_files(still_remaining));
        } else {
            let split_features = split_large_misc(&still_remaining, layer);
            features.extend(split_features);
        }
    }

    // Sort features by file count (larger features first), but misc always last
    features.sort_by(|a, b| {
        let a_is_misc = a.name.starts_with("other");
        let b_is_misc = b.name.starts_with("other");
        match (a_is_misc, b_is_misc) {
            (true, false) => std::cmp::Ordering::Greater,
            (false, true) => std::cmp::Ordering::Less,
            _ => b.files.len().cmp(&a.files.len()),
        }
    });
    
    features
}

/// Group files by their semantic purpose
fn group_by_purpose(files: &[PathBuf], index: &CodebaseIndex) -> Vec<Feature> {
    let mut purpose_groups: HashMap<String, Vec<PathBuf>> = HashMap::new();
    
    for file in files {
        if let Some(feature_name) = extract_feature_name(file, index) {
            purpose_groups
                .entry(feature_name)
                .or_default()
                .push(file.clone());
        }
    }
    
    // Create features for groups with enough files
    purpose_groups.into_iter()
        .filter(|(_, files)| files.len() >= MIN_GROUP_SIZE_GENERIC)
        .map(|(name, files)| Feature::new(name).with_files(files))
        .collect()
}

/// Create single-file features for meaningfully named files
fn create_single_file_features(files: &[PathBuf], index: &CodebaseIndex) -> Vec<Feature> {
    let mut features = Vec::new();
    
    for file in files {
        // Only create single-file features for named (non-generic) files
        if is_named_file(file) {
            if let Some(name) = extract_feature_name(file, index) {
                features.push(Feature::new(&name).with_files(vec![file.clone()]));
            }
        }
    }
    
    features
}

/// Get a human-readable misc name for a layer
fn get_misc_name(layer: Layer) -> &'static str {
    match layer {
        Layer::Frontend => "other ui files",
        Layer::Backend => "other services",
        Layer::Api => "other endpoints",
        Layer::Database => "other data files",
        Layer::Shared => "other utilities",
        Layer::Tests => "other tests",
        _ => "other files",
    }
}

/// Group files by their immediate parent directory
fn group_by_parent_directory(files: &[PathBuf]) -> Vec<Feature> {
    let mut parent_groups: HashMap<String, Vec<PathBuf>> = HashMap::new();
    
    // Skip these generic parent names
    let skip_parents = [
        "src", "lib", "app", "pages", "components", "api", "routes",
        "utils", "helpers", "types", "models", "services", "hooks",
    ];
    
    for file in files {
        if let Some(parent) = file.parent() {
            if let Some(parent_name) = parent.file_name().and_then(|n| n.to_str()) {
                // Skip generic parent names
                if skip_parents.contains(&parent_name.to_lowercase().as_str()) {
                    continue;
                }
                
                parent_groups
                    .entry(parent_name.to_string())
                    .or_default()
                    .push(file.clone());
            }
        }
    }
    
    // Create features for directories with enough files
    parent_groups.into_iter()
        .filter(|(_, files)| files.len() >= MIN_GROUP_SIZE_GENERIC)
        .map(|(name, files)| Feature::new(name).with_files(files))
        .collect()
}

/// Split a large misc group into smaller groups by parent directory
fn split_large_misc(files: &[PathBuf], layer: Layer) -> Vec<Feature> {
    let mut features = Vec::new();
    let mut parent_groups: HashMap<String, Vec<PathBuf>> = HashMap::new();
    let mut ungrouped = Vec::new();
    
    for file in files {
        if let Some(parent) = file.parent() {
            if let Some(parent_name) = parent.file_name().and_then(|n| n.to_str()) {
                parent_groups
                    .entry(parent_name.to_string())
                    .or_default()
                    .push(file.clone());
            } else {
                ungrouped.push(file.clone());
            }
        } else {
            ungrouped.push(file.clone());
        }
    }
    
    // Create features for each parent group
    for (name, group_files) in parent_groups {
        if group_files.len() >= MIN_GROUP_SIZE_GENERIC {
            features.push(Feature::new(&name).with_files(group_files));
        } else {
            ungrouped.extend(group_files);
        }
    }
    
    // If we still have ungrouped files, add them as misc
    if !ungrouped.is_empty() {
        features.push(Feature::new(get_misc_name(layer)).with_files(ungrouped));
    }
    
    features
}

/// Detect features based on directory structure
fn detect_directory_features(files: &[PathBuf], _layer: Layer) -> Vec<Feature> {
    let mut dir_groups: HashMap<String, Vec<PathBuf>> = HashMap::new();

    for file in files {
        if let Some(feature_dir) = extract_feature_directory(file) {
            dir_groups
                .entry(feature_dir)
                .or_default()
                .push(file.clone());
        }
    }

    // Create features for directories with files (allow single files for explicit feature dirs)
    dir_groups.into_iter()
        .filter(|(_, files)| !files.is_empty())
        .map(|(name, files)| Feature::new(name).with_files(files))
        .collect()
}

/// Extract a feature directory name from a path
fn extract_feature_directory(path: &Path) -> Option<String> {
    let components: Vec<_> = path.components()
        .filter_map(|c| c.as_os_str().to_str())
        .collect();

    // Look for feature-indicating directories
    let feature_dirs = [
        "features", "modules", "domains", "packages", "apps",
    ];

    // Check for explicit feature directories (features/auth/...)
    for (i, comp) in components.iter().enumerate() {
        if feature_dirs.contains(&comp.to_lowercase().as_str()) && i + 1 < components.len() {
            return Some(components[i + 1].to_string());
        }
    }

    // For app router style (app/dashboard/..., app/settings/...)
    for (i, comp) in components.iter().enumerate() {
        if *comp == "app" && i + 1 < components.len() {
            let next = components[i + 1];
            // Skip common non-feature directories
            if !["api", "components", "lib", "utils", "(", "_"].iter()
                .any(|s| next.starts_with(s)) 
            {
                if next.starts_with('(') || next.starts_with('[') {
                    continue;
                }
                return Some(next.to_string());
            }
        }
    }

    // For pages router style (pages/dashboard/...)
    for (i, comp) in components.iter().enumerate() {
        if *comp == "pages" && i + 1 < components.len() {
            let next = components[i + 1];
            if !["api", "_app", "_document", "_error"].contains(&next) {
                return Some(next.to_string());
            }
        }
    }

    None
}

/// Detect features based on file naming patterns (using exports and purpose)
fn detect_naming_clusters(files: &[PathBuf], index: &CodebaseIndex) -> Vec<Feature> {
    let mut prefix_groups: HashMap<String, Vec<PathBuf>> = HashMap::new();

    for file in files {
        // Try export-based clustering first
        if let Some(file_index) = index.files.get(file) {
            // Use exports to determine cluster
            for export in &file_index.summary.exports {
                let export_lower = export.to_lowercase();
                // Extract prefix from export names
                if let Some(prefix) = extract_export_prefix(&export_lower) {
                    prefix_groups
                        .entry(prefix)
                        .or_default()
                        .push(file.clone());
                    break; // Use first meaningful export
                }
            }
        }
        
        // Fall back to filename prefix
        if !prefix_groups.values().any(|v| v.contains(file)) {
            if let Some(prefix) = extract_naming_prefix(file) {
                prefix_groups
                    .entry(prefix)
                    .or_default()
                    .push(file.clone());
            }
        }
    }

    // Only create features for prefixes with multiple files
    prefix_groups.into_iter()
        .filter(|(_, files)| files.len() >= MIN_GROUP_SIZE_GENERIC)
        .map(|(name, files)| Feature::new(name).with_files(files))
        .collect()
}

/// Extract a prefix from an export name
fn extract_export_prefix(export: &str) -> Option<String> {
    // Common patterns: UserService -> user, handleAuth -> auth, useSettings -> settings
    
    // Only strip prefix if followed by uppercase (indicating camelCase boundary)
    let prefixes_to_strip = ["use", "get", "set", "handle", "create", "fetch", "with"];
    let mut work = export;
    
    for prefix in prefixes_to_strip {
        if work.len() > prefix.len() && work.starts_with(prefix) {
            let next_char = work.chars().nth(prefix.len());
            if let Some(c) = next_char {
                if c.is_uppercase() {
                    // Valid prefix boundary - strip it
                    work = &work[prefix.len()..];
                    break;
                }
            }
        }
    }
    
    if work.is_empty() || work.len() < 3 {
        return None;
    }
    
    // Convert from camelCase/PascalCase to prefix (take first word)
    let mut prefix = String::new();
    for (i, c) in work.chars().enumerate() {
        if i > 0 && c.is_uppercase() {
            break; // Stop at next capital letter
        }
        prefix.push(c.to_ascii_lowercase());
    }
    
    if prefix.len() >= 3 {
        Some(prefix)
    } else {
        None
    }
}

/// Extract a meaningful prefix from a filename for clustering
fn extract_naming_prefix(path: &Path) -> Option<String> {
    let filename = path.file_stem()?.to_str()?;
    
    // Remove common suffixes first
    let clean_name = filename
        .trim_end_matches(".test")
        .trim_end_matches(".spec")
        .trim_end_matches(".service")
        .trim_end_matches(".controller")
        .trim_end_matches(".model")
        .trim_end_matches(".component")
        .trim_end_matches(".hook")
        .trim_end_matches(".util")
        .trim_end_matches(".helper");

    // Split by common separators
    let parts: Vec<&str> = clean_name
        .split(|c| c == '-' || c == '_' || c == '.')
        .collect();

    if parts.is_empty() {
        return None;
    }

    let prefix = parts[0].to_lowercase();
    
    // Filter out common non-descriptive prefixes
    let ignored_prefixes = [
        "index", "main", "app", "page", "layout", "use", "get", "set",
        "create", "update", "delete", "fetch", "handle", "on", "is", "has",
    ];
    
    if ignored_prefixes.contains(&prefix.as_str()) || prefix.len() < 3 {
        if parts.len() > 1 {
            return Some(parts[1].to_lowercase());
        }
        return None;
    }

    Some(prefix)
}

/// Detect features based on dependency relationships
fn detect_dependency_clusters(files: &[PathBuf], index: &CodebaseIndex) -> Vec<Feature> {
    if files.len() < 2 {
        return Vec::new();
    }

    // Build adjacency map based on imports
    let mut adjacency: HashMap<PathBuf, HashSet<PathBuf>> = HashMap::new();
    let file_set: HashSet<_> = files.iter().cloned().collect();

    for file in files {
        if let Some(file_index) = index.files.get(file) {
            // Check depends_on
            for dep in &file_index.summary.depends_on {
                if file_set.contains(dep) {
                    adjacency.entry(file.clone()).or_default().insert(dep.clone());
                    adjacency.entry(dep.clone()).or_default().insert(file.clone());
                }
            }
            // Check used_by
            for user in &file_index.summary.used_by {
                if file_set.contains(user) {
                    adjacency.entry(file.clone()).or_default().insert(user.clone());
                    adjacency.entry(user.clone()).or_default().insert(file.clone());
                }
            }
        }
    }

    // Find connected components (clusters)
    let mut visited: HashSet<PathBuf> = HashSet::new();
    let mut clusters: Vec<Vec<PathBuf>> = Vec::new();

    for file in files {
        if visited.contains(file) {
            continue;
        }

        let mut cluster = Vec::new();
        let mut stack = vec![file.clone()];

        while let Some(current) = stack.pop() {
            if visited.contains(&current) {
                continue;
            }
            visited.insert(current.clone());
            cluster.push(current.clone());

            if let Some(neighbors) = adjacency.get(&current) {
                for neighbor in neighbors {
                    if !visited.contains(neighbor) {
                        stack.push(neighbor.clone());
                    }
                }
            }
        }

        // Allow smaller clusters (2+ files) when they have dependency relationships
        if cluster.len() >= MIN_GROUP_SIZE_GENERIC {
            clusters.push(cluster);
        }
    }

    // Convert clusters to features with generated names
    clusters.into_iter()
        .enumerate()
        .map(|(i, files)| {
            let name = generate_cluster_name(&files, index, i);
            Feature::new(name).with_files(files)
        })
        .collect()
}

/// Generate a name for a dependency cluster
fn generate_cluster_name(files: &[PathBuf], index: &CodebaseIndex, fallback_index: usize) -> String {
    // First try: use common purpose words
    let mut purpose_words: HashMap<String, usize> = HashMap::new();
    for file in files {
        if let Some(file_index) = index.files.get(file) {
            let purpose_lower = file_index.summary.purpose.to_lowercase();
            for word in purpose_lower.split_whitespace() {
                let w = word.trim_matches(|c: char| !c.is_alphanumeric());
                if w.len() >= 4 {
                    *purpose_words.entry(w.to_string()).or_default() += 1;
                }
            }
        }
    }
    
    // Remove common words
    let skip_words = ["file", "module", "code", "the", "and", "for", "with", "this", "that"];
    for word in skip_words {
        purpose_words.remove(word);
    }
    
    if let Some((word, count)) = purpose_words.into_iter().max_by_key(|(_, count)| *count) {
        if count >= files.len() / 2 {
            return word;
        }
    }

    // Second try: common prefix among filenames
    let names: Vec<String> = files.iter()
        .filter_map(|f| f.file_stem())
        .filter_map(|n| n.to_str())
        .map(|s| s.to_lowercase())
        .collect();

    if names.is_empty() {
        return format!("cluster-{}", fallback_index + 1);
    }

    let first = &names[0];
    let mut common_len = 0;

    'outer: for i in 1..=first.len() {
        let prefix = &first[..i];
        for name in &names[1..] {
            if !name.starts_with(prefix) {
                break 'outer;
            }
        }
        common_len = i;
    }

    if common_len >= 3 {
        let prefix = first[..common_len]
            .trim_end_matches('-')
            .trim_end_matches('_')
            .to_string();
        if prefix.len() >= 3 {
            return prefix;
        }
    }

    // Fall back to the most common word
    let mut word_counts: HashMap<&str, usize> = HashMap::new();
    for name in &names {
        for word in name.split(|c| c == '-' || c == '_' || c == '.') {
            if word.len() >= 3 {
                *word_counts.entry(word).or_default() += 1;
            }
        }
    }

    if let Some((word, _)) = word_counts.into_iter().max_by_key(|(_, count)| *count) {
        return word.to_string();
    }

    format!("cluster-{}", fallback_index + 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_named_file() {
        assert!(is_named_file(Path::new("authentication.ts")));
        assert!(is_named_file(Path::new("user-profile.tsx")));
        assert!(!is_named_file(Path::new("index.ts")));
        assert!(!is_named_file(Path::new("mod.rs")));
        assert!(!is_named_file(Path::new("page.tsx")));
    }

    #[test]
    fn test_extract_feature_directory() {
        assert_eq!(
            extract_feature_directory(Path::new("src/features/auth/login.tsx")),
            Some("auth".to_string())
        );
        assert_eq!(
            extract_feature_directory(Path::new("app/dashboard/page.tsx")),
            Some("dashboard".to_string())
        );
        assert_eq!(
            extract_feature_directory(Path::new("src/utils/format.ts")),
            None
        );
    }

    #[test]
    fn test_extract_naming_prefix() {
        assert_eq!(
            extract_naming_prefix(Path::new("user-profile.tsx")),
            Some("user".to_string())
        );
        assert_eq!(
            extract_naming_prefix(Path::new("auth.service.ts")),
            Some("auth".to_string())
        );
        assert_eq!(
            extract_naming_prefix(Path::new("index.ts")),
            None
        );
    }

    #[test]
    fn test_extract_export_prefix() {
        assert_eq!(extract_export_prefix("userService"), Some("user".to_string()));
        assert_eq!(extract_export_prefix("handleAuth"), Some("auth".to_string()));
        assert_eq!(extract_export_prefix("useSettings"), Some("settings".to_string()));
    }

    #[test]
    fn test_generate_cluster_name_fallback() {
        use std::collections::HashMap;
        use chrono::Utc;
        
        let files = vec![
            PathBuf::from("user-list.tsx"),
            PathBuf::from("user-profile.tsx"),
            PathBuf::from("user-settings.tsx"),
        ];
        
        // Create minimal empty index for test
        let index = CodebaseIndex {
            root: PathBuf::new(),
            files: HashMap::new(),
            symbols: Vec::new(),
            dependencies: Vec::new(),
            patterns: Vec::new(),
            cached_at: Utc::now(),
        };
        
        assert_eq!(generate_cluster_name(&files, &index, 0), "user");
    }
}
