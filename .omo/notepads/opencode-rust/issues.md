# issues — opencode-rust

## Task 1 — unresolved tension: SQLite needs a C toolchain

**For todo 19 (SQLite parity) and todo 91 (packaging) to resolve together, not
separately.** Recording it here so neither discovers it late.

Todo 91 requires: "no OpenSSL and no C-toolchain-dependent dependency in the
default feature set … Must NOT require a C compiler for a default build."

Todo 19 requires byte-level schema parity with the TypeScript `opencode.db`,
which is real SQLite. Every production-grade SQLite binding for Rust compiles C:

- `rusqlite` → `libsqlite3-sys` → bundles and compiles the SQLite amalgamation.
- `sqlx` with `sqlite` → same `libsqlite3-sys`.
- `libsql` / `limbo` (`turso`) → pure Rust, but not production-grade for a
  file-format-compatible store today.
- `rusqlite` with `--features sqlcipher` or a system `libsqlite3` only moves the
  C dependency from build time to link time; it does not remove it.

So the two constraints as written cannot both hold. Todo 1 pins **no** SQLite
crate — that choice belongs to todo 19 — but the decision must be made
knowingly. The options, none of them free:

1. Accept `cc` as a build requirement and drop "no C toolchain" from todo 91,
   keeping only "no OpenSSL". Cheapest, and `cc` is present on every platform
   the release matrix targets. Cost: `aarch64` and `musl` cross-builds need a
   working cross C toolchain, which is the actual reason the constraint was
   written.
2. Ship SQLite behind a non-default feature. Contradicts todo 19, since the DB
   is not optional for a drop-in replacement.
3. Use a pure-Rust engine and give up on reading an existing `opencode.db`.
   Contradicts the parity requirement outright.
4. Statically link a prebuilt SQLite per target in CI. Keeps a C artifact but
   removes the compiler from the build; adds real release-pipeline complexity.

**Same tension, already live in TLS.** `reqwest` 0.13's `rustls` feature selects
the `aws-lc-rs` crypto provider, whose `aws-lc-sys` crate builds C — verified in
the resolved graph. Todo 91's reference line says "rustls only, no OpenSSL",
which this satisfies; the "no C compiler" half it does not. If a C-free default
is genuinely required, the TLS fallback is `rustls` with the `ring` provider
(`rustls-no-provider` plus an explicit `rustls` dependency), and `ring` still
ships pre-generated assembly and needs `cc`. There is no production-grade C-free
TLS stack in Rust today. Todo 91 should reconcile the wording with reality
rather than have a build silently violate it.

Both `cc` and `gcc` are present on this machine (`/usr/bin/cc`, `/usr/bin/gcc`);
`cmake` is not, which matters because `aws-lc-sys` prefers cmake and falls back
to its own build script.

### RESOLVED (orchestrator, after Task 2) — option 1, with the cross-compile gap closed by zig

The user pointed at `/config/workspace/ProdDir/AI/codegraph-rust` as a project
that "uses SQLite without depending on a C toolchain". **That premise is false,
and the way it is false is the answer.** Measured in that repo:

- `Cargo.toml:47` — `rusqlite = { version = "0.31", features = ["bundled"] }`.
  `bundled` *is* the flag that compiles the SQLite C amalgamation.
- Hard proof a C compiler ran: `target/{debug,release}/build/libsqlite3-sys-*/out/`
  contains `c877a2978823c39d-sqlite3.o` and `libsqlite3.a` (3 of each across
  profiles). 68,636 `.o` files in that target tree overall.
- `cargo tree -i cc` → `cc v1.2.66 ← cmake v0.1.58 ← aws-lc-sys v0.42.0 ←
  aws-lc-rs ← rustls ← reqwest`. Identical to the TLS half of the tension above,
  independently confirmed in a second project.

So codegraph-rust compiles C for both SQLite and TLS. What it does **not** do is
require the developer to assemble a per-target C cross-toolchain — and that was
the real fear behind todo 91's wording. Its release pipeline
(`.github/workflows/release-please.yml:178-250`) solves it with **`cargo-zigbuild`**:

