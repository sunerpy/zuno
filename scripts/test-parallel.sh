#!/usr/bin/env bash
# Run the whole workspace test suite in parallel across suites.
#
# WHY THIS EXISTS
#
# `cargo test --workspace` builds in parallel but then runs the roughly two
# hundred test suites it produced strictly one after another. Measured on a
# 32-core host with a warm
# target, the build part is a 0.7 s no-op and the run part is 219.9 s — so the
# local test loop is bound by serialised *execution*, not by compilation. The 224
# suites sum to 206.1 s of in-harness time, and the slowest single suite is
# 46.9 s, so there is roughly a 4x gap between what cargo does and what the
# machine can do.
#
# This script closes that gap by launching the already-built test binaries
# concurrently. It is the Windows CI scheduler because one process per test
# case makes nextest disproportionately expensive on Windows, and it remains the
# offline fallback behind `make test-par` on developer machines.
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
# Linux CI uses pinned cargo-nextest. Windows deliberately keeps this
# binary-granularity scheduler: it starts roughly two hundred harnesses rather
# than several thousand individual test processes.
set -uo pipefail

JOBS=${JOBS:-8}
THREADS=${THREADS:-4}
SUITE_TIMEOUT=${SUITE_TIMEOUT:-300}
OFFLINE=${OFFLINE:---offline}
RUN_DOCTESTS=${RUN_DOCTESTS:-1}
# Windows commonly starts Python with a legacy console encoding such as GBK.
# Test names and failure output are UTF-8 data, so one non-ASCII character must
# not crash the scheduler before it writes codes.tsv and the final verdict.
export PYTHONUTF8=${PYTHONUTF8:-1}
export PYTHONIOENCODING=${PYTHONIOENCODING:-utf-8:backslashreplace}
OFFLINE_ARGS=()
read -r -a OFFLINE_ARGS <<< "$OFFLINE"

case "$RUN_DOCTESTS" in
  0 | 1) ;;
  *)
    echo "RUN_DOCTESTS must be 0 or 1, got: $RUN_DOCTESTS"
    exit 2
    ;;
esac

ROOT=$(cd "$(dirname "$0")/.." && pwd)
WORK=${WORK:-$ROOT/target/test-parallel}
cd "$ROOT" || exit 1

PYTHON=
for candidate in python3 python; do
  if command -v "$candidate" > /dev/null 2>&1 \
    && "$candidate" -c 'import json, os, sys' > /dev/null 2>&1; then
    PYTHON=$(command -v "$candidate")
    break
  fi
done
if [[ -z "$PYTHON" ]]; then
  echo "Python is required to schedule the suites and parse Cargo's JSON output"
  exit 1
fi

rm -rf "$WORK"
mkdir -p "$WORK/logs"

triple=$(rustc -vV | sed -n 's/^host: //p')
runner_var="CARGO_TARGET_$(printf '%s' "$triple" | tr 'a-z-' 'A-Z_')_RUNNER"
runner_script="$WORK/dumpenv.py"
capture_path="$WORK/cargo-env.json"
runner_python_for_cargo="$PYTHON"
runner_script_for_cargo="$runner_script"
capture_path_for_runner="$capture_path"
case "$triple" in
  *-windows-*)
    command -v cygpath > /dev/null 2>&1 \
      || { echo "cygpath is required by the Windows Git Bash scheduler"; exit 1; }
    runner_python_for_cargo=$(cygpath -ms "$PYTHON")
    runner_script_dir_for_cargo=$(cygpath -ms "$(dirname "$runner_script")")
    runner_script_for_cargo="$runner_script_dir_for_cargo/$(basename "$runner_script")"
    capture_path_for_runner=$(cygpath -m "$capture_path")
    ;;
esac
for runner_path in "$runner_python_for_cargo" "$runner_script_for_cargo"; do
  case "$runner_path" in
    *[[:space:]]*)
      echo "Cargo runner path must not contain whitespace: $runner_path"
      exit 1
      ;;
  esac
done
runner_command="$runner_python_for_cargo $runner_script_for_cargo"

overall_start=$(date +%s.%N)

