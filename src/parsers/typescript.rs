use anyhow::{Context, Result};
use tree_sitter::{Language, Parser, Query, QueryCursor, StreamingIterator};

use super::{LanguageParser, ParseResult, is_function_local};
use crate::db::{ParsedImport, ParsedReference, ParsedSymbol, ParsedTypeRelation};

/// Node kinds whose body holds function-local declarations.
const CALLABLE_KINDS: &[&str] = &[
    "function_declaration",
    "generator_function_declaration",
    "generator_function",
    "function_expression",
    "function",
    "arrow_function",
    "method_definition",
    "class_static_block",
];

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

/// The local names an import statement binds: `import Money, { formatMoney,
/// tax as vat } from './money'` binds `Money`, `formatMoney` and `vat`.
///
/// The *local* name is what the file's own references spell, so that is what is
/// recorded. An aliased import therefore no longer matches the definition's
/// name, and attribution falls back to the ambiguous-name behaviour instead of
/// pointing the usage at the wrong definition. A bare side-effect import
/// (`import './polyfill'`) binds nothing.
fn import_names(source: &[u8], import: tree_sitter::Node) -> Vec<String> {
    let mut names = Vec::new();
    let mut cursor = import.walk();
    let clause = import
        .children(&mut cursor)
        .find(|n| n.kind() == "import_clause");
    let Some(clause) = clause else {
        return names;
    };

    let mut clause_cursor = clause.walk();
    for child in clause.named_children(&mut clause_cursor) {
        match child.kind() {
            // `import Money from './money'`
            "identifier" => names.push(text_from(source, child)),
            // `import * as money from './money'` — the namespace binding, not
            // the members reached through it.
            "namespace_import" => {
                let mut ns_cursor = child.walk();
                names.extend(
                    child
                        .named_children(&mut ns_cursor)
                        .filter(|n| n.kind() == "identifier")
                        .map(|n| text_from(source, n)),
                );
            }
            // `import { formatMoney, tax as vat } from './money'`
            "named_imports" => {
                let mut specs = child.walk();
                for spec in child.named_children(&mut specs) {
                    if spec.kind() != "import_specifier" {
                        continue;
                    }
                    if let Some(local) = spec
                        .child_by_field_name("alias")
                        .or_else(|| spec.child_by_field_name("name"))
                    {
                        names.push(text_from(source, local));
                    }
                }
            }
            _ => {}
        }
    }
    names
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

                // Use def_node for end_line, falling back to parent node for methods
                let end_node = def_node.or_else(|| node.parent()).unwrap_or(node);
                if is_function_local(end_node, CALLABLE_KINDS) {
                    continue;
                }

                let exported = def_node.is_some_and(is_exported);
                let visibility = if exported { "pub" } else { "private" };

                let scope = if name == "method_name" {
                    find_class_scope(src, node)
                } else {
                    None
                };

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

        // --- Type relations (extends/implements) ---
        //
        // `class_heritage` bundles an optional `extends_clause` (single
        // supertype, field `value`) and an optional `implements_clause`
        // (one or more supertypes as plain named children — the grammar
        // gives `implements` no field name to hang them off). Interface
        // heritage is a different shape again: `extends_type_clause` holds
        // its supertypes as repeated `type` fields, which tree-sitter's API
        // only lets us read back as named children, same as `implements`.
        // JS has `class_heritage`/`extends_clause` but no `implements` or
        // interfaces, so the query below only asks for `implements_clause`
        // and `interface_declaration` in the TypeScript grammar.
        let type_rel_query_str = if self.is_typescript {
            r#"
            (class_declaration name: (type_identifier) @class_name (class_heritage) @class_heritage) @class_def
            (interface_declaration name: (type_identifier) @iface_name (extends_type_clause) @iface_extends) @iface_def
            "#
        } else {
            r#"
            (class_declaration name: (identifier) @class_name (class_heritage) @class_heritage) @class_def
            "#
        };

        let type_rel_query = Query::new(&self.language, type_rel_query_str)
            .context("compiling TS/JS type relation query")?;

        let mut cursor = QueryCursor::new();
        let mut matches = cursor.matches(&type_rel_query, root, src);

        while let Some(m) = matches.next() {
            let captures: Vec<_> = m
                .captures
                .iter()
                .map(|c| (type_rel_query.capture_names()[c.index as usize], c.node))
                .collect();

            let def_node = captures
                .iter()
                .find(|(n, _)| n.ends_with("_def"))
                .map(|(_, n)| *n);
            let Some(def_node) = def_node else { continue };
            if is_function_local(def_node, CALLABLE_KINDS) {
                continue;
            }

            let sub_name = captures
                .iter()
                .find(|(n, _)| n.ends_with("_name"))
                .map(|(_, n)| text_from(src, *n));
            let Some(sub_name) = sub_name else { continue };
            // The sub is always the class/interface this heritage clause is
            // attached to, so it's always declared right here -- `sub_line`
            // is always `Some`.
            let sub_line = captures
                .iter()
                .find(|(n, _)| n.ends_with("_name"))
                .map(|(_, n)| Some(n.start_position().row as i64 + 1))
                .unwrap();

            for &(name, node) in &captures {
                match name {
                    "class_heritage" => {
                        let mut hcursor = node.walk();
                        for clause in node.named_children(&mut hcursor) {
                            match clause.kind() {
                                "extends_clause" => {
                                    if let Some(value) = clause.child_by_field_name("value") {
                                        let super_name = std::str::from_utf8(
                                            &src[value.start_byte()..clause.end_byte()],
                                        )
                                        .unwrap_or("")
                                        .to_string();
                                        result.type_relations.push(ParsedTypeRelation {
                                            sub_name: sub_name.clone(),
                                            sub_line,
                                            super_name,
                                            kind: "extends".to_string(),
                                        });
                                    }
                                }
                                "implements_clause" => {
                                    let mut icursor = clause.walk();
                                    for super_type in clause.named_children(&mut icursor) {
                                        result.type_relations.push(ParsedTypeRelation {
                                            sub_name: sub_name.clone(),
                                            sub_line,
                                            super_name: text_from(src, super_type),
                                            kind: "implements".to_string(),
                                        });
                                    }
                                }
                                // The JavaScript grammar has no `extends_clause`
                                // wrapper: `class_heritage` holds the supertype
                                // expression directly (`class C extends B {}`).
                                _ => {
                                    result.type_relations.push(ParsedTypeRelation {
                                        sub_name: sub_name.clone(),
                                        sub_line,
                                        super_name: text_from(src, clause),
                                        kind: "extends".to_string(),
                                    });
                                }
                            }
                        }
                    }
                    "iface_extends" => {
                        let mut ecursor = node.walk();
                        for super_type in node.named_children(&mut ecursor) {
                            result.type_relations.push(ParsedTypeRelation {
                                sub_name: sub_name.clone(),
                                sub_line,
                                super_name: text_from(src, super_type),
                                kind: "extends".to_string(),
                            });
                        }
                    }
                    _ => {}
                }
            }
        }

        // --- Imports ---
        let import_query = Query::new(
            &self.language,
            r#"
            (import_statement) @import
            "#,
        )
        .context("compiling TS/JS import query")?;

        let mut cursor = QueryCursor::new();
        let mut matches = cursor.matches(&import_query, root, src);

        while let Some(m) = matches.next() {
            for c in m.captures {
                let Some(source_node) = c.node.child_by_field_name("source") else {
                    continue;
                };
                let path = text_from(src, source_node)
                    .trim_matches(|c: char| c == '\'' || c == '"')
                    .to_string();
                if !path.is_empty() {
                    result.imports.push(ParsedImport {
                        import_path: path,
                        alias: None,
                        names: import_names(src, c.node),
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
                    from_scope: None,
                    // `money.formatMoney()` names a member of some receiver,
                    // not the bare name an import binds.
                    qualified: ref_query.capture_names()[c.index as usize] == "method_call",
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

    /// The local names an import binds — what the file's own references spell.
    #[test]
    fn import_names_are_the_local_bindings() {
        let parser = TypeScriptParser::new("typescript");
        let source = r#"
import { formatMoney, tax as vat } from './money';
import Money from './money-class';
import * as fmt from './fmt';
import './polyfill';
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
        assert_eq!(names("./money"), vec!["formatMoney", "vat"]);
        assert_eq!(names("./money-class"), vec!["Money"]);
        assert_eq!(names("./fmt"), vec!["fmt"]);
        assert!(names("./polyfill").is_empty());
    }

    #[test]
    fn test_function_locals_are_not_symbols() {
        let parser = TypeScriptParser::new("typescript");
        let source = r#"
export const CURRENCY = "USD";

export function formatMoney(cents: number): string {
    const major = Math.floor(cents / 100);
    function helper() { return 1; }
    return `${major}${helper()}`;
}

export class Wallet {
    add(cents: number): void {
        const next = cents;
        console.log(next);
    }
}

export const render = (x: number) => {
    const out = x * 2;
    return out;
};

export const gen = function* () {
    const yielded = 1;
    yield yielded;
};

export class Registry {
    static {
        const staticLocal = 1;
        console.log(staticLocal);
    }
}
"#;
        let result = parser.parse(source).unwrap();
        let names: Vec<_> = result.symbols.iter().map(|s| s.name.as_str()).collect();

        assert!(names.contains(&"CURRENCY"));
        assert!(names.contains(&"formatMoney"));
        assert!(names.contains(&"Wallet"));
        assert!(names.contains(&"add"));
        assert!(names.contains(&"render"));
        assert!(names.contains(&"gen"));
        assert!(names.contains(&"Registry"));

        for local in ["major", "helper", "next", "out", "yielded", "staticLocal"] {
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
    fn test_class_extends() {
        let parser = TypeScriptParser::new("typescript");
        let result = parser.parse("class C extends B {}").unwrap();
        assert_eq!(
            relation_tuples(&result.type_relations),
            vec![("C", Some(1), "B", "extends")]
        );
    }

    #[test]
    fn test_class_implements_multiple() {
        let parser = TypeScriptParser::new("typescript");
        let result = parser.parse("class C implements I, J {}").unwrap();
        assert_eq!(
            relation_tuples(&result.type_relations),
            vec![("C", Some(1), "I", "implements"), ("C", Some(1), "J", "implements")]
        );
    }

    #[test]
    fn test_class_extends_and_implements() {
        let parser = TypeScriptParser::new("typescript");
        let result = parser
            .parse("class C extends B implements I {}")
            .unwrap();
        assert_eq!(
            relation_tuples(&result.type_relations),
            vec![("C", Some(1), "B", "extends"), ("C", Some(1), "I", "implements")]
        );
    }

    #[test]
    fn test_interface_extends_multiple() {
        let parser = TypeScriptParser::new("typescript");
        let result = parser.parse("interface I extends K, L {}").unwrap();
        assert_eq!(
            relation_tuples(&result.type_relations),
            vec![("I", Some(1), "K", "extends"), ("I", Some(1), "L", "extends")]
        );
    }

    #[test]
    fn test_class_with_no_heritage_has_no_relations() {
        let parser = TypeScriptParser::new("typescript");
        let result = parser.parse("class C {}").unwrap();
        assert!(result.type_relations.is_empty());
    }

    #[test]
    fn test_generic_supertype_keeps_raw_text() {
        let parser = TypeScriptParser::new("typescript");
        let result = parser.parse("class C extends Base<T> {}").unwrap();
        assert_eq!(
            relation_tuples(&result.type_relations),
            vec![("C", Some(1), "Base<T>", "extends")]
        );
    }

    #[test]
    fn test_javascript_class_extends() {
        let parser = TypeScriptParser::new("javascript");
        let result = parser.parse("class C extends B {}").unwrap();
        assert_eq!(
            relation_tuples(&result.type_relations),
            vec![("C", Some(1), "B", "extends")]
        );
    }
}
