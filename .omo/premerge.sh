#!/usr/bin/env bash
# premerge.sh <n> [<n>...] — merge task branches into main with the three known
# wave-7 breakage classes turned into hard stops instead of silent damage.
#
# Why this exists: a mechanical union-merge is safe ONLY for homogeneous
# single-line lists (`pub mod` runs). Wave 7 proved it is unsafe for:
#   1. Cargo.toml   — section semantics; line-union produced three
#                     [dev-dependencies] tables and mis-filed 7 runtime deps.
#   2. lib.rs heads — `//!` position semantics; a second inner-doc run below the
#                     items is error[E0753], 14 errors, crate does not compile.
#   3. Cargo.lock   — not unionable; a merge took one side wholesale and dropped
#                     8 packages. `cargo build` silently repairs it, so only a
#                     --locked check catches it.
# This script refuses to auto-resolve 1-3 and makes you rewrite them by fact
# (`git show <branch>:<path>`), then runs the real gate.
set -uo pipefail

root=/config/workspace/ProdDir/AI/opencode-rust
cd "$root" || exit 2

die() { printf '\n\033[31mSTOP\033[0m %s\n' "$*" >&2; exit 1; }
ok()  { printf '\033[32m  ok\033[0m %s\n' "$*"; }
note(){ printf '\033[33m  ..\033[0m %s\n' "$*"; }

[ $# -ge 1 ] || die "usage: premerge.sh <n> [<n>...]"

# Refuse to start from a dirty tree: a failed merge must be distinguishable
# from pre-existing local edits.
[ -z "$(git status --porcelain)" ] || die "main has uncommitted changes; commit or stash first"

base=$(git rev-parse --short HEAD)
printf 'main starts at %s\n' "$base"

for n in "$@"; do
  br="task-$n"
  git rev-parse --verify -q "$br" >/dev/null || die "$br does not exist"
  ahead=$(git rev-list --count "main..$br")
  [ "$ahead" -gt 0 ] || die "$br has no commits ahead of main — the subagent did not commit"

  printf '\n=== merging %s (%s commit(s)) ===\n' "$br" "$ahead"

  if git merge --no-edit --no-ff "$br" >/tmp/premerge.$$ 2>&1; then
    ok "clean merge"
  else
    conflicts=$(git diff --name-only --diff-filter=U)
    [ -n "$conflicts" ] || { cat /tmp/premerge.$$; die "merge failed with no conflict list"; }
    printf '  conflicts:\n%s\n' "$(echo "$conflicts" | sed 's/^/    /')"

    # The three classes that must NEVER be auto-resolved.
    hazard=0
    while IFS= read -r f; do
      case "$f" in
        *Cargo.toml)
          note "$f is a TOML conflict — section semantics. Do NOT union."
          note "   rewrite by fact:  git show $br:$f   vs   git show HEAD:$f"
          note "   then verify exactly one [dependencies] and one [dev-dependencies]"
          hazard=1 ;;
        Cargo.lock)
          note "Cargo.lock is NOT unionable and NOT hand-mergeable."
          note "   resolve by:  git checkout --ours Cargo.lock && cargo metadata --offline >/dev/null"
          note "   then confirm no package was dropped:  git diff --stat HEAD -- Cargo.lock"
          hazard=1 ;;
        *lib.rs|*mod.rs)
          note "$f — if the conflict is in the leading //! block, merge the PROSE"
          note "   into the single existing header. A //! run below any item is E0753."
          hazard=1 ;;
      esac
    done <<< "$conflicts"

    [ "$hazard" -eq 1 ] && die "hazard-class conflict in $br: resolve by hand, then re-run with the remaining branches"
    die "conflicts in $br need manual resolution"
  fi
done

echo
echo "=== gate ==="

# Wave 45: I resolved a code conflict with a script that only understood
# append-only prose, then ran `git add -A && git commit` anyway -- so a file with
# `<<<<<<<` markers landed on main. Nothing in the gate noticed, because a
# conflict marker inside a Rust file is a *parse* error only if that file is
# compiled, and the file in question was a test helper.
#
# This check runs before anything expensive and refuses to proceed while a marker
# exists anywhere in the tree, staged or not.
note "no conflict markers anywhere"
marked=$(grep -rlE '^(<{7}|={7}|>{7})( |$)' \
  --include='*.rs' --include='*.toml' --include='*.md' --include='*.json' \
  --include='*.sh' --include='*.yml' --include='*.yaml' \
  crates docs .omo Cargo.toml 2>/dev/null || true)
if [ -n "$marked" ]; then
  printf '%s\n' "$marked" | sed 's/^/    /'
  die "conflict markers present -- resolve them before the gate runs, and never blind-commit a merge"
fi
ok "no conflict markers"

