mod agent;
mod config;
mod git;
mod github;
mod log;
mod merger;
mod poller;
mod prompt;
mod tmux;
mod types;

use std::process::ExitCode;
use std::sync::Arc;

use tokio::signal::unix::{signal, SignalKind};

use crate::agent::AgentRunner;
use crate::config::load_config;
use crate::github::GitHubClient;
use crate::log::init_logger;
use crate::poller::start_pollers;
use crate::tmux::tmux_available;
use crate::types::{ConfigError, RepoConfig};

#[tokio::main]
async fn main() -> ExitCode {
    let config = match load_config() {
        Ok(c) => c,
        Err(err) => return on_config_error(&err),
    };

    init_logger(config.globals.log_level);

    if !tmux_available().await {
        eprintln!("error: `tmux` is required but was not found in PATH");
        return ExitCode::from(2);
    }

    tracing::info!(
        repo_count = config.repos.len() as u64,
        "pr-manager starting"
    );
    for repo in &config.repos {
        log_repo_startup(repo);
    }

    let mut repo_arcs: Vec<Arc<RepoConfig>> = Vec::with_capacity(config.repos.len());
    let mut clients: Vec<Arc<GitHubClient>> = Vec::with_capacity(config.repos.len());
    for repo in config.repos {
        let client = match GitHubClient::new(&repo) {
            Ok(c) => Arc::new(c),
            Err(err) => {
                tracing::error!(repo = %repo.github_repo, err = %err, "failed to construct github client");
                return ExitCode::from(1);
            }
        };
        clients.push(client);
        repo_arcs.push(Arc::new(repo));
    }

    let runner = Arc::new(AgentRunner::new());
    let pollers = start_pollers(repo_arcs, clients, runner.clone());

    let signal_name = match wait_for_signal().await {
        Ok(s) => s,
        Err(err) => {
            tracing::error!(err = %err, "failed to install signal handlers");
            return ExitCode::from(1);
        }
    };

    let in_flight = runner.active_count().await;
    tracing::info!(signal = %signal_name, in_flight = in_flight as u64, "shutting down");

    pollers.cancel().await;
    runner.shutdown().await;

    ExitCode::SUCCESS
}

fn log_repo_startup(repo: &RepoConfig) {
    tracing::info!(
        repo = %repo.github_repo,
        repo_path = %repo.repo_path.display(),
        poll_interval_seconds = repo.poll_interval_seconds,
        recent_merges_limit = repo.recent_merges_limit,
        worktree_base = %repo.worktree_base.display(),
        logs_base = %repo.logs_base.display(),
        agent = repo.agent.name.as_str(),
        agent_bin = %repo.agent.bin,
        agent_args = %repo.agent.args.join(" "),
        agent_auth = repo.auth_mode.as_str(),
        managed_scope = %repo.managed_scope.describe(),
        "watching repo"
    );
}

async fn wait_for_signal() -> std::io::Result<&'static str> {
    let mut sigint = signal(SignalKind::interrupt())?;
    let mut sigterm = signal(SignalKind::terminate())?;
    tokio::select! {
        _ = sigint.recv() => Ok("SIGINT"),
        _ = sigterm.recv() => Ok("SIGTERM"),
    }
}

fn on_config_error(err: &ConfigError) -> ExitCode {
    eprintln!("config error: {}", err.message);
    for issue in &err.issues {
        eprintln!("  - {issue}");
    }
    ExitCode::from(2)
}
