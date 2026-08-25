use anyhow::{Context, Result};
use tree_sitter::{Language, Parser, Query, QueryCursor, StreamingIterator};

use super::{LanguageParser, ParseResult, is_function_local};
use crate::db::{ParsedImport, ParsedReference, ParsedSymbol, ParsedTypeRelation};

/// Node kinds whose body holds function-local declarations.
const CALLABLE_KINDS: &[&str] = &["function_item", "closure_expression"];

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

    pub fn find_scope(source: &[u8], node: tree_sitter::Node) -> Option<String> {
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

/// The signature of a `function_item`: one entry per parameter (as written,
/// `&self` included) and the return type spelling, if any.
fn signature_of(
    source: &[u8],
    def_node: tree_sitter::Node,
) -> (Option<Vec<String>>, Option<String>) {
    let params = def_node.child_by_field_name("parameters").map(|p| {
        (0..p.named_child_count() as u32)
            .filter_map(|i| p.named_child(i))
            .filter(|c| !c.is_extra())
            .map(|c| text_from(source, c).trim().to_string())
            .collect()
    });
    let returns = def_node
        .child_by_field_name("return_type")
        .map(|r| text_from(source, r).trim().to_string());
    (params, returns)
}

/// The base identifier of an impl's Self type: `Foo` as-is, or `Vec` out of
/// a generic `Vec<T>` -- the bare name a `ParsedSymbol` was recorded under.
fn base_type_name(source: &[u8], node: tree_sitter::Node) -> String {
    match node.kind() {
        "generic_type" => node
            .child_by_field_name("type")
            .map(|n| text_from(source, n))
            .unwrap_or_else(|| text_from(source, node)),
        _ => text_from(source, node),
    }
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

                if is_function_local(def_node, CALLABLE_KINDS) {
                    continue;
                }

                let visibility = Self::extract_visibility(src, def_node);
                let scope = Self::find_scope(src, node);

                let (params, returns) = if kind == "fn" {
                    signature_of(src, def_node)
                } else if kind == "const" {
                    (
                        None,
                        def_node
                            .child_by_field_name("type")
                            .map(|t| text_from(src, t).trim().to_string()),
                    )
                } else {
                    (None, None)
                };

                result.symbols.push(ParsedSymbol {
                    name: sym_name,
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

        // --- Type relations (impl Trait for Type / trait supertraits) ---
        //
        // `impl_item` has an optional `trait` field: present only for `impl
        // Trait for Type` (an inherent `impl Type { .. }` has no `trait`
        // field, so the query below simply doesn't match it -- the required
        // zero rows). Its `type` field is the impl's Self type: a bare
        // `type_identifier` for `impl Trait for Foo`, or a `generic_type`
        // for `impl Trait for Vec<T>`, whose own `type` field holds the
        // base identifier "Vec". That base identifier, not the raw
        // `Vec<T>` text, is what has to match a `ParsedSymbol` name for
        // `index_file_definitions` to join this relation to its symbol.
        //
        // `sub_line` can't be the impl block's own line, since `Type` isn't
        // declared there, only used. It has to be the line of `Type`'s own
        // struct/enum/type declaration -- if that's in this file, it's
        // already sitting in `result.symbols`, and `sub_line` is `Some` of
        // that line. An impl whose Self type is declared elsewhere -- a
        // different file (the common case for a trait impl in Rust; nothing
        // requires a type and its impls to share a file, or even a walk
        // order where the type's file is seen first), a primitive, a tuple,
        // ... -- has no local declaration to give a line, so `sub_line` is
        // `None`. The relation is still emitted either way: `index_file_definitions`
        // resolves a `None` sub by name against the whole index instead of a
        // local line lookup, the same way it already does for an unresolved
        // `super_name`.
        //
        // `trait A: B + C {}` is simpler: the trait's own name is both the
        // declaration and the sub, so `sub_line` is just that node's line.
        let type_rel_query = Query::new(
            &self.language,
            r#"
            (impl_item trait: (_) @impl_trait type: (_) @impl_type) @impl_def
            (trait_item name: (type_identifier) @trait_name bounds: (trait_bounds) @trait_bounds) @trait_def
            "#,
        )
        .context("compiling Rust type relation query")?;

        let type_decl_lines: std::collections::HashMap<&str, i64> = result
            .symbols
            .iter()
            .filter(|s| matches!(s.kind.as_str(), "struct" | "enum" | "type"))
            .map(|s| (s.name.as_str(), s.line))
            .collect();

        let mut cursor = QueryCursor::new();
        let mut matches = cursor.matches(&type_rel_query, root, src);

        while let Some(m) = matches.next() {
            let captures: Vec<_> = m
                .captures
                .iter()
                .map(|c| (type_rel_query.capture_names()[c.index as usize], c.node))
                .collect();

            if let Some(&(_, trait_node)) = captures.iter().find(|(n, _)| *n == "impl_trait") {
                let Some(&(_, type_node)) = captures.iter().find(|(n, _)| *n == "impl_type")
                else {
                    continue;
                };
                let sub_name = base_type_name(src, type_node);
                let sub_line = type_decl_lines.get(sub_name.as_str()).copied();
                result.type_relations.push(ParsedTypeRelation {
                    sub_name,
                    sub_line,
                    super_name: text_from(src, trait_node),
                    kind: "implements".to_string(),
                });
            } else if let Some(&(_, name_node)) = captures.iter().find(|(n, _)| *n == "trait_name")
            {
                let Some(&(_, bounds_node)) = captures.iter().find(|(n, _)| *n == "trait_bounds")
                else {
                    continue;
                };
                let sub_name = text_from(src, name_node);
                let sub_line = Some(name_node.start_position().row as i64 + 1);
                let mut bcursor = bounds_node.walk();
                for bound in bounds_node.named_children(&mut bcursor) {
                    if bound.kind() == "lifetime" {
                        continue;
                    }
                    result.type_relations.push(ParsedTypeRelation {
                        sub_name: sub_name.clone(),
                        sub_line,
                        super_name: text_from(src, bound),
                        kind: "extends".to_string(),
                    });
                }
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
                        names: Vec::new(),
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
                    from_scope: None,
                    qualified: ref_query.capture_names()[c.index as usize] != "call_name",
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

    #[test]
    fn test_function_locals_are_not_symbols() {
        let parser = RustParser::new();
        let source = r#"
pub const TOP: u32 = 1;

pub fn run() -> u32 {
    const INNER: u32 = 2;
    static LOCAL_STATIC: u32 = 3;
    fn helper() -> u32 { 4 }
    TOP + INNER + LOCAL_STATIC + helper()
}

pub struct S;

impl S {
    pub fn method(&self) -> u32 {
        1
    }
}
"#;
        let result = parser.parse(source).unwrap();
        let names: Vec<_> = result.symbols.iter().map(|s| s.name.as_str()).collect();

        assert!(names.contains(&"TOP"));
        assert!(names.contains(&"run"));
        assert!(names.contains(&"S"));
        assert!(names.contains(&"method"));

        for local in ["INNER", "LOCAL_STATIC", "helper"] {
            assert!(!names.contains(&local), "{local} is a function local");
        }
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
    fn test_impl_trait_for_type() {
        let parser = RustParser::new();
        let source = "struct Foo;\nimpl fmt::Display for Foo {}\n";
        let result = parser.parse(source).unwrap();
        assert_eq!(
            relation_tuples(&result.type_relations),
            vec![("Foo", Some(1), "fmt::Display", "implements")]
        );
    }

    #[test]
    fn test_inherent_impl_has_no_relations() {
        let parser = RustParser::new();
        let source = "struct Foo;\nimpl Foo {\n    fn method(&self) {}\n}\n";
        let result = parser.parse(source).unwrap();
        assert!(result.type_relations.is_empty());
    }

    #[test]
    fn test_trait_supertraits() {
        let parser = RustParser::new();
        let result = parser.parse("trait A: B + C {}").unwrap();
        assert_eq!(
            relation_tuples(&result.type_relations),
            vec![
                ("A", Some(1), "B", "extends"),
                ("A", Some(1), "C", "extends"),
            ]
        );
    }

    #[test]
    fn test_generic_supertype_keeps_raw_text() {
        let parser = RustParser::new();
        let source = "struct Foo;\nimpl From<u8> for Foo {}\n";
        let result = parser.parse(source).unwrap();
        assert_eq!(
            relation_tuples(&result.type_relations),
            vec![("Foo", Some(1), "From<u8>", "implements")]
        );
    }

    #[test]
    fn test_generic_self_type_uses_base_identifier() {
        let parser = RustParser::new();
        let source = "struct Vec<T> {\n    item: T,\n}\nimpl Trait for Vec<T> {}\n";
        let result = parser.parse(source).unwrap();
        assert_eq!(
            relation_tuples(&result.type_relations),
            vec![("Vec", Some(1), "Trait", "implements")]
        );
    }

    #[test]
    fn test_fn_params_and_return_round_trip() {
        let parser = RustParser::new();
        let source = "fn add(a: i32, b: i32) -> i32 { a + b }";
        let result = parser.parse(source).unwrap();
        let add = result.symbols.iter().find(|s| s.name == "add").unwrap();
        assert_eq!(
            add.params,
            Some(vec!["a: i32".to_string(), "b: i32".to_string()])
        );
        assert_eq!(add.returns, Some("i32".to_string()));
    }

    #[test]
    fn test_fn_generic_return_round_trips() {
        let parser = RustParser::new();
        let source = "fn make() -> Result<Foo, E> { todo!() }";
        let result = parser.parse(source).unwrap();
        let make = result.symbols.iter().find(|s| s.name == "make").unwrap();
        assert_eq!(make.returns, Some("Result<Foo, E>".to_string()));
    }

    #[test]
    fn test_fn_no_params_gives_empty_vec_not_none() {
        let parser = RustParser::new();
        let source = "fn noop() {}";
        let result = parser.parse(source).unwrap();
        let noop = result.symbols.iter().find(|s| s.name == "noop").unwrap();
        assert_eq!(noop.params, Some(vec![]));
    }

    #[test]
    fn test_fn_no_arrow_gives_none_return() {
        let parser = RustParser::new();
        let source = "fn noop() {}";
        let result = parser.parse(source).unwrap();
        let noop = result.symbols.iter().find(|s| s.name == "noop").unwrap();
        assert_eq!(noop.returns, None);
    }

    #[test]
    fn test_method_receiver_included_as_written() {
        let parser = RustParser::new();
        let source = "struct S;\nimpl S {\n    fn method(&self, x: i32) -> i32 { x }\n}\n";
        let result = parser.parse(source).unwrap();
        let method = result.symbols.iter().find(|s| s.name == "method").unwrap();
        assert_eq!(
            method.params,
            Some(vec!["&self".to_string(), "x: i32".to_string()])
        );
    }

    #[test]
    fn test_non_callable_has_no_signature() {
        let parser = RustParser::new();
        let source = "struct MyStruct { field: i32 }";
        let result = parser.parse(source).unwrap();
        let s = result
            .symbols
            .iter()
            .find(|s| s.name == "MyStruct")
            .unwrap();
        assert_eq!(s.params, None);
        assert_eq!(s.returns, None);
    }

    #[test]
    fn test_const_records_declared_type() {
        let parser = RustParser::new();
        let source = "pub const MAX_SIZE: usize = 100;";
        let result = parser.parse(source).unwrap();
        let c = result
            .symbols
            .iter()
            .find(|s| s.name == "MAX_SIZE")
            .unwrap();
        assert_eq!(c.params, None);
        assert_eq!(c.returns, Some("usize".to_string()));
    }

    #[test]
    fn test_impl_for_undeclared_type_has_no_local_line() {
        // `Unknown` has no local declaration to join a `sub_line` against
        // (e.g. it's declared in another file, or is a primitive/tuple type),
        // so the relation is still produced but with `sub_line: None` --
        // `index_file_definitions` resolves it by name against the whole
        // index instead of a local (name, line) lookup.
        let parser = RustParser::new();
        let result = parser.parse("impl Trait for Unknown {}").unwrap();
        assert_eq!(
            relation_tuples(&result.type_relations),
            vec![("Unknown", None, "Trait", "implements")]
        );
    }
}
