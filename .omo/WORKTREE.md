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

## Wave 10 dispatch record (2026-08-06)

Seven agents live. `main` = 2046 tests at dispatch.

| todo | crate(s) | session |
|---|---|---|
| 45 | `oc-mcp` (sole) | `ses_029f65abeffez1G4gpAFDjt3Wm` |
| 48 | `oc-lsp` (sole) | `ses_029f520bdffeQ6ZaDcCyKnfcFi` |
| 51 | `oc-server` (sole) | `ses_029f3d33affe3LrNk9oLFL50ey` |
| 63 | `oc-agent` (sole) | `ses_029f13b25ffeH7P1ZVnBbCrwjV` |
| 69 | `oc-goal` (sole) | `ses_029ef71a4ffeMcWvqq6AcpXjcD` |
| 71 | `oc-tools/risk.rs` | `ses_029e991b8ffexjUj6duUpHB6XG` |
| 100 | `oc-tools/memory.rs` | `ses_029e1e1c2ffefd9B0N0V0qSwag` |

Held back: **70** (`oc-tools/batch.rs`) and **79** (`oc-tools/format.rs` + `oc-agent/plan_file.rs`)
— `oc-tools` is already at its two-editor cap and `oc-agent` is taken by 63.

Merge order: the five sole-owner crates first (any order, no contention), then 71
and 100 last since both touch `oc-tools/src/lib.rs`.

### Probes done up front so subagents did not have to

- **`codegraph serve --mcp` speaks real NDJSON.** Drove it by hand with three lines
  on stdin; got `protocolVersion 2024-11-05`, `serverInfo codegraph 0.42.9`, and a
  non-empty `tools/list`. Todo 45's live-server acceptance criterion is satisfiable
  as written, and the handshake bytes are in its prompt.
- **`rust-analyzer` and `typescript-language-server` are both on PATH** via mise
  shims, so todo 48's two-live-server criterion is satisfiable. Warned it not to
  point rust-analyzer at this repo.
- Extracted the hermes memory description verbatim for todo 100 and jcode's
  `GateOutcome`/`Justification`/reflect-prompt shape for todo 71, so neither had to
  paraphrase from memory — the plan explicitly forbids paraphrasing the former.

### Standing instruction added to every prompt this wave

CodeGraph MCP has been uninitialized in **every** worktree for two waves running.
Prompts now say so up front and tell the agent to fall back to Read/Grep/Glob
immediately instead of burning turns on it. Same for `context7`, which has been
quota-exhausted for several siblings.

## Wave 11 dispatch record (2026-08-06)

Six agents live. `main` = 2141 tests at dispatch, 62/103 todos done.

| todo | crate(s) | session |
|---|---|---|
| 46 | `oc-mcp/remote.rs` (sole) | `ses_0290a7709ffeqWpqPJx8wzoCg2` |
| 52 | `oc-server/api/**` | `ses_028fb8174ffem4wEM7JpXQXNFI` |
| 53 | `oc-server/events.rs` | `ses_029090234ffeLC0beC5vQW86QW` |
| 57 | `oc-plugin` (sole, fresh) | (see task list) |
| 63 | `oc-agent` (sole, fresh) | `ses_028f7c574ffefvcs7y3mVE3htZ` |
| 70 | `oc-tools/batch.rs` (sole) | (see task list) |

52 and 53 share `oc-server` — coordinated by **omission**: 53 owns `GET /api/event` and
`GET /api/session/{sessionID}/event`; 52 was told not to implement those two and not to
touch `auth.rs`/`directory.rs`/`event.rs`/`server.rs`/`main.rs`.

Todo 63 is a **retry**: its first attempt was cancelled by a stale-activity timeout at
90 minutes having produced only a `Cargo.toml` edit. The retry prompt tells it to write
files early rather than read for an hour.

### Probes done up front

- **Remote MCP live targets exist and need no auth.** POSTed a real `initialize` to
  three of the user's four configured remote servers. All 200. `learn.microsoft.com`
  and `developers.openai.com` answer with **SSE** framing; `knowledge-mcp.global.api.aws`
  answers with **plain JSON** and negotiates **`2025-03-26`**, not `2024-11-05`. So a
  Streamable-HTTP POST can be answered either way on the *same* transport — that is not
  the same thing as falling back to the SSE transport, and conflating them is the
  obvious way to get this wrong.
- **Captured the real binary's OpenAPI document** to `.omo/fixtures/oracle-openapi-1.18.12.json`
  (479 KB, committed — note `.omo/refs/` is gitignored, `.omo/fixtures/` is not). Measured:
  **162 paths / 188 operations, 58 under `/api/`**. The protocol groups declare 56
  `HttpApiEndpoint` calls. **The plan says 61.** Three numbers, none matching; todo 52 was
  given the fixture and told to report the real count. `/doc`, `/openapi.json` and
  `/api/doc` all return 200.
- **Counted the plugin hook set**: the `Hooks` interface has **24** optional keys; the
  plan's prose lists 21 and its acceptance criterion says 20. Told todo 57 the interface
  is the contract and to reconcile all three. Also flagged that `tool` is a **map**, not
  a callback, and that `auth`/`provider` are their own interfaces.
- The plan's todo 57 line points its evidence at `task-51-opencode-rust.txt` — a typo;
  told it to write `task-57`.

### Standing instructions now in every prompt

CodeGraph MCP has been uninitialized in every worktree for **three waves**; `context7` is
quota-exhausted. Both are stated up front with "fall back immediately" so agents stop
burning turns on them.

## Wave 12 dispatch record (2026-08-06)

Seven agents live. `main` = 2202 tests, 68/103 todos done.

| todo | crate(s) | blocks |
|---|---|---|
| 47 | `oc-mcp/catalog.rs` (sole) | 64 |
| 54 | `oc-server/compat_v1.rs` (sole) | 57-62 |
| 55 | `oc-cli` (sole, fresh) | 56, 80-85 |
| 58 | `oc-plugin/jsonrpc.rs` + new `oc-plugin-sdk` | 59-62 |
| 78 | `oc-acp` (sole, fresh) | 86 |
| 79 | `oc-tools/format.rs` + `oc-agent/plan_file.rs` | 86 |
| 97 | trait in `oc-engine`, fake in `oc-testkit`, test in `oc-plugin` | 60, 73 |

58 and 97 share `oc-plugin` — at the two-editor cap. 97 was told to add only a test
file and to stay out of `src/{jsonrpc,hooks,manifest,discovery,payload,auth,provider}.rs`.

