# learnings — opencode-rust

## Task 1 — workspace scaffold

Resolved versions, all pinned once in the root `[workspace.dependencies]`. These
came from `cargo add` against a throwaway crate on 2026-08-05 with rustc 1.96.0,
then verified by an actual `cargo build --workspace`, not from memory. Registry
in use is the `aliyun` mirror; `cargo build` locked 197 packages.

| crate | version | notes |
| --- | --- | --- |
| async-trait | 0.1.91 | |
| axum | 0.8.9 | default features; `http2`, `macros`, `ws`, `multipart` are OFF and each is a later todo's decision |
| base64 | 0.23.1 | |
| clap | 4.6.5 | `derive`, `env` |
| crossterm | 0.29.0 | must stay aligned with whatever `ratatui-crossterm` resolves to |
| dirs | 6.0.0 | |
| futures | 0.3.33 | |
| globset | 0.4.20 | |
| grep-regex | 0.1.14 | |
| grep-searcher | 0.1.17 | |
| ignore | 0.4.33 | |
| insta | 1.48.0 | `json`, `yaml`, `redactions` |
| notify | 8.2.0 | default features on purpose — see below |
| proptest | 1.11.0 | |
| ratatui | 0.30.2 | `serde`; 0.30 is split into `ratatui-core` / `-widgets` / `-crossterm` / `-macros` |
| reqwest | 0.13.4 | `default-features = false` + `json`, `stream`, `rustls`, `charset`, `http2` |
| schemars | 1.2.2 | `derive` + `std` are already default; `uuid1` / `url2` / `indexmap2` integrations are opt-in |
| serde | 1.0.229 | `derive` |
| serde_json | 1.0.151 | |
| sha2 | 0.11.0 | |
| tempfile | 3.27.0 | |
| thiserror | 2.0.19 | 2.x, not 1.x |
| time | 0.3.55 | `formatting`, `parsing`, `macros`, `local-offset` |
| tokio | 1.53.1 | `full` |
| tokio-util | 0.7.19 | `io`, `codec` |
| toml | 1.1.4 | 1.x — the 0.8/0.9 API notes found in older references do not apply |
| tower | 0.5.3 | `util`, `timeout` |
| tower-http | 0.7.0 | `fs`, `trace`, `cors` |
| tracing | 0.1.44 | |
| tracing-appender | 0.2.5 | |
| tracing-subscriber | 0.3.23 | `env-filter`, `fmt` |
| url | 2.5.8 | |
| uuid | 1.24.0 | `v4`, `v7`, `serde` |
| walkdir | 2.5.0 | |
| which | 8.0.5 | |
| wiremock | 0.6.5 | |

Non-obvious settings, each of which cost a build cycle to find:

1. **`reqwest` 0.13 renamed the TLS feature.** The `rustls-tls` name that every
   0.12-era snippet uses does not exist; `cargo add` rejects it outright with
   "unrecognized feature for crate reqwest: rustls-tls". The 0.13 spelling is
   plain `rustls`. Also note `charset` and `http2` are NOT in reqwest's default
   set once `default-features = false` is set, so both are listed explicitly.
2. **`unsafe_code = "forbid"` only applies to crates that opt in.** A crate
   without `[lints] workspace = true` compiles unsafe code silently. All 33
   have the key; a new crate without it is a review defect, not a style nit.
3. **A `[profile.*.package.X]` pin for a package nothing depends on is a cargo
   warning**, and 16 of them appeared the moment the profile pins landed:
   `warning: profile package spec 'ratatui' in profile 'dev' did not match any
   packages`, once per package per profile. Because the acceptance bar is zero
   warnings, `oc-tui` declares `ratatui` + `crossterm` now — that pulls
   `ratatui-core`, `ratatui-widgets`, `ratatui-crossterm`, `unicode-width`,
   `unicode-segmentation`, and `unicode-truncate` transitively and satisfies all
   eight specs in both profiles. Do not "clean up" those two unused deps.
