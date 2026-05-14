use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Notify;
use tokio::task::JoinHandle;

use crate::agent::AgentRunner;
use crate::git::fetch_origin_prune;
use crate::github::GitHubClient;
use crate::merger::{try_native_merge, MergeOutcome, NeedsAgentReason};
use crate::prompt::build_prompt;
use crate::types::{GitHubError, OpenAutoMergePr, PrEvent, RepoConfig};

const SEEN_CAP: usize = 200;

/// One independent polling loop per watched repo. They share the AgentRunner
/// (registry is keyed by repo+pr) and a single cancel Notify.
pub struct PollerSet {
    handles: Vec<JoinHandle<()>>,
    cancel: Arc<Notify>,
}

impl PollerSet {
    /// Signal every loop to exit and join them. Each loop wakes from its sleep
    /// or its inner select, runs no further ticks, and the JoinHandles resolve.
    pub async fn cancel(self) {
        self.cancel.notify_waiters();
        for h in self.handles {
            if let Err(err) = h.await {
                tracing::warn!(err = %err, "poller task join failed");
            }
        }
    }
}

pub fn start_pollers(
    repos: Vec<Arc<RepoConfig>>,
    clients: Vec<Arc<GitHubClient>>,
    runner: Arc<AgentRunner>,
) -> PollerSet {
    assert_eq!(
        repos.len(),
        clients.len(),
        "start_pollers requires one client per repo"
    );

    let cancel = Arc::new(Notify::new());
    let mut handles = Vec::with_capacity(repos.len());

    for (repo, client) in repos.into_iter().zip(clients) {
        let runner = runner.clone();
        let cancel_loop = cancel.clone();
        let handle = tokio::spawn(async move {
            run_repo_loop(repo, client, runner, cancel_loop).await;
        });
        handles.push(handle);
    }

    PollerSet { handles, cancel }
}

async fn run_repo_loop(
    repo: Arc<RepoConfig>,
    client: Arc<GitHubClient>,
    runner: Arc<AgentRunner>,
    cancel: Arc<Notify>,
) {
    let mut last_main_sha: Option<String> = None;
    let mut seen_queue: VecDeque<String> = VecDeque::new();
    let mut seen_set: HashSet<String> = HashSet::new();

    let interval = Duration::from_secs(repo.poll_interval_seconds);

    // First tick fires immediately; subsequent ticks are interval-paced.
    loop {
        if let Err(err) = tick(
            &repo,
            client.as_ref(),
            runner.as_ref(),
            &mut last_main_sha,
            &mut seen_queue,
            &mut seen_set,
        )
        .await
        {
            // tick() catches and logs every recoverable failure itself; an
            // Err here is reserved for unhandled-panic-style cases that
            // bubble up from the inner futures.
            tracing::error!(repo = %repo.github_repo, err = %err, "unhandled poller error");
        }

        tokio::select! {
            _ = cancel.notified() => {
                break;
            }
            _ = tokio::time::sleep(interval) => {}
        }
    }
}

