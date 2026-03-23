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
        let parts: Vec<&str> = line.splitn(2, '\t').collect();
        if parts.len() != 2 {
            continue;
        }
        let status = parts[0].trim();
        let file = parts[1].trim().to_string();

        match status {
            "D" => deleted.push(file),
            _ => modified.push(file), // A, M, R, C, etc.
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
