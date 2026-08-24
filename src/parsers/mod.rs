pub mod csharp;
pub mod go;
pub mod python;
pub mod rust_parser;
pub mod swift;
pub mod typescript;

use crate::db::{ParsedImport, ParsedReference, ParsedSymbol};
use anyhow::Result;

/// Result of parsing a single file
#[derive(Debug, Default)]
pub struct ParseResult {
    pub symbols: Vec<ParsedSymbol>,
    pub imports: Vec<ParsedImport>,
    pub references: Vec<ParsedReference>,
}

/// Trait that each language parser implements
pub trait LanguageParser {
    fn parse(&self, source: &str) -> Result<ParseResult>;
}

/// True when the declaration `node` sits inside the body of a function, method
/// or closure, as named by `callable_kinds`. Such definitions are function
/// locals: they have no cross-file meaning, so parsers skip them instead of
/// listing them alongside module- and class-level symbols.
///
/// Pass the declaration node itself (not its name node), so a top-level
/// definition is not mistaken for a local by matching its own kind.
pub fn is_function_local(node: tree_sitter::Node, callable_kinds: &[&str]) -> bool {
    let mut current = node.parent();
    while let Some(parent) = current {
        if callable_kinds.contains(&parent.kind()) {
            return true;
        }
        current = parent.parent();
    }
    false
}

/// Detect language from file extension
pub fn detect_language(path: &str) -> Option<&'static str> {
    let ext = path.rsplit('.').next()?;
    match ext {
        "rs" => Some("rust"),
        "go" => Some("go"),
        "py" => Some("python"),
        "ts" | "tsx" => Some("typescript"),
        "js" | "jsx" => Some("javascript"),
        "swift" => Some("swift"),
        "cs" => Some("csharp"),
        // Markup, not a tree-sitter language: `.xaml` files are indexed so the
        // Roslyn sidecar can attribute their data bindings, and carry no
        // symbols of their own (`get_parser` returns None).
        "xaml" => Some("xaml"),
        _ => None,
    }
}

/// Get the appropriate parser for a language
pub fn get_parser(language: &str) -> Option<Box<dyn LanguageParser>> {
    match language {
        "rust" => Some(Box::new(rust_parser::RustParser::new())),
        "go" => Some(Box::new(go::GoParser::new())),
        "python" => Some(Box::new(python::PythonParser::new())),
        "typescript" | "javascript" => Some(Box::new(typescript::TypeScriptParser::new(language))),
        "swift" => Some(Box::new(swift::SwiftParser::new())),
        "csharp" => Some(Box::new(csharp::CSharpParser::new())),
        _ => None,
    }
}

/// Parse a file given its path and content
#[allow(dead_code)]
pub fn parse_file(path: &str, content: &str) -> Result<Option<(String, ParseResult)>> {
    let language = match detect_language(path) {
        Some(lang) => lang,
        None => return Ok(None),
    };

    let parser = match get_parser(language) {
        Some(p) => p,
        None => return Ok(None),
    };

    let result = parser.parse(content)?;
    Ok(Some((language.to_string(), result)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_detect_language() {
        assert_eq!(detect_language("src/main.rs"), Some("rust"));
        assert_eq!(detect_language("main.go"), Some("go"));
        assert_eq!(detect_language("app.py"), Some("python"));
        assert_eq!(detect_language("index.ts"), Some("typescript"));
        assert_eq!(detect_language("index.tsx"), Some("typescript"));
        assert_eq!(detect_language("app.js"), Some("javascript"));
        assert_eq!(detect_language("app.jsx"), Some("javascript"));
        assert_eq!(detect_language("main.swift"), Some("swift"));
        assert_eq!(detect_language("Program.cs"), Some("csharp"));
        assert_eq!(detect_language("data.json"), None);
        assert_eq!(detect_language("Makefile"), None);
    }
}
