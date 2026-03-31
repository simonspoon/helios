use anyhow::{Context, Result};
use std::collections::HashMap;

use crate::db::Database;
use crate::git;
use crate::parsers;

#[derive(Debug, serde::Serialize)]
struct AddedSymbol {
    file: String,
    name: String,
    kind: String,
    line: i64,
}

#[derive(Debug, serde::Serialize)]
struct RemovedSymbol {
    file: String,
    name: String,
    kind: String,
    line: i64,
}

#[derive(Debug, serde::Serialize)]
struct ModifiedSymbol {
    file: String,
    name: String,
    kind: String,
    old_line: i64,
    new_line: i64,
}

pub fn run(json: bool, compact: bool) -> Result<()> {
    let cwd = std::env::current_dir().context("getting current directory")?;
    let db_path = cwd.join(".helios/index.db");

    if !db_path.exists() {
        if json {
            let output = serde_json::json!({"error": "No index found. Run `helios init` first."});
            let formatted = if compact {
                serde_json::to_string(&output)?
            } else {
                serde_json::to_string_pretty(&output)?
            };
            println!("{}", formatted);
        } else {
            println!("No index found. Run `helios init` first.");
        }
        return Ok(());
    }

    if !git::is_git_repo() {
        if json {
            let output = serde_json::json!({"error": "Not a git repository."});
            let formatted = if compact {
                serde_json::to_string(&output)?
            } else {
                serde_json::to_string_pretty(&output)?
            };
            println!("{}", formatted);
        } else {
            println!("Not a git repository. Diff requires git.");
        }
        return Ok(());
    }

    let db = Database::open(&db_path).context("opening database")?;

    let last_commit = db.get_metadata("last_indexed_commit")?;
    let last_commit = match last_commit {
        Some(c) => c,
        None => {
            if json {
                let output = serde_json::json!({"error": "No indexed commit found. Run `helios init` first."});
                let formatted = if compact {
                    serde_json::to_string(&output)?
                } else {
                    serde_json::to_string_pretty(&output)?
                };
                println!("{}", formatted);
            } else {
                println!("No indexed commit found. Run `helios init` first.");
            }
            return Ok(());
        }
    };

    let (modified_files, deleted_files) = git::changed_files(&last_commit)?;

    if modified_files.is_empty() && deleted_files.is_empty() {
        if json {
            let output = serde_json::json!({
                "added": [],
                "removed": [],
                "modified": [],
            });
            let formatted = if compact {
                serde_json::to_string(&output)?
            } else {
                serde_json::to_string_pretty(&output)?
            };
            println!("{}", formatted);
        } else {
            println!("No symbol changes detected.");
        }
        return Ok(());
    }

    // Build a set of all indexed file paths for quick lookup
    let all_db_files: std::collections::HashSet<String> =
        db.all_files()?.into_iter().map(|f| f.path).collect();

    let mut added: Vec<AddedSymbol> = Vec::new();
    let mut removed: Vec<RemovedSymbol> = Vec::new();
    let mut modified: Vec<ModifiedSymbol> = Vec::new();

    // Process modified/added files
    for file_path in &modified_files {
        let full_path = cwd.join(file_path);
        if !full_path.exists() {
            continue;
        }

        let content = match std::fs::read_to_string(&full_path) {
            Ok(c) => c,
            Err(_) => continue, // skip binary or unreadable files
        };

        // Parse current file
        let current_symbols = match parsers::parse_file(file_path, &content)? {
            Some((_lang, result)) => result.symbols,
            None => continue, // unsupported language
        };

        // Get DB symbols for this file
        let db_symbols = get_exact_file_symbols(&db, file_path)?;

        let is_new_file = !all_db_files.contains(file_path);

        if is_new_file {
            // All symbols are added
            for sym in &current_symbols {
                added.push(AddedSymbol {
                    file: file_path.clone(),
                    name: sym.name.clone(),
                    kind: sym.kind.clone(),
                    line: sym.line,
                });
            }
        } else {
            // Compare by name
            let current_by_name: HashMap<&str, &crate::db::ParsedSymbol> = current_symbols
                .iter()
                .map(|s| (s.name.as_str(), s))
                .collect();
            let db_by_name: HashMap<&str, &crate::db::SymbolRecord> =
                db_symbols.iter().map(|s| (s.name.as_str(), s)).collect();

            // Added: in current but not in DB
            for (name, sym) in &current_by_name {
                if !db_by_name.contains_key(name) {
                    added.push(AddedSymbol {
                        file: file_path.clone(),
                        name: sym.name.clone(),
                        kind: sym.kind.clone(),
                        line: sym.line,
                    });
                }
            }

            // Removed: in DB but not in current
            for (name, sym) in &db_by_name {
                if !current_by_name.contains_key(name) {
                    removed.push(RemovedSymbol {
                        file: file_path.clone(),
                        name: sym.name.clone(),
                        kind: sym.kind.clone(),
                        line: sym.line,
                    });
                }
            }

            // Modified: same name, different line/end_line/kind/visibility
            for (name, current_sym) in &current_by_name {
                if let Some(db_sym) = db_by_name.get(name)
                    && (current_sym.line != db_sym.line
                        || current_sym.end_line != db_sym.end_line
                        || current_sym.kind != db_sym.kind
                        || current_sym.visibility != db_sym.visibility)
                {
                    modified.push(ModifiedSymbol {
                        file: file_path.clone(),
                        name: current_sym.name.clone(),
                        kind: current_sym.kind.clone(),
                        old_line: db_sym.line,
                        new_line: current_sym.line,
                    });
                }
            }
        }
    }

    // Process deleted files: all DB symbols are removed
    for file_path in &deleted_files {
        let db_symbols = get_exact_file_symbols(&db, file_path)?;
        for sym in &db_symbols {
            removed.push(RemovedSymbol {
                file: file_path.clone(),
                name: sym.name.clone(),
                kind: sym.kind.clone(),
                line: sym.line,
            });
        }
    }

    // Output
    if json {
        let output = serde_json::json!({
            "added": added,
            "removed": removed,
            "modified": modified,
        });
        let formatted = if compact {
            serde_json::to_string(&output)?
        } else {
            serde_json::to_string_pretty(&output)?
        };
        println!("{}", formatted);
    } else {
        if added.is_empty() && removed.is_empty() && modified.is_empty() {
            println!("No symbol changes detected.");
            return Ok(());
        }

        for sym in &added {
            println!("+ {} {} ({}:{})", sym.kind, sym.name, sym.file, sym.line);
        }
        for sym in &removed {
            println!("- {} {} ({}:{})", sym.kind, sym.name, sym.file, sym.line);
        }
        for sym in &modified {
            println!(
                "~ {} {} ({}:{} -> {})",
                sym.kind, sym.name, sym.file, sym.old_line, sym.new_line
            );
        }
    }

    Ok(())
}

/// Get symbols for an exact file path from the DB.
/// query_symbols uses LIKE with substring match, so we query then filter for exact path.
fn get_exact_file_symbols(db: &Database, file_path: &str) -> Result<Vec<crate::db::SymbolRecord>> {
    let results = db.query_symbols(Some(file_path), None, None, None, None)?;
    Ok(results
        .into_iter()
        .filter(|(_, path)| path == file_path)
        .map(|(sym, _)| sym)
        .collect())
}
