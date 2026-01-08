//! Feature clustering for grouping related files
//!
//! Uses dependency graph analysis, directory structure, and naming patterns
//! to identify feature clusters within each architectural layer.

#![allow(dead_code)]

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
                    if let Some((_, feat)) = grouping.file_assignments.get_mut(file) {
                        *feat = Some(feature.name.clone());
                    }
                }
                group.features.push(feature);
            }
        }
    }
}

/// Maximum files in a misc/other group before we try harder to split it
const MAX_MISC_FILES: usize = 10;

/// Minimum files needed to form a group (otherwise leave ungrouped)
const MIN_GROUP_SIZE: usize = 2;

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

    // Strategy 2: Group by immediate parent directory (more aggressive)
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

    // Strategy 3: Cluster by naming patterns (for unassigned files)
    let remaining: Vec<_> = files.iter()
        .filter(|f| !assigned.contains(*f))
        .cloned()
        .collect();
    
    let name_features = detect_naming_clusters(&remaining);
    for feature in name_features {
        for file in &feature.files {
            assigned.insert(file.clone());
        }
        features.push(feature);
    }

    // Strategy 4: Cluster by dependency relationships (for remaining unassigned)
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

    // Handle remaining files - only create misc if small enough
    let still_remaining: Vec<_> = files.iter()
        .filter(|f| !assigned.contains(*f))
        .cloned()
        .collect();
    
    if !still_remaining.is_empty() {
        if still_remaining.len() <= MAX_MISC_FILES {
            // Small enough - create a single misc group
            let other_name = get_misc_name(layer);
            features.push(Feature::new(other_name).with_files(still_remaining));
        } else {
            // Too large - try to split by parent directory with lower threshold
            let split_features = split_large_misc(&still_remaining, layer);
            features.extend(split_features);
        }
    }

    // Sort features by file count (larger features first)
    features.sort_by(|a, b| b.files.len().cmp(&a.files.len()));
    
    features
}

/// Get a human-readable misc name for a layer
fn get_misc_name(layer: Layer) -> &'static str {
    match layer {
        Layer::Frontend => "other ui files",
        Layer::Backend => "other services",
        Layer::API => "other endpoints",
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
    
    // Only create features for directories with enough files
    parent_groups.into_iter()
        .filter(|(_, files)| files.len() >= MIN_GROUP_SIZE)
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
    
    // Create features for each parent group (even single files now)
    for (name, group_files) in parent_groups {
        if group_files.len() >= MIN_GROUP_SIZE {
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
        // Look for feature-like directories
        if let Some(feature_dir) = extract_feature_directory(file) {
            dir_groups
                .entry(feature_dir)
                .or_default()
                .push(file.clone());
        }
    }

    // Only create features for directories with multiple files
    dir_groups.into_iter()
        .filter(|(_, files)| files.len() >= 2)
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
        if feature_dirs.contains(&comp.to_lowercase().as_str()) {
            // The next component is likely the feature name
            if i + 1 < components.len() {
                return Some(components[i + 1].to_string());
            }
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
                // Check if it's a route group like (auth) or [id]
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

/// Detect features based on file naming patterns
fn detect_naming_clusters(files: &[PathBuf]) -> Vec<Feature> {
    let mut prefix_groups: HashMap<String, Vec<PathBuf>> = HashMap::new();

    for file in files {
        if let Some(prefix) = extract_naming_prefix(file) {
            prefix_groups
                .entry(prefix)
                .or_default()
                .push(file.clone());
        }
    }

    // Only create features for prefixes with multiple files
    prefix_groups.into_iter()
        .filter(|(_, files)| files.len() >= 2)
        .map(|(name, files)| Feature::new(name).with_files(files))
        .collect()
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

    // Use the first part as the prefix (if meaningful)
    let prefix = parts[0].to_lowercase();
    
    // Filter out common non-descriptive prefixes
    let ignored_prefixes = [
        "index", "main", "app", "page", "layout", "use", "get", "set",
        "create", "update", "delete", "fetch", "handle", "on", "is", "has",
    ];
    
    if ignored_prefixes.contains(&prefix.as_str()) || prefix.len() < 3 {
        // Try the second part if available
        if parts.len() > 1 {
            return Some(parts[1].to_lowercase());
        }
        return None;
    }

    Some(prefix)
}

/// Detect features based on dependency relationships
fn detect_dependency_clusters(files: &[PathBuf], index: &CodebaseIndex) -> Vec<Feature> {
    if files.len() < 3 {
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

        if cluster.len() >= 2 {
            clusters.push(cluster);
        }
    }

    // Convert clusters to features with generated names
    clusters.into_iter()
        .enumerate()
        .map(|(i, files)| {
            let name = generate_cluster_name(&files, i);
            Feature::new(name).with_files(files)
        })
        .collect()
}

/// Generate a name for a dependency cluster
fn generate_cluster_name(files: &[PathBuf], index: usize) -> String {
    // Try to find a common prefix among filenames
    let names: Vec<String> = files.iter()
        .filter_map(|f| f.file_stem())
        .filter_map(|n| n.to_str())
        .map(|s| s.to_lowercase())
        .collect();

    if names.is_empty() {
        return format!("cluster-{}", index + 1);
    }

    // Find longest common prefix
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
        // Clean up the prefix (remove trailing separators)
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

    format!("cluster-{}", index + 1)
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn test_generate_cluster_name() {
        let files = vec![
            PathBuf::from("user-list.tsx"),
            PathBuf::from("user-profile.tsx"),
            PathBuf::from("user-settings.tsx"),
        ];
        assert_eq!(generate_cluster_name(&files, 0), "user");
    }
}

