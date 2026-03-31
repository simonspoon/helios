use anyhow::{Context, Result};
use std::collections::BTreeMap;

use crate::db::Database;
use crate::errors::NoIndexError;

pub fn run(json: bool, compact: bool, limit: Option<i64>, offset: Option<i64>) -> Result<()> {
    let cwd = std::env::current_dir().context("getting current directory")?;
    let db_path = cwd.join(".helios/index.db");

    if !db_path.exists() {
        return Err(NoIndexError.into());
    }

    let db = Database::open(&db_path).context("opening database")?;

    let paginated = limit.is_some() || offset.is_some();
    let all_symbols = db.query_symbols(None, None, None, None, None, limit, offset)?;

    if json {
        let mut files: BTreeMap<String, Vec<serde_json::Value>> = BTreeMap::new();

        for (sym, file_path) in &all_symbols {
            files
                .entry(file_path.clone())
                .or_default()
                .push(serde_json::json!({
                    "name": sym.name,
                    "kind": sym.kind,
                    "line": sym.line,
                    "column": sym.column,
                    "visibility": sym.visibility,
                    "scope": sym.scope,
                }));
        }

        // Also include imports per file
        let all_files = db.all_files()?;
        let mut output_files = Vec::new();

        for file in &all_files {
            let imports = db.get_imports_for_file(file.id)?;
            let syms = files.get(&file.path).cloned().unwrap_or_default();

            // Skip files with no symbols when paginated (symbols may have been filtered)
            if paginated && syms.is_empty() {
                continue;
            }

            output_files.push(serde_json::json!({
                "path": file.path,
                "language": file.language,
                "symbols": syms,
                "imports": imports.iter().map(|i| {
                    serde_json::json!({
                        "path": i.import_path,
                        "alias": i.alias,
                    })
                }).collect::<Vec<_>>(),
            }));
        }

        let mut output = serde_json::json!({
            "files": output_files,
            "total_files": all_files.len(),
            "total_symbols": all_symbols.len(),
        });
        if paginated {
            let total_count = db.count_symbols(None, None, None, None, None)?;
            output["total_count"] = serde_json::json!(total_count);
            output["limit"] = serde_json::json!(limit);
            output["offset"] = serde_json::json!(offset.unwrap_or(0));
        }
        let formatted = if compact {
            serde_json::to_string(&output)?
        } else {
            serde_json::to_string_pretty(&output)?
        };
        println!("{}", formatted);
    } else {
        // Markdown export (backward compatible with old code-index)
        let file_count = db.file_count()?;
        let symbol_count = db.symbol_count()?;
        let by_lang = db.files_by_language()?;
        let by_kind = db.symbols_by_kind()?;

        println!("# Code Index");
        println!();
        println!("**Files:** {} | **Symbols:** {}", file_count, symbol_count);
        println!();

        println!("## Languages");
        for (lang, count) in &by_lang {
            println!("| {} | {} files |", lang, count);
        }
        println!();

        println!("## Symbol Summary");
        for (kind, count) in &by_kind {
            println!("| {} | {} |", kind, count);
        }
        println!();

        // Group by directory then file
        let mut dirs: BTreeMap<String, BTreeMap<String, Vec<String>>> = BTreeMap::new();

        for (sym, file_path) in &all_symbols {
            let dir = std::path::Path::new(file_path.as_str())
                .parent()
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_else(|| ".".to_string());

            let scope_str = sym
                .scope
                .as_ref()
                .map(|s| format!(" ({})", s))
                .unwrap_or_default();

            dirs.entry(dir)
                .or_default()
                .entry(file_path.clone())
                .or_default()
                .push(format!(
                    "- `{}` {} **{}**{}  *(line {})*",
                    sym.kind, sym.visibility, sym.name, scope_str, sym.line
                ));
        }

        for (dir, files) in &dirs {
            println!("## {}/", if dir.is_empty() { "." } else { dir });
            println!();
            for (file, symbols) in files {
                println!("### {}", file);
                for line in symbols {
                    println!("{}", line);
                }
                println!();
            }
        }

        if paginated {
            let total_count = db.count_symbols(None, None, None, None, None)?;
            let offset_val = offset.unwrap_or(0);
            let start = offset_val + 1;
            let end = offset_val + all_symbols.len() as i64;
            println!("Showing {}-{} of {} symbols", start, end, total_count);
        }
    }

    Ok(())
}
