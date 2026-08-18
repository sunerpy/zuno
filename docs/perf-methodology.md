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

## D0 data-representation baselines

D0 is a measurement, not a change: §5.2 of the perf plan forbids the D1-D4
representation work from starting before it produces numbers, and §10.1 forbids
reusing the reference implementation's figures. Nothing in this section changed a
representation. The measurements live in
`crates/zuno-testkit/tests/representation.rs` and are reproduced with:

```sh
cargo test -p zuno-testkit --test representation -- --nocapture --test-threads=1
```

The fixture half reads the same pinned W-real snapshot the memory gates use, with
`sqlite3 -readonly`, and writes nothing. The layout half needs no fixture.

### D0-a — large-payload sharing

Every string leaf in the pinned session's 3,620 `part.data` blobs, with map keys
counted separately from values. A key is one of a handful of repeating names and
so is interning rather than `CompactString` territory; counting the two together
attributed 51% of the payload to D4 when most of it was repeated key text.

| quantity | value |
| --- | --- |
| string bytes | 69,546,661 |
| value leaves | 1,064,767 leaves / 35,850,313 B (51.55%) |
| map keys | 5,752,007 leaves / 33,696,348 B (48.45%) |
| largest single value leaf | 152,309 B |

Value-leaf histogram, upper bound inclusive, as a share of value bytes:

| leaf size | leaves | bytes | share |
| --- | ---: | ---: | ---: |
| ≤24 B | 531,938 | 2,207,743 | 6.16% |
| ≤64 B | 503,634 | 24,732,337 | 68.99% |
| ≤256 B | 25,996 | 2,186,014 | 6.10% |
| ≤1,024 B | 1,584 | 866,789 | 2.42% |
| ≤16,384 B | 1,035 | 3,806,593 | 10.62% |
| ≤262,144 B | 74 | 2,050,837 | 5.72% |
| >262,144 B | 0 | 0 | 0.00% |

**D1 candidates** (value leaf ≥ 1,024 B): 1,110 leaves, 5,858,454 B — 8.42% of all
string bytes. **D4 candidates** (value leaf ≤ 24 B): 532,444 leaves, 2,207,743 B —
3.17%. Five recomputations agree byte-for-byte, so the reported spread is 1.0000x
by construction rather than by sampling.

The distribution's mass is in the 25-64 B band (68.99% of value bytes), which is
neither an `Arc<str>` nor a `CompactString` case: too large to inline at 24 bytes
and far too small for a refcount to beat a copy.

### D0-b — enum boxing

Layout is a compile-time constant, so every run is byte-identical and the spread
is 1.0000x by construction. Variant payloads are tabulated as the tuple of each
variant's field types, and the table is checked against the measured stride — a
variant missing from it fails the test rather than producing a stale projection.

| type | inline stride | largest variant | runner-up | stride floor if boxed | saving/element |
| --- | ---: | --- | --- | ---: | ---: |
| `StreamEvent` | 120 B | `GeneratedImage` = 120 B | `ProviderReasoningItem` = 96 B | 104 B | 16 B |
| `TurnEvent` | 128 B | `Provider` = 128 B | `ToolDispatchCompleted` = 128 B | 136 B | 0 B |

Hot-type footprints measured alongside them, which §3.4 records as an assertion
**both** this project and the reference implementation lacked:

| type | size | align |
| --- | ---: | ---: |
| `MessageRecord` | 96 B | 8 B |
| `PartRecord` | 120 B | 8 B |
| `MessageWithParts` | 120 B | 8 B |
| `PartKind` | 1 B | 1 B |
| `MessageRole` | 1 B | 1 B |
| `StreamEvent` | 120 B | 8 B |
| `TurnEvent` | 128 B | 8 B |
| `ProjectedMessage` | 56 B | 8 B |
| `Message` | 32 B | 8 B |
| `RequestContentBlock` | 104 B | 8 B |
| `String` | 24 B | 8 B |
| `serde_json::Value` | 32 B | 8 B |

