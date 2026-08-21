#!/usr/bin/env bash
# Run the whole workspace test suite in parallel across suites.
#
# WHY THIS EXISTS
#
# `cargo test --workspace` builds in parallel but then runs the 224 test suites
# it produced strictly one after another. Measured on a 32-core host with a warm
# target, the build part is a 0.7 s no-op and the run part is 219.9 s — so the
# local test loop is bound by serialised *execution*, not by compilation. The 224
# suites sum to 206.1 s of in-harness time, and the slowest single suite is
# 46.9 s, so there is roughly a 4x gap between what cargo does and what the
# machine can do.
#
# This script closes that gap by launching the already-built test binaries
# concurrently. It changes nothing about which tests run: `make ci` still calls
# plain `cargo test`, and this is an additive local fast path.
#
# WHY IT CAPTURES CARGO'S ENVIRONMENT INSTEAD OF ASSUMING IT
#
# The first version of this runner invoked the test binaries straight from a
# shell and subprocess-heavy suites failed because their environment differed
# from Cargo's. The cause is PATH: Cargo runs test binaries with mise *installs*
# resolved, while a bare shell inherits mise *shims* first, and a shim cannot be
# spawned directly. Cargo also exports LD_LIBRARY_PATH for the aws-lc-sys,
# libsqlite3-sys and jemalloc build-script outputs, plus SSL_CERT_FILE and
# SSL_CERT_DIR, and `.cargo/config.toml` force-sets
# JEMALLOC_SYS_WITH_MALLOC_CONF.
#
# Rather than re-derive any of that, this script captures the real environment
# from a real cargo test-run through CARGO_TARGET_<TRIPLE>_RUNNER, and reuses it.
# The capture happens on every invocation because LD_LIBRARY_PATH embeds
# build-script output hashes that move whenever a build script reruns.
#
# WHAT WAS REJECTED, AND WHY
#
# Running `cargo test -p <crate>` for all 36 crates concurrently keeps cargo's
# environment for free and is exactly correct (4280 passed / 0 failed / 8
# ignored) but it is *slower* than sequential: 284.6 s against 219.9 s. Cargo
# takes an exclusive lock on the build directory for the whole run, so all 36
# invocations queue on it — every one of the 36 logged "Blocking waiting for file
# lock on build directory".
#
# `cargo-nextest` is the productised form of what this script does by hand and
# would be the better answer, but it is not installed on this host and adding it
# is out of scope.
set -uo pipefail

JOBS=${JOBS:-8}
THREADS=${THREADS:-4}
OFFLINE=${OFFLINE:---offline}

ROOT=$(cd "$(dirname "$0")/.." && pwd)
WORK=${WORK:-$ROOT/target/test-parallel}
cd "$ROOT" || exit 1

command -v python3 > /dev/null 2>&1 \
  || { echo "python3 is required to schedule the suites and parse cargo's JSON output"; exit 1; }

rm -rf "$WORK"
mkdir -p "$WORK/logs"

triple=$(rustc -vV | sed -n 's/^host: //p')
runner_var="CARGO_TARGET_$(printf '%s' "$triple" | tr 'a-z-' 'A-Z_')_RUNNER"

# ── 1. Build every test binary, and record where each one lives. ─────────────
# `--no-run` so the build is separated from the run; the JSON stream is the only
# supported way to learn the executable paths, and the manifest directory is the
# cwd cargo would have used.
echo "==> building test binaries"
if ! cargo test --workspace $OFFLINE --no-run --message-format=json \
  > "$WORK/artifacts.json" 2> "$WORK/build.log"; then
  echo "build failed:"
  tail -30 "$WORK/build.log"
  exit 1
fi

python3 - "$WORK" <<'PY'
import json, os, sys
work = sys.argv[1]
rows, seen = [], set()
for line in open(f'{work}/artifacts.json'):
    line = line.strip()
    if not line.startswith('{'):
        continue
    try:
        m = json.loads(line)
    except json.JSONDecodeError:
        continue
    if m.get('reason') != 'compiler-artifact':
        continue
    exe = m.get('executable')
    if not exe or not m.get('profile', {}).get('test'):
        continue
    if exe in seen:
        continue
    seen.add(exe)
    rows.append((exe, os.path.dirname(m['manifest_path'])))
