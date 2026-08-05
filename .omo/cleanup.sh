#!/usr/bin/env bash
# .omo/cleanup.sh — reclaim disk after a wave. Safe: only touches paths this project creates.
set -u
root=/config/workspace/ProdDir/AI/opencode-rust
before=$(df --output=avail /config | tail -1)
# 1. temp dirs created by our tests/harnesses, older than 30 min
find /tmp -maxdepth 1 -mmin +30 \( -name 'oc-*' -o -name 'ulw-*' -o -name '.tmpoc*' \) -exec rm -rf {} + 2>/dev/null
# 2. prune merged worktrees and their branches
git -C "$root" worktree prune 2>/dev/null
for wt in $(git -C "$root" worktree list --porcelain | awk '/^worktree/{print $2}' | grep '/oc-wt/'); do
  b=$(git -C "$wt" rev-parse --abbrev-ref HEAD 2>/dev/null)
  if [ -n "$b" ] && git -C "$root" branch --merged main | tr -d ' *' | grep -qx "$b"; then
    echo "removing merged worktree $wt ($b)"
    git -C "$root" worktree remove --force "$wt" 2>/dev/null
    git -C "$root" branch -d "$b" 2>/dev/null
  fi
done
# 3. drop stale build artifacts for crates compiled inside removed worktrees
#    (the CARGO_MANIFEST_DIR hazard documented in .omo/WORKTREE.md)
if [ "${1:-}" = "--deep" ]; then
  ( cd "$root" && CARGO_TARGET_DIR="$root/target" cargo clean --profile test 2>/dev/null )
fi
after=$(df --output=avail /config | tail -1)
echo "reclaimed: $(( (after - before) / 1024 ))MB   free now: $(df -h /config | tail -1 | awk '{print $4}')"