# Wave 54: I wrote `git add -A -f` to get past .omo's blanket ignore rule, and the
# `-f` took `/target` with it -- 48,148 build-product files landed on main under a
# fully green gate. The conflict-marker check above could not see it, because build
# products are a different kind of dirty. Same root cause as that incident though:
# an operation wider than the intent.
note "no build products tracked"
tracked_target=$(git ls-files target/ | wc -l | tr -d ' ')
if [ "$tracked_target" != "0" ]; then
  die "$tracked_target files under target/ are tracked -- something used a wide \`git add -f\`; unstage them before merging"
fi
ok "no build products tracked"

# Order matters: metadata --locked first, because a broken lock makes every
# later failure look like a code problem. `cargo build` would silently repair it.
note "cargo metadata --locked --offline"
if ! cargo metadata --locked --offline >/dev/null 2>/tmp/meta.$$; then
  note "retrying once — metadata can report a stale exit code right after a manifest write"
  cargo metadata --locked --offline >/dev/null 2>/tmp/meta.$$ \
    || { sed 's/^/    /' /tmp/meta.$$; die "lock is not reproducible; CI's --locked build would fail here"; }
fi
ok "lock reproducible"

# Exactly one of each dependency table per manifest. The wave-7 bug was three
# [dev-dependencies] tables in one file, which cargo metadata ACCEPTED.
note "manifest table uniqueness"
bad=""
while IFS= read -r m; do
  for tbl in dependencies dev-dependencies build-dependencies; do
    c=$(grep -c "^\[$tbl\]$" "$m")
    [ "$c" -le 1 ] || bad="$bad\n    $m has $c [$tbl] tables"
  done
done < <(find crates -name Cargo.toml -not -path '*/target/*')
[ -z "$bad" ] || { printf "%b\n" "$bad"; die "duplicate dependency tables"; }
ok "one table each"

# Inner doc comments must precede all items. Catch a stranded //! run before
# the compiler does, with a line number.
note "stranded //! blocks"
strand=$(find crates -name '*.rs' -not -path '*/target/*' -print0 \
  | xargs -0 awk 'FNR==1{seen=0} /^[[:space:]]*(pub[[:space:]]+)?(mod|use|fn|struct|enum|trait|impl|const|static|type)\b/{seen=1} /^[[:space:]]*\/\/!/{if(seen){print FILENAME":"FNR; nextfile}}')
[ -z "$strand" ] || { printf '%s\n' "$strand" | sed 's/^/    /'; die "inner doc comment after an item (error[E0753])"; }
ok "no stranded inner docs"

note "cargo build --workspace --offline"
cargo build --workspace --offline 2>&1 | tail -3
[ "${PIPESTATUS[0]}" -eq 0 ] || die "build failed"
ok "build clean"

# build being green does NOT mean the tests compile — test-only code is not
# built by `cargo build`. This is the integration gate.
note "cargo test --workspace --offline"
out=$(cargo test --workspace --offline 2>&1)
echo "$out" | grep -E "^(error|warning: unused)" | head -20
pass=$(echo "$out" | grep -oE '^test result: ok\. [0-9]+' | grep -oE '[0-9]+' | paste -sd+ | bc 2>/dev/null || echo 0)
fail=$(echo "$out" | grep -cE '^test result: FAILED')
[ "$fail" -eq 0 ] || { echo "$out" | grep -B5 "^failures:" | head -40; die "$fail failing test target(s)"; }

# A test target that fails to COMPILE never emits a `test result:` line, so the
# count above cannot see it. Merging task-84 printed "0 failing targets" while the
# same output carried `error[E0463]` and `error: doctest failed`. A gate that
# recognises one spelling of failure is not a gate.
for spelling in \
  '^error: doctest failed' \
  '^error: could not compile' \
  '^error: test failed' \
  '^error\[E[0-9]+\]'
do
  hits=$(echo "$out" | grep -cE "$spelling")
  [ "$hits" -eq 0 ] || {
    echo "$out" | grep -E "$spelling" -A6 | head -30
    die "$hits occurrence(s) of /$spelling/ — a test target failed to build"
  }
done
ok "$pass tests pass, 0 failing targets, no compile-time test failures"

note "cargo clippy --workspace --all-targets --offline"
cl=$(cargo clippy --workspace --all-targets --offline 2>&1 | grep -cE '^(warning|error)')
[ "$cl" -eq 0 ] || { cargo clippy --workspace --all-targets --offline 2>&1 | grep -E '^(warning|error)' -A3 | head -30; die "$cl clippy diagnostic(s)"; }
ok "0 clippy warnings"

note "cargo fmt --all --check"
cargo fmt --all --check || die "fmt not clean"
ok "fmt clean"

rm -f /tmp/premerge.$$ /tmp/meta.$$
printf '\n\033[32mMERGED\033[0m %s -> %s  (%s tests)\n' "$base" "$(git rev-parse --short HEAD)" "$pass"