with open(f'{work}/suites.tsv', 'w') as fh:
    for exe, cwd in rows:
        fh.write(f'{exe}\t{cwd}\n')
print(f'    {len(rows)} test binaries')
PY

suites=$(wc -l < "$WORK/suites.tsv")
[[ $suites -gt 0 ]] || { echo "no test binaries were produced"; exit 1; }

# ── 2. Capture the environment cargo runs test binaries under. ───────────────
# One cheap suite is enough: the environment is per-run, not per-suite. `--list`
# makes the subject enumerate its tests and exit instead of running them.
echo "==> capturing cargo's test environment"
cat > "$WORK/dumpenv.sh" <<'EOF'
#!/bin/sh
env > "$CAPTURE_TO"
exec "$@" --list
EOF
chmod +x "$WORK/dumpenv.sh"

CAPTURE_TO="$WORK/cargo-env.txt" \
  env "$runner_var=$WORK/dumpenv.sh" \
  cargo test -p zuno-error $OFFLINE > "$WORK/capture.log" 2>&1 || true

if [[ ! -s "$WORK/cargo-env.txt" ]]; then
  echo "could not capture cargo's test environment; see $WORK/capture.log"
  exit 1
fi
# A capture that lost PATH would silently reintroduce the shim failure this
# script exists to avoid, so the two load-bearing variables are checked by name.
grep -q '^PATH=' "$WORK/cargo-env.txt" \
  || { echo "captured environment has no PATH; refusing to run"; exit 1; }

# ── 3. Run every suite, longest-first, JOBS at a time. ───────────────────────
# Longest-first matters: one suite is 46.9 s and the whole run cannot finish
# sooner than that, so it has to start immediately rather than be picked up last.
# Durations come from the previous run when one exists; the first run has none
# and simply uses cargo's order.
echo "==> running $suites suites (JOBS=$JOBS THREADS=$THREADS, width $((JOBS * THREADS)))"
start=$(date +%s.%N)

python3 - "$WORK" "$JOBS" "$THREADS" "$runner_var" <<'PY'
import concurrent.futures as cf
import json, os, subprocess, sys, time

work, jobs, threads, runner_var = sys.argv[1], int(sys.argv[2]), sys.argv[3], sys.argv[4]

env = {}
for line in open(f'{work}/cargo-env.txt').read().split('\n'):
    if '=' in line:
        key, value = line.split('=', 1)
        env[key] = value
# The capture ran under the runner shim; leaving these in would make every suite
# re-enter the shim and only list its tests.
env.pop(runner_var, None)
env.pop('CAPTURE_TO', None)

rows = [l.rstrip('\n').split('\t') for l in open(f'{work}/suites.tsv') if l.strip()]

cache = f'{os.path.dirname(work)}/test-parallel-durations.json'
try:
    known = json.load(open(cache))
except (OSError, ValueError):
    known = {}
rows.sort(key=lambda r: -known.get(os.path.basename(r[0]), 0.0))

def run(indexed):
    index, (exe, cwd) = indexed
    suite_env = dict(env)
    # Cargo sets these per-package; a stale value from the captured package
    # would point a suite at the wrong manifest.
    suite_env['CARGO_MANIFEST_DIR'] = cwd
    suite_env['CARGO_MANIFEST_PATH'] = os.path.join(cwd, 'Cargo.toml')
    suite_env['PWD'] = cwd
    began = time.monotonic()
    done = subprocess.run(
        [exe, f'--test-threads={threads}'],
        cwd=cwd, env=suite_env, capture_output=True, text=True,
    )
    elapsed = time.monotonic() - began
    with open(f'{work}/logs/{index}.log', 'w') as fh:
        fh.write(done.stdout + done.stderr)
    return done.returncode, exe, elapsed

with cf.ThreadPoolExecutor(max_workers=jobs) as pool:
    results = list(pool.map(run, enumerate(rows)))

