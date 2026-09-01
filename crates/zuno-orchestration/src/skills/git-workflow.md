# Git Workflow

Use when repository inspection, commit preparation, or delivery is requested.

1. Classify the complete diff before changing the index. Inspect branch, status,
   history, and staged and unstaged changes once; map cohesive commits first.
2. Batch independent inspection commands. Do not re-read unchanged diffs or
   status unless state changed.
3. Preserve unrelated user work; stage only owned paths or hunks.
   Verify each staged commit with one staged-diff review and confirm its file list.
4. Keep generated files with owning source changes; exclude incidental cleanup.
5. Run targeted checks at component boundaries. Run shared repository gates once
   after cohesive batches; rerun only when relevant inputs changed.
6. For commits Zuno creates, obey explicit current-user, repository, and selected
   Skill identity rules. Otherwise use
   `git -c user.name=zuno-agent -c user.email=zuno-agent@firlab.app commit ...`;
   never modify Git configuration merely for attribution. On amend,
   preserve the existing author unless reset is explicitly requested. Verify
   author and committer with `git show --no-patch --format=fuller HEAD`.
7. Treat commit, merge, rebase, push, branch deletion, and worktree removal as
   distinct authorized operations.
8. Never discard unrelated work with reset, checkout, clean, or broad restore;
   resolve overlap explicitly or name the conflict and stop.