4. **`notify` keeps its default features.** `macos_fsevent` is a default; adding
   it explicitly alongside `default-features = false` silently drops the Linux
   inotify backend. Todo 50 owns any real backend selection.
5. **`resolver = "3"`** is what makes `rust-version` participate in resolution —
   that is why `cargo build` reports "Locking 197 packages to latest Rust 1.96
   compatible versions" rather than pulling something newer that then fails.
6. Debug profile uses `debug = "line-tables-only"`: panics and backtraces keep
   file and line, but links far faster than full debuginfo. jcode goes further
   with `debug = 0`, which loses that.

## Task 2 — typed error taxonomy (`oc-error`)

### The final variant list

Nine public types. `Recovery` is the answer every error owes its caller;
`Recoverable` is the trait that produces it; the rest are the taxonomy proper.

| type | variants |
| --- | --- |
| `Recovery` | `Retry { after: Option<Duration> }`, `Compact`, `Reauthenticate`, `Fail` |
| `ProviderError` | `ContextLimit { limit_tokens, used_tokens }`, `RateLimited { retry_after }`, `Transient { status, source }`, `Auth { provider, source }`, `Refused { provider, provider_text }`, `Fatal { status, source }` |
| `ToolError` | `Denied { tool }`, `InvalidArgs { tool, source }`, `Timeout { tool, elapsed }`, `NotFound { tool }`, `Failed { tool, source }` |
| `ConfigError` | `Io { path, source }`, `Json { path, source }`, `Invalid { path, issues }`, `Frontmatter { path, source }`, `DirectoryTypo { path, dir, suggestion }`, `RemoteAuth { url, remote }` |
| `DbError` | `Open { path, source }`, `Migration { version, source }`, `Query { source }`, `Busy { retry_after }`, `NotFound { table, id }`, `Decode { table, source }` |
| `PluginError` | `Load { plugin, source }`, `Hook { plugin, hook, source }`, `Timeout { plugin, hook, elapsed }`, `IncompatibleApi { plugin, required, provided }` |
| `McpError` | `Connect { server, source }`, `Handshake { server, source }`, `Protocol { server, source }`, `Timeout { server, elapsed }`, `ToolCall { server, tool, source }` |
| `LspError` | `NotInstalled { server, command }`, `Spawn { server, command, source }`, `Initialize { server, source }`, `Protocol { server, source }`, `Timeout { server, elapsed }`, `Exited { server, code }` |
| `Error` | one `#[from]` newtype per domain above, all `#[error(transparent)]` |

Also public: `ConfigIssue { key_path: Vec<String>, detail: String }` and
`BoxSource = Box<dyn Error + Send + Sync + 'static>`.

**The retryable set, for the record.** `ProviderError::{RateLimited, Transient}`,
`ToolError::Timeout`, `DbError::Busy`, `PluginError::Timeout`,
`McpError::{Connect, Timeout}`, `LspError::{Timeout, Exited}`. Nothing in
`ConfigError` retries. **`ProviderError::ContextLimit` is deliberately NOT
retryable** — it maps to `Recovery::Compact`. Conflating "recoverable" with
"retryable" is what makes a retry loop burn its whole attempt budget re-sending a
request that overflows the same window every time.

**Only two variants carry a delay from the wire:**
`ProviderError::RateLimited.retry_after` and `DbError::Busy.retry_after`. Every
other retryable variant yields `Recovery::Retry { after: None }`, meaning "the
peer named no delay, apply your own backoff".

### What `thiserror` 2.0.19 actually does

Verified by compiling a throwaway probe enum in `oc-error` before writing the
real taxonomy, then deleting it. All five behaviours confirmed by a passing test,
not assumed:

1. **`#[source] source: Option<BoxSource>` works.** The derive special-cases an
   `Option` source: `None` yields `Error::source() == None`, `Some(e)` yields that
   `e`. This is what allows one variant to serve both "we have a cause" and "the
   provider only gave us a status code" without splitting into two variants.
