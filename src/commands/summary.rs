use anyhow::{Context, Result};
use std::collections::BTreeMap;

use crate::db::Database;

pub fn run(path: Option<&str>, json: bool) -> Result<()> {
    let cwd = std::env::current_dir().context("getting current directory")?;
    let db_path = cwd.join(".helios/index.db");

    if !db_path.exists() {
        anyhow::bail!("No index found. Run `helios init` first.");
    }

    let db = Database::open(&db_path).context("opening database")?;

    let prefix = path.unwrap_or("");
    let symbols = db.symbols_in_directory(prefix)?;

    if json {
        // Group by directory
        let mut dirs: BTreeMap<String, Vec<serde_json::Value>> = BTreeMap::new();
        for (sym, file_path) in &symbols {
            let dir = std::path::Path::new(file_path)
                .parent()
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_else(|| ".".to_string());

            dirs.entry(dir).or_default().push(serde_json::json!({
                "file": file_path,
                "name": sym.name,
                "kind": sym.kind,
                "visibility": sym.visibility,
                "line": sym.line,
            }));
        }

        let output = serde_json::json!({
            "path": if prefix.is_empty() { "." } else { prefix },
            "total_symbols": symbols.len(),
            "directories": dirs,
        });
        println!("{}", serde_json::to_string_pretty(&output)?);
    } else {
        let file_count = db.file_count()?;
        let symbol_count = db.symbol_count()?;
        let by_kind = db.symbols_by_kind()?;
        let by_lang = db.files_by_language()?;

        println!("# Project Summary");
        if !prefix.is_empty() {
            println!("Path: {}", prefix);
        }
        println!();
        println!("**Files:** {} | **Symbols:** {}", file_count, symbol_count);
        println!();

        if !by_lang.is_empty() {
            println!("## Languages");
            for (lang, count) in &by_lang {
                println!("- {}: {} files", lang, count);
            }
            println!();
        }

        if !by_kind.is_empty() {
            println!("## Symbol Kinds");
            for (kind, count) in &by_kind {
                println!("- {}: {}", kind, count);
            }
            println!();
        }

        // Group symbols by directory
        let mut dirs: BTreeMap<String, Vec<(&str, &str, &str, &str)>> = BTreeMap::new();
        for (sym, file_path) in &symbols {
            let dir = std::path::Path::new(file_path.as_str())
                .parent()
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_else(|| ".".to_string());

            dirs.entry(dir)
                .or_default()
                .push((file_path, &sym.kind, &sym.visibility, &sym.name));
        }

        for (dir, items) in &dirs {
            println!("## {}/", if dir.is_empty() { "." } else { dir });
            let mut current_file = "";
            for (file, kind, vis, name) in items {
                if *file != current_file {
                    println!("\n### {}", file);
                    current_file = file;
                }
                println!("- `{}` {} {}", kind, vis, name);
            }
            println!();
        }
    }

    Ok(())
}
