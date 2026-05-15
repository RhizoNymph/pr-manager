# pr-manager

Standalone poller that watches one or more GitHub repos and, whenever a default
branch advances, tries to merge `main` into each open auto-merge PR. A native
fast-path handles clean merges and lockfile-only conflicts (regenerated via the
matching package manager) and pushes the result without spawning an agent. PRs
that hit real semantic conflicts fall through to a detached **tmux session**
per PR running a configured coding agent. Each agent session is auto-closed as
soon as the PR's head SHA advances past what we spawned against, or the PR
leaves the open auto-merge list. Conflicts that are not obvious are escalated
to the user instead of being guessed at.

No webhooks, no MCP, and no interactive chat session required - just a
long-running process plus `tmux` and a supported agent CLI on PATH.

## Quickstart

Prereqs: `tmux` on PATH and the agent CLI/harness you plan to use (`claude`,
`codex`, or a custom harness command) on PATH.

Install `pr-manager` from APT:

```sh
curl -fsSL https://rhizonymph.github.io/pr-manager/gpg.key | sudo gpg --dearmor -o /usr/share/keyrings/pr-manager.gpg
echo "deb [signed-by=/usr/share/keyrings/pr-manager.gpg] https://rhizonymph.github.io/pr-manager stable main" | sudo tee /etc/apt/sources.list.d/pr-manager.list
sudo apt update
sudo apt install pr-manager
```

Or install from crates.io:

```sh
cargo install pr-manager-cli
```

Or install a downloaded release binary:

```sh
mkdir -p ~/.local/bin
install -m 0755 ./pr-manager ~/.local/bin/pr-manager
```

Make sure `~/.local/bin` is on PATH, or invoke the binary as
`~/.local/bin/pr-manager`.

Then create a config and run it:

```sh
mkdir -p ~/.config/pr-manager
$EDITOR ~/.config/pr-manager/config.toml
export GITHUB_TOKEN=github_pat_...
pr-manager
```

That's it. A packaged or downloaded binary looks for
`~/.config/pr-manager/config.toml` by default. Pass `--config <path>` or set
`PR_MANAGER_CONFIG=<path>` to point elsewhere.

The simplest config is one repo:

```toml
[[repos]]
github_repo = "owner/name"
repo_path = "/abs/path/to/checkout"
```

Each repo gets its own poll loop, its own seen-set, its own cache directory
(`<owner>__<repo>` namespaced), and its own tmux sessions
(`pr-manager-<owner>__<repo>-pr-<n>`). Repos run independently — a slow
GitHub call for one never blocks the others.

