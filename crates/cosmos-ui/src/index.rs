//! Minimal index models for UI shell mode.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

pub const MAX_INDEX_FILE_BYTES: u64 = 1_000_000;

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
            "rs" => Self::Rust,
            "js" | "jsx" | "mjs" | "cjs" => Self::JavaScript,
            "ts" | "tsx" => Self::TypeScript,
            "py" | "pyi" => Self::Python,
            "go" => Self::Go,
            _ => Self::Unknown,
        }
    }

    pub fn icon(&self) -> &'static str {
        match self {
            Self::Rust => "rs",
            Self::JavaScript => "js",
            Self::TypeScript => "ts",
            Self::Python => "py",
            Self::Go => "go",
            Self::Unknown => "??",
        }
    }
}

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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Dependency {
    pub from_file: PathBuf,
    pub import_path: String,
    pub line: usize,
    pub is_external: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Pattern {
    pub kind: PatternKind,
    pub file: PathBuf,
    pub line: usize,
    pub description: String,
    #[serde(default)]
    pub reliability: PatternReliability,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexError {
    pub path: PathBuf,
    pub reason: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum PatternKind {
    LongFunction,
    DeepNesting,
    ManyParameters,
    GodModule,
    DuplicatePattern,
    MissingErrorHandling,
    PotentialResourceLeak,
    UnusedImport,
    TodoMarker,
}

impl PatternKind {
    pub fn severity(&self) -> PatternSeverity {
        match self {
            Self::LongFunction => PatternSeverity::Medium,
            Self::DeepNesting => PatternSeverity::High,
            Self::ManyParameters => PatternSeverity::Low,
            Self::GodModule => PatternSeverity::High,
            Self::DuplicatePattern => PatternSeverity::Medium,
            Self::MissingErrorHandling => PatternSeverity::High,
            Self::PotentialResourceLeak => PatternSeverity::High,
            Self::UnusedImport => PatternSeverity::Low,
            Self::TodoMarker => PatternSeverity::Info,
        }
    }

    pub fn reliability(&self) -> PatternReliability {
        match self {
            Self::MissingErrorHandling => PatternReliability::High,
            Self::PotentialResourceLeak => PatternReliability::Medium,
            Self::LongFunction => PatternReliability::Medium,
            Self::DeepNesting => PatternReliability::Medium,
            Self::ManyParameters => PatternReliability::Low,
            Self::GodModule => PatternReliability::Low,
            Self::DuplicatePattern => PatternReliability::Low,
            Self::UnusedImport => PatternReliability::Low,
            Self::TodoMarker => PatternReliability::Low,
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

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct FileSummary {
    pub purpose: String,
    pub exports: Vec<String>,
    pub used_by: Vec<PathBuf>,
    pub depends_on: Vec<PathBuf>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileIndex {
    pub path: PathBuf,
    pub language: Language,
    pub loc: usize,
    pub content_hash: String,
    pub symbols: Vec<Symbol>,
    pub dependencies: Vec<Dependency>,
    pub patterns: Vec<Pattern>,
    pub complexity: f64,
    pub last_modified: DateTime<Utc>,
    pub summary: FileSummary,
    pub layer: Option<String>,
    pub feature: Option<String>,
}

impl FileIndex {
    pub fn suggestion_density(&self) -> f64 {
        if self.loc == 0 {
            return 0.0;
        }
        self.patterns.len() as f64 / (self.loc as f64 / 100.0)
    }

    pub fn priority_indicator(&self) -> char {
        let density = self.suggestion_density();
        if density > 5.0 {
            '\u{25CF}'
        } else if density > 2.0 {
            '\u{25D0}'
        } else if density > 0.0 {
            '\u{25CB}'
        } else {
            ' '
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CodebaseIndex {
    pub root: PathBuf,
    pub files: HashMap<PathBuf, FileIndex>,
    #[serde(default)]
    pub index_errors: Vec<IndexError>,
    #[serde(default)]
    pub git_head: Option<String>,
}

impl CodebaseIndex {
    pub fn new(root: &Path) -> anyhow::Result<Self> {
        Ok(Self {
            root: root.to_path_buf(),
            files: HashMap::new(),
            index_errors: Vec::new(),
            git_head: git_head(root),
        })
    }

    pub fn stats(&self) -> IndexStats {
        IndexStats {
            file_count: self.files.len(),
            total_loc: self.files.values().map(|f| f.loc).sum(),
            symbol_count: self.files.values().map(|f| f.symbols.len()).sum(),
            skipped_files: self.index_errors.len(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct IndexStats {
    pub file_count: usize,
    pub total_loc: usize,
    pub symbol_count: usize,
    pub skipped_files: usize,
}

#[derive(Debug, Clone)]
pub struct FlatTreeEntry {
    pub name: String,
    pub path: PathBuf,
    pub is_dir: bool,
    pub depth: usize,
    pub priority: char,
}

pub fn has_uncommitted_changes(root: &Path) -> bool {
    use std::process::Command;

    let output = match Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(root)
        .output()
    {
        Ok(o) => o,
        Err(_) => return true,
    };

    if output.status.success() {
        !output.stdout.is_empty()
    } else {
        true
    }
}

fn git_head(root: &Path) -> Option<String> {
    use std::process::Command;

    let output = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(root)
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let head = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if head.is_empty() {
        None
    } else {
        Some(head)
    }
}
