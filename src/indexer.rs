use anyhow::{Context, Result};
use ignore::WalkBuilder;
use sha2::{Digest, Sha256};
use std::path::Path;

use crate::db::{Database, SymbolRecord};
use crate::parsers;
use crate::sidecar::AnalyzeOutput;

/// Choose which candidate definitions a reference resolves to.
///
/// Reference resolution is scope-aware: if the reference site's enclosing scope
/// (`from_scope`, e.g. its C# class or namespace) is known and one or more
/// candidates were declared in that same scope, only those in-scope definitions
/// are linked — a call to a method defined in its own class resolves to that
/// class's definition rather than a same-named method elsewhere.
///
/// When there is no scope context, or no candidate matches the scope, we fall
/// back to linking ALL candidates. This preserves the behavior for unambiguous
/// names (a single candidate) and for genuinely ambiguous names that cannot be
/// disambiguated by scope (link to every candidate rather than guessing).
fn resolve_reference_candidates(
    from_scope: Option<&str>,
    candidates: &[(SymbolRecord, String)],
) -> Vec<i64> {
    if let Some(scope) = from_scope {
        let in_scope: Vec<i64> = candidates
            .iter()
            .filter(|(sym, _)| sym.scope.as_deref() == Some(scope))
            .map(|(sym, _)| sym.id)
            .collect();
        if !in_scope.is_empty() {
            return in_scope;
        }
    }
    candidates.iter().map(|(sym, _)| sym.id).collect()
}

/// Index all supported files in a directory.
///
/// `semantic_csharp` is true when the Roslyn sidecar ran for this walk: `.cs`
/// reference resolution is then deferred to `ingest_semantic`, which runs
/// after the walk (symbols and imports are still inserted here either way).
pub fn index_full(db: &Database, root: &Path, semantic_csharp: bool) -> Result<IndexStats> {
    let mut stats = IndexStats::default();

    let walker = WalkBuilder::new(root)
        .hidden(true) // respect hidden files
        .git_ignore(true) // respect .gitignore
        .git_global(true)
        .git_exclude(true)
        .build();

    for entry in walker {
        let entry = entry.context("walking directory")?;
        let path = entry.path();

        if !path.is_file() {
            continue;
        }

        let rel_path = path
            .strip_prefix(root)
            .unwrap_or(path)
            .to_string_lossy()
            .to_string();

        // Skip .helios directory
        if rel_path.starts_with(".helios") {
            continue;
        }

        if let Some(language) = parsers::detect_language(&rel_path) {
            match index_file(db, path, &rel_path, language, semantic_csharp) {
                Ok(file_stats) => {
                    stats.files_indexed += 1;
                    stats.symbols_found += file_stats.symbols;
                    stats.imports_found += file_stats.imports;
                }
                Err(e) => {
                    eprintln!("warning: failed to index {}: {}", rel_path, e);
                    stats.files_errored += 1;
                }
            }
        }
    }

    Ok(stats)
}

/// Index a single file
pub fn index_file(
    db: &Database,
    abs_path: &Path,
    rel_path: &str,
    language: &str,
    semantic_csharp: bool,
) -> Result<FileStats> {
    let content =
        std::fs::read_to_string(abs_path).with_context(|| format!("reading {}", rel_path))?;

    let content_hash = {
        let mut hasher = Sha256::new();
        hasher.update(content.as_bytes());
        format!("{:x}", hasher.finalize())
    };

    // Check if file has changed
    if let Some(existing) = db.get_file_by_path(rel_path)?
        && existing.content_hash == content_hash
    {
        // Already indexed with same content
        return Ok(FileStats {
            symbols: 0,
            imports: 0,
        });
    }

    // Upsert file record
    let file_id = db.upsert_file(rel_path, &content_hash, language)?;

    // Clear old data for this file
    db.clear_file_data(file_id)?;

    // Parse
    let parser = match parsers::get_parser(language) {
        Some(p) => p,
        None => {
            return Ok(FileStats {
                symbols: 0,
                imports: 0,
            });
        }
    };

    let parse_result = parser
        .parse(&content)
        .with_context(|| format!("parsing {}", rel_path))?;

    // Insert symbols
    let mut symbol_count = 0;
    for sym in &parse_result.symbols {
        db.insert_symbol(file_id, sym)?;
        symbol_count += 1;
    }

    // Insert imports
    let mut import_count = 0;
    for imp in &parse_result.imports {
        db.insert_import(file_id, imp)?;
        import_count += 1;
    }

    // Insert references — resolve to known symbols. Scope-aware: when the
    // reference site's enclosing scope is known and one or more candidates share
    // it, we link only those in-scope definitions (same class/namespace wins over
    // same-named definitions elsewhere). When no scope context is available or no
    // candidate matches, we fall back to linking ALL candidates — preserving the
    // ambiguous-name behavior so `helios deps` never silently drops a usage.
    //
    // In semantic mode `.cs` references are skipped here: the Roslyn sidecar
    // output is ingested after the walk with exact DocId resolution (P3-M4).
    if !(semantic_csharp && language == "csharp") {
        for reference in &parse_result.references {
            let candidates = db.find_symbol_by_name(&reference.symbol_name)?;
            let symbol_ids =
                resolve_reference_candidates(reference.from_scope.as_deref(), &candidates);
            db.insert_references(&symbol_ids, file_id, reference.line, reference.column)?;
        }
    }

    Ok(FileStats {
        symbols: symbol_count,
        imports: import_count,
    })
}

