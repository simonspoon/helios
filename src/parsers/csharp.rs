use anyhow::{Context, Result};
use tree_sitter::{Language, Parser, Query, QueryCursor, StreamingIterator};

use super::{LanguageParser, ParseResult};
use crate::db::{ParsedImport, ParsedReference, ParsedSymbol, ParsedTypeRelation, UsageKind};

pub struct CSharpParser {
    language: Language,
}

impl CSharpParser {
    pub fn new() -> Self {
        Self {
            language: tree_sitter_c_sharp::LANGUAGE.into(),
        }
    }
}

fn text_from(source: &[u8], node: tree_sitter::Node) -> String {
    std::str::from_utf8(&source[node.byte_range()])
        .unwrap_or("")
        .to_string()
}

/// Collect the full text of a qualified_name or identifier node
fn qualified_text(source: &[u8], node: tree_sitter::Node) -> String {
    // For qualified_name, the full text includes dots already
    text_from(source, node)
}

/// A method/constructor's parameters, as written (one entry per parameter).
fn params_of(source: &[u8], def_node: tree_sitter::Node) -> Option<Vec<String>> {
    def_node.child_by_field_name("parameters").map(|p| {
        (0..p.named_child_count() as u32)
            .filter_map(|i| p.named_child(i))
            .filter(|c| !c.is_extra())
            .map(|c| text_from(source, c).trim().to_string())
            .collect()
    })
}

/// Extract visibility from modifier children of a declaration node.
/// C# defaults: members default to private, top-level types default to internal.
fn detect_visibility(source: &[u8], node: tree_sitter::Node) -> String {
    let mut cursor = node.walk();
    if cursor.goto_first_child() {
        loop {
            if cursor.node().kind() == "modifier" {
                let mod_text = text_from(source, cursor.node()).trim().to_string();
                if mod_text.contains("public") {
                    return "pub".to_string();
                } else if mod_text.contains("private")
                    || mod_text.contains("protected")
                    || mod_text.contains("internal")
                {
                    return "private".to_string();
                }
            }
            if !cursor.goto_next_sibling() {
                break;
            }
        }
    }
    // No explicit modifier. Interface members are implicitly public; everything
    // else defaults to private for members, internal (mapped to private) for types.
    if in_interface_body(node) {
        "pub".to_string()
    } else {
        "private".to_string()
    }
}

/// True when `node` is a declaration whose nearest enclosing type is an interface.
fn in_interface_body(node: tree_sitter::Node) -> bool {
    let mut current = node.parent();
    while let Some(parent) = current {
        match parent.kind() {
            "interface_declaration" => return true,
            "class_declaration" | "struct_declaration" | "record_declaration"
            | "enum_declaration" => return false,
            _ => {}
        }
        current = parent.parent();
    }
    false
}

/// Walk up to find enclosing class/struct/namespace name for scope
pub(crate) fn find_scope(source: &[u8], node: tree_sitter::Node) -> Option<String> {
    let mut current = node.parent();
    while let Some(parent) = current {
        match parent.kind() {
            "class_declaration"
            | "struct_declaration"
            | "record_declaration"
            | "interface_declaration"
            | "enum_declaration" => {
                if let Some(name_node) = parent.child_by_field_name("name") {
                    return Some(text_from(source, name_node));
                }
            }
            "namespace_declaration" => {
                if let Some(name_node) = parent.child_by_field_name("name") {
                    return Some(qualified_text(source, name_node));
                }
            }
            _ => {}
        }
        current = parent.parent();
    }
    None
}