Held back: **64** and **101** (both `oc-agent`, taken by 79), **73** (`oc-tui`, wants
97's lease interface first).

### Probes done up front

- **`@agentclientprotocol/sdk` v0.21.0 is on disk** at
  `opencode/node_modules/@agentclientprotocol/sdk`, so todo 78's live-SDK criterion is
  satisfiable with no network fetch.
- **Extracted the real CLI command list**: `index.ts:45-103` registers **23** commands.
  The plan's implement list (12) + reject list (7) = 19, so **`AcpCommand`,
  `AttachCommand`, `PluginCommand`, `TuiThreadCommand`** appear in the real
  registration and in neither plan list, plus `GenerateCommand` which the plan defers.
  That is precisely the "command vanishes without an entry" failure todo 55 exists to
  prevent, so it was given the real list.

### A plan self-contradiction found and resolved

Todo 97's **title** says `crates/oc-tui/src/terminal_lease.rs` while its **body** says
"Must NOT put the trait in `oc-tui`" and its acceptance requires
`cargo tree -p oc-plugin` to show no `oc-tui`/`ratatui`. The title is wrong; the trait
goes in `oc-engine`. Told the agent so and asked it to record the contradiction.

### Running count of plan-vs-source discrepancies

Four now, all found by measuring rather than trusting prose: 61→58 `/api` operations,
20→21 plugin hooks, 19→23 CLI commands, and 97's title-vs-body. Plus two evidence-path
typos (todo 57 → `task-51`, todo 58 → `task-52`). **The source has been right every
time.** Prompts now say so explicitly and tell agents to count for themselves.

## MY OWN ERROR (2026-08-06): dispatched todo 60 twice into one worktree

I fired `task()` for todo 60 twice in the same wave — `bg_43686d92` (category `deep`)
and `bg_db66d6b0` (category `ultrabrain`) — both pointed at `oc-wt/t60`. Two agents
writing the same worktree corrupts both.

`background_cancel(bg_43686d92)` returned "Task not found", so I could not cancel it
that way. The worktree was still clean at the time I noticed, so nothing was lost.

**Rule**: before dispatching, list the todo numbers already sent **this wave** and
check the new one against that list. One worktree, one agent — the per-crate cap is
about merge conflicts; this is worse, it is two writers on one checkout.

If it happens again and both are live: the first writer's uncommitted work is the one
at risk. Check `git status` in the worktree, and if two sets of files are appearing,
stop both and re-dispatch one.

## MY ERROR (2026-08-06): todo 60 dispatched twice into one worktree

I launched todo 60 twice — `bg_43686d92` (category `deep`, session
`ses_0277498f3ffeKAs7CvFukXrMrR`) and then `bg_db66d6b0` (category `ultrabrain`,
session `ses_027742e4effeO4MAl2Z7UQ6gfM`) — both pointed at
`/config/workspace/ProdDir/AI/oc-wt/t60`. Cancelling the first returned
"Task not found", so it had already ended or was never registered.

**Two agents in one worktree is a corruption hazard**: they write the same files
with no coordination, and the loser's edits silently vanish or interleave. At the
moment I noticed, `t60` was still clean, so nothing was lost.

**Rule**: one worktree, one agent, always. Before `task()`, confirm the worktree
has no live agent. A dispatch is not idempotent — re-reading a prompt does not
mean re-sending it.

**Watch for**: duplicated modules, two competing API shapes for the same thing, or
a commit whose diff contains both. If `t60` shows any of that, reset it and resume
exactly one session.

### It happened twice. The mechanism, and the fix.

Todo 60 and then todo 73 were each dispatched **twice into the same worktree** in one
wave. Both times the worktree was still clean when I noticed, so nothing was lost, but
that was luck, not process.

**Why it happened**: I wrote a long prompt, the turn ended, and on the next turn I did
not have a reliable list of what had already gone out — so I re-derived the dispatch
plan from the plan file, which of course still showed the todo as pending. The plan
file is not a dispatch ledger.

**The fix, now mandatory**: before every `task()`, run

```
git worktree list        # which worktrees exist
```

and check the target against the **dispatch table in this file** for the current wave.
A worktree that exists but has no table row is either finished or double-dispatched —
resolve that before sending anything.

Second, **append the row to the table at dispatch time, not at the end of the wave.**
The table is the ledger; if it is written after the fact it cannot prevent this.

Cancelling works if the task is still registered (`background_cancel` succeeded for
todo 73's duplicate, and returned "Task not found" for todo 60's, which had already
ended). Either way, check the worktree's `git status` afterwards: two writers produce
duplicated modules or two competing shapes for the same API, and that is the signature
to look for.

### Wave 13 dispatch ledger

| todo | crate | session | dispatched |
|---|---|---|---|
| 56 | `oc-cli/cmd/*` | `ses_027735b19ffewx40JYj9p3KRiH` | yes |
| 60 | `oc-plugin/js` | `ses_027742e4effeO4MAl2Z7UQ6gfM` | yes (a first, unregistered attempt ended on its own) |
| 73 | `oc-tui` | `ses_027702a2effev2qAFABiAy9C9l` | yes (duplicate `bg_bbef31e3` cancelled) |
| 64 | `oc-agent/model_policy` | `ses_0276e452cffejbP0zrOsMqDH3x` | yes |
| 59 | `oc-plugin/wasm` | — | **not yet** |
| 101 | `oc-agent/reflection` | — | **not yet** |

59 shares `oc-plugin` with 60, and 101 shares `oc-agent` with 64 — both at the
two-editor cap, so both wait for their partner to merge.

## Wave 14 dispatch ledger (2026-08-06) — written AT dispatch, per the new rule

`main` = 2387 tests, 78/103 done.

| todo | crate | session | dispatched |
|---|---|---|---|
| 101 | `oc-agent/reflection.rs` (sole) | `ses_0271b46f0ffearjGUHm2UVKZtd` | yes |
| 74 | `oc-tui/keybind.rs` + TUI config | `ses_0271a5d12ffeb64b164UINjdLV` | yes |
| 75 | `oc-tui/theme.rs` + 33 assets | `ses_027198f21ffeerJyilIlAFpvWe` | yes |
| 65 | `oc-tools/task.rs` (sole) | `ses_0271870bcffeq5YnuArg6hjPfx` | yes |
| 56 | `oc-cli/cmd/*` | `ses_027735b19ffewx40JYj9p3KRiH` | carried over from wave 13, still working |
| 59 | `oc-plugin/wasm.rs` | — | **held**: needs `wasmtime`, absent from the offline registry cache |
| 61 | `oc-plugin/config_tools.rs` | — | **held**: `oc-plugin` at the 2-editor cap once 59 goes |
| 77 | `oc-tui/attention.rs` | — | **held**: would be the 3rd editor in `oc-tui` |

74 and 75 both need the **TUI-only config surface** (`keybinds`, `leader_timeout`,
`theme`, `prompt`, `scroll_*`, `diff_style`, `mouse`, `max_*`), which upstream keeps in
`packages/tui/src/config/index.tsx` and which is **absent from the main config schema**
— so `oc-config` does not model it. Both were told to put it in its own module with
independent fields and to report which keys they defined, so the merge is a union
rather than a rewrite.

### Probes done up front

- `packages/tui/src/config/keybind.ts` is 471 lines with **184** `keybind(` calls —
  the plan's number is right. But only **164** are `^\s+<name>: keybind(...)`, so ~20
  are in another position; todo 74 was told to find out what they are rather than
  assume 184 named actions. Also `app_exit: keybind("ctrl+c,ctrl+d,<leader>q")` proves
  one action can carry **multiple** key strings and **mix** a chord with a leader
  sequence — so the table is action→[key…], not action→key.
- `packages/tui/src/theme/assets/` has exactly **33** JSON files. `insta` is already in
  the workspace table and the registry cache, so 33 snapshots are offline-buildable.
- **`wasmtime` is not in the offline registry cache** (0 hits). Todo 59 needs it and the
  workspace builds `--offline`; that has to be resolved before dispatching it.

## Wave 15 dispatch ledger (2026-08-06)

`main` = 2541 tests, 83/103 done. Six agents, one per worktree.

| todo | crate | session | dispatched |
|---|---|---|---|
| 59 | `oc-plugin/wasm.rs` (feature-gated) | `ses_026374ef2ffeVKvgfrfRZYyPia` | yes |
| 61 | `oc-plugin/config_tools.rs` | `ses_026361cccffeNQv77T18EHJl3h` | yes |
| 66 | `oc-agent/continuation.rs` | `ses_026341b32ffeK1NxLxxn9X2Cp6` | yes |
| 77 | `oc-tui/attention.rs` | `ses_02630f437ffe1coQOH2A6rMTfS` | yes |
| 80 | `oc-db/session_list.rs` + `oc-cli` | `ses_026327538ffe45xzIgZRzO17v4` | yes |
| 87 | `oc-testkit/cassettes.rs` | — | **withdrawn**: `Blocked by: 29,30,31,86` — 86 needs all of 1-85, so 87 is a wave-14 task, not now. Worktree removed. |

59 and 61 share `oc-plugin` (cap 2). 80 touches `oc-db` **and** `oc-cli`; nothing else
is in either this wave.

### `wasmtime` unblocked — measured

Todo 59 was held last wave because `wasmtime` was absent from the offline registry
cache and the workspace builds `--offline`. Resolved by fetching it once:

- `cargo add wasmtime` → **47.0.3**. The first `cargo build` timed out on a slow
  mirror; a second attempt completed in **44s** and populated the cache, after which
  `cargo build --offline` succeeds. So the fetch is a one-time cost, already paid.
- Verified the API todo 59 needs, in a throwaway crate: `Config::consume_fuel(true)`,
  `Config::epoch_interruption(true)`, `Store::set_fuel`, `Store::set_epoch_deadline`
  all present and working.
- **Verified it builds under `[lints.rust] unsafe_code = "forbid"`**, which is the
  workspace policy and todo 59's explicit "Must NOT use `unsafe`" requirement.

Note `cargo search` is unusable here — the registry is replaced by the `aliyun` mirror
and it errors with "crates-io is replaced with non-remote-registry source". Use
`cargo add --dry-run` to check availability instead.

## Wave 16 dispatch ledger (2026-08-07)

`main` = 2655 tests, 88/103 done. Three agents; only three todos are unblocked.

| todo | crate | session | dispatched |
|---|---|---|---|
| 62 | `oc-plugin/tests/integration.rs` (sole) | `ses_025aad319ffee2q49dpz0bne4E` | yes |
| 76 | `oc-tui/views/` (sole) | `ses_025a9a8c2ffeKfRl3Pm4j7qY87` | yes |
| 81 | `oc-db/retention.rs` (sole) | `ses_025a89acaffeTNxfTGYvPTsnS8` | yes |

Everything else is a **strict chain**: `81 → 82 → {83,84} → 85 → 86 → {87,88,91} → {89,92} → 90 → 103`.
So parallelism collapses from here: after this wave it is mostly one or two at a time,
and **86** (the full differential compat suite) is the choke point — it needs 62, 76,
and the entire 81-85 chain.

Remaining after this wave: 82, 83, 84, 85, 86, 87, 88, 89, 90, 91, 92, 103 = 12.

### Wave 15 result

All five merged: **59** (wasm tier, feature-gated), **61** (config-dir Zod tools),
**66** (continuation + job board), **77** (attention), **80** (global session listing).
2541 → 2655 tests.

Mutation-verified this wave, ten in total, every one caught by exactly the right test:
`enabled:false` still emitting a cue; no-degrade-to-notification-only; every lane
addressable (5 tests); dropping the `id DESC` tiebreak; `--archived` made exclusive;
`wasm = []` with an unconditional wasmtime dep (the feature-gate graph test);
`enabled` master switch ignored.

Two things worth carrying forward:

- **`wasmtime` is now in the offline cache.** A cold fetch failed once on a slow mirror
  and succeeded on retry in 44s. `cargo search` is unusable here (aliyun mirror
  replacement); use `cargo add --dry-run`.
- **Todo 61's Zod fixture symlinks the oracle tree's real `zod`** at
  `opencode/packages/opencode/node_modules/zod`, with an `OPENCODE_ZOD_FIXTURE`
  override and a printed skip when absent. That is the pattern for "test against the
  real dependency without vendoring it", now used by todos 45, 46, 60, 61 and 78.

## Wave 17 dispatch ledger (2026-08-07)

`main` = 2935 tests, 92/103 done. Two agents; the chain permits no more.

| todo | crate | session | dispatched |
|---|---|---|---|
| 83 | `oc-db/artifact_gc.rs` | `ses_02531d123ffe8Q645RCqiDEXSf` | yes |
| 84 | `oc-db/vacuum.rs` + `oc-cli/cmd/db_maint.rs` | `ses_025305e64ffefIJtATEFCKtPMS` | yes |

Both in `oc-db` — at the two-editor cap, coordinated by file ownership.

Remaining after this wave: **85, 86, 87, 88, 89, 90, 91, 92, 103** = 9, and the chain
is nearly linear from here: `{83,84} → 85 → 86 → {87,88,91} → {89,92} → 90 → 103`.
**86** is the choke point — the full differential compat suite, blocked on 62, 76 and
all of 81-85.

### Wave 16 result

**62** (three-tier plugin integration), **76** (TUI views — 253 new tests), **81**
(retention selector), **82** (transactional prune). 2673 → 2935.

Two verification notes worth carrying:

- **Todo 82 found the plan's table count wrong** and pinned the truth in a test whose
  message reads *"the plan's 12-table count is stale"* — the real related-table count
  is **10**. That is the fifth plan count I have seen contradicted by the source
  (61→58 `/api` ops, 20→21 hooks, 19→23 CLI commands, 184 keybind calls vs 164 named,
  12→10 tables). Prompts now tell agents to verify every inherited number.
- **Todo 62's integration suite is `#[cfg(all(feature = "wasm", unix))]`** with a
  `wasm_integration_skip!` macro that emits a printed skip for each of the six tests
  when the feature is off. I confirmed the skip message is actually printed (6
  occurrences under `--nocapture`) — so the "no silent skip" rule is honoured, and the
  6/6 pass I saw without the feature was the skip stubs, not the real tests. With
  `--features wasm` the real six run and pass, and reversing the hook bus fails four
  of them.

## Wave 18: the choke point (2026-08-07)

`main` = 2987 tests, 95/103 done. **One agent** — todo 86 gates everything left.

| todo | crate | session |
|---|---|---|
| 86 | `oc-testkit/tests/compat_suite.rs` + `docs/divergences.toml` | `ses_02453e2d1ffe1ZQY034DBSZaV6` |

After 86: `{87, 88, 91} → {89, 92} → 90 → 103`, then F1-F4.

### Wave 17 result: 83, 84, 85 merged. 2943 → 2987.

### A data-destroying defect I found by running the binary, not by reading tests

Todo 85's tests were all green and its implementation of the specified behaviour was
correct. I ran the command against the real environment anyway:

```
$ opencode-rust session prune --older-than 90 --all-projects --format json
selected sessions : 0
db rows to delete : 0
artifact items    : 106
artifact bytes    : 4.19 GB      <- including one 2.9 GB snapshot store
```

**Zero sessions selected, zero rows, and 4.19 GB of the user's snapshot history
proposed for deletion.** With `--delete --yes` it would be gone.

Mechanism, traced: upstream's DB path is **channel-dependent**
(`packages/core/src/database/database.ts:45-55`). The user's release install writes
`opencode.db` (**5,656 sessions**); our from-source build resolves channel `local` and
opens `opencode-local.db` (**0 sessions**). But snapshot stores live in
`data/snapshot/`, **shared across every channel** — 85 of them on disk. Todo 83's
reference count asks "which sessions in the *currently open* DB reference this store",
and with the wrong-channel DB the answer is "none", so every store looked
unreferenced. The preview looked entirely legitimate.

Two rounds of fixes, both verified by me:
1. `ensure_visible_session_owners` refuses artifact GC when the open DB has **zero
   total** sessions (not zero *selected*), naming the path and the count. Re-run:
   **4.19 GB → 0**.
2. The refusal was then **silent** — the table read `Artifact bytes 0` and
   `warnings: []`, i.e. "nothing to prune" rendered identically to "I cannot see your
   data". Now it emits a warning naming the database, on both surfaces, for preview and
   delete alike. Mutation-verified: suppressing the push fails
   `session_prune_empty_database_warns_for_preview_and_delete`.

**Three rules earned here:**
- *A reference count over a data set you might not be able to see must fail closed.*
- *"No results" and "cannot see the data" must never render identically.*
- **Tests cannot find this class of bug.** It needed running the real binary against
  the real environment. Todo 83's own module docs already stated the right principle —
  retain when ambiguous — and the code resolved it the unsafe way anyway. Hands-on QA
  is not a formality.

Whether the channel-DB path is itself an eighth declared divergence is now todo 86's
call; it was told to report the contradiction rather than quietly make the count eight.

### A gate bug in my own tooling, fixed

`.omo/premerge.sh` decided pass/fail solely from `^test result: FAILED`. Merging
task-84 it printed `ok  2936 tests pass, 0 failing targets` while the same output
carried `error[E0463]` and `error: doctest failed` — **a test target that fails to
compile emits no `test result:` line at all.** It was transient (three clean re-runs
after), but the gate was blind by construction. It now also fails on
`^error: doctest failed`, `^error: could not compile`, `^error: test failed`, and
`^error\[E[0-9]+\]`, self-tested against the known-good tree for false positives.

Same shape as two findings already in `issues.md` — a test that can only fail one way,
and five overlapping safety fixtures proving a disjunction rather than its terms.
**A check that can only detect one shape of failure is not a check.**

## Wave 19 dispatch ledger (2026-08-07)

`main` = 3003 tests, 96/103 done. **86 is merged — the choke point is cleared.**

| todo | crate | session |
|---|---|---|
| 87 | `oc-testkit/src/cassettes.rs` | `ses_02424fa4affeR3PmOdz5MKvRG3` |
| 88 | `oc-testkit/tests/memory.rs` (G1/G2) | `ses_02425ed86ffeIuf2WRyJLNcXZ5` |

**91 withdrawn from this wave** — `Blocked by: 86,87`, and 87 is still in flight. Its
worktree was removed. The proven pipeline it must copy is confirmed present at
`/config/workspace/ProdDir/AI/codegraph-rust/.github/workflows/release-please.yml:178+`
(six-target matrix, `use_zigbuild: true` on both musl legs). This repo has no
`.github/` yet.

Remaining: **87, 88, 89, 90, 91, 92, 103** = 7, then F1-F4.

### Todo 86's verdict, which is the important artifact of wave 18

The suite is 8 tests and emits `target/compat/compat-report.json`. I read the report
rather than the summary. **22 surfaces: 15 compared, 4 partially, 3 not compared** —
and every non-`compared` verdict carries a reason, which is exactly what was asked for:

- `provider-wire-protocol` **not compared** — no HTTP client in the harness by
  construction; explicitly deferred to todo 87.
- `tui-rendering` **not compared and never will be** — Q1's answer was an equivalent
  ratatui interface, not a pixel reproduction.
- `acp-transport` **not compared against the real binary** — todo 78 validates against
  the real `@agentclientprotocol/sdk`, which is a live-counterpart check instead.
- `api-operations` **partial**: 56 of 58 upstream operations served. The two missing are
  `GET /api/event` and `GET /api/session/{sessionID}/event` — the gap I found by driving
  the merged binary in wave 11. It is now recorded as a **known gap, not a divergence**,
  with the correct reasoning: success criterion 4 requires upstream's operation set to be
  a subset of ours, and today it is not.

**Both mutations I ran bit correctly.** Renaming `todo_session_idx` failed
`db_schema_matches_a_database_the_real_binary_created` naming the index in both
directions. Adding an eighth divergence entry failed the count assertion with a message
demanding a `DECLARED_COUNT` bump in the same commit.

### The finding worth escalating: "seven" was never the complete set

`docs/divergences.toml` has exactly the seven the plan enumerates, and the suite asserts
7. But todo 86 found **at least six more deliberate differences already declared in
code**, two of which were explicitly nominated for this allow-list by the task that
created them (`subpath-is-implemented` and `subpath-matches-literally`, both marked
"DIVERGENCE CANDIDATE … for Todo 86's allow-list" in `decisions.md`). Plus
`context-md-excluded`, `malformed-auth-json-is-an-error`,
`failed-format-restores-pre-format-bytes`, and `memory-subsystem`.

It correctly did **not** add them — that would have broken the count assertion, which
exists to force this conversation. They are emitted in the report's
`nominated_divergences` array with citations. **Todo 103 already requires an eighth entry
(memory) and a count bump**, so the number moves regardless; whoever revises it must bump
`oc_testkit::divergence::DECLARED_COUNT` in the same commit or the suite refuses.

It also ruled the channel-DB filename **faithful behaviour, not a divergence** — our
`opencode-local.db` versus an installed release's `opencode.db` mirrors
`database.ts:45-55` exactly. Recorded as a known gap because it presents as a parity bug
the first time anyone tries it; todo 92 owns documenting it.

## Wave 20 (2026-08-07): the integration gap, and todo 104

`main` = 3024 tests, 98/103 done (104 added mid-flight).

### Todo 88 refused to run, and it was right

The memory-gate task investigated, found it could not measure anything honestly, and
**stopped without producing a number**. I verified all five claims:

| claim | verified at |
|---|---|
| no `tui` command registered | `oc-cli/src/command.rs` — grep returns nothing |
| `run --auto` refused | `oc-cli/src/cmd/run.rs:186` |
| headless `run` had **no tools** | `oc-cli/src/cmd/run.rs:126` — `ToolRegistryDispatcher::new(Vec::new(), Vec::new(), AllowAll, …)` |
| server could not prompt | `oc-server/src/api/mod.rs:147` — `post(unsupported)` |
| `App::run()` never called | `oc-tui/src/app.rs:574` |

**The binary could not execute a turn in which a model calls a tool** — invisible to
3,009 passing tests, because every todo tested its own piece and none owned the seam.

Todo 104 was created and closed it. It also found a **sixth** gap nobody had spotted:
`CompletionRequest` had **no `tools` field at all**, so no provider could have been
offered a tool regardless of the dispatcher. Its doc comment for the new field is the
right reasoning: *"a provider that held its own tool list could answer with a call the
loop would then refuse."*

Both of my mutations bit: reverting the dispatcher to `Vec::new()` failed 2 of 3
tool-turn tests; blanking `tools:` in `completion_request` failed the offer test
specifically. And `crates/oc-cli/tests/tool_turn.rs` now drives the **real binary**
against a loopback `MockProvider`, asserting the written file's contents, that the
provider was called twice, and that the tool result went back in the second request.

**This is the third instance of one structural failure** — wave 11's `/api/event` gap,
wave 17's 4.19 GB prune, and now tool execution. Rule, recorded in `issues.md`:
*a plan decomposed into per-file todos produces per-file correctness and says nothing
about the seams. Every seam needs an owner.* Todo 62 is the only seam in this plan that
got a dedicated todo, and it is the only one that was right first time.

### In flight

| todo | crate | session |
|---|---|---|
| 88 | `oc-testkit/tests/memory.rs` (G1/G2), resumed post-104 | `ses_023acd04cffeoOaA4rLTutn9hG` |
| 91 | `.github/` + `Makefile` + packaging | (dispatching) |

Remaining after: **89, 90, 92, 103** = 4, then F1-F4.

## Wave 21 (2026-08-07): the seam, second half

`main` = 3045 tests, 99/103 done (104 and 105 added mid-flight; the plan is now 105 items).

### Todo 88 was blocked twice, and refused twice. Both refusals were correct.

Round one found five gaps; todo 104 closed them and also found a sixth nobody had
spotted (`CompletionRequest` had no `tools` field at all). Round two found the
**remaining half**, and I verified it in the code:

`crates/oc-cli/src/cmd/tui.rs:19-25` says so itself —
> *"Submitting a prompt does not start a turn. The turn driver needs a session, a
> provider registry and a database resolved on the TUI's own thread … the engine
> channel exists but nothing sends on it. `run` is the surface that executes a turn
> today."*

Why that blocks the founding claim: the frozen harness measures the **TUI**.
`perf/workload.rs:114-123` launches the subject under `script -qefc` in a real PTY, and
`oracle_command` at `:272` builds `<program> --pure --prompt '…' --model test/test-model
--auto`. So our `tui` renders but cannot execute; our `run` executes but is headless.
**Measuring `run` against a TUI baseline would be a massaged pass** — which is exactly
why two agents declined, and they were right both times.

Todo 105 dispatched to close it: extract `run`'s composition root, drive it from the
TUI's prompt submission, wire the real permission prompt instead of `HeadlessApproval`,
stop the status strip lying, and prove it with a PTY test issuing the frozen
`oracle_command` shape against our own binary.

**Session**: `ses_02366f3b7ffexced6AiZghCo5w`.

### Todo 91 merged, and its OpenSSL assertion is honestly designed

Six-target matrix (`x86_64`/`aarch64` musl via `cargo zigbuild`; four native
macOS/Windows legs), `Makefile` with `ci: metadata fmt-check lint test deny`,
`deny.toml`, and **21 tests** in `crates/oc-cli/tests/release_surface.rs`.

`make ci` passes locally: *"advisories ok, bans ok, licenses ok, sources ok"* then
*"OK metadata + fmt + clippy + tests + cargo-deny"*.

Worth recording: I tried to mutation-test the no-OpenSSL assertion by adding a real
`openssl` dependency, and **it cannot build in this environment at all** —
`openssl-sys` fails with *"Could not find directory of OpenSSL installation"*. The
agent had anticipated exactly this and self-tests the matcher against **synthetic
graph lines** (`the_package_matcher_catches_a_real_openssl_entry` feeds
`openssl v0.10.68`, `openssl-sys`, `native-tls`, `openssl-src` and asserts one hit
each, plus a prefix-confusion negative). That is the right design: the assertion
mechanism is proven without needing a dependency the host cannot compile.

### Running tally of plan counts contradicted by the source: six

61→58 `/api` ops · 20→21 plugin hooks · 19→23 CLI commands · 184 keybind calls vs 164
named · 12→10 prune tables · "seven divergences" is seven declared but at least
thirteen real. Every prompt now tells the agent to verify inherited numbers.

## A regression my own verification caught (2026-08-07)

Todo 105's first attempt was interrupted with the work uncommitted. Reviewing it I found
it **regressed todo 104's tool-turn tests**:

```
append-only cache violation on turn 2: stable history message 1 changed
```

from `crates/oc-llm/src/cache.rs:153` — todo 31's prompt-cache stability check, firing
because a persisted message mutated in place between turn 1 and turn 2 of the tool loop.

**The same two tests pass on `main`** (`test result: ok. 3 passed`), so it was the
refactor, not a flake. Localised before delegating the fix: the failure is on **turn 2**
(the continuation after the tool result), `TurnHost::drive` persists-then-runs in the
same order as `main`, and the most likely culprit is `resolve_session`'s changed
signature (`&RunArgs` → `&TurnPlan`).

**Why this is worth recording**: had the interruption not happened, the agent might have
committed and the merge gate *would* have caught it — but only as a red workspace run
with no diagnosis. Reviewing the uncommitted tree first produced the localisation for
free. The lesson is the one already in `issues.md` in another form: *a green subagent
report is not evidence; the diff and the failing output are.* Here there was no report at
all and the diff still gave up the answer.

Also note what the check bought: without todo 31's four cache-stability mechanisms this
refactor would have silently cost a prompt-cache hit on every turn of every session, and
nothing would have failed. The assertion that fired is the reason the regression is
visible at all.

## Wave 22 (2026-08-07): the seam is closed, 88 dispatched for the third time

`main` = 3057 tests, **100/105 done**. Todo 105 merged.

### The full arc of one integration gap

Todo 88 refused to run **twice**, and both refusals were correct. It took two new todos
to make the founding claim measurable at all:

| round | what was missing | closed by |
|---|---|---|
| 1 | no `tui` command; `run --auto` refused; `run` dispatched `Vec::new(), Vec::new(), AllowAll`; prompt route `unsupported`; `App::run()` never called; **and `CompletionRequest` had no `tools` field at all** | todo 104 (`e61e01c`) |
| 2 | the TUI rendered but its prompt submission never started a turn — *"the engine channel exists but nothing sends on it"* | todo 105 (`d0d4c27`) |

Todo 105's result, verified by me: `cmd/turn.rs` is the shared composition root and both
`run.rs` and `tui.rs` call it; `tests/tui_turn.rs` drives the **real binary** under a real
PTY and **accepts the frozen `oracle_command` shape**
(`<program> --pure --prompt <text> --model <id> --auto`), which is precisely what
`perf/workload.rs:272` issues. So TUI-vs-TUI is now an honest comparison. Disabling
`host.drive(...)` fails both PTY tests.

### A regression my review caught before it merged

Todo 105's first attempt was interrupted uncommitted, and reviewing the tree I found it
broke todo 104's tool-turn tests:

```
append-only cache violation on turn 2: stable history message 1 changed
```

`oc-llm/src/cache.rs:153` — todo 31's prompt-cache stability check, firing because a
persisted message mutated in place between turns. The same tests passed on `main`
(`ok. 3 passed`), so it was the refactor. Localised to turn 2 (the post-tool-result
continuation) and handed over with the diagnosis; fixed before merge.

**Two things worth keeping from that**: without todo 31's four cache-stability mechanisms
this refactor would have silently cost a prompt-cache hit on every turn of every session
and *nothing would have failed* — the assertion that fired is the only reason it was
visible. And reviewing an uncommitted tree produced the localisation for free, which is
the same lesson as *a green subagent report is not evidence; the diff and the failing
output are.*

### The tail is strictly linear from here

`88 → 89 → 90 → 92 → 103 → F1-F4`. I created worktrees for 89/90/92/103, confirmed each
is blocked by its predecessor, and removed them again — only **88** is dispatchable.
Session `ses_022ef1a30fferqml6j8kd22kWO`.

## Wave 23 (2026-08-07): the fourth seam, and 88 dispatched for the fourth time

`main` = 3077 tests, **101/106 done**. Todo 106 merged.

### Todo 88 has now been blocked three times, and every refusal built a real feature

| round | what was actually missing | closed by |
|---|---|---|
| 1 | tool execution end to end — no `tui` command, `run` dispatching `AllowAll` with two empty vectors, `unsupported` prompt route, and **`CompletionRequest` with no `tools` field at all** | 104 |
| 2 | the TUI's prompt submission never started a turn | 105 |
| 3 | **the three internal agents were never invoked** | 106 |

Round 3 is worth stating precisely, because the easy fix was the wrong one. The frozen
harness counts `completed_tool_turns(captured) = (captured - 1) / 2`, so a 2-request turn
scores **0**. Our port sent 2. The tempting move is to call the harness TS-specific and
edit `PRELUDE_REQUESTS`. But the harness's own doc comment records what it measured from
live 1.18.12 traffic: *"A new session's prelude generates the session title … A restored
session's prelude is a compaction summary."*

I checked, and the harness was right:

- no title-generating model request existed anywhere in `oc-engine` or `oc-cli` — every
  `title` hit was a *tool output* title or a passed-in *option*
- `grep -rn "compaction::|select_boundary|should_compact" crates/oc-engine/src/loop.rs`
  returned **nothing**; `oc-engine::compaction` was referenced only by `oc-agent`'s roster
  metadata and its own module
- `INTERNAL_NAMES` was referenced only by its own tests

And todo 63 had predicted the consequence in a doc comment at `builtin.rs:858-860`:
dropping any of the three *"silently removes auto-compaction, session titles"*. They were
declared, tested as data, and never called.

Todo 106 fixed it **in the product**: `oc-engine/src/prelude.rs::generate_title`, wired on
`TurnHost` so `run` and `tui` both get it. `tool_turn.rs` now asserts
`captured.len() == FROZEN_PRELUDE_REQUESTS + FROZEN_RESPONSES_PER_TURN` **and**
`completed_tool_turns(captured.len()) == 1`, plus that the prelude advertises no tools.
My mutation — `generate_title` returning `Ok(None)` — drops the capture to 1 and fails with
*"the frozen gate scores 0 completed turn(s)"*.

### Four seams, four identical failures

1. Wave 11 — `/event` served, `/api/event` 404.
2. Wave 17 — prune proposed deleting 4.19 GB it could not attribute.
3. Wave 20/21 — the agent could not use a tool.
4. Wave 23 — titles, auto-compaction and summaries silently absent.

Every one invisible to a green suite, because **per-file todos produce per-file
correctness and say nothing about the seams**. Todo 62 is still the only seam in this plan
that had a dedicated owner, and still the only one right first time. If this plan were
rewritten, the lesson is one line: *give every seam a todo.*

### One open question handed to 88

Round 3 also reported that only `measure_typescript_baseline` is public while the
single-workload runner and samplers are `pub(crate)`, and rejected `#[path]` importing as
manufacturing an unfrozen methodology. That was the right instinct. 88 was told to read
that function's signature first — if it takes a program path it may already be
subject-agnostic despite the name — and, if a seam really is missing from a crate it may
not edit, to report it precisely rather than work around it.

Session `ses_022a074fcffeO59D11sF1wVbUt`.

## Wave 24 (2026-08-07): the fifth seam — the binary cannot open an existing database

`main` = 3077 tests, **101/107 done**. Todo 88 has now refused **four** times; all four
refusals correct, and each produced a real missing feature (104, 105, 106, now 107).

### What round 4 found, verified by me

The frozen `W-real` source is the user's real 2.6 GB backup
`/config/.local/share/opencode/opencode.db.bak.20260408`. Measured:

```
SELECT count(*) FROM migration            -> no such table: migration
SELECT count(*) FROM __drizzle_migrations -> 10
14 tables, including session
```
The **live** DB has all 38 rows in `migration`. So the backup is a genuine legacy install.

Our `apply()` (`crates/oc-db/src/migration/mod.rs:71-78`) has exactly two paths: empty →
`create_current`, has `session` → `verify_journal`. And `verify_journal` runs
`SELECT id FROM migration` on a table that is not there, returning
`DbError::Migration { version: 38 }`. **We implement greenfield creation and journal
verification, and no migration path at all.**

Upstream's `applyOnly` (`migration.ts:43-79`) does three things we do not: creates the
journal `IF NOT EXISTS`; seeds it from `__drizzle_migrations` when empty — with the comment
*"Existing installs used Drizzle's migration journal. Seed the new journal once so
TypeScript migrations don't replay old SQL"*; then runs only the unrecorded migrations.
That is exactly this disk, and why the released TS binary opened the backup while our
release TUI exited before sampling.

**This is a first-launch defect, not a perf-gate detail**: any user whose install predates
the `migration` table cannot run this binary at all.

### Why todo 20's twenty tests could not catch it

Todo 20 diffs our schema byte-for-byte against a database **the real binary created**, and
round-trips a Rust-created DB back through the real binary. Both are *greenfield*. Neither
ever opens a database the real binary had **already been using**.

**A test that only exercises the greenfield path says nothing about the upgrade path.**
That is the new rule, and it generalises the four earlier seams: each was a transition
nobody owned — between two routers, between a selector and its data, between a registry
and a runner, between declared agents and the loop.

### One question closed

Round 3 had worried that only `measure_typescript_baseline` is public while the
single-workload runner and samplers are `pub(crate)`. Round 4 settled it: that function
**is** subject-agnostic at the process boundary — it resolves `OC_TESTKIT_ORACLE` and
publishes raw `RunMeasurement`s — and a public-API-only composition for five interleaved
AB/BA pairs was proved with 7 passing tests, copying no internals. **No seam is missing
from the frozen crate.** The gate is implementable the moment the database opens.

Todo 107 dispatched: `ses_02267b79affeolOovqZCdf60eJ`.

## Wave 25 (2026-08-07): the fifth seam closed, 88 dispatched for the fifth time

`main` = 3088 tests, **102/107 done**. Todo 107 merged.

### Todo 107's fix, verified by me against the real 2.6 GB database

The agent's real-backup test is **opt-in** (gated on `OPENCODE_LEGACY_DB`) with a printed
skip naming the file, because copying 2.6 GB per test run is not viable. That is the right
call — but it means the decisive claim was unverified, so I ran it myself:

```
copied /config/.local/share/opencode/opencode.db.bak.20260408 (2630582272 bytes)
real backup: sessions 2345 -> 2345, messages 92378 -> 92378, journal 0 -> 38,
             seeded [10 drizzle ids], executed 28
```

**2,345 sessions and 92,378 messages preserved exactly.** Journal seeded from
`__drizzle_migrations` with the 10 recorded ids, then only the remaining **28** executed —
no replay. Five tests cover it, including one asserting the seeded names match Drizzle's
exactly and one asserting the migrated schema equals what the current creator produces.

And it fails **safely**. Removing the seeding step (my mutation) fails 4 of 6 tests with:

```
Migration { version: 38, source: … msg: "table `project` already exists" … }
```

An error on a pre-existing table, **not** a schema recreation over live data — which is
precisely what the QA scenario demanded.

### Five seams, five identical failures

`/api/event` 404 · prune's unattributable 4.19 GB · tool execution · the internal agents ·
legacy migration. Every one invisible to a green suite.

The new rule, from this one: **a test that only exercises the greenfield path says nothing
about the upgrade path.** Todo 20 has 20 tests including a byte-for-byte schema diff
against a database the real binary created, and a round-trip back through it — all
greenfield. None ever opened a database the real binary had *already been using*.

Generalising all five: each was a **transition nobody owned** — between two routers,
between a selector and its data, between a registry and a runner, between declared agents
and the loop, between an old install and a new binary. Todo 62 remains the only seam with
a dedicated todo and the only one right first time.

### Todo 88's fifth attempt

All four blockers closed. Its own earlier finding is confirmed usable:
`measure_typescript_baseline` is subject-agnostic at the process boundary, and the
public-API-only composition for five interleaved AB/BA pairs was already proved with 7
tests. It was told to rebuild that composition, and warned about the wall clock — the
schedule is ~100 minutes of pure measurement (150s × 5 × 2 sides for `w-idle`, 450s × 5 × 2
for `w-real`) and its last attempt died 636s in. Instructed to report partial figures
rather than nothing if it cannot finish.

Session `ses_0223c678bffeW2JyrhgxVE1xzD`.

## Wave 26 (2026-08-07): the sixth seam — a config-only provider cannot start the binary

`main` = 3088 tests, **102/108 done**. Todo 88 has refused **five** times; all five correct,
and each produced a real feature (104, 105, 106, 107, now 108).

### Round 5's finding, reproduced by me with both binaries

Identical clean environment (`env -i`, empty `XDG_CACHE_HOME`,
`OPENCODE_DISABLE_MODELS_FETCH=1`), and a config that **fully** specifies
`provider.test.models.test-model` — cost, limit, `tool_call`, `options.baseURL`:

- **Ours**: dies before any turn with `the model catalog is unavailable: …`
- **Released 1.18.12**: `opencode models` exits **0**, lists dozens of models, and
  `grep -c "^test/test-model"` returns **1**

The mechanism is `packages/core/src/models-dev.ts:196-223` — three fallbacks before the
flag matters: on-disk cache, a **compile-time bundled snapshot** (`OPENCODE_MODELS_DEV`),
then `return {}` — *an empty catalog, never an error* — with config providers merged over
the result.

Ours has neither the snapshot nor the empty fallback. `CatalogError::FetchDisabled` fails
fast, and its own module docs argue for it: *"returning an empty catalog and letting the
user discover it as 'no models found' three screens later"* is the failure it was written
to avoid.

**That argument is right for the case it describes and wrong for this one.** Fail-fast is
correct when the user names a model nobody defined; it is wrong when the config already
defines the model completely, because there is nothing to look up. Todo 108 splits the two.

### Why five waves of tests never caught it

`crates/oc-cli/tests/tui_turn.rs:120-139` and `tests/tool_turn.rs:133-141` **both inject
`OPENCODE_MODELS_PATH`**. So every end-to-end seam test handed the product the very thing
the product should not have needed.

**A fixture that injects the variable the product should not need is a fixture that hides
the defect.** Removing both injections is part of todo 108's acceptance criteria — the
tests must prove the product, not the workaround.

Note `oc-testkit/src/env.rs:218` sets the flag deliberately to enforce the
no-live-provider invariant. That invariant is correct and stays; the product must work
under it.

### Six seams, one family

`/api/event` 404 · prune's unattributable 4.19 GB · tool execution · the internal agents ·
legacy migration · config-only providers. Every one a transition nobody owned, every one
invisible to a green suite. **Two of the last three were first-launch failures for a real
user**, found only because a perf gate refused to fake a number.

Session `ses_02210d5a7ffeim5kVovHbnxsqE`.

## Wave 27 (2026-08-07): the sixth seam closed, 88 dispatched for the sixth time

`main` = 3095 tests, **103/108 done**. Todo 108 merged.

### Todo 108's fix, verified by me against both binaries

Under `env -i`, empty cache, `OPENCODE_DISABLE_MODELS_FETCH=1`, a config-only provider, and
**no `OPENCODE_MODELS_PATH`**:

```
$ opencode-rust models
test/test-model            exit=0
```
and the released 1.18.12 binary agrees — `grep -c "^test/test-model"` returns 1 for the
same config.

The unknown-model path is preserved and improved. It now names the model *and* all three
ways out:

> `model 'nope/nothing' is not available: no 'provider' block in your configuration
> defines it, OPENCODE_DISABLE_MODELS_FETCH is set so no fetch … was attempted, and no
> cached catalog exists at … Define the provider and model under 'provider' in your config,
> or unset OPENCODE_DISABLE_MODELS_FETCH …, or set OPENCODE_MODELS_PATH …`

That is strictly better than what it replaced: the old message could not name the model
because it fired before the lookup.

**The design is the interesting part.** `CatalogProvenance::unresolved_model` returns an
error *only* for `FetchForbidden`, and `select_model` calls it **inside** the
`ok_or_else` — after the catalog lookup has already failed. Its doc comment states the
invariant: *"Calling this before checking the resolved catalog would resurrect the defect
it exists to avoid: a config that fully specifies the requested model has nothing to look
up and must not see an error at all."*

I mutated exactly that — hoisting the provenance check above the lookup — and
`a_config_specified_model_selects_with_no_catalog_at_all` failed. The ordering is
load-bearing and pinned.

Both `OPENCODE_MODELS_PATH` injections are gone from the seam tests, replaced by a comment
recording why: *"Injecting a fixture here is what hid todo 108 — the binary could not start
without one — through five waves."*

### Six seams, six identical failures, three of them first-launch

`/api/event` 404 · prune's unattributable 4.19 GB · tool execution · the internal agents ·
legacy databases · config-only providers.

**Three of the last four would have stopped a real user on first launch**, and all three
were found only because a perf gate refused to fake a number. The plan's own verification
strategy did not catch them; an agent declining to measure a broken subject did.

### 88's sixth attempt

All five blockers closed, and both of its earlier structural findings confirmed usable.
The remaining risk is purely the wall clock: ~100 minutes of measurement, and its last two
runs died at 636s and 652s on product bugs now fixed. It was told to preserve per-launch
figures and report partial verdicts rather than losing everything again.

Session `ses_021e9d6eaffemW7aUekVWRwEUG`.

## Wave 28 (2026-08-07): todo 109 dispatched — the seventh seam

Todo 88's harness merged (**3104 tests**), verdict still UNMEASURABLE. Blocker #6 is a real
product defect: `options.baseURL` is ignored, so a provider configured the standard upstream
way has no endpoint. Verified by me against our binary under one clean env —
`unrecoverable provider failure` with only `options.baseURL`, `transient provider failure`
once a top-level `api` is added, proving the second dials and the first never does.

Oracle spec: `provider.ts:355-358`, `options.endpoint ?? options.baseURL`. Upstream's
`model.api` is an **SDK-shape hint** (`:230-232` picks `sdk.responses` vs `sdk.chat`;
`:368` reads `api.npm`), not a URL. We conflated them.

**Second instance of one anti-pattern in two waves**: #5 was an injected env var
(`OPENCODE_MODELS_PATH`), #6 an injected config key (`"api"`). Generalised rule now in the
notepad: *a fixture that supplies something the real input shape does not have hides a defect.*

Todo 88 stays open, now `Blocked by: 86,93,109`. Its resumable harness survives the fix via a
context fingerprint, so the ~50 minutes of completed passes are not thrown away.

worktree: oc-wt/t109 | branch task-109

## Wave 29 (2026-08-07): 88 (7th) + 110 dispatched IN PARALLEL — disjoint files

Todo 109 merged and verified (**3113 tests**). I mutation-tested it four ways myself:
restoring the defect, swapping the precedence, leaking the endpoint keys into the SDK
option bag, and dropping the emptiness test — all four caught, at both the unit and the
integration layer. Hands-on with the real binary: `only options.baseURL` flipped from
`unrecoverable provider failure` to `transient` (it now dials); a live server received
requests when `endpoint` beat a dead `baseURL`; the no-endpoint case exits **1** naming
`provider.test.options.baseURL`; `models` still exits 0 for an endpoint-less provider, so
todo 108 did not regress.

### SEAM #7, measured while auditing 109's two "adjacent gaps"

`provider.options` is read at exactly one place — the endpoint keys — so every other
provider-level option is dropped, **including `apiKey`**. A real listener logged
`AUTH=None` for a config that puts `baseURL` and `apiKey` together the way the docs show.
Upstream seeds the whole SDK bag from the provider (`:1676`) and makes `options.apiKey`
primary over the stored credential (`:1719`); ours has that inverted. Also confirmed:
`${VAR}` in base URLs is never expanded despite `resolved.rs:85` promising it.
→ todos **110** and **111**.

### Why this does NOT block todo 88 (checked before dispatching)

The frozen workload puts `apiKey` in provider options (`fixtures.rs:48`), which looked like
a seventh blocker. It is not: `MockProvider` never inspects `Authorization` — there is no
auth enforcement anywhere in `oc-testkit` — and cassettes drop auth headers before matching
(`cassette.rs:57`). Proven by an exit-0 turn against a live server that saw `AUTH=None`.

### Parallel, not sequential

88 lives in `crates/oc-testkit/tests/memory.rs`; 110 lives in `crates/oc-cli/src/cmd/turn.rs`.
No shared file, no input dependency — so they fan out together. 111 is genuinely sequential
after 110 (same file region). Separate worktrees mean 88 measures its own build, and its
context fingerprint invalidates a stale pass if the binary moves under it.

worktrees: oc-wt/t88 (task-88), oc-wt/t110 (task-110)

## Wave 30 (2026-08-07): todo 110 merged and verified; 111 dispatched

**3123 tests** on `main` (`6543a2c`), 105/113 done. Todo 88 still measuring in oc-wt/t88.

### 110 verified against the oracle, not against its own report

I checked the one thing a doc comment could have been wrong about. The agent claimed an
asymmetry — `apiKey` tests `=== undefined` while `baseURL` tests `!== ""` — so an
explicitly-empty key must NOT fall back to a stored credential. Confirmed at
`provider.ts:1720`:

```ts
if (options["apiKey"] === undefined && provider.key) options["apiKey"] = provider.key
```

That is `undefined`, not `""`. The asymmetry is upstream's, and 110 reproduced it.
Hands-on, an empty key really does send `AUTH='Bearer '` rather than falling back —
which is the safe behaviour: falling back would present a real vendor key to a local
endpoint the user never authorised.

### Five mutations, all caught

Invert the apiKey precedence · drop the provider seed · swap the overlay direction ·
shallow instead of deep overlay · drop `apiKey` from the exclusion. The seed mutation
fails 2 integration + 3 unit tests, which is the proof the tests are not vacuous — a
test planting keys in a map the code never reads could not fail that way.

### Hands-on QA, five scenarios

`AUTH='Bearer sk-from-options'` where it used to be `AUTH=None` (twice — the title
prelude is a second, separately-authenticated request). A provider-level `extraBody`
now reaches the request body, proving the seed. **The 401 message needed a real 401** to
test: dead port 1 gives a connection failure, and `describe_turn_failure` only fires on
`ProviderError::Auth`. Against a server that actually replies 401:

> `authentication rejected by provider test: set `provider.test.options.apiKey`, or run `opencode auth login test``

And a deliberately-secret key is echoed **zero** times, in the message or the data dir.

### 110's three corrections to my plan text

1. `useCompletionUrls` cannot reach the wire for this transport — it gates
   `SurfaceRule::Azure`, and `openai-compatible` is `Fixed(Chat)`, so `resolve_surface`
   returns before consulting it. The wire-observable proof of the seed is `extraBody`.
2. A non-string `apiKey` is unreachable from config: `ProviderOptions::api_key` is
   `Option<String>`, so `{"apiKey": 7}` is refused at load. It asserts the
   unreachability instead, so loosening the schema fails a named test.
3. A keyless provider must NOT be refused at plan time, unlike a missing endpoint — a
   local endpoint legitimately has none.

All three are right, and I had written the criterion loosely enough to be satisfied
badly. Recorded as: *an acceptance criterion that names a mechanism can be wrong about
the mechanism; the agent that checks is worth more than the criterion.*

worktree: oc-wt/t111 (task-111) | 88 still running in oc-wt/t88

## Wave 31 (2026-08-07): todo 111 merged and verified; seam #8 found and deliberately deferred

`main` = `8a46d9d`, **3130 tests**, 106/114 done. Todo 88 still measuring in oc-wt/t88.

### 111 verified by mutation, four ways

Substitute empty for an unset variable · drop the `offset > 0` guard · swap `value` for
`truthy_value` · expand before choosing the rung. All four caught.

The empty-substitution mutation is caught at the **unit** layer only, and that is correct
rather than a gap: a literal `${VAR}` host and a collapsed empty host both fail to dial, so
the wire layer genuinely cannot tell them apart. Its test's doc comment says exactly that
and *"does not pretend to"*. An honest test boundary beats a test that appears to prove more
than it can.

I checked the one claim a doc comment could have been wrong about: `Env::value` really is
the nullish read (`env.rs:128`), and `truthy_value` (`:134`) is the `||` variant. Using
`value` matches the oracle's `?? item`, so a variable set to `""` substitutes empty while an
*unset* one keeps its placeholder. That asymmetry is upstream's, and the swap mutation
fails two tests.

### Hands-on QA, three scenarios

`${GW_HOST}` exported the way a shell export reaches a child → server observed
`host='127.0.0.1:8801'` twice. Unset → exit 1, nothing dialled. Plain URL → unaffected,
2 requests.

### Seam #8, and the pattern behind three of them

111 found that **every** connection-level failure renders as
`transient provider failure (status=None)`. The URL is in the error value; `#[error]` does
not walk `#[source]`, so it is dropped before the user sees it.

This is the **third instance of one class**. Todo 109 fixed it at one site, todo 110 at a
second. Fixing site three the same way leaves site four broken — it wants one fix at the
rendering seam. 111 correctly declined: it changes user-visible text across the whole CLI,
which is a different todo, not a rider on `${VAR}` expansion. → todo **112**, blocked by
88/89/90 on purpose, since it touches a surface every in-flight branch renders through.

### Two rules earned this wave

- *An acceptance criterion that names a mechanism can be wrong about the mechanism.* Todo
  110 corrected three of mine; 111 corrected a fourth. The agent that checks is worth more
  than the criterion.
- *Fixing one instance of a rendering defect per site hides the class.* Three sites in one
  wave is the signal to go up a level.

## Wave 32 (2026-08-08): THE MEASUREMENT LANDED. G1 PASS 0.021 · G2 FAIL 1.074

`main` = `e1d6736`, **3130 tests**, 107/117 done. Todo 88 merged after seven attempts.

### The result, recomputed by me from raw samples (not taken from the report)

| gate | Rust median | committed TS | ceiling | ratio | verdict |
|---|---|---|---|---|---|
| G1 `W-idle` | **20,040 KiB** | 954,240 | 477,120 | **0.021** | **PASS** |
| G2 `W-real` | **3,249,508 KiB** | 3,026,992 | 1,513,496 | **1.074** | **FAIL** |

All four medians reproduce exactly from the per-sample data. The paired TS runs reproduced
the committed baseline to within 5.1% / 2.5%, so this is **not** an unmeasurable-baseline
excuse — the TS side behaved. The 0.50 factor is still in frozen `methodology.rs:54-55`,
and 0.50 × 3,026,992 = 1,513,496 exactly.

**G1 is the thesis proven: 20 MB against 954 MB, a 47× reduction idle.**

### G2's root cause, traced by me

`run_turn` opens every turn with `hydrate_session` — the **whole** session, 931 messages /
3,620 parts / 105 MB — and `retained_history` only trims at a compaction marker. That set
is then re-represented twice more in the same turn (`project_history`,
`provider_messages`). Three-plus live fully-decoded copies of 105 MB explains a 3.2 GB peak.
Upstream does the same thing, which is why TS also sits near 3 GB and the two are within
10%. We ported the architecture faithfully, memory behaviour included. → todo **113**.

### The gate is invisible to CI — a finding in its own right

`should_run_expensive_gate` returns false unless `OC_MEMORY_GATE_MODE=run` or the parent
cargo command names the memory target. Under `cargo test --workspace` (premerge, CI) the
gate prints **`ok`**. Confirmed both ways: `--workspace` → ok; `-p oc-testkit --test memory`
→ FAILED in 2.21s from cache. Defensible for a 100-minute test, but **a green suite does not
mean G2 passes.** Todo 92 must say so and F1 must not read a green `make ci` as compliance.

### HAZARD for future waves: measurement pollution

88's agent deliberately refused to run any other repo command during its measurement. The
artifact also lives in the worktree's own `target/`, which is not shared — replaying the
gate on `main` silently re-runs the full 100 minutes. **I hit that and had to kill it.**
Use `OC_MEMORY_GATE_MODE=skip` unless you mean to spend the time.

Consequence: **113, 89 and 90 must not measure concurrently.** They are in disjoint files
but share one machine, so they are serialized by that resource, not by their inputs.
113 goes first because it owns the project's headline claim.

worktree: oc-wt/t113 (task-113)

## Wave 33 (2026-08-08): G2 PASSES. Both memory gates green — the project's core claim is proven.

`main` = `c0baeb8`, **3138 tests**, 108/118 done.

| gate | todo 88 | todo 113 | ceiling | verdict |
|---|---|---|---|---|
| G1 `W-idle` | 20,040 KiB | **19,776** | 477,120 | **PASS** 0.0207 |
| G2 `W-real` | 3,249,508 KiB | **1,494,236** | 1,513,496 | **PASS** 0.4936 |

**2.17x reduction on W-real**, same immutable subject. ≤50% of the TS peak is now measured
true on both gates.

### The fix

Two-phase hydration: decode metadata + compaction markers + candidate summary text first,
hydrate parts only after a *successful* marker's `tail_start_id`. The JSON predicates run
**inside SQLite**, so the 99.98% of bytes that are completed `tool` output never become Rust
JSON trees. Repair still scans the whole session, so a pending call hidden behind a valid
compaction is still fixed. All three `retained_history` fallbacks reproduced exactly.

### My "moving target" alarm was a false alarm — but 114 still matters

`context.json` records the measured DB as `opencode.db.bak.20260408`, an **immutable** April
snapshot (sha256 matches `e2cde4df…`) in which our subject genuinely is the largest. 88 and
113 measured the same thing; the comparison holds. But nothing in the repo *pins* that — it
came from an ambient `OPENCODE_DB`, and a fresh checkout would pick today's 300 MB session.
That is what 114 fixes.

### MY ERROR, recorded: I reported an equivalent mutant as an uncaught gap

I claimed the dangling-`tail_start_id` fallback was untested because `.unwrap_or(0)` passed
all 3138 tests. **That mutant cannot fail** — `tail_index = 0` makes the following
`drain(..0)` a no-op, so it is semantically identical to the early return. I sent an agent
back to write a test for a mutation no test could ever catch.

A *real* mutation, `.unwrap_or(messages.len())`, **is** caught by the test it had already
written. Mutating the no-marker branch breaks four integration tests. All three fallbacks
are genuinely guarded.

New rule: *before reporting a mutation as uncaught, prove the mutant changes behaviour. An
equivalent mutant is a no-op refactor, not a test gap.* The round still paid for itself —
the same push produced the two genuinely-missing failed-summary tests — but M1 was the real
finding and M2 was my mistake.

### Caught before dispatch: todo 114's instructions would have broken both gates

My own plan text told 114 to bump `methodology_revision`. `BaselineReport::validate`
(`baseline.rs:165`) enforces `baseline.methodology_revision == PERF_METHODOLOGY_REVISION` as
a **hard equality**, and the committed baseline records **2**. Bumping to 3 without
regenerating the baseline makes every gate fail to load it — destroying the two PASSes and
costing a ~100-minute TS re-measurement. Plan corrected: prefer keeping revision 2 and
recording the subject as *data*, not methodology.

*Second time this wave that a plan criterion of mine was wrong about a mechanism. The
pattern is now unmistakable: verify the mechanism before writing the instruction.*

### Ordering: 114 → 89 → 90, strictly sequential

Not by input dependency but by two shared resources: `oc-testkit`'s methodology surface (the
hash test), and one machine for measurements that must not run concurrently.

