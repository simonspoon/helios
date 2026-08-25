use anyhow::{Context, Result};
use tree_sitter::{Language, Parser, Query, QueryCursor, StreamingIterator};

use super::{LanguageParser, ParseResult, is_function_local};
use crate::db::{ParsedImport, ParsedReference, ParsedSymbol, ParsedTypeRelation, UsageKind};

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

/// Each parameter's source spelling, verbatim (type, optionality, default,
/// rest, destructuring pattern -- whatever is written). `def_node` is the
/// function_declaration/method_definition/arrow_function itself.
fn callable_params(source: &[u8], def_node: tree_sitter::Node) -> Option<Vec<String>> {
    if let Some(params_node) = def_node.child_by_field_name("parameters") {
        let mut cursor = params_node.walk();
        return Some(
            params_node
                .named_children(&mut cursor)
                .map(|p| text_from(source, p))
                .collect(),
        );
    }
    // Arrow functions may bind a single bare parameter instead of a
    // parenthesized list: `x => x * 2`.
    if let Some(param_node) = def_node.child_by_field_name("parameter") {
        return Some(vec![text_from(source, param_node)]);
    }
    Some(Vec::new())
}

/// The declared return type's source spelling, with the leading `:` and
/// surrounding whitespace stripped.
fn callable_returns(source: &[u8], def_node: tree_sitter::Node) -> Option<String> {
    def_node.child_by_field_name("return_type").map(|n| {
        text_from(source, n)
            .trim_start_matches(':')
            .trim()
            .to_string()
    })
}

/// A variable/const declarator's own type annotation, if any -- the `type`
/// field is on the declarator, not on the enclosing lexical_declaration.
fn declared_type(source: &[u8], declarator_node: tree_sitter::Node) -> Option<String> {
    declarator_node.child_by_field_name("type").map(|n| {
        text_from(source, n)
            .trim_start_matches(':')
            .trim()
            .to_string()
    })
}

/// Walks up to the enclosing class or interface's name. Used for methods and
/// for fields/property signatures (a `property_signature` inside a bare
/// `object_type`, e.g. a type alias, has no such ancestor and gets `None`).
fn find_class_scope(source: &[u8], node: tree_sitter::Node) -> Option<String> {
    let mut current = node.parent();
    while let Some(parent) = current {
        if matches!(
            parent.kind(),
            "class_declaration" | "class" | "interface_declaration"
        ) && let Some(name_node) = parent.child_by_field_name("name")
        {
            return Some(text_from(source, name_node));
        }
        current = parent.parent();
    }
    None
}

