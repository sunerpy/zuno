#!/usr/bin/env python3
"""Schedule already-built Cargo test binaries and require their harness evidence."""

import concurrent.futures as cf
import json, os, shutil, sys, threading
from ci_process import cancellation_signals, run as run_process

work = sys.argv[1]
jobs = int(sys.argv[2])
threads = int(sys.argv[3])
runner_var = sys.argv[4]
suite_timeout = int(sys.argv[5])
duration_hints = sys.argv[6]
if jobs < 1 or threads < 1 or suite_timeout < 1:
    raise SystemExit(f'JOBS, THREADS and SUITE_TIMEOUT must be positive')

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

def run(indexed):
    index, (exe, cwd, target, suite_key) = indexed
    suite_env = dict(env)
    # Cargo sets these per-package; a stale value from the captured package
    # would point a suite at the wrong manifest.
    suite_env['CARGO_MANIFEST_DIR'] = cwd
    suite_env['CARGO_MANIFEST_PATH'] = os.path.join(cwd, 'Cargo.toml')
    suite_env['PWD'] = cwd
    arguments = [exe, f'--test-threads={threads}']
    if target == 'startup':
        arguments.append('--nocapture')
    result = run_process(
        arguments, cwd, suite_env, suite_timeout,
        f'{work}/logs/{index}.log', cancelled,
    )
    return index, result.code, exe, target, suite_key, result.elapsed

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

cancelled = threading.Event()
with cancellation_signals(cancelled):
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
