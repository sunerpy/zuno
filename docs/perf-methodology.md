# Performance Measurement Methodology

## Scope

Revision 2 freezes the TypeScript baseline and the formulas consumed by the
Wave 14 Rust comparison. The baseline runner invokes only the real `opencode`
TypeScript binary. A workload without a measurable TypeScript baseline fails;
it is never recorded as zero or waived.

## Revision history

- **Revision 1** discarded the first 90 seconds of *every* workload as warm-up.
- **Revision 2** scopes that discard to `W-soak` alone and takes the peak over
  the whole trace for the two bounded cold-start workloads. The four frozen
  formulas are byte-identical across the two revisions; only the sampling rule
  changed, so the digest below is unchanged and revision 2 registers it again.
  *Sampling and aggregation* records the measurement that forced the correction.

A revision bump makes numbers from the two revisions non-comparable. Every
number in `benchmarks/ts-baseline.json` is recomputed from the artifact's own
retained raw samples under the revision it declares.

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
the complete tree. Sampling occurs every 2 seconds.

**The warm-up discard applies to `W-soak` only.** `W-soak` measures steady-state
growth across hours, so its startup transient is noise and its samples from the
first 90 seconds are discarded. `W-idle` and `W-real` are bounded cold-start
workloads whose peak *is* the startup-plus-turn spike, so their peak is taken
over the whole trace and no sample is discarded.

Revision 1 discarded the first 90 seconds of all three workloads, which was
wrong by construction for the two bounded ones. `W-idle`'s trace is only 148
seconds long, so the discard threw away 45 of its 75 samples — 60% of them, and
the entire cold start with them. Recomputed from the retained raw samples:
`W-idle`'s median per-run peak is 954,240 KiB (932 MB) over the whole trace
against the 746,408 KiB (729 MB) revision 1 reported, so the rule hid 203 MB of
real peak.

`W-real` is unaffected. Its turn is typed only once hydration settles at the
90-second mark, so its peak lands after the former discard window either way and
both rules give the same median of 3,026,992 KiB (2,956 MB).

The correction landed before any Rust binary was measured, so it cannot have
been fitted to a comparison result — the case a `PERF_METHODOLOGY_REVISION` bump
exists to record.

The 90-second mark keeps one role that is not aggregation: a restored session's
first turn is not typed until hydration has settled, so `W-real`'s keystrokes
reach a TUI that has finished replaying its parts rather than one still
hydrating.

G1 and G2 each use five runs. The reported value is the median of the five
per-run peak total-tree RSS values. TypeScript and Rust comparison runs use an
alternating order under identical environment settings to limit machine drift.
Raw samples, per-run peaks, binary version, machine identity, and kernel version
remain in the baseline artifact.

Run-to-run stability is evidenced from those five retained per-run peaks as
their min, median, max, and max/min ratio. A second full baseline pass is not
run to establish it: the within-pass spread of the five runs already measures
the same quantity from data the artifact already holds, and the measured spread
is wider than a two-pass 10% agreement criterion would have tolerated — 1.14x
for `W-idle` and 1.18x for `W-real`. Reporting the spread states that variance
instead of asserting a tolerance the machine does not meet.

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
