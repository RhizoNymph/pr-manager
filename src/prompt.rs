use crate::types::{ManagedReason, PrEvent, RepoConfig};

pub fn build_prompt(repo: &RepoConfig, event: &PrEvent) -> String {
    let pr = &event.pr;
    let main_sha = &event.main_sha;
    let recent = &event.recent;
    let recent_list = if recent.is_empty() {
        "(none)".to_string()
    } else {
        recent
            .iter()
            .map(|m| format!("#{}", m.number))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let body_trimmed = pr.body.trim();
    let body = if body_trimmed.is_empty() {
        "(no description)"
    } else {
        body_trimmed
    };

    // Why this PR is being touched at all. Auto-merge PRs are blocked waiting
    // on the merge; opted-in ones are simply being kept current, so don't tell
    // the agent auto-merge will take over when it won't.
    let objective = match event.managed_reason {
        ManagedReason::AutoMerge => {
            "A pull request needs main merged into\n\
its head branch so GitHub auto-merge can take over."
        }
        ManagedReason::OptInLabel | ManagedReason::RepoOptIn => {
            "A pull request is opted in to pr-manager and\n\
needs main merged into its head branch to stay current with the default\n\
branch. Nothing will merge it for you afterwards; the branch just has to end\n\
up up-to-date."
        }
    };

    let template = format!(
        "You are pr-manager, an automated agent. {objective} Perform the merge and\n\
push the result, deferring to the user only when the conflict resolution is\n\
not obvious.\n\
\n\
pr-manager already attempted a non-LLM fast path before invoking you:\n\
  1. clean `git merge` (would have skipped you if it succeeded)\n\
  2. lockfile-only conflict resolution by regenerating package-lock.json,\n\
     pnpm-lock.yaml, yarn.lock, Cargo.lock, poetry.lock, or uv.lock from the merged\n\
     manifest (would also have skipped you on success)\n\
You were spawned because at least one conflict is not a recognized lockfile,\n\
or a lockfile resolver was unavailable / failed. Expect a real semantic\n\
conflict.\n\
\n\
Repository: {repo}\n\
PR #{num}: {title}\n\
\n\
Description:\n\
{body}\n\
\n\
Event metadata:\n\
  pr_number:     {num}\n\
  head_branch:   {head_branch}\n\
  head_sha:      {head_sha}\n\
  main_branch:   {base_branch}\n\
  main_sha:      {main_sha}\n\
  recent_merges: {recent_list}\n\
  managed_because: {managed_reason}\n\
\n\
You are already running in the prepared repository directory for this PR.\n\
pr-manager has already run `git fetch origin --prune` and attempted\n\
`git merge origin/{base_branch} --no-edit` before invoking you. Start by\n\
running `git status` and inspecting the current state.\n\
\n\
If git merge reports conflicts, decide whether the resolution is OBVIOUS:\n\
\n\
  OBVIOUS = the conflict is textual / structural with no semantic\n\
  ambiguity. Examples:\n\
    * lockfiles (package-lock.json, pnpm-lock.yaml, Cargo.lock,\n\
      poetry.lock, uv.lock) — regenerate, or take the version that matches the\n\
      merged package manifest.\n\
    * Both branches added imports / use statements in the same block.\n\
    * Both branches added entries to the same list, table, enum, or\n\
      switch — keep both, in a sensible order.\n\
    * Adjacent unrelated edits the merge tool flagged together.\n\
    * One side renamed/moved, the other side made an independent edit\n\
      to the same file — apply both intents.\n\
\n\
  AMBIGUOUS = anything else, especially:\n\
    * Both sides edited the same logic with conflicting intent.\n\
    * The PR description and the recent_merges context don't tell you\n\
      which semantic the user wants.\n\
    * Resolving requires understanding domain behavior beyond what is\n\
      visible in the diff.\n\
\n\
For OBVIOUS conflicts:\n\
  Use the PR description and \"gh pr view <n>\" / \"gh pr diff <n>\" on the\n\
  recent_merges numbers to inform the resolution. Edit each conflicted\n\
  file, then:\n\
    git add -A\n\
    git commit --no-edit\n\
    git push origin \"HEAD:{head_branch}\"\n\
\n\
If there are no conflicts and the merge commit already exists, push it:\n\
    git push origin \"HEAD:{head_branch}\"\n\
\n\
For AMBIGUOUS conflicts:\n\
  Do NOT push. Print a clear summary naming each conflicted file and\n\
  explaining what is ambiguous, so the user can take over.\n\
\n\
Hard rules:\n\
  * Work only in the current repository directory.\n\
  * Do not change branches.\n\
  * NEVER push to {base_branch}. Only push to \"HEAD:{head_branch}\".\n\
  * NEVER force-push.\n\
  * If a git command fails for a reason you don't understand (network errors,\n\
    ref-already-locked, etc.), abort cleanly and explain why; don't improvise.\n\
\n\
When done (success or abort), print a one-line summary on the last line so\n\
it is easy to scan in logs. Examples:\n\
  PR #{num}: pushed merge of {base_branch} into {head_branch}\n\
  PR #{num}: aborted — ambiguous conflicts in <files>",
        repo = repo.github_repo,
        num = pr.number,
        title = pr.title,
        body = body,
        head_branch = pr.head_branch,
        head_sha = pr.head_sha,
        base_branch = pr.base_branch,
        main_sha = main_sha,
        recent_list = recent_list,
        managed_reason = event.managed_reason.as_str(),
    );

    template
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{AgentConfig, AuthMode, ManagedScope, OpenPr, RecentMerge, RepoId};
    use std::path::PathBuf;

    fn repo() -> RepoConfig {
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
            pr_authors: Vec::new(),
            managed_scope: ManagedScope::AutoMergeOnly,
        }
    }

    fn event(managed_reason: ManagedReason) -> PrEvent {
        PrEvent {
            pr: OpenPr {
                number: 7,
                title: "Add widget".into(),
                body: "body".into(),
                head_branch: "feature".into(),
                head_sha: "deadbeef".into(),
                head_repo_id: 1,
                base_repo_id: 1,
                base_branch: "main".into(),
                author_login: Some("alice".into()),
                auto_merge_enabled: managed_reason == ManagedReason::AutoMerge,
                labels: Vec::new(),
            },
            main_sha: "cafebabe".into(),
            recent: vec![RecentMerge {
                number: 5,
                title: "Earlier".into(),
                merged_at: "2026-01-01T00:00:00Z".into(),
            }],
            managed_reason,
        }
    }

    #[test]
    fn auto_merge_prompt_mentions_auto_merge_taking_over() {
        let prompt = build_prompt(&repo(), &event(ManagedReason::AutoMerge));
        assert!(prompt.contains("so GitHub auto-merge can take over"));
        assert!(prompt.contains("managed_because: auto_merge"));
    }

    #[test]
    fn opted_in_prompt_does_not_promise_auto_merge() {
        for reason in [ManagedReason::OptInLabel, ManagedReason::RepoOptIn] {
            let prompt = build_prompt(&repo(), &event(reason));
            assert!(
                !prompt.contains("auto-merge can take over"),
                "opted-in prompt must not claim auto-merge will finish the job"
            );
            assert!(prompt.contains("stay current with the default"));
            assert!(prompt.contains(&format!("managed_because: {}", reason.as_str())));
        }
    }

    #[test]
    fn prompt_never_targets_the_base_branch_for_pushes() {
        for reason in [
            ManagedReason::AutoMerge,
            ManagedReason::OptInLabel,
            ManagedReason::RepoOptIn,
        ] {
            let prompt = build_prompt(&repo(), &event(reason));
            assert!(prompt.contains("NEVER push to main"));
            assert!(prompt.contains("git push origin \"HEAD:feature\""));
        }
    }
}
