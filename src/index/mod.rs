//! Codebase indexing engine for Cosmos
//!
//! Uses tree-sitter for multi-language AST parsing to build
//! semantic understanding of the codebase.

#![allow(dead_code)]

pub mod parser;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

/// Supported programming languages
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Language {
    Rust,
    JavaScript,
    TypeScript,
    Python,
    Go,
    Unknown,
}

impl Language {
    pub fn from_extension(ext: &str) -> Self {
        match ext.to_lowercase().as_str() {
            "rs" => Language::Rust,
            "js" | "jsx" | "mjs" | "cjs" => Language::JavaScript,
            "ts" | "tsx" => Language::TypeScript,
            "py" | "pyi" => Language::Python,
            "go" => Language::Go,
            _ => Language::Unknown,
        }
    }

    pub fn icon(&self) -> &'static str {
        match self {
            Language::Rust => "rs",
            Language::JavaScript => "js",
            Language::TypeScript => "ts",
            Language::Python => "py",
            Language::Go => "go",
            Language::Unknown => "??",
        }
    }
}

/// A symbol extracted from the AST (function, struct, class, etc.)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Symbol {
    pub name: String,
    pub kind: SymbolKind,
    pub file: PathBuf,
    pub line: usize,
    pub end_line: usize,
    pub complexity: f64,
    pub visibility: Visibility,
}

