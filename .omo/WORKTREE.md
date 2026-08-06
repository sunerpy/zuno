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
