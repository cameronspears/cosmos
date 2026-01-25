use crate::index::{CodebaseIndex, FlatTreeEntry};
use std::path::PathBuf;

/// Build a flat file tree for display with sorting
pub(super) fn build_file_tree(index: &CodebaseIndex) -> Vec<FlatTreeEntry> {
    use std::collections::BTreeSet;

    // Collect all unique directories from file paths
    let mut directories: BTreeSet<PathBuf> = BTreeSet::new();
    for path in index.files.keys() {
        let mut current = PathBuf::new();
        for component in path.components() {
            current.push(component);
            // Only add parent directories (not the file itself)
            if current != *path {
                directories.insert(current.clone());
            }
        }
    }

    // Build combined list of directories and files
    let mut all_entries: Vec<FlatTreeEntry> = Vec::new();

    // Add directories
    for dir_path in &directories {
        let depth = dir_path.components().count().saturating_sub(1);
        let name = dir_path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("")
            .to_string();

        all_entries.push(FlatTreeEntry {
            name,
            path: dir_path.clone(),
            is_dir: true,
            depth,
            priority: ' ',
        });
    }

    // Add files
    for (path, file_index) in &index.files {
        let priority = file_index.priority_indicator();
        let depth = path.components().count().saturating_sub(1);
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("")
            .to_string();

        all_entries.push(FlatTreeEntry {
            name,
            path: path.clone(),
            is_dir: false,
            depth,
            priority,
        });
    }

    // Hierarchical sort: by path, with directories before files at each level
    all_entries.sort_by(|a, b| {
        // Compare by full path, but ensure directories come before their contents
        // by comparing component by component
        let a_components: Vec<_> = a.path.components().collect();
        let b_components: Vec<_> = b.path.components().collect();

        // Compare each component
        for i in 0..a_components.len().min(b_components.len()) {
            let a_comp = a_components[i].as_os_str().to_string_lossy().to_lowercase();
            let b_comp = b_components[i].as_os_str().to_string_lossy().to_lowercase();

            match a_comp.cmp(&b_comp) {
                std::cmp::Ordering::Equal => continue,
                other => return other,
            }
        }

        // If all compared components are equal, shorter path (directory) comes first
        // This ensures parent directories come before their contents
        a_components.len().cmp(&b_components.len())
    });

    all_entries
}

/// Build a grouped tree for display
pub(super) fn build_grouped_tree(
    grouping: &crate::grouping::CodebaseGrouping,
    index: &CodebaseIndex,
) -> Vec<crate::grouping::GroupedTreeEntry> {
    use crate::grouping::{GroupedTreeEntry, Layer};

    let mut entries = Vec::new();

    // Add layers in order
    for layer in Layer::all() {
        if let Some(group) = grouping.groups.get(layer) {
            if group.file_count() == 0 {
                continue;
            }

            // Add layer header
            entries.push(GroupedTreeEntry::layer(
                *layer,
                group.file_count(),
                group.expanded,
            ));

            if group.expanded {
                // Add features first, sorted by file count (largest first)
                let mut sorted_features: Vec<_> = group.features.iter().collect();
                sorted_features.sort_by(|a, b| b.files.len().cmp(&a.files.len()));

                for feature in sorted_features {
                    if feature.files.is_empty() {
                        continue;
                    }

                    // Add feature header
                    entries.push(GroupedTreeEntry::feature(
                        &feature.name,
                        feature.files.len(),
                        true,
                    ));

                    // Sort files: priority files first, then alphabetically
                    let mut sorted_files: Vec<_> = feature.files.iter().collect();
                    sorted_files.sort_by(|a, b| {
                        let pri_a = index
                            .files
                            .get(*a)
                            .map(|f| f.priority_indicator())
                            .unwrap_or(' ');
                        let pri_b = index
                            .files
                            .get(*b)
                            .map(|f| f.priority_indicator())
                            .unwrap_or(' ');
                        // Priority files (●) come first
                        match (pri_a == '●', pri_b == '●') {
                            (true, false) => std::cmp::Ordering::Less,
                            (false, true) => std::cmp::Ordering::Greater,
                            _ => a.cmp(b),
                        }
                    });

                    // Add files in this feature with contextual names
                    for file_path in sorted_files {
                        let priority = index
                            .files
                            .get(file_path)
                            .map(|f| f.priority_indicator())
                            .unwrap_or(' ');

                        // Use contextual display name for generic files
                        let name = crate::grouping::display_name_with_context(file_path);

                        entries.push(GroupedTreeEntry::file(&name, file_path.clone(), priority));
                    }
                }

                // Add ungrouped files with priority sorting
                let mut sorted_ungrouped: Vec<_> = group.ungrouped_files.iter().collect();
                sorted_ungrouped.sort_by(|a, b| {
                    let pri_a = index
                        .files
                        .get(*a)
                        .map(|f| f.priority_indicator())
                        .unwrap_or(' ');
                    let pri_b = index
                        .files
                        .get(*b)
                        .map(|f| f.priority_indicator())
                        .unwrap_or(' ');
                    match (pri_a == '●', pri_b == '●') {
                        (true, false) => std::cmp::Ordering::Less,
                        (false, true) => std::cmp::Ordering::Greater,
                        _ => a.cmp(b),
                    }
                });

                for file_path in sorted_ungrouped {
                    let priority = index
                        .files
                        .get(file_path)
                        .map(|f| f.priority_indicator())
                        .unwrap_or(' ');

                    // Use contextual display name
                    let name = crate::grouping::display_name_with_context(file_path);

                    entries.push(GroupedTreeEntry::file(&name, file_path.clone(), priority));
                }
            }
        }
    }

    entries
}
