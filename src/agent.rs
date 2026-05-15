use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Context;
use thiserror::Error;
use tokio::sync::Mutex;

use crate::merger::cleanup_worktree_path;
use crate::tmux::{has_session, kill_session, new_detached_session, NewSessionArgs, TmuxError};
use crate::types::{AuthMode, PrEvent, RepoConfig, RepoId};

#[derive(Debug, Clone)]
pub struct ActiveSession {
    pub repo_id: RepoId,
    pub pr_number: i64,
    pub session_name: String,
    pub spawn_head_sha: String,
    #[allow(dead_code)]
    pub main_sha: String,
    pub prompt_file: PathBuf,
    pub repo_path: PathBuf,
    pub worktree_base: PathBuf,
    pub worktree_path: PathBuf,
    /// Per-invocation log capturing the agent's stdout+stderr plus a trailing
    /// `EXIT: <n>` line. Persistent — kept on close/sweep/shutdown so failed
    /// runs can be inspected after the tmux session is gone.
    pub log_file: PathBuf,
    pub started_at_ms: u128,
    pub agent_name: String,
}

#[derive(Debug, Error)]
pub enum SpawnError {
    #[error("write prompt file: {0}")]
    PromptWrite(String),
    #[error("prepare log file: {0}")]
    LogSetup(String),
    #[error("tmux: {0}")]
    Tmux(#[from] TmuxError),
}

pub struct AgentRunner {
    sessions: Mutex<HashMap<(RepoId, i64), ActiveSession>>,
}

pub fn session_name_for_pr(repo_id: &RepoId, pr_number: i64) -> String {
    format!("pr-manager-{}-pr-{pr_number}", repo_id.for_tmux())
}

impl AgentRunner {
    pub fn new() -> Self {
        Self {
            sessions: Mutex::new(HashMap::new()),
        }
    }

    /// Snapshot of currently-tracked sessions for one repo.
    pub async fn active_for_repo(&self, repo_id: &RepoId) -> Vec<ActiveSession> {
        self.sessions
            .lock()
            .await
            .values()
            .filter(|s| &s.repo_id == repo_id)
            .cloned()
            .collect()
    }

    /// Start a detached tmux session running the configured agent.
    pub async fn spawn(
        &self,
        repo: &RepoConfig,
        event: &PrEvent,
        prompt: &str,
        worktree_path: PathBuf,
    ) -> Result<(), SpawnError> {
        let name = session_name_for_pr(&repo.repo_id, event.pr.number);

        // Drop any stale session with the same name (e.g. from a prior crash).
        kill_session(&name).await;

        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or_default();
        let prompt_file = std::env::temp_dir().join(format!(
            "pr-manager-{}-pr-{}-{}.prompt",
            repo.repo_id.for_tmux(),
            event.pr.number,
            now_ms
        ));

        if let Err(e) = write_prompt_file(&prompt_file, prompt) {
            cleanup_worktree_path(&repo.repo_path, &repo.worktree_base, &worktree_path).await;
            return Err(SpawnError::PromptWrite(e.to_string()));
        }

        let log_file = repo
            .logs_base
            .join(format!("pr-{}-{}.log", event.pr.number, now_ms));
        if let Err(e) = std::fs::create_dir_all(&repo.logs_base) {
            drop_prompt_file(&prompt_file);
            cleanup_worktree_path(&repo.repo_path, &repo.worktree_base, &worktree_path).await;
            return Err(SpawnError::LogSetup(format!(
                "create {}: {}",
                repo.logs_base.display(),
                e
            )));
        }

        let command = build_shell_command(repo, &prompt_file, &log_file);
        let res = new_detached_session(NewSessionArgs {
            name: &name,
            cwd: &worktree_path,
            command: &command,
        })
        .await;

        if let Err(e) = res {
            drop_prompt_file(&prompt_file);
            cleanup_worktree_path(&repo.repo_path, &repo.worktree_base, &worktree_path).await;
            return Err(SpawnError::Tmux(e));
        }

        let session = ActiveSession {
            repo_id: repo.repo_id.clone(),
            pr_number: event.pr.number,
            session_name: name.clone(),
            spawn_head_sha: event.pr.head_sha.clone(),
            main_sha: event.main_sha.clone(),
            prompt_file: prompt_file.clone(),
            repo_path: repo.repo_path.clone(),
            worktree_base: repo.worktree_base.clone(),
            worktree_path: worktree_path.clone(),
            log_file: log_file.clone(),
            started_at_ms: now_ms,
            agent_name: repo.agent.name.clone(),
        };

        self.sessions
            .lock()
            .await
            .insert((repo.repo_id.clone(), event.pr.number), session);

        tracing::info!(
            repo = %repo.github_repo,
            pr = event.pr.number,
            session = %name,
            agent = %repo.agent.name,
            attach = %format!("tmux attach -t {name}"),
            cwd = %worktree_path.display(),
            log_file = %log_file.display(),
            command = %command,
            "spawned agent session in tmux"
        );

        Ok(())
    }

    /// Force-close the session for a PR (kill tmux, drop registry entry).
    pub async fn close(&self, repo_id: &RepoId, pr_number: i64, reason: &str) -> bool {
        let sess = self
            .sessions
            .lock()
            .await
            .remove(&(repo_id.clone(), pr_number));
        let sess = match sess {
            Some(s) => s,
            None => return false,
        };
        kill_session(&sess.session_name).await;
        drop_prompt_file(&sess.prompt_file);
        cleanup_worktree_path(&sess.repo_path, &sess.worktree_base, &sess.worktree_path).await;
        tracing::info!(
            repo = %repo_id.as_str(),
            pr = pr_number,
            session = %sess.session_name,
            agent = %sess.agent_name,
            log_file = %sess.log_file.display(),
            reason = %reason,
            "closed agent session"
        );
        true
    }

