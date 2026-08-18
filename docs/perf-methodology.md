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
- **W-real:** copy the pinned database snapshot, restore the **pinned session**
  (see *W-real subject pin*), record its ID and message/part counts, render it,
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

## Allocator comparison

M1 measured two release-profile Zuno binaries on 2026-08-18 with the same pinned
W-real snapshot, workload implementation, 2-second process-tree sampler, windows,
and five-run aggregation above. The default binary used jemalloc with
`dirty_decay_ms:1000,muzzy_decay_ms:1000,narenas:4`; the control used
`--no-default-features`, which selects the Linux GNU fallback
`MALLOC_ARENA_MAX=4,MALLOC_MMAP_THRESHOLD_=262144`.

| allocator | workload | five peak RSS runs (KiB) | min / median / max (KiB) | max/min |
| --- | --- | --- | --- | --- |
| tuned jemalloc | W-idle | 27,340; 27,352; 27,920; 27,132; 27,044 | 27,044 / 27,340 / 27,920 | 1.0324x |
| tuned glibc | W-idle | 26,404; 26,204; 26,536; 25,332; 25,364 | 25,332 / 26,204 / 26,536 | 1.0475x |
| tuned jemalloc | W-real | 878,000; 1,198,872; 1,210,148; 1,549,164; 862,132 | 862,132 / 1,198,872 / 1,549,164 | 1.7969x |
| tuned glibc | W-real | 1,656,720; 1,653,376; 1,653,348; 1,653,236; 1,652,628 | 1,652,628 / 1,653,348 / 1,656,720 | 1.0025x |

The jemalloc W-real median is 454,476 KiB (27.49%) below tuned glibc, while its
W-idle median is 1,136 KiB (4.34%) higher. W-real also has a wide 1.7969x spread,
so the five raw peaks remain part of the result rather than presenting the median
as a stability claim. The default remains tuned jemalloc because the sustained,
large-session workload is the memory risk this setting addresses. A 1-second
decay can add `mmap`/`munmap` work and page faults during repeated large
allocation/free cycles; throughput was not measured here, so the decision is a
deliberate peak-RSS trade-off, not a throughput improvement claim.

The measured binaries had SHA-256
`acc509815cc2179fd02549e095672aa775954e44d31faebd5d28f0da0dc49796`
(jemalloc) and
`458193fc429efdda280b2b0f5838dcf5f94d19484f4313d66383763a90bd7480`
(glibc). Raw JSON reports were written to `target/perf/allocator-jemalloc.json`
and `target/perf/allocator-system.json`; their SHA-256 digests were
`dbeee391b90397b7e48b1c26bf45b1d799a9271fcd7db805766c5598072f44e0`
and `795d9cfa9c492867280a97628ae026fad6c36d592213dd3f08ec7bbdc1b45ed4`.

## W-real subject pin

`W-real` measures one specific session in one specific database snapshot. Both
are recorded as `W_REAL_SUBJECT` in `crates/zuno-testkit/src/perf/subject.rs`:

| field | value |
| --- | --- |
| session | `ses_2bcaee257ffeFZNJrmtpi3ZglR` |
| messages | 931 |
| parts | 3,620 |
| `SUM(LENGTH(part.data))` | 105,118,812 |
| snapshot | `/config/.local/share/opencode/opencode.db.bak.20260408` |
| snapshot bytes | 2,630,582,272 |
| snapshot SHA-256 | `e2cde4df08cd580d0a4f03068b2d861275ca8aef983fef6578968f7f7a2a18a7` |

Selection used to be *"whichever session holds the most `part.data` bytes at
measurement time"* against whatever database `OPENCODE_DB` resolved to. That made
the workload a moving target while the G2 ceiling stayed fixed at `0.50 x` the
TypeScript median measured for **one** session, so the gate became arbitrarily
harder or easier with no change in the code. Measured on 2026-08-08: the session
the committed baseline describes had been deleted from the live database
altogether, and the then-heaviest session was 299,771,941 bytes — 2.85x larger —
against an unchanged 1,513,496 KiB ceiling.

The pin is enforced, not documented:

- The resolved database is checked against the pinned byte length and SHA-256
  **before** it is copied. The path is only where to look; a byte-identical copy
  elsewhere is accepted and a mutated database at the pinned path is not.
- The session is read **by ID**, and its message count, part count and part bytes
  must equal the pinned values.
- Any mismatch is a typed error naming what was expected, what was found, and the
  recapture procedure. The heaviest session present is reported in that message
  as information; it is never substituted for the pin.
- Every report's recorded subject — the committed baseline included — is compared
  against the pin, and a disagreement fails the gate.

**Re-pinning requires re-measuring.** The subject and the ceiling come from one
measurement, so changing `W_REAL_SUBJECT` without regenerating
`benchmarks/ts-baseline.json` reintroduces exactly the defect the pin closes. The
full procedure is in `W_REAL_RECAPTURE`, which every pin failure prints.

This is **not** a `PERF_METHODOLOGY_REVISION` bump. Pinning *which* session is
measured changes no formula, no threshold, no repetition count and no sampling
rule, so the hashed section below is byte-identical and revision 2 still describes
this measurement. A bump would instead make the revision-2 baseline unloadable and
discard the measured G1/G2 results.

## Liveness progress

Only a new message or part row, a part status transition, a tool call starting
or completing, or a token-usage update resets the G4 progress watchdog. A
heartbeat, repeated identical status, or bytes arriving on a stream that never
completes a turn do not count as progress.

## Frozen threshold formulas

The text between the markers is hashed by `zuno-testkit`. Changing it requires an
explicit `PERF_METHODOLOGY_REVISION` bump and a newly registered digest.

<!-- PERF_FORMULAS_START -->
**G1 pass** iff `median_peak(rust, W-idle) ≤ 0.50 × median_peak(ts, W-idle)`.

**G2 pass** iff `median_peak(rust, W-real) ≤ 0.50 × median_peak(ts, W-real)`.

**G3 pass** iff the Theil–Sen slope of RSS over the final 50% of samples is `≤ 1 MB / turn` **and** `peak(final 10%) ≤ 1.5 × peak(turns 40-60)`.

**G4 pass** iff no turn exceeds **120s without state progress** and no turn exceeds a **hard deadline of 1800s**.
<!-- PERF_FORMULAS_END -->