impl LanguageParser for CSharpParser {
    fn parse(&self, source: &str) -> Result<ParseResult> {
        let mut parser = Parser::new();
        parser
            .set_language(&self.language)
            .context("setting C# language")?;

        let tree = parser.parse(source, None).context("parsing C# source")?;

        let root = tree.root_node();
        let src = source.as_bytes();
        let mut result = ParseResult::default();

        // --- Symbols ---
        let symbol_query = Query::new(
            &self.language,
            r#"
            (class_declaration name: (identifier) @class_name) @class_def
            (struct_declaration name: (identifier) @struct_name) @struct_def
            (record_declaration name: (identifier) @record_name) @record_def
            (interface_declaration name: (identifier) @interface_name) @interface_def
            (enum_declaration name: (identifier) @enum_name) @enum_def
            (method_declaration name: (identifier) @method_name) @method_def
            (property_declaration name: (identifier) @prop_name) @prop_def
            (constructor_declaration name: (identifier) @ctor_name) @ctor_def
            (field_declaration (variable_declaration type: (_) @field_type (variable_declarator name: (identifier) @field_name))) @field_def
            (enum_member_declaration name: (identifier) @enum_member_name) @enum_member_def
            (namespace_declaration name: (_) @ns_name) @ns_def
            (file_scoped_namespace_declaration name: (_) @fns_name) @fns_def
            "#,
        )
        .context("compiling C# symbol query")?;

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
                    "class_name" => ("class", text_from(src, node)),
                    "struct_name" => ("struct", text_from(src, node)),
                    "record_name" => ("class", text_from(src, node)), // record is a class variant
                    "interface_name" => ("interface", text_from(src, node)),
                    "enum_name" => ("enum", text_from(src, node)),
                    "method_name" => ("fn", text_from(src, node)),
                    // A property is a member that can be written like a field
                    // AND has a body like a method (getter/setter, or an
                    // expression body `=> ...` that `helios flow` needs to
                    // find), so it's neither "fn" nor "field" -- its own kind.
                    "prop_name" => ("property", text_from(src, node)),
                    "ctor_name" => ("fn", text_from(src, node)),
                    "field_name" => ("field", text_from(src, node)),
                    "enum_member_name" => ("field", text_from(src, node)), // enum members, like the Roslyn side
                    "ns_name" => ("mod", qualified_text(src, node)),
                    "fns_name" => ("mod", qualified_text(src, node)),
                    _ => continue,
                };

                // Find the corresponding _def node for visibility
                let def_suffix = match name {
                    "class_name" => "class_def",
                    "struct_name" => "struct_def",
                    "record_name" => "record_def",
                    "interface_name" => "interface_def",
                    "enum_name" => "enum_def",
                    "method_name" => "method_def",
                    "prop_name" => "prop_def",
                    "ctor_name" => "ctor_def",
                    "field_name" => "field_def",
                    "enum_member_name" => "enum_member_def",
                    "ns_name" => "ns_def",
                    "fns_name" => "fns_def",
                    _ => continue,
                };

                let def_node = captures
                    .iter()
                    .find(|(n, _)| *n == def_suffix)
                    .map(|(_, n)| *n)
                    .unwrap_or(node);

                let visibility = if kind == "mod" {
                    // Namespaces don't have visibility modifiers
                    "pub".to_string()
                } else if name == "enum_member_name" {
                    // Enum members can't carry modifiers and are always public,
                    // same as the Roslyn side's DeclaredAccessibility for them.
                    "pub".to_string()
                } else {
                    detect_visibility(src, def_node)
                };

                let scope = find_scope(src, node);

