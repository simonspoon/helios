use std::collections::{HashSet, VecDeque};

use anyhow::{Context, Result};

use crate::db::{Database, SymbolDefinition};
use crate::errors::NoIndexError;
use crate::parsers::detect_language;

/// A symbol target: the name to look up plus the narrowing the user asked for.
struct SymbolTarget {
    name: String,
    scope: Option<String>,
    file: Option<String>,
    /// Whether an empty result should be retried as a file target. Only a bare
    /// dotted target sets this — it may be a module path rather than a
    /// qualified name.
    may_be_file: bool,
}

/// Which definition(s) a target names, and how it was spelled.
///
/// `deps` answers two different questions depending on its target, and a bare
/// string has to be sorted into one of them. A target is a *file* when it names
/// one — it has a path separator or a source extension — and a *symbol*
/// otherwise. `--scope` / `--file` force symbol mode, since they only narrow a
/// definition.
fn parse_target(target: &str, scope: Option<&str>, file: Option<&str>) -> Option<SymbolTarget> {
    // `path/to/file.ts:name` — a file and a name, so unambiguously a symbol.
    if let Some((path, name)) = target.rsplit_once(':')
        && !name.is_empty()
        && !path.is_empty()
    {
        return Some(SymbolTarget {
            name: name.to_string(),
            scope: scope.map(str::to_string),
            file: Some(path.to_string()),
            may_be_file: false,
        });
    }

    if scope.is_some() || file.is_some() {
        return Some(SymbolTarget {
            name: target.to_string(),
            scope: scope.map(str::to_string),
            file: file.map(str::to_string),
            may_be_file: false,
        });
    }

    if target.contains('/') || detect_language(target).is_some() {
        return None;
    }

    // `Class.Method`: the last segment is the name, everything before it the
    // scope. A dotted target that matches no definition is retried as a file
    // (`pkg.util.money` is how Python and C# name a module or namespace).
    match target.rsplit_once('.') {
        Some((qualifier, name)) if !qualifier.is_empty() && !name.is_empty() => {
            Some(SymbolTarget {
                name: name.to_string(),
                scope: Some(qualifier.to_string()),
                file: None,
                may_be_file: true,
            })
        }
        // Any other dotted spelling (`.`, `..money`) is not a qualified name,
        // but it is still how a module can be written, so keep the retry.
        _ => Some(SymbolTarget {
            name: target.to_string(),
            scope: None,
            file: None,
            may_be_file: target.contains('.'),
        }),
    }
}

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

pub fn run(
    target: &str,
    json: bool,
    compact: bool,
    depth: u32,
    scope: Option<&str>,
    file: Option<&str>,
) -> Result<()> {
    let cwd = std::env::current_dir().context("getting current directory")?;
    let db_path = cwd.join(".helios/index.db");

    if !db_path.exists() {
        return Err(NoIndexError.into());
    }

    let db = Database::open(&db_path).context("opening database")?;

    // Resolve a symbol target to the definitions it selects. Only those
    // definitions' ids are queried, so `--scope`/`--file`/`Class.Method`
    // exclude the same-named definitions the user did not mean.
    let symbol = parse_target(target, scope, file);
    let defs: Vec<SymbolDefinition> = match &symbol {
        Some(t) => db.find_definitions(&t.name, t.scope.as_deref(), t.file.as_deref())?,
        None => Vec::new(),
    };
    let is_file = match &symbol {
        None => true,
        Some(t) => t.may_be_file && defs.is_empty(),
    };
    let symbol_ids: Vec<i64> = defs.iter().map(|d| d.id).collect();

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
            let deps = db.symbol_dependencies(&symbol_ids)?;
            let refs = db.symbol_references(&symbol_ids)?;

            let output = serde_json::json!({
                "target": target,
                "definitions": defs,
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
            let deps = db.symbol_dependencies(&symbol_ids)?;
            let refs = db.symbol_references(&symbol_ids)?;

            if !defs.is_empty() {
                println!("Definitions of {}:", target);
                for def in &defs {
                    match &def.scope {
                        Some(s) => println!("  {}:{} (scope {})", def.path, def.line, s),
                        None => println!("  {}:{}", def.path, def.line),
                    }
                }
            }

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
