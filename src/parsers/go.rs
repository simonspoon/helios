use anyhow::{Context, Result};
use tree_sitter::{Language, Parser, Query, QueryCursor, StreamingIterator};

use super::{LanguageParser, ParseResult, is_function_local};
use crate::db::{ParsedImport, ParsedReference, ParsedSymbol, UsageKind};

/// Node kinds whose body holds function-local declarations.
const CALLABLE_KINDS: &[&str] = &["function_declaration", "method_declaration", "func_literal"];

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

/// Walk up from a struct field's `field_identifier` capture to the enclosing
/// `type_spec` and return the struct's own name (the field's `scope`).
fn find_enclosing_struct(source: &[u8], field_ident: tree_sitter::Node) -> Option<String> {
    let mut node = field_ident.parent()?; // field_declaration
    loop {
        if node.kind() == "struct_type" {
            let type_spec = node.parent()?;
            if type_spec.kind() != "type_spec" {
                return None;
            }
            let name_node = type_spec.child_by_field_name("name")?;
            return Some(text_from(source, name_node));
        }
        node = node.parent()?;
    }
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
            (struct_type (field_declaration_list (field_declaration name: (field_identifier) @field_name)))
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
                    "field_name" => "field",
                    _ => continue,
                };

                // Use the parent declaration node for end_line
                let def_node = c.node.parent().unwrap_or(c.node);
                if is_function_local(def_node, CALLABLE_KINDS) {
                    continue;
                }

                let visibility = Self::visibility_from_name(&sym_text);
                let scope = if cname == "method_name" {
                    find_receiver_type(src, c.node)
                } else if cname == "field_name" {
                    find_enclosing_struct(src, c.node)
                } else {
                    None
                };

                let (params, returns) = match cname {
                    "fn_name" | "method_name" => (
                        def_node.child_by_field_name("parameters").map(|p| {
                            let mut pc = p.walk();
                            p.named_children(&mut pc)
                                .map(|param| text_from(src, param).trim().to_string())
                                .collect()
                        }),
                        def_node
                            .child_by_field_name("result")
                            .map(|r| text_from(src, r).trim().to_string()),
                    ),
                    "const_name" | "var_name" | "field_name" => (
                        None,
                        def_node
                            .child_by_field_name("type")
                            .map(|t| text_from(src, t).trim().to_string()),
                    ),
                    _ => (None, None),
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
                        names: Vec::new(),
                    });
                }
            }
        }

        // --- References (function calls, member writes) ---
        //
        // Calls (`call_name`, `method_call`) are unqualified or qualified
        // reads of the callee, `member: false`, which at index time
        // (`resolve_member_candidates` in `src/indexer.rs`) restricts
        // resolution to field-kind symbols only, so a member write can
        // never land on an unrelated same-named function or type elsewhere
        // in the index.
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
                    from_scope: None,
                    qualified: ref_query.capture_names()[c.index as usize] == "method_call",
                    // These captures are calls, which read the callee.
                    usage_kind: UsageKind::Read,
                    // Calls, not member writes.
                    member: false,
                });
            }
        }

        // Member write targets: `x.count = 1` (Write), `x.count += 1` and
        // every other compound-assign operator (ReadWrite), and
        // `x.count++`/`x.count--` (ReadWrite).
        //
        // Go's `assignment_statement` uses a single node kind for every
        // operator (`=`, `+=`, `-=`, ...), distinguished only by the text of
        // its `operator` field, and its `left` side is a comma-separated
        // `expression_list` that may hold several targets at once (`x.a,
        // y.b = 1, 2`). Neither is expressible as a plain capture query, so
        // the statement node is captured whole and walked here. Go's short
        // variable declaration (`x := 1`) is a distinct node kind
        // (`short_var_declaration`) that this query does not match, so it
        // correctly produces no reference.
        let write_query = Query::new(
            &self.language,
            r#"
            (assignment_statement) @assign_stmt
            (inc_statement) @incdec_stmt
            (dec_statement) @incdec_stmt
            "#,
        )
        .context("compiling Go member-write query")?;

        let mut cursor = QueryCursor::new();
        let mut matches = cursor.matches(&write_query, root, src);

        while let Some(m) = matches.next() {
            for c in m.captures {
                let cname = write_query.capture_names()[c.index as usize];
                match cname {
                    "assign_stmt" => {
                        let (Some(left), Some(operator)) = (
                            c.node.child_by_field_name("left"),
                            c.node.child_by_field_name("operator"),
                        ) else {
                            continue;
                        };
                        let usage_kind = if text_from(src, operator) == "=" {
                            UsageKind::Write
                        } else {
                            UsageKind::ReadWrite
                        };
                        let mut lc = left.walk();
                        for target in left.named_children(&mut lc) {
                            if target.kind() != "selector_expression" {
                                continue;
                            }
                            let Some(field) = target.child_by_field_name("field") else {
                                continue;
                            };
                            result.references.push(ParsedReference {
                                symbol_name: text_from(src, field),
                                line: field.start_position().row as i64 + 1,
                                column: field.start_position().column as i64,
                                from_scope: None,
                                qualified: true,
                                usage_kind,
                                member: true,
                            });
                        }
                    }
                    "incdec_stmt" => {
                        let Some(target) = c.node.named_child(0) else {
                            continue;
                        };
                        if target.kind() != "selector_expression" {
                            continue;
                        }
                        let Some(field) = target.child_by_field_name("field") else {
                            continue;
                        };
                        result.references.push(ParsedReference {
                            symbol_name: text_from(src, field),
                            line: field.start_position().row as i64 + 1,
                            column: field.start_position().column as i64,
                            from_scope: None,
                            qualified: true,
                            usage_kind: UsageKind::ReadWrite,
                            member: true,
                        });
                    }
                    _ => {}
                }
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
    fn test_params_and_returns() {
        let parser = GoParser::new();
        let source = r#"
package main

func Add(a int, b, c string) (int, error) {
    return a, nil
}

func NoArgs() {
}

func OneResult(x int) error {
    return nil
}
"#;
        let result = parser.parse(source).unwrap();
        let fns: Vec<_> = result.symbols.iter().filter(|s| s.kind == "fn").collect();

        let add = fns.iter().find(|s| s.name == "Add").unwrap();
        assert_eq!(
            add.params,
            Some(vec!["a int".to_string(), "b, c string".to_string()])
        );
        assert_eq!(add.returns, Some("(int, error)".to_string()));

        let no_args = fns.iter().find(|s| s.name == "NoArgs").unwrap();
        assert_eq!(no_args.params, Some(vec![]));
        assert_eq!(no_args.returns, None);

        let one_result = fns.iter().find(|s| s.name == "OneResult").unwrap();
        assert_eq!(one_result.params, Some(vec!["x int".to_string()]));
        assert_eq!(one_result.returns, Some("error".to_string()));
    }

    #[test]
    fn test_typed_const_records_returns() {
        let parser = GoParser::new();
        let source = r#"
package main

const MaxRetries int = 3
const Untyped = 3
"#;
        let result = parser.parse(source).unwrap();
        let max = result
            .symbols
            .iter()
            .find(|s| s.name == "MaxRetries")
            .unwrap();
        assert_eq!(max.returns, Some("int".to_string()));
        assert_eq!(max.params, None);

        let untyped = result.symbols.iter().find(|s| s.name == "Untyped").unwrap();
        assert_eq!(untyped.returns, None);
    }

    #[test]
    fn test_struct_has_no_params_or_returns() {
        let parser = GoParser::new();
        let source = r#"
package main

type Config struct {
    Host string
}
"#;
        let result = parser.parse(source).unwrap();
        let cfg = result.symbols.iter().find(|s| s.name == "Config").unwrap();
        assert_eq!(cfg.params, None);
        assert_eq!(cfg.returns, None);
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

    #[test]
    fn test_function_locals_are_not_symbols() {
        let parser = GoParser::new();
        let source = r#"
package main

var Global = 1

func Run() int {
	var local = 2
	const localConst = 3
	return local + localConst
}
"#;
        let result = parser.parse(source).unwrap();
        let names: Vec<_> = result.symbols.iter().map(|s| s.name.as_str()).collect();

        assert!(names.contains(&"Global"));
        assert!(names.contains(&"Run"));
        assert!(!names.contains(&"local"));
        assert!(!names.contains(&"localConst"));
    }

    #[test]
    fn test_ordinary_call_is_still_read() {
        let parser = GoParser::new();
        let result = parser
            .parse("package main\nfunc f() { x.Method() }")
            .unwrap();
        let r = result
            .references
            .iter()
            .find(|r| r.symbol_name == "Method")
            .unwrap();
        assert_eq!(r.usage_kind, UsageKind::Read);
    }

    #[test]
    fn test_bare_assignment_target_emits_no_reference() {
        let parser = GoParser::new();
        let result = parser
            .parse("package main\nfunc f() { count = 1; count := 1 }")
            .unwrap();
        assert!(
            result.references.is_empty(),
            "bare assignment/declaration targets must not be captured as references: {:?}",
            result.references
        );
    }

    #[test]
    fn test_field_write_emits_member_write_reference() {
        let parser = GoParser::new();
        let result = parser
            .parse("package main\nfunc f() { x.count = 1 }")
            .unwrap();
        let r = result
            .references
            .iter()
            .find(|r| r.symbol_name == "count")
            .unwrap();
        assert!(r.member);
        assert!(r.qualified);
        assert_eq!(r.usage_kind, UsageKind::Write);
    }

    #[test]
    fn test_compound_field_assignment_and_incdec_are_readwrite() {
        let parser = GoParser::new();
        let result = parser
            .parse("package main\nfunc f() { x.count += 1; x.count++; x.count-- }")
            .unwrap();
        let writes: Vec<_> = result
            .references
            .iter()
            .filter(|r| r.symbol_name == "count")
            .collect();
        assert_eq!(writes.len(), 3, "{:?}", result.references);
        for r in writes {
            assert!(r.member);
            assert_eq!(r.usage_kind, UsageKind::ReadWrite);
        }
    }

    #[test]
    fn test_multi_target_assignment_emits_one_write_per_field() {
        let parser = GoParser::new();
        let result = parser
            .parse("package main\nfunc f() { x.a, y.b = 1, 2 }")
            .unwrap();
        let a = result
            .references
            .iter()
            .find(|r| r.symbol_name == "a")
            .unwrap();
        let b = result
            .references
            .iter()
            .find(|r| r.symbol_name == "b")
            .unwrap();
        assert_eq!(a.usage_kind, UsageKind::Write);
        assert_eq!(b.usage_kind, UsageKind::Write);
    }

    #[test]
    fn test_short_var_declaration_of_field_read_is_not_a_write() {
        let parser = GoParser::new();
        let result = parser
            .parse("package main\nfunc f() { z := x.count }")
            .unwrap();
        assert!(
            result.references.is_empty(),
            "`:=` must not produce a member write reference: {:?}",
            result.references
        );
    }

    #[test]
    fn test_struct_fields_indexed_with_scope_and_visibility() {
        let parser = GoParser::new();
        let source = r#"
package main

type Server struct {
    Port int
    host string
}
"#;
        let result = parser.parse(source).unwrap();
        let fields: Vec<_> = result
            .symbols
            .iter()
            .filter(|s| s.kind == "field")
            .collect();
        assert_eq!(fields.len(), 2, "{:?}", fields);

        let port = fields.iter().find(|s| s.name == "Port").unwrap();
        assert_eq!(port.visibility, "pub");
        assert_eq!(port.scope, Some("Server".to_string()));
        assert_eq!(port.returns, Some("int".to_string()));
        assert_eq!(port.params, None);

        let host = fields.iter().find(|s| s.name == "host").unwrap();
        assert_eq!(host.visibility, "private");
        assert_eq!(host.scope, Some("Server".to_string()));
    }

    #[test]
    fn test_multi_name_field_declaration_emits_one_symbol_per_name() {
        let parser = GoParser::new();
        let source = r#"
package main

type Point struct {
    a, b int
}
"#;
        let result = parser.parse(source).unwrap();
        let fields: Vec<_> = result
            .symbols
            .iter()
            .filter(|s| s.kind == "field")
            .collect();
        let names: Vec<_> = fields.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"a"));
        assert!(names.contains(&"b"));
    }
}
