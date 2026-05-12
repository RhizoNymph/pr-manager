use std::collections::HashMap;
use std::env;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::types::{
    AgentConfig, AgentName, AuthMode, Config, ConfigError, Globals, LogLevel, RepoConfig, RepoId,
};

const DEFAULT_TOKEN_ENV: &str = "GITHUB_TOKEN";
const DEFAULT_CONFIG_FILENAME: &str = "pr-manager.toml";

pub fn load_config() -> Result<Config, ConfigError> {
    let argv: Vec<String> = env::args().skip(1).collect();
    load_config_from(&argv)
}

pub fn load_config_from(argv: &[String]) -> Result<Config, ConfigError> {
    // dotenvy populates process env so token env vars from `.env` are visible
    // when the TOML loader resolves them. Ignore "file not found".
    let _ = dotenvy::dotenv();

    let config_path_arg = parse_argv(argv)?;
    let path = resolve_config_path(config_path_arg)?;
    load_toml(&path)
}

/// Resolution order:
///   1. `--config <path>` (or `--config=<path>`) on the command line.
///   2. `PR_MANAGER_CONFIG` env var.
///   3. `./pr-manager.toml` in the current directory.
fn resolve_config_path(cli_arg: Option<String>) -> Result<PathBuf, ConfigError> {
    if let Some(p) = cli_arg.filter(|v| !v.is_empty()) {
        return Ok(PathBuf::from(p));
    }
    if let Ok(p) = env::var("PR_MANAGER_CONFIG") {
        if !p.is_empty() {
            return Ok(PathBuf::from(p));
        }
    }
    let cwd_default = PathBuf::from(DEFAULT_CONFIG_FILENAME);
    if cwd_default.exists() {
        return Ok(cwd_default);
    }
    Err(ConfigError::new(
        "no config file",
        vec![format!(
            "no config file found; pass --config <path>, set PR_MANAGER_CONFIG=<path>, or place {DEFAULT_CONFIG_FILENAME} in the current directory"
        )],
    ))
}

// --- TOML schema -------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct TomlRoot {
    #[serde(default)]
    defaults: TomlDefaults,
    #[serde(default)]
    repos: Vec<TomlRepo>,
}

