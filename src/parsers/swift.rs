use anyhow::{Context, Result};
use tree_sitter::{Language, Parser, Query, QueryCursor, StreamingIterator};

use super::{LanguageParser, ParseResult};
use crate::db::{ParsedImport, ParsedReference, ParsedSymbol};

pub struct SwiftParser {
    language: Language,
}

impl SwiftParser {
    pub fn new() -> Self {
        Self {
            language: tree_sitter_swift::LANGUAGE.into(),
        }
    }
}

fn text_from(source: &[u8], node: tree_sitter::Node) -> String {
    std::str::from_utf8(&source[node.byte_range()])
        .unwrap_or("")
        .to_string()
}

fn detect_visibility(source: &[u8], node: tree_sitter::Node) -> String {
    // Walk up to find the declaration, then check for modifiers child
    let text = text_from(source, node);
    if text.starts_with("public ") || text.starts_with("open ") {
        "pub".to_string()
    } else if text.starts_with("private ") || text.starts_with("fileprivate ") {
        "private".to_string()
    } else {
        // Swift default is internal
        "private".to_string()
    }
}

fn find_scope(source: &[u8], node: tree_sitter::Node) -> Option<String> {
    let mut current = node.parent();
    while let Some(parent) = current {
        if (parent.kind() == "class_declaration" || parent.kind() == "protocol_declaration")
            && let Some(name_node) = parent.child_by_field_name("name")
        {
            return Some(text_from(source, name_node));
        }
        current = parent.parent();
    }
    None
}

impl LanguageParser for SwiftParser {
    fn parse(&self, source: &str) -> Result<ParseResult> {
        let mut parser = Parser::new();
        parser
            .set_language(&self.language)
            .context("setting Swift language")?;

        let tree = parser.parse(source, None).context("parsing Swift source")?;

        let root = tree.root_node();
        let src = source.as_bytes();
        let mut result = ParseResult::default();

        // In tree-sitter-swift, struct/class/enum/extension/actor all use class_declaration
        // with a declaration_kind field. Protocol uses protocol_declaration.
        let symbol_query = Query::new(
            &self.language,
            r#"
            (function_declaration name: (simple_identifier) @fn_name) @fn_def
            (class_declaration name: (user_type) @class_name) @class_def
            (class_declaration name: (type_identifier) @class_name2) @class_def2
            (protocol_declaration name: (user_type) @protocol_name) @protocol_def
            (protocol_declaration name: (type_identifier) @protocol_name2) @protocol_def2
            (typealias_declaration name: (type_identifier) @type_name) @type_def
            "#,
        )
        .context("compiling Swift symbol query")?;

        let mut cursor = QueryCursor::new();
        let mut matches = cursor.matches(&symbol_query, root, src);

        while let Some(m) = matches.next() {
            let captures: Vec<_> = m
                .captures
                .iter()
                .map(|c| (symbol_query.capture_names()[c.index as usize], c.node))
                .collect();

            for &(name, node) in &captures {
                let (kind, sym_text) = match name {
                    "fn_name" => ("fn", text_from(src, node)),
                    "class_name" | "class_name2" => {
                        // Find declaration_kind by walking the class_declaration parent
                        let def_parent = captures
                            .iter()
                            .find(|(n, _)| *n == "class_def" || *n == "class_def2")
                            .map(|(_, n)| *n);
                        let kind = if let Some(def) = def_parent {
                            if let Some(dk) = def.child_by_field_name("declaration_kind") {
                                match text_from(src, dk).as_str() {
                                    "struct" => "struct",
                                    "class" => "class",
                                    "enum" => "enum",
                                    "extension" => {
                                        continue;
                                    }
                                    "actor" => "class",
                                    _ => "class",
                                }
                            } else {
                                "class"
                            }
                        } else {
                            "class"
                        };
                        (kind, text_from(src, node))
                    }
                    "protocol_name" | "protocol_name2" => ("trait", text_from(src, node)),
                    "type_name" => ("type", text_from(src, node)),
                    _ => continue,
                };

                let def_node = captures
                    .iter()
                    .find(|(n, _)| n.ends_with("_def"))
                    .map(|(_, n)| *n)
                    .unwrap_or(node);

                let visibility = detect_visibility(src, def_node);
                let scope = find_scope(src, node);

                result.symbols.push(ParsedSymbol {
                    name: sym_text,
                    kind: kind.to_string(),
                    line: node.start_position().row as i64 + 1,
                    column: node.start_position().column as i64,
                    end_line: def_node.end_position().row as i64 + 1,
                    visibility,
                    scope,
                });
            }
        }

        // --- Imports ---
        let import_query = Query::new(
            &self.language,
            r#"
            (import_declaration (identifier) @import_path)
            "#,
        )
        .context("compiling Swift import query")?;

        let mut cursor = QueryCursor::new();
        let mut matches = cursor.matches(&import_query, root, src);

        while let Some(m) = matches.next() {
            for c in m.captures {
                let text = text_from(src, c.node);
                if !text.is_empty() {
                    result.imports.push(ParsedImport {
                        import_path: text,
                        alias: None,
                    });
                }
            }
        }

        // --- References ---
        let ref_query = Query::new(
            &self.language,
            r#"
            (call_expression (simple_identifier) @call_name)
            "#,
        )
        .context("compiling Swift reference query")?;

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
    fn test_parse_swift_basics() {
        let parser = SwiftParser::new();
        let source = r#"
import Foundation

public class NetworkManager {
    public func fetchData(from url: String) -> Data? {
        return nil
    }

    private func parseResponse() {
    }
}

struct Config {
    let host: String
    let port: Int
}

enum Status {
    case active
    case inactive
}

protocol Fetchable {
    func fetch() -> Data
}
"#;
        let result = parser.parse(source).unwrap();

        assert!(
            !result.symbols.is_empty(),
            "Should find symbols in Swift code"
        );

        // Check imports
        let imports: Vec<_> = result.imports.iter().map(|i| &i.import_path).collect();
        assert!(
            imports.contains(&&"Foundation".to_string()),
            "Should find Foundation import, got: {:?}",
            imports
        );

        // Check some symbol types
        let kinds: Vec<_> = result.symbols.iter().map(|s| (&s.name, &s.kind)).collect();

        // NetworkManager should be class, Config should be struct, Status should be enum
        assert!(
            kinds
                .iter()
                .any(|(n, k)| n == &"NetworkManager" && k == &"class"),
            "Should find NetworkManager class, got: {:?}",
            kinds
        );
    }
}
