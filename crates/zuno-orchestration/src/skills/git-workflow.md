# Git Workflow

Use this Skill when a requested change requires repository-aware inspection,
commit preparation, or delivery.

1. Inspect branch identity, worktree status, relevant history, the full diff,
   and the index before deciding a Git operation.
2. Separate pre-existing user changes from the requested change. Build a
   cohesive commit map and stage only the exact paths or hunks owned by one
   commit.
3. Review the staged diff and staged file list before committing. Keep generated
   files with their owning source change and do not include incidental cleanup.
4. Treat commit, merge, rebase, push, branch deletion, and worktree removal as
   distinct externally visible operations; perform only those the request
   authorizes.
5. Never discard unrelated work with reset, checkout, clean, or broad restore.
   Resolve overlap explicitly or stop with the conflicting paths named.