/// Re-index only changed files (incremental)
pub fn index_incremental(
    db: &Database,
    root: &Path,
    modified: &[String],
    deleted: &[String],
) -> Result<IndexStats> {
    let mut stats = IndexStats::default();

    // Remove deleted files
    for path in deleted {
        db.delete_file(path)?;
        stats.files_deleted += 1;
    }

    // Re-index modified/added files
    for rel_path in modified {
        let abs_path = root.join(rel_path);
        if !abs_path.is_file() {
            continue;
        }

        if let Some(language) = parsers::detect_language(rel_path) {
            // `update` keeps the tree-sitter path for `.cs` (W1) — syntactic.
            match index_file(db, &abs_path, rel_path, language, false) {
                Ok(file_stats) => {
                    stats.files_indexed += 1;
                    stats.symbols_found += file_stats.symbols;
                    stats.imports_found += file_stats.imports;
                }
                Err(e) => {
                    eprintln!("warning: failed to index {}: {}", rel_path, e);
                    stats.files_errored += 1;
                }
            }
        }
    }

    Ok(stats)
}

/// Post-walk C# resolution step (design §4.3). Runs once per `helios init`.
///
/// When the sidecar ran (`semantic = Some`):
/// 1. **Stamp** (P3-M3): match each `Definition` to the symbol row
///    `index_file` inserted for `(file_id, line, name)` and set its `docid`.
///    Definitions themselves stay inserted by the existing symbol path — the
///    sidecar only stamps identity. No match (a kind the tree-sitter path
///    does not index, or a file outside the walk) → skip silently.
/// 2. **Map**: build the in-memory `docid → symbol_id` map from stamped rows.
/// 3. **Reset**: delete all references sourced from `.cs` files — in semantic
///    mode the sidecar output is the entire `.cs` reference set, which also
///    clears stale tree-sitter rows on hash-unchanged files (arch §6.5).
/// 4. **Insert** (P3-M4): one exact reference row per mapped symbol id for
///    each `Reference` with `is_definition == false` (P3-M6). Unstamped
///    docids (framework/NuGet) are dropped silently (P3-M5). The wire is
///    1-based columns, storage is 0-based — convert at insert.
///
/// Either way, record which resolver produced the current `.cs` references
/// in the `csharp_resolver` metadata row (P3-M7).
pub fn ingest_semantic(db: &Database, semantic: Option<&AnalyzeOutput>) -> Result<()> {
    let output = match semantic {
        Some(output) => output,
        None => return db.set_metadata("csharp_resolver", "treesitter"),
    };

    for def in &output.definitions {
        let Some(file) = db.get_file_by_path(&def.file)? else {
            continue;
        };
        db.stamp_symbol_docid(file.id, def.start_line, &def.name, &def.docid)?;
    }

    let docid_map = db.docid_symbol_map()?;
    db.delete_references_from_language("csharp")?;

    for reference in &output.references {
        if reference.is_definition {
            continue;
        }
        let Some(symbol_ids) = docid_map.get(&reference.docid) else {
            continue;
        };
        let Some(file) = db.get_file_by_path(&reference.file)? else {
            continue;
        };
        db.insert_references(symbol_ids, file.id, reference.line, reference.col - 1)?;
    }

    db.set_metadata("csharp_resolver", "roslyn")
}

#[derive(Debug, Default)]
pub struct IndexStats {
    pub files_indexed: usize,
    pub files_errored: usize,
    pub files_deleted: usize,
    pub symbols_found: usize,
    pub imports_found: usize,
}

