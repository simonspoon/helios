use anyhow::{Context, Result};
use tree_sitter::{Language, Parser, Query, QueryCursor, StreamingIterator};

use super::{LanguageParser, ParseResult, is_function_local};
use crate::db::{ParsedImport, ParsedReference, ParsedSymbol, ParsedTypeRelation, UsageKind};

/// Node kinds whose body holds function-local definitions.
const CALLABLE_KINDS: &[&str] = &["function_definition", "lambda"];

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

/// The local names a `from x import ...` statement binds: `from .money import
/// format_money, tax as vat` binds `format_money` and `vat`.
///
/// The *local* name is what the file's own references spell, so that is what is
/// recorded. An aliased import therefore no longer matches the definition's
/// name, and attribution falls back to the ambiguous-name behaviour instead of
/// pointing the usage at the wrong definition. `from x import *` binds no name
/// the parser can see.
fn import_from_names(source: &[u8], statement: tree_sitter::Node) -> Vec<String> {
    let mut cursor = statement.walk();
    statement
        .children_by_field_name("name", &mut cursor)
        .filter_map(|node| match node.kind() {
            "aliased_import" => node.child_by_field_name("alias"),
            _ => Some(node),
        })
        .map(|node| text_from(source, node))
        .collect()
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

                // Use the parent definition node for end_line
                let def_node = c.node.parent().unwrap_or(c.node);
                if is_function_local(def_node, CALLABLE_KINDS) {
                    continue;
                }

                let visibility = Self::visibility_from_name(&sym_text);
                let scope = find_class_scope(src, c.node);

                let (params, returns) = if kind == "fn" {
                    (
                        def_node.child_by_field_name("parameters").map(|p| {
                            let mut pc = p.walk();
                            p.named_children(&mut pc)
                                .map(|param| text_from(src, param).trim().to_string())
                                .collect()
                        }),
                        def_node
                            .child_by_field_name("return_type")
                            .map(|t| text_from(src, t).trim().to_string()),
                    )
                } else {
                    (None, None)
                };

                result.symbols.push(ParsedSymbol {
                    name: sym_text,
                    kind: kind.to_string(),
                    line: c.node.start_position().row as i64 + 1,
                    column: c.node.start_position().column as i64,
                    end_line: def_node.end_position().row as i64 + 1,
                    visibility: visibility.to_string(),
                    scope,
                    params,
                    returns,
                });
            }
        }

        // --- Type relations (base classes) ---
        //
        // A class's bases live in an optional `superclasses` field holding
        // an `argument_list`. The field is absent entirely for `class C:`
        // (no parens), so the query below simply doesn't match it. `class
        // C():` does have the field, just with an empty `argument_list`, so
        // it matches but contributes no relations. Either way that's the
        // required zero rows.
        //
        // `argument_list` mixes positional bases with keyword arguments
        // (`metaclass=Meta`) with no way to ask the query for "positional
        // only", so keyword_argument children are filtered out by hand.
        let type_rel_query = Query::new(
            &self.language,
            r#"
            (class_definition name: (identifier) @class_name superclasses: (argument_list) @bases) @class_def
            "#,
        )
        .context("compiling Python type relation query")?;

        let mut cursor = QueryCursor::new();
        let mut matches = cursor.matches(&type_rel_query, root, src);

        while let Some(m) = matches.next() {
            let captures: Vec<_> = m
                .captures
                .iter()
                .map(|c| (type_rel_query.capture_names()[c.index as usize], c.node))
                .collect();

            let Some(&(_, def_node)) = captures.iter().find(|(n, _)| *n == "class_def") else {
                continue;
            };
            if is_function_local(def_node, CALLABLE_KINDS) {
                continue;
            }

            let Some(&(_, name_node)) = captures.iter().find(|(n, _)| *n == "class_name") else {
                continue;
            };
            let Some(&(_, bases_node)) = captures.iter().find(|(n, _)| *n == "bases") else {
                continue;
            };

            let sub_name = text_from(src, name_node);
            // The sub is always the class this bases clause is attached to,
            // so it's always declared right here -- `sub_line` is always `Some`.
            let sub_line = Some(name_node.start_position().row as i64 + 1);

            let mut bcursor = bases_node.walk();
            for base in bases_node.named_children(&mut bcursor) {
                if base.kind() == "keyword_argument" {
                    continue;
                }
                result.type_relations.push(ParsedTypeRelation {
                    sub_name: sub_name.clone(),
                    sub_line,
                    super_name: text_from(src, base),
                    kind: "extends".to_string(),
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
                    // Use the grandparent (expression_statement) for end_line
                    let assignment_node = c.node.parent();
                    let def_node = assignment_node.and_then(|p| p.parent()).unwrap_or(c.node);
                    let returns = assignment_node
                        .and_then(|a| a.child_by_field_name("type"))
                        .map(|t| text_from(src, t).trim().to_string());
                    result.symbols.push(ParsedSymbol {
                        name: sym_text,
                        kind: "const".to_string(),
                        line: c.node.start_position().row as i64 + 1,
                        column: c.node.start_position().column as i64,
                        end_line: def_node.end_position().row as i64 + 1,
                        visibility: "pub".to_string(),
                        scope: None,
                        params: None,
                        returns,
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
                    // `from x import a, b` binds a and b; plain `import x.y`
                    // binds the module, which usages spell as a prefix
                    // (`x.y.f()`), not as the name a reference records.
                    let names = match c.node.parent() {
                        Some(parent) if parent.kind() == "import_from_statement" => {
                            import_from_names(src, parent)
                        }
                        _ => Vec::new(),
                    };
                    result.imports.push(ParsedImport {
                        import_path: text,
                        alias: None,
                        names,
                    });
                }
            }
        }

        // --- References ---
        //
        // Attribute write targets (`x.count = 1`, `x.count += 1`) are
        // deliberately not captured -- see the comment above the reference
        // query in `rust_parser.rs` for why: no parser in this codebase
        // indexes plain fields as symbols, so a write capture has no
        // correct symbol to resolve to and can only produce a confidently
        // wrong "who mutates this" answer.
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
                    from_scope: None,
                    // `money.format_money()` names an attribute of some
                    // receiver, not the bare name an import binds.
                    qualified: ref_query.capture_names()[c.index as usize] == "method_call",
                    // These captures are calls (including `T()` constructor
                    // calls, which the grammar has no separate node for),
                    // which read the callee/constructed type.
                    usage_kind: UsageKind::Read,
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
    fn test_params_and_returns() {
        let parser = PythonParser::new();
        let source = r#"
def add(x: int, y: int = 3) -> int:
    return x + y

def no_args():
    pass

def untyped_return(a):
    return a

class Service:
    def method(self, *args, **kwargs):
        pass
"#;
        let result = parser.parse(source).unwrap();

        let add = result.symbols.iter().find(|s| s.name == "add").unwrap();
        assert_eq!(
            add.params,
            Some(vec!["x: int".to_string(), "y: int = 3".to_string()])
        );
        assert_eq!(add.returns, Some("int".to_string()));

        let no_args = result.symbols.iter().find(|s| s.name == "no_args").unwrap();
        assert_eq!(no_args.params, Some(vec![]));
        assert_eq!(no_args.returns, None);

        let untyped = result
            .symbols
            .iter()
            .find(|s| s.name == "untyped_return")
            .unwrap();
        assert_eq!(untyped.returns, None);

        let method = result.symbols.iter().find(|s| s.name == "method").unwrap();
        assert_eq!(
            method.params,
            Some(vec![
                "self".to_string(),
                "*args".to_string(),
                "**kwargs".to_string()
            ])
        );
    }

    #[test]
    fn test_class_has_no_params_or_returns() {
        let parser = PythonParser::new();
        let result = parser.parse("class C:\n    pass\n").unwrap();
        let cls = result.symbols.iter().find(|s| s.name == "C").unwrap();
        assert_eq!(cls.params, None);
        assert_eq!(cls.returns, None);
    }

    #[test]
    fn test_annotated_constant_records_returns() {
        let parser = PythonParser::new();
        let source = r#"
MAX_SIZE: int = 100
UNTYPED_CONST = 5
"#;
        let result = parser.parse(source).unwrap();
        let max = result
            .symbols
            .iter()
            .find(|s| s.name == "MAX_SIZE")
            .unwrap();
        assert_eq!(max.returns, Some("int".to_string()));

        let untyped = result
            .symbols
            .iter()
            .find(|s| s.name == "UNTYPED_CONST")
            .unwrap();
        assert_eq!(untyped.returns, None);
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

    /// `from x import a, b as c` binds a and c; plain `import x` binds a module
    /// name that no reference spells on its own.
    #[test]
    fn import_names_are_the_local_bindings() {
        let parser = PythonParser::new();
        let source = r#"
from .money import format_money, tax as vat
from ..util import helpers
import os
"#;
        let result = parser.parse(source).unwrap();
        let names = |path: &str| -> Vec<String> {
            result
                .imports
                .iter()
                .find(|i| i.import_path == path)
                .unwrap()
                .names
                .clone()
        };
        assert_eq!(names(".money"), vec!["format_money", "vat"]);
        assert_eq!(names("..util"), vec!["helpers"]);
        assert!(names("os").is_empty());
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

    #[test]
    fn test_function_locals_are_not_symbols() {
        let parser = PythonParser::new();
        let source = r#"
def outer():
    def inner():
        return 1

    class Local:
        pass

    return inner()


class Service:
    def method(self):
        return 1
"#;
        let result = parser.parse(source).unwrap();
        let names: Vec<_> = result.symbols.iter().map(|s| s.name.as_str()).collect();

        assert!(names.contains(&"outer"));
        assert!(names.contains(&"Service"));
        assert!(names.contains(&"method"));
        assert!(!names.contains(&"inner"));
        assert!(!names.contains(&"Local"));
    }

    /// `ParsedTypeRelation` has no `PartialEq`, so tests compare this plain
    /// tuple projection instead.
    fn relation_tuples(relations: &[ParsedTypeRelation]) -> Vec<(&str, Option<i64>, &str, &str)> {
        relations
            .iter()
            .map(|r| {
                (
                    r.sub_name.as_str(),
                    r.sub_line,
                    r.super_name.as_str(),
                    r.kind.as_str(),
                )
            })
            .collect()
    }

    #[test]
    fn test_class_extends_multiple_bases() {
        let parser = PythonParser::new();
        let result = parser.parse("class C(Base, Mixin):\n    pass\n").unwrap();
        assert_eq!(
            relation_tuples(&result.type_relations),
            vec![("C", Some(1), "Base", "extends"), ("C", Some(1), "Mixin", "extends")]
        );
    }

    #[test]
    fn test_class_with_no_bases_has_no_relations() {
        let parser = PythonParser::new();
        let result = parser.parse("class C:\n    pass\n").unwrap();
        assert!(result.type_relations.is_empty());
    }

    #[test]
    fn test_class_with_empty_parens_has_no_relations() {
        let parser = PythonParser::new();
        let result = parser.parse("class C():\n    pass\n").unwrap();
        assert!(result.type_relations.is_empty());
    }

    #[test]
    fn test_keyword_argument_is_not_a_base_class() {
        let parser = PythonParser::new();
        let result = parser
            .parse("class C(Base, metaclass=Meta):\n    pass\n")
            .unwrap();
        assert_eq!(
            relation_tuples(&result.type_relations),
            vec![("C", Some(1), "Base", "extends")]
        );
    }

    #[test]
    fn test_qualified_and_generic_bases_keep_raw_text() {
        let parser = PythonParser::new();
        let result = parser
            .parse("class C(abc.ABC, Generic[T]):\n    pass\n")
            .unwrap();
        assert_eq!(
            relation_tuples(&result.type_relations),
            vec![
                ("C", Some(1), "abc.ABC", "extends"),
                ("C", Some(1), "Generic[T]", "extends")
            ]
        );
    }

    #[test]
    fn test_ordinary_call_is_still_read() {
        let parser = PythonParser::new();
        let result = parser.parse("x.method()").unwrap();
        let r = result
            .references
            .iter()
            .find(|r| r.symbol_name == "method")
            .unwrap();
        assert_eq!(r.usage_kind, UsageKind::Read);
    }

    #[test]
    fn test_assignment_targets_emit_no_reference() {
        let parser = PythonParser::new();
        let result = parser.parse("count = 1\nx.count = 1\nx.count += 1\n").unwrap();
        assert!(
            result.references.is_empty(),
            "assignment targets must not be captured as references: {:?}",
            result.references
        );
    }
}