2. **A field named `source` is picked up implicitly**, `#[source]` attribute or
   not. Every one is annotated here anyway — the intent should not depend on a
   naming convention a future rename could silently break.
3. **`{field:?}` works in `#[error("…")]`**, so `Option<u64>` and
   `Option<Duration>` render without a helper. `Duration` has no `Display`, only
   `Debug`, so `{after:?}` is the only way to show it — it prints as `30s`, not
   `Duration { secs: 30 }`.
4. **`#[error(transparent)]` + `#[from]` on a newtype variant** forwards both
   `Display` and `source()` to the inner error. The aggregate `Error` is therefore
   free: `Error::from(ToolError::Failed{…}).to_string()` equals the inner text,
   and the full cause chain survives one more hop.
5. **A shorthand `#[error("… ({} issue(s))", issues.len())]` is accepted** —
   trailing format arguments may be arbitrary expressions over the variant's
   fields, which avoids a hand-written `impl Display`.

Two things that differ from 1.x-era examples: `Option` source support is not
something to reach for a `#[source]`-less workaround for, and there is no need
for `#[backtrace]` unless nightly backtraces are wanted (not used here).

### Measured sizes

`size_of::<Error>() == 80`, `ProviderError` 48, `ConfigError` 72, `Recovery` 16.
Clippy's `result_large_err` threshold is 128 bytes, so nothing needs boxing yet.
A test asserts the 128-byte budget so the crate that blows past it fails in
`oc-error` rather than as a warning in whatever unrelated crate happens to return
the fat `Result`. Watch `ConfigError` — it is the widest at 72 bytes because
`Invalid` holds a `PathBuf` plus a `Vec`.

### Cheap facts worth not rediscovering

- **Integration tests in `tests/` see both `[dependencies]` and
  `[dev-dependencies]`.** `tests/taxonomy.rs` uses `serde_json` (a normal
  dependency) and `tests/no_anyhow_in_libraries.rs` uses `walkdir` (dev-only);
  neither needed anything declared twice.
- **`cargo fmt` was NOT clean on hand-written code.** Task 1 left every crate
  rustfmt-clean (spot-checked `oc-config`, `oc-types`, `oc-cli`, `oc-tui`,
  `oc-db`), so `cargo fmt -p oc-error` was run to match. Run it per-crate, never
  `--all`, while sibling agents hold files open.
- **`size_of` needs no import in edition 2024** — it is in the prelude, so
  `std::mem::size_of` is redundant and clippy would flag the import.
- **`std::io::Error::other(msg)`** is the short constructor for a test cause;
  `Error::new(ErrorKind::Other, msg)` is the older spelling and clippy prefers
  the former.

## Task 3

### `Notify` semantics this primitive depends on

- Tokio 1.53.1's `Notified` snapshots the `notify_waiters()` generation when the
  future is **created**, so a future created before `notify_waiters()` resolves
  even if it was not polled or explicitly enabled first. The contract test
  `interrupt_notify_waiters_tracks_creation_and_stores_no_permit` locks this
  exact behavior and also proves that a future created after `notify_waiters()`
  does not resolve: `notify_waiters()` stores no permit for future waiters.
- `InterruptSignal::notified()` still pins the future and calls `enable()` before
  re-reading the atomic flag. That makes registration explicit and keeps the
  cancellation protocol correct if a future Tokio version stops giving
  creation-time registration. The flag handles fires that happened before the
  waiter existed; the enabled waiter handles fires between registration and the
  flag re-check.
- Because creation-time registration is real in the pinned Tokio, deleting only
  `enable()` does not make the requested hammer time out. The exact removal was
  run and recorded honestly in Task 3 evidence; it stayed green but introduced
  an `unused_mut` warning, which the zero-warning gate turns into the recorded
  deliberate failure.

### Atomic ordering