                // Methods and constructors are callable: params always Some,
                // a method's return type (constructors have none). A property
                // or field has a declared type but isn't callable; an enum
                // member has neither.
                let (params, returns) = match name {
                    "method_name" => (
                        params_of(src, def_node),
                        def_node
                            .child_by_field_name("returns")
                            .map(|r| text_from(src, r).trim().to_string()),
                    ),
                    "ctor_name" => (params_of(src, def_node), None),
                    "prop_name" => (
                        None,
                        def_node
                            .child_by_field_name("type")
                            .map(|t| text_from(src, t).trim().to_string()),
                    ),
                    "field_name" => (
                        None,
                        captures
                            .iter()
                            .find(|(n, _)| *n == "field_type")
                            .map(|(_, t)| text_from(src, *t).trim().to_string()),
                    ),
                    _ => (None, None),
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

        // --- Type relations (base_list: extends/implements) ---
        //
        // A class/struct/interface/record's base types (if any) live in a
        // `base_list` child with no field name, so its entries can only be
        // read back as named children (identifier / qualified_name /
        // generic_name), same as the TS/JS parser does for its heritage
        // clauses.
        //
        // C# syntax cannot distinguish a base class from an interface:
        // `class Foo : Bar, IBaz` is ambiguous without semantic (type)
        // information. We approximate with "first entry is the base class,
        // the rest are interfaces" for class/record (which can have at most
        // one base class, always listed first), and "every entry is an
        // interface" for struct/interface (which cannot extend a class).
        // This guess is wrong whenever a class implements an interface
        // without an explicit base class (e.g. `class Foo : IDisposable`
        // is recorded as "extends"). That's acceptable only because
        // `helios init` also runs the Roslyn sidecar, which calls
        // `delete_type_relations_from_language("csharp")` and replaces the
        // whole C# set with the semantically accurate answer; this parser's
        // output is only ever load-bearing for `helios update`, where no
        // better source is available.
        let type_rel_query = Query::new(
            &self.language,
            r#"
            (class_declaration name: (identifier) @class_name (base_list) @base_list) @class_def
            (struct_declaration name: (identifier) @struct_name (base_list) @base_list) @struct_def
            (record_declaration name: (identifier) @record_name (base_list) @base_list) @record_def
            (interface_declaration name: (identifier) @interface_name (base_list) @base_list) @interface_def
            "#,
        )
        .context("compiling C# type relation query")?;

        let mut cursor = QueryCursor::new();
        let mut matches = cursor.matches(&type_rel_query, root, src);

        while let Some(m) = matches.next() {
            let captures: Vec<_> = m
                .captures
                .iter()
                .map(|c| (type_rel_query.capture_names()[c.index as usize], c.node))
                .collect();

            let sub_name = captures
                .iter()
                .find(|(n, _)| n.ends_with("_name"))
                .map(|(_, n)| text_from(src, *n));
            let Some(sub_name) = sub_name else { continue };

            // The C# sub is always the type this base_list is attached to, so
            // it's always declared right here -- `sub_line` is always `Some`.
            let sub_line = captures
                .iter()
                .find(|(n, _)| n.ends_with("_name"))
                .map(|(_, n)| Some(n.start_position().row as i64 + 1))
                .unwrap();

            // Structs and interfaces can't extend a class; every base_list
            // entry there is an interface. Classes and records can have one
            // base class, always listed first if present.
            let struct_or_interface = captures
                .iter()
                .any(|(n, _)| *n == "struct_name" || *n == "interface_name");

            let base_list = captures.iter().find(|(n, _)| *n == "base_list");
            let Some((_, base_list_node)) = base_list else {
                continue;
            };

            let mut bcursor = base_list_node.walk();
            for (idx, super_type) in base_list_node.named_children(&mut bcursor).enumerate() {
                let kind = if !struct_or_interface && idx == 0 {
                    "extends"
                } else {
                    "implements"
                };
                result.type_relations.push(ParsedTypeRelation {
                    sub_name: sub_name.clone(),
                    sub_line,
                    super_name: text_from(src, super_type),
                    kind: kind.to_string(),
                });
            }
        }

        // --- Imports (using directives) ---
        let import_query = Query::new(
            &self.language,
            r#"
            (using_directive (_) @using_target)
            "#,
        )
        .context("compiling C# import query")?;

        let mut cursor = QueryCursor::new();
        let mut matches = cursor.matches(&import_query, root, src);

        while let Some(m) = matches.next() {
            // Collect all captures for this match
            let captured: Vec<_> = m.captures.iter().map(|c| c.node).collect();

            if captured.is_empty() {
                continue;
            }

            // For aliased using (using Alias = Namespace), we get two captures:
            // the alias identifier and the qualified_name.
            // For simple using, we get one capture: identifier or qualified_name.
            let using_node = captured[0].parent().unwrap();
            let has_alias = using_node.child_by_field_name("name").is_some();

            if has_alias {
                // Aliased using: name field is the alias, type child is the target
                let alias_node = using_node.child_by_field_name("name").unwrap();
                let alias_text = text_from(src, alias_node);
                // Find the qualified_name or identifier child that isn't the alias
                let mut wcursor = using_node.walk();
                if wcursor.goto_first_child() {
                    loop {
                        let child = wcursor.node();
                        if (child.kind() == "qualified_name" || child.kind() == "identifier")
                            && child != alias_node
                        {
                            let path = qualified_text(src, child);
                            if !path.is_empty() {
                                result.imports.push(ParsedImport {
                                    import_path: path,
                                    alias: Some(alias_text.clone()),
                                    names: Vec::new(),
                                });
                            }
                            break;
                        }
                        if !wcursor.goto_next_sibling() {
                            break;
                        }
                    }
                }
            } else {
                // Simple using: first non-keyword child is the path
                let node = captured[0];
                if node.kind() == "identifier" || node.kind() == "qualified_name" {
                    let text = qualified_text(src, node);
                    if !text.is_empty() && text != "using" {
                        result.imports.push(ParsedImport {
                            import_path: text,
                            alias: None,
                            names: Vec::new(),
                        });
                    }
                }
            }
        }

        // --- References (invocations, object creation, member writes) ---
        //
        // This is the tree-sitter fallback path only: `helios init` also
        // runs the Roslyn sidecar, which replaces this parser's whole C#
        // reference set. Calls and `new T()` are unqualified or qualified
        // reads of the callee/constructed type, `member: false`. Member
        // write targets -- `x.Count = 1` (Write), `x.Count += 1` /
        // `x.Count++` / `++x.Count` (ReadWrite) -- are captured as
        // `member: true, qualified: true`, which at index time
        // (`resolve_member_candidates` in `src/indexer.rs`) restricts
        // resolution to `"field"`/`"property"` symbols only (see `prop_name`
        // and `field_name` above), recording nothing when no such candidate
        // exists. That's what makes it safe for a member write to never
        // land on an unrelated same-named method elsewhere in the index.
        //
        // C# unifies `=`, `+=`, `??=`, etc. into one `assignment_expression`
        // node, so the plain-vs-compound distinction is read from the
        // `operator` field's text at match time rather than from the node
        // kind. Likewise `postfix_unary_expression` covers `x.Count++` /
        // `x.Count--` *and* the null-forgiving operator `x.Count!` with the
        // same node kind -- the query pins the literal `"++"` / `"--"`
        // token so a `!` never matches and is never mislabeled as a write.
        let ref_query = Query::new(
            &self.language,
            r#"
            (invocation_expression function: (identifier) @call_name)
            (invocation_expression function: (member_access_expression name: (identifier) @member_call))
            (object_creation_expression type: (_) @new_type)
            (assignment_expression left: (member_access_expression name: (identifier) @assign_target)) @assign_expr
            (postfix_unary_expression (member_access_expression name: (identifier) @postfix_target) "++")
            (postfix_unary_expression (member_access_expression name: (identifier) @postfix_target) "--")
            (prefix_unary_expression "++" (member_access_expression name: (identifier) @prefix_target))
            (prefix_unary_expression "--" (member_access_expression name: (identifier) @prefix_target))
            "#,
        )
        .context("compiling C# reference query")?;

        let mut cursor = QueryCursor::new();
        let mut matches = cursor.matches(&ref_query, root, src);

        while let Some(m) = matches.next() {
            let captures: Vec<_> = m
                .captures
                .iter()
                .map(|c| (ref_query.capture_names()[c.index as usize], c.node))
                .collect();

            for &(cap_name, node) in &captures {
                let (text, member, qualified, usage_kind) = match cap_name {
                    "call_name" => (text_from(src, node), false, false, UsageKind::Read),
                    "member_call" => (text_from(src, node), false, true, UsageKind::Read),
                    "new_type" => {
                        // For generic types like List<int>, just get the identifier
                        let text = if node.kind() == "generic_name" {
                            if let Some(id) = node.child_by_field_name("name") {
                                text_from(src, id)
                            } else {
                                // Walk to first identifier child
                                let mut wc = node.walk();
                                if wc.goto_first_child() && wc.node().kind() == "identifier" {
                                    text_from(src, wc.node())
                                } else {
                                    text_from(src, node)
                                }
                            }
                        } else {
                            text_from(src, node)
                        };
                        (text, false, false, UsageKind::Read)
                    }
                    "assign_target" => {
                        // Plain `=` is a Write; any compound form (`+=`,
                        // `??=`, ...) reads the old value too, so ReadWrite.
                        let is_plain_eq = captures
                            .iter()
                            .find(|(n, _)| *n == "assign_expr")
                            .and_then(|(_, e)| e.child_by_field_name("operator"))
                            .map(|op| text_from(src, op) == "=")
                            .unwrap_or(true);
                        let usage_kind = if is_plain_eq {
                            UsageKind::Write
                        } else {
                            UsageKind::ReadWrite
                        };
                        (text_from(src, node), true, true, usage_kind)
                    }
                    "postfix_target" | "prefix_target" => {
                        (text_from(src, node), true, true, UsageKind::ReadWrite)
                    }
                    _ => continue,
                };

                if !text.is_empty() {
                    result.references.push(ParsedReference {
                        symbol_name: text,
                        line: node.start_position().row as i64 + 1,
                        column: node.start_position().column as i64,
                        from_scope: find_scope(src, node),
                        qualified,
                        usage_kind,
                        member,
                    });
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
    fn test_parse_csharp_basics() {
        let parser = CSharpParser::new();
        let source = r#"
using System;
using System.Collections.Generic;
using Alias = System.Text;

namespace MyApp.Models {
    public class Person {
        public string Name { get; set; }
        public int Age { get; set; }

        public Person(string name, int age) {
            Name = name;
            Age = age;
        }

        public void Greet() {
            Console.WriteLine("Hello");
        }

        private int Calculate(int x) {
            return x * 2;
        }
    }

    public interface IRepository<T> {
        T GetById(int id);
        void Delete(int id);
    }

    public enum Status {
        Active,
        Inactive
    }

    public record Point(int X, int Y);

    public struct Vector {
        public double X;
        public double Y;
    }

    internal class Helper {
        public void DoWork() {
            var p = new Person("Alice", 30);
            p.Greet();
        }
    }
}
"#;
        let result = parser.parse(source).unwrap();

        // --- Imports ---
        let imports: Vec<_> = result.imports.iter().map(|i| &i.import_path).collect();
        assert!(
            imports.contains(&&"System".to_string()),
            "Should find System import, got: {:?}",
            imports
        );
        assert!(
            imports.contains(&&"System.Collections.Generic".to_string()),
            "Should find System.Collections.Generic import, got: {:?}",
            imports
        );

        // Check aliased import
        let aliased: Vec<_> = result
            .imports
            .iter()
            .filter(|i| i.alias.is_some())
            .collect();
        assert!(!aliased.is_empty(), "Should find aliased import");
        assert_eq!(
            aliased[0].alias.as_deref(),
            Some("Alias"),
            "Alias name should be 'Alias'"
        );
        assert_eq!(
            aliased[0].import_path, "System.Text",
            "Aliased import path should be 'System.Text'"
        );

        // --- Symbols ---
        let sym_names: Vec<_> = result
            .symbols
            .iter()
            .map(|s| (&s.name, &s.kind, &s.visibility))
            .collect();

        // Namespace
        assert!(
            sym_names
                .iter()
                .any(|(n, k, _)| n.as_str() == "MyApp.Models" && k.as_str() == "mod"),
            "Should find MyApp.Models namespace, got: {:?}",
            sym_names
        );

        // Class
        assert!(
            sym_names.iter().any(|(n, k, v)| n.as_str() == "Person"
                && k.as_str() == "class"
                && v.as_str() == "pub"),
            "Should find public Person class, got: {:?}",
            sym_names
        );

        // Interface
        assert!(
            sym_names
                .iter()
                .any(|(n, k, _)| n.as_str() == "IRepository" && k.as_str() == "interface"),
            "Should find IRepository interface, got: {:?}",
            sym_names
        );

        // Enum
        assert!(
            sym_names
                .iter()
                .any(|(n, k, _)| n.as_str() == "Status" && k.as_str() == "enum"),
            "Should find Status enum, got: {:?}",
            sym_names
        );

        // Record (mapped to class)
        assert!(
            sym_names
                .iter()
                .any(|(n, k, _)| n.as_str() == "Point" && k.as_str() == "class"),
            "Should find Point record as class, got: {:?}",
            sym_names
        );

        // Struct
        assert!(
            sym_names
                .iter()
                .any(|(n, k, _)| n.as_str() == "Vector" && k.as_str() == "struct"),
            "Should find Vector struct, got: {:?}",
            sym_names
        );

        // Methods
        assert!(
            sym_names.iter().any(|(n, k, v)| n.as_str() == "Greet"
                && k.as_str() == "fn"
                && v.as_str() == "pub"),
            "Should find public Greet method, got: {:?}",
            sym_names
        );
        assert!(
            sym_names.iter().any(|(n, k, v)| n.as_str() == "Calculate"
                && k.as_str() == "fn"
                && v.as_str() == "private"),
            "Should find private Calculate method, got: {:?}",
            sym_names
        );

        // Properties are indexed as their own kind
        assert!(
            sym_names
                .iter()
                .any(|(n, k, _)| n.as_str() == "Name" && k.as_str() == "property"),
            "Should find Name property, got: {:?}",
            sym_names
        );

        // Constructor
        let ctor_syms: Vec<_> = result
            .symbols
            .iter()
            .filter(|s| {
                s.kind == "fn" && s.name == "Person" && s.scope.as_deref() == Some("Person")
            })
            .collect();
        assert!(
            !ctor_syms.is_empty(),
            "Should find Person constructor with Person scope, got: {:?}",
            result
                .symbols
                .iter()
                .map(|s| (&s.name, &s.kind, &s.scope))
                .collect::<Vec<_>>()
        );

        // Visibility: internal class
        assert!(
            sym_names.iter().any(|(n, k, v)| n.as_str() == "Helper"
                && k.as_str() == "class"
                && v.as_str() == "private"),
            "Should find internal Helper class mapped to private, got: {:?}",
            sym_names
        );

        // Interface members are implicitly public
        assert!(
            sym_names.iter().any(|(n, k, v)| n.as_str() == "GetById"
                && k.as_str() == "fn"
                && v.as_str() == "pub"),
            "Interface member GetById should be public, got: {:?}",
            sym_names
        );

        // Scope: Greet should be scoped to Person
        let greet = result
            .symbols
            .iter()
            .find(|s| s.name == "Greet")
            .expect("Greet should exist");
        assert_eq!(
            greet.scope.as_deref(),
            Some("Person"),
            "Greet should be scoped to Person"
        );

        // --- References ---
        let ref_names: Vec<_> = result.references.iter().map(|r| &r.symbol_name).collect();
        assert!(
            ref_names.contains(&&"WriteLine".to_string()),
            "Should find WriteLine reference, got: {:?}",
            ref_names
        );
        assert!(
            ref_names.contains(&&"Greet".to_string()),
            "Should find Greet reference, got: {:?}",
            ref_names
        );
        assert!(
            ref_names.contains(&&"Person".to_string()),
            "Should find Person object creation reference, got: {:?}",
            ref_names
        );
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
    fn test_method_params_and_return_round_trip() {
        let parser = CSharpParser::new();
        let source = "class C {\n    public int Add(int a, int b) { return a + b; }\n}\n";
        let result = parser.parse(source).unwrap();
        let add = result.symbols.iter().find(|s| s.name == "Add").unwrap();
        assert_eq!(
            add.params,
            Some(vec!["int a".to_string(), "int b".to_string()])
        );
        assert_eq!(add.returns, Some("int".to_string()));
    }

    #[test]
    fn test_method_generic_return_round_trips() {
        let parser = CSharpParser::new();
        let source = "class C {\n    public List<int> Make() { return null; }\n}\n";
        let result = parser.parse(source).unwrap();
        let make = result.symbols.iter().find(|s| s.name == "Make").unwrap();
        assert_eq!(make.returns, Some("List<int>".to_string()));
    }

    #[test]
    fn test_void_return_stored_as_written() {
        let parser = CSharpParser::new();
        let source = "class C {\n    public void Noop() { }\n}\n";
        let result = parser.parse(source).unwrap();
        let noop = result.symbols.iter().find(|s| s.name == "Noop").unwrap();
        assert_eq!(noop.returns, Some("void".to_string()));
    }

    #[test]
    fn test_method_no_params_gives_empty_vec_not_none() {
        let parser = CSharpParser::new();
        let source = "class C {\n    public void Noop() { }\n}\n";
        let result = parser.parse(source).unwrap();
        let noop = result.symbols.iter().find(|s| s.name == "Noop").unwrap();
        assert_eq!(noop.params, Some(vec![]));
    }

    #[test]
    fn test_constructor_records_params_no_return() {
        let parser = CSharpParser::new();
        let source = "class Person {\n    public Person(string name, int age) { }\n}\n";
        let result = parser.parse(source).unwrap();
        let ctor = result
            .symbols
            .iter()
            .find(|s| s.name == "Person" && s.kind == "fn")
            .unwrap();
        assert_eq!(
            ctor.params,
            Some(vec!["string name".to_string(), "int age".to_string()])
        );
        assert_eq!(ctor.returns, None);
    }

    #[test]
    fn test_non_callable_has_no_signature() {
        let parser = CSharpParser::new();
        let result = parser.parse("class C { }").unwrap();
        let c = result.symbols.iter().find(|s| s.name == "C").unwrap();
        assert_eq!(c.params, None);
        assert_eq!(c.returns, None);
    }

    #[test]
    fn test_property_records_declared_type() {
        let parser = CSharpParser::new();
        let source = "class C {\n    public int Age { get; set; }\n}\n";
        let result = parser.parse(source).unwrap();
        let age = result.symbols.iter().find(|s| s.name == "Age").unwrap();
        assert_eq!(age.params, None);
        assert_eq!(age.returns, Some("int".to_string()));
    }

    #[test]
    fn test_class_extends_and_implements() {
        let parser = CSharpParser::new();
        let result = parser
            .parse("class Circle : ShapeBase, IShape { }")
            .unwrap();
        assert_eq!(
            relation_tuples(&result.type_relations),
            vec![
                ("Circle", Some(1), "ShapeBase", "extends"),
                ("Circle", Some(1), "IShape", "implements")
            ]
        );
    }

    #[test]
    fn test_interface_extends_multiple_bases_all_implements() {
        let parser = CSharpParser::new();
        let result = parser.parse("interface IA : IB, IC { }").unwrap();
        assert_eq!(
            relation_tuples(&result.type_relations),
            vec![
                ("IA", Some(1), "IB", "implements"),
                ("IA", Some(1), "IC", "implements")
            ]
        );
    }

    #[test]
    fn test_struct_base_list_all_implements() {
        let parser = CSharpParser::new();
        let result = parser.parse("struct S : IFoo { }").unwrap();
        assert_eq!(
            relation_tuples(&result.type_relations),
            vec![("S", Some(1), "IFoo", "implements")]
        );
    }

    #[test]
    fn test_record_can_extend() {
        let parser = CSharpParser::new();
        let result = parser
            .parse("record R : BaseRecord, IBar { }")
            .unwrap();
        assert_eq!(
            relation_tuples(&result.type_relations),
            vec![
                ("R", Some(1), "BaseRecord", "extends"),
                ("R", Some(1), "IBar", "implements")
            ]
        );
    }

    #[test]
    fn test_class_with_no_base_list_has_no_relations() {
        let parser = CSharpParser::new();
        let result = parser.parse("class C { }").unwrap();
        assert!(result.type_relations.is_empty());
    }

    #[test]
    fn test_generic_and_qualified_supertype_keep_raw_text() {
        let parser = CSharpParser::new();
        let result = parser
            .parse("class Circle : ShapeBase, System.IDisposable, List<T> { }")
            .unwrap();
        assert_eq!(
            relation_tuples(&result.type_relations),
            vec![
                ("Circle", Some(1), "ShapeBase", "extends"),
                ("Circle", Some(1), "System.IDisposable", "implements"),
                ("Circle", Some(1), "List<T>", "implements")
            ]
        );
    }

    #[test]
    fn test_ordinary_call_is_still_read() {
        let parser = CSharpParser::new();
        let result = parser
            .parse("class C { void f() { x.Method(); } }")
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
        let parser = CSharpParser::new();
        let result = parser
            .parse("class C { void f() { count = 1; } }")
            .unwrap();
        assert!(
            !result.references.iter().any(|r| r.symbol_name == "count"),
            "a bare (unqualified) assignment target must not be captured: {:?}",
            result.references
        );
    }

    #[test]
    fn test_plain_field_indexed_with_scope_and_visibility() {
        let parser = CSharpParser::new();
        let source = "class C {\n    public int Count;\n    private string name;\n}\n";
        let result = parser.parse(source).unwrap();
        let count = result.symbols.iter().find(|s| s.name == "Count").unwrap();
        assert_eq!(count.kind, "field");
        assert_eq!(count.scope.as_deref(), Some("C"));
        assert_eq!(count.visibility, "pub");
        assert_eq!(count.returns, Some("int".to_string()));
        assert_eq!(count.params, None);

        let name = result.symbols.iter().find(|s| s.name == "name").unwrap();
        assert_eq!(name.kind, "field");
        assert_eq!(name.visibility, "private");
    }

    #[test]
    fn test_multi_declarator_field_emits_one_symbol_per_name() {
        let parser = CSharpParser::new();
        let result = parser.parse("class C {\n    public int a, b;\n}\n").unwrap();
        let a = result.symbols.iter().find(|s| s.name == "a").unwrap();
        let b = result.symbols.iter().find(|s| s.name == "b").unwrap();
        assert_eq!(a.kind, "field");
        assert_eq!(b.kind, "field");
        assert_eq!(a.returns, Some("int".to_string()));
        assert_eq!(b.returns, Some("int".to_string()));
    }

    #[test]
    fn test_enum_members_indexed_as_public_fields_scoped_to_enum() {
        let parser = CSharpParser::new();
        let result = parser
            .parse("enum Status {\n    Active,\n    Inactive\n}\n")
            .unwrap();
        let active = result.symbols.iter().find(|s| s.name == "Active").unwrap();
        assert_eq!(active.kind, "field");
        assert_eq!(active.visibility, "pub");
        assert_eq!(active.scope.as_deref(), Some("Status"));
    }

    #[test]
    fn test_plain_assignment_to_member_is_write() {
        let parser = CSharpParser::new();
        let result = parser
            .parse("class C { void f() { x.Count = 1; } }")
            .unwrap();
        let r = result
            .references
            .iter()
            .find(|r| r.symbol_name == "Count")
            .unwrap();
        assert_eq!(r.usage_kind, UsageKind::Write);
        assert!(r.member);
        assert!(r.qualified);
    }

    #[test]
    fn test_compound_assignment_and_incr_decr_are_readwrite() {
        let parser = CSharpParser::new();
        let result = parser
            .parse(
                "class C { void f() { x.Count += 1; x.Count++; ++x.Count; x.Count--; --x.Count; } }",
            )
            .unwrap();
        let writes: Vec<_> = result
            .references
            .iter()
            .filter(|r| r.symbol_name == "Count")
            .collect();
        assert_eq!(writes.len(), 5, "got: {:?}", result.references);
        for r in writes {
            assert_eq!(r.usage_kind, UsageKind::ReadWrite);
            assert!(r.member);
        }
    }

    #[test]
    fn test_null_forgiving_operator_is_not_a_write() {
        let parser = CSharpParser::new();
        let result = parser
            .parse("class C { void f() { var y = x.Count!; } }")
            .unwrap();
        assert!(
            !result
                .references
                .iter()
                .any(|r| r.symbol_name == "Count" && r.member),
            "null-forgiving `x.Count!` must not be captured as a member write: {:?}",
            result.references
        );
    }
}
