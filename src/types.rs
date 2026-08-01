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

/// A non-empty, trimmed GitHub label name used as a per-PR opt-in marker.
///
/// Constructed only through [`ManageLabel::new`], so an empty or
/// whitespace-only label cannot reach [`ManagedScope`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManageLabel(String);

impl ManageLabel {
    /// Trims surrounding whitespace and rejects a blank label.
    pub fn new(raw: &str) -> Option<Self> {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return None;
        }
        Some(Self(trimmed.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// GitHub label names are case-preserving but humans are not, so match
    /// case-insensitively — same rule as `pr_authors`.
    pub fn matches(&self, label: &str) -> bool {
        self.0.eq_ignore_ascii_case(label.trim())
    }
}

/// Which open PRs pr-manager treats as under its management for one repo.
///
/// Resolved once at config load. `manage_all_prs` overrides `manage_label`
/// there, so the ambiguous state "manage every PR *and* also match a label"
/// is unrepresentable here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ManagedScope {
    /// Default: only PRs with GitHub auto-merge enabled.
    AutoMergeOnly,
    /// Auto-merge PRs, plus any open PR carrying this opt-in label.
    AutoMergeOrLabel(ManageLabel),
    /// Every open PR, auto-merge or not (`manage_all_prs = true`).
    AllOpen,
}

impl ManagedScope {
    /// Whether this PR is under management, and why. `None` means "ignore it".
    pub fn admits(&self, pr: &OpenPr) -> Option<ManagedReason> {
        match self {
            ManagedScope::AutoMergeOnly => {
                pr.auto_merge_enabled.then_some(ManagedReason::AutoMerge)
            }
            ManagedScope::AutoMergeOrLabel(label) => {
                if pr.auto_merge_enabled {
                    Some(ManagedReason::AutoMerge)
                } else if pr.labels.iter().any(|l| label.matches(l)) {
                    Some(ManagedReason::OptInLabel)
                } else {
                    None
                }
            }
            ManagedScope::AllOpen => Some(if pr.auto_merge_enabled {
                ManagedReason::AutoMerge
            } else {
                ManagedReason::RepoOptIn
            }),
        }
    }

    /// Human-readable form for the startup log line.
    pub fn describe(&self) -> String {
        match self {
            ManagedScope::AutoMergeOnly => "auto_merge".to_string(),
            ManagedScope::AutoMergeOrLabel(label) => {
                format!("auto_merge|label:{}", label.as_str())
            }
            ManagedScope::AllOpen => "all_open".to_string(),
        }
    }
}

/// Why a PR ended up under management. Carried into logs so an operator can
/// tell an auto-merge PR from one that opted in some other way.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ManagedReason {
    /// GitHub auto-merge is enabled on the PR.
    AutoMerge,
    /// The PR carries the repo's configured opt-in label.
    OptInLabel,
    /// The repo opted every open PR in via `manage_all_prs`.
    RepoOptIn,
}