`flag` and `epoch` use `Ordering::SeqCst` for every load, store, and increment.
The reset protocol observes two atomics as one ordered state machine: `fire()`
increments the epoch before publishing the flag; `reset_if_epoch()` checks the
epoch, clears the flag, then checks the epoch again and restores the flag plus a
broadcast if a newer fire raced. A weaker mixed Acquire/Release scheme could be
made correct, but would require a separate proof across two atomic locations and
would buy nothing on this low-frequency control path. `SeqCst` preserves one
global order that sync readers, async waiters, and OS-thread race tests can all
reason about directly.

## Task 4 — XDG layout and project resolution (`oc-paths`)

### The resolved path table, measured on this machine

Produced by `opencode debug paths` from the real 1.18.12 binary and reproduced
byte-for-byte by `Layout::debug_paths_dump()`. `HOME=/config`, no `XDG_*` set:

| getter | value | oracle |
| --- | --- | --- |
| `home()` | `/config` | `OPENCODE_TEST_HOME ?? os.homedir()` |
| `data()` | `/config/.local/share/opencode` | `global.ts:11` |
| `bin()` | `/config/.cache/opencode/bin` | `global.ts:22` |
| `log()` | `/config/.local/share/opencode/log` | `global.ts:23` |
| `repos()` | `/config/.local/share/opencode/repos` | `global.ts:24` |
| `cache()` | `/config/.cache/opencode` | `global.ts:12` |
| `config()` | `/config/.config/opencode` | `global.ts:13` |
| `state()` | `/config/.local/state/opencode` | `global.ts:14` |
| `temp()` | `/tmp/opencode` | `global.ts:15` |

Not printed by `debug paths`, so unit-tested against source instead:

| getter | value |
| --- | --- |
| `snapshot_root()` | `<data>/snapshot` |
| `snapshot_store(id, wt)` | `<data>/snapshot/<id>/<sha1(wt)>` |
| `tool_output()` | `<data>/tool-output` |
| `auth_file()` | `<data>/auth.json` |
| `mcp_auth_file()` | `<data>/mcp-auth.json` |
| `models_cache()` | `<cache>/models.json`, or `<cache>/models-<sha1(source)>.json` |
| `db_path()` | `<data>/opencode.db` on `latest`/`beta`/`prod`, else `<data>/opencode-<channel>.db` |

### The snapshot hash is SHA-1, over the raw worktree string

`Hash.fast` = `createHash("sha1").update(input).digest("hex")`
(`packages/core/src/util/hash.ts:4-6`). Input is `ctx.worktree` verbatim — **not**
canonicalized, **not** trailing-slash-normalized. `/repo` hashes to
`83630750896a66f949c084b8d0e97c1f692b3608` and `/repo/` to
`9feece9c0dfe9efe2cb209e4c589790fd731e71a`, so the two spellings get two stores.
Todo 23 must hash exactly what it was handed. The same SHA-1 keys
`models-<hash>.json` (`models-dev.ts:163`) and the remote-derived project id,
`Hash.fast("git-remote:" + normalized)` (`project.ts:78`).

`sha2` is pinned in the workspace but SHA-2 is the wrong family; no SHA-1 crate is
pinned, so `sha1.rs` implements FIPS 180-4 in ~60 lines. Every test vector is
coreutils `sha1sum` output, never this implementation read back to itself.

### Six things in `global.ts` and its neighbours that surprised me

1. **`path.join` normalizes and `PathBuf::join` does not**, and it shows up in the
   layout. Measured `data` rows: `XDG_DATA_HOME=/tmp/x//data` →
   `/tmp/x/data/opencode`; `=/tmp/x/../y` → `/tmp/y/opencode`; `=x/y/..` →
   `x/opencode`; `=a/../../b` → `../b/opencode`. A `PathBuf`-based join produces a
   different string for four of those. Hence `node_path.rs`, a line-for-line port
   of `path.posix` — used for every join in the crate.
2. **A relative `XDG_DATA_HOME` is honoured verbatim.** `XDG_DATA_HOME=relx` makes
   the oracle report `data relx/opencode`. This is the reason the `dirs` crate is
   **not** used: `dirs` discards a relative XDG value and substitutes the
   home-relative default, which would silently relocate the whole data directory.
   `dirs` stays pinned in the workspace but `oc-paths` does not depend on it.
