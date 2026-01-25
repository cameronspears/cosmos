//! Tree-sitter based parser for multi-language AST analysis

use super::{Dependency, Language, Symbol, SymbolKind, Visibility};
use std::path::Path;
use tree_sitter::Parser;

/// Parse a file and extract symbols and dependencies
pub fn parse_file(
    path: &Path,
    content: &str,
    language: Language,
) -> anyhow::Result<(Vec<Symbol>, Vec<Dependency>)> {
    let mut parser = Parser::new();

    // Set the language
    let ts_language = match language {
        Language::Rust => tree_sitter_rust::LANGUAGE.into(),
        Language::JavaScript => tree_sitter_javascript::LANGUAGE.into(),
        Language::TypeScript => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
        Language::Python => tree_sitter_python::LANGUAGE.into(),
        Language::Go => tree_sitter_go::LANGUAGE.into(),
        Language::Unknown => return Ok((Vec::new(), Vec::new())),
    };

    parser.set_language(&ts_language)?;

    let tree = parser
        .parse(content, None)
        .ok_or_else(|| anyhow::anyhow!("Failed to parse file"))?;

    let root = tree.root_node();

    // Extract symbols and dependencies based on language
    let symbols = match language {
        Language::Rust => extract_rust_symbols(&root, content, path),
        Language::JavaScript | Language::TypeScript => extract_js_symbols(&root, content, path),
        Language::Python => extract_python_symbols(&root, content, path),
        Language::Go => extract_go_symbols(&root, content, path),
        Language::Unknown => Vec::new(),
    };

    let dependencies = match language {
        Language::Rust => extract_rust_deps(&root, content, path),
        Language::JavaScript | Language::TypeScript => extract_js_deps(&root, content, path),
        Language::Python => extract_python_deps(&root, content, path),
        Language::Go => extract_go_deps(&root, content, path),
        Language::Unknown => Vec::new(),
    };

    Ok((symbols, dependencies))
}

/// Extract symbols from Rust code
fn extract_rust_symbols(root: &tree_sitter::Node, content: &str, path: &Path) -> Vec<Symbol> {
    let mut symbols = Vec::new();
    let mut cursor = root.walk();

    extract_rust_symbols_recursive(&mut cursor, content, path, &mut symbols);
    symbols
}

fn extract_rust_symbols_recursive(
    cursor: &mut tree_sitter::TreeCursor,
    content: &str,
    path: &Path,
    symbols: &mut Vec<Symbol>,
) {
    loop {
        let node = cursor.node();
        let kind = node.kind();

        match kind {
            "function_item" | "function_signature_item" => {
                if let Some(name_node) = node.child_by_field_name("name") {
                    let name = get_node_text(&name_node, content);
                    let visibility = if has_pub_modifier(&node, content) {
                        Visibility::Public
                    } else {
                        Visibility::Private
                    };

                    symbols.push(Symbol {
                        name,
                        kind: SymbolKind::Function,
                        file: path.to_path_buf(),
                        line: node.start_position().row + 1,
                        end_line: node.end_position().row + 1,
                        complexity: estimate_complexity(&node, content),
                        visibility,
                    });
                }
            }
            "impl_item" => {
                // Extract methods from impl blocks
                if cursor.goto_first_child() {
                    extract_rust_symbols_recursive(cursor, content, path, symbols);
                    cursor.goto_parent();
                }
            }
            "struct_item" => {
                if let Some(name_node) = node.child_by_field_name("name") {
                    let name = get_node_text(&name_node, content);
                    symbols.push(Symbol {
                        name,
                        kind: SymbolKind::Struct,
                        file: path.to_path_buf(),
                        line: node.start_position().row + 1,
                        end_line: node.end_position().row + 1,
                        complexity: 1.0,
                        visibility: if has_pub_modifier(&node, content) {
                            Visibility::Public
                        } else {
                            Visibility::Private
                        },
                    });
                }
            }
            "enum_item" => {
                if let Some(name_node) = node.child_by_field_name("name") {
                    let name = get_node_text(&name_node, content);
                    symbols.push(Symbol {
                        name,
                        kind: SymbolKind::Enum,
                        file: path.to_path_buf(),
                        line: node.start_position().row + 1,
                        end_line: node.end_position().row + 1,
                        complexity: 1.0,
                        visibility: if has_pub_modifier(&node, content) {
                            Visibility::Public
                        } else {
                            Visibility::Private
                        },
                    });
                }
            }
            "trait_item" => {
                if let Some(name_node) = node.child_by_field_name("name") {
                    let name = get_node_text(&name_node, content);
                    symbols.push(Symbol {
                        name,
                        kind: SymbolKind::Trait,
                        file: path.to_path_buf(),
                        line: node.start_position().row + 1,
                        end_line: node.end_position().row + 1,
                        complexity: 1.0,
                        visibility: if has_pub_modifier(&node, content) {
                            Visibility::Public
                        } else {
                            Visibility::Private
                        },
                    });
                }
            }
            "mod_item" => {
                if let Some(name_node) = node.child_by_field_name("name") {
                    let name = get_node_text(&name_node, content);
                    symbols.push(Symbol {
                        name,
                        kind: SymbolKind::Module,
                        file: path.to_path_buf(),
                        line: node.start_position().row + 1,
                        end_line: node.end_position().row + 1,
                        complexity: 1.0,
                        visibility: if has_pub_modifier(&node, content) {
                            Visibility::Public
                        } else {
                            Visibility::Private
                        },
                    });
                }
            }
            "const_item" => {
                if let Some(name_node) = node.child_by_field_name("name") {
                    let name = get_node_text(&name_node, content);
                    symbols.push(Symbol {
                        name,
                        kind: SymbolKind::Constant,
                        file: path.to_path_buf(),
                        line: node.start_position().row + 1,
                        end_line: node.end_position().row + 1,
                        complexity: 1.0,
                        visibility: if has_pub_modifier(&node, content) {
                            Visibility::Public
                        } else {
                            Visibility::Private
                        },
                    });
                }
            }
            _ => {}
        }

        // Recurse into children
        if cursor.goto_first_child() {
            extract_rust_symbols_recursive(cursor, content, path, symbols);
            cursor.goto_parent();
        }

        if !cursor.goto_next_sibling() {
            break;
        }
    }
}

