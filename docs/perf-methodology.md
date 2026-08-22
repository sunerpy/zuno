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

## R2-R5 transcript render cost

R2-R5 are the plan's four render-side items: a prepared-frame cache, incremental
body reuse for an O(n²), a per-message line cache, and a large-buffer shrink.
§10.1 forbids adopting the reference implementation's numbers and §1 requires a
measured one, so the cost each item claims to fix was measured on this project
first. The measurements live in `crates/zuno-tui/tests/render_cost.rs` and are
reproduced with:

```sh
ZUNO_RENDER_COST=1 cargo test -p zuno-tui --offline --release \
  --test render_cost -- --nocapture --test-threads=1
```

Five runs per point, min / median / max and `max/min`, the shape *Allocator
comparison* and *D0* use. Release profile, because a debug frame is not the frame
a user pays for. The workload alternates user prompts with assistant replies
carrying reasoning, markdown prose with a bullet list, a fenced Rust block and a
completed tool call; 931 messages is the pinned W-real subject's message count.
The assertions that run without `ZUNO_RENDER_COST` carry no clock: they pin the
workload's shape and the properties any reuse depends on.

### The attribution, which reordered the work

Before deciding what to cache, the cost was attributed. One `markdown::render`
call, five runs:

| body | min | median | max | max/min | delta over prose |
| --- | ---: | ---: | ---: | ---: | ---: |
| prose only | 10.410 µs | 11.544 µs | 14.720 µs | 1.4140x | — |
| prose + 1 Rust fence | 17.029 ms | 17.237 ms | 17.279 ms | 1.0147x | 17.225 ms |
| prose + 2 Rust fences | 34.213 ms | 34.571 ms | 35.029 ms | 1.0239x | 34.560 ms |
| prose + 1 JSON fence | 26.040 µs | 31.780 µs | 40.802 µs | 1.5669x | 20.236 µs |

Laying rows out costs 11.544 µs. One Rust fence cost **1,493x that**, two cost
exactly twice one, and a JSON fence cost 851x less than a Rust one. The
proportionality to fence count rules out a one-off, and the gap between two
languages parsing bodies of the same size identifies the cost as
`HighlightConfiguration::new` compiling the grammar's `HIGHLIGHTS_QUERY` — Rust's
query is large and JSON's is small. It ran once per fence per frame.

At 931 messages, 465 replies × 17.225 ms is 8.01 s of the 8.269 s frame measured
below: **96.9%** of the cost of rendering a transcript was recompiling queries
that never change. §3.3's row for a syntax-highlight cache reads "未实现高亮" —
accurate when the plan was written and stale since highlighting shipped, which is
the drift §10.1 warns about. Memoising it is not one of R2-R5 and it was the
largest item in the area by two orders of magnitude.

The configuration is now built at most once per grammar
(`crates/zuno-tui/src/views/highlight.rs`). Its key is the grammar alone, and no
input can stale it: a configuration is a function of a `Language` and a
`&'static str` query, both fixed at compile time, and colour is applied
afterwards from the highlight events, so a live re-theme does not reach it. The
bound is 16 slots, one per grammar, allocated with the static — a cache keyed on a
`Copy` enum cannot grow with content, session length or uptime. A failed build is
cached rather than retried, per §6.5.

### One full frame, `TranscriptView::lines`, before and after the memo

| msgs | rows | before, min / median / max | max/min | after, min / median / max | max/min | speedup |
| ---: | ---: | --- | ---: | --- | ---: | ---: |
| 2 | 28 | 17.569 / 17.943 / 21.439 ms | 1.2203x | 156.582 / 160.613 / 176.938 µs | 1.1300x | 111.7x |
| 16 | 224 | 139.575 / 141.500 / 145.759 ms | 1.0443x | 1.157 / 1.206 / 1.340 ms | 1.1581x | 117.3x |
| 64 | 896 | 561.889 / 563.248 / 576.490 ms | 1.0260x | 4.525 / 4.591 / 5.401 ms | 1.1935x | 122.7x |
| 256 | 3,584 | 2.282 / 2.306 / 2.334 s | 1.0225x | 17.713 / 18.478 / 19.352 ms | 1.0925x | 124.8x |
| 512 | 7,168 | 4.530 / 4.564 / 4.604 s | 1.0165x | 35.106 / 35.304 / 36.238 ms | 1.0322x | 129.3x |
| 931 | 13,023 | 8.191 / 8.269 / 8.378 s | 1.0228x | 63.568 / 63.696 / 63.787 ms | 1.0034x | 129.8x |

Per-frame cost is linear in transcript size both before and after: 465.5x the
messages cost 460.86x the time before and 396.58x after, at a steady 68-80 µs per
message. So there is no O(n²) *within* one frame.

### The O(n²) is across frames, and it is real

A streaming delta changes only the tail, so the frame it forces should cost what
the tail costs. Measured with the memo in place, whole frame versus the tail alone:

| msgs | whole frame, min / median / max | max/min | tail as % of frame |
| ---: | --- | ---: | ---: |
| 2 | 292.048 / 297.505 / 323.649 µs | 1.1082x | 53.72% |
| 16 | 1.305 / 1.317 / 1.355 ms | 1.0384x | 11.71% |
| 64 | 4.652 / 4.713 / 4.977 ms | 1.0700x | 3.40% |
| 256 | 17.910 / 18.325 / 18.447 ms | 1.0300x | 0.88% |
| 512 | 35.136 / 35.532 / 35.975 ms | 1.0239x | 0.45% |
| 931 | 64.051 / 64.819 / 67.202 ms | 1.0492x | 0.24% |

At 931 messages **99.76%** of every streaming frame re-derived rows that could not
have changed. F frames each doing O(n) work for an O(1) change is the plan's
O(n²), and the share grows with the transcript, which is what makes it that shape
rather than a constant overhead. A keystroke is the same frame with 100% of it
redundant, since no message changed at all.

### What a cache hit can cost, measured before building one

`lines` returns owned rows, so any cache above it pays a clone per hit. If the
clone cost the render, the right answer was to close the items.

| msgs | rows | clone, min / median / max | max/min | clone as % of a render |
| ---: | ---: | --- | ---: | ---: |
| 2 | 28 | 4.795 / 5.645 / 6.977 µs | 1.4551x | 3.51% |
| 16 | 224 | 36.540 / 46.940 / 49.026 µs | 1.3417x | 3.89% |
| 64 | 896 | 157.012 / 169.717 / 204.648 µs | 1.3034x | 3.70% |
| 256 | 3,584 | 565.287 / 595.067 µs / 1.428 ms | 2.5270x | 3.22% |
| 512 | 7,168 | 1.117 / 1.148 / 2.410 ms | 2.1568x | 3.25% |
| 931 | 13,023 | 3.799 / 3.933 / 5.779 ms | 1.5210x | 6.17% |

A hit costs 6.17% of a miss at 931 messages, so reuse was worth building.

### R4 as built, and what it delivered

`crates/zuno-tui/src/views/message.rs` caches rows **per message**, not per
frame. Per frame would miss on every delta of a streaming turn, because the
trailer carries a spinner that advances on every folded event.

The key is the width, the reasoning affordance, the global tool-output default,
the per-call tool-affordance revision, the preceding role, a content fingerprint
of the message, and the resolved theme compared by `Arc` identity. `ViewContext`
holds `Arc<RwLock<Arc<Resolved>>>` and `set_theme` installs a new `Arc`, so a
pointer comparison is a *complete* test for a palette change — including
`thinkingOpacity`, which `Palette::entries` does not report and which a
field-by-field hash would have missed. The comparison is free of an ABA hazard
only because the entry holds the `Arc` it rendered with, so that address cannot be
reused. The fingerprint is derived from the parts rather than tracked as a
revision counter, because the fold mutates parts in place from several places and
a counter is a thing an edit can forget to bump, whose failure mode is a frame
showing content the transcript no longer holds.

The bound is **32,768 rows** across all entries, not an entry count: an expanded
`Reasoning` body is wrapped with no row cap, so one entry can be arbitrarily tall
and §6.2's 2,048-entry reference bound would have admitted an unbounded number of
bytes. At 396 measured bytes per row (5,156,568 bytes over 13,023 rows) a full
cache holds about **12.98 MB, or 1.08%** of the 1,198,872 KiB tuned-jemalloc
W-real median in *Allocator comparison*. The measured 931-message session occupies
13,023 rows, 39.7% of the budget, so an ordinary long session caches whole and
never evicts.

`Component::render` is the only caller of the cached path, so it is what a user
pays. Both cases were previously the full-frame cost in the table above:

| msgs | case | min / median / max | max/min | uncached frame | speedup |
| ---: | --- | --- | ---: | ---: | ---: |
| 2 | unchanged | 398.320 / 409.788 / 413.685 µs | 1.0386x | 160.613 µs | — |
| 2 | streaming delta | 670.411 / 731.135 / 976.780 µs | 1.4570x | 297.505 µs | — |
| 16 | unchanged | 503.670 / 505.388 / 517.883 µs | 1.0282x | 1.206 ms | 2.4x |
| 16 | streaming delta | 719.830 / 735.289 / 762.197 µs | 1.0589x | 1.317 ms | 1.8x |
| 64 | unchanged | 724.302 / 741.633 / 784.725 µs | 1.0834x | 4.591 ms | 6.2x |
| 64 | streaming delta | 1.013 / 1.033 / 1.349 ms | 1.3309x | 4.713 ms | 4.6x |
| 256 | unchanged | 2.150 / 2.203 / 2.791 ms | 1.2982x | 18.478 ms | 8.4x |
| 256 | streaming delta | 2.694 / 2.731 / 3.267 ms | 1.2127x | 18.325 ms | 6.7x |
| 512 | unchanged | 4.500 / 4.586 / 4.799 ms | 1.0665x | 35.304 ms | 7.7x |
| 512 | streaming delta | 5.322 / 5.434 / 6.002 ms | 1.1278x | 35.532 ms | 6.5x |
| 931 | unchanged | 8.462 / 9.905 / 10.853 ms | 1.2826x | 63.696 ms | 6.4x |
| 931 | streaming delta | 9.553 / 10.501 / 11.544 ms | 1.2084x | 64.819 ms | 6.2x |