3. **`||` vs `??` is observable, and the two are one line apart.**
   `xdg-basedir@5.1.0` uses `env.XDG_DATA_HOME || join(home, ".local", "share")`,
   so `XDG_DATA_HOME=` (empty) **falls back** — confirmed, still reports
   `/config/.local/share/opencode`. `Global.Path.home` uses
   `process.env.OPENCODE_TEST_HOME ?? os.homedir()`, so `OPENCODE_TEST_HOME=`
   (empty) is **used as-is** — confirmed, prints an empty `home`. `Env::truthy_value`
   and `Env::value` model the two and are deliberately not interchangeable.
4. **`os.tmpdir()` strips one trailing slash only when the result stays non-empty.**
   `TMPDIR=/probe/` → `/probe/opencode`, but `TMPDIR=/` → `/opencode` (the
   `length > 1` guard). The ladder is `TMPDIR || TMP || TEMP || "/tmp"`, all three
   verified.
5. **`OPENCODE_CONFIG_DIR` does *not* move `Global.Path.config`.** `debug paths`
   still prints the XDG directory; the override only appears in
   `Global.make()`'s `config` field (`global.ts:64`) and as the *last* entry of the
   config-directory chain. Two separate accessors, `config()` and
   `effective_config()`. Todo 8 needs the distinction.
6. **`OPENCODE_DISABLE_CHANNEL_DB` is case-sensitive while every other flag is
   not.** `Flag.truthy` lower-cases (`flag.ts:4-6`), but `database.ts:50-52`
   compares the raw string against `"1"`/`"true"`. So `TRUE` enables
   `OPENCODE_DISABLE_PROJECT_CONFIG` but **not** `OPENCODE_DISABLE_CHANNEL_DB`.
   Modelled as `Env::flag` and `Env::exact_flag`.

### `OPENCODE_DB`, all three forms, verified against the binary

`database.ts:43-55`. Order matters: the override is checked before any channel
rule, so `OPENCODE_DB=:memory:` beats `OPENCODE_DISABLE_CHANNEL_DB=1`.

- `:memory:` → passed through as a sentinel. Modelled as `DbLocation::Memory`,
  not a `PathBuf`, so a consumer cannot `create_dir_all` its parent.
- absolute → used verbatim.
- **relative → joined onto `data()`, not the cwd.** Observed, not read: with
  `XDG_DATA_HOME=<tmp>/xdg` and `OPENCODE_DB=relprobe.db`, run from an empty
  directory, the real binary created `<tmp>/xdg/opencode/relprobe.db` (plus
  `-shm`/`-wal`) and left the working directory empty.

Channel suffix: `latest`/`beta`/`prod` → `opencode.db`; anything else →
`opencode-<channel>.db` with `[^a-zA-Z0-9._-]` → `-`. A build with no
`OPENCODE_CHANNEL` reports `local`, hence `opencode-local.db` — see issues.md.

### `FSUtil.up` is the primitive under every discovery step

`fs-util.ts:168-182`. Three behaviours consumers depend on:

- **`stop` is checked *after* the directory is searched**, so it is inclusive: a
  walk bounded by the worktree still reads the worktree's own `.opencode`.
- **`stop` is string equality, not ancestry.** A `stop` off the chain never
  matches and the walk runs to the filesystem root.
- **Targets are probed in the order given, per directory.** For
  `[".jsonc", ".json"]` a directory holding both yields `.jsonc` first, which is
  precisely why `ConfigPaths.files`' closing `toReversed()` ends up putting
  `.json` before `.jsonc` within one directory.

`fs.exists` is true for a file *or* a directory, so a *file* named `.opencode` is
collected by the config chain.

### The config chain order, for todo 8