worktree: oc-wt/t114 (task-114)

## Wave 34 (2026-08-08): todo 114 merged — the gate is now reproducible

`main` = `9bdc26c`, **3149 tests**, 109/118 done.

### The pin, verified field by field against ground truth

`crates/oc-testkit/src/perf/subject.rs` commits **seven** fields. I checked every one
directly rather than trusting the report:

| field | pinned | measured by me |
|---|---|---|
| session | `ses_2bcaee257ffe…` | same |
| messages / parts / bytes | 931 / 3,620 / 105,118,812 | **exact match** |
| db bytes | 2,630,582,272 | `stat` agrees |
| db sha256 | `e2cde4df08cd580d…` | `sha256sum` agrees |

`select_largest_session` is **deleted** as a subject source. The heaviest session is still
queried, but only to describe what a wrong database contains inside the failure message —
never to become the subject. Three typed errors make every mismatch loud.

### It kept revision 2, as the corrected plan required

`PERF_METHODOLOGY_REVISION` is still `2`; the diff to `methodology.rs` is **pure addition**
(nothing removed), so the `0.50` factor and the hashed formula section are untouched and the
committed baseline still loads. It also added a test proving a **one-byte** drift in the
formula section no longer matches its digest — turning the hash from decoration into a lock.

### Three mutations, all caught

