use anyhow::{Context, Result};
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

/// Get files changed between a commit and the current working tree
/// Returns (added/modified, deleted) file paths
pub fn changed_files(since_commit: &str) -> Result<(Vec<String>, Vec<String>)> {
    let output = Command::new("git")
        .args(["diff", "--name-status", since_commit])
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