async fn tick(
    repo: &RepoConfig,
    github: &GitHubClient,
    runner: &AgentRunner,
    last_main_sha: &mut Option<String>,
    seen_queue: &mut VecDeque<String>,
    seen_set: &mut HashSet<String>,
) -> anyhow::Result<()> {
    let (main_sha, main_branch) = match github.get_default_branch_sha().await {
        Ok(v) => v,
        Err(err) => {
            log_warn(repo, &err, "failed to fetch default-branch SHA");
            return Ok(());
        }
    };

    // Always list open PRs — we need them both for new-event emission AND
    // for reconciling the active session registry against current state.
    let prs = match github.list_open_auto_merge_prs().await {
        Ok(v) => v,
        Err(err) => {
            log_warn(repo, &err, "failed to list open auto-merge PRs");
            return Ok(());
        }
    };

    let prs = filter_by_authors(repo, prs);

    let mut by_number: HashMap<i64, OpenAutoMergePr> = HashMap::new();
    for pr in &prs {
        by_number.insert(pr.number, pr.clone());
    }

    reconcile_sessions(repo, runner, &by_number).await;

    if last_main_sha.is_none() {
        tracing::info!(
            repo = %repo.github_repo,
            main_sha = %main_sha,
            main_branch = %main_branch,
            "initialized; not emitting on first tick"
        );
        *last_main_sha = Some(main_sha);
        return Ok(());
    }

    let prev = last_main_sha.as_deref().unwrap_or_default();
    if prev == main_sha {
        tracing::debug!(repo = %repo.github_repo, main_sha = %main_sha, "main unchanged");
        return Ok(());
    }

    tracing::info!(
        repo = %repo.github_repo,
        from = %prev,
        to = %main_sha,
        main_branch = %main_branch,
        "main advanced; checking auto-merge PRs"
    );

    let recent = if !prs.is_empty() {
        match github.list_recently_merged(repo.recent_merges_limit).await {
            Ok(v) => v,
            Err(err) => {
                log_warn(repo, &err, "failed to list recently merged PRs; continuing");
                Vec::new()
            }
        }
    } else {
        Vec::new()
    };

    // Single fetch in the user's repo before any agent runs. Agents share the
    // same `.git` via worktrees, so concurrent `git fetch` from each agent
    // would race on `.git/objects` and packed-refs locks. If fetch fails we
    // skip this tick's spawns and retry next tick (last_main_sha stays put).
    if !prs.is_empty() {
        if let Err(err) = fetch_origin_prune(&repo.repo_path).await {
            tracing::warn!(
                repo = %repo.github_repo,
                err = %err,
                repo_path = %repo.repo_path.display(),
                "git fetch origin --prune failed; deferring spawns to next tick"
            );
            return Ok(());
        }
    }

    for pr in &prs {
        if pr.head_repo_id != pr.base_repo_id {
            tracing::info!(repo = %repo.github_repo, pr = pr.number, "skipped fork PR (out of scope for v1)");
            continue;
        }
        let key = format!("{}:{}:{}", pr.number, pr.head_sha, main_sha);
        if seen_set.contains(&key) {
            tracing::debug!(repo = %repo.github_repo, key = %key, "already emitted; skipping");
            continue;
        }

        // Try the native fast path first: clean merges and lockfile-only
        // conflicts get pushed without paying for an agent. The merger always
        // cleans up its worktree before returning, so the agent (if needed)
        // starts from the same fresh state it always has.
        match try_native_merge(repo, pr).await {
            MergeOutcome::Pushed => {
                remember(seen_queue, seen_set, key);
                tracing::info!(
                    repo = %repo.github_repo,
                    pr = pr.number,
                    head_sha = %pr.head_sha,
                    main_sha = %main_sha,
                    "merged and pushed natively (no conflicts)"
                );
                continue;
            }
            MergeOutcome::PushedAfterLockfile { lockfiles } => {
                remember(seen_queue, seen_set, key);
                tracing::info!(
                    repo = %repo.github_repo,
                    pr = pr.number,
                    head_sha = %pr.head_sha,
                    main_sha = %main_sha,
                    lockfiles = %lockfiles.join(","),
                    "merged and pushed natively (lockfile-only conflicts)"
                );
                continue;
            }
            MergeOutcome::NeedsAgent { reason } => {
                log_needs_agent(repo, pr.number, &reason);
            }
        }

        let event = PrEvent {
            pr: pr.clone(),
            main_sha: main_sha.clone(),
            recent: recent.clone(),
        };
        let prompt = build_prompt(repo, &event);

        match runner.spawn(repo, &event, &prompt).await {
            Ok(()) => {
                remember(seen_queue, seen_set, key);
                tracing::info!(
                    repo = %repo.github_repo,
                    pr = pr.number,
                    head_sha = %pr.head_sha,
                    main_sha = %main_sha,
                    "emitted event"
                );
            }
            Err(err) => {
                tracing::error!(repo = %repo.github_repo, err = %err, "failed to emit event");
            }
        }
    }

    *last_main_sha = Some(main_sha);
    Ok(())
}

async fn reconcile_sessions(
    repo: &RepoConfig,
    runner: &AgentRunner,
    by_number: &HashMap<i64, OpenAutoMergePr>,
) {
    // 1. Drop entries for sessions whose tmux session is gone (agent exited
    //    on its own — typically a successful push). Sweep is global so each
    //    repo's loop redundantly nudges it; harmless and cheap.
    runner.sweep().await;

    // 2. Force-close sessions whose target PR has moved on. Two signals:
    //    - PR no longer in the open auto-merge list.
    //    - PR head_sha advanced past the SHA we spawned against.
    for sess in runner.active_for_repo(&repo.repo_id).await {
        let current = by_number.get(&sess.pr_number);
        match current {
            None => {
                runner
                    .close(
                        &repo.repo_id,
                        sess.pr_number,
                        "pr no longer open with auto-merge",
                    )
                    .await;
            }
            Some(current) if current.head_sha != sess.spawn_head_sha => {
                let from = sess.spawn_head_sha.chars().take(7).collect::<String>();
                let to = current.head_sha.chars().take(7).collect::<String>();
                let reason = format!("head_sha advanced {from} → {to}");
                runner.close(&repo.repo_id, sess.pr_number, &reason).await;
            }
            Some(_) => {}
        }
    }
}