The decisive quantity is not the stride but the **population**. Neither enum is
ever collected into a length-unbounded container: the only multi-element home
either has is the 64-slot `TURN_EVENT_CHANNEL_CAPACITY` channel, and `StreamEvent`
reaches a consumer only inside `TurnEvent::Provider`. So the whole D2 opportunity
across both is bounded by **1,024 B** — 16 B x 64 for `StreamEvent`, and 0 B for
`TurnEvent`, whose largest and runner-up variants are the same size, so boxing
would make it 8 B *larger*. A test fails if that bound ever exceeds 16 KiB, which
is the case where the bounded-population argument stops holding.

### D0-c — derived copies

Measured over five runs on the same pinned session, with both projections given
their own owned copy of the stored transcript and both timed regions ending with
that copy destroyed. The symmetry is load-bearing: without it the moving path
alone pays for freeing 70 MB of JSON, which the first attempt misread as the move
being 264x slower.

| quantity | value |
| --- | --- |
| hydrated transcript string payload | 70,482,874 B |
| request content reaching a provider | 3,693,470 B (5.24% of stored) |
| provenance ids the cloning path also carries | 47,550 B |
| peak live, `project_history` (clones) | 74,223,894 B = 1.0530x stored |
| peak live, `project_history_owned` (moves) | 70,482,874 B = 1.0000x stored |

| projection | five runs (ms) | min / median / max | max/min |
| --- | --- | --- | --- |
| clones | 925.2; 937.7; 978.0; and two more | 925.2 / 937.7 / 978.0 | 1.0571x |
| moves | 969.5; 971.2; 978.1; and two more | 969.5 / 971.2 / 978.1 | 1.0088x |
| teardown alone, paid by both | 967.9; 974.1; 976.2; and two more | 967.9 / 974.1 / 976.2 | 1.0085x |

Both wall times are dominated by the ~970 ms teardown both paths pay, so the
projections themselves are indistinguishable at this sample size — a null result
on time.

### What D0 says about D1-D4

These are findings, not approvals. Each is measured on this project, so §10.1 is
satisfied and none of them rests on a transferred number.

- **D1 (`Arc<str>` for large payloads) — not worth doing here.** The runtime
  request path already moves rather than clones, so the second resident
  transcript copy D1 targets is not present on it. The duplicate the remaining
  cloning projection creates is 3.74 MB against a 70.48 MB transcript, because
  95% of stored payload never reaches a provider request at all. A test fails if
  that fraction ever exceeds 20%.
- **D2 (boxing large variants) — not worth doing here,** bounded at 1,024 B total
  by population, and actively harmful for `TurnEvent`.
- **D3 (`SmallVec` for part lists)** was not measured: it is gated on D2, and D2
  is closed.
- **D4 (`CompactString`) — weaker than it looks.** Only 3.17% of string bytes sit
  in leaves that would inline; the payload's mass is the 25-64 B band, which
  inlines nowhere. The 5.75 M map keys are a separate and larger opportunity, but
  that is interning, not `CompactString`.

## Startup budget

G1's startup budget is enforced by `crates/zuno-cli/tests/startup.rs`, which
`make test` runs in CI's `test` job — the reference implementation has eight
startup budgets and runs none of them in CI, and §7 records that as the weakness
rather than the thing to copy. Budgets are re-measured here rather than adopted,
because this is a different binary with a different startup path.

Nine runs per invocation, first run discarded (it pays for faulting the binary's
pages in), isolated `XDG_CONFIG_HOME` and `XDG_DATA_HOME`, debug profile:

| invocation | min | median | max | max/min | budget | headroom |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| `zuno --version` | 3.648 ms | 4.144 ms | 4.563 ms | 1.2509x | 30 ms | 7.2x |
| `zuno --version --long` | 3.744 ms | 4.007 ms | 4.432 ms | 1.1839x | 30 ms | 7.5x |
| `zuno --help` | 4.115 ms | 4.153 ms | 5.012 ms | 1.2181x | 30 ms | 7.2x |
| `zuno session list` | 14.778 ms | 15.399 ms | 16.230 ms | 1.0983x | 100 ms | 6.5x |

Phase attribution comes from `ZUNO_STARTUP_PROFILE=1`, which writes one
`zuno-startup` line per process to **stderr** — never stdout, for the reason
`zuno-observability`'s crate docs give. A dispatching invocation writes two lines,
because it re-execs once to hand the command process its environment. Measured
phase split for `zuno session list`: parent `parse` 1,163 µs, `environment`
293 µs; command process `logging` 796 µs, `dispatch` 28,385 µs.