The two smallest sizes are dominated by the harness: `render_offscreen` builds a
fresh 100x40 terminal per call, which the 2-message *unchanged* row prices at
about 410 µs. That floor is why those two rows show no speedup, and it is subtracted
from nothing else — it is present in every row of this table equally.

The result that matters: at 931 messages a frame went from 63.696 ms to 9.905 ms
and a streaming frame from 64.819 ms to 10.501 ms, so both now fit inside the
16.67 ms active redraw interval that `app.rs` caps streaming at. Before this
neither did, at any transcript longer than about 250 messages.

Combined with the highlight memo, one frame of the 931-message transcript went
from 8.269 s to 9.905 ms — **835x**.

### R2, R3 and R5

- **R3 (incremental body reuse for the O(n²)) — done, by R4.** The quadratic is
  confirmed above and removed: `views_transcript_cache_recalls_the_prefix_across_a_streaming_append`
  asserts that appending to the tail re-renders exactly one message and recalls
  all 40 others, so a delta's cost is proportional to the change and not to the
  transcript. Keying per message yields exact, prefix and suffix reuse at once, so
  §6.2's four-way `build_body_from_base` and its longest-common-prefix search are
  not reproduced — there is no prefix to search for when each message answers for
  itself.
- **R2 (prepared-frame cache) — closed on the numbers.** Layered on R4 it cannot
  beat the 3.933 ms clone floor measured above, so its ceiling is the 5.97 ms
  between that floor and the 9.905 ms shipping frame: a 1.6x improvement on a path
  already 6.4x faster and already inside the frame budget. Against that it would
  add a second cache with its own key, and it would miss on every streaming delta
  — the case §6.2 opened it for — because the frame's trailer holds the spinner.
  A real opportunity does remain and it is not this one: an unchanged frame
  materialises and clones all 13,023 rows to draw a 40-row viewport, so 99.7% of
  the clone is never seen. Windowing that needs the per-message row counts R4
  already holds, which is §6.2's "行映射"; it is recorded here as measured and
  deferred rather than folded into R2.
- **R5 (large-buffer shrink) — closed on the numbers, twice over.** There was no
  retained large buffer to shrink: a prepared frame is built and dropped inside
  one `render`, and at its largest it measured 5,156,568 bytes, 0.42% of the
  W-real median; the render loop's only other buffers are the two deferred-event
  queues, bounded by the terminal and engine channel capacities. R4 does introduce
  the first long-lived render buffer, and R5's concern is answered there by
  construction rather than by a shrink after the fact: the bound is stated above,
  `views_transcript_cache_stays_inside_its_row_bound` enforces it across frames,
  and `views_transcript_cache_forgets_a_message_that_no_longer_exists` prevents a
  replaced transcript from holding rows against it forever.

### Prepared frame footprint

| msgs | rows | spans | bytes | % of the W-real median |
| ---: | ---: | ---: | ---: | ---: |
| 2 | 28 | 168 | 11,088 | 0.0009% |
| 16 | 224 | 1,344 | 88,704 | 0.0072% |
| 64 | 896 | 5,376 | 354,816 | 0.0289% |
| 256 | 3,584 | 21,504 | 1,419,264 | 0.1156% |
| 512 | 7,168 | 43,008 | 2,838,528 | 0.2312% |
| 931 | 13,023 | 78,125 | 5,156,568 | 0.4200% |

Counted as each `Line`, each `Span` and the bytes of its text. Layout is a
compile-time constant and the workload is deterministic, so these are exact rather
than sampled.

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
test asserts that ordering. **G4's review tightened what it asserts** — see
*G4 review* below.

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

## G4 review

G4 was a review of the soak and liveness gates, not new machinery. Two findings.

**The ordering assertion did not assert the ordering.** It compared `STALL_AFTER`
against G4's 120 s, but a stall is *noticed* at the first check after it crosses
the threshold, so the worst case is `STALL_AFTER + CHECK_EVERY`. At the shipped
values that is 95 s, 25 s ahead of the gate, and the ordering holds. It would also
have held in the assertion while being false in fact: raising `CHECK_EVERY` to
40 s puts worst-case detection at 130 s, past the gate, and the original assertion
passed. `watchdog_defaults_are_the_frozen_constants` now asserts the sum.
Mutation: raising `CHECK_EVERY` to 40 s and updating the frozen-value pin to match
— which is what a future author would do — fails with *"a stall can go unnoticed
for 130s (90s threshold plus one 40s check), which is past G4's frozen 120s
progress timeout — the gate would fail with nothing in the log to explain it"*.