Silent fallback to the heaviest session · that fallback **plus** the drift comparison removed
(the true silent swap) · the sha256 comparison neutered. Each fails a named test. My first
attempt at M1 didn't compile because I invented a helper that doesn't exist — worth noting
that *my* mutation was wrong before the code was.

### Five corrections it made to my analysis, all correct

1. My acceptance criteria **contradicted my own correction** — they still demanded the hash
   "fails at the old revision", which presupposes the bump the correction forbids. It
   satisfied the intent (a falsifiable lock) over the letter. Right call.
2. The notepad's "Owed: todo 114" line still said the pin *needs* a revision bump. Dangerous
   as written; I have now annotated it **at its source** so no future reader acts on it.
3. Database identity is **not optional**, as my plan implied — a session id alone can be
   satisfied by a same-id session in a different database, and `part.data` bytes drive the
   peak. Correct, and now part of the pin.
4. The real drift is **24.7×**, not 2.85×: the live DB is 65,092,177,920 bytes against the
   snapshot's 2,630,582,272. I had only compared sessions.
5. One unreproduced flake in `dispatcher_routes_every_launch_without_a_waiting_window`
   (passed 5/5 in isolation), recorded not hidden.

That is **four separate agents** who have now corrected criteria I wrote. Confirmed rule:
*verify the mechanism before writing the instruction — and say so when a criterion is wrong
rather than satisfying it badly.*

