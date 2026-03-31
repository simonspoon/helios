use anyhow::{Context, Result};
use std::collections::HashMap;

use crate::db::Database;

/// Read the body of a symbol from its source file.
/// Returns the source lines from `line` to `end_line` (both 1-based).
/// If end_line is 0 (legacy data without end_line), returns just the single start line.
fn read_body(
    file_cache: &mut HashMap<String, Vec<String>>,
    cwd: &std::path::Path,
    file_path: &str,
    line: i64,
    end_line: i64,
) -> Option<String> {
    let lines = file_cache.entry(file_path.to_string()).or_insert_with(|| {
        let abs_path = cwd.join(file_path);
        std::fs::read_to_string(abs_path)
            .unwrap_or_default()
            .lines()
            .map(|l| l.to_string())
            .collect()
    });

    let start = (line as usize).saturating_sub(1);
    let end = if end_line > 0 {
        end_line as usize
    } else {
        // Legacy data: just show the single line
        line as usize
    };

    if start >= lines.len() {
        return None;
    }

    let end = end.min(lines.len());
    Some(lines[start..end].join("\n"))
}

#[allow(clippy::too_many_arguments)]
pub fn run(
    file: Option<&str>,
    kind: Option<&str>,
    grep: Option<&str>,
    scope: Option<&str>,
    visibility: Option<&str>,
    json: bool,
    compact: bool,
    body: bool,
    limit: Option<i64>,
    offset: Option<i64>,
) -> Result<()> {
    let cwd = std::env::current_dir().context("getting current directory")?;
    let db_path = cwd.join(".helios/index.db");

    if !db_path.exists() {
        anyhow::bail!("No index found. Run `helios init` first.");
    }

    let db = Database::open(&db_path).context("opening database")?;
    let results = db.query_symbols(file, kind, grep, scope, visibility, limit, offset)?;

    let paginated = limit.is_some() || offset.is_some();

    let mut file_cache: HashMap<String, Vec<String>> = HashMap::new();

    if json {
        let items: Vec<_> = results
            .iter()
            .map(|(sym, path)| {
                let mut obj = serde_json::json!({
                    "file": path,
                    "line": sym.line,
                    "column": sym.column,
                    "end_line": sym.end_line,
                    "kind": sym.kind,
                    "visibility": sym.visibility,
                    "name": sym.name,
                    "scope": sym.scope,
                });
                if body {
                    let body_text = read_body(&mut file_cache, &cwd, path, sym.line, sym.end_line);
                    obj["body"] = serde_json::json!(body_text);
                }
                obj
            })
            .collect();

        if paginated {
            let total_count = db.count_symbols(file, kind, grep, scope, visibility)?;
            let output = serde_json::json!({
                "symbols": items,
                "total_count": total_count,
                "limit": limit,
                "offset": offset.unwrap_or(0),
            });
            let formatted = if compact {
                serde_json::to_string(&output)?
            } else {
                serde_json::to_string_pretty(&output)?
            };
            println!("{}", formatted);
        } else {
            let formatted = if compact {
                serde_json::to_string(&items)?
            } else {
                serde_json::to_string_pretty(&items)?
            };
            println!("{}", formatted);
        }
    } else {
        for (sym, path) in &results {
            if body {
                let end = if sym.end_line > 0 {
                    sym.end_line
                } else {
                    sym.line
                };
                println!("--- {}:{}-{} ---", path, sym.line, end);
                if let Some(body_text) =
                    read_body(&mut file_cache, &cwd, path, sym.line, sym.end_line)
                {
                    println!("{}", body_text);
                }
                println!();
            } else {
                println!(
                    "{}:{}:{} {} {} {}",
                    path, sym.line, sym.column, sym.kind, sym.visibility, sym.name
                );
            }
        }
        if results.is_empty() {
            println!("No symbols found");
        }
        if paginated {
            let total_count = db.count_symbols(file, kind, grep, scope, visibility)?;
            let offset_val = offset.unwrap_or(0);
            let start = offset_val + 1;
            let end = offset_val + results.len() as i64;
            println!("Showing {}-{} of {} symbols", start, end, total_count);
        }
    }

    Ok(())
}
