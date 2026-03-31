use anyhow::{Context, Result};
use tree_sitter::{Language, Parser, Query, QueryCursor, StreamingIterator};

use super::{LanguageParser, ParseResult};
use crate::db::{ParsedImport, ParsedReference, ParsedSymbol};

pub struct RustParser {
    language: Language,
}

impl RustParser {
    pub fn new() -> Self {
        Self {
            language: tree_sitter_rust::LANGUAGE.into(),
        }
    }

    fn extract_visibility(source: &[u8], node: tree_sitter::Node) -> String {
        // visibility_modifier is a direct child of the definition node (function_item, struct_item, etc.)
        for i in 0..node.child_count() as u32 {
            if let Some(child) = node.child(i)
                && child.kind() == "visibility_modifier"
            {
                let text = std::str::from_utf8(&source[child.byte_range()]).unwrap_or("");
                if text.starts_with("pub") {
                    return "pub".to_string();
                }
            }
        }
        "private".to_string()
    }

    fn find_scope(source: &[u8], node: tree_sitter::Node) -> Option<String> {
        let mut current = node.parent();
        while let Some(parent) = current {
            match parent.kind() {
                "impl_item" => {
                    for i in 0..parent.child_count() as u32 {
                        if let Some(child) = parent.child(i)
                            && child.kind() == "type_identifier"
                        {
                            return Some(
                                std::str::from_utf8(&source[child.byte_range()])
                                    .unwrap_or("")
                                    .to_string(),
                            );
                        }
                    }
                }
                "trait_item" => {
                    if let Some(name_node) = parent.child_by_field_name("name") {
                        return Some(
                            std::str::from_utf8(&source[name_node.byte_range()])
                                .unwrap_or("")
                                .to_string(),
                        );
                    }
                }
                "mod_item" => {
                    if let Some(name_node) = parent.child_by_field_name("name") {
                        return Some(
                            std::str::from_utf8(&source[name_node.byte_range()])
                                .unwrap_or("")
                                .to_string(),
                        );
                    }
                }
                _ => {}
            }
            current = parent.parent();
        }
        None
    }
}

fn text_from(source: &[u8], node: tree_sitter::Node) -> String {
    std::str::from_utf8(&source[node.byte_range()])
        .unwrap_or("")
        .to_string()
}