worktree: oc-wt/t89 (task-89)

## Wave 35 (2026-08-08): todo 89 merged — G3/G4 measured over a real 2-hour soak

`main` = `2778843`, **3156 tests**, 110/118 done.

### The measurement, recomputed by me from the 500 raw samples

| gate | measured | bound | verdict |
|---|---|---|---|
| G3 slope | **0.0001775568 MiB/turn** | 1.0 | **PASS** |
| G3 peak ratio | **0.9938255268** | 1.5 | **PASS** |
| G4 | no trip | 120s progress / 1800s hard | **PASS** |

Both statistics reproduce **exactly** — the Theil–Sen slope to ten decimals. My first peak-ratio
attempt disagreed (0.9224) because I guessed the windows; theirs is *stricter* than mine —
final **tenth** against turns **40–60**, i.e. compared against the early-life plateau where a
leak would first show. Recomputing with their spec reproduced `0.9938255268` exactly. Note the
final peak is *below* the mid-life peak, so memory ended lower than it started.

### It really drove the real drivers

Two LSP servers **connected with live PIDs** (`rust` 2668273, `typescript` 2668292, zero
failures), 50,000 watched files, 713 watch events accepted / 506 published, 52 MB tool output,
**111 MB PTY output**, and one real compaction — over 500 turns / 7,200 seconds. This is the
opposite of the cassette-only soak the plan forbade.