# ── 1. Build every test binary, and record where each one lives. ─────────────
# `--no-run` so the build is separated from the run; the JSON stream is the only
# supported way to learn the executable paths, and the manifest directory is the
# cwd cargo would have used.
echo "==> building test binaries"
if ! cargo test --workspace "${OFFLINE_ARGS[@]}" \
  --no-run \
  --timings \
  --message-format=json \
  > "$WORK/artifacts.json" \
  2> >(tee "$WORK/build.log" >&2); then
  echo "build failed:"
  tail -30 "$WORK/build.log"
  exit 1
fi

"$PYTHON" - "$WORK" <<'PY'
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
    target = m.get('target', {}).get('name', os.path.basename(exe))
    cwd = os.path.dirname(m['manifest_path'])
    suite_key = f'{os.path.basename(cwd)}:{target}'
    rows.append((exe, cwd, target, suite_key))
with open(f'{work}/suites.tsv', 'w') as fh:
    for exe, cwd, target, suite_key in rows:
        fh.write(f'{exe}\t{cwd}\t{target}\t{suite_key}\n')
print(f'    {len(rows)} test binaries')
PY

suites=$(wc -l < "$WORK/suites.tsv")
[[ $suites -gt 0 ]] || { echo "no test binaries were produced"; exit 1; }
build_done=$(date +%s.%N)

# ── 2. Capture the environment cargo runs test binaries under. ───────────────
# One cheap Cargo invocation is enough: the environment is per-run, not
# per-suite. The runner records it and exits successfully without starting the
# subject; executing the native Windows test binary from the Python runner
# caused STATUS_ACCESS_VIOLATION on windows-2022 and is unnecessary for capture.
echo "==> capturing cargo's test environment"
cat > "$runner_script" <<'PY'
import json
import os

capture_to = os.environ["CAPTURE_TO"]
with open(capture_to, "w", encoding="utf-8") as handle:
    json.dump(dict(os.environ), handle, ensure_ascii=False)
PY

CAPTURE_TO="$capture_path_for_runner" \
  env "$runner_var=$runner_command" \
  cargo test -p zuno-error "${OFFLINE_ARGS[@]}" > "$WORK/capture.log" 2>&1 || true

if [[ ! -s "$capture_path" ]]; then
  echo "could not capture cargo's test environment; see $WORK/capture.log"
  exit 1
fi
# A capture that lost PATH would silently reintroduce the shim failure this
# script exists to avoid, so the two load-bearing variables are checked by name.
"$PYTHON" - "$capture_path" <<'PY'
import json
import shutil
import subprocess
import sys

env = json.load(open(sys.argv[1], encoding="utf-8"))
if not isinstance(env, dict) or not env.get("PATH"):
    raise SystemExit("captured environment has no PATH; refusing to run")
rg = shutil.which("rg", path=env["PATH"])
if rg is None:
    raise SystemExit(
        "captured Cargo environment cannot resolve required runtime dependency `rg`"
    )
subprocess.run(
    [rg, "--version"],
    env=env,
    stdout=subprocess.DEVNULL,
    stderr=subprocess.PIPE,
    check=True,
)
PY
capture_done=$(date +%s.%N)

# ── 3. Isolate startup telemetry, then run functional suites concurrently. ───
# The startup suite records wall-clock measurements, so run it before unrelated
# test processes can compete with it. Shared hosted runners do not enforce the
# absolute ceilings; stable hosts opt in with ZUNO_ENFORCE_STARTUP_BUDGET=1.
# Every functional suite remains in the bounded worker pool. Longest-first then
# keeps the 46.9 s floor from becoming the final straggler. Durations come from
# the previous run when one exists.
echo "==> running $suites suites (at most $JOBS binaries concurrently, $THREADS harness threads each, timeout=${SUITE_TIMEOUT}s)"
start=$(date +%s.%N)

"$PYTHON" - "$WORK" "$JOBS" "$THREADS" "$runner_var" "$SUITE_TIMEOUT" \
  "$ROOT/scripts/test-parallel-duration-hints.json" <<'PY'
import concurrent.futures as cf
import json, os, shutil, signal, subprocess, sys, time

work = sys.argv[1]
jobs = int(sys.argv[2])
threads = int(sys.argv[3])
runner_var = sys.argv[4]
suite_timeout = int(sys.argv[5])
duration_hints = sys.argv[6]
if jobs < 1 or threads < 1:
    raise SystemExit(f'JOBS and THREADS must be positive, got {jobs} and {threads}')