**The ordering is about the shipped binary, not about the soak run.** The G3/G4
soak's liveness watchdog is a struct local to
`crates/zuno-testkit/tests/soak.rs:156`, and `zuno-testkit` does not depend on
`zuno-observability` at all. So during a soak run no `Watchdog` thread exists and
no `WorkGuard` is ever held; the 90 s line cannot precede the 120 s failure there.
Where the ordering does hold is a `zuno run` turn in the shipped binary, which
`DispatchArguments::silence_is_a_stall` classifies as guarded. This is not a
defect — the soak harness is not the product — but the sentence above previously
invited the wrong reading, so it now names which.

Nothing else changed. The 500-turn count, the two-hour floor, G3's Theil–Sen
slope and peak-ratio predicates, G4's 120 s and 1800 s, and the `busy > 0` gate
are all as they were.

## M2 retained buffer capacity

M2 was `shrink_to_fit` plus an allocator purge. Measurement split it: the shrink
found real retained bytes, and the purge is redundant with tuning already shipped.

**What the shrink found.** Every incremental framer in the workspace appends
network chunks to one buffer and `drain`s complete frames off the front. `drain`
and `clear` keep the allocation, so the buffer's capacity becomes the high-water
mark of the largest frame the stream ever carried and stays there. Measured on
`zuno_llm::sse::SseParser` by a throwaway probe before any change:

