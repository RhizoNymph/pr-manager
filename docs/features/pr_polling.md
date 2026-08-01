# pr_polling

## Scope
- Standalone process (no MCP or chat bridge required).
- Watches one or more GitHub repos. Each repo is polled by its own tokio
  task with its own `last_main_sha` and seen-set; tasks share a single
  `AgentRunner` whose registry is keyed on `(RepoId, pr_number)`.
- Per repo, polls for: (a) default-branch HEAD SHA, (b) all open PRs,
  narrowed to the repo's **managed set** (see "Management scope"),
  (c) recently merged PRs for prompt context.
- Per `(PR, new main SHA)` pair, runs a **native fast-path** in-process
  (clean `git merge`, lockfile-only conflict resolution by re-running the
  matching package manager) and pushes when it can finish without an LLM.
- For PRs the fast-path returns `NeedsAgent` for, spawns one **detached
  tmux session** named `pr-manager-<owner>__<repo>-pr-<n>` running the
  configured coding agent against a tempfile prompt inside the repo's
  `repo_path`.
- Supports built-in `agent = "claude"` and `agent = "codex"` harnesses, plus
  custom harnesses declared under `[harnesses.<name>]`.
- Reconciles each repo's active sessions on every tick:
  - Sweep: drop entries whose tmux session is gone (global; cheap).
  - Force-close: PR's `head_sha` advanced past what we spawned against,
    or PR is no longer in the managed set (scoped to this repo's
    sessions only).
- The prompt instructs the agent to merge `origin/<main>` into the PR
  branch inside a dedicated throwaway worktree, push when clean or when
  conflict resolution is obvious, and abort + report when conflicts are
  semantically ambiguous. Because the native fast-path already tried clean
  merge and lockfile resolution, the agent is told to expect a real
  semantic conflict.
- Filters out PRs from forks (`head.repo.id != base.repo.id`); fork PRs
  require pushing to a fork remote and are out of scope.

## Non-scope
- No MCP server, channel notifications, or chat bridge.
- No webhooks (polling only).
- No code-understanding merge logic in-process. The native fast-path
  handles only mechanical cases (no conflicts, or all-lockfile conflicts);
  anything else is delegated to the spawned agent subprocess running in
  tmux.

## Configuration

A single TOML config file declares `[defaults]` and one or more `[[repos]]`
entries. Resolution order: `--config <path>` flag, `PR_MANAGER_CONFIG`
env var, `$XDG_CONFIG_HOME/pr-manager/config.toml`
(or `~/.config/pr-manager/config.toml` when `XDG_CONFIG_HOME` is unset),
`./pr-manager.toml` in the current directory; the loader errors if none
of those exist. Tokens are NOT in the file — `token_env` (per-repo or
default) names an env var read at startup. `.env` in the working directory
is loaded via dotenv if present, but is optional: variables exported in the
process environment work just as well. The simplest valid file
is one repo:

```toml
[[repos]]
github_repo = "owner/name"
repo_path = "/abs/path/to/checkout"
```

## Management scope

"Managed" = pr-manager will merge the default branch into that PR's head
branch when main advances, and may spend an agent run on it. Two per-repo
keys (also settable in `[defaults]`) decide the set:

| `manage_all_prs` | `manage_label` | Resolved `ManagedScope` | Managed set |
| ---------------- | -------------- | ----------------------- | ----------- |
| unset / `false`  | unset          | `AutoMergeOnly`         | `auto_merge != null` |
| unset / `false`  | `"pr-manager"` | `AutoMergeOrLabel(..)`  | `auto_merge != null` **or** labeled |
| `true`           | *(any)*        | `AllOpen`               | every open PR |

`manage_all_prs = true` is an override: it wins over `manage_label`, which
is why the resolved enum carries only one of the two — "manage everything
*and* match a label" is not representable past config load. A repo setting
`manage_all_prs = false` explicitly overrides a `[defaults]` value of `true`
and falls back to label matching if a label is configured.