#[derive(Debug, Deserialize, Default, Clone)]
struct TomlDefaults {
    poll_interval_seconds: Option<u64>,
    recent_merges_limit: Option<u32>,
    log_level: Option<String>,
    agent: Option<String>,
    agent_bin: Option<String>,
    agent_args: Option<String>,
    agent_auth: Option<String>,
    claude_bin: Option<String>,
    claude_extra_args: Option<String>,
    codex_bin: Option<String>,
    codex_extra_args: Option<String>,
    /// Default name of the env var to read each repo's GitHub token from.
    /// Tokens themselves are NEVER in the file.
    token_env: Option<String>,
    /// Override the cache root (defaults to $XDG_CACHE_HOME or ~/.cache).
    cache_root: Option<String>,
    /// Default PR-author allowlist. See `pr_authors` on TomlRepo.
    pr_authors: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
struct TomlRepo {
    github_repo: String,
    repo_path: String,
    // Per-repo overrides for any default. Same shape as TomlDefaults
    // (excluding cache_root and log_level, which are process-wide).
    token_env: Option<String>,
    poll_interval_seconds: Option<u64>,
    recent_merges_limit: Option<u32>,
    agent: Option<String>,
    agent_bin: Option<String>,
    agent_args: Option<String>,
    agent_auth: Option<String>,
    claude_bin: Option<String>,
    claude_extra_args: Option<String>,
    codex_bin: Option<String>,
    codex_extra_args: Option<String>,
    /// Allowlist of PR author logins. When set and non-empty, pr-manager only
    /// processes PRs whose author appears here (case-insensitive). Unset or
    /// empty means "no filter" — every open auto-merge PR is handled. Per-repo
    /// value replaces (not merges with) the default.
    pr_authors: Option<Vec<String>>,
}

// --- Loader ------------------------------------------------------------------

fn load_toml(path: &Path) -> Result<Config, ConfigError> {
    let raw = std::fs::read_to_string(path).map_err(|e| {
        ConfigError::new(
            "failed to read config file",
            vec![format!("{}: {}", path.display(), e)],
        )
    })?;

    let parsed: TomlRoot = toml::from_str(&raw).map_err(|e| {
        ConfigError::new(
            "failed to parse config file",
            vec![format!("{}: {}", path.display(), e)],
        )
    })?;

    let mut issues: Vec<String> = Vec::new();

    if parsed.repos.is_empty() {
        issues.push("config file must declare at least one [[repos]] entry".into());
    }

    let log_level = parse_log_level(parsed.defaults.log_level.as_deref(), &mut issues);

    let cache_root = match parsed.defaults.cache_root.as_deref() {
        Some(p) => absolutize(p),
        None => match resolve_default_cache_root() {
            Some(p) => p,
            None => {
                issues.push("unable to determine home directory for cache root".into());
                PathBuf::new()
            }
        },
    };

    let mut repos: Vec<RepoConfig> = Vec::with_capacity(parsed.repos.len());
    let mut seen_repo_ids: HashMap<String, usize> = HashMap::new();

    for (idx, raw_repo) in parsed.repos.iter().enumerate() {
        let issue_prefix = format!("repos[{idx}] ({}):", raw_repo.github_repo);

        if !is_owner_name(&raw_repo.github_repo) {
            issues.push(format!(
                "{issue_prefix} github_repo must be in 'owner/name' form"
            ));
            continue;
        }
        let (owner, name) = split_owner_name(&raw_repo.github_repo);
        let repo_id = RepoId::new(&owner, &name);

        if let Some(prev) = seen_repo_ids.insert(repo_id.as_str().to_string(), idx) {
            issues.push(format!(
                "{issue_prefix} duplicate of repos[{prev}]; each github_repo must appear once"
            ));
            continue;
        }

        let token_env_name = raw_repo
            .token_env
            .clone()
            .or_else(|| parsed.defaults.token_env.clone())
            .unwrap_or_else(|| DEFAULT_TOKEN_ENV.to_string());

        let github_token = match env::var(&token_env_name) {
            Ok(v) if !v.is_empty() => v,
            _ => {
                issues.push(format!(
                    "{issue_prefix} env var {token_env_name} (named by token_env) is unset or empty"
                ));
                continue;
            }
        };

        let poll_interval_seconds = raw_repo
            .poll_interval_seconds
            .or(parsed.defaults.poll_interval_seconds)
            .unwrap_or(60);
        if poll_interval_seconds == 0 {
            issues.push(format!("{issue_prefix} poll_interval_seconds must be > 0"));
        }

        let recent_merges_limit = raw_repo
            .recent_merges_limit
            .or(parsed.defaults.recent_merges_limit)
            .unwrap_or(10);
        if recent_merges_limit > 50 {
            issues.push(format!(
                "{issue_prefix} recent_merges_limit must be in [0, 50]"
            ));
        }

        let agent_kind = parse_agent_name(
            raw_repo
                .agent
                .as_deref()
                .or(parsed.defaults.agent.as_deref()),
            &issue_prefix,
            &mut issues,
        );

        let auth_mode = parse_auth_mode(
            raw_repo
                .agent_auth
                .as_deref()
                .or(parsed.defaults.agent_auth.as_deref()),
            &issue_prefix,
            &mut issues,
        );

        let repo_path = absolutize(&raw_repo.repo_path);

        let repo_key = format!("{owner}__{name}");
        let worktree_base = cache_root.join("pr-manager").join(&repo_key).join("wt");
        let logs_base = cache_root.join("pr-manager").join(&repo_key).join("logs");

        let agent_inputs = AgentInputs {
            agent_bin: raw_repo
                .agent_bin
                .as_deref()
                .or(parsed.defaults.agent_bin.as_deref()),
            agent_args: raw_repo
                .agent_args
                .as_deref()
                .or(parsed.defaults.agent_args.as_deref()),
            claude_bin: raw_repo
                .claude_bin
                .as_deref()
                .or(parsed.defaults.claude_bin.as_deref()),
            claude_extra_args: raw_repo
                .claude_extra_args
                .as_deref()
                .or(parsed.defaults.claude_extra_args.as_deref()),
            codex_bin: raw_repo
                .codex_bin
                .as_deref()
                .or(parsed.defaults.codex_bin.as_deref()),
            codex_extra_args: raw_repo
                .codex_extra_args
                .as_deref()
                .or(parsed.defaults.codex_extra_args.as_deref()),
        };

        let agent = build_agent_config(&agent_inputs, agent_kind, &worktree_base);

        let pr_authors = resolve_pr_authors(
            raw_repo.pr_authors.as_deref(),
            parsed.defaults.pr_authors.as_deref(),
            &issue_prefix,
            &mut issues,
        );

        repos.push(RepoConfig {
            repo_id,
            github_repo: raw_repo.github_repo.clone(),
            github_owner: owner,
            github_name: name,
            github_token,
            poll_interval_seconds,
            recent_merges_limit,
            worktree_base,
            logs_base,
            repo_path,
            agent,
            auth_mode,
            pr_authors,
        });
    }

    if !issues.is_empty() {
        return Err(ConfigError::new("invalid config file", issues));
    }

    Ok(Config {
        globals: Globals { log_level },
        repos,
    })
}

// --- helpers -----------------------------------------------------------------

fn parse_log_level(value: Option<&str>, issues: &mut Vec<String>) -> LogLevel {
    match value {
        None => LogLevel::Info,
        Some("trace") => LogLevel::Trace,
        Some("debug") => LogLevel::Debug,
        Some("info") => LogLevel::Info,
        Some("warn") => LogLevel::Warn,
        Some("error") => LogLevel::Error,
        Some("fatal") => LogLevel::Fatal,
        Some(other) => {
            issues.push(format!(
                "log_level: invalid level {other:?}; expected one of trace,debug,info,warn,error,fatal"
            ));
            LogLevel::Info
        }
    }
}

fn parse_agent_name(value: Option<&str>, prefix: &str, issues: &mut Vec<String>) -> AgentName {
    match value {
        None => AgentName::Claude,
        Some("claude") => AgentName::Claude,
        Some("codex") => AgentName::Codex,
        Some(other) => {
            issues.push(format!(
                "{prefix} invalid agent {other:?}; expected one of claude,codex"
            ));
            AgentName::Claude
        }
    }
}

fn resolve_pr_authors(
    repo_value: Option<&[String]>,
    default_value: Option<&[String]>,
    prefix: &str,
    issues: &mut Vec<String>,
) -> Vec<String> {
    let source = repo_value.or(default_value).unwrap_or(&[]);
    let mut out = Vec::with_capacity(source.len());
    for entry in source {
        let trimmed = entry.trim();
        if trimmed.is_empty() {
            issues.push(format!("{prefix} pr_authors contains an empty entry"));
            continue;
        }
        out.push(trimmed.to_ascii_lowercase());
    }
    out
}

fn parse_auth_mode(value: Option<&str>, prefix: &str, issues: &mut Vec<String>) -> AuthMode {
    match value {
        None => AuthMode::OAuth,
        Some("oauth") => AuthMode::OAuth,
        Some("api") => AuthMode::Api,
        Some(other) => {
            issues.push(format!(
                "{prefix} invalid value {other:?}; expected one of oauth,api"
            ));
            AuthMode::OAuth
        }
    }
}

fn resolve_default_cache_root() -> Option<PathBuf> {
    if let Ok(p) = env::var("XDG_CACHE_HOME") {
        if !p.is_empty() {
            return Some(absolutize(&p));
        }
    }
    dirs::home_dir().map(|h| h.join(".cache"))
}

fn absolutize(p: &str) -> PathBuf {
    let buf = PathBuf::from(p);
    if buf.is_absolute() {
        return buf;
    }
    match env::current_dir() {
        Ok(cwd) => cwd.join(buf),
        Err(_) => buf,
    }
}

/// Inputs for building the agent command. Each field is a per-repo override
/// already merged with `[defaults]`.
struct AgentInputs<'a> {
    agent_bin: Option<&'a str>,
    agent_args: Option<&'a str>,
    claude_bin: Option<&'a str>,
    claude_extra_args: Option<&'a str>,
    codex_bin: Option<&'a str>,
    codex_extra_args: Option<&'a str>,
}

