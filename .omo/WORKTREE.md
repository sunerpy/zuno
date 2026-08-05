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
