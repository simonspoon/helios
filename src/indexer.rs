use anyhow::{Context, Result};
use ignore::WalkBuilder;
use sha2::{Digest, Sha256};
use std::path::Path;

use crate::db::Database;
use crate::parsers;

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

    // Insert references — try to resolve to known symbols
    for reference in &parse_result.references {
        let symbols = db.find_symbol_by_name(&reference.symbol_name)?;
        if let Some((sym, _)) = symbols.first() {
            db.insert_reference(sym.id, file_id, reference.line, reference.column)?;
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