### Three mutations, all caught by precisely-named tests

Blind the slope to growth → `a_deliberate_two_mib_per_turn_slope_fails_and_reports_the_measurement`
+ `only_the_final_half_determines_the_theil_sen_slope`. Watchdog never fires →
`a_stalled_turn_trips_the_progress_watchdog`. Any event counts as progress →
`heartbeats_raw_bytes_and_repeated_state_do_not_reset_g4_progress`. That last test name is the
one that matters: it pins the *semantics* of progress, which is exactly the subtle failure the
inherited wisdom warned about.

### The frozen-crate edits are legitimate

Only two, both **visibility-only** (`pub(crate)` → `pub`) with no behaviour change, plus the
re-exports. Critically, `sample_process_tree` *delegates to the same sampler* the memory gate
uses rather than forking a second implementation — its doc comment says so explicitly. The four
frozen thresholds, `subject.rs`, and `ts-baseline.json` are untouched.

The expensive gate is `#[ignore]`d with a reason string, so it stays out of the normal suite —
consistent with `memory.rs`'s convention of keeping ~hours-long gates opt-in.

worktree: oc-wt/t90 (task-90)

## Wave 36 (2026-08-08): todo 90 merged — all six gates G1-G6 now exist

`main` = `632099b`, **3179 tests**, 111/118 done. Every non-functional gate in the plan is
implemented and measured.

