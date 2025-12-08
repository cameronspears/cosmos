//! Heuristic-based file categorization
//!
//! Fast, pattern-based detection of architectural layers based on:
//! - Directory structure
//! - File naming patterns
//! - Import analysis

#![allow(dead_code)]

use super::{CodebaseGrouping, Layer};
use crate::index::{CodebaseIndex, Dependency, FileIndex};
use std::collections::HashSet;
use std::path::Path;

/// Categorize all files in a codebase using heuristics
pub fn categorize_codebase(index: &CodebaseIndex) -> CodebaseGrouping {
    let mut grouping = CodebaseGrouping::new();

    for (path, file_index) in &index.files {
        let layer = detect_layer(path, file_index);
        grouping.assign_file(path.clone(), layer);
    }

    grouping
}

/// Detect the architectural layer for a single file
pub fn detect_layer(path: &Path, file_index: &FileIndex) -> Layer {
    // Priority order: Tests > Config > specific patterns > directory > imports > default

    // 1. Test files (highest priority - tests can be anywhere)
    if is_test_file(path) {
        return Layer::Tests;
    }

    // 2. Config files
    if is_config_file(path) {
        return Layer::Config;
    }

    // 3. Infrastructure files
    if is_infra_file(path) {
        return Layer::Infra;
    }

    // 4. Directory-based detection
    if let Some(layer) = detect_by_directory(path) {
        return layer;
    }

    // 5. File pattern detection
    if let Some(layer) = detect_by_file_pattern(path) {
        return layer;
    }

    // 6. Import-based detection
    if let Some(layer) = detect_by_imports(&file_index.dependencies) {
        return layer;
    }

    // 7. Default based on language patterns
    detect_by_language_conventions(path, file_index)
}