| after | `len()` | `capacity()` |
| --- | ---: | ---: |
| one 4 MiB event | 0 | 8,388,608 |
| 1,000 further small events | 0 | 8,388,608 |
| `finish()` (drops the parser's buffer) | 0 | 0 |

8,388,608 bytes held for zero live bytes, per live stream. That is 0.68% of M1's
1,198,872 KiB tuned-jemalloc W-real median and 1.63x the largest prepared TUI
frame R5 measured at 5,156,568 bytes. A separate probe confirmed the buffer's
growth carries no overshoot to reclaim — 8,388,608 bytes of capacity for
8,388,608 live bytes, ratio 1.0000 — so the recoverable bytes are exactly the
ones stranded after a drain.

Four buffers have this shape. Each keeps a 64 KiB floor rather than shrinking to
`len`, so the steady-state path never reallocates:

| buffer | cap on one frame | what it strands without the release |
| --- | ---: | ---: |
| `zuno_llm::sse::SseParser::buffer` | 8 MiB | 8,388,608 B, measured |
| `zuno_provider_bedrock::eventstream::EventStreamDecoder::buffer` | 16 MiB | up to 16,777,216 B |
| `zuno_mcp::remote::sse::SseDecoder::bytes` | **was unbounded** | unbounded |
| `zuno_lsp::client::Framer::buffer` | 64 MiB | up to 67,108,864 B |

The MCP decoder had no per-event cap at all — the §2 defect this plan says not to
port, present in this workspace. It now takes the same 8 MiB cap and the same
`ZUNO_STREAM_MAX_EVENT_BYTES` override the provider transports use, and refuses
rather than truncates. Mutating the cap away fails with *"the decoder accepted
270336 bytes with no delimiter and no refusal, against a 65536-byte cap, so it is
unbounded"*.

The LSP framer is the largest and the worst-placed: one per language server,
alive as long as the server, so its 64 MiB is a process-lifetime cost rather than
a per-stream one. It carries its own 64 KiB constant rather than importing
`zuno_llm`'s, because `zuno-lsp` has no business depending on the model-provider
crate.

**What the 64 KiB floor costs.** 65,536 bytes per live buffer, 0.0053% of the
W-real median — against 8,388,608 bytes measured stranded without it, a 128x
reduction in what one stream can hold. The floor clears both the 8 KiB SSE decode
chunk and the LSP client's 16 KiB header cap, so an ordinary stream never reaches
a shrink. Two tests assert that directly: after one large event, 1,000 further
small events and 500 further LSP messages leave `capacity()` unchanged.

**The purge half is closed on the allocator's configuration.**
`.cargo/config.toml` gives jemalloc `dirty_decay_ms:1000,muzzy_decay_ms:1000`, so
freed pages return to the OS within about a second unasked. An explicit purge
would only advance that by less than one second, while the process-tree sampler
runs every 2 seconds — so a purge could not even be *observed* by the gates it
would be added to satisfy. Retained capacity is not free memory, which is why the
shrink is the part that mattered: it is what makes those pages free at all.

**What M2 did not find.** R5 already established there is no long-lived oversized
render buffer (largest prepared frame 5,156,568 B = 0.42% of the median), and R4's
row cache is bounded by construction at 32,768 rows. A probe of that cache
measured its retained-versus-logical bytes at ratio **1.0000** over a
931-message fixture — 707,560 bytes allocated for 707,560 logical — so there is
nothing there to shrink. Its one recoverable structure is the slot vector, 49,152
bytes at a 1,024-slot capacity, which `truncate_to` already shortens on a replaced
transcript and which is 0.004% of the median. Left alone.

## G2 slow-frame threshold

A slow frame is one pass of the TUI draw path, measured around the single call
that renders the component tree. Before this, **no frame duration was measured
anywhere**: `crates/zuno-tui/src/app.rs` used `Instant` only for redraw-cadence
timestamps, and the one `slow_frame` reference in the workspace was a test that
*injected* a 35 ms delay to check tick behaviour.

The threshold, and why 40 ms against frames that now cost 10 ms:

| anchor | value | ratio to the 40 ms threshold |
| --- | ---: | ---: |
| active redraw interval (`app.rs`) | 16.67 ms | 2.40x |
| unchanged 931-message frame (R4) | 9.905 ms | 4.04x |
| streaming frame (R4) | 10.501 ms | 3.81x |
| the same frame before R2-R5 | 8,269 ms | 0.005x |

The last row is the point. Today's frames are comfortably silent, and the reason a
threshold is worth having is that the 8.269 s frame shipped undetected until it
was measured deliberately. 2.4x the active interval means a frame must miss two
consecutive slots to trip it, so cadence jitter cannot. 40 ms is also the
reference implementation's value; that is a coincidence worth stating rather than
a derivation, since the multiples above stand on this project's own numbers.

Placement: the history lives inside `UiState`, behind the mutex that already
serialises every draw, so all four draw sites — startup, terminal input, the
scheduled frame, and the post-reclaim repaint — are measured once with no second
lock and no shared counter. `views/` is untouched. Timing uses
`std::time::Instant`, not the runtime clock, because a paused test clock would
record every draw as free.

The history is bounded at 64 records. Each `SlowFrame` is two `u64`s and a
`&'static str` — 32 bytes, asserted — so the entry count *is* the byte bound:
2,048 bytes, 0.00017% of the W-real median. That is the opposite of R4's row
cache, where an entry could be arbitrarily tall and the bound had to be expressed
in rows. 64 rather than the reference's 512 because a rendering regression
produces slow frames continuously, so the oldest 448 would be the same answer
repeated. The slow-frame *count* is kept separately from the retained records,
since a count derived from them would stop growing at the bound.

Tests drive the **shipped** 40 ms threshold rather than a lowered one: a fixture
with a 60 ms draw asserts the report exists, carries a duration at or above 60 ms,
and attributes the pre-loop frame to `startup` and the keystroke frame to
`terminal_input`. A second test asserts an ordinary offscreen frame is silent, so
the threshold is not merely reachable. Mutation: raising the constant to 400 ms
fails with *"60ms frames drawn against a 400ms threshold produced no report at
all; 3 frames were measured"*.

## G3 short-term memory ring and incident attribution

An **incident** here is a runtime alert about a live process, never a gate.
Conflating the two would either fail builds on healthy sessions or let a leaking
process run silently, so these thresholds are derived independently of G1-G4's
ceilings and are never compared against them.

**The reference implementation's thresholds could not be adopted.** It warns at
1 GiB of PSS and escalates at 2 GiB. M1 measured a *healthy* 931-message W-real
session at a 1,198,872 KiB (1.143 GiB) median with its highest of five runs at
1,549,164 KiB (1.477 GiB). A 1 GiB warning fires on every normal large session
and a 2 GiB critical fires on the ordinary run-to-run spread. A test pins that
premise, so the derivation cannot be quietly replaced by the borrowed figure.

| threshold | value | derivation from this project's measurements |
| --- | ---: | --- |
| `WARNING_RSS_KIB` | 2 GiB | 1.354x the highest healthy peak (1,549,164 KiB); 1.789x the median |
| `CRITICAL_RSS_KIB` | 4 GiB | 2x the warning; 2.71x the highest healthy peak |
| `WARNING_GROWTH_KIB` | 512 MiB | 42.7% of what one whole measured 931-message session costs |
| `CRITICAL_GROWTH_KIB` | 2 GiB | 1.71x that entire session, so no single session explains it |
| `WARNING_ACTIVE_SESSIONS` | 32 | at the measured per-session cost, already far past what the workload implies |
| `CRITICAL_ACTIVE_SESSIONS` | 128 | 4x the warning |
| `DOMINANT_SHARE` | 2/3 | see below |

Deliberately *not* derived from G2's ceiling: that ceiling constrains a five-run
median while this compares a single live sample, and M1's own run 4 (1,549,164
KiB) exceeded the ceiling while the gate passed.

**The ring, the sample interval and the growth window are one decision.** 512
samples at a 2-second interval covers 17.07 minutes, which contains the
15-minute growth window with 62 samples of margin. A window longer than the ring
would silently measure growth against whatever sample happened to survive and
under-report it, so the relationship is asserted rather than assumed. The
2-second interval is the one `crates/zuno-testkit/src/perf/workload.rs` already
uses for the gates, so a runtime trace and a gate trace describe growth at the
same resolution.

