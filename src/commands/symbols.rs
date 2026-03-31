use anyhow::{Context, Result};
use regex::Regex;
use std::collections::HashMap;

use crate::db::Database;
use crate::errors::NoIndexError;

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
        return Err(NoIndexError.into());
    }

    let db = Database::open(&db_path).context("opening database")?;

    // Compile grep pattern as regex (if provided) before querying.
    // The SQL LIKE pre-filter narrows results; the regex does precise matching.
    let grep_re = match grep {
        Some(pattern) => {
            Some(Regex::new(pattern).with_context(|| format!("invalid regex pattern: {pattern}"))?)
        }
        None => None,
    };

    // Extract a LIKE-friendly substring from the regex for SQL pre-filtering.
    // Strip regex metacharacters, keep the longest literal run for LIKE narrowing.
    let like_hint = grep.and_then(|pattern| {
        let literal: String = pattern
            .chars()
            .filter(|c| c.is_alphanumeric() || *c == '_')
            .collect();
        if literal.is_empty() {
            None
        } else {
            Some(literal)
        }
    });

    // regex_total_count holds the true count after regex filtering (only set when regex is active).
    let (results, regex_total_count) = if let Some(ref re) = grep_re {
        // When regex filtering, fetch all LIKE-matched results (no SQL limit/offset)
        // so we can apply regex, then handle pagination in Rust.
        // Pass the literal hint to LIKE for pre-filtering, not the raw regex.
        let all = db.query_symbols(
            file,
            kind,
            like_hint.as_deref(),
            scope,
            visibility,
            None,
            None,
        )?;
        let filtered: Vec<_> = all
            .into_iter()
            .filter(|(sym, _)| re.is_match(&sym.name))
            .collect();
        let total = filtered.len() as i64;

        // Apply limit/offset in Rust after regex filtering
        let start = offset.unwrap_or(0) as usize;
        let page = if limit.is_some() || offset.is_some() {
            let end = match limit {
                Some(l) => (start + l as usize).min(filtered.len()),
                None => filtered.len(),
            };
            if start < filtered.len() {
                filtered[start..end].to_vec()
            } else {
                Vec::new()
            }
        } else {
            filtered
        };
        (page, Some(total))
    } else {
        let r = db.query_symbols(file, kind, grep, scope, visibility, limit, offset)?;
        (r, None)
    };

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
            let total_count = match regex_total_count {
                Some(c) => c,
                None => db.count_symbols(file, kind, grep, scope, visibility)?,
            };
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
            let total_count = match regex_total_count {
                Some(c) => c,
                None => db.count_symbols(file, kind, grep, scope, visibility)?,
            };
            let offset_val = offset.unwrap_or(0);
            let start = offset_val + 1;
            let end = offset_val + results.len() as i64;
            println!("Showing {}-{} of {} symbols", start, end, total_count);
        }
    }

    Ok(())
}
