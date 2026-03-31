use anyhow::{Context, Result};
use tree_sitter::{Language, Parser, Query, QueryCursor, StreamingIterator};

use super::{LanguageParser, ParseResult};
use crate::db::{ParsedImport, ParsedReference, ParsedSymbol};

pub struct TypeScriptParser {
    language: Language,
    is_typescript: bool,
}

impl TypeScriptParser {
    pub fn new(lang: &str) -> Self {
        let (language, is_typescript) = match lang {
            "typescript" => (tree_sitter_typescript::LANGUAGE_TSX.into(), true),
            _ => (tree_sitter_javascript::LANGUAGE.into(), false),
        };
        Self {
            language,
            is_typescript,
        }
    }
}

fn text_from(source: &[u8], node: tree_sitter::Node) -> String {
    std::str::from_utf8(&source[node.byte_range()])
        .unwrap_or("")
        .to_string()
}

fn find_class_scope(source: &[u8], node: tree_sitter::Node) -> Option<String> {
    let mut current = node.parent();
    while let Some(parent) = current {
        if (parent.kind() == "class_declaration" || parent.kind() == "class")
            && let Some(name_node) = parent.child_by_field_name("name")
        {
            return Some(text_from(source, name_node));
        }
        current = parent.parent();
    }
    None
}

fn is_exported(node: tree_sitter::Node) -> bool {
    if let Some(parent) = node.parent() {
        parent.kind() == "export_statement"
    } else {
        false
    }
}

