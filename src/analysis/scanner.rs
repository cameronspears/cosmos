use anyhow::Result;
use regex::Regex;
use std::fs;
use std::path::Path;
use walkdir::WalkDir;

/// Represents a TODO, HACK, or FIXME found in the codebase
#[derive(Debug, Clone)]
pub struct TodoEntry {
    pub path: String,
    pub line_number: usize,
    pub kind: TodoKind,
    pub text: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TodoKind {
    Todo,
    Hack,
    Fixme,
    Xxx,
}

impl TodoKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            TodoKind::Todo => "TODO",
            TodoKind::Hack => "HACK",
            TodoKind::Fixme => "FIXME",
            TodoKind::Xxx => "XXX",
        }
    }
}

impl std::fmt::Display for TodoKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

/// Scans files for TODO, HACK, FIXME, and XXX comments
pub struct TodoScanner {
    pattern: Regex,
    ignore_dirs: Vec<String>,
}

impl TodoScanner {
    pub fn new() -> Self {
        // Match TODO/HACK/FIXME/XXX in comment contexts (after // or # or within /* */)
        // Also matches standalone markers followed by colon or text
        let pattern = Regex::new(r"(?:\/\/|#|/\*|\*)\s*(?i)(TODO|HACK|FIXME|XXX)\b[:\s]*(.*)").unwrap();
        let ignore_dirs = vec![
            ".git".to_string(),
            "node_modules".to_string(),
            "target".to_string(),
            "vendor".to_string(),
            "dist".to_string(),
            "build".to_string(),
            ".next".to_string(),
            "__pycache__".to_string(),
            ".venv".to_string(),
            "venv".to_string(),
        ];
        Self { pattern, ignore_dirs }
    }

    /// Scan a directory for TODO/HACK/FIXME comments
    pub fn scan(&self, root: &Path) -> Result<Vec<TodoEntry>> {
        let mut entries = Vec::new();

        for entry in WalkDir::new(root)
            .follow_links(false)
            .into_iter()
            .filter_entry(|e| !self.should_ignore(e))
        {
            let entry = match entry {
                Ok(e) => e,
                Err(_) => continue,
            };

            if !entry.file_type().is_file() {
                continue;
            }

            // Skip binary and non-text files
            let path = entry.path();
            if !self.is_likely_text_file(path) {
                continue;
            }

            if let Ok(content) = fs::read_to_string(path) {
                let relative_path = path
                    .strip_prefix(root)
                    .unwrap_or(path)
                    .to_string_lossy()
                    .to_string();

                for (line_num, line) in content.lines().enumerate() {
                    if let Some(captures) = self.pattern.captures(line) {
                        let kind_str = captures.get(1).map(|m| m.as_str()).unwrap_or("");
                        let text = captures
                            .get(2)
                            .map(|m| m.as_str().trim())
                            .unwrap_or("")
                            .to_string();

                        let kind = match kind_str.to_uppercase().as_str() {
                            "TODO" => TodoKind::Todo,
                            "HACK" => TodoKind::Hack,
                            "FIXME" => TodoKind::Fixme,
                            "XXX" => TodoKind::Xxx,
                            _ => continue,
                        };

                        entries.push(TodoEntry {
                            path: relative_path.clone(),
                            line_number: line_num + 1,
                            kind,
                            text,
                        });
                    }
                }
            }
        }

        // Sort by kind priority (FIXME > HACK > TODO > XXX), then by path
        entries.sort_by(|a, b| {
            let kind_priority = |k: &TodoKind| match k {
                TodoKind::Fixme => 0,
                TodoKind::Hack => 1,
                TodoKind::Todo => 2,
                TodoKind::Xxx => 3,
            };
            kind_priority(&a.kind)
                .cmp(&kind_priority(&b.kind))
                .then_with(|| a.path.cmp(&b.path))
        });

        Ok(entries)
    }

    fn should_ignore(&self, entry: &walkdir::DirEntry) -> bool {
        entry
            .file_name()
            .to_str()
            .map(|name| {
                self.ignore_dirs.contains(&name.to_string())
                    || name.starts_with('.')
            })
            .unwrap_or(false)
    }

    fn is_likely_text_file(&self, path: &Path) -> bool {
        let text_extensions = [
            "rs", "js", "ts", "tsx", "jsx", "py", "rb", "go", "java", "kt", "scala",
            "c", "cpp", "h", "hpp", "cs", "swift", "m", "mm", "php", "pl", "pm",
            "sh", "bash", "zsh", "fish", "ps1", "bat", "cmd",
            "html", "htm", "css", "scss", "sass", "less",
            "json", "yaml", "yml", "toml", "xml", "ini", "cfg", "conf",
            "md", "markdown", "txt", "rst", "adoc",
            "sql", "graphql", "gql",
            "vue", "svelte", "astro",
            "dockerfile", "makefile", "cmake",
            "ex", "exs", "erl", "hrl",
            "hs", "lhs", "elm", "clj", "cljs", "cljc",
            "lua", "r", "jl", "nim", "zig", "v", "d",
        ];

        // Check by extension
        if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
            return text_extensions.contains(&ext.to_lowercase().as_str());
        }

        // Check common extensionless files
        if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
            let name_lower = name.to_lowercase();
            return matches!(
                name_lower.as_str(),
                "makefile" | "dockerfile" | "gemfile" | "rakefile" | "procfile" | "brewfile"
            );
        }

        false
    }
}

impl Default for TodoScanner {
    fn default() -> Self {
        Self::new()
    }
}

