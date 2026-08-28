# Git Worktree Isolation

Use this Skill when the user requests a Git worktree or authorizes worktree
isolation for substantial parallel work.

1. Resolve the repository root and inspect the current branch, status, relevant
   refs, and `git worktree list --porcelain`. Preserve every unrelated staged,
   unstaged, and untracked user change.
2. Decide whether isolation is useful. Do not create a worktree for a read-only
   inspection or a small change in a clean checkout merely because the mechanism
   exists.
3. Run `git worktree add` only when the user's request explicitly authorizes
   creation. Use an explicit reviewed base revision, branch name, and absolute
   destination outside the main checkout; do not rely on unresolved variables,
   command substitution, or broad globs.
4. Remember that the new checkout contains committed Git state only. Do not move,
   copy, stash, reset, or discard another checkout's uncommitted changes unless
   the user separately requests that exact operation.
5. Perform the isolated task inside the created worktree. Re-check its branch and
   status before editing, committing, or reporting results.
6. Merge, rebase, cherry-pick, push, or delete a branch only when the user has
   authorized that operation. Inspect divergence and the exact commits first.
7. Before cleanup, inspect the target with `git status --short --branch`, include
   untracked files, and verify that required commits are reachable elsewhere.
   Never remove a dirty worktree or delete an unmerged branch. Use
   `git worktree prune` only for reviewed stale administrative entries.
8. Report the worktree path, branch, base revision, retained changes, integration
   state, and whether cleanup remains.

This Skill guides user-authorized shell operations. Zuno does not own leases, quotas, or automatic cleanup for worktrees created this way.

This Skill does not grant tools, permissions, filesystem access, network access,
or environment access. Use only capabilities already exposed by the active
Agent profile and permission policy.