Validation: an empty or whitespace-only `manage_label` is a config error
(`ManageLabel::new` rejects it), not a silently-ignored key. Label matching
is ASCII case-insensitive, matching the `pr_authors` rule.

`ManagedScope::admits(&OpenPr) -> Option<ManagedReason>` is the single
decision point. `ManagedReason` is one of:

| Reason | Meaning |
| ------ | ------- |
| `auto_merge` | GitHub auto-merge is armed on the PR. Reported under every scope. |
| `opt_in_label` | Non-auto-merge PR carrying the repo's `manage_label`. |
| `repo_opt_in` | Non-auto-merge PR swept in by `manage_all_prs`. |

The reason rides along into every merge/spawn log line as `managed_reason`
and into the prompt as `managed_because`. The repo's scope appears once at
startup as `managed_scope` and again on the "main advanced" line as `scope`.

Ordering: `pr_authors` is applied **before** the scope filter, so the
allowlist always wins — a labeled PR from an author outside the allowlist is
never managed. Fork filtering is unchanged and independent (a fork PR is
skipped in the spawn loop regardless of scope).

Opting in is fully reversible from the GitHub UI: removing the label (or
disarming auto-merge) drops the PR from the managed set on the next tick,
and reconcile force-closes any agent session already running for it.

Opted-in PRs get the identical pipeline to auto-merge PRs, including agent
escalation on semantic conflicts. `manage_all_prs = true` on a busy repo can
therefore mean many concurrent tmux sessions; `pr_authors` is the intended
way to scope that down.

## Data and control flow

```text
user's terminal
  └─ runs:    cargo run --release [-- --config pr-manager.toml]
        ├─ src/main.rs
        │     ├─ load_config()                 (TOML)
        │     ├─ tmux_available()              (fail fast if tmux missing)
        │     ├─ init_logger()                 (tracing -> stderr)
        │     ├─ for each repo:
        │     │     GitHubClient::new(repo)   (one client per repo)
        │     ├─ Arc<AgentRunner::new()>      (shared across repos)
        │     └─ start_pollers(repos, clients, runner) -> PollerSet
        │           one tokio task per repo, each on its own tick:
        │             1. fetch main SHA (this repo)
        │             2. list open PRs (this repo)
        │                  filter_by_authors  -> pr_authors allowlist
        │                  filter_managed     -> managed_scope.admits(pr)
        │             3. runner.sweep()                <- drops ended sessions globally
        │             4. for each runner.active_for_repo(repo_id):
        │                  if PR gone or head_sha advanced:
        │                    runner.close(repo_id, pr#, reason)
        │             5. if main advanced:
        │                  git fetch origin --prune in repo.repo_path
        │                  for each new (pr#, head_sha, main_sha):
        │                    try_native_merge(repo, pr)
        │                      ├─ Pushed                  -> mark seen, continue
        │                      ├─ PushedAfterLockfile     -> mark seen, continue
        │                      └─ NeedsAgent              -> fall through:
        │                           runner.spawn(repo, event, prompt, worktree_path)
        │                             ├─ writes prompt -> tempfile
        │                             └─ tmux new-session -d
        │                                  -s pr-manager-<repo_id>-pr-N
        │                                  -c <worktree_path>
        │                                  "<agent command> < tempfile"
        └─ user can attach: tmux attach -t pr-manager-<repo_id>-pr-N
```

Per-repo poller loop (every `poll_interval_seconds`, default 60s):
1. `GET /repos/{repo}/commits/{default_branch}` -> `mainSha`.
2. `GET /repos/{repo}/pulls?state=open` -> every open PR, minus forks
   (`head.repo` absent). Then two filters, in this order:
   - **Authors.** If `repo.pr_authors` is non-empty, drop PRs whose
     `user.login` is not in the allowlist (case-insensitive). Unknown author
     (missing `user`) is dropped when the allowlist is active. Empty/unset
     allowlist = no filter.
   - **Scope.** `repo.managed_scope.admits(pr)` keeps the PR and tags it with
     a `ManagedReason`; `None` drops it. See "Management scope".
