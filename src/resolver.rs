//! Resolve import specifiers to indexed files.
//!
//! Parsers record an import as the raw specifier text (`../util/money`,
//! `crate::db::Database`, `.helpers`). That string is not a stable identity for
//! the imported file: the same file is written differently by importers at
//! different depths, and two unrelated files can share a specifier. Resolving it
//! to a root-relative indexed path here gives the import graph a file -> file
//! edge, which is what makes "who imports this file" answerable from the file's
//! own path.
//!
//! Only languages whose specifiers name a *file* are resolved: TypeScript /
//! JavaScript (relative), Python (relative and dotted-absolute) and Rust
//! (`crate::` / `self::` / `super::` module paths). Go, Swift and C# specifiers
//! name a package or namespace — a directory or an assembly, not one file — so
//! they are left unresolved and keep behaving as raw specifiers.

use std::collections::HashSet;

/// Root-relative path of the file an import specifier names, if that file is
/// indexed. `files` is the set of indexed root-relative paths.
pub fn resolve_import(
    source_path: &str,
    language: &str,
    specifier: &str,
    files: &HashSet<String>,
) -> Option<String> {
    match language {
        "typescript" | "javascript" => resolve_ts(source_path, specifier, files),
        "python" => resolve_python(source_path, specifier, files),
        "rust" => resolve_rust(source_path, specifier, files),
        _ => None,
    }
}

/// Directory part of a root-relative path ("" for a file at the root).
fn dir_of(path: &str) -> &str {
    match path.rfind('/') {
        Some(i) => &path[..i],
        None => "",
    }
}

/// File stem (name without its final extension).
fn stem_of(path: &str) -> &str {
    let name = path.rsplit('/').next().unwrap_or(path);
    match name.rfind('.') {
        Some(i) if i > 0 => &name[..i],
        _ => name,
    }
}

/// Join `rel` onto directory `base`, collapsing `.` and `..`. Returns None if
/// the path climbs above the index root.
fn join_normalized(base: &str, rel: &str) -> Option<String> {
    let mut parts: Vec<&str> = if base.is_empty() {
        Vec::new()
    } else {
        base.split('/').collect()
    };
    for segment in rel.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            other => parts.push(other),
        }
    }
    Some(parts.join("/"))
}

/// First candidate that is an indexed file.
fn first_indexed(candidates: &[String], files: &HashSet<String>) -> Option<String> {
    candidates.iter().find(|c| files.contains(*c)).cloned()
}

const TS_EXTENSIONS: [&str; 6] = ["ts", "tsx", "js", "jsx", "mjs", "cjs"];

/// TS/JS: relative specifiers only. Non-relative ones are package names
/// (`react`) or path aliases, which need tsconfig/package resolution.
fn resolve_ts(source_path: &str, specifier: &str, files: &HashSet<String>) -> Option<String> {
    if !specifier.starts_with("./")
        && !specifier.starts_with("../")
        && specifier != "."
        && specifier != ".."
    {
        return None;
    }
    let base = join_normalized(dir_of(source_path), specifier)?;
    if base.is_empty() {
        return None;
    }

    let mut candidates = vec![base.clone()];
    // `./money.js` in TS ESM sources names money.ts on disk.
    let extensionless = match base.rsplit('/').next().and_then(|n| n.rsplit_once('.')) {
        Some((_, ext)) if TS_EXTENSIONS.contains(&ext) => &base[..base.len() - ext.len() - 1],
        _ => base.as_str(),
    };
    for ext in TS_EXTENSIONS {
        candidates.push(format!("{extensionless}.{ext}"));
    }
    for ext in TS_EXTENSIONS {
        candidates.push(format!("{extensionless}/index.{ext}"));
    }
    first_indexed(&candidates, files)
}