pub(crate) struct FileStats {
    symbols: usize,
    imports: usize,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::ParsedSymbol;
    use crate::sidecar::{Definition, Reference};

    fn add_file(db: &Database, path: &str, language: &str) -> i64 {
        db.upsert_file(path, "hash", language).unwrap()
    }

    fn add_symbol(db: &Database, file_id: i64, name: &str, line: i64, scope: &str) -> i64 {
        db.insert_symbol(
            file_id,
            &ParsedSymbol {
                name: name.to_string(),
                kind: "fn".to_string(),
                line,
                column: 4,
                end_line: line + 2,
                visibility: "pub".to_string(),
                scope: Some(scope.to_string()),
            },
        )
        .unwrap()
    }

    fn def(docid: &str, name: &str, file: &str, start_line: i64) -> Definition {
        Definition {
            docid: docid.to_string(),
            name: name.to_string(),
            file: file.to_string(),
            start_line,
        }
    }

    fn wire_ref(docid: &str, file: &str, line: i64, col: i64, is_definition: bool) -> Reference {
        Reference {
            docid: docid.to_string(),
            file: file.to_string(),
            line,
            col,
            is_definition,
        }
    }

    /// All reference rows as (symbol_id, file_id, line, column), ordered.
    fn all_refs(db: &Database) -> Vec<(i64, i64, i64, i64)> {
        let mut stmt = db
            .conn
            .prepare(
                "SELECT symbol_id, file_id, line, column FROM references_
                 ORDER BY symbol_id, file_id, line, column",
            )
            .unwrap();
        let rows = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
            .unwrap();
        rows.collect::<Result<Vec<_>, _>>().unwrap()
    }

    /// Two classes each declaring `Save()`: stamping matches by
    /// (file_id, line, name) and each usage resolves to the correct symbol
    /// via the docid map — no `.first()`, no scope heuristic (P3-M3, P3-M4).
    /// Also proves the 1-based wire column is stored 0-based.
    #[test]
    fn ambiguous_name_resolved_exactly_by_docid() {
        let db = Database::open_in_memory().unwrap();
        let file_a = add_file(&db, "A.cs", "csharp");
        let file_b = add_file(&db, "B.cs", "csharp");
        let file_main = add_file(&db, "Program.cs", "csharp");
        let save_a = add_symbol(&db, file_a, "Save", 5, "A");
        let save_b = add_symbol(&db, file_b, "Save", 7, "B");

        let output = AnalyzeOutput {
            definitions: vec![
                def("M:App.A.Save", "Save", "A.cs", 5),
                def("M:App.B.Save", "Save", "B.cs", 7),
            ],
            references: vec![
                wire_ref("M:App.A.Save", "Program.cs", 10, 9, false),
                wire_ref("M:App.B.Save", "Program.cs", 11, 13, false),
            ],
        };
        ingest_semantic(&db, Some(&output)).unwrap();

        // Stamped by (file_id, line, name) (P3-M3)
        let map = db.docid_symbol_map().unwrap();
        assert_eq!(map.get("M:App.A.Save"), Some(&vec![save_a]));
        assert_eq!(map.get("M:App.B.Save"), Some(&vec![save_b]));

        // Each usage attributed to the right definition; wire col - 1 stored
        assert_eq!(
            all_refs(&db),
            vec![
                (save_a, file_main, 10, 8),
                (save_b, file_main, 11, 12),
            ]
        );
        assert_eq!(
            db.get_metadata("csharp_resolver").unwrap().as_deref(),
            Some("roslyn")
        );
    }

    /// References to docids that were never stamped (framework/NuGet, e.g.
    /// Console.WriteLine) are dropped silently — zero dangling rows (P3-M5).
    /// Definitions that match no indexed symbol row are skipped silently.
    #[test]
    fn unstamped_docids_dropped_silently() {
        let db = Database::open_in_memory().unwrap();
        let file = add_file(&db, "Program.cs", "csharp");
        add_symbol(&db, file, "Main", 3, "Program");

        let output = AnalyzeOutput {
            // no symbol row at (Program.cs, 99, Nope) — stamp skips silently
            definitions: vec![def("M:App.Nope", "Nope", "Program.cs", 99)],
            references: vec![wire_ref("M:System.Console.WriteLine", "Program.cs", 4, 9, false)],
        };
        ingest_semantic(&db, Some(&output)).unwrap();

        assert!(db.docid_symbol_map().unwrap().is_empty());
        assert!(all_refs(&db).is_empty());
    }

