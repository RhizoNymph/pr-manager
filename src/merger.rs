//! Native fast-path for merging `origin/<base>` into a PR's head branch
//! without spawning a coding agent.
//!
//! The merger handles the two cases where an LLM adds no value:
//!   1. Clean merge (no conflicts) — just push.
//!   2. Conflicts limited to known lockfiles — regenerate via the matching
//!      package manager, commit, push.
//!
//! Anything else (real semantic conflicts, missing package manager, push
//! rejection, unexpected git error) returns [`MergeOutcome::NeedsAgent`] and
//! the caller spawns the agent as before. The merger always cleans up its
//! worktree before returning so the agent starts from a fresh slate.
//!
//! The merger trusts that the caller has already run
//! `git fetch origin --prune` in `repo_path` this tick, so `origin/<branch>`
//! refs are current.
use std::path::{Component, Path, PathBuf};
use std::process::{Output, Stdio};

use thiserror::Error;
use tokio::process::Command;

use crate::types::{OpenAutoMergePr, RepoConfig};

#[derive(Debug)]
pub enum MergeOutcome {
    /// Clean merge — pushed and worktree removed. Skip the agent.
    Pushed,
    /// All conflicts were lockfiles; regenerated, committed, pushed, worktree
    /// removed. Skip the agent. Lockfile paths are repo-relative.
    PushedAfterLockfile { lockfiles: Vec<String> },
    /// Native attempt could not finish; worktree has been cleaned up. Caller
    /// should spawn the agent.
    NeedsAgent { reason: NeedsAgentReason },
}

#[derive(Debug)]
pub enum NeedsAgentReason {
    /// At least one conflict is not a recognized lockfile. Repo-relative paths.
    SemanticConflicts { files: Vec<String> },
    /// All conflicts were lockfiles, but a resolver failed (tool missing,
    /// non-zero exit, etc.).
    LockfileResolverFailed { lockfile: String, detail: String },
    /// Anything else: worktree setup, push rejection, unexpected git error.
    Other(String),
}

#[derive(Debug, Clone, Copy)]
#[allow(clippy::enum_variant_names)]
enum Lockfile {
    NpmPackageLock,
    PnpmLock,
    YarnLock,
    CargoLock,
    PoetryLock,
}

impl Lockfile {
    fn detect(repo_relative: &str) -> Option<Self> {
        let base = Path::new(repo_relative).file_name()?.to_str()?;
        match base {
            "package-lock.json" => Some(Self::NpmPackageLock),
            "pnpm-lock.yaml" => Some(Self::PnpmLock),
            "yarn.lock" => Some(Self::YarnLock),
            "Cargo.lock" => Some(Self::CargoLock),
            "poetry.lock" => Some(Self::PoetryLock),
            _ => None,
        }
    }

    fn resolver(self) -> (&'static str, &'static [&'static str]) {
        // Each command assumes the conflicted lockfile has been deleted first
        // (see `resolve_lockfile`); the package manager then regenerates from
        // the post-merge manifest.
        match self {
            Self::NpmPackageLock => (
                "npm",
                &["install", "--package-lock-only", "--no-audit", "--no-fund"],
            ),
            Self::PnpmLock => ("pnpm", &["install", "--lockfile-only"]),
            Self::YarnLock => ("yarn", &["install", "--mode", "update-lockfile"]),
            Self::CargoLock => ("cargo", &["generate-lockfile"]),
            Self::PoetryLock => ("poetry", &["lock"]),
        }
    }

    #[cfg(test)]
    fn as_str(self) -> &'static str {
        match self {
            Self::NpmPackageLock => "package-lock.json",
            Self::PnpmLock => "pnpm-lock.yaml",
            Self::YarnLock => "yarn.lock",
            Self::CargoLock => "Cargo.lock",
            Self::PoetryLock => "poetry.lock",
        }
    }
}

#[derive(Debug, Error)]
enum SetupError {
    #[error("worktree path {path} is not safe under pr-manager cache base {base}: {reason}")]
    PathEscape {
        path: PathBuf,
        base: PathBuf,
        reason: &'static str,
    },
    #[error("git: {0}")]
    Git(String),
    #[error("io: {0}")]
    Io(String),
}

