use anyhow::{Context, Result};
use std::time::Instant;

use crate::db::Database;
use crate::git;
use crate::indexer;

pub fn run(json: bool, compact: bool, quiet: bool) -> Result<()> {
    let cwd = std::env::current_dir().context("getting current directory")?;
    let helios_dir = cwd.join(".helios");

    // Create .helios directory
    std::fs::create_dir_all(&helios_dir).context("creating .helios directory")?;

    let db_path = helios_dir.join("index.db");
    let db = Database::open(&db_path).context("opening database")?;

    let start = Instant::now();
    let stats = indexer::index_full(&db, &cwd)?;
    let elapsed = start.elapsed();

    // Store current git commit if in a git repo
    if git::is_git_repo()
        && let Some(commit) = git::head_commit()?
    {
        db.set_metadata("last_indexed_commit", &commit)?;
    }

    // Report totals from DB (not just newly indexed counts, which are 0 on cache hits)
    let total_files = db.file_count()?;
    let total_symbols = db.symbol_count()?;

    if !quiet {
        if json {
            let output = serde_json::json!({
                "files_indexed": stats.files_indexed,
                "files_unchanged": total_files as usize - stats.files_indexed,
                "files_errored": stats.files_errored,
                "total_files": total_files,
                "total_symbols": total_symbols,
                "elapsed_ms": elapsed.as_millis(),
            });
            let formatted = if compact {
                serde_json::to_string(&output)?
            } else {
                serde_json::to_string_pretty(&output)?
            };
            println!("{}", formatted);
        } else {
            if stats.symbols_found > 0 {
                println!(
                    "Indexed {} files ({} symbols) in {:.2}s",
                    stats.files_indexed,
                    stats.symbols_found,
                    elapsed.as_secs_f64()
                );
            } else {
                println!(
                    "Index up to date ({} files, {} symbols) in {:.2}s",
                    total_files,
                    total_symbols,
                    elapsed.as_secs_f64()
                );
            }
            if stats.files_errored > 0 {
                println!(
                    "{} files had errors (see warnings above)",
                    stats.files_errored
                );
            }
            println!("Database: {}", db_path.display());

            // Suggest adding .helios to .gitignore
            let gitignore = cwd.join(".gitignore");
            if gitignore.exists() {
                let content = std::fs::read_to_string(&gitignore).unwrap_or_default();
                if !content.contains(".helios") {
                    println!("\nTip: Add .helios/ to your .gitignore");
                }
            } else {
                println!("\nTip: Create a .gitignore with .helios/ entry");
            }
        }
    }

    Ok(())
}
