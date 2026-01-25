//! Heuristic-based file categorization
//!
//! Fast, pattern-based detection of architectural layers based on:
//! - File naming patterns (highest specificity)
//! - Directory structure (path segment matching)
//! - Import analysis
//! - Symbol-based hints

use super::{CodebaseGrouping, Layer};
use crate::index::{CodebaseIndex, Dependency, FileIndex, SymbolKind};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::{Component, Path};

/// Confidence level for layer detection
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Confidence {
    /// Very confident - explicit patterns (e.g., route.ts, .component.tsx)
    High,
    /// Reasonably confident - directory or import signals
    Medium,
    /// Low confidence - fallback/default
    Low,
}

/// Result of layer detection with confidence
#[derive(Debug, Clone, Copy)]
pub struct LayerDetection {
    pub layer: Layer,
    pub confidence: Confidence,
}

impl LayerDetection {
    pub fn high(layer: Layer) -> Self {
        Self {
            layer,
            confidence: Confidence::High,
        }
    }

    pub fn medium(layer: Layer) -> Self {
        Self {
            layer,
            confidence: Confidence::Medium,
        }
    }

    pub fn low(layer: Layer) -> Self {
        Self {
            layer,
            confidence: Confidence::Low,
        }
    }
}

impl Confidence {
    pub fn from_score(score: f64) -> Self {
        if score >= 0.8 {
            Confidence::High
        } else if score >= 0.6 {
            Confidence::Medium
        } else {
            Confidence::Low
        }
    }
}

/// Categorize all files in a codebase using heuristics
pub fn categorize_codebase(index: &CodebaseIndex) -> CodebaseGrouping {
    let mut grouping = CodebaseGrouping::new();

    for (path, file_index) in &index.files {
        let detection = detect_layer_with_confidence(path, file_index);
        grouping.assign_file_with_confidence(path.clone(), detection.layer, detection.confidence);
    }

    grouping
}

/// Detect the architectural layer for a single file with confidence score
pub fn detect_layer_with_confidence(path: &Path, file_index: &FileIndex) -> LayerDetection {
    // Priority order (reordered for better accuracy):
    // 1. Tests > Config > Infra (these are unambiguous)
    // 2. File patterns (most specific - route.ts, .component.tsx)
    // 3. Directory structure (path segment matching)
    // 4. Symbol-based hints (exports, function patterns)
    // 5. Import-based detection
    // 6. Language conventions (fallback)

    // 1. Test files (highest priority - tests can be anywhere)
    if is_test_file(path) {
        return LayerDetection::high(Layer::Tests);
    }

    // 2. Config files
    if is_config_file(path) {
        return LayerDetection::high(Layer::Config);
    }

    // 3. Infrastructure files
    if is_infra_file(path) {
        return LayerDetection::high(Layer::Infra);
    }

    // 4. File pattern detection (before directory - more specific)
    if let Some(layer) = detect_by_file_pattern(path) {
        return LayerDetection::high(layer);
    }

    // 5. Symbol-based hints
    if let Some(layer) = detect_by_symbols(file_index) {
        return LayerDetection::medium(layer);
    }

    // 6. Directory-based detection (using segment matching)
    if let Some(layer) = detect_by_directory_segments(path) {
        return LayerDetection::medium(layer);
    }

    // 7. Import-based detection
    if let Some(layer) = detect_by_imports(&file_index.dependencies) {
        return LayerDetection::medium(layer);
    }

    // 8. Default based on language patterns
    LayerDetection::low(detect_by_language_conventions(path, file_index))
}

/// Check if a path has a specific segment (exact component match)
fn has_path_segment(path: &Path, segment: &str) -> bool {
    path.components().any(|c| {
        if let Component::Normal(s) = c {
            s.to_str()
                .map(|s| s.eq_ignore_ascii_case(segment))
                .unwrap_or(false)
        } else {
            false
        }
    })
}

/// Check if a path has any of the given segments
fn has_any_path_segment(path: &Path, segments: &[&str]) -> bool {
    segments.iter().any(|s| has_path_segment(path, s))
}

/// Get all path segments as lowercase strings
fn path_segments(path: &Path) -> Vec<String> {
    path.components()
        .filter_map(|c| {
            if let Component::Normal(s) = c {
                s.to_str().map(|s| s.to_lowercase())
            } else {
                None
            }
        })
        .collect()
}