pub async fn try_native_merge(repo: &RepoConfig, pr: &OpenAutoMergePr) -> MergeOutcome {
    let wt = repo.worktree_base.join(format!("pr-{}", pr.number));

    if let Err(e) = validate_worktree_path(&repo.worktree_base, &wt) {
        return MergeOutcome::NeedsAgent {
            reason: NeedsAgentReason::Other(format!("unsafe worktree path: {e}")),
        };
    }

    if let Err(e) = setup_worktree(&repo.repo_path, &repo.worktree_base, &wt, &pr.head_branch).await
    {
        return MergeOutcome::NeedsAgent {
            reason: NeedsAgentReason::Other(format!("worktree setup: {e}")),
        };
    }

    let merge_target = format!("origin/{}", pr.base_branch);
    let merge_out = match git_capture(&wt, &["merge", &merge_target, "--no-edit"]).await {
        Ok(o) => o,
        Err(e) => {
            cleanup_worktree(&repo.repo_path, &repo.worktree_base, &wt).await;
            return MergeOutcome::NeedsAgent {
                reason: NeedsAgentReason::Other(format!("git merge spawn: {e}")),
            };
        }
    };

    if merge_out.status.success() {
        return finalize_push(
            &repo.repo_path,
            &repo.worktree_base,
            &wt,
            &pr.head_branch,
            MergeOutcome::Pushed,
        )
        .await;
    }

    let conflicts = match git_capture(&wt, &["diff", "--name-only", "--diff-filter=U"]).await {
        Ok(o) if o.status.success() => parse_lines(&o.stdout),
        _ => Vec::new(),
    };
    if conflicts.is_empty() {
        let stderr = String::from_utf8_lossy(&merge_out.stderr)
            .trim()
            .to_string();
        let _ = git_capture(&wt, &["merge", "--abort"]).await;
        cleanup_worktree(&repo.repo_path, &repo.worktree_base, &wt).await;
        return MergeOutcome::NeedsAgent {
            reason: NeedsAgentReason::Other(format!(
                "git merge failed without conflicts: {stderr}"
            )),
        };
    }

    let unrecognized: Vec<String> = conflicts
        .iter()
        .filter(|p| Lockfile::detect(p).is_none())
        .cloned()
        .collect();
    if !unrecognized.is_empty() {
        let _ = git_capture(&wt, &["merge", "--abort"]).await;
        cleanup_worktree(&repo.repo_path, &repo.worktree_base, &wt).await;
        return MergeOutcome::NeedsAgent {
            reason: NeedsAgentReason::SemanticConflicts { files: conflicts },
        };
    }

    for rel in &conflicts {
        let kind = Lockfile::detect(rel).expect("filtered above");
        if let Err(detail) = resolve_lockfile(&wt, rel, kind).await {
            let _ = git_capture(&wt, &["merge", "--abort"]).await;
            cleanup_worktree(&repo.repo_path, &repo.worktree_base, &wt).await;
            return MergeOutcome::NeedsAgent {
                reason: NeedsAgentReason::LockfileResolverFailed {
                    lockfile: rel.clone(),
                    detail,
                },
            };
        }
    }

    if let Err(e) = git_check(&wt, &["add", "-A"]).await {
        let _ = git_capture(&wt, &["merge", "--abort"]).await;
        cleanup_worktree(&repo.repo_path, &repo.worktree_base, &wt).await;
        return MergeOutcome::NeedsAgent {
            reason: NeedsAgentReason::Other(format!("git add after lockfile resolve: {e}")),
        };
    }
    if let Err(e) = git_check(&wt, &["commit", "--no-edit"]).await {
        let _ = git_capture(&wt, &["merge", "--abort"]).await;
        cleanup_worktree(&repo.repo_path, &repo.worktree_base, &wt).await;
        return MergeOutcome::NeedsAgent {
            reason: NeedsAgentReason::Other(format!("git commit after lockfile resolve: {e}")),
        };
    }

    finalize_push(
        &repo.repo_path,
        &repo.worktree_base,
        &wt,
        &pr.head_branch,
        MergeOutcome::PushedAfterLockfile {
            lockfiles: conflicts,
        },
    )
    .await
}

async fn finalize_push(
    repo_path: &Path,
    worktree_base: &Path,
    wt: &Path,
    head_branch: &str,
    success: MergeOutcome,
) -> MergeOutcome {
    let push_spec = format!("HEAD:{head_branch}");
    if let Err(e) = git_check(wt, &["push", "origin", &push_spec]).await {
        cleanup_worktree(repo_path, worktree_base, wt).await;
        return MergeOutcome::NeedsAgent {
            reason: NeedsAgentReason::Other(format!("git push: {e}")),
        };
    }
    cleanup_worktree(repo_path, worktree_base, wt).await;
    success
}

