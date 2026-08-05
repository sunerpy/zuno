#!/usr/bin/env bash
# Regenerates .omo/evidence/task-1-opencode-rust.txt from live command output.
set -uo pipefail
cd "$(dirname "$0")/.."

OUT=.omo/evidence/task-1-opencode-rust.txt
mkdir -p "$(dirname "$OUT")"

run() {
  printf '\n$ %s\n' "$*" >>"$OUT"
  "$@" >>"$OUT" 2>&1
  local ec=$?
  printf '[exit %d]\n' "$ec" >>"$OUT"
  return $ec
}

sh_run() {
  printf '\n$ %s\n' "$1" >>"$OUT"
  bash -c "$1" >>"$OUT" 2>&1
  local ec=$?
  printf '[exit %d]\n' "$ec" >>"$OUT"
  return $ec
}

section() { printf '\n%s\n%s\n' "$1" "$(printf '%.0s-' $(seq 1 ${#1}))" >>"$OUT"; }

: >"$OUT"
{
  echo "task 1 — 33-crate cargo workspace with unsafe forbidden"
  echo "repo:      $(pwd)"
  echo "captured:  $(date -Is)"
  echo "host:      $(uname -srm)"
  echo
  echo "Every block below is verbatim command output, captured by"
  echo "scripts/capture-task-1-evidence.sh at the timestamp above."
} >>"$OUT"

section "environment"
run rustc --version
run cargo --version
run git --version
run jq --version
sh_run 'bun --version 2>/dev/null || node --version'
run cat rust-toolchain.toml

section "ACCEPTANCE 1 — cargo metadata lists exactly the 33 named crates"
sh_run "cargo metadata --format-version 1 --no-deps | jq -r '.packages[].name' | sort | tee /tmp/t1-roster.txt | nl -ba"
sh_run 'wc -l < /tmp/t1-roster.txt'
sh_run 'diff /tmp/t1-roster.txt crates.expected && echo "IDENTICAL: cargo metadata roster == crates.expected"'
sh_run 'cmp /tmp/t1-roster.txt crates.expected && echo "cmp: byte-for-byte identical"'

section "ACCEPTANCE 2 / QA HAPPY — cold cargo build --workspace, zero warnings"
printf '\n(target/ is removed first so this is a real full build, not a cache hit.\n Registry "Adding"/"Locking" lines are dropped from the transcript only;\n the warning count below is taken from the unfiltered log.)\n' >>"$OUT"
sh_run 'rm -rf target && cargo build --workspace 2>&1 | tee /tmp/t1-build.log | grep -vE "^ +(Adding|Locking|Updating) "; exit ${PIPESTATUS[0]}'
sh_run 'grep -c "^   Compiling oc-" /tmp/t1-build.log; echo "first-party crates compiled (expect 33)"'
sh_run 'grep -in "warning" /tmp/t1-build.log; echo "grep -i warning over the UNFILTERED log exited $? (1 == no match == zero warnings)"'
sh_run 'tail -1 /tmp/t1-build.log'

section "ACCEPTANCE 3 — every crate opts into the workspace lints"
sh_run 'for f in crates/*/Cargo.toml; do grep -q "^workspace = true" "$f" || echo "MISSING [lints] workspace = true: $f"; done; echo "crates checked: $(ls -d crates/*/ | wc -l)"'
sh_run 'grep -A1 -h "^\[lints\]" crates/oc-error/Cargo.toml crates/oc-cli/Cargo.toml'
sh_run 'grep -n "unsafe_code\|resolver\|edition\|members" Cargo.toml | head'

section "ACCEPTANCE 4 — no per-crate dependency versions"
sh_run 'grep -nE "^[a-z0-9_-]+ *= *\"[0-9]" crates/*/Cargo.toml; echo "grep exited $? (1 == no literal version in any member manifest)"'
sh_run 'grep -rn "workspace = true" crates/oc-tui/Cargo.toml'

section "ACCEPTANCE 5 — no OpenSSL-backed TLS"
printf '\n(Honest scope: no member crate depends on reqwest yet, so the WORKSPACE\n lockfile is silent on TLS and a grep over it would prove nothing. The pin is\n verified two ways instead: the root manifest, and an isolated build of exactly\n that dependency line in /tmp/t1-tls.)\n' >>"$OUT"
sh_run 'grep -A8 "^reqwest" Cargo.toml'
sh_run 'grep -c "^name = \"reqwest\"" Cargo.lock; echo "reqwest entries in the workspace lockfile (expect 0 — nothing depends on it yet)"'
sh_run 'cat /tmp/t1-tls/Cargo.toml 2>/dev/null || echo "(probe crate absent)"'
sh_run 'cd /tmp/t1-tls 2>/dev/null && cargo tree -e normal -i rustls 2>/dev/null | head -6'
sh_run 'cd /tmp/t1-tls 2>/dev/null && grep -cE "^name = \"(openssl|openssl-sys|native-tls)\"$" Cargo.lock; echo "openssl / native-tls packages in the probe lockfile (expect 0)"'
sh_run 'cd /tmp/t1-tls 2>/dev/null && grep -E "^name = \"openssl" Cargo.lock; echo "grep exited $? (only openssl-probe, a cert-path finder with no OpenSSL linkage, may appear)"'
sh_run 'tail -1 /tmp/t1-tls-build.log 2>/dev/null'

