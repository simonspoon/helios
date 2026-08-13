use anyhow::{Context, Result};
use std::time::{Duration, Instant};

use crate::db::Database;
use crate::git;
use crate::indexer;
use crate::sidecar;

pub fn run(json: bool, compact: bool, quiet: bool, timeout_secs: u64) -> Result<()> {
    let cwd = std::env::current_dir().context("getting current directory")?;
    let helios_dir = cwd.join(".helios");

    // Create .helios directory
    std::fs::create_dir_all(&helios_dir).context("creating .helios directory")?;

    let db_path = helios_dir.join("index.db");
    let db = Database::open(&db_path).context("opening database")?;

    // Roslyn sidecar: detect once per run, analyze before the walk. Any
    // failure in the ladder (dotnet absent, helper missing, ping fails,
    // analyze errors or times out) falls back to the tree-sitter path with a
    // single warning and init still succeeds (P3-M1, P3-M2, P3-S1).
    // The walk's indexed .cs set: the helper reports on exactly these files,
    // keeping its output aligned with what ingest_semantic can stamp. Snapshot
    // taken before the analyze/walk, so a .cs file created mid-run is indexed
    // without semantic references until the next init — the walk reports such
    // files and a warning below surfaces them. An empty set skips the
    // sidecar entirely — no dotnet spawns, nothing the ingest could keep.
    let cs_files = match indexer::indexed_csharp_files(&cwd) {
        Ok(files) => files,
        Err(e) => {
            eprintln!(
                "warning: listing C# files failed ({e:#}); resolving C# references with tree-sitter"
            );
            Vec::new()
        }
    };
    let semantic: Option<sidecar::AnalyzeOutput> = if cs_files.is_empty() {
        None
    } else {
        sidecar::detect().and_then(
            |s| match s.analyze(&cwd, &cs_files, Duration::from_secs(timeout_secs)) {
                Ok(output) => Some(output),
                Err(e) => {
                    eprintln!(
                        "warning: helios-roslyn analyze failed ({e:#}); resolving C# references with tree-sitter"
                    );
                    None
                }
            },
        )
    };

    let start = Instant::now();
    // In semantic mode the walk skips `.cs` reference resolution; the ingest
    // below stamps DocIds and inserts the exact reference set instead, and
    // either way records the resolver provenance (P3-M3..M7).
    let cs_snapshot = semantic.is_some().then_some(cs_files.as_slice());
    let stats = indexer::index_full(&db, &cwd, cs_snapshot)?;
    if !stats.cs_missing_from_snapshot.is_empty() {
        eprintln!(
            "warning: {} C# file(s) appeared after the Roslyn snapshot and have no semantic references until the next init: {}",
            stats.cs_missing_from_snapshot.len(),
            stats.cs_missing_from_snapshot.join(", ")
        );
    }
    indexer::ingest_semantic(&db, semantic.as_ref())?;
    // Needs the complete file set, so it runs after the walk.
    indexer::resolve_imports(&db)?;
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
    // Which resolver produced the C# references (P3-M7). Surfaced here, not
    // only in `status`, so a fallback is visible in the summary rather than in
    // a warning line that scrolls past.
    // Only meaningful when the repo actually has C# in it.
    let csharp_resolver = if cs_files.is_empty() {
        None
    } else {
        db.get_metadata("csharp_resolver")?
    };

    if !quiet {
        if json {
            let mut output = serde_json::json!({
                "files_indexed": stats.files_indexed,
                "files_unchanged": total_files as usize - stats.files_indexed,
                "files_errored": stats.files_errored,
                "total_files": total_files,
                "total_symbols": total_symbols,
                "elapsed_ms": elapsed.as_millis(),
            });
            if let Some(ref resolver) = csharp_resolver {
                output["csharp_resolver"] = serde_json::json!(resolver);
            }
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
            if let Some(ref resolver) = csharp_resolver {
                println!("C# resolver: {}", resolver);
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
