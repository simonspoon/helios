use anyhow::{Context, Result};
use tree_sitter::{Language, Parser, Query, QueryCursor, StreamingIterator};

use super::{LanguageParser, ParseResult, is_function_local};
use crate::db::{ParsedImport, ParsedReference, ParsedSymbol};

/// Node kinds whose body holds function-local declarations.
const CALLABLE_KINDS: &[&str] = &[
    "function_declaration",
    "init_declaration",
    "deinit_declaration",
    "subscript_declaration",
    "computed_property",
    "willset_didset_block",
    "lambda_literal",
];

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

/// Each parameter's source spelling, verbatim (external label, internal
/// name, type, default). `def_node` is the function_declaration itself --
/// tree-sitter-swift hangs `parameter` nodes off it as plain positional
/// children rather than under a parameter-list node, and it attaches a
/// defaulted parameter's `= <value>` as *siblings* after the parameter
/// rather than nesting them inside it, so a default has to be picked up by
/// looking at the following two children.
fn callable_params(source: &[u8], def_node: tree_sitter::Node) -> Option<Vec<String>> {
    let mut cursor = def_node.walk();
    let children: Vec<_> = def_node.children(&mut cursor).collect();
    let mut params = Vec::new();
    for (i, child) in children.iter().enumerate() {
        if child.kind() != "parameter" {
            continue;
        }
        let mut end = child.end_byte();
        if children.get(i + 1).is_some_and(|n| n.kind() == "=")
            && let Some(value) = children.get(i + 2)
        {
            end = value.end_byte();
        }
        let text = std::str::from_utf8(&source[child.start_byte()..end])
            .unwrap_or("")
            .trim()
            .to_string();
        params.push(text);
    }
    Some(params)
}

/// The declared return type's source spelling (no leading `->`), if any.
fn callable_returns(source: &[u8], def_node: tree_sitter::Node) -> Option<String> {
    def_node
        .child_by_field_name("return_type")
        .map(|n| text_from(source, n).trim().to_string())
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

                if is_function_local(def_node, CALLABLE_KINDS) {
                    continue;
                }

                let visibility = detect_visibility(src, def_node);
                let scope = find_scope(src, node);

                // Only `fn` (function_declaration) is callable here --
                // init/method/etc. aren't captured as symbols by this
                // query, so there's nothing else to give params/returns to.
                let (params, returns) = if name == "fn_name" {
                    (
                        callable_params(src, def_node),
                        callable_returns(src, def_node),
                    )
                } else {
                    (None, None)
                };

                result.symbols.push(ParsedSymbol {
                    name: sym_text,
                    kind: kind.to_string(),
                    line: node.start_position().row as i64 + 1,
                    column: node.start_position().column as i64,
                    end_line: def_node.end_position().row as i64 + 1,
                    visibility,
                    scope,
                    params,
                    returns,
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
                        names: Vec::new(),
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
                    from_scope: None,
                    // Only bare calls are captured.
                    qualified: false,
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
    fn test_params_and_returns_round_trip() {
        let parser = SwiftParser::new();
        let source = r#"
func fetch(_ name: String, count: Int = 0, completion: @escaping () -> Void) -> [String] {
    return []
}

func noop() {}

func untyped() {
}

struct Config {
    let host: String
}
"#;
        let result = parser.parse(source).unwrap();
        let sym = |name: &str| result.symbols.iter().find(|s| s.name == name).unwrap();

        assert_eq!(
            sym("fetch").params,
            Some(vec![
                "_ name: String".to_string(),
                "count: Int = 0".to_string(),
                "completion: @escaping () -> Void".to_string(),
            ])
        );
        assert_eq!(sym("fetch").returns, Some("[String]".to_string()));

        assert_eq!(sym("noop").params, Some(vec![]));
        assert_eq!(sym("untyped").returns, None);

        let config = sym("Config");
        assert_eq!(config.params, None);
        assert_eq!(config.returns, None);
    }

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

    #[test]
    fn test_function_locals_are_not_symbols() {
        let parser = SwiftParser::new();
        let source = r#"
func outer() {
    func inner() {}
    inner()
}

class Service {
    func method() {}

    init() {
        func inInit() {}
    }

    var computed: Int {
        func inGetter() -> Int { return 1 }
        return inGetter()
    }
}
"#;
        let result = parser.parse(source).unwrap();
        let names: Vec<_> = result.symbols.iter().map(|s| s.name.as_str()).collect();

        assert!(names.contains(&"outer"));
        assert!(names.contains(&"Service"));
        assert!(names.contains(&"method"));

        for local in ["inner", "inInit", "inGetter"] {
            assert!(!names.contains(&local), "{local} is a function local");
        }
    }
}
