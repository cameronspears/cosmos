//! Codebase indexing engine for Cosmos
//!
//! Uses tree-sitter for multi-language AST parsing to build
//! semantic understanding of the codebase.

pub mod parser;

use crate::util::hash_str;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

// ═══════════════════════════════════════════════════════════════════════════
//  PATTERN DETECTION THRESHOLDS
// ═══════════════════════════════════════════════════════════════════════════

/// Number of lines above which a function is considered "long"
pub const LONG_FUNCTION_THRESHOLD: usize = 50;

/// Number of lines above which a file is considered a "god module"
pub const GOD_MODULE_LOC_THRESHOLD: usize = 500;

/// Maximum file size (bytes) to index for AST parsing
pub const MAX_INDEX_FILE_BYTES: u64 = 1_000_000;

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
    #[serde(default)]
    pub reliability: PatternReliability,
}

/// A file that was skipped during indexing (with a reason)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexError {
    pub path: PathBuf,
    pub reason: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
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
    /// Potential resource leak or unbounded resource retention
    PotentialResourceLeak,
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
            PatternKind::PotentialResourceLeak => PatternSeverity::High,
            PatternKind::UnusedImport => PatternSeverity::Low,
            PatternKind::TodoMarker => PatternSeverity::Info,
        }
    }

    pub fn reliability(&self) -> PatternReliability {
        match self {
            PatternKind::MissingErrorHandling => PatternReliability::High,
            PatternKind::PotentialResourceLeak => PatternReliability::Medium,
            PatternKind::LongFunction => PatternReliability::Medium,
            PatternKind::DeepNesting => PatternReliability::Medium,
            PatternKind::ManyParameters => PatternReliability::Low,
            PatternKind::GodModule => PatternReliability::Low,
            PatternKind::DuplicatePattern => PatternReliability::Low,
            PatternKind::UnusedImport => PatternReliability::Low,
            PatternKind::TodoMarker => PatternReliability::Low,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum PatternReliability {
    Low,
    #[default]
    Medium,
    High,
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
}

impl FileSummary {
    /// Generate a static summary from file index data
    pub fn from_file_index(file_index: &FileIndex, rel_path: &Path, root: &Path) -> Self {
        // Infer purpose from filename and exports
        let purpose = infer_purpose(rel_path, &file_index.symbols, file_index.language);

        // Get public exports
        let exports: Vec<String> = file_index
            .symbols
            .iter()
            .filter(|s| s.visibility == Visibility::Public)
            .map(|s| s.name.clone())
            .take(10)
            .collect();

        // depends_on will be populated by the codebase index
        let depends_on: Vec<PathBuf> = file_index
            .dependencies
            .iter()
            .filter(|d| !d.is_external)
            .filter_map(|d| resolve_import_path(&d.import_path, rel_path, root))
            .collect();

        Self {
            purpose,
            exports,
            used_by: Vec::new(), // Populated later by build_dependency_graph
            depends_on,
        }
    }
}

/// Infer the purpose of a file from its name and exports
fn infer_purpose(path: &Path, symbols: &[Symbol], _language: Language) -> String {
    let filename = path
        .file_stem()
        .and_then(|n| n.to_str())
        .unwrap_or("unknown");

    let parent = path
        .parent()
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
    let public_symbols: Vec<_> = symbols
        .iter()
        .filter(|s| s.visibility == Visibility::Public)
        .collect();

    let functions: Vec<&str> = public_symbols
        .iter()
        .filter(|s| matches!(s.kind, SymbolKind::Function | SymbolKind::Method))
        .map(|s| s.name.as_str())
        .collect();

    let types: Vec<&str> = public_symbols
        .iter()
        .filter(|s| {
            matches!(
                s.kind,
                SymbolKind::Struct | SymbolKind::Class | SymbolKind::Enum
            )
        })
        .map(|s| s.name.as_str())
        .collect();

    let traits: Vec<&str> = public_symbols
        .iter()
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
            format!(
                "Defines {} and {} other types",
                types[..2].join(", "),
                types.len() - 2
            )
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
            format!(
                "Provides {} and {} other functions",
                names.join(", "),
                functions.len() - 3
            )
        };
        parts.push(func_desc);
    }

    // If we still have nothing useful, provide a basic description
    if parts.is_empty() {
        if symbols.is_empty() {
            return format!("{} module (no exports)", capitalize(filename));
        } else {
            return format!(
                "{} module with {} symbols",
                capitalize(filename),
                symbols.len()
            );
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
    if lower.ends_with("utils")
        || lower.ends_with("util")
        || lower.ends_with("helpers")
        || lower.ends_with("helper")
    {
        let name = lower
            .replace("utils", "")
            .replace("util", "")
            .replace("helpers", "")
            .replace("helper", "")
            .trim_end_matches('_')
            .trim_end_matches('-')
            .to_string();
        if name.is_empty() {
            return "General utility functions".to_string();
        }
        return format!("{} utility functions", capitalize(&name));
    }

    if lower.ends_with("service") || lower.ends_with("services") {
        let name = lower
            .replace("service", "")
            .replace("services", "")
            .trim_end_matches('_')
            .trim_end_matches('-')
            .to_string();
        if name.is_empty() {
            return "Service layer logic".to_string();
        }
        return format!("{} service operations", capitalize(&name));
    }

    if lower.ends_with("controller") || lower.ends_with("controllers") {
        let name = lower
            .replace("controller", "")
            .replace("controllers", "")
            .trim_end_matches('_')
            .trim_end_matches('-')
            .to_string();
        if name.is_empty() {
            return "Request controller handlers".to_string();
        }
        return format!("{} request handlers", capitalize(&name));
    }

    if lower.ends_with("handler") || lower.ends_with("handlers") {
        let name = lower
            .replace("handler", "")
            .replace("handlers", "")
            .trim_end_matches('_')
            .trim_end_matches('-')
            .to_string();
        if name.is_empty() {
            return "Event/request handlers".to_string();
        }
        return format!("{} event handlers", capitalize(&name));
    }

    if lower.ends_with("model") || lower.ends_with("models") {
        let name = lower
            .replace("model", "")
            .replace("models", "")
            .trim_end_matches('_')
            .trim_end_matches('-')
            .to_string();
        if name.is_empty() {
            return "Data model definitions".to_string();
        }
        return format!("{} data model", capitalize(&name));
    }

    if lower.ends_with("api") {
        let name = lower
            .replace("api", "")
            .trim_end_matches('_')
            .trim_end_matches('-')
            .to_string();
        if name.is_empty() {
            return "API endpoint definitions".to_string();
        }
        return format!("{} API operations", capitalize(&name));
    }

    if lower.ends_with("client") {
        let name = lower
            .replace("client", "")
            .trim_end_matches('_')
            .trim_end_matches('-')
            .to_string();
        if name.is_empty() {
            return "Client implementation".to_string();
        }
        return format!("{} client", capitalize(&name));
    }

    if lower.ends_with("store") {
        let name = lower
            .replace("store", "")
            .trim_end_matches('_')
            .trim_end_matches('-')
            .to_string();
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
fn resolve_import_path(import: &str, from_file: &Path, root: &Path) -> Option<PathBuf> {
    // Handle relative imports
    if import.starts_with('.') {
        let parent = from_file.parent()?;
        let base = normalize_path(&parent.join(import));
        let base_on_disk = root.join(&base);

        if base.extension().is_some() {
            if base_on_disk.exists() {
                return Some(base);
            }
        } else {
            // Try common extensions
            for ext in &["rs", "ts", "tsx", "js", "jsx", "py", "go"] {
                let candidate = base.with_extension(ext);
                if root.join(&candidate).exists() {
                    return Some(candidate);
                }
            }

            // Try as directory with index
            let dir_candidate = base.join("mod.rs");
            if root.join(&dir_candidate).exists() {
                return Some(dir_candidate);
            }
        }
    }

    // Handle crate/module imports (simplified)
    if import.starts_with("crate::") || import.starts_with("super::") {
        let parts: Vec<&str> = import.split("::").collect();
        if parts.len() >= 2 {
            // Build path from parts
            let path_parts: Vec<&str> = parts[1..]
                .iter()
                .take_while(|p| !p.chars().next().map(|c| c.is_uppercase()).unwrap_or(false))
                .copied()
                .collect();

            if !path_parts.is_empty() {
                let path_str = path_parts.join("/");
                let candidate = PathBuf::from(format!("src/{}.rs", path_str));
                if root.join(&candidate).exists() {
                    return Some(candidate);
                }
                let module_candidate = PathBuf::from(format!("src/{}/mod.rs", path_str));
                if root.join(&module_candidate).exists() {
                    return Some(module_candidate);
                }
                return None;
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
    /// Stable hash of file contents for cache invalidation
    #[serde(default)]
    pub content_hash: String,
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
        let pattern_weight: f64 = self
            .patterns
            .iter()
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
    #[serde(default)]
    pub index_errors: Vec<IndexError>,
    /// Git HEAD commit hash at time of indexing (for fast cache validation)
    #[serde(default)]
    pub git_head: Option<String>,
}

impl CodebaseIndex {
    /// Create a new index for a codebase
    pub fn new(root: &Path) -> anyhow::Result<Self> {
        // Capture git HEAD for fast cache validation
        let git_head = get_git_head(root);

        let mut index = Self {
            root: root.to_path_buf(),
            files: HashMap::new(),
            index_errors: Vec::new(),
            git_head,
        };

        index.scan(root)?;

        // Build the dependency graph after all files are indexed
        index.build_dependency_graph();

        Ok(index)
    }

    /// Scan directory and index all supported files
    fn scan(&mut self, root: &Path) -> anyhow::Result<()> {
        use rayon::prelude::*;

        // Phase 1: Collect all file paths (single-threaded, fast)
        let file_entries: Vec<_> = WalkDir::new(root)
            .into_iter()
            // Never prune traversal at depth 0 (the scan root itself), even if its
            // basename matches an ignored directory name like "target".
            .filter_entry(|e| e.depth() == 0 || !is_ignored(e.path()))
            .filter_map(|e| e.ok())
            .filter(|e| e.path().is_file())
            .filter_map(|entry| {
                let path = entry.path();
                let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
                let language = Language::from_extension(ext);
                if language == Language::Unknown {
                    None
                } else {
                    Some((path.to_path_buf(), language))
                }
            })
            .collect();

        // Phase 2: Index files in parallel
        let results: Vec<_> = file_entries
            .par_iter()
            .map(|(path, language)| {
                let rel_path = path.strip_prefix(root).unwrap_or(path).to_path_buf();
                match Self::index_file_static(path, *language, root) {
                    Ok(file_index) => Ok((rel_path, file_index)),
                    Err(err) => Err((rel_path, err.to_string())),
                }
            })
            .collect();

        // Phase 3: Merge results (single-threaded)
        for result in results {
            match result {
                Ok((rel_path, file_index)) => {
                    self.files.insert(rel_path, file_index);
                }
                Err((rel_path, reason)) => {
                    self.index_errors.push(IndexError {
                        path: rel_path,
                        reason,
                    });
                }
            }
        }

        Ok(())
    }

    /// Index a single file (static version for parallel processing)
    fn index_file_static(
        path: &Path,
        language: Language,
        root: &Path,
    ) -> anyhow::Result<FileIndex> {
        let metadata = std::fs::metadata(path)?;
        if metadata.len() > MAX_INDEX_FILE_BYTES {
            return Err(anyhow::anyhow!(
                "File too large to index ({} bytes, limit {} bytes)",
                metadata.len(),
                MAX_INDEX_FILE_BYTES
            ));
        }

        let bytes = std::fs::read(path)?;
        if bytes.len() as u64 > MAX_INDEX_FILE_BYTES {
            return Err(anyhow::anyhow!(
                "File too large to index ({} bytes, limit {} bytes)",
                bytes.len(),
                MAX_INDEX_FILE_BYTES
            ));
        }

        let content = String::from_utf8(bytes)
            .map_err(|_| anyhow::anyhow!("File is not valid UTF-8, skipping"))?;

        let modified = metadata
            .modified()
            .map(DateTime::<Utc>::from)
            .unwrap_or_else(|_| Utc::now());

        let content_hash = hash_str(&content);

        // Single-pass content analysis: loc, sloc, complexity, TODOs
        let analysis = analyze_content_single_pass(&content);

        // Parse with tree-sitter
        let (symbols, deps) = parser::parse_file(path, &content, language)?;

        // Detect patterns from symbols and analysis
        let mut patterns = Vec::new();

        // Check for long functions
        for sym in &symbols {
            if matches!(sym.kind, SymbolKind::Function | SymbolKind::Method)
                && sym.line_count() > LONG_FUNCTION_THRESHOLD
            {
                patterns.push(Pattern {
                    kind: PatternKind::LongFunction,
                    file: path.to_path_buf(),
                    line: sym.line,
                    description: format!("{} is {} lines", sym.name, sym.line_count()),
                    reliability: PatternKind::LongFunction.reliability(),
                });
            }
        }

        // Check for god module
        if analysis.loc > GOD_MODULE_LOC_THRESHOLD {
            patterns.push(Pattern {
                kind: PatternKind::GodModule,
                file: path.to_path_buf(),
                line: 1,
                description: format!("File has {} lines", analysis.loc),
                reliability: PatternKind::GodModule.reliability(),
            });
        }

        // Add TODO patterns from single-pass analysis
        for (line_num, description) in analysis.todo_patterns {
            patterns.push(Pattern {
                kind: PatternKind::TodoMarker,
                file: path.to_path_buf(),
                line: line_num,
                description,
                reliability: PatternKind::TodoMarker.reliability(),
            });
        }

        for (line_num, description) in analysis.missing_error_patterns {
            patterns.push(Pattern {
                kind: PatternKind::MissingErrorHandling,
                file: path.to_path_buf(),
                line: line_num,
                description,
                reliability: PatternKind::MissingErrorHandling.reliability(),
            });
        }

        for (line_num, description) in analysis.resource_leak_patterns {
            patterns.push(Pattern {
                kind: PatternKind::PotentialResourceLeak,
                file: path.to_path_buf(),
                line: line_num,
                description,
                reliability: PatternKind::PotentialResourceLeak.reliability(),
            });
        }

        let mut file_index = FileIndex {
            path: path.to_path_buf(),
            language,
            loc: analysis.loc,
            content_hash,
            symbols,
            dependencies: deps,
            patterns,
            complexity: analysis.complexity,
            last_modified: modified,
            summary: FileSummary::default(),
            layer: None,
            feature: None,
        };

        // Generate summary
        let rel_path = path.strip_prefix(root).unwrap_or(path);
        file_index.summary = FileSummary::from_file_index(&file_index, rel_path, root);

        Ok(file_index)
    }

    /// Build the dependency graph (populate used_by for all files)
    pub fn build_dependency_graph(&mut self) {
        // Precompute lookup tables to avoid quadratic scans
        let mut path_lookup: HashMap<PathBuf, Vec<PathBuf>> = HashMap::new();
        let mut stem_lookup: HashMap<String, Vec<PathBuf>> = HashMap::new();
        let mut mod_parent_lookup: HashMap<String, Vec<PathBuf>> = HashMap::new();

        for indexed_path in self.files.keys() {
            let normalized = normalize_path(indexed_path);
            path_lookup
                .entry(normalized.clone())
                .or_default()
                .push(indexed_path.clone());
            path_lookup
                .entry(normalized.with_extension(""))
                .or_default()
                .push(indexed_path.clone());

            if let Some(stem) = normalized.file_stem().and_then(|n| n.to_str()) {
                if !stem.is_empty() {
                    stem_lookup
                        .entry(stem.to_string())
                        .or_default()
                        .push(indexed_path.clone());
                }
                if stem == "mod" {
                    if let Some(parent) = normalized
                        .parent()
                        .and_then(|p| p.file_name())
                        .and_then(|n| n.to_str())
                    {
                        mod_parent_lookup
                            .entry(parent.to_string())
                            .or_default()
                            .push(indexed_path.clone());
                    }
                }
            }
        }

        // Collect all dependencies first
        let mut used_by_map: HashMap<PathBuf, Vec<PathBuf>> = HashMap::new();

        for (file_path, file_index) in &self.files {
            for dep in &file_index.summary.depends_on {
                // Normalize the dependency path
                let dep_normalized = normalize_path(dep);
                let mut matches: std::collections::HashSet<PathBuf> =
                    std::collections::HashSet::new();

                if let Some(paths) = path_lookup.get(&dep_normalized) {
                    matches.extend(paths.iter().cloned());
                }
                let dep_no_ext = dep_normalized.with_extension("");
                if let Some(paths) = path_lookup.get(&dep_no_ext) {
                    matches.extend(paths.iter().cloned());
                }

                if let Some(dep_stem) = dep_normalized.file_stem().and_then(|n| n.to_str()) {
                    if let Some(paths) = stem_lookup.get(dep_stem) {
                        matches.extend(paths.iter().cloned());
                    }
                    if let Some(paths) = mod_parent_lookup.get(dep_stem) {
                        matches.extend(paths.iter().cloned());
                    }
                    if dep_stem == "mod" {
                        if let Some(parent) = dep_normalized
                            .parent()
                            .and_then(|p| p.file_name())
                            .and_then(|n| n.to_str())
                        {
                            if let Some(paths) = stem_lookup.get(parent) {
                                matches.extend(paths.iter().cloned());
                            }
                        }
                    }
                }

                for indexed_path in matches {
                    used_by_map
                        .entry(indexed_path)
                        .or_default()
                        .push(file_path.clone());
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

    /// Get total statistics
    pub fn stats(&self) -> IndexStats {
        IndexStats {
            file_count: self.files.len(),
            total_loc: self.files.values().map(|f| f.loc).sum(),
            symbol_count: self.files.values().map(|f| f.symbols.len()).sum(),
            skipped_files: self.index_errors.len(),
        }
    }

    /// Apply grouping information to file indexes
    pub fn apply_grouping(&mut self, grouping: &crate::grouping::CodebaseGrouping) {
        for (path, assignment) in &grouping.file_assignments {
            if let Some(file_index) = self.files.get_mut(path) {
                file_index.layer = Some(assignment.layer);
                file_index.feature = assignment.feature.clone();
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
    pub symbol_count: usize,
    pub skipped_files: usize,
}

/// Flattened file tree entry for UI display
#[derive(Debug, Clone)]
pub struct FlatTreeEntry {
    pub name: String,
    pub path: PathBuf,
    pub is_dir: bool,
    pub depth: usize,
    pub priority: char,
}

/// Result of single-pass content analysis
struct ContentAnalysis {
    loc: usize,
    complexity: f64,
    todo_patterns: Vec<(usize, String)>, // (line_number, description)
    missing_error_patterns: Vec<(usize, String)>,
    resource_leak_patterns: Vec<(usize, String)>,
}

/// Analyze content in a single pass: count lines, calculate complexity, find TODOs
fn analyze_content_single_pass(content: &str) -> ContentAnalysis {
    let mut loc = 0;
    let mut complexity = 1.0; // Base complexity
    let mut todo_patterns = Vec::new();
    let mut missing_error_patterns = Vec::new();
    let mut resource_leak_patterns = Vec::new();

    // Decision point keywords for complexity
    let decision_keywords = [
        "if ", "else ", "elif ", "for ", "while ", "match ", "case ", "catch ", "&&", "||", "?",
        "try ", "switch ",
    ];

    for (i, line) in content.lines().enumerate() {
        loc += 1;

        // Check for TODO/FIXME/HACK markers
        let upper = line.to_uppercase();
        if upper.contains("TODO") || upper.contains("FIXME") || upper.contains("HACK") {
            todo_patterns.push((i + 1, line.trim().to_string()));
        }

        let trimmed = line.trim();
        if trimmed.contains(".unwrap()") || trimmed.contains(".expect(") {
            missing_error_patterns.push((
                i + 1,
                "Unchecked unwrap/expect can panic instead of handling failure".to_string(),
            ));
        }
        if trimmed.contains("catch") && trimmed.contains("{}") {
            missing_error_patterns.push((
                i + 1,
                "Empty catch block swallows errors without handling".to_string(),
            ));
        }
        if trimmed.contains("Err(_) => {}") || trimmed.contains("Err(_e) => {}") {
            missing_error_patterns.push((i + 1, "Empty error branch ignores failures".to_string()));
        }
        if trimmed.contains("std::mem::forget(") || trimmed.contains("Box::leak(") {
            resource_leak_patterns.push((
                i + 1,
                "Potential resource leak keeps memory/resources alive indefinitely".to_string(),
            ));
        }

        // Count complexity decision points in this line
        for keyword in &decision_keywords {
            complexity += line.matches(keyword).count() as f64;
        }
    }

    ContentAnalysis {
        loc,
        complexity,
        todo_patterns,
        missing_error_patterns,
        resource_leak_patterns,
    }
}

/// Check if a path should be ignored
fn is_ignored(path: &Path) -> bool {
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");

    // Common ignore patterns
    let ignored = [
        "target",
        "node_modules",
        ".git",
        ".svn",
        ".hg",
        "dist",
        "build",
        "__pycache__",
        ".pytest_cache",
        "vendor",
        ".idea",
        ".vscode",
        ".cosmos",
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

/// Get the current git HEAD commit hash for the repository
fn get_git_head(root: &Path) -> Option<String> {
    use std::process::Command;

    let output = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(root)
        .output()
        .ok()?;

    if output.status.success() {
        let head = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if !head.is_empty() {
            return Some(head);
        }
    }
    None
}

/// Check if there are uncommitted changes in the git repository
pub fn has_uncommitted_changes(root: &Path) -> bool {
    use std::process::Command;

    let output = match Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(root)
        .output()
    {
        Ok(o) => o,
        Err(_) => return true, // Assume changes if git fails
    };

    if output.status.success() {
        // If output is empty, no uncommitted changes
        !output.stdout.is_empty()
    } else {
        true // Assume changes if command fails
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

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
        let analysis = analyze_content_single_pass(code);
        assert!(analysis.complexity > 1.0);
    }

    #[test]
    fn test_pattern_severity() {
        assert!(PatternKind::DeepNesting.severity() > PatternKind::UnusedImport.severity());
    }

    #[test]
    fn test_resolve_import_path_uses_repo_root() {
        let mut root = std::env::temp_dir();
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        root.push(format!("cosmos_index_test_{}", nanos));

        let src_dir = root.join("src");
        fs::create_dir_all(&src_dir).unwrap();
        fs::write(src_dir.join("foo.rs"), "").unwrap();

        let resolved = resolve_import_path("./foo", Path::new("src/bar.rs"), &root);
        assert_eq!(resolved, Some(PathBuf::from("src/foo.rs")));

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn test_scan_does_not_ignore_root_named_target() {
        let mut parent = std::env::temp_dir();
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        parent.push(format!("cosmos_index_root_target_{}", nanos));

        let root = parent.join("target");
        let src_dir = root.join("src");
        fs::create_dir_all(&src_dir).unwrap();
        fs::write(src_dir.join("main.rs"), "fn main() {}\n").unwrap();

        let index = CodebaseIndex::new(&root).unwrap();
        assert!(index.stats().file_count > 0);

        let _ = fs::remove_dir_all(&parent);
    }
}