Bound: every `MemorySample` is fixed-size at 40 bytes, asserted, so 512 samples
is 20,480 bytes — 0.0017% of the W-real median, allocated once at construction.
Observation count is kept separately from the retained samples, or a week-old
process would look newly started. Mutation: removing the eviction fails with
*"the ring grew to 1536 samples against a 512-sample bound"*.

**Attribution comes from `/proc`, not from the allocator.** The reference reads
jemalloc's `retained` through `jemalloc-ctl`, which is not in this workspace's
dependency graph. Linux publishes the split this needs — `RssAnon`, `RssFile` and
`RssShmem`, which sum to `VmRSS` — so the levels are measured rather than
inferred. What is lost is separating "the allocator is holding freed pages" from
"the heap is live"; the 1-second decay makes that distinction short-lived anyway,
so one level names both and says so. A test checks the split sums to this
process's own `VmRSS` within 5%.

The four levels, in the order they are tried — each reached only when the ones
above do not explain the size, so the answer is the most specific cause that fits:

| level | condition | why it is at this position |
| --- | --- | --- |
| `SessionCount` | ≥ 32 active sessions | the one cause that is not a defect; reporting a leak here sends a reader hunting one that does not exist |
| `MappedGrowth` | file + shmem ≥ 2/3 of RSS | file-backed growth is not something a heap fix addresses |
| `AnonymousHeap` | anonymous ≥ 2/3 of RSS | heap, stacks, and pages freed but not yet returned |
| `Unattributed` | nothing dominates | "we do not know" and "it is the heap" send a reader to different places |

The 2/3 share is load-bearing and was found by testing: at a bare majority,
anonymous and mapped bytes **partition** `VmRSS`, so one always wins and
`Unattributed` is unreachable — a dead fourth level. Two thirds leaves a genuine
middle. Real profiles still land decisively: this process's own startup split is
2,320 KiB file-backed against 184 KiB anonymous, 92.6% mapped. A test walks all
four levels from concrete splits so none can become unreachable again, and the
50/50 case asserts `Unattributed` rather than a coin flip.

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
measurement time"* against whatever database `ZUNO_DB` resolved to. That made
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

## Local build and test loop

Measured on 2026-08-20 on the 32-core / 61 GB development host, `cargo 1.96.0`.
Every figure below is wall-clock over at least three runs reported as
min / median / max with the max/min ratio, on the same target directory state.

### Where the time actually goes

The starting hypothesis was link time and disk I/O: `target/debug` was 155 GB and
a single test binary was 197 MB. **That hypothesis was wrong**, and the
measurement that refuted it is the first table.

| probe | runs (s) | min / median / max | max/min |
| --- | --- | --- | --- |
| warm `cargo build --workspace` (no-op) | 0.352; 0.376; 0.410 | 0.352 / 0.376 / 0.410 | 1.1648x |
| warm `cargo test --workspace --no-run` | 0.705 | — | — |
| warm `cargo test --workspace` | 219.876 | — | — |

A warm build is a **0.7 s no-op** while the same warm `cargo test` takes
**219.9 s**. Compilation is not the cost; running the tests is. Cargo builds test
binaries in parallel and then runs the resulting suites strictly one at a time.
The 224 suites sum to 206.1 s of in-harness time — so ~94% of the wall-clock is
serialised execution, and the top 10 suites alone are 143.3 s of it (70%). The
slowest single suite, `crates/zuno-testkit/tests/representation.rs`, is 46.9 s
across 4 tests; it is the D0 measurement suite, so its cost is the measurement
itself.

Compilation was then measured on its own, from a workspace-cold state produced by
`cargo clean -p` over all 36 workspace crates (registry dependencies stay warm,
which is what a branch switch or a `touch` on a low crate actually resembles):

| probe | runs (s) | min / median / max | max/min |
| --- | --- | --- | --- |
| L1 — touch one test file, rebuild that suite | 1.143; 1.033; 1.067; 1.038; 1.045 | 1.033 / 1.045 / 1.143 | 1.1065x |
| L2 — touch `zuno-error` (30 dependents), all suites | 12.734; 13.663; 13.969 | 12.734 / 13.663 / 13.969 | 1.0970x |
| L3 — workspace-cold, 36 crates + 186 test binaries | 29.551; 33.723; 27.878 | 27.878 / 29.551 / 33.723 | 1.2097x |

A single relink is 1.0 s and a full workspace-cold rebuild of every test binary
is 29.6 s. `zuno-error` and `zuno-process` have the largest fan-out in the graph
at 30 transitive workspace dependents, so L2 is the worst realistic edit, and it
costs 13.7 s. Decomposing L3 bounds the remaining structural lever:

| stage of L3 | runs (s) | min / median / max | max/min |
| --- | --- | --- | --- |
| `cargo check --workspace --all-targets` (analysis only) | 21.999; 12.479; 13.643 | 12.479 / 13.643 / 21.999 | 1.7630x |
| `cargo build --workspace` (libs + bins, no test binaries) | 17.946; 17.953; 17.775 | 17.775 / 17.946 / 17.953 | 1.0100x |

So of a 29.6 s cold rebuild, 17.9 s is the libraries and the `zuno` binary, and
linking all **186 test binaries adds 11.6 s**. That figure is the ceiling on
consolidating the 141 integration-test files into fewer, larger binaries — see
*Rejected* below.

### Adopted: `split-debuginfo = "unpacked"` for dev and test

`line-tables-only` was already set, but `size -A` on a 196 MB test binary showed
83.0 MB still in `.debug*` sections against 49.8 MB of `.text` — 42% of every
binary is DWARF the linker must copy. `unpacked` leaves it in `.dwo` sidecars
instead.

| probe | before (s) | after (s) | change |
| --- | --- | --- | --- |
| L3 workspace-cold | 27.878 / 29.551 / 33.723 (1.2097x) | 26.661 / 26.734 / 27.007 (1.0130x) | **−9.5% median**, and the spread collapses |
| L1 single relink | 0.993 / 1.047 / 19.763 | 0.954 / 0.961 / 20.378 | **−8.2% median** |
| `target/debug/zuno` | 196.3 MB | 168.6 MB | −14.1% |
| 186 test binaries, total | 6.98 GB | 5.52 GB | −20.9% |

The L1 rows each carry one ~20 s first run: the first build after any profile
change rebuilds the workspace. Both columns share that shape, so the medians are
comparable.

The tighter L3 spread is worth as much as the median: 1.2097x → 1.0130x means the
cold rebuild became predictable, not just faster.

**The `line-tables-only` panic behaviour survives**, verified rather than
assumed. Forcing a real panic under the new profile still reports
`panicked at crates/zuno-paths/src/project.rs:410:9`, and with `RUST_BACKTRACE=full`
43 of 47 frames resolve to `file:line` including this workspace's own frames. The
cost is 79,973 `.dwo` sidecar files totalling 0.28 GB — cheap against the 1.46 GB
removed from the binaries, but it is a large file count, and `cargo clean`
removes them with everything else.

Release behaviour is untouched: the profile keys are `[profile.dev]` and
`[profile.test]` only.

### Adopted: `make test-par` — run the suites concurrently

`scripts/test-parallel.sh` builds with `--no-run`, then launches the resulting
test binaries concurrently, then runs doctests through cargo. It is an **additive
local fast path**: `make ci` still depends on `make test`, so the gate is
unchanged and this cannot make CI green by running less.

Matched pair, both measured on the final adopted profile:

| runner | runs (s) | min / median / max | max/min | result |
| --- | --- | --- | --- | --- |
| `cargo test --workspace` | 196.258; 197.209; 194.626 | 194.626 / 196.258 / 197.209 | 1.0133x | 4280 passed / 0 failed / 8 ignored |
| `make test-par` | 53.20; 53.15; 53.86 | 53.15 / 53.20 / 53.86 | 1.0134x | 4280 passed / 0 failed / 8 ignored |

**3.69x, with byte-identical test counts** — 224 harness summaries, the same 4280
passes, 0 failures and 8 ignored in every run. The parallel floor is the 46.9 s
`representation.rs` suite, which is why the scheduler is longest-first and caches
per-suite durations in `target/test-parallel-durations.json`.

Concurrency width was swept rather than guessed; all four configurations produced
4280 / 0 / 8:

| JOBS x THREADS | width | wall (s) |
| --- | ---: | ---: |
| 4 x 4 | 16 | 63.04 |
| 8 x 4 | 32 | 58.61 |
| 12 x 4 | 48 | 56.73 |
| 16 x 2 | 32 | 54.37 |

The default is `JOBS=8 THREADS=4`; both are environment overrides. Returns are
flat past width 32 because the run is floored by its longest suite.

**The script captures cargo's environment instead of assuming it**, and this is
the load-bearing detail. A first version invoked test binaries directly from a
shell and subprocess-heavy suites failed even when run alone. The cause was
`PATH`: Cargo resolves mise **installs**, while a bare shell inherits mise
**shims** first and a shim cannot be spawned directly. Cargo also exports
`LD_LIBRARY_PATH` for the aws-lc-sys, libsqlite3-sys and jemalloc build-script
outputs, plus `SSL_CERT_FILE` and `SSL_CERT_DIR`. The script
therefore captures the real environment from a real cargo run through
`CARGO_TARGET_<TRIPLE>_RUNNER` on every invocation, because `LD_LIBRARY_PATH`
embeds build-script output hashes that move when a build script reruns.