/// Python: `.mod` / `..pkg.mod` relative to the importing file's package, and
/// dotted absolute modules searched from each ancestor directory of the
/// importer (so `src/`-style layouts resolve without reading a config).
fn resolve_python(source_path: &str, specifier: &str, files: &HashSet<String>) -> Option<String> {
    let dots = specifier.chars().take_while(|c| *c == '.').count();
    let rest = specifier[dots..].replace('.', "/");

    if dots > 0 {
        // One dot is the importer's own package (its directory); each extra dot
        // climbs one package.
        let up = "../".repeat(dots - 1);
        let base = join_normalized(dir_of(source_path), &format!("{up}{rest}"))?;
        return module_candidates(&base, files);
    }

    // Absolute dotted module: search from the importer's package root outwards
    // (nearest first, ending at the index root), so `src/`-style layouts
    // resolve without reading a config. The search deliberately starts *above*
    // the importer's own package — Python 3 has no implicit relative imports,
    // so `import json` inside a package means the stdlib module, not the
    // sibling `json.py`.
    let mut dir = package_root(dir_of(source_path), files);
    loop {
        let base = if dir.is_empty() {
            rest.clone()
        } else {
            format!("{dir}/{rest}")
        };
        if let Some(hit) = module_candidates(&base, files) {
            return Some(hit);
        }
        if dir.is_empty() {
            return None;
        }
        dir = dir_of(dir);
    }
}

/// First directory at or above `dir` that is not itself a package (no
/// `__init__.py`) — where an absolute module search starts. A PEP 420
/// namespace package has no `__init__.py` and is indistinguishable from a
/// plain script directory here, so it is treated as the search root.
fn package_root<'a>(dir: &'a str, files: &HashSet<String>) -> &'a str {
    let mut dir = dir;
    while !dir.is_empty() && files.contains(&format!("{dir}/__init__.py")) {
        dir = dir_of(dir);
    }
    dir
}

/// `pkg/mod` -> `pkg/mod.py` or the package's `__init__.py`.
fn module_candidates(base: &str, files: &HashSet<String>) -> Option<String> {
    if base.is_empty() {
        return None;
    }
    first_indexed(
        &[format!("{base}.py"), format!("{base}/__init__.py")],
        files,
    )
}

/// Rust: `crate::`, `self::` and `super::` paths. A use path names an item, not
/// a file, so the longest prefix of module segments that maps to a file wins
/// (`crate::db::Database` -> `src/db.rs`). External crates (`std`, `anyhow`)
/// have no indexed file and stay unresolved.
fn resolve_rust(source_path: &str, specifier: &str, files: &HashSet<String>) -> Option<String> {
    let segments: Vec<&str> = specifier.split("::").collect();
    let (root, rest) = match *segments.first()? {
        "crate" => (crate_root(source_path, files)?, &segments[1..]),
        "self" => (module_dir(source_path), &segments[1..]),
        "super" => {
            let crate_root = crate_root(source_path, files)?;
            let mut dir = module_dir(source_path);
            let mut rest = &segments[1..];
            // `super::super::x` climbs once per leading `super`. The crate root
            // is the ceiling: climbing past it names a module that is not in
            // this crate, and would otherwise match an unrelated file above it.
            loop {
                if dir == crate_root {
                    return None;
                }
                dir = dir_of(&dir).to_string();
                match rest.first() {
                    Some(&"super") => rest = &rest[1..],
                    _ => break,
                }
            }
            (dir, rest)
        }
        _ => return None,
    };

    // Longest module prefix first: `db::Database` must try `db/Database` before
    // settling on `db`.
    for take in (1..=rest.len()).rev() {
        let base = join_normalized(&root, &rest[..take].join("/"))?;
        if let Some(hit) = first_indexed(&[format!("{base}.rs"), format!("{base}/mod.rs")], files) {
            return Some(hit);
        }
    }
    None
}