env = json.load(open(f'{work}/cargo-env.json', encoding='utf-8'))
if not isinstance(env, dict) or not env.get('PATH'):
    raise SystemExit('captured Cargo environment is not a string map with PATH')
# The capture ran under the runner shim; leaving these in would make every suite
# re-enter the shim and only list its tests.
env.pop(runner_var, None)
env.pop('CAPTURE_TO', None)

rows = [l.rstrip('\n').split('\t') for l in open(f'{work}/suites.tsv') if l.strip()]

cache = f'{os.path.dirname(work)}/test-parallel-durations.json'
try:
    known = json.load(open(duration_hints))
except (OSError, ValueError):
    known = {}
try:
    known.update(json.load(open(cache)))
except (OSError, ValueError):
    pass
rows.sort(key=lambda r: -known.get(r[3], 0.0))

def terminate_tree(process):
    if os.name == 'nt':
        subprocess.run(
            ['taskkill', '/PID', str(process.pid), '/T', '/F'],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            check=False,
        )
        return
    try:
        os.killpg(process.pid, signal.SIGKILL)
    except ProcessLookupError:
        pass

def run_once(exe, cwd, target, suite_env):
    began = time.monotonic()
    creationflags = (
        getattr(subprocess, 'CREATE_NEW_PROCESS_GROUP', 0)
        if os.name == 'nt'
        else 0
    )
    arguments = [exe, f'--test-threads={threads}']
    if target == 'startup':
        arguments.append('--nocapture')
    process = subprocess.Popen(
        arguments,
        cwd=cwd,
        env=suite_env,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        encoding='utf-8',
        errors='replace',
        start_new_session=os.name != 'nt',
        creationflags=creationflags,
    )
    try:
        stdout, stderr = process.communicate(timeout=suite_timeout)
        code = process.returncode
        output = stdout + stderr
    except subprocess.TimeoutExpired:
        terminate_tree(process)
        try:
            stdout, stderr = process.communicate(timeout=10)
        except subprocess.TimeoutExpired:
            process.kill()
            stdout, stderr = process.communicate()
        code = 124
        output = (
            stdout
            + stderr
            + f'\nTIMEOUT: suite {target} exceeded {suite_timeout} seconds; '
              'its process tree was terminated\n'
        )
    elapsed = time.monotonic() - began
    return code, output, elapsed

def run(indexed):
    index, (exe, cwd, target, suite_key) = indexed
    suite_env = dict(env)
    # Cargo sets these per-package; a stale value from the captured package
    # would point a suite at the wrong manifest.
    suite_env['CARGO_MANIFEST_DIR'] = cwd
    suite_env['CARGO_MANIFEST_PATH'] = os.path.join(cwd, 'Cargo.toml')
    suite_env['PWD'] = cwd
    code, output, elapsed = run_once(exe, cwd, target, suite_env)
    with open(f'{work}/logs/{index}.log', 'w') as fh:
        fh.write(output)
    return index, code, exe, target, suite_key, elapsed

def publish_startup_measurement(index):
    source = f'{work}/logs/{index}.log'
    stable = f'{work}/startup.log'
    shutil.copyfile(source, stable)
    summary = os.environ.get('GITHUB_STEP_SUMMARY')
    if not summary:
        return
    text = open(source, encoding='utf-8', errors='replace').read()
    marker = 'G1 STARTUP MEASUREMENT'
    if marker not in text:
        return
    measurement = text[text.index(marker):].split('\nok\n', 1)[0].rstrip()
    with open(summary, 'a', encoding='utf-8') as output:
        output.write(
            '### Startup measurement\n\n'
            'Absolute wall-clock budgets are observational on shared hosted runners. '
            'Use `ZUNO_ENFORCE_STARTUP_BUDGET=1` on an otherwise-idle stable host '
            'to enforce them.\n\n'
            '```text\n'
        )
        output.write(measurement)
        output.write('\n```\n\n')

indexed_rows = list(enumerate(rows))
isolated_suites = {'startup'}
isolated_rows = [
    indexed for indexed in indexed_rows if indexed[1][2] in isolated_suites
]
parallel_rows = [
    indexed for indexed in indexed_rows if indexed[1][2] not in isolated_suites
]
isolated_counts = {
    target: sum(row[2] == target for row in rows) for target in isolated_suites
}
invalid_isolated = {
    target: count for target, count in isolated_counts.items() if count != 1
}
if invalid_isolated:
    raise SystemExit(
        f'isolated timing suites must each resolve exactly once: {invalid_isolated}'
    )