    /// Drop registry entries whose tmux session no longer exists. Returns the
    /// `(repo_id, pr_number)` pairs that were swept.
    pub async fn sweep(&self) -> Vec<(RepoId, i64)> {
        let snapshot: Vec<ActiveSession> = self.sessions.lock().await.values().cloned().collect();

        let mut exited = Vec::new();
        for sess in snapshot {
            if has_session(&sess.session_name).await {
                continue;
            }
            // Re-check + remove under the lock to avoid racing with spawn.
            let removed = self
                .sessions
                .lock()
                .await
                .remove(&(sess.repo_id.clone(), sess.pr_number));
            if let Some(removed) = removed {
                drop_prompt_file(&removed.prompt_file);
                cleanup_worktree_path(
                    &removed.repo_path,
                    &removed.worktree_base,
                    &removed.worktree_path,
                )
                .await;
                exited.push((removed.repo_id.clone(), removed.pr_number));
                let now_ms = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map(|d| d.as_millis())
                    .unwrap_or(removed.started_at_ms);
                let exit_code = read_exit_code(&removed.log_file);
                tracing::info!(
                    repo = %removed.repo_id.as_str(),
                    pr = removed.pr_number,
                    session = %removed.session_name,
                    agent = %removed.agent_name,
                    ran_for_ms = (now_ms - removed.started_at_ms) as u64,
                    log_file = %removed.log_file.display(),
                    exit_code = exit_code.map(|c| c as i64),
                    "agent session ended"
                );
            }
        }
        exited
    }

    /// Kill all tracked sessions and clean up prompt files.
    pub async fn shutdown(&self) {
        let drained: Vec<ActiveSession> = {
            let mut guard = self.sessions.lock().await;
            let v = guard.values().cloned().collect();
            guard.clear();
            v
        };
        for sess in drained {
            kill_session(&sess.session_name).await;
            drop_prompt_file(&sess.prompt_file);
            cleanup_worktree_path(&sess.repo_path, &sess.worktree_base, &sess.worktree_path).await;
            tracing::info!(
                repo = %sess.repo_id.as_str(),
                pr = sess.pr_number,
                session = %sess.session_name,
                agent = %sess.agent_name,
                log_file = %sess.log_file.display(),
                "killed agent session on shutdown"
            );
        }
    }

    pub async fn active_count(&self) -> usize {
        self.sessions.lock().await.len()
    }
}

fn build_shell_command(
    repo: &RepoConfig,
    prompt_file: &std::path::Path,
    log_file: &std::path::Path,
) -> String {
    let mut parts = Vec::with_capacity(7 + repo.agent.args.len());
    parts.push(shell_quote(&repo.agent.bin));
    for a in &repo.agent.args {
        parts.push(shell_quote(a));
    }
    parts.push("<".to_string());
    parts.push(shell_quote(&prompt_file.to_string_lossy()));
    parts.push(">".to_string());
    let log_quoted = shell_quote(&log_file.to_string_lossy());
    parts.push(log_quoted.clone());
    parts.push("2>&1".to_string());
    let main_cmd = parts.join(" ");
    // Tmux runs the command via the user's shell, so `;` and `>>` work. We
    // append the exit code so sweep() can recover success/failure even
    // though tmux discards the process status itself.
    let auth_prelude = match repo.auth_mode {
        // Strip API-key env vars so the agent CLI falls through to its
        // on-disk OAuth credentials. These leak in easily via the parent
        // shell or a wrapping agent session; without scrubbing, claude/codex
        // can silently bill an API account instead of the user's OAuth
        // subscription.
        AuthMode::OAuth => "unset ANTHROPIC_API_KEY ANTHROPIC_AUTH_TOKEN OPENAI_API_KEY; ",
        // Inherit whatever the parent process exposes.
        AuthMode::Api => "",
    };
    format!("{auth_prelude}{main_cmd}; echo \"EXIT: $?\" >> {log_quoted}")
}

/// POSIX-style single-quote shell escaping. Wraps the value in `'...'` and
/// escapes embedded single quotes with the standard `'\''` dance.
fn shell_quote(value: &str) -> String {
    let escaped = value.replace('\'', "'\\''");
    format!("'{escaped}'")
}

fn write_prompt_file(path: &std::path::Path, contents: &str) -> anyhow::Result<()> {
    use std::fs::OpenOptions;
    use std::io::Write;
    #[cfg(unix)]
    use std::os::unix::fs::OpenOptionsExt;

    let mut opts = OpenOptions::new();
    opts.create(true).write(true).truncate(true);
    #[cfg(unix)]
    {
        opts.mode(0o600);
    }
    let mut f = opts.open(path).with_context(|| format!("open {path:?}"))?;
    f.write_all(contents.as_bytes())
        .with_context(|| format!("write {path:?}"))?;
    Ok(())
}

fn drop_prompt_file(path: &std::path::Path) {
    let _ = std::fs::remove_file(path);
}

/// Recover the agent's exit status from the log file's trailing
/// `EXIT: <n>` line (written by `build_shell_command`'s shell wrapper).
/// Returns `None` if the log is missing, unreadable, or the marker is
/// absent — typically because the session was force-killed before the
/// shell could emit it, or because the log was rotated away by a user.
fn read_exit_code(path: &std::path::Path) -> Option<i32> {
    let contents = std::fs::read_to_string(path).ok()?;
    let last = contents.lines().rev().find(|l| !l.trim().is_empty())?;
    last.trim().strip_prefix("EXIT:")?.trim().parse().ok()
}
