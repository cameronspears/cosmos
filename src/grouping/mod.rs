//! File grouping and categorization for Cosmos
//!
//! Organizes codebase files into architectural layers (Frontend, Backend, API, etc.)
//! and feature clusters for a more intuitive project explorer.

pub mod heuristics;
pub mod features;

use crate::index::CodebaseIndex;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

// Re-export confidence for use by other modules
pub use heuristics::Confidence;

/// Generic filenames that need parent directory context
const GENERIC_FILENAMES: &[&str] = &[
    "route.ts", "route.js", "route.tsx", "route.jsx",
    "page.ts", "page.tsx", "page.js", "page.jsx",
    "index.ts", "index.tsx", "index.js", "index.jsx",
    "layout.ts", "layout.tsx", "layout.js", "layout.jsx",
    "+page.svelte", "+layout.svelte", "+server.ts", "+server.js",
    "mod.rs", "lib.rs", "main.rs",
    "__init__.py", "views.py", "models.py", "urls.py",
];

/// Get a display name with context for generic filenames
/// 
/// For files like "route.ts", shows "users/route.ts" using the parent directory
pub fn display_name_with_context(path: &Path) -> String {
    let filename = path.file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("?");
    
    // Check if this is a generic filename that needs context
    if GENERIC_FILENAMES.iter().any(|g| filename == *g) {
        // Get parent directory name
        if let Some(parent) = path.parent().and_then(|p| p.file_name()).and_then(|n| n.to_str()) {
            // Skip unhelpful parent names
            let skip_parents = ["src", "app", "lib", "pages", "routes", "api"];
            if !skip_parents.contains(&parent) {
                return format!("{}/{}", parent, filename);
            }
            
            // Try grandparent if parent is generic
            if let Some(grandparent) = path.parent()
                .and_then(|p| p.parent())
                .and_then(|p| p.file_name())
                .and_then(|n| n.to_str()) 
            {
                if !skip_parents.contains(&grandparent) {
                    return format!("{}/{}", grandparent, filename);
                }
            }
        }
    }
    
    filename.to_string()
}

/// Architectural layer classification
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, PartialOrd, Ord)]
pub enum Layer {
    /// UI components, pages, layouts, styles
    Frontend,
    /// Server logic, handlers, middleware
    Backend,
    /// Route definitions, API endpoints
    API,
    /// Models, migrations, queries, schemas
    Database,
    /// Shared types, utilities, constants
    Shared,
    /// Configuration files
    Config,
    /// Test files
    Tests,
    /// CI/CD, Docker, scripts, infrastructure
    Infra,
    /// Files that couldn't be categorized
    Unknown,
}

impl Layer {
    /// Get human-readable display name for the layer
    pub fn label(&self) -> &'static str {
        match self {
            Layer::Frontend => "User Interface",
            Layer::Backend => "Server Logic",
            Layer::API => "Endpoints",
            Layer::Database => "Data Layer",
            Layer::Shared => "Shared Code",
            Layer::Config => "Settings",
            Layer::Tests => "Test Suite",
            Layer::Infra => "Infrastructure",
            Layer::Unknown => "Other Files",
        }
    }

    /// Get layer by index (for quick jumping)
    pub fn from_index(idx: usize) -> Option<Layer> {
        match idx {
            1 => Some(Layer::Frontend),
            2 => Some(Layer::Backend),
            3 => Some(Layer::API),
            4 => Some(Layer::Database),
            5 => Some(Layer::Shared),
            6 => Some(Layer::Config),
            7 => Some(Layer::Tests),
            8 => Some(Layer::Infra),
            9 => Some(Layer::Unknown),
            _ => None,
        }
    }

    /// Get all layers in display order
    pub fn all() -> &'static [Layer] {
        &[
            Layer::Frontend,
            Layer::Backend,
            Layer::API,
            Layer::Database,
            Layer::Shared,
            Layer::Config,
            Layer::Tests,
            Layer::Infra,
            Layer::Unknown,
        ]
    }

    pub fn parse(raw: &str) -> Option<Layer> {
        match raw.trim().to_lowercase().as_str() {
            "frontend" | "ui" => Some(Layer::Frontend),
            "backend" | "server" => Some(Layer::Backend),
            "api" | "endpoint" | "endpoints" => Some(Layer::API),
            "database" | "db" | "data" => Some(Layer::Database),
            "shared" | "common" => Some(Layer::Shared),
            "config" | "configuration" => Some(Layer::Config),
            "tests" | "test" => Some(Layer::Tests),
            "infra" | "infrastructure" => Some(Layer::Infra),
            "unknown" | "other" => Some(Layer::Unknown),
            _ => None,
        }
    }
}

impl Default for Layer {
    fn default() -> Self {
        Layer::Unknown
    }
}

/// A feature grouping within a layer
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Feature {
    /// Feature name (e.g., "authentication", "user-profile")
    pub name: String,
    /// Optional description from LLM
    pub description: Option<String>,
    /// Files belonging to this feature
    pub files: Vec<PathBuf>,
    /// Confidence score (0.0 - 1.0) for this grouping
    pub confidence: f64,
}

impl Feature {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            description: None,
            files: Vec::new(),
            confidence: 1.0,
        }
    }

    pub fn with_files(mut self, files: Vec<PathBuf>) -> Self {
        self.files = files;
        self
    }
}