    /// Records with is_definition == true are not inserted as references (P3-M6).
    #[test]
    fn definition_sites_not_inserted_as_references() {
        let db = Database::open_in_memory().unwrap();
        let file = add_file(&db, "A.cs", "csharp");
        add_symbol(&db, file, "Save", 5, "A");

        let output = AnalyzeOutput {
            definitions: vec![def("M:App.A.Save", "Save", "A.cs", 5)],
            references: vec![wire_ref("M:App.A.Save", "A.cs", 5, 17, true)],
        };
        ingest_semantic(&db, Some(&output)).unwrap();

        assert!(all_refs(&db).is_empty());
    }

    /// The `.cs` reference reset clears stale tree-sitter rows (including on
    /// hash-unchanged files) but leaves other languages' rows untouched.
    #[test]
    fn cs_reference_reset_spares_non_cs_rows() {
        let db = Database::open_in_memory().unwrap();
        let cs_file = add_file(&db, "A.cs", "csharp");
        let py_file = add_file(&db, "util.py", "python");
        let save = add_symbol(&db, cs_file, "Save", 5, "A");
        let helper = add_symbol(&db, py_file, "helper", 2, "util");

        // Stale tree-sitter rows: one sourced from a .cs file, one from .py
        db.insert_reference(save, cs_file, 20, 3).unwrap();
        db.insert_reference(helper, py_file, 9, 0).unwrap();

        let output = AnalyzeOutput {
            definitions: vec![def("M:App.A.Save", "Save", "A.cs", 5)],
            references: vec![wire_ref("M:App.A.Save", "A.cs", 12, 9, false)],
        };
        ingest_semantic(&db, Some(&output)).unwrap();

        // .cs stale row replaced by the exact one; python row survives
        assert_eq!(
            all_refs(&db),
            vec![(save, cs_file, 12, 8), (helper, py_file, 9, 0)]
        );
    }

    /// A docid stamped onto multiple rows (partial types) maps to all of
    /// them, and a reference inserts one row per mapped id.
    #[test]
    fn partial_type_docid_maps_to_all_rows() {
        let db = Database::open_in_memory().unwrap();
        let file_a = add_file(&db, "A.Part1.cs", "csharp");
        let file_b = add_file(&db, "A.Part2.cs", "csharp");
        let part1 = add_symbol(&db, file_a, "A", 1, "App");
        let part2 = add_symbol(&db, file_b, "A", 1, "App");

        let output = AnalyzeOutput {
            definitions: vec![
                def("T:App.A", "A", "A.Part1.cs", 1),
                def("T:App.A", "A", "A.Part2.cs", 1),
            ],
            references: vec![wire_ref("T:App.A", "A.Part1.cs", 8, 5, false)],
        };
        ingest_semantic(&db, Some(&output)).unwrap();

        assert_eq!(
            all_refs(&db),
            vec![(part1, file_a, 8, 4), (part2, file_a, 8, 4)]
        );
    }

    /// Provenance row: "roslyn" when the semantic ingest ran, "treesitter"
    /// on fallback (P3-M7).
    #[test]
    fn provenance_row_written_for_both_legs() {
        let db = Database::open_in_memory().unwrap();
        ingest_semantic(&db, None).unwrap();
        assert_eq!(
            db.get_metadata("csharp_resolver").unwrap().as_deref(),
            Some("treesitter")
        );

        let output = AnalyzeOutput::default();
        ingest_semantic(&db, Some(&output)).unwrap();
        assert_eq!(
            db.get_metadata("csharp_resolver").unwrap().as_deref(),
            Some("roslyn")
        );
    }

    /// In semantic mode `index_file` defers `.cs` reference resolution to the
    /// ingest (no tree-sitter rows), while symbols are still inserted by the
    /// existing path; syntactic mode is unchanged.
    #[test]
    fn semantic_mode_skips_cs_reference_block_in_index_file() {
        let source = "public class Person {\n    public void Greet() { Helper(); }\n    public void Helper() { }\n}\n";
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("Person.cs");
        std::fs::write(&path, source).unwrap();

        let db = Database::open_in_memory().unwrap();
        index_file(&db, &path, "Person.cs", "csharp", true).unwrap();
        assert!(db.symbol_count().unwrap() > 0, "symbols still inserted");
        assert!(all_refs(&db).is_empty(), "cs references deferred to ingest");

        let db = Database::open_in_memory().unwrap();
        index_file(&db, &path, "Person.cs", "csharp", false).unwrap();
        assert!(
            !all_refs(&db).is_empty(),
            "syntactic mode keeps tree-sitter references"
        );
    }
}
