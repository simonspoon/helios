use anyhow::{Context, Result};

use crate::db::Database;
use crate::errors::NoIndexError;

pub fn run(language: Option<&str>, json: bool, compact: bool) -> Result<()> {
    let cwd = std::env::current_dir().context("getting current directory")?;
    let db_path = cwd.join(".helios/index.db");

    if !db_path.exists() {
        return Err(NoIndexError.into());
    }

    let db = Database::open(&db_path).context("opening database")?;
    let files = db.files_with_counts(language)?;

    if json {
        let entries: Vec<serde_json::Value> = files
            .iter()
            .map(|f| {
                serde_json::json!({
                    "path": f.path,
                    "language": f.language,
                    "symbols": f.symbol_count,
                    "imports": f.import_count,
                    "last_indexed_at": f.last_indexed_at,
                })
            })
            .collect();

        let output = serde_json::json!(entries);
        let formatted = if compact {
            serde_json::to_string(&output)?
        } else {
            serde_json::to_string_pretty(&output)?
        };
        println!("{}", formatted);
    } else {
        if files.is_empty() {
            println!("No files indexed.");
            return Ok(());
        }

        // Calculate column widths
        let path_w = files.iter().map(|f| f.path.len()).max().unwrap_or(4).max(4);
        let lang_w = files
            .iter()
            .map(|f| f.language.len())
            .max()
            .unwrap_or(8)
            .max(8);

        println!(
            "{:<path_w$}  {:<lang_w$}  {:>7}  {:>7}  INDEXED_AT",
            "PATH", "LANGUAGE", "SYMBOLS", "IMPORTS",
        );

        for f in &files {
            println!(
                "{:<path_w$}  {:<lang_w$}  {:>7}  {:>7}  {}",
                f.path, f.language, f.symbol_count, f.import_count, f.last_indexed_at,
            );
        }
    }

    Ok(())
}