/// A field-like definition node's visibility, matching TS's `private`/
/// `protected` accessibility modifiers to `"private"` and everything else
/// (including explicit `public` and JS, which has no such modifiers) to
/// `"pub"` -- the same two-value convention the rest of this file uses.
fn field_visibility(source: &[u8], field_def: tree_sitter::Node) -> &'static str {
    let mut cursor = field_def.walk();
    let restricted = field_def.named_children(&mut cursor).any(|c| {
        c.kind() == "accessibility_modifier"
            && matches!(text_from(source, c).as_str(), "private" | "protected")
    });
    if restricted { "private" } else { "pub" }
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
                (public_field_definition name: (property_identifier) @field_name) @field_def
                (public_field_definition name: (private_property_identifier) @field_name) @field_def
                (property_signature name: (property_identifier) @field_name) @field_def
                (required_parameter (accessibility_modifier) pattern: (identifier) @ctor_field_name) @ctor_field_def
                (required_parameter "readonly" pattern: (identifier) @ctor_field_name) @ctor_field_def
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
                (field_definition property: (property_identifier) @field_name) @field_def
                (field_definition property: (private_property_identifier) @field_name) @field_def
                "#,
            )
        };

        let symbol_query =
            Query::new(&self.language, &query_str).context("compiling TS/JS symbol query")?;

        let mut cursor = QueryCursor::new();
        let mut matches = cursor.matches(&symbol_query, root, src);

        // A constructor parameter property can carry both `readonly` and an
        // accessibility modifier (`private readonly a: T`), which are two
        // separate query alternatives above -- dedup on the identifier
        // node so such a parameter is only indexed once.
        let mut seen_ctor_fields = std::collections::HashSet::new();

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
                    "field_name" => "field",
                    "ctor_field_name" => {
                        if !seen_ctor_fields.insert(node.id()) {
                            continue;
                        }
                        "field"
                    }
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

                // A constructor parameter property's def node (a
                // `required_parameter`) sits inside the constructor's own
                // `method_definition`, which is itself a `CALLABLE_KINDS`
                // member -- so a locality check from there would always
                // read as "inside a callable" and wrongly exclude every
                // parameter property. Check the constructor's own ancestry
                // instead, same as `method_name` does by starting from
                // `method_definition` rather than from inside it.
                let local_check_node = if name == "ctor_field_name" {
                    let mut cur = end_node.parent();
                    while let Some(p) = cur {
                        if p.kind() == "method_definition" {
                            break;
                        }
                        cur = p.parent();
                    }
                    cur.unwrap_or(end_node)
                } else {
                    end_node
                };
                if is_function_local(local_check_node, CALLABLE_KINDS) {
                    continue;
                }

                let visibility = if kind == "field" {
                    field_visibility(src, end_node)
                } else {
                    let exported = def_node.is_some_and(is_exported);
                    if exported { "pub" } else { "private" }
                };

                let scope = if matches!(name, "method_name" | "field_name" | "ctor_field_name") {
                    find_class_scope(src, node)
                } else {
                    None
                };

                // `fn`/method callables get their parameter list and return
                // type; a const/var declarator or field gets its own type
                // annotation (this parser never reclassifies an arrow-bound
                // const as `fn`, so an arrow function's own signature is not
                // surfaced here); everything else has neither.
                let (params, returns) = match name {
                    "fn_name" => {
                        let fn_node = def_node.unwrap_or(node);
                        (
                            callable_params(src, fn_node),
                            callable_returns(src, fn_node),
                        )
                    }
                    "method_name" => {
                        let fn_node = node.parent().unwrap_or(node);
                        (
                            callable_params(src, fn_node),
                            callable_returns(src, fn_node),
                        )
                    }
                    "const_name" | "var_name" | "field_name" | "ctor_field_name" => {
                        let declarator_node = node.parent().unwrap_or(node);
                        (None, declared_type(src, declarator_node))
                    }
                    _ => (None, None),
                };

                result.symbols.push(ParsedSymbol {
                    name: sym_text,
                    kind: kind.to_string(),
                    line: node.start_position().row as i64 + 1,
                    column: node.start_position().column as i64,
                    end_line: end_node.end_position().row as i64 + 1,
                    visibility: visibility.to_string(),
                    scope,
                    params,
                    returns,
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
        //
        // Member/field writes (`x.count = 1`, `x.count += 1`, `x.count++`)
        // are captured as `member: true` usages, now that class fields and
        // interface/object-type property signatures are indexed as
        // `field`-kind symbols above -- `member: true` narrows resolution
        // to those candidates at index time (see `src/indexer.rs`), so a
        // write with no matching field candidate is correctly dropped
        // rather than landing on an unrelated same-named symbol. A bare
        // assignment target (`count = 1`) is not a member access and stays
        // uncaptured, same as before.
        let ref_query = Query::new(
            &self.language,
            r#"
            (call_expression function: (identifier) @call_name)
            (call_expression function: (member_expression property: (property_identifier) @method_call))
            (new_expression constructor: (identifier) @new_call)
            (assignment_expression left: (member_expression property: (property_identifier) @member_write))
            (augmented_assignment_expression left: (member_expression property: (property_identifier) @member_readwrite))
            (update_expression argument: (member_expression property: (property_identifier) @member_readwrite))
            "#,
        )
        .context("compiling TS/JS reference query")?;

        let mut cursor = QueryCursor::new();
        let mut matches = cursor.matches(&ref_query, root, src);

        while let Some(m) = matches.next() {
            for c in m.captures {
                let text = text_from(src, c.node);
                let capture_name = ref_query.capture_names()[c.index as usize];
                let member = matches!(capture_name, "member_write" | "member_readwrite");
                result.references.push(ParsedReference {
                    symbol_name: text,
                    line: c.node.start_position().row as i64 + 1,
                    column: c.node.start_position().column as i64,
                    from_scope: None,
                    // `money.formatMoney()`/`x.count` name a member of some
                    // receiver, not the bare name an import binds.
                    qualified: capture_name == "method_call" || member,
                    usage_kind: match capture_name {
                        "member_write" => UsageKind::Write,
                        "member_readwrite" => UsageKind::ReadWrite,
                        // Calls / `new T()`, which read the callee/constructed type.
                        _ => UsageKind::Read,
                    },
                    member,
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
        let parser = TypeScriptParser::new("typescript");
        let source = r#"
export function createServer(config: Config, opts?: Options, ...rest: string[]): Promise<Server> {
    return new Server(config);
}

export function noop(): void {}

function untyped() {}

export const handler = (a: number, b: number): number => a + b;

class Server {
    start(host: string): void {}
    stop(): void {}
}

export const PORT: number = 3000;
const label = "x";

export interface Config {
    host: string;
}
"#;
        let result = parser.parse(source).unwrap();
        let sym = |name: &str| result.symbols.iter().find(|s| s.name == name).unwrap();

        assert_eq!(
            sym("createServer").params,
            Some(vec![
                "config: Config".to_string(),
                "opts?: Options".to_string(),
                "...rest: string[]".to_string(),
            ])
        );
        assert_eq!(
            sym("createServer").returns,
            Some("Promise<Server>".to_string())
        );

        assert_eq!(sym("noop").params, Some(vec![]));
        assert_eq!(sym("untyped").returns, None);

        assert_eq!(
            sym("handler").params,
            None,
            "an arrow bound to a const stays kind `const`, so its own params aren't surfaced"
        );

        assert_eq!(sym("start").params, Some(vec!["host: string".to_string()]));
        assert_eq!(sym("start").returns, Some("void".to_string()));
        assert_eq!(sym("stop").params, Some(vec![]));

        assert_eq!(sym("PORT").returns, Some("number".to_string()));
        assert_eq!(sym("PORT").params, None);
        assert_eq!(sym("label").returns, None);

        let iface = sym("Config");
        assert_eq!(iface.params, None);
        assert_eq!(iface.returns, None);
    }

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
            vec![
                ("C", Some(1), "I", "implements"),
                ("C", Some(1), "J", "implements")
            ]
        );
    }

    #[test]
    fn test_class_extends_and_implements() {
        let parser = TypeScriptParser::new("typescript");
        let result = parser.parse("class C extends B implements I {}").unwrap();
        assert_eq!(
            relation_tuples(&result.type_relations),
            vec![
                ("C", Some(1), "B", "extends"),
                ("C", Some(1), "I", "implements")
            ]
        );
    }

    #[test]
    fn test_interface_extends_multiple() {
        let parser = TypeScriptParser::new("typescript");
        let result = parser.parse("interface I extends K, L {}").unwrap();
        assert_eq!(
            relation_tuples(&result.type_relations),
            vec![
                ("I", Some(1), "K", "extends"),
                ("I", Some(1), "L", "extends")
            ]
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

    #[test]
    fn test_ordinary_call_is_still_read() {
        let parser = TypeScriptParser::new("typescript");
        let result = parser.parse("x.method();").unwrap();
        let r = result
            .references
            .iter()
            .find(|r| r.symbol_name == "method")
            .unwrap();
        assert_eq!(r.usage_kind, UsageKind::Read);
    }

    #[test]
    fn test_bare_assignment_target_emits_no_reference() {
        let parser = TypeScriptParser::new("typescript");
        let result = parser.parse("count = 1;").unwrap();
        assert!(
            result.references.is_empty(),
            "a bare assignment target is not a member access: {:?}",
            result.references
        );
    }

    #[test]
    fn test_member_write_is_captured() {
        let parser = TypeScriptParser::new("typescript");
        let result = parser.parse("x.count = 1;").unwrap();
        let r = result
            .references
            .iter()
            .find(|r| r.symbol_name == "count")
            .unwrap();
        assert_eq!(r.usage_kind, UsageKind::Write);
        assert!(r.member);
        assert!(r.qualified);
    }

    #[test]
    fn test_member_compound_assignment_and_increment_are_readwrite() {
        let parser = TypeScriptParser::new("typescript");
        let result = parser
            .parse("x.count += 1; x.count++; ++x.count; x.count--; --x.count;")
            .unwrap();
        let count_refs: Vec<_> = result
            .references
            .iter()
            .filter(|r| r.symbol_name == "count")
            .collect();
        assert_eq!(count_refs.len(), 5);
        for r in count_refs {
            assert_eq!(r.usage_kind, UsageKind::ReadWrite);
            assert!(r.member);
            assert!(r.qualified);
        }
    }

    #[test]
    fn test_class_field_declaration_emits_no_reference() {
        let parser = TypeScriptParser::new("typescript");
        let result = parser.parse("class C { count = 0; }").unwrap();
        assert!(
            result.references.is_empty(),
            "a field declaration with an initializer is not a usage: {:?}",
            result.references
        );
    }

    #[test]
    fn test_class_fields_are_indexed_with_scope_and_visibility() {
        let parser = TypeScriptParser::new("typescript");
        let source = r#"
class Wallet {
    readonly balance: number;
    label = "USD";
    private secret: string;
    protected guard?: boolean;
    public open: boolean;
    #hidden: number = 0;
}
"#;
        let result = parser.parse(source).unwrap();
        let field = |name: &str| {
            result
                .symbols
                .iter()
                .find(|s| s.name == name && s.kind == "field")
                .unwrap_or_else(|| panic!("no field symbol named {name}"))
        };

        let balance = field("balance");
        assert_eq!(balance.scope, Some("Wallet".to_string()));
        assert_eq!(balance.visibility, "pub");
        assert_eq!(balance.returns, Some("number".to_string()));
        assert_eq!(balance.params, None);

        assert_eq!(field("label").visibility, "pub");
        assert_eq!(field("secret").visibility, "private");
        assert_eq!(field("guard").visibility, "private");
        assert_eq!(field("open").visibility, "pub");
        assert_eq!(field("#hidden").scope, Some("Wallet".to_string()));
    }

    #[test]
    fn test_interface_property_signatures_are_indexed_as_fields() {
        let parser = TypeScriptParser::new("typescript");
        let source = r#"
interface Config {
    host: string;
    port?: number;
}
"#;
        let result = parser.parse(source).unwrap();
        let host = result
            .symbols
            .iter()
            .find(|s| s.name == "host" && s.kind == "field")
            .unwrap();
        assert_eq!(host.scope, Some("Config".to_string()));
        assert_eq!(host.visibility, "pub");
        assert_eq!(host.returns, Some("string".to_string()));

        let port = result
            .symbols
            .iter()
            .find(|s| s.name == "port" && s.kind == "field")
            .unwrap();
        assert_eq!(port.returns, Some("number".to_string()));
    }

    #[test]
    fn test_constructor_parameter_properties_are_indexed_as_fields() {
        let parser = TypeScriptParser::new("typescript");
        let source = r#"
class Wallet {
    constructor(private readonly balance: number, public label: string, plain: string) {}
}
"#;
        let result = parser.parse(source).unwrap();
        let fields: Vec<_> = result
            .symbols
            .iter()
            .filter(|s| s.kind == "field")
            .collect();
        assert_eq!(
            fields.len(),
            2,
            "plain (non-property) parameters are not fields: {fields:?}"
        );

        let balance = fields.iter().find(|s| s.name == "balance").unwrap();
        assert_eq!(balance.scope, Some("Wallet".to_string()));
        assert_eq!(balance.visibility, "private");
        assert_eq!(balance.returns, Some("number".to_string()));

        let label = fields.iter().find(|s| s.name == "label").unwrap();
        assert_eq!(label.visibility, "pub");
    }

    #[test]
    fn test_javascript_class_fields_are_indexed() {
        let parser = TypeScriptParser::new("javascript");
        let source = r#"
class C {
    x = 5;
    #y = 1;
}
"#;
        let result = parser.parse(source).unwrap();
        let names: Vec<_> = result
            .symbols
            .iter()
            .filter(|s| s.kind == "field")
            .map(|s| s.name.as_str())
            .collect();
        assert!(names.contains(&"x"));
        assert!(names.contains(&"#y"));
    }
}