3. Reconcile sessions:
   - `runner.sweep()` removes registry entries (across all repos) whose
     tmux session no longer exists.
   - For each `runner.active_for_repo(repo_id)` session, close it if the
     PR left the managed set or its current `head.sha` differs
     from the one we spawned against.
4. If `mainSha === lastMainSha`, done.
5. Else: `GET /repos/{repo}/pulls?state=closed&sort=updated&direction=desc`
   -> take first N where `merged_at != null`.
6. If at least one managed PR exists, run `git fetch origin --prune`
   in the repo's `repo_path` once. On failure, log and skip the spawn loop
   without advancing `lastMainSha` so the next tick retries.
7. For each managed PR, key = `${pr.number}:${pr.head.sha}:${mainSha}`.
   If key already in this repo's `seen` set, skip. Else call `try_native_merge`:
   - `Pushed` / `PushedAfterLockfile` -> add key to `seen`, no agent spawn.
   - `NeedsAgent` with a prepared worktree -> emit, add key to `seen`, spawn
     the agent in that worktree.
   - `NeedsAgent` without a prepared worktree -> log and skip agent spawn for
     that event.
8. `lastMainSha = mainSha`.

Each repo's `seen` set is bounded to the last 200 keys (FIFO).

## Native fast-path (`src/merger.rs`)

The merger is the only path that runs git/merge work in the pr-manager
process itself. It is intentionally narrow: it handles cases where an LLM
adds no value, and falls back to the agent for anything else.

For each `(pr, head_sha, main_sha)` triple, given a `&RepoConfig`:

1. Compute `wt = ${repo.worktree_base}/pr-${pr.number}`. Verified to be
   under `repo.worktree_base` before any `--force` worktree operation.
2. Drop any stale state at `wt`: `git worktree remove --force` if git knows
   about it, otherwise `rm -rf`. Then
   `git worktree add --detach $wt origin/<head_branch>` from `repo.repo_path`.
3. `git merge origin/<base_branch> --no-edit` inside `$wt`.
4. **Clean merge**: `git push origin HEAD:<head_branch>`,
   `git worktree remove --force $wt`, return `Pushed`.
5. **Conflict**: list with `git diff --name-only --diff-filter=U`.
   - If any conflicted file is not a recognized lockfile: leave the conflicted
     merge in place and return
     `NeedsAgent { SemanticConflicts { files }, worktree_path: Some($wt) }`.
   - Else, for each conflicted lockfile: delete the file, then run the
     matching package manager from the lockfile's directory:
     | Lockfile basename   | Resolver                                                |
     | ------------------- | ------------------------------------------------------- |
     | `package-lock.json` | `npm install --package-lock-only --no-audit --no-fund`  |
     | `pnpm-lock.yaml`    | `pnpm install --lockfile-only`                          |
     | `yarn.lock`         | `yarn install --mode update-lockfile` (Yarn v3+)        |
     | `Cargo.lock`        | `cargo generate-lockfile`                               |
     | `poetry.lock`       | `poetry lock`                                           |
     | `uv.lock`           | `uv lock`                                               |
     If any resolver exits non-zero or the file is not produced: restore the
     merge conflict state when possible and return
     `NeedsAgent { LockfileResolverFailed { lockfile, detail }, worktree_path: Some($wt) }`.
   - On success: `git add -A`, `git commit --no-edit`,
     `git push origin HEAD:<head_branch>`, `git worktree remove --force $wt`,
     return `PushedAfterLockfile { lockfiles }`.
6. **Push rejection / unexpected error** after setup: leave the worktree for
   the agent and return `NeedsAgent { Other(detail), worktree_path: Some($wt) }`.
   If setup failed before a safe worktree exists, return `worktree_path: None`.

Lockfile detection is by basename only (`Path::file_name`), so monorepo
paths like `packages/web/pnpm-lock.yaml` and `crates/core/Cargo.lock` are
recognized. The resolver runs in the lockfile's parent directory.

