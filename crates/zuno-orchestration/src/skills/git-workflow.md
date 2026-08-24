# Git Workflow

Use this Skill when a requested change requires repository-aware inspection,
commit preparation, or delivery.

1. Inspect the current branch, worktree status, relevant history, and staged and
   unstaged diffs before acting.
2. Preserve unrelated user changes. Scope edits and staging to the requested
   files, and verify the exact staged diff before a commit.
3. Run the relevant checks and capture their final status before describing the
   change as verified.
4. Commit, merge, push, create or delete branches, or remove worktrees only when
   the user's request authorizes that specific action.
5. Do not use destructive reset or checkout operations to discard work that was
   not created by the current task.

This Skill does not grant tools, permissions, filesystem access, network access,
or environment access. Use only capabilities already exposed by the active
Agent profile and permission policy.