A wall-clock budget is only half the gate. The assertion that actually catches a
blocking step added to startup is structural and has no clock in it:
`zuno --version` must reach neither the `bootstrap_restart` nor the `logging`
phase, and must leave zero files in the log directory. That holds on a loaded
runner where a timing budget would have to be loosened.

## Linker

§8.2 quotes the reference implementation's mold result (2.9 s with lld, 2.0 s with
mold) and §10.1 requires re-measuring it here. Re-measured on 2026-08-18 against
this workspace's `zuno` binary (203,921,136 bytes, 84,452,158 bytes of `.text`),
five interleaved runs of `touch crates/zuno-cli/src/main.rs && cargo rustc
--offline -p zuno-cli --bin zuno`:

| linker | five runs (s) | min / median / max | max/min |
| --- | --- | --- | --- |
| toolchain default | 1.34; 1.45; 1.22; 1.19; 1.17 | 1.17 / 1.22 / 1.45 | 1.2393x |
| explicit `-fuse-ld=bfd` | 7.39; 5.99; 6.28; 6.48; 5.79 | 5.79 / 6.28 / 7.39 | 1.2764x |

**The toolchain default already is lld.** Rust 1.96 passes
`-B<sysroot>/lib/rustlib/<triple>/bin/gcc-ld -fuse-ld=lld` on
`x86_64-unknown-linux-gnu` with no configuration, which is why the default column
is 5.15x faster than bfd. A first attempt compared the default against an explicit
`-fuse-ld=lld` and measured no difference — correctly, because both were lld.

So the change §8.2 contemplates is already in effect, and **`mold` is not
installed on this host** (`ld.lld`, `lld` and `clang` are). Its headroom over lld
on this binary is therefore *unmeasured*, and §10.1 means it is not adopted on the
strength of someone else's figure. No linker entry was added: an unconditional
`-fuse-ld=mold` fails the build on every machine without mold and cargo config
cannot express "if the tool exists", while any `RUSTFLAGS`-carried linker flag
changes every crate's fingerprint. `crates/zuno-cli/tests/build_config.rs` pins
the measured fact instead, so a toolchain that stops shipping `rust-lld` or a
config edit that overrides the linker fails a build rather than silently costing
5 seconds a link.

## Liveness watchdog

`zuno_observability::watchdog` reports a stalled process from an independent OS
thread, so it still reports when the async runtime is the thing that is wedged.
Thresholds, each with what it protects against:

| threshold | value | protects against |
| --- | ---: | --- |
| `STALL_AFTER` | 90 s | a busy turn that stopped progressing being noticed only when a human complains |
| `CHECK_EVERY` | 5 s | a stall reported so late its surrounding log context has scrolled away; also bounds shutdown latency |
| `ALIVE_EVERY` | 300 s | silence being ambiguous between "nothing went wrong" and "the watchdog thread died" |
| `MAX_THREADS_DUMPED` | 48 | one stall in a many-threaded process flooding the log |
| `MAX_STALL_BACKOFF` | 600 s | a persistent stall emitting 720 identical reports at the check interval |

90 s sits deliberately **below** G4's frozen 120 s progress timeout, so a stalled
turn is described in the log before the gate that fails the build on it trips. A
test asserts that ordering.

Two properties make it safe to ship. A missing heartbeat is a stall only while a
`WorkGuard` is alive, so a CLI waiting at a prompt is not reported; and the guard
is taken only for commands whose silence really is a stall —
`DispatchArguments::silence_is_a_stall` classifies `tui` and `serve` as *not*
guarded, because holding a guard across them would report a stall every 90 s while
a user reads the screen. Every wait is bounded: the thread parks on
`Condvar::wait_timeout` for at most `CHECK_EVERY` and `shutdown` notifies before
joining, measured at well under 5 s even with an hour-long check interval
configured.

Cost, measured: `zuno session list` median 15.399 ms with the watchdog wired in
against 15.301 ms before — inside the 1.0983x five-run spread, so no measurable
cost.

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