/// Directory holding the crate root (`main.rs` / `lib.rs`) above `source_path`.
fn crate_root(source_path: &str, files: &HashSet<String>) -> Option<String> {
    let mut dir = dir_of(source_path);
    loop {
        for root in ["main.rs", "lib.rs"] {
            let candidate = if dir.is_empty() {
                root.to_string()
            } else {
                format!("{dir}/{root}")
            };
            if files.contains(&candidate) {
                return Some(dir.to_string());
            }
        }
        if dir.is_empty() {
            return None;
        }
        dir = dir_of(dir);
    }
}

/// Directory `self::` names: a file's own module directory. `mod.rs`, `lib.rs`
/// and `main.rs` are their directory's module; any other file owns a child
/// module directory named after its stem.
fn module_dir(source_path: &str) -> String {
    let dir = dir_of(source_path);
    let stem = stem_of(source_path);
    if matches!(stem, "mod" | "lib" | "main") {
        dir.to_string()
    } else if dir.is_empty() {
        stem.to_string()
    } else {
        format!("{dir}/{stem}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn files(paths: &[&str]) -> HashSet<String> {
        paths.iter().map(|p| p.to_string()).collect()
    }

    #[test]
    fn ts_relative_specifiers_resolve_regardless_of_importer_depth() {
        let idx = files(&[
            "src/util/money.ts",
            "src/domain/cart.ts",
            "src/util/index.ts",
            "src/app.ts",
        ]);
        // Same target, three different specifier spellings.
        for (source, spec) in [
            ("src/domain/cart.ts", "../util/money"),
            ("src/util/format.ts", "./money"),
            ("src/app.ts", "./util/money"),
        ] {
            assert_eq!(
                resolve_import(source, "typescript", spec, &idx).as_deref(),
                Some("src/util/money.ts"),
                "{source} -> {spec}"
            );
        }
    }

    #[test]
    fn ts_extension_index_and_package_specifiers() {
        let idx = files(&["src/util/money.ts", "src/util/index.ts", "src/comp/Btn.tsx"]);
        // TS ESM: `.js` on the wire, `.ts` on disk.
        assert_eq!(
            resolve_import("src/app.ts", "typescript", "./util/money.js", &idx).as_deref(),
            Some("src/util/money.ts")
        );
        // Directory import falls back to index.*
        assert_eq!(
            resolve_import("src/app.ts", "typescript", "./util", &idx).as_deref(),
            Some("src/util/index.ts")
        );
        assert_eq!(
            resolve_import("src/app.ts", "javascript", "./comp/Btn", &idx).as_deref(),
            Some("src/comp/Btn.tsx")
        );
        // Packages and unresolvable aliases stay unresolved.
        assert_eq!(
            resolve_import("src/app.ts", "typescript", "react", &idx),
            None
        );
        assert_eq!(
            resolve_import("src/app.ts", "typescript", "@/util/money", &idx),
            None
        );
        // Climbing above the root resolves to nothing rather than panicking.
        assert_eq!(
            resolve_import("src/app.ts", "typescript", "../../outside", &idx),
            None
        );
    }

    #[test]
    fn python_relative_and_absolute_modules() {
        let idx = files(&[
            "pkg/__init__.py",
            "pkg/util/money.py",
            "pkg/util/__init__.py",
            "pkg/domain/cart.py",
            "src/app/main.py",
        ]);
        assert_eq!(
            resolve_import("pkg/domain/cart.py", "python", "..util.money", &idx).as_deref(),
            Some("pkg/util/money.py")
        );
        assert_eq!(
            resolve_import("pkg/util/format.py", "python", ".money", &idx).as_deref(),
            Some("pkg/util/money.py")
        );
        // `from . import x` inside a package names the package itself.
        assert_eq!(
            resolve_import("pkg/util/format.py", "python", ".", &idx).as_deref(),
            Some("pkg/util/__init__.py")
        );
        // Absolute module found from an ancestor of the importer.
        assert_eq!(
            resolve_import("pkg/domain/cart.py", "python", "pkg.util.money", &idx).as_deref(),
            Some("pkg/util/money.py")
        );
        assert_eq!(
            resolve_import("pkg/domain/cart.py", "python", "os", &idx),
            None
        );
    }

    /// Python 3 has no implicit relative imports: inside a package, `import
    /// json` is the stdlib module even when a sibling `json.py` exists.
    #[test]
    fn python_absolute_import_does_not_shadow_with_a_sibling() {
        let idx = files(&["app/__init__.py", "app/json.py", "app/models.py"]);
        assert_eq!(
            resolve_import("app/models.py", "python", "json", &idx),
            None
        );
        // A sibling *is* reachable the way Python spells it.
        assert_eq!(
            resolve_import("app/models.py", "python", ".json", &idx).as_deref(),
            Some("app/json.py")
        );
        // Outside a package (no __init__.py) the importer's own directory is
        // still the search root — that is where `python app/main.py` looks.
        let flat = files(&["scripts/helpers.py", "scripts/main.py"]);
        assert_eq!(
            resolve_import("scripts/main.py", "python", "helpers", &flat).as_deref(),
            Some("scripts/helpers.py")
        );
    }

    #[test]
    fn ts_bare_parent_specifier_resolves_to_the_parent_index() {
        let idx = files(&["src/index.ts", "src/util/fmt.ts"]);
        assert_eq!(
            resolve_import("src/util/fmt.ts", "typescript", "..", &idx).as_deref(),
            Some("src/index.ts")
        );
    }

    /// Climbing past the crate root is not a resolution — a same-named file
    /// outside the crate must not be matched by accident.
    #[test]
    fn rust_super_above_the_crate_root_resolves_to_nothing() {
        // `db.rs` beside the crate, i.e. outside it, is the trap.
        let idx = files(&["db.rs", "src/main.rs", "src/app.rs", "src/db.rs"]);
        assert_eq!(
            resolve_import("src/app.rs", "rust", "super::db", &idx).as_deref(),
            Some("src/db.rs")
        );
        assert_eq!(
            resolve_import("src/app.rs", "rust", "super::super::db", &idx),
            None
        );
        // The crate root itself has no `super`.
        assert_eq!(
            resolve_import("src/main.rs", "rust", "super::db", &idx),
            None
        );
    }

    #[test]
    fn rust_crate_self_and_super_paths() {
        let idx = files(&[
            "src/main.rs",
            "src/db.rs",
            "src/parsers/mod.rs",
            "src/parsers/typescript.rs",
            "src/parsers/csharp.rs",
            "src/commands/deps.rs",
        ]);
        // Longest module prefix wins: `Database` is an item, not a file.
        assert_eq!(
            resolve_import("src/commands/deps.rs", "rust", "crate::db::Database", &idx).as_deref(),
            Some("src/db.rs")
        );
        assert_eq!(
            resolve_import("src/indexer.rs", "rust", "crate::parsers", &idx).as_deref(),
            Some("src/parsers/mod.rs")
        );
        assert_eq!(
            resolve_import("src/parsers/mod.rs", "rust", "self::typescript", &idx).as_deref(),
            Some("src/parsers/typescript.rs")
        );
        assert_eq!(
            resolve_import("src/parsers/typescript.rs", "rust", "super::csharp", &idx).as_deref(),
            Some("src/parsers/csharp.rs")
        );
        assert_eq!(
            resolve_import(
                "src/commands/deps.rs",
                "rust",
                "std::collections::HashSet",
                &idx
            ),
            None
        );
    }

    #[test]
    fn package_languages_are_left_unresolved() {
        let idx = files(&["pkg/util/money.go", "App/Money.cs", "Sources/Money.swift"]);
        assert_eq!(
            resolve_import("pkg/domain/cart.go", "go", "example.com/pkg/util", &idx),
            None
        );
        assert_eq!(
            resolve_import("App/Cart.cs", "csharp", "App.Money", &idx),
            None
        );
        assert_eq!(
            resolve_import("Sources/Cart.swift", "swift", "Money", &idx),
            None
        );
    }
}
