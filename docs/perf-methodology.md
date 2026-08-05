# Performance Measurement Methodology

## Scope

Revision 1 freezes the TypeScript baseline and the formulas consumed by the
Wave 14 Rust comparison. The baseline runner invokes only the real `opencode`
TypeScript binary. A workload without a measurable TypeScript baseline fails;
it is never recorded as zero or waived.

## Workloads

- **W-idle:** cold start, one cassette-backed prompt containing one tool call,
  then a 60-second settle period.
- **W-real:** copy the user's `opencode.db`, select the largest session by
  `SUM(LENGTH(part.data))`, record its ID and message/part counts, render it,
  then execute one turn. The source database is never opened writable.
- **W-soak:** at least 500 turns over at least two hours, cassette-backed, with
  a watcher on at least 50,000 files, at least two real LSP servers, one tool
  call producing at least 50 MB, one PTY emitting at least 100 MB, and at least
  one compaction cycle. A 20-turn run is smoke-only and cannot satisfy G3.

## Sampling and aggregation

On Linux, each sample enumerates the root process and every transitive child
through `/proc/<pid>/task/*/children`, reads each live process's RSS, and sums
the complete tree. Sampling occurs every 2 seconds. Samples from the first 90
seconds are discarded as warm-up.

G1 and G2 each use five runs. The reported value is the median of the five
per-run peak total-tree RSS values. TypeScript and Rust comparison runs use an
alternating order under identical environment settings to limit machine drift.
Raw samples, per-run peaks, binary version, machine identity, and kernel version
remain in the baseline artifact.

## Liveness progress

Only a new message or part row, a part status transition, a tool call starting
or completing, or a token-usage update resets the G4 progress watchdog. A
heartbeat, repeated identical status, or bytes arriving on a stream that never
completes a turn do not count as progress.

## Frozen threshold formulas

The text between the markers is hashed by `oc-testkit`. Changing it requires an
explicit `PERF_METHODOLOGY_REVISION` bump and a newly registered digest.

<!-- PERF_FORMULAS_START -->
**G1 pass** iff `median_peak(rust, W-idle) ≤ 0.50 × median_peak(ts, W-idle)`.

**G2 pass** iff `median_peak(rust, W-real) ≤ 0.50 × median_peak(ts, W-real)`.

**G3 pass** iff the Theil–Sen slope of RSS over the final 50% of samples is `≤ 1 MB / turn` **and** `peak(final 10%) ≤ 1.5 × peak(turns 40-60)`.

**G4 pass** iff no turn exceeds **120s without state progress** and no turn exceeds a **hard deadline of 1800s**.
<!-- PERF_FORMULAS_END -->