### G5/G6 results

G5: **17** persistent bounded channels, each with a declared policy and its own behaviour
test; 2 documented single-completion exclusions; **0** undeclared constructions.
G6: LSP/MCP/PTY/plugin × 2 sessions, ≥33 enumerated fixture PIDs → **0 orphans** in both
clean shutdown and parent `SIGKILL`.

### The registry is genuinely anti-vacuous — I mutation-tested it twice

Added an undeclared bounded channel to `oc-acp` production source →
`source_channel_inventory_matches_the_declared_registry` FAILED. Reintroduced an
**unbounded** channel → same test FAILED. One side is walked from source, so **the ninth
seam cannot silently return.** That is the most valuable artefact in this commit.

### The ninth seam is properly closed

`oc-acp/src/transport.rs` went from `mpsc::unbounded_channel()` to
`mpsc::channel(OUTBOUND_FRAME_CHANNEL_CAPACITY)` with capacity 64, a doc comment declaring
the lossless-block policy, and `.await` at the send site so producers wait rather than
allocate.

### THREE defects I caught that its own green gates did not

1. **A clippy warning it introduced and mislabelled.** It reported a `useless_conversion` in
   `oc-plugin/src/js/host.rs` as *"非阻塞的既有 warning"* — non-blocking and pre-existing. It
   was neither: `main` had 0 warnings and this commit wrote it. One command attributes a
   warning to a commit; do it before calling one pre-existing.
