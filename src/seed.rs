//! Seeding a linked worktree's index from the main worktree's.
//!
//! The index stores repo-relative paths, so a copy of the main worktree's
//! `.helios/index.db` is valid in any checkout of the same repo; the normal
//! incremental update then brings it in line with the worktree's files.

use anyhow::{Context, Result};
use std::path::Path;
use std::process::Command;

use crate::commands;
use crate::git;

/// If `cwd` is the top of a linked git worktree with no index while the main
/// worktree has one, copy the main index here and bring it up to date.
/// Returns Ok(false) when not applicable (the caller then reports no index).
pub fn seed_from_main_worktree(cwd: &Path) -> Result<bool> {
    let db_path = cwd.join(".helios/index.db");
    if db_path.exists() {
        return Ok(false);
    }
    let Some(main_root) = git::linked_worktree_main_root(cwd) else {
        return Ok(false);
    };
    let main_db = main_root.join(".helios/index.db");
    if !main_db.is_file() {
        return Ok(false);
    }

    std::fs::create_dir_all(cwd.join(".helios")).context("creating .helios directory")?;
    // VACUUM INTO gives a consistent snapshot even with a WAL in play. The
    // temp name is per-process so concurrent seeders do not collide.
    let tmp = cwd.join(format!(".helios/index.db.seeding.{}", std::process::id()));
    let _ = std::fs::remove_file(&tmp);
    let cleanup_tmp = || {
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", tmp.display()));
        }
    };
    let prepare = || -> Result<()> {
        let src = rusqlite::Connection::open_with_flags(
            &main_db,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )?;
        src.execute("VACUUM INTO ?1", [tmp.to_string_lossy().as_ref()])
            .context("copying the main worktree's index")?;
        drop(src);

        // The recorded commit is shared by both worktrees' history; if it is
        // not resolvable here, drop it so update falls back to a full re-index.
        let db = crate::db::Database::open(&tmp).context("opening seeded index")?;
        if let Some(commit) = db.get_metadata("last_indexed_commit")?
            && !commit_exists(cwd, &commit)
        {
            db.delete_metadata("last_indexed_commit")?;
        }
        Ok(())
    };
    if let Err(e) = prepare() {
        cleanup_tmp();
        return Err(e);
    }

    // hard_link fails if the target exists, so a concurrent seeder's db is
    // never clobbered; in that case use theirs.
    let linked = std::fs::hard_link(&tmp, &db_path);
    cleanup_tmp();
    match linked {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => return Ok(false),
        Err(e) => return Err(e).context("installing the seeded index"),
    }

    eprintln!(
        "note: seeded .helios index from main worktree {}",
        main_root.display()
    );
    // update::run reads cwd/.helios/index.db, so the db is already in place;
    // on failure remove it so later commands do not trust a half-updated seed.
    if let Err(e) = commands::update::run(false, false, true) {
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", db_path.display()));
        }
        return Err(e).context("updating the seeded index");
    }
    Ok(true)
}

fn commit_exists(cwd: &Path, commit: &str) -> bool {
    Command::new("git")
        .args(["cat-file", "-e", &format!("{commit}^{{commit}}")])
        .current_dir(cwd)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}
