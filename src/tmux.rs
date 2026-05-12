use std::path::Path;
use std::process::Stdio;

use thiserror::Error;
use tokio::process::Command;

/// Errors raised when tmux invocations fail in a way that can't be silently
/// folded into "no, that session doesn't exist." Used only for the operations
/// where we actually care about the failure (`new-session`); the rest treat
/// any non-zero exit as a no-op match for the TS implementation.
#[derive(Debug, Error)]
pub enum TmuxError {
    #[error("tmux exited with status {status}: {stderr}")]
    NonZeroExit { status: i32, stderr: String },
    #[error("failed to spawn tmux: {0}")]
    Spawn(String),
}

pub async fn tmux_available() -> bool {
    match Command::new("tmux")
        .arg("-V")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
    {
        Ok(status) => status.success(),
        Err(_) => false,
    }
}

/// Note on `=name` syntax: tmux target specifiers default to prefix match. The
/// `=` prefix forces an exact match so a session named "pr-manager-pr-12"
/// cannot be confused with "pr-manager-pr-1".
pub async fn has_session(name: &str) -> bool {
    let target = format!("={name}");
    match Command::new("tmux")
        .args(["has-session", "-t", &target])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
    {
        Ok(status) => status.success(),
        Err(_) => false,
    }
}

pub async fn kill_session(name: &str) {
    let target = format!("={name}");
    // Session may not exist; treat as no-op.
    let _ = Command::new("tmux")
        .args(["kill-session", "-t", &target])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await;
}

pub struct NewSessionArgs<'a> {
    pub name: &'a str,
    pub cwd: &'a Path,
    /// Shell command to run in the session. Passed verbatim to tmux.
    pub command: &'a str,
}

pub async fn new_detached_session(args: NewSessionArgs<'_>) -> Result<(), TmuxError> {
    let cwd = args.cwd.to_string_lossy().into_owned();
    let output = Command::new("tmux")
        .args([
            "new-session",
            "-d",
            "-s",
            args.name,
            "-c",
            &cwd,
            args.command,
        ])
        .output()
        .await
        .map_err(|e| TmuxError::Spawn(e.to_string()))?;
    if !output.status.success() {
        return Err(TmuxError::NonZeroExit {
            status: output.status.code().unwrap_or(-1),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        });
    }
    Ok(())
}