section "ACCEPTANCE 6 — reference clones relocated into the repo"
run ls -la .omo/refs
sh_run 'for d in jcode claw-code codex omo-slim; do printf "%-12s %s  entries=%s\n" "$d" "$(du -sh .omo/refs/$d | cut -f1)" "$(ls -A .omo/refs/$d | wc -l)"; done'
sh_run 'du -sh .omo/refs'

section "ACCEPTANCE 7 — preflight exits 0 on this machine"
sh_run 'bash scripts/preflight.sh'

section "ACCEPTANCE 8 — preflight exits non-zero naming a masked binary"
sh_run 'rm -rf /tmp/t1-nojq && mkdir -p /tmp/t1-nojq/bin && for t in bash sh git awk df find ls wc tr sed cat grep printf uname cargo rustc bun; do p=$(command -v $t 2>/dev/null) && ln -sf "$p" /tmp/t1-nojq/bin/$t; done; echo "PATH will contain (note: no jq):"; ls /tmp/t1-nojq/bin | tr "\n" " "; echo'
sh_run 'env -i PATH=/tmp/t1-nojq/bin HOME="$HOME" RUSTUP_HOME="${RUSTUP_HOME:-$HOME/.rustup}" CARGO_HOME="${CARGO_HOME:-$HOME/.cargo}" /tmp/t1-nojq/bin/bash scripts/preflight.sh'
sh_run 'MIN_DISK_GB=999999 bash scripts/preflight.sh'

section "QA FAILURE — an unsafe block is rejected by the workspace lint"
sh_run 'cat > crates/oc-types/src/bad.rs <<RS
//! Temporary QA fixture for task 1: proves \`unsafe_code = "forbid"\` is live.

pub fn read_through_raw_pointer() -> i32 {
    let value = 7i32;
    let ptr = &raw const value;
    unsafe { *ptr }
}
RS
printf "mod bad;\n" >> crates/oc-types/src/lib.rs
echo "--- crates/oc-types/src/bad.rs ---"; cat crates/oc-types/src/bad.rs
echo "--- crates/oc-types/src/lib.rs ---"; cat crates/oc-types/src/lib.rs'
sh_run 'cargo build -p oc-types 2>&1 | tee /tmp/t1-unsafe.log; exit ${PIPESTATUS[0]}'
sh_run 'grep -c "usage of an .unsafe. block" /tmp/t1-unsafe.log; echo "occurrences of the expected error (expect 1)"'

section "QA FAILURE — revert, and the workspace builds clean again"
sh_run 'rm -f crates/oc-types/src/bad.rs
printf "%s\n" "//! Wire and domain types shared across the workspace (sessions, messages, parts, tool payloads)." > crates/oc-types/src/lib.rs
echo "--- crates/oc-types/src/ ---"; ls crates/oc-types/src/
echo "--- crates/oc-types/src/lib.rs ---"; cat crates/oc-types/src/lib.rs'
sh_run 'cargo build --workspace 2>&1 | tee /tmp/t1-rebuild.log; exit ${PIPESTATUS[0]}'
sh_run 'grep -icE "^(warning|error)" /tmp/t1-rebuild.log; echo "warning/error lines after revert (expect 0)"'

section "NOTE — the C-toolchain tension recorded in issues.md, measured"
sh_run 'ls -d /tmp/t1-tls/target/debug/build/aws-lc-sys-*/ 2>/dev/null || echo "(tls probe crate no longer present; see .omo/notepads/opencode-rust/issues.md)"'
sh_run 'find /tmp/t1-tls/target/debug/build/aws-lc-sys-*/out -name "*.a" 2>/dev/null | head -3; find /tmp/t1-tls/target/debug/build/aws-lc-sys-*/out -name "*.o" 2>/dev/null | wc -l | sed "s/^/object files compiled by aws-lc-sys: /"'
sh_run 'for t in cc gcc cmake; do p=$(command -v "$t") && echo "$t -> $p" || echo "$t -> ABSENT"; done'

printf '\n%s\n' "end of evidence" >>"$OUT"
echo "wrote $OUT ($(wc -l <"$OUT") lines)"
