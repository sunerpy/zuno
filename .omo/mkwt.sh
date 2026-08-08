#!/usr/bin/env bash
# mkwt.sh <n> — create worktree for todo <n> at /config/workspace/ProdDir/AI/oc-wt/t<n>
set -euo pipefail
n="$1"; root=/config/workspace/ProdDir/AI/opencode-rust; wt=/config/workspace/ProdDir/AI/oc-wt/t$n
git -C "$root" worktree list --porcelain | grep -q "^worktree $wt$" && { echo "$wt (exists)"; exit 0; }
# A branch may already exist from an earlier attempt whose work was merged.
# Reuse it, reset to main, rather than failing the dispatch.
if git -C "$root" show-ref --verify --quiet "refs/heads/task-$n"; then
  git -C "$root" branch -f "task-$n" main
  git -C "$root" worktree add -q "$wt" "task-$n"
else
  git -C "$root" worktree add -q -b "task-$n" "$wt" main
fi
echo "$wt"