fn remember(queue: &mut VecDeque<String>, set: &mut HashSet<String>, key: String) {
    if set.contains(&key) {
        return;
    }
    set.insert(key.clone());
    queue.push_back(key);
    while queue.len() > SEEN_CAP {
        if let Some(old) = queue.pop_front() {
            set.remove(&old);
        }
    }
}

fn filter_by_authors(repo: &RepoConfig, prs: Vec<OpenAutoMergePr>) -> Vec<OpenAutoMergePr> {
    if repo.pr_authors.is_empty() {
        return prs;
    }
    let mut kept = Vec::with_capacity(prs.len());
    for pr in prs {
        let matched = pr
            .author_login
            .as_deref()
            .map(|login| {
                repo.pr_authors
                    .iter()
                    .any(|allowed| allowed.eq_ignore_ascii_case(login))
            })
            .unwrap_or(false);
        if matched {
            kept.push(pr);
        } else {
            tracing::debug!(
                repo = %repo.github_repo,
                pr = pr.number,
                author = pr.author_login.as_deref().unwrap_or("<unknown>"),
                "skipping pr: author not in pr_authors allowlist"
            );
        }
    }
    kept
}

fn log_needs_agent(repo: &RepoConfig, pr_number: i64, reason: &NeedsAgentReason) {
    match reason {
        NeedsAgentReason::SemanticConflicts { files } => {
            tracing::info!(
                repo = %repo.github_repo,
                pr = pr_number,
                conflicts = %files.join(","),
                "native merge handing off to agent: semantic conflicts"
            );
        }
        NeedsAgentReason::LockfileResolverFailed { lockfile, detail } => {
            tracing::warn!(
                repo = %repo.github_repo,
                pr = pr_number,
                lockfile = %lockfile,
                detail = %detail,
                "native merge handing off to agent: lockfile resolver failed"
            );
        }
        NeedsAgentReason::Other(detail) => {
            tracing::warn!(
                repo = %repo.github_repo,
                pr = pr_number,
                detail = %detail,
                "native merge handing off to agent: other failure"
            );
        }
    }
}

fn log_warn(repo: &RepoConfig, err: &GitHubError, msg: &str) {
    tracing::warn!(
        repo = %repo.github_repo,
        err = %err.message,
        kind = "GitHubError",
        status = err.status.map(|s| s as u64),
        endpoint = %err.endpoint,
        "{msg}"
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{AgentConfig, AuthMode, RepoId};
    use std::path::PathBuf;

    fn repo_with_authors(authors: Vec<&str>) -> RepoConfig {
        RepoConfig {
            repo_id: RepoId::new("acme", "widgets"),
            github_repo: "acme/widgets".into(),
            github_owner: "acme".into(),
            github_name: "widgets".into(),
            github_token: "t".into(),
            poll_interval_seconds: 60,
            recent_merges_limit: 10,
            worktree_base: PathBuf::from("/tmp/pr-manager/acme__widgets/wt"),
            logs_base: PathBuf::from("/tmp/pr-manager/acme__widgets/logs"),
            repo_path: PathBuf::from("/tmp/acme/widgets"),
            agent: AgentConfig {
                name: "claude".into(),
                bin: "claude".into(),
                args: vec!["-p".into()],
            },
            auth_mode: AuthMode::OAuth,
            pr_authors: authors
                .into_iter()
                .map(|s| s.to_ascii_lowercase())
                .collect(),
        }
    }

    fn pr_with_author(number: i64, author: Option<&str>) -> OpenAutoMergePr {
        OpenAutoMergePr {
            number,
            title: format!("PR #{number}"),
            body: String::new(),
            head_branch: "feature".into(),
            head_sha: "deadbeef".into(),
            head_repo_id: 1,
            base_repo_id: 1,
            base_branch: "main".into(),
            author_login: author.map(str::to_string),
        }
    }

    #[test]
    fn filter_passes_everything_when_allowlist_empty() {
        let repo = repo_with_authors(vec![]);
        let prs = vec![
            pr_with_author(1, Some("alice")),
            pr_with_author(2, Some("bob")),
            pr_with_author(3, None),
        ];
        let kept = filter_by_authors(&repo, prs);
        assert_eq!(kept.len(), 3);
    }

    #[test]
    fn filter_keeps_only_allowlisted_authors_case_insensitively() {
        let repo = repo_with_authors(vec!["Alice", "renovate[bot]"]);
        let prs = vec![
            pr_with_author(1, Some("alice")),
            pr_with_author(2, Some("Bob")),
            pr_with_author(3, Some("RENOVATE[bot]")),
            pr_with_author(4, None),
        ];
        let kept = filter_by_authors(&repo, prs);
        let kept_nums: Vec<i64> = kept.iter().map(|p| p.number).collect();
        assert_eq!(kept_nums, vec![1, 3]);
    }
}