| target | builder |
| --- | --- |
| `x86_64-unknown-linux-musl` | `cargo zigbuild` (Zig 0.13.0 via `mlugg/setup-zig@v2`) |
| `aarch64-unknown-linux-musl` | `cargo zigbuild` |
| `x86_64-apple-darwin` | native `cargo build` on `macos-latest` |
| `aarch64-apple-darwin` | native on `macos-latest` |
| `x86_64-pc-windows-msvc` | native on `windows-latest` |
| `aarch64-pc-windows-msvc` | native on `windows-11-arm` |

Zig ships a complete hermetic C cross-compiler as a single download, so the musl
and aarch64 legs — exactly the legs that motivated "no C toolchain" — need no
system cross-gcc, no `cross` docker images, and no per-target apt packages.

**Decision for this project**: adopt option 1 from the list above, unchanged in
substance but no longer a compromise. `rusqlite` with `bundled` for byte-level
`opencode.db` parity (todo 19-20); `cc` is an accepted build requirement;
cross-compilation goes through `cargo-zigbuild` for the musl/aarch64-linux legs
and native runners elsewhere (todo 91). Todo 91's wording is corrected in the
plan from "must not require a C compiler" to "must not require a *per-target C
cross-toolchain*", which is both achievable and what was actually wanted.
Options 2, 3 and 4 are dropped: 2 and 3 contradict parity, and 4 (prebuilt
per-target SQLite) is strictly more release-pipeline complexity than zigbuild for
the same outcome.

## Task 4 — open questions and gaps in `oc-paths`

### 1. A `cargo` build reads a different database than the installed binary

`db_path()` derives the channel from `option_env!("OPENCODE_CHANNEL")`, defaulting
to `local` — which mirrors the oracle exactly (`installation/version.ts:7` reports
`"local"` when the define is absent, and `database.ts:54` then suffixes the file).
So a plain `cargo build` opens `opencode-local.db` while the installed 1.18.12
binary opens `opencode.db`.

That is faithful, but it means **development builds do not see the user's real
sessions**, which will look like a parity bug the first time someone tries it.
**For todos 19-20 and 91**: decide whether the release build sets
`OPENCODE_CHANNEL=latest` (matching the TypeScript release), and whether a
development build should offer an opt-in to the release database. `db_path()` and
`db_path_for_channel()` are both public precisely so this can be settled without
touching `oc-paths`.

### 2. Windows path semantics are not implemented