with open(f'{work}/codes.tsv', 'w') as fh:
    for code, exe, elapsed in results:
        fh.write(f'{code}\t{elapsed:.3f}\t{exe}\n')

known.update({os.path.basename(exe): elapsed for _, exe, elapsed in results})
try:
    json.dump(known, open(cache, 'w'), indent=0, sort_keys=True)
except OSError:
    pass
PY

binaries_done=$(date +%s.%N)

# ── 4. Doctests. ────────────────────────────────────────────────────────────
# `--no-run` does not build doctests and no test binary contains them, so
# skipping this step would drop 31 tests while still reporting success. Cargo
# owns doctests, so this is one plain cargo invocation.
echo "==> running doctests"
cargo test --workspace $OFFLINE --doc > "$WORK/doctests.log" 2>&1
doctest_code=$?
end=$(date +%s.%N)

# ── 5. Report, and refuse to look successful without evidence. ──────────────
echo
python3 - "$WORK" "$suites" "$doctest_code" <<'PY'
import glob, os, re, sys

work, expected, doctest_code = sys.argv[1], int(sys.argv[2]), int(sys.argv[3])
passed = failed = ignored = summaries = 0
result = re.compile(
    r'^test result:.*?(\d+) passed; (\d+) failed; (\d+) ignored', re.M)

def tally(text):
    """Sum one log's harness summaries; returns (summaries, passed, failed, ignored)."""
    n = p = f = g = 0
    for a, b, c in result.findall(text):
        p += int(a); f += int(b); g += int(c); n += 1
    return n, p, f, g

for path in sorted(glob.glob(f'{work}/logs/*.log')):
    try:
        text = open(path).read()
    except OSError:
        continue
    n, p, f, g = tally(text)
    summaries += n; passed += p; failed += f; ignored += g

# Doctests are counted separately so their absence is detectable. Folding them
# into the same total hides it: dropping the doctest step entirely still left
# 4249 passes and 189 summaries, which looked healthy while silently skipping 31
# tests. The binary count cannot reveal that, so the doctest log is checked on
# its own terms.
try:
    doctest_text = open(f'{work}/doctests.log').read()
except OSError:
    doctest_text = ''
doctest_summaries, p, f, g = tally(doctest_text)
summaries += doctest_summaries; passed += p; failed += f; ignored += g

codes = [l.rstrip('\n').split('\t') for l in open(f'{work}/codes.tsv') if l.strip()]
broken = [c for c in codes if c[0] != '0']

print(f'suites launched   {len(codes)} (expected {expected})')
print(f'harness summaries {summaries}')
print(f'tests             {passed} passed, {failed} failed, {ignored} ignored')

slow = sorted(codes, key=lambda c: -float(c[1]))[:5]
print('slowest suites:')
for _, secs, exe in slow:
    print(f'  {float(secs):7.2f}s  {os.path.basename(exe)}')

problems = []
if len(codes) != expected:
    problems.append(f'{expected} suites were built but {len(codes)} ran')
if summaries < expected:
    problems.append(
        f'{expected} suites ran but only {summaries} printed a result; '
        'a suite that produces no summary has not been verified')
if doctest_summaries == 0:
    problems.append(
        'no doctest summaries were parsed; `--no-run` does not build doctests '
        'and no test binary contains them, so skipping them drops 31 tests '
        'while every other count still looks healthy')
if passed == 0:
    problems.append('zero tests passed, so this run proves nothing')
if failed:
    problems.append(f'{failed} tests failed')
if broken:
    for code, _, exe in broken:
        problems.append(f'exit {code} from {os.path.basename(exe)}')
if doctest_code != 0:
    problems.append(f'doctests exited {doctest_code}')

if problems:
    print('\nFAILED')
    for problem in problems:
        print(f'  - {problem}')
    print(f'\nlogs: {work}/logs/')
    sys.exit(1)
print('\nOK')
PY
verdict=$?

awk -v a="$start" -v m="$binaries_done" -v b="$end" 'BEGIN {
  printf "\nsuites %.2fs | doctests %.2fs | wall %.2fs\n", m - a, b - m, b - a
}'
exit $verdict
