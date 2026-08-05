#!/usr/bin/env bash
# mkwt.sh <n> — create worktree for todo <n> at /config/workspace/ProdDir/AI/oc-wt/t<n>
set -euo pipefail
n="$1"; root=/config/workspace/ProdDir/AI/opencode-rust; wt=/config/workspace/ProdDir/AI/oc-wt/t$n
git -C "$root" worktree list --porcelain | grep -q "^worktree $wt$" && { echo "$wt (exists)"; exit 0; }
git -C "$root" worktree add -q -b "task-$n" "$wt" main
echo "$wt"