/// Check if file is a test file
fn is_test_file(path: &Path) -> bool {
    let filename = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("")
        .to_lowercase();

    // Test directory patterns (using segment matching)
    if has_any_path_segment(
        path,
        &["test", "tests", "__tests__", "spec", "specs", "__test__"],
    ) {
        return true;
    }

    // Test file patterns
    let test_patterns = [
        ".test.", ".spec.", "_test.", "_spec.", ".test", ".spec", // End patterns
        "test_", "spec_", // Start patterns
    ];
    if test_patterns.iter().any(|p| filename.contains(p)) {
        return true;
    }

    // Exact test file names
    let test_names = ["conftest.py", "jest.config", "vitest.config", "pytest.ini"];
    test_names.iter().any(|n| filename.starts_with(n))
}

/// Check if file is a config file
fn is_config_file(path: &Path) -> bool {
    let filename = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("")
        .to_lowercase();

    let config_patterns = [
        "config.",
        "configuration.",
        ".config.",
        ".conf",
        "settings.",
        ".env",
        "environment.",
        "tsconfig",
        "jsconfig",
        "webpack.config",
        "vite.config",
        "next.config",
        "nuxt.config",
        "svelte.config",
        "tailwind.config",
        "postcss.config",
        "babel.config",
        "eslint",
        "prettier",
        ".eslintrc",
        ".prettierrc",
        "cargo.toml",
        "package.json",
        "pyproject.toml",
        "go.mod",
        "makefile",
        "rakefile",
        "gemfile",
    ];

    config_patterns.iter().any(|p| filename.contains(p))
}

/// Check if file is infrastructure-related
fn is_infra_file(path: &Path) -> bool {
    let filename = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("")
        .to_lowercase();

    // Infra directories (using segment matching)
    let infra_dirs = [
        ".github",
        ".gitlab",
        ".circleci",
        "ci",
        "cd",
        "docker",
        "k8s",
        "kubernetes",
        "terraform",
        "ansible",
        "scripts",
        "bin",
        "deploy",
        "infra",
    ];
    if has_any_path_segment(path, &infra_dirs) {
        return true;
    }

    // Infra file patterns
    let infra_patterns = [
        "dockerfile",
        "docker-compose",
        ".dockerignore",
        "jenkinsfile",
        ".travis",
        "cloudbuild",
    ];
    if infra_patterns.iter().any(|p| filename.contains(p)) {
        return true;
    }

    // Only match yaml/yml if in typical infra contexts
    if filename.ends_with(".yaml") || filename.ends_with(".yml") {
        let infra_yaml_names = ["deployment", "service", "ingress", "configmap", "secret"];
        return infra_yaml_names.iter().any(|n| filename.contains(n));
    }

    false
}

