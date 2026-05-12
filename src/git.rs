use std::path::Path;
use std::process::Stdio;

use thiserror::Error;
use tokio::process::Command;

#[derive(Debug, Error)]
pub enum GitError {
    #[error("git exited with status {status}: {stderr}")]
    NonZeroExit { status: i32, stderr: String },
    #[error("failed to spawn git: {0}")]
    Spawn(String),
}

/// `git fetch origin --prune` in the user's checkout. Done by the poller so
/// that concurrent agents don't race on `.git/objects` / `.git/refs` locks
/// each tick.
pub async fn fetch_origin_prune(repo_path: &Path) -> Result<(), GitError> {
    let output = Command::new("git")
        .args(["fetch", "origin", "--prune"])
        .current_dir(repo_path)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .await
        .map_err(|e| GitError::Spawn(e.to_string()))?;
    if !output.status.success() {
        return Err(GitError::NonZeroExit {
            status: output.status.code().unwrap_or(-1),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        });
    }
    Ok(())
}