/// Extract dependencies from Rust code
fn extract_rust_deps(root: &tree_sitter::Node, content: &str, path: &Path) -> Vec<Dependency> {
    let mut deps = Vec::new();
    let mut cursor = root.walk();

    loop {
        let node = cursor.node();

        if node.kind() == "use_declaration" {
            let import_text = get_node_text(&node, content);
            let is_external = !import_text.contains("crate::")
                && !import_text.contains("super::")
                && !import_text.contains("self::");

            deps.push(Dependency {
                from_file: path.to_path_buf(),
                import_path: import_text
                    .replace("use ", "")
                    .replace(";", "")
                    .trim()
                    .to_string(),
                line: node.start_position().row + 1,
                is_external,
            });
        }

        if cursor.goto_first_child() {
            continue;
        }

        while !cursor.goto_next_sibling() {
            if !cursor.goto_parent() {
                return deps;
            }
        }
    }
}

/// Extract symbols from JavaScript/TypeScript code
fn extract_js_symbols(root: &tree_sitter::Node, content: &str, path: &Path) -> Vec<Symbol> {
    let mut symbols = Vec::new();
    let mut cursor = root.walk();

    loop {
        let node = cursor.node();
        let kind = node.kind();

        match kind {
            "function_declaration" | "function" => {
                if let Some(name_node) = node.child_by_field_name("name") {
                    let name = get_node_text(&name_node, content);
                    symbols.push(Symbol {
                        name,
                        kind: SymbolKind::Function,
                        file: path.to_path_buf(),
                        line: node.start_position().row + 1,
                        end_line: node.end_position().row + 1,
                        complexity: estimate_complexity(&node, content),
                        visibility: Visibility::Public,
                    });
                }
            }
            "arrow_function" => {
                // Arrow functions assigned to variables
                if let Some(parent) = node.parent() {
                    if parent.kind() == "variable_declarator" {
                        if let Some(name_node) = parent.child_by_field_name("name") {
                            let name = get_node_text(&name_node, content);
                            symbols.push(Symbol {
                                name,
                                kind: SymbolKind::Function,
                                file: path.to_path_buf(),
                                line: node.start_position().row + 1,
                                end_line: node.end_position().row + 1,
                                complexity: estimate_complexity(&node, content),
                                visibility: Visibility::Public,
                            });
                        }
                    }
                }
            }
            "class_declaration" => {
                if let Some(name_node) = node.child_by_field_name("name") {
                    let name = get_node_text(&name_node, content);
                    symbols.push(Symbol {
                        name,
                        kind: SymbolKind::Class,
                        file: path.to_path_buf(),
                        line: node.start_position().row + 1,
                        end_line: node.end_position().row + 1,
                        complexity: 1.0,
                        visibility: Visibility::Public,
                    });
                }
            }
            "method_definition" => {
                if let Some(name_node) = node.child_by_field_name("name") {
                    let name = get_node_text(&name_node, content);
                    symbols.push(Symbol {
                        name,
                        kind: SymbolKind::Method,
                        file: path.to_path_buf(),
                        line: node.start_position().row + 1,
                        end_line: node.end_position().row + 1,
                        complexity: estimate_complexity(&node, content),
                        visibility: Visibility::Public,
                    });
                }
            }
            "interface_declaration" => {
                if let Some(name_node) = node.child_by_field_name("name") {
                    let name = get_node_text(&name_node, content);
                    symbols.push(Symbol {
                        name,
                        kind: SymbolKind::Interface,
                        file: path.to_path_buf(),
                        line: node.start_position().row + 1,
                        end_line: node.end_position().row + 1,
                        complexity: 1.0,
                        visibility: Visibility::Public,
                    });
                }
            }
            _ => {}
        }

        if cursor.goto_first_child() {
            continue;
        }

        while !cursor.goto_next_sibling() {
            if !cursor.goto_parent() {
                return symbols;
            }
        }
    }
}

