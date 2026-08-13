use anyhow::{Context, Result};
use std::path::{Component, Path};
use std::process::Command;

/// Get the current HEAD commit hash
pub fn head_commit() -> Result<Option<String>> {
    let output = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .output()
        .context("running git rev-parse HEAD")?;

    if output.status.success() {
        let hash = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if hash.is_empty() {
            Ok(None)
        } else {
            Ok(Some(hash))
        }
    } else {
        Ok(None)
    }
}

/// The index root's location inside the repo, as a `/`-terminated prefix that
/// git's paths carry and the index's do not (empty when the index is rooted at
/// the repo root). An index root outside the repo, or a repo root git won't
/// report, yields no prefix — the paths are then left as git spelled them.
fn index_prefix(root: &Path) -> String {
    let Ok(output) = Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .current_dir(root)
        .output()
    else {
        return String::new();
    };
    if !output.status.success() {
        return String::new();
    }

    let top = Path::new(String::from_utf8_lossy(&output.stdout).trim()).to_path_buf();
    let top = top.canonicalize().unwrap_or(top);
    let root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());

    let Ok(rel) = root.strip_prefix(&top) else {
        return String::new();
    };
    let segments: Vec<String> = rel
        .components()
        .filter_map(|c| match c {
            Component::Normal(s) => Some(s.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect();
    if segments.is_empty() {
        String::new()
    } else {
        format!("{}/", segments.join("/"))
    }
}

/// Get files changed between a commit and the current working tree, as paths
/// relative to `root`.
///
/// git reports every path relative to the repo root, but an index built in a
/// subdirectory stores paths relative to that subdirectory. Rebasing here is
/// what lets a caller find the path in the index at all; a path outside `root`
/// belongs to no index entry and is dropped.
///
/// Returns (added/modified, deleted) file paths
pub fn changed_files(since_commit: &str, root: &Path) -> Result<(Vec<String>, Vec<String>)> {
    // `--no-relative` because a user's `diff.relative = true` would otherwise
    // scope the diff to the cwd and pre-strip the prefix, leaving the rebase
    // below to drop every path.
    let output = Command::new("git")
        .args(["diff", "--no-relative", "--name-status", since_commit])
        .current_dir(root)
        .output()
        .context("running git diff")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("git diff failed: {}", stderr);
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut modified = Vec::new();
    let mut deleted = Vec::new();

    for line in stdout.lines() {
        let mut fields = line.split('\t');
        let Some(status) = fields.next().map(str::trim) else {
            continue;
        };
        let paths: Vec<String> = fields.map(|f| f.trim().to_string()).collect();
        let Some(first) = paths.first().cloned() else {
            continue;
        };

        match status.chars().next() {
            Some('D') => deleted.push(first),
            // A rename or copy is reported as `R100<TAB>old<TAB>new`: the index
            // has to read the new path, and for a rename forget the old one —
            // treating the pair as one modified path leaves both wrong.
            Some(c @ ('R' | 'C')) if paths.len() == 2 => {
                if c == 'R' {
                    deleted.push(first);
                }
                modified.push(paths[1].clone());
            }
            _ => modified.push(first), // A, M, T, etc.
        }
    }

    let prefix = index_prefix(root);
    if !prefix.is_empty() {
        let rebase = |paths: Vec<String>| -> Vec<String> {
            paths
                .into_iter()
                .filter_map(|p| p.strip_prefix(&prefix).map(str::to_string))
                .collect()
        };
        modified = rebase(modified);
        deleted = rebase(deleted);
    }

    Ok((modified, deleted))
}

/// Check if current directory is a git repository
pub fn is_git_repo() -> bool {
    Command::new("git")
        .args(["rev-parse", "--git-dir"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Get all tracked files in the repository
#[allow(dead_code)]
pub fn tracked_files() -> Result<Vec<String>> {
    let output = Command::new("git")
        .args(["ls-files"])
        .output()
        .context("running git ls-files")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("git ls-files failed: {}", stderr);
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    Ok(stdout.lines().map(|l| l.to_string()).collect())
}
