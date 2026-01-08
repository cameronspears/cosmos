//! File grouping and categorization for Cosmos
//!
//! Organizes codebase files into architectural layers (Frontend, Backend, API, etc.)
//! and feature clusters for a more intuitive project explorer.

#![allow(dead_code)]

pub mod heuristics;
pub mod features;

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

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

    /// Get icon for the layer
    pub fn icon(&self) -> &'static str {
        match self {
            Layer::Frontend => "◇",  // Diamond - UI
            Layer::Backend => "◆",   // Filled diamond - Server
            Layer::API => "⬡",       // Hexagon - Routes
            Layer::Database => "◈",  // Diamond with dot - Data
            Layer::Shared => "○",    // Circle - Shared
            Layer::Config => "⚙",    // Gear - Settings
            Layer::Tests => "◎",     // Target - Tests
            Layer::Infra => "▣",     // Square with fill - Infra
            Layer::Unknown => "·",   // Dot - Unknown
        }
    }
    
    /// Get the index of this layer (for quick jumping with number keys)
    pub fn index(&self) -> usize {
        match self {
            Layer::Frontend => 1,
            Layer::Backend => 2,
            Layer::API => 3,
            Layer::Database => 4,
            Layer::Shared => 5,
            Layer::Config => 6,
            Layer::Tests => 7,
            Layer::Infra => 8,
            Layer::Unknown => 9,
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

    /// Add or update a feature
    pub fn add_feature(&mut self, feature: Feature) {
        if let Some(existing) = self.features.iter_mut().find(|f| f.name == feature.name) {
            existing.files.extend(feature.files);
        } else {
            self.features.push(feature);
        }
    }
}

/// Complete file grouping for a codebase
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CodebaseGrouping {
    /// Groups organized by layer
    pub groups: HashMap<Layer, FileGroup>,
    /// File -> (Layer, Option<Feature>) mapping for quick lookup
    pub file_assignments: HashMap<PathBuf, (Layer, Option<String>)>,
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

    /// Assign a file to a layer (without feature)
    pub fn assign_file(&mut self, path: PathBuf, layer: Layer) {
        self.file_assignments.insert(path.clone(), (layer, None));
        self.groups
            .entry(layer)
            .or_insert_with(|| FileGroup::new(layer))
            .add_file(path);
    }

    /// Assign a file to a layer and feature
    pub fn assign_file_to_feature(&mut self, path: PathBuf, layer: Layer, feature_name: &str) {
        self.file_assignments.insert(path.clone(), (layer, Some(feature_name.to_string())));
        
        let group = self.groups
            .entry(layer)
            .or_insert_with(|| FileGroup::new(layer));
        
        if let Some(feature) = group.features.iter_mut().find(|f| f.name == feature_name) {
            feature.files.push(path);
        } else {
            let mut feature = Feature::new(feature_name);
            feature.files.push(path);
            group.features.push(feature);
        }
    }

    /// Get the layer for a file
    pub fn get_layer(&self, path: &PathBuf) -> Option<Layer> {
        self.file_assignments.get(path).map(|(layer, _)| *layer)
    }

    /// Get groups in display order
    pub fn groups_ordered(&self) -> Vec<&FileGroup> {
        Layer::all()
            .iter()
            .filter_map(|layer| self.groups.get(layer))
            .filter(|g| g.file_count() > 0)
            .collect()
    }

    /// Get mutable access to a specific group
    pub fn get_group_mut(&mut self, layer: Layer) -> Option<&mut FileGroup> {
        self.groups.get_mut(&layer)
    }

    /// Total file count
    pub fn total_files(&self) -> usize {
        self.file_assignments.len()
    }
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