fn build_agent_config(
    inputs: &AgentInputs<'_>,
    agent: AgentName,
    worktree_base: &Path,
) -> AgentConfig {
    let generic_args = inputs.agent_args.map(parse_args);

    match agent {
        AgentName::Claude => {
            let bin = inputs
                .agent_bin
                .or(inputs.claude_bin)
                .unwrap_or("claude")
                .to_string();
            let args = match generic_args {
                Some(args) => args,
                None => {
                    let mut v = parse_args(inputs.claude_extra_args.unwrap_or(""));
                    v.push("-p".to_string());
                    v
                }
            };
            AgentConfig {
                name: AgentName::Claude,
                bin,
                args,
            }
        }
        AgentName::Codex => {
            let bin = inputs
                .agent_bin
                .or(inputs.codex_bin)
                .unwrap_or("codex")
                .to_string();
            let args = match generic_args {
                Some(args) => args,
                None => {
                    let extra = parse_args(inputs.codex_extra_args.unwrap_or(""));
                    let mut v: Vec<String> = vec![
                        "exec".into(),
                        "--ask-for-approval".into(),
                        "never".into(),
                        "--sandbox".into(),
                        "workspace-write".into(),
                        "--add-dir".into(),
                        worktree_base.to_string_lossy().into_owned(),
                    ];
                    v.extend(extra);
                    v.push("-".into());
                    v
                }
            };
            AgentConfig {
                name: AgentName::Codex,
                bin,
                args,
            }
        }
    }
}

