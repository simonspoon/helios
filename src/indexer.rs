use anyhow::{Context, Result};
use ignore::WalkBuilder;
use sha2::{Digest, Sha256};
use std::path::Path;

use crate::db::{Database, SymbolRecord};
use crate::parsers;

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

/// Index all supported files in a directory
pub fn index_full(db: &Database, root: &Path) -> Result<IndexStats> {
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
            match index_file(db, path, &rel_path, language) {
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
    for reference in &parse_result.references {
        let candidates = db.find_symbol_by_name(&reference.symbol_name)?;
        let symbol_ids = resolve_reference_candidates(reference.from_scope.as_deref(), &candidates);
        db.insert_references(&symbol_ids, file_id, reference.line, reference.column)?;
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
            match index_file(db, &abs_path, rel_path, language) {
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