async fn setup_worktree(
    repo_path: &Path,
    worktree_base: &Path,
    wt: &Path,
    head_branch: &str,
) -> Result<(), SetupError> {
    validate_worktree_path(worktree_base, wt)?;
    std::fs::create_dir_all(worktree_base)
        .map_err(|e| SetupError::Io(format!("mkdir {}: {e}", worktree_base.display())))?;
    reject_symlink_worktree_path(worktree_base, wt)?;

    // Drop any stale worktree at this path. Two cases:
    //   1. git already knows about it -> `git worktree remove --force`.
    //   2. plain directory left over from a non-git tool -> rm -rf.
    if known_worktree(repo_path, wt).await {
        git_check(
            repo_path,
            &["worktree", "remove", "--force", &wt.to_string_lossy()],
        )
        .await
        .map_err(SetupError::Git)?;
    } else if wt.exists() {
        remove_stale_worktree_path(worktree_base, wt)?;
    }

    if let Some(parent) = wt.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| SetupError::Io(format!("mkdir {}: {e}", parent.display())))?;
    }

    let head_ref = format!("origin/{head_branch}");
    git_check(
        repo_path,
        &[
            "worktree",
            "add",
            "--detach",
            &wt.to_string_lossy(),
            &head_ref,
        ],
    )
    .await
    .map_err(SetupError::Git)?;

    Ok(())
}

async fn cleanup_worktree(repo_path: &Path, worktree_base: &Path, wt: &Path) {
    if let Err(err) = validate_worktree_path(worktree_base, wt) {
        tracing::warn!(err = %err, "refusing to clean unsafe worktree path");
        return;
    }
    if let Err(err) = reject_symlink_worktree_path(worktree_base, wt) {
        tracing::warn!(err = %err, "refusing to clean symlink worktree path");
        return;
    }

    let path = wt.to_string_lossy();
    let res = git_capture(repo_path, &["worktree", "remove", "--force", &path]).await;
    if let Ok(out) = &res {
        if out.status.success() {
            return;
        }
    }
    // Worktree may have already been removed (success path), or git refuses
    // for some reason. Ensure the path is gone.
    if let Err(err) = remove_stale_worktree_path(worktree_base, wt) {
        tracing::warn!(err = %err, "failed to remove stale pr-manager worktree path");
    }
}

async fn known_worktree(repo_path: &Path, wt: &Path) -> bool {
    let needle = format!("worktree {}", wt.display());
    match git_capture(repo_path, &["worktree", "list", "--porcelain"]).await {
        Ok(out) if out.status.success() => String::from_utf8_lossy(&out.stdout)
            .lines()
            .any(|l| l == needle),
        _ => false,
    }
}

fn validate_worktree_path(worktree_base: &Path, wt: &Path) -> Result<(), SetupError> {
    if !worktree_base.is_absolute() {
        return Err(path_escape(
            wt,
            worktree_base,
            "cache base must be absolute",
        ));
    }
    if !wt.is_absolute() {
        return Err(path_escape(
            wt,
            worktree_base,
            "worktree path must be absolute",
        ));
    }
    if has_parent_dir(worktree_base) || has_parent_dir(wt) {
        return Err(path_escape(
            wt,
            worktree_base,
            "paths must not contain '..' components",
        ));
    }
    if wt.file_name().is_none() {
        return Err(path_escape(
            wt,
            worktree_base,
            "worktree path has no final component",
        ));
    }
    match wt.parent() {
        Some(parent) if parent == worktree_base => Ok(()),
        _ => Err(path_escape(
            wt,
            worktree_base,
            "worktree path must be a direct child of the cache base",
        )),
    }
}

fn remove_stale_worktree_path(worktree_base: &Path, wt: &Path) -> Result<(), SetupError> {
    validate_worktree_path(worktree_base, wt)?;
    let meta = match std::fs::symlink_metadata(wt) {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(SetupError::Io(format!("stat {}: {e}", wt.display()))),
    };

    if meta.file_type().is_symlink() {
        return Err(path_escape(
            wt,
            worktree_base,
            "refusing to remove symlink at worktree path",
        ));
    }
    if meta.is_dir() {
        std::fs::remove_dir_all(wt)
            .map_err(|e| SetupError::Io(format!("remove {}: {e}", wt.display())))?;
    } else {
        std::fs::remove_file(wt)
            .map_err(|e| SetupError::Io(format!("remove {}: {e}", wt.display())))?;
    }
    Ok(())
}

fn reject_symlink_worktree_path(worktree_base: &Path, wt: &Path) -> Result<(), SetupError> {
    validate_worktree_path(worktree_base, wt)?;
    match std::fs::symlink_metadata(wt) {
        Ok(meta) if meta.file_type().is_symlink() => Err(path_escape(
            wt,
            worktree_base,
            "refusing to operate on symlink at worktree path",
        )),
        Ok(_) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(SetupError::Io(format!("stat {}: {e}", wt.display()))),
    }
}

