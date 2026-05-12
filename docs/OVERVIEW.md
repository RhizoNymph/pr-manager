```yaml
Overview:
  description: >
    A standalone process that watches one or more GitHub repositories on a
    polling interval. Each repo gets its own tick loop with its own
    last_main_sha and seen-set; loops are independent and run concurrently.
    When a default branch advances, the poller tries to merge main into
    each open auto-merge PR's head branch natively (clean merge, or
    lockfile-only conflicts resolved by re-running the matching package
    manager) and pushes the result. PRs that hit real semantic conflicts
    fall through to a detached tmux session per PR running the configured
    coding agent. Each agent session is automatically closed when the
    PR's head SHA advances past what was spawned against, or when the
    PR leaves the open auto-merge list. The agent registry is shared
    across repos and keyed on (RepoId, pr_number).
  subsystems:
    poller: >
      Interval-driven GitHub poller, one tokio task per repo. Each tick
      (per repo) fetches the default-branch SHA, lists open auto-merge
      PRs, optionally filters them by the per-repo `pr_authors` allowlist
      (so two operators can split the same repo without stepping on each
      other), reconciles its own active tmux sessions against current PR
      state, and for each new (pr_number, head_sha, main_sha) triple
      invokes the merger fast-path; only PRs the merger hands back fall
      through to an agent session. Each loop dedupes events by
      (pr_number, head_sha, main_sha) within its own seen-set.
      `start_pollers` spawns the per-repo tasks and returns a PollerSet
      with a single shared cancel Notify.
    merger: >
      Native fast-path. Sets up a per-PR detached worktree under the
      repo's worktreeBase, runs `git merge origin/<base> --no-edit`, and
      on success pushes HEAD to origin/<head_branch>. On conflicts: if
      every conflicted file is a recognized lockfile (package-lock.json,
      pnpm-lock.yaml, yarn.lock, Cargo.lock, poetry.lock, uv.lock), deletes
      each one and re-invokes the matching package manager (`npm install
      --package-lock-only`, `pnpm install --lockfile-only`, `yarn install
      --mode update-lockfile`, `cargo generate-lockfile`, `poetry lock`,
      `uv lock`)
      from the lockfile's directory, then commits and pushes. Anything
      else (semantic conflicts, missing/failed resolver, push rejection)
      is reported as NeedsAgent; the worktree is fully cleaned up so the
      agent starts from scratch.
    github_client: >
      Thin HTTP wrapper around the GitHub REST API (commits, pulls, list
      closed-merged) using reqwest. One client per repo, each holding its
      own bearer token resolved from env at startup.
    agent_runner: >
      Single registry of active tmux sessions shared across all repos,
      keyed on (RepoId, pr_number). spawn(repo, event, prompt) writes
      the prompt to a tempfile and starts a detached tmux session named
      `pr-manager-<repo_id>-pr-<n>` running the configured agent command
      with cwd = repo.repo_path; agent stdout+stderr is redirected to a
      persistent log file under the repo's logsBase with a trailing
      `EXIT: <code>` line so success/failure is recoverable after the
      session exits. sweep() drops registry entries whose tmux session
      no longer exists and reports the captured exit code.
      close(repo_id, pr_number) force-kills a single session.
      active_for_repo(repo_id) snapshots that repo's sessions for the
      poller's reconcile pass. shutdown() kills all sessions across
      every repo.
    tmux: >
      Thin wrapper around the `tmux` CLI: -V (availability check),
      has-session, kill-session, new-session -d. Uses `=name` target
      syntax for exact-match. RepoId.for_tmux() sanitizes the slug
      (alphanumeric + `_`/`-` only) before it is embedded in a session
      name.
    git: >
      Thin wrapper around the `git` CLI. Today exposes only
      `fetch_origin_prune` so each repo's poller can run `git fetch
      origin --prune` in its own repo_path once per tick before
      spawning, instead of every agent racing on `.git/objects` and
      packed-refs locks.
    prompt: >
      Per-event prompt builder. Substitutes pr_number, head_branch,
      main_branch, head_sha, main_sha, recent_merges, and the per-PR
      worktree path directly into the instructions the agent sees.
    config: >
      Hand-rolled validated TOML loader. Resolves the config file in
      this order: --config <path> flag, PR_MANAGER_CONFIG env var,
      ./pr-manager.toml in cwd; errors if none exist. Parses [defaults]
      (any of poll_interval_seconds, recent_merges_limit, log_level,
      agent, agent_bin, agent_args, agent_auth, claude_bin,
      claude_extra_args, codex_bin, codex_extra_args, token_env,
      cache_root) plus one or more [[repos]] entries (github_repo +
      repo_path required; any [defaults] key except log_level and
      cache_root may be overridden per-repo). Tokens are NEVER stored
      in the file — token_env names an env var (default GITHUB_TOKEN)
      read at startup; .env is loaded via dotenv before token
      resolution. Derives an absolute worktreeBase per repo (under
      cache_root or XDG_CACHE_HOME or ~/.cache, namespaced by
      `<owner>__<name>`) embedded into the prompt and passed to Codex's
      default --add-dir, plus a sibling logsBase where each agent
      invocation persists its stdout/stderr.
  agent_defaults:
    claude: "claude [claude_extra_args] -p < <promptFile>"
    codex: >
      codex exec --ask-for-approval never --sandbox workspace-write --add-dir
      <worktreeBase> [codex_extra_args] - < <promptFile>
  data_flow: >
    Startup: load_config produces a Config { globals, repos }. main builds
    one GitHubClient per repo and one shared AgentRunner, then calls
    start_pollers which spawns one tokio task per repo and returns a
    PollerSet. On signal, PollerSet.cancel() notifies all loops and joins
    them, then runner.shutdown() kills any remaining tmux sessions.

    Per repo, each tick: poller fetches main SHA + open PRs. It asks the
    runner to sweep ended sessions, then closes any of *its own* sessions
    whose PR has moved on (head_sha advanced or PR left the list). If
    main advanced since the last tick and there is at least one open PR,
    the poller runs `git fetch origin --prune` once in repo.repo_path so
    the merger and any subsequent agents see current origin/<branch>
    refs without racing each other on `.git` locks; if that fetch fails
    the spawn loop is skipped this tick and retried next tick. For each
    new (pr#, head_sha, main_sha) triple the poller calls the merger
    fast-path. On Pushed / PushedAfterLockfile the triple is recorded as
    seen and no agent runs. On NeedsAgent the poller falls through to
    runner.spawn(repo, event, prompt), which starts a tmux session
    running the configured agent against a tempfile prompt; the agent
    does its git/gh work inside repo.repo_path via the same per-PR
    worktree path under pr-manager's cache (the merger has already
    cleaned it up). When the merger or agent pushes the merge, the next
    poll for that repo sees the head_sha advance and (for agent
    sessions) force-closes the session.

Features Index:
  pr_polling:
    description: >
      Multi-repo GitHub poller that tries a native merge fast-path per
      auto-merge PR and falls back to a coding agent in tmux for real
      conflicts.
    entry_points: [src/main.rs]
    depends_on: [poller, github_client, merger, agent_runner, tmux, prompt, config]
    doc: docs/features/pr_polling.md
```
