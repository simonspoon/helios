use anyhow::{Context, Result};
use std::time::Instant;

use crate::db::Database;
use crate::errors::NoIndexError;
use crate::git;
use crate::indexer;

pub fn run(json: bool, compact: bool, quiet: bool) -> Result<()> {
    let cwd = std::env::current_dir().context("getting current directory")?;
    let db_path = cwd.join(".helios/index.db");

    if !db_path.exists() {
        return Err(NoIndexError.into());
    }

    let db = Database::open(&db_path).context("opening database")?;

    if !git::is_git_repo() {
        // No git — do a full re-index
        if !quiet && !json {
            println!("Not a git repo — performing full re-index");
        }
        let start = Instant::now();
        let stats = indexer::index_full(&db, &cwd)?;
        let elapsed = start.elapsed();

        print_stats(&stats, elapsed, json, compact, quiet)?;
        return Ok(());
    }

    let last_commit = db.get_metadata("last_indexed_commit")?;

    let (modified, deleted) = match &last_commit {
        Some(commit) => git::changed_files(commit)?,
        None => {
            // No previous commit stored — full re-index
            if !quiet && !json {
                println!("No previous index commit — performing full re-index");
            }
            let start = Instant::now();
            let stats = indexer::index_full(&db, &cwd)?;
            let elapsed = start.elapsed();

            if let Some(commit) = git::head_commit()? {
                db.set_metadata("last_indexed_commit", &commit)?;
            }

            print_stats(&stats, elapsed, json, compact, quiet)?;
            return Ok(());
        }
    };

    let total_changes = modified.len() + deleted.len();
    if total_changes == 0 {
        if !quiet {
            if json {
                let output = serde_json::json!({
                    "status": "up_to_date",
                    "files_indexed": 0,
                    "files_deleted": 0,
                });
                let formatted = if compact {
                    serde_json::to_string(&output)?
                } else {
                    serde_json::to_string_pretty(&output)?
                };
                println!("{}", formatted);
            } else {
                println!("Index is up to date");
            }
        }
        return Ok(());
    }

    let start = Instant::now();
    let stats = indexer::index_incremental(&db, &cwd, &modified, &deleted)?;
    let elapsed = start.elapsed();

    // Update stored commit
    if let Some(commit) = git::head_commit()? {
        db.set_metadata("last_indexed_commit", &commit)?;
    }

    print_stats(&stats, elapsed, json, compact, quiet)?;
    Ok(())
}

fn print_stats(
    stats: &indexer::IndexStats,
    elapsed: std::time::Duration,
    json: bool,
    compact: bool,
    quiet: bool,
) -> Result<()> {
    if quiet {
        return Ok(());
    }
    if json {
        let output = serde_json::json!({
            "files_indexed": stats.files_indexed,
            "files_deleted": stats.files_deleted,
            "files_errored": stats.files_errored,
            "symbols_found": stats.symbols_found,
            "imports_found": stats.imports_found,
            "elapsed_ms": elapsed.as_millis(),
        });
        let formatted = if compact {
            serde_json::to_string(&output)?
        } else {
            serde_json::to_string_pretty(&output)?
        };
        println!("{}", formatted);
    } else {
        println!(
            "Updated: {} files indexed, {} deleted ({} symbols, {} imports) in {:.2}s",
            stats.files_indexed,
            stats.files_deleted,
            stats.symbols_found,
            stats.imports_found,
            elapsed.as_secs_f64()
        );
        if stats.files_errored > 0 {
            println!("{} files had errors", stats.files_errored);
        }
    }
    Ok(())
}