2. **A `cfg(windows)` dependency that broke offline verification.** It pinned
   `process-wrap =9.1.0`, absent from this machine's cache. On Linux `build`/`test`/`clippy`
   never resolve a Windows-gated dep, so all three were green — but
   `cargo metadata --locked --offline` resolves the **full graph** and failed, which is
   `Makefile:74` and my merge gate, i.e. **`make ci` would have been broken for every future
   task**. GitHub CI has network and would have passed, hiding it further. I verified the
   cached `=9.0.1` carries the `job-object` feature and the exact API used
   (`process_wrap::std::{ChildWrapper, CommandWrap, JobObject}`) before instructing the
   downgrade, and checked the transitive `nix 0.30.1` was cached too.
   → New rule: *`cargo metadata --locked --offline` is the only gate on a Linux host that
   sees `cfg(windows)` deps. Run it before claiming a dependency change is safe.*
3. **I had to revert a merge from `main`.** `premerge.sh` merges *then* gates, so the failed
   lock check left `main` at a commit whose `make ci` was broken. I reset to `68cddf6`,
   confirmed the lock healthy again, and only re-merged after the fix. Worth hardening the
   script eventually: gate on a scratch ref before touching `main`.

### Scope: two new crates, and an honest record of why

It created `oc-process` (a real child-containment supervisor using `PR_SET_PDEATHSIG` +
process groups on Linux, Job Objects on Windows) and `oc-reaping-fixture`, and wired
`oc-process` into five production crates. That is far beyond "write a test file".

I asked whether G6 was *measured* failing beforehand. It answered honestly:

> before `oc-process`: **NOT MEASURED; orphan count unknown**

and labelled the `oc-plugin` JavaScript-host change as *uniformity, not necessity*. It also
declined to promote a remembered intermediate failure into a numeric before-measurement. An
accurate record beats a flattering one — but note for F1: **the production supervisor is
enforcement for a new contract, not a fix for a quantified pre-existing defect.**

### Remaining chain is strictly sequential: 112 → 92 → 103 → F1-F4

Not by file conflict but by content dependency: 103's own text says to *"document it in Todo
92's compatibility matrix"*, and 92 documents the error rendering 112 changes.

worktree: oc-wt/t112 (task-112)

## Wave 37 (2026-08-08): todo 112 merged — the error-rendering class fixed at one seam

`main` = `0ff3a3a`, **3196 tests**, 112/118 done. Only 92, 103 and F1-F4 remain.

### The transformation, verified by me with the real binary

Three failure kinds that were **byte-identical** before (`transient provider failure
(status=None)`) now each name their cause:

| kind | after |
|---|---|
| unset `${GW_HOST}` | `…: builder error for url (http://${gw_host}/v1/chat/completions): Parsed Url is not a valid Uri` |
| typo'd host | `…: error sending request for url (http://gatway.example.com/…): dns error: … No address associated with hostname` |
| dead port | `…: tcp connect error: Connection refused (os error 111)` |

One seam, not per-variant: it verified `describe_turn_failure` is the **only** user-facing
renderer of a `TurnError` in the workspace, so "one seam" is a property of the code rather
than a hope. Depth 8, `": "` separator, and duplicate suppression that does **not** end the
walk on a skipped link — each pinned by a test a mutation breaks.

### The security requirement, tested against a hostile server

I stood up a listener that **echoes the `Authorization` header back inside its 401 body** —
the actual leak vector — with `apiKey = sk-SUPERSECRET-DO-NOT-ECHO`. Output:

> `authentication rejected by provider test: provider `test` returned HTTP 401: {"error": {"message": "Incorrect API key provided: Bearer <redacted>", …}}; set `provider.test.options.apiKey`, or run `opencode auth login test``

**0 occurrences** of the secret in stdout+stderr and **0** anywhere under the isolated
HOME/XDG/TMPDIR. Todo 110's guarantee survives the chain walk.

Three mutations of mine, all caught: redaction disabled → the scrub test; seam reverted to
`to_string()` → the URL test *and* the leak test (proving the leak test cannot pass
vacuously); `MAX_CAUSES` 8 → 1 → three chain tests.

### It reported an equivalent mutant correctly — the lesson took

Dropping `!text.is_empty()` changes nothing, because `str::contains("")` is vacuously true.
It said so explicitly and kept the guard with a comment. That is the exact trap **I** fell
into two waves ago with `.unwrap_or(0)`, now avoided by an agent that read the notepad.

### A real infrastructure defect it surfaced, which I then fixed

`.omo/evidence/` was gitignored as *"local proof, not shared source"* while **5** files had
been force-added — so the directory was half-tracked and todo 112's evidence was silently
dropped from its first commit.

That policy stopped being right once the gates got expensive: todo 88's transcript is the
only record of a ~100-minute paired measurement, 89's of a 2-hour soak. The decisive
argument is mechanical — **untracked files do not propagate into a `git worktree`, so the
Final Wave (which runs in worktrees) would have audited plan compliance against zero
evidence.** Now 104 files tracked, 1.5 MB, generators and probe JSON still ignored. I swept
for credentials first; every `sk-` hit was synthetic or an artifact of the word "ta**sk-**".

worktree: oc-wt/t92 (task-92)
