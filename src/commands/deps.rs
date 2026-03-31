use std::collections::{HashSet, VecDeque};

use anyhow::{Context, Result};

use crate::db::Database;
use crate::errors::NoIndexError;

/// BFS traversal result: (path, depth_level)
struct BfsResult {
    entries: Vec<(String, u32)>,
}

/// BFS over file dependencies or dependents up to max_depth.
/// Returns entries with their depth level (1-indexed).
fn bfs_file_deps(
    db: &Database,
    start: &str,
    max_depth: u32,
    get_neighbors: impl Fn(&Database, &str) -> Result<Vec<String>>,
) -> Result<BfsResult> {
    let mut visited = HashSet::new();
    visited.insert(start.to_string());

    let mut queue: VecDeque<(String, u32)> = VecDeque::new();
    queue.push_back((start.to_string(), 0));

    let mut entries: Vec<(String, u32)> = Vec::new();

    while let Some((current, current_depth)) = queue.pop_front() {
        if current_depth >= max_depth {
            continue;
        }

        let neighbors = get_neighbors(db, &current)?;
        for neighbor in neighbors {
            if visited.insert(neighbor.clone()) {
                let depth_level = current_depth + 1;
                entries.push((neighbor.clone(), depth_level));
                queue.push_back((neighbor, depth_level));
            }
        }
    }

    Ok(BfsResult { entries })
}

pub fn run(target: &str, json: bool, compact: bool, depth: u32) -> Result<()> {
    let cwd = std::env::current_dir().context("getting current directory")?;
    let db_path = cwd.join(".helios/index.db");

    if !db_path.exists() {
        return Err(NoIndexError.into());
    }

    let db = Database::open(&db_path).context("opening database")?;

    // Determine if target is a file path or symbol name
    let is_file = target.contains('/') || target.contains('.');

    if json {
        if is_file {
            let deps_result =
                bfs_file_deps(&db, target, depth, |db, path| db.file_dependencies(path))?;
            let dependents_result =
                bfs_file_deps(&db, target, depth, |db, path| db.file_dependents(path))?;

            let deps_json: Vec<serde_json::Value> = deps_result
                .entries
                .iter()
                .map(|(path, d)| {
                    serde_json::json!({
                        "path": path,
                        "depth": d,
                    })
                })
                .collect();

            let dependents_json: Vec<serde_json::Value> = dependents_result
                .entries
                .iter()
                .map(|(path, d)| {
                    serde_json::json!({
                        "path": path,
                        "depth": d,
                    })
                })
                .collect();

            let output = serde_json::json!({
                "target": target,
                "depth": depth,
                "dependencies": deps_json,
                "dependents": dependents_json,
            });

            let formatted = if compact {
                serde_json::to_string(&output)?
            } else {
                serde_json::to_string_pretty(&output)?
            };
            println!("{}", formatted);
        } else {
            // Symbol mode: ignore depth, keep depth=1 behavior
            let deps = db.symbol_dependencies(target)?;
            let refs = db.symbol_references(target)?;

            let output = serde_json::json!({
                "target": target,
                "dependencies": deps,
                "dependents": refs.iter()
                    .map(|(path, line, col)| {
                        serde_json::json!({"file": path, "line": line, "column": col})
                    })
                    .collect::<Vec<_>>(),
            });

            let formatted = if compact {
                serde_json::to_string(&output)?
            } else {
                serde_json::to_string_pretty(&output)?
            };
            println!("{}", formatted);
        }
    } else {
        if is_file {
            let deps_result =
                bfs_file_deps(&db, target, depth, |db, path| db.file_dependencies(path))?;
            let dependents_result =
                bfs_file_deps(&db, target, depth, |db, path| db.file_dependents(path))?;

            if !deps_result.entries.is_empty() {
                println!("Dependencies (what {} imports):", target);
                for (dep, d) in &deps_result.entries {
                    let indent = "  ".repeat(*d as usize);
                    println!("{}-> {} (depth {})", indent, dep, d);
                }
            }

            if !dependents_result.entries.is_empty() {
                println!("Dependents (what imports {}):", target);
                for (dep, d) in &dependents_result.entries {
                    let indent = "  ".repeat(*d as usize);
                    println!("{}-> {} (depth {})", indent, dep, d);
                }
            }

            if deps_result.entries.is_empty() && dependents_result.entries.is_empty() {
                println!("No dependencies found for {}", target);
            }
        } else {
            // Symbol mode: ignore depth, keep depth=1 behavior
            let deps = db.symbol_dependencies(target)?;
            let refs = db.symbol_references(target)?;

            if !deps.is_empty() {
                println!("Dependencies (imports in files defining {}):", target);
                for dep in &deps {
                    println!("  {} -> {} (import)", target, dep);
                }
            }

            if !refs.is_empty() {
                println!("References (where {} is used):", target);
                for (path, line, col) in &refs {
                    println!("  {}:{}:{} -> {} (reference)", path, line, col, target);
                }
            }

            if deps.is_empty() && refs.is_empty() {
                println!("No dependencies found for {}", target);
            }
        }
    }

    Ok(())
}
