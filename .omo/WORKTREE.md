# Parallel execution via git worktrees

Measured facts (orchestrator experiment, before Wave 1 remainder):

- Each worktree has its **own git index** (`.git/worktrees/<name>/index`), so
  concurrent `git commit` from N agents does not contend on `index.lock`.
  Verified: two simultaneous commits both succeeded (`91e25e7`, `1932e7a`).
- A **shared `CARGO_TARGET_DIR` does serialize builds** — cargo prints
  `Blocking waiting for file lock on build directory`. The block is short
  (3.58s vs 3.76s wall on a 33-crate touch-rebuild) and it buys compiling the
  197-package dependency closure **once** instead of once per worktree
  (559 MB per target dir). Net: share it.
- The main worktree is unaffected by commits made in a linked worktree.

## Rules for agents working in a worktree

1. Your worktree is your own checkout on your own branch. Commit there.
2. `CARGO_TARGET_DIR` is exported to the shared target dir. Do not override it.
   If cargo says it is blocking on the build directory, wait — that is another
   agent building, not a failure.
3. **The notepad is shared and lives ONLY in the main worktree.** Append to the
   absolute path `/config/workspace/ProdDir/AI/opencode-rust/.omo/notepads/opencode-rust/*.md`
   with `>>` (O_APPEND is safe for concurrent appends). Never stage or commit a
   notepad file from a worktree — that is what makes merges conflict.
4. Same for `.omo/evidence/` — write to the main worktree's absolute path.
5. Never `git add .` / `git add -A`. Stage your own files by explicit path.

## HAZARD: shared CARGO_TARGET_DIR + removed worktree = stale `CARGO_MANIFEST_DIR`

Observed after merging Wave 3. Several crates embed their manifest path at
compile time, e.g. `oc-config/src/schema/tests.rs`:

```rust
const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures");
```

When a test binary is compiled **inside a worktree**, that path is baked in as
`/config/workspace/ProdDir/AI/oc-wt/tNN/...`. Because every worktree shares one
`CARGO_TARGET_DIR`, that artifact is then reused for runs in the main worktree.
After the worktree is removed, the path no longer exists and tests fail with
`No such file or directory` — with nothing wrong in the source.

**Symptom**: a test passes with `cargo test -p <crate>` but fails under
`cargo test --workspace`, and the panic message names a `oc-wt/tNN` path.

**Fix**: `cargo clean -p <crate>` for each affected crate after removing a
worktree, then re-run. Cheap — only that crate rebuilds.

**Prevention**: after merging a branch and removing its worktree, run
`cargo clean -p` for every crate that branch touched before trusting a green
suite. Better still, the guard tests that assert a **floor** (see below) turn
this from a silent pass into a loud failure.

### Todo 2's floor assertion earned its keep

`oc-error/tests/no_anyhow_in_libraries.rs` asserts it scanned at least 33 source
files. With the stale manifest path it scanned **zero** — and instead of passing
vacuously it failed with:

> `scanned only 0 source files under /config/workspace/ProdDir/AI/oc-wt/t17/crates;`
> `the scan is looking in the wrong place and would pass vacuously`

That is exactly the class of bug a floor assertion exists to catch. Every guard
test that walks a directory must assert a minimum count.

## COST DISCIPLINE for subagent tasks (added after Todo 93 burned ~4 hours)

Todo 93 ran a 50-minute measurement, then started a SECOND identical 50-minute
pass purely to satisfy a QA line I had written as "two independent runs agree
within 10%". The information was already in the first run's retained samples.

Rules for every future task prompt:

1. **Never ask for a repeat of an expensive run to prove stability.** Ask for the
   spread (min/median/max, max÷min) of the repetitions the run already performs.
2. **Put a wall-clock budget in the prompt** for anything that drives a real
   binary, and say what to do when it is exceeded: record a labelled `null` with
   a reason, not another attempt.
3. **Say explicitly which artifacts are already on disk** so the agent extends
   rather than regenerates. Todo 93's second dispatch succeeded in 18 minutes
   precisely because the prompt listed what existed and forbade re-measuring.
4. **Require temp cleanup in the prompt.** Use `/tmp/oc-<task>/` as a single
   prefix so `.omo/cleanup.sh` can reclaim it, and delete it on success.
5. **A long-running measurement must be resumable from its raw output.** Todo 93's
   revision-2 correction cost 18 minutes instead of another hour only because the
   harness had retained every raw sample in the JSON.

