# Git Workflow

Use this Skill when repository inspection, commit preparation, or delivery is
requested.

1. Classify the complete diff before changing the index. Inspect the branch,
   status, history, and staged and unstaged changes once; map cohesive commits
   before staging.
2. Batch independent inspection commands when results are needed together.
   Do not re-read unchanged diffs or repeat status unless state changed.
3. Preserve unrelated user work. Stage only exact owned paths or hunks.
   Verify each staged commit with one staged-diff review and confirm its file
   list.
4. Keep generated files with owning source changes; exclude incidental cleanup.
5. Run targeted checks at real component boundaries.
   Run shared repository gates once after cohesive batches; rerun only when
   relevant inputs changed.
6. Treat commit, merge, rebase, push, branch deletion, and worktree removal as
   distinct authorized operations.
7. Never discard unrelated work with reset, checkout, clean, or broad restore;
   resolve overlap explicitly or name the conflict and stop.