`node_path` is `path.posix` only. `path.win32` is a different algorithm (drive
letters, UNC roots, `\` separators, per-drive working directories) and
`FSUtil.windowsPath` has four real rewrite rules for `/c:/`, `/c/`, `/cygdrive/c`
and `/mnt/c` (`fs-util.ts:257-264`) that are currently a no-op because
`process.platform !== "win32"` short-circuits them.

`windows_path()` exists as a named identity function so the branch has a home.
**For whichever todo first targets Windows**: `node_path` needs a `win32` arm and
every `join`/`dirname`/`resolve` call site needs to pick a flavour. Nothing in the
crate is Windows-correct today; it is Windows-*shaped*.

### 3. `Env::from_process` lossily converts non-UTF-8 variables

`std::env::vars()` panics on a non-UTF-8 variable, so `from_process` uses
`vars_os` plus `to_string_lossy`. A non-UTF-8 `XDG_DATA_HOME` therefore resolves
to a path containing U+FFFD rather than the original bytes.

The oracle mangles it too — `process.env` is a JavaScript string map — but not
necessarily *identically*, since V8's and Rust's replacement behaviour need not
agree byte for byte. **Unverified**, because producing a non-UTF-8 environment
variable requires `set_var`, which is `unsafe` and forbidden here. If a user ever
reports a mismatch on a non-UTF-8 locale, this is the first place to look.

### 4. `xdg_base` with no home at all: the oracle throws, this crate does not

`xdg-basedir` yields `undefined` when `os.homedir()` returns nothing, and
`global.ts:11` then calls `path.join(undefined, "opencode")`, which throws a
`TypeError` at import. This crate returns a *relative* base instead
(`.local/share/opencode`).

Unreachable on Unix, where `getpwuid` always resolves for a live process, so it is
recorded rather than resolved. **If a todo ever runs this in a container with no
`HOME` and no passwd entry**, decide whether to reproduce the crash or keep the
relative fallback — and note that the two behaviours are not interchangeable for a
process that then writes files.

### 5. Paths whose oracle behaviour I could **not** determine

Two getters the plan named are reproduced from source but never observed in
action, because no command reachable without provider credentials writes to them.
The paths are asserted against `packages/core/src/tool-output-store.ts:17`/`:118`
and `packages/opencode/src/mcp/auth.ts:37`, not against a file appearing on disk:

- **`tool_output()` = `<data>/tool-output`.** Supporting evidence: the directory
  exists in this machine's real data dir, created by past real sessions. But its
  *filenames* are not settled here — and todo 83 has already decided to diverge on
  them (encoding the session id so a prune can attribute precisely). **Todo 83 owns
  the filename scheme; `oc-paths` provides only the directory.**
- **`mcp_auth_file()` = `<data>/mcp-auth.json`.** Not present on this machine (no
  MCP OAuth has been performed), so the name comes from source alone. **Todo 24
  should confirm** the real binary writes exactly that name on first MCP OAuth.

`auth_file()` is confirmed: `/config/.local/share/opencode/auth.json` exists on
disk, mode `-rw-------`. Todo 24 should note the 0600 mode is what upstream
actually produces — the plan's "mode 0600" requirement matches observed reality.

### 6. Existing snapshot stores are keyed on a PRE-MIGRATION project id — computing
### `Project.resolve` correctly is not enough to find them

**This is the most consequential thing this task found, and it is unresolved.**

The worktree half of `snapshot_store()` is confirmed against a store the real
binary created. For the oracle source tree:

```text
worktree        /config/workspace/ProdDir/AI/opencode
sha1(worktree)  942a3d4a25f2a566e06876200a532c3a0984f4f7
on disk         ~/.local/share/opencode/snapshot/
                  4b0ea68d7af9a6031a7ffda7ad66e0cb83315750/
                  942a3d4a25f2a566e06876200a532c3a0984f4f7   <- exists
```

The **project id** half does not line up. That repo has
`origin = https://github.com/anomalyco/opencode.git`, so `Project.resolve`'s
`remote ?? previous ?? root` precedence (`project.ts:115`) yields
`sha1("git-remote:github.com/anomalyco/opencode")` =
`012780c4098d08caa4ea8c479ed0a4690489f38d`. But the store on disk sits under
`4b0ea68d7af9a6031a7ffda7ad66e0cb83315750`, which is that repository's **root
commit**, and `.git/opencode` still caches that same root-commit value.

Checked more broadly: **every** `.git/opencode` marker found on this machine
equals its repository's root commit, including repos that have a remote
(`opsx-backend`, `hermes-agent`, `r-tools`). Not one holds a remote-derived hash.

The mechanism is visible in the source. `packages/opencode/src/project/project.ts`
`fromDirectory` (`:213-221`) calls `projectV2.resolve()`, takes `data.id` — the
remote-derived id — and then calls `migrateProjectId(data.previous, projectID)` to
move database rows from the old id to the new one. `Project.commit`
(`project.ts:124-126`) is the bridge that rewrites `.git/opencode`, and
`project.ts:40-49` documents that the *old* service still owns persistence and
migration while the two coexist.

So the id is mid-migration on real disks: rows and snapshot stores created before
remote-precedence landed are keyed on the root commit, and the marker has not been
rewritten on this machine.

**What this means for todo 23 (snapshots) and todo 20 (project/session schema):**
`oc-paths` gives you a faithful `Project.resolve` — each branch is unit-tested —
but calling `snapshot_store(resolve_project(dir).id, worktree)` will point at
`012780c4…/942a3d4a…`, which **does not exist**, while the user's real snapshots
live under `4b0ea68d…/942a3d4a…`. Reading only the computed id silently abandons
every existing snapshot, which is exactly the drop-in-replacement promise this
task exists to protect.

Todo 23 must therefore implement the **migration**, not just the computation:
consult `ResolvedProject::previous` (already exposed for this reason), and either
adopt the previous id when a store exists under it or move the store — matching
whatever `migrateProjectId` does for database rows. Todo 20 owns the row half and
the two must agree, or a session and its snapshots will end up under different
ids.

**Unresolved and needing a decision, not a guess:** whether upstream 1.18.13
rewrites `.git/opencode` to the remote-derived id on next run (in which case the
Rust binary must do the same, at the same moment), or whether the marker is
intentionally left alone and `previous` is consulted forever. The evidence here
shows the marker unchanged, but that may only mean the installed binary has not
run in those directories since the change.
