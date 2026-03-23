use anyhow::{Context, Result};

use crate::db::Database;

pub fn run(file: Option<&str>, kind: Option<&str>, grep: Option<&str>, json: bool) -> Result<()> {
    let cwd = std::env::current_dir().context("getting current directory")?;
    let db_path = cwd.join(".helios/index.db");

    if !db_path.exists() {
        anyhow::bail!("No index found. Run `helios init` first.");
    }

    let db = Database::open(&db_path).context("opening database")?;
    let results = db.query_symbols(file, kind, grep)?;

    if json {
        let items: Vec<_> = results
            .iter()
            .map(|(sym, path)| {
                serde_json::json!({
                    "file": path,
                    "line": sym.line,
                    "column": sym.column,
                    "kind": sym.kind,
                    "visibility": sym.visibility,
                    "name": sym.name,
                    "scope": sym.scope,
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&items)?);
    } else {
        for (sym, path) in &results {
            println!(
                "{}:{}:{} {} {} {}",
                path, sym.line, sym.column, sym.kind, sym.visibility, sym.name
            );
        }
        if results.is_empty() {
            println!("No symbols found");
        }
    }

    Ok(())
}
