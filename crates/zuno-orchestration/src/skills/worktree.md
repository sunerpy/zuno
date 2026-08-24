# Worktree Preflight

Zuno does not yet expose a lifecycle-owned worktree lease through this pack.
This Skill performs preflight checks only.

1. Inspect the repository root, current branch, status, existing worktrees,
   candidate base revision, and uncommitted user changes.
2. Identify path, branch, ownership, quota, and cleanup conflicts that a future
   lease request would need to resolve.
3. Report a proposed worktree path and base revision without creating either.
4. Do not run `git worktree add`, `git worktree remove`, `git worktree prune`,
   create or delete branches, or claim that Zuno will clean them up.
5. Stop after the preflight report. A separate user-authorized runtime operation
   is required for any mutation.

This Skill does not grant tools, permissions, filesystem access, network access,
or environment access. Use only capabilities already exposed by the active
Agent profile and permission policy.
