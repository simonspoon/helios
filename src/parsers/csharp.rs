use anyhow::{Context, Result};
use tree_sitter::{Language, Parser, Query, QueryCursor, StreamingIterator};

use super::{LanguageParser, ParseResult};
use crate::db::{ParsedImport, ParsedReference, ParsedSymbol};

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
    // Default: private for members, internal (mapped to private) for types
    "private".to_string()
}

/// Walk up to find enclosing class/struct/namespace name for scope
fn find_scope(source: &[u8], node: tree_sitter::Node) -> Option<String> {
    let mut current = node.parent();
    while let Some(parent) = current {
        match parent.kind() {
            "class_declaration"
            | "struct_declaration"
            | "record_declaration"
            | "interface_declaration" => {
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
                    "prop_name" => ("fn", text_from(src, node)), // properties as fn (getter/setter)
                    "ctor_name" => ("fn", text_from(src, node)),
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
                } else {
                    detect_visibility(src, def_node)
                };

                let scope = find_scope(src, node);

                result.symbols.push(ParsedSymbol {
                    name: sym_text,
                    kind: kind.to_string(),
                    line: node.start_position().row as i64 + 1,
                    column: node.start_position().column as i64,
                    visibility,
                    scope,
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
                        });
                    }
                }
            }
        }

        // --- References (invocations and object creation) ---
        let ref_query = Query::new(
            &self.language,
            r#"
            (invocation_expression function: (identifier) @call_name)
            (invocation_expression function: (member_access_expression name: (identifier) @member_call))
            (object_creation_expression type: (_) @new_type)
            "#,
        )
        .context("compiling C# reference query")?;

        let mut cursor = QueryCursor::new();
        let mut matches = cursor.matches(&ref_query, root, src);

        while let Some(m) = matches.next() {
            for c in m.captures {
                let cap_name = ref_query.capture_names()[c.index as usize];
                let text = match cap_name {
                    "call_name" | "member_call" => text_from(src, c.node),
                    "new_type" => {
                        // For generic types like List<int>, just get the identifier
                        if c.node.kind() == "generic_name" {
                            if let Some(id) = c.node.child_by_field_name("name") {
                                text_from(src, id)
                            } else {
                                // Walk to first identifier child
                                let mut wc = c.node.walk();
                                if wc.goto_first_child() && wc.node().kind() == "identifier" {
                                    text_from(src, wc.node())
                                } else {
                                    text_from(src, c.node)
                                }
                            }
                        } else {
                            text_from(src, c.node)
                        }
                    }
                    _ => continue,
                };

                if !text.is_empty() {
                    result.references.push(ParsedReference {
                        symbol_name: text,
                        line: c.node.start_position().row as i64 + 1,
                        column: c.node.start_position().column as i64,
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

        // Properties
        assert!(
            sym_names
                .iter()
                .any(|(n, k, _)| n.as_str() == "Name" && k.as_str() == "fn"),
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
}
