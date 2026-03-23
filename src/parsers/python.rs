use anyhow::{Context, Result};
use tree_sitter::{Language, Parser, Query, QueryCursor, StreamingIterator};

use super::{LanguageParser, ParseResult};
use crate::db::{ParsedImport, ParsedReference, ParsedSymbol};

pub struct PythonParser {
    language: Language,
}

impl PythonParser {
    pub fn new() -> Self {
        Self {
            language: tree_sitter_python::LANGUAGE.into(),
        }
    }

    fn visibility_from_name(name: &str) -> &'static str {
        if name.starts_with("__") && !name.ends_with("__") {
            "private"
        } else if name.starts_with('_') {
            "protected"
        } else {
            "pub"
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
        if parent.kind() == "class_definition"
            && let Some(name_node) = parent.child_by_field_name("name")
        {
            return Some(text_from(source, name_node));
        }
        current = parent.parent();
    }
    None
}

impl LanguageParser for PythonParser {
    fn parse(&self, source: &str) -> Result<ParseResult> {
        let mut parser = Parser::new();
        parser
            .set_language(&self.language)
            .context("setting Python language")?;

        let tree = parser
            .parse(source, None)
            .context("parsing Python source")?;

        let root = tree.root_node();
        let src = source.as_bytes();
        let mut result = ParseResult::default();

        // --- Symbol definitions ---
        let symbol_query = Query::new(
            &self.language,
            r#"
            (function_definition name: (identifier) @fn_name)
            (class_definition name: (identifier) @class_name)
            "#,
        )
        .context("compiling Python symbol query")?;

        let mut cursor = QueryCursor::new();
        let mut matches = cursor.matches(&symbol_query, root, src);

        while let Some(m) = matches.next() {
            for c in m.captures {
                let cname = symbol_query.capture_names()[c.index as usize];
                let sym_text = text_from(src, c.node);

                let kind = match cname {
                    "fn_name" => "fn",
                    "class_name" => "class",
                    _ => continue,
                };

                let visibility = Self::visibility_from_name(&sym_text);
                let scope = find_class_scope(src, c.node);

                result.symbols.push(ParsedSymbol {
                    name: sym_text,
                    kind: kind.to_string(),
                    line: c.node.start_position().row as i64 + 1,
                    column: c.node.start_position().column as i64,
                    visibility: visibility.to_string(),
                    scope,
                });
            }
        }

        // --- Module-level UPPER_CASE assignments (constants) ---
        let const_query = Query::new(
            &self.language,
            r#"
            (module (expression_statement (assignment left: (identifier) @const_name)))
            "#,
        )
        .context("compiling Python constant query")?;

        let mut cursor = QueryCursor::new();
        let mut matches = cursor.matches(&const_query, root, src);

        while let Some(m) = matches.next() {
            for c in m.captures {
                let sym_text = text_from(src, c.node);
                if sym_text.chars().all(|c| c.is_uppercase() || c == '_') && !sym_text.is_empty() {
                    result.symbols.push(ParsedSymbol {
                        name: sym_text,
                        kind: "const".to_string(),
                        line: c.node.start_position().row as i64 + 1,
                        column: c.node.start_position().column as i64,
                        visibility: "pub".to_string(),
                        scope: None,
                    });
                }
            }
        }

        // --- Imports ---
        let import_query = Query::new(
            &self.language,
            r#"
            (import_statement name: (dotted_name) @import_path)
            (import_from_statement module_name: (dotted_name) @from_path)
            (import_from_statement module_name: (relative_import) @rel_path)
            "#,
        )
        .context("compiling Python import query")?;

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
            (call function: (identifier) @call_name)
            (call function: (attribute attribute: (identifier) @method_call))
            "#,
        )
        .context("compiling Python reference query")?;

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
    fn test_parse_functions_and_classes() {
        let parser = PythonParser::new();
        let source = r#"
def hello():
    pass

def _private_helper():
    pass

class MyClass:
    def __init__(self):
        pass

    def public_method(self):
        pass

    def _protected_method(self):
        pass

    def __private_method(self):
        pass
"#;
        let result = parser.parse(source).unwrap();

        let hello = result.symbols.iter().find(|s| s.name == "hello").unwrap();
        assert_eq!(hello.kind, "fn");
        assert_eq!(hello.visibility, "pub");

        let private = result
            .symbols
            .iter()
            .find(|s| s.name == "_private_helper")
            .unwrap();
        assert_eq!(private.visibility, "protected");

        let cls = result.symbols.iter().find(|s| s.name == "MyClass").unwrap();
        assert_eq!(cls.kind, "class");
        assert_eq!(cls.visibility, "pub");

        let pub_method = result
            .symbols
            .iter()
            .find(|s| s.name == "public_method")
            .unwrap();
        assert_eq!(pub_method.scope, Some("MyClass".to_string()));
    }

    #[test]
    fn test_parse_imports() {
        let parser = PythonParser::new();
        let source = r#"
import os
import sys
from pathlib import Path
from collections import defaultdict
"#;
        let result = parser.parse(source).unwrap();
        assert!(!result.imports.is_empty());
        let paths: Vec<_> = result.imports.iter().map(|i| &i.import_path).collect();
        assert!(paths.contains(&&"os".to_string()));
    }

    #[test]
    fn test_parse_constants() {
        let parser = PythonParser::new();
        let source = r#"
MAX_SIZE = 100
DEFAULT_NAME = "hello"
my_variable = "not a constant"
"#;
        let result = parser.parse(source).unwrap();
        let consts: Vec<_> = result
            .symbols
            .iter()
            .filter(|s| s.kind == "const")
            .collect();
        assert!(consts.iter().any(|s| s.name == "MAX_SIZE"));
        assert!(consts.iter().any(|s| s.name == "DEFAULT_NAME"));
        assert!(!consts.iter().any(|s| s.name == "my_variable"));
    }
}
