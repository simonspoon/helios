use anyhow::{Context, Result};

use crate::db::Database;

pub fn run(target: &str, json: bool, compact: bool) -> Result<()> {
    let cwd = std::env::current_dir().context("getting current directory")?;
    let db_path = cwd.join(".helios/index.db");

    if !db_path.exists() {
        anyhow::bail!("No index found. Run `helios init` first.");
    }

    let db = Database::open(&db_path).context("opening database")?;

    // Determine if target is a file path or symbol name
    let is_file = target.contains('/') || target.contains('.');

    if json {
        let mut output = serde_json::json!({
            "target": target,
            "dependencies": [],
            "dependents": [],
        });

        if is_file {
            let deps = db.file_dependencies(target)?;
            let dependents = db.file_dependents(target)?;
            output["dependencies"] = serde_json::json!(deps);
            output["dependents"] = serde_json::json!(dependents);
        } else {
            let deps = db.symbol_dependencies(target)?;
            let refs = db.symbol_references(target)?;
            output["dependencies"] = serde_json::json!(deps);
            output["dependents"] = serde_json::json!(
                refs.iter()
                    .map(|(path, line, col)| {
                        serde_json::json!({"file": path, "line": line, "column": col})
                    })
                    .collect::<Vec<_>>()
            );
        }

        let formatted = if compact {
            serde_json::to_string(&output)?
        } else {
            serde_json::to_string_pretty(&output)?
        };
        println!("{}", formatted);
    } else {
        if is_file {
            let deps = db.file_dependencies(target)?;
            let dependents = db.file_dependents(target)?;

            if !deps.is_empty() {
                println!("Dependencies (what {} imports):", target);
                for dep in &deps {
                    println!("  {} -> {} (import)", target, dep);
                }
            }

            if !dependents.is_empty() {
                println!("Dependents (what imports {}):", target);
                for dep in &dependents {
                    println!("  {} -> {} (import)", dep, target);
                }
            }

            if deps.is_empty() && dependents.is_empty() {
                println!("No dependencies found for {}", target);
            }
        } else {
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