/// Extract dependencies from JavaScript/TypeScript code
fn extract_js_deps(root: &tree_sitter::Node, content: &str, path: &Path) -> Vec<Dependency> {
    let mut deps = Vec::new();
    let mut cursor = root.walk();

    loop {
        let node = cursor.node();

        if node.kind() == "import_statement" {
            if let Some(source) = node.child_by_field_name("source") {
                let import_path = get_node_text(&source, content)
                    .trim_matches(|c| c == '"' || c == '\'' || c == '`')
                    .to_string();
                let is_external = !import_path.starts_with('.') && !import_path.starts_with('/');

                deps.push(Dependency {
                    from_file: path.to_path_buf(),
                    import_path,
                    line: node.start_position().row + 1,
                    is_external,
                });
            }
        }

        if cursor.goto_first_child() {
            continue;
        }

        while !cursor.goto_next_sibling() {
            if !cursor.goto_parent() {
                return deps;
            }
        }
    }
}

/// Extract symbols from Python code
fn extract_python_symbols(root: &tree_sitter::Node, content: &str, path: &Path) -> Vec<Symbol> {
    let mut symbols = Vec::new();
    let mut cursor = root.walk();

    loop {
        let node = cursor.node();
        let kind = node.kind();

        match kind {
            "function_definition" => {
                if let Some(name_node) = node.child_by_field_name("name") {
                    let name = get_node_text(&name_node, content);
                    let visibility = if name.starts_with('_') {
                        if name.starts_with("__") && !name.ends_with("__") {
                            Visibility::Private
                        } else {
                            Visibility::Internal
                        }
                    } else {
                        Visibility::Public
                    };

                    symbols.push(Symbol {
                        name,
                        kind: SymbolKind::Function,
                        file: path.to_path_buf(),
                        line: node.start_position().row + 1,
                        end_line: node.end_position().row + 1,
                        complexity: estimate_complexity(&node, content),
                        visibility,
                    });
                }
            }
            "class_definition" => {
                if let Some(name_node) = node.child_by_field_name("name") {
                    let name = get_node_text(&name_node, content);
                    symbols.push(Symbol {
                        name,
                        kind: SymbolKind::Class,
                        file: path.to_path_buf(),
                        line: node.start_position().row + 1,
                        end_line: node.end_position().row + 1,
                        complexity: 1.0,
                        visibility: Visibility::Public,
                    });
                }
            }
            _ => {}
        }

        if cursor.goto_first_child() {
            continue;
        }

        while !cursor.goto_next_sibling() {
            if !cursor.goto_parent() {
                return symbols;
            }
        }
    }
}

/// Extract dependencies from Python code
fn extract_python_deps(root: &tree_sitter::Node, content: &str, path: &Path) -> Vec<Dependency> {
    let mut deps = Vec::new();
    let mut cursor = root.walk();

    loop {
        let node = cursor.node();

        if node.kind() == "import_statement" || node.kind() == "import_from_statement" {
            let import_text = get_node_text(&node, content);
            let is_external = !import_text.contains(" ."); // Relative imports use dots

            deps.push(Dependency {
                from_file: path.to_path_buf(),
                import_path: import_text.trim().to_string(),
                line: node.start_position().row + 1,
                is_external,
            });
        }

        if cursor.goto_first_child() {
            continue;
        }

        while !cursor.goto_next_sibling() {
            if !cursor.goto_parent() {
                return deps;
            }
        }
    }
}