Why delete the lockfile before regenerating: package managers refuse to
parse files that contain merge conflict markers. Regeneration takes the
post-merge manifest as input and produces a lockfile consistent with both
sides' dependency changes. Cargo's `generate-lockfile` may bump unrelated
deps within their version constraints; for an auto-merge into a feature
branch this is acceptable.

The agent continues from the merger's prepared state. It starts with
`cwd = $wt`, inspects `git status`, resolves obvious conflicts or reports
ambiguous ones, and never creates or removes worktrees itself. The runner
removes `$wt` after natural exit, force-close, or shutdown.

## Agent harnesses

The runner always writes the prompt to an internal tempfile and redirects it
to the harness on stdin. This keeps custom harnesses on the same contract as
the built-in Claude and Codex harnesses.

Default Claude command:

```sh
claude [claude_extra_args] -p < "$promptFile"
```

Default Codex command:

```sh
codex exec --ask-for-approval never --sandbox workspace-write \
  --add-dir "$worktreeBase" [codex_extra_args] - < "$promptFile"
```

`agent_bin` overrides the selected provider binary. `agent_args` replaces the
entire provider arg vector after the binary; if it is set, provider-specific
defaults such as Claude's `-p` or Codex's `exec ... -` are not added. Env
and TOML arg strings are whitespace-split; TOML arg arrays are passed as-is.

Any non-built-in `agent` value must name a custom harness table:

```toml
[defaults]
agent = "local_agent"

[harnesses.local_agent]
bin = "my-agent"
args = ["run", "--stdin"]
```

Custom harnesses receive the prompt on stdin. If a tool requires a prompt
file path instead, wrap it in a small script that reads stdin and adapts to
that tool's interface.

## Running

```sh
cp pr-manager.toml.example pr-manager.toml
cp .env.example .env
# fill in [[repos]] in pr-manager.toml; set GITHUB_TOKEN in .env
cargo run --release
```

`cargo run --release` looks for `./pr-manager.toml` by default; pass
`--config <path>` or set `PR_MANAGER_CONFIG=<path>` to point elsewhere.
The only supported CLI flag is `--config`; everything else lives in the
TOML file.

Requires `tmux` on PATH (the script exits with code 2 at startup if `tmux -V`
fails) and the selected agent CLI on PATH.

For unattended Claude automation, set `claude_extra_args =
"--dangerously-skip-permissions"` in `[defaults]` (or per-repo). Codex
defaults to non-interactive execution with workspace-write sandboxing and
an added writable directory for pr-manager's worktree cache.

## tmux session naming

Each PR uses a fixed session name: `pr-manager-<owner>__<repo>-pr-<n>`,
where the repo slug is sanitized via `RepoId::for_tmux()` (alphanumeric +
`_`/`-` only). The repo prefix prevents collisions when one process
watches multiple repos.

On `runner.spawn(repo, event, prompt, worktree_path)`:
1. Any existing session with that exact name is killed first
   (`tmux kill-session -t =pr-manager-<repo_id>-pr-<n>`) to defend against
   stale sessions from a prior crash. The `=` prefix forces exact-match.
2. The prompt is written to
   `${TMPDIR}/pr-manager-<repo_id>-pr-<n>-<ms>.prompt` with mode 0600.
3. `tmux new-session -d -s pr-manager-<repo_id>-pr-<n> -c <worktree_path>
   "<cmd>"` is invoked, where `<cmd>` is the provider command plus
   `< <promptFile>`, with each token POSIX-quoted.

Attach: `tmux attach -t pr-manager-<repo_id>-pr-<n>` (the exact command
appears in the spawn log line).

## Spawned-agent environment

`agent_auth` (TOML key, in `[defaults]` or any `[[repos]]` entry) selects
how the spawned agent authenticates against its provider. Valid values:

