use anyhow::{Context, Result};
use ignore::WalkBuilder;
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::path::Path;

use crate::db::{Database, SymbolRecord};
use crate::parsers;
use crate::resolver;
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

/// The walker every helios pass uses: hidden files and gitignored paths skipped.
fn walk(root: &Path) -> ignore::Walk {
    WalkBuilder::new(root)
        .hidden(true) // respect hidden files
        .git_ignore(true) // respect .gitignore
        .git_global(true)
        .git_exclude(true)
        .build()
}

/// Root-relative, '/'-separated path of a walked entry — the one path
/// vocabulary shared by the database, the sidecar file list, and the
/// helper's own RelativePath output.
fn walk_rel_path(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

/// Root-relative paths of the C# files a full index of `root` will cover.
/// Passed to the Roslyn sidecar so it reports on exactly the indexed file set
/// instead of guessing which paths the walk skips. Walk errors propagate —
/// a silently shorter list would drop those files' references (the sidecar
/// output replaces the entire `.cs` reference set).
pub fn indexed_csharp_files(root: &Path) -> Result<Vec<String>> {
    let mut files = Vec::new();
    for entry in walk(root) {
        let entry = entry.context("walking directory")?;
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let rel_path = walk_rel_path(root, path);
        if !rel_path.starts_with(".helios") && parsers::detect_language(&rel_path) == Some("csharp")
        {
            files.push(rel_path);
        }
    }
    Ok(files)
}

/// Index all supported files in a directory.
///
/// Two passes over the file set. The first inserts each file's symbols and
/// imports; the second resolves its references. They are separate because a
/// reference resolves against the definitions already in the database, so a
/// one-pass walk records nothing for a file it reaches before the file that
/// defines the name it uses — and the walk order is the filesystem's, so which
/// usages survived varied by machine. The second pass runs once every
/// definition is in, and re-parses rather than holding every file's references
/// in memory for the length of the walk.
///
/// `cs_snapshot` is the file list the Roslyn sidecar analyzed, when it ran for
/// this walk (`None` = tree-sitter mode): `.cs` reference resolution is then
/// deferred to `ingest_semantic`, which runs after the walk (symbols and
/// imports are still inserted here either way). `.cs` files the walk sees that
/// are missing from the snapshot — created between the snapshot and the walk —
/// are reported in `IndexStats::cs_missing_from_snapshot`; they carry no
/// semantic references until the next init.
pub fn index_full(
    db: &Database,
    root: &Path,
    cs_snapshot: Option<&[String]>,
) -> Result<IndexStats> {
    let mut stats = IndexStats::default();
    let semantic_csharp = cs_snapshot.is_some();
    let snapshot: Option<HashSet<&str>> =
        cs_snapshot.map(|files| files.iter().map(String::as_str).collect());

    // Files this walk parsed, for the reference pass below. Only those: a file
    // whose content hash was unchanged keeps the reference rows it already has,
    // and re-inserting them would duplicate every one.
    let mut parsed: Vec<(std::path::PathBuf, String, &'static str)> = Vec::new();

    let walker = walk(root);

    for entry in walker {
        let entry = entry.context("walking directory")?;
        let path = entry.path();

        if !path.is_file() {
            continue;
        }

        let rel_path = walk_rel_path(root, path);

        // Skip .helios directory
        if rel_path.starts_with(".helios") {
            continue;
        }

        if let Some(language) = parsers::detect_language(&rel_path) {
            if language == "csharp"
                && let Some(snapshot) = &snapshot
                && !snapshot.contains(rel_path.as_str())
            {
                stats.cs_missing_from_snapshot.push(rel_path.clone());
            }
            match index_file_definitions(db, path, &rel_path, language) {
                Ok(file_stats) => {
                    stats.files_indexed += 1;
                    stats.symbols_found += file_stats.symbols;
                    stats.imports_found += file_stats.imports;
                    if language == "csharp" && file_stats.reparsed {
                        stats.cs_changed += 1;
                    }
                    if file_stats.reparsed && !(semantic_csharp && language == "csharp") {
                        parsed.push((path.to_path_buf(), rel_path.clone(), language));
                    }
                }
                Err(e) => {
                    eprintln!("warning: failed to index {}: {}", rel_path, e);
                    stats.files_errored += 1;
                }
            }
        }
    }

    for (abs_path, rel_path, language) in &parsed {
        if let Err(e) = index_file_references(db, abs_path, rel_path, language) {
            eprintln!("warning: failed to index references in {}: {}", rel_path, e);
        }
    }

    Ok(stats)
}

/// The digest stored in `files.content_hash` — the index's record of what it
/// last parsed for a path.
fn content_hash(content: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(content.as_bytes());
    format!("{:x}", hasher.finalize())
}

/// The changed files that would actually change the index.
///
/// `git diff` answers a different question than helios needs: it reports every
/// changed path in the repo, most of which have no parser, and it reports an
/// uncommitted edit against HEAD forever — including one `update` has already
/// indexed. Both inflate staleness and cost a redundant re-index. So filter to
/// paths helios indexes, then drop any whose on-disk content already matches
/// the hash recorded at index time; a deletion only counts if the path is in
/// the index to begin with.
pub fn stale_files(
    db: &Database,
    root: &Path,
    since_commit: &str,
) -> Result<(Vec<String>, Vec<String>)> {
    let (modified, deleted) = crate::git::changed_files(since_commit, root)?;

    let mut stale_modified = Vec::new();
    for rel_path in modified {
        if parsers::detect_language(&rel_path).is_none() {
            continue;
        }
        let abs_path = root.join(&rel_path);
        if !abs_path.is_file() {
            continue;
        }
        // Unreadable (binary, permissions) — leave it to `index_file`, which
        // reports the error rather than silently dropping the file.
        let indexed_hash = db.get_file_by_path(&rel_path)?.map(|f| f.content_hash);
        let current_hash = std::fs::read_to_string(&abs_path)
            .ok()
            .map(|c| content_hash(&c));
        if current_hash.is_some() && current_hash == indexed_hash {
            continue;
        }
        stale_modified.push(rel_path);
    }

    let mut stale_deleted = Vec::new();
    for rel_path in deleted {
        if db.get_file_by_path(&rel_path)?.is_some() {
            stale_deleted.push(rel_path);
        }
    }

    Ok((stale_modified, stale_deleted))
}

/// Index a single file: its definitions, then its references.
///
/// The incremental path only — every definition in the rest of the repo is
/// already indexed, so one pass resolves correctly. A full index resolves
/// references in a separate pass (see `index_full`).
pub fn index_file(
    db: &Database,
    abs_path: &Path,
    rel_path: &str,
    language: &str,
    semantic_csharp: bool,
) -> Result<FileStats> {
    let stats = index_file_definitions(db, abs_path, rel_path, language)?;
    // In semantic mode `.cs` references come from the Roslyn sidecar, ingested
    // after the walk with exact DocId resolution (P3-M4).
    if stats.reparsed && !(semantic_csharp && language == "csharp") {
        index_file_references(db, abs_path, rel_path, language)?;
    }
    Ok(stats)
}

/// Parse a file and insert its symbols and imports, replacing whatever the
/// index held for that path. A file whose content hash is unchanged is left
/// alone (`reparsed: false`).
fn index_file_definitions(
    db: &Database,
    abs_path: &Path,
    rel_path: &str,
    language: &str,
) -> Result<FileStats> {
    let content =
        std::fs::read_to_string(abs_path).with_context(|| format!("reading {}", rel_path))?;

    let content_hash = content_hash(&content);

    // Check if file has changed
    if let Some(existing) = db.get_file_by_path(rel_path)?
        && existing.content_hash == content_hash
    {
        // Already indexed with same content
        return Ok(FileStats {
            symbols: 0,
            imports: 0,
            reparsed: false,
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
                reparsed: true,
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

    Ok(FileStats {
        symbols: symbol_count,
        imports: import_count,
        reparsed: true,
    })
}

/// Re-parse a file the index already holds and record its references against
/// the definitions currently in the database.
///
/// Resolution is scope-aware: when the reference site's enclosing scope is known
/// and one or more candidates share it, only those in-scope definitions are
/// linked (same class/namespace wins over same-named definitions elsewhere).
/// When no scope context is available or no candidate matches, ALL candidates
/// are linked — preserving the ambiguous-name behavior so `helios deps` never
/// silently drops a usage.
fn index_file_references(
    db: &Database,
    abs_path: &Path,
    rel_path: &str,
    language: &str,
) -> Result<()> {
    let Some(parser) = parsers::get_parser(language) else {
        return Ok(());
    };
    let Some(file) = db.get_file_by_path(rel_path)? else {
        return Ok(());
    };
    let content =
        std::fs::read_to_string(abs_path).with_context(|| format!("reading {}", rel_path))?;
    let parse_result = parser
        .parse(&content)
        .with_context(|| format!("parsing {}", rel_path))?;

    for reference in &parse_result.references {
        let candidates = db.find_symbol_by_name(&reference.symbol_name)?;
        let symbol_ids = resolve_reference_candidates(reference.from_scope.as_deref(), &candidates);
        db.insert_references(
            &symbol_ids,
            file.id,
            reference.line,
            reference.column,
            reference.qualified,
        )?;
    }
    Ok(())
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
        if parsers::detect_language(path) == Some("csharp") {
            stats.cs_changed += 1;
        }
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
                    if language == "csharp" && file_stats.reparsed {
                        stats.cs_changed += 1;
                    }
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
        // Semantic rows resolve exactly by DocId, so the bare/qualified
        // distinction the import filter needs does not apply to them.
        db.insert_references(
            symbol_ids,
            file.id,
            reference.line,
            reference.col - 1,
            false,
        )?;
    }

    db.set_metadata("csharp_resolver", "roslyn")
}

/// Post-index pass: point every import row at the indexed file it names.
///
/// Runs after the walk, not during it, because resolution needs the whole
/// indexed file set — a file's importers are usually walked before it. Every
/// row is re-resolved on each pass so specifiers that newly resolve (or stop
/// resolving) after files are added or deleted stay correct. Returns the number
/// of imports that resolved to a file.
pub fn resolve_imports(db: &Database) -> Result<usize> {
    let files = db.all_files()?;
    let ids: HashMap<String, i64> = files.iter().map(|f| (f.path.clone(), f.id)).collect();
    let paths: HashSet<String> = ids.keys().cloned().collect();

    let mut resolved_count = 0;
    let mut updates = Vec::new();
    for import in db.all_imports_with_source()? {
        let resolved = resolver::resolve_import(
            &import.source_path,
            &import.language,
            &import.import_path,
            &paths,
        )
        // An import of the file it sits in is not an edge worth recording.
        .filter(|target| target != &import.source_path)
        .and_then(|target| ids.get(&target).copied());
        if resolved.is_some() {
            resolved_count += 1;
        }
        // On an incremental run almost every row resolves to what it already
        // held; only the differences are worth writing.
        if resolved != import.resolved_file_id {
            updates.push((import.id, resolved));
        }
    }
    db.apply_import_resolutions(&updates)?;

    Ok(resolved_count)
}

#[derive(Debug, Default)]
pub struct IndexStats {
    pub files_indexed: usize,
    pub files_errored: usize,
    pub files_deleted: usize,
    pub symbols_found: usize,
    pub imports_found: usize,
    /// `.cs` files the walk indexed that the Roslyn sidecar snapshot missed
    /// (created mid-run); they have no semantic references until the next init.
    pub cs_missing_from_snapshot: Vec<String>,
    /// `.cs` files this pass rewrote (content changed) or deleted. Under a
    /// semantic (roslyn) index these files' outbound references degrade to
    /// tree-sitter and inbound semantic references onto their symbols cascade
    /// away with the deleted symbol rows (W1) — `update` warns on this.
    pub cs_changed: usize,
}

pub(crate) struct FileStats {
    symbols: usize,
    imports: usize,
    /// False when the content hash matched and the file's rows were left as-is.
    reparsed: bool,
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
            vec![(save_a, file_main, 10, 8), (save_b, file_main, 11, 12),]
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
            references: vec![wire_ref(
                "M:System.Console.WriteLine",
                "Program.cs",
                4,
                9,
                false,
            )],
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
        db.insert_reference(save, cs_file, 20, 3, false).unwrap();
        db.insert_reference(helper, py_file, 9, 0, false).unwrap();

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

    /// `cs_changed` counts only `.cs` files whose rows were rewritten or
    /// deleted — unchanged-hash files and other languages don't count.
    #[test]
    fn cs_changed_counts_rewritten_and_deleted_cs_files() {
        let dir = tempfile::tempdir().unwrap();
        let cs = dir.path().join("A.cs");
        std::fs::write(&cs, "class A {}").unwrap();
        std::fs::write(dir.path().join("b.py"), "def b():\n    pass\n").unwrap();

        let db = Database::open_in_memory().unwrap();
        let stats = index_full(&db, dir.path(), None).unwrap();
        assert_eq!(stats.cs_changed, 1);

        // Second pass over identical content: nothing rewritten.
        let stats = index_full(&db, dir.path(), None).unwrap();
        assert_eq!(stats.cs_changed, 0);

        // Incremental: rewritten A.cs and deleted Gone.cs count; the
        // unchanged b.py in the modified list does not.
        std::fs::write(&cs, "class A { void M() { } }").unwrap();
        let stats = index_incremental(
            &db,
            dir.path(),
            &["A.cs".to_string(), "b.py".to_string()],
            &["Gone.cs".to_string()],
        )
        .unwrap();
        assert_eq!(stats.cs_changed, 2);
    }

    /// The pass runs after the walk, so importers walked before their target
    /// still resolve; a deleted target un-resolves the edge on the next pass.
    #[test]
    fn resolve_imports_keys_dependents_by_file_after_the_walk() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src/util")).unwrap();
        std::fs::create_dir_all(dir.path().join("src/domain")).unwrap();
        std::fs::write(
            dir.path().join("src/util/money.ts"),
            "export function money() {}\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("src/domain/cart.ts"),
            "import { money } from '../util/money';\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("src/app.ts"),
            "import { money } from './util/money';\nimport React from 'react';\n",
        )
        .unwrap();

        let db = Database::open_in_memory().unwrap();
        index_full(&db, dir.path(), None).unwrap();
        // Before resolution the file's own path answers nothing.
        assert!(db.file_dependents("src/util/money.ts").unwrap().is_empty());

        assert_eq!(resolve_imports(&db).unwrap(), 2, "'react' has no file");
        assert_eq!(
            db.file_dependents("src/util/money.ts").unwrap(),
            vec!["src/app.ts".to_string(), "src/domain/cart.ts".to_string()]
        );

        // Target deleted: the edge goes away rather than pointing at a stale id.
        index_incremental(&db, dir.path(), &[], &["src/util/money.ts".to_string()]).unwrap();
        resolve_imports(&db).unwrap();
        assert!(db.file_dependents("src/util/money.ts").unwrap().is_empty());
        assert_eq!(
            db.file_dependencies("src/domain/cart.ts").unwrap(),
            vec!["../util/money".to_string()]
        );
    }

    /// Files referencing `formatMoney`, keyed by the definition they are
    /// attributed to.
    fn callers_of(db: &Database, name: &str, defined_in: &str) -> Vec<String> {
        let ids: Vec<i64> = db
            .find_symbol_by_name(name)
            .unwrap()
            .into_iter()
            .filter(|(_, path)| path == defined_in)
            .map(|(sym, _)| sym.id)
            .collect();
        let mut paths: Vec<String> = db
            .symbol_references(&ids)
            .unwrap()
            .into_iter()
            .map(|(path, _, _)| path)
            .collect();
        paths.sort();
        paths.dedup();
        paths
    }

    /// Two definitions of one name: each importer's usages belong to the
    /// definition it imported, and a file that imports neither still lists
    /// against both (nothing says which it means).
    #[test]
    fn references_attribute_to_the_imported_definition() {
        let dir = tempfile::tempdir().unwrap();
        for sub in ["src/util", "src/legacy", "src/domain", "src/reports"] {
            std::fs::create_dir_all(dir.path().join(sub)).unwrap();
        }
        std::fs::write(
            dir.path().join("src/util/money.ts"),
            "export function formatMoney() {}\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("src/legacy/money.ts"),
            "export function formatMoney() {}\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("src/domain/cart.ts"),
            "import { formatMoney } from '../util/money';\nexport const t = () => formatMoney();\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("src/reports/audit.ts"),
            "import { formatMoney } from '../legacy/money';\nexport const a = () => formatMoney();\n",
        )
        .unwrap();
        // No import of the name at all — ambiguous, so both keep the caller.
        std::fs::write(
            dir.path().join("src/reports/globals.ts"),
            "export const g = () => formatMoney();\n",
        )
        .unwrap();

        let db = Database::open_in_memory().unwrap();
        index_full(&db, dir.path(), None).unwrap();
        // Before resolution every caller counts against both definitions.
        assert_eq!(
            callers_of(&db, "formatMoney", "src/util/money.ts").len(),
            3,
            "all callers attributed to both definitions at index time"
        );

        resolve_imports(&db).unwrap();

        assert_eq!(
            callers_of(&db, "formatMoney", "src/util/money.ts"),
            vec![
                "src/domain/cart.ts".to_string(),
                "src/reports/globals.ts".to_string()
            ]
        );
        assert_eq!(
            callers_of(&db, "formatMoney", "src/legacy/money.ts"),
            vec![
                "src/reports/audit.ts".to_string(),
                "src/reports/globals.ts".to_string()
            ]
        );
    }

    /// An importer that switches specifiers moves with it: the re-index must
    /// drop the old import's names, or its usages stay attributed to the file
    /// it no longer imports.
    #[test]
    fn attribution_follows_a_changed_import() {
        let dir = tempfile::tempdir().unwrap();
        for sub in ["src/util", "src/legacy"] {
            std::fs::create_dir_all(dir.path().join(sub)).unwrap();
        }
        std::fs::write(
            dir.path().join("src/util/money.ts"),
            "export function formatMoney() {}\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("src/legacy/money.ts"),
            "export function formatMoney() {}\n",
        )
        .unwrap();
        let cart = dir.path().join("src/cart.ts");
        std::fs::write(
            &cart,
            "import { formatMoney } from './util/money';\nexport const t = () => formatMoney();\n",
        )
        .unwrap();

        let db = Database::open_in_memory().unwrap();
        index_full(&db, dir.path(), None).unwrap();
        resolve_imports(&db).unwrap();
        assert_eq!(
            callers_of(&db, "formatMoney", "src/legacy/money.ts"),
            Vec::<String>::new()
        );

        std::fs::write(
            &cart,
            "import { formatMoney } from './legacy/money';\nexport const t = () => formatMoney();\n",
        )
        .unwrap();
        index_incremental(&db, dir.path(), &["src/cart.ts".to_string()], &[]).unwrap();
        resolve_imports(&db).unwrap();

        assert_eq!(
            callers_of(&db, "formatMoney", "src/legacy/money.ts"),
            vec!["src/cart.ts".to_string()]
        );
        assert_eq!(
            callers_of(&db, "formatMoney", "src/util/money.ts"),
            Vec::<String>::new()
        );
    }

    /// An import binds a bare name, so it says nothing about usages reached
    /// through a receiver: a method call and a namespace-qualified call keep
    /// their attribution even when the same name is imported in that file.
    #[test]
    fn qualified_usages_are_exempt_from_import_attribution() {
        let dir = tempfile::tempdir().unwrap();
        for sub in ["src/util", "src/legacy"] {
            std::fs::create_dir_all(dir.path().join(sub)).unwrap();
        }
        std::fs::write(
            dir.path().join("src/util/money.ts"),
            "export function formatMoney() {}\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("src/legacy/money.ts"),
            "export function formatMoney() {}\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("src/widget.ts"),
            "export class Widget {\n  formatMoney() { return 1; }\n}\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("src/cart.ts"),
            "import { formatMoney } from './util/money';\n\
             import { Widget } from './widget';\n\
             import * as legacy from './legacy/money';\n\
             const w = new Widget();\n\
             export const t = () => formatMoney() + w.formatMoney() + legacy.formatMoney();\n",
        )
        .unwrap();

        let db = Database::open_in_memory().unwrap();
        index_full(&db, dir.path(), None).unwrap();
        resolve_imports(&db).unwrap();

        // `w.formatMoney()` is the widget's method, not the imported function.
        assert_eq!(
            callers_of(&db, "formatMoney", "src/widget.ts"),
            vec!["src/cart.ts".to_string()]
        );
        // `legacy.formatMoney()` goes through the namespace binding, so the
        // legacy definition keeps the caller too.
        assert_eq!(
            callers_of(&db, "formatMoney", "src/legacy/money.ts"),
            vec!["src/cart.ts".to_string()]
        );
    }

    /// One local name bound to two files — the `try: import fast / except
    /// ImportError: import slow` idiom — has two real answers, so both keep the
    /// usage rather than each cancelling the other out.
    #[test]
    fn a_name_imported_from_two_files_keeps_both_definitions() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("pkg")).unwrap();
        std::fs::write(dir.path().join("pkg/__init__.py"), "").unwrap();
        std::fs::write(dir.path().join("pkg/fast.py"), "def dumps():\n    pass\n").unwrap();
        std::fs::write(dir.path().join("pkg/slow.py"), "def dumps():\n    pass\n").unwrap();
        std::fs::write(
            dir.path().join("pkg/app.py"),
            "try:\n    from .fast import dumps\nexcept ImportError:\n    from .slow import dumps\n\
             \n\ndef go():\n    return dumps()\n",
        )
        .unwrap();

        let db = Database::open_in_memory().unwrap();
        index_full(&db, dir.path(), None).unwrap();
        resolve_imports(&db).unwrap();

        for target in ["pkg/fast.py", "pkg/slow.py"] {
            assert_eq!(
                callers_of(&db, "dumps", target),
                vec!["pkg/app.py".to_string()],
                "{target} lost the usage"
            );
        }
    }

    /// Attribution is a read-time view of the import graph, not a deletion: a
    /// change on the *defining* side, which never re-indexes the referencing
    /// file, must not leave the usage attributed to nothing at all.
    #[test]
    fn a_changed_definition_does_not_strand_the_usage() {
        let dir = tempfile::tempdir().unwrap();
        for sub in ["src/util", "src/legacy"] {
            std::fs::create_dir_all(dir.path().join(sub)).unwrap();
        }
        let util = dir.path().join("src/util/money.ts");
        std::fs::write(&util, "export function formatMoney() {}\n").unwrap();
        std::fs::write(
            dir.path().join("src/legacy/money.ts"),
            "export function formatMoney() {}\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("src/cart.ts"),
            "import { formatMoney } from './util/money';\nexport const t = () => formatMoney();\n",
        )
        .unwrap();

        let db = Database::open_in_memory().unwrap();
        index_full(&db, dir.path(), None).unwrap();
        resolve_imports(&db).unwrap();
        assert_eq!(
            callers_of(&db, "formatMoney", "src/util/money.ts"),
            vec!["src/cart.ts".to_string()]
        );

        // The imported definition is renamed; `cart.ts` itself does not change,
        // so an incremental update never revisits its reference rows.
        std::fs::write(&util, "export function formatMoneyV2() {}\n").unwrap();
        index_incremental(&db, dir.path(), &["src/util/money.ts".to_string()], &[]).unwrap();
        resolve_imports(&db).unwrap();

        // The import no longer names a definition in that file, so the usage
        // falls back to the remaining candidate instead of vanishing.
        assert_eq!(
            callers_of(&db, "formatMoney", "src/legacy/money.ts"),
            vec!["src/cart.ts".to_string()]
        );
    }

    /// Python `from .money import format_money` disambiguates the same way,
    /// while an aliased import — whose local name no definition carries —
    /// keeps the ambiguous-name behaviour rather than guessing.
    #[test]
    fn python_from_imports_attribute_and_aliases_fall_back() {
        let dir = tempfile::tempdir().unwrap();
        for sub in ["pkg/util", "pkg/legacy", "pkg/domain"] {
            std::fs::create_dir_all(dir.path().join(sub)).unwrap();
        }
        for init in ["pkg", "pkg/util", "pkg/legacy", "pkg/domain"] {
            std::fs::write(dir.path().join(init).join("__init__.py"), "").unwrap();
        }
        std::fs::write(
            dir.path().join("pkg/util/money.py"),
            "def format_money():\n    pass\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("pkg/legacy/money.py"),
            "def format_money():\n    pass\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("pkg/domain/cart.py"),
            "from ..util.money import format_money\n\ndef total():\n    return format_money()\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("pkg/domain/audit.py"),
            "from ..legacy.money import format_money as fm\n\ndef audit():\n    return fm()\n",
        )
        .unwrap();

        let db = Database::open_in_memory().unwrap();
        index_full(&db, dir.path(), None).unwrap();
        resolve_imports(&db).unwrap();

        assert_eq!(
            callers_of(&db, "format_money", "pkg/util/money.py"),
            vec!["pkg/domain/cart.py".to_string()]
        );
        // `fm()` is a reference to `fm`, a name no definition carries — the
        // alias is not resolved, so nothing is attributed and nothing is lost.
        assert_eq!(
            callers_of(&db, "format_money", "pkg/legacy/money.py"),
            Vec::<String>::new()
        );
    }

    #[test]
    fn walk_reports_cs_files_missing_from_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Old.cs"), "class Old {}").unwrap();
        std::fs::write(dir.path().join("New.cs"), "class New {}").unwrap();

        let db = Database::open_in_memory().unwrap();
        let snapshot = vec!["Old.cs".to_string()];
        let stats = index_full(&db, dir.path(), Some(&snapshot)).unwrap();
        assert_eq!(stats.cs_missing_from_snapshot, vec!["New.cs".to_string()]);

        let db = Database::open_in_memory().unwrap();
        let full: Vec<String> = vec!["Old.cs".into(), "New.cs".into()];
        let stats = index_full(&db, dir.path(), Some(&full)).unwrap();
        assert!(stats.cs_missing_from_snapshot.is_empty());

        // Tree-sitter mode has no snapshot to diff against.
        let db = Database::open_in_memory().unwrap();
        let stats = index_full(&db, dir.path(), None).unwrap();
        assert!(stats.cs_missing_from_snapshot.is_empty());
    }
}