/// Extract symbols from Go code
fn extract_go_symbols(root: &tree_sitter::Node, content: &str, path: &Path) -> Vec<Symbol> {
    let mut symbols = Vec::new();
    let mut cursor = root.walk();

    loop {
        let node = cursor.node();
        let kind = node.kind();

        match kind {
            "function_declaration" => {
                if let Some(name_node) = node.child_by_field_name("name") {
                    let name = get_node_text(&name_node, content);
                    let visibility = if name
                        .chars()
                        .next()
                        .map(|c| c.is_uppercase())
                        .unwrap_or(false)
                    {
                        Visibility::Public
                    } else {
                        Visibility::Private
                    };

                    symbols.push(Symbol {
                        name,
                        kind: SymbolKind::Function,
                        file: path.to_path_buf(),
                        line: node.start_position().row + 1,
                        end_line: node.end_position().row + 1,
                        complexity: estimate_complexity(&node, content),
                        visibility,
                    });
                }
            }
            "method_declaration" => {
                if let Some(name_node) = node.child_by_field_name("name") {
                    let name = get_node_text(&name_node, content);
                    let visibility = if name
                        .chars()
                        .next()
                        .map(|c| c.is_uppercase())
                        .unwrap_or(false)
                    {
                        Visibility::Public
                    } else {
                        Visibility::Private
                    };

                    symbols.push(Symbol {
                        name,
                        kind: SymbolKind::Method,
                        file: path.to_path_buf(),
                        line: node.start_position().row + 1,
                        end_line: node.end_position().row + 1,
                        complexity: estimate_complexity(&node, content),
                        visibility,
                    });
                }
            }
            "type_declaration" => {
                // Could be struct, interface, etc.
                if let Some(spec) = node.named_child(0) {
                    if let Some(name_node) = spec.child_by_field_name("name") {
                        let name = get_node_text(&name_node, content);
                        let sym_kind = if spec.kind() == "struct_type" {
                            SymbolKind::Struct
                        } else if spec.kind() == "interface_type" {
                            SymbolKind::Interface
                        } else {
                            SymbolKind::Struct
                        };

                        symbols.push(Symbol {
                            name,
                            kind: sym_kind,
                            file: path.to_path_buf(),
                            line: node.start_position().row + 1,
                            end_line: node.end_position().row + 1,
                            complexity: 1.0,
                            visibility: Visibility::Public,
                        });
                    }
                }
            }
            _ => {}
        }

        if cursor.goto_first_child() {
            continue;
        }

        while !cursor.goto_next_sibling() {
            if !cursor.goto_parent() {
                return symbols;
            }
        }
    }
}

/// Extract dependencies from Go code
fn extract_go_deps(root: &tree_sitter::Node, content: &str, path: &Path) -> Vec<Dependency> {
    let mut deps = Vec::new();
    let mut cursor = root.walk();

    loop {
        let node = cursor.node();

        if node.kind() == "import_declaration" {
            // Handle both single imports and import groups
            for i in 0..node.named_child_count() {
                if let Some(child) = node.named_child(i) {
                    if child.kind() == "import_spec" || child.kind() == "interpreted_string_literal"
                    {
                        let import_path =
                            get_node_text(&child, content).trim_matches('"').to_string();
                        let is_external =
                            !import_path.starts_with('.') && import_path.contains('/');

                        deps.push(Dependency {
                            from_file: path.to_path_buf(),
                            import_path,
                            line: child.start_position().row + 1,
                            is_external,
                        });
                    }
                }
            }
        }

        if cursor.goto_first_child() {
            continue;
        }

        while !cursor.goto_next_sibling() {
            if !cursor.goto_parent() {
                return deps;
            }
        }
    }
}

// Helper functions

fn get_node_text(node: &tree_sitter::Node, content: &str) -> String {
    let start = node.start_byte();
    let end = node.end_byte();
    content[start..end].to_string()
}

fn has_pub_modifier(node: &tree_sitter::Node, content: &str) -> bool {
    for i in 0..node.child_count() {
        if let Some(child) = node.child(i) {
            if child.kind() == "visibility_modifier" {
                let text = get_node_text(&child, content);
                return text.contains("pub");
            }
        }
    }
    false
}

fn estimate_complexity(node: &tree_sitter::Node, content: &str) -> f64 {
    let text = get_node_text(node, content);
    let mut complexity = 1.0;

    // Count decision points
    let keywords = [
        "if", "else", "for", "while", "match", "case", "&&", "||", "?",
    ];
    for kw in &keywords {
        complexity += text.matches(kw).count() as f64 * 0.5;
    }

    complexity
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_rust_parsing() {
        let content = r#"
            pub fn hello() {
                println!("Hello");
            }
            
            struct Foo {
                bar: i32,
            }
        "#;

        let (symbols, _) = parse_file(Path::new("test.rs"), content, Language::Rust).unwrap();

        assert!(!symbols.is_empty());
    }

    #[test]
    fn test_js_parsing() {
        let content = r#"
            function hello() {
                console.log("Hello");
            }
            
            class Foo {
                bar() {}
            }
        "#;

        let (symbols, _) = parse_file(Path::new("test.js"), content, Language::JavaScript).unwrap();

        assert!(!symbols.is_empty());
    }
}