When the poller spawns an agent, the log line includes a `tmux attach` command
to follow along; detach with `Ctrl-b d`. See [Configuration](#configuration)
for the full reference.

## Agent harnesses

pr-manager ships with two built-in harnesses:

| Harness | Default command |
|---|---|
| `claude` | `claude [claude_extra_args] -p < /tmp/pr-manager-<repo>-pr-<n>-<ts>.prompt` |
| `codex` | `codex exec --ask-for-approval never --sandbox workspace-write --add-dir <worktree-cache> [codex_extra_args] - < /tmp/pr-manager-<repo>-pr-<n>-<ts>.prompt` |

Set `agent = "codex"` in `[defaults]` (or override per-repo) to use Codex.
`agent_bin` overrides the selected provider's binary, and `agent_args`
replaces the entire provider arg vector after the binary. All arg strings
are whitespace-split; arg arrays are passed as-is.

Any other `agent` value names a custom harness declared under
`[harnesses.<name>]`:

```toml
[defaults]
agent = "local_agent"

[harnesses.local_agent]
bin = "my-agent"
args = ["run", "--stdin"]
```

Custom harnesses receive the same prompt stdin as the built-in harnesses.
If a tool cannot read prompts from stdin, wrap it with a small script and
point `bin` at that wrapper.

## Agent arguments

Use the provider-specific extra args for normal customization:

```toml
[defaults]
agent = "claude"
claude_extra_args = "--permission-mode bypassPermissions"
```

This produces:

```sh
claude --permission-mode bypassPermissions -p < /tmp/pr-manager-<repo>-pr-<n>-<ts>.prompt
```

For Codex, use `codex_extra_args` for options that belong after the built-in
`codex exec --ask-for-approval never --sandbox workspace-write --add-dir
<worktree-cache>` prefix and before the final `-` prompt argument:

```toml
[defaults]
agent = "codex"
codex_extra_args = "-m gpt-5.2"
```

Use `agent_args` only when you need to replace the complete argument list
after the binary. When `agent_args` is set, pr-manager does not add provider
defaults, so include the prompt-reading argument yourself:

```toml
[defaults]
agent = "claude"
agent_args = "--permission-mode bypassPermissions -p"
```

```toml
[defaults]
agent = "codex"
agent_args = "exec --ask-for-approval never --sandbox workspace-write -"
```

Any of these can also live inside a `[[repos]]` entry to override defaults
for one repo.

## How it works

For each watched repo, a dedicated task ticks every `poll_interval_seconds`
(default 60s):

1. Fetch the default-branch HEAD SHA.
2. List open PRs with `auto_merge != null` (fork PRs are filtered).
3. Reconcile this repo's active tmux sessions:
   - Sweep registry entries whose tmux session is gone.
   - Force-close any session whose PR head SHA advanced, or whose PR is no
     longer in the open auto-merge list.
4. If main advanced this tick and there is at least one open PR, run
   `git fetch origin --prune` once in the repo's `repo_path` so the merger and
   any subsequent agents see current `origin/<branch>` refs without racing
   each other on `.git` locks. If that fetch fails, defer spawns to the next
   tick.
5. For each unseen `(pr#, head_sha, main_sha)` triple, try the native merger
   fast-path: a per-PR detached worktree runs `git merge origin/<base>` and,
   on success, pushes `HEAD` to `origin/<head_branch>`. If conflicts are
   limited to known lockfiles (`package-lock.json`, `pnpm-lock.yaml`,
   `yarn.lock`, `Cargo.lock`, `poetry.lock`, `uv.lock`), each is deleted and regenerated
   via the matching package manager, then committed and pushed. The triple is
   recorded as seen and no agent runs.
6. If the merger reports `NeedsAgent` (semantic conflicts, missing/failed
   resolver, push rejection), pr-manager keeps the prepared worktree, writes
   the prompt to a tempfile, and spawns a tmux session named
   `pr-manager-<owner>__<repo>-pr-<n>` with `cwd` set to that worktree.

Sessions run in parallel both within a repo and across repos. Each PR uses its
own throwaway worktree under
`${XDG_CACHE_HOME:-~/.cache}/pr-manager/<owner>__<repo>/wt/pr-<n>` - never
inside your checkout - so forced cleanup only targets pr-manager-owned paths.
The native merger and agent use the same per-PR worktree. pr-manager removes
it from both the filesystem and Git's worktree list when the agent exits, is
force-closed, or the process shuts down.

## Watching a session

When a session spawns, the log line includes the exact command:

```json
{"repo":"acme/widgets","pr":123,"session":"pr-manager-acme__widgets-pr-123","agent":"codex","attach":"tmux attach -t pr-manager-acme__widgets-pr-123",...}
```

Attach to follow along; detach with `Ctrl-b d`. The session disappears when
the agent exits or the poller closes it.

For post-mortem on a session that already ended, every spawn also captures
stdout+stderr to a persistent file:

```
$XDG_CACHE_HOME/pr-manager/<owner>__<name>/logs/pr-<n>-<ms>.log
```

The exact path appears as `log_file` in both the spawn log line and the
session-end log line, alongside an `exit_code` field parsed from the
trailing `EXIT: <n>` marker. Logs are never auto-deleted.

## Configuration

pr-manager reads a TOML file. Resolution order:

1. `--config <path>` flag.
2. `PR_MANAGER_CONFIG=<path>` env var.
3. `$XDG_CONFIG_HOME/pr-manager/config.toml` (or `~/.config/pr-manager/config.toml` when `XDG_CONFIG_HOME` is unset).
4. `./pr-manager.toml` in the current directory.

If none of those exist the process exits with a config error.

Tokens (`GITHUB_TOKEN` or whatever `token_env` names) are read directly from
the process environment. A `.env` file in the working directory is loaded if
present, but is optional — exporting the variables in your shell, a systemd
unit's `Environment=`, or any other mechanism that puts them in the
environment before pr-manager starts works equally well.

```toml
[defaults]
# Any of these may be overridden per-repo.
poll_interval_seconds = 60
recent_merges_limit = 10
log_level = "info"
agent = "claude"             # or "codex"
agent_auth = "oauth"         # or "api"
claude_extra_args = "--permission-mode bypassPermissions"
# agent may also name a custom [harnesses.<name>] table
# token_env = "GITHUB_TOKEN" # default; per-repo `token_env` overrides
# cache_root = "/abs/path"   # defaults to $XDG_CACHE_HOME or ~/.cache
# pr_authors = ["alice", "renovate[bot]"]
#   Restrict to PRs from these author logins (case-insensitive). Unset or
#   [] = process every open auto-merge PR. Lets two operators run
#   pr-manager against the same repo without stepping on each other.

[[repos]]
github_repo = "owner/repo1"
repo_path = "/abs/path/1"
# any [defaults] key may be overridden here, e.g.:
# token_env = "GITHUB_TOKEN_REPO1"
# agent = "codex"
# pr_authors = ["alice"]   # replaces (not merges with) the default list

[[repos]]
github_repo = "owner/repo2"
repo_path = "/abs/path/2"
```

`[defaults]` keys: `poll_interval_seconds`, `recent_merges_limit`, `log_level`,
`agent`, `agent_bin`, `agent_args`, `agent_auth`, `claude_bin`,
`claude_extra_args`, `codex_bin`, `codex_extra_args`, `token_env`,
`cache_root`, `pr_authors`. `[harnesses.<name>]` tables may define custom
`bin` and `args` values. `log_level` and `cache_root` are process-wide;
everything else can be overridden per-repo (`pr_authors` replaces, rather
than merges with, the default list).

`[[repos]]` requires `github_repo` (`owner/name`) and `repo_path` (absolute
path to a local checkout). Any `[defaults]` key except `log_level` and
`cache_root` may also appear here as a per-repo override.

**Tokens are never stored in the config file.** `token_env` names an env var
(default `GITHUB_TOKEN`) that pr-manager reads at startup. Use a different
`token_env` per repo if you need distinct tokens. `.env` is loaded
automatically before tokens are resolved.

For unattended Claude automation, set `claude_extra_args =
"--permission-mode bypassPermissions"` (or `--dangerously-skip-permissions`
depending on your CLI version). Codex defaults to non-interactive execution
with workspace-write sandboxing and an `--add-dir` for the pr-manager
worktree cache; use `codex_extra_args` or `agent_args` if your local Codex
profile needs different execution policy.

## Scripts

- `pr-manager` — runs the installed binary and uses
  `~/.config/pr-manager/config.toml` by default.
- `pr-manager --config /path/to/file.toml` — runs the installed binary with
  an explicit config path.
- `cargo run --release` — uses `./pr-manager.toml`
- `cargo run --release -- --config /path/to/file.toml` — explicit config path
- `cargo build --release` — build the standalone binary at `target/release/pr-manager`
- `cargo install --path .` — install the binary to `~/.cargo/bin/pr-manager`.
  With a config at `~/.config/pr-manager/config.toml` and `GITHUB_TOKEN`
  exported in your environment, you can then run `pr-manager` from anywhere
  with no flags and no `.env`.

Release maintainers: see [docs/releasing.md](docs/releasing.md) for the
crates.io (`pr-manager-cli`) and GitHub Pages-backed apt publishing workflow.

## Running as a systemd service

User-service templates:

- `pr-manager.service.example` - for a repo checkout built locally with
  `cargo build --release`.
- `pr-manager.service.binary.example` - for an installed binary, whether it
  came from `apt` at `/usr/bin/pr-manager`, `cargo install` at
  `~/.cargo/bin/pr-manager`, or a downloaded release at
  `~/.local/bin/pr-manager`.

```sh
mkdir -p ~/.config/systemd/user
cp pr-manager.service.binary.example ~/.config/systemd/user/pr-manager.service
# or, for a repo checkout built locally:
# cp pr-manager.service.example ~/.config/systemd/user/pr-manager.service
# edit the file: replace the ExecStart binary/config paths and the
# EnvironmentFile path with your own absolute paths.
systemctl --user daemon-reload
systemctl --user enable --now pr-manager

# If you want it to keep running after you log out (recommended):
sudo loginctl enable-linger "$USER"

# Inspect:
systemctl --user status pr-manager
journalctl --user -u pr-manager -f
```

The unit runs as your user (so `claude`/`codex` see your OAuth creds and
your checkouts), reads tokens from the configured `EnvironmentFile=`,
extends `PATH` so the agent CLI and lockfile resolvers are visible, and
uses `KillMode=process` so a `systemctl restart` doesn't kill the tmux
server underneath running agent sessions. See the comments in the file
for details.

## Layout

- `src/main.rs` — entrypoint; checks for tmux, builds N github clients, starts pollers, awaits signal
- `src/poller.rs` — one tick loop per repo; dedup, fork filter, session reconciliation, merger dispatch
- `src/merger.rs` — native merge fast-path (clean merge or lockfile regeneration) per PR worktree
- `src/git.rs` — thin wrapper around the `git` CLI (per-tick `git fetch origin --prune`)
- `src/github.rs` — HTTP client around the GitHub REST API (reqwest); one client per repo
- `src/agent.rs` — tmux session registry keyed on `(repo_id, pr_number)` (spawn/sweep/close/shutdown)
- `src/tmux.rs` — thin wrapper around the `tmux` CLI
- `src/prompt.rs` — per-event prompt builder (written to a tempfile)
- `src/config.rs` — TOML loader (resolves config path, validates, merges defaults+overrides)
- `src/types.rs`, `src/log.rs` — types, logger

See `docs/OVERVIEW.md` and `docs/features/pr_polling.md` for the full design,
invariants, and limitations.