/// Check if file is a test file
fn is_test_file(path: &Path) -> bool {
    let path_str = path.to_string_lossy().to_lowercase();
    let filename = path.file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("")
        .to_lowercase();

    // Test directory patterns
    let test_dirs = ["test/", "tests/", "__tests__/", "spec/", "specs/", "__test__/"];
    if test_dirs.iter().any(|d| path_str.contains(d)) {
        return true;
    }

    // Test file patterns
    let test_patterns = [
        ".test.", ".spec.", "_test.", "_spec.",
        ".test", ".spec",  // End patterns
        "test_", "spec_",  // Start patterns
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
    let filename = path.file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("")
        .to_lowercase();

    let config_patterns = [
        "config.", "configuration.", ".config.", ".conf",
        "settings.", ".env", "environment.",
        "tsconfig", "jsconfig", "webpack.config", "vite.config",
        "next.config", "nuxt.config", "svelte.config",
        "tailwind.config", "postcss.config", "babel.config",
        "eslint", "prettier", ".eslintrc", ".prettierrc",
        "cargo.toml", "package.json", "pyproject.toml", "go.mod",
        "makefile", "rakefile", "gemfile",
    ];

    config_patterns.iter().any(|p| filename.contains(p))
}

/// Check if file is infrastructure-related
fn is_infra_file(path: &Path) -> bool {
    let path_str = path.to_string_lossy().to_lowercase();
    let filename = path.file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("")
        .to_lowercase();

    // Infra directories
    let infra_dirs = [
        ".github/", ".gitlab/", ".circleci/", "ci/", "cd/",
        "docker/", "k8s/", "kubernetes/", "terraform/",
        "ansible/", "scripts/", "bin/", "deploy/", "infra/",
    ];
    if infra_dirs.iter().any(|d| path_str.contains(d)) {
        return true;
    }

    // Infra file patterns
    let infra_patterns = [
        "dockerfile", "docker-compose", ".dockerignore",
        "jenkinsfile", ".travis", "cloudbuild",
        "deployment.", "service.", ".yaml", ".yml",
    ];

    // Only match yaml/yml if in typical infra contexts
    if filename.ends_with(".yaml") || filename.ends_with(".yml") {
        // Check if it's likely infra yaml
        let infra_yaml_names = ["deployment", "service", "ingress", "configmap", "secret"];
        return infra_yaml_names.iter().any(|n| filename.contains(n));
    }

    infra_patterns.iter().any(|p| filename.contains(p))
}

/// Detect layer based on directory structure
fn detect_by_directory(path: &Path) -> Option<Layer> {
    let path_str = path.to_string_lossy().to_lowercase();

    // Frontend directories
    let frontend_dirs = [
        "components/", "pages/", "views/", "layouts/", "templates/",
        "app/", "src/app/", "client/", "frontend/", "web/",
        "ui/", "styles/", "css/", "assets/", "public/",
        "hooks/", "contexts/", "providers/", "stores/",
    ];
    if frontend_dirs.iter().any(|d| path_str.contains(d)) {
        // But check if it's actually an API route
        if path_str.contains("/api/") || path_str.contains("route.") {
            return Some(Layer::API);
        }
        return Some(Layer::Frontend);
    }

    // Backend directories
    let backend_dirs = [
        "server/", "backend/", "services/", "service/",
        "handlers/", "controllers/", "middleware/",
        "core/", "domain/", "business/", "logic/",
    ];
    if backend_dirs.iter().any(|d| path_str.contains(d)) {
        return Some(Layer::Backend);
    }

    // API directories
    let api_dirs = [
        "api/", "routes/", "endpoints/", "rest/", "graphql/",
        "trpc/", "rpc/",
    ];
    if api_dirs.iter().any(|d| path_str.contains(d)) {
        return Some(Layer::API);
    }

    // Database directories
    let db_dirs = [
        "models/", "model/", "entities/", "entity/",
        "schemas/", "schema/", "migrations/", "db/", "database/",
        "repositories/", "repository/", "dao/", "prisma/", "drizzle/",
    ];
    if db_dirs.iter().any(|d| path_str.contains(d)) {
        return Some(Layer::Database);
    }

    // Shared directories
    let shared_dirs = [
        "shared/", "common/", "lib/", "utils/", "util/",
        "helpers/", "types/", "interfaces/", "constants/",
    ];
    if shared_dirs.iter().any(|d| path_str.contains(d)) {
        return Some(Layer::Shared);
    }

    None
}

/// Detect layer based on file naming patterns
fn detect_by_file_pattern(path: &Path) -> Option<Layer> {
    let filename = path.file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("")
        .to_lowercase();

    // Frontend patterns
    let frontend_patterns = [
        ".tsx", ".jsx", ".vue", ".svelte", ".astro",
        ".css", ".scss", ".sass", ".less", ".styled.",
        "component.", ".component.", "hook.", "use",
        "page.", ".page.", "layout.", ".layout.",
    ];
    
    // Check TSX/JSX files that are likely frontend
    if filename.ends_with(".tsx") || filename.ends_with(".jsx") {
        // Exclude route files
        if !filename.contains("route") && !filename.contains("api") {
            return Some(Layer::Frontend);
        }
    }
    
    if frontend_patterns.iter().any(|p| filename.contains(p)) {
        return Some(Layer::Frontend);
    }

    // API patterns (framework-specific route files)
    let api_patterns = [
        "route.ts", "route.js", "+server.", "+page.server.",
        ".api.", "api.", "endpoint.", "handler.",
    ];
    if api_patterns.iter().any(|p| filename.contains(p)) {
        return Some(Layer::API);
    }

    // Backend patterns
    let backend_patterns = [
        "service.", ".service.", "controller.", ".controller.",
        "middleware.", ".middleware.", "resolver.", ".resolver.",
    ];
    if backend_patterns.iter().any(|p| filename.contains(p)) {
        return Some(Layer::Backend);
    }

    // Database patterns
    let db_patterns = [
        "model.", ".model.", "entity.", ".entity.",
        "schema.", ".schema.", "migration.", ".migration.",
        "repository.", ".repository.", "dao.", ".dao.",
    ];
    if db_patterns.iter().any(|p| filename.contains(p)) {
        return Some(Layer::Database);
    }

    // Shared patterns
    let shared_patterns = [
        "types.", ".types.", "type.", ".type.",
        "util.", ".util.", "helper.", ".helper.",
        "constant.", ".constant.", "interface.", ".interface.",
    ];
    if shared_patterns.iter().any(|p| filename.contains(p)) {
        return Some(Layer::Shared);
    }

    None
}

/// Detect layer based on import analysis
fn detect_by_imports(dependencies: &[Dependency]) -> Option<Layer> {
    let imports: HashSet<&str> = dependencies.iter()
        .filter(|d| d.is_external)
        .map(|d| d.import_path.as_str())
        .collect();

    // Frontend framework imports
    let frontend_imports = [
        "react", "vue", "svelte", "solid-js", "preact",
        "@angular", "next", "nuxt", "astro", "@remix",
        "styled-components", "@emotion", "tailwindcss",
        "@tanstack/react-query", "swr", "zustand", "jotai", "recoil",
    ];
    
    let frontend_score: usize = imports.iter()
        .filter(|i| frontend_imports.iter().any(|f| i.starts_with(f)))
        .count();

    // Backend framework imports
    let backend_imports = [
        "express", "fastify", "koa", "hono", "elysia",
        "nestjs", "@nestjs", "trpc", "@trpc",
        "actix", "axum", "rocket", "warp", "tower",
        "flask", "django", "fastapi", "starlette",
        "gin", "echo", "fiber", "chi",
    ];
    
    let backend_score: usize = imports.iter()
        .filter(|i| backend_imports.iter().any(|b| i.starts_with(b)))
        .count();

    // Database imports
    let db_imports = [
        "prisma", "@prisma", "drizzle", "typeorm", "sequelize",
        "mongoose", "mongodb", "pg", "mysql", "sqlite",
        "sqlx", "diesel", "sea-orm",
        "sqlalchemy", "peewee", "tortoise",
        "gorm", "ent",
    ];
    
    let db_score: usize = imports.iter()
        .filter(|i| db_imports.iter().any(|d| i.starts_with(d)))
        .count();

    // Return highest scoring layer
    let max_score = frontend_score.max(backend_score).max(db_score);
    if max_score == 0 {
        return None;
    }

    if frontend_score == max_score {
        Some(Layer::Frontend)
    } else if backend_score == max_score {
        Some(Layer::Backend)
    } else {
        Some(Layer::Database)
    }
}

/// Detect layer based on language-specific conventions
fn detect_by_language_conventions(path: &Path, file_index: &FileIndex) -> Layer {
    use crate::index::Language;

    let filename = path.file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("");

    match file_index.language {
        Language::Rust => {
            // Rust conventions
            if filename == "main.rs" || filename == "lib.rs" {
                Layer::Backend // Default for Rust entry points
            } else if filename == "mod.rs" {
                // Look at parent directory
                let parent = path.parent()
                    .and_then(|p| p.file_name())
                    .and_then(|n| n.to_str())
                    .unwrap_or("");
                
                if ["api", "routes", "handlers"].contains(&parent) {
                    Layer::API
                } else if ["models", "db", "schema"].contains(&parent) {
                    Layer::Database
                } else {
                    Layer::Shared
                }
            } else {
                Layer::Backend // Default for Rust
            }
        }
        Language::TypeScript | Language::JavaScript => {
            // JS/TS - check file extension for hints
            if filename.ends_with(".tsx") || filename.ends_with(".jsx") {
                Layer::Frontend
            } else if filename.ends_with(".ts") || filename.ends_with(".js") {
                // Pure TS/JS files - could be anything, default to shared
                Layer::Shared
            } else {
                Layer::Unknown
            }
        }
        Language::Python => {
            // Python conventions
            if filename == "__init__.py" {
                Layer::Shared
            } else if filename.starts_with("test_") || filename.endswith("_test.py") {
                Layer::Tests
            } else {
                Layer::Backend // Default for Python
            }
        }
        Language::Go => {
            // Go conventions
            if filename == "main.go" {
                Layer::Backend
            } else if filename.ends_with("_test.go") {
                Layer::Tests
            } else {
                Layer::Backend // Default for Go
            }
        }
        Language::Unknown => Layer::Unknown,
    }
}

// Helper trait for Python filename check
trait StringExt {
    fn endswith(&self, suffix: &str) -> bool;
}

impl StringExt for &str {
    fn endswith(&self, suffix: &str) -> bool {
        self.ends_with(suffix)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

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
    fn test_detect_by_directory() {
        assert_eq!(
            detect_by_directory(Path::new("src/components/Button.tsx")),
            Some(Layer::Frontend)
        );
        assert_eq!(
            detect_by_directory(Path::new("server/handlers/auth.ts")),
            Some(Layer::Backend)
        );
        assert_eq!(
            detect_by_directory(Path::new("src/api/users/route.ts")),
            Some(Layer::API)
        );
        assert_eq!(
            detect_by_directory(Path::new("prisma/schema.prisma")),
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
            Some(Layer::API)
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
}

