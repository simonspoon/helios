use anyhow::{Context, Result};
use std::collections::BTreeMap;

use crate::db::Database;

pub fn run(json: bool) -> Result<()> {
    let cwd = std::env::current_dir().context("getting current directory")?;
    let db_path = cwd.join(".helios/index.db");

    if !db_path.exists() {
        anyhow::bail!("No index found. Run `helios init` first.");
    }

    let db = Database::open(&db_path).context("opening database")?;
    let all_symbols = db.symbols_in_directory("")?;

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

        let output = serde_json::json!({
            "files": output_files,
            "total_files": all_files.len(),
            "total_symbols": all_symbols.len(),
        });
        println!("{}", serde_json::to_string_pretty(&output)?);
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
    }

    Ok(())
}