impl Symbol {
    pub fn line_count(&self) -> usize {
        self.end_line.saturating_sub(self.line) + 1
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SymbolKind {
    Function,
    Method,
    Struct,
    Class,
    Enum,
    Interface,
    Trait,
    Module,
    Constant,
    Variable,
}

impl SymbolKind {
    pub fn icon(&self) -> char {
        match self {
            SymbolKind::Function | SymbolKind::Method => 'f',
            SymbolKind::Struct | SymbolKind::Class => 'S',
            SymbolKind::Enum => 'E',
            SymbolKind::Interface | SymbolKind::Trait => 'T',
            SymbolKind::Module => 'M',
            SymbolKind::Constant => 'C',
            SymbolKind::Variable => 'v',
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Visibility {
    Public,
    Private,
    Internal,
}

/// A dependency/import found in the code
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Dependency {
    pub from_file: PathBuf,
    pub import_path: String,
    pub line: usize,
    pub is_external: bool,
}

/// Recognized code patterns
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Pattern {
    pub kind: PatternKind,
    pub file: PathBuf,
    pub line: usize,
    pub description: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PatternKind {
    /// Long function (>50 lines)
    LongFunction,
    /// Deeply nested code (>4 levels)
    DeepNesting,
    /// Many parameters (>5)
    ManyParameters,
    /// God class/module (>500 lines)
    GodModule,
    /// Duplicate code pattern
    DuplicatePattern,
    /// Missing error handling
    MissingErrorHandling,
    /// Unused import
    UnusedImport,
    /// TODO/FIXME marker
    TodoMarker,
}

impl PatternKind {
    pub fn severity(&self) -> PatternSeverity {
        match self {
            PatternKind::LongFunction => PatternSeverity::Medium,
            PatternKind::DeepNesting => PatternSeverity::High,
            PatternKind::ManyParameters => PatternSeverity::Low,
            PatternKind::GodModule => PatternSeverity::High,
            PatternKind::DuplicatePattern => PatternSeverity::Medium,
            PatternKind::MissingErrorHandling => PatternSeverity::High,
            PatternKind::UnusedImport => PatternSeverity::Low,
            PatternKind::TodoMarker => PatternSeverity::Info,
        }
    }

    pub fn description(&self) -> &'static str {
        match self {
            PatternKind::LongFunction => "Function exceeds 50 lines",
            PatternKind::DeepNesting => "Code nesting exceeds 4 levels",
            PatternKind::ManyParameters => "Function has more than 5 parameters",
            PatternKind::GodModule => "File exceeds 500 lines",
            PatternKind::DuplicatePattern => "Similar code pattern detected",
            PatternKind::MissingErrorHandling => "Error handling may be missing",
            PatternKind::UnusedImport => "Import appears unused",
            PatternKind::TodoMarker => "TODO/FIXME marker found",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum PatternSeverity {
    Info,
    Low,
    Medium,
    High,
}

/// Summary information about a file for quick reference
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct FileSummary {
    /// What this file does (inferred from name, exports, doc comments)
    pub purpose: String,
    /// Public symbols exported by this file
    pub exports: Vec<String>,
    /// Files that import/use this file
    pub used_by: Vec<PathBuf>,
    /// Files this file depends on (imports)
    pub depends_on: Vec<PathBuf>,
    /// Quick metrics string
    pub metrics: String,
}

impl FileSummary {
    /// Generate a static summary from file index data
    pub fn from_file_index(file_index: &FileIndex, rel_path: &Path) -> Self {
        // Infer purpose from filename and exports
        let purpose = infer_purpose(rel_path, &file_index.symbols, file_index.language);
        
        // Get public exports
        let exports: Vec<String> = file_index.symbols.iter()
            .filter(|s| s.visibility == Visibility::Public)
            .map(|s| s.name.clone())
            .take(10)
            .collect();
        
        // Build metrics string
        let func_count = file_index.symbols.iter()
            .filter(|s| matches!(s.kind, SymbolKind::Function | SymbolKind::Method))
            .count();
        
        let metrics = format!(
            "{} LOC | {} funcs | complexity: {:.0}",
            file_index.loc,
            func_count,
            file_index.complexity
        );
        
        // depends_on will be populated by the codebase index
        let depends_on: Vec<PathBuf> = file_index.dependencies.iter()
            .filter(|d| !d.is_external)
            .filter_map(|d| resolve_import_path(&d.import_path, rel_path))
            .collect();
        
        Self {
            purpose,
            exports,
            used_by: Vec::new(), // Populated later by build_dependency_graph
            depends_on,
            metrics,
        }
    }
    
    /// Format for display in the UI
    pub fn display(&self) -> String {
        let mut lines = Vec::new();
        
        lines.push(self.purpose.clone());
        lines.push(String::new());
        
        if !self.exports.is_empty() {
            let exports_str = if self.exports.len() > 5 {
                format!("{}, +{} more", self.exports[..5].join(", "), self.exports.len() - 5)
            } else {
                self.exports.join(", ")
            };
            lines.push(format!("Exports: {}", exports_str));
        }
        
        if !self.used_by.is_empty() {
            let used_by_str: Vec<_> = self.used_by.iter()
                .take(3)
                .filter_map(|p| p.file_name().and_then(|n| n.to_str()))
                .collect();
            let suffix = if self.used_by.len() > 3 {
                format!(", +{} more", self.used_by.len() - 3)
            } else {
                String::new()
            };
            lines.push(format!("Used by: {}{}", used_by_str.join(", "), suffix));
        }
        
        if !self.depends_on.is_empty() {
            let deps_str: Vec<_> = self.depends_on.iter()
                .take(3)
                .filter_map(|p| p.file_name().and_then(|n| n.to_str()))
                .collect();
            let suffix = if self.depends_on.len() > 3 {
                format!(", +{} more", self.depends_on.len() - 3)
            } else {
                String::new()
            };
            lines.push(format!("Depends: {}{}", deps_str.join(", "), suffix));
        }
        
        lines.push(String::new());
        lines.push(self.metrics.clone());
        
        lines.join("\n")
    }
}

/// Infer the purpose of a file from its name and exports
fn infer_purpose(path: &Path, symbols: &[Symbol], _language: Language) -> String {
    let filename = path.file_stem()
        .and_then(|n| n.to_str())
        .unwrap_or("unknown");
    
    let parent = path.parent()
        .and_then(|p| p.file_name())
        .and_then(|n| n.to_str())
        .unwrap_or("");
    
    // Common filename patterns -> descriptions
    let known_purpose = match filename.to_lowercase().as_str() {
        "mod" => Some(format!("{} module definitions", parent)),
        "main" => Some("Application entry point".to_string()),
        "lib" => Some("Library root module".to_string()),
        "index" => Some(format!("{} module exports", parent)),
        "config" | "configuration" => Some("Configuration management".to_string()),
        "types" => Some("Type definitions".to_string()),
        "constants" | "consts" => Some("Constant definitions".to_string()),
        "tests" | "test" => Some("Test suite".to_string()),
        _ => None,
    };
    
    // Get public symbols grouped by kind
    let public_symbols: Vec<_> = symbols.iter()
        .filter(|s| s.visibility == Visibility::Public)
        .collect();
    
    let functions: Vec<&str> = public_symbols.iter()
        .filter(|s| matches!(s.kind, SymbolKind::Function | SymbolKind::Method))
        .map(|s| s.name.as_str())
        .collect();
    
    let types: Vec<&str> = public_symbols.iter()
        .filter(|s| matches!(s.kind, SymbolKind::Struct | SymbolKind::Class | SymbolKind::Enum))
        .map(|s| s.name.as_str())
        .collect();
    
    let traits: Vec<&str> = public_symbols.iter()
        .filter(|s| matches!(s.kind, SymbolKind::Trait | SymbolKind::Interface))
        .map(|s| s.name.as_str())
        .collect();
    
    // Build a descriptive summary
    let mut parts = Vec::new();
    
    // Start with known purpose if we have one, otherwise infer from filename
    if let Some(purpose) = known_purpose {
        parts.push(purpose);
    } else {
        // Try to infer from filename pattern
        let inferred = infer_from_filename(filename, parent);
        if !inferred.is_empty() {
            parts.push(inferred);
        }
    }
    
    // Add type information
    if !types.is_empty() {
        let type_desc = if types.len() == 1 {
            format!("Defines {} type", types[0])
        } else if types.len() <= 3 {
            format!("Defines {} types", types.join(", "))
        } else {
            format!("Defines {} and {} other types", types[..2].join(", "), types.len() - 2)
        };
        parts.push(type_desc);
    }
    
    // Add trait/interface information
    if !traits.is_empty() {
        let trait_desc = if traits.len() == 1 {
            format!("Defines {} trait", traits[0])
        } else {
            format!("Defines {} traits", traits.join(", "))
        };
        parts.push(trait_desc);
    }
    
    // Add function information
    if !functions.is_empty() {
        let func_desc = if functions.len() == 1 {
            format!("Provides {} function", humanize_name(functions[0]))
        } else if functions.len() <= 4 {
            let names: Vec<_> = functions.iter().map(|n| humanize_name(n)).collect();
            format!("Provides {} functions", names.join(", "))
        } else {
            let names: Vec<_> = functions.iter().take(3).map(|n| humanize_name(n)).collect();
            format!("Provides {} and {} other functions", names.join(", "), functions.len() - 3)
        };
        parts.push(func_desc);
    }
    
    // If we still have nothing useful, provide a basic description
    if parts.is_empty() {
        if symbols.is_empty() {
            return format!("{} module (no exports)", capitalize(filename));
        } else {
            return format!("{} module with {} symbols", capitalize(filename), symbols.len());
        }
    }
    
    // Join parts intelligently
    if parts.len() == 1 {
        parts[0].clone()
    } else {
        // First part as the main description, rest as details
        format!("{}. {}", parts[0], parts[1..].join(". "))
    }
}

/// Infer purpose from filename patterns (camelCase, snake_case, etc.)
fn infer_from_filename(filename: &str, parent: &str) -> String {
    let lower = filename.to_lowercase();
    
    // Check for common suffixes/patterns
    if lower.ends_with("utils") || lower.ends_with("util") || lower.ends_with("helpers") || lower.ends_with("helper") {
        let name = lower.replace("utils", "").replace("util", "").replace("helpers", "").replace("helper", "").trim_end_matches('_').trim_end_matches('-').to_string();
        if name.is_empty() {
            return "General utility functions".to_string();
        }
        return format!("{} utility functions", capitalize(&name));
    }
    
    if lower.ends_with("service") || lower.ends_with("services") {
        let name = lower.replace("service", "").replace("services", "").trim_end_matches('_').trim_end_matches('-').to_string();
        if name.is_empty() {
            return "Service layer logic".to_string();
        }
        return format!("{} service operations", capitalize(&name));
    }
    
    if lower.ends_with("controller") || lower.ends_with("controllers") {
        let name = lower.replace("controller", "").replace("controllers", "").trim_end_matches('_').trim_end_matches('-').to_string();
        if name.is_empty() {
            return "Request controller handlers".to_string();
        }
        return format!("{} request handlers", capitalize(&name));
    }
    
    if lower.ends_with("handler") || lower.ends_with("handlers") {
        let name = lower.replace("handler", "").replace("handlers", "").trim_end_matches('_').trim_end_matches('-').to_string();
        if name.is_empty() {
            return "Event/request handlers".to_string();
        }
        return format!("{} event handlers", capitalize(&name));
    }
    
    if lower.ends_with("model") || lower.ends_with("models") {
        let name = lower.replace("model", "").replace("models", "").trim_end_matches('_').trim_end_matches('-').to_string();
        if name.is_empty() {
            return "Data model definitions".to_string();
        }
        return format!("{} data model", capitalize(&name));
    }
    
    if lower.ends_with("api") {
        let name = lower.replace("api", "").trim_end_matches('_').trim_end_matches('-').to_string();
        if name.is_empty() {
            return "API endpoint definitions".to_string();
        }
        return format!("{} API operations", capitalize(&name));
    }
    
    if lower.ends_with("client") {
        let name = lower.replace("client", "").trim_end_matches('_').trim_end_matches('-').to_string();
        if name.is_empty() {
            return "Client implementation".to_string();
        }
        return format!("{} client", capitalize(&name));
    }
    
    if lower.ends_with("store") {
        let name = lower.replace("store", "").trim_end_matches('_').trim_end_matches('-').to_string();
        if name.is_empty() {
            return "State store management".to_string();
        }
        return format!("{} state management", capitalize(&name));
    }
    
    if lower.ends_with("hook") || lower.ends_with("hooks") {
        return "React hooks".to_string();
    }
    
    if lower.starts_with("use") && filename.len() > 3 {
        // React hook pattern: useAuth, useQuery, etc.
        let hook_name = humanize_camel_case(&filename[3..]);
        return format!("{} React hook", hook_name);
    }
    
    // Check parent directory for context
    if !parent.is_empty() {
        let parent_lower = parent.to_lowercase();
        if parent_lower == "components" || parent_lower == "component" {
            return format!("{} component", capitalize(filename));
        }
        if parent_lower == "pages" || parent_lower == "views" {
            return format!("{} page/view", capitalize(filename));
        }
        if parent_lower == "hooks" {
            return format!("{} hook", capitalize(filename));
        }
        if parent_lower == "utils" || parent_lower == "lib" || parent_lower == "helpers" {
            return format!("{} utilities", capitalize(filename));
        }
        if parent_lower == "api" || parent_lower == "routes" {
            return format!("{} API endpoints", capitalize(filename));
        }
    }
    
    // Default: humanize the filename
    let humanized = humanize_camel_case(filename);
    if humanized != filename {
        format!("{} functionality", humanized)
    } else {
        String::new()
    }
}

/// Convert camelCase or PascalCase to human-readable form
fn humanize_camel_case(s: &str) -> String {
    let mut result = String::new();
    let mut prev_was_upper = false;
    
    for (i, c) in s.chars().enumerate() {
        if c.is_uppercase() {
            if i > 0 && !prev_was_upper {
                result.push(' ');
            }
            result.push(c.to_lowercase().next().unwrap_or(c));
            prev_was_upper = true;
        } else if c == '_' || c == '-' {
            result.push(' ');
            prev_was_upper = false;
        } else {
            result.push(c);
            prev_was_upper = false;
        }
    }
    
    // Capitalize first letter
    let mut chars = result.chars();
    match chars.next() {
        None => String::new(),
        Some(first) => first.to_uppercase().chain(chars).collect(),
    }
}

/// Humanize a symbol name for display
fn humanize_name(name: &str) -> String {
    let char_count = name.chars().count();
    // Keep short names as-is
    if char_count <= 12 {
        return name.to_string();
    }
    // Truncate very long names (use char count, not byte length, for Unicode safety)
    if char_count > 25 {
        let truncated: String = name.chars().take(22).collect();
        return format!("{}...", truncated);
    }
    name.to_string()
}

/// Capitalize the first letter of a string
fn capitalize(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        None => String::new(),
        Some(c) => c.to_uppercase().chain(chars).collect(),
    }
}

/// Try to resolve an import path to a file path
fn resolve_import_path(import: &str, from_file: &Path) -> Option<PathBuf> {
    // Handle relative imports
    if import.starts_with('.') {
        let parent = from_file.parent()?;
        let cleaned = import.trim_start_matches("./").trim_start_matches("../");
        
        // Try common extensions
        for ext in &["rs", "ts", "tsx", "js", "jsx", "py", "go"] {
            let candidate = parent.join(format!("{}.{}", cleaned, ext));
            if candidate.exists() {
                return Some(candidate);
            }
        }
        
        // Try as directory with index
        let dir_candidate = parent.join(cleaned).join("mod.rs");
        if dir_candidate.exists() {
            return Some(dir_candidate);
        }
    }
    
    // Handle crate/module imports (simplified)
    if import.starts_with("crate::") || import.starts_with("super::") {
        let parts: Vec<&str> = import.split("::").collect();
        if parts.len() >= 2 {
            // Build path from parts
            let path_parts: Vec<&str> = parts[1..].iter()
                .take_while(|p| !p.chars().next().map(|c| c.is_uppercase()).unwrap_or(false))
                .copied()
                .collect();
            
            if !path_parts.is_empty() {
                let path_str = path_parts.join("/");
                return Some(PathBuf::from(format!("src/{}.rs", path_str)));
            }
        }
    }
    
    None
}

/// Index of a single file
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileIndex {
    pub path: PathBuf,
    pub language: Language,
    pub loc: usize,
    pub sloc: usize, // Source lines (excluding blanks/comments)
    pub symbols: Vec<Symbol>,
    pub dependencies: Vec<Dependency>,
    pub patterns: Vec<Pattern>,
    pub complexity: f64,
    pub last_modified: DateTime<Utc>,
    /// File summary for quick reference
    #[serde(default)]
    pub summary: FileSummary,
    /// Architectural layer (populated by grouping module)
    #[serde(default)]
    pub layer: Option<crate::grouping::Layer>,
    /// Feature name within the layer (populated by grouping module)
    #[serde(default)]
    pub feature: Option<String>,
}

impl FileIndex {
    pub fn suggestion_density(&self) -> f64 {
        let pattern_weight: f64 = self.patterns.iter()
            .map(|p| match p.kind.severity() {
                PatternSeverity::High => 3.0,
                PatternSeverity::Medium => 2.0,
                PatternSeverity::Low => 1.0,
                PatternSeverity::Info => 0.5,
            })
            .sum();
        
        // Normalize by file size
        if self.loc > 0 {
            pattern_weight / (self.loc as f64 / 100.0)
        } else {
            0.0
        }
    }

    pub fn priority_indicator(&self) -> char {
        let density = self.suggestion_density();
        if density > 5.0 {
            '\u{25CF}' // ● High
        } else if density > 2.0 {
            '\u{25D0}' // ◐ Medium
        } else if density > 0.0 {
            '\u{25CB}' // ○ Low
        } else {
            ' ' // None
        }
    }
}

/// The complete codebase index
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CodebaseIndex {
    pub root: PathBuf,
    pub files: HashMap<PathBuf, FileIndex>,
    pub symbols: Vec<Symbol>,
    pub dependencies: Vec<Dependency>,
    pub patterns: Vec<Pattern>,
    pub cached_at: DateTime<Utc>,
}

impl CodebaseIndex {
    /// Create a new index for a codebase
    pub fn new(root: &Path) -> anyhow::Result<Self> {
        let mut index = Self {
            root: root.to_path_buf(),
            files: HashMap::new(),
            symbols: Vec::new(),
            dependencies: Vec::new(),
            patterns: Vec::new(),
            cached_at: Utc::now(),
        };

        index.scan(root)?;
        
        // Build the dependency graph after all files are indexed
        index.build_dependency_graph();
        
        Ok(index)
    }

    /// Scan directory and index all supported files
    fn scan(&mut self, root: &Path) -> anyhow::Result<()> {
        for entry in WalkDir::new(root)
            .into_iter()
            .filter_entry(|e| !is_ignored(e.path()))
            .filter_map(|e| e.ok())
        {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }

            let ext = path.extension()
                .and_then(|e| e.to_str())
                .unwrap_or("");
            
            let language = Language::from_extension(ext);
            if language == Language::Unknown {
                continue;
            }

            if let Ok(file_index) = self.index_file(path, language) {
                // Aggregate symbols, dependencies, patterns
                self.symbols.extend(file_index.symbols.clone());
                self.dependencies.extend(file_index.dependencies.clone());
                self.patterns.extend(file_index.patterns.clone());
                
                let rel_path = path.strip_prefix(root).unwrap_or(path).to_path_buf();
                self.files.insert(rel_path, file_index);
            }
        }

        Ok(())
    }

    /// Index a single file
    fn index_file(&self, path: &Path, language: Language) -> anyhow::Result<FileIndex> {
        let content = std::fs::read_to_string(path)?;
        let metadata = std::fs::metadata(path)?;
        let modified = metadata.modified()
            .map(|t| DateTime::<Utc>::from(t))
            .unwrap_or_else(|_| Utc::now());

        let loc = content.lines().count();
        let sloc = content.lines()
            .filter(|l| !l.trim().is_empty())
            .count();

        // Parse with tree-sitter
        let (symbols, deps) = parser::parse_file(path, &content, language)?;
        
        // Detect patterns
        let mut patterns = Vec::new();
        
        // Check for long functions
        for sym in &symbols {
            if matches!(sym.kind, SymbolKind::Function | SymbolKind::Method) {
                if sym.line_count() > 50 {
                    patterns.push(Pattern {
                        kind: PatternKind::LongFunction,
                        file: path.to_path_buf(),
                        line: sym.line,
                        description: format!("{} is {} lines", sym.name, sym.line_count()),
                    });
                }
            }
        }
        
        // Check for god module
        if loc > 500 {
            patterns.push(Pattern {
                kind: PatternKind::GodModule,
                file: path.to_path_buf(),
                line: 1,
                description: format!("File has {} lines", loc),
            });
        }

        // Scan for TODO/FIXME
        for (i, line) in content.lines().enumerate() {
            let upper = line.to_uppercase();
            if upper.contains("TODO") || upper.contains("FIXME") || upper.contains("HACK") {
                patterns.push(Pattern {
                    kind: PatternKind::TodoMarker,
                    file: path.to_path_buf(),
                    line: i + 1,
                    description: line.trim().to_string(),
                });
            }
        }

        // Calculate complexity (simplified cyclomatic)
        let complexity = calculate_complexity(&content, language);

        let mut file_index = FileIndex {
            path: path.to_path_buf(),
            language,
            loc,
            sloc,
            symbols,
            dependencies: deps,
            patterns,
            complexity,
            last_modified: modified,
            summary: FileSummary::default(),
            layer: None,
            feature: None,
        };
        
        // Generate summary (rel_path will be set properly after insertion)
        let rel_path = path.strip_prefix(&self.root).unwrap_or(path);
        file_index.summary = FileSummary::from_file_index(&file_index, rel_path);
        
        Ok(file_index)
    }
    
    /// Build the dependency graph (populate used_by for all files)
    pub fn build_dependency_graph(&mut self) {
        // Collect all dependencies first
        let mut used_by_map: HashMap<PathBuf, Vec<PathBuf>> = HashMap::new();
        
        for (file_path, file_index) in &self.files {
            for dep in &file_index.summary.depends_on {
                // Normalize the dependency path
                let dep_normalized = normalize_path(dep);
                
                // Find matching file in index
                for indexed_path in self.files.keys() {
                    if paths_match(indexed_path, &dep_normalized) {
                        used_by_map
                            .entry(indexed_path.clone())
                            .or_default()
                            .push(file_path.clone());
                    }
                }
            }
        }
        
        // Now update each file's used_by
        for (path, used_by) in used_by_map {
            if let Some(file_index) = self.files.get_mut(&path) {
                file_index.summary.used_by = used_by;
            }
        }
    }

    /// Get files sorted by suggestion density (most actionable first)
    pub fn files_by_priority(&self) -> Vec<(&PathBuf, &FileIndex)> {
        let mut files: Vec<_> = self.files.iter().collect();
        files.sort_by(|a, b| {
            b.1.suggestion_density()
                .partial_cmp(&a.1.suggestion_density())
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        files
    }

    /// Get total statistics
    pub fn stats(&self) -> IndexStats {
        IndexStats {
            file_count: self.files.len(),
            total_loc: self.files.values().map(|f| f.loc).sum(),
            total_sloc: self.files.values().map(|f| f.sloc).sum(),
            symbol_count: self.symbols.len(),
            pattern_count: self.patterns.len(),
            high_priority_patterns: self.patterns.iter()
                .filter(|p| p.kind.severity() >= PatternSeverity::High)
                .count(),
        }
    }

    /// Get file tree structure
    pub fn file_tree(&self) -> FileTree {
        let mut tree = FileTree::new();
        for path in self.files.keys() {
            tree.insert(path, &self.files[path]);
        }
        tree
    }

    /// Apply grouping information to file indexes
    pub fn apply_grouping(&mut self, grouping: &crate::grouping::CodebaseGrouping) {
        for (path, (layer, feature)) in &grouping.file_assignments {
            if let Some(file_index) = self.files.get_mut(path) {
                file_index.layer = Some(*layer);
                file_index.feature = feature.clone();
            }
        }
    }

    /// Generate grouping for this codebase using heuristics
    pub fn generate_grouping(&self) -> crate::grouping::CodebaseGrouping {
        let mut grouping = crate::grouping::heuristics::categorize_codebase(self);
        crate::grouping::features::detect_features(&mut grouping, self);
        grouping
    }
}

#[derive(Debug, Clone)]
pub struct IndexStats {
    pub file_count: usize,
    pub total_loc: usize,
    pub total_sloc: usize,
    pub symbol_count: usize,
    pub pattern_count: usize,
    pub high_priority_patterns: usize,
}

/// File tree structure for UI display
#[derive(Debug, Clone, Default)]
pub struct FileTree {
    pub entries: Vec<FileTreeEntry>,
}

#[derive(Debug, Clone)]
pub struct FileTreeEntry {
    pub name: String,
    pub path: PathBuf,
    pub is_dir: bool,
    pub depth: usize,
    pub priority: char,
    pub expanded: bool,
    pub children: Vec<FileTreeEntry>,
}

impl FileTree {
    pub fn new() -> Self {
        Self { entries: Vec::new() }
    }

    pub fn insert(&mut self, path: &Path, file_index: &FileIndex) {
        let components: Vec<_> = path.components().collect();
        self.insert_recursive(&mut self.entries.clone(), &components, 0, file_index);
    }

    fn insert_recursive(
        &mut self,
        _entries: &mut Vec<FileTreeEntry>,
        components: &[std::path::Component],
        depth: usize,
        file_index: &FileIndex,
    ) {
        // Simplified tree building - actual implementation would be more complex
        if components.is_empty() {
            return;
        }

        let name = components[0].as_os_str().to_string_lossy().to_string();
        let is_last = components.len() == 1;
        
        let entry = FileTreeEntry {
            name,
            path: file_index.path.clone(),
            is_dir: !is_last,
            depth,
            priority: if is_last { file_index.priority_indicator() } else { ' ' },
            expanded: false,
            children: Vec::new(),
        };

        self.entries.push(entry);
    }

    /// Flatten tree for display
    pub fn flatten(&self) -> Vec<FlatTreeEntry> {
        let mut result = Vec::new();
        self.flatten_recursive(&self.entries, &mut result);
        result
    }

    fn flatten_recursive(&self, entries: &[FileTreeEntry], result: &mut Vec<FlatTreeEntry>) {
        for entry in entries {
            result.push(FlatTreeEntry {
                name: entry.name.clone(),
                path: entry.path.clone(),
                is_dir: entry.is_dir,
                depth: entry.depth,
                priority: entry.priority,
            });
            if entry.expanded {
                self.flatten_recursive(&entry.children, result);
            }
        }
    }
}

#[derive(Debug, Clone)]
pub struct FlatTreeEntry {
    pub name: String,
    pub path: PathBuf,
    pub is_dir: bool,
    pub depth: usize,
    pub priority: char,
}

/// Calculate cyclomatic complexity (simplified)
fn calculate_complexity(content: &str, _language: Language) -> f64 {
    // Count decision points
    let decision_keywords = [
        "if ", "else ", "elif ", "for ", "while ", "match ", 
        "case ", "catch ", "&&", "||", "?", "try ", "switch "
    ];
    
    let mut complexity = 1.0; // Base complexity
    
    for keyword in &decision_keywords {
        complexity += content.matches(keyword).count() as f64;
    }
    
    complexity
}

/// Check if a path should be ignored
fn is_ignored(path: &Path) -> bool {
    let name = path.file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("");
    
    // Common ignore patterns
    let ignored = [
        "target", "node_modules", ".git", ".svn", ".hg",
        "dist", "build", "__pycache__", ".pytest_cache",
        "vendor", ".idea", ".vscode", ".cosmos",
    ];
    
    ignored.contains(&name) || name.starts_with('.')
}

/// Normalize a path by removing redundant components
fn normalize_path(path: &Path) -> PathBuf {
    let mut result = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::ParentDir => {
                result.pop();
            }
            std::path::Component::CurDir => {}
            _ => {
                result.push(component);
            }
        }
    }
    result
}

/// Check if two paths match (accounting for src/ prefix and extensions)
fn paths_match(indexed: &Path, dependency: &Path) -> bool {
    // Direct match
    if indexed == dependency {
        return true;
    }
    
    // Try without extensions
    let indexed_stem = indexed.with_extension("");
    let dep_stem = dependency.with_extension("");
    
    if indexed_stem == dep_stem {
        return true;
    }
    
    // Try matching file names only
    let indexed_name = indexed.file_stem().and_then(|n| n.to_str());
    let dep_name = dependency.file_stem().and_then(|n| n.to_str());
    
    if let (Some(i), Some(d)) = (indexed_name, dep_name) {
        // Handle mod.rs -> directory mapping
        if i == "mod" {
            if let Some(indexed_parent) = indexed.parent().and_then(|p| p.file_name()).and_then(|n| n.to_str()) {
                return indexed_parent == d;
            }
        }
        if d == "mod" {
            if let Some(dep_parent) = dependency.parent().and_then(|p| p.file_name()).and_then(|n| n.to_str()) {
                return dep_parent == i;
            }
        }
    }
    
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_language_detection() {
        assert_eq!(Language::from_extension("rs"), Language::Rust);
        assert_eq!(Language::from_extension("js"), Language::JavaScript);
        assert_eq!(Language::from_extension("ts"), Language::TypeScript);
        assert_eq!(Language::from_extension("py"), Language::Python);
        assert_eq!(Language::from_extension("go"), Language::Go);
        assert_eq!(Language::from_extension("txt"), Language::Unknown);
    }

    #[test]
    fn test_complexity_calculation() {
        let code = "if x { } else { } for i in items { if y { } }";
        let complexity = calculate_complexity(code, Language::Rust);
        assert!(complexity > 1.0);
    }

    #[test]
    fn test_pattern_severity() {
        assert!(PatternKind::DeepNesting.severity() > PatternKind::UnusedImport.severity());
    }
}