/// A group of files organized by layer and optional feature
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileGroup {
    /// The architectural layer
    pub layer: Layer,
    /// Features within this layer
    pub features: Vec<Feature>,
    /// Files not assigned to a specific feature
    pub ungrouped_files: Vec<PathBuf>,
    /// Whether this group is expanded in the UI
    #[serde(default = "default_expanded")]
    pub expanded: bool,
}

fn default_expanded() -> bool {
    false
}

impl FileGroup {
    pub fn new(layer: Layer) -> Self {
        Self {
            layer,
            features: Vec::new(),
            ungrouped_files: Vec::new(),
            expanded: false,
        }
    }

    /// Total file count in this group
    pub fn file_count(&self) -> usize {
        self.features.iter().map(|f| f.files.len()).sum::<usize>() + self.ungrouped_files.len()
    }

    /// Add a file to this group (ungrouped)
    pub fn add_file(&mut self, path: PathBuf) {
        self.ungrouped_files.push(path);
    }

}

/// File assignment with confidence tracking
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileAssignment {
    pub layer: Layer,
    pub feature: Option<String>,
    pub confidence: Confidence,
}

impl Default for FileAssignment {
    fn default() -> Self {
        Self {
            layer: Layer::Unknown,
            feature: None,
            confidence: Confidence::Low,
        }
    }
}

#[derive(Debug, Clone)]
pub struct LayerOverride {
    pub layer: Layer,
    pub confidence: Confidence,
}

/// Complete file grouping for a codebase
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CodebaseGrouping {
    /// Groups organized by layer
    pub groups: HashMap<Layer, FileGroup>,
    /// File -> assignment mapping for quick lookup (with confidence)
    pub file_assignments: HashMap<PathBuf, FileAssignment>,
    /// Whether LLM enhancement has been applied
    pub llm_enhanced: bool,
}

impl CodebaseGrouping {
    pub fn new() -> Self {
        Self {
            groups: HashMap::new(),
            file_assignments: HashMap::new(),
            llm_enhanced: false,
        }
    }

    /// Assign a file to a layer with explicit confidence
    pub fn assign_file_with_confidence(&mut self, path: PathBuf, layer: Layer, confidence: Confidence) {
        self.file_assignments.insert(path.clone(), FileAssignment {
            layer,
            feature: None,
            confidence,
        });
        self.groups
            .entry(layer)
            .or_insert_with(|| FileGroup::new(layer))
            .add_file(path);
    }

    /// Reassign a file to a new layer and confidence.
    /// This is safe to call before feature detection (ungrouped files only).
    pub fn reassign_file_with_confidence(
        &mut self,
        path: &PathBuf,
        layer: Layer,
        confidence: Confidence,
    ) {
        if let Some(existing) = self.file_assignments.get(path) {
            let old_layer = existing.layer;
            if let Some(group) = self.groups.get_mut(&old_layer) {
                group.ungrouped_files.retain(|p| p != path);
                for feature in &mut group.features {
                    feature.files.retain(|p| p != path);
                }
                group.features.retain(|f| !f.files.is_empty());
            }
        }

        self.file_assignments.insert(path.clone(), FileAssignment {
            layer,
            feature: None,
            confidence,
        });
        self.groups
            .entry(layer)
            .or_insert_with(|| FileGroup::new(layer))
            .add_file(path.clone());
    }

}

pub fn generate_grouping_with_overrides(
    index: &CodebaseIndex,
    overrides: &HashMap<PathBuf, LayerOverride>,
) -> CodebaseGrouping {
    let mut grouping = heuristics::categorize_codebase(index);

    if !overrides.is_empty() {
        for (path, override_entry) in overrides {
            if index.files.contains_key(path) {
                grouping.reassign_file_with_confidence(
                    path,
                    override_entry.layer,
                    override_entry.confidence,
                );
            }
        }
        grouping.llm_enhanced = true;
    }

    features::detect_features(&mut grouping, index);
    grouping
}

/// Entry in the flattened grouped tree for UI rendering
#[derive(Debug, Clone)]
pub struct GroupedTreeEntry {
    pub kind: GroupedEntryKind,
    pub name: String,
    pub path: Option<PathBuf>,
    pub depth: usize,
    pub expanded: bool,
    pub file_count: usize,
    pub priority: char,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GroupedEntryKind {
    Layer(Layer),
    Feature,
    File,
}

impl GroupedTreeEntry {
    pub fn layer(layer: Layer, file_count: usize, expanded: bool) -> Self {
        Self {
            kind: GroupedEntryKind::Layer(layer),
            name: layer.label().to_string(),
            path: None,
            depth: 0,
            expanded,
            file_count,
            priority: ' ',
        }
    }

    pub fn feature(name: &str, file_count: usize, expanded: bool) -> Self {
        Self {
            kind: GroupedEntryKind::Feature,
            name: name.to_string(),
            path: None,
            depth: 1,
            expanded,
            file_count,
            priority: ' ',
        }
    }

    pub fn file(name: &str, path: PathBuf, priority: char, depth: usize) -> Self {
        Self {
            kind: GroupedEntryKind::File,
            name: name.to_string(),
            path: Some(path),
            depth,
            expanded: false,
            file_count: 0,
            priority,
        }
    }
}

