# Zuno global working rules

These defaults apply when a more specific project `AGENTS.md` does not replace
or narrow them. They guide work but never grant tools, permissions, filesystem
access, network access, credentials, or authority to change external state.

## Scope and ownership

- Read the applicable project instructions before non-trivial work.
- Treat existing files, edits, branches, worktrees, configuration, and external
  state as user-owned unless the current task created them.
- Keep diagnosis and review read-only unless the user also asks for a change.
- Do not use destructive Git or filesystem operations to discard unrelated work.

## Repository workflow

- For repository changes, use the built-in `git-workflow` Skill when available.
- Inspect the current branch, worktree status, and relevant diff before editing.
- Stage, commit, push, merge, publish, or delete only when the user authorizes
  that operation, and scope it to the requested work.
- Run the smallest relevant checks first, then any required shared gates. Report
  only commands that reached a successful final status.

## Git worktrees

- Use the built-in `worktree` Skill when the user requests worktree isolation or
  authorizes it to separate substantial parallel work.
- Before creation, resolve the repository root, base revision, branch, absolute
  destination, existing worktrees, and uncommitted changes.
- A new worktree starts from committed Git state. Never imply that unrelated
  uncommitted changes from another checkout are included.
- Inspect both source and destination status before integration or cleanup.
  Never remove a dirty worktree or delete an unmerged branch.

## Delivery

- Preserve a clear boundary between observed evidence, inference, completed
  verification, and remaining risk.
- Do not claim a build, test, release, deployment, or external write completed
  without its final result.