impl LanguageParser for TypeScriptParser {
    fn parse(&self, source: &str) -> Result<ParseResult> {
        let mut parser = Parser::new();
        parser
            .set_language(&self.language)
            .context("setting TypeScript/JavaScript language")?;

        let tree = parser
            .parse(source, None)
            .context("parsing TypeScript/JavaScript source")?;

        let root = tree.root_node();
        let src = source.as_bytes();
        let mut result = ParseResult::default();

        // --- Symbol definitions ---
        let query_str = if self.is_typescript {
            // TSX grammar
            String::from(
                r#"
                (function_declaration name: (identifier) @fn_name) @fn_def
                (class_declaration name: (type_identifier) @class_name) @class_def
                (method_definition name: (property_identifier) @method_name)
                (lexical_declaration (variable_declarator name: (identifier) @const_name)) @const_def
                (variable_declaration (variable_declarator name: (identifier) @var_name)) @var_def
                (interface_declaration name: (type_identifier) @iface_name) @iface_def
                (type_alias_declaration name: (type_identifier) @type_name) @type_def
                (enum_declaration name: (identifier) @enum_name) @enum_def
                "#,
            )
        } else {
            // JavaScript grammar
            String::from(
                r#"
                (function_declaration name: (identifier) @fn_name) @fn_def
                (class_declaration name: (identifier) @class_name) @class_def
                (method_definition name: (property_identifier) @method_name)
                (lexical_declaration (variable_declarator name: (identifier) @const_name)) @const_def
                (variable_declaration (variable_declarator name: (identifier) @var_name)) @var_def
                "#,
            )
        };

        let symbol_query =
            Query::new(&self.language, &query_str).context("compiling TS/JS symbol query")?;

        let mut cursor = QueryCursor::new();
        let mut matches = cursor.matches(&symbol_query, root, src);

        while let Some(m) = matches.next() {
            let captures: Vec<_> = m
                .captures
                .iter()
                .map(|c| (symbol_query.capture_names()[c.index as usize], c.node))
                .collect();

            for &(name, node) in &captures {
                let kind = match name {
                    "fn_name" => "fn",
                    "class_name" => "class",
                    "method_name" => "fn",
                    "const_name" | "var_name" => "const",
                    "iface_name" => "interface",
                    "type_name" => "type",
                    "enum_name" => "enum",
                    _ => continue,
                };

                let sym_text = text_from(src, node);

                // Find the _def parent for export check
                let def_node = captures
                    .iter()
                    .find(|(n, _)| n.ends_with("_def"))
                    .map(|(_, n)| *n);

                let exported = def_node.is_some_and(is_exported);
                let visibility = if exported { "pub" } else { "private" };

                let scope = if name == "method_name" {
                    find_class_scope(src, node)
                } else {
                    None
                };

                // Use def_node for end_line, falling back to parent node for methods
                let end_node = def_node.or_else(|| node.parent()).unwrap_or(node);
                result.symbols.push(ParsedSymbol {
                    name: sym_text,
                    kind: kind.to_string(),
                    line: node.start_position().row as i64 + 1,
                    column: node.start_position().column as i64,
                    end_line: end_node.end_position().row as i64 + 1,
                    visibility: visibility.to_string(),
                    scope,
                });
            }
        }

        // --- Imports ---
        let import_query = Query::new(
            &self.language,
            r#"
            (import_statement source: (string) @import_path)
            "#,
        )
        .context("compiling TS/JS import query")?;

        let mut cursor = QueryCursor::new();
        let mut matches = cursor.matches(&import_query, root, src);

        while let Some(m) = matches.next() {
            for c in m.captures {
                let text = text_from(src, c.node);
                let path = text
                    .trim_matches(|c: char| c == '\'' || c == '"')
                    .to_string();
                if !path.is_empty() {
                    result.imports.push(ParsedImport {
                        import_path: path,
                        alias: None,
                    });
                }
            }
        }

        // --- References ---
        let ref_query = Query::new(
            &self.language,
            r#"
            (call_expression function: (identifier) @call_name)
            (call_expression function: (member_expression property: (property_identifier) @method_call))
            (new_expression constructor: (identifier) @new_call)
            "#,
        )
        .context("compiling TS/JS reference query")?;

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
    fn test_parse_typescript() {
        let parser = TypeScriptParser::new("typescript");
        let source = r#"
export interface Config {
    host: string;
    port: number;
}

export type Result<T> = T | Error;

export function createServer(config: Config): Server {
    return new Server(config);
}

class Server {
    private port: number;

    constructor(config: Config) {
        this.port = config.port;
    }

    start(): void {
        console.log("starting");
    }
}

const DEFAULT_PORT = 3000;
export const MAX_CONNECTIONS = 100;

enum Status {
    Active,
    Inactive,
}
"#;
        let result = parser.parse(source).unwrap();

        let iface = result
            .symbols
            .iter()
            .find(|s| s.name == "Config" && s.kind == "interface");
        assert!(iface.is_some());

        let func = result
            .symbols
            .iter()
            .find(|s| s.name == "createServer" && s.kind == "fn");
        assert!(func.is_some());
        assert_eq!(func.unwrap().visibility, "pub");

        let class = result.symbols.iter().find(|s| s.name == "Server");
        assert!(class.is_some());

        let enum_sym = result
            .symbols
            .iter()
            .find(|s| s.name == "Status" && s.kind == "enum");
        assert!(enum_sym.is_some());
    }

    #[test]
    fn test_parse_javascript() {
        let parser = TypeScriptParser::new("javascript");
        let source = r#"
function add(a, b) {
    return a + b;
}

class Calculator {
    multiply(a, b) {
        return a * b;
    }
}

const PI = 3.14;
"#;
        let result = parser.parse(source).unwrap();

        let add = result.symbols.iter().find(|s| s.name == "add");
        assert!(add.is_some());
        assert_eq!(add.unwrap().kind, "fn");

        let calc = result.symbols.iter().find(|s| s.name == "Calculator");
        assert!(calc.is_some());
    }

    #[test]
    fn test_parse_imports() {
        let parser = TypeScriptParser::new("typescript");
        let source = r#"
import { useState, useEffect } from 'react';
import axios from 'axios';
import * as path from 'path';
"#;
        let result = parser.parse(source).unwrap();
        let paths: Vec<_> = result.imports.iter().map(|i| &i.import_path).collect();
        assert!(paths.contains(&&"react".to_string()));
        assert!(paths.contains(&&"axios".to_string()));
        assert!(paths.contains(&&"path".to_string()));
    }
}