results = []
failure_details = 0

def report(result, completed, total):
    global failure_details
    index, code, exe, target, _, elapsed = result
    state = 'ok' if code == 0 else f'exit {code}'
    print(
        f'    completed {completed}/{total}: '
        f'{target} ({os.path.basename(exe)}, {elapsed:.2f}s, {state})',
        flush=True,
    )
    if code != 0 and failure_details < 5:
        failure_details += 1
        try:
            lines = open(
                f'{work}/logs/{index}.log',
                encoding='utf-8',
                errors='replace',
            ).read().splitlines()
        except OSError as error:
            print(f'      could not read failure log: {error}', flush=True)
            return
        print('      failure tail:', flush=True)
        for line in lines[-16:]:
            print(f'        {line}', flush=True)

completed = 0
for indexed in isolated_rows:
    print(f'    running isolated timing suite: {indexed[1][2]}', flush=True)
    result = run(indexed)
    results.append(result)
    publish_startup_measurement(result[0])
    completed += 1
    report(result, completed, len(rows))

with cf.ThreadPoolExecutor(max_workers=jobs) as pool:
    futures = [pool.submit(run, indexed) for indexed in parallel_rows]
    for future in cf.as_completed(futures):
        result = future.result()
        results.append(result)
        completed += 1
        if (
            completed == 1
            or completed % 10 == 0
            or result[1] != 0
            or completed == len(rows)
        ):
            report(result, completed, len(rows))

with open(f'{work}/codes.tsv', 'w') as fh:
    for _, code, exe, _, _, elapsed in sorted(results):
        fh.write(f'{code}\t{elapsed:.3f}\t{exe}\n')

known.update({suite_key: elapsed for _, _, _, _, suite_key, elapsed in results})
try:
    json.dump(known, open(cache, 'w'), indent=0, sort_keys=True)
except OSError:
    pass
PY

binaries_done=$(date +%s.%N)

# ── 4. Doctests. ────────────────────────────────────────────────────────────
# `--no-run` does not build doctests and no test binary contains them, so
# skipping this step without an explicit owner would drop 31 tests while still
# reporting success. Local fallback runs retain them. Hosted Windows sets
# RUN_DOCTESTS=0 because the Linux source gate already owns the same doctest
# surface once; repeating rustdoc on Windows added more than eight minutes to the
# critical path without adding a platform-specific executable contract.
doctest_code=0
doctest_state=disabled
if [[ "$RUN_DOCTESTS" == "1" ]]; then
  doctest_state=ran
  echo "==> running doctests"
  cargo test --workspace "${OFFLINE_ARGS[@]}" \
    --doc \
    --no-fail-fast \
    2>&1 | tee "$WORK/doctests.log"
  doctest_code=${PIPESTATUS[0]}
else
  : > "$WORK/doctests.log"
  echo "==> skipping doctests explicitly (RUN_DOCTESTS=0)"
fi
end=$(date +%s.%N)

# ── 5. Report, and refuse to look successful without evidence. ──────────────
echo
"$PYTHON" - "$WORK" "$suites" "$doctest_code" "$doctest_state" <<'PY'
import glob, os, re, sys

work = sys.argv[1]
expected = int(sys.argv[2])
doctest_code = int(sys.argv[3])
doctest_state = sys.argv[4]
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
if doctest_state == 'disabled':
    print('doctests          skipped explicitly; another CI leg must own them')
else:
    print(f'doctest summaries {doctest_summaries}')

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
if doctest_state == 'ran' and doctest_summaries == 0:
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
if doctest_state == 'ran' and doctest_code != 0:
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

awk \
  -v a="$overall_start" \
  -v c="$build_done" \
  -v d="$capture_done" \
  -v s="$start" \
  -v m="$binaries_done" \
  -v b="$end" \
  'BEGIN {
  printf "\nbuild %.2fs | env %.2fs | suites %.2fs | doctests %.2fs | total %.2fs\n",
    c - a, d - c, m - s, b - m, b - a
}'
exit "$verdict"