- `oauth` (default) — prepends
  `unset ANTHROPIC_API_KEY ANTHROPIC_AUTH_TOKEN OPENAI_API_KEY` to the
  shell command so the agent CLI falls through to its on-disk OAuth
  credentials.
- `api` — leaves the parent process env intact. Whatever key the
  spawned agent inherits is what it uses.

Why `oauth` is the default: both Claude and Codex CLIs prefer an API
key over OAuth credentials when one is present in the environment, and
that key leaks in easily — from the user's shell rc, from a `.env`
loaded by some other tool, or from a wrapping agent session that injects
provider API keys into every child process. Without scrubbing, the
spawned agent silently bills against an API account instead of the
user's subscription. The startup log line records the selected value as
`agent_auth`.

## Per-invocation agent logs

Every spawn writes the agent's stdout+stderr to a persistent file:

```text
${cache_root or XDG_CACHE_HOME or $HOME/.cache}/pr-manager/<owner>__<name>/logs/pr-<n>-<ms>.log
```

The shell wrapper appends an `EXIT: <code>` line after the agent exits, so
even after the tmux session is gone the log still encodes whether the run
succeeded. The session-end log line (`agent session ended`) carries
`log_file` and `exit_code` fields; the spawn line also carries `log_file`
so attaching to tmux and tailing the log are interchangeable.

These logs are never auto-cleaned — failed runs (ambiguous-conflict
aborts, push rejections, agent crashes) need to be inspectable after the
fact. Force-closed sessions (PR `head_sha` advanced, PR closed,
shutdown) may not have an `EXIT:` line because tmux killed the shell
before it ran; in those cases `exit_code` is `null` and the log still
contains everything the agent printed.

## Auto-close on PR update

The poller does not need a back-channel from the agent - GitHub is the source
of truth. Two distinct close paths:

- **Natural exit.** The agent pushes the merge and exits cleanly; tmux closes
  the session because there is nothing left to run in the pane. The next
  `runner.sweep()` (any repo's tick will trigger one) notices the session is
  gone, removes the registry entry, deletes the prompt tempfile, and removes
  the worktree.
- **Force-close.** The repo's poller observes that the PR's `head.sha` has
  advanced past `spawnHeadSha`, or the PR has left the managed set — closed,
  auto-merge disarmed, opt-in label removed, or `manage_all_prs` turned off
  and the process restarted. The runner kills the tmux session, deletes the
  prompt tempfile, and removes the worktree.

Either way, the prompt tempfile and per-PR worktree are cleaned up.

## Files

- `src/main.rs` - entrypoint. Checks tmux availability, loads config,
  builds N GitHub clients and one shared AgentRunner, calls
  `start_pollers`, installs SIGINT/SIGTERM handlers.
- `src/config.rs` - `load_config` resolves the TOML config path
  (`--config` flag, `PR_MANAGER_CONFIG`, then `./pr-manager.toml`),
  parses + validates it, and returns a typed `Config { globals,
  repos: Vec<RepoConfig> }`. Returns `ConfigError` on missing/invalid
  input. Tokens always read from env (default `GITHUB_TOKEN`, or
  whatever `token_env` names). `resolve_managed_scope` collapses
  `manage_all_prs` + `manage_label` into one `ManagedScope`.
- `src/types.rs` - `RepoId`, `Globals`, `Config`, `RepoConfig`,
  `AgentConfig`, `OpenPr`, `PrEvent`, `RecentMerge`, `ManageLabel`,
  `ManagedScope` (+ `admits` / `describe`), `ManagedReason`, error types.
- `src/github.rs` - `GitHubClient` wrapping reqwest; one per repo.
  `list_open_prs` returns every open non-fork PR with `auto_merge_enabled`
  and `labels` populated; it applies no management filtering of its own.
- `src/poller.rs` - `start_pollers` spawns one task per repo and returns
  a `PollerSet` whose `cancel()` notifies them all and joins. Owns
  `filter_by_authors` and `filter_managed`, and the internal `ManagedPr`
  (an `OpenPr` plus the `ManagedReason` that admitted it).
- `src/prompt.rs` - `build_prompt(repo, event)` returns the per-event
  prompt with metadata substituted inline; the opening objective and the
  `managed_because` line vary with `event.managed_reason`.
- `src/agent.rs` - `AgentRunner` exposes `spawn / active_for_repo /
  close / sweep / shutdown`. Owns the global session registry keyed on
  `(RepoId, pr_number)`.
- `src/tmux.rs` - wrapper around the tmux CLI.
- `src/git.rs` - thin wrapper around the `git` CLI; today exposes
  `fetch_origin_prune` so the poller can run `git fetch origin --prune`
  in each repo's `repo_path` before the merger or any agents touch
  `.git`.
- `src/merger.rs` - native fast-path. `try_native_merge(repo, pr)`
  returns `MergeOutcome::{Pushed, PushedAfterLockfile, NeedsAgent}`.
  Cleans up after native pushes. For agent handoff, returns the prepared
  per-PR worktree path.
- `src/log.rs` - structured-JSON logger to stderr (tracing-subscriber).

## Worktree-isolated merge flow

pr-manager owns a dedicated cache directory outside any user repository, and
every throwaway worktree lives there — used both by the native merger and
by the spawned agent. The base is computed at startup per repo:

```text
${cache_root or XDG_CACHE_HOME or $HOME/.cache}/pr-manager/<owner>__<name>/wt
```

Per-PR worktree path: `${repo.worktree_base}/pr-<n>`. Because `$WT` is
always under pr-manager's cache and never inside any user repo, `--force`
operations on it cannot affect the user's worktrees. The agent runs with
`cwd = $WT`; pr-manager removes `$WT` from the filesystem and Git's worktree
list after the tmux session ends or is force-closed.

### Limitations

If two distinct `[[repos]]` entries point at the same `owner/name` (or
the user runs two pr-manager processes against the same repo), they
resolve to the same `worktreeBase` and tmux session names. The TOML
loader rejects duplicate `github_repo` values within one process; for
cross-process collisions, run one pr-manager per host per repo.

## Invariants and constraints
- `tmux` must be on PATH; the script exits with code 2 at startup if it
  is not.
- Logger goes to stderr.
- Each per-repo poller never crashes the process: a failed GitHub call
  is logged and that repo's loop continues at the next interval. A
  repo's failure does not affect other repos' loops. `ConfigError` at
  startup is fatal.
- Reconciliation runs every tick (per repo), independent of whether
  main advanced.
- Initial main_sha is observed on the first tick of each repo but not
  emitted.
- Dedup key (per repo) includes `pr.head.sha` so a PR that gets new
  commits after a merge re-fires on the next main change.
- Each repo's agent runs with `cwd = ${repo.worktree_base}/pr-<n>`. The
  user's configured `repo_path` is used as the stable Git control checkout for
  fetches and worktree operations, but it is never the target of merge work.
- Fork PRs are filtered at the poller.
- Management scope is resolved once at config load, never per tick, so a
  repo's `ManagedScope` is constant for the process lifetime. Changing
  `manage_label` / `manage_all_prs` requires a restart; changing a PR's
  label or auto-merge state does not.
- `pr_authors` is applied before `managed_scope`, so widening the scope can
  never pull in a PR the author allowlist excludes.
- A PR admitted by any reason gets the identical downstream pipeline. There
  is no separate, cheaper path for opted-in PRs — `ManagedReason` affects
  logging and prompt wording only.
- Prompt tempfiles live in `os.tmpdir()` with mode 0600 and are removed
  on close/sweep/shutdown.
- Agent log files under `<cache>/pr-manager/<owner>__<name>/logs/` are
  never deleted by pr-manager — the user owns retention.
- Session names use exact-match (`=name`) target syntax for tmux ops to
  avoid prefix-match collisions, and include a sanitized repo prefix to
  avoid collisions across repos.
- Tokens are never stored in the TOML config; they are read from env at
  startup. `token_env` (per-repo or default) names the env var.
```