impl ManagedReason {
    pub fn as_str(&self) -> &'static str {
        match self {
            ManagedReason::AutoMerge => "auto_merge",
            ManagedReason::OptInLabel => "opt_in_label",
            ManagedReason::RepoOptIn => "repo_opt_in",
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
    /// process every managed PR." Non-empty means "only process PRs whose
    /// author.login (case-insensitive) matches an entry." Lets two operators
    /// run pr-manager against the same repo without stepping on each other's
    /// PRs. Applied *after* `managed_scope`.
    pub pr_authors: Vec<String>,
    /// Which open PRs this repo puts under management. Defaults to
    /// auto-merge-only; `manage_all_prs` / `manage_label` widen it.
    pub managed_scope: ManagedScope,
}

#[derive(Debug, Clone)]
pub struct RecentMerge {
    pub number: i64,
    #[allow(dead_code)]
    pub title: String,
    #[allow(dead_code)]
    pub merged_at: String,
}

/// One open PR as returned by the GitHub list endpoint, before any
/// management filtering. `ManagedScope::admits` decides whether pr-manager
/// acts on it.
#[derive(Debug, Clone)]
pub struct OpenPr {
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
    /// True when GitHub auto-merge is armed on the PR (`auto_merge != null`).
    pub auto_merge_enabled: bool,
    /// Label names currently on the PR, used to match the repo's opt-in
    /// `manage_label`.
    pub labels: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct PrEvent {
    pub pr: OpenPr,
    pub main_sha: String,
    pub recent: Vec<RecentMerge>,
    /// Why this PR is under management. Drives the prompt's framing — an
    /// auto-merge PR is being unblocked, an opted-in one is just being kept
    /// current with the default branch.
    pub managed_reason: ManagedReason,
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

#[cfg(test)]
mod tests {
    use super::*;

    fn pr(auto_merge_enabled: bool, labels: &[&str]) -> OpenPr {
        OpenPr {
            number: 1,
            title: "PR".into(),
            body: String::new(),
            head_branch: "feature".into(),
            head_sha: "deadbeef".into(),
            head_repo_id: 1,
            base_repo_id: 1,
            base_branch: "main".into(),
            author_login: Some("alice".into()),
            auto_merge_enabled,
            labels: labels.iter().map(|s| s.to_string()).collect(),
        }
    }

    fn label(raw: &str) -> ManageLabel {
        ManageLabel::new(raw).expect("test label must be non-empty")
    }

    #[test]
    fn manage_label_rejects_blank() {
        assert!(ManageLabel::new("").is_none());
        assert!(ManageLabel::new("   ").is_none());
        assert_eq!(label(" pr-manager ").as_str(), "pr-manager");
    }

    #[test]
    fn manage_label_matches_case_insensitively() {
        let l = label("PR-Manager");
        assert!(l.matches("pr-manager"));
        assert!(l.matches(" PR-MANAGER "));
        assert!(!l.matches("pr-manager-2"));
    }

    #[test]
    fn auto_merge_only_admits_auto_merge_prs() {
        let scope = ManagedScope::AutoMergeOnly;
        assert_eq!(scope.admits(&pr(true, &[])), Some(ManagedReason::AutoMerge));
    }

    #[test]
    fn auto_merge_only_ignores_labels() {
        let scope = ManagedScope::AutoMergeOnly;
        assert_eq!(scope.admits(&pr(false, &["pr-manager"])), None);
    }

    #[test]
    fn label_scope_admits_labeled_pr_without_auto_merge() {
        let scope = ManagedScope::AutoMergeOrLabel(label("pr-manager"));
        assert_eq!(
            scope.admits(&pr(false, &["bug", "PR-Manager"])),
            Some(ManagedReason::OptInLabel)
        );
    }

    #[test]
    fn label_scope_still_admits_auto_merge_prs_without_the_label() {
        let scope = ManagedScope::AutoMergeOrLabel(label("pr-manager"));
        assert_eq!(
            scope.admits(&pr(true, &["bug"])),
            Some(ManagedReason::AutoMerge)
        );
    }

    #[test]
    fn label_scope_ignores_unlabeled_non_auto_merge_prs() {
        let scope = ManagedScope::AutoMergeOrLabel(label("pr-manager"));
        assert_eq!(scope.admits(&pr(false, &["bug", "wip"])), None);
    }

    #[test]
    fn all_open_admits_everything_and_reports_the_right_reason() {
        let scope = ManagedScope::AllOpen;
        assert_eq!(
            scope.admits(&pr(false, &[])),
            Some(ManagedReason::RepoOptIn)
        );
        assert_eq!(scope.admits(&pr(true, &[])), Some(ManagedReason::AutoMerge));
    }

    #[test]
    fn describe_renders_each_scope() {
        assert_eq!(ManagedScope::AutoMergeOnly.describe(), "auto_merge");
        assert_eq!(ManagedScope::AllOpen.describe(), "all_open");
        assert_eq!(
            ManagedScope::AutoMergeOrLabel(label("pr-manager")).describe(),
            "auto_merge|label:pr-manager"
        );
    }
}
