use anyhow::{Context, Result};

use crate::db::Database;
use crate::git;

pub fn run(json: bool, compact: bool) -> Result<()> {
    let cwd = std::env::current_dir().context("getting current directory")?;
    let db_path = cwd.join(".helios/index.db");

    if !db_path.exists() {
        if json {
            let output = serde_json::json!({"indexed": false});
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

    let db = Database::open(&db_path).context("opening database")?;

    let file_count = db.file_count()?;
    let symbol_count = db.symbol_count()?;
    let import_count = db.import_count()?;
    let by_lang = db.files_by_language()?;
    let last_commit = db.get_metadata("last_indexed_commit")?;

    // Git staleness info
    let in_git = git::is_git_repo();
    let head = if in_git { git::head_commit()? } else { None };

    let stale_count = match (&last_commit, in_git) {
        (Some(commit), true) => {
            let (modified, deleted) = git::changed_files(commit)?;
            modified.len() + deleted.len()
        }
        _ => 0,
    };

    if json {
        let languages: serde_json::Value = by_lang
            .iter()
            .map(|(lang, count)| {
                serde_json::json!({
                    "language": lang,
                    "files": count,
                })
            })
            .collect();

        let mut output = serde_json::json!({
            "indexed": true,
            "db_path": ".helios/index.db",
            "files": file_count,
            "symbols": symbol_count,
            "imports": import_count,
            "languages": languages,
        });

        if let Some(ref commit) = last_commit {
            output["last_indexed_commit"] = serde_json::json!(commit);
        }
        if let Some(ref h) = head {
            output["head_commit"] = serde_json::json!(h);
        }
        if last_commit.is_some() && in_git {
            output["stale_files"] = serde_json::json!(stale_count);
        }

        let formatted = if compact {
            serde_json::to_string(&output)?
        } else {
            serde_json::to_string_pretty(&output)?
        };
        println!("{}", formatted);
    } else {
        println!("Index: .helios/index.db");
        println!(
            "Files: {} | Symbols: {} | Imports: {}",
            file_count, symbol_count, import_count
        );

        if !by_lang.is_empty() {
            let langs: Vec<String> = by_lang
                .iter()
                .map(|(lang, count)| format!("{} ({})", lang, count))
                .collect();
            println!("Languages: {}", langs.join(", "));
        }

        if let Some(ref commit) = last_commit {
            println!("Last indexed commit: {}", &commit[..commit.len().min(7)]);
        }
        if let Some(ref h) = head {
            println!("Current HEAD: {}", &h[..h.len().min(7)]);
        }
        if last_commit.is_some() && in_git {
            println!("Stale files: {}", stale_count);
        }
    }

    Ok(())
}
