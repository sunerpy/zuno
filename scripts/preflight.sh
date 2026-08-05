#!/usr/bin/env bash
# Preflight gate: refuse to start work in an environment that cannot finish it.
# Every failure names the tool and how to install it. Exits non-zero if any
# required check fails; optional checks only warn.
set -uo pipefail

MIN_DISK_GB=${MIN_DISK_GB:-20}
MIN_RUSTC=${MIN_RUSTC:-1.96.0}

failures=0
warnings=0

pass() { printf '  ok    %s\n' "$1"; }

fail() {
  printf '  FAIL  %s\n' "$1"
  printf '        -> %s\n' "$2"
  failures=$((failures + 1))
}

warn() {
  printf '  warn  %s\n' "$1"
  printf '        -> %s\n' "$2"
  warnings=$((warnings + 1))
}

# Compare dotted versions without bc or sort -V edge cases: pad each field to
# 5 digits and compare the resulting strings lexicographically.
version_key() {
  printf '%s' "$1" | awk -F'[^0-9]+' '{printf "%05d%05d%05d", $1, $2, $3}'
}

echo "preflight: opencode-rust"
echo
echo "required tools"

if command -v git >/dev/null 2>&1; then
  pass "git $(git --version | awk '{print $3}')"
else
  fail "git not found" "install git (apt install git / brew install git)"
fi

if command -v cargo >/dev/null 2>&1 && command -v rustc >/dev/null 2>&1; then
  rustc_version=$(rustc --version | awk '{print $2}')
  if [ "$(version_key "$rustc_version")" -ge "$(version_key "$MIN_RUSTC")" ]; then
    pass "rustc $rustc_version (cargo $(cargo --version | awk '{print $2}'))"
  else
    fail "rustc $rustc_version is older than $MIN_RUSTC" \
      "run 'rustup update stable'; this workspace needs edition 2024 and resolver 3"
  fi
else
  fail "rust toolchain not found" \
    "install rustup from https://rustup.rs, then 'rustup toolchain install $MIN_RUSTC'"
fi

# A JS runtime is not optional: the plugin compat host and the differential
# oracle against the TypeScript opencode both need one.
if command -v bun >/dev/null 2>&1; then
  pass "bun $(bun --version)"
elif command -v node >/dev/null 2>&1; then
  pass "node $(node --version)"
else
  fail "neither bun nor node found" \
    "install bun (curl -fsSL https://bun.sh/install | bash) or node 20+; the JS compat host and the differential oracle both require one"
fi

if command -v jq >/dev/null 2>&1; then
  pass "jq $(jq --version)"
else
  fail "jq not found" \
    "install jq (apt install jq / brew install jq); the crate-roster gate pipes cargo metadata through it"
fi

echo
echo "resources"

avail_kb=$(df -Pk . | awk 'NR==2 {print $4}')
avail_gb=$((avail_kb / 1024 / 1024))
if [ "$avail_gb" -ge "$MIN_DISK_GB" ]; then
  pass "disk ${avail_gb}G free (need ${MIN_DISK_GB}G)"
else
  fail "only ${avail_gb}G free on $(pwd), need ${MIN_DISK_GB}G" \
    "free space or set CARGO_TARGET_DIR to a larger volume; a full debug build of this workspace plus test artifacts does not fit below ${MIN_DISK_GB}G"
fi

echo
echo "optional tools"

if command -v cargo-deny >/dev/null 2>&1 || cargo deny --version >/dev/null 2>&1; then
  pass "cargo-deny"
else
  warn "cargo-deny not found" \
    "install with 'cargo install cargo-deny'; needed by the supply-chain gate, not by a normal build"
fi

if [ -d .omo/refs ] && [ -n "$(ls -A .omo/refs 2>/dev/null)" ]; then
  pass "reference clones present ($(find .omo/refs -mindepth 1 -maxdepth 1 -type d | wc -l | tr -d ' ') trees)"
else
  warn ".omo/refs is missing or empty" \
    "reference clones are gitignored; re-clone them if a task cites .omo/refs paths"
fi

echo
if [ "$failures" -gt 0 ]; then
  echo "preflight FAILED: $failures required check(s) failed, $warnings warning(s)"
  exit 1
fi
echo "preflight passed: 0 failures, $warnings warning(s)"