It also refuses to look successful without evidence: it fails if the number of
suites that ran differs from the number built, if any suite produced no harness
summary, or if zero tests passed. Doctests are a separate step for the same
reason — `--no-run` does not build them and no test binary contains them, so
omitting them would silently drop 31 tests.

### Reclaimed: 105 GB of `target/debug`

`target` was 158 GB, of which `target/debug/incremental` was 80 GB and
`target/debug/deps` 73 GB. Of 1,156 executables in `deps`, **975 were stale** —
artifacts from long-superseded builds — holding 53.2 GB. Pruning brought `target`
to 53 GB and returned 77 GB of free space. Cargo never garbage-collects these, so
this grows without bound; a periodic `cargo clean` is the only control.

No build-time win is claimed for the pruning. L2 was 13.663 s median on the
155 GB target and 12.633 s on the pruned one — a 7.5% difference against a
1.0970x/1.178x spread, which is not separable from noise.

### Rejected

Each was measured, and the measurement is why it was rejected.

- **`cargo test -p <crate>` for all 36 crates in parallel.** Exactly correct
  (4280 / 0 / 8) and keeps cargo's environment for free, but **slower than
  sequential: 284.570 s against 219.876 s.** Cargo holds an exclusive lock on the
  build directory for the whole run, and all 36 invocations logged `Blocking
  waiting for file lock on build directory`. This also rejects a shared
  `CARGO_TARGET_DIR` across worktrees by the same mechanism: it would share the
  cache and then serialise every build behind one lock.
- **Stripping debuginfo to make spawns cheaper.** 43 suites spawn the 196 MB
  `zuno` binary, so its size looked like a per-spawn cost. Three interleaved
  rounds of 30 spawns each: full 196.3 MB binary 4.4 / 4.5 / 4.5 ms per spawn,
  stripped 84.3 MB binary 5.1 / 5.1 / 4.9 ms. Spawn cost is **size-independent**
  here — `mmap` does not read what is never touched.
- **`codegen-units` tuning.** Forcing `codegen-units = 1`, which serialises
  codegen completely, gave L3 of 28.547 / 29.222 / 42.114 (1.4753x) against the
  default's 29.551 median. If codegen were the bottleneck this would have been
  catastrophic; it is inside the spread. Raising it to 512 gave 40.221 and 28.288
  over two runs — too few to report, and pointless once cgu=1 costs nothing.
- **Disabling incremental.** `CARGO_INCREMENTAL=0` is genuinely faster cold —
  L3 of 23.347 / 24.638 / 27.491 against 29.551 — but slower on the edit loop it
  exists for: L1 median 1.236 s against 1.047 s with it on. The 80 GB was
  accumulated garbage, not working set, so pruning keeps the edit-loop win without
  the disk cost. Incremental stays on.
- **Consolidating the 141 integration-test files into fewer binaries.** Bounded
  above at **11.6 s** — the difference between a 29.6 s workspace-cold rebuild of
  everything and a 17.9 s rebuild with no test binaries at all. Since the whole
  cold rebuild is 29.6 s and the test *run* is 196 s, this is the wrong axis, and
  it would mean restructuring 141 files. Not pursued.
- **`mold`** and **`cargo-nextest`** are both absent from this host. `nextest` is
  the productised form of `test-parallel.sh` and would be the better answer;
  installing it was out of scope. `mold`'s status is unchanged from *Linker*
  above: the toolchain default is already lld.

### Effect on `make ci`

| state | runs (s) | min / median / max | max/min |
| --- | --- | --- | --- |
| before | 254.167; 220.811; 199.689 | 199.689 / 220.811 / 254.167 | 1.2728x |
| after | 237.469; 212.111; 198.004 | 198.004 / 212.111 / 237.469 | 1.1993x |

**`make ci` is not measurably faster.** The medians differ by 8.700 s (3.9%),
which is well inside both spreads, so the honest reading is no measurable change.
That is expected: `make ci` is ~93% serialised test execution, and
`split-debuginfo` only touches the build. `make ci` passed on every run in both
states.

The local loop is where the win lands: `make test-par` at 53.20 s against
196.258 s for the same 4280 tests.

## Frozen threshold formulas

The text between the markers is hashed by `zuno-testkit`. Changing it requires an
explicit `PERF_METHODOLOGY_REVISION` bump and a newly registered digest.

<!-- PERF_FORMULAS_START -->
**G1 pass** iff `median_peak(rust, W-idle) ≤ 0.50 × median_peak(ts, W-idle)`.

**G2 pass** iff `median_peak(rust, W-real) ≤ 0.50 × median_peak(ts, W-real)`.

**G3 pass** iff the Theil–Sen slope of RSS over the final 50% of samples is `≤ 1 MB / turn` **and** `peak(final 10%) ≤ 1.5 × peak(turns 40-60)`.

**G4 pass** iff no turn exceeds **120s without state progress** and no turn exceeds a **hard deadline of 1800s**.
<!-- PERF_FORMULAS_END -->