`directories()` = global config, then project `.opencode` nearest-first (unless
`OPENCODE_DISABLE_PROJECT_CONFIG`), then `$HOME/.opencode`, then
`OPENCODE_CONFIG_DIR`; deduplicated first-occurrence-wins. The `$HOME` probe uses
`start === stop === home`, so it is a single-directory check, never a walk.
`files()` is the reverse — outermost first. `fileInDirectory()` returns
`[name.json, name.jsonc]`, the opposite order from the `files()` probe; both are
upstream's and neither is a typo.

### Project identity, for todos 20/23

`Project.resolve` (`project.ts:110-122`): `git.repo.discover` finds the nearest
`.git`, then `rev-parse --show-toplevel` / `--git-dir` / `--git-common-dir`.
Outside a repository, `directory` becomes `path.parse(input).root` (`/`) and the
id is `global` — so every non-repository directory on a machine shares one project
id. Inside, the id is `remote ?? cached ?? rootCommit ?? global`, where `cached`
reads `<commonDirectory>/opencode`.

**`--git-dir` and `--git-common-dir` differ in a linked worktree** and the
distinction is load-bearing: the snapshot store keys on the *worktree*, the id
marker lives in the *common* directory. Verified with a real `git worktree add` in
a temp repo — `git_directory` came back as `<repo>/.git/worktrees/linked` while
`common_directory` stayed `<repo>/.git`.

Remote normalization (`project.ts:81-103`) was pinned by **executing the oracle's
own `url`/`parts` helpers under `bun`** over 19 inputs rather than reading them.
Two results are counter-intuitive and I had one of them wrong before running it:

- `github.com:owner/repo` → **undefined**. WHATWG accepts `github.com` as a URL
  scheme (letters and `.` are legal), so `new URL` succeeds with an opaque path,
  the SCP fallback is never reached, and `hostname` comes out empty. Rust's
  `url::Url::parse` behaves identically, so the port matches for free.
- `http://[::1]/a/b.git` → `[::1]/a/b`, brackets kept. `url::Url::host_str()`
  agrees with JS `hostname` here.

`git@github.com:owner/repo.git` and `https://github.com/owner/repo.git` both
normalize to `github.com/owner/repo`, so changing transport does not fork a
project's sessions.

### Cheap facts

- **The 1.18.12 binary is a valid oracle for the 1.18.13 source.**
  `git diff --stat 7fe993879f..aefaf140c1` over the thirteen layout-relevant files
  (`global.ts`, `database.ts`, `config/paths.ts`, `tool-output-store.ts`,
  `models-dev.ts`, `auth/index.ts`, `mcp/auth.ts`, `snapshot/index.ts`,
  `util/hash.ts`, `debug/index.ts`, `project.ts`, `git.ts`, `fs-util.ts`) is
  **empty** across 18 commits. Check this before blaming version skew for a
  future differential failure.
- **The `opencode` on `PATH` is a mise shim that re-execs `mise`.** It aborts
  under `env -i` and, worse, rewrites the environment — useless for a differential
  test. Use the real binary at
  `~/.local/share/mise/installs/opencode/latest/opencode`.
- **`cargo` is also a mise shim.** `cargo run` under `env -i` with a redirected
  `XDG_CONFIG_HOME` fails with `Config files in ~/.config/mise/config.toml are not
  trusted`. Build first, then invoke `target/debug/examples/<name>` directly. This
  cost one evidence regeneration.
- **`std::env::home_dir()` is un-deprecated and correct again** as of recent Rust;
  it returns `/config` here and matches Node's `os.homedir()` on Unix (`HOME`,
  then `getpwuid`).
- **`std::env::set_var` is `unsafe` in edition 2024**, and this workspace forbids
  `unsafe_code`, so **no test in this workspace can mutate the process
  environment**. Every env-dependent type needs an injectable environment from the
  start; retrofitting one later means rewriting its whole test suite.
- **`url` costs ~15 transitive crates** (`idna`, five `icu_*`, `zerovec`, …). It is
  already a workspace dependency, and hand-rolling WHATWG parsing to save the
  build time would be the wrong trade for a value that keys the project id.