## HAZARD: concurrent tasks in one crate can be written against different versions of a shared type

Wave 5 hit this for real. Todo 28 (`event.rs`) and Todo 31 (`cache.rs`) ran
concurrently. Todo 28 became the owner of `Message` and changed its shape from
`{ role, text: String }` to `{ role, content: Vec<RequestContentBlock> }`.
`registry/provider.rs` re-exports `event::Message`, so Todo 31's import silently
retargeted after the merge.

**Why it slipped past the usual gates**: `cargo build --workspace` stayed green,
because no *source* line touched the removed field — only five `#[cfg(test)]`
assertions did. The break surfaced only at test-compile time.

Rules going forward:
1. **`cargo test --workspace` is the integration gate, not `cargo build`.** A
   green build across concurrent merges proves nothing about test compilation.
2. When a wave introduces a **shared type** other tasks consume, land and merge
   the owner first, or paste the type's exact definition into the dependents'
   prompts. Wave 5 dispatched all four together and paid one fix task for it.
3. After merging a batch, run `cargo clean -p` for every crate in the batch
   before trusting the suite (see the stale-`CARGO_MANIFEST_DIR` hazard above).

## HAZARD: mechanical union-merge is unsafe for TOML and for file-head docs

Wave 7 cost three fix commits, all from the same root cause: my union-merge script
de-duplicates **by line**, which is correct only for homogeneous single-line lists.

Two shapes where it breaks:

1. **TOML section semantics.** Three branches each added `[dev-dependencies]`
   entries; line-union produced **three `[dev-dependencies]` tables** and
   `cargo metadata` failed with "Cannot declare ('dev-dependencies',) twice".
   It also mis-filed seven runtime deps into the dev section.
2. **Rust inner doc comments.** Three branches each wrote a `//!` module header;
   line-union appended the 2nd and 3rd **after** the first section's items, giving
   `error[E0753]: expected outer doc comment`. `cargo build` caught it but only
   after the manifest was already fixed.

**Rule**: when a merge conflicts in a `Cargo.toml` or in the first ~20 lines of a
`lib.rs`, do NOT trust the script. Run `git show <branch>:<path>` for **every**
branch involved, then rewrite the file from those facts. The script is fine for
`pub mod` lines and nothing else.

**Also**: `cargo metadata` can report a stale exit code right after a manifest
write. Re-run it before concluding the manifest is still broken.

## Merging: use `.omo/premerge.sh <n> [<n>...]`

Encodes the three wave-7 breakage classes as hard stops rather than silent damage:

- **`Cargo.toml` conflict** → refuses to proceed. Section semantics; a line-union
  produced three `[dev-dependencies]` tables and mis-filed 7 runtime deps.
  Resolve by `git show <branch>:<path>` for every side, then rewrite by fact.
- **`Cargo.lock` conflict** → refuses. Not unionable. Take one side, re-resolve,
  then confirm nothing was dropped.
- **`lib.rs` / `mod.rs` conflict** → warns that the leading `//!` block must be
  merged as *prose into the single existing header*; a `//!` run below any item
  is `error[E0753]`.

Then it gates, in this order (order matters):

1. `cargo metadata --locked --offline` (with one retry for the stale-exit-code
   quirk) — **first**, because a broken lock makes every later failure look like
   a code problem, and `cargo build` silently repairs it.
2. Manifest table uniqueness — catches the duplicate-table bug `cargo metadata`
   *accepts*.
3. Stranded `//!` scan — reports `file:line` before the compiler does.
4. `cargo build --workspace`.
5. `cargo test --workspace` — **the real integration gate**; build can be green
   while tests fail to compile.
6. `cargo clippy --workspace --all-targets` = 0, `cargo fmt --all --check`.

Both static checks were self-tested against known-good `main` and report clean,
so a hit is a real regression, not a false positive.

## Disk budget, measured 2026-08-06

`/config` sits at 95% (40G free) with six live worktrees. Per-worktree cost is
**~2.4-3.4G of `target/`** (each worktree gets its own; `CARGO_TARGET_DIR` is NOT
exported, so there is no cross-worktree sharing and no cross-worktree cache reuse).
`opencode-rust/target` is another 5.9G.

Budget rule: **~3G per concurrent worktree, plus ~1.5G growth as it adds deps.**
Six concurrent tasks is roughly the ceiling at current free space. Prefer merging
and removing a worktree before adding a seventh.

