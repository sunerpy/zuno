# opencode-rust

A Rust port of [`opencode`](https://github.com/sst/opencode), pinned to
compatibility baseline **1.18.13**.

`opencode-rust --version` reports `1.18.13`. That is deliberate and it is not the
build's identity: npm plugins read the running version as a semver range and skip
themselves when it does not match, so the short version has to be the pinned
baseline. Ask for the real identity explicitly:

```console
$ opencode-rust --version
1.18.13
$ opencode-rust --version --long
opencode-rust 0.1.0 (Rust package 0.1.0; plugin compatibility 1.18.13)
```

Both identities appear because conflating them would either break plugin loading
or lie to an operator. It is a declared divergence
(`split-version-identity`).

## Documentation

| page | what it answers |
|---|---|
| [docs/compatibility-matrix.md](docs/compatibility-matrix.md) | every surface's state: implemented, explicit 503 gap, added, rejected, not-registered |
| [docs/divergences.md](docs/divergences.md) | the twelve deliberate differences, each with its reason |
| [docs/rejected-inputs.md](docs/rejected-inputs.md) | every deprecated config form, its replacement, and the exact error message |
| [docs/migration.md](docs/migration.md) | opening an existing database, the channel-database rule, the 38 migrations |
| [docs/session-retention.md](docs/session-retention.md) | the C8 prune operator guide — `--archive` reversible, `--delete` not |
| [docs/plugin-authoring.md](docs/plugin-authoring.md) | all three plugin tiers, with a Rust example |
| [docs/perf-methodology.md](docs/perf-methodology.md) | how the memory and liveness gates are measured |

Every table in those pages is generated from the code it describes, and
`cargo test -p oc-cli --test docs` fails when the code moves and the prose does
not. Regenerate with `OC_DOCS_REGENERATE=1 cargo test -p oc-cli --test docs`.

## Running it side by side with the TypeScript binary

Both binaries read the same data directory, so the only thing to get right is
*which database file* each one opens.

```sh
# 1. Back up first. Migration is forward-only.
cp "$XDG_DATA_HOME/opencode/opencode.db" "$XDG_DATA_HOME/opencode/opencode.db.backup"

# 2. Read-only sanity check: does this binary see your data?
opencode-rust debug paths
OPENCODE_DISABLE_CHANNEL_DB=1 opencode-rust session list

# 3. Same question, other binary, same environment.
opencode session list
```

If step 2 shows an empty list, you have hit the channel-database rule, not a
compatibility bug — see the first gap below.

### Rolling back

There is no uninstall command and no self-updater; both are deliberately
[rejected](docs/compatibility-matrix.md). Rolling back is therefore just:

```sh
# Stop using this binary. The TypeScript one is untouched.
opencode

# If you migrated a legacy database and want the exact prior bytes back:
cp "$XDG_DATA_HOME/opencode/opencode.db.backup" "$XDG_DATA_HOME/opencode/opencode.db"
```

The released binary can keep using a database this port has migrated: a
Rust-created database is opened by the real 1.18.12 binary without replaying
migrations, and the resulting schema is compared object by object against a
database that binary created itself
(`crates/oc-db/tests/schema.rs`, `crates/oc-testkit/tests/compat_suite.rs`). That
is tested at the schema and journal level only, which is why the backup in step 1
is not optional.

## Four things a side-by-side user hits first

**1. The database filename differs, and it looks like data loss.** A build from
source resolves `opencode-local.db`; an installed release resolves `opencode.db`.
Same rule in both implementations — a channel define, not a divergence — but the
symptom is an empty session list. Use `OPENCODE_DISABLE_CHANNEL_DB=1` or point
`OPENCODE_DB` at the file you mean. Details and the full precedence order:
[docs/migration.md](docs/migration.md#the-channel-database).

**2. Event subscriptions use SSE.** `/api/event` immediately emits
`server.connected` and then live events; `/api/session/{sessionID}/event` replays
durable events after `?after=<sequence>` and continues live. The older `/event`
cursor stream remains available for compatibility. Slow subscribers stay bounded
and receive an explicit lag diagnostic rather than growing memory.

The API differential invokes all 58 upstream operations against both binaries.
35 of the 58 upstream operations have local backends; the remaining 23 return an
operation-specific `503 backend_unavailable` and remain reported as compatibility
gaps. A registered `501` can never satisfy the matrix.

**3. An old install needs a migration you should know about.** A database
predating the `migration` table carries a `__drizzle_migrations` journal instead.
That case is handled — the journal is created, seeded from the names Drizzle
recorded, and the remaining migrations run — but it is a one-way upgrade of your
real data. See [docs/migration.md](docs/migration.md#opening-an-existing-database).

**4. Provider coverage is stated per wire family, not per vendor.** If your
provider id is not claimed by a family, you get an error naming it rather than a
request quietly built in the wrong shape. Declared as
`provider-coverage-by-wire-family` in [docs/divergences.md](docs/divergences.md).

## Building and testing

```sh
cargo build --release
cargo test --workspace
cargo clippy --workspace --all-targets
cargo fmt --all --check
```

`unsafe_code` is forbidden workspace-wide.

## Non-functional gates

Six gates back the port's resource claims. Every figure below was measured on
Linux; none is a projection. Two of the six are opt-in rather than part of the
ordinary suite, and one has a half that has never run here — both stated in the
caveats.

### G1 and G2 — peak resident memory

<!-- generated:BEGIN memory-gate-measurement -->
Derived from the newest committed measurement artefact,
[`.omo/evidence/task-123-opencode-rust.txt`](.omo/evidence/task-123-opencode-rust.txt).
The ceilings are not measured here:
[`benchmarks/ts-baseline.json`](benchmarks/ts-baseline.json) freezes each one
at half the TypeScript median for the same workload, and every other column
below is computed from the five per-repetition Rust peaks the artefact records.

| gate | workload | Rust median peak | frozen ceiling | margin | five-run spread | Rust / TypeScript | verdict |
|---|---|---:|---:|---:|---:|---:|---|
| G1 | `W-idle` | 20,380 KiB | 477,120 KiB | 456,740 KiB | 444 KiB | 0.0214 | PASS |
| G2 | `W-real` | 1,494,024 KiB | 1,513,496 KiB | 19,472 KiB | 17,032 KiB | 0.4936 | PASS |

G2's five `W-real` peaks were 1,493,496 · 1,493,948 · 1,494,024 · 1,510,444 ·
1,510,528 KiB. Every one of the five is under the ceiling, and the median's
19,472 KiB margin — 1.29% of the ceiling — is 2,440 KiB wider than the 17,032
KiB five-run spread. That ordering is the claim worth checking: a margin
narrower than the spread is a coin flip that landed, not a pass. The superseded
measurement in
[`.omo/evidence/task-122-opencode-rust.txt`](.omo/evidence/task-122-opencode-rust.txt)
is the shape being avoided: a 164,552 KiB spread around a median that finished
13,692 KiB over the same ceiling — FAIL.
<!-- generated:END memory-gate-measurement -->

### G3 to G6

| gate | what it bounds | measured | bound | verdict |
|---|---|---|---|---|
| G3 | memory growth per turn over a 500-turn soak | 0.0001775568 MiB/turn | 1.0 MiB/turn | PASS |
| G3 | final/middle peak ratio | 0.9938255268 | 1.5 | PASS |
| G4 | liveness during the soak | neither bound tripped | 120 s without state progress; 1800 s hard deadline per turn | PASS |
| G5 | unbounded channels on producer/consumer boundaries | 17 bounded + 2 declared exclusions, 0 undeclared | — | PASS |
| G6 | orphaned processes after the parent dies | 0 orphans on Linux, clean shutdown **and** `SIGKILL` | — | PASS on Linux; Windows half unexecuted |

### Four caveats, stated rather than buried

**The G2 ceiling does not scale with the subject.** It is a fixed number — half
one TypeScript median, measured once, on one session — so the gate can flip to
FAIL on a materially larger session with no change in this code. The margin and
the five-run spread above are what decide how much room there actually is, and
the ordering between the two matters more than either on its own.

**G6's Windows half has never been executed.** The measured result above comes
from `crates/oc-process/tests/containment.rs`, which is
`#![cfg(target_os = "linux")]`. The Windows Job-object path lives in
`crates/oc-process/tests/windows_containment.rs` behind `#![cfg(windows)]`, and
on a Linux host it is **NOT EXECUTED** — not skipped-but-fine, not inferred from
the Linux result. It needs native Windows CI or a Windows machine before G6 can
be claimed cross-platform.

**A green `cargo test --workspace` does not mean G1-G6 pass.** The expensive gates
are opt-in, and the ordinary suite skips or ignores them:

```sh
# G1 + G2. Skipped entirely unless the mode is `run`.
OC_MEMORY_GATE_MODE=run cargo test -p oc-testkit --test memory -- --nocapture --test-threads=1

# G3 + G4, real-driver soak. #[ignore]d: it occupies two real language servers,
# a 50,000-file watcher, a PTY, and two hours of wall clock.
OC_MEMORY_GATE_MODE=skip cargo test -p oc-testkit --test soak \
  g3_and_g4_real_driver_soak_stays_bounded_and_live -- \
  --ignored --exact --nocapture --test-threads=1

# G5 and G6 do run in the ordinary suite.
cargo test -p oc-testkit --test backpressure
cargo test -p oc-process --test containment
```

**G2's subject is pinned, and reproducing it elsewhere requires a recapture.**
The measured session is `ses_2bcaee257ffeFZNJrmtpi3ZglR` (931 messages, 3,620
parts, 105,118,812 part bytes) inside a 2.6 GB database snapshot identified by
sha256. `crates/oc-testkit/src/perf/subject.rs` holds the pin and prints a
four-step recapture procedure on any mismatch; step four is re-measuring the
TypeScript baseline, because the subject and the ceiling have to come from one
measurement. On a machine without that snapshot, G2 fails the pin rather than
measuring something else and calling it G2.

Method, formulas, and the frozen revision: [docs/perf-methodology.md](docs/perf-methodology.md).
