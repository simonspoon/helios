use anyhow::{Context, Result};
use tree_sitter::{Language, Parser, Query, QueryCursor, StreamingIterator};

use super::{LanguageParser, ParseResult};
use crate::db::{ParsedImport, ParsedReference, ParsedSymbol};

pub struct GoParser {
    language: Language,
}

impl GoParser {
    pub fn new() -> Self {
        Self {
            language: tree_sitter_go::LANGUAGE.into(),
        }
    }

    fn visibility_from_name(name: &str) -> &'static str {
        if name.starts_with(|c: char| c.is_uppercase()) {
            "pub"
        } else {
            "private"
        }
    }
}

fn text_from(source: &[u8], node: tree_sitter::Node) -> String {
    std::str::from_utf8(&source[node.byte_range()])
        .unwrap_or("")
        .to_string()
}

fn find_receiver_type(source: &[u8], method_node: tree_sitter::Node) -> Option<String> {
    let parent = method_node.parent()?;
    let receiver = parent.child_by_field_name("receiver")?;
    // Walk children of receiver (parameter_list) to find type_identifier
    let mut cursor = receiver.walk();
    for child in receiver.children(&mut cursor) {
        if child.kind() == "parameter_declaration" {
            let mut inner = child.walk();
            for c in child.children(&mut inner) {
                if c.kind() == "type_identifier" {
                    return Some(text_from(source, c));
                }
                if c.kind() == "pointer_type" {
                    let mut ptr = c.walk();
                    for pc in c.children(&mut ptr) {
                        if pc.kind() == "type_identifier" {
                            return Some(text_from(source, pc));
                        }
                    }
                }
            }
        }
    }
    None
}

impl LanguageParser for GoParser {
    fn parse(&self, source: &str) -> Result<ParseResult> {
        let mut parser = Parser::new();
        parser
            .set_language(&self.language)
            .context("setting Go language")?;

        let tree = parser.parse(source, None).context("parsing Go source")?;
        let root = tree.root_node();
        let src = source.as_bytes();
        let mut result = ParseResult::default();

        // --- Symbol definitions ---
        let symbol_query = Query::new(
            &self.language,
            r#"
            (function_declaration name: (identifier) @fn_name)
            (method_declaration name: (field_identifier) @method_name)
            (type_declaration (type_spec name: (type_identifier) @type_name type: (struct_type)))
            (type_declaration (type_spec name: (type_identifier) @iface_name type: (interface_type)))
            (const_declaration (const_spec name: (identifier) @const_name))
            (var_declaration (var_spec name: (identifier) @var_name))
            "#,
        )
        .context("compiling Go symbol query")?;

        let mut cursor = QueryCursor::new();
        let mut matches = cursor.matches(&symbol_query, root, src);

        while let Some(m) = matches.next() {
            for c in m.captures {
                let cname = symbol_query.capture_names()[c.index as usize];
                let sym_text = text_from(src, c.node);

                let kind = match cname {
                    "fn_name" => "fn",
                    "method_name" => "fn",
                    "type_name" => "struct",
                    "iface_name" => "interface",
                    "const_name" | "var_name" => "const",
                    _ => continue,
                };

                let visibility = Self::visibility_from_name(&sym_text);
                let scope = if cname == "method_name" {
                    find_receiver_type(src, c.node)
                } else {
                    None
                };

                // Use the parent declaration node for end_line
                let def_node = c.node.parent().unwrap_or(c.node);
                result.symbols.push(ParsedSymbol {
                    name: sym_text,
                    kind: kind.to_string(),
                    line: c.node.start_position().row as i64 + 1,
                    column: c.node.start_position().column as i64,
                    end_line: def_node.end_position().row as i64 + 1,
                    visibility: visibility.to_string(),
                    scope,
                });
            }
        }

        // --- Imports ---
        let import_query = Query::new(
            &self.language,
            r#"
            (import_declaration (import_spec path: (interpreted_string_literal) @import_path))
            (import_declaration (import_spec_list (import_spec path: (interpreted_string_literal) @list_import_path)))
            "#,
        )
        .context("compiling Go import query")?;

        let mut cursor = QueryCursor::new();
        let mut matches = cursor.matches(&import_query, root, src);

        while let Some(m) = matches.next() {
            for c in m.captures {
                let text = text_from(src, c.node);
                let path = text.trim_matches('"').to_string();
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
            (call_expression function: (selector_expression field: (field_identifier) @method_call))
            "#,
        )
        .context("compiling Go reference query")?;

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
    fn test_parse_functions_and_methods() {
        let parser = GoParser::new();
        let source = r#"
package main

func Hello() string {
    return "hello"
}

func privateHelper() int {
    return 42
}

type Server struct {
    Port int
}

func (s *Server) Start() error {
    return nil
}
"#;
        let result = parser.parse(source).unwrap();
        let fns: Vec<_> = result.symbols.iter().filter(|s| s.kind == "fn").collect();
        assert!(fns.len() >= 3);

        let hello = fns.iter().find(|s| s.name == "Hello").unwrap();
        assert_eq!(hello.visibility, "pub");

        let helper = fns.iter().find(|s| s.name == "privateHelper").unwrap();
        assert_eq!(helper.visibility, "private");

        let start = fns.iter().find(|s| s.name == "Start").unwrap();
        assert_eq!(start.visibility, "pub");
        assert_eq!(start.scope, Some("Server".to_string()));
    }

    #[test]
    fn test_parse_structs_and_interfaces() {
        let parser = GoParser::new();
        let source = r#"
package main

type Config struct {
    Host string
    Port int
}

type Handler interface {
    Handle(req Request) Response
}
"#;
        let result = parser.parse(source).unwrap();
        let names: Vec<_> = result.symbols.iter().map(|s| (&s.name, &s.kind)).collect();
        assert!(names.contains(&(&"Config".to_string(), &"struct".to_string())));
        assert!(names.contains(&(&"Handler".to_string(), &"interface".to_string())));
    }

    #[test]
    fn test_parse_imports() {
        let parser = GoParser::new();
        let source = r#"
package main

import (
    "fmt"
    "os"
)
"#;
        let result = parser.parse(source).unwrap();
        let paths: Vec<_> = result.imports.iter().map(|i| &i.import_path).collect();
        assert!(paths.contains(&&"fmt".to_string()));
        assert!(paths.contains(&&"os".to_string()));
    }

    #[test]
    fn test_parse_consts() {
        let parser = GoParser::new();
        let source = r#"
package main

const MaxRetries = 3
const defaultTimeout = 30
"#;
        let result = parser.parse(source).unwrap();
        let consts: Vec<_> = result
            .symbols
            .iter()
            .filter(|s| s.kind == "const")
            .collect();
        assert!(consts.len() >= 2);

        let max = consts.iter().find(|s| s.name == "MaxRetries").unwrap();
        assert_eq!(max.visibility, "pub");
    }
}
