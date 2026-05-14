use std::path::PathBuf;
use thiserror::Error;

#[derive(Debug, Clone)]
pub struct AgentConfig {
    /// Resolved harness name. Built-ins are `claude` and `codex`; custom
    /// harness names come from `[harnesses.<name>]` in the TOML config.
    pub name: String,
    pub bin: String,
    /// Arguments passed after the binary. The prompt is always redirected to
    /// stdin, matching the built-in Claude/Codex harnesses.
    pub args: Vec<String>,
}

/// How the spawned agent authenticates against its provider.
///
/// `OAuth` is the default and scrubs API-key env vars (`ANTHROPIC_API_KEY`,
/// `ANTHROPIC_AUTH_TOKEN`, `OPENAI_API_KEY`) from the spawned shell so the
/// agent CLI falls through to its on-disk OAuth credentials. This avoids the
/// surprising case where a key leaks in from the parent shell or a wrapping
/// agent session and silently bills against an API account.
///
/// `Api` keeps the parent process env intact, so whatever key the agent
/// inherits is what it uses. Pick this when you actually want API-key
/// billing or have set an explicit key for pr-manager's child agents.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthMode {
    OAuth,
    Api,
}

impl AuthMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            AuthMode::OAuth => "oauth",
            AuthMode::Api => "api",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogLevel {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
    Fatal,
}

/// Slug identifying one watched repo, used as the registry key, the cache
/// directory name, and the prefix in tmux session names. Derived from the
/// `owner/name` pair as `owner__name`.
#[derive(Debug, Clone, Hash, PartialEq, Eq)]
pub struct RepoId(String);

impl RepoId {
    pub fn new(owner: &str, name: &str) -> Self {
        Self(format!("{owner}__{name}"))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Form safe to embed in a tmux session name. tmux happily takes most
    /// printable chars but `/`, `:`, and `.` are uncomfortable in target
    /// specifiers — sanitize anything outside `[A-Za-z0-9_-]` to `-`.
    pub fn for_tmux(&self) -> String {
        self.0
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                    c
                } else {
                    '-'
                }
            })
            .collect()
    }
}

/// Process-wide settings that aren't per-repo.
#[derive(Debug, Clone)]
pub struct Globals {
    pub log_level: LogLevel,
}

/// Top-level configuration: the process-wide bits plus one entry per watched
/// repo. Always at least one repo (validated by the loader).
#[derive(Debug, Clone)]
pub struct Config {
    pub globals: Globals,
    pub repos: Vec<RepoConfig>,
}

/// Resolved per-repo configuration. Defaults from the global config block
/// have already been merged in; per-repo overrides win.
#[derive(Debug, Clone)]
pub struct RepoConfig {
    pub repo_id: RepoId,
    pub github_repo: String,
    pub github_owner: String,
    pub github_name: String,
    /// Resolved from an env var (default `GITHUB_TOKEN`, or whatever
    /// `token_env` names). The token itself is never stored in the config
    /// file.
    pub github_token: String,
    pub poll_interval_seconds: u64,
    pub recent_merges_limit: u32,
    /// Absolute path under which pr-manager creates throwaway git worktrees.
    /// Always outside any user repository. Each PR gets `${worktreeBase}/pr-<n>`.
    pub worktree_base: PathBuf,
    /// Absolute path under which pr-manager writes one log file per agent
    /// invocation (`pr-<n>-<ms>.log`). Persistent — never auto-cleaned — so
    /// failed runs can be inspected after the tmux session is gone.
    pub logs_base: PathBuf,
    /// Absolute path to a local checkout of this repo. The configured agent
    /// is spawned with this as cwd so its `git`/`gh` commands operate on the
    /// right repository.
    pub repo_path: PathBuf,
    pub agent: AgentConfig,
    pub auth_mode: AuthMode,
    /// Lowercased allowlist of PR author logins. Empty means "no filter,
    /// process every open auto-merge PR." Non-empty means "only process PRs
    /// whose author.login (case-insensitive) matches an entry." Lets two
    /// operators run pr-manager against the same repo without stepping on
    /// each other's PRs.
    pub pr_authors: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct RecentMerge {
    pub number: i64,
    #[allow(dead_code)]
    pub title: String,
    #[allow(dead_code)]
    pub merged_at: String,
}

#[derive(Debug, Clone)]
pub struct OpenAutoMergePr {
    pub number: i64,
    pub title: String,
    pub body: String,
    pub head_branch: String,
    pub head_sha: String,
    pub head_repo_id: i64,
    pub base_repo_id: i64,
    pub base_branch: String,
    /// GitHub login of the PR author, when the API returned it. Used by
    /// per-repo author allowlists; `None` is treated as "unknown author" and
    /// is filtered out when an allowlist is configured.
    pub author_login: Option<String>,
}

#[derive(Debug, Clone)]
pub struct PrEvent {
    pub pr: OpenAutoMergePr,
    pub main_sha: String,
    pub recent: Vec<RecentMerge>,
}

#[derive(Debug, Error)]
#[error("{message}")]
pub struct ConfigError {
    pub message: String,
    pub issues: Vec<String>,
}

impl ConfigError {
    pub fn new(message: impl Into<String>, issues: Vec<String>) -> Self {
        Self {
            message: message.into(),
            issues,
        }
    }
}

#[derive(Debug, Error)]
#[error("{message}")]
pub struct GitHubError {
    pub message: String,
    pub status: Option<u16>,
    pub endpoint: String,
}

impl GitHubError {
    pub fn new(
        message: impl Into<String>,
        status: Option<u16>,
        endpoint: impl Into<String>,
    ) -> Self {
        Self {
            message: message.into(),
            status,
            endpoint: endpoint.into(),
        }
    }
}