/// Detect layer based on file naming patterns (most specific)
fn detect_by_file_pattern(path: &Path) -> Option<Layer> {
    let original_filename = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
    let filename = original_filename.to_lowercase();

    // === API patterns (check first - most specific) ===
    // Framework-specific route files
    let api_file_patterns = [
        "route.ts",
        "route.js",
        "route.tsx",
        "route.jsx",
        "+server.ts",
        "+server.js",
        "+page.server.ts",
        "+page.server.js",
    ];
    if api_file_patterns.iter().any(|p| filename == *p) {
        return Some(Layer::Api);
    }

    // API naming patterns
    let api_patterns = [".api.", "api.", "endpoint.", "handler."];
    if api_patterns.iter().any(|p| filename.contains(p)) {
        return Some(Layer::Api);
    }

    // === Backend patterns ===
    let backend_patterns = [
        "service.",
        ".service.",
        "controller.",
        ".controller.",
        "middleware.",
        ".middleware.",
        "resolver.",
        ".resolver.",
        "worker.",
        ".worker.",
        "job.",
        ".job.",
        "queue.",
        ".queue.",
    ];
    if backend_patterns.iter().any(|p| filename.contains(p)) {
        return Some(Layer::Backend);
    }

    // === Database patterns ===
    let db_patterns = [
        "model.",
        ".model.",
        "entity.",
        ".entity.",
        "schema.",
        ".schema.",
        "migration.",
        ".migration.",
        "repository.",
        ".repository.",
        "dao.",
        ".dao.",
        "seed.",
        ".seed.",
        "query.",
        ".query.",
    ];
    if db_patterns.iter().any(|p| filename.contains(p)) {
        return Some(Layer::Database);
    }

    // === Shared patterns ===
    let shared_patterns = [
        "types.",
        ".types.",
        "type.",
        ".type.",
        "util.",
        ".util.",
        "utils.",
        ".utils.",
        "helper.",
        ".helper.",
        "helpers.",
        ".helpers.",
        "constant.",
        ".constant.",
        "constants.",
        ".constants.",
        "interface.",
        ".interface.",
        "interfaces.",
        ".interfaces.",
    ];
    if shared_patterns.iter().any(|p| filename.contains(p)) {
        return Some(Layer::Shared);
    }

    // === Frontend patterns ===

    // React hooks pattern - "use" followed by uppercase letter
    if original_filename.len() > 3 {
        let chars: Vec<char> = original_filename.chars().collect();
        if chars[0] == 'u'
            && chars[1] == 's'
            && chars[2] == 'e'
            && chars[3].is_ascii_uppercase()
            && (filename.ends_with(".ts")
                || filename.ends_with(".tsx")
                || filename.ends_with(".js")
                || filename.ends_with(".jsx"))
        {
            return Some(Layer::Frontend);
        }
    }

    // Component/page/layout patterns
    let frontend_file_patterns = [
        "component.",
        ".component.",
        "page.",
        ".page.",
        "layout.",
        ".layout.",
        "hook.",
        ".hook.",
        "context.",
        ".context.",
        "provider.",
        ".provider.",
        "store.",
        ".store.",
    ];
    if frontend_file_patterns.iter().any(|p| filename.contains(p)) {
        return Some(Layer::Frontend);
    }

    // Style files
    let style_extensions = [".css", ".scss", ".sass", ".less", ".styled."];
    if style_extensions
        .iter()
        .any(|e| filename.ends_with(e) || filename.contains(e))
    {
        return Some(Layer::Frontend);
    }

    // Frontend-specific extensions
    let frontend_extensions = [".vue", ".svelte", ".astro"];
    if frontend_extensions.iter().any(|e| filename.ends_with(e)) {
        return Some(Layer::Frontend);
    }

    // TSX/JSX files that don't match API patterns - likely frontend
    if filename.ends_with(".tsx") || filename.ends_with(".jsx") {
        // Already checked API patterns above, so this is likely UI
        if !filename.contains("route") && !filename.contains("api") {
            return Some(Layer::Frontend);
        }
    }

    None
}

/// Detect layer based on symbol analysis
fn detect_by_symbols(file_index: &FileIndex) -> Option<Layer> {
    let symbols = &file_index.symbols;

    if symbols.is_empty() {
        return None;
    }

    // Count different symbol types
    let mut has_component = false;
    let mut has_handler = false;
    let mut has_model = false;
    let mut has_hook = false;

    for symbol in symbols {
        let name_lower = symbol.name.to_lowercase();

        // React components (PascalCase functions/classes returning JSX)
        if matches!(symbol.kind, SymbolKind::Function | SymbolKind::Class) {
            let first_char = symbol.name.chars().next().unwrap_or('a');
            if first_char.is_ascii_uppercase() {
                has_component = true;
            }
        }

        // Hooks
        if name_lower.starts_with("use") && name_lower.len() > 3 {
            let fourth_char = symbol.name.chars().nth(3).unwrap_or('a');
            if fourth_char.is_ascii_uppercase() {
                has_hook = true;
            }
        }

        // Handler patterns
        if name_lower.contains("handler")
            || name_lower.contains("controller")
            || name_lower.starts_with("handle")
            || name_lower.starts_with("on_")
        {
            has_handler = true;
        }

        // Model/Entity patterns
        if matches!(symbol.kind, SymbolKind::Struct | SymbolKind::Class)
            && (name_lower.contains("model")
                || name_lower.contains("entity")
                || name_lower.contains("schema")
                || name_lower.contains("record"))
        {
            has_model = true;
        }
    }

    // Priority: hooks/components → frontend, handlers → backend, models → database
    if has_hook {
        return Some(Layer::Frontend);
    }
    if has_component && !has_handler {
        return Some(Layer::Frontend);
    }
    if has_handler {
        return Some(Layer::Backend);
    }
    if has_model {
        return Some(Layer::Database);
    }

    None
}

