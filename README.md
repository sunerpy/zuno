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
| [docs/divergences.md](docs/divergences.md) | the seven deliberate differences, each with its reason |
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
Thirteen currently have local backends; the other 45 return an operation-specific
`503 backend_unavailable` and remain reported as compatibility gaps. A registered
`501` can never satisfy the matrix.

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

Six gates back the port's resource claims. All six are measured; none is a
projection.

| gate | what it bounds | measured | bound | verdict |
|---|---|---|---|---|
| G1 | peak RSS, idle workload (`W-idle`) | 19,776 KiB | 477,120 KiB | PASS, ratio 0.0207 |
| G2 | peak RSS, real-session workload (`W-real`) | 1,494,236 KiB | 1,513,496 KiB | PASS, ratio 0.4936 |
| G3 | memory growth per turn over a 500-turn soak | 0.0001775568 MiB/turn | 1.0 MiB/turn | PASS |
| G3 | final/middle peak ratio | 0.9938255268 | 1.5 | PASS |
| G4 | liveness during the soak | neither bound tripped | 120 s without state progress; 1800 s hard deadline per turn | PASS |
| G5 | unbounded channels on producer/consumer boundaries | 17 bounded + 2 declared exclusions, 0 undeclared | — | PASS |
| G6 | orphaned processes after the parent dies | 0 orphans, clean shutdown **and** `SIGKILL` | — | PASS |

### Three caveats, stated rather than buried

**G2 passes by 19,260 KiB — 1.27%.** That is the whole margin. The gate will flip
to FAIL on a materially larger session, and the ceiling does not scale with the
subject: it is a fixed number from one TypeScript measurement.

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