fn parse_args(value: &str) -> Vec<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        Vec::new()
    } else {
        trimmed.split_whitespace().map(str::to_string).collect()
    }
}

fn is_owner_name(value: &str) -> bool {
    let mut parts = value.splitn(2, '/');
    let owner = parts.next();
    let name = parts.next();
    matches!((owner, name), (Some(o), Some(n)) if !o.is_empty() && !n.is_empty() && !n.contains('/'))
}

fn split_owner_name(value: &str) -> (String, String) {
    let mut parts = value.splitn(2, '/');
    let owner = parts.next().unwrap_or("").to_string();
    let name = parts.next().unwrap_or("").to_string();
    (owner, name)
}

/// Parses argv. The only supported flag is `--config <path>` /
/// `--config=<path>`. Returns the value if present.
fn parse_argv(argv: &[String]) -> Result<Option<String>, ConfigError> {
    let mut config_path: Option<String> = None;
    let mut issues: Vec<String> = Vec::new();
    let mut i = 0;
    while i < argv.len() {
        let arg = &argv[i];
        if arg == "--" {
            i += 1;
            continue;
        }
        if arg == "-h" || arg == "--help" {
            issues.push("help output is not implemented; see README.md".into());
            i += 1;
            continue;
        }
        if !arg.starts_with("--") {
            issues.push(format!("unexpected argument: {arg}"));
            i += 1;
            continue;
        }

        let (raw_flag, inline_value) = match arg.find('=') {
            Some(eq) => (&arg[2..eq], Some(arg[eq + 1..].to_string())),
            None => (&arg[2..], None),
        };

        if raw_flag != "config" {
            issues.push(format!(
                "unknown option: --{raw_flag} (the only supported flag is --config <path>)"
            ));
            i += 1;
            continue;
        }

        let value = match inline_value {
            Some(v) => {
                i += 1;
                v
            }
            None => match argv.get(i + 1) {
                Some(v) => {
                    i += 2;
                    v.clone()
                }
                None => {
                    issues.push("missing value for --config".into());
                    i += 1;
                    continue;
                }
            },
        };
        if config_path.is_some() {
            issues.push("--config may only be specified once".into());
        } else {
            config_path = Some(value);
        }
    }

    if !issues.is_empty() {
        return Err(ConfigError::new("invalid command line", issues));
    }
    Ok(config_path)
}