/// Detect layer based on directory structure using path segment matching
fn detect_by_directory_segments(path: &Path) -> Option<Layer> {
    let segments = path_segments(path);

    // Check for API first (before frontend, since app/api should be API not Frontend)
    let api_segments = [
        "api",
        "routes",
        "endpoints",
        "rest",
        "graphql",
        "trpc",
        "rpc",
    ];
    if segments.iter().any(|s| api_segments.contains(&s.as_str())) {
        return Some(Layer::Api);
    }

    // Database directories
    let db_segments = [
        "models",
        "model",
        "entities",
        "entity",
        "schemas",
        "schema",
        "migrations",
        "db",
        "database",
        "repositories",
        "repository",
        "dao",
        "prisma",
        "drizzle",
    ];
    if segments.iter().any(|s| db_segments.contains(&s.as_str())) {
        return Some(Layer::Database);
    }

    // Backend directories
    let backend_segments = [
        "server",
        "backend",
        "services",
        "service",
        "handlers",
        "controllers",
        "middleware",
        "core",
        "domain",
        "business",
        "logic",
        "jobs",
        "workers",
        "queues",
    ];
    if segments
        .iter()
        .any(|s| backend_segments.contains(&s.as_str()))
    {
        return Some(Layer::Backend);
    }

    // Frontend directories (check after API/Backend to avoid app/api conflicts)
    let frontend_segments = [
        "components",
        "pages",
        "views",
        "layouts",
        "templates",
        "client",
        "frontend",
        "web",
        "ui",
        "styles",
        "css",
        "assets",
        "public",
        "hooks",
        "contexts",
        "providers",
        "stores",
    ];
    if segments
        .iter()
        .any(|s| frontend_segments.contains(&s.as_str()))
    {
        return Some(Layer::Frontend);
    }

    // Special handling for "app" directory (Next.js, etc.)
    // Only treat as frontend if no API/backend signals
    if segments.contains(&"app".to_string()) {
        return Some(Layer::Frontend);
    }

    // Shared directories
    let shared_segments = [
        "shared",
        "common",
        "lib",
        "libs",
        "utils",
        "util",
        "helpers",
        "types",
        "interfaces",
        "constants",
    ];
    if segments
        .iter()
        .any(|s| shared_segments.contains(&s.as_str()))
    {
        return Some(Layer::Shared);
    }

    None
}

/// Detect layer based on import analysis
fn detect_by_imports(dependencies: &[Dependency]) -> Option<Layer> {
    let imports: HashSet<&str> = dependencies
        .iter()
        .filter(|d| d.is_external)
        .map(|d| d.import_path.as_str())
        .collect();

    if imports.is_empty() {
        return None;
    }

    // Frontend framework imports
    let frontend_imports = [
        "react",
        "react-dom",
        "vue",
        "svelte",
        "solid-js",
        "preact",
        "@angular",
        "next",
        "nuxt",
        "astro",
        "@remix-run",
        "styled-components",
        "@emotion",
        "tailwindcss",
        "@tanstack/react-query",
        "swr",
        "zustand",
        "jotai",
        "recoil",
        "@radix-ui",
        "@headlessui",
        "framer-motion",
    ];

    let frontend_score: usize = imports
        .iter()
        .filter(|i| frontend_imports.iter().any(|f| i.starts_with(f)))
        .count();

    // Backend framework imports
    let backend_imports = [
        "express",
        "fastify",
        "koa",
        "hono",
        "elysia",
        "nestjs",
        "@nestjs",
        "trpc",
        "@trpc",
        "actix",
        "actix-web",
        "axum",
        "rocket",
        "warp",
        "tower",
        "flask",
        "django",
        "fastapi",
        "starlette",
        "aiohttp",
        "gin",
        "echo",
        "fiber",
        "chi",
        "bull",
        "agenda",
        "bee-queue", // Job queues
    ];

    let backend_score: usize = imports
        .iter()
        .filter(|i| backend_imports.iter().any(|b| i.starts_with(b)))
        .count();

    // Database imports
    let db_imports = [
        "prisma",
        "@prisma",
        "drizzle",
        "drizzle-orm",
        "typeorm",
        "sequelize",
        "knex",
        "mongoose",
        "mongodb",
        "pg",
        "mysql",
        "mysql2",
        "sqlite",
        "sqlite3",
        "sqlx",
        "diesel",
        "sea-orm",
        "tokio-postgres",
        "sqlalchemy",
        "peewee",
        "tortoise",
        "databases",
        "gorm",
        "ent",
        "bun",
        "redis",
        "ioredis", // Cache/data stores
    ];

    let db_score: usize = imports
        .iter()
        .filter(|i| db_imports.iter().any(|d| i.starts_with(d)))
        .count();

    // Return highest scoring layer (with tiebreaker priority)
    let max_score = frontend_score.max(backend_score).max(db_score);
    if max_score == 0 {
        return None;
    }

    // Priority order for ties: API/Backend > Database > Frontend
    if backend_score == max_score {
        Some(Layer::Backend)
    } else if db_score == max_score {
        Some(Layer::Database)
    } else {
        Some(Layer::Frontend)
    }
}