The large `/tmp` consumers are NOT this project's and must not be reclaimed here:
`gs-pg-atlas2` (28G, live postgres socket), `atlas-t` (15G, another project's cargo
target), `r12-b4-momus.*` (2.6G, `godot-mcp-*` crates), `oc15`, `tv-cache`.
Inside `/tmp/opencode` (the pre-approved scratch) `godot-mcp-f3-dry-run-*` (11G) and
`t14-target` (3.2G) belong to unrelated finished tasks — not ours to delete.

Note: `fuser -m <dir>` answers "who has this *mount* open" and on `/tmp` returns
hundreds of PIDs. It is useless for deciding whether a directory is in use.

## Merge-conflict blast radius: cap concurrent editors per crate

Three of the wave-7 breakages came from four tasks landing in `oc-tools` at once.
Rule now: **at most two in-flight tasks per crate**, and when a choice exists,
dispatch the todo whose crate has no other editor. Todos in a fresh crate
(`oc-pty`, `oc-watch`, `oc-goal`, `oc-memory`) are free of this cost entirely.

## Wave 9 dispatch record (2026-08-06)

Five agents live, chosen to respect the per-crate concurrency cap:

| todo | crate(s) | session |
|---|---|---|
| 44 | `oc-tools/registry.rs` | `ses_02ae5455affeThfGDZoq6x7RMA` |
| 72 | `oc-tools/{output_policy,timeout}.rs` + surgical `shell.rs` | `ses_02ae24ba5ffeM8Qfpiis23Wph7` |
| 49 | `oc-pty` (sole owner) | `ses_02adc3a78ffe4eFZqnnLrZyRzv` |
| 68 | `oc-goal` (sole owner) | `ses_02ad60be4ffeEHk3HsIYosMhXQ` |
| 99 | `oc-memory` (sole owner) | `ses_02ac70caaffeBXKAcwkFdeKeaC` |

Held back deliberately: **71** (`risk.rs`) and **79** (`format.rs`) — both land in
`oc-tools`, which already has two editors. Dispatch them once 44 and 72 merge.

Merge order when they return: the three sole-owner crates first (49, 68, 99 — no
`lib.rs` contention), then 44, then 72 last because it edits `shell.rs`, which is
merged mutation-tested code and the most expensive thing to reconcile.

### Probes done up front so subagents did not have to

- `portable-pty = "0.9.0"` compiles and runs here, **and builds under
  `unsafe_code = "forbid"`** — so todo 49's "no first-party unsafe" is satisfiable.
  Gotcha handed to the agent: `drop(pair.slave)` before reading or the reader never
  sees EOF.
- `rusqlite 0.40.1` bundled ships SQLite **3.53.2** with FTS5 **and**
  `tokenize='trigram'`; external-content FTS5 over a **VIEW** works. Measured the
  CJK failure mode: `unicode61` returns **0** for `MATCH '"连接失败"'`, trigram
  returns **1**. That is why todo 102's trigram table is mandatory, not a nicety.
- One FTS5 virtual table adds **5** rows to `sqlite_master`'s table inventory
  (`x`, `x_config`, `x_data`, `x_docsize`, `x_idx`); two add 10 plus a view. That is
  why the FTS layer had to stay out of `migration::apply` — it would have broken
  todo 20's `user_tables().len() == 20` assertion and its byte-compat snapshot
  against the real binary.

### Wave 9 in-flight snapshot (for recovery after a context reset)

All five agents are mid-implementation with **0 commits** on their branches; progress
is visible only as untracked files in each worktree. If this session is interrupted,
recover by reading each worktree's `git status` rather than assuming nothing happened:

- `t44` — `registry.rs` + `tests/registry.rs` written, manifest and `lib.rs` edited
- `t49` — `oc-pty` sources appearing
- `t72` — `output_policy.rs` + `timeout.rs` written
- `t68`, `t99` — reading phase, nothing on disk yet

Continuation ids are in the dispatch table above. Use `task(task_id="ses_...")` to
resume a specific agent rather than starting fresh; a fresh session re-reads every
file and costs ~3-4× the tokens.

**Merge order is not negotiable**: 49, 68, 99 (sole-owner crates, no `lib.rs`
contention) → 44 → 72 last. 72 edits `shell.rs`, which is merged, mutation-tested
code; reconciling it against 44's `lib.rs` edits is the most expensive conflict on
the board, so it goes last when everything else is already proven.

Gate every merge with `.omo/premerge.sh <n>`; it stops on the three hazard classes
rather than silently unioning them.