impl LanguageParser for RustParser {
    fn parse(&self, source: &str) -> Result<ParseResult> {
        let mut parser = Parser::new();
        parser
            .set_language(&self.language)
            .context("setting Rust language")?;

        let tree = parser.parse(source, None).context("parsing Rust source")?;
        let root = tree.root_node();
        let src = source.as_bytes();
        let mut result = ParseResult::default();

        // --- Symbol definitions ---
        let symbol_query = Query::new(
            &self.language,
            r#"
            (function_item name: (identifier) @fn_name) @fn_def
            (struct_item name: (type_identifier) @struct_name) @struct_def
            (enum_item name: (type_identifier) @enum_name) @enum_def
            (trait_item name: (type_identifier) @trait_name) @trait_def
            (type_item name: (type_identifier) @type_name) @type_def
            (const_item name: (identifier) @const_name) @const_def
            (static_item name: (identifier) @static_name) @static_def
            (mod_item name: (identifier) @mod_name) @mod_def
            "#,
        )
        .context("compiling Rust symbol query")?;

        let mut cursor = QueryCursor::new();
        let mut matches = cursor.matches(&symbol_query, root, src);

        while let Some(m) = matches.next() {
            let captures: Vec<_> = m
                .captures
                .iter()
                .map(|c| (symbol_query.capture_names()[c.index as usize], c.node))
                .collect();

            for &(name, node) in &captures {
                let (kind, sym_name) = match name {
                    "fn_name" => ("fn", text_from(src, node)),
                    "struct_name" => ("struct", text_from(src, node)),
                    "enum_name" => ("enum", text_from(src, node)),
                    "trait_name" => ("trait", text_from(src, node)),
                    "type_name" => ("type", text_from(src, node)),
                    "const_name" | "static_name" => ("const", text_from(src, node)),
                    "mod_name" => ("mod", text_from(src, node)),
                    _ => continue,
                };

                // Find the _def parent for visibility
                let def_node = captures
                    .iter()
                    .find(|(n, _)| n.ends_with("_def"))
                    .map(|(_, n)| *n)
                    .unwrap_or(node);

                let visibility = Self::extract_visibility(src, def_node);
                let scope = Self::find_scope(src, node);

                result.symbols.push(ParsedSymbol {
                    name: sym_name,
                    kind: kind.to_string(),
                    line: node.start_position().row as i64 + 1,
                    column: node.start_position().column as i64,
                    end_line: def_node.end_position().row as i64 + 1,
                    visibility,
                    scope,
                });
            }
        }

        // --- Use/import statements ---
        let import_query = Query::new(
            &self.language,
            r#"
            (use_declaration argument: (scoped_identifier) @use_path)
            (use_declaration argument: (scoped_use_list path: (scoped_identifier) @use_list_path))
            (use_declaration argument: (identifier) @use_simple)
            "#,
        )
        .context("compiling Rust import query")?;

        let mut cursor = QueryCursor::new();
        let mut matches = cursor.matches(&import_query, root, src);

        while let Some(m) = matches.next() {
            for c in m.captures {
                let cname = import_query.capture_names()[c.index as usize];
                if cname == "use_path" || cname == "use_list_path" || cname == "use_simple" {
                    let path = text_from(src, c.node);
                    result.imports.push(ParsedImport {
                        import_path: path,
                        alias: None,
                    });
                }
            }
        }

        // --- References (function calls) ---
        let ref_query = Query::new(
            &self.language,
            r#"
            (call_expression function: (identifier) @call_name)
            (call_expression function: (scoped_identifier name: (identifier) @scoped_call))
            (call_expression function: (field_expression field: (field_identifier) @method_call))
            "#,
        )
        .context("compiling Rust reference query")?;

        let mut cursor = QueryCursor::new();
        let mut matches = cursor.matches(&ref_query, root, src);

        while let Some(m) = matches.next() {
            for c in m.captures {
                let text = text_from(src, c.node);
                result.references.push(ParsedReference {
                    symbol_name: text,
                    line: c.node.start_position().row as i64 + 1,
                    column: c.node.start_position().column as i64,
                });
            }
        }

        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_functions() {
        let parser = RustParser::new();
        let source = r#"
pub fn hello() {
    println!("hello");
}

fn private_fn() -> i32 {
    42
}
"#;
        let result = parser.parse(source).unwrap();
        let fns: Vec<_> = result.symbols.iter().filter(|s| s.kind == "fn").collect();
        assert_eq!(fns.len(), 2);
        assert_eq!(fns[0].name, "hello");
        assert_eq!(fns[0].visibility, "pub");
        assert_eq!(fns[1].name, "private_fn");
        assert_eq!(fns[1].visibility, "private");
    }

    #[test]
    fn test_parse_structs_and_enums() {
        let parser = RustParser::new();
        let source = r#"
pub struct MyStruct {
    field: i32,
}

enum MyEnum {
    A,
    B(String),
}

pub trait MyTrait {
    fn do_thing(&self);
}
"#;
        let result = parser.parse(source).unwrap();
        let names: Vec<_> = result.symbols.iter().map(|s| &s.name).collect();
        assert!(names.contains(&&"MyStruct".to_string()));
        assert!(names.contains(&&"MyEnum".to_string()));
        assert!(names.contains(&&"MyTrait".to_string()));

        let my_struct = result
            .symbols
            .iter()
            .find(|s| s.name == "MyStruct")
            .unwrap();
        assert_eq!(my_struct.kind, "struct");
        assert_eq!(my_struct.visibility, "pub");

        let my_enum = result.symbols.iter().find(|s| s.name == "MyEnum").unwrap();
        assert_eq!(my_enum.kind, "enum");
        assert_eq!(my_enum.visibility, "private");
    }

    #[test]
    fn test_parse_use_statements() {
        let parser = RustParser::new();
        let source = r#"
use std::collections::HashMap;
use anyhow::Result;
"#;
        let result = parser.parse(source).unwrap();
        assert!(!result.imports.is_empty());
        let paths: Vec<_> = result.imports.iter().map(|i| &i.import_path).collect();
        assert!(
            paths
                .iter()
                .any(|p| p.contains("HashMap") || p.contains("collections"))
        );
    }

    #[test]
    fn test_parse_impl_methods() {
        let parser = RustParser::new();
        let source = r#"
pub struct Server {
    port: u16,
}

impl Server {
    pub fn new(port: u16) -> Self {
        Self { port }
    }

    fn start(&self) {
    }
}
"#;
        let result = parser.parse(source).unwrap();
        let fns: Vec<_> = result.symbols.iter().filter(|s| s.kind == "fn").collect();
        assert!(fns.len() >= 2);

        let new_fn = fns.iter().find(|s| s.name == "new").unwrap();
        assert_eq!(new_fn.visibility, "pub");
        assert_eq!(new_fn.scope, Some("Server".to_string()));
    }

    #[test]
    fn test_parse_consts_and_types() {
        let parser = RustParser::new();
        let source = r#"
pub const MAX_SIZE: usize = 100;
pub type Result<T> = std::result::Result<T, Error>;
"#;
        let result = parser.parse(source).unwrap();
        let names: Vec<_> = result.symbols.iter().map(|s| (&s.name, &s.kind)).collect();
        assert!(names.contains(&(&"MAX_SIZE".to_string(), &"const".to_string())));
        assert!(names.contains(&(&"Result".to_string(), &"type".to_string())));
    }
}