/// Detect layer based on language-specific conventions
fn detect_by_language_conventions(path: &Path, file_index: &FileIndex) -> Layer {
    use crate::index::Language;

    let filename = path.file_name().and_then(|n| n.to_str()).unwrap_or("");

    match file_index.language {
        Language::Rust => {
            // Rust conventions
            if filename == "main.rs" || filename == "lib.rs" {
                Layer::Backend
            } else if filename == "mod.rs" {
                // Look at parent directory
                let parent = path
                    .parent()
                    .and_then(|p| p.file_name())
                    .and_then(|n| n.to_str())
                    .unwrap_or("")
                    .to_lowercase();

                if ["api", "routes", "handlers"].contains(&parent.as_str()) {
                    Layer::Api
                } else if ["models", "db", "schema", "entities"].contains(&parent.as_str()) {
                    Layer::Database
                } else if ["utils", "helpers", "types"].contains(&parent.as_str()) {
                    Layer::Shared
                } else {
                    Layer::Backend
                }
            } else {
                Layer::Backend
            }
        }
        Language::TypeScript | Language::JavaScript => {
            if filename.ends_with(".tsx") || filename.ends_with(".jsx") {
                Layer::Frontend
            } else if filename.ends_with(".ts") || filename.ends_with(".js") {
                Layer::Shared
            } else {
                Layer::Unknown
            }
        }
        Language::Python => {
            if filename == "__init__.py" {
                Layer::Shared
            } else if filename.starts_with("test_") || filename.ends_with("_test.py") {
                Layer::Tests
            } else {
                Layer::Backend
            }
        }
        Language::Go => {
            if filename == "main.go" {
                Layer::Backend
            } else if filename.ends_with("_test.go") {
                Layer::Tests
            } else {
                Layer::Backend
            }
        }
        Language::Unknown => Layer::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_has_path_segment() {
        assert!(has_path_segment(Path::new("src/api/users.ts"), "api"));
        assert!(has_path_segment(Path::new("src/API/users.ts"), "api")); // case insensitive
        assert!(!has_path_segment(Path::new("src/myapi/users.ts"), "api")); // substring shouldn't match
        assert!(!has_path_segment(Path::new("src/api-v2/users.ts"), "api")); // partial match
    }

    #[test]
    fn test_is_test_file() {
        assert!(is_test_file(Path::new("src/__tests__/foo.test.ts")));
        assert!(is_test_file(Path::new("tests/unit/bar.rs")));
        assert!(is_test_file(Path::new("foo.spec.tsx")));
        assert!(is_test_file(Path::new("test_utils.py")));
        assert!(!is_test_file(Path::new("src/components/Button.tsx")));
    }

    #[test]
    fn test_is_config_file() {
        assert!(is_config_file(Path::new("tsconfig.json")));
        assert!(is_config_file(Path::new("vite.config.ts")));
        assert!(is_config_file(Path::new(".eslintrc.js")));
        assert!(is_config_file(Path::new("Cargo.toml")));
        assert!(!is_config_file(Path::new("src/main.rs")));
    }

    #[test]
    fn test_detect_by_directory_segments() {
        // API should be detected before frontend for app/api paths
        assert_eq!(
            detect_by_directory_segments(Path::new("app/api/users/route.ts")),
            Some(Layer::Api)
        );
        assert_eq!(
            detect_by_directory_segments(Path::new("src/components/Button.tsx")),
            Some(Layer::Frontend)
        );
        assert_eq!(
            detect_by_directory_segments(Path::new("server/handlers/auth.ts")),
            Some(Layer::Backend)
        );
        assert_eq!(
            detect_by_directory_segments(Path::new("prisma/schema.prisma")),
            Some(Layer::Database)
        );
    }

    #[test]
    fn test_detect_by_file_pattern() {
        assert_eq!(
            detect_by_file_pattern(Path::new("useAuth.ts")),
            Some(Layer::Frontend)
        );
        assert_eq!(
            detect_by_file_pattern(Path::new("route.ts")),
            Some(Layer::Api)
        );
        assert_eq!(
            detect_by_file_pattern(Path::new("user.service.ts")),
            Some(Layer::Backend)
        );
        assert_eq!(
            detect_by_file_pattern(Path::new("user.model.ts")),
            Some(Layer::Database)
        );
    }

    #[test]
    fn test_api_takes_precedence() {
        // route.ts should be API even if in components directory
        assert_eq!(
            detect_by_file_pattern(Path::new("src/components/api/route.ts")),
            Some(Layer::Api)
        );
    }
}
