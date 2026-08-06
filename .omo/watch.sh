#!/usr/bin/env bash
# watch.sh <n> [<n>...] — foreground monitor for in-flight task branches.
#
# Polls every 60s and prints a one-line-per-branch status. Exits 0 as soon as
# EVERY named branch has at least one commit ahead of main, or after the cap.
#
# Why a polling loop rather than idling on notifications: a subagent can work for
# two hours, and the orchestrator wants to see progress accumulate (untracked
# files appearing, then a commit) rather than learning about it only at the end.
# `wt=` is the real progress signal before a commit exists.
set -uo pipefail

root=/config/workspace/ProdDir/AI/opencode-rust
wt=/config/workspace/ProdDir/AI/oc-wt
cd "$root" || exit 2

[ $# -ge 1 ] || { echo "usage: watch.sh <n> [<n>...]" >&2; exit 2; }

interval=${WATCH_INTERVAL:-60}
cap=${WATCH_CAP:-30}          # polls, so default ~30 minutes

for ((poll = 1; poll <= cap; poll++)); do
  done_count=0
  line=""
  for n in "$@"; do
    ahead=$(git log --oneline "main..task-$n" 2>/dev/null | wc -l | tr -d ' ')
    dirty=$(cd "$wt/t$n" 2>/dev/null && git status --porcelain 2>/dev/null | wc -l | tr -d ' ')
    dirty=${dirty:-0}
    if [ "$ahead" -gt 0 ]; then
      done_count=$((done_count + 1))
      line="$line  t$n:COMMITTED"
    else
      line="$line  t$n:wt=$dirty"
    fi
  done
  printf '[%s] poll %2d/%d%s\n' "$(date +%H:%M:%S)" "$poll" "$cap" "$line"

  if [ "$done_count" -eq $# ]; then
    echo "ALL $# BRANCHES COMMITTED"
    exit 0
  fi
  sleep "$interval"
done

echo "CAP REACHED with $done_count/$# committed"
exit 1