fn has_parent_dir(path: &Path) -> bool {
    path.components()
        .any(|component| matches!(component, Component::ParentDir))
}

fn path_escape(path: &Path, base: &Path, reason: &'static str) -> SetupError {
    SetupError::PathEscape {
        path: path.to_path_buf(),
        base: base.to_path_buf(),
        reason,
    }
}

async fn resolve_lockfile(wt: &Path, rel: &str, kind: Lockfile) -> Result<(), String> {
    let abs = wt.join(rel);
    // Delete the conflicted file so the package manager regenerates fresh
    // from the merged manifest. Resolvers like `npm install --package-lock-only`
    // and `cargo generate-lockfile` will complain if the existing file still
    // contains conflict markers.
    if abs.exists() {
        std::fs::remove_file(&abs).map_err(|e| format!("remove {}: {e}", abs.display()))?;
    }

    let dir = abs
        .parent()
        .ok_or_else(|| format!("no parent for {}", abs.display()))?;
    let (bin, args) = kind.resolver();
    let output = Command::new(bin)
        .args(args)
        .current_dir(dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .map_err(|e| format!("spawn {bin}: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "{bin} {} (exit {}) in {}: {}",
            args.join(" "),
            output.status.code().unwrap_or(-1),
            dir.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    if !abs.exists() {
        return Err(format!(
            "{bin} succeeded but did not produce {}",
            abs.display()
        ));
    }
    Ok(())
}

async fn git_capture(cwd: &Path, args: &[&str]) -> Result<Output, String> {
    Command::new("git")
        .args(args)
        .current_dir(cwd)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .map_err(|e| format!("spawn git: {e}"))
}

async fn git_check(cwd: &Path, args: &[&str]) -> Result<(), String> {
    let out = git_capture(cwd, args).await?;
    if !out.status.success() {
        return Err(format!(
            "git {} (exit {}): {}",
            args.join(" "),
            out.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(())
}

fn parse_lines(bytes: &[u8]) -> Vec<String> {
    String::from_utf8_lossy(bytes)
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_known_lockfiles() {
        assert!(matches!(
            Lockfile::detect("package-lock.json"),
            Some(Lockfile::NpmPackageLock)
        ));
        assert!(matches!(
            Lockfile::detect("packages/web/pnpm-lock.yaml"),
            Some(Lockfile::PnpmLock)
        ));
        assert!(matches!(
            Lockfile::detect("crates/core/Cargo.lock"),
            Some(Lockfile::CargoLock)
        ));
        assert!(matches!(
            Lockfile::detect("yarn.lock"),
            Some(Lockfile::YarnLock)
        ));
        assert!(matches!(
            Lockfile::detect("backend/poetry.lock"),
            Some(Lockfile::PoetryLock)
        ));
    }

    #[test]
    fn detect_rejects_non_lockfiles() {
        assert!(Lockfile::detect("src/main.rs").is_none());
        assert!(Lockfile::detect("README.md").is_none());
        assert!(Lockfile::detect("Cargo.toml").is_none());
        assert!(Lockfile::detect("package.json").is_none());
    }

    #[test]
    fn parse_lines_strips_blank_and_trims() {
        let got = parse_lines(b"  a\n\nb \n\n");
        assert_eq!(got, vec!["a".to_string(), "b".to_string()]);
    }

    #[test]
    fn validate_worktree_path_accepts_direct_cache_child() {
        let base = PathBuf::from("/tmp/pr-manager/acme__widgets/wt");
        let wt = base.join("pr-123");
        assert!(validate_worktree_path(&base, &wt).is_ok());
    }

    #[test]
    fn validate_worktree_path_rejects_escapes() {
        let base = PathBuf::from("/tmp/pr-manager/acme__widgets/wt");

        assert!(validate_worktree_path(&base, &base.join("nested/pr-123")).is_err());
        assert!(
            validate_worktree_path(&base, &PathBuf::from("/tmp/pr-manager/other/wt/pr-123"))
                .is_err()
        );
        assert!(validate_worktree_path(
            &PathBuf::from("/tmp/pr-manager/../acme__widgets/wt"),
            &PathBuf::from("/tmp/pr-manager/../acme__widgets/wt/pr-123"),
        )
        .is_err());
    }

    #[test]
    fn lockfile_str_round_trip() {
        for k in [
            Lockfile::NpmPackageLock,
            Lockfile::PnpmLock,
            Lockfile::YarnLock,
            Lockfile::CargoLock,
            Lockfile::PoetryLock,
        ] {
            assert_eq!(
                Lockfile::detect(k.as_str()).map(|d| d.as_str()),
                Some(k.as_str())
            );
        }
    }
}
