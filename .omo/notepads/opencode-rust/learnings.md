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

## Task 5 — oc-observability (tracing subscriber, stdout-safe routing)

### The two oracle env vars: exact names, exact accepted values

Both live in one place, `packages/core/src/observability/logging.ts`. Read them there,
not from memory — two of the three details below are counter-intuitive.

`OPENCODE_LOG_LEVEL` (logging.ts:56-65)
- Uppercased, then looked up in a **four**-key map: `DEBUG INFO WARN ERROR`.
- So `debug`, `Debug`, `DEBUG` all work.
- **Anything else silently becomes `INFO`.** Not an error, not a warning.
- **`TRACE` is NOT accepted.** It maps to `INFO` like any other unknown value. The
  CLI's `--log-level` offers the identical four choices (`index.ts:58-62`), so
  accepting a fifth value in Rust would be a silent behaviour divergence. `TRACE` is
  reachable only through a programmatic directive string.

`OPENCODE_PRINT_LOGS` (logging.ts:67-69)
- Compared with `=== "1"`. **Not** through the `truthy()` helper at
  `flag.ts:3-6` that most other `OPENCODE_*` booleans use.
- So `OPENCODE_PRINT_LOGS=true` does **not** enable printing. Verified against the
  real binary for `1 true TRUE yes 0 ""` — only `"1"` turns the sink on.
- Printing is **additive**: `[fileLogger(), stderrLogger]`. It never replaces the
  file sink, and the sink it adds is `process.stderr`. Never stdout.

The CLI writes the flags into `process.env` before anything reads them
(`index.ts:66-69`), which is how `--print-logs` / `--log-level` win over the
environment. In Rust the same precedence is expressed as data — `LogConfig::level:
Option<LogLevel>` — with no global mutation.

### The log path

`packages/core/src/global.ts:11,23`: `log = path.join(xdgData, "opencode", "log")`,
i.e. `$XDG_DATA_HOME/opencode/log`. `oc-paths` owns this; do not re-derive it.
`oc_paths::log()` did not exist at Task 5's base commit, so `LogConfig::dir` is a
parameter (see issues.md).

### WorkerGuard lifetime — the trap every caller must know

`tracing_appender::non_blocking()` returns `(NonBlocking, WorkerGuard)`. The guard's
`Drop` shuts down and flushes the background writer thread, so **dropping the guard
stops all file logging**, silently and with no error.

`oc_observability::init()` returns a `LogHandle` that owns it. That makes the handle's
lifetime the process's logging lifetime:

```rust
let _logging = oc_observability::init(config)?;   // right: lives to end of main
let _         = oc_observability::init(config)?;  // WRONG: dropped immediately, no logs
```

`LogHandle` is `#[must_use]` with a message naming this, and `into_guard()` exists for
a caller that wants to park the guard elsewhere. Letting the handle fall out of scope
at the end of `main` is also what flushes the last records.

### tracing-subscriber facts worth not rediscovering

- **`fmt::layer()` writes to STDOUT by default.** Every layer must set `with_writer`
  explicitly. This is the single most likely way to reintroduce a stdout leak, and it
  looks like a formatting choice rather than a protocol bug.
- `FmtSpan` is **not `Copy`**. Building two layers with the same span-event config
  needs `.clone()`.
- `EnvFilter::builder().with_default_directive(level.into()).parse_lossy("")` gives a
  level-only filter; `.parse(directives)` returns a typed
  `tracing_subscriber::filter::ParseError` for a bad directive string.
- `Option<L>` implements `Layer<S>`, so an optional sink is just
  `.with(print_logs.then(|| ...))` — no boxing, no `Vec<Box<dyn Layer>>`.
- The default `fmt` format renders the whole enclosing span stack inline on every
  record: `turn{session="…" turn=0 agent="build"}:tool_call{tool="bash" call_id="…"}:`.
  That is the concrete payoff over a hand-rolled logger: an event emitted deep inside
  a tool arrives already attributed to its session and turn with nobody threading an
  id through the call graph.
- `Span::record` on a field that was **never declared** is a silent no-op. Late-bound
  fields (a model chosen after config resolution, an HTTP status) must be declared as
  `tracing::field::Empty` in the `info_span!` invocation or they vanish.
- Field rendering differs by sigil: `%value` (Display) renders unquoted
  (`tool=bash`), a `&'static str` renders Debug-quoted (`phase="pending"`). A test
  asserting on file content has to match the right one.

### Proving a stdout guarantee needs a child process

A property about a process's fd 1 cannot be asserted from inside `cargo test`:
`libtest` writes its own progress lines to the same stdout, so the captured bytes mix
protocol frames with harness chatter and the assertion has to allow-list the chatter —
which is the loophole a real leak walks through. `dup2` would work but needs `unsafe`,
which this workspace forbids. The answer is a small `src/bin/` fixture
(`oc-log-probe`) reachable from an integration test via `env!("CARGO_BIN_EXE_<name>")`.

### A textual source guard has a substring hazard

`no_stdout_in_library.rs` bans `print!` — and `print!` is a substring of `eprintln!`,
the *legitimate* stderr diagnostic. The first version of the guard therefore forbade
the very thing it wanted people to use instead. Fix: match banned tokens only at an
identifier boundary (the preceding char must not be alphanumeric or `_`). A guard that
false-positives on the correct alternative is a guard someone deletes.

## Task 6 — oc-testkit (differential harness, cassette replay, capturing mock provider)

### THE CASSETTE FORMAT (read this before touching any provider work — todos 29/30/87/94/95/96)

Root: `<oracle-tree>/packages/llm/test/fixtures/recordings/<route>/<name>.json`
Written by `@opencode-ai/http-recorder` v1.18.13. Measured across all 40 files / 52
interactions at commit `aefaf140c1`. Rust structs live in `oc_testkit::cassette`.

```json
{ "version": 1,
  "metadata": { "name": "anthropic-messages/streams-text",
                "recordedAt": "2026-04-28T21:18:45.535Z",
                "tags": ["prefix:anthropic-messages","provider:anthropic"] },
  "interactions": [
    { "transport": "http",
      "request":  { "method":"POST","url":"https://api.anthropic.com/v1/messages",
                    "headers":{"anthropic-version":"2023-06-01","content-type":"application/json"},
                    "body":"{\"model\":\"...\",\"stream\":true}" },
      "response": { "status":200,
                    "headers":{"content-type":"text/event-stream; charset=utf-8"},
                    "body":"event: message_start\ndata: {...}\n\n...",
                    "bodyEncoding":"base64" } } ] }
```

Non-obvious facts, each verified against the real files:

- **`version` is always 1.** Top-level keys are exactly `{version, metadata, interactions}`.
- **`metadata` is optional and has two shapes.** 31 files: `{name, recordedAt, tags}`.
  9 files also carry `{provider, route, transport, model}`. Deserialize as an open map,
  not a fixed struct.
- **Interactions are tagged by `transport`: `"http"` | `"websocket"`.** All 52 recorded
  ones are http. The websocket arm is real in the schema (`packages/llm/test/recorded-websocket.ts`
  uses it) but nothing is committed under it.
- **Bodies are strings, never nested JSON.** A request body is a JSON *string* containing JSON.
- **A streaming response is ONE buffered string. There is no chunk array and no timing.**
  `text/event-stream` matches the recorder's text content-type test, so the whole stream is
  drained into `body`. Event boundaries survive. Network chunk boundaries, inter-chunk delays
  and backpressure are gone and are NOT recoverable from the file. A todo that needs to assert
  on streaming *timing* must record that separately.
- **`bodyEncoding` is present only for non-text bodies, and only as `"base64"`.** Exactly 4
  occurrences, all `bedrock-converse`, content-type `application/vnd.amazon.eventstream`
  (AWS eventstream binary framing: 4-byte big-endian total length prelude). The schema also
  accepts `"text"` but the recorder never writes it — treat the field's absence as text.
- **Headers are a tiny allow-list, applied before writing.** Responses retain ONLY
  `content-type`. Requests retain `content-type`, `accept`, `openai-beta`, plus
  `anthropic-version` where the anthropic recordings allow it explicitly. `authorization`,
  `x-api-key`, `x-goog-api-key`, `cookie`, `x-amz-security-token` are DROPPED. A cassette can
  therefore never be used to assert on credential headers — assert those against `MockProvider`,
  which captures every header.
- **Every recorded request is `POST`.** Statuses: 51x 200, 1x 400 (an intentional sad path in
  `anthropic-messages/rejects-malformed-assistant-tool-order-without-patch`).
- **8 cassettes hold 2 interactions** (tool loops, cache second-call, reasoning continuation);
  the other 32 hold 1.
- **11 real endpoints are covered**: openai `/v1/responses` (10), anthropic `/v1/messages` (9),
  openrouter (8), gemini `streamGenerateContent?alt=sse` (5), openai `/v1/chat/completions` (5),
  bedrock `converse-stream` (4), groq (4), cloudflare AI gateway (2), cloudflare workers-ai (2),
  togetherai (2), deepseek (1). Cloudflare URLs contain literal `{account}`/`{gateway}`
  placeholders — the recorder redacted them, so a matcher must not expect real ids there.

**Matching (`packages/http-recorder/src/matching.ts`, ported in `cassette::canonical_snapshot`):**
- **No hashing anywhere.** The key is a canonical JSON string of `{method, url, headers, body}`.
- Object keys sorted recursively; **array order preserved**; a non-JSON body compared exactly.
- **Replay is a strict cursor, not a search.** Request *n* may only be served by interaction *n*.
  A mismatch does not advance the cursor. Finishing with interactions unconsumed is an error
  (`CassetteUnused`), as is running past the end (`CassetteExhausted`). This is what makes an
  extra/missing/reordered provider call a test failure rather than an invisible difference.
- Names may contain `/`, resolve to `<root>/<name>.json`, and must reject empty/absolute/`..`.

**Record vs replay mode**, if a new recording is ever needed: `auto` = CI set and not
`"false"`/`"0"` → replay; else cassette exists → replay; else record. Several consumer tests use
their own `RECORD=true` to force an explicit record layer — `RECORD` is *not* read by the
recorder. The recorder refuses to write a file if it still detects a secret in it.

### THE ORACLE INVOCATIONS THAT WORK

Installed release binary (the default; ~0.46 s per run):
```
$ opencode --version                     # -> 1.18.12
  which opencode -> /config/.local/share/mise/shims/opencode
  real binary    -> /config/.local/share/mise/installs/opencode/1.18.12/opencode
```

From the pinned source tree (~1.1 s per run):
```
$ bun run --conditions=browser /config/workspace/ProdDir/AI/opencode/packages/opencode/src/index.ts --version
  -> local
```
`--conditions=browser` is required (it is what `packages/opencode/package.json`'s `dev` script
uses). Any cwd works — bun resolves `node_modules` from the entry file, not the cwd — so the
harness runs it in the scripted temp project directory like the binary.

**A from-source run reports `local`, not `1.18.13`.** `packages/core/src/installation/version.ts`
reads a global `OPENCODE_VERSION` that only the bundler injects
(`packages/opencode/script/build.ts:194`) and falls back to the literal `"local"`. Setting
`OPENCODE_VERSION` in the environment does NOT help; it is a compile-time `define`. So the
from-source flavour cannot self-report a version, which is why it is not the default.

### ENVIRONMENT CONTRACT FOR SCRIPTING EITHER SIDE

`packages/core/src/global.ts:10-30` resolves paths from `xdg-basedir` at *module load*, so
`XDG_*` must be set before the process starts (there is no runtime re-read). `OPENCODE_TEST_HOME`
overrides `paths.home` and is the only one read lazily. `packages/core/src/global.ts:37-45`
eagerly `mkdir -p`s data/config/state/tmp/log/bin/repos on every start, so a scripted run always
materializes seven directories — do not treat their existence as a signal.

`ScriptedEnv` clears the environment entirely and passes through only `PATH`. It always sets
`OPENCODE_DISABLE_AUTOUPDATE=1` and `OPENCODE_DISABLE_MODELS_FETCH=1`, which is how "the harness
never makes a live call" is enforced for the *oracle's own* network use.

`packages/opencode/src/config/paths.ts` — the `.opencode` chain walks up from cwd and **stops at
the worktree root**, then always appends `$HOME/.opencode`, then `$OPENCODE_CONFIG_DIR`.
`ConfigFixture::mark_worktree_root()` writes the empty `.git` dir that stops the walk, so a
fixture chooses whether an ancestor layer is visible.

### ID SHAPE (needed by any normalizer or journal comparison)

`packages/schema/src/identifier.ts`: an id is `<prefix>_` + 12 lowercase hex (48 bits of
`timestamp*0x1000 + counter`, bitwise-inverted for descending ids) + 14 base62 characters from
`crypto.getRandomValues`. Total 26 characters after the underscore. Prefixes
(`packages/core/src/id/id.ts`): `job evt ses msg per que prt pty tool wrk`.

## Task 7 — the complete config key inventory (oracle: `packages/core/src/v1/config/config.ts:32-190`)

Read this before touching todos 8-12. Every type below was read out of the oracle,
not remembered. `Opt` = the key is optional (all of them are). "PositiveInt" is
`>= 1` and maps to `NonZeroU32`; "NonNegativeInt" is `>= 0` and maps to `u32`;
`Schema.Finite` maps to `f64`.

### 33 top-level keys as implemented in `crates/oc-config/src/schema.rs`

| key | oracle type | Rust type |
| --- | --- | --- |
| `$schema` | String | `Option<String>` (serde rename) |
| `shell` | String | `Option<String>` |
| `logLevel` | Literals DEBUG/INFO/WARN/ERROR | `Option<LogLevel>` (UPPERCASE) |
| `server` | ServerConfig | `{port: NonZeroU32, hostname, mdns: bool, mdnsDomain, cors: Vec<String>}` |
| `command` | Record<String, CommandInfo> | `OrderedMap<CommandConfig>`; `template` is **required**, then description/agent/model/variant/subtask |
| `skills` | {paths, urls} | `SkillsConfig{paths: Vec<String>, urls: Vec<String>}` |
| `references` | Record<String, Entry> | `OrderedMap<ReferenceEntry>` — **three-way union** |
| `reference` | same as `references`, @deprecated | kept (see decisions.md) |
| `watcher` | {ignore: String[]} | `WatcherConfig` |
| `snapshot` | Boolean (default true) | `Option<bool>` |
| `plugin` | Array<String \| [String, Record<String,Unknown>]> | `Vec<PluginSpec>` |
| `share` | Literals manual/auto/disabled | `ShareMode` |
| `autoupdate` | Boolean \| Literal "notify" | `Autoupdate` union |
| `disabled_providers` | String[] | `Vec<String>` |
| `enabled_providers` | String[] | `Vec<String>` |
| `model` | String (`provider/model`) | `Option<String>` |
| `small_model` | String | `Option<String>` |
| `default_agent` | String | `Option<String>` |
| `subagent_depth` | NonNegativeInt (default 1) | `Option<u32>` |
| `username` | String | `Option<String>` |
| `agent` | StructWithRest(plan/build/general/explore/title/summary/compaction, [Record<String, AgentInfo>]) | `OrderedMap<AgentConfig>` — the named keys carry **no** extra type, so a plain map is faithful |
| `provider` | Record<String, ProviderConfig> | `OrderedMap<ProviderConfig>` |
| `mcp` | Record<String, (Local\|Remote) \| {enabled: Boolean}> | `OrderedMap<McpServerConfig>` — **three** arms, not two |
| `formatter` | Boolean \| Record<String, Entry> | `FormatterConfig` union |
| `lsp` | Boolean \| Record<String, Entry> + a schema-level check | `LspConfig` union |
| `instructions` | String[] | `Vec<String>` (todo 8: concat-dedup, not last-wins) |
| `permission` | Action \| StructWithRest(15 keys, [Record<String, Rule>]) | `PermissionConfig` union |
| `tools` | Record<String, Boolean> | `OrderedMap<bool>` |
| `attachment` | {image: {auto_resize, max_width, max_height, max_base64_bytes}} | all four PositiveInt except the bool |
| `enterprise` | {url} | `EnterpriseConfig` |
| `tool_output` | {max_lines, max_bytes} PositiveInt | defaults 2000 / 51200 |
| `compaction` | {auto, prune, tail_turns, preserve_recent_tokens, reserved} | auto default true, prune default false, tail_turns default 2, three NonNegativeInt |
| `experimental` | {disable_paste_summary, batch_tool, openTelemetry, primary_tools, continue_loop_on_deny, mcp_timeout, policies} | `mcp_timeout` PositiveInt; `policies: Vec<PolicyStatement>` |

### Deliberately absent (todo 10's rejection list)
`mode` (deprecated alias of `agent`), `layout` (`auto`|`stretch`, "always uses
stretch"), `autoshare`, and agent-level `tools` / `maxSteps`. All five DO exist in
the oracle; they are omitted here so the rejection pass has something to catch.

### Nested vocabularies worth not re-deriving

* **Reference entry** (`config/reference.ts:5-21`): `String` | `{repository, branch?,
  description?, hidden?}` | `{path, description?, hidden?}`. `repository` and `path`
  are the disjoint discriminators.
* **Agent** (`config/agent.ts:12-41`): model, variant, temperature, top_p, prompt,
  **tools (dep)**, disable, description, mode(subagent|primary|all), hidden,
  options, color, steps(PositiveInt), **maxSteps (dep)**, permission — plus a rest
  record. `color` is `/^#[0-9a-fA-F]{6}$/` **or** one of
  primary/secondary/accent/success/warning/error/info.
* **Agent KNOWN_KEYS** (`:43-60`) has **16** entries and includes `name`, which is
  *not* a schema field. Consequence: `name` must NOT be swept into `options`.
* **Permission** (`config/permission.ts:18-34`): read, edit, glob, grep, list, bash,
  task, external_directory, lsp, skill take `Action | Record<String, Action>`;
  todowrite, question, webfetch, websearch, doom_loop take a **bare Action only**.
  A bare string at the top of `permission` decodes to `{"*": action}` (`:40-41`).
* **Provider** (`config/provider.ts:82-126`): api, name, env[], id, npm, whitelist[],
  blacklist[], options(StructWithRest), models. Options names apiKey, baseURL,
  enterpriseUrl, setCacheKey, timeout(PositiveInt|false), headerTimeout(same),
  chunkTimeout(PositiveInt) and passes everything else through.
* **Model** (`:13-80`): id, name, family, release_date, attachment, reasoning,
  temperature, tool_call, interleaved(bool|String|{field}), cost{input!, output!,
  cache_read?, cache_write?, context_over_200k{...}}, limit{context!, input?,
  output!}, modalities{input[],output[]} over text/audio/image/video/pdf,
  experimental, status(alpha|beta|deprecated|active), provider{npm,api}, options,
  headers, variants(Record<String, StructWithRest({disabled?})>).
* **MCP** (`config/mcp.ts:6-62`): local{type,command!,cwd,environment,enabled,timeout},
  remote{type,url!,enabled,headers,oauth(OAuth|false),timeout},
  oauth{clientId,clientSecret,scope,callbackPort(1..=65535),redirectUri}.
* **LSP** (`config/lsp.ts:5-78`): entry is `{disabled: true}` | `{command!, extensions?,
  disabled?, env?, initialization?}`, and the union carries a schema-level check —
  a server id outside the **39** builtin ids must declare `extensions`. That id list
  is copied verbatim into `schema::lsp::BUILTIN_SERVER_IDS`.
* **Policy** (`packages/core/src/policy.ts:11-15` + `config/experimental.ts:9-14`):
  `{action, effect, resource}`, all **required**; `effect` is allow|deny and `action`
  is the single literal `"provider.use"` (from `Catalog.PolicyActions`).

### Effect Schema constructs that do not map cleanly onto serde

1. **`StructWithRest(A, [Record(String, X)])`** = named fields plus a typed
   catch-all. serde's `#[serde(flatten)] map` is the equivalent, but it cannot
   coexist with `deny_unknown_fields` on the same struct — which is fine, because
   the oracle never denies where it writes a rest.
2. **Unknown-key policy is per level, not global.** Top level: **hard error** —
   `packages/opencode/src/config/parse.ts:40-53` runs its own `topLevelExtraKeys`
   scan and throws `unrecognized_keys` *before* decoding. Nested: **silently
   dropped**, because Effect's default `onExcessProperty` is `"ignore"`. Anything
   stricter than that at a nested level would reject configs the real binary accepts.
3. **`Schema.decodeTo(..., transform)`** (agent `normalize`, permission
   `normalizeInput`) means decoding is not a pure projection — it rewrites the
   value. The agent sweep is reproduced; the permission `"*"` expansion is offered
   as a method instead, so the parsed value still records which form was written.
4. **`Schema.Literal(false)`** needs a dedicated type; `bool` would accept `true`.
   `schema::ordered::False` covers the two places it appears.
5. **`Schema.Finite`** is a JS number, with no int/float distinction. `f64` is the
   consequence, and re-serializing `272000` yields `272000.0`.
6. **`PositiveInt` / `Int.isBetween(1, 65535)`** map exactly onto `NonZeroU32` /
   `NonZeroU16` — no hand-written validator needed, and the error message is decent.
7. **`propertyOrder: "original"`** (`config/parse.ts:55`) has no serde equivalent
   and no `serde_json`/`indexmap` equivalent in this workspace. See issues.md — this
   is the single most dangerous finding for todos 8 and 17.

### `packages/web/src/content/docs/config.mdx` has 36 titled JSON blocks, not 33
Three are titled `tui.json` and are the **TUI's own** config file — `theme`,
`keybinds`, `attention`, `diff_style`, `mouse`, `scroll_acceleration`,
`scroll_speed` are NOT `opencode.json` keys. Do not add them to `Config`.
## Task 8
- Verified ascending precedence from the 1.18.13 oracle: global config.json -> opencode.json -> opencode.jsonc (config.ts:246-260); OPENCODE_CONFIG (401-404); ancestor project opencode.json(c), outermost first and nearest last (406-410 plus paths.ts:10-21); config directories in Global.Path.config -> project .opencode nearest-first -> project ancestors -> HOME/.opencode -> OPENCODE_CONFIG_DIR order, with opencode.json before opencode.jsonc in each eligible directory (config.ts:416-465 plus paths.ts:23-40); OPENCODE_CONFIG_CONTENT (468-475); system managed opencode.json then opencode.jsonc (516-522); macOS managed preferences last and therefore overriding all config sources (524-534); OPENCODE_PERMISSION after all sources (545-551).
- Post-processing verified from config.ts:553-584: tools produce permission defaults before explicit permission wins; username gets a fallback; disable-autocompact and disable-prune flags patch compaction.
- instructions is the sole array exception: mergeConfigConcatArrays at config.ts:45-49 computes Array.from(new Set([...target.instructions, ...source.instructions])). Earlier/lower-precedence entries stay first, source entries append, and the first occurrence wins de-duplication. Every other array follows mergeDeep replacement semantics.
- File-backed layers without $schema receive https://opencode.ai/config.json, matching config.ts:231-235; inline and managed-preference virtual sources do not trigger file mutation.


## Task 9 — `{env:VAR}` / `{file:path}` substitution (`oc-config::variable`)

Oracle: `packages/opencode/src/config/variable.ts:33-90`. Every rule below was
established by **executing** the TypeScript module under bun 1.3.14, not by
reading it. Probe transcripts: `.omo/evidence/task-9-opencode-rust.txt`.

### The comment-skip rule, exactly (Todo 8 and Todo 12 must agree with this)

Applies to `{file:...}` **only**. For a token at byte offset `i` in the
already-env-expanded text: `line_start` = index of the last `\n` before `i`, plus
one, else 0. Skip iff `js_trim_start(text[line_start..i])` starts with `//`. A
skipped token is emitted verbatim and **its file is never read**, so a missing
file inside a comment cannot fail the load.

| text | skipped? |
| --- | --- |
| `  // {file:x}` | yes |
| `/// {file:x}` | yes |
| `\u{feff}// {file:x}` | yes — `trimStart` eats a BOM |
| `\u{a0}// {file:x}` | yes |
| CRLF comment line | yes |
| `// {file:a} and {file:b}` | yes, **both** |
| `{"a":1} // {file:x}` | **no** — a trailing comment is not a comment line |
| `{"a":"// {file:x}"}` | **no** — `//` inside a JSON string value |
| `/* {file:x} */` | **no** — block comments are not recognized at all |
| `{\r  // {file:x}\r}` | **no** — `lastIndexOf("\n")` ignores a lone CR |
| ` / {file:x}` | **no** — one slash is not a comment |

### `{env:...}` IS substituted inside comment lines

The env pass is one unconditional `String.replace` over the whole text with no
comment check whatsoever; only the file pass has one. Measured: `//  {env:FOO}`,
`{"a":1} // {env:FOO}` and `/* {env:FOO} */` all expand. The clearest case is
`// {env:OC_X}{file:./rel.md}` → `// E{file:./rel.md}` — env expanded, file
skipped, on the same line.

**Consequence for Todo 8/12:** do not implement "skip tokens in comments" as one
rule. It is `{file:}`-only. Anything that strips JSONC comments *before*
substitution would also change behaviour, because the oracle strips comments
*after* (`config.ts:220-227`: substitute → `ConfigParse.jsonc` → schema).

### Escape rule

`content = js_trim(lossy_utf8(bytes))`, emitted as `JSON.stringify(content)[1..-1]`
— the **body** of a JSON string with no surrounding quotes, dropped between quotes
that are already in the document. Escapes: `\"` `\\` `\b`(08) `\t`(09) `\n`(0A)
`\f`(0C) `\r`(0D), and any other `c < 0x20` as `\u00xx` **lowercase** hex.
Not escaped: `/`, U+007F (DEL), U+0085, all non-ASCII. Identical to
`serde_json::to_string(s)[1..len-1]`, and a proptest in `variable.rs` proves the
equality for arbitrary strings — reuse that trick rather than re-deriving the table.

### Trim set is `String.prototype.trim`, **not** `char::is_whitespace`

`09 0B 0C 20 A0 FEFF 1680 2000-200A 202F 205F 3000 0A 0D 2028 2029`. Two deltas
against Rust, both observable in real files:

* **U+FEFF** — JS trims it, Unicode `White_Space` does not. A prompt file saved
  with a BOM loses it, and `\u{feff}// {file:x}` **is** a comment line.
* **U+0085** (NEL) — Unicode `White_Space` does, JS does not, so it survives at
  the edges where `str::trim` would have eaten it.

Anywhere else in this port that mirrors a JS `.trim()` / `.trimStart()` has the
same hazard. `is_js_whitespace` in `oc-config::variable` is the reference set.

### Path resolution — three shapes, and the oracle is deliberately inconsistent

* `~/rest` → `path.join(os.homedir(), rest)`, which **normalizes**.
  `{file:~/../x}` with home `/config` reports `/x`.
* absolute → used **verbatim, unnormalized**. `{file:/a/../b}` reports `/a/../b`
  and the *kernel* resolves the `..`, so it is symlink-aware — unlike a textual
  `path.resolve`. Normalizing it here would change both the reported path and,
  under symlinks, which file is read.
* anything else → `path.resolve(dirname(configFile), spec)`, which normalizes.
  The base is the **config file's directory**, never `process.cwd()`.

A bare `~` is not special: `{file:~}` resolves to `<configdir>/~`.
`os.homedir()` is the real home — `OPENCODE_TEST_HOME` does **not** reach
`variable.ts`, so `oc_paths::home()` (which honours it) is the wrong function here.
With no usable `HOME`, `path.join("", "x") == "x"`, so a `~/` reference silently
degrades to config-relative rather than failing.

### Token grammar is two regexes and nothing more

`/\{env:([^}]+)\}/g` and `/\{file:[^}]+\}/g`.

* `{env:}` / `{file:}` are **not** tokens (`[^}]+` needs a character) — left literal.
* Unterminated `{env:FOO` / `{file:x` — left literal.
* A token may span a line break, a quote, anything but `}`. `{"a":"{env:FOO"}`
  matches `{env:FOO"}` with the variable name `FOO"` and yields `{"a":"`.
* `{env:{env:A}}` resolves the variable literally named `{env:A`, then a stray `}`.
* Missing env var → **empty string** (`|| ""`), never an error, never the token.
* The env pass runs first over the whole text, so an env value can supply a whole
  `{file:}` token or part of a file path. A file **body** is not rescanned.
* Read failures: ENOENT → `bad file reference: "<token>" <resolved> does not exist`;
  any other failure (e.g. a directory) → `bad file reference: "<token>"` with no
  suffix. `missing: "empty"` swallows **every** read failure, not just absence.

## Orchestrator — plan corrected by Todo 9's investigation (comment-skip asymmetry)

The plan's Todo 9 said "tokens appearing inside comment lines are not
substituted". That is true for only one of the two token forms. Verified in
`packages/opencode/src/config/variable.ts`:

- `:36-38` — `{env:VAR}` is a single blanket `text.replace(/\{env:([^}]+)\}/g, ...)`
  with **no line inspection at all**. Env tokens inside `//` comments ARE
  substituted. Missing variable → `|| ""`, i.e. empty string, not an error.
- `:47-61` — `{file:path}` iterates matches and, for each, checks
  `text.slice(lineStart, index).trimStart().startsWith("//")`; if so the token is
  emitted verbatim and skipped. Only file tokens honour comments.
- `:87` — file content is `JSON.stringify(content).slice(1, -1)`: escaped but with
  the surrounding quotes stripped, so it drops into an existing JSON string.
- Order matters: the env pass runs to completion first, so a file path can be
  built from an env variable (`{file:{env:HOME}/x.md}` works).

Todo 9's implementation encodes the asymmetry with a test per side
(`env_tokens_in_a_comment_line_are_substituted_too`,
`file_tokens_on_a_comment_line_are_left_untouched`). The plan text has been
corrected to match the oracle. Todo 8's JSONC parsing and Todo 12's differential
must not "fix" this asymmetry — it is the compatible behaviour.

## Orchestrator — plan corrected by Todo 9's investigation (comment-skip asymmetry)

The plan's Todo 9 said "tokens appearing inside comment lines are not
substituted". True for only ONE of the two forms. Verified in
`packages/opencode/src/config/variable.ts`:

- `:36-38` — `{env:VAR}` is a single blanket `text.replace(/\{env:([^}]+)\}/g, ...)`
  with **no line inspection**. Env tokens inside `//` comments ARE substituted.
  Missing variable → `|| ""`, i.e. empty string, never an error.
- `:47-61` — `{file:path}` iterates matches and per match checks
  `text.slice(lineStart, index).trimStart().startsWith("//")`; if so the token is
  emitted verbatim. Only file tokens honour comments.
- `:87` — content is `JSON.stringify(content).slice(1, -1)`: escaped, quotes
  stripped, so it drops into an existing JSON string.
- Order matters: the env pass completes first, so a file path can be built from an
  env variable.

Todo 9 encoded the asymmetry with a test per side
(`env_tokens_in_a_comment_line_are_substituted_too`,
`file_tokens_on_a_comment_line_are_left_untouched`). The plan text is corrected.
**Todo 12's differential must not "fix" this asymmetry — it is the compatible
behaviour.**

## Task 11

Oracle read in full: `packages/opencode/src/session/instruction.ts` (237 lines), the walk/glob primitives at `packages/core/src/fs-util.ts:147-198`, and the upstream behavioural spec `packages/opencode/test/session/instruction.test.ts` (264 lines). Every rule below is quoted from those, not from the task summary — and two of them contradict the summary.

### The filename cascade: "first class" means first class over the WHOLE ancestor range, not per level

`instruction.ts:123-133`:

```
for (const file of instructionFiles) {
  const matches = yield* fs.findUp(file, ctx.directory, ctx.worktree)
  if (matches.length > 0) { matches.forEach((i) => paths.add(path.resolve(i))); break }
}
```

`findUp` (`fs-util.ts:154-166`) is **not** a first-hit search: it walks `start` upward, pushes `join(current, target)` for *every* level where it exists, and checks `stop === current` **after** searching, so `stop` (the worktree) is inclusive. It returns all matches.

Exact rule, verbatim: *for each filename class in order (`AGENTS.md`, then `CLAUDE.md` when Claude compatibility is on, then upstream's `CONTEXT.md`), scan the entire ancestor range `directory ..= worktree`; the first class with at least one hit anywhere in that range wins; all of its hits at every level are loaded; no later class is looked for at all.*

Two consequences the "per level" reading gets backwards:
1. An `AGENTS.md` at a **deep** level suppresses a `CLAUDE.md` at a **shallower** level. The decision is never per level.
2. Within the winning class, ancestor levels **do** stack — `a/b/AGENTS.md` and `a/AGENTS.md` are both loaded, deepest first. **The task prompt's "do not stack every ancestor level" is wrong as written.** Upstream's own comment ("so we don't stack AGENTS.md/CLAUDE.md from every ancestor", `:122`) is about not stacking different *filename classes*; `matches.forEach(...)` at `:129` provably stacks levels of the winning class.

`Instruction.find` (`:171-177`) is the *other* shape and the only per-level one: one directory, first class present wins, returns a single path. That is what the upward append uses.

### Global is ALSO first-wins — at most ONE global file is ever loaded

`instruction.ts:115-120` loops `globalFiles = [$CONFIG/AGENTS.md, $HOME/.claude/CLAUDE.md]` (`:60-63`) and `break`s on the first that exists. **The task prompt's "Global: `$CONFIG/AGENTS.md`, then optional `~/.claude/CLAUDE.md`" reads as if both load; they do not.** If `$CONFIG/AGENTS.md` exists, `~/.claude/CLAUDE.md` is never stat'd. Confirmed by the upstream test at `instruction.test.ts:213-230`, which gets exactly 2 blocks (one global + one project) with `rules[0]` global and `rules[1]` project — global first in output order.

`disableClaudeCodePrompt` removes `CLAUDE.md` from **both** lists (`:62` and `:66`), verified by `instruction.test.ts:232-248`.

### Upward append: ordering and the four dedup rules

`instruction.ts:179-221`. Root is `InstanceState.directory` (not worktree). `current = dirname(target)`, loop bound `while (current.startsWith(root) && current !== root)` — so **the root directory itself is excluded**; that is not a gap, because root's own instruction file is already in `systemPaths` and would be filtered anyway.

Ordering: entries are `push`ed (`:214`), so parent instructions are appended **at the end**, deepest ancestor first, shallowest last.

Four things make an entry "the same" and skip it — `:196` and `:206`: (1) `found === target`, the file being read; (2) `sys.has(found)`, already in `systemPaths`; (3) `already.has(found)`, an earlier `read` tool call reported it in `state.metadata.loaded` (`extract`, `:17-32`); (4) the per-`messageID` claim set already holds it. Rule (4) is what makes a parent `AGENTS.md` attach **once**, not once per level and not once per read. `Instruction.clear` (`:105-108`) drops the claim set. All four verified by `instruction.test.ts:115-207`.

Note this is a *different* dedup key from Todo 8's merged-array dedup: Todo 8 dedups `instructions[]` by **raw string** before resolution (`discovery_instructions_keep_earlier_entries_first_and_deduplicate`); the loader dedups by **resolved absolute path** afterwards, in one insertion-ordered set shared by global + project + glob results (`instruction.ts:113`, a JS `Set`).

### Concurrency and timeout, with oracle lines

`instruction.ts:162` local reads `{ concurrency: 8 }`; `:163` remote fetches `{ concurrency: 4 }`; `:97` `Effect.timeout(5000)` per fetch. Failure handling: `:92` read errors catch to `""`, `:98` fetch errors/timeouts catch to `null`, `:100` null body becomes `""`, and `:166-167` drop empty strings from the output. So a failed or empty instruction is silently absent and **never** aborts the load.

One upstream weakness worth knowing: the 5s bound covers only `http.execute` (headers). The body read at `:101` is unbounded, so a server that answers headers and then stalls the body hangs upstream forever. "Abandoned at 5s" is only true if the whole operation is bounded.

### instructions[] resolution, three shapes

`instruction.ts:135-150`. URLs (`http://`/`https://`, case-sensitive prefix match, `:137`/`:158-160`) are skipped in the path pass and fetched separately. `~/` is rewritten as `join(global.home, raw.slice(2))` (`:138`). Then:
- **absolute** entry → `glob(basename(instruction), { cwd: dirname(instruction), absolute: true, include: "file" })` (`:141-145`) — note it globs only the **basename**, and passes **no `dot` option**, so dotfiles are excluded;
- **relative** entry → `globUp(instruction, ctx.directory, ctx.worktree)` (`:83`), which globs at every ancestor level with `dot: true` (`fs-util.ts:188`) and inclusive `stop`;
- with `OPENCODE_DISABLE_PROJECT_CONFIG`, the relative branch instead globs `globUp(instruction, global.config, global.config)` (`:87`) and the whole project cascade is skipped (`:123`).

Output block format, exact: `Instructions from: {source}\n{content}` (`:166-167`, and `:214` for the append). Output order: global, then project cascade deepest-first, then `instructions[]` glob matches in config order, then remote URLs in config order.

## Task 10 — the ten deprecated config forms, verified in the oracle

Every form below was confirmed by reading the oracle tree at
`/config/workspace/ProdDir/AI/opencode`. None of the plan's ten turned out to be
mis-classified; all ten are genuinely deprecated upstream.

| # | Form | Oracle proof | Already covered? |
|---|---|---|---|
| 1 | `mode.<name>` | `packages/core/src/v1/config/config.ts:95` — ``@deprecated Use `agent` field instead.``; normalized at `packages/opencode/src/config/config.ts:536-543` (spreads each entry into `agent` and forces `mode:"primary"`) | **Todo 7 rejected it** as `unrecognized key` (not a `Config` field + `deny_unknown_fields`); Task 10 supplies the actionable message |
| 2 | a `{mode,modes}/` agent directory | `packages/opencode/src/config/agent.ts:32-58` — `loadMode` globs `{mode,modes}/*.md` and forces `mode:"primary"`; the modern loader at `:11-30` globs `{agent,agents}/**/*.md` | **new in Task 10** |
| 3 | agent `tools: {..}` | `packages/core/src/v1/config/agent.ts:68-77` — folds each entry into `permission`, with `write`/`edit`/`patch` **all collapsing to `permission.edit`** | **new in Task 10.** Todo 7 only kept it out of the provider-options sweep (`SWEEP_EXEMPT_KEYS`); nothing rejected it |
| 4 | agent `maxSteps` | `packages/core/src/v1/config/agent.ts:79` — `const steps = agent.steps ?? agent.maxSteps` | **new in Task 10** (same as above: exempted from the sweep, never rejected) |
| 5 | `layout` | `packages/core/src/v1/config/config.ts:127` — `@deprecated Always uses stretch layout.` | **Todo 7 rejected it**; Task 10 supplies the message |
| 6 | `autoshare` | `packages/core/src/v1/config/config.ts:61-63` — ``@deprecated Use 'share' field instead.``; applied at `packages/opencode/src/config/config.ts:578-580` (`autoshare === true` → `share: "auto"`) | **Todo 7 rejected it**; Task 10 supplies the message |
| 7 | `CONTEXT.md` | `packages/opencode/src/session/instruction.ts:68` — the oracle's own trailing comment is literally `// deprecated` | **new in Task 10** |
| 8 | a global TOML `config` file | `packages/opencode/src/config/config.ts:262-275` — `existsSync(path.join(Global.Path.config, "config"))`, imported `with { type: "toml" }`, then **written to `config.json` and `unlink`ed** | **new in Task 10** |
| 9 | `reference` (singular) | `packages/core/src/v1/config/config.ts:48-50` — ``@deprecated Use 'references' field instead.`` | **new in Task 10.** Todo 7 deliberately *accepted* it (it is in `KNOWN_TOP_LEVEL_KEYS` with a doc comment saying it is "not on the legacy-rejection list"). The plan does list it, so Task 10 rejects it in the legacy pass while the schema keeps parsing it — see decisions.md |
| 10 | auth-prompt `condition` | `packages/plugin/src/index.ts:102-103` (text prompt) and `:115-116` (select prompt) — ``/** @deprecated Use `when` instead */`` on both shapes | **new in Task 10** |

Score: **3 of 10 already rejected** by Todo 7 (`mode`, `layout`, `autoshare` — all
three as top-level unknown keys, message `unrecognized key`), **7 newly detected**
here.

Two further oracle facts worth keeping:

* Top-level `tools` is **not** deprecated (`core/v1/config/config.ts:129`, folded
  into `permission` at `opencode/config/config.ts:565-576`). Only the **agent-level**
  `tools` is. Rejecting the top-level one would be a false positive.
* `mode` inside an *agent* definition (`agent.build.mode = "primary"`) is the
  modern, correct key (`core/v1/config/agent.ts:26`). Only the **top-level** `mode`
  map is deprecated. The two are one character apart in a config file and easy to
  conflate.

## Task 11 — instruction discovery and the `instructions[]` loader

### The filename cascade: "first class wins" means the first *filename*, not the first *file*

The plan's prose ("stop at the first filename class found — do not stack every
ancestor level") reads as if only one file is ever loaded. **The oracle does not
do that**, and the difference is observable on any monorepo.
`packages/opencode/src/session/instruction.ts:122-131`:

```ts
// The first project-level match wins so we don't stack AGENTS.md/CLAUDE.md from every ancestor.
if (!Flag.OPENCODE_DISABLE_PROJECT_CONFIG) {
  for (const file of instructionFiles) {
    const matches = yield* fs.findUp(file, ctx.directory, ctx.worktree)   // :124
    if (matches.length > 0) {
      matches.forEach((item) => paths.add(path.resolve(item)))            // :128
      break                                                              // :129
    }
  }
}
```

`findUp` (`packages/core/src/fs-util.ts:154-166`) returns **every** ancestor level
that holds `target`, nearest first. So the `break` only stops the loop from
trying the *next filename*; all levels of the winning filename are added. The
oracle's own comment is about not mixing `AGENTS.md` with `CLAUDE.md`, not about
collapsing the levels. Concretely, with `directory=repo/sub`, `worktree=repo`:

| tree | loaded |
| --- | --- |
| `repo/AGENTS.md` + `repo/sub/AGENTS.md` | **both** |
| `repo/AGENTS.md` + `repo/sub/CLAUDE.md` | only `repo/AGENTS.md` (a nearer `CLAUDE.md` loses to a further `AGENTS.md`) |
| `repo/CLAUDE.md` + `repo/sub/CLAUDE.md` | **both** |
| `repo/AGENTS.md` + `repo/CLAUDE.md` (same dir) | only `AGENTS.md` |

Reproduces the Todo 9 lesson: the plan prose was imprecise, the oracle decided it.
Tests `the_first_filename_class_wins_and_claims_every_level` and
`a_nearer_claude_md_does_not_beat_a_further_agents_md` pin all four rows.

**The global probe is a different rule.** `instruction.ts:115-120` loops over
`globalFiles` and `break`s on the first that *exists*, so **at most one** global
file is ever loaded — `$CONFIG/AGENTS.md`, else `~/.claude/CLAUDE.md`, never both.
Two "first wins" rules, opposite shapes; do not merge them.

### The flag that disables Claude compatibility

`flags.disableClaudeCodePrompt` (`instruction.ts:62` and `:66`), defined in
`packages/opencode/src/effect/runtime-flags.ts:23-26` as the OR of two variables:

- `OPENCODE_DISABLE_CLAUDE_CODE` (broad — also gates `disableClaudeCodeSkills`)
- `OPENCODE_DISABLE_CLAUDE_CODE_PROMPT` (targeted)

Either one drops `~/.claude/CLAUDE.md` from the global candidates **and**
`CLAUDE.md` from the project cascade. Both are exported as public constants.

### How the upward append de-duplicates

`Instruction.resolve` (`instruction.ts:179-220`) is a *separate* mechanism from
the cascade, triggered when a file is read mid-session. It walks up from
`dirname(target)` and calls `find(dir)` — the first cascade filename **in that
one directory**, no walk — then applies **four** independent skips (`:186-206`):

1. `found === target` — never attach the file being read;
2. `sys.has(found)` — the system set already paid for it;
3. `already.has(found)` — a completed `read` tool call this session already
   loaded it (`extract()` at `:17-31` mines `part.state.metadata.loaded`,
   skipping compacted parts);
4. `set.has(found)` where `set = claims.get(messageID)` — this assistant message
   already attached it.

Guard 4 is the per-message ledger (`state.claims`, `:74`, cleared by
`Instruction.clear`, `:105-108`). Reading `pkg/src/main.rs` then `pkg/src/lib.rs`
in one message attaches `pkg/AGENTS.md` **once** — that is what makes it exactly
once rather than once per file read. This port hands the ledger to the caller as
`UpwardClaims` so the property is assertable instead of hidden in service state.

The loop bound is `current.startsWith(root) && current !== root` (`:187`) where
`root = resolve(InstanceState.directory)` — the **directory**, not the worktree,
and a **string prefix**, not an ancestry test. `/repo` therefore treats
`/repo-vendor` as inside it. Reproduced deliberately; a "fix" here would be a
differential divergence.

### Bun glob semantics that `globset` does not give you for free

- `*` must not cross `/` → `GlobBuilder::literal_separator(true)`. The default
  lets `*.md` match `docs/nested/x.md`.
- `globUp` runs the pattern once **per ancestor directory up to `/`**
  (`fs-util.ts:184-199`). A recursive scan for a literal `AGENTS.md` would walk
  every sibling of every ancestor — near `/` that is the whole disk. A literal
  pattern must short-circuit to an existence check.
- `globUp` passes `dot: true`, but the absolute-entry branch (`instruction.ts:141-146`)
  does not, so a dotfile-hiding rule is needed for the latter only.

## Task 10 — the ten deprecated forms, each verified against the oracle

Every form below was re-verified in `/config/workspace/ProdDir/AI/opencode` rather
than taken from the plan. Two of the plan's claims were wrong; both are flagged.

| # | Deprecated form | Modern replacement | Oracle proof | Detectable from config text? | Test |
|---|---|---|---|---|---|
| 1 | top-level `mode.<name>` | `agent.<name>` with `mode: "primary"` | schema `core/src/v1/config/config.ts:90-92` (`@deprecated Use \`agent\` field instead.`); normalization `opencode/src/config/config.ts:536-543` | yes | `mode_block_names_the_agent_replacement` |
| 2 | a `{mode,modes}/` agent directory | `agent/` (or `agents/`) | `opencode/src/config/agent.ts:37` globs `{mode,modes}/*.md`; the modern loader at `:13` globs `{agent,agents}/**/*.md` | **no — filesystem fact** | `mode_directory_names_the_agent_directory`, `the_plural_modes_directory_is_rejected_too` |
| 3 | agent `tools: {..}` | `permission` | schema `core/src/v1/config/agent.ts:21-23`; conversion `:68-77` | yes | `agent_tools_names_permission` |
| 4 | agent `maxSteps` | `steps` | schema `core/src/v1/config/agent.ts:37`; fallback `:79` (`agent.steps ?? agent.maxSteps`) | yes | `agent_max_steps_names_steps` |
| 5 | top-level `layout` | removed, no replacement | `core/src/v1/config/config.ts:127` — `@deprecated Always uses stretch layout.` | yes | `layout_is_reported_as_removed` |
| 6 | top-level `autoshare` | `share` (`true` → `"auto"`) | schema `core/src/v1/config/config.ts:61-63`; runtime `opencode/src/config/config.ts:575-577`; migration `core/src/v1/config/migrate.ts:42` | yes | `autoshare_names_share` |
| 7 | a discovered `CONTEXT.md` | `AGENTS.md` | `opencode/src/session/instruction.ts:64-68`, with the oracle's own `// deprecated`; first-class-wins at `:124-132` | **no — filesystem fact** | `context_file_names_agents_md` |
| 8 | a global TOML `config` file | `config.json` | `opencode/src/config/config.ts:262-275` | **no — filesystem fact** | `toml_config_names_config_json` |
| 9 | top-level `reference` | `references` | schema `core/src/v1/config/config.ts:47-49`; fallback `core/src/v1/config/migrate.ts:65` (`info.references ?? info.reference`) | yes | `reference_names_references` |
| 10 | auth-prompt `condition` | `when`, a `{ key, op, value }` rule | type `plugin/src/index.ts:102-104`, repeated at `:115-117`, `:131-133`, `:145-147`; evaluated `opencode/src/cli/cmd/providers.ts:68-77` | **no — runtime JS closure** | `auth_prompt_condition_names_when` |

### Corrections to the plan's description of these forms

- **Form 8 is not `config.toml`.** `config.ts:262` joins the global config dir with
  the bare literal `"config"` — no extension — and parses it with a dynamic
  `import(..., { with: { type: "toml" } })`. A detector that looked for
  `config.toml` would find nothing. `LEGACY_TOML_CONFIG_FILE = "config"`.
- **Form 10 is not a value a config file can hold.** `condition` is typed
  `(inputs: Record<string, string>) => boolean` — a closure — while its replacement
  `when` is a static `Rule` object. A closure has no JSON encoding, so scanning
  config text for `condition` proves nothing about whether a plugin uses it. It is
  also *not* a config key at all: it is a field of
  `AuthHook.methods[].prompts[]`, read only during `auth login`.
- **Form 2's globs differ in depth, not just in name.** The legacy glob
  `{mode,modes}/*.md` is flat; the modern `{agent,agents}/**/*.md` recurses. So an
  `.md` nested two levels under `mode/` was never loaded by the oracle either, and
  rejecting it would be a false positive. `inspect_directory` therefore reports only
  `*.md` directly inside `mode/` or `modes/`.
- **Form 1 merges, it does not overwrite.** `mergeDeep(result.agent ?? {}, {[name]: {...mode, mode: "primary"}})`
  spreads the legacy entry *into* `agent`, and the spread order means a `mode`
  field inside the legacy block is overwritten by `"primary"`. Because the whole
  entry is spread verbatim, a `mode.build.maxSteps` is **two** deprecated inputs;
  `inspect_config` scans `mode.*` for agent-level forms as well as `agent.*`, so the
  author is not sent round the loop twice.
- **Form 3's collapse is lossy and worth saying in the message.** `write`, `edit`,
  and `patch` all map onto the single `permission.edit` key
  (`core/src/v1/config/agent.ts:71-74`), so `tools: {write: false, patch: true}`
  cannot be expressed as two rules. `true` → `"allow"`, `false` → `"deny"`.
- **Forms 4, 5, 6, 9 are not normalized in `config.ts:545-584`.** That block only
  handles `OPENCODE_PERMISSION`, top-level `tools`, `username`, `autoshare`, and the
  compaction flags. `maxSteps` is resolved in the agent decoder, and `reference` in
  the v1 migration. Citing `:545-584` for all of them would have been wrong.

## Wave 15 research — hermes-agent memory architecture (source of the added scope)

Source: `NousResearch/hermes-agent` @ `a6e1e270b1103cc026275419a21ba9b5f581f96b`, cloned to
`.omo/refs/hermes-agent/`. Candidate identification was real work: `NousResearch/Hermes`
is **404**; the agent lives at `NousResearch/hermes-agent` ("The agent that grows with
you", 225k stars, active 2026-08-05). `letta-ai/letta` (MemGPT) exists and has
core/recall/archival tiers, but hermes deliberately does NOT copy that split — and its
divergence is the interesting part.

**Storage — two unrelated things, not one system.**
- Curated memory = two Markdown files, `$HERMES_HOME/memories/{MEMORY.md,USER.md}`
  (`tools/memory_tool.py:53`), entries split by `"\n§\n"` (`:67`), capped in
  **characters** (2200 + 1375, `:165`) because "char counts are model-independent"
  (`:22`). Config records the equivalence: ~800 + ~500 tokens
  (`cli-config.yaml.example:691-693`). **Total resident budget ≈ 1300 tokens.** That cap
  IS the design; an uncapped store becomes a log the model ignores.
- Session archive = SQLite schema v25 (`hermes_state_common.py:155`) with
  `messages.active` / `.compacted` flags — compacted messages are **marked, never
  deleted** (`:185`), so the archive stays complete. Retrieval is **FTS5
  external-content** (`:403`) plus a **trigram** table for CJK (`:467`) built excluding
  `role='tool'`, because the trigram index costs ~2.6x the text it covers while tool rows
  are ~90% of bytes and near-pure noise (`:456-466`).

**No vector store in the official path.** Even the bundled `plugins/memory/holographic`
is SQLite+FTS5 with `auto_extract: false` by default. LanceDB / mem0 / supermemory are
external plugins, and `MemoryManager` allows only **one** at a time to avoid tool-schema
bloat (`agent/memory_provider.py:6-9`).

**Write path — three independent mechanisms.**
1. Explicit `memory` tool (`tools/memory_tool.py:1152`). Three properties worth copying
   verbatim: only `add`/`replace`/`remove`; `replace`/`remove` locate by **short unique
   substring**, not ID (no ID bookkeeping for the model); **batch is atomic and the cap is
   checked only on the FINAL result**, which is the only way "it's full" has a solution —
   one call can delete stale entries and add a new one. Its SKIP list explicitly excludes
   task progress / completed-work logs / TODO state, pointing at session_search instead:
   the guardrail that stops memory degenerating into a log.
2. Every-N-turns nudge, default 10 (`agent/turn_context.py:584`).
3. **Background review fork** — the actual innovation
   (`agent/background_review.py:1-17`): "Main conversation and prompt cache are never
   touched." Spawned only after the response is delivered
   (`agent/turn_finalizer.py:714-724`, guarded on `final_response and not interrupted`,
   wrapped in `except Exception: pass`), with a **tool whitelist** of memory/skill only and
   a runtime deny message (`:893-909`), and `compression_enabled = False` (`:881`).

**Read path — the frozen snapshot is the key mechanism.** Both files are injected into the
system prompt **as a snapshot taken at session start**; mid-session writes hit disk
immediately but do NOT change the prompt (`tools/memory_tool.py:12-16`, `:682`). This
single choice buys three things: prefix cache survives the session, the prompt is
byte-stable, and **memory can never be summarized away because it was never in the message
stream**. The compaction path re-verifies that the *currently rendered* blocks appear
verbatim in the cached prompt and that no stale header remains
(`agent/conversation_compression.py:211`) — comparing rendered-vs-cached, not
snapshot-vs-snapshot, because the latter locks stale memory for a whole session in any
fresh-agent path. Injected block carries a live usage header:
`MEMORY (your personal notes) [63% — 1,390/2,200 chars]` (`:731`).

**Consolidation, and the anti-thrash details.** No auto-eviction: a full store refuses the
add and shows current entries. A **3-failures-per-turn circuit breaker** returns a terminal
error telling the model to stop and continue its reply, because "a failed memory side
effect must never block the turn's reply" (`:161-201`). Success responses deliberately do
NOT echo all entries — doing so was observed causing 5 redundant repeat batches
(`:717-723`). Writes are injection-scanned with a strict ruleset (`:86`) since memory
enters the system prompt and persists; external drift is detected and refused with a
`.bak.<ts>` (`:807`).

**"Continuous learning" is real, but it lives in the SKILLS layer, not the memory layer.**
Memory is fact storage; the writable skill library is where a turn gets distilled into a
reusable rule (`agent/background_review.py:346-352`: memory says "who the user is and what
state operations are in", skills say "how to do this class of task"). The load-bearing
safety valve is the **do-not-learn list** (`:274-300`): never record
environment-dependent failures, never record negative claims about tools ("X is broken"
hardens into a refusal the agent cites at itself for months after the fix), never record
transient errors that self-resolved, never record one-off narratives, and **never write up
an unresolved failure as a working procedure** — that hands a future session an untested
sequence of failures as validated guidance.

**Comparison — omo notepads.** `dist/index.js:114960` defines exactly four files
(learnings/decisions/issues/problems) scaffolded with `flag: "wx"` (never overwrite,
`:114991`), read by the orchestrator via glob+Read and injected as "Inherited Wisdom",
writes forced append-only by a hook. **Plan-scoped working memory, not cross-session
learning** — no retrieval, no consolidation, no reflection. But its four-way semantic split
and its append-only hard rule are worth keeping; that split is exactly what a coding agent
needs to survive across turns.

**Comparison — upstream opencode: NO PERSISTENT MEMORY, definitively.**
`find packages -iname '*memor*'` → only `app/src/context/tab-memory.ts` (UI tab state). No
memory tool in the registry. Instruction loading is read-only
(`session/instruction.ts:60-68`); skill discovery's only write is a `.opencode-version`
cache marker. `grep -rn "reflect|learning|distill"` → two noise hits. Compaction produces a
fixed five-section summary into the **message stream**, never to disk
(`core/src/session/compaction.ts:15`), so the next session knows nothing of the last one.
**Consequence for this project: Wave 15 has no upstream contract to match, so it is a
declared divergence rather than a compatibility risk — and it must be gateable.**

**Judgment adopted into the plan.** Load-bearing: character cap the model can see; frozen
snapshot; post-response review fork with a write-only whitelist; the do-not-learn list.
Explicitly out of scope as decoration for this project: vector store (retrieval targets
here are identifiers/paths/error codes/commit hashes, where lexical beats semantic, and an
embedding dependency would break the self-contained-binary requirement), MemGPT's paging
archival API, the pluggable provider abstraction, and the 7-day curator daemon.

## Task 12

- `--pure` is parsed by the top-level CLI at `packages/opencode/src/index.ts:62-71`, which writes `OPENCODE_PURE=1`; `packages/opencode/src/cli/cmd/debug/index.ts:65-67` reports `external plugins disabled (--pure)`, and `packages/opencode/src/plugin/tui/runtime.ts:1088-1106` turns only the external plugin origin list into `[]` while retaining internal plugins. Oracle config diffs prove that neither `--pure` nor `OPENCODE_PURE=true` suppresses any config layer.
- Config-related truth parsing is not uniform at the process boundary. `OPENCODE_DISABLE_PROJECT_CONFIG=TRUE` follows `Flag.truthy` and is accepted case-insensitively, while the real 1.18.12 command rejects `OPENCODE_PURE=TRUE` and `TrUe` through its Effect boolean schema before the command runs. Lowercase `true` and `1` are accepted.
- Both the installed oracle (`1.18.12`) and the source oracle (`1.18.13`, commit `aefaf140c1`, self-reporting `local`) exhibit the same `OPENCODE_PERMISSION` new-key ordering: remeda emits new keys in reverse source order.
## Task 16

- Oracle wildcard semantics (`packages/core/src/util/wildcard.ts:3-13`): normalize every `\\` to `/`; escape regex metacharacters as literals; translate `*` to any number of UTF-16 code units and `?` to exactly one UTF-16 code unit; anchor the whole match; enable dot-all; treat a trailing `" *"` as an optional space plus arguments so `git *` matches both `git` and `git push`; matching is case-insensitive only on Windows.
- Permission keys use the same wildcard matcher as value patterns (`packages/opencode/src/permission/index.ts:28-37`). The explicit config key set is `read, edit, glob, grep, list, bash, task, external_directory, todowrite, question, webfetch, websearch, lsp, doom_loop, skill`; `StructWithRest` means arbitrary custom keys remain valid.
- `reject` resolves the selected pending first, then rejects every remaining pending with the same `sessionID`, regardless of permission or pattern. Optional correction feedback applies only to the selected request; sibling rejections have no feedback (`index.ts:121-139`).
- `always` first resolves the selected pending, appends an `allow` runtime rule for each selected `always` pattern using the selected permission, then clears only same-session pendings whose every pattern evaluates to `allow` against the runtime-approved rules alone. Other sessions, different permissions, and partially covered pattern lists remain pending (`index.ts:142-166`).
- Todo 12 key-order allow-list reasoning does not hold generally. Outer config keys can overlap because permission keys themselves are wildcard patterns (for example `bash` and `*`), so reversing newly-added `OPENCODE_PERMISSION` keys can change the winning rule.

## Task 18 — references / formatter / lsp union arms

- `references` (`packages/core/src/config/reference.ts:18`) is `Record(String, Union([String, Git, Local]))` — 3 arms: bare string; `Git {repository, branch?, description?, hidden?}` (`:5-10`); `Local {path, description?, hidden?}` (`:12-16`). The bare-string arm carries no `description`/`hidden`, so the shorthand is unclassifiable at config level — kept verbatim for the loader.
- `formatter` (`packages/core/src/v1/config/formatter.ts:12`) is `Union([Boolean, Record(String, Entry)])` — 2 arms; `Entry` (`:5-10`) has all four fields optional, so `{"gofmt":{}}` is valid.
- `lsp` (`packages/core/src/v1/config/lsp.ts:76`) is `Union([Boolean, Record(String, Entry)])` — 2 outer arms; `Entry` (`:7-18`) is itself `Union([{disabled: Literal(true)}, {command, extensions?, disabled?, env?, initialization?}])` — 2 inner arms. **7 arms total** as the plan says (3 + 2 + 2), plus the inner union which the plan counted inside the lsp 2. I tested 3 + 2 + 2 outer/inner explicitly = 10 round-trip tests.
- Surprising: **absent means disabled** for both `formatter` and `lsp`. The runtime gates on truthiness — `if (!cfg.formatter)` (`packages/opencode/src/format/index.ts:120`) and `if (!cfg.lsp)` (`packages/opencode/src/lsp/lsp.ts:151`) — and no default is merged in; opencode's own tests set `{lsp: true}` / `{formatter: true}` explicitly.
- Surprising: `ruff` and `uv` are one backend, so disabling **either** disables **both** (`format/index.ts:138-143`), and a `ruff` override is dropped when `uv` is disabled.
- Surprising: the runtime removes a server on *truthy* `disabled`, and an object arm **enables the built-ins first** and only then applies overrides (`lsp.ts:155-181`) — so an unmentioned built-in is enabled, not disabled.
- The 38 built-in LSP server ids (`lsp.ts:22-61`) include the space-bearing id `"php intelephense"`.
- Plan claim checked and **partly wrong**: the prompt says "todo 10 rejects the singular `reference`". The *oracle schema* accepts it (`packages/core/src/v1/config/config.ts:48-50`, `@deprecated`) and todo 7 keeps the field so a layer still parses; todo 10's legacy pass is what rejects it (`oc-config/src/legacy.rs:302-307`). Both are true at different layers. My resolution layer reads only `references`.

## Task 13 — agent loading from config and markdown

### The name-derivation rule, verified against the real binary (not inferred)

An agent's name is its path **relative to the config directory**, minus the
`agent/` or `agents/` prefix and minus the extension. Oracle:
`packages/opencode/src/config/agent.ts:22` calling
`configEntryNameFromPath` at `packages/opencode/src/config/entry-name.ts:14-18`.

Verified empirically with `opencode agent list` on opencode 1.18.12, sealed temp
HOME/XDG:

| file | agent name |
|---|---|
| `$XDG_CONFIG_HOME/opencode/agent/review/security.md` | `review/security` (**not** `security`) |
| `<project>/.opencode/agent/deep/nested/thing.md` | `deep/nested/thing` (**not** `thing`) |
| `$XDG_CONFIG_HOME/opencode/agent/flat.md` | `flat` |

Three details that only the oracle source states, all reproduced:

* The prefix match is **anchored** to the relative path. `entry-name.ts:11-12`
  carries an upstream comment: matching the prefix anywhere in an *absolute* path
  mis-keyed agents whose home/parent directories happened to contain a segment
  called `agent` (upstream issue #25713). A path with no matching prefix falls
  back to `path.basename`.
* `agent/` is tried **before** `agents/`, so `agents/agent/x.md` is `agent/x`,
  not `x`.
* Only the final extension is stripped, and a leading dot is not an extension:
  `agent/a.tar.md` -> `a.tar`, `agent/.hidden` -> `.hidden`.

**A frontmatter `name:` key overrides the derived name entirely.** The oracle
spreads `md.data` *over* the derived name (`config/agent.ts:24-28`) and then keys
the result map on the result (`:29`). Verified: `agent/original.md` carrying
`name: renamed-by-frontmatter` listed as `renamed-by-frontmatter`, and `original`
did not appear at all.

### The seven built-ins, with the oracle line for each

`packages/opencode/src/agent/agent.ts`:

| agent | oracle lines | mode | hidden | prompt | note |
|---|---|---|---|---|---|
| `build` | `:142-156` | primary | no | none | description only |
| `plan` | `:157-181` | primary | no | none | description only |
| `general` | `:182-195` | subagent | no | none | description only |
| `explore` | `:196-217` | subagent | no | `prompt/explore.txt` | |
| `compaction` | `:218-232` | primary | **yes** | `prompt/compaction.txt` | no description |
| `title` | `:233-248` | primary | **yes** | `prompt/title.txt` | `temperature: 0.5` |
| `summary` | `:249-263` | primary | **yes** | `prompt/summary.txt` | no description |

`build`, `plan`, and `general` genuinely have **no prompt** — that is part of
their definition, not an omission. Only `title` sets a temperature. The four
prompt files were copied byte-for-byte (`md5sum`-verified at import) rather than
retyped; a test pins their exact byte lengths (823 / 871 / 648 / 2120) and anchor
phrases, because a truncated built-in prompt changes agent behaviour silently.

### Override precedence, confirmed

* A user definition **overrides** a built-in field-by-field with `??` semantics
  (`agent.ts:280-293`): an override that sets only `model` keeps the built-in's
  prompt, temperature and hidden flag. Verified Rust-side and against the binary
  (`plan` given `mode: all` printed as `plan (all)`).
* **An overridden built-in stays native.** `agent list` sorts natives first
  (`cli/cmd/agent.ts:241-246`); an overridden `plan` still printed inside the
  native block, before the user's own agents. So "native" is a property of the
  name, not of whether config touched it.
* `disable: true` deletes the agent, built-in or not (`agent.ts:268-271`).
* A name with no built-in becomes a new agent with `mode: "all"`
  (`agent.ts:274-279`). Verified: a markdown agent whose frontmatter omits `mode`
  printed as `(all)`.

### Layer order for `agent`, both boundaries verified against the binary

```
global config < OPENCODE_CONFIG < project files < per-dir .opencode
  < MARKDOWN agents < OPENCODE_CONFIG_CONTENT < managed preferences
```

The markdown layer sits in the **middle**, not last (`config/config.ts:460` vs
`:467-475`). Both boundaries were probed, because getting either wrong is silent:

* `opencode.json` defining `agent.collide.mode = primary` **plus**
  `agent/collide.md` with `mode: subagent` -> the binary printed
  `collide (subagent)`. **Markdown beats a config file.**
* `agent/contentwin.md` with `mode: subagent` **plus**
  `OPENCODE_CONFIG_CONTENT={"agent":{"contentwin":{"mode":"all"}}}` -> the binary
  printed `contentwin (all)`. **The env layer beats markdown.**

### `opencode agent list --format json` does not exist

The plan's acceptance criterion names this flag. `cli/cmd/agent.ts:235-257`
declares no options at all, and running it exits 1 with a yargs usage error and
zero bytes on stdout. The real output is, per agent, a `name (mode)` header line
followed by the resolved permission ruleset as indented JSON whose closing `]`
sits at column 0.

### `#` after whitespace is a comment even inside a plain frontmatter scalar

`color: #ff5733` unquoted made the binary report
`Expected string | "primary" | ... | undefined, got null color` — the value was
consumed as a comment and the key resolved to null. So a hex colour **must** be
quoted, and `description: fixes issue #25713` really does lose the `#25713`.
I initially assumed the opposite and wrote a test asserting it; the probe
overturned that.

## Task 17 — tool visibility (oc-permission/src/visibility.rs)

**The hiding predicate, verbatim from the oracle** (`packages/opencode/src/permission/index.ts:204-219`,
`disabled`): for each tool, map its name onto a permission key, then
`ruleset.findLast(rule => Wildcard.match(permission, rule.permission))` — the search matches the
**permission key only, ignoring the pattern** — and the tool is hidden iff that last rule has
`pattern === "*" && action === "deny"`.

Two consequences that are easy to get wrong:
- It is a *conservative sufficient* condition, not "no input can be allowed". `[{bash,"*",deny},
  {bash,"rm *",deny}]` denies every input, yet the last permission-matching rule has pattern
  `"rm *"`, so the oracle keeps `bash` **visible**. Ported as-is; drop-in parity beats being clever.
- A later *narrower* rule un-hides. `{"bash": {"*":"deny","echo *":"allow"}}` → last matching rule is
  `echo *`/allow → `bash` stays visible, and `rm -rf /` is still denied at call time.
  Oracle test parity: `test/permission/next.test.ts:452-556`.

**The complete alias table** (both groups confirmed in the oracle, not guessed):
- `edit`, `write`, `apply_patch` → key **`edit`**  (`permission/index.ts:205`)
- `list_mcp_resources`, `list_mcp_resource_templates`, `read_mcp_resource` → key **`read`**
  (`permission/index.ts:206`; the same three names are declared as `MCP_RESOURCE_TOOLS` in
  `session/tools.ts:28-32`)
- every other tool is governed by its own name.

**Agent/session merge precedence — the prompt had this backwards.** `merge(...rulesets) =>
rulesets.flat()` and both call sites are `Permission.merge(agent.permission, session.permission ?? [])`
(`session/tools.ts:87`, `tool/registry.ts:280`). Agent rules are appended **first**, session rules
**last**, so under `findLast` **session rules win**. Agent permission is the *base* layer, not the
override layer. A session `{"edit":"allow"}` re-enables all three edit aliases for a plan agent; an
agent `{"edit":"deny"}` with an empty session ruleset keeps them hidden.

**Permission keys are wildcard patterns too**, so the key-level match must go through the same
`wildcard_match`: `{"*":"deny"}` hides every tool, and outer key order decides
(`{"*":"deny","bash":"allow"}` → only bash survives; `{"bash":"allow","*":"deny"}` → nothing survives).
This is the Todo 16 finding applied to visibility.

**In v1.18.13 the oracle's only production caller of `visibleTools` is `registry.describeCodeMode`
(`tool/registry.ts:281`)**, over MCP tools; `registry.tools()` itself does *not* yet filter builtins by
permission. So Todos 38/44 own the decision of where to apply this filter — the utility is oracle-exact,
its wiring is not yet dictated by the oracle.

## Task 104 — raw `OPENCODE_PERMISSION` key order

The inherited claim that remeda `mergeDeep` reverses newly added object keys was
false. Raw probes against installed `1.18.12` and source `1.18.13 @ aefaf140c1`,
plus inspection of remeda `2.26.0`, all showed the same recursive rule: existing
keys retain their positions, overwritten values change in place, and source-only
keys append in source order.

The apparent reversal came from the Rust differential harness itself. Parsing
oracle output into `serde_json::Value` sorted object keys before comparison, so
the harness was not observing raw oracle bytes. A flattened typed deserializer
can remove the oracle-only top-level `mode` field while preserving the insertion
order of the remaining config, including nested permission objects.

## Task 14 — skill discovery: the six roots, both render forms, the remote index

### The six roots, in the oracle's order, one citation each

`discoverSkills` (`packages/opencode/src/skill/index.ts:173-233`). Every row below was
confirmed by building a fixture for it and diffing `opencode debug skill`, not by reading
alone.

| # | root | pattern | `dot` | oracle |
|---|------|---------|-------|--------|
| 1 | `$HOME/.claude` | `skills/**/SKILL.md` | true | `:21`, `:187`, `:191-193` |
| 2 | `$HOME/.agents` | `skills/**/SKILL.md` | true | `:22`, `:188`, `:191-193` |
| 3 | every `.claude`/`.agents` from `directory` up to `worktree` | `skills/**/SKILL.md` | true | `:196-202` |
| 4 | every config directory | `{skill,skills}/**/SKILL.md` | false | `:24`, `:205-208` |
| 5 | each `skills.paths[]` entry | `**/SKILL.md` | false | `:25`, `:210-220` |
| 6 | each cache dir a `skills.urls[]` index produced | `**/SKILL.md` | false | `:222-227` |

Shared scan options: `absolute: true, include: "file", symlink: true` (`:148-156`), so `**`
descends into symlinked directories and the **symlink** path is what `location` reports.
`**` matches zero segments, so `~/.agents/skills/SKILL.md` is a match. The built-in
`customize-opencode` is registered *before* root 1 (`:276-283`) with `location` the literal
string `<built-in>`.

Four rules that are not visible in the table:

- **The Claude switch also silences the project walk.** `externalDirs` is built once
  (`:186-188`) and reused for the `$HOME` probe *and* the ancestor walk, so
  `OPENCODE_DISABLE_CLAUDE_CODE_SKILLS=1` removes `proj/.claude/skills` too. Measured.
- **`OPENCODE_DISABLE_PROJECT_CONFIG` does not reach root 3.** It gates
  `ConfigPaths.directories` (root 4), not `fsys.up`. Measured: a project `.agents` skill
  survives the flag.
- **`skills.paths[]` relative entries resolve against the CWD, not the worktree** —
  `path.join(directory, expanded)` at `:213`. Measured inside a git repo with the process in
  `proj/sub/deeper`: `relskills` under the CWD was found, `relskills` under the repo root was
  not. **The plan's "relative to workspace" is wrong.**
- **The path set is keyed by the walked string, not a canonical path.** `state.matches` is a
  `Set<string>` (`:168`) and nothing canonicalizes. So a
  `~/.claude/skills/x -> ~/.agents/skills/x` symlink yields **two** matches with one `name`,
  which lands in duplicate-**name** handling, not path de-duplication. 27 of this machine's
  136 skills are exactly that alias.

### Both render forms, exact bytes

`Skill.fmt` (`:321-346`). Only caller of the verbose form is `session/system.ts:108`, on
every request. Neither form is reachable from any CLI command, so there is nothing to diff —
they are protected by `insta` snapshots plus per-rule assertions.

List form (`join("\n")`, **no** trailing newline):

```
## Available Skills
- **<name>**: <description>
```

Verbose form (`join("\n")`, **no** trailing newline; two-space and four-space indents):

```
<available_skills>
  <skill>
    <name><name></name>
    <description><description></description>
    <location><escapeHtml(location)></location>
  </skill>
</available_skills>
```

Empty case, both forms: `No skills are currently available.`

Three details that are easy to lose: a skill with **no** `description` is filtered out
(`:322`) *before* the emptiness check (`:323`), so an all-description-less set renders as the
empty sentinel while still being in `all()`; `escapeHtml` is applied to `location` **only**,
never to `name` or `description`; and the sort is `a.name.localeCompare(b.name)`, not a byte
sort.

### `localeCompare`, measured rather than approximated

Probed `String.prototype.localeCompare` under the oracle's Node. Primary weight order for
printable ASCII is

```
 _-,;:!?.'"()[]{}@*/\&#%`^+<=>|~$0123456789 <letters, case-insensitive>
```

and **case is tertiary**: `"Zebra".localeCompare("zzz") < 0`, so a table that simply places
`Z` after `z` gets it backwards. Two levels reproduce every measured case: primary key with
letters folded to lowercase, then a case tiebreak (lowercase first). Cases pinned in tests:
`aB<Ab`, `ab<aB`, `a-b>a_b`, `ab-c<abc`, `zz>z-z`, `a1<aA`, `a<a-`, `Zebra<zzz`.
Non-ASCII sorts after the table by code point — a recorded divergence from ICU, and no skill
name on this machine contains anything outside `[a-z0-9_-]`.

### Remote index protocol (`skill/discovery.ts`)

Verified end to end by pointing the real binary at `python3 -m http.server`.

```
GET <url>/index.json                                   :50-51 (trailing slash added, so
                                                        https://h/sub -> https://h/sub/index.json)
{"skills":[{"name","files":[...],"version"?}]}          :13-21
entry without "SKILL.md" in files -> warn, drop, never fetched   :67-73
cache root  $XDG_CACHE_HOME/opencode/skills/<name>/     :35, :79
files resolve against <url>/<name>/                     :90
no version, or .opencode-version already matches ->
  download in place; an existing file is skipped        :38, :87-92
version changed -> stage in <root>.tmp-<uuid>, require a
  SKILL.md, stamp .opencode-version, rename with
  <root>.old-<uuid> as rollback, always clean staging   :93-125
root returned only if <root>/SKILL.md exists            :126
concurrency 4 skills / 8 files                          :10-11
```

Live check: an index of `remoteok`(SKILL.md+helper.md), `remotebad`(README.md only) and
`remoteextra`(version `v1`) produced exactly `remoteok` and `remoteextra` from
`~/.cache/opencode/skills`, with `.opencode-version` written only for `remoteextra`, and
`remotebad` never requested.

### `opencode debug skill` truncates its own stdout through a pipe

```
$ for i in 1 2 3; do opencode debug skill | wc -c; done      -> 40960 40960 57344
$ for i in 1 2 3; do opencode debug skill > f; wc -c < f; done -> 2807771 x3
```

`debug/skill.ts` ends with a bare `process.stdout.write(...)` and the process exits without
draining the pipe. **`oc_testkit::Oracle::run` captures through a pipe and therefore cannot
be used for any oracle command with large output.** Both halves of the skill differential
redirect stdout to a file. Anyone writing a differential against a verbose `debug`
subcommand needs the same workaround, or `Oracle::run` needs a file-capture mode.

### The oracle's duplicate-name winner is racy

`loadSkills` uses `Effect.forEach(..., { concurrency: "unbounded" })` (`:240-243`) and each
load starts with an async read, so the *order the writes land* is I/O timing. Fixture with
`dupe` under `~/.claude`, `~/.agents` and a config directory: three consecutive runs picked
`.agents`, `config`, `config`. Three runs over the real tree: **name set identical every
time, location set different every time.** The name set is the contract.

### Frontmatter: `gray-matter` + js-yaml 4, and only two keys

`isSkillFrontmatter` (`:53-59`) reads `name` and `description` and nothing else — confirmed,
not assumed: a file carrying `license`, `allowed-tools` and `version` alongside them loaded
with those ignored. Delimiter and scalar behaviour, all measured:

| fixture | oracle |
|---|---|
| `----\nname: x\n----` | not a delimiter; no frontmatter; skill dropped |
| `---\n# comment only\n---` | empty data; skill dropped |
| no closing `---` | frontmatter parses, `content` is **empty** |
| `---\r\n…\r\n---\r\nBody\r\n` | `content` is `Body\r\n` |
| `---yaml` / `---json` | parsed by that engine |
| `name:` or `description:` (null) | present-but-not-a-string; skill dropped |
| `name: yes` | the **string** `"yes"`; skill loaded |
| `name: true` / `name: 123` | not a string; skill dropped |
| `description: Use when: X` | loads, via the `sanitize` block-scalar retry |
| `description: >` folded | folds to `line one line two\npara two\n` |

`name: yes` staying a string is why YAML 1.2 core matters: `serde_yaml` (libyaml, YAML 1.1)
would make it a boolean and silently drop such a skill. `yaml_rust2` resolves like js-yaml 4.

## Task 15 — command resolution precedence and argument expansion

### The precedence chain, with the oracle line for each level

Four sources write into ONE `Record<string, Info>`. A later level overwrites an
earlier one, except the last, which does not.

| # | level | oracle line | overwrite? |
|---|---|---|---|
| 1 | built-in `init`, `review` | `command/index.ts:70-88` | seeds the map |
| 2 | `cfg.command` entries (incl. markdown commands) | `:90-103` | unconditional |
| 3 | MCP prompts | `:105-132` | unconditional |
| 4 | skills | `:134-152` | **only if the name is free** |

The whole "skills never override" rule is ONE line — `command/index.ts:135`:
`if (commands[item.name]) continue`. A losing skill is dropped entirely; it does
not appear twice and its description does not leak.

Verified twice: read from the oracle AND observed on the real binary via
`GET /command` on a fixture that collides all four levels (transcript in
`.omo/evidence/task-15-opencode-rust.txt` §1). Config beat the built-in `review`;
an MCP prompt beat `command["srv:hello"]`; skills named `collide` and
`srv:noargs` vanished from the listing.

**Overwriting keeps the ORIGINAL listing position.** A config `review` stays in
slot 1 where the built-in put it, because assigning to an existing JavaScript
object property does not move the key. `OrderedMap::insert` already reproduces
this, so nothing extra was needed — but a `HashMap` + sort would have got it
wrong.

### MCP prompts are keyed `client:prompt`, sanitized

`mcp/catalog.ts:100-105` keys prompt records `sanitize(client) + ":" +
sanitize(name)`, where `sanitize` (`:113`) maps every character outside
`[A-Za-z0-9_-]` to `_`. So prompt `hello` on server `srv` is the command
`srv:hello`. Level 3 IS an unconditional overwrite, but it can only collide with
a config command whose key is literally that colon-qualified spelling — worth
knowing before someone "fixes" a collision that cannot happen.

`command/index.ts:117-118`: the server is asked for its prompt with every
declared argument bound to the LITERAL string `"$1"`, `"$2"`, … so the returned
text still carries those placeholders and ordinary expansion fills them
afterwards. Hints come from the argument COUNT (`:130`), never from the text.

### Argument expansion — every rule, all observed

Oracle `session/prompt.ts:1372-1395`; regexes `:1594-1596`; hints
`command/index.ts:36-43`. Pinned by a 59-case differential against a verbatim
JavaScript transcription (`tests/fixtures/command_expansion_oracle.cjs`).

- **The highest placeholder is greedy.** `A=[$1] B=[$2]` with four arguments
  gives `A=[one] B=[two three four]`. Greediness follows the NUMBER, not the
  position in the text: `B=[$2] A=[$1]` still makes `$2` greedy. This is the
  single most surprising rule and the plan does not mention it.
- **A positional past the end is EMPTY**, never an error: `$3` with two
  arguments → `C=[]`. (The plan's failure scenario, confirmed.)
- **`$ARGUMENTS` with no input is empty**, and the final `.trim()` then removes
  the gap: `"Input: $ARGUMENTS"` + `""` → `"Input:"`.
- **`$0` is a JavaScript artefact.** `args[-1]` is `undefined`, so `$0` renders
  the literal text `undefined` — UNLESS `$0` is itself the highest placeholder,
  when `slice(-1)` makes it the LAST argument. `$00` behaves the same; `$01` is
  just position 1.
- **A bare `$` before a digit IS a placeholder.** `COST IS $5.00` becomes
  `COST IS .00`. `$x` and a trailing `$` are literal. `hints` reports `['$5']`.
- **`$10` is the TENTH argument**, not `$1` then `0`; `\d+` is greedy.
- **`\d` is ASCII-only**: `$٣` (U+0663) is not a placeholder.
- **Absurd numbers are fine**: `$999`, `$99999999999999999999` → empty.
- **Append fallback**: a template mentioning NO placeholder at all gets the raw
  input appended after `\n\n`, provided the input is not blank.
- **`$ARGUMENTS` is the RAW input; `$N` are tokenized.** `"quoted arg"  spaced`
  → `$1` = `quoted arg spaced` (greedy, unquoted) but `$ARGUMENTS` keeps the
  quotes and the double space.

### The `$`-pattern trap inside `$ARGUMENTS` (found by running, not reading)

`:1391` is `withArgs.replaceAll("$ARGUMENTS", input.arguments)` — a STRING
replacement, so ECMA-262 `GetSubstitution` runs over the USER'S OWN ARGUMENTS:

| user types | lands as |
|---|---|
| `$$` | `$` |
| `$&` | the literal text `$ARGUMENTS` |
| `` $` `` | everything in the template before the placeholder |
| `$'` | everything after it |
| `$1`, `$<name>` | left alone (no capture groups) |

Positional substitution at `:1383` uses a FUNCTION replacer and does none of
this — `$$` and `$&` stay literal there. So the two substitution steps have
DIFFERENT escaping rules, and a naive `str::replace` for `$ARGUMENTS` diverges
the moment a user pastes a shell `$$` or a ref containing `$&`.

### Tokenizer (`argsRegex` + `quoteTrimRegex`)

`/(?:\[Image\s+\d+\]|"[^"]*"|'[^']*'|[^\s"']+)/gi` then `/^["']|["']$/g`:

- `[Image 3]` is ONE token, case-insensitively (`[image 12]` too).
- A quoted run is one token and loses its quotes, so `""` yields an EMPTY token.
- An UNPAIRED quote matches no alternative and is skipped whole: `" second`
  yields just `["second"]`, not `['"', 'second']`.
- `don't` splits into `don`, `t` — the apostrophe opens a group that never
  closes.
- Whitespace runs collapse; the quote-trim is anchored so a lone `"` cannot
  underflow.

### Hints are sorted LEXICOGRAPHICALLY

`command/index.ts:40` dedupes through a `Set` (insertion-ordered) then calls
`.sort()` — a string sort. So `$2 and $10` yields `['$10','$2']`, observed on the
real binary. The raw spelling survives (`$01` stays `$01`). `$ARGUMENTS` is
appended AFTER the sort, never inside it. A skill's hints are hardcoded empty
(`:150`) even when its body contains `$1` — but expansion still runs over that
body.

### Built-in shapes, observed

- `init`: description `guided AGENTS.md setup`, no `subtask`, `${path}`
  interpolated ONCE (`String.replace` with a string pattern), template 3500
  chars for a 15-char worktree (file is 3492).
- `review`: description `review changes [commit|branch|pr], defaults to
  uncommitted`, **`subtask: true`**, 4704 chars, and it contains NO `${path}` so
  its substitution is a no-op.
- A config entry replacing a built-in replaces it WHOLESALE — the config `review`
  loses `subtask: true` unless it declares it.
- `variant` exists on the config entry (`config/command.ts:10`) but
  `command/index.ts:91-102` never copies it into `Info`, so the resolved command
  has no variant field. Adding one would invent a field `/command` does not
  return.

## Task 24 — `oc-auth`: the two credential files

### `auth.json` — three shapes, exact field names (oracle: `packages/opencode/src/auth/index.ts`)

Provider-keyed JSON object, values discriminated by `type` (`:35`).

| `type` | oracle | fields (exact on-disk spelling) |
| --- | --- | --- |
| `oauth` | `:14-21` | `refresh` str, `access` str, `expires` **NonNegativeInt** ms, `accountId?` str, `enterpriseUrl?` str |
| `api` | `:23-27` | `key` str, `metadata?` `Record<string,string>` |
| `wellknown` | `:29-33` | `key` str, `token` str |

`expires` is `NonNegativeInt` (`:18`, importing from `@opencode-ai/core/schema`), so a
negative value fails to decode and the whole entry is dropped — confirmed by seeding
`expires: -5` and watching `auth list` not show it. `OAUTH_DUMMY_KEY =
"opencode-oauth-dummy-key"` at `:8` is the placeholder an OAuth provider stores where
an API key would go; re-exported as `oc_auth::OAUTH_DUMMY_KEY`.

Confirmed against the live `$XDG_DATA_HOME/opencode/auth.json` on this machine (read
structurally, values never printed): 10 `api` entries and 2 `oauth`, one of which
carries `accountId`. Both optional-field shapes occur in the wild.

### What `OPENCODE_AUTH_CONTENT` actually overrides (`:58-66`)

It replaces **the whole result of `all()`**, and `all()` is the only reader — so `get`
is overridden too. The file is not consulted at all when the variable parses.

Three consequences, each **observed against the 1.18.12 binary**, not inferred:

1. A malformed value falls through to the file. `:62` is a bare `catch {}`. Verified:
   `OPENCODE_AUTH_CONTENT='{not json'` listed the file's credentials.
2. Writes still go to the **file**, never to the variable. `set`/`remove` (`:73-89`)
   both start from `all()`, so a mutation under an active override writes
   *override ∪ mutation* to disk and **destroys whatever the file held**. Verified: a
   file holding `filealpha`+`filebeta`, plus an override naming `envgamma`+`filebeta`,
   plus `auth logout filebeta`, left exactly `{"envgamma":…}` — `filealpha` gone.
   `oc-auth` reproduces this deliberately; diverging would mean the two binaries
   disagree about the user's credentials.
3. It is not a schema bypass: an override entry of an unknown shape is dropped exactly
   as a file entry would be.

### `mcp-auth.json` — the MCP OAuth shape (oracle: `packages/opencode/src/mcp/auth.ts`)

Server-name-keyed object of `Entry` (`:25-31`), **every field optional** because the
flow fills them in over several steps. All camelCase on disk.

| field | oracle | contents |
| --- | --- | --- |
| `tokens` | `:9-14`, `:26` | `accessToken` str, `refreshToken?` str, `expiresAt?` num, `scope?` str |
| `clientInfo` | `:17-22`, `:27` | `clientId` str, `clientSecret?` str, `clientIdIssuedAt?` num, `clientSecretExpiresAt?` num |
| `codeVerifier` | `:28` | the PKCE verifier, live only between redirect and callback |
| `oauthState` | `:29` | the CSRF `state`, checked on callback |
| `serverUrl` | `:30` | which URL these credentials were issued for |

`Tokens.expiresAt` / `ClientInfo.clientIdIssuedAt` are plain `Schema.Number`, **not**
`NonNegativeInt` — unlike `auth.json`'s `expires`. Modelled as `i64`.

`getForUrl` (`:89-95`) returns `undefined` when the entry records **no** `serverUrl` at
all, not just when it differs. An entry with no recorded URL is never assumed to match.

`clearField` (`:122-130`) returns `undefined` for an absent server, and `mutate`'s
`if (!next) return` (`:79`) means **no write happens** — so clearing a field on an
unknown server leaves the file untouched (does not even create it).

### The write path (`fs-util.ts:110-113`, called with `0o600` at `auth/index.ts:79` and `mcp/auth.ts:80`)

```ts
const content = JSON.stringify(data, null, 2)
yield* fs.writeFileString(path, content)
if (mode) yield* fs.chmod(path, mode)      // <-- AFTER the write
```

- Encoding is `JSON.stringify(data, null, 2)`: two-space indent, **no trailing
  newline**. Byte-verified with `xxd` against a file 1.18.12 wrote — ends `}\n}` with
  no final `0a`. `serde_json::to_vec_pretty` matches exactly.
- The chmod is a *follow-up*, so the file exists at the umask (typically `0644`) with
  the tokens already in it for a moment. `oc-auth` passes the mode to `open(2)` via
  `OpenOptionsExt::mode` instead, then also `set_permissions` because `mode()` only
  applies on creation and an existing permissive file must still be repaired.
- A write **does** repair a permissive file: observed `0644` → `0600` after
  `auth logout`.

### Decoding is per-entry and lossy in the oracle

`Record.filterMap` (`:66`) silently drops any value that fails to decode, and the next
write persists their absence. Observed: 5 seeded entries (1 good, 1 no `type`, 1
`type:"banana"`, 1 negative `expires`, 1 with an extra unknown field) → `auth list`
showed 2, and one `auth logout` left those 2 on disk with the extra field stripped. An
unknown extra field is therefore tolerated on read and dropped on write.

## Task 19 — opening `opencode.db`: what the four pragmas actually report

`database.ts:27-32` issues **five** pragmas plus a checkpoint, not four. In order:
`journal_mode = WAL`, `synchronous = NORMAL`, `busy_timeout = 5000`,
`cache_size = -64000`, `foreign_keys = ON`, then `wal_checkpoint(PASSIVE)`. The plan's
acceptance criterion names four; `cache_size` is the fifth and is applied and verified
too. `PRAGMA_SEQUENCE` in `crates/oc-db/src/open.rs` is asserted line-for-line against
that list.

### Read-back values, measured on a fresh file database

| pragma | set as | reads back as | type |
| --- | --- | --- | --- |
| `journal_mode` | `WAL` | `wal` | text, lowercase |
| `synchronous` | `NORMAL` | `1` | integer |
| `busy_timeout` | `5000` | `5000` | integer, ms |
| `cache_size` | `-64000` | `-64000` | integer, negative = KiB not pages |
| `foreign_keys` | `ON` | `1` | integer |

`journal_mode` is the only one recorded in the database file; the rest are
per-connection. A reopen with no pragmas reports `wal` but `synchronous = 2` and
`cache_size = -2000`. That asymmetry is why `Pool` owns connection creation and has no
constructor taking a caller's `Connection`.

### `:memory:` reports `memory`, not `wal`

SQLite refuses WAL for an in-memory database and does **not** error — it keeps `memory`
journalling and returns that. A verifier asserting `wal` unconditionally asserts
something SQLite never promised, so `verify_pragmas` expects `memory` for
`DbLocation::Memory`. `foreign_keys` and `busy_timeout` still apply normally there.

### Two of the four pragmas are already correct by accident on this driver

- `libsqlite3-sys` 0.38.1 `build.rs:126` compiles the amalgamation with
  `-DSQLITE_DEFAULT_FOREIGN_KEYS=1`. Stock SQLite defaults it **off** — measured 0 on the
  system CLI 3.53.4.
- `rusqlite` 0.40.1 `src/inner_connection.rs:118` calls `sqlite3_busy_timeout(db, 5000)`
  on every connection, which is exactly the oracle's value.

So `foreign_keys` and `busy_timeout` read back correct **even if the code never issues
them**, and a pragma-readback test alone cannot prove the implementation works. Both
defaults are pinned by
`tests/open.rs::the_stack_below_this_crate_already_defaults_two_pragmas_to_the_oracle_values`
so a driver bump that flips either fails loudly rather than silently disabling cascades.
Enforcement is proven behaviourally instead: a dangling-FK insert is rejected with
`SqliteFailure(ConstraintViolation, extended_code 787)` / "FOREIGN KEY constraint
failed", and a `foreign_keys = OFF` control shows the identical insert accepted.

### What WAL puts on disk, and when it disappears

While a connection is open: `opencode.db`, `opencode.db-wal`, `opencode.db-shm` (8 KiB /
16 KiB / 32 KiB in the QA run). **A clean shutdown checkpoints and deletes both
sidecars** — after the last connection closed, only `opencode.db` remained. So for
**todo 82 (prune)** and **todo 84 (vacuum)**: the sidecars must be handled when present
(moving or deleting the main file alone loses committed transactions) but their absence
is not evidence of a missing WAL. `oc_db::sidecar_files(path)` returns the pair;
`Pool::sidecar_files()` returns them for a file pool and empty for memory.

WAL is recorded in the file header at bytes 18-19 (both `2`), which is why an external
SQLite 3.53.4 — a different build from the bundled 3.53.2 — opens the file, reports
`journal_mode = wal` and reads back both committed rows.

### Pinned versions

`rusqlite 0.40.1` → `libsqlite3-sys 0.38.1` → **SQLite 3.53.2**
(`sqlite_source_id() = 2026-06-03 19:12:13 d6e03d8c777cfa2d35e3b60d8ec3e0187f3e9f99d8e2ee9cac695fd6fcdf1a24`).
Compiled in and relevant later: `ENABLE_FTS5` (todos 101-102), `THREADSAFE=1`,
`ENABLE_JSON1`, `ENABLE_RTREE`, `ENABLE_STAT4`.

### `IMMEDIATE`, not `DEFERRED`, is what makes `busy_timeout` work

A `DEFERRED` transaction takes a read lock and asks to upgrade on its first write; if
another writer committed in between, SQLite fails with `SQLITE_BUSY_SNAPSHOT`, which the
busy handler is explicitly **not** allowed to retry because the snapshot is already
stale. `Pool::transaction` therefore uses `TransactionBehavior::Immediate`, taking the
write lock up front so a second writer waits out the timeout instead of failing.
Measured: writer B contended for 335ms against a writer holding the lock 300ms, and both
committed — inside the 5000ms budget.

## Task 23 — oc-snapshot: per-project git object store

**The store path, observed not inferred.** Ran the real binary (`opencode` 1.18.12 at
`/config/.local/share/mise/installs/opencode/1.18.12/opencode`; the `mise` shim is broken, call the
install path directly) as `debug snapshot track` in a fixture worktree under a temp `XDG_DATA_HOME`.
It created exactly:

```
$XDG_DATA_HOME/opencode/snapshot/<projectID>/<sha1(worktree absolute path STRING)>
```

- component 1 = `projectID` from `oc_paths::project::resolve_project` (remote hash → cached
  `.git/opencode` marker → root commit → `global`). The fixture had no remote, so it was the root
  commit, and the binary also *wrote* that id into `<git-common-dir>/opencode`.
- component 2 = `sha1` hex of the worktree path **string**. Not normalized, not canonicalized, no
  trailing-slash handling. `Hash.fast` (`packages/core/src/util/hash.ts`) is plain SHA-1 despite the
  name; oracle site is `packages/opencode/src/snapshot/index.ts:71`.
- `oc-paths` (todo 4) already implements the whole thing as `Layout::snapshot_store()` /
  `Layout::worktree_hash()` / `snapshot_root()`. **Consume it; do not re-derive the hash.**

**Git commands and env the oracle uses.** `git init` is the only invocation driven by environment
(`GIT_DIR` + `GIT_WORK_TREE`, cwd = worktree); *everything else* passes `--git-dir <store>
--work-tree <worktree>` as flags, preceded by `-c` overrides. Three flag sets exist and are not
interchangeable: `core` = `core.longpaths`+`core.symlinks`; `cfg` = `core.autocrlf=false` + core;
`quote` = cfg + `core.quotepath=false`. Path-listing and diffing use `quote`; staging uses `cfg`;
`read-tree`/`checkout-index` use `core`; `write-tree` and `gc` use no `-c` at all. Init writes eight
`config` keys in a fixed order, the last four (`feature.manyFiles`, `index.version=4`,
`index.threads`, `core.untrackedCache`) purely to bound the first `add` on a huge checkout.

**`seed()` is why a store is cheap.** On first init the oracle writes the source repo's objects dir
into `<store>/objects/info/alternates` and copies the source `index` file in. Consequence worth
remembering: a snapshot tree whose content the user already committed is resolved **through the
alternate** and no object is written into the store at all. A test that wants a store-local object to
exist must snapshot *uncommitted* content.

**File lists never travel as argv.** `git add`/`rm --cached` receive
`--pathspec-from-file=- --pathspec-file-nul` and the names arrive on stdin as NUL-separated
`:(top,literal)<path>` pathspecs. That is simultaneously the injection defence and the reason a file
named `*` or `:(exclude)x` is handled as itself. `check-ignore` gets a protective `./` prefix for
names starting with `:` and echoes it back, so it must be stripped again.

**GC parameters.** `prune = "7.days"` (`index.ts:23`), `limit = 2 MiB` for untracked files
(`:24`), `cleanup()` = `git gc --prune=7.days` guarded on `exists(gitdir)` and `enabled()`
(`:300-316`), cadence = one minute delay then `Schedule.spaced(1 hour)` forked for the store's
lifetime (`:761-766`). Measured facts about real `git` 2.43 that source-reading does not give you:
`gc` **repacks** loose objects, unreachable ones into a cruft pack with `.mtimes`, so a loose file
disappearing proves nothing — only `cat-file -e` distinguishes reclaimed from repacked. Objects
inside the 7-day window survive; older unreachable ones are dropped. The **latest** snapshot tree
stays reachable through the index's cache-tree and therefore survives forever; a **superseded** tree
older than seven days is reclaimed.
## Task 20

- Verified `schema.gen.ts` creates **19 application tables** in its one `up(tx)`: `workspace`, `data_migration`, `account_state`, `account`, `control_account`, `credential`, `event_sequence`, `event`, `permission`, `project_directory`, `project`, `message`, `part`, `session_context_epoch`, `session_input`, `session_message`, `session`, `todo`, `session_share`. `migration` is created afterward, so a fresh current DB has 20 tables. A migrated legacy DB may additionally retain `__drizzle_migrations`.
- The six cloud-side tables are `workspace`, `account_state`, `account`, `control_account`, `credential`, and `permission`; all are created because the generated `up` and migration journal cover them. `data_migration` is the seventh non-session/project table and is migration infrastructure rather than cloud-side state.
- Exact explicit indexes: `event_aggregate_seq_idx`, `event_aggregate_type_seq_idx`, `permission_project_action_resource_idx`, `message_session_time_created_id_idx`, `part_message_id_id_idx`, `part_session_idx`, `session_input_session_pending_delivery_seq_idx`, `session_input_session_admitted_seq_idx`, `session_input_session_promoted_seq_idx`, `session_message_session_seq_idx`, `session_message_session_type_seq_idx`, `session_message_session_time_created_id_idx`, `session_message_time_created_idx`, `session_project_idx`, `session_workspace_idx`, `session_parent_idx`, `todo_session_idx`.
- Exact real-user journal ids in observed `rowid` order: `20260127222353_familiar_lady_ursula`, `20260211171708_add_project_commands`, `20260213144116_wakeful_the_professor`, `20260225215848_workspace`, `20260227213759_add_session_workspace_id`, `20260303231226_add_workspace_fields`, `20260228203230_blue_harpoon`, `20260309230000_move_org_to_state`, `20260312043431_session_message_cursor`, `20260323234822_events`, `20260410174513_workspace-name`, `20260413175956_chief_energizer`, `20260423070820_add_icon_url_override`, `20260428004200_add_session_path`, `20260427172553_slow_nightmare`, `20260501142318_next_venus`, `20260504145000_add_sync_owner`, `20260507164347_add_workspace_time`, `20260511000411_data_migration_state`, `20260510033149_session_usage`, `20260511173437_session-metadata`, `20260601010001_normalize_storage_paths`, `20260601202201_amazing_prowler`, `20260602002951_lowly_union_jack`, `20260602182828_add_project_directories`, `20260603001617_session_message_projection_indexes`, `20260603040000_session_message_projection_order`, `20260603141458_session_input_inbox`, `20260603160727_jittery_ezekiel_stane`, `20260604172448_event_sourced_session_input`, `20260605003541_add_session_context_snapshot`, `20260605042240_add_context_epoch_agent`, `20260611035744_credential`, `20260611192811_lush_chimera`, `20260612174303_project_dir_strategy`, `20260622142730_simplify_session_context_epoch`, `20260622170816_reset_v2_session_state`, `20260622202450_simplify_session_input`.
- The journal contains 38 ids. Exact first id: `20260127222353_familiar_lady_ursula`; exact last id: `20260622202450_simplify_session_input`.
- The real user's historical `rowid` order differs from current generated order for three pairs; completion is set-based in upstream `applyOnly`, so current generated order is correct for fresh seeding while existing journals must not be reordered.


## Task 93

### The TypeScript binary's memory, measured for the first time

Binary: `/config/.local/share/mise/installs/opencode/1.18.12/opencode`, version
**1.18.12** (the released binary, not the from-source flavour — running the TS
entry point under Bun would measure a different process tree than users run).

Machine: `ip-192-168-157-161`, kernel `6.17.0-1019-aws`, Intel Xeon 6975P-C,
32 logical CPUs, 64,767,892 KiB RAM (61.8 GiB). Sampling is total-process-tree
RSS: root plus every transitive child via `/proc/<pid>/task/*/children`, every
2 seconds.

**W-real — the number the whole project exists for.** Hydrating the user's
largest real session (`ses_2bcaee257ffeFZNJrmtpi3ZglR`, 931 messages, 3,620
parts, 100.2 MB of `part.data`) and running one turn:

| | KiB | MB |
| --- | --- | --- |
| min | 2,939,880 | 2,870 |
| **median** | **3,026,992** | **2,956** |
| max | 3,465,388 | 3,384 |

max/min = **1.1788x**. That is ~2.96 GB of resident memory for a single turn on
one session, and the tree was still growing 55s after the keystroke because every
provider request re-serialises the whole session. First hard number behind the
"the TypeScript binary exhausts memory" premise. G2 gives Rust a ceiling of
**1,478.0 MB**.

**W-idle — cold start plus one cassette-backed tool turn, 148s trace:**

| | KiB | MB |
| --- | --- | --- |
| min | 878,432 | 857 |
| **median** | **954,240** | **931.9** |
| max | 1,001,568 | 978 |

max/min = **1.1402x**. G1 gives Rust a ceiling of **465.9 MB**. So the released
TypeScript binary needs ~932 MB to start up and answer one trivial prompt.

### Revision 1 hid 203 MB of W-idle's peak

Revision 1 discarded the first 90 seconds of *every* workload as warm-up. W-idle's
trace is 148s / 75 samples, so that dropped 45 samples — 60% of them and the whole
cold start. Recomputed over the whole trace from the artifact's own retained
samples: median **954,240 KiB (931.9 MB)** vs the **746,408 KiB (728.9 MB)**
revision 1 published. Delta **207,832 KiB = 203.0 MB**.

W-real is untouched by the correction: its turn is only typed once hydration
settles at the 90s mark, so its peak lands after the old discard window either
way. Both rules give 3,026,992 KiB. Verified per-run, all five reps identical
under both rules.

### Run-to-run variance is wider than 10% on this machine

Neither workload would have passed a "two independent passes agree within 10%"
criterion: 1.14x and 1.18x. Useful to know before Wave 14 — a Rust/TS comparison
this close to the 0.50 gate boundary would be noise-dominated, but at 466 MB vs
932 MB and 1,478 MB vs 2,956 MB the margin is far outside the spread.

### W-idle's peak is genuinely cold-start-dominated

Per-rep, whole-trace peak vs post-90s peak: 878,432/746,408; 1,001,568/768,324;
954,240/767,080; 888,280/740,752; 990,168/695,156. Every rep peaks in its first
90 seconds and then *falls* by 130-300 MB. So the startup transient is not merely
included in the peak — it **is** the peak, and the RSS afterwards is 20-30% lower.
## Task 93

**Measured TypeScript baseline (revision 2), `benchmarks/ts-baseline.json`.** Machine: `ip-192-168-157-161`, kernel `6.17.0-1019-aws`, Intel(R) Xeon(R) 6975P-C, 32 logical CPUs, 64,767,892 KiB RAM (61.8 GiB). Binary measured: `/config/.local/share/mise/installs/opencode/1.18.12/opencode`, self-reports **1.18.12**. The pinned oracle source tree is **1.18.13** — the released binary installed on the machine is what users run, so that is what the artifact attributes. 1,500 raw samples retained; every published peak re-derives from them.

- **W-idle** median per-run peak **954,240 KiB = 931.9 MiB**; five peaks 878,432 / 888,280 / 954,240 / 990,168 / 1,001,568 KiB; spread max/min **1.140**; 75 samples per run over a 148 s trace.
- **W-real** median per-run peak **3,026,992 KiB = 2,956.0 MiB** on session `ses_2bcaee257ffeFZNJrmtpi3ZglR` (931 messages, 3,620 parts, 105,118,812 bytes of `part.data`); five peaks 2,939,880 / 3,016,072 / 3,026,992 / 3,063,628 / 3,465,388 KiB; spread **1.179**; 225 samples per run over a 448 s trace.
- **W-soak** `null`. See `decisions.md` and `issues.md`.
- Thresholds Wave 14 inherits by substitution: **G1 ≤ 465.9 MiB**, **G2 ≤ 1,478.0 MiB**. G3/G4 are absolute predicates and need no TS median, but **G3 has no TS evidence at all**.

**How a trivial turn runs with no live provider.** `MockProvider` (axum, loopback) replays the oracle's own recorded cassettes from `<tree>/packages/llm/test/fixtures/recordings/`; `ScriptedEnv` hands the child a cleared environment with `OPENCODE_CONFIG_CONTENT` pointing `baseURL` at that loopback port, plus `OPENCODE_DISABLE_AUTOUPDATE=1` and `OPENCODE_DISABLE_MODELS_FETCH=1`. No network is reachable and `oc-testkit` has no HTTP *client* in its dependency graph. Two further conditions are load-bearing and were each found by observing a failure, not by reading source:
1. **`permission: {"*": "allow"}`** in the generated config, or the custom tool blocks on an interactive approval that an unattended TUI never answers.
2. **Fake completed local npm state** — an empty `node_modules/` directory plus a `package-lock.json` declaring `@opencode-ai/plugin` — in *both* `<project>/.opencode/` and `<XDG_CONFIG_HOME>/opencode/`. Without it the binary tries to reify plugin dependencies over the network and the turn never starts.

**Every run needs exactly one tool-free text request before the tool loop, but it is not the same request on both paths.** A **new** session's prelude generates the session title. A **restored** session's prelude is a **compaction summary**, because W-real deliberately selects the largest session and it overflows the model's context window. Serving that prelude from the tool-loop cassette makes the TUI print `Tool call not allowed while generating summary: get_weather` and the turn never completes — visible only in the PTY transcript, since the provider still counted requests. Serving `openai-chat/streams-text` first, unconditionally, then the tool loop, produced 3 requests (1,629,657 → 1,767,929 → 1,768,561 bytes, `tools=0` then `tools=11`) and a completed turn.

**`--prompt` is discarded when `--session` restores a session**; the saved draft input wins, so the turn must be typed into the PTY. Measured on the 105 MB session: RSS sat flat at ~680 MB for 126 s with zero provider requests until a `\r` was written; then 13 s from keystroke to the first request, and the tree was still climbing at 1.1 GB 55 s later, because each request re-serialises the whole session.

**`--agent build` is not the fix for that**, and neither is `--mini`. A run with `--agent build` still hit the compaction prelude at 43.5 s. The auto-approval path needs the full TUI (`--auto` without `--mini`).

**Zombies and kernel threads publish a `/proc/<pid>/status` with no `VmRSS` line.** A 150 s × 5-run sampling loop hits this: run 1 of a full pass died with `invalid process-tree data for pid 1951313 at /proc/1951313/status: VmRSS field is absent`. Such a process holds no resident memory, so it must contribute nothing rather than abort a measurement already underway — but a `VmRSS` line that is *present and unparsable* must still fail, since that would be a format the code misreads.

**`mise` shims are not the binary.** `which opencode` resolves to `/config/.local/share/mise/shims/opencode`, a symlink to `/config/.local/bin/mise`. It works interactively but the absolute installs path is what belongs in a baseline: a launcher inside the measured process tree is a launcher inside the measured RSS.

**Do not point the harness at the live `opencode.db`.** It is **54 GB** with an 815 MB WAL. A `sqlite3 .backup` of it ran **4 h 7 min**, had written a 19 GB partial copy, and was still going when I killed it — it had taken `/config` from 674 GB to 693 GB used. `opencode.db.bak.20260408` (2.6 GB, 2,345 sessions, 92,378 messages, 329,432 parts) backs up in ~50 s and is what every measured run used, via `OPENCODE_DB`.
## Task 27

- The genuine corpus contains 47 SSE responses across 36 cassettes. It has 446 LF blank-line separators. The only CRLF blank-line separators are one each in `gemini/gemini-2-5-flash-image`, `gemini/streams-text`, and `gemini/streams-tool-call` (3 total).
- Incremental UTF-8 rule: emit only the valid prefix; when `Utf8Error::error_len()` is `None`, buffer the incomplete trailing bytes and prepend them to the next network chunk. Emit U+FFFD only for a genuinely invalid sequence or an unfinished code point at end-of-stream.
- The recordings preserve SSE frame boundaries but not original network chunk boundaries or timing, so the every-byte split sweep is required independently of cassette replay.


## Task 25

**The bundled-factory count is 24, not 23.** The plan (`.omo/plans/opencode-rust.md:364-370`) and the task brief both say "23 bundled SDK factories". `BUNDLED_PROVIDERS` in `packages/opencode/src/provider/provider.ts` opens at line 107 and closes at line 134, and holds **24** keys. `awk 'NR>=107 && NR<=136' … | grep -c '^[[:space:]]*"'` → `24`. The cited line range in the plan is correct; only the count is one low. Three independent ways of counting, none of which yields 23:

- **24 registry keys**, in source order: `@ai-sdk/amazon-bedrock`, `@ai-sdk/amazon-bedrock/mantle`, `@ai-sdk/anthropic`, `@ai-sdk/azure`, `@ai-sdk/google`, `@ai-sdk/google-vertex`, `@ai-sdk/google-vertex/anthropic`, `@ai-sdk/openai`, `@ai-sdk/openai-compatible`, `@openrouter/ai-sdk-provider`, `@ai-sdk/xai`, `@ai-sdk/mistral`, `@ai-sdk/groq`, `@ai-sdk/deepinfra`, `@ai-sdk/cerebras`, `@ai-sdk/cohere`, `@ai-sdk/gateway`, `@ai-sdk/togetherai`, `@ai-sdk/perplexity`, `@ai-sdk/vercel`, `@ai-sdk/alibaba`, `gitlab-ai-provider`, `@ai-sdk/github-copilot`, `venice-ai-sdk-provider`.
- **24 distinct SDK factory functions.** Every key resolves to its own `create*` export. `@ai-sdk/openai-compatible → createOpenAICompatible` and `@ai-sdk/github-copilot → createOpenaiCompatible` (from `@opencode-ai/core/github-copilot/copilot-provider`) differ in both module and capitalisation — two functions, not one.
- **22 distinct npm packages.** `@ai-sdk/amazon-bedrock` contributes two keys (base and `/mantle`) and `@ai-sdk/google-vertex` two (base and `/anthropic`), via subpath exports rather than separate packages.

Nothing in todo 25 depends on the number — the registry is keyed by string — but **todo 26 (catalog) and todos 29/30/94/95/96 do**, and a hard-coded 23 would silently drop one.

**Separately, `custom()` at `provider.ts:168` returns 22 loaders**, which is a different set from `BUNDLED_PROVIDERS` and keyed by *provider id* (`opencode`, `openai`, `meta`, `xai`, `github-copilot`, `azure`, `azure-cognitive-services`, `amazon-bedrock`, `llmgateway`, `openrouter`, `nvidia`, `vercel`, `google-vertex`, `google-vertex-anthropic`, `sap-ai-core`, `zenmux`, `gitlab`, `cloudflare-workers-ai`, `cloudflare-ai-gateway`, `cerebras`, `kilo`, `snowflake-cortex`) rather than by npm name. A provider id and an npm package name are **not** interchangeable keys; todo 26 needs both maps.

**Three of those loaders route to a different SDK surface per call, and that is what forces `Spec` to carry per-provider parameters:**

- `selectAzureLanguageModel` (`provider.ts:154-160`) walks `chat` → `responses` → `messages` → `languageModel`, gated on a `useChat` flag. Its endpoint is assembled from a `resourceName` resolved from provider options, then env, then stored auth. Needs base URL + API version + a construction-time surface.
- `selectBedrockMantleLanguageModel` (`:162-166`) sends model ids `openai.gpt-oss-safeguard-20b` and `-120b` to `chat()` and everything else to `responses()`, falling back to `languageModel()`. Routing is **per model**, not per provider, and sits on top of a region for the signer.
- `github-copilot`'s `getModel` (`:225-239`) prefers `model.api.endpoint` when the catalog declares one, else matches `/^gpt-(\d+)/` and picks `responses()` when `N >= 5` and the id is not `gpt-5-mini`, else `chat()`. Also **per model**.

So the surface choice lives in two places, not one: `Spec.surface` for a choice fixed at construction (Azure), `CompletionRequest.surface` for a choice made per model (Mantle, Copilot). A registry keyed only by provider name cannot express any of the three.

## Task 22 - message/part payload parity (crates/oc-db/src/message.rs)

### The `Part` union has TWELVE variants, not the nine the plan lists
`packages/schema/src/v1/session.ts:357-370` is the authority. Todo 22's list omits
`snapshot`, `agent` and `retry`. They are live: the real 1.18.12 binary's
`opencode export` decoded and re-emitted all three from rows this crate wrote.
Todos 34 (stream projection), 76 (TUI rendering) and 101 (FTS) must handle twelve.

### Discriminator and payload shape, per variant
`type` is the discriminator on every part. `req`/`opt` per Schema.optional.

| tag | required | optional |
|---|---|---|
| `text` (:102) | `text` | `synthetic`, `ignored`, `time{start,end?}`, `metadata` |
| `subtask` (:204) | `prompt`, `description`, `agent` | `model{providerID,modelID}`, `command` |
| `reasoning` (:118) | `text`, `time{start,end?}` | `metadata` |
| `file` (:171) | `mime`, `url` | `filename`, `source` (union on `type`: `file`/`symbol`/`resource`) |
| `tool` (:315) | `callID`, `tool`, `state` | `metadata` |
| `step-start` (:233) | *(none)* | `snapshot` |
| `step-finish` (:240) | `reason`, `cost`, `tokens{input,output,reasoning,cache{read,write},total?}` | `snapshot` |
| `snapshot` (:87) | `snapshot` | - |
| `patch` (:94) | `hash`, `files[]` | - |
| `agent` (:181) | `name` | `source{value,start,end}` |
| `retry` (:220) | `attempt`, `error`(APIError), `time{created}` | - |
| `compaction` (:195) | `auto` | `overflow`, `tail_start_id` |

`tool.state` is a **nested** union discriminated on `status` (:304-312):
`pending{input,raw}` / `running{input,time{start},title?,metadata?}` /
`completed{input,output,title,metadata,time{start,end,compacted?},attachments?}` /
`error{input,error,time{start,end},metadata?}`. `attachments` is `FilePart[]` -
i.e. **parts nested inside a part's payload**, each carrying its own
id/sessionID/messageID (those are NOT stripped when nested; only the top-level
row's are).

`message.data` keeps `role`, the `Info` discriminator (:490).
`user` (:332) requires `role`, `time{created}`, `agent`, `model{providerID,modelID,variant?}`.
`assistant` (:453) requires `role`, `time{created,completed?}`, `parentID`,
`modelID`, `providerID`, `mode`, `agent`, `path{cwd,root}`, `cost`, `tokens{...}`.

### The strip contract, both directions
`sql.ts:19-20` states it as a subtraction:
`V1MessageData = Omit<Info,"id"|"sessionID">`,
`V1PartData = Omit<Part,"id"|"sessionID"|"messageID">`.
Write side is `projector.ts:78-88`; read side is `message-v2.ts:80-93`, which puts
them back **from the columns**. Confirmed empirically: `opencode export` on a
Rust-written session printed `id`/`sessionID` inside `info`, sourced from the
columns, and no real `part.data` in a 1M-row production table contains any of the
three keys.

### Variants actually present in the user's real database
51 GiB `opencode.db`, 233 500 messages, **1 035 733 parts**, counted read-only:
`tool` 317980, `step-start` 218899, `step-finish` 217066, `reasoning` 129099,
`text` 108713, `patch` 42683, `compaction` 1255, `file` **37**, `subtask` **1**.
`snapshot`/`agent`/`retry` = 0 in this install.
The long tail matters: a fixture-only suite would plausibly get `file` and
`subtask` wrong and nothing would notice.

### Real rows carry fields the schema does not declare
A production `file` part has `synthetic`, which is **not** in `FilePart` at
`:171` (it is in `TextPart`). A strict typed decoder would have dropped it and
broken the round trip for every attachment a user has. This is why the blob is
carried as `serde_json::Map` and only the discriminator is typed - parity beats
type safety on a column whose writer is another program.

`step-start` blobs in production are literally `{"type":"step-start"}` - a
one-key object with no payload at all.

### `opencode export <sessionID>` is the sharpest available parity oracle
It decodes both `data` blobs through the TypeScript schema before printing, so a
wrong field name or an unknown tag fails there rather than silently rendering
wrong. Works headless with `--pure` and an isolated `HOME`/`XDG_*`, exits 0, and
prints the `{info, parts}` hydration shape. Costs under a second. Any future task
touching either blob should use it. `opencode db --pure --format json <sql>` (the
harness Todo 20 built) is the weaker cousin - it proves the file opens, not that
the payload decodes.

### rusqlite 0.40.1 statement tracing
No `set_authorizer` in this version. `Connection::trace_v2(TraceEventCodes::
SQLITE_TRACE_STMT, Some(f))` works and needs only `&self`; disable with
`TraceEventCodes::empty()` (there is no `SQLITE_TRACE_NONE` constant). The
callback is a bare `fn` pointer - it cannot capture - so a tally must live in a
`static AtomicUsize`. Requires the `trace` feature, added additively on the
crate's own `rusqlite` line without touching the root manifest.

## Task 21 — session CRUD, the three list scopes, the subtree delete

### `remove()`'s exact order of operations

`packages/opencode/src/session/session.ts:608-629`, with the two steps it defers
to elsewhere:

1. `:609` — `get(sessionID)`; a missing session fails before anything is deleted.
2. `:613-618` — `cancelBackgroundJobs(background, sessionID)`, guarded by a
   `hasInstance` check so a broken session can still be cleaned up without
   instance state. The filter is at `:940-955`: a running job whose `id`,
   `metadata.sessionId` **or** `metadata.parentSessionId` is this session.
3. `:619-622` — `const kids = yield* children(sessionID)`, then `remove(child.id)`
   for each. **The whole subtree delete is application code.** Children are fully
   removed before the parent.
4. `:624` — publish `SessionV1.Event.Deleted`, whose projector is the *only* real
   row delete: `projector.ts:259-261`,
   `db.delete(SessionTable).where(eq(SessionTable.id, event.data.sessionID))`.
5. `:625` — `events.remove(sessionID)` → `core/src/event.ts:513-523`:
   `DELETE FROM event_sequence WHERE aggregate_id = ?` **then**
   `DELETE FROM event WHERE aggregate_id = ?`, both inside one `db.transaction`.
   Order matters only cosmetically here (`event.aggregate_id` cascades from
   `event_sequence`), but both statements are explicit upstream.
6. `:626-628` — the whole body is inside a `try`, and a failure is *logged, not
   propagated*. Rust's `remove` returns the error instead; swallowing it would
   report a delete that did not happen.

`parent_id` has **no foreign key and no cascade** — the only FK on `session` is
`project_id → project(id) ON DELETE CASCADE` (`schema.rs:153-184`). So step 3 is
not an optimisation, it is the only thing that removes descendants.

Two tables need explicit sweeps beyond the declared cascades:

- **`part`** — `part.session_id` is `part_session_idx`, an index, never a
  constraint; the only FK on `part` is `message_id → message(id)`. A part whose
  message belongs to a *surviving* session is invisible to the cascade and
  outlives its own session with a dangling `session_id`. Reproduced in the
  fixture as `prt_orphan` and swept with `DELETE FROM part WHERE session_id = ?`.
- **`event` / `event_sequence`** — keyed by `aggregate_id`, a plain text column
  with no schema-visible relationship to `session.id`.

`parent_id` having no FK also means nothing prevents an `a → b → a` cycle, so the
subtree walk is iterative with a visited set rather than recursive. A cycle
terminates instead of overflowing the stack; there is a test for it.

### The three scopes' SQL predicates

The scopes are mutually exclusive because upstream's schema says so:
`ListInput = Schema.Union([ListDirectoryInput, ListProjectInput, ListAllInput])`
(`core/src/session.ts:56-76`). Modelled as an enum, not three `Option` fields.

| scope | predicate | oracle |
| --- | --- | --- |
| directory | `directory = ?` (exact, never a prefix) | `core/src/session.ts:274`, `session.ts:559` |
| project | `project_id = ?` | `core/src/session.ts:276` |
| project + subpath | `path = ? OR substr(path,1,length(?)+1) = ? \|\| '/'` | see below |
| global | none | `core/src/session.ts:294` (`undefined` where clause) |

Narrowing filters, all scope-independent: `workspace_id = ?` (`:275`),
`parent_id IS NULL` for roots (`session.ts:560`), `title LIKE '%?%'`
(`session.ts:563`), `<sort> >= ?` for `start` (`:561`), `<sort> < ?` for the
keyset `cursor` (`:562`), `time_archived IS NULL` (`:564`).

Ordering: `time_updated DESC, id DESC` (`session.ts:574`). The `id` tie-break is
load-bearing, not decorative — `time_updated` is a millisecond clock reading, so
two sessions in the same millisecond would otherwise come back in an arbitrary
order, and a keyset cursor over an unstable order skips or repeats rows. The
opt-in `created` sort swaps in `time_created`, which is what the v2 list uses
(`core/src/session.ts:272`).

Two upstream inconsistencies worth knowing, both surfaced rather than papered
over:
- `listGlobal` defaults `limit` to 100 (`:575`) and `listByProject` to 100
  (`:997`), while the v2 `list` applies none (`core/src/session.ts:299`). The
  Rust `ListQuery::limit` is `Option`, unset by default, with the constant
  exported as `UPSTREAM_LIST_LIMIT` so the request layer applies it where
  upstream does. A store that silently truncated at 100 is indistinguishable from
  a store that only had 100 rows.
- `listGlobal` hides archived sessions unless asked; `listByProject` has no
  archived handling and returns them. Rust makes it an explicit
  `ArchivedFilter`, defaulting to hiding nothing.

### What `subpath` should filter on

`session.path`, which is the **worktree-relative** directory, written once at
creation by `sessionPath` (`session.ts:171-173`):
`path.relative(path.resolve(worktree), cwd).replaceAll("\\", "/")`. Called at
both create sites, `:683` and `:699`.

Consequences that matter:
- **A session at the worktree root stores `""`, not `NULL`.** `path.relative` of
  a directory against itself is the empty string and `toRow` stores it verbatim.
  Upstream then treats that empty string as *absent* in two places —
  `info.ts:42` (`row.path ? make(row.path) : undefined`) and `listByProject`'s
  `if (input.path)` guard (`:969`) — which is why writing `NULL` instead would
  land in a different branch of the oracle's
  `OR (path IS NULL AND directory = ?)` arm (`:980-984`).
- The comparison is **lexical**, never filesystem-resolved. `path.relative` does
  not touch the disk, does not resolve symlinks, and does not require either path
  to exist. `std::fs::canonicalize` does all three and would diverge on any
  worktree reached through a symlink (macOS `/tmp → /private/tmp`, a symlinked
  home). `src/session/path.rs` reimplements Node's `normalizeString` textually.
- Prefix semantics come from the legacy filter (`session.ts:969-984`): the
  subpath itself **plus everything beneath it**, i.e. `path = ?` OR
  `path LIKE ? || '/%'`. The trailing `/` is what stops `pkg` from matching
  `pkgx`.

### `create`'s field set

`session.ts:513-533`: `id` (`SessionID.descending()`), `slug` (`Slug.create()`),
`version` (`InstallationVersion`), `projectID` from `ctx.project.id`, `directory`,
`path`, `workspaceID`, `parentID`, `title` defaulting to
`(parentID ? "Child session - " : "New session - ") + new Date().toISOString()`
(`:48-49`, `:523`), `agent`, `model`, `metadata`, `permission`, `cost: 0`,
`tokens: EmptyTokens`, `time.created == time.updated == Date.now()`.

The projector inserts with `onConflictDoNothing` and treats a conflict as
`SessionAlreadyProjected` (`projector.ts:215-224`), which the create path
resolves in favour of the row already on disk — "Concurrent creation lost the
projection race. The existing Session identity wins." (`core/src/session.ts:249-259`).
Rust returns a `Creation::{Inserted, AlreadyExists}` enum so a caller can tell a
fresh session from one it lost a race for; returning a bare row would hide it.

`id` and `slug` are **not** generated in this module. Upstream generates them
outside the store too, and both are identifier concerns with their own byte
format (`packages/schema/src/identifier.ts`: 26 chars, 6 hex bytes of
`~(timestamp * 0x1000 + counter)` then 20 base-62 random) — a later id todo owns
that, not the storage layer.

### `touch`

`session.ts:751-753` — `patch(sessionID, { time: { updated: Date.now() } })`,
nothing else. Because it goes through `patch`, which reads the session first, a
missing session is an error rather than a silent no-op; Rust reports
`DbError::NotFound` and asserts it.

## Task 31
- Per-provider effort fields from the TypeScript oracle: OpenAI/Azure/Mantle and compatible providers use `reasoningEffort` (`transform.ts:935-974,1750-1769`); Anthropic uses `thinking.budgetTokens` or adaptive `thinking` plus top-level `effort` (`:976-1022,1779-1800,1807-1809`); Bedrock uses `reasoningConfig` (`:1024-1069,1813-1814`); Google/Vertex uses `thinkingConfig` (`:703-718,1071-1075,1810-1812`); OpenRouter uses `reasoning.effort` (`:745-750,801-808`).
- The default reasoning path also matters to provider adapters: `transform.ts:1273-1297` installs medium effort plus reasoning summary/encrypted reasoning where supported, and `:1352-1410` gates and namespaces those options.
- Exact prompt split: the reference puts base prompt, self-dev guidance, AGENTS instructions, prompt overlays, preferred-tool guidance, and the available-skill catalog in the static prefix (`jcode-base/src/prompt.rs:478-530`). Memory and active skill begin the dynamic side at `:533-547`; per-turn reminders and effort directives append to that dynamic side (`jcode-app-core/src/agent/prompting.rs:70-124`). This implementation freezes only caller-supplied project/session-stable text in `PromptCache::new`; all memory and turn context must enter through `DynamicContext`, which creates the final user message.
- The integrated three-turn test proved identical static bytes while dynamic clock/memory changed, append-only history grew, and late MCP caused one tool-list rebuild only.

## Task 28

### Full stream event vocabulary (24 variants)
`TextDelta`, `ToolUseStart`, `ToolInputDelta`, `ToolUseEnd`, `ToolUseSignature`, `ToolResult`, `GeneratedImage`, `ReasoningStart`, `ReasoningDelta`, `ReasoningSignatureDelta`, `ProviderReasoningItem`, `ReasoningEnd`, `ReasoningDone`, `MessageEnd`, `RetryRollback { attempt, max }`, `TokenUsage`, `ConnectionType`, `ConnectionPhase`, `StatusDetail`, `Error { message, retry_after }`, `SessionId`, `Compaction`, `UpstreamProvider`, `NativeToolCall`.

### Five reasoning representations and why they cannot be collapsed
1. `ContentBlock::Reasoning` is unsigned/plain reasoning. It remains in the transcript but generic replay excludes it because providers such as Anthropic reject thinking without the matching signature.
2. `ContentBlock::ReasoningTrace` is history/debug-only. It remains available for recall but is never replayed, preventing permanent token cost with no model benefit.
3. `ContentBlock::SignedThinking { thinking, signature }` preserves the provider signature beside the text, making later replay valid.
4. `ContentBlock::ProviderEncryptedReasoning { id, summary, encrypted_content, status }` preserves a provider-native replay item, notably OpenAI Responses reasoning state when `store=false`.
5. `ThoughtSignature` is a separate newtype stored per `ToolUse`, not a reasoning-block signature. Gemini 3 requires it on the matching future `functionCall`; dropping or misattaching it breaks multi-turn tool use.

### Verified Part count and projection reachability
`packages/schema/src/v1/session.ts:357-370` declares exactly 12 variants, in order: `text`, `subtask`, `reasoning`, `file`, `tool`, `step-start`, `step-finish`, `snapshot`, `patch`, `agent`, `retry`, `compaction`. This confirms Todo 22's correction. Provider events directly carry text, reasoning, tool, file/image, step completion/usage, retry, and compaction data. Step start, snapshot, patch, agent, and subtask are generated from engine/session context at the corresponding event boundary; they are not payloads a provider sends, so the 24-variant provider vocabulary is complete without inventing provider events for engine-owned data.

`StreamEvent::Error.retry_after` uses `Option<std::time::Duration>`, matching `oc_error::ProviderError::RateLimited` rather than losing precision in an integer-seconds field.

## Task 38 — `oc-tool`: the tool trait, schemars schemas, central augmentation

**Todos 39-44, 47, 65, 70, 99-100 implement against this.** Read this section before
writing a tool; you should not need to write a JSON schema or a `ToolContext`.

### The trait you implement: `TypedTool`

```rust
#[async_trait]
pub trait TypedTool: Send + Sync + 'static {
    type Params: JsonSchema + DeserializeOwned + Send;
    fn id(&self) -> &str;
    fn description(&self) -> &str;
    async fn run(&self, params: Self::Params, ctx: ToolContext) -> Result<ToolOutput, ToolError>;
}
```

Declare a params struct with `#[derive(Deserialize, JsonSchema)]`, doc-comment each
field (schemars turns them into the `description` the model reads), use `Option<T>`
for optional fields (that is where `required` comes from). **Write no JSON.** Then
`oc_tool::erase(MyTool)` gives you `Arc<dyn Tool>` for the registry.

`#[serde(deny_unknown_fields)]` is safe: the injected properties are stripped from the
arguments before your params struct sees them, and schemars emits
`additionalProperties: false` which the injected properties satisfy because they are in
`properties`.

### The object-safe trait the registry stores: `Tool`

```rust
#[async_trait]
pub trait Tool: Send + Sync {
    fn id(&self) -> &str;
    fn description(&self) -> &str;
    fn raw_parameters_schema(&self) -> Value;      // un-augmented, named for it
    async fn execute(&self, args: Value, ctx: ToolContext) -> Result<ToolOutput, ToolError>;
    fn definition(&self) -> ToolDefinition { /* THE augmentation point */ }
}
```

Implement `Tool` **directly only for MCP proxies** (todo 47), where no Rust type
describes the parameters. `ToolDefinition { id, description, parameters }`.

### The augmentation: injected keys and their cost

Injected by `Tool::definition` into every object schema, derived or proxied:

| key | type | in `required`? | description |
|---|---|---|---|
| `intent` | `string` | **yes** | `Required short label shown in the UI: why this call is being made.` |
| `accept_large_output` | `boolean` | no | `Re-run accepting the stated token cost of a withheld result.` |

**Cost: 237 bytes of compact JSON per tool per request** (measured: 373 → 610 on a
three-field params struct; `tests/schema_augmentation.rs` pins the ceiling at 260).
With ~19 tools exposed that is ~4.5 KB on every request for the life of a session, so
if you add a third cross-cutting property, justify it against that number. Descriptions
are deliberately terse; the long explanation belongs in the refusal message, which is
only rendered when relevant.

Read the keys back with `guard::intent(&args) -> Option<&str>` and
`guard::accepts_large_output(&args) -> bool`. Never re-type the literals — use
`schema::INTENT_KEY` / `schema::ACCEPT_LARGE_OUTPUT_KEY`.

Three deliberate schemars settings, each paid per request: **draft-07** (what
tool-calling APIs consume; the 2020-12 default uses `$defs`/`$dynamicRef` providers do
not implement), **`inline_subschemas = true`** (a `$ref` hop providers handle
inconsistently), and **`$schema` + `title` stripped** (46 bytes no provider reads, plus
the Rust type name, which says nothing the tool's id does not).

### `ToolContext` fields

```rust
pub struct ToolContext {
    pub session_id: String,
    pub message_id: String,
    pub call_id: String,
    pub agent: String,
    pub depth: u32,                            // 0 at turn level; for_subcall increments
    pub permission: Arc<dyn PermissionAsker>,
    pub interrupt: Arc<dyn InterruptHandle>,
}
```

Methods: `new(...)`, `for_subcall(call_id)`, `is_interrupted() -> bool` (sync, no
runtime needed), `ask(tool, PermissionAsk) -> Result<(), ToolError>`, `tool_call() ->
oc_permission::ToolCall`. `Clone` is cheap and shares the two collaborators.

Test helpers exported: `AllowAll`, `DenyAll`, `NeverInterrupted`.

`PermissionAsk { permission, patterns, metadata, always }` — the oracle's
`Omit<Request, "id"|"sessionID"|"tool">`. `PermissionAsk::new(permission, pattern)` for
the common case; `into_request(id, session_id, tool)` completes it. Map your tool id to
its permission key with `oc_permission::visibility::permission_key` — do **not** pass
the raw tool id (`edit`/`write`/`apply_patch` share one key).

### `ToolOutput`

```rust
pub struct ToolOutput {
    pub title: String,
    pub output: String,
    pub metadata: serde_json::Map<String, Value>,
    pub attachments: Vec<Attachment>,
}
```

`ToolOutput::text(title, output)`, `.with_metadata(k, v)`, `.with_attachment(a)`.
`Attachment { mime, filename, url, source }` serializes to the oracle's `FilePart`
minus the three ids: `{"type":"file","mime":…,"filename":…,"url":…}`. `source` is a
`Value` on purpose — it is the `FileSource | SymbolSource` union the message-part layer
owns, and re-declaring it here would be a second copy.

### Size detection (NOT policy)

`OutputLimits::from_config(Option<&ToolOutputConfig>)` → defaults 2000 lines /
51200 bytes, **each field defaulted independently** (the oracle applies `??` at read
time, so a config setting only `max_lines` keeps the default `max_bytes`).

`measure(text, limits) -> SizeMeasurement { lines, bytes, limits, verdict }`;
`verdict: SizeVerdict::{WithinLimits, Oversized(LimitExceeded::{Lines,Bytes,Both})}`.
Lines = `'\n'` count + 1 (so `""` is 1 line), bytes = UTF-8 bytes. Limits are
**inclusive** — exactly at the limit fits, matching the oracle's `<=`.

`ToolOutputStore::{in_layout, new, persist, read, entries}` writes the full text with
`create_new` (the oracle's `flag: "wx"`). `ToolOutput::record_output_path(&path)`
appends to the `outputPaths` metadata key; `output_paths()` reads it back.

**This crate does not truncate and does not decide what the model sees on overflow.**
That is todo 72's alone.

## Task 26

### The three catalog env vars, exactly

| var | parsing | effect |
|---|---|---|
| `OPENCODE_MODELS_URL` | JavaScript `||` — **`""` means unset** (`models-dev.ts:160`) | changes the source **and** the cache filename |
| `OPENCODE_MODELS_PATH` | raw value (`flag.ts:46`) | read this file **instead of the cache**; never written to |
| `OPENCODE_DISABLE_MODELS_FETCH` | `Flag.truthy` (`flag.ts:3-6`) — only `"1"` and case-insensitive `"true"` | no fetch, ever, including the 60-minute startup refresh |

`=0`, `=no`, `=yes`, `=2` all leave fetching **enabled**. `oc-paths::Env` already has
both parsers (`truthy_value` and `flag`); do not re-parse strings.

### Cache path rules
- default source → `<cache>/opencode/models.json`
- any other source → `<cache>/opencode/models-<sha1(source)>.json`, SHA-1 over the
  URL's raw UTF-8 bytes, lowercase hex (`models-dev.ts:161-164`), so a mirror cannot
  poison the default cache.
- `oc_paths::Layout::models_cache()` / `models_cache_for_source()` already implement
  this. `XDG_CACHE_HOME` is the only override; `OPENCODE_CONFIG_DIR` does **not**
  affect the cache.
- TTL is 5 minutes (`:165`); write is temp-file-then-rename with pid+millis in the
  temp name (`:202-215`).
- A corrupt **cache** is deleted and treated as a miss; a corrupt/absent **explicit
  path** is an error and is never deleted (`:184-196`). The asymmetry is deliberate:
  a cache is ours, a path is the user's instruction.

### Availability precedence, verified in isolation against 1.18.12
Three independent sources, applied in this order, **last sufficient one wins**:
1. env var — first *declared* var (catalog order) with a non-empty value (`provider.ts:1527`)
2. stored auth — **only `type: "api"`** (`:1540`)
3. config — the mere existence of a `provider.<id>` block (`:1588-1595`)

Each verified alone with a pinned catalog and isolated HOME: env var alone → provider
appears; `auth.json {"type":"api"}` alone → appears; `{"provider":{"groq":{}}}` alone,
no credential at all → appears. **`auth.json {"type":"oauth"}` alone → NOTHING.**
OAuth providers reach availability through their own `custom()` loader, which knows
how to refresh; the generic path must not guess. Todos 29/30/94/95/96 own that.

### `opencode models` sorts with ICU collation, not byte order
`models.ts:38`/`:56-62` use `localeCompare`. It disagrees with `str::cmp` on real
catalog data (`"glm-5-turbo" < "glm-5.1"` under ICU, the reverse byte-wise). Every id
in the whole 180-provider catalog is ASCII over 68 characters; over that alphabet
`localeCompare` = primary level (punctuation `_ - : . @ / ~`, then digits, then
case-folded letters, prefix-first) + tertiary case level (lowercase first). Verified
over **all 4,753,986 pairs** of the 3,084 distinct ids: zero disagreements. Ported in
`catalog/collate.rs`. Provider ids additionally float every `opencode*` id to the front.

### With fetching disabled and no cache, the released binary is NOT empty
`models-dev.ts` has three fallbacks, not one: cache (`:218`), then a **catalog
snapshot compiled into the binary** (`OPENCODE_MODELS_DEV`, `:198-200`, read `:220-221`),
then `{}` (`:222`). Rung 2 is live — `OPENCODE_DISABLE_MODELS_FETCH=1` with an empty
cache still listed 7 `opencode/*` models and exited 0, writing no cache. So `return {}`
is essentially unreachable in a release build. Any todo that quotes "returns an empty
catalog" as the oracle's behaviour is quoting rung 3.

### A model with no catalog entry, on `@ai-sdk/openai-compatible`, whose wire id
contains `deepseek`, defaults to `interleaved: {field: "reasoning_content"}`
(`provider.ts:1485-1487`), gated on there being no existing entry. Real quirk, ported.

### `tool_call` defaults to **`true`**
Alone among the capability booleans (`provider.ts:1464`; every other one defaults
`false`). A `false` default makes every config-declared model refuse to call tools.

### Declaring `modalities` turns the undeclared ones OFF
`:1466-1481` reads each flag independently, so `{"modalities":{"input":["image"]}}`
turns image on **and text off**. Only an entirely absent `modalities` block inherits.

## Task 96
- Gemini request lowering is native: `systemInstruction.parts` holds system text; non-system messages become ordered `contents[]` entries with `role: user|model`; each block becomes a Gemini `parts[]` member (`text`, `inlineData`, `functionCall`, or `functionResponse`). No OpenAI-shaped serializer is used.
- Verified canonical `thinkingConfig` mapping through `oc_llm::effort`: `off -> {includeThoughts:false,thinkingBudget:0}`; `low -> {includeThoughts:true,thinkingLevel:"low"}`; `medium -> {...,"medium"}`; `high|xhigh|max -> {...,"high"}`. Declared token-budget capability still selects `thinkingBudget`, including catalog maximum clamping.
- `thoughtSignature` is a sibling of `functionCall` in one Gemini part. The stream decoder emits `ToolUseSignature(ThoughtSignature)` immediately after that tool input; the next-turn request places the unchanged value back on the matching assistant `functionCall` part.
- Verified endpoint rules: Vertex Gemini uses `aiplatform.googleapis.com` for `global` and `<region>-aiplatform.googleapis.com` otherwise. Vertex-Anthropic uses `aiplatform.{us|eu}.rep.googleapis.com` for continental `us`/`eu`, `aiplatform.googleapis.com` for `global`, and `<region>-aiplatform.googleapis.com` for ordinary regions. Paths end in `:streamGenerateContent?alt=sse` for Gemini and `:streamRawPredict` for Anthropic.

## Task 29 — Anthropic provider

- Anthropic signed thinking is a three-stage wire protocol: `content_block_start`
  identifies a `thinking` block, `thinking_delta` appends visible reasoning, and
  `signature_delta` appends the opaque signature. The provider emits canonical
  `ReasoningStart`, `ReasoningDelta`, `ReasoningSignatureDelta`, and
  `ReasoningEnd` events; `StreamAccumulator` then keeps reasoning and signature
  together for `RequestContentBlock::SignedThinking` replay.
- Tool JSON arrives as one or more `input_json_delta.partial_json` fragments. The
  provider brackets each call with `ToolUseStart`/`ToolUseEnd` and forwards every
  fragment as `ToolInputDelta`; it does not parse incomplete JSON or execute tools
  during streaming.
- Anthropic usage is split across `message_start.message.usage` and
  `message_delta.usage`. Input, cache-read, and cache-creation tokens come from the
  first event; final output tokens come from the latter. The decoder retains all
  four counters in one canonical `TokenUsage` event.
- API-key and OAuth modes use different wire headers. API keys use `x-api-key`;
  OAuth credentials use `Authorization: Bearer ...` plus Anthropic's OAuth beta
  header. Both modes resolve credentials through `oc-auth`; cassette recordings
  intentionally cannot prove auth headers because the recorder redacts secrets.
- Cache control belongs on stable prompt content only. The serializer marks the
  final static system block with `cache_control: {type:"ephemeral"}` and does not
  mark dynamic message content. The real cache cassette confirms cache-creation
  tokens on the first request and cache-read tokens on the second.
- A successful response may report a model different from the requested model.
  The stream surfaces this once as `StreamEvent::StatusDetail` rather than silently
  accepting the substitution or treating it as a transport failure.

## Task 95
- AWS EventStream has 16 bytes of fixed framing overhead: the 8-byte big-endian
  length prelude, 4-byte prelude CRC, and 4-byte message CRC. Absolute stream
  offsets must be retained before draining the incremental buffer so corrupt-frame
  diagnostics remain useful under arbitrary transport chunking.
- Bedrock's outer `chunk` payload contains base64-encoded provider-native JSON, so
  decoding requires two independent incremental UTF-8/JSON buffers: one for the
  EventStream payload and one for the decoded inner bytes.
- The published AWS S3 SigV4 vector is suitable for a Bedrock signer because the
  signing algorithm is service-independent. The byte-exact expected signature is
  `f0e8bdb87c964420e857bd35b5d6ed310bd44f0170aba48dd91039c6036bdb41`.
- A full split sweep over the recorded 1,077-byte stream proved framing output is
  invariant at every byte boundary; corrupting the final CRC produced a typed
  error at absolute byte offset 149 rather than a panic.

## Task 94 — the OpenAI-compatible profile: verified oracle rules and claimed ids

### The Azure rule is NOT a model-id rule — the plan's wording is wrong

`packages/opencode/src/provider/provider.ts:154-160`:

```ts
function selectAzureLanguageModel(sdk: any, modelID: string, useChat: boolean) {
  if (useChat && sdk.chat) return sdk.chat(modelID)
  if (sdk.responses)       return sdk.responses(modelID)
  if (sdk.messages)        return sdk.messages(modelID)
  if (sdk.chat)            return sdk.chat(modelID)
  return sdk.languageModel(modelID)
}
```

Called from `:265` (`azure`) and `:285` (`azure-cognitive-services`), both as
`selectAzureLanguageModel(sdk, modelID, Boolean(options?.["useCompletionUrls"]))`.

**`modelID` is accepted and never read.** Azure's selection is a surface-
availability walk gated by one provider option. Todo 94's acceptance criterion
asks for "Azure's model selection … per model id"; implementing that literally
would invent behaviour the oracle does not have, and a test could then "confirm"
a per-model Azure behaviour that does not exist. `surface.rs::azure_surface`
therefore takes **no** `model_id` parameter, and
`azure_picks_the_same_endpoint_for_every_model_id` asserts the absence of the
dependency over 6 model ids. Only Copilot is genuinely per-model.

Two Azure ids share the selector; they differ only in how the base URL is
assembled (`:240-278` builds from a resource name; `:279-292` builds
`https://<name>.cognitiveservices.azure.com/openai[/v1]`). That is spec data, so
one `SurfaceRule::Azure` row serves both.

### The Copilot rule, verified — `provider.ts:225-239`

```ts
if (sdk.responses === undefined && sdk.chat === undefined) return sdk.languageModel(modelID)
if (model && "endpoint" in model.api) {
  if (model.api.endpoint === "responses" && sdk.responses) return sdk.responses(modelID)
  if (model.api.endpoint === "chat"      && sdk.chat)      return sdk.chat(modelID)
}
const match = /^gpt-(\d+)/.exec(modelID)
if (match && Number(match[1]) >= 5 && !modelID.startsWith("gpt-5-mini")) return sdk.responses(modelID)
return sdk.chat(modelID)
```

Three tiers: declared endpoint, then a `gpt-N` **version comparison** with one
explicit exclusion, then chat. The version comparison is why this does not
reintroduce the model-id literals that `oc-llm`'s
`policy_sources_contain_no_model_id_literals` forbids — the only literal is
`gpt-5-mini`, the oracle's own exclusion, which a version comparison cannot
express. `gpt-41` routes to `/responses` because the regex captures all digits.
`gpt-oss-20b` routes to `/chat/completions` because no digit follows `gpt-`.

One divergence, documented at `copilot_surface`: after the declared-endpoint
block the oracle calls `sdk.responses(...)` / `sdk.chat(...)` with no presence
check, so a chat-only SDK plus `gpt-5` throws a `TypeError`. This port falls back
to the available surface. No configuration the first guard admits can observe a
different *successful* answer; only the crash is removed.

### The 21 provider ids this profile claims

`alibaba, azure, azure-cognitive-services, cerebras, cloudflare-ai-gateway,
cloudflare-workers-ai, cohere, deepinfra, deepseek, github-copilot, gitlab, groq,
meta, mistral, openai-compatible, openrouter, perplexity, togetherai, venice,
vercel, xai` — plus any id whose config declares
`provider.<id>.options.npm = "@ai-sdk/openai-compatible"`, which is the same
opt-in the oracle reads (`provider.ts:108`, consumed at `:1198` and `:1485`).
Silence is never consent: an unlisted, undeclared id is refused.

Three of the claimed ids do not take the SDK default surface, per the oracle's own
custom loaders: `xai` → `responses` (`:212-217`), `meta` → `responses`
(`:218-223`), and OpenRouter/Vercel are pinned to `chat` because they are routers.
`openrouter` and `vercel` additionally carry `routes_upstreams: true` — both put
the resolved upstream in a top-level `provider` field on each chunk.

### Six ids refused, with destinations

`amazon-bedrock` → `oc-provider-bedrock` (95); `anthropic` →
`oc-provider-anthropic` (29); `google`, `google-vertex`,
`google-vertex-anthropic` → `oc-provider-google` (96); `openai` →
`oc-provider-openai` (30). The refusal names the crate **and** why the wire
differs, because "unsupported" alone sends the reader nowhere.

### Cassettes replayed — 6 distinct vendors this profile claims

| vendor | host | cassette |
|---|---|---|
| DeepSeek | `api.deepseek.com` | `openai-compatible-chat/deepseek-streams-text` |
| Groq | `api.groq.com` | `openai-compatible-chat/groq-streams-tool-call` |
| OpenRouter | `openrouter.ai` | `openai-compatible-chat/openrouter-streams-text` |
| TogetherAI | `api.together.xyz` | `openai-compatible-chat/togetherai-streams-tool-call` |
| Cloudflare Workers AI | `api.cloudflare.com` | `cloudflare-workers-ai/…gpt-oss-20b-tools-tool-call` |
| Cloudflare AI Gateway | `gateway.ai.cloudflare.com` | `cloudflare-ai-gateway/…gpt-oss-20b-tools-tool-call` |

The corpus holds **seven** OpenAI-compatible endpoints across 11 route
directories / 40 files. The seventh is `api.openai.com` itself, whose provider id
this profile **refuses**, so it is replayed only under a user-declared compatible
id as canonical-shape evidence and is not counted toward the four.

### What the corpus taught that no specification says

- **`reasoning_content` exists only in the two Cloudflare cassettes.** Fourteen
  `delta.reasoning_content` fragments each, and **the first is the empty string** —
  so a reasoning block must open on the field's *presence*, not on non-empty
  content. Nothing in the corpus emits `delta.reasoning`, `delta.thinking` or
  `reasoning_details`; the profile reads `reasoning` too because gateways do send
  it, but that is not corpus-verified.
- **OpenRouter opens with an SSE comment frame**, `: OPENROUTER PROCESSING`. It
  must not become an event. `oc_llm::sse::parse_frame` already drops `:`-prefixed
  lines, which is why consuming that parser rather than writing one mattered here.
- **Groq sends a tool call whole** — name and complete arguments in one fragment —
  while **TogetherAI splits it**, sending `id`+`name` with empty arguments, then a
  second fragment with arguments only, no `id`, no `name`. Both must produce one
  bracketed call. Tool-call identity across chunks is `index`, not `id`.
- **Duplicate terminal chunks are normal.** OpenRouter, TogetherAI and both
  Cloudflare endpoints each send `finish_reason` on two chunks. Exactly one
  `MessageEnd` may be emitted. Groq repeats its `usage` on a final
  `choices: []` chunk — that second `TokenUsage` is kept, because suppressing it
  would hide a real duplicate from whoever reconciles accounting.
- **`tool_calls: null` and `usage: null` both occur.** `#[serde(default)]` covers
  an absent key, not a present null, so a `Vec` field needs an explicit
  null-tolerant deserializer.
- **Vendors carry fields no schema mentions**: `obfuscation`,
  `system_fingerprint`, `service_tier`, `x_groq`, `token_ids`, `stop_reason`,
  `native_finish_reason`, `seed`, and a top-level `text`/`role` on TogetherAI's
  `choices[]`. A `deny_unknown_fields` reader would turn every vendor release into
  an outage.

## Task 32

### Single-loop phase order

- Oracle `prompt.ts:1055-1058`: load the session, clean revert state, persist the user message, then touch the session. `run_turn` receives an already-persisted user message, verifies the session, and touches it before emitting `TurnStarted`.
- Oracle `prompt.ts:1088-1097`: enter the one loop, mark work active, hydrate/filter history, and find the latest user/assistant/task state. The Rust loop hydrates once per provider step and derives agent/model from the latest stored user message.
- Oracle `prompt.ts:1100-1129`: a nominal `stop` does not exit while a non-orphaned tool call exists. The Rust loop likewise dispatches and re-enters whenever the completed stream accumulated a tool call, independent of the provider stop reason.
- Oracle `prompt.ts:1141,1170-1201`: resolve model and agent, then persist the assistant shell before streaming. The Rust order is `AgentResolved` -> `ModelResolved` -> `AssistantMessageCreated`.
- Oracle `prompt.ts:1226-1241`: resolve tools before request processing. The Rust loop calls `AvailableTools`, passes the definitions through `PromptCache<ToolDefinition>`, freezes the resulting snapshot, and emits `ToolSnapshotLocked`.
- Oracle `prompt.ts:1255-1286`: convert stored messages, assemble system state, then process the provider stream. The Rust conversion emits provider-safe `RequestContentBlock` values and puts volatile context only at the trailing-message cache boundary.
- Oracle `prompt.ts:1288-1335`: checkpoint/finalize and either stop or continue. Rust checkpoints the assistant, dispatches tool calls sequentially through one seam, persists each result, emits `StepCompleted`, and re-enters the same loop.
- Oracle `prompt.ts:1343-1347`: the run-state layer guarantees one live loop per session. Todo 37 must wrap this exact `run_turn`; it must not add a second state machine.

### Event vocabulary

`TurnStarted`, `HistoryRepaired`, `AgentResolved`, `ModelResolved`, `AssistantMessageCreated`, `ToolSnapshotLocked`, `ProviderRequestStarted`, raw provider `Provider { event }`, `AssistantCheckpointed`, `ToolDispatchStarted`, `ToolDispatchCompleted`, `ToolResultAppended`, `StepCompleted`, `TurnCompleted`, and `TurnInterrupted`.

### Tool-result repair

Before every provider request, every stored `tool` part whose state is not terminal (`completed` or `error`) is rewritten to `error` with `[Tool execution was interrupted]` and `metadata.interrupted=true`. Conversion then always places a matching `ToolResult { is_error: true }` after the `ToolUse`; the fake-provider test proves the provider receives the repaired pair and the database contains the repair.

### Duplicate-loop cost observed in jcode

`turn_loops.rs` is 1,193 lines and `turn_streaming_mpsc.rs` is 1,720 lines: 2,913 lines of parallel state machine. The former has the cancel-at-loop-head guard at lines 35-41; the latter has the stream-wait cancel arm at lines 367-391. Keeping two copies made their mid-stream cancel behavior diverge. Task 32 keeps both checks in one function.

## Task 37
- Oracle single-loop rule: `packages/opencode/src/session/prompt.ts:1343-1347` delegates every prompt loop to `state.ensureRunning(sessionID, ...)`; `run-state.ts:57-68,88-94` keeps one runner per session and reuses it. Rust reproduces the safety boundary with one exclusive `SessionRunGuard`; a second `begin_turn` is rejected.
- Stale-control lookup semantics: `SessionControl` stores only `session_id` plus the shared registry. `abort()` resolves the current `ActiveSession` at call time and fires that live signal, never a signal captured when the handle was created.
- Busy state is in-process only. Oracle `packages/opencode/src/session/status.ts:26-48` stores non-idle entries in an instance-local `Map` and deletes them on idle; Rust likewise stores active entries only in `SessionRunRegistry`, with no SQLite persistence.
- Turn cleanup captures the interrupt epoch and calls `reset_if_epoch(epoch)` after unregistering. This preserves a newer concurrent fire instead of clearing it.
## Task 36
- Budgets: `MAX_CONTEXT_LIMIT_RETRIES = 5` bounds repeated compaction when a still-oversized conversation cannot be made to fit; attempts 1..=5 are accepted and attempt 6 returns `RetryError::BudgetExhausted`.
- Budgets: `MAX_INCOMPLETE_CONTINUATION_ATTEMPTS = 3` prevents repeated length/incomplete stops from continuing forever; attempts 1..=3 are accepted and attempt 4 returns the typed exhaustion error.
- Budgets: `MAX_EMPTY_POST_TOOL_CONTINUATION_ATTEMPTS = 5` prevents an empty response after tools from silently ending a run or looping forever; this is the turn-43 failure that ended a 20-hour run half-done. Attempts 1..=5 are accepted and the sixth observation fails with an error naming `empty-post-tool-continuation attempts`.
- `ProviderError::retry_after()` wins exactly when present. Without it, retry delay follows the OpenCode oracle: 2s exponential backoff (2s, 4s, 8s, 16s) capped at 30s.
- `ProviderError::ContextLimit`, `Auth`, `Refused`, and `Fatal` are never replayed as provider retries; decisions use the enum and `is_retryable()`, never rendered text.


## Task 34

### Complete StreamEvent -> Part mapping

| Persisted `Part` variant | Producing stream events / lifecycle | Projection behavior |
|---|---|---|
| `text` | `TextDelta`; terminal `MessageEnd` or `finish_incomplete` | Accumulates incrementally in memory; upserts at the 4096-byte batch window and once at terminal completion. |
| `subtask` | None | A delegation/user-input concern, not a provider-stream event; this projector cannot produce it. |
| `reasoning` | `ReasoningStart`, `ReasoningDelta`, `ReasoningSignatureDelta`, `ReasoningEnd`, `ReasoningDone`; `ProviderReasoningItem` | Incremental reasoning is batched like text; signature/duration are terminal metadata. Provider-native encrypted items become separate reasoning parts carrying provider metadata. |
| `file` | `GeneratedImage` | Persists the generated image path/mime and provider metadata as a file part. |
| `tool` | `ToolUseStart` + `ToolInputDelta` + `ToolUseEnd`; `ToolUseSignature`; `ToolResult`; `NativeToolCall`; truncated-stream `finish_incomplete` | Raw input fragments accumulate without parsing. The complete input is parsed once at end; provider results complete/error the same part; truncated/invalid input becomes a synthetic error part. |
| `step-start` | Projector `start` lifecycle, before the first stream event | Persists the optional pre-step snapshot. |
| `step-finish` | `MessageEnd` | Persists finish reason, optional completed snapshot, tokens, cache tokens and cost; also updates assistant message accounting. |
| `snapshot` | None | The oracle embeds snapshot hashes in `step-start`/`step-finish`; no provider stream event produces the standalone legacy/schema variant. |
| `patch` | `MessageEnd` terminal effects | Diffs the pre-step snapshot and persists hash/files when the patch is non-empty. |
| `agent` | None | Agent mentions are parsed from user input/configuration, not provider-stream output; this projector cannot produce them. |
| `retry` | `RetryRollback { attempt, max }` | Deletes all flushed parts from the abandoned attempt, clears accumulators, then persists a retry marker. |
| `compaction` | `Compaction { trigger, pre_tokens, openai_encrypted_content }` | Persists an auto compaction boundary with overflow and provider metadata. |

Non-part status events are intentionally observed without persistence here: `ConnectionType`, `ConnectionPhase`, `StatusDetail`, `Error`, `SessionId`, and `UpstreamProvider`; Todo 37 owns status projection. `TokenUsage` accumulates accounting consumed by `MessageEnd` rather than creating its own part.

### Measured batching

The `stream_five_thousand_text_deltas_are_batched_below_the_documented_write_bound` test fed 5,000 one-byte `TextDelta` events. Measured `delta_writes=2`; asserted bound `<= 2` (one 4096-byte size-window upsert plus one terminal upsert). The resulting DB state contained one text part with exactly 5,000 bytes.

## Task 33

### Alias resolver (single choke point)

Source shape: `.omo/refs/jcode/crates/jcode-tool-types/src/lib.rs:74-141`; canonical lookup and miss recovery are both in `crates/oc-engine/src/dispatch.rs::ToolRegistryDispatcher::dispatch`. A leading `functions.` namespace is stripped before the table is applied; any other namespace is preserved.

Complete implemented aliases (jcode names are adapted to this plan's opencode wire IDs):

- `communicate` -> `task`
- `task_runner`, `subagent` -> `task`
- `launch` -> `open`
- `shell`, `shell_exec` -> `bash`
- `read_file`, `file_read` -> `read`
- `write_file`, `file_write` -> `write`
- `edit_file`, `file_edit` -> `edit`
- `file_grep` -> `grep`
- `skill_manage` -> `skill`
- `discover_tools` -> `integration_tools`
- `todoread`, `todo_read`, `todo_write`, `todos`, `todo` -> `todowrite`
- PascalCase: `Bash` -> `bash`, `Read` -> `read`, `Write` -> `write`, `Edit` -> `edit`, `Grep` -> `grep`, `Agent`/`Task` -> `task`, `Skill` -> `skill`, `WebFetch` -> `webfetch`, `WebSearch` -> `websearch`, `TodoWrite` -> `todowrite`, `ApplyPatch` -> `apply_patch`, `Question` -> `question`, `PlanExit` -> `plan_exit`, `Lsp` -> `lsp`, `Execute` -> `execute`, `ScheduleWakeup` -> `schedule`.

Examples pinned by tests: `Bash` -> `bash`, `functions.bash` -> `bash`, `functions.shell_exec` -> `bash`; `mcp.functions.bash` remains unchanged.

### Permission-pattern derivation

The permission key is always obtained from `oc_permission::visibility::permission_key`, so `edit`/`write`/`apply_patch` share `edit`, and `list_mcp_resources`/`list_mcp_resource_templates`/`read_mcp_resource` share `read`; the dispatcher does not duplicate that alias table. Patterns are derived from arguments as follows:

- `bash`: `command` (Todo 40 must issue subsequent precise asks for each tree-sitter-extracted compound-command resource).
- `read`, `write`, `edit`: non-empty values from `filePath`, `file_path`, `path`.
- `apply_patch`: every path following `*** Add File:`, `*** Update File:`, `*** Delete File:`, or `*** Move to:` in `patchText`/`patch_text`/`patch`.
- `glob`, `grep`: non-empty `pattern`, then `query`.
- `webfetch`: `url`; `websearch`: `query`.
- `task`: `subagent_type` or `subagentType`; `skill`: `name`.
- `read_mcp_resource`: `uri`, `resource_name`, `server`; MCP list tools: `server`.
- `todowrite`, `question`, `invalid`, `plan_exit`, `lsp`, `execute`: `*`.
- Other/plugin tools: first all non-empty known resource keys (`path`, `filePath`, `file_path`, `url`, `uri`, `query`, `pattern`, `command`, `name`); if none exist, stable key-sorted canonical JSON with `intent` and `accept_large_output` removed; an otherwise empty object becomes `*`.

Duplicate patterns are removed without changing first-seen order. The original arguments are also placed in permission metadata. `jsonschema = 0.37.1` is exact-pinned and used before permission or execution, so malformed/schema-invalid calls become model-visible error results without running the tool.

## Task 35
- Oracle summary prompt compatibility shape: `## Objective`, `## Important Details`, `## Work State` (`### Completed`, `### Active`, `### Blocked`), `## Next Move`, and `## Relevant Files`; every section remains present, terse, and exact paths/symbols/commands/errors/URLs/IDs are preserved.
- Trigger resolution: `auto=true`, `prune=false`, `tail_turns=2`; derived `reserved=min(20_000,max_output)` unless configured, usable threshold is `context - max(max_output,reserved)`; derived `preserve_recent_tokens=clamp(usable/4,2_000,8_000)` unless configured. Threshold compaction requires auto and used >= usable; typed `ContextLimit` always enters compaction.
- Boundary rule: retain leading initial context; identify recent real user turns; scan backward within `preserve_recent_tokens`; then walk the split backward whenever the retained suffix starts with or contains a `ToolResult` whose matching `ToolUse` lies before the split. This avoids provider 400 responses for orphaned tool messages.
- Oracle contradiction: current TypeScript `SessionCompaction.process` resolves an explicit `compaction` agent model or falls back to the current user model; it does not directly select global `small_model`. Todo 35 explicitly requires the small model, so the Rust API takes `small_model_id` and the cassette asserts that exact model is called.
## Task 39

### Exact model-facing parameters
- read: `filePath` — `The absolute path to the file or directory to read`; `offset` — `The line number to start reading from (1-indexed)`; `limit` — `The maximum number of lines to read (defaults to 2000)`.
- write: `content` — `The content to write to the file`; `filePath` — `The absolute path to the file to write (must be absolute, not relative)`.
- edit: `filePath` — `The absolute path to the file to modify`; `oldString` — `The text to replace`; `newString` — `The text to replace it with (must be different from oldString)`; `replaceAll` — `Replace all occurrences of oldString (default false)`.
- apply_patch: `patchText` — `The full patch text that describes all changes to be made`.

### Exact tool descriptions
read:
Read a file or directory from the local filesystem. If the path does not exist, an error is returned.

Usage:
- The filePath parameter should be an absolute path.
- By default, this tool returns up to 2000 lines from the start of the file.
- The offset parameter is the line number to start from (1-indexed).
- To read later sections, call this tool again with a larger offset.
- Use the grep tool to find specific content in large files or files with long lines.
- If you are unsure of the correct file path, use the glob tool to look up filenames by glob pattern.
- Contents are returned with each line prefixed by its line number as `<line>: <content>`. For example, if a file has contents "foo\n", you will receive "1: foo\n". For directories, entries are returned one per line (without line numbers) with a trailing `/` for subdirectories.
- Any line longer than 2000 characters is truncated.
- Call this tool in parallel when you know there are multiple files you want to read.
- Avoid tiny repeated slices (30 line chunks). If you need more context, read a larger window.
- This tool can read image files and PDFs and return them as file attachments.

write:
Writes a file to the local filesystem.

Usage:
- This tool will overwrite the existing file if there is one at the provided path.
- If this is an existing file, you MUST use the Read tool first to read the file's contents. This tool will fail if you did not read the file first.
- ALWAYS prefer editing existing files in the codebase. NEVER write new files unless explicitly required.
- NEVER proactively create documentation files (*.md) or README files. Only create documentation files if explicitly requested by the User.
- Only use emojis if the user explicitly requests it. Avoid writing emojis to files unless asked.

edit:
Performs exact string replacements in files. 

Usage:
- You must use your `Read` tool at least once in the conversation before editing. This tool will error if you attempt an edit without reading the file. 
- When editing text from Read tool output, ensure you preserve the exact indentation (tabs/spaces) as it appears AFTER the line number prefix. The line number prefix format is: line number + colon + space (e.g., `1: `). Everything after that space is the actual file content to match. Never include any part of the line number prefix in the oldString or newString.
- ALWAYS prefer editing existing files in the codebase. NEVER write new files unless explicitly required.
- Only use emojis if the user explicitly requests it. Avoid adding emojis to files unless asked.
- The edit will FAIL if `oldString` is not found in the file with an error "oldString not found in content".
- The edit will FAIL if `oldString` is found multiple times in the file with an error "Found multiple matches for oldString. Provide more surrounding lines in oldString to identify the correct match." Either provide a larger string with more surrounding context to make it unique or use `replaceAll` to change every instance of `oldString`. 
- Use `replaceAll` for replacing and renaming strings across the file. This parameter is useful if you want to rename a variable for instance.

apply_patch:
Use the `apply_patch` tool to edit files. Its patch language is a stripped-down, file-oriented diff envelope: `*** Begin Patch`, one or more Add/Delete/Update sections, then `*** End Patch`. Add lines require `+`; Update supports optional `*** Move to:` and `@@` hunks. The implementation preserves the exact oracle parameter and grammar surface.

### Verified conditional rule
Oracle `packages/opencode/src/tool/registry.ts:292-295`: `const usePatch = input.modelID.includes("gpt-") && !input.modelID.includes("oss") && !input.modelID.includes("gpt-4")`; `ApplyPatchTool` is present when `usePatch`; `EditTool` and `WriteTool` are present when `!usePatch`. This is substring-based, not a semantic version comparison.


## Task 42 — web tools (webfetch, websearch)

**Verified search env vars.** The plan named three; there are eight, and the plan
pointed at the wrong file. `packages/core/src/flag/flag.ts` contains **none** of
them (grep: 0 matches). The real sources are
`packages/opencode/src/effect/runtime-flags.ts:31-39` and
`packages/core/src/tool/websearch.ts:76-83`:

| variable | what it does |
|---|---|
| `OPENCODE_WEBSEARCH_PROVIDER` | **routes only** (`exa`\|`parallel`); does NOT enable the tool |
| `OPENCODE_ENABLE_EXA` | enables Exa, and with it the tool |
| `OPENCODE_EXPERIMENTAL_EXA` | legacy spelling of the above, still honoured |
| `OPENCODE_ENABLE_PARALLEL` | enables Parallel, and with it the tool |
| `OPENCODE_EXPERIMENTAL_PARALLEL` | legacy spelling, still honoured |
| `OPENCODE_EXPERIMENTAL` | blanket switch — **enables Exa ONLY** |
| `EXA_API_KEY` | key, in the URL as `?exaApiKey=` |
| `PARALLEL_API_KEY` | key, as `Authorization: Bearer` |

Two traps in that table. `OPENCODE_WEBSEARCH_PROVIDER=exa` does **not** make the
tool appear — `webSearchEnabled` (`registry.ts:58-60`) never reads it, so setting
it on a non-`opencode` provider leaves the tool absent and the override inert. And
`OPENCODE_EXPERIMENTAL` is in `enableExa`'s disjunction but *not* `enableParallel`'s;
verified in both `runtime-flags.ts:31-39` and `core/src/tool/websearch.ts:79-80`, so
the asymmetry is deliberate, not a typo in one place.

**Registry key vs wire id — plan confirmed, and it generalizes.** `fetch` is the
registry key, `webfetch` the wire id (`registry.ts:216`, `webfetch.ts:24`). The plan
did not mention that `websearch` has the same split: registry key `search`
(`registry.ts:218`). `Tool::id()` returns the **wire** id in both cases, because that
is what the model emits and what `oc-config`'s `KNOWN_KEYS` uses as the permission
key. The registry-side keys are internal handles with no wire meaning.

**`http` → `https`: the description lies, and upstream lies the same way.**
`webfetch.txt` states "HTTP URLs will be automatically upgraded to HTTPS". No
upstream implementation does it: v1 (`webfetch.ts:34-36`) prefix-checks, v2
(`webfetch.ts:82-84`, `assertHttpUrl`) protocol-checks, and neither rewrites. The
request goes out against the URL as given. Reproduced the behaviour and kept the
text byte-identical rather than shipping a different description than upstream.

**The three bounds, with lines.**

| | webfetch | websearch |
|---|---|---|
| size | 5 MiB — `core/src/tool/webfetch.ts:17` | 256 KiB — `core/src/tool/websearch.ts:25` |
| time | 30 s default / 120 s max — `:18-19` | 25 s — `:181` |
| redirects | 10 hops — **no oracle line** | 10 hops |

The redirect cap is this port's. Upstream never states one and inherits undici's
default, which is how a cycle becomes unbounded. Ten is undici's and every browser's
limit, so naming it changes no reachable page.

**API keys are environment-only.** `core/src/tool/websearch.ts:81-82` reads
`process.env` directly and never touches the credential store; `auth.json` holds
model-provider credentials and neither `exa` nor `parallel` is a model provider
there. Checked before assuming, per the task brief.

**`cargo test <filter>` under-runs across targets.** `cargo test -p oc-tools web` —
the plan's literal acceptance command — reports `76 passed` for the lib (test names
carry the module path `webfetch::tests::…`) and `0 passed; 22 filtered out` for the
`webfetch` integration binary, whose test fn names contain no "web". The hazard note
in the brief is real and bit here. Use `--test <name>`.

**wiremock's `set_body_string` sets its own `Content-Type: text/plain`,** silently
overwriting an earlier `insert_header("content-type", "text/html")`. Cost four
failing tests that believed they were serving HTML. Use `set_body_raw(body, mime)`.
Separately, hyper *panics* on a hand-written `content-length` that disagrees with the
body, so a "lying header" cannot be faked; test the declared-size path with an
accurate oversized length instead.

## Task 41 — search: the exact rg flags, the ordering truth, and the ignore/hidden semantics

### The exact flags the oracle passes (`packages/core/src/ripgrep.ts`)

| entry point | argv |
|---|---|
| `glob` (:160-168) | `--no-config --files [--hidden] [--follow] --glob=<pattern> --glob=!**/.git/** .` |
| `find` (:192-200) | as `glob`, but the `--glob` is omitted entirely when the pattern is `*` |
| `grep` (:221-231) | `--no-config --json --hidden --no-messages [--glob=<include>] --glob=!**/.git/** -- <pattern> <file\|.>` |

What each implies for an embedded engine built on `ignore` + `grep-searcher`:

- `--files` → walk yielding regular files only; a directory is a traversal step, never a result.
- `--glob=<p>` → `OverrideBuilder::add`. **A whitelist, not a post-filter** — see below.
- `--glob=!**/.git/**` is appended **last** on every call, so in the gitignore last-match-wins
  ordering it beats any include: a caller asking for `**/*` still never gets the object store.
- `--hidden` is passed **unconditionally for grep** and **never for glob**. Note the polarity trap:
  `WalkBuilder::hidden(true)` means *skip* hidden, so `--hidden` is `hidden(false)`. Getting this
  backwards is a silent 2-file difference on a small tree and invisible on a large one.
- `--no-messages` → per-entry and per-file errors are skipped, not surfaced.
- `--no-config` → nothing may read `RIPGREP_CONFIG_PATH`.
- `--json` → the record shape is `packages/schema/src/filesystem.ts:14-33`; `text` **keeps the line
  terminator**, `offset` is the byte offset of the *line*, not of the match.
- No `-U`/`--multiline`, no `--sort`, no `--crlf`, no `-i`. So: `line_terminator(Some(b'\n'))` on the
  matcher, `multi_line(false)` on the searcher, `BinaryDetection::quit(0)`, case-sensitive.
- Caps the oracle applies after the fact: line text sliced at 2000 (`ripgrep.ts:267`) and submatches
  at 100 (`:20`). The 2000 is a JS `String.length` comparison, i.e. **UTF-16 code units** — a
  byte-counting or char-counting port disagrees on the first non-ASCII long line.

### `--glob` is a whitelist whose precedence beats gitignore AND the hidden rule

`ignore-0.4.33/src/dir.rs:511-522` returns an override match *immediately*, before any ignore file is
consulted, and `walk.rs:481` skips a hidden path only "if the path hasn't been whitelisted". So the
oracle's `glob` tool, which always passes `--glob=<pattern>`, **returns gitignored and hidden files**
when the pattern names them. Measured against 1.18.12:

```
$ opencode debug rg files --glob '**/*.ts'
.hidden_file.ts        <- hidden, returned: the glob whitelisted the file
ignored.ts             <- in .gitignore, returned: same reason
nested/deep/d.ts  src/a.ts  src/b.ts
```
and in the same run `.hidden_dir/e.ts` and `node_modules/pkg/f.ts` are **absent**, while `**/*`
returns both. The difference is that a whitelist glob is matched against **directories** too: `**/*`
matches `.hidden_dir`, so the directory is whitelisted and traversed; `**/*.ts` does not, so the
directory is pruned by the hidden/ignore rule before the walk ever sees its children. Corollary that
bit the differential: `--glob '.hidden_dir/**'` returns **nothing**, because it does not match
`.hidden_dir` itself; `--glob '{.hidden_dir,.hidden_dir/**}'` returns the file.

### `require_git` is on

`ignore`'s and ripgrep's default is `require_git = true`: a `.gitignore` in a tree with **no `.git`
anywhere** is not applied. Verified with rg 15.1.0 — `secret.ts` comes back despite being listed. Any
fixture that means to test ignore semantics must `git init`, or it silently tests nothing.

### Ordering: there is no oracle order to preserve

No `--sort` is passed, so the walk is parallel. Five consecutive `opencode debug rg files` runs over
one **unchanged** ten-file tree returned five different orders (transcript in the evidence file).
"Identical ordering" in the acceptance criterion can therefore only mean identical after sorting both
sides. Both `oc-search` backends emit **path-sorted** results (`WalkBuilder::sort_by_file_path`, i.e.
`rg --sort=path`), which is not a change to anything that could have been depended on and buys three
things the oracle lacks: deterministic truncation ("the first 100 of a stable order"), correct
`grep` grouping (a path can never head two separate groups), and a diffable transcript.

### Submatch spans must be located on the line *without* its terminator

`grep-searcher` matches against `lines::without_terminator(...)` (`searcher/core.rs:120`), so calling
`Matcher::find_iter` on `SinkMatch::bytes()` — which *includes* the `\n` — finds nothing for any
`$`-anchored pattern: Rust's regex has no "before a final newline" rule. The differential caught this
as `"submatches": []` against the oracle's `start: 0, end: 23` for `export const needle = 0$`.
Offsets are unaffected because the terminator only ever sits at the end.

## Task 40 — ShellTool resource extraction and lifecycle
- tree-sitter 0.26 uses grammar `LANGUAGE.into()` plus `Parser::set_language`; recursive `Node::children` traversal extracts nested command substitutions without evaluating them.
- Tokio `Command::process_group(0)` gives each Unix shell a dedicated process group, so cancellation and the hard ceiling can terminate the shell and descendants together.
- `ToolOutputStore` remains the sole overflow persistence path; ShellTool records `outputPaths` while leaving todo 72 to decide model-visible refusal/promotion policy.

## Task 30 — genuine OpenAI family
- `ApiSurface::Default` means Responses for the genuine OpenAI provider because the oracle constructs `sdk.responses(modelID)`; Chat is selected explicitly and targets `/v1/chat/completions`.
- Stateless Responses reasoning requires `store: false` plus `include: ["reasoning.encrypted_content"]`. Continuation replays `encrypted_content` byte-for-byte but deliberately omits the prior response item `id`.
- Sampling controls are unsafe for the o-series, non-chat GPT-5, Codex, and computer-use families. All of `temperature`, `top_p`, `frequency_penalty`, and `presence_penalty` are omitted together; `gpt-5-chat-*` remains a normal chat model.
- Real recordings buffer complete SSE bodies, so the provider tests deliberately split each body into 17-byte chunks before feeding the shared `oc_llm::sse::SseParser`; this proves arbitrary transport chunking without claiming the cassette retained original network boundaries.

## [2026-08-06] Task 50: debounced file watching over notify, bounded channel

### inotify does not report one notification per logical change — it reports 2-3

Measured on this host (Linux, `notify` 8.2.0 → `INotifyWatcher`). Creating and
writing one small file yields, in order:

```
Create(File)                    IN_CREATE
Modify(Data(Any))               IN_MODIFY
Access(Close(Write))            IN_CLOSE_WRITE
```

A 1,000-file burst therefore produces ~3,000 kernel notifications, of which
**2,003 survive classification** (`Access` is discarded, matching the oracle,
which publishes only create/update/delete — `watcher.ts:85-89`). Coalescing folds
those 2,003 into exactly **1,000** delivered events. The 2.0x ratio is the whole
justification for a debouncer: the oracle publishes one event per notification, so
a consumer that re-reads a file on every event re-reads it 2-3 times per save.

Corollary for anyone writing an assertion: **`accepted == published` is the bug
signal, not the healthy state.** Both the single-edit and the burst test assert
`accepted > published` for exactly this reason.

### The inotify race that WILL make your test flake: files in a just-created subdir

`built_in_ignored_folders_are_never_reported` failed on the first run with:

```
the source file was not reported: {"/tmp/.tmp2zhbVr/src"}
```

`src` was reported; `src/main.rs`, written immediately after `create_dir_all`, was
**never reported at all**. This is not a bug in the port. inotify watches one
directory at a time, so a recursive watcher has to add a watch for `src` when it
*processes* `src`'s creation event — and anything written between the `mkdir` and
that watch landing is invisible to the kernel. `notify` cannot fix this and
neither can we; `@parcel/watcher` has the same hole.

Two consequences:

1. **Any test that writes into a directory it just created must retry the write
   until the path shows up**, with a deadline. A single write plus a sleep is a
   coin flip. The retry loop is also what a real consumer needs, so it is not
   test-only scaffolding.
2. A consumer that must not miss files in new directories needs a rescan on
   directory-creation events. Todo 64 will care.

### Making filesystem tests deterministic — the four rules that got 3/3 identical runs

Three consecutive full runs: `13 passed` in **3.02s each**, wall clock 3.32s.
Identical to 10 ms. That did not happen by accident:

1. **`notify::Watcher::watch()` returns before the kernel is delivering.** Every
   fixture writes a probe file in a retry loop and blocks until it is observed,
   then drains the probe's own events. Skip this and the first assertion in every
   test races the watch registration. This is the single highest-value line in the
   test file.
2. **No fixed `sleep` as a synchronisation primitive.** Two helpers only:
   `poll_until(budget, cond)` (5 ms steps, hard deadline) and
   `drain_until_quiet(stream, quiet, budget)` which returns `(events, went_quiet)`
   so a timeout is distinguishable from a genuine quiescence and fails with the
   observed state.
3. **Inject the debounce.** Tests use 250 ms with `max_wait` at 30 s, which makes
   "the burst produces exactly one flush" structurally true rather than
   disk-speed-dependent — the write phase is far shorter than 250 ms of quiet.
4. **Assert bounds and per-path counts, never raw totals.** `dropped` came out
   5648 / 6764 / 7718 across runs (it depends on how many flush cycles the loop
   completes). Asserting `dropped > 0` plus `held <= capacity + max_pending` is
   stable; asserting `dropped == 7718` would have flaked immediately.

Plus: **keep the clock out of the state machine.** `debounce.rs` never calls
`Instant::now()` — every entry point takes an `Instant`. All 22 of its unit tests
are pure functions of an explicit timeline and cannot flake, so the coalescing
*rules* are tested there and the filesystem tests only confirm the wiring. That
split is why the fast tests are the thorough ones.

### The structural bound is the proof; RSS is only corroboration

The acceptance criterion asked for a memory-delta ceiling. Measured, from
`/proc/self/status` VmRSS: **248 KiB** delta for the 1,000-file burst and **0 KiB**
for a fully stalled consumer taking 4,000 files, both against an 8 MiB ceiling.

Those numbers are real but they are not what proves boundedness — RSS is
page-granular and these buffers are tens of KiB. What proves it:

- `watcher.pending() <= max_pending` and `stream.queued() <= capacity` asserted
  **after every single write**, 1,000 and 4,000 times respectively, so a transient
  excursion fails the test even if RSS never notices.
- The stalled consumer held **exactly 80 = capacity(16) + max_pending(64)**. Not
  "about 80" — the configured ceiling, hit precisely, with nothing being read.

If you need a memory assertion in this workspace: `oc_testkit::perf::process_tree`
is **`pub(crate)`** and measures a *child* process tree, so it cannot be reused for
the test's own RSS. Reading `/proc/self/status` is four lines and adds no
dependency.

### Deliberately tiny capacities are how you test a pressure path

`capacity(16)` + `max_pending(64)` reaches the give-up path with 4,000 files in
under a second. Trying to hit the production 1024/4096 ceilings with real load
would need a burst large enough to be slow *and* would make the outcome depend on
host speed. Two `#[must_use]` builder knobs turned "asserted in a doc comment"
into "asserted in a test".

### `GitignoreBuilder` anchors to the builder root and ignores the `from` path

Extending todo 41's notes with the matcher-side finding. `GitignoreBuilder::add_line`
(`ignore-0.4.33/src/gitignore.rs:460-540`) anchors every pattern to the
**builder's** root; the `from: Option<PathBuf>` argument is kept only for error
reporting. So feeding a nested `sub/.gitignore` into one root-anchored builder
**mis-anchors its `/`-prefixed patterns**: `/foo` in `sub/.gitignore` must mean
`sub/foo`, and one builder reads it as `foo`.

Correctness needs **one matcher per directory that owns a `.gitignore`**,
consulted deepest-first — which is what `WalkBuilder` does internally with a type
it does not export. A watcher cannot use `WalkBuilder` at all (it is handed a path
and must answer, not enumerate), so `oc-watch` builds that map itself, lazily and
cached: building it eagerly would mean walking the whole repo at startup, the one
cost a watcher exists to avoid.

Also confirmed: `matched_path_or_any_parents` **panics** if the path is not under
the matcher root (`gitignore.rs:243`), so strip and bounds-check before calling it.

### `require_git` bites the matcher side too

Todo 41 recorded this for the walker. It applies verbatim here: a `.gitignore` in a
tree with no `.git` is not applied, so a watcher fixture that forgets it tests
nothing and passes. An **empty `.git` directory is enough** — the check is for
presence, so `fs::create_dir(root.join(".git"))` avoids shelling out to `git init`
in a test. (`.git` is itself in `IGNORED_FOLDERS`, so nothing inside it is reported
anyway.)

## [2026-08-06] Task 43: the conditional tools — measured exposure, the client enum, and todo replace semantics

### `opencode debug agent <name> --tool` does NOT print a tool list

The brief (and, I suspect, todo 44's) assumption. `--help` says `--tool` is
"Tool id to execute". The resolved list is the **`tools` object of the plain
`debug agent <name>` call**, a `{ id: bool }` map. Todo 44's differential must read
that, and it must vary the **agent** as well as the environment — see the plan_exit
finding below.

Reproducible invocation (clean env, temp `HOME`, `--pure`, scratch git repo):

```
env -i HOME=$TMP PATH=/usr/bin:/bin TERM=dumb OPENCODE_CLIENT=... \
  /config/.local/share/mise/installs/opencode/1.18.12/opencode debug agent build --pure
```

### The measured exposure conditions (18 invocations, full transcript in the evidence file)

| wire id | condition | oracle |
|---|---|---|
| `invalid` | always, and it **is** model-visible | `registry.ts:227` |
| `todowrite` | always | `registry.ts:237` |
| `question` | client ∈ {`app`,`cli`,`desktop`} **or** `OPENCODE_ENABLE_QUESTION_TOOL` | `registry.ts:202,228` |
| `plan_exit` | plan mode **and** client == `"cli"`, **then** an agent-keyed permission gate | `registry.ts:243` + `agent/agent.ts:128,164` |

`invalid` is present in all 18 configurations with the description "Do not use". That
reads contradictory and is not: the name has to be in the model's vocabulary for a
correction message to arrive attributed to a tool call, while the description
discourages deliberate use.

### The client is a bare string, and both edge cases bite

`OPENCODE_CLIENT` is `Config.string(...).withDefault("cli")`
(`runtime-flags.ts:57`) — no validation, so an unrecognised client is a normal state
that matches no gate. Two measured surprises:

- **Case-sensitive.** `OPENCODE_CLIENT=CLI` offers neither `question` nor `plan_exit`.
  An `eq_ignore_ascii_case` would silently change behaviour.
- **Set-but-empty is not defaulted.** `OPENCODE_CLIENT=` offers neither either; the
  `"cli"` default applies only when the variable is *absent*. A
  `.filter(|v| !v.is_empty())` would silently change behaviour.

Modelled as `exposure::Client(String)` — a newtype, not a closed enum — for exactly
that reason.

### `OPENCODE_EXPERIMENTAL` is a FALLBACK, not an override

`enabledByExperimental` (`runtime-flags.ts:11-15`) reads the specific flag as an
`Option` and substitutes the blanket switch only when it is **absent**. Measured:

- `OPENCODE_EXPERIMENTAL=true OPENCODE_EXPERIMENTAL_PLAN_MODE=false` → plan mode **off**
- `OPENCODE_EXPERIMENTAL=false OPENCODE_EXPERIMENTAL_PLAN_MODE=true` → plan mode **on**

A disjunction reading — which is the obvious one, and which the sibling `websearch`
gating in this crate legitimately uses for `enable_exa` — gets the first case wrong.
The two flags genuinely differ: `enableExa` puts `experimental` in a disjunction
(`runtime-flags.ts:31-39`), `experimentalPlanMode` uses the option fallback. Do not
generalise one to the other. Hence `exposure::parse_bool` returns
`Option<bool>` so "unset" and "explicitly false" stay distinguishable.

`0`/`false`/`no`/`off` and `1`/`true`/`yes`/`on` are honoured; anything else counts as
absent and falls through to the blanket switch.

### `todo` replace semantics: the primary key forces the strategy

`(session_id, position)` is the primary key, so upstream's `Todo.update`
(`session/todo.ts:29-51`) does the only thing that works: **one transaction**,
`DELETE WHERE session_id = ?`, then insert with `position` = the item's index in the
model's array. Three consequences, each now asserted rather than assumed:

- `position` **is** the ordering; nothing else in the row records it, so a read needs
  an explicit `ORDER BY position`.
- `todos: []` is a **clear**, not a no-op — upstream returns early *after* the delete.
- A shorter second list only works because of the delete; without it you get
  `UNIQUE constraint failed: todo.session_id, todo.position`.

`time_created`/`time_updated` are also NOT NULL with **no SQL default** — drizzle's
`Timestamps` supplies both from `Date.now()` at insert time
(`core/src/database/schema.sql.ts:3-10`, where `$onUpdate` fires on insert too). An
insert naming only the five columns the plan quotes fails.

The FK is live: `oc-db` issues `PRAGMA foreign_keys = ON` on every connection, verified
by reading the pragma back off a pooled connection (1), so a write for an unknown
session **fails** and deleting a session cascades its todos away. Both tested.

### `serde`'s derived enum decoder cannot name the allowed values

`#[derive(Deserialize)]` on a string enum reports a numeric input as
`invalid type: integer 0, expected variant identifier` — the permitted values do not
appear anywhere, so a model cannot correct the call. Hand-writing `Deserialize` over
one visitor whose `expecting` enumerates them yields
`invalid type: integer 0, expected one of "high", "medium", "low"`.

`visit_u64` **and** `visit_i64` both need explicit arms: `serde_json` routes `0`
through `visit_u64` and `-1` through `visit_i64`, and `Visitor`'s default arms produce
a message against a borrowed expectation that reads worse.

## [2026-08-06] Task 67: sqlite goal store with split status ownership and budgets

### codex keeps goals in a SEPARATE database file, deliberately

`codex-rs/state/src/sqlite.rs:29-33` lists five runtime databases by filename:
`logs_2.sqlite`, `goals_1.sqlite`, `memories_1.sqlite`, `state_5.sqlite`,
`thread_history_1.sqlite`. Goals are not in `state_5.sqlite`, and the main
migration set contains `0034_drop_thread_goals.sql` — the table used to live in
the shared state DB and was moved out. Two consequences worth reusing:

1. **The `_N` suffix is the migration strategy.** An incompatible schema change
   revs the filename rather than migrating in place. For state that is cheap to
   lose and expensive to corrupt, that is the right trade — and it means the
   schema can be a single `CREATE TABLE IF NOT EXISTS` with no journal at all.
2. **A goal has no FK to the thread.** `thread_goals.thread_id` is a plain
   `TEXT PRIMARY KEY` (`goals_migrations/0001_thread_goals.sql:2`). Keyed *by* a
   thread, not owned by a thread row. That is what lets it survive compaction.

`oc-goal` ports this as `goal_1.db` under `oc_paths::data()`, one table, no FK.
Adding it to `oc-db`'s `opencode.db` was never an option: `TABLE_COUNT = 19`
plus the schema-differential test against a real DB would have failed, and the
byte-compatibility promise is worth more than the convenience of one file.

### codex's objective cap is split across two layers; folding it into the store is better

- `codex-rs/protocol/src/protocol.rs:4076` `MAX_THREAD_GOAL_OBJECTIVE_CHARS = 4_000`,
  and `:4082` **rejects** anything longer, counting with `chars().count()`.
- `codex-rs/tui/src/goal_files.rs:121-136` — the *caller* spills to
  `attachments/<uuid-v4>/goal-objective.md` first and substitutes a sentence.

So every future caller of codex's store has to remember to spill. Putting both
below the store's API instead makes "the column is never longer than 4,000
chars" a property callers cannot break. Cost: the store owns a filesystem
dependency it would otherwise not have.

**`chars().count()`, not `len()`.** A byte cap cuts a CJK objective to a third of
an ASCII one. `oc-goal` has a test that a 4,000-character CJK objective (12,000
bytes) does *not* spill, which is the only way to catch a byte/char slip.

### reading a model-authored path back is a security boundary, not a parse

`goal_files.rs:157-172` does not just strip the prefix/suffix — it checks the
path is inside `$CODEX_HOME/attachments`, is named `goal-objective.md`, and that
its parent directory name parses as a UUID. All three matter: the objective is
model-writable, so a pointer that resolved to any path would be an
arbitrary-file-read primitive handed to the model. Ported verbatim in
`oc-goal/src/spill.rs::objective_pointer_path`, with a test covering four
distinct forgeries.

### `rusqlite` 0.40.1 and CHECK-constraint violations

- A `CHECK` violation arrives as `Error::SqliteFailure(inner, _)` with
  `inner.code == ErrorCode::ConstraintViolation` — the same code as an FK or
  UNIQUE violation. `oc_db::is_constraint_violation` already covers it; there is
  no separate `ErrorCode` for CHECK, so distinguishing a CHECK from a UNIQUE
  needs the extended code or the message string. `oc-goal` never needs to: the
  `CHECK` exists so an unknown status is *unstorable*, and a violation is a bug
  in this crate, not input.
- `oc_db::map_error` maps it to `DbError::Query`, correctly non-retryable.
- **`RETURNING` needs `query`, not `execute`.** `Connection::execute` with a
  `RETURNING` clause returns a row count and discards the row.
  `statement.query(params)?.next()?` is the shape that gives you
  `Option<&Row>` — and `None` is how a guarded upsert reports its refusal.
- `Pool::transaction` is typed on `DbError`, so a closure that also produces a
  domain error has to collapse it. Doing that in one named helper
  (`into_db_error`) beats sprinkling `map_err` at every call site.

## [2026-08-06] Task 98: character-capped resident memory (`oc-memory`)

Porting `.omo/refs/hermes-agent/tools/memory_tool.py` + `tools/threat_patterns.py`.
Five things in that reference are load-bearing and non-obvious; four of them are
traps that fail *silently*.

### 1. The threat-pattern scope taxonomy — and why memory gets the broadest set

`threat_patterns.py:14-24` tags every pattern with a scope, and the scopes NEST:

| scope | count | applied to | implies |
|---|---|---|---|
| `all` | 11 | everywhere | `context`, `strict` |
| `context` | 17 | context files + memory + tool results | `strict` |
| `strict` | 8 | memory writes + skill installs only | — |

So `scan_for_threats(content, scope="strict")` runs **36** patterns, not 8
(`:182-201`). Memory writes use `strict` (`memory_tool.py:86-89`) for a reason
worth carrying: memory enters the system prompt as a *frozen snapshot*, so a
poisoned entry persists for a whole session and across sessions until removed.
Aggressive checks are acceptable precisely because the content is user-curated
and a false positive can be resolved by rewriting the entry. Tool results — web
pages, GitHub issues, MCP responses — get the *narrower* set, because the user
did not author them and cannot edit them, so blocking there breaks the session.

Anchoring rule (`:26-32`): patterns anchor on **C2-specific vocabulary or
unambiguous attack behaviour, never on bossy English**. `you must` alone appears
all over legitimate `AGENTS.md`, so the pattern is `you\s+must\s+...(register|
connect|report|beacon)`. The reference removed `praxis` from its C2-framework
brand list because it is a common word (`:109-114`) — one false positive there
blocks an entire legitimate file.

### 2. TRAP — the invisible-codepoint check must run BEFORE normalisation

`threat_patterns.py:231-234` spells it out: NFKC normalisation **strips some of
the 17 invisible codepoints**, so a scanner that normalises first and then looks
for them reports clean on exactly the input the check exists for. Raw text first,
fold second. There is no test that catches getting this backwards unless you
write one specifically (`invisible_codepoint_survives_folding`).

### 3. TRAP — the multi-word filler is bounded on purpose

`_FILLER = r"(?:\w+\s+){0,8}"` (`:55-59`), and the comment records that the
earlier `(?:\w+\s+)*` "is ambiguous and can backtrack heavily on adversarial
near-misses". In Python that is a real DoS in a path that runs on every write.
Rust's `regex` cannot backtrack at all, so the bound stops being what saves you —
but keep it anyway, because it is also the pattern's *meaning*: "these tokens,
near each other", not "anywhere in the entry".

### 4. TRAP (Rust-specific) — `RegexSet` over these 36 patterns exceeds the size limit

Eleven of the reference's patterns use bounded repetition (`[^\n]{0,2048}`,
`[^>]{0,512}`). Rust's `regex` **materialises** a bounded repetition rather than
counting it, so the *union* blows the default 10 MB program budget:

```
CompiledTooBig(10485760)
```

Individually every pattern is small. Fix: compile a `Vec<Regex>` instead of one
`RegexSet`. That is also better — first-match can short-circuit, and declaration
order is preserved by construction, which makes a finding reproducible from the
input alone (the reference intersects Python `set`s at `:234-237`, so *its*
reported codepoint is unspecified when an entry contains several).

Corollary for the no-`expect`-in-libraries rule: `OnceLock::get_or_init` cannot
return `Result`, so a fallible regex build has nowhere to put the error. Adding a
`Threat::ScannerUnavailable(id)` variant makes the scanner **fail closed** —
content that could not be screened is refused — with no `expect` and no `Result`
in the hot API. A `#[cfg(test)]` assertion proves the variant is unreachable.

### 5. Drift detection uses TWO structural signals, neither of them mtime

`memory_tool.py:807-856`. Both are *content* signals:

1. **Round-trip mismatch** — parse-then-serialize does not reproduce the file
   bytes, so rewriting from parsed entries would lose data.
2. **Entry-size overflow** — a single parsed entry exceeds the *whole store's*
   cap. Impossible for a tool-written entry, since the cap is checked against the
   entire store; it means an external writer appended free-form text into what the
   parser now reads as one entry.

Neither catches a hand edit that KEEPS the §-delimited shape — that needs a
stamp. See issues.md for why all three now run.

Related, and the same class of bug: `_read_raw_checked` (`:750-770`) distinguishes
*unreadable* from *empty*, and treats invalid UTF-8 as unreadable. A
read-modify-write caller that took a failed read for an empty store would rewrite
the file down to whatever the current batch adds and **wipe every prior entry**.
`Ok(None)` = absent = clean empty store; existing-but-undecodable = abort.

Also: `_write_file` uses atomic rename, which is *why* no file locking is needed —
a reader always sees one complete version of the file.

### 6. The anti-thrash rule: success returns usage, failure returns entries

The single most valuable comment in the reference, `memory_tool.py:711-723`:

> We do NOT echo the full entries list here — dumping it invites the model to
> "find more to fix" and re-issue the same operations (observed thrash: the
> correct batch on call 1, then 5 redundant repeats). Entries are only shown on
> the error/over-budget paths, where the model genuinely needs them to decide what
> to consolidate.

So the rule runs both directions: **success is terminal and minimal** (`success`,
`done`, `scope`, `usage`, `entry_count`, plus "do not repeat it"), **failure is
actionable and carries the entries**. Adding the entry list to the success path
looks like helpfulness and costs five redundant tool calls per write. Encode both
halves and doc-comment the reason, or a future maintainer will "helpfully" undo
it. Todo 100's acceptance depends on the failure half; nothing but the comment
protects the success half.

### 7. Why chars and not tokens, from the reference's own config

`cli-config.yaml.example:691-693`:

```yaml
# Character limits (~2.75 chars per token, model-independent)
memory_char_limit: 2200   # ~800 tokens
user_char_limit: 1375     # ~500 tokens
```

The token figure is a **comment, not a computation**. A token cap needs a
tokenizer (dependency), a model id (config), and moves under content already on
disk when either changes — a store that fit yesterday overflows today with no
write in between.

Unit choice inside "characters": `chars().count()`, not `len()`. Under a byte cap
the same instruction in Chinese costs 3× what it costs in English while occupying
a third of the attention budget it was paying for. `char_count("先读代码再改")`
is 6; its `len()` is 18.

## Task 102 — FTS5 archival search and `session_search`

- FTS installation is deliberately opt-in through `oc_db::fts::ensure(&mut
  Connection)`. Keeping the FTS views, virtual tables, and triggers out of
  `migration::apply` / `schema::up` preserves byte-level parity with databases
  created by the real OpenCode binary while allowing callers that want archival
  search to enable it idempotently.
- Both FTS tables are external-content indexes keyed by `message.rowid`. The
  source views aggregate final `text`, `reasoning`, `subtask`, and tool-part
  content for the main index; the trigram source excludes tool parts to avoid
  paying the roughly 2.6x CJK index cost for machine-heavy output. CJK-script
  detection selects trigram search; ordinary text uses the `unicode61` index.
- Sync requires triggers on both `message` and `part`. Part insert/update/delete,
  moving a part to another message, and message cascade deletion all update the
  external-content indexes. Compaction does not delete source rows, so archived
  pre-compaction text remains searchable.
- `session_search` infers exactly three SQL-only modes: `query` means discovery;
  `session_id + around_message_id` means anchored scroll; no mode fields means
  recent-session browse. Invalid combinations are `ToolError::InvalidArgs` and
  remain model-correctable. Discovery returns a snippet, a +/-5-message window,
  three-message bookends, and before/after counts.
- Child/noisy sessions are down-ranked rather than filtered. The regression test
  proves a root-session hit outranks a child hit while the child remains in the
  result set, preventing repetitive session classes from monopolizing recall.
- External-content rowids are not durable across `VACUUM`; callers that compact
  the database must run `oc_db::fts::rebuild` afterwards so both FTS tables are
  repopulated from their source views.

## [2026-08-06] Task 44: full conditional registry measurements

The working oracle invocation is `env -i ... /config/.local/share/mise/installs/opencode/1.18.12/opencode debug agent <agent> --pure`; `--tool` executes one tool and does not print a list. Read the JSON `tools` object and select entries whose value is `true` to obtain the provider-visible set.

Measured visible sets:

- `openai/gpt-5.2`, build: `invalid question bash read glob grep task webfetch todowrite skill apply_patch`.
- `anthropic/claude-sonnet-4-5`, build, narrow `bash/git push*` deny: `invalid question bash read glob grep edit write task webfetch todowrite skill`.
- `openai/gpt-4.1`, build, blanket bash deny: `invalid question read glob grep edit write task webfetch todowrite skill`; debug JSON retains `bash: false`.
- `openai/gpt-oss-120b`, build, Exa and LSP enabled: `invalid question bash read glob grep edit write task webfetch todowrite websearch skill lsp`.
- `opencode/gpt-5.2`, plan, plan mode enabled: `invalid question bash read glob grep task webfetch todowrite websearch skill apply_patch plan_exit`.

The full literal transcripts are in `.omo/evidence/task-44-opencode-rust.txt`. `FileTools::exposed_for_model` matched `registry.ts:292-295` exactly, including both `gpt-4` and `oss` carve-outs.

## Task 99 — frozen resident-memory snapshots

- A session needs two deliberately separate memory views: mutable `MemoryStore`
  handles for immediate durable writes, and immutable rendered strings captured by
  `SessionMemory::open`. Re-rendering the live handles in `inject_into` would make
  a mid-session write change the static provider-cache prefix; storing the rendered
  strings separately makes that failure structurally unavailable.
- Cache freshness is a comparison between the cached prompt and independently
  re-opened, currently rendered stores. Comparing two values captured by the same
  session only proves that the stale snapshot agrees with itself. Current empty or
  disabled scopes still need a negative check for their stable `Scope::label`, or
  a removed block remains latched forever.
- Freshness needs three states. `Fresh` proves both enabled current blocks match;
  `Stale` proves readable current memory differs; `Unknown` means a current scope
  could not be read and therefore forces the conservative rebuild path. Treating
  unreadable as empty would both hide data loss and incorrectly classify a cached
  header as ordinary staleness.
- `oc-engine` compaction already preserves the initial system prefix outside the
  summarizer input. Two real `run_compaction` passes prove both frozen scope blocks
  survive byte-for-byte while neither `MEMORY (` block reaches either summarizer
  request. No compaction-specific memory reinjection is needed.
- First-party global/project blocks are injected directly. Only externally recalled
  memory is wrapped in `<memory-context>` after case-insensitive removal of forged
  fences and forged `[System note: ...]` payloads. The trusted note is then added
  exactly once by the wrapper.

## Task 72 — output refusal and cancel-safe timeout promotion

- Large output must be persisted before policy evaluation returns a refusal. This
  guarantees that refusing prompt-expensive content never destroys the only full
  copy and gives the caller a stable retrieval path.
- Cross-cutting raw arguments must be inspected before `strip_cross_cutting`.
  `ShellTool` therefore implements `Tool` directly while keeping its typed inherent
  `run`; otherwise `accept_large_output` disappears before output policy can see it.
- `tokio::time::timeout` must borrow `&mut JoinHandle<T>` for promotion. Consuming
  or dropping the handle on timeout would cancel ownership of the only reachable
  completion path; borrowing allows the background manager to adopt the same live
  process without restarting it.
- Foreground timeout and process lifetime are different policies. The 120-second
  default and 600-second cap bound interactive attention, while cancellation and
  the pre-existing 30-minute hard ceiling remain the only termination boundaries.

## [2026-08-06] Task 68 — goal tools, hidden injection, guarded continuation

- A goal that must survive compaction cannot be injected by first writing it into
  conversation history. `GoalContinuation::inject_hidden_context` reads the active
  goal from SQL for every request and appends a synthetic pseudo-user entry only to
  the request transcript. Compaction can remove every prior message and the next
  request still reconstructs the same rubric.
- Synthetic XML-like context must escape model-authored objective text. Escaping
  `&`, `<`, `>`, quotes, and apostrophes before interpolation prevents an objective
  from closing the trusted goal element and forging adjacent instructions.
- A one-turn deferral is state, not an in-memory boolean. Persisting and atomically
  consuming it in `goal_1.db` makes the "defer exactly once" contract survive a
  restart and prevents a repeated read from deferring forever.
- "Blocked" needs evidence that survives the turn that produced it. A persisted
  `(failure_key, count)` streak lets the update tool require three matching signals;
  a changed signal restarts the streak rather than aggregating unrelated failures.
- The safest terminal-error behavior is to block immediately. Provider or
  compaction terminal failures occur outside the model's opportunity to call
  `update_goal`; leaving the goal active would let the continuation scheduler
  repeatedly submit the same doomed turn.

## [2026-08-06] Task 49: portable-pty gotchas, PTY test determinism, and the measured 100 MB number

### `drop(pair.slave)` before reading the master, or the reader thread never ends

The single highest-value line in `session.rs`. While the slave fd is open the
kernel keeps the pty writable, so `read()` on the master **never returns 0** even
after the child is dead and reaped. Without the drop, every session leaks one
thread blocked forever in `read`, and the leak is invisible: status flips to
`exited` correctly (a different thread observes that), output stops arriving, and
nothing looks wrong until you count threads.

```rust
let child = pair.slave.spawn_command(builder)?;
drop(pair.slave);          // <- not optional
let reader = pair.master.try_clone_reader()?;
```

### On Linux a closed pty reports EIO, not EOF

The reader loop must treat *any* error other than `Interrupted` as end-of-output:

```rust
Ok(0) => break,
Err(e) if e.kind() == ErrorKind::Interrupted => {}
Err(_) => break,           // EIO on a closed pty is normal, not a fault
```
Logging that error would produce one spurious warning per session exit.

### `child.wait()` IS the reap, and it is the only reap

`portable_pty` has no reaper. The blocking `wait()` on the session's waiter thread
is what collects the zombie, so **the thread existing is the containment**. Drop
the waiter and an externally killed child stays in the process table forever.

The test that proves it is `/proc/<pid>` disappearing — a zombie **keeps** its
`/proc` entry, so "killed but unreaped" and "reaped" are distinguishable. Checking
`kill -0` cannot tell them apart; both succeed on a zombie.

### `ExitStatus::exit_code()` is `u32` and a signalled child still has one

`portable_pty::ExitStatus` maps a signal to `code = status.code().unwrap_or(1)`
plus `signal: Some(name)` (`lib.rs:208-236`). So `kill -KILL` yields
`exit_code() == 1`, not `None` and not 137. Anything asserting `exit_code.is_some()`
holds for signalled children too, which is what the external-kill QA relies on.

### `cargo metadata --locked --offline` fails on a Windows-only transitive dep

Exactly the hazard WORKTREE.md item 4 warns about, hit for real. `portable-pty`
pulls `shared_library` **only on Windows**. `cargo build` on Linux never fetches
it, so it lands in `Cargo.lock` but not in the registry cache, and `cargo metadata`
— which resolves *all* targets — refuses:

```
error: failed to download `shared_library v0.1.9`
Caused by: attempting to make an HTTP request, but --offline was specified
```

One `cargo fetch` fixes it permanently. **Rule for any task adding a dependency
with `[target."cfg(windows)".dependencies]`: run `cargo fetch` (online) once, then
verify `cargo metadata --locked --offline`.** A plain build will never surface it.
Checking is two lines:

```sh
git diff Cargo.lock | grep '^+name = ' | tr -d '"' | sed 's/^+name = //' \
  | while read p; do ls ~/.cargo/registry/cache/*/"$p"-*.crate >/dev/null 2>&1 || echo "MISSING: $p"; done
```

### Making PTY tests deterministic: sequence the exits, never time them

Todo 50's four rules carry over unchanged. One addition specific to processes:

**Gate each child on `read` from its own pty and release it with a `write`.** That
turns "these 30 sessions exit in this exact order" into a structural fact. The
alternatives both fail: `exit N` children race the create loop (observed —
`assert_eq!(list().len(), 30)` failed with **26**, because the retention cap began
evicting before all 30 existed), and gate *files* need 30 shells spinning on
`sleep`, which is slower and host-speed dependent.

**A cap test cannot wait for every session's `exited` status.** Once more than the
cap has exited, the earliest exits are already gone, so `wait_for_exit` times out
with `Err(NotFound)` — observed, 20 s wasted. The correct wait is
"exited **or** evicted", justified because eviction is only ever triggered from
`record_exit`, so evicted implies exited. This is the second-order race a naive
port would paper over with a sleep.

Result: 6/6 green under six-way concurrent `cargo test -p oc-pty`, whole suite
~2.6 s, and the slow test's spread across the six runs is 1.27-1.45 s.

### The measured 100 MB number

End to end through a real pty (`yes | head -c 104857600`), read by the session's
own reader thread:

```
total_written=107691589  retained=2097152  reserved=2097152
discarded=105594437      limit=2097152     process RSS delta 4724 KiB
```

`retained` and `reserved` are **exactly** 2 MiB — the ceiling, hit precisely.
Driving the ring directly, RSS delta is 2312 KiB for the same 100 MiB.

RSS is corroboration, not proof: it is page-granular and includes transient read
buffers the allocator has not returned. What proves it is `reserved_bytes() <=
limit` asserted after **every** one of the 12,800 pushes, plus the identity
`discarded == total_written - retained`, which fails if a byte goes unaccounted.

### A floor assertion belongs on the producer too

`assert!(total_written >= 100 MiB)` is the analogue of todo 2's file-count floor.
Without it, a `yes | head` that silently produced nothing would pass every ceiling
assertion vacuously. Same for the shell list: `assert!(!shells.is_empty())`.


## [2026-08-06] Task 45: MCP stdio is NDJSON, and real-server proof matters

A real `codegraph 0.42.9 serve --mcp` exchange is one UTF-8 JSON value per line. It accepted protocol `2024-11-05`, returned four tools, and a `codegraph_search` call for `task45-no-symbol` completed with `isError: false` and `No results found`. There are no LSP `Content-Length` headers. The live test initializes an isolated project and prints the literal initialize/list/call responses under `--nocapture`; if the binary is unavailable it emits an explicit skip rather than silently replacing the server with a fixture.

The reader must classify every parsed line before touching a waiter. Responses are routed by numeric id through `HashMap<u64, oneshot::Sender<_>>`; notifications are broadcast independently, and an unknown response id is logged and ignored. Tests prove that an interleaved notification and an unknown id cannot consume or desynchronize the real response. A multi-kilobyte UTF-8 result also proves framing is newline-based, not fixed-buffer based.

`notifications/tools/list_changed` triggers a fresh paginated `tools/list` and broadcasts the resulting cache snapshot. Child stderr is drained independently; close/drop kill and reap the child so neither a full stderr pipe nor a zombie can stall the transport.

Validation: package tests 9/9; live codegraph handshake/list/call 1/1; three rounds of six concurrent `cargo test -p oc-mcp stdio --offline` runs 18/18; workspace build, workspace all-target clippy, fmt, and rust-analyzer diagnostics all clean.

## [2026-08-06] Task 71: destructive shell commands need a pre-spawn three-verdict gate

- `shell::analyze_command` is the right assessment boundary: its tree-sitter walk exposes every command in compound statements, subshells, and command substitutions without executing any of them. The risk pass consumes those resources and recursively parses static `eval`, `sh -c`/`bash -lc`, `su -c`, and `find -exec` payloads.
- The parser intentionally omits some shell expansions from `CommandResource.tokens`; `$HOME` was the concrete case. Protected symbolic targets therefore need a narrow source-level recovery in addition to token analysis. Static shell quoting and backslash concatenation also need normalization: `r'm'`, `r\m`, and `/e''tc` are the same shell words as `rm` and `/etc`.
- The three outcomes have distinct recovery semantics: safe commands run; bounded or runtime-computed destructive commands reflect and require a substantive `justification`; filesystem root, home, credential stores, system paths, and device nodes are denied permanently. A justification never changes a catastrophic verdict.
- The gate runs after lexical cwd resolution but before permission prompts, environment hooks, process spawn, explicit background adoption, or foreground timeout promotion. Timeout promotion only adopts an already-started task, so this one pre-spawn insertion covers all shell execution paths.
- The adversarial matrix now covers compound/subshell commands, shell/eval/su wrappers, `sudo`/`env`/`timeout`/`chroot`, `xargs`, `find -delete`/`-exec`, escaped and concatenated shell words, static brace expansion, globs, dynamic command names and targets, upward traversal, redirects, root/home/credentials/device nodes, and explicit background dispatch.

## [2026-08-06] Task 51: secure HTTP core and bounded event fan-out

- Axum layers apply only to routes already present, and the last layer is outermost. `ServerBuilder` therefore merges every feature router before adding directory selection and adds Basic auth last, so future routes cannot accidentally bypass authentication.
- Authentication matches the oracle's exact environment semantics: an absent or empty `OPENCODE_SERVER_PASSWORD` disables auth, while `OPENCODE_SERVER_USERNAME` defaults to `opencode` only when absent. `AuthConfig` redacts its password in `Debug`.
- SDK directory routing is query `directory` first, then `x-opencode-directory`, then the startup directory. Query parsing removes the form-encoding layer before one component decode, which is why `%252Fworkspace` in the query and `%2Fworkspace` in the header both resolve to `/workspace`.
- Each event subscriber owns a fixed `VecDeque` (default 64). Overflow drops newest events and increments one saturating scalar; after retained events drain, `Delivery::Lagged { dropped }` makes loss observable without allocating per dropped event. Three rounds of six concurrent pressure-test processes passed 18/18.
- `EventSubscription::recv` creates its `Notify` waiter before checking queue state. This ordering closes the empty-check/await lost-wakeup window while retaining a synchronous bounded publish path.

## [2026-08-06] Task 69: markdown goal projection — breaking the write-then-watch loop

### The self-render loop is the bug this shape ships with, and a stamp is the wrong fix

This module writes `.opencode/goal/<sessionID>.md` **and** consumes watch events
for it. Its own atomic rename fires a `Change`, so without suppression the first
render re-ingests forever.

`GoalProjection` retains the **exact bytes** of its last render plus the `Goal`
they were rendered from, and an ingest that finds the file byte-identical returns
`Ingest::OwnRender` without touching SQL and without rewriting the file. Three
things fall out of choosing bytes over `oc-memory`'s mtime+len stamp:

1. **No same-size-within-one-timestamp-tick false negative.** `oc-memory` needs a
   stamp because it compares against a version it no longer holds; a projection has
   just *produced* the bytes it is about to compare, so equality is free and exact.
2. **A save that changed nothing correctly reads as "no edit"** — the user opened
   the file, saved without typing, and nothing happens. A stamp would see a new
   mtime and treat it as an edit, then report zero rejections and rewrite the file,
   which is a visible no-op churn on every editor autosave.
3. **It is not one-shot.** An ignore-next-event token would be consumed by the
   first of `oc-watch`'s coalesced events and then let the next one through.
   Tested: `a_render_does_not_trigger_a_re_ingest` ingests six times.

The subtler half: the **rejection baseline must be the last render, not current
SQL**. SQL moves on between the render and the user's save — `record_usage`
changes `tokens_used`, `tokens_remaining` *and* `updated_at_ms` in one statement —
so diffing the document against live SQL reports three edits the user never made.
`an_untouched_field_is_not_reported_when_sql_moved_on_since_the_render` is the
regression test; it fails loudly against the naive version.

### Atomic-rename details that actually mattered

- `with_file_name(format!("{name}.tmp.{nanos}"))`, **not** `with_extension`.
  `oc-memory` uses `with_extension`, which replaces the existing extension:
  `ses_1.md` becomes `ses_1.tmp.<nanos>`. Harmless there; here a session id
  containing a dot would make the temp name collide with a *different* session's
  document. `with_file_name` appends instead, so the temp path is always distinct
  from every target.
- Nanos in the name, so two concurrent renders of the same document cannot land on
  one temp file and interleave.
- The rename's error arm removes the temp file, so a failure does not leave litter
  next to a document a human is going to open.
- Neither this nor `oc-memory` calls `sync_all`. For a *projection* that is right:
  the file is derived, a render lost to a power cut is regenerated from SQL on the
  next material change, and fsyncing on every token-count update buys nothing SQL
  does not already guarantee.

### Measured: the mutation proof for atomicity is not close

Replacing temp+rename with a direct `fs::write` made
`the_render_is_atomic_under_a_concurrent_reader` fail with **292,699 of 293,732
reads observing a partial document**, all of them `0 bytes` — the truncate window.
Two lessons: a reader that only checked "non-empty" would have caught this one but
not a torn tail, and 1,000 renders against a spinning reader produces ~294k reads,
so the test's `>= 50` floor is three orders of magnitude of headroom rather than a
tight bound.

### `document_path` validated the derived filename, which is exactly backwards

First draft checked `Path::new(&format!("{id}.md")).file_name() == Some(&file)`.
Appending `.md` turns `..` into `...md`, which is a perfectly legal single
component — so the check **accepted the one input that most needed refusing** and
resolved `..` to `/repo/.opencode/goal/...md`. Caught by the hostile-id test on the
first run. Validate the *input*, never the string you built from it.

## [2026-08-06] Task 100: the `memory` tool — retargeted description, and which `ToolContext` field is a turn

### `ToolContext` has no turn identifier; `session_id` is the only usable key

The reference's circuit breaker is "per turn". `ToolContext`
(`crates/oc-tool/src/context.rs:169-190`) carries `session_id`, `message_id`,
`call_id` and `depth`. None of them is a turn, and two of the three are traps:

| field | keying on it | why |
|---|---|---|
| `call_id` | never fires | unique per call, so the counter resets every attempt |
| `message_id` | **never fires, but tests green** | `oc-engine` mints a new assistant message **per step** (`loop.rs:620-624`); a retry costs a step, so the id differs every attempt — yet a test that reuses one id passes |
| `session_id` | fires correctly | stable across every step of a turn |

`message_id` is the dangerous one: it looks like "the model's current message"
and it *is* — for one step. The breaker test therefore passes `msg_1..msg_4`
deliberately, so keying on `message_id` fails it.

The reference never states its key because it does not have to: the counter lives
on a `MemoryStore` that is "one instance per AIAgent" (`memory_tool.py:148-149`)
and per-turn-ness comes entirely from an *external* `reset_consolidation_failures()`
at the turn boundary (`:176-178`). Ported as
`MemoryTool::reset_for_turn(session_id)`. **It has no caller yet** — wiring the
engine is outside this todo's crate boundary. Until then the streak still resets on
the reference's other reset: a **successful write clears it** (`:704-706`), because
the cap counts a stuck loop, not a lifetime tally. Failure direction of the gap is
safe: it can trip one turn early, never late.

### The retargeted description (1969 chars, full text in the evidence file)

Structure carried from `memory_tool.py:1152` unchanged: HOW / WHEN / IF FULL /
TARGETS / SKIP. Two sections diverge, both because this project is a coding agent:

- **TARGETS** — the reference splits by *who the note is about* (`memory` = agent
  notes, `user` = user profile). Todo 98 split by *where the note applies*
  (`global` = habits that travel, `project` = one repo's rules). Naming the
  reference's targets would have advertised a store that does not exist. The clause
  now also states the *cost* of choosing wrong in both directions ("a repo rule
  filed globally is paid for in every unrelated session; a travelling habit filed
  per-project is relearned in every checkout"), because the model has no other way
  to learn that the two stores have different blast radii.
- **SKIP** — keeps "task progress, completed-work logs, temporary TODO state" and
  points them at **both** owners this project has: the goal tools
  (`get_goal`/`update_goal`, todo 68) and `session_search` (todo 102). The
  reference names only `session_search` because it has no goal tool.

`target` is the one required field. Everything else is optional, so the model can
send either shape — see issues.md for why `schemars` forced that.

### Every `///` on a params type is billed on every request, all session

`schemars` copies rustdoc verbatim into the wire schema. A rationale paragraph on
`MemoryParams` therefore rides alongside the description in **every** request for
the whole session, and it is written for a maintainer who will never read it there.
Two consequences now enforced by a test
(`no_maintainer_rationale_rides_in_the_wire_schema`):

1. Maintainer reasoning goes in `//!` module docs, which do not ship. Where a
   `#[derive(JsonSchema)]` type needs a note, use a plain `//` comment *above* the
   doc line — invisible to `schemars`, visible in the source.
2. Intra-doc links (`` [`Scope`] ``) render **literally** in the schema. A params
   type's docs must be plain prose.

This applies to every tool in `oc-tools`, not just this one.

### A tool result that carries an error is not a `ToolError`

`dispatch.rs:496-515` maps `Ok(Err(ToolError))` to `ToolDispatchResult::error`,
which sets `is_error: true` and **replaces the tool's body with a rendered error
string**. The turn survives either way (`loop.rs:751-790` persists and continues),
so "don't fail the turn" is not the reason to avoid `Err` — losing the payload is.
A memory refusal has to carry `usage`, `current/limit` and often the entry list;
returning `Err` would throw all of that away. So every store refusal, including the
breaker's terminal one, is `Ok(ToolOutput)` with `success: false`. The only `Err` is
an unusable call *shape*, where there is genuinely nothing to report about memory.
## [2026-08-06] Task 48 — LSP framing, live diagnostics, and oracle isolation

- LSP stdout must have exactly one reader. A request/write/read sequence cannot work:
  servers publish diagnostics and issue reverse requests while responses are in
  flight. One incremental `Content-Length` framer plus an id-keyed oneshot map
  handles split headers, coalesced frames, notifications, and out-of-order replies.
- A diagnostics test must isolate the selected server. An object-valued `lsp`
  config enables every built-in before overrides; overriding only TypeScript also
  starts ESLint/Biome candidates. The fixture disables every other built-in and then
  supplies one explicit command.
- The installed OpenCode 1.18.12 returns the expected TS2322 only with an explicit
  TypeScript command override, and returns `[]` for a Rust E0425 that the same host's
  rust-analyzer reports. Differential assertions therefore compare TypeScript
  exactly and separately require a real rust-analyzer diagnostic rather than
  encoding the oracle's empty Rust result.
- The task prose says 39 built-ins, but the pinned `server.ts` exports 38 `Info`
  values and `BUILTIN_SERVER_IDS` also contains 38. The registry test compares the
  entire ordered id vector to the schema constant, avoiding a stale magic count.

## [2026-08-06] Task 46: remote MCP needs both HTTP response shapes and a real fallback proof

- Streamable HTTP does not imply one response content type. The live AWS endpoint returned ordinary JSON and negotiated protocol `2025-03-26`; the live Microsoft endpoint returned SSE and negotiated `2024-11-05`. The same POST path must therefore dispatch by `Content-Type` and route either shape through the same JSON-RPC waiter map.
- Transport order is observable behavior: POST Streamable HTTP first, then GET legacy SSE only after a non-auth failure. A 401/403 stops fallback and starts OAuth, because trying the second transport would duplicate discovery and can overwrite pending authorization state.
- Legacy SSE cannot POST until its `endpoint` event arrives. The source reader starts only after the request waiter is registered, which prevents an immediately buffered `message` event from racing ahead of its id.
- OAuth is a state machine, not a bearer-header toggle: protected-resource discovery selects the authorization server; metadata supplies authorize/token/register endpoints; DCR supplies a client when config does not; PKCE verifier and CSRF state persist before returning the browser URL; code or refresh exchange writes tokens through `McpAuthStore` at `0600`.
- The configured live file is JSONC and currently contains fields the Task 46 schema does not accept globally. The live test strips JSONC and decodes only the two `mcp` entries it owns, so unrelated future config fields cannot turn a reachable transport proof into a schema-validation skip.

## [2026-08-06] Task 53 — durable SSE replay and live fan-out

- A reconnect-safe stream must subscribe before taking its replay snapshot. The
  snapshot's sequence boundary then suppresses live events at or below that
  boundary, closing both the replay/live gap and duplicate window.
- SSE cursors are session-bound (`<session>:<sequence>`). Parsing with the final
  colon keeps the session portion opaque, and rejecting a cursor from another
  session prevents cross-session replay.
- Reusing `event`/`event_sequence` needs a separate aggregate namespace:
  `sse:<session>`. Using the bare session id would couple SSE cursor allocation to
  unrelated domain events already persisted for that session.
- SQLite `:memory:` is connection-local. Migrating a standalone connection and
  then opening a pool creates two different databases; migration must run through
  the exact pool used by the service. A failing-first test and live curl both
  caught this despite the workspace build being green.

## [2026-08-06] Task 63: lean built-in agent roster

**opencode's real native set is seven, not three, and only three of them are internals.**
`packages/opencode/src/agent/agent.ts:140-265` declares `build, plan, general, explore,
compaction, title, summary` — already ported as data in `oc_catalog::agent::builtin`
(todo 13/18). Sorting them by *who invokes them*:

| native | invoked by | what is lost if absent | lean roster |
| --- | --- | --- | --- |
| `build` | the user | the primary write-capable turn | replaced by `orchestrator` |
| `plan` | the user (a mode) | a primary agent that cannot edit; the only home for `plan_exit` | **left to the catalog's natives** (see decisions) |
| `general` | a `task` call | a bounded executor | replaced by `worker` |
| `explore` | a `task` call | fast repo recon | replaced by `explorer` |
| `compaction` | the engine, on overflow | auto-compaction — nothing else provides it | **carried** |
| `title` | the engine, after turn 1 | readable session lists | **carried** (temp 0.5 is upstream's, `agent.ts:239`) |
| `summary` | the engine, on resume | session summaries | **carried** |

The three carried ones share a signature that identifies an internal mechanically:
`hidden: true` + a prompt + `{"*": "deny"}`. `oc-agent` carries them **by reference** —
`internal()` calls `oc_catalog::agent::builtin::get(name)` and reuses `native.prompt`,
`native.mode`, `native.hidden`, `native.temperature` — so the upstream prompt text
(`prompt/*.txt`, md5-verified at import by todo 13) still lives in exactly one crate.
A test asserts the reused prompt is the catalog's pointer, not a copy.

**Capability type used for vision detection: `oc_llm::catalog::resolved::ModelCapabilities`,
field `input.image`** (a `ModalityFlags` bool, populated from models.dev's
`modalities.input` at `oc-llm/src/catalog/merge.rs:194`). Do **not** use the sibling
`attachment` flag: the pinned fixture
(`oc-llm/tests/fixtures/models-dev-pinned.json:145,160`) contains models with
`attachment: true` whose only input modality is `text`, so `attachment` over-reports and
would put `looker` in the roster for a model that errors on an image. There is a test
pinning that distinction (`attachment_support_alone_does_not_make_a_model_vision_capable`).
Note `ModalityFlags::default()` is text-on/everything-else-off, and
`merge.rs:972-989` shows a config declaring `["image"]` turns **text off** — the flags are
independent, not additive.

**`oc_permission::visibility::permission_key` collapses aliases before any lookup**, so a
permission set is not a tool list. `edit`/`write`/`apply_patch` all key to `edit`, and
`is_tool_hidden("write", rules)` looks for rules whose `permission` wildcard-matches
`"edit"` — a rule literally keyed `"write"` never matches anything. Also: `evaluate` and
`is_tool_hidden` both take the **last** matching rule, so a deny-by-default set must emit
`{"*","*",Deny}` first and its allows last. Emitting them the other way round yields a set
that reads as an allow-list and behaves as deny-all; there is a test asserting allow
positions are all greater than the wildcard's index.

## [2026-08-06] Task 57 — plugin contract and ordered hook bus

- Count only property signatures directly inside `interface Hooks`: there are 21.
  Regex-counting every `?:` also counts nested optional fields and callback parameters,
  which produced the stale 24. `HookName::ALL` is now an executable ordered oracle.
- `tool`, `auth`, and `provider` are resources, not `(input, output)` callbacks. The bus
  gathers them through dedicated trait methods while still exposing them as typed
  `HookInvocation` variants, so one exhaustive table covers the full interface without
  pretending resource maps are functions.
- Auth prompt validators, deprecated conditions, loaders, authorizers, and OAuth
  callbacks are live function handles. They are represented by `Arc<dyn ...>` callback
  traits rather than serialized data, preserving the resident-runtime requirement of
  Task 60.
- Mutation order is observable behavior. `HookBus` stores plugins exactly in supplied
  configuration order and awaits each callback before invoking the next; the test uses
  add-then-multiply on one temperature so concurrent or reversed dispatch cannot pass.
- Relative plugin specs must be resolved before config layers lose provenance. Auto
  discovery scans `plugin/` then `plugins/`, sorts entries within each directory, includes
  dotfiles and symlinks, and preserves the supplied config-directory order.

## [2026-08-06] Task 70 — execute composition

- Binding must consume structured metadata, not scrape rendered tool text. Adding
  unique `grep.metadata.files` made `$each` fan-out stable and machine-readable.
- Deterministic output and concurrent execution are compatible: submit ready calls
  concurrently, retain their declaration indices, then sort completed records only
  at the rendering boundary.
- Recursion protection belongs after alias normalization. Guarding only the literal
  `execute` spelling leaves aliases as a trivial bypass.
## [2026-08-06] Task 52 — owned HTTP API surface

- The oracle exposes 58 `/api` operations. Excluding the two event streams owned by
  task 53 leaves exactly 56 operations; the generated OpenAPI registry is tested as
  a method/path set against the pinned 1.18.12 fixture.
- Session listing belongs on `oc_db::session::ListQuery`: the HTTP layer only parses
  the mutually exclusive directory/project scopes, applies the API default limit
  of 50, and selects updated or created ordering. Literal subtree matching remains
  in the store, where `%` and `_` cannot become wildcard bugs.
- `oc-pty::PtyService` already provides the complete synchronous lifecycle needed
  by list/create/get/update/delete handlers. Operations with no local backend are
  registered explicitly and return structured `501 not_implemented` responses;
  they never fabricate successful payloads.
- Production startup opens and migrates the shared database, seeds the global
  project idempotently, and merges the API router before server middleware. Tests
  use an isolated shared-memory pool through the same initializer.


## [2026-08-06] Task 55: exhaustive CLI surface and version gate

The mechanically extracted `packages/opencode/src/index.ts:45-103` registration set has **23** symbols:
`AcpCommand`, `AgentCommand`, `AttachCommand`, `ConsoleCommand`, `DbCommand`, `DebugCommand`,
`ExportCommand`, `GenerateCommand`, `GithubCommand`, `ImportCommand`, `McpCommand`, `ModelsCommand`,
`PluginCommand`, `PrCommand`, `ProvidersCommand`, `RunCommand`, `ServeCommand`, `SessionCommand`,
`StatsCommand`, `TuiThreadCommand`, `UninstallCommand`, `UpgradeCommand`, `WebCommand`.

The committed fixture is regenerated mechanically with:

```sh
sed -n '45,103p' packages/opencode/src/index.ts | grep -oE "[A-Z][A-Za-z]*Command" | sort -u
```

`checkPluginCompatibility` is a semver-**range** check, not exact equality
(`packages/opencode/src/plugin/shared.ts:194-204`): invalid versions and major-zero versions bypass the
check; otherwise `package.json`'s string `engines.opencode` is tested with
`semver.satisfies(opencodeVersion, range)` and mismatch throws. `loader.ts:123-130` applies this only
to npm plugins, catches the throw as stage `compatibility`, and therefore skips loading that candidate;
file plugins bypass it. The compatibility identity must consequently be valid stable semver `1.18.13`.

`flag.ts:3-78` contains 33 unique `OPENCODE_*` names. The startup snapshot deliberately contains 37:
those 33 plus the CLI/runtime values `OPENCODE`, `OPENCODE_PID`, `OPENCODE_PRINT_LOGS`, and
`OPENCODE_LOG_LEVEL`. It reuses `oc_paths::Env` for `Flag.truthy` inputs and
`oc_tools::exposure::ExposureFlags` for the measured experimental fallback, so
`OPENCODE_EXPERIMENTAL=true OPENCODE_EXPERIMENTAL_PLAN_MODE=false` still leaves plan mode off.

## [2026-08-06] Task 58: out-of-process JSON-RPC plugins

- The stdio protocol is strict NDJSON JSON-RPC 2.0. One reader classifies every
  frame before consulting the id-indexed waiter map, so notifications, server
  requests, and unknown response ids cannot consume another request's response.
- Startup concurrency and dispatch ordering are separate properties:
  `join_all` starts every plugin initialization concurrently and preserves the
  input vector's order; the existing sequential `HookBus` remains the sole
  authority for applying mutations. A two-process startup gate proves startup is
  actually concurrent, and non-commutative text mutations prove dispatch order.
- Every initialization, hook, and tool request uses the process spec's deadline.
  A timeout disables the plugin, records one typed diagnostic, fails pending
  requests, signals process shutdown, and lets the bus complete the turn.
- Explicit shutdown closes stdin, signals and reaps the child, and joins the
  reader/supervisor tasks with bounded grace periods. `Drop` remains a safety net
  that aborts the reader and transfers the child task to Tokio rather than
  blocking synchronously.
- `oc-plugin-sdk` reserves stdout for protocol frames and exposes a reusable
  conformance suite. The 158-line Rust example registers one tool and three hooks
  and passes that suite without host-private types.
- Mutation proof caught both load-bearing properties: replacing the injected
  60 ms request deadline with 2 s failed the hung-hook test, and reversing the
  resolved plugin vector failed the configuration-order test. Restoring the
  implementation returned all five JSON-RPC integration tests to green.

## [2026-08-06] Task 47: the namespacing rule as it behaves, and the one-shot rebuild

### `tool_name` is `sanitize(server) + "_" + sanitize(tool)` and nothing else

Todo 45 ported it; I reused it rather than writing a second rule. Measured behaviour,
not paraphrased:

- Allowed set is `[a-zA-Z0-9_-]`. Every other UTF-16 code unit becomes one `_`.
- Separator is a plain `_`. **There is no length truncation** — the oracle
  (`mcp/catalog.ts:117-119`) has none, so a 300-character server name produces a
  300-character prefix.
- `search` on `docs` → `docs_search`. `search/all` on `my docs` → `my_docs_search_all`.

The consequence that matters for design: **the rule is not injective.** `a.b` and
`a/b` both sanitize to `a_b`, and an underscore inside a server name makes the
server/tool boundary ambiguous. So a namespaced id can never be split back into
`(server, tool)` to route a call. The oracle avoids this by capturing the original
`ToolDefinition` and the client in the closure (`mcp/catalog.ts:42-67`); my
`McpToolProxy` stores `id` (namespaced, for the model) and `tool` (server-local, for
the wire) as two separate fields. Only code-mode splits, and only for display
(`tool/code-mode.ts:39-55`).

The plan's reference to `session/tools.ts:388-492` says the namespaced id is "split
back into server+tool at call time". It is not. Do not implement it that way.

### The locked-list rebuild fires exactly once, and the mechanism is not mine

Todo 31's `LockedTools::tools_for_request(available, status)` owns the once-only
property. Reading its body is the only way to feed it correctly:

- First call always freezes `available`, and records `late_mcp_resolved = (status == Ready)`.
- If the first call was `Pending`, the *first* subsequent `Ready` compares the whole
  list; if it differs, the snapshot is replaced, `rebuild_count = 1`, and
  `rebuilt_for_late_mcp` is true for that one request.
- **`late_mcp_resolved` is set to true on that first `Ready` whether or not the list
  changed.** So a second late connection can never rebuild.
- Only `reset()` re-arms it.

Therefore the integration rule is: feed the **complete** id list (order and content
both participate in `PartialEq`) plus a discovery status, and **never call `reset()`**.
`Catalog::tools_for_request` is three lines for exactly that reason.

The status signal needed inventing. `Catalog::new(expected_server_names)` records what
configuration asked for, and each `connected`/`unavailable` removes one name;
`discovery_status()` is `Ready` only when the expected set is empty. Consequences:
a server that never reports keeps discovery `Pending` forever, which is the safe
answer (the allowance is held in reserve rather than spent on a partial view), and
zero configured servers is `Ready` immediately rather than never.

### A mutation test found a vacuous pass in my own test

My first `unavailable()` cleared both the status **and** `entry.handle`. The
connected-only test still passed with the gate deleted, because the tools disappeared
for a second, accidental reason. I changed the code so `unavailable` keeps the tools
*and* the handle, making `ServerStatus::is_connected()` the single decision point —
then deleting the gate leaks `broken_dangerous` into the merged list and the test says
so. Retaining state you could have dropped is sometimes what makes a rule testable.

### Every MCP list method pages the same way

`resources/list`, `resources/templates/list`, and `prompts/list` all use
`{cursor}` → `{<key>: [...], nextCursor}`, so one generic `fetch_list(method, key)`
covers them. `tools/list` deliberately keeps its own loop: it also swaps the cached
tool snapshot, and folding it in would couple that cache to every pure read.

## [2026-08-06] Task 79: the built-in formatter table, and how to stub a formatter

### The table is 26 entries, and two are keyed under a name that is not their export

`grep -c '^export const' packages/opencode/src/format/formatter.ts` = **26**. Two
of those exports carry a `name` field that differs from the identifier, and the
config keys on the **`name`**, not the export:

- `export const clang` -> `name: "clang-format"` (`formatter.ts:166-167`)
- `export const rlang`  -> `name: "air"`          (`formatter.ts:218-219`)

So `{"formatter": {"clang": {...}}}` silently declares a *new* formatter with no
extensions rather than overriding clang-format. Asserted both ways:
`definition("clang-format").is_some()` and `definition("clang").is_none()`.

### The table is language -> command PLUS a per-formatter availability CLOSURE

The plan describes it as "a table of language -> command". It is not: each `Info`
carries `enabled(context): Promise<string[] | false>`, and there are **eight
distinct closure shapes** across the 26. Ported as an `Availability` enum so the
table can be walked by a test — a table of closures cannot be:

| variant | who uses it | oracle |
|---|---|---|
| `Program` | most of the table | `which(x)` |
| `ProgramWithMarker` | clang-format, ocamlformat | `which` + `findUp(marker)` |
| `ProgramWithHelpFirstLine` | air | `--help` line 1 has "R language" AND "formatter" (`:218-234`) |
| `ProgramWithHelpExitZero` | uv | `uv format --help` exits 0 (`:236-247`) |
| `NodeMarker` | biome | `biome.json`/`biome.jsonc` + `Npm.which` |
| `NodePackage` | prettier, oxfmt | `package.json` declares it + `Npm.which` |
| `VendoredPackage` | pint | `composer.json` declares `laravel/pint`; command is `./vendor/bin/pint`, **never** a PATH lookup (`:360-374`) |
| `RuffConfig` | ruff | three layers (`:189-216`) |

Plus two orthogonal flags: `oxfmt` is behind `experimentalOxfmt` (`:96`), and
`uv` is `shadowed_by: ruff` (`:238` — `if (await ruff.enabled(context)) return false`).
The shadow check must live **outside** the availability probe or the two recurse:
asking "is ruff available" must not ask "does anything shadow ruff".

### `path.extname()` means one of the oracle's own entries can never match

`htmlbeautifier` claims `".html.erb"` (`formatter.ts:271`). `path.extname()`
returns only the final segment, so `index.html.erb` is `".erb"` — that entry is
dead upstream too. Carried verbatim; silently correcting the oracle's table would
be a divergence hiding inside a port. `the_extension_is_the_final_segment_with_a_leading_dot`
pins the semantics.

### Ruff's fallback is a substring match, and that is upstream's rule

`formatter.ts:205-215`: after the config files, ruff is enabled if
`requirements.txt`/`pyproject.toml`/`Pipfile` *contains the string* `"ruff"`. A
comment naming ruff counts. Loose, but tightening it here would silently stop
formatting projects the oracle formats.

### How to stub a formatter deterministically, without downloading one

Nothing is installed (todo 41's ripgrep line held). Each test writes its own
`/bin/sh` script into a per-test `tempfile::tempdir()`, behind one `script()`
helper so no site can skip the pre-flight:

- `rewriting_stub` — rewrites the file to a byte-exact known value, exit 0. Makes
  "did it format" an equality check rather than a guess about a real formatter's
  style.
- `failing_stub` — leaves the file alone, literal stderr, `exit 3`. Makes the
  stderr assertion exact; a real formatter's message varies by version.
- `destructive_stub` — **truncates the file and then fails**. This is the case
  that decides whether an edit can be lost, and no real formatter reproduces it
  on demand.
- `recording_stub` — appends every path it was handed to a log, then rewrites.
  Turns "was this formatter even offered the file" into a line count.
- `hanging_stub` — `sleep 600`, for the ceiling.

Three details that are not obvious:

1. **Target the LAST positional argument**, not `$1`. A built-in command is
   `clang-format -i $FILE`, so `$1` is the flag. `for target in "$@"; do :; done`
   resolves the last one in POSIX sh.
2. **Every stub changes the file's LENGTH as well as its content**, so "the bytes
   changed" cannot be true by coincidence. Habit borrowed from the `oc-snapshot`
   stat-cache flake, even though nothing here consults git.
3. **Inject the program locator; never touch `PATH`.** Mutating the environment
   is `unsafe` and forbidden in this workspace. A stub `ProgramLocator` also lets
   a test drive a *built-in* definition on a machine that does not have that
   formatter installed. Note a **configured** command is taken verbatim
   (`format/index.ts:154` replaces the probe with `async () => info.command ?? false`),
   so those fixtures pass an absolute path; the locator governs the built-ins.

### The oracle's positive-only cache means not caching is unobservable

`format/index.ts:42-48` caches `enabled()`'s answer per formatter, but
`cmd === false || cmd === undefined` re-probes — so a negative is never cached.
Skipping the cache entirely therefore changes nothing observable, and it means an
operator installing a formatter mid-session is picked up.

### `findUp`'s stop bound includes the stop directory itself

`util/filesystem.ts:192-200` pushes `start`, then loops `if (stop === current) break`
**before** pushing the parent — so `stop` is in the list and its parent is not.
A port that stops one directory early misses a marker at the worktree root, which
is exactly where `biome.json` and `.clang-format` usually live.

## [2026-08-06] Task 54: the re-measured plugin call set is 20 methods, not six

**The plan's "six SDK methods" undercounted by 3.3x, for two independent reasons.**
Both are re-run hazards, not plan sloppiness, and both will recur.

1. **The plugin list had three entries, not two.** `/config/.config/opencode/opencode.json:87-92`
   enables `opencode-antigravity-auth@1.6.0`, `@sunerpy/opencode-kiro-auth@0.20.1`
   **and** `@sunerpy/oh-my-openagent@4.21.0` (line 89 is a commented-out `file://`
   spec). The plan's prose said "the two auth plugins"; the third is a
   session-orchestration plugin and is responsible for **13 of the 14 additions**.
   The config lines the plan cited already contained it.
2. **One callsite aliases the SDK namespace.** `client.app.log` is written
   `const app = _client?.app;` then `app.log({...})`
   (AG `dist/src/plugin/logger.js:45-50`). A grep for `client.` cannot see it.
   **Any future capture must search method-path fragments — `auth.set`,
   `showToast`, `app.log` — independently of the receiver name.**

**`client.provider.oauth` is not a method.** It is a namespace object
(`Provider.oauth = new Oauth(...)`, `packages/sdk/js/src/gen/sdk.gen.ts:715-750,753-774`)
whose two children are what plugins call: `.authorize` and `.callback`, both from
KIRO `dist/core/request/request-handler.js:783-790` with body `{method: 0}`.
Implementing the plan's spelling literally would have served
`/provider/{id}/oauth` — a path the oracle does not have — and left both real
calls unrouted. **A namespace in a call chain reads exactly like a method; check
the SDK for `new` before mapping one to a route.**

Confirmed verbatim from the plan's six: `auth.set`, `session.abort`,
`session.messages`, `session.prompt`, `tui.showToast`. Net: 6 -> 20 routes, all 20
present in `.omo/fixtures/oracle-openapi-1.18.12.json`.

**The SDK does not prefix its requests.** `InstanceHttpApi` composes its groups
with no `.prefix("/api")` (`.../httpapi/api.ts:61-76`) and the generated client
asks for bare `/session/{id}/abort`, `/tui/show-toast`
(`packages/sdk/js/src/gen/sdk.gen.ts:437,1120`). Serving only `/api/*` leaves
*every* plugin call unrouted — that is why a pre-`/api` surface exists at all,
and why the toast path is `/tui/show-toast` (server `groups/tui.ts:45,140-149`,
SDK `sdk.gen.ts:1115-1126`), not `/tui/showToast`.

### axum: a scoped catch-all is `nest` + inner fallback, never a `{*rest}` sibling

Measured the hard way — 12 of 16 tests failed on the first attempt:

```
Invalid route "/auth/{*rest}": Insertion failed due to conflict with
previously registered route: /auth/{providerID}
```

`matchit` treats a wildcard and a named parameter **at the same depth** as
*conflicting*, not as one outranking the other. So "register the specific route,
then a `{*rest}` above it" **panics at assembly** on the first prefix that has a
parameterised route. A global `Router::fallback` is the other dead end: it also
answers for unmatched `/api/*`, and merging two routers that both have a fallback
panics.

What works: **one `Router::nest` per prefix, each inner router carrying its own
fallback.** axum grafts an inner fallback into the outer router *at the nest
prefix* (`axum-0.8.9/src/routing/mod.rs:227-229`) — exactly the scoping needed.
Three consequences worth knowing before reaching for this:

- The nest **strips its prefix** before the fallback runs, so the handler must
  read `OriginalUri` or it reports a path the caller never sent.
- A nest at `/foo` matches bare `/foo`, but the grafted fallback sits one segment
  deeper — prefixes with no measured root route need an explicit bare route.
- `method_not_allowed_fallback` must be applied **after** the routes it covers;
  axum retrofits it onto already-registered `MethodRouter`s only
  (`path_router.rs:116-126`).

## [2026-08-06] Task 73: TUI lifecycle and TTY ownership

Terminal restoration is testable without a controlling TTY by separating the
idempotent `TerminalLifecycle` transition from crossterm. `FakeLifecycle` records
active state and enter/restore ordering while ratatui's `TestBackend` proves the
component tree and reclaim repaint. The headline panic test panics from
`Component::handle_event`, observes that the panic hook made the lifecycle inactive
*before* the reporter ran, and then proves the guard's later drop is idempotent.

Panic hooks are process-global, so replacing/restoring one in every test is itself a
race. Task 73 installs one permanent dispatcher with `OnceLock`, delegates to the
pre-existing hook outside an active session, and serializes active session contexts
with a process-global mutex guard held for the session lifetime. Eighteen concurrent
test processes across three rounds all returned zero failures.

## [2026-08-06] Task 64: preset-based model policy, and slim's installer sleight-of-hand

**Slim's `DEFAULT_MODELS` is nine `undefined` entries, and the comment above it is the
whole design argument** (`.omo/refs/omo-slim/src/config/constants.ts:26-41`), verbatim:

> Default models for each agent.
> All set to undefined so agents follow the global/session model.
> Users can override per-agent via oh-my-opencode-slim.json agents.\<name\>.model.

**Slim's five presets are NOT shipped policy — they are installer scaffolding.**
This is the fact that decides where preset data may live. `MODEL_MAPPINGS`
(`src/cli/providers.ts:11-56`) has five agent-keyed maps — `openai`, `kimi`,
`copilot`, `zai-plan`, `opencode-go` — each `{agent → {model, variant}}` over six
agents (`opencode-go` adds a seventh). But its ONLY consumer is `generateLiteConfig`
(`:79-137`), which **writes them into the user's config file at install time**
(`config.presets[presetName] = buildPreset(presetName)`). The runtime then reads
`config.presets` (`src/index.ts:209-215`) and never touches the constant. Two of the
five are even gated behind `GENERATED_PRESETS = ['openai', 'opencode-go']` (`:8`) —
the other three are unreachable from the installer at all.

So "provide named presets" and "no model id literal in the crate" are not in tension:
preset **shape** is code, preset **data** is configuration. A preset compiled into the
binary is `CATEGORY_MODEL_REQUIREMENTS` with better manners and rots identically.

**Two preset body shapes have to be accepted, and an untagged enum silently eats one.**
Flat (`{agent: {model, variant}}`, what slim's installer emits) is a *superset* of the
structured field set (`{"agents": {...}, "categories": {...}}`), so
`#[serde(untagged)]` reads `{"worker": …}` as a structured body with two defaulted
empty maps and **drops every entry without a word**. `PresetBody`'s `Deserialize` is
hand-written and branches on the presence of a reserved section key instead.

**The shape of omo's Claude thinking-budget conditional, ported without its model list.**
`oh-my-openagent/dist/index.js:28822-28829`:

```js
var CLAUDE_THINKING_BUDGET_TOKENS = 32000;
function buildClaudeThinkingConfig(model) {
  if (isClaudeOpus47OrLaterModel(model) || isClaudeFableOrMythosModel(model)) {
    return {};                     // newer models: let native variants take over
  }
  return { thinking: { type: "enabled", budgetTokens: CLAUDE_THINKING_BUDGET_TOKENS } };
}
```

The valuable half is the *empty return* — "the model knows better than my table, get
out of the way". The rotten half is `isClaudeOpus47OrLaterModel`: a hand-written
name-matching predicate (there are also `isGptNativeSisyphusModel`, `isGpt5_5Model`,
`isGpt5_6Model` right below it at `:28832-28843`, each a regex or substring over the
model name), so every model release needs a new one. Ported version: the branch is
taken on a **catalog fact**. `model_policy::declared_variants` lifts
`ResolvedModel::variants` — keyed by **name** — into todo 31's `DeclaredVariants` —
keyed by canonical **effort** — so a model that declares `"max"` wins over the generic
provider-family mapping inside `resolve_effort`, and a model that declares nothing
gets the budget shape. A variant name that is *not* a canonical level but *is*
declared (slim ships one: `variant: 'thinking'`, `providers.ts:48`) comes back
verbatim as `EffortOutcome::ModelVariant` with the catalog's own option object.
Nothing is synthesised, and no predicate needs updating when a model ships.

**A crate-wide model-id source scan finds a false positive the prose scan never could.**
Todo 63's `looks_like_model_id` only ever saw *rendered strings*. Pointed at source it
failed instantly on `builtin.rs:49`'s citation `` `dist/index.js:24475` `` — a
two-segment path with digits on the right is indistinguishable from `provider/model`.
Fix: exclude tokens containing `:` from the `provider/model` branch only (no provider
spells a model with a colon; the family branch runs first so real ids stay caught).
The predicate therefore had to **move** into `model_policy` and be shared rather than
duplicated — `builtin.rs` was owned by another task and could not be edited.

**Every guard test that walks a directory needs BOTH floors.** `>= 6 files found` and
`>= 3 files scanned`, because the exclusion list (`tests.rs`) could otherwise grow
until the scan covers nothing. The exclusion itself is *verified*: for each excluded
file the test reads the parent module and asserts it contains
`"#[cfg(test)]\nmod tests;"`, so "a test module cannot reach the binary" is checked
rather than assumed.

**omo's category count is eight, and the plan's line numbers are one-off-inside-the-object.**
`CATEGORY_MODEL_REQUIREMENTS` (`dist/index.js:24652`, plan said 24660) declares
`visual-engineering, ultrabrain, deep, artistry, quick, unspecified-low,
unspecified-high, writing` — 8, as the plan says — with 1-5 rungs each, every rung a
model id plus up to ten provider ids. `AGENT_MODEL_REQUIREMENTS` is at `:24467` (plan
said 24475). Both plan numbers point inside the object rather than at `var`, so they
find it; nothing else in the plan's description of either table was wrong.

## [2026-08-06] Task 74: the 184-entry keybind table, and what the "odd" call sites are

The prompt's "~20 `keybind(` calls in another position" are not nested or differently
shaped calls. `grep -c 'keybind('` = 184 and `grep -o | wc -l` = 184, so every call is on
its own line inside `Definitions`. The 164/20 split is purely syntactic: 20 entries have a
**dot in the name** and therefore must be written as a quoted object key
(`keybind.ts:202-221` — `dialog.select.*` ×7, `dialog.prompt.submit`, `dialog.mcp.toggle`,
`dialog.move_session.*` ×3, `prompt.autocomplete.*` ×5, `permission.prompt.fullscreen`,
`plugins.toggle`, `dialog.plugins.install`). They are ordinary rows. So the table is 184
entries = 183 actions + the `leader` row (`:46`), which configures the leader chord.

`CommandMap` (`:256-420`) has **163** entries = 164 unquoted names minus `leader`. The 20
dotted names are deliberately absent and reach their command id through
`CommandMap[name] ?? name` (`:423`) — for them the name *is* the command.

Extraction command (self-checking; exits non-zero on an unparsed row or a count != 184),
following todo 55's precedent of a committed mechanically regenerable fixture:

```sh
python3 extract.py packages/tui/src/config/keybind.ts \
  > crates/oc-tui/tests/fixtures/upstream-keybinds-1.18.13.tsv
```

`extract.py` slices the `Definitions` and `CommandMap` blocks by their literal opening and
`satisfies` closing lines, matches `^\s*("?)(name)\1:\s*keybind\((val),\s*"(desc)"\)`,
resolves `LeaderDefault` and the one object-shaped default, and emits
`name\tkeys\tcommand\tprevent_default\tdescription`. Full body in
`.omo/evidence/task-74-opencode-rust.txt`. Verified byte-identical on regeneration.

**The multi-key / leader-mixing rule.** `app_exit: "ctrl+c,ctrl+d,<leader>q"` (`:48`) proves
two things at once: one action carries **several** comma-separated spellings, and a single
spelling list **mixes** plain chords with leader sequences. 28 of 184 defaults contain
`<leader>` and 28 carry more than one spelling. The table is therefore
action → [sequence…], and resolution is sequence → action with pending state — never
action → key.

**Scope derivation, measured rather than guessed.** Upstream attaches bindings to
renderables, so a flat global map would report dozens of "conflicts" in the shipped
defaults. Two candidate rules, measured over the real table:

| rule | scopes | conflicting (scope, sequence) pairs |
|---|---|---|
| first `_` segment | 33 | 1 — `dialog.select.submit` vs `dialog.prompt.submit`, both `return` |
| namespace before last `.`, else first `_` segment | **39** | **0** |

The second rule is implemented. Because the defaults are conflict-free under it, any
conflict a build reports provably comes from user config — which is what makes the report
signal instead of noise. 39 namespaces = 38 action scopes + `leader`. 43 of 184 are `none`.

**Three normalizations a terminal forces.** `enter` and `return` both appear in the table
and are one key. `E` (`:64`, bare uppercase) and `shift+i` (`:221`) are the same shape
spelled two ways. `?` (`:75`) is a shifted glyph a terminal may report with or without the
SHIFT flag. `Chord::new` folds uppercase ASCII into lowercase+shift and strips SHIFT from
non-alphabetic characters, so both spellings and both event shapes resolve.

## [2026-08-06] Task 75: four-layer theme resolution with 33 built-in themes

**The oracle's four-layer order is not the order the plan states, and the oracle
states its own order in a comment.** `packages/tui/src/theme/index.ts:171-183`:

```ts
function listThemes() {
  // Priority: defaults < plugin installs < custom files < generated system.
  const themes = { ...DEFAULT_THEMES, ...pluginThemes, ...customThemes }
  if (!systemTheme) return themes
  return { ...themes, system: systemTheme }
}
```

So it is **built-in < plugin-provided < user custom < system**. The plan's prose puts
user custom *before* plugin-provided. Sixth plan-vs-source discrepancy on the board;
the source was right again.

**The fourth rung of the ladder can only be tested on the name `system`.** The system
layer is not merged key-by-key — `:179-182` publishes it as a single entry named
`system`. It therefore shadows that one name and nothing else, which is a property
worth its own test: getting it wrong would make *every* theme silently become the
terminal-derived one. `theme_layers_override_the_layer_below_them` walks
builtin→plugin→custom on `dracula` and then custom→system on `system`.

**Terminal capability was probed without a TTY by taking the answer as an input, not
by faking the terminal.** The escape-sequence round trip that really answers "what is
your palette" needs the stdin/stdout pair todo 73's `TerminalSession` owns, so
`theme.rs` declares `trait TerminalPalette { fn query(&self) -> Option<TerminalColors> }`
and ports only the *derivation* (`index.ts:360-469`). Tests pass a `FakePalette`
holding an `Option`, so "no terminal" is `FakePalette(None)` — one line, no TTY, no
process state. The real impl reads `COLORFGBG` and returns `None` when it is unset,
which is every non-interactive run.

**`COLORFGBG` had to be parsed by a pure function to keep the suite concurrency-safe.**
Setting an env var to test the probe would race every other test in the process — the
same class of hazard todo 73 hit with the process-global panic hook. `parse_colorfgbg`
is public and tested directly; `EnvironmentPalette::query` is a two-line read over it.
Result: nothing in `theme_tests.rs` touches global state, so the 38 theme tests run
concurrently with the 10 `app_tests.rs` tests that do own the hook. 18 concurrent test
processes across 3 rounds, zero failures.

**`cargo test -p oc-tui theme` only matches test *names*, not target names.** An
integration test in `tests/theme.rs` whose functions are called `resolves_everything`
reports `0 passed; N filtered out` for that filter — a silent skip. Putting the tests
in `src/theme_tests.rs` included as `mod tests` from `theme.rs` makes every path
`theme::tests::…`, so the module path itself satisfies the filter. Measured: 38 passed,
10 filtered out.

**A raw string cannot hold a hex colour if it is opened with a single `#`.** `r#"…"#`
around JSON containing `"#101010"` terminates at the `"#` of the literal. `r##"…"##`.
Two of these; both were parse errors, not silent bugs, but they cost a build cycle.

**One macro-declared field table is what makes 52 palette colours maintainable.**
`declare_palette!` takes `field => "jsonKey"` pairs once and generates the struct, the
`REQUIRED_KEYS`/`OPTIONAL_KEYS` slices, `entries()` (used by the snapshot view), and
the resolution loop. A field the resolver never fills, or a JSON key no field consumes,
is not expressible. Side effect worth knowing: issues come out in *declaration* order
(the oracle's `Theme` member order, `index.ts:36-92`), not the sorted order the backing
`BTreeMap` would suggest — I wrote a test asserting `issues[0].key == "accent"` and it
is `"primary"`.

## [2026-08-06] Task 65: the delegation tool's override precedence, stacked on todo 64's

**The ladder is four rungs now, and rung 1 is a tool argument.** Todo 64 built three
(`per-agent config override > active preset > session model`). `task` adds one above
all of them:

| rung | who says it | where |
| --- | --- | --- |
| 1 | **this `task` call's `model` / `effort` arguments** | `task::TaskTool::plan` |
| 2 | `agent.<name>.model` from the user's config | `ModelPolicy::with_agent_override` |
| 3 | the active preset's entry for the agent, or for the `category` shorthand | `ModelPolicy::resolve` / `resolve_category` |
| 4 | the parent session's model | `ModelPolicy::with_session_model` |

The stacking is *additive, not a reimplementation*: `plan()` calls
`resolve`/`resolve_category` for rungs 2-4, keeps the returned `Resolution`'s
diagnostics, and only then lets an explicit `model` argument displace
`Resolution::model`. Nothing about rungs 2-4 is restated in `oc-tools`. Verified by
two tests: one where the call argument beats a config override *and* a preset entry,
and one where removing the argument lets the preset rung answer again.

**Rung 1 inherits todo 64's skip-on-unavailable rule, and it matters more here.** An
unreachable or unqualified `model` argument does not fail the delegation — it becomes
a note and the ladder continues. Refusing outright would throw away a task the caller
has already framed over a model name it guessed. What is forbidden is *silence*: every
skip lands in `DelegationPlan::notes` and is rendered inside the result envelope as
`<note>…</note>`, so a caller reading only the body still learns its `effort` was
dropped. That is the concrete reading of "must not accept a model or effort the
resolved provider cannot honor **without saying so**".

**Effort honouring needs three separate refusals, and only one of them is todo 64's.**
`resolve_variant` already answers "is this name canonical, model-declared, or
neither". Two more questions have to be asked *before* it, and they are the ones a
tool can answer and a config-time resolver cannot:

1. no model resolved at all → nothing can be asked about reasoning support;
2. the model resolved but `reasoning == false` → a canonical level cannot be honoured,
   and sending reasoning options to a non-reasoning model is a provider error, so the
   options are dropped **and** noted.

`ReasoningEffort::Off` is exempt from (2): asking a non-reasoning model not to reason
is trivially honoured.

**`ToolContext::depth` alone cannot bound delegation recursion.** It counts *tool
composition* (`for_subcall` increments it), so it is `0` for every turn-level call —
including a `task` call made inside a child session, which is precisely the hop the
bound exists to stop. Upstream's measure (walking `parentID`,
`packages/opencode/src/tool/task.ts:106-117`) is the mirror image: it sees delegation
hops and is blind to a `task` nested inside `execute`. Each is blind to the other's
recursion, so the guard is `max(session_ancestry, ctx.depth) >= subagent_depth`, with
a test for each half. The session half needs a host method (`delegation_depth`)
because only the session store knows the parent chain.

**`ToolError::Denied` has no `#[source]`**, so no fix-naming prose can ride on a
permission refusal. Every other rejection can chain a typed error and be asserted
through `error.source().to_string()`; a denial cannot. The guidance therefore travels
on the outbound `PermissionAsk` metadata (`task::GUIDANCE_KEY`) — which is arguably
the better place anyway, since that is what the human approving actually reads — and a
recording asker makes it assertable. Worth knowing before writing another gated tool
whose acceptance criterion mentions a denial message.

**`#[schemars(skip)]` + a declared serde field is how you refuse an argument by name.**
`deny_unknown_fields` alone gives serde's generic "unknown field" error, which names
no fix. Declaring `load_skills` on the params struct but hiding it from the derived
schema means: no caller learns the name from this tool, a caller that sends it anyway
is refused with a message pointing at per-agent permissions, and every *other* unknown
field still gets `deny_unknown_fields`. Confirmed working on schemars 1.2.2.
## Task 101: background reflection fork

### Delivery, not invocation, is the scheduling boundary

Reflection bookkeeping must advance only after a turn produced a final response
and was not interrupted. Counting tool-loop attempts or interrupted turns makes
the periodic trigger nondeterministic from the user's perspective. The fork now
accepts both facts explicitly and rejects the turn before evaluating triggers.

### A narrow injected tool boundary is stronger than prompt-only prohibition

The background runner receives an injected `Arc<dyn oc_tool::Tool>` but dispatches
only the exact id `memory`. A second structural guard fixes compaction to
`CompactionMode::Disabled`. This keeps the fork isolated from shell tools and from
the parent transcript even if a runner attempts unsupported behavior.

### Recovery evidence needs adjacency and a completed outcome

The useful fail-then-succeed signal is an adjacent repetition of the same command,
not merely two matching commands anywhere in the transcript. Requiring a later
successful outcome prevents unresolved failures and one-off incidents from being
promoted into durable memory. The five Hermes exclusions are therefore executable
filters rather than prompt prose.

## [2026-08-07] Task 77: the six audio assets, and how a display server and an audio device were faked

**The six imports, and there are only FIVE distinct files.** `attention.ts:17-22`
imports six paths from the excluded `@opencode-ai/ui`; `attention.ts:47-56` maps them
to slots:

| slot | file |
|---|---|
| `default` | `@opencode-ai/ui/audio/bip-bop-01.mp3` |
| `question` | `@opencode-ai/ui/audio/bip-bop-03.mp3` |
| `permission` | `@opencode-ai/ui/audio/staplebops-06.mp3` |
| `error` | `@opencode-ai/ui/audio/nope-03.mp3` |
| `done` | `@opencode-ai/ui/audio/bip-bop-01.mp3` — **the same file as `default`** |
| `subagent_done` | `@opencode-ai/ui/audio/yup-01.mp3` |

Six imports, five files. The plan says "four mp3 files" twice, including in its
MUST-NOT. The slot *names* are the compatibility surface even when the bytes are not,
so `SoundName` keeps all six spellings verbatim and `attention.sounds.<slot>` lets a
user fill them one at a time.

**The upstream asset directory is 90 files and carries no attribution.**
`packages/ui/src/assets/audio/` holds 45 `.mp3` + 45 `.aac` — `alert-01..10`,
`bip-bop-01..10`, `nope-01..12`, `staplebops-01..07`, `yup-01..06`. Measured: no
LICENSE, NOTICE, README, or `.txt`/`.md` of any kind under `packages/ui/src/assets`.
`packages/ui/package.json` says `"license": "MIT"` and `packages/ui/LICENSE` is the
MIT text — that covers *the package*, not the redistribution rights in a numbered
sound library someone bought. Hence the decision in `decisions.md`.

**`renderer.triggerNotification` returns a synchronous `boolean`, and that is the tell.**
`attention.ts:30` types it `triggerNotification(message, title?): boolean` and
`:185` calls it. OpenTUI's implementation is not vendored anywhere in the checkout
(`grep -rn triggerNotification` finds only those two lines). Every platform's native
notification API is asynchronous, so a synchronous boolean cannot be waiting on one —
what it can be doing is writing an escape sequence and letting the emulator raise the
notification. That is why the Rust port's real notifier writes **OSC 777**
(`ESC ] 777 ; notify ; <title> ; <body> ST`), understood by kitty, WezTerm, foot, and
urxvt and ignored by everything else. Ignoring is the right failure for a courtesy
channel, and it means the notification path needs no platform dependency at all.

**Faking a display server: make the sink a type parameter, not a mock.**
`OscNotifier<W: Write>` is the *real* implementation and a test constructs it over a
`Vec<u8>`, so the assertion is on the exact bytes rather than on "a mock was called".
That is stronger than a mock and it is also the correct design: a TUI that owns the
alternate screen must decide for itself which stream a sequence goes to.

**Faking an audio device: the trait's default implementation plays nothing, and that
is also what ships.** `SilentPlayer` is not a test double — it is the only real
`SoundPlayer` today, because the crate ships nothing to decode. So the "no hardware in
tests" property and the "no unlicensable assets" decision are the *same* fact, not two
constraints that had to be reconciled. `RecordingPlayer` records `(path, volume)` per
call and answers a configured bool, which is what makes *"the channel was never
reached"* distinguishable from *"the channel said no"* — a distinction the
enabled-matrix test depends on, since it asserts call counts and not just outcomes.

**A `Vec<Diagnostic>` compared with `assert_eq!` breaks on NaN.** The volume-clamp
table had a `(f64::NAN, 0.0)` row; `VolumeClamped { configured: NaN }` is never equal
to itself, so the failure printed two identical-looking sides. Pulled that row out and
asserted it structurally with `matches!(.. if configured.is_nan())`. Any diagnostic
enum carrying an `f64` has this landmine.

**A guard test that forbids a macro must exclude the file that names it.**
`attention_no_audio_asset_is_compiled_into_this_crate` greps every `.rs` under the
crate for `include_bytes!` — and found its own two occurrences. Excluding
`attention_tests.rs` is safe (it is `#[cfg(test)]`, so it cannot reach a binary) but
the exclusion has to be *justified inline*, per todo 64's rule, or the guard quietly
becomes a guard over nine files instead of ten. Floors: `>= 40` files walked,
`>= 6` sources read; measured 79 and 10.

## [2026-08-07] Task 66: session continuation and the background job board

### omo/opencode's continuation semantics, as they actually are in the source

`packages/opencode/src/tool/task.ts` resolves `task_id` in exactly one expression:

```ts
const session = params.task_id ? yield* sessions.get(SessionID.make(params.task_id)) : undefined
const nextSession = session ?? (yield* sessions.create({ parentID: ctx.sessionID, ... }))
```

(`:136-137` and `:167-172`). That is the whole of it — **reuse in place, no history
rebuild**, and the `?? create` shape is why "no `task_id`" silently means "fresh
session" rather than an error. There is nothing to port beyond the shape; the work is
the *state* that makes the handle resolvable, which upstream keeps in a background-job
service and slim keeps in a plugin-side board.

### The two id spaces are genuinely independent, and upstream proves it by accident

Upstream reports `jobId` on two adjacent code paths with two different meanings:

- `background.extend(...)` (an amendment to a running lane) reports
  `jobId: nextSession.id` — a **session** id (`task.ts:256-263`);
- `background.start(...)` (a new lane) reports `jobId: info.id` — the background
  **service's** handle (`task.ts:288-295`).

Same field, same tool, same turn, two id spaces, and the caller cannot tell which it
got. So "one continuation may keep the session id while getting a fresh background id"
is not a design nicety — it is the only way to describe what upstream already does, and
naming the two spaces separately is the fix. Here: the session id is the conversation,
the job id is one dispatch into it, and a lane accumulates job ids over its life while
keeping one session id and one alias.

### slim's Active rule is prose, and prose is the wrong layer for it

`.omo/refs/omo-slim/src/agents/orchestrator.ts:226-231` ("Active Task Amendments")
tells the model that a running lane "cannot receive another `task` call, even with its
`task_id`". But slim's board still *lists* the lane, and slim's tool still *accepts*
the call — the only thing stopping it is the model reading and obeying a paragraph.
`:231` even admits the failure mode: "A `running [resumed]` board label reflects
lifecycle bookkeeping, not confirmation that a new instruction reached the specialist."
That sentence is an apology for a missing refusal. Made it a refusal.

### Where slim's board actually renders (the plan cites the wrong file)

`board-injection.ts` is 1216 lines of **placement** — cache-safe anchoring, byte-
identical replay of frozen boards, tail-zone stripping. The four rendered fields live
in `src/utils/background-job-board.ts`: `formatForPromptWithMetadata` (:657-690) builds
the two sections, `formatJob` (:838-861) and `formatReusableJob` (:755-769) render
`alias / taskID / agent / state`. Worth knowing which file to open.

### slim's alias prefix table encodes nothing the agent name does not

`AGENT_PREFIX` (`background-job-board.ts:119-127`) maps seven agents to three-letter
prefixes, and **every entry is that agent's own first three characters** —
`council: 'cou'`, `explorer: 'exp'`, `fixer: 'fix'`. The fallback right below it is
`agent.slice(0, 3)`. The table is therefore a hand-maintained restatement of the
fallback, and a roster change needs an edit for no gain. Taking the prefix from the
name and asserting the roster's prefixes stay pairwise distinct gets the same property
with nothing to maintain — and the assertion fires the moment two agents would collide,
which is the only time an explicit table would have been needed.

### Two events, not one: finishing and being read

A lane that has completed but whose answer the parent has not read yet must NOT be
addressable — a re-dispatch would overwrite an answer already waiting. So `settle` and
`reconcile` are separate operations and only the second makes a lane reusable. slim has
this too (`terminalUnreconciled`), but folds unread **failures** into the same Active
section (`:662-664`), which renders a lane that already failed as "still working". The
distinction that actually matters is whether a re-dispatch destroys an answer, and only
a completed-but-unread lane has one to destroy.

### `Option<usize>` beats `usize` for a message count across a process boundary

The board is in-process; the message store is not. A lane can outlive the session it
names (compaction, delete, a store restored from a copy), and appending to a session
that no longer exists creates a one-message conversation and calls it a continuation.
`message_count -> Option<usize>` makes "absent" distinguishable from "empty", so that
case becomes a refusal that also drops the stale lane from the board. A `usize` return
would have reported `0` and the append would have silently succeeded.

## [2026-08-07] Task 59: an optional WebAssembly component hook tier

**The empty component linker is the capability boundary.** The host inspects component
imports before instantiation and rejects every import, including WASI filesystem and
socket interfaces. That is stronger and easier to audit than constructing a broad WASI
context and trying to subtract authority later. A future grant must become an explicit
field on `WasmPluginSpec` and one deliberately linked interface.

**Fuel, epoch interruption, and store limits cover different failure classes.** Fuel
halts instruction-heavy loops deterministically; an epoch deadline bounds wall-clock
execution; `StoreLimits` caps linear-memory growth and object counts. All three budgets
are reset per invocation. A failure disables only that resident component, records a
typed `PluginDiagnostic`, and returns success to the shared sequential `HookBus`, so a
bad guest cannot abort sibling hooks or the turn.

**JSON strings make the component ABI stable while Rust hook payloads evolve.** The WIT
world exports one function for every `HookName::ALL` entry. Inputs and replacement
outputs cross as JSON strings; the currently observable mutation codec is
`experimental-chat-system-transform`. The one-to-one export table and a test over
`HookName::ALL` make a newly added authoritative hook fail until the WIT is updated.

**Optional means absent from the normal dependency graph, not merely unused in code.**
`wasmtime = 47.0.3` is exact-pinned, has default features disabled, and is reachable only
through `oc-plugin/wasm`. An offline `cargo tree --no-default-features` integration test
mechanically proves that the default `oc-plugin` graph contains no `wasmtime` package.
Mutation-removing `optional = true` made that test fail with the complete leaked graph.

## [2026-08-07] Task 61: Zod schemas stay with the resident JavaScript host

The real fixture uses Zod v4 from
`/config/workspace/ProdDir/AI/opencode/packages/opencode/node_modules/zod`. The test
symlinks that package into an isolated fixture's `node_modules`, so the TypeScript
module imports exactly `zod` as a user tool does. Absence of Bun/Node, Zod, or Unix
symlink support produces an explicit `SKIP` line instead of a vacuous green test.

The two conversion paths differ materially. When every `args` member is a Zod type,
the shim builds `z.object(args)`, calls Zod's own `toJSONSchema`, and retains that same
object for `safeParse` before every execution. Optionality and enum semantics therefore
come from Zod itself. A non-Zod shape instead filters JSON-Schema-like object/boolean
members into `properties` and marks the retained keys required; no Zod semantics are
invented in Rust. Rust receives only the finished JSON Schema and a stable tool index.

Config modules are imported concurrently but their collected results retain config
directory, `tool` before `tools`, sorted filename, and export order. A failed import is
one diagnostic attached to its source path and does not hide healthy sibling modules.
Execution uses the existing resident-host timeout, memory ceiling, restart policy, and
terminal/permission bridge. After a restart, the callable handle is re-resolved from
the stable tool index rather than reusing a stale JavaScript handle id.

## [2026-08-07] Task 80: cross-project session listing, and how to point the real binary at a fixture DB

### The real binary opens whatever `OPENCODE_DB` names — measured, not assumed

`packages/core/src/database/database.ts:44-47`:

```ts
if (Flag.OPENCODE_DB) {
  if (Flag.OPENCODE_DB === ":memory:" || isAbsolute(Flag.OPENCODE_DB)) return Flag.OPENCODE_DB
  return join(Global.Path.data, Flag.OPENCODE_DB)
}
```

So an **absolute** path is used verbatim. Verified end to end: seeded three
projects and five sessions into a file with `sqlite3`, started
`opencode serve --port <n> --hostname 127.0.0.1` with `OPENCODE_DB=<that file>`,
and `GET /experimental/session` returned exactly those rows with their project
summaries. `oc_paths::db_path` honours the same variable, so **both binaries can
be pointed at one file** and the differential is a real set equality rather than
two independently seeded stores. This is the missing piece task 52's notepad
recorded as "no live differential was run".

Three operational gotchas, each of which cost a cycle:

1. **`--port 0` does not give you an ephemeral port.** The oracle prints
   `opencode server listening on http://127.0.0.1:4096` — it silently falls back
   to its default. Reserve a port with `TcpListener::bind("127.0.0.1:0")`, read
   `local_addr().port()`, drop the listener, and pass that number.
2. **This machine exports `http_proxy=http://127.0.0.1:1080`**, and a loopback
   request through it returns `HTTP 000`. `curl --noproxy '*'` works; from Rust,
   a raw `TcpStream` sidesteps it entirely.
3. **The oracle's server ignores `Connection: close`.** `read_to_end` blocks
   until the read timeout (`WouldBlock` after 20s). Read the headers, take
   `Content-Length`, then `read_exact` that many bytes.

`reqwest::blocking` is **not** available in this workspace — the workspace table
enables `form`/`json`/`stream`/`rustls`/`charset`/`http2` and no `blocking`.
Adding it would edit a manifest five sibling crates share, so the differential
speaks HTTP/1.1 on a raw socket (~70 lines including a chunked branch).

### The one byte-level divergence in the whole payload: `2.0` vs `2`

With the shapes otherwise identical, `serde_json::to_string` and
`JSON.stringify` disagreed on exactly one thing: an integral `f64`. JavaScript
has one numeric type, so `cost: 2` (SQLite `real`) serialises as `2`; Rust emits
`2.0`. Both parse to the same number, and `serde_json::Value` equality says
they are equal — so a *semantic* comparison passes while the bytes differ. Any
client that hashes, caches or diffs the document sees two different documents.

Fixed with a `serialize_with` that emits an integer when `fract() == 0.0` and the
value fits `i64`. `1e300` is integral but does not fit, and falls through to the
float path, which renders `1e+300` — which is also what `JSON.stringify` writes.
NaN and infinity have a NaN `fract()` so they never reach the cast.

**The differential asserts both** `Value` equality and `to_string` equality. The
byte check is the one that found this.

### The composed listing query, and its cost

One statement: the row filter as a subquery (so `LIMIT` applies to *sessions*,
before any join), then `LEFT JOIN project`, then a correlated
`(SELECT COUNT(*) FROM message WHERE message.session_id = listed.id)`.

- **`LEFT`, not inner.** A session whose `project` row is gone must still list
  with `project: null` — upstream preserves it too, via `?? null`
  (`session.ts:595`), because its version is two statements and a map lookup.
- **Correlated, not `GROUP BY`.** A joined grouped aggregate scans and groups the
  whole `message` table — the largest table in the database — even to list ten
  sessions. The correlated form runs one index probe per **returned row** against
  the existing `message_session_time_created_id_idx` (`schema.rs:208`), so cost is
  bounded by the page size, not by the message count. No new index was needed and
  none was added; the schema is locked by todo 20's byte-compat snapshot.
- **Cost needs no aggregate at all** — `session.cost` is maintained on the row.

Upstream's `listGlobal` is instead two statements plus an `IN (…)` lookup
(`session.ts:578-595`). One statement is fewer round trips and identical output;
the differential proves the output is identical.

### `--archived` widens. It is not a filter.

`session.ts:564` is `if (!input?.archived) conditions.push(isNull(time_archived))`.
The flag *removes* a predicate. `oc-db`'s `ArchivedFilter` is three-way
(`Any`/`Active`/`Archived`) because todo 21 needed the exclusive form elsewhere,
so `GlobalListRequest::archived` is deliberately a **`bool`** that maps only to
`Any`/`Active` — exposing the three-way enum at the CLI boundary is an invitation
to wire `--archived` to `Archived` and silently redefine it.

### Where the `session list` scope actually came from

`Session.list` reads `InstanceState.context` and pushes `projectID: ctx.project.id`
into every query (`session.ts:548-555`); `listByProject` then makes it the first,
unconditional predicate (`:964`). No input turns it off. That is the whole reason
the CLI could never list across projects while `/experimental/session` always
could. `ProjectScope` is a two-arm enum with **no `Default`**, so the ambient
project is now a *resolved default in the CLI layer* that a flag overrides, not a
hidden predicate in the store.

## [2026-08-07] Task 62: three-tier integration without private process handles

- A single heterogeneous `HookBus` is the only useful ordering oracle. The test
  uses noncommutative mutations (`[x]`, then `!`, then duplication) so tier
  grouping, sorting, or reversed iteration cannot accidentally pass.
- Public loaders do not expose child handles or PIDs. Exact lifecycle testing can
  still stay outside implementation code: a `/bin/sh` `exec` wrapper records the
  JSON-RPC PID without introducing an intermediate process, while the JS fixture
  records `process.pid` during initialization. Shutdown then polls only those
  recorded PIDs with a deadline.
- Failure isolation must positively observe the surviving tiers, not merely assert
  one diagnostic. Killing JSON-RPC yields `[x|x]`; fuel-halting WASM yields `[x!]`;
  removing JS yields `x!|x!`.
- Feature-off coverage remains visible by retaining the six named test targets and
  printing an explicit skip reason. Runtime absence in the dedicated JS degradation
  case is an assertion path, not a suite skip.
- Mutation proofs confirmed both load-bearing assertions: reversed bus iteration
  changed `[x]!|[x]!` to `[x|x!]`, and suppressing WASM dispose failed with
  `dispose did not reach surviving wasm tier`.

## [2026-08-07] Task 81: retention selection must close whole subtrees

- Age eligibility applies to candidate roots, then selection expands through every transitive `parent_id` descendant; an old child never pulls in its newer parent. A visited set is mandatory because `parent_id` has no FK and cycles are schema-valid.
- Protection is evaluated over the entire candidate subtree. A shared, compacting, active, or no-server-recent descendant vetoes its ancestor candidate; otherwise retaining that descendant while selecting the ancestor would violate closure.
- Liveness is an injected probe result, not a database inference. Reachable `/api/session/active` responses protect reported IDs; only `Unreachable` activates the default one-hour `time_updated` fallback.
- Mutation proof was non-vacuous: replacing descendant traversal with the root alone shrank to a two-node tree (`[(0,false),(0,false)]`) and selected 1 of 2 rows.

## [2026-08-07] Task 76: the view layer's capability inventory, and how incremental streaming is asserted off-screen

**The 204-file reference is a capability inventory, and treating it as one made the
todo finishable.** All ten plan items landed except a process-spawning
`$EDITOR`/clipboard implementation, in 10,633 lines across 12 modules — against
31,729 upstream lines. What was ported *exactly* is the short list of things that are
contracts rather than chrome, each cited in its module header:

- the three permission replies (`oc_permission::ReplyKind`, not invented here);
- the `diff_style` fork, `permission.tsx:38-42` — `stacked` always wins, `auto`
  splits only at `width > 120` (**exclusive**; 120 columns is unified);
- the scroll precedence, `util/scroll.ts:18-27` — acceleration beats `scroll_speed`,
  and the default is a **constant 3** lines per notch, not 1;
- `MacOSScrollAccel`'s curve `1 + 0.8·(e^(v/3) − 1)` capped at 6, with its two
  guards: a >150 ms gap resets the streak, and a <6 ms gap returns 1 *without
  recording* (some terminals emit several events per physical notch, and recording
  them accelerates one notch straight to the cap);
- the nine per-tool icons and their "Writing command…"-style placeholders;
- `normalizePromptContent` (`editor.ts:12-24`) — a **single**-line paste loses its
  trailing newline, a multi-line paste keeps it;
- the clipboard ladder `copy_command` (`clipboard.ts:75-91`), Wayland before X11,
  `xclip` before `xsel`.

**Incremental streaming is asserted as prefix growth between frames, not as a final
state.** Draw, feed one `TextDelta`, draw again — then assert (a) each delta returned
`redraw: true`, (b) frame N contains its own delta and *not* the next one, and (c)
every frame's whitespace-stripped text is a **prefix** of the following frame's. (c)
is the part that distinguishes "rendered incrementally" from "re-rendered from
scratch each time and happens to end up right". It only works because the transcript
is a *fold* over `TurnEvent` with rendering as a pure function of the fold — an
implementation that rendered from a provider stream directly could not be stepped.

**`StreamEvent::RetryRollback` is a rendering correctness requirement, not an
optimisation.** Its doc comment says consumers must discard the interrupted attempt.
A transcript that appends instead shows the model's answer twice. One test.

**A dialog that renders full-height hides the thing it is asking about.**
`Dialog::desired_height` first took only `available`, so every dialog covered the
transcript and the "renders over a live base" test failed. It now takes
`(content_rows, available)` — the host passes the row count the dialog's own
`lines()` call produced, because `lines()` takes `&mut self` and a size query must
not be able to mutate. The permission prompt then caps itself at 15
(`permission.tsx:626`), which is what keeps the prompt *decidable*: the user can see
the command scrolling past behind it.

**Four directory-scanning guards, each with a floor, and three of them mutation-proven.**
`views_tests.rs` scans the 12 view sources for: a literal colour (20 spellings incl.
`Rgba::opaque`), a raw key (15 `KeyCode`/`KeyModifiers` spellings), and an action name
absent from the shipped table. Floors: ≥12 files, ≥40 action names checked. The
action-name scan had to be restricted to match arms **inside `fn handle_action`
bodies** (brace-depth tracked) because a tool name and an action name are both
snake_case strings in a match arm — a whole-file scan reported `"bash" =>` from the
tool-icon table as an unknown action. A fifth test is the complement of the colour
scan: every painting module must actually *read* `ViewContext`, because a view that
paints nothing also has no literals.

**`help_show` ships unbound.** `keybind.ts` gives it `keys: "none"`, so it is
reachable only through the command palette. A help test asserting "the keys the
keymap resolved" against it failed; it had to be retargeted to `session_interrupt`,
and the unbound-by-default fact is now pinned by its own test. Two other actions in
that family are worth knowing about for the same reason.

**`space` is a scarce key inside a dialog.** The binding table has exactly one
`space` row — `dialog.mcp.toggle` — so a multi-select question has to claim it;
adding a second `space` row would be a conflict `Keymap::from_config` rejects at
construction. The question prompt matches that action *and* accepts the raw `' '` as
a fallback for a user who rebound it.

## [2026-08-07] Task 82: preview-first pruning and exact loss accounting

- The live schema has ten session-attributable prune tables, not the plan’s stale count of twelve: `session_context_epoch`, `session_input`, `session_message`, `todo`, `part`, `message`, `session_share`, `session`, `event_sequence`, and `event`.
- Pruning consumes todo 81’s descendant-closed `RetentionReport.selected` ids directly. Sorting and deduplicating those ids is safe; walking `parent_id` again would create a second, divergent selector.
- Preview bytes are logical payload bytes (the sum of non-null column lengths), not SQLite page reclamation. This makes preview deterministic and attributable to the selected rows; page-level bytes cannot be assigned reliably inside shared B-trees.
- `part.session_id` and durable event aggregate ids are not protected by session foreign keys. Exact deletion therefore needs both a final global `part` orphan sweep and explicit cleanup of raw plus `sse:<session_id>` event aggregates.
- Four mutation proofs were non-vacuous: changing the default to delete, bypassing confirmation, bypassing remote-unshare refusal, and omitting the orphan sweep each failed its dedicated test.

## [2026-08-07] Task 83: conservative filesystem reclamation

- Snapshot attribution must use `project.worktree`, not `session.directory`: the latter can be a subdirectory and hashes to a different store. A LEFT JOIN makes missing project metadata visible so ambiguity can retain rather than delete.
- `oc_tool::store::session_of` intentionally splits from the right. A Rust UUIDv7 name is attributable even when a session id contains underscores; an upstream `tool_<ascending-id>` has no separator pair and returns `None`.
- Holding SQLite’s `IMMEDIATE` transaction prevents a concurrent database writer from creating a new survivor after the reference set is read. It cannot prevent unrelated filesystem writers, so every removal rechecks the candidate’s file/directory type and refuses a changed shape.
- Safe byte reporting recursively uses `symlink_metadata` and counts only regular-file lengths. Snapshot Git alternates and any other symlink are never followed, so reported reclaimed bytes remain store-local.
- Three mutation proofs were non-vacuous: removing `store.is_referenced()` deleted the live snapshot store; attributing every `None` tool name deleted a fresh upstream file; enabling legacy cleanup by default deleted its fixture before opt-in.

## [2026-08-07] Task 84: explicit VACUUM, and why a size measurement in WAL mode lies

**The live database has 20 tables, not 19 and not 12.** `schema::TABLE_COUNT` is 19 —
the tables `schema::up` creates — and `migration::apply` adds a 20th, `migration`, for
its own bookkeeping. `db stats` reads the inventory from `sqlite_master` at runtime
rather than from any constant, and `tests/vacuum.rs` pins all 20 names in sort order.
Todo 82's `PRUNE_TABLES = 10` is a different and also correct number: the
session-attributable subset. Three numbers, all right, none interchangeable — anyone
writing a "how many tables" assertion has to say *which* count they mean.

**Measuring a database's size in WAL mode requires a truncating checkpoint on both
sides, or the number is wrong in both directions.** Every connection is opened in WAL
mode (`open.rs`'s `PRAGMA_SEQUENCE`, a port of `database.ts:22-33`), so a delete's
pages land in `opencode.db-wal` and the main file does not change at all. Measure
naively and a prune looks like it reclaimed nothing *for the wrong reason*, while the
footprint including the sidecar has **grown**. `vacuum()` therefore issues
`PRAGMA wal_checkpoint(TRUNCATE)` before it measures and again after the rewrite.
`TRUNCATE`, not the `PASSIVE` form the open sequence uses: passive leaves `-wal` at its
high-water mark, so bytes that hold nothing get counted. Two tests pin this — one
asserts `main_bytes == page_size * page_count` right after a truncating checkpoint, the
other asserts an uncheckpointed prune shows up as `wal_bytes > 0` with `main_bytes`
unchanged.

Sizes are re-`stat`ed from the filesystem on every call and never cached; the whole
point of the before/after pair is that they were observed at two different times.
`DatabaseSize` counts `-wal` and `-shm` alongside the main file because they are part of
the database, not a cache.

**The measured effect, on a 2.0 MiB fixture with half its sessions pruned:**
main file 2,125,824 → 2,125,824 bytes (prune reclaimed **0**, with 228 pages now on the
freelist *inside* the file) → 1,179,648 bytes after `VACUUM` (reclaimed **946,176**).
The same sequence through the real CLI: 1,531,904 bytes unchanged by the delete with
158 freelist pages, then 651,264 bytes reclaimed. `PRAGMA auto_vacuum` is asserted to
be 0 (NONE) first, because with it on SQLite would return pages on commit and the whole
distinction would evaporate.

**Free space costs zero new packages.** `rustix` 1.1.4 with feature `fs` was already in
`Cargo.lock` — `tempfile` pulls exactly that — so `[target.'cfg(unix)'.dependencies]
rustix` added one manifest edge and **no package**. `f_bavail * (f_frsize or f_bsize)`,
with `f_bsize` as the documented fallback for filesystems that report `f_frsize` as 0.
The syscall's `unsafe` stays inside the dependency, so `oc-db` keeps
`unsafe_code = "forbid"`; a hand-rolled `libc::statvfs` could not. And it needs no
subprocess, which matters concretely: `df -Pk` parsing would fail under the stripped
`PATH` that `oc-cli`'s differential suite runs `db` against. `statvfs` needs a path that
exists, so a database that has not been created yet is answered from its parent
directory.

**A CLI subcommand can be added without touching a frozen `--help` surface** by
dispatching on the value of the existing positional, which is how `db path` already
worked. `differential.rs` compares `db --help`'s long options against the real binary
exactly unless an addition is declared in `ADDED_LONG_FLAGS`; `db stats`,
`db integrity-check` and `db vacuum` add **zero** flags, so nothing had to be declared
and the comparison stayed green. Verified by hand: `db --help` prints the same six long
options as before.

## [2026-08-07] Task 85: one prune service, two adapters, and real liveness

- CLI and HTTP parity is strongest when both serialize the exact same report with the same function; a byte-equality test catches adapter-specific defaults and response reshaping that semantic assertions miss.
- Ephemeral loopback ports require explicit process discovery. A lifetime-scoped URL record is only a candidate: the CLI must connect, validate `/api/session/active`, and aggregate every successful response before it can claim `Liveness::Reachable`.
- Stale discovery files are safe when connection success is the evidence boundary. A reachable response with an empty map means “this process reports no active sessions”; zero successful responses means `Unreachable` and activates the one-hour recency guard.
- Previewing artifact reclamation requires evaluating survivors as if selected sessions were gone. Filtering only after actual deletion makes a dry run under-report reclaimable snapshot/tool-output artifacts.

## [2026-08-07] Task 86: what the compatibility suite genuinely compares, and what it does not

`cargo test --test compat_suite` is now the single gate. 8 tests, all green with the
real 1.18.12 binary present, and it writes `target/compat/compat-report.json`
(`OC_COMPAT_REPORT` overrides). The report is the deliverable: a green suite answers
"did anything I checked disagree", not "what did you check".

### The suite AGGREGATES; it does not re-run

It does not spawn nested `cargo test`. Doubling workspace runtime to re-derive results
the caller already has would also make its failure mode "some other target failed",
which is the diagnostic that made assembling this necessary. Instead:

- It **re-asserts the two load-bearing DB contracts itself**, because the plan's QA
  scenario requires renaming an index to fail *this* command. Re-expressed with
  name-keyed `BTreeMap`s rather than `oc-db`'s flat-vector `assert_eq!`, so the failure
  reads `index session_project_idx exists in the database the real binary created but
  NOT in the Rust database` instead of dumping two whole schemas.
- Everything else is a **surface registry row** naming `crates/…/tests/x.rs::test_name`.
  `every_registered_evidence_test_still_exists` reads each file and greps for
  `fn <name>(`, with a floor of 15 resolved so it cannot pass vacuously. A renamed or
  deleted differential fails here rather than silently shrinking the claim. This caught
  five of my own guessed names on the first run (agents, skills, models, paths,
  compat_v1) — the registry earned its keep before it was even committed.

### 19 surfaces compared, 3 deliberately not

Compared (`compared`, 15): db-schema, db-migration-journal, config-merge,
config-permission, agents, skills, commands, tool-registry, cli-commands,
cli-disposition, paths, search, session-rows, message-export, models-catalog.

Compared with a stated exception (`partially_compared`, 4):
- **api-operations** — 56 of 58 upstream operations served; 2 absent (below); 2 added.
- **lsp-diagnostics** — TypeScript only. Task 48's evidence records the oracle returning
  an **empty** diagnostics array for a Rust fixture, so asserting exact equality there
  would assert an oracle defect. Not claimed.
- **v1-compat-surface** — the measured plugin callsites, not the full 67-route v1 surface.
- **execute-parameter-contract** — a divergence, and the only one *verified*: the live
  schemars schema is compared against what the TOML declares.

NOT compared, each saying so in the report in words a skimmer cannot miss:
- **provider-wire-protocol** — `oc-testkit` has no HTTP client *by construction*
  (`Cargo.toml`'s load-bearing absence), and todo 87 owns cassette-replayed parity.
  Provider coverage is a *declared divergence*, not a measured equality. This is the
  single largest thing the suite does not prove.
- **tui-rendering** — never will be; Q1 chose an equivalent ratatui interface.
- **acp-transport** — todo 78 validates against the real SDK on disk, which is a
  live-counterpart check, not an oracle differential.

### The full normalization list — three entries, and why three is the right number

Every mask is a licence to differ and enough of them make any two programs agree, so
the list is short on purpose and each entry carries its reason in the artifact:

1. **db-schema**: SQL whitespace runs, backtick/double-quote identifier quoting, trailing
   semicolons, identifier and type letter case. SQLite re-emits the `CREATE` it was
   given; quoting and spacing are not part of the schema's meaning. Structure — object
   names, column order, notnull, defaults, foreign-key actions — is compared exactly.
2. **db-schema + db-migration-journal**: the temporary directory each side's database
   lives under. The harness chose both; a path it invented cannot be a compatibility
   fact, and nothing inside the database records it.
3. **api-operations**: OpenAPI schema bodies, descriptions and component ordering. The
   comparison is over the path+method SET. Response shapes are compared per group by
   `oc-server/tests/api.rs`; claiming a document-level byte match here would overstate it.

Everything else in the suite is byte-exact or set-exact. `oc-config`'s 14-tree
differential runs `Normalizer::none()`; `oc-cli`'s long-option check is exact equality
against `oracle ∪ ADDED_LONG_FLAGS`, not a superset.

### The API gap is exact, not tolerant

`missing == API_KNOWN_GAPS` and `extra == {the two C8 prune operations}` are both
*equalities*. A third absence, or a third addition, fails — the exemption cannot widen
by accident. The two absences are `GET /api/event` and
`GET /api/session/{sessionID}/event`; an equivalent stream exists at the compat path
`/event` (`oc-server/src/events/route.rs:20`), so the capability is present and only the
upstream paths are not. Recorded as a gap, not a divergence: an omission is not a
decision, and laundering it into `docs/divergences.toml` is precisely what the plan's
"must NOT normalize away a real difference" forbids.

## Task 87 — provider-family cassette matrix

The compatibility surface is a Cartesian product, not a handful of representative
fixtures: five registered provider families × eight scenarios = **40 cells**. The
test derives the required families from `ProviderRegistry::registered()` and checks
that every `(family, scenario)` pair occurs exactly once, so registering a new provider
without assigning it to a cassette family fails instead of silently shrinking coverage.

Evidence provenance is part of every cell. The final matrix contains **5 Recorded**
plain-text cells, **30 Authored** cells for protocol shapes absent from the committed
recordings, and **5 explicit Gap** cells. A green authored fixture proves the Rust
decoder handles that shape; it does not claim a provider emitted those bytes. Every
non-gap cell is replayed through the existing `CassettePlayer` and the production family
decoder, then compared against an exact ordered `Vec<StreamEvent>`.

The five named gaps are all opaque-reasoning artifacts that the committed corpus does
not contain: OpenAI and OpenAI-compatible signed thinking, plus OpenAI-compatible,
Bedrock, and Gemini encrypted reasoning items. Gemini's recorded opaque tool signature
is preserved and asserted separately; it must not be generalized into evidence for an
encrypted reasoning item.

The registry mutation proof is the important anti-vacuity test: adding a synthetic
`new-provider` registration makes matrix validation fail with
`registered provider `new-provider` has no cassette family`. Counting a hard-coded
family enum would not catch that change.

## [2026-08-07] Task 104: what the end-to-end tool turn actually required

The blocker's own list was accurate but **incomplete**. Passing todo 44's
assembled registry into `run`'s dispatcher is necessary and not sufficient: with
the registry wired, `tool_snapshot_locked` reported nine tools and the captured
provider request still carried **no `tools` key at all**. There was a second,
unlisted gap.

`oc_llm::registry::CompletionRequest` had exactly three fields — `model_id`,
`surface`, `messages`. `oc-provider-compatible`'s `RequestBody` had a `tools`
field, `build()` gated it on `Quirks::accepts_tools()`, and `body_for()` never
assigned it. Two layers each looked complete in isolation:

- `oc-engine/src/loop.rs:632-651` asks `available_tools()`, freezes the snapshot,
  emits `ToolSnapshotLocked` with the ids — and then builds a `CompletionRequest`
  that has nowhere to put them.
- `request.rs:119-123` correctly omits `tools` when the field is `None`, and its
  own test `tools_are_omitted_for_a_model_that_cannot_use_them` sets the field by
  hand, so it proved the gate and never the plumbing.

**The generalisable shape: a struct field that no production code path assigns is
invisible to every test that sets it directly.** `oc-provider-compatible` has 30+
request tests and all of them construct `RequestBody` themselves.

The fix adds a provider-neutral `ToolSchema { name, description, parameters }` to
`CompletionRequest` and translates it per family in the provider — OpenAI nests
under `function`, Anthropic and Gemini do not — so `oc-llm` still does not depend
on `oc-tool`.

### The cassette that proved it

`openai-chat/drives-a-tool-loop-end-to-end` (two interactions, real 1.18.x
recording). Its recorded call is `get_weather({"city":"Paris"})`, a tool this
runtime does not have, which turns out to be **more** useful than a matching one:
it proves the request carries the registry and that an unknown call still
produces a tool result the loop sends back, with zero authored bytes.

Executing a *real* tool needs the model to name one this runtime has, and no
recording of that can exist yet. The second test rewrites the recorded stream —
parsing each SSE frame, replacing the tool name and collapsing the five argument
fragments into one, re-serialising — and declares the result
`MockResponse::authored`, so `authored_scenarios()` reports it. Frame sequence,
finish reason and usage frame stay recorded. **A textual splice does not work**:
the arguments arrive as `{\"`, `city`, `\":\"`, `Paris`, `\"}` across five frames
and any string patch leaves malformed JSON that the provider rejects before the
loop is reached.

Two mechanical facts a first attempt gets wrong:

1. **`intent` is a required property on every tool call.** `oc-tool`'s schema
   augmentation injects it, so a hand-written argument object without it fails
   validation with `"intent" is a required property` — before the tool runs.
2. **The test must drive the binary with `tokio::process`, not
   `std::process`.** The mock provider's axum server runs on the test's runtime;
   a synchronous `output()` stops driving it, the response never gets written,
   and the run hangs instead of failing. That cost three debugging rounds.

### The TUI's exit key is shadowed by the editor

`ctrl+c` appears twice in the shipped 184-binding table: `input_clear` in scope
`input` (`keybind.rs:1800`) and `app_exit` in scope `app` (`keybind.rs:944`).
`ctrl+d` likewise collides with `input_delete` (`keybind.rs:2024`). The scope
chain resolves `input` first, so a screen that watched only for `app_exit` boots
a TUI **nobody can leave** — verified live in tmux before the fix: the alternate
screen stayed up through both keys.

The faithful behaviour is the reference TUI's: the first press clears a typed
prompt, and a press with nothing to clear exits. So the screen treats
`input_clear`/`input_delete` as exit when the buffer is empty.

The near-miss worth recording: my first scope test asserted
`matches!(resolve(...), Resolution::Action { .. })` and **passed** while
resolving the wrong action. Asserting that *something* resolved says nothing.
The test now asserts the action name and records the shadowing as the reason the
compensation exists.

Also: a printable key resolves to no action, so the dispatcher forwards it to the
component tree and **the host is what routes it into the editor**
(`InputEditor::insert_char`'s doc says so). Without that the prompt renders and
cannot be typed into — invisible to every view test, because each drives the
editor through `handle_action` directly.

## [2026-08-07] Task 91: the cross-platform release pipeline, and what running it taught me

### The two musl legs are not a claim — I built them, offline, with zig only

Both `cargo zigbuild --release --target {x86_64,aarch64}-unknown-linux-musl
--offline` completed **in ~64s each** on this host and produced statically linked,
stripped ELF binaries (26.7 MB x86_64, 23.7 MB aarch64). `ldd` on the x86_64 one:
`not a dynamic executable`.

The decisive part is the aarch64 leg. This host has **no aarch64 C compiler of any
kind** — no `aarch64-linux-gnu-gcc`, no `aarch64-linux-musl-gcc`, no qemu — yet a
static aarch64 binary linked, including the bundled SQLite C amalgamation and
`aws-lc-sys`'s C. Only Zig can have done that. Docker was present and running and
was never invoked. So the corrected constraint ("no *per-target* C
cross-toolchain") is not merely satisfiable, it is **measured**.

Gotcha for anyone reproducing it: `zig` behind a mise shim fails with
`Error: Failed to find zig / empty string, expected a semver version`, because
cargo-zigbuild probes `zig version` and the shim errors when no version is
selected. Symlink the real binary onto PATH:
`ln -s ~/.local/share/mise/installs/zig/0.13.0/zig /tmp/bin/zig`.

### The x86_64 musl artifact was fully smoke-tested here; the aarch64 one cannot be

`./target/release/oc-smoke --binary target/x86_64-unknown-linux-musl/release/opencode-rust`
→ PASS on all three checks. A static musl binary runs on a glibc host of the same
architecture, which is why the release workflow smokes that leg in place.

`./target/aarch64-unknown-linux-musl/release/opencode-rust --version`
→ `exec format error`. That is the honest limit, and the answer is **an arm64
runner, not an emulator**: the workflow hands that one archive to
`ubuntu-24.04-arm`. Exactly one of the six artifacts is cross-compiled at all.

### `macos-latest` is Apple Silicon now — copying codegraph's matrix verbatim
### would have shipped an unexecutable artifact

codegraph-rust's proven matrix puts **both** darwin targets on `macos-latest`.
That was fine when its goal was "produce a binary"; it is wrong when the goal is
"execute the binary you produced", because an `x86_64-apple-darwin` build on an
arm64 runner is cross-compiled and cannot run there. This pipeline uses
`macos-15-intel` for the x86_64 leg. **The proven pipeline was proven for a
weaker property than the one this todo demands.**

### `openssl-probe` is the trap, and the plan's issues.md warning was right

`cargo tree -p oc-cli` has 394 unique lines. `grep -i openssl` returns exactly
one: `openssl-probe v0.2.1`, which locates the host certificate store, links
nothing, and arrives via `rustls-native-certs <- rustls-platform-verifier <-
reqwest`. Match `"<name> "` with the trailing space, on the four crates that
actually link it (`openssl`, `openssl-sys`, `openssl-src`, `native-tls`). Count
with the correct matcher: **0**.

Corollary that took a moment to get right: **family-prefix matching is correct for
wasmtime and wrong for OpenSSL.** Wasmtime resolves to 32 packages
(`wasmtime-internal-*`, `cranelift-*`, `wasmparser`, `wasm-encoder`) so naming
each would go stale; OpenSSL has a legitimate sibling one character away. Two
matchers, and a self-test for each asymmetry, or someone will "unify" them.

### Flipping reqwest to native-tls cannot even compile here — which IS the argument

The mutation added `openssl v0.10.81` + `openssl-sys v0.9.117` to the lock and
then failed to build: *"The system library `openssl` required by crate
`openssl-sys` was not found … PKG_CONFIG_PATH is not set."* A static musl release
artifact can never have that system library. So "rustls only" is not a taste
preference, it is what makes a single-file static binary possible.

Because the assertion shells out to `cargo tree` rather than building, the
**pre-mutation test binary** reads the mutated manifest and reports correctly.
That is a genuinely useful property: `target/debug/deps/<test>-<hash>` can be run
directly against a mutated tree that no longer compiles.

### `cargo-deny` with no config rejects everything — 392 errors

Stock cargo-deny allows no licence at all: `cargo deny check licenses` on this
workspace produced **392** `error[rejected]`. The allow list in `deny.toml` was
built from a `cargo metadata` licence census (37 distinct expressions across 372
packages) and then **narrowed until cargo-deny reported no unmatched allowance or
exception** — `warning[license-not-encountered]` and
`warning[license-exception-not-encountered]` are how you tell a real allow list
from a hopeful one. Dual-licensed crates resolve on their best option, so
`MIT OR MPL-2.0` (termina) and `CC0-1.0 OR MIT-0 OR Apache-2.0` (dunce) need no
exception; only three crates do, each with a single non-standard licence:
`notify` (CC0-1.0), `webpki-root-certs` (CDLA-Permissive-2.0 — the Mozilla CA
bundle, a *data* licence), `option-ext` (MPL-2.0).

Two other findings: `wildcards = "deny"` fires on all 34 first-party path
dependencies until you add `allow-wildcard-paths = true`; and a `[graph] targets`
restriction makes the audit **weaker** in a way its output never shows — a crate
that only links outside the list stops being judged. Left unrestricted; it passes.

### A positive control is what makes a "no X" assertion mean anything

`the_graph_query_does_detect_the_runtime_when_the_feature_is_on` runs
`cargo tree -p oc-plugin --features wasm` and requires **>= 10** wasm-family
packages (32 today). Without it, both no-wasmtime tests could be green because
the query broke — wrong `-p`, changed `cargo tree` output shape, a matcher that
matches nothing. Same shape as the lesson already in issues.md: *a check that can
only detect one shape of failure is not a check.* Every "X is absent" assertion in
this file carries a paired "the thing that would contain X is present".

## [2026-08-07] Task 105: this port's random ids cannot express "later", and one refactor exposed it

**The bug, in one line**: `MessageStore::messages_for_session` breaks a
`time_created` tie with `ORDER BY … id ASC` — faithful to upstream, whose ids are
*time-ordered identifiers* — but this port's ids are `Uuid::new_v4()`. Two messages
written in the same millisecond therefore order by **coin flip**.

Todo 105 moved the prompt's write to immediately before `run_turn`. `main` had
~40 ms of `tool_runtime::assemble` between the prompt write and the first
`assistant_message()`, which reliably crossed a millisecond boundary; `TurnHost`
correctly moved that work into `open()`, the gap closed, and the flip started.
Half the time the reply sorted **ahead of the prompt it answers**, the request
prefix changed between step 1 and step 2, and todo 31's tracker refused it:

```
append-only cache violation on turn 2: stable history message 1 changed
```

**`main` was not correct — it was lucky, and the luck was load-bearing.** Any
future change that removes work from between two writes in one session can bring
this back. The fix is `oc_db::message::created_after(now, latest)` +
`MessageStore::latest_time_created`, applied at both write sites
(`loop.rs::assistant_message`, `turn.rs::TurnHost::drive`). The real fix would be
time-ordered ids; the clamp is the cheap half and it is now pinned by
`loop_reply_sorts_after_a_prompt_stamped_ahead_of_the_clock`.

**Generalisable**: whenever this port keeps an upstream ordering rule whose
tie-break depends on a property upstream's ids have and ours do not, the rule is
correct and the *data* is wrong. Grep for other `ORDER BY … id` on tables we write
with `prefixed_id`.

## [2026-08-07] Task 105: `script -qefc` gives a real PTY with a 0x0 window size

Measured: `script -qefc "sh -c 'stty size'" /dev/null < /dev/null` prints `0 0`.
`script` copies the winsize from **its own stdin**, and under a test harness or
`Stdio::piped()` that is a pipe with no size. The pty is real, `is_terminal()` is
true, and the TUI paints 28 empty frames. It looks exactly like a render bug.

Fix from inside the launched command, the only place that owns that terminal:

```
script -qefc "stty rows 40 cols 120; <program> …" /dev/null
```

`perf/workload.rs` has the same 0x0 pty and **must keep it**: the TS oracle
baseline was measured through it, so it is apples-to-apples. It only means neither
side pays for rendering.

## [2026-08-07] Task 105: a nondeterministic failure reported as deterministic costs the diagnosis

`.omo/WORKTREE.md` recorded this regression as "the refactor, not a flake". It was
the refactor **and** a flake — 2 failures in 8 runs, ~15-25%. Its stated most-likely
cause (`resolve_session`'s `&RunArgs` → `&TurnPlan`) was wrong, and a line-by-line
diff of that function could never have found the real cause, because the cause was
a change in **timing** with no logic difference. What found it was instrumenting
`hydrate_session` and re-running until it failed.

**Rule**: before asserting a failure is deterministic, run it at least 8 times.
Before proposing a most-likely cause in a prompt, say how confident it is — a
subagent that trusts a wrong localisation spends its budget in the wrong file.

## [2026-08-07] Task 88: accepting the command shape is not accepting the frozen workload

Todo 105 proved the real Rust TUI accepts `<program> --pure --prompt … --model …
--auto` and completes one tool turn with exactly **2 provider requests**. Todo 93's
runner recognizes a turn only after **3**: one TypeScript-only title/compaction prelude
plus the two tool-loop requests. Its formula is `(captured - 1) / 2`, so the successful
Rust turn is deterministically counted as zero.

The same mismatch is sharper for W-real. The frozen driver assumes a restored TS TUI
discards `--prompt` and therefore types the first turn after 90 seconds. The Rust TUI
submits `--prompt` for an existing session immediately, so the driver submits a second
prompt at 90 seconds. A PTY and a working turn seam are necessary but not sufficient;
the provider response plan and completion predicate are part of an executable workload.

General rule: an end-to-end compatibility test must cover the harness's full protocol,
not only its argv. “The binary accepts the same invocation” says nothing about whether
the harness will feed it the same logical operation or recognize completion.

## [2026-08-07] Task 106: the fourth declared-and-never-invoked seam, and why `cargo build` could not see the regression it caused

`oc_agent::builtin::INTERNAL_NAMES = ["compaction", "title", "summary"]` had 21
passing tests and no caller. Todo 63's own doc comment predicted the cost —
*"dropping any of them silently removes auto-compaction, session titles, or
session summaries"* — and declaring them did not supply them either. The measured
consequence was the perf gate: our binary sent **2** provider requests for one
tool turn where upstream sends **3**, so the frozen
`completed_tool_turns(captured) = (captured - 1) / 2` scored our turn as **0** and
timed out. The harness was right; we were missing the feature.

**Now 3, on both surfaces**: `pty_requests=3` for the `--prompt` flag path and for
keystrokes typed into the pty, and 3 captured in the headless `run` test.
`completed_tool_turns(3) = 1`.

### The regression `cargo build --workspace` reported as green

Widening `compaction::run_compaction` from `&Connection` to `&mut Connection` —
necessary because a shared `&Connection` held across the provider stream makes the
whole future non-`Send`, and the TUI spawns its turn driver — left the build clean
while **four test targets failed to compile** (`oc-memory/src/snapshot.rs:587`,
`oc-engine/tests/compaction.rs:371,514,534`). This is the wave-5 hazard again, and
it is now the second time it has cost a fix pass: **`cargo test --workspace` is the
integration gate; a green `cargo build` across a signature change proves nothing.**

### `&Connection` across an await is the thing that makes a future unspawnable

Worth generalising, because it will recur. `rusqlite::Connection` is `Send` but not
`Sync`, so `&Connection: Send` is false and `&mut Connection: Send` is true. Any
async function in this workspace that interleaves DB writes with provider streaming
must take `&mut Connection` or it can never be `tokio::spawn`ed. `run_turn` already
did, by luck rather than intent; `run_compaction` did not, and had never been
called from a spawned task because it had never been called at all.

### The append-only cache violation did not fire, and not by luck

This task adds a request to the **front** of every turn, which is exactly the shape
that produced `append-only cache violation on turn 2: stable history message 1
changed` (`oc-llm/src/cache.rs:153`) in todo 105's first attempt. It is structurally
safe here:

- `run_turn` creates its `PromptCache` **inside** the call (`loop.rs:561`), so the
  tracker's first observation is already the post-prelude, post-compaction prefix.
  There is no earlier prefix to differ from and nothing to reset.
- The prelude's own requests never reach that tracker: `collect_text` builds bare
  `CompletionRequest`s with `tools: []`, and compaction owns a `CacheTracker` and
  `LockedTools` created and dropped inside `compact_if_overflowing`.
- Compaction **appends** (marker, summary message, summary text) and honouring it is
  dropping a *prefix* of stored history. Nothing is mutated in place, so what the
  tracker sees stays append-only within the turn.

The general rule: **the safe place to change a request prefix is before the cache
tracker for that request exists.** Anything that changes it between step *n* and
step *n+1* is the violation, whatever wrote it.

### Compaction is honoured by reading, not by rewriting

`loop::retained_history` was the design decision that made this small. A compaction
attempt persists a marker naming `tail_start_id`, and all three of its writes sort
*after* the tail because they are stamped when the attempt runs. So honouring a
compaction is `&history[tail_index..]` — the marker projects to nothing (`compaction`
is not a request-bearing `PartKind`), the summary projects to the assistant text
message, and the request that comes out is byte-identical to what
`run_compaction` returned in its `messages` field, without either being
reconstructed. No second projection to drift.

**A failed attempt is deliberately ignored.** It persists the marker *and* an errored
summary carrying no text; honouring that marker would drop the history and substitute
nothing — a conversation starting mid-thought, indistinguishable from a working
compaction from outside. So a marker takes effect only once its paired summary has
text and no error, and a dangling `tail_start_id` leaves history intact. Same family
as wave-17's 4.19 GB prune: **when a reduction's replacement might be missing, retain.**

### "Not needed" and "could not" must not share a variant

The first version reported `CompactionOutcome::NotNeeded` as a skip, and the PTY
transcript then read *"compaction: the session has no finished assistant message, so
nothing has been measured"* on the very first turn of every session. That is the
wave-18 lesson in a new place: a line on every ordinary turn is a line the user
learns to ignore, and then a real loss goes unread. `compact_if_overflowing` now
returns `Ok(false)` for a history that fits and `Err(Reason)` only when the history
overflowed and the attempt could not be made. Pinned by
`an_ordinary_turn_reports_nothing_at_all`.

### The context window was already in the catalog

`oc_llm::catalog::resolved::ModelLimit { context, input, output }` — models.dev's
`limit.*`, carried through `merge.rs:486-490` so a config override wins. Upstream
reads the same two fields for the same decision (`session/overflow.ts:10-19`), so no
constant was invented. They are `f64` because models.dev publishes JSON numbers;
`token_count` maps non-finite/negative/zero to 0, and 0 already means "no threshold
compaction" to `CompactionPolicy`. Known gap: I did **not** use `limit.input`,
because `TokenWindow` has no field for it and adding one changes todo 35's tested
type — a model declaring a smaller input ceiling compacts slightly later than
upstream would.

## [2026-08-07] Task 88: a fresh-schema differential does not prove an old database can be opened

The schema suite proves two current databases have identical objects and that a
Rust-created 38-entry journal round-trips through TS. W-real supplied the missing
direction: an April user database with a `session` table and **no `migration` table**.
TS 1.18.12 migrated its writable clone and ran; Rust failed before raw mode with
`migration to schema version 38 failed`.

`migration::apply` has only two success shapes: empty DB → create current schema, or
existing `session` DB → verify all current journal ids. A real legacy database is
neither. Compatibility must test old→current as well as current↔current; otherwise the
first production action on a user's actual history can fail while every schema diff is
green.

The public perf API was more composable than its name suggested. Two sequential
`measure_typescript_baseline` calls can route their ten G1/G2 launches through an
immediate subject dispatcher: assign each call five positions of
`interleaved_pair_order(5)`, duplicate each position for W-idle/W-real, then split the
public reports. This yields five chronological AB/BA pairs without touching private
samplers or overlapping process trees. The product, not the API, is now the blocker.

## [2026-08-07] Task 107: a legacy database is a third state, not a variant of two

**The defect, in one line.** `migration::apply` had two paths — empty →
`create_current`, has `session` → `verify_journal` — and `verify_journal`'s first
statement was `SELECT id FROM migration`. A real install predating that table takes
the second path and has nothing to read, so the binary died on the user's own
history. Measured: `~/.local/share/opencode/opencode.db.bak.20260408`, 2.6 GB,
14 tables, `session` with **2,345 sessions** and 92,378 messages, **no `migration`
table**, `__drizzle_migrations` with 10 rows. The released TypeScript binary opens
it; ours printed `migration to schema version 38 failed`. Confirmed by reverting
`migration/` with `git stash` and running the CLI against a copy.

**How I built the fixture, and why not by trimming.** Copying 2.6 GB per test is
not viable, and trimming a real database risks importing whatever incidental state
it happens to hold. Instead the fixture is *reconstructed*: `sqlite3 -readonly
… .schema` output from the real backup, committed verbatim as
`crates/oc-db/tests/fixtures/legacy_pre_migration.sql`, plus synthetic rows and the
real backup's ten `__drizzle_migrations` names in the order that table holds them.
It is embedded with `include_str!` rather than read at run time, specifically to
dodge this file's stale-`CARGO_MANIFEST_DIR` hazard — a baked-in `oc-wt/tNN` path
breaks once the worktree is removed, and embedded bytes have no such failure mode.
The real backup is still exercised once, opt-in via `OPENCODE_LEGACY_DB`, and that
run is the decisive artifact: **sessions 2345 → 2345, messages 92378 → 92378,
journal 0 → 38 ids, 10 seeded, 28 executed, 34.9 s.**

**The 10 Drizzle names are NOT `MIGRATION_IDS[..10]`.** Rows 6 and 7 hold
`20260303231226_add_workspace_fields` *before* `20260228203230_blue_harpoon` — the
reverse of the generated order. So seeding must copy what the old journal says, and
a test asserting `seeded == MIGRATION_IDS[..10]` would be asserting something false
about the user's real data. The test asserts against the recorded names instead, and
separately that they equal what the fixture's `__drizzle_migrations` contains.

**Validating a ported migration chain without trusting inspection.** Before writing
any Rust I transcribed all 38 upstream migrations into two `.sql` files and diffed
the result against the user's **installed** `opencode.db`, which the TypeScript
binary migrated itself. Both directions came out identical — objects and full column
metadata (type, nullability, default, pk position) — for `legacy .schema + 11..38`
and for `1..38 from empty`. That is a stronger check than reading the TypeScript,
and it caught the two places where a *migrated* database legitimately differs from a
*freshly created* one: `account_state.id` is `NOT NULL` in the chain and not in
`schema.gen.ts`, and `workspace.time_used` carries `DEFAULT 0` in the chain because
SQLite refuses to add a `NOT NULL` column without one. Both differences are present
in the real migrated database, so reproducing them is fidelity, not drift.

**Deriving the id list from the SQL that runs.** `MIGRATION_IDS` is now a `const`
computed in a `while` loop over `steps::MIGRATIONS`, not a second hand-written copy.
A journal naming a migration nobody executed is worse than no journal, and two lists
maintained by hand will diverge. The old hand-written array and the derived one agree
on all 38, which is how I know the port is complete.

## [2026-08-07] Task 88: a configured model and a catalog-backed model are different startup paths

After todo 107, the real legacy database migrated and the TUI remained alive through
the 90-second hydration boundary. The next frozen run exposed an earlier W-idle seam:
`ScriptedEnv` disables models.dev fetches and provides a complete provider/model in
`OPENCODE_CONFIG_CONTENT`, which TypeScript accepts, but Rust still requires either a
cached global catalog or `OPENCODE_MODELS_PATH` and exits before the turn.

The existing TUI and headless turn tests both add `OPENCODE_MODELS_PATH`, so they prove
the seam only in a richer environment than the performance harness uses. A feature's
end-to-end test must include the poorest supported closed-world environment, especially
when the test claims to mirror another harness. Otherwise an extra fixture can conceal
a mandatory startup dependency that the real entry point does not have.

## [2026-08-07] Task 108: an error is not a fallback, and a fixture that supplies
## what the product should not need hides the gap forever

**The defect.** `OPENCODE_DISABLE_MODELS_FETCH=1` with an empty cache returned
`CatalogError::FetchDisabled` **even when the config fully specified the provider
and the model**. An air-gapped user with a private gateway could not start the
binary. Measured against both binaries under one `env -i` environment, empty
`XDG_CACHE_HOME`, no `OPENCODE_MODELS_PATH`: ours exit 1, no output; released
1.18.12 exit 0, eight models, including the config-only `test/test-model`.

**Upstream's chain is three fallbacks and the last one is a success, not an
error** — `models-dev.ts:196-223`: on-disk cache, then a compile-time bundled
snapshot, then `return {}`. Config providers are merged *over* that result
(`provider.ts:1425-1520`), which is why a self-contained config always works
there. Our port had rung 1 and turned rung 3 into an error.

**The reasoning that was right and misapplied.** `catalog/error.rs` argued
fail-fast because the alternatives — hanging on a forbidden fetch, or an empty
catalog the user meets as "no models found" three screens later — are both worse.
That is correct **for a model nobody defines** and wrong when the config already
defines it, because then there is nothing to look up. The fix split the two cases
rather than deleting the error: `load()` succeeds with an empty document carrying
a `CatalogProvenance`, and `FetchDisabled` is raised only after the **resolved**
catalog (config merged in) fails to contain a *requested* model. It still names
the model, the flag, the source, the cache path and the `provider` block.

**The generalisable shape**: the same empty value meant two opposite things, and
the caller could not tell them apart. Failing on both breaks the legitimate case;
failing on neither hides the illegitimate one. The answer is to carry the *reason*
alongside the value — here a four-variant `CatalogProvenance` — so each case gets
its own answer. Same family as wave 17's *"no results" and "cannot see the data"
must never render identically*, one level earlier: **do not let a caller choose
between two wrong answers when the producer already knew which was right.**

**And the fixture lesson, which is the sharper one.** `oc-cli/tests/tui_turn.rs`
and `tool_turn.rs` both injected `OPENCODE_MODELS_PATH`. That is why five waves of
seam tests never caught this: they handed the binary the very catalog the product
should not have needed. Removing both injections is part of the product fix, not
housekeeping — and the proof is that **mutation 1 (restoring the fail-fast) now
fails both seam tests**, which it could not have done before. A fixture that
supplies a dependency the product is supposed to do without does not test the
product; it tests the fixture.

## [2026-08-07] Task 88: a public runner can compare two subjects without copying its frozen workload

The frozen runner already exposes everything the gate needs except a two-subject
entry point: `OC_TESTKIT_ORACLE` selects the launched binary, each public report
contains five runs per workload, and `interleaved_pair_order(5)` defines the AB/BA
order. Two sequential runner passes plus an immediate dispatcher therefore recover
five TypeScript and five Rust runs while leaving the private workload, PTY windows,
process-tree sampler and aggregation rule single-sourced in `oc_testkit::perf`.

Long measurements need identity-aware resume, not merely an output-file check. The
Task 88 work directory fingerprints the harness executable, both subject binaries
and the immutable W-real database. A completed pass survives interruption only when
all four identities still match; changing any measured bytes invalidates it. This
keeps a two-hour gate recoverable without allowing stale measurements to pass.

The real attempt also reinforced that a performance gate must fail closed before
publishing partial numbers. TypeScript completed W-idle and W-real repetitions, but
the Rust W-real launch emitted zero provider requests. Because the frozen runner
writes a report only after all five repetitions, there is no valid Rust median and
therefore no numerical G1/G2 verdict to report. The useful result is the blocked
boundary and its durable evidence, not a ratio reconstructed from incomplete logs.

## [2026-08-07] Task 109: `api` is a shape hint, `options` is where the endpoint lives

Upstream keeps two different questions in two different places, and conflating them
is what made a documented provider config undialable. `model.api` is an **SDK-shape
hint** — `provider.ts:230-232` reads `api.endpoint` to pick `sdk.responses` vs
`sdk.chat`, `:368` reads `api.npm` to pick a factory — and its `url` field is only
the catalog merge's own rung (`:1455`). The URL the SDK is actually constructed with
is chosen later, in `resolveSDK` (`:1698-1700`), where the **provider's** option bag
wins: `options.baseURL`, when a non-empty string, beats `model.api.url`. The bedrock
loader (`:355-358`) adds the second spelling: `options.endpoint ?? options.baseURL`.

Our `model_spec` read `model.api.url` and nothing else, so a provider carrying its
endpoint in `provider.<id>.options.baseURL` — the shape every upstream doc page
shows, and the shape todo 88's frozen workload emits — reached the transport with no
endpoint at all and was declined before a socket was opened. Full ladder now:
`options.endpoint` → `options.baseURL` → `model.api.url`, each rung tested non-empty.

**Where a precedence lives matters as much as what it is.** Resolving this during the
catalog merge (writing the option into `ResolvedModel.api.url`) was the tempting fix
and it is wrong twice: `api.url` is a *printed* field (`models.rs:151`, `opencode
models --verbose`), so rewriting it makes our catalog output diverge from the
oracle's for the same input; and it leaves `model_spec` still reading `api.url`, i.e.
two readers of "where do I dial?" that can disagree. Resolved at spec construction
instead, so `Spec::base_url` is the single answer.

**An endpoint is a URL, not an SDK parameter.** `model_spec` forwards every
`model.options` entry via `with_option`; `endpoint`/`baseURL` are now skipped there.
They would have been *inert* today, because `Spec::options` is read by allow-listed
key only — and inert-today is exactly how a body field named after a URL survives
until someone widens that read.

**Fixtures fail in both directions.** Blocker #6 was a fixture supplying something
real input lacks (a top-level `api`). Fixing it exposed the mirror image three tests
down: `catalog_with_two_models_and_a_title_override()` **omitted** something real
input must have — any endpoint at all — so it described a provider no turn could run
against, and building a `base_url: None` spec from it passed unnoticed for waves.
Ask both questions of a fixture: what does it add that reality does not, and what
does it leave out that reality always has?

**Mutation-test the test, not just the fix.** Four mutations were tried; three were
caught immediately. The fourth — deleting the option-bag exclusion — was **not**,
because the test planted `baseURL` in the *provider's* options while the code
iterates the *model's*. It passed vacuously and would have shipped. A test that
never fails when its target is broken is worse than no test, because it is counted.

**Two adjacent gaps found, deliberately left (each needs its own todo).**
(1) `${VAR}` placeholders in a base URL are never expanded, though
`catalog/resolved.rs:85` documents them and upstream expands them at `:1701-1716`; an
azure-shaped catalog URL reaches the transport with a literal `${...}` in it.
(2) **Provider**-level options are never forwarded to the transport at all — only
model-level ones are — although upstream hands `{ ...provider.options }` to the SDK
(`:1673`). So provider-level `useCompletionUrls`, `timeout`, `headerTimeout`,
`chunkTimeout`, `setCacheKey` are silently inert here even where `Spec::options`
readers for them exist (`oc-provider-compatible/src/surface.rs:306`, `quirks.rs:148`).

## [2026-08-07] Task 110: the SDK option bag is seeded from the provider, and a config key beats a stored one

Two independent defects lived in one line of omission. `model_spec` forwarded only
`model.options`, so (a) every provider-level option was dropped and (b) `apiKey` — one
of those options — never reached the transport, leaving the stored credential as the
only auth source. Upstream: `provider.ts:1676` seeds the bag from the *provider*
(`const options = { ...provider.options }`), `:1497` overlays the model's deep with the
model winning, and `:1719` makes `options.apiKey` primary with the credential as
fallback. Ours had (a) missing and (b) inverted.

**A dropped option and a dropped credential look identical in a green suite, and they
are not the same severity.** `useCompletionUrls`, `capabilities` and `extraBody` being
inert is a config bug. `apiKey` being inert is a 401 for every correctly-configured
user. They were one omission, and only the second one is a launch blocker — worth
remembering when a "plumbing" gap is triaged as cosmetic.

**Presence tests and emptiness tests are not interchangeable, and upstream is precise
about which is which.** `:1699` requires `baseURL !== ""`; `:1719` only asks whether
`apiKey === undefined`. Both readings are right for their field: an empty URL cannot be
dialled, whereas an empty key is a user declaring the endpoint takes none — and
falling back there would present a real vendor key to an endpoint the user never named.
Copying `baseURL`'s emptiness test onto `apiKey` was mutation M7 and it is caught.

**The oracle's own schema can make a defensive branch unreachable — assert that rather
than test the branch.** `provider_api_key`'s `as_str` guard cannot be reached from a
config file, because `ProviderOptions::api_key` is typed `Option<String>`
(`oc-config/src/schema/provider.rs:54`) and `{"apiKey": 7}` is refused at load. The
guard stays (the resolved options are a free-form JSON map), but the test asserts the
*unreachability* — so a schema loosening fails a named test instead of turning a number
into `Bearer 7` at a gateway.

**"Reaches the surface that reads it" is not always "reaches the wire".** The todo
expected a provider-level `useCompletionUrls` to be observable on a request. It cannot
be for this transport: it gates `SurfaceRule::Azure` only, and `openai-compatible` is
`Fixed(Chat)`, so `resolve_surface` returns before consulting it
(`surface.rs:279-286`). The reader is still worth asserting through — directly — and
the wire-observable proof of the seed has to come from a different option. `extraBody`
is the one, because it becomes request-body keys (`provider.rs:185`), which a mock
server parses and a test can read. Pick the forwarded option whose effect is visible at
the layer you are willing to assert at.

**Assert on every captured request, not the first.** The measured symptom was
`AUTH=None` **twice** — the title prelude and the turn are two separate provider
requests through two separate `EngineModel`s. A fix that authenticated only the turn
would still 401 the prelude, and a test reading `captured[0]` would not notice.

**Nine mutations, nine caught, no vacuous test this round** — and M1 is what proves
it: removing the provider seed fails the `useCompletionUrls` test, so that test cannot
be planting the key in a map the code never reads. That is the trap todo 109 fell into,
and dropping the seed is the cheap check for it.

## [2026-08-08] Task 111: a byte-identity test can pass while the guard it exists for is gone

`expand_variables` is a hand-rolled port of `/\$\{([^}]+)\}/g`, and `[^}]+` needs at
least one character — so `${}` must stay literal. The scan enforces that with an
`offset > 0` guard. I wrote a byte-identity test listing `${}` among the inputs and it
passed. **Then I deleted the guard and it still passed.**

The reason is worth remembering because it generalises past this function: with the
guard gone the mutant treated `${}` as a placeholder named `""`, looked `""` up in an
environment that did not bind it, hit the `?? item` fallback, and emitted the original
`${}` — byte-identical. The test asserted the right output for the wrong reason, and it
could not have distinguished the two implementations no matter how many inputs I added,
because the *fixture* made both paths converge.

Binding `""` in the fixture (`Env::from_pairs([("SET", …), ("", "empty-name")])`) is
what makes the claim observable: now the mutant substitutes `empty-name` and the test
fails. Nothing can export an empty-named variable through a POSIX `environ`, so the
input is unreachable in production — but the *guard* is the port's fidelity to the
oracle's regex, and a guard nothing tests is a guard that will be deleted during a
future "simplification".

The general shape: **a fallback path and a correct path that produce the same bytes for
a given fixture are indistinguishable, and adding more inputs does not help — you have
to change the fixture so the two paths diverge.** Todo 109's agent found one of these in
its own work (values planted in the wrong map, passing vacuously); this is the same
failure with a different mechanism. Mutating your own fix is how you find it; asserting
harder is not.

## [2026-08-08] Task 111: the resolved base URL is diagnosable data that the CLI throws away

While writing todo 111's failure-path test I asserted the CLI's output still names the
misspelled variable, because the todo's QA scenario asks for "a diagnosable literal".
It does not, and the fix is not at fault:

* `ProviderError::transient` (`oc-error/src/provider.rs:115`) attaches the transport
  error as `#[source]`, and reqwest's own message names the URL. The literal `${VAR}`
  therefore *is* in the error value.
* `describe_turn_failure` renders `error.to_string()`, and `Transient`'s
  `#[error("transient provider failure (status={status:?})")]` does not walk the source
  chain. The user reads `transient provider failure (status=None)`.

So every connection-level provider failure — wrong host, wrong port, dead gateway, TLS
refusal, and now an unexpanded placeholder — renders as the same seven words naming
nothing actionable. This is the same class of defect todo 109 fixed at plan time
(`unrecoverable provider failure (status=None)` for a missing endpoint) and todo 110
fixed for auth: a correctly-classified error whose *rendering* discards the one detail
the user needs. The remaining instance is the transport-level one, and it is one
source-chain walk in `describe_turn_failure` away from being fixed. Recorded in
`issues.md`; deliberately not done inside a URL-expansion commit because it changes the
user-visible text of every provider failure.

## [2026-08-08] Task 111: upstream's `varsLoaders` has no equivalent here, and the order it would occupy matters

`resolveSDK` expands base URLs **twice** (`provider.ts:1698-1717`): first through
`varsLoaders[model.providerID]`, then from the environment. The loader pass is how
`azure` injects `AZURE_RESOURCE_NAME` (`:270`), `amazon-bedrock` turns `options.region`
into `AWS_REGION` (`:364`), `google-vertex` supplies `GOOGLE_VERTEX_{PROJECT,LOCATION,
ENDPOINT}` (`:521`) and `cloudflare` supplies `CLOUDFLARE_ACCOUNT_ID` (`:760`).

**This workspace has no custom-loader registry at all.** Searched for `CustomLoader`,
`vars_loader`, `VarsLoader` and each of those variable names: the only `AWS_REGION` hit
is `oc-provider-bedrock/src/provider.rs:63` reading the ambient process environment for
its own region default, which is not a vars loader and does not feed URL expansion. The
`custom_loader` hits are `oc-tools`' unrelated plugin-tool loader.

Only the environment pass was ported. What is worth writing down is the *order* for
whoever adds a loader registry later: the loader pass runs **first**, so the environment
wins on any name both supply. Adding a loader after the environment pass would silently
invert that precedence, and nothing in the current tests would notice because there is
no loader to conflict with. `expand_variables`' doc comment says so at the call site.
## [2026-08-08] Task 88 attempt 7: the number finally exists, and W-real reverses the idle result

The exact preflight that failed before todo 109 now completes in 0.08 s: a real
`opencode-rust run`, config-only model, endpoint only in `provider.options.baseURL`,
and cassette-backed provider produced all three frozen requests. No `apiKey`
workaround and no top-level `api` key were added.

The unchanged revision-2 runner then completed both durable passes in 6,420.95 s.
W-idle is dramatically below the gate: Rust peaks `[20572, 20528, 19632, 20040,
19588]` KiB (median 20,040) versus paired TypeScript `[1003276, 1025708,
1018024, 931436, 997892]` KiB (median 1,003,276), and the committed 954,240 KiB
baseline gives a 477,120 KiB ceiling. Rust/committed is 0.021001: G1 PASS.

W-real reverses the outcome. Rust peaks `[3078088, 3250116, 3249508, 3077236,
3249624]` KiB (median 3,249,508) versus paired TypeScript `[2951288, 2466488,
2877584, 3140184, 3134244]` KiB (median 2,951,288). Against the committed
3,026,992 KiB median and 1,513,496 KiB ceiling, Rust/committed is 1.073511 and
Rust/paired is 1.101047: G2 FAIL. The paired TS median is within 2.50% of the
committed baseline, so the failure is not explained by a stale TypeScript baseline.

This is the seventh useful outcome from a gate that refused to manufacture a
number. The prior six defects are gone; the remaining failure is now the product's
measured memory behavior on the 931-message / 3,620-part session, not an execution
seam. A tiny fresh session is not evidence about history hydration.

## [2026-08-08] Task 113: ownership, selective hydration, and fixed-size cache state make the W-real gate pass

The memory defect was not one oversized allocation. It was the overlap of several
semantically equivalent representations: fully decoded database history, retained
history, projected history, provider messages, and prompt-cache prefix bytes. Moving
only the history path reduced G2 from 3,249,508 KiB to 1,680,888 KiB, but still failed
the 1,513,496 KiB ceiling. The last 186,652 KiB came from removing another full-prefix
lifetime: the append-only cache now retains a fixed-size SHA-256 fingerprint rather
than the serialized prefix itself.

The final unchanged-gate chain is therefore useful diagnostic evidence:

- Task 88: 3,249,508 KiB median — FAIL.
- First Task 113 implementation: 1,680,888 KiB median — FAIL.
- Final Task 113 implementation: 1,494,236 KiB median — PASS.

The final W-real peaks were `[1659152, 1495272, 1494236, 1493788, 1493900]`
KiB. The median is only 19,260 KiB (1.27%) below the ceiling. This is a valid
numeric pass, but it is fragile: future work should treat any new full-history or
full-prefix resident copy as likely to consume the entire margin. W-idle remained
healthy at a 19,776 KiB median.

Selective hydration must begin from metadata, not decoded parts. The database path
first discovers message ordering and compaction boundaries from cheap rows, then
hydrates only the required ranges. From there the engine moves owned payloads through
compaction, prelude construction, projection, and provider conversion. An API can be
logically zero-copy at one function boundary and still retain the source collection
at its caller; the useful question is which complete representations are live at the
same peak, not how many `.clone()` calls appear locally.

Correctness needs a pre-optimization oracle at the provider boundary. The shared
fallback oracle fully hydrates the session using the old path and compares serialized
provider-visible JSON bytes. It covers failed and empty summaries, a dangling
compaction tail, and no compaction marker. This is stronger than comparing internal
message structs because it pins the exact bytes prompt caching and the provider see.

Mutation testing exposed an important distinction: replacing explicit dangling-tail
handling with `.unwrap_or(0)` still passes because draining `..0` drops nothing and
therefore has identical behavior. A test cannot kill an observationally equivalent
mutation. Replacing it with `.unwrap_or(messages.len())` is the meaningful mutation;
it drops history and the byte-equivalence oracle fails. Record equivalent mutations
honestly rather than weakening a test or claiming sensitivity it cannot have.

## [2026-08-08] Task 113 follow-up: a zero exclusive drain is not a trimmed window

The exact `.position(...).unwrap_or(0)` mutation was re-run after a reviewer reported
it as a remaining gap. It still passes, and the reason is a Rust range identity, not
fixture data: `messages.drain(..0)` drains the empty half-open range `0..0`. Both the
explicit `None` fallback and the mutation therefore call `store.hydrate(messages)`
with every element in the same order. There is no session shape that can make those
provider bytes differ.

The successful-compaction/missing-tail fixture now contains an explicit sensitivity
check that projecting `full` differs from projecting `&full[1..]`. This proves the
first message is prompt-bearing and the fixture detects a real first-message drop.
`.unwrap_or(1)` fails the renamed test
`loop_successful_compaction_with_missing_tail_falls_back_to_byte_identical_full_history`
with `optimized loader changed provider-visible history bytes` (exit 101).

The third branch was also mutation-tested directly. Adding `messages.drain(..1)` to
the no-marker `None` arm fails
`loop_without_compaction_marker_is_byte_identical_to_full_history` with the same byte
oracle (exit 101). Thus no-marker fallback is covered; the only surviving mutation is
the one that performs no mutation to observable state.

## [2026-08-08] Todo 114: a fixture that cannot disagree with the code proves nothing — build the decoy in

The recurring trap on this project is a fixture that supplies something the real input shape
does not have, so both code paths converge and the test passes vacuously. Todo 114 was
maximally exposed to it: the whole task is *"which session gets selected"*, so a fixture
database containing only the pinned session would pass identically under the pin and under the
old largest-session rule.

**The technique that closes it: make the fixture able to disagree, then assert it does.**
The determinism fixture seeds a **heavier decoy** (1,280 part bytes vs the pinned 96) and ends
with an explicit guard:

```rust
assert!(
    heavier_decoy().part_data_bytes() > first.session.part_data_bytes,
    "the decoy must be heavier or this test proves nothing"
);
```

That assertion is not decoration — it fails if a future edit shrinks the decoy and silently
turns the test vacuous. Mutation M3 (revert to largest-session selection) is caught **only**
because of the decoy.

Same principle applied to the identity check: the test asserts wrong-length-right-digest and
right-length-wrong-digest **independently**, then adds a **positive** case with the honest pin.
Without that positive case, `return Ok(())` at the top of the function and `return Err(...)`
at the top would both look identical to a test that only ever expects failure.

## [2026-08-08] Todo 114: extract a duplicated check instead of testing it twice

The gate needed a fast-fail identity check (reject a wrong database in seconds rather than
after a 100-minute pass), and the capture path needed the same check for correctness. Writing
it twice would have meant two implementations that can drift, and only one of them reachable
from a unit test.

Extracted it as one public `verify_pinned_database` that both call. Result: the gate path now
exercises library code the unit tests already cover, and mutation M9 (neutering the shared
function) is caught. The honest residual — deleting the *call* from the gate — is not
unit-caught, but it is also not a correctness regression, because the capture path enforces the
same check unconditionally. Worth stating explicitly rather than claiming full coverage.

## [2026-08-08] Todo 90: a backpressure registry must be source-derived and behavior-backed

A hand-maintained list can prove every listed channel has a policy while silently missing the
one channel that matters. The G5 registry therefore scans production Rust source and compares
the exact constructor set with its declarations. It recognizes bounded and unbounded Tokio,
standard-library, async-channel, crossbeam and flume spellings, including multiline calls; adding
one unregistered construction fails before its behavior can be forgotten.

The registry check alone is still insufficient. Each of the 17 persistent bounded channels has
an independent probe that fills or closes the channel, checks the declared policy, and requires
an unrelated task to increment a progress counter. Removing that increment makes the gate fail
with `no independent progress was observed`; "the send eventually returned" is not evidence that
the rest of the runtime remained live.

G6 also needs an enumerator self-test. A cleanup test whose PID enumerator accidentally returns
an empty set passes every scenario vacuously. `orphan_enumerator_reports_a_live_pid` first proves
the current process is visible, and a mutation returning no PIDs is rejected before either
containment scenario can claim zero survivors.

## [2026-08-08] Todo 90: killing only the direct host is not process-tree containment

The first abnormal-termination fixture exposed a race where the host exited before its
grandchild. A monitor that stops when the direct child exits leaves that grandchild alive. The
Linux guard must own a process group and kill that group on both branches: caller death and direct
host exit. The real G6 fixture deliberately gives every LSP/MCP/PTY/plugin host a grandchild and
asserts at least 33 enumerated PIDs, preventing a direct-child-only fixture from passing.

## [2026-08-08] Todo 90 review correction: do not convert a design argument into a measured defect

The pre-`oc-process` launch paths were never run under the final real-host G6 fixture. There is
therefore no pre-guard survivor count and no measured before/after pair to cite. The durable
numbers begin after containment exists: at least 33 fixture PIDs and zero survivors after both
clean shutdown and parent `SIGKILL`. An intermediate guard revision exposed the need to kill the
host process group when the direct host exits first, but its exact survivor list was not retained.
The accurate claim is that `oc-process` enforces the new abnormal-exit contract, not that it fixes
a quantified old orphan count.

This distinction also sharpens scope review. LSP, MCP, PTY and JSON-RPC plugin launch wrapping are
directly exercised by G6. `oc-cli` activation is the production composition-root integration; the
fixture activates itself. The JavaScript plugin host wrapping is uniform coverage of the plugin
crate's second persistent host implementation and is not exercised by this G6 fixture. Calling all
six changes “required by the observed failure” would be false.

## [2026-08-08] Todo 90 review correction: warning provenance is part of verification

The first final report described Clippy's `useless_conversion` as a non-blocking pre-existing
warning. Both adjectives were wrong: the merge gate rejects every warning, `main` was clean, and
this commit introduced `.map(OsString::from)` over an iterator already yielding `OsString`.

Rule for future gates: before calling a diagnostic pre-existing, compare the warning line against
the branch base (or run the exact command on the base). A successful exit code is not a clean
Clippy gate when stdout/stderr contains `warning:`. The fix is the mechanism, not an allow: take
the `Vec<OsString>` returned by `memory_flags` directly.

The `lsp_diagnostics` limitation remains correctly attributed: its MCP request cwd is fixed to the
orchestrator checkout and rejects sibling-worktree paths before analysis. Running
`rust-analyzer diagnostics .` from the worktree is the appropriate same-backend fallback.

## [2026-08-08] Todo 90 review correction: full-graph offline metadata is a dependency gate

A target-gated dependency is invisible to checks that only build the host target. On Linux,
`cargo build`, `cargo test`, and `cargo clippy` all passed while the uncached Windows-only
`process-wrap = 9.1.0` still made the workspace impossible to resolve offline. The repository's
`make ci` path and merge gate use `cargo metadata --locked --offline --format-version 1`, which
resolves the complete dependency graph across targets and exposed the failure.

Rule for future dependency changes: run locked offline metadata before claiming the change is
safe. It is the gate that sees `cfg(windows)` dependencies from a Linux host. A host-target build
proves only that target's selected graph; it does not prove the lock can be consumed under the
repository's offline invariant.

## [2026-08-08] Todo 112 — when the same defect appears three times, the fix is one level up

Nine seams have now been found by tests written for adjacent todos. Three of them were
one class: *our error rendering drops the `#[source]` chain, so a wrapped failure surfaces
as a category name with no detail.* Todo 109 fixed instance one, todo 110 instance two,
each correctly and each locally. Instance three still reached a user.

The generalisable lesson is about **where** a fix goes, not about errors:

> A per-site fix to a rendering, formatting or reporting defect requires every future
> author to remember the defect exists. A fix at the seam requires nobody to remember
> anything. When the second instance appears, the seam fix is already overdue.

`oc_error::source::describe` is 20 lines. The two per-site fixes it generalises are
smaller still — which is exactly why they got written twice.

### The counterweight: a blanket walk is not automatically better

`foo: bar: baz: qux` for every error would be a regression in legibility. What made the
walk safe was deciding three things explicitly rather than defaulting them:
depth (8, because a real transport failure is already 3-4 links deep), separator (`": "`,
matching how the messages already read), and duplicate suppression (so an
`#[error(transparent)]` wrapper and a variant that interpolates its own source do not say
the same words twice — while a *skipped* link does not end the walk, because the detail
may be beneath it). Each of the three is pinned by a test that a mutation breaks.

### Widening a message surface safely

Two tests made this a text change instead of a text gamble:
- one asserting the category still leads the message, for **every** variant of the enum
  rather than the ones with known assertions — the next assertion will be written against
  whichever variant a table skipped;
- one matching the enum exhaustively with no wildcard arm, so a new variant fails to
  compile until its author decides what the seam renders.

Neither needed an existing expectation re-worded. That is the evidence the widening was
additive.

## [2026-08-08] Todo 92: how to write a docs test that is not vacuous, and one race it hides

### The shape that proves nothing, and the shape that works

A hand-written Markdown table plus a test that reads that same Markdown is the
vacuous-fixture failure in documentation form — both sides are one artifact, so it
passes for any content including content that contradicts the code. The fix is
mechanical and worth reusing: delimit each table with
`<!-- generated:BEGIN name --> / <!-- generated:END name -->`, derive the EXPECTED
side from a live code artifact, compare against the committed bytes, and offer
`OC_DOCS_REGENERATE=1` so the correct response to a failure is *taking the code's
version*, not retyping it. Prose outside the markers is never touched, so a page
stays readable while its tables cannot drift.

**The strongest block in `crates/oc-cli/tests/docs.rs` is the `/api` one, and the
reason generalises.** It does not read the route-registration source; it builds the
real router and issues one request per operation, classifying anything that answers
`501` as a stub. So "registered but does nothing" is *measured*, and a stub that
gains a handler is reclassified without anyone editing a table. Reading the source
would have produced the same table today and a wrong one after the next handler
lands.

### The race: one test per generated block is wrong when blocks share a file

First draft had a test per block. Under regeneration two tests writing two blocks
of the same page each did read-modify-write and silently discarded the other's
write — surfacing as `has no <!-- generated:BEGIN divergence-index --> marker` on a
file that plainly had it. **One test per FILE, not per block.** Recorded in a doc
comment on the consolidated test, because splitting it back into four is the
obvious refactor and reintroduces the bug invisibly.

### Todo 10's "rejection list" is not a table, and that is better

There is no const array of rejected forms. There are 10 `DeprecatedForm` variants
whose messages are *constructed per input* by `Deprecation::message()`, embedding
the offending file's absolute path. So no table-vs-table comparison is possible —
the test has to run the detectors and compare rendered output. That is a stronger
assertion than the plan's framing implied: it is against behaviour, not against a
literal. Two forms render from two different detectors with two different
replacements (`AuthPromptCondition` most notably), which a table-shaped assumption
would have documented as one.

Generalised: **when an acceptance criterion says "compare the documented table to
the code's table", check first whether the code has a table.** If the values are
computed, comparing rendered output is the honest translation of the intent.

### Four mutations, and why M2 was the one that mattered

M1 (8th entry, count left at 7) fails on the count guard the compat suite already
had — it proves nothing new. **M2 is the acceptance scenario: 8th entry AND
`DECLARED_COUNT` bumped to 8, i.e. exactly the edit todo 103 will make.** It fails
with `divergence-detail is stale` and prints all eight entries. Choosing the
mutation the *next* task performs, rather than a mutation that trips an existing
guard, is what made the proof worth the round.

## [2026-08-08] Todo 103: a strict kill-switch test needs a non-empty control

The byte-parity assertion would be vacuous if both memory files were empty: an
implementation that accidentally opened them would still return the base prompt.
The integration test therefore seeds a real project entry, proves the enabled path
changes the prompt, and only then compares `memory: false` with bytes copied from a
subsystem-absent control. Mutating the disabled path to append one newline was
caught at the byte-vector comparison.

The same isolation pattern applies to reflection. The disabled test deliberately
constructs a usable `MemoryTool` underneath `ReflectionFork`; therefore the absence
of a task handle and runner notification can only be explained by
`ReflectionConfig.enabled`, not by a missing dependency or an untriggered cadence.

One more reusable boundary: configurable budgets must travel with the opened
store, not be consulted only during config parsing. Rendering, usage reporting,
batch validation, and snapshot consistency all need the same resolved limit or the
prompt header can advertise a value different from the value writes enforce.

## [2026-08-09] Todo 118: a compatibility matrix needs dispositions, not just rows

A complete path list can prove only that a client will not get 404. It cannot tell
an implementation, a 501 stub, and a deliberately visible backend gap apart. The
reusable shape is one executable row per operation with independently reviewable
status, normalized body, and side-effect dimensions. Hard cases may be exempt, but
the exemption must be attached to the row and the operation must still be invoked.

The strongest negative assertion runs before exemptions: reject 501 first. If an
exemption is consulted first, adding a stub to a hard operation can make a matrix
green without adding any capability — exactly the failure this todo closed.

For streaming APIs, route registration and content type are also insufficient.
The smallest non-vacuous test must observe one semantic frame and one event caused
through the public mutation path. Sharing the EventService at the composition root
is what makes that second assertion test the product rather than a private fixture.

## [2026-08-09] Todo 130: a version pin must be checked against a running process, not against another constant

F1's finding B1 was one number doing two jobs. `PINNED_SOURCE_VERSION = "1.18.13"` was
the version reported to the npm plugin gate — a wire value about the *source tree* this
port was read from — and it was also being written into the compatibility report's
`pinned_source_version` as the version the differential was *measured against*, while
the hard-coded oracle path pointed at 1.18.12. Both statements were individually
defensible. Together they made every artifact name a build that never ran.

The reusable shape is two named pins with different jobs, and only one of them verified:

- the **executed release** (`oracle::PINNED_RELEASE`), declared once, and refused by
  `Oracle::discover_pinned()` unless the resolved binary self-reports it;
- the **source baseline** (the located tree's `package.json`, and
  `oc_plugin::js::spec::REPORTED_PLUGIN_API_VERSION`), which is a wire value and has no
  business agreeing with the binary on disk.

The load-bearing detail is what the assertion compares. A test that checks a recorded
version against another hard-coded version proves two hand-typed strings match — the
same vacuity class as this project's other eleven seams. The right-hand side has to be
process output: here `Oracle::reported_version()`, the trimmed first stdout line of
really executing `--version`. Mutating the constant to the old 1.18.13 fails both the
unit gate and, one layer up, the report assembly — and the second failure prevents the
artifact from being *written* rather than written with a wrong claim.

Same principle for a committed capture. Pinning `.omo/fixtures/oracle-openapi-*.json`
by a sha256 constant would again be two committed values agreeing. Refetching `/doc`
from the running pinned release and requiring byte-equality is what makes a "recapture"
a fact instead of a commit-message claim. A one-byte mutation that preserved the file's
length (`"opencode api"` → `"opencode API"`) proves a length check alone would have
missed it.

**Declare the release; discover the path.** Hard-coding `…/mise/installs/opencode/X/…`
pins the harness to one machine's package manager, and the existing `Oracle::discover()`
already honoured `OC_TESTKIT_ORACLE` then `PATH`. What deserves pinning is the release
identity, not the route to it.

Also worth reusing: absence and disagreement are different facts and need different
handling. A machine without `opencode` cannot verify anything, so it skips (loudly). A
machine with the *wrong* `opencode` will happily produce green artifacts attributing
their measurements to a build that did not run, so it must be fatal.
## [2026-08-09] Todo 127 — five things the source said and the binary contradicted

Implemented the twelve read-only catalogue and filesystem API operations. Every one of
the five corrections below came from **running the 1.18.12 binary and diffing**, not
from reading `packages/`. Reading the source got each of them wrong first.

1. **`Schema.UndefinedOr` does not omit the key.** `GET /api/integration/{unknown}`
   answers `{"location":…,"data":null}`. I implemented "skip the key when absent"
   because that is what the schema reads like; the Effect encoder emits the declared
   property regardless.
2. **`localeCompare` is not byte order, and it is observable twice.** `fs/list` lists
   `alpha.txt` before `Cargo.toml`; integration names put `openai` before `OpenCode`.
   Byte order inverts both. Any port of a JS sort needs a case-insensitive-first
   comparator or a differential that catches it.
3. **v1 and V2 serve different bytes for the same asset — three times now.** The
   built-in skill's description (V2 lists `commands`), four agent system prompts (V2's
   `title` writes `->`/`<=50` where v1 writes `→`/`≤50`), and both built-in command
   templates. `oc-catalog` is byte-correct **for v1** — verified by diffing against
   `packages/opencode/src/**` — and `packages/core/src/plugin/**` carries its own
   revision. A port that mirrors v1 must keep V2 assets separately rather than
   "fixing" the v1 files.
4. **V2's command roster has three levels, not four.** v1 promotes every skill to a
   command; V2 has no such transform. Serving the v1 registry over `/api/command`
   offered `customize-opencode`, a command the oracle does not have.
5. **The released binary disagrees with itself for the first seconds after
   startup.** `/api/agent`, `/api/command` and `/api/provider` answer `[]` right after
   it prints "listening", then converge on seven agents and two commands. A cold
   differential reports differences that are entirely the oracle's own initialization
   race. Warm it and *assert* the warm-up, or the flake reads as a port defect.

**The transferable rule:** for any operation whose body is a projection, the source
tells you the field names and the binary tells you the values. Both are needed, and
where they disagree the binary wins.

### The matrix was comparing nothing, and that is a distinct failure from a stub

Before this todo, `api_behaviour_matrix_compares_live_status_body_and_side_effects`
looped over every row and asserted only that non-exact rows *carried an exemption
reason*. A row marked `Compared` was never actually compared in that loop. So the
suite could report "58 of 58 invoked" while comparing five. Fixed by calling
`compare_api_observation` inside the loop for `Compared` rows — and then verified by
mutation: four deliberate body/status changes now fail it, where before they would
have passed. **An exemption ledger that nothing checks is worse than no ledger: it
reads as coverage.**

### Two harness details that would have made the differential lie

- Both matrix servers now serve a `work/` subdirectory instead of the state root. The
  oracle creates `home/`, `data/`, `cache/` inside its root; the subject does not, so
  `/api/fs/list` disagreed about a directory whose difference the harness itself
  created. The fix is to move the state out of the served tree, not to normalize the
  entries away.
- The subject read the developer's real `HOME` and `auth.json` through
  `Env::from_process()`, so `/api/provider` depended on which credentials the machine
  happened to hold. `ApiState::with_env` now pins it. *A catalogue endpoint that reads
  ambient credentials is a test that passes or fails by machine.*
## [2026-08-09] todo 124 — parse-layer assertions cannot prove routing (SEAM #11 closed)

**The rule, stated so it generalises past this file:** an assertion on source
text, on a parse result, or on a static roster proves only that a *declaration*
exists. It can never prove which *code path runs*. Todo 116's two guards read
`DispatchArguments::is_pending()` — the enum variant parsing produced — and the
routing decision is one step later, in `cmd/mod.rs`'s `match`. Re-pointing any
arm at `PendingCommandDispatcher` left both guards green while the binary exited
1. **A guard must name the handler that executed, not the variant that parsed.**

**Why todo 116's mutation check was not enough, and this is the reusable part:**
I did mutate before accepting 116 — but I mutated `PENDING_COMMANDS`, the
*roster*, and two roster-reading tests duly failed. The mutation and the test
read the same layer, so it proved only self-consistency. **A mutation only
validates a test if it is applied at the layer the production code decides at,
not at the layer the test reads.** Ask, before accepting: does this mutation
change what a *user* observes? F2's did — it ran the binary.

**Sampling one arm would have failed the same way.** Verified here by mutating
**all 12** arms of the `match` one at a time: the new binary-driving guard fails
on every one, and both parse-level guards stay green on every one. A guard
covering only `agent` would have fallen to the identical mutation one arm over.
The bijection test (`every_implemented_command_actually_has_a_handler`) is what
promotes the probe table from a sample to full coverage — it is load-bearing, not
decoration.

**Absence assertions are half a test.** "The stub sentence did not appear" also
passes for an arm replaced with `Ok(())`. Each probe therefore asserts a fragment
**only that command's handler emits**. Confirmed by mutating an arm to `Ok(())`
and watching the guard fail on the missing-evidence branch.

**Keep the weak guard, rename it to its true scope.** The parse-level check still
catches a command registered directly onto `DispatchArguments::Pending`
(`completion`'s shape), which the binary-driving guard does not. Deleting it
would have lost real coverage; leaving its name overstating its reach is how the
next reader gets misled. Renaming it and documenting the limit in place costs
nothing and stops the claim from drifting again.

**Seventh fixture-friendlier-than-reality defect, second in this exact file.**
Two in one file says the file's *idiom* was wrong, not that one test was sloppy:
`dispatch_request` made parse-level assertions the path of least resistance.
Fixing the idiom (a `Probe` that carries its own observable, and a helper that
drives the real binary) is what stops a third.
## [2026-08-09] Todo 125: help text is a surface, and no consistency test was reading it

`completion` sat in `PENDING_COMMANDS` with an honest recorded reason while `--help`
advertised "Generate shell completion output" in both the root listing and the
command's own help. Five disposition tests in `tests/surface.rs` passed throughout,
because every one of them compares the disposition table, the pending roster, and the
dispatch arm against each other — and all three were correct and mutually consistent.
The one inconsistency that reached a user was between the roster and the *help*, and
nothing read help.

This is the `export`/SEAM #11 shape again with a new surface: several mechanisms
agreeing with one another while none of them touches what the user touches. The
generalisable rule is that **an entry in a "does not work" roster is also a claim
about every surface that describes the command**, and each such surface needs its own
assertion. Recording the reason in one place is necessary and not sufficient.

Two design points worth reusing:

- Assert on the *structured* description (`clap::Command::get_about` /
  `get_long_about`) and then assert the rendered `--help` contains it, whitespace
  normalized. Parsing help columns is fragile against wrapping; reading the struct
  alone proves a field rather than a user-visible line. Doing both gets exactness and
  rendered truth without a parser.
- Encode the *shape* of the lie, not its wording. The rule "a pending command's
  description must not open with a capability verb" catches "Generate shell completion
  output" and also catches the next "Print the …" or "Export the …" on a stub. A rule
  matching the literal string would have caught nothing new.

Second, independent reason no test could have caught it: `completion` has no row in
`disposition.rs` at all, because upstream registers it through yargs' built-in
`.completion()` rather than as a `*Command` class, so it is absent from the frozen
23-symbol upstream fixture and `disposition_for("completion")` returns `None`. Every
disposition assertion about it was vacuously satisfied. **A command whose disposition
lookup returns `None` is unguarded by the entire disposition suite** — worth checking
for the other surfaces that key off that table.
## Todo 126 — a doc test that reads the artefact, and two defects in the handover

**The handover did not compile.** 371 uncommitted lines of docs-test machinery
used `oc_testkit::FrozenThresholds` and `oc_testkit::load_committed_baseline`;
both live under `oc_testkit::perf::`. `--no-run` failed with three
E0425/E0433. The mechanism had therefore never executed once. First move on any
resumed work: build it before reading it for correctness.

**A generator can restate the number it exists to derive.** The same handover
interpolated `grouped(19_260)` and the literal "about 165,000 KiB" into the G2
prose — todo 113's superseded margin and todo 122's spread, hand-typed inside
the code that was supposed to end hand-typing. Seam class 8 ("the test double is
friendlier than reality") has a sibling: *the generator is friendlier than the
data*. Both halves of a comparison being computed is not enough; the **prose
around them** has to be computed too, or it keeps asserting a conclusion the
data no longer supports.

The fix that actually holds: `g2_robustness_prose` branches on
`margin > spread`, `margin <= spread`, and `median > ceiling`, so a weaker
measurement produces weaker wording. Mutation-proved by editing one artefact
peak — the regenerated sentence flipped to "**narrower than** the 27,032 KiB
five-run spread" on its own.

**Discovery beats naming.** `memory_gate_measurements` scans
`.omo/evidence/task-<n>-opencode-rust.txt`, keeps whichever parse as G1/G2
measurements, and takes the highest task number. Proved with a synthetic
`task-999` artefact: the test immediately demanded a README citing it. A test
that hardcodes `task-123-...` would have gone stale the same way the README did.

**The artefact's narrative is a cross-check, never the source.** Three
assertions make the derived side dominate: the ceiling it prints must equal
`0.50 x` the committed baseline median; the median it restates twice must equal
the median of its own five peaks; the verdict it records must equal the verdict
its median implies. `a_measurement_whose_prose_contradicts_its_own_peaks_is_rejected`
feeds 90,000,000 KiB peaks under a `PASS` narrative and requires the panic.

**Found while there: README's API split was inverted.** "Twenty-three currently
have local backends; the other 35 return 503" against the generated
`api-operations` block's 35 implemented / 23 gaps. Two docs in the same repo
contradicting each other after todos 127/128. Fixed by deriving both halves from
the same route probe the matrix already runs, not by retyping the corrected
pair — `assert_api_counts` now covers `README.md` too.

**Every new helper has a caller.** Checked deliberately after last wave's
near-miss where a helper lost its only call site and clippy caught it. Clippy at
`-D warnings` confirms; a helper with no caller is the same lie as a test that
does not assert.

## Wave 47 — todo 133 (pinning the narrowings)

- **`prune_expired` in `oc-pty/src/ticket.rs:194-201` sweeps the FRONT only.** That
  is correct in production, where `issue` always stamps `Instant::now()` and the
  `VecDeque` is therefore ordered oldest-first, so a non-expired front implies no
  expired entry — and `consume_at` performs no per-entry age check, which makes the
  FIFO invariant load-bearing. A test that back-dates a ticket with `issue_at` while
  a fresher ticket is still outstanding pushes the stale one BEHIND the front and it
  escapes the sweep entirely, so the route accepts it. Any expiry test must drain
  the store first (assert `tickets().is_empty()`) so the stale ticket is provably
  the oldest.
- **A scope-mismatch 403 is indistinguishable from an expiry 403 at the route.**
  `pty::connect` redeems with `TicketScope { directory: Some(resolved), … }`; a
  fixture using `TicketScope::for_session` (directory `None`) gets 403 for the wrong
  reason. The fix that makes it honest is a *positive control*: redeem a FRESH ticket
  carrying the identical hand-built scope first and require 400 (the WebSocket
  upgrade boundary). Only then can the stale ticket's 403 be attributed to expiry.
- **`chat_context_value` (`oc-plugin/src/jsonrpc.rs:1219-1236`) serializes
  `ResolvedModel` with Rust field names, so a JS plugin sees `provider_id`, not the
  `providerID` upstream's type declares.** `chat.message` in the SAME codec spells it
  `providerID` (`:972`), so the two encodings disagree with each other. Measured
  against the real kiro `0.20.6` plugin: with `model.provider_id` matching and
  `provider.info.id` not, its `chat.headers` hook injects nothing — it survives only
  on its `?? input?.provider?.info?.id` fallback. `chat.params` shares the input
  shape. Pinned by an assertion in `oc-plugin/tests/js.rs` that fails when the seam
  is closed.
- **`/config/.cache/opencode/packages/@sunerpy/opencode-kiro-auth@0.20.2` exists as
  an EMPTY DIRECTORY** — no `node_modules`, no `package.json`. A `ls -d` glob makes
  it look installed; only `0.20.6` can actually load. Probe for
  `node_modules/<package>/package.json`, not for the version directory.
- **Deriving a version from `SUPPORTED_JS_PLUGINS` instead of typing it** is what
  stops the three-way drift criterion 6 suffered. A test that names a version
  literally can outlive the package.
- **Freezing a gap set by COUNT cannot catch a swap.** Fourteen stays fourteen when
  one gap closes and another opens. `FROZEN_API_GAPS` lists method+path and diffs
  the list against what the live server actually answers, in both directions, plus a
  third assertion for a member that leaves the set while its matrix row still exempts
  status and body — an operation counted as neither gap nor parity is invisible.

## Wave 48 — todo 132 (HTTP permission/question broker)

- **Upstream schema validation precedes request ownership checks.** Malformed
  permission/question reply bodies return `400` even when the path names another
  session's pending request; a valid cross-session body reaches the handler and
  returns `404`. To preserve fail-closed behavior, the error path claims and drops
  only a matching owned request. This rejects an owned disconnected reply without
  consuming or revealing another session's pending request.
- **Branded path IDs are part of the HTTP contract, not merely generated-name
  conventions.** Upstream permission IDs start with `per` and question IDs with
  `que`; schema middleware returns `400` for `request_matrix` before a handler runs.
  A valid branded but absent ID reaches ownership lookup and returns `404`.
- **A half-written HTTP body is the deterministic disconnect fixture.** Declaring
  `Content-Length: 128`, writing only `{"reply":`, and closing the socket makes
  Axum's body collection fail, after which the error path claims and drops the
  matching owned request. This exercises the production cancellation path without
  timing-dependent client abort APIs.
- **Authored tool calls must satisfy the current schema before they can test a
  broker.** The first fixture omitted required `intent`; dispatch stopped at
  argument validation, so no request could park. Captured provider payloads and
  persisted tool errors distinguished this from an SSE or broker-sharing defect.
- **A process-local broker can reuse the durable event service without becoming
  durable state.** Pending answer senders cannot survive process restart, while
  asked/replied events remain observable through the existing session/global SSE
  streams. Keeping these concerns separate avoids pretending a restored event can
  resume a missing turn.

## [2026-08-10] 把 SEAM #14 的教训泛化去查全仓库——结果基本干净，且找到了正确模式的范本

SEAM #14 的病根是：**测试名听起来像一个类别，实际只覆盖其中一种情形**
（`disconnected_permission_reply_fails_closed` 覆盖回复者断连，不覆盖观察者断连）。

我据此扫了整个仓库，重点看**名字声称普遍性**的测试（`every_*` / `all_*` / `any_*`）——这类名字
一旦名不副实，危害最大。抽查结果：

- `every_claimed_provider_id_constructs_with_only_a_base_url` → 迭代生产常量 `family::CLAIMED`。✅
- `every_governed_tool_id_is_a_real_registry_builtin` → 迭代 `BUILTIN_ORDER` 与 `GOVERNED_TOOL_IDS`。✅
- `all_authoritative_hooks_dispatch_with_their_typed_payloads` → **手写枚举**每个 hook 的 dispatch，
  不迭代 `HookName::ALL`。我一度以为这就是同形缺陷。

**但它是对的，而且用了最强的守法**：`crates/oc-plugin/tests/hooks.rs:22`

```rust
const EXPECTED: [HookName; 21] = HookName::ALL;
```

这是**编译期**长度断言——加第 22 个 hook 会直接编译失败，而不是静默逃过。配套
`manifest.rs:41` 还断言 `HookName::ALL` 与 `ORACLE_HOOKS` 一致。所以三件事各司其职：
名册与上游一致（manifest）、数量被钉死（编译期）、每个都被真的 dispatch（手写枚举）。

### 值得记下的模式

**手写枚举本身不是缺陷——缺少「集合完整性」的守卫才是。** 判据是：

> 若给被测集合加一个成员，是否有东西会失败？编译期失败 > 测试失败 > 什么都不发生。

`hooks.rs` 是「编译期失败」。todo 119 的 `FROZEN_API_GAPS` 与 todo 133 的收窄钩子是「测试失败」。
SEAM #14 的断连测试是「什么都不发生」——它既没有断连方式的枚举，也没有集合完整性守卫。

这条扫描没找到新缺陷，但**把判据写下来比结论更有用**：下次写「覆盖一个类别」的测试时，先问
「加一个成员会不会有东西失败」。
## Wave 49 — todo 134 (observer loss and request deadline)

- **A reply connection and an observation connection have independent lifetimes.**
  Todo 132 proved that dropping a half-written reply body rejects the request; it
  said nothing about dropping the session SSE stream that tells a human the request
  exists. The latter needs an observer-lifetime guard attached to the SSE body.
- **Last-observer cleanup and a deadline close different holes.** An observer guard
  rejects immediately when a session's count falls from one to zero, while the
  five-minute watchdog rejects a request that was never observed. Neither mechanism
  can replace the other, and both send only `Reject`/`Rejected`.
- **Observer accounting belongs to session SSE, not global SSE.** Only
  `/event?sessionID=...` and `/api/session/{sessionID}/event` identify which
  session's prompts that client can answer. A global stream cannot safely prove
  ownership of any one session's pending request.
- **Resource-pressure mitigation must not shrink thread stacks below toolchain
  requirements.** A 1 MiB `ulimit -s` made the external TypeScript oracle segfault,
  and `RUST_MIN_STACK=1 MiB` made `rustc` segfault. Restricting `CARGO_BUILD_JOBS`
  and `RUST_TEST_THREADS` to one avoided the host's transient `EAGAIN` without
  changing process semantics; the final workspace run passed 3,321 tests.
## Task 137 — "the test names a version" is not "the test loads that plugin"

Wave 49. The 12th instance of **a test double being friendlier than reality**,
and the second time the *same* gap in criterion 7 hid behind a green suite.

`three_tiers_follow_configuration_order` was green for many waves while proving
nothing about the two real auth plugins criterion 7 names. It loaded a synthetic
`integration-js` fixture whose id was literally `integration-js`. Nothing in the
test even referenced the real packages, so no amount of reading the assertions
would reveal the gap — only reading the *fixture* would.

**Three things only a runtime probe could tell me**, each of which changed the
test design:

1. **The four tiers share NO mutating hook.** Measured: antigravity registers
   `event`/`tool`/`auth`; kiro registers `config`/`auth`/`provider`/
   `chat.headers`; the Rust example registers `chat.params`/`shell.env`/
   `experimental.text.complete`; a WASM component can only mutate
   `experimental.chat.system.transform` (`wasm.rs::apply_hook_output` — every
   other arm is `Ok(())`). So the obvious design — "dispatch one hook, watch the
   string accumulate through four tiers" — is **impossible with real plugins**.
   `HookBus::dispatch` filters by manifest, so an unshared hook reaches one
   plugin. The synthetic fixture existed precisely because it *did* implement the
   shared hook. That is the whole reason the shortcut was taken.

   The fix: assert order on the one thing both real plugins *do* mutate — the
   `auth` vector, which `HookBus` appends in configuration order — and pin
   position with a wrapper that records dispatch order without changing
   behaviour. Plus a negative test in reverse order, so no single ordering
   satisfies both.

2. **Antigravity emits a Compatibility diagnostic on every load** — "plugin
   declares @opencode-ai/plugin ^0.15.30; host reports 1.18.13" — and keeps
   serving. The pre-existing health predicate in `TierSession` was
   `diagnostics().is_empty()`, which for the real antigravity plugin means
   **permanently unhealthy from load**. Copying that predicate would have made
   every real-plugin test assert against a plugin the wrapper had already
   silenced, and it would still have been green. Health must narrow to fault
   kinds (`Crashed`/`TimedOut`/`Protocol`/`FailedToLoad`).

3. **Kiro's real `config` hook writes to the user's credential store.**
   `bootstrapAuthIfNeeded` (`dist/plugin/auth-bootstrap.js:44`) inserts
   `{type:'api', key:'kiro-bootstrap-placeholder'}` into
   `~/.local/share/opencode/auth.json` when a Kiro CLI DB exists. `config` is one
   of only four hooks kiro registers, so "dispatch every hook the plugin
   declares" — the natural thoroughness instinct — **mutates real user
   credentials from a unit test**. Recorded in the test's docstring so a future
   coverage-increasing edit trips over it.

**The transferable lesson:** when a test uses a fixture *in a slot where the
criterion names a real artifact*, the fixture is not a simplification — it is the
gap. Grep the fixture's id, not the test's assertions.
## [2026-08-10] Task 136: "readable" and "usable" fail separately, and only one assertion sees the difference

The four mutations against the new `session_interop.rs` split cleanly into two
groups, and the split is the lesson.

M3 (`resolve_session` ignores `SessionChoice::Existing` and forks) was caught by the
**session-identity** assertion. M4 (`retained_history` returns only the newest
message) was caught by the **wire-replay** assertion — and by nothing else. Under M4
the session still listed, still exported, and its transcript still **grew**: the port
appended a fresh user+assistant pair to the right session row. Every storage-level
assertion I would naturally write was satisfied while the port had silently stopped
carrying the history it inherited. The only thing that noticed was reading the user
turns out of the **outbound provider request**.

So the strongest available form of "the opposite implementation can decode what the
other wrote" is not "it parsed" and not even "the transcript grew" — it is *the bytes
came back out of the decoder and onto the wire*. For any round-trip claim, find the
place where the decoded value is used, not the place where it is stored.

**A mutation test that does not rebuild the subject binary proves nothing.** My first
M1 run looked uncaught for exactly one reason: `Subject::discover_or_build` reuses an
existing `target/debug/opencode-rust`, so the suite re-ran against the *pre-mutation*
binary. I nearly concluded `model_reference` was no longer the production writer and
went looking for a second one. `cargo build -p oc-cli --bin opencode-rust` between
mutation and run is mandatory; without it the green result is an artefact.

**M1's asymmetry is the tests being precise, not a gap.** Re-injecting todo 115's
`modelID` spelling failed the Rust→TS direction and the row-shape guard, and left the
TS→Rust direction passing — correctly, because in that direction the *oracle* created
the session, so this port's mis-spelling never reached upstream's decoder. A mutation
that fails every test is weaker evidence than one that fails exactly the tests whose
seam it crosses.

**Measured while doing this — an undeclared part-shape divergence.** One assistant
turn: release 1.18.15 persists `text, step-start, text, step-finish` (4 parts), this
port persists `text, text` (2). Reproduced on the `run` path inside a git repository
and outside one, so it is not the "step-start only carries a snapshot" case —
production data has 218,899 `step-start` rows whose blob is literally
`{"type":"step-start"}`. Interoperability holds across it in both directions, which
is itself worth knowing, but it is undeclared in `docs/divergences.toml` (13 entries).
- **An empty `.git` directory does NOT make upstream treat a directory as a
  worktree, and the difference silently changes what a comparison compares.**
  `compat_suite.rs` marks fixture roots with an empty `.git` to stop config
  discovery leaking to ancestors, which works. It does not work for VCS
  detection: upstream resolves the worktree with `git rev-parse --show-toplevel`,
  which rejects an empty directory, assigns a non-VCS project the worktree `/`
  (`packages/opencode/src/project/project.ts:217`), and then reports a
  *different* plan glob in `agent list` — measured as the relative
  `tmp/.../plans/*.md` against this port's absolute path. A parity probe under an
  empty `.git` therefore compares two answers to different questions while
  looking perfectly reasonable. `crates/oc-cli/tests/cli_parity.rs` runs a real
  `git init` and fails loudly when `git` is absent, because a silent fallback
  would keep the probes running against the wrong project shape.
- **The released binary emits SGR colour under `NO_COLOR=1` and `TERM=dumb`.**
  `NO_COLOR` is honoured by the TUI theme, not by the CLI's error and prompt
  writers: `import probe.json` writes `ESC[91m ESC[1m Error: ESC[0m`, and
  `@clack/prompts` also writes its box's closing colour reset to **stderr** — so
  after SGR stripping the oracle's stderr for `mcp list` is a lone `"\n"` where a
  plain writer's is empty. Any CLI stream differential against this oracle needs
  an SGR rule and a trailing-blank-line rule, and the second must be
  unconditional: guarding it on "does this stream look decorated?" makes it fire
  for stdout and not for stderr.
- **`run` cannot be output-compared against the released binary at all, because
  1.18.15 answers with no credential present.** Measured under a cleared
  environment with an empty `auth.json`: `run --agent nosuch hi` printed a real
  model reply from the bundled `opencode/big-pickle` gateway model and exited 0.
  Any "compare a turn" probe against this oracle is a live provider call, which
  `oc-testkit` has no capability to make. Only `run`'s argv-refusal paths are
  comparable; the turn belongs to the cassette-driven tests.
- **A count of exemptions cannot protect an exemption ledger; the members have to
  be frozen by name.** `crates/oc-cli/tests/cli_parity.rs` keeps
  `COMMANDS_WITH_EXEMPTIONS` as a name set compared against what the table
  actually says, plus two floors (>= 9 commands with every stream compared, >= 11
  with exit and stdout compared). With only a count, one command's exemption
  closing while another's opens leaves the number unchanged — the same swap
  `FROZEN_API_GAPS` was made a name list to catch.
- **An exemption needs a witness that is OBSERVED, not asserted.** Each exempting
  probe names a weaker fact — both sides refuse, or the oracle's stderr contains
  a specific fragment — and the test runs both binaries to check it. That is what
  keeps "this stream is not comparable" from degrading into "this command is not
  tested": the `tui` exemption, for instance, stands or falls on upstream really
  printing `Failed to change directory`, which is what proves the two binaries
  were asked different questions rather than behaving differently.

## Task 138 — stale subject binaries

- **Cargo's no-op build is the cheap identity check the harness was missing.** A
  warm `cargo build -p oc-cli --bin opencode-rust` measured 0.29, 0.28, 0.27,
  0.27 and 0.26 seconds (0.27-second median). Running it once per test process is
  cheap enough to keep enabled and strictly stronger than comparing one mtime or
  Git revision, because Cargo tracks the whole dirty dependency graph.
- **A source mutation is proven only when the process under test changes.** With
  `hydrate_retained_history` temporarily returning an empty vector and no manual
  build, the repaired harness rebuilt the subject and `session_interop` failed
  three lifecycle tests with `has no user message to answer`; restoring source
  let the same harness rebuild back to four passing tests.
- **An override path alone is not provenance.** The previous subject label named
  only the executable, so nobody reading a report could tell whether it came from
  workspace discovery or `OC_TESTKIT_SUBJECT`. Provenance now names automatic
  Cargo builds, explicit paths, pre-existing discovery, and the environment
  override separately.
## [2026-08-10] Task 139 — a negotiated resource is inert until a production consumer calls it

- `JsPlugin::build` already retained Kiro's `ProviderHook.models`, and
  `HookBus::dispatch(Provider)` already returned it, but no production caller loaded
  JavaScript plugins from the `models` command or invoked the retained loader. A
  resource being negotiated and a hook being dispatched are therefore weaker facts
  than its models appearing in the catalog.
- Kiro's provider depends on its `config` hook creating `provider.kiro-auth.models`
  before the provider loader normalizes those models. The bridge called every normal
  hook as `(input, output)`; the SDK's `config` hook is the exception and accepts one
  mutable `Config`. The JavaScript host returned an error, `JsPlugin::call` contained
  it, and the catalog stayed unchanged. The correct production order is plugin
  `config` -> catalog resolve -> provider resource collection -> provider model load.
- `ProviderHook.models` also takes two SDK arguments, `(provider, context)`. Passing
  one synthetic `{provider, auth}` object happened to be friendlier than reality and
  made Kiro see no `provider.models`. The bridge now sends the two real arguments.
- The acceptance probe's `env -i PATH=/usr/bin:/bin` removes mise shims. A test that
  inherits the developer shell can pass while the documented probe cannot start any
  JavaScript plugin. Runtime discovery must preserve explicit test-path injection but
  may fall back from ambient `PATH` to standard per-user Bun/mise installation roots.
- Rebuilding before measuring mattered again: the first `target/debug/opencode-rust`
  run showed ten providers from a stale binary. After `cargo build -p oc-cli`, the
  pre-fix source showed the real eight-provider baseline.
## Todo 140 — upstream writes `step-start` unconditionally, which settles todo 136's doubt

Re-derived from the oracle tree rather than inherited from todo 136's evidence.

Upstream's processor calls `snapshot.track()` first, but the `session.updatePart(...)`
on the NEXT line runs whether that returned a hash or `undefined`
(`packages/opencode/src/session/processor.ts:424-432`). Snapshots are enabled only for
a git project with snapshots not disabled (`packages/opencode/src/snapshot/index.ts:167-170`)
and `track()` returns `undefined` otherwise (`:318-323`). So a non-git project still
persists `step-start`, just without the `snapshot` field.

That independently confirms todo 136's "reproduced in a git repository and outside
one" and closes the "step-start only carries a snapshot" hypothesis from the source
rather than from repetition.

One plain single-step turn upstream: `step-start`, `text`, `step-finish`. The text
deltas accumulate into ONE `text` part (`:486-530`), not several. `patch` appears only
when `patch.files.length > 0` (`:457-468`).

What is actually lost by not writing them:
  * `step-finish.cost`/`.tokens` is the ONLY source upstream's projector reads to
    aggregate session usage (`packages/core/src/session/projector.ts:36-42`, applied
    `:90-108`, `:325-328`).
  * the first `step-start.snapshot` and last `step-finish.snapshot` are the two bounds
    of a turn's DISPLAYED diff (`packages/opencode/src/session/summary.ts:82-99`),
    refreshed by `revert` (`packages/opencode/src/session/revert.ts:70-77`). The file
    restore itself uses `patch` parts (`:45-52`), so revert's correctness does not
    depend on them.
  * `run --format json` emits both as `step_start`/`step_finish`
    (`packages/opencode/src/cli/cmd/run.ts:740-746`).
  * the current App SKIPS both when rendering
    (`packages/app/src/context/server-session.ts:26-31`) — so no TUI consequence.

## Todo 140 — SEAM #15 bit again, and the mutation was designed around it

Mutating the gap's CONSTANT would have proved nothing (the constant is the record, not
the behaviour). The stale-direction mutation had to change the real production path:
`checkpoint_assistant` was made to persist a `step-start` part. That requires an
explicit `cargo build -p oc-cli` first, because `Subject::discover_or_build` only
builds when the binary is ABSENT (`crates/oc-testkit/src/subject.rs:68-77`) — without
it the witness measures the previous binary and reports a false green.

Rule for any future behaviour-mutation proof in this repo: rebuild the subject
explicitly, never rely on `discover_or_build`.
## [2026-08-10] Task 141: observe the public boundary, then mutate one release trigger

- A persist-before-live unit assertion is weaker than the user-visible guarantee. The
  discriminating guard must first consume the public session SSE frame and only then
  ask the public history route for that exact durable sequence. Moving fan-out before
  append with a bounded delay made the otherwise tiny race deterministic and proved
  that the route-level assertion, rather than scheduler luck, catches the inversion.
- Fail-closed lifecycle tests need to keep every unrelated release mechanism alive.
  Holding both `RequestBroker` and the asker task while independently removing the
  malformed-reply claim, observer-zero collection, or watchdog made each test time out
  only for its named trigger. Letting the broker or sender drop would turn channel
  closure into a false positive.
- A feature-gate skip is a diagnostic, not coverage. The default workspace run can say
  why WASM integration did not run while remaining green; the required CI job must
  invoke the feature-enabled integration target itself.
## [2026-08-10] SEAM #17：见证只断言「都失败」时，声明的三个面里有一个连探针都没有

todo 142 修 F4 指出的那条弱点，实测出来比它描述的更深一层。

`Witness::BothFail` 只求值一个谓词——两边 exit 都非 0。它的两个使用者正是
`diagnostics-name-their-cause` 的两个探针。但**这条 divergence 声明了三个面**
（`serve` 绑不上端口、`run` 无消息、`run` 模型无法解析），表里只有前两个的探针，
**第三个面一个探针都没有**。所以问题不只是「两个探针的见证太弱」，而是三分之一的声明
根本没有任何测量。

**决定性演示**：把 `ServeError::Bind` 的 Display 里 `{address}` 删掉（正是这条
divergence 声称我们比上游多给的东西），重建 subject 二进制，跑**改造前**的 harness：

    test result: ok. 9 passed; 0 failed; 0 ignored

全绿。声明的诊断丢了地址，整个 parity target 一声不响，因为两边仍然都 exit 1。

> **一个见证如果不读它声称保护的那个东西，它就不保护那个东西。**
> 而「声明覆盖 N 个面」和「表里有 N 个探针」是两个独立的事实，必须各自有门去钉。

修法：`Witness::DocumentedDiagnostics { oracle_form, subject_form }`，每个探针查四件事——
两边仍拒绝、oracle 带上游的形态、subject 带点名的形态、**且任一方都不带对方的形态**。
第四条是关键：只查前者，我们退化成同样不透明会通过；只查后者，上游哪天开始点名了不会
被发现（而那正是该关掉这条 entry 的信号）。另加 `DIAGNOSTICS_SURFACES` 把三个 argv 按名
冻结，并用一个**不跑任何进程**的测试比对——这样在没装 oracle 的机器上，降级见证也照样失败。

顺带发现的第二处：entry 自己的收尾散文还写着「exempts those two stderr streams …
still asserting both sides refuse」，两处都已不成立。docs 门在我改了 toml 之后立刻红了，
证明那段散文是有门守的，不是死文字。

## [2026-08-10] todo 143：antigravity 在 `models` 上是**结构性**不可见，不只是断言写错了

F2-B1 说「断言 provider `google` 分不清夹具和 antigravity」。我先量了再修，结论比
报告更强：**antigravity 对 `models` 这个 surface 的贡献是 0 字节**。

`models --verbose`，同一份 `env -i`，唯一差别是插件列表里有没有 antigravity：
两次都是 2944 行，`diff` 空，stderr 也都空。

读源码给出原因，不是猜的：
- `crates/oc-cli/src/cmd/models.rs:92-127` 只 dispatch `Config` 和 `Provider` 两个 hook。
- kiro-auth 两个都注册（`dist/plugin.js:68` config、`:424` provider），所以它的模型
  真的会进 plain stdout —— 老测试的 kiro 那一半是**真守卫**。
- antigravity 只注册 `event`/`tool`/`auth`（`dist/src/plugin.js:1138-1143`）。它在
  `models` 上按构造就是惰性的。

**所以计划给的路线 (a)「从初始 catalog 去掉 google」是做不到的**——去掉之后 google 行
直接消失，不会因为 antigravity 跑了而回来（也实测了 oauth 型 auth entry，同样没有）。
`models` 永远只显示 catalog 声明过的 provider。这是我对 todo 前提的一处修正。

修法：证据放到 antigravity **真正动手**的地方——它为 provider `google` 注册的 auth
resource，断言的是它自己的方法标签 `OAuth with Google (Antigravity)`，并在测试里同时
断言这个字符串不在夹具 catalog 文本里。加负控（只装 kiro）时先断言 kiro 自己的 hook
在，避免「因为什么都没加载所以证明了缺席」这种空洞负控。

**可迁移的判据**：当「加/减一个成员什么都不会失败」时，先别急着改断言——先量这个成员
到底在这个 surface 上有没有任何可观测效果。如果答案是「没有」，那问题不是断言写弱了，
而是**断言选错了 surface**；此时改断言而不换 surface，只会造出第二个假见证。

## [2026-08-15] C-2：共享 make target 是 CI 与本地一致性的资产，也是它唯一的负债

`ci.yml` 的设计原则是「CI 调用开发者本地跑的同一个 make target」，所以"本地绿"和"CI 绿"
不会漂成两个意思。C-2 暴露的是这个原则的**成本**：make target 的默认值也一起被继承了，
包括那些只对本地成立的默认值。

`OFFLINE ?= --offline`（`Makefile:32`）在本地是对的选择，在冷 CI runner 上是错的。
症状极具误导性 —— `cargo build … --offline` 报的是 `no matching package named 'async-trait'
found`，读起来像依赖声明坏了，而 `async-trait` 明明在 `Cargo.lock` 里。真正的原因是
registry 是空的，离线 resolve 无从下手。

**可迁移的判据**：当 CI 复用本地 make/just/npm target 时，把每个 target 的默认变量单独
过一遍，问「这个默认值假设了机器上已经有什么」。`--offline` 假设 registry 已填充，
`command -v <tool>` 假设工具已装 —— 这两条正好就是 C-2 的两个缺陷。
CI 侧的修法不是改默认值（那会伤害本地体验），而是**在调用 target 之前把它假设的前置条件
准备好**：一步 `cargo fetch --locked`，一步装工具。

### 一个掩盖机制值得单独记：在线的那一步会让离线的问题晚两步才炸

`ci.yml:69` 的 `cargo metadata --locked --format-version 1` 是**故意在线**的（这样锁文件
的可复现性独立于代码问题被验证）。它成功了，于是日志开头是绿的，让人以为依赖解析没问题；
真正的离线失败发生在两步之后的 `make lint` / `make release`。
learnings.md:3526 记过同源的另一面：`cargo metadata --locked --offline` 会因 Windows-only
传递依赖失败。两条合起来给出一条规则：**`cargo fetch` 不要带 `--target`** —— 不带时它取
所有 target 的依赖，一次把这两个坑都填掉。

## [2026-08-15] C-2：注释与代码矛盾时，先去读第三方 action 的源码再决定改哪一边

`ci.yml:55-61` 的注释说「不要给 `toolchain:` 输入，rust-toolchain.toml 已经钉了 1.96.0，
在这里写 channel 会静默覆盖它」，而第 61 行写着 `toolchain: stable`。看起来该按注释删掉
那个 input —— **实测不行**。

读 `dtolnay/rust-toolchain@6c977a6` 的 `action.yml`：
- `:35-39` 空值时 `echo "'toolchain' is a required input" >&2; exit 1`。它自己不读
  `rust-toolchain.toml`（GitHub 不强制 `required: true`，所以 action 自己校验）。
- `:96-98` 它做的全部事情是 `rustup toolchain install <ver> --profile minimal` 加
  `rustup default <ver>`（后者还带 `continue-on-error: true`）。

所以注释里"会静默覆盖 toml"这句本身是错的：**toolchain 文件的优先级高于 `rustup default`**，
写 `stable` 不会让构建用 stable 编译，只会让 rustup 多装一条 toolchain，并且把
rustfmt/clippy 这两个 component 装到那条**用不上**的 toolchain 上。真正的失败模式不是
"版本被覆盖"，而是"component 装错了 toolchain"。

结论：改代码为 `toolchain: "1.96.0"`（与 toml 同值，4 处），注释改成陈述实测机制。
代价是多了一处重复字面量，用一行交叉引用注释标出来，因为**没有任何测试绑定这两处**。

**可迁移的判据**：注释与代码矛盾时，两边都可能是错的。判断哪边错的唯一办法是读被调用方
的实现（第三方 action 就是 `git clone` 它的 tag 然后读 `action.yml`），而不是相信注释里
对第三方行为的转述 —— 那种转述写下来时可能就是错的，也可能只是过期了。

## [2026-08-15] C-2：`needs` 门禁要放行"没跑"，就必须按 job 名 + 按结果值双重枚举

`ci-success` 的设计是严格门：任何非 `success` 都红，因为 `skipped` / `cancelled` 也能让
一次没跑完测试的 run 挂上绿勾。现在需要一个例外（`windows` 因为账户没有 GitHub-hosted
额度而永远拿不到 runner），但**"忽略 skipped"这个宽泛写法会把门重新打开**：`cancel-in-progress`
下一个被新 push 取消的 job、或一个因为 `if:` 写错而整体跳过的 test job，都会变成绿。

写法是 shell 函数枚举白名单，判定同时约束 job 名和结果值：

    may_be_skipped() { case "$1" in windows) return 0 ;; *) return 1 ;; esac; }
    …
    elif [ "$result" = "skipped" ] && may_be_skipped "$job"; then

三种情形实测（把 `run:` 脚本抽出来喂 `NEEDS_JSON` 直接跑）：
`windows=skipped` → exit 0；`windows=failure` → exit 1；`test=skipped` → exit 1。
**这个抽出来单跑的手法值得复用**：`ci-success` 的逻辑只能在真实 run 里被观察到，而真实 run
每次要几十分钟；用 `python3 -c "yaml.safe_load(...)['jobs']['ci-success']['steps'][0]['run']"`
导出脚本再 `bash -n` + 喂几组 `NEEDS_JSON`，把一个只能在 CI 里验证的门变成本地秒级可测。

顺带：`continue-on-error: true` 在这里是错的工具。它让 job 失败也不影响 run 结论，于是
"没跑"和"跑了但失败"被压成同一个结果 —— 而这个区分正是这次要保住的东西。

## [2026-08-15] C-2：GitHub 的 `workflow_job` 投递**不重试**，aws-cn 的 CodeBuild 端点又不稳 —— 组合成「job 永久卡 queued」

这是本次最有复用价值的运维发现，症状与继承经验里那个「wedged queued run」几乎一样，
但**根因完全不同**，按老经验去查会走错方向。

实测（hook id=666011311 的投递记录）：
    08:07:38.008 workflow_job/queued code=502 failed to connect to host
    08:07:38.799 workflow_job/queued code=200
    08:07:45.789 workflow_job/queued code=200
    08:09:18.873 workflow_job/in_progress code=500 … context deadline exceeded
    错误文本里 GitHub 自己写着：**giving up after 1 attempt(s)**

那条 502 就是 `Test` job 的 queued 事件。CodeBuild 因此从未得知这个 job，
**永远不会为它派发 runner**，job 就无限停在 `queued`、`runner_name` 为空、`steps=0`。
同一个 run 里另外两个 job 拿到了真实 runner UUID 并正常跑完 —— 所以
「CodeBuild 接线坏了」这个假设当场被排除。

**判据（与配额耗尽、与标签路由都要区分开）**
- 同 run 内有别的 CodeBuild job 拿到了真实 runner → 接线是好的，不要去查 IAM / 连接 / 标签。
- 去读投递记录的 `status_code`，而不是只看 job 状态：非 200 的 `queued` 事件就是答案。
- 标签 superset 路由问题会让**标签少的**那个 job 挨饿，但本仓库每个 job 都是 2 个标签且
  第二个唯一（`release_surface.rs` 的 `every_codebuild_job_has_a_unique_label_set` 钉死 11 组），
  所以那条也排除。
- CodeBuild 的 `webhook.lastTriggeredAt` 在 aws-cn 上**恒为 null**，即使派发明明成功。
  继承的 skill 把这个字段当作「唯一干净的判据」，**在 aws-cn 上不成立**，不要用它下结论。

**失败是突发相关的，不是随机的**。两轮实测：
    第 1 轮  6 秒内 3 个 queued 事件 → 1 成功 2 失败（502、500）
    第 2 轮  单独一个 queued 事件（`gh run rerun --job <id>`）→ 200，runner 立刻派发
所以恢复手法是**逐个、错开地重发 queued 事件**，而不是重跑整个 run（重跑整个 run 会再造一次
突发，大概率再失败）。

**可用的恢复路径（按优先级）**
1. `gh run cancel <run>` → 等 run 变 `completed` → `gh run rerun <run> --job <job-id>`，
   一次只重跑一个 job。这会为该 job 单独发一个 queued 事件，命中率最高。
   （注意 `rerun` 要求 run 已 completed，而卡住的 run 不会自己 completed，所以必须先 cancel。）
2. 空提交重新触发。代价是一次性再发 N 个突发事件，且污染历史。
3. **重发投递（`POST /repos/{o}/{r}/hooks/{id}/deliveries/{d}/attempts`）在本机 token 上是 404**，
   走不通 —— 别浪费时间。

顺带一个反直觉的好消息：这次 `gh run cancel` **干净地**把 run 收成了 `cancelled`
（继承经验里那次是怎么都取消不掉、只能 bump concurrency generation）。所以先试 cancel，
不要一上来就 bump `g2` → `g3`。

## [2026-08-15] C-2：「CI 从未执行过这一步」会把缺陷**按层叠**藏起来，一层修完才看见下一层

本任务的形状值得记下来，因为它会重复出现在任何「CI 第一次真正跑起来」的场景里。

同一个 job 里的步骤是串行短路的，所以**第一步的缺陷会把它后面所有步骤的缺陷一起藏住**。
实测的层数：

    Test job     步骤 6 Format check   ← 缺陷 A（oxfmt 没装）挡住了下面全部
                 步骤 10 Clippy        ← 缺陷 B（--offline 无 registry），A 修完才暴露
                 步骤 11 Test suite    ← 缺陷 F（oracle tree 在仓库之外），B 修完才暴露
    Artifact job 步骤 5 Package        ← 缺陷 B，修完后才暴露缺陷 E（smoke 请求形状过期）

**所以「修完 N 个缺陷」不等于「CI 会绿」**，估工时不能按已知缺陷数线性算。
更该做的是：修完一层就**立刻把那一层的成功当作独立证据固定下来**（本次是逐步骤记录
`Format check: success`、`Clippy: success`），这样即便下游还红，上游的修复也是已证明的，
不会因为 job 整体 failure 而被误判为没做成。

**另一个可迁移的判据**：缺陷 E 和 F 有共同结构 ——
「一份验证资产（断言 / fixture / oracle 路径）与它验证的对象各自演进，而唯一会发现分歧的
执行路径从未运行」。E 里是 driver 落后运行时 8 天；F 里是测试依赖一个只存在于某台机器上的
目录。**凡是一条门禁长期没有真正执行过，就不要假定它此刻还是正确的**；它更可能已经腐坏，
而且腐坏方向通常是「本地能过」，因为本地是它唯一被运行过的地方。

## [2026-08-15] C-3：smoke 的 wire-shape 回归要同时钉住「哪些请求不能有工具」和「哪些请求必须有工具」

Defect E 的修复不是把 `captured[0]` 改成 `captured[1]` 就结束。权威形状是：

    request 0  title prelude       tools=[]，不能有 role:tool
    request 1  首次真实 turn       必须包含 bash/read/glob/grep
    request 2  tool-result followup 必须包含同一注册表，且必须有 role:tool

只检查 request 1 会留下两个空洞：title agent 若错误拿到工具不会失败；第二次 turn 若丢失注册表
也不会失败。`oc-smoke` 现在对三个位置分别断言，并用已有的 `openai-chat/streams-text`
recording 服务 prelude，再接原来的 tool-loop recording；不需要也不允许手写或重录 cassette。
以后 runtime wire shape 再变化，先同步这个三位置回归测试，CI smoke 才不会在数天后成为第一个
发现者。

## [2026-08-15] C-3：可选外部 fixture 的 discovery 必须有仓库边界、正向控制和可见 skip

Defect F 的安全形状是三件事缺一不可：

1. discovery 只拼 `<repository-root>/<pinned-relative-path>`，绝不对 `ancestors()` 搜索；
2. 单元测试同时证明仓库外 fixture 被拒绝、仓库内 fixture 会被接受，避免把「永远返回 None」
   误当成边界修复；
3. fixture 缺失时打印 `SKIP:`、钉住的 SDK 版本和精确期望路径，然后 return，不能 panic，
   也不能无输出地绿。

本仓库没有 Node package manifest、lockfile 或 install gate，因此测试时联网安装 SDK 不是 hermetic
acquisition，而是引入一个新的网络与版本漂移来源。保留 repository-local fixture seam：将来若真正
vendor 钉住的 0.21.0 SDK，live test 会自动恢复执行，且 harness 仍会断言 package version。

### C-3 追加更正：精简 smoke 根没有独立 title cassette

上段提到 `openai-chat/streams-text` 是 `tool_turn.rs` 在完整 oracle tree 中使用的权威 recording，
但它没有提交到 `packaging/smoke/cassettes`。release smoke 的 hermetic 解法是复用同一个已提交
tool-loop cassette 的第 2 个 interaction：它是真实 provider 的文本 continuation，先作为 title
prelude 响应，再完整重放 cassette 的 tool-call + continuation。三份响应的 provenance 都是
`Recorded`，`authored_scenarios()` 仍为空；没有新增、手写或重录 fixture。

## [2026-08-15] C-3 追加：differential 缺 oracle 必须走中央 resolver，不能直接 `Oracle::discover`

PR run `31878406895` 的 `Test` job 已实际拿到 CodeBuild runner，但
`oc-catalog/tests/agent_differential.rs` 的 6 个真实 oracle 用例在干净 runner 上因
`BinaryNotFound` 失败。根因不是产品逻辑，而是该文件绕过了仓库已有的
`pinned_oracle_or_skip` 合同。

正确形状是：纯 fixture 守卫继续无条件执行；真正依赖 `opencode` 的 differential 通过中央
resolver 获取钉住版本。候选完全缺失时逐项打印 `SKIPPED` 和未覆盖内容后返回；用户显式指定错误
路径或发现了版本不一致时仍 hard fail，不能把配置错误降级成 skip。结构测试清单也必须纳入该
文件，防止以后退回直接 discovery。

## [2026-08-15] C-3 追加：中央 oracle 路由清单必须覆盖每一种 differential，不只是首个失败文件

PR run `31879788919` attempt 2 的 `Test` job 已通过 checkout、toolchain、lockfile、format 和
Clippy，随后在完整测试套件中暴露 `skill_differential.rs` 仍有两个直接 `Oracle::discover`。
这说明修复 `agent_differential.rs` 的 6 个用例只能消掉当时可见的第一层，不能证明同类调用已穷尽。

可持续的修复不是再加一处分散 skip，而是把每个真实二进制 differential 都登记进
`ROUTED_DIFFERENTIALS`，由结构测试强制其引用 `pinned_oracle`。运行时合同统一为三态：找到且版本
正确就真实比较；完全找不到就明确列出未覆盖内容后 skip；显式错误 override 或版本不一致就 hard
fail。Defect H 同时验证了这三条路径，因此缺少 oracle 的干净 CI 不再误红，错误配置也不会被误绿。

另一个 CI 取证经验：不要只记录 job 的总 conclusion。该 attempt 的 `Artifact smoke (host)` 已完整
成功，直接证明 Defect E/F；`Test` 也在失败前证明 format 与 Clippy 成功；`Supply chain` 则进入
`cargo deny` 后因必需 Test 失败而被取消。逐步骤结论能区分产品缺陷、后续取消和网络噪声。
## [2026-08-15] Atlas 复核：webhook 投递失败率的总量测量

C-2 记录了突发相关性（第 1 轮 3 事件 → 2 失败），我补一次总量测量，因为
「偶发」和「28% 恒定失败」对是否能依赖这条 CI 是两个不同的结论。

    gh api "repos/sunerpy/zuno/hooks/666011311/deliveries?per_page=100" \
      --jq '[.[]|select(.event=="workflow_job")] | group_by(.status_code)[]
            | "code=\(.[0].status_code): \(length)"'

    workflow_job 投递总数 61
      code=200  44 次
      code=500  11 次
      code=502   6 次        → 失败 17/61 ≈ 28%

只看决定「job 能否拿到 runner」的 `queued`：**5/21 ≈ 24% 永久丢失**。
每个丢失的 `queued` 等于一个永远停在 0 步骤的 job，因为 GitHub 不重试
（投递错误文本里它自己写着 `giving up after 1 attempt(s)`）。

**这不是配置错误，无平台级修法。** 两处查证：
- 继承的 `github-actions-on-codebuild` skill 记了 4 个坑，**没有这一条**；
  它的「唯一干净判据 `lastTriggeredAt`」在 aws-cn 上也不成立（C-2 已记）。
- AWS 官方 `action-runner-troubleshoot-webhook.html` 对 hanging job 只给两个
  方向：读 Response 日志、标签路由。本项目 11 组标签集各自唯一
  （`release_surface.rs:870` 钉死），**已排除**路由成因。

结论：aws-cn 分区的 GitHub→CodeBuild 投递不可靠是**固有属性**，只能靠 C-2 记的
逐个错开重发来运维。任何「CI 绿了就等于稳定」的判断在此分区都不成立；判断
CI 是否健康要看 job 是否**拿到 runner**，不能只看 run 的 conclusion。

## [2026-08-15] C-4：密闭子进程要 JS runtime 时，唯一诚实的来源是生产发现函数

`.env_clear()` + 显式 `PATH` 是对的（不让开发机环境漏进被测进程），错的是把
`PATH` 写成常量。凡子进程要加载 JS 插件，`PATH` 必须由
`oc_plugin::discover_runtime`（`crates/oc-plugin/src/js/runtime.rs:141`）在**当前
主机**上解析出来，再把它所在的**单个目录**前置到系统基座。这样既保持密闭
（交出一个目录，不是 ambient PATH），又与子进程自己的发现逻辑同源。

发现顺序值得记住，写测试时别绕过它：
`PATH` → `BUN_INSTALL/bin/bun` → `$MISE_DATA_DIR/installs/<rt>/latest/bin/<rt>`
→ `$XDG_DATA_HOME/mise/...` → `$HOME/.local/share/mise/...`；`bun` 优先于 `node`。
本机 `node`/`bun` 只存在于 mise，且 mise shim 在 `env -i` 下也能正常输出版本号
（配置不受信的告警只影响 stderr，不影响 exec）。

判据：一个「插件测试」在没有 runtime 的机器上必须可见 skip 或点名 panic。
若它只是断言失败在空 stdout/stderr 上，那条测试等于没测插件——本仓库已连续
出现三次同类（C-3 的 `ancestors()` 越界、C-3 的 oracle 直连、C-4 的写死 PATH）。

顺带：本仓库 worktree 放在 `/config/workspace/ProdDir/AI/oc-wt/<n>` 时，
`oc_testkit::oracle::locate_source_tree()` 沿祖先找 `<ancestor>/opencode` 仍能命中
`/config/workspace/ProdDir/AI/opencode`，所以 cassette 类测试在 worktree 里是**真跑**
而非 skip；换到别处（如 /tmp）就会整片转成 skip，读测试数量时要注意这点差异。

## 2026-08-15 — 移除 opencode oracle 差分测试套件（task C1）

**背景。** Zuno 已是独立项目，用户明确「跟 opencode 没什么关系了，无需考虑兼容」「兼容测试可以移除」。
只保留插件 ABI。6 个 differential/compat 文件共 59 个测试被逐个判定。

**判定标准可操作化。** 关键不是文件名，而是测试的**主语**：
- 主语是「Zuno 是否与 opencode 一致」→ 删
- 主语是「Zuno 是否正确工作」→ 留（即使文件名叫 `differential.rs`）

用脚本按 `#[test]` 块切分、逐块统计 oracle 符号引用数，比整文件 grep 精确得多。
结果：59 个测试里 56 个是 oracle 比对，3 个不是。但 `oc-cli/tests/differential.rs`
是例外——它 21 个测试里有 12 个只是**断言目标**指向 oracle（"...matches oracle"），
实际驱动的是真实 Zuno CLI 行为，这 12 个被改写成以 Zuno 为主语后全部保留。
**教训：逐块 grep 计数只能定位嫌疑，不能替代读测试体判断主语。**

**grep "oracle" 在本仓库几乎无信号。** 全仓库约 300 处 `oracle` 出现在
doc comment 里引用上游 TypeScript 路径（`//! Oracle: packages/core/src/v1/config/...`）。
同理 grep `/config/` 有 127 个命中，其中 ~90 个是 `packages/opencode/src/config/config.ts`
这类散文。**要定位真实代码消费者，必须 grep 具体符号**
（`Oracle::` / `pinned_oracle` / `Subject::` / `CompatReport` / `DivergenceList`），
再用 `grep -v '^\S*: *//'` 排除注释行。

**oracle 基础设施不能删——数量级远超任务清单。** 删掉 6 个文件后仍有 22 个测试文件
跨 9 个 crate 在代码层消费 `oc-testkit/src/{oracle,subject,compat_report,divergence}.rs`，
包括 `oc-cli/tests/docs.rs`（文档生成器）和 `oc-server/tests/compat_v1.rs`（插件 ABI 门）。
删基础设施等于连带删这 22 个文件，远超范围。
**教训：判断「基础设施是否孤立」要按符号数完整枚举消费者，不能凭直觉。**

**审计清单型 guard 会正确报警，不要绕过。**
`oc-testkit/tests/no_pinned_oracle_paths.rs` 的 `ROUTED_DIFFERENTIALS` 是手工维护的
文件清单，其 doc 明写 "Renaming or removing one of these fails here, loudly, with
the reason"。它列了我删的 4 个文件，于是失败——这是 guard 按设计工作。
正确做法：删掉那 4 项，并在 const 的 doc 里**记录删了哪四个、为什么**。
静默缩小审计清单恰恰是这个 guard 存在的目的所要防止的事。

**删除会让别处的"事实声明"变成谎言，必须一并修。**
同一文件模块 doc 声称保留 `.omo/fixtures/oracle-openapi-1.18.18.json` 是
"because compat_suite.rs refetches /doc ... and compares the bytes"。我删了那个
re-deriver。但 fixture 本身有 5 个存活消费者，含 `oc-plugin-sdk/build.rs`
（**构建期**读取以生成插件 SDK）和 `oc-server/tests/compat_v1.rs`（`include_str!`）。
所以 fixture 必须留，而它的**理由**必须改写为「插件 ABI 契约的冻结输入，而非再推导物」。
**教训：删一个文件后，grep 全仓库对该文件的散文引用，逐条判断哪些从「历史」变成了「假声明」。
历史可以留（"曾经由 X 记录"），当下的机制声明必须改。**

**fixture 的消费者要独立确认再决定去留。**
`oc-config/tests/fixtures/user-config.json` 看起来是 oracle 差分的专属输入，实际还有
`oc-config/tests/legacy.rs:18` 和 `oc-config/src/schema/tests.rs:855` 当作真实世界
config 解析语料。删测试文件时保留了 fixture。

**测试数变化要能精确对账。** 3526 → 3481（-45），217 → 214 suites（-3）。
删 59（5+6+8+3+21+16）+ 加 14（1+12+1）= -45，**逐个吻合**，证明零测试被意外破坏。
**教训：预先统计每个待删文件的测试数，事后做减法对账——这是唯一能区分
「删掉的测试」和「被我改坏的测试」的方法。**

**`cargo test` 默认 fail-fast 会截断证据。** 第一次全量跑在 144/217 suites 处中止，
因为 `oc-testkit oracle::tests::a_binary_reporting_another_release_is_named_and_refused`
以 `ExecutableFileBusy`（errno 26, "Text file busy"）失败——写完临时可执行文件后
立即 exec 的并发竞态，与本次改动无关（该文件我没碰）。单独跑通过。
**教训：验证删除类改动必须用 `--no-fail-fast`，否则一个偶发 flake 会把后面 70 个
suite 的结果全部隐藏，看起来像「改坏了很多东西」。**

**`zuno run` 没有 `--prompt` 参数**，message 是位置参数：
`zuno run --model <M> "消息"`。用 `--prompt` 会报
`error: unexpected argument '--prompt' found / tip: a similar argument exists: '--port'`。
任务书里给的命令需要改写才能执行。

**`make smoke-artifact` 必须 unset `CARGO_TARGET_DIR`**（Makefile:28 硬编码
`TARGET_DIR := target`）。另外用 `setsid nohup make ... > log 2>&1 &` 时，
若把 `unset VAR &&` 放在同一个后台链的开头，日志文件可能不生成——
改成 `setsid nohup make ... >log 2>&1 </dev/null &` 并 `sleep 5` 确认文件存在再轮询。

**shell 工具 120s 超时会杀掉 `sleep 180`**，但不会杀掉 `setsid nohup` 的子进程。
轮询长任务用 `sleep 110` + `ps -p <pid>` 判活，单次不要超过 115s。

## 2026-08-15 补 — 用「复现 CI 环境」代替「一次 CI 往返修一个文件」

**教训的由来。** 首轮删掉 6 个 oracle 差分后推送，CI 仍失败，暴露第 7 个文件
`oc-config/tests/discovery_differential.rs`。它在本机不可见——因为本机装了
`opencode`；文件名不含 `compat`；且它所在的 crate（oc-config）看起来「已处理过」。
若继续「推一次、CI 报一个、修一个」，每轮约 15 分钟，还会漏。

**正确做法：本地复现 runner 的 PATH，一次跑出全部同类失败。**
```
PATH="/config/.local/share/mise/installs/node/26.5.0/bin:/config/.cargo/bin:\
/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin" \
  CARGO_TARGET_DIR=<独立目录> cargo test --workspace --no-fail-fast
```
关键是 PATH 要**精确**模拟 runner，不能过度裁剪。第一次我用了极简 PATH，
把 `sqlite3` / `bun` / `node` 也剥掉了，得到 14 个失败，其中 13 个是
**沙箱假象**（`sqlite3: CannotFindBinaryPath`、`bun (via PATH)` 找不到），
只有 1 个是真的。**教训：沙箱比目标环境更贫瘠时，产出的失败清单不可信；
要先确认 runner 上有什么（本例 CI 日志里的 PATH 就列了 node，
且后来 perf::database 在 CI 上通过反证了 runner 自带 sqlite3）。**

本机这些工具全在 mise 下（`/config/.local/share/mise/installs/<tool>/<ver>/bin`），
`which` 只看到 shim（`/config/.local/share/mise/shims/x` → `/config/.local/bin/mise`）。
用 `mise which <tool>` 拿真实路径来拼装沙箱 PATH。

**「缺失即必须显式声明」的门在前提消失后就是纯负担。**
`oracle.rs` 的 `ENV_ALLOW_MISSING_ORACLE`（`OC_TESTKIT_ALLOW_MISSING_ORACLE`）
让「机器上没有 opencode」在单元测试里成为**失败**，除非显式设置环境变量声明
「我故意没测」。这在「本项目全部前提是与 opencode 对等」时是合理的郑重声明；
前提废止后，它只意味着**任何没装过 opencode 的机器都跑不过这个单元测试**。
改成普通跳过，与同文件 `pinned_oracle_or_skip` 既有契约一致——后者正是
存活的 8 个比对套件在 CI 上干净跳过、没有失败的原因。

**判断「某测试是否受我改动影响」要用 git 而不是直觉。**
`git diff --name-only <base>..HEAD | grep <crate>` 为空即证明未触碰。
本例用它证明 layer J（oc-lsp rust-analyzer）与本任务无关。

**skip guard 只覆盖「不存在」，不覆盖「存在但不可用」。**
`live_servers.rs` 用 `command_path("rust-analyzer")` 判断存在即跳过，
但 runner 上 rust-analyzer **存在**、启动后却索引不了，于是
`Unavailable { server_id: "rust" }` 变成硬失败。
**教训：外部依赖的 guard 要区分三态——缺失（跳过）、就绪（断言）、
启动但未就绪（也应跳过并打印未测内容）。只做二态判断会在 CI 上炸。**

**`pkill -f` 会匹配到自己的命令串（本次踩中）。**
`pkill -TERM -f 'CARGO_TARGET_DIR=/tmp/opencode/...'` 把发起它的 shell 一起杀了，
命令挂死到超时。同理 `pgrep -f <pat>` 也会把自己列出来，导致误判「还在跑」。
**只能按显式 PID kill**；判活用 `ps -C cargo --no-headers` 或 `ps -o pid -p <PID>`。
本次共清理 3 个残留 zuno 进程：2716、2724（本次中断产生），
以及并行任务 r4 遗留的 3500740（`--pure --prompt route probe --auto`，
已 99.6% CPU 跑满 1h26m）。

**双 CARGO_TARGET_DIR 会吃光磁盘。** 为复现 CI 又开了
`/tmp/opencode/c1b-nooracle`，单独占 14G，磁盘从 28G 掉到 14G（96%）。
用完立刻 `rm -rf` 回收。**验证前后都要 `df -h /`**，
本机磁盘耗尽已两次伪装成 rustc/clippy 报错。

## rustup 代理会让 `which(binary)` 撒谎（task-c5, oc-lsp live 测试）

`~/.cargo/bin/rust-analyzer` 是 rustup 的 **proxy 符号链接**，只要装了 rustup 它就存在，
与 `rust-analyzer` component 是否安装无关。component 缺失时执行该 proxy 会立刻退出：

```
$ env -i PATH=/config/.cargo/bin:/usr/bin:/bin RUSTUP_TOOLCHAIN=1.90.0 rust-analyzer --version
error: Unknown binary 'rust-analyzer' in official toolchain '1.90.0-x86_64-unknown-linux-gnu'.
exit=1
```

所以 `which::which("rust-analyzer").ok()` 形式的守卫在 CI 上会**通过**，随后进程秒退。
本机之所以看不出来，是因为 mise 装了真 rust-analyzer 并在 rustup fallback 链里兜底
（`info: falling back to "/config/.local/share/mise/shims/rust-analyzer"`，exit 0）——
即“本机与 CI 表现相反”的又一例。判断一个 CLI 是否真的可用，必须**执行**它
（`--version` + 断言 exit status），不能只做 PATH 查找。

CI（`dtolnay/rust-toolchain` toolchain 1.96.0、components 仅 `rustfmt,clippy`）上
的完整链路：proxy 秒退 → `ManagerError::Initialize{ClientError::Closed}` →
supervisor 用尽 `RestartPolicy::default()` 的 6 次 launch（退避 100/200/400/800/1600ms
≈3.1s）→ `publish_stopped` → `ManagerError::Unavailable`。CI 日志时间戳
19:55:05.5 起、19:55:09.26 报错，3.76s，与该退避总和吻合，证明**不是** 50s 超时。

## oc-lsp 的 readiness 语义与 `Unavailable` 的信息损失

`crates/oc-lsp/src/manager.rs` 里 `Unavailable{server_id}` 只有三个构造点，全在
`wait_for_client`：supervisor 已 `Stopped`、watch channel 关闭、或 caller 的
`CLIENT_READY_TIMEOUT`（50s）耗尽。真正的根因（`Registry::NotInstalled`、`Spawn`、
`MissingPipe`、`Initialize{Framing|Protocol|Closed|Timeout}`）都被 supervisor 用
`tracing::warn!` 记录后**折叠**掉，公开 API 只剩 `Unavailable`。超时常量：
`INITIALIZE_TIMEOUT=45s`、`REQUEST_TIMEOUT=10s`、`DIAGNOSTICS_TIMEOUT=20s`、
`CLIENT_READY_TIMEOUT=50s`；`$/progress`（含 rust-analyzer 的 `rustAnalyzer/Indexing`）
被 `dispatch_message` 静默忽略，不参与 readiness。`Manager::diagnostics` 对
diagnostics 超时不报错，改为回落到缓存快照——即“等不到诊断”会表现为**成功但空**。

结论：`Unavailable` 无法在 API 层区分“环境起不来”与“Zuno 握手有 bug”。live 测试
选择把 `Unavailable` 当跳过，是因为握手本身另有 hermetic 覆盖
（`manager.rs`/`client.rs` 里的 Python stub server 单测），逃生口有界。

## live 外部工具测试的三态守卫写法

`crates/oc-lsp/tests/live_rust_analyzer.rs` 现在建模三态而非两态：
1. PATH 上没有 → 普通跳过；
2. 解析到了但跑不起来 → 跳过，但必须喊出 server id + 底层错误（`--version` 的
   exit status 与 stderr 原文）；
3. 起来了 → 断言，答错就红。

第 2 态又分两条路：秒退型被 `--version` 探针抓住（0.00s），能答 `--version` 但不说
LSP 的被 `Err(error @ ManagerError::Unavailable{..})` 分支抓住（55s，= 50s ready
预算 + shutdown）。两条路都不能省：只有探针会漏掉后者，只有 `Unavailable` 分支会
让前者也等 50s 且不打印 stderr。

## [2026-08-15] R4 — a rename that lands in some copies of a constant and not others

`PROJECT_DIRECTORY` existed three times: `oc-agent/src/plan_file.rs`, `oc-goal/src/projection.rs`,
and `oc-paths/src/config_chain.rs` (as `PROJECT_CONFIG_DIRECTORY`). The `.zuno` rename was applied to
all three by hand, and the R3 inventory then reported two of them as "upstream-artifact-reference"
because a classifier catch-all filed them by filename rather than by role. Both halves of that are
the same defect shape: **a value duplicated across crates has no single place to be right**, so
neither a human edit nor an automated audit can be complete by construction. Collapsed to one
definition in `oc-paths`, with the other two as `pub use` re-exports so no caller changed.

The generalisable rule: when a constant names something that appears on disk, the number of
definitions is part of the risk. Three copies of a path name is not a style problem, it is a
guarantee that some future rename will be partial and green.

## [2026-08-15] R4 — `Ok(None)` is the dangerous return type for a renamed on-disk path

Renaming a config *directory* is cheap: discovery either finds it or does not. Renaming a directory
that holds **durable documents a human edits** is not, and the danger is concentrated in functions
whose "absent" case is a success value rather than an error:

- `oc-agent::read_plan` returned `Ok(None)` for a missing plan, which upstream's `reminders.ts:55,73`
  interprets as "no plan yet, tell the model to write one". With the plan sitting in the pre-rename
  `.opencode/plans/`, that writes a fresh plan over a document the user cannot see is being ignored.
- `oc-goal`'s `ingest` took `Ingest::Restored` and regenerated the projection from the database,
  discarding whatever the human had typed into the objective region of the legacy file — and the
  module's own docs promise that region is the user's to edit.

Both were silent, both destroyed user intent, and both were invisible to every existing test because
the tests only ever exercised trees where the legacy path did not exist. The fix is the same shape as
the existing `legacy_db_path` hard cut: **detection only**, no read, no copy, no merge, no fallback —
`oc_paths::unmigrated_project_path` returns `Some(old)` in exactly one of four states and the caller
turns that into a typed error naming both paths and the `mv` that resolves it.

Test the matrix, not the happy path: neither exists / only old / only new / both. The
"only new" and "both" cases are the ones that catch an over-eager detector that would start
erroring on healthy projects, and they are the cases a fix-shaped-like-a-guard usually forgets.

## [2026-08-15] R4 — a guard test that pins prose can pin the defect

`crates/oc-cli/tests/docs.rs` asserted the README contains the literal
「不会导入或恢复 opencode 会话」. That sentence was the thing being corrected, so the test failed the
moment the documentation became accurate — the guard had frozen a misleading claim in place and would
have made the correct wording look like a regression.

Two lessons. First, the failure was *useful*: the test forced the correction to be deliberate instead
of drive-by. Second, re-pinning must move in the direction of more falsifiability, never less. The
replacement requires **two** needles per readme — the independence claim AND the bounded-import fact
(`zuno session import` reads only Zuno's own export) — because either one alone reads as the opposite
claim: the independence sentence alone is what let readers conclude no import exists, and naming the
command alone would advertise adopting an opencode session. A comment records that deleting either
needle silently restores the defect, so the next person cannot quietly halve the assertion.

## [2026-08-15] R4 — 「待另行决定」是文档里的定时炸弹

上一轮我修 Finding 8 时，把 `oc-testkit/src/lib.rs` 那句失效的 drop-in 承诺改成了「差分套件是保留
的验证资产，保留至另行决定退役或重塑它们」。一天之内 main 就执行了那个决定，把整面 oracle 差分
删了——**我的句子以与它替换掉的那句完全相同的方式变成了错的**。

这不是运气差，是句式本身的问题。「X 保留至某决定作出」把一个尚未发生的外部事件编码进了断言，
而没有任何机制在事件发生时提醒你回来改。它读起来很谨慎、很诚实，实际上是自带过期时间且不会
报警的陈述。

可行的替代：只陈述当前可验证的状态（「差分已移除，留下的是 N 个文件仍在消费的共享脚手架」），
把待决事项放进计划或 issue 而不是放进源码文档；或者如果非要在代码里提，就让它可被测试钉住——
本仓库 `docs.rs` 的 `contains_all` 就是这个作用，它逼着文档与代码一起改。

判据很简单：**如果一句文档的真假取决于一件还没发生的事，它就不该出现在源码里。**

## [2026-08-15] R4 — 删除冲突标记时，宽松正则会吃掉正文

`.omo/notepads/*` 两侧都是追加，冲突必须两侧全留、只删标记行。直觉写法
`grep -rE '^(<{7}|={7}|>{7})'` 是错的：`^={7}` 匹配任何以 7 个等号开头的行，而本仓库的证据文件
大量使用 `======================...` 作为分隔横线，长度远超 7。宽松模式在三个证据文件里报出命中，
照它清理就会把横线删成空行。

安全做法三步：**严格锚定**（`^(<{7} HEAD$|={7}$|>{7} .*$)`，注意 `$` 让 `=======` 必须恰好 7 个）；
**先断言数量再动手**（本次断言恰为 3，且顺序为 open/separator/close，不符就停）；**按行号删除而非
按模式替换**，这样即使模式有漏也不会误伤正文。

还有一个容易漏的副作用：`=======` 那一行往往正占着两段之间的空行位置，删掉它两段就会贴在一起，
Markdown 不再解析为两节。删完要检查接缝，必要时补回空行。
## [2026-08-15] C-4：CI 的 Test job 有 node，所以 runtime guard 不会把门禁变成空跑

`.github/workflows/ci.yml` 的 `Test` job 在 `make test` 之前就跑
`actions/setup-node@v7 node-version: "22"`（本是为 oxfmt 装的）。副作用很重要：
`discover_runtime` 在 CI 上一定能在 PATH 找到 node，所以 plugin_models /
tool_turn / session_mutation 里「找不到 runtime 就可见 skip」的守卫在 CI 上
**不会触发**，这些测试在 CI 是真跑真断言。判断一个 runtime guard 会不会把门禁
悄悄变成空跑，要去看该 job 装了什么工具，而不是只看守卫代码本身。

## [2026-08-15] C-4：cargo test 的 fail-fast 会把后面所有套件藏起来

`cargo test --workspace`（`make test`）默认在第一个失败的 test binary 处停止，
不带 `--no-fail-fast`。所以一次 CI 红只能证明「第一个失败」，不能证明「只有这个
失败」。判断某套件在上一轮是否真的跑过，直接对旧 job 日志 grep
`Running tests/<name>` 的计数：计数 0 就是根本没跑到，而不是跑过且通过。
本轮据此证明 oc-config differential 是被 plugin_models 遮住的第 5 层，不是回归。

## [2026-08-15] C-4：磁盘写满的假故障有可识别特征

/ 到 100%（剩 1.2G）时 `cargo test --workspace` 被中途杀死，日志**在测试名中间
截断**且没有任何 `test result:` 汇总行——没有 rustc 报错，也没有 FAILED。
这与真实测试失败的形状不同（真失败必有 `test result: FAILED` 与 panic 文案）。
凡遇日志无汇总行地突然结束，先 `df -h /`。清掉数小时未被触碰的陈旧
`/tmp/opencode/<name>` 构建缓存即可恢复（构建缓存可重建，删除是可逆的）；
删之前用 `stat -c %y` 看 mtime，避开并行任务正在写的目录。

## [2026-08-15] C-4：pinned oracle 缺失与 live-config 缺失是两种不同性质的红

`BinaryNotFound { role: "oracle" }` 是**可移植性缺陷**：干净机器上没有真实
`opencode`，仓库已有既定修法 `pinned_oracle_or_skip` + 登记
`no_pinned_oracle_paths.rs` 中央清单（见 Defect G/H）。
而 `oc-config/tests/differential.rs:329` 读 `/config/.config/opencode/opencode.json`
的硬失败是**刻意设计**，源码与 panic 文案都写明「criterion-2 的机器特定输入
不可用时必须可见失败」。前者该修成 skip，后者改动等于变更 criterion 2 的取证
口径，属策略决定。看到 `/config/` 硬编码不要一律当 bug——先读它的 panic 文案和
所属验收准则。
## 2026-08-15 — R5：清单生成器必须允许分类失败

R3 的 943 行清单虽然写着 0 个未分类，但 `classify()` 的默认分支会把任何未知匹配
无条件归入 `upstream-artifact-reference`，摘要中的未分类数也写死为 0；因此它只能生成
绿色报告，不能发现新增残留。R5 将默认分支改为 `unclassified`，由实际行数据计算
`Classified` / `Unclassified`，完整写出 TSV 后在未分类数大于 0 时退出 1。分类规则只接受
明确的插件 ABI、历史引用、上游来源语义、上游标识符或精确资产路径，不再用宽兜底。

当前同一声明范围（区分大小写的 `crates/*/src/**/*` 小写 `opencode`）为 944 次：
813 个 upstream artefact/reference、67 个 plugin ABI、52 个 historical citation、
12 个 `zuno-owned-stale-identity`、0 个 unclassified。相对 R3 的 943 次净增 1：
`oc-testkit` 当前源码新增 2 次、移除 1 次。旧清单的 824 个 upstream 中，有 12 个其实是
Zuno 自有旧身份；按当前源码先净增 1，再重分类 12 个，得到 813。

临时探针在 `/tmp/opencode` 的隔离源树镜像中增加一行未知 `opencode`，未触碰真实源码：
总数 944 → 945、未分类 0 → 1、生成器退出码 1；临时目录随后删除。这证明动态计数和
失败门都不是只改了显示文本。

建议后续把此门迁移为类似 `crates/oc-testkit/tests/no_pinned_oracle_paths.rs` 的一等仓库
测试：收益是 `cargo test` / CI 自动阻止分类规则和源码清单漂移，不再依赖人工运行
`.omo` 脚本；成本是要把规则与例外迁入受维护的测试代码，并在每个合法上游标识新增时
显式更新规则。R5 不实施迁移，避免把证据修复扩大成仓库测试设计变更。

## 2026-08-15 — R5 续：静态 TSV 追不上源码，分类规则必须按行为判据而非文件名

清单重建到 `origin/main` = `a5dca38` 后的真实总数是 **852**：728 个
`upstream-artifact-reference`、69 个 `plugin-abi`、55 个 `historical-citation`、
**0** 个 `zuno-owned-stale-identity`、0 个 `unclassified`。声明范围不变，仍是区分大小写的
`crates/*/src/**/*` 小写 `opencode`。

### 944 → 852 的两个独立成因，用三次对照实验分离

同一份源树跑新旧两版生成器，同一版生成器跑新旧两份源树，就能把「源码变了」和
「规则变了」拆开，避免把两者混成一个不可解释的净差：

| 实验 | 生成器 | 源树 | 总数 | 分类 |
|---|---|---|---|---|
| A | 旧（7616994） | 旧（1121bf3） | 944 | 813 / 67 / 52 / 12 / 0 |
| B | 旧（7616994） | 新（a5dca38） | 852 | 730 / 69 / 52 / **1** / 0 |
| C | 新（本次） | 新（a5dca38） | 852 | 728 / 69 / 55 / **0** / 0 |

- A 精确复现已提交基线，说明基线可重算，不是手抄数字。
- **A → B 的 −92 全部是源码漂移**，与规则无关。逐文件归因后落在 13 个提交上（不是 8 个）：
  `aad2130 fix(identity)` −58（其中 `customize-opencode.md` 独占 −34）、
  `821ebc6 refactor(paths)` 合并 `aad2130` 共 −20、`39fca14 fix(db)` −14、
  另有 4 处跨提交小项；`config_chain.rs` +3 与 `oc-testkit/src/lib.rs` +3 是新增的旧路径
  探测机制和改正后的身份段落。**贡献最大的 `aad2130` 不在任务列出的提交清单里**，
  说明只按给定清单推断数字会算错。
- **B → C 总数不变，只有 3 行换类**，即规则调整没有偷偷扩大或缩小扫描口径。

### 关键发现：旧规则在当前 main 上仍误报 1 次，且误报的正是迁移诊断

实验 B 显示旧的 `{文件名: 手抄子串}` 映射在 `a5dca38` 上仍命中
`crates/oc-goal/src/projection.rs:892` —— 那是**指示用户把文件搬到 `.zuno/goal/` 的报错文案**。
把它标成「Zuno 自有旧身份」，下一个读清单的人就会去「修掉」这句诊断，从而恢复
`821ebc6` 刚刚消灭的静默覆盖：旧路径有文档而新路径没有时，模型会在用户看不见的
情况下另写一份。这不是假想风险，是实测到的既有误报。

### 判据：看这行代码「拿这个字符串做什么」，不看它在哪个文件

- 子句 A（写入）：项目目录族常量绑定到 `.opencode` 字面量，且常量名不含 `LEGACY`。
  `821ebc6` 把三份重复的 `PROJECT_DIRECTORY` 收敛成唯一的 `oc-paths::PROJECT_DIRECTORY`，
  所以守住定义点等于守住全部消费者；旧身份要复活只能重新引入一份本地常量副本，
  而这正是子句 A 匹配的形状——在任何文件里都匹配。
- 子句 B（声称）：未被撤回的 drop-in／兼容承诺。**必须按句子判定，不能按物理行**：
  `crates/oc-testkit/src/lib.rs` 改正后的段落里，承诺短语在第 4、6 行，而
  「does **not**」「neither … nor」在第 3、5 行。按行加否定守卫会把改正后的文本报成缺陷。
- 子句 M（迁移机制，判为 `historical-citation`）：带 `legacy` / `unmigrated` / `pre-rename` /
  `does not read` / `move the file` 等词汇的旧路径提及，是为了离开旧路径才写的。
  **排在 `plugin-abi` 之后**，因为 `engines.opencode` 的子串里含 `.opencode`；插件 ABI 是
  与他人代码的活契约，优先级高于其他一切读法。
- 明确不认领：既无迁移词汇也无兼容承诺、却声称 Zuno 自有状态在 `.opencode` 下的散文。
  那是文档与代码不一致，属于另一类缺陷，不假装覆盖。

### 反向宽化的实测教训（本次自己踩到并修掉）

第一版子句 M 把 `deliberately out of scope` 也当迁移词汇，结果吞掉了
`crates/oc-paths/src/config_chain.rs:49` —— 那行是 `engines.opencode`，属于 plugin-abi。
症状正是「把规则放宽到能容下剩余项」：类别名还在，成员已经不诚实。修法是删掉那个
词条并把子句 M 降到 plugin-abi 之下，而不是给它加特例。

### 注入证明（7 例全通过，`/tmp` 隔离镜像，真实源码零改动）

正例：本地 `PROJECT_DIRECTORY = ".opencode"`、**旧映射从未命名过的 `oc-server` 里的同型缺陷**
（证明新规则能「发现」而非只能「复述」）、未撤回的 drop-in 承诺。
反例：`LEGACY_PROJECT_DIRECTORY` 常量、被前一行撤回的同一承诺、旧路径诊断——三者均不触发。
兜底：无人认领的 `opencode` 使总数 852 → 853、`unclassified` 0 → 1、退出码 1。

镜像与真实工作树的 `crates/` 字节一致，由生成器自己记录的
`Source snapshot SHA-256: 7b1e580c…` 双向核对；rebase 后的工作树上另做了一次直接注入，
撤销后该哈希与 852/0 完全复原，`git status crates/` 干净。

### 声明范围的两个易混数字（都实测，勿互相替换）

852 是 `crates/*/src/**/*` 的**出现次数**。全仓库已跟踪文件的小写 `opencode` 是
**6753 次出现 / 5658 行**——次数与行数差 1095，任何只写一个数而不写单位的比较都会错。
其中 `.omo/` 历史记录占 5391 次（不可变档案）、`crates/` 在 `src/` 之外占 390 次、
`docs/` + README 占 104 次；不区分大小写的全仓口径是 8121 次。若要升级为全仓身份审计，
应新建清单与分类契约，不要静默扩大本 TSV 的口径。

### 一个次要但真实的诚实性缺陷：类别归零后从摘要里消失

`Counter` 只输出被递增过的键，所以 12 → 0 之后 `zuno-owned-stale-identity` 整行从摘要里
消失了。「缺这一行」和「这一类从未被考虑」在读者眼里一样。现在按固定的 `CATEGORIES`
清单输出，归零也显式写 `0`。这与「不可失败的门」是同一族缺陷：一条永不触发的规则和
一条不存在的规则，从输出上看不出区别——所以零值必须与注入证明配对才算证据。

## 2026-08-15 — R6：成功准则退役的判据，与「不可失败的门」在文档层的同型缺陷

### 判据：准则退役只有两个合法理由，「难」不在其中

本轮退役了 `## Success criteria` 的准则 1、2、5，逐条给出的理由都是可核验的事实，不是判断：

| 准则 | 退役理由（实测） | 存活意图落到哪 |
|---|---|---|
| 1 | 它点名的 `cargo test --test compat_suite` 所在文件已删除，命令无法运行 | 新增 1a：`cargo test -p oc-cli --test db_migration_ceiling`（迁移天花板拒绝，单二进制即可验证） |
| 2 | 自身已带 `STATUS DISCLOSED … does not hold`，且量测它的 `oc-config/tests/differential.rs` 已删除（该文件硬编码开发者个人绝对路径做逐字节比对） | 新增 2a：`oc-config` 的 discovery/instructions/variable_substitution/legacy + `oc-catalog` 的 skill/agent 发现 |
| 3、4 | **未退役**，只改了「身份」：其验证测试仍存在（`cli_parity.rs` / `docs.rs` / `compat_v1.rs`），因此保持在册并点名 | — |
| 5 | 用户明确撤回（跨二进制会话接管），**没有存活意图可写** | 无。但显式写明两件事：量测文件不删、`zuno session import` 不是本准则描述的东西 |

关键区别，写进了准则里以免下一轮再混：**插件 ABI 是硬要求，与 opencode 二进制的行为等价不是**。
准则 6/7 明确标注被排除在本次退役之外。

### 文档层的同型缺陷：一条无法运行的命令 = 一个不可失败的门

`docs/compatibility-matrix.md` 曾让读者运行 `cargo test -p oc-testkit --test compat_suite` 并
`cat target/compat/compat-report.json`。测试文件已删、该产物已无人写入。这与代码里的
`if let Ok(...)` 空转是同一族：读者按页面照做，得到的不是失败而是「什么也没发生」。
改法是陈述现状——`known_gaps` 已迁到 `oc_testkit::compat_report::known_gaps` 并渲染成页面上的
生成块，由 `cargo test -p oc-cli --test docs` 断言。

### 「决定尚待」句是没有闹钟的定时器（第二次实测到）

README.md:98 与 README.en.md:90 都写着「是否删除或重构 differential suites 尚待单独决定」。
该决定其实已被执行（整面差分套件已删），句子却还在等待。本轮改成陈述已发生的事实，并点名
仍然存活的三个验证资产（`cli_parity.rs`、`rollback.rs`、`session_interop.rs`），避免下一个读者
把「保留了测试」误读成「跨二进制兼容仍是产品目标」。

### 退役不等于删除量测：`docs/divergences.toml` 的处理

`non-pure-plugin-generated-trees` 的 reason 以「成功准则 2 被收窄」开头。准则退役后，正确做法
不是删条目——那会连同「这个差异当初是怎么被知道的」一起抹掉——而是追加一句 `RETAINED 2026-08-15`
说明准则已退役、条目作为实测记录保留。改 reason 必须跑 `OC_DOCS_REGENERATE=1 cargo test -p oc-cli
--test docs` 重生 `divergences.md`，否则 `page.contains(entry.reason)` 会失败。

## 任务 r7：F1 两个 Blocker（准则 4 自相矛盾、准则 2a 宿主独立性声明为假）

### 「不可能失败的门禁」有文档层形态，且比代码层更难发现

本轮两个 Blocker 是同一缺陷族的两半：

- **代码层（Blocker 2）**：`crates/oc-catalog/tests/skill.rs` 的语料检查硬编码
  `/config/.config/opencode/skill` 与 `/config/.agents/skills`，对缺失的根 `continue`，
  打印 `skipped` 却仍然 PASS。除一台机器外，它遍历 0 个文件、断言 0 次，且通过与否取决于
  仓库外的可变内容。
- **文档层（Blocker 1）**：准则 4 在同一段里既要求「comparing ... **against the real
  binary**」，又断言「Its verification is in the repository and **needs no second
  binary**」。前半句是整面差分套件被删之前的遗留，后半句是删除之后补写的。两句都在，读者
  按前半句去找那个矩阵会找不到，按后半句去信任则会漏掉「501 直接失败」这一真正的强约束。

文档层更难发现，因为它不会红。代码层的空转至少能用 `checked == 0` 观察到；一段过期的准则
描述只会在有人真的去执行它时才暴露。**结论：删除一个门禁时，必须同时 grep 所有描述它的
句子**——本轮除准则 4 外，同一句谎言还活在两处：`docs/compatibility-matrix.md:199`
（「The differential matrix invokes all 58 operations against both binaries」）和
`crates/oc-testkit/src/compat_report.rs:582` 的 `known_gaps` detail 常量
（「invoked against both processes ... rejects any 501 before applying a differential
exemption」）。后者是代码常量，会渲染进生成块，改文档而不改常量会让 `check_block` 失败。

### 修正描述可以顺带加强门禁，不必权衡

把准则 4 改成描述实际存在的验证时，有两处顺带变严，且都不是新目标：

1. 十个 `503` 缺口的旧措辞只点了三组（integration connect / credential / single-message
   read），漏掉了 `POST /api/session/{id}/permission` 与
   `GET /api/session/{id}/permission/{requestID}`。按生成表逐行核对后补全为四组，枚举与
   实际 10 行完全对应。
2. 旧措辞给 `/compact` 与 `/wait` 留了豁免，理由是「隔离的 oracle 无 provider 时也答
   503，比对两条不可用路径是假 parity」。该理由完全依赖跨进程比对；比对不存在了，豁免也就
   不再描述任何东西。生成表里这两条现在读作 `implemented`——把它们从豁免移回 48 条之内，
   是收紧而不是放松。

### 「冻结」要说清是靠什么冻结的

旧措辞只说缺口集合「enumerated and **frozen**: a test must fail if that set grows」。实际
机制是 `docs.rs::check_block` 对生成块**逐字节**比较（仅 `OC_DOCS_REGENERATE=1` 时才改写），
所以集合**增或减**都会让 `docs_compatibility_matrix_matches_every_code_table` 失败，直到有人
重生并 review diff。写清机制之后，读者不需要相信「有个测试会失败」，可以自己去看是哪个。

反过来，旧措辞里「or if an operation leaves it **without gaining a compared backend**」是
过度承诺：探针只能把离开缺口集的操作重新分类为 `implemented`，没有任何测试强制为它新增一条
逐操作比对。已按实际机制改写。

### 准则 3 与准则 4 形状不同，不应被拉平

F1 指出准则 4 措辞「overstating the surviving verification」，隐含准则 3、4 应一致。逐字核对
后结论是**两者形状本就不同，且都成立**：`cli_parity.rs` 确实执行安装的 pinned release
（`oracle_is_available()` 在 `cli_parity.rs:734`，门控 4 个比对测试加第 5 个的 witness 半边），
所以准则 3 的跨二进制说法描述的是存在的比对。真正需要补的不是把它改成本地化，而是**明确它
从不声称宿主独立**——缺失时每个测试打印点名「未比对了什么」的 `SKIPPED`，且错版本二进制会
panic 而非被接受。把这句写进准则，才能防止下一个读者以为 `cargo test -p oc-cli --test
cli_parity` 在任何机器上都在真的比对。

## 任务 r8：正向 needle 抓不到「写错的说法」，只能抓「缺失的说法」

F4 的 Blocker 1 是本会话第三次「守卫测试冻结了它本该守卫的缺陷」，但它的机制与前两次不同，
值得单独记一条。前两次（`docs.rs:1640` 冻结过期 README 句子、`no_pinned_oracle_paths.rs` 冻结
已删除的 differential 清单）都是 needle 内容本身过期；这一次 needle 内容**在当时是准确的**，
过期的是它描述的命令拼写，而 `contains_all` 这种断言形态**在原理上无法发现这件事**。

实测三态（`README.md`，`readmes_define_zuno_as_independent_while_retaining_the_plugin_abi`）：

| 文档状态 | 旧 needle（两条正向） | 新门禁 |
| --- | --- | --- |
| 只有错拼写 `zuno session import` | **PASS** —— 缺陷被冻结 | FAILED：`must mention "`zuno export` 与 `zuno import` 构成"` |
| 正确句子 + 旁边保留错拼写 | **PASS** —— 更隐蔽 | FAILED：`must not mention "zuno session export": `zuno session` carries only ["list","prune","delete"]…` |
| 只有正确拼写 | PASS（但当时不成立） | PASS |

第二行是关键：给文档补上正确句子、同时把错句子留在别处，**旧门禁两条正向 needle 全部满足**。
所以「把 needle 改准确」并不足以修掉这一类缺陷——正向 needle 的语义是「文档里有这句话」，
永远不等于「文档里没有相反的话」。要让门禁能对**错误主张**失败，必须补一条否定断言
（`contains_none_in`）。这是 `contains_all` / `check_block` 之外的第三种形态，本次新增。

### 否定断言必须从代码派生，否则它会从「过期」升级成「主动错误」

正向 needle 过期只是失去效力；否定 needle 过期会**禁止唯一可用的写法**。若将来 `export` /
`import` 真的挪到 `session` 之下，硬编码的 `"zuno session import"` 黑名单会把正确文档判红，
而修红的最省力做法恰好是删掉这条 needle——回到 F4 明令禁止的动作。

所以 `rejected_round_trip_spellings()` 从注册的 `clap` 树派生，并带两条反向断言：
`export`/`import` 必须是顶层命令；`session` 之下必须**没有**同名子命令。命令一旦搬家，**这个
函数**先带着可操作信息炸掉（"…is now registered, so forbidding it in the READMEs would forbid a
working invocation; update the documentation first"），而不是让门禁静默反转。失败信息里
`["list", "prune", "delete"]` 是运行时从 clap 读出来的，不是写死的字符串——这就是派生的证据。

### 长 prose needle 必须先折叠空白，否则钉住的是排版而非主张

两个 README 都是硬换行的。`"`zuno import` reads Zuno's own `zuno export` documents only"` 跨行，
直接 `contains` 会把 needle 钉在某个换行位置：一次不改变任何主张的 reflow 就会让门禁变红，
而红的样子看起来像有人改坏了文档。`unwrapped()` 把连续空白折叠成单空格后再匹配，钉住句子
本身。同理，`docs/readme/README.en.md` 在 `Makefile` 的 `OXFMT_FILES` 里，oxfmt 会重排它的
换行——不折叠空白的话，`make fmt-check` 与 docs 门禁会互相拆台。

## 机械重命名会篡改引文：把「引用」与「本项目行为」在文本层面分离（task-r9，2026-08-16）

`.opencode` → `.zuno` 那次 sweep 改的不只是活需求，还改掉了**逐字引用上游源码的引文**。
被篡改的引文比 stale 标识更坏：它看起来是维护过的，却让文档断言上游做了它没做的事。
本轮实测到 4 个站点（全部对照 `/tmp/opencode-src/` 真源逐行核验后修复）：

- `crates/oc-agent/src/plan_file.rs:8`——```ts 块里 `path.join(instance.worktree, ".zuno", "plans")`，
  上游 `session/session.ts:333` 实为 `".opencode"`。
- `crates/oc-paths/src/config_chain.rs:7-10`——标为 ported-from 的伪代码把
  `OPENCODE_DISABLE_PROJECT_CONFIG` 与两处 `targets: [".opencode"]` 改成了 `ZUNO_*` / `.zuno`；
  上游 `config/paths.ts:27-39` 三处都是 `.opencode`。
- `crates/oc-llm/src/catalog/source.rs:48`——「Measured on 1.18.12 ... `ZUNO_DISABLE_MODELS_FETCH=1`
  and no `ZUNO_MODELS_PATH`」。这条最隐蔽：1.18.12 只读 `OPENCODE_*`，所以按字面复现的话
  fetch 根本没被关掉，**记录下来的实测无法复现**。
- `crates/oc-llm/src/catalog/source.rs:76`——"under `ZUNO_DISABLE_MODELS_FETCH`, upstream …"。

**可复用的做法**：修复不是把名字换回去就完事，而是在文本里把两种身份显式分开，让下一次
sweep 无法再无声篡改。三种写法都用上了：引文块前加一句「The transcription below is the
**oracle's**, so it keeps upstream's own spellings verbatim」；叙述句里两个名字并列
（`OPENCODE_DISABLE_MODELS_FETCH` upstream, `ZUNO_DISABLE_MODELS_FETCH` here）；路径引用就地
标注「upstream's own directory name, quoted here as a citation rather than as a requirement」。

**判定规则**（审计 40 个站点时用的，含 36 个判 CORRECT 的）：`.zuno` / `ZUNO_*` 描述 Zuno
自己的行为 → 正确；出现在 ```ts / ported-from / Oracle: / 「measured against the real binary」
的语境里 → 篡改。fixture（`crates/**/tests/fixtures/`）本轮全部干净。

即将到来的 36 个 `oc-*` → `zuno-*` crate 重命名属于同一类风险：引文、fixture、历史记录都会被
波及，且 394 个文件的 diff 里没人看得见。

## 「没有生产调用者」有两种，处理方式完全不同（task-r9，2026-08-16）

同样是 grep 只在自身模块 + 自身测试 + `pub use` 里命中，粒度不同，结论就不同：

- **活 crate 里的死函数**（`oc-agent` 的 `read_plan` / `write_plan`）：`oc-agent` 被
  `oc-cli` / `oc-tools` / `oc-memory` 依赖，所以这是两个函数的问题，删除是小而语义化的改动。
- **整个 crate 没有依赖者**（`oc-goal`）：`cargo metadata` 实测 `oc-goal` 的依赖者是 **NONE**。
  它自带 `store.rs`（`CREATE TABLE goal`）、`tools.rs`、`status.rs`、`spill.rs`、`continuation.rs`、
  `projection.rs`，共 88 个测试——是一整个未接线的子系统（todo 67/68/69/50），不是一个孤立函数。
  删 `ingest` 只会让剩下的同样不可达；删整 crate 会撞上冻结的 36 crate roster
  （`crates.expected` + `MINIMUM_CRATES` + 计划枚举与计数，`release_surface.rs` 要求同一个 commit 内一起改）。

**先用 `cargo metadata` 查 crate 级依赖者，再判函数级死代码**——否则会把「一整个功能没接线」
误诊成「一个函数忘了删」，然后做出一个既没修好也不可评审的改动。

## 准则 11：goal 必须在共享 composition root 结算（task-r10，2026-08-16）

`oc-goal` 已从孤立 crate 接入 `crates/oc-cli/src/cmd/turn.rs::TurnHost`，而不是分别在
`run`、TUI、server 复制组装逻辑。三个 goal 工具进入共享 configured built-ins；每个请求都从
SQL 权威状态重建动态上下文；projection ingest、usage delta、turn outcome 与 terminal error 都在
同一 host 收口。composition-root 守卫继续断言全仓只有 `turn.rs` 的单一 `run_turn(` 调用点。

blocker 的关键边界不是 tool call，而是真实 turn：`update_goal(status="blocked")` 只把
`blocking_condition` 暂存到 `goal_pending_failure_signal`；`record_turn_outcome` 在 turn 结束时最多
消费一次。同一 turn 内反复调用不会伪造连续失败，第三个条件相同的真实 turn 才进入 `blocked`；
条件改变、取得进展或 terminal status 都会清理暂存信号。

server compact 现在把 HTTP admission `SessionRunGuard` 和 `TurnEventSender` 直接移交
`SessionMutationExecutor::compact`。production executor 由 `TurnHost::compact` 统一做 goal usage
与失败阻断，随后先释放 admission guard，再调用 guarded idle continuation；新增 API 回归证明
compact executor 返回前 session 始终 `Busy`，返回后才变为 `Idle`。

另一个容易遗漏的生命周期边界是 headless renderer：producer 完成后若外层仍持有最后一个
`TurnEventSender`，`render_events` 会永久等 channel 关闭。goal continuation 接入后暴露了这个旧
所有权缺口；在 producer 尾部显式 `drop(sender)` 后，`configured_model` 的三个 CLI route probe
从稳定 30 秒超时恢复为 6/6 通过。

最终证据：`oc-goal` 90 tests + 1 doctest、`oc-server --test api` 42/42、`oc-cli --lib`
119/119、`release_surface` 25/25、后台 `cargo test --workspace` exit 0、`make lint`、
`make fmt-check`、`make smoke-artifact` 全部通过；隔离数据目录并只读复用现有 auth 的真实
`zuno run --model myopenai/global.anthropic.claude-haiku-4-5-20251001-v1:0` 返回
`GOAL_INTEGRATION_OK`。

## r11 — 36 个 crate 从 oc- 改名为 zuno- 的可迁移经验

**并非所有改名 sweep 都有上游引文风险，判据是"这个 token 上游是否可能出现"。**
此前 `.opencode` → `.zuno` 那次改坏了八处上游引文，因为 `.opencode` 是上游和本项目
共用的词汇。`oc-`/`oc_` 不是：上游是 TypeScript，从未有过 `oc-*` 或 `oc_*` 标识符。
先花一次 grep 证明这一点，而不是靠"上次出过事所以这次逐条人工审 588 处"——
判据是：树中同时提到 `packages/opencode` 且匹配 `\boc[-_][a-z]` 的每一行，都在上游
路径旁引用我们自己的 crate 路径。证明成立后，机械 sed 就是正确工具；证明不成立时
才需要逐条判定。把"要不要机械替换"变成一个可证伪的问题，而不是一种态度。

**`\b` 在下划线后不成立，所以 `\boc_` 匹配不到 `CARGO_BIN_EXE_oc-process-fixture`。**
`_` 是 word 字符，`_oc` 内部没有 word 边界。11 处 `env!("CARGO_BIN_EXE_oc-*")` 因此
被 sweep 漏掉。这类漏改是编译期错误（`environment variable ... not defined at
compile time`），所以会被抓到；但同样的边界盲区若落在字符串字面量里就是静默的。
改名后除了 `\boc`，还要单独扫 `[A-Za-z0-9]_oc[-_]`。

**insta 快照的文件名由模块路径推导，改 crate 名必须同时 git mv 快照文件。**
`crates/oc-tui/src/snapshots/oc_tui__theme__tests__*.snap` 共 33 个，文件名前缀就是
crate 的下划线名。只改 crate 不改文件名，insta 会去找 `zuno_tui__*`，33 个主题测试
全部报快照缺失——这是改名会静默丢掉测试的一条真实路径，而且 `cargo check` 完全干净。
凡是"把身份写进文件名"的机制（insta 快照、`src/bin/<name>.rs` 推导 bin 名）都要在
改名清单里单列，不能只想着 manifest 和 import。

**捕获对（capture pair）比单个 fixture 更脆：改输入等于作废输出的溯源。**
`crates/zuno-tools/tests/fixtures/webfetch_page.{html,md,turndown.md,txt}` 是一组：
`.txt` 是 `htmlparser2` 的输出、`.turndown.md` 是 `turndown` 的输出，都由运行上游
函数处理 `.html` 捕获而来并逐字节断言。要改的 `oc-tools` 字符串在**输入** `.html`
里，改了它，两份上游捕获就不再是那个输入的输出。判断一个 fixture 能不能改，要先问
"它是不是某个捕获的输入"，而不只是"它自己是不是捕获"。

**`git checkout -- <未 staged 的文件>` 会回退该文件的全部工作区改动，不只是你刚加的那几行。**
我为验证 `make lint` 真的会失败，往 `crates/zuno-error/src/lib.rs` 追加了一处
`ptr_arg` canary，然后用 `git checkout --` 撤销。当时 index 里还是 sweep 前的内容，
于是整个文件的 sweep 被一并抹掉，`use oc_error::` 复活，被 doctest 抓到（3485/1）。
在大规模未提交改动的树里做临时探针，撤销要用与写入对称的方式（记录行号后删行、或先
`cp` 备份再还原），不要用 `git checkout --`。反过来说，这次是 gate 抓住了我，
canary 验证本身是对的——只是撤销方式错了。

**逐 suite 比对，而不是只比总数。** 改名可能让某个 suite 不再被发现，而总数恰好被
另一处补上的可能性虽小，但"总数一致"根本不覆盖"suite 集合一致"。把
`Running <path> (<target>)` 与紧随的 `test result:` 配对导出成一张表，改名前后各一
份，把旧表的 crate 名前缀归一化后 `diff`。本次 213 个 suite 逐行完全一致。

**`cargo fmt` 会因为改名而重排 use 分组，这是预期的、无害的。**
`zuno_` 排在 `std`/`serde_json` 之后，而 `oc_` 排在其之前，所以 524 个文件的 use
顺序要变。看到 `fmt-check` 退出码 2 时先确认 diff 只是顺序，不要以为 sweep 出错。

**这个仓库的 crate 改名无法拆成能构建的中间提交。** 根 `Cargo.toml` 的
`[workspace.dependencies]` 用 `path = "crates/oc-*"` 指向各 crate，
`crates/zuno-cli/tests/docs.rs` 又按字面断言 README 与 docs 内容——"仅目录"和
"仅代码不含文档"的提交都过不了 gate。想拆分要先确认拆出来的每一半各自能过 gate，
否则单个原子提交才是诚实的选择。

## Task r12 — "ready" 不等于"fork 完成"，以及 env 改名的两个真实陷阱

**`wait_for_ready` 返回 ≠ 进程树建好了，这两个条件之间没有任何同步。**
`crates/zuno-testkit/tests/backpressure.rs` 的 G6 两个 reaping 测试在
`wait_for_ready` 之后立刻采样进程树并断言 `>= 33`，CI run 31929364100 在
CodeBuild 上只拿到 27 个 pid 而失败，重跑即过，本机 3/3 通过、每次 0.22-0.30s，
同一测试在 runner 上耗时 24s。差额正好可以算出来：每个受 guard 保护的 host 是
4 进程链（`supervise` guard → `monitor` guard → host → grandchild），而
`PtyService::create` 只等最外层 guard spawn 就返回，2 个 PTY session 各欠 3 个
pid = 6，33 − 6 = **27**，与 CI 数字逐一吻合。lsp/mcp/plugin 三类不漏，是因为它们
的 handshake 要求子进程先应答，而 `spawn_grandchild()` 在应答循环之前执行，所以
parent 写 `ready` 时它们的孙进程必然已存在。**只有 PTY 这条路径没有任何回执。**

**修法是补同步，不是降阈值。** 阈值 33 是"完整拓扑"的定义，降到慢机器恰好能到的
数字，会让 reaping 断言跑在一棵从没建完的树上——这个 gate 存在的唯一目的就是证明
`parent_sigkill` 能回收*整棵*树。改成有界轮询（复用同文件 `wait_for_ready` 的
`REAP_TIMEOUT` + 10ms 睡眠模式），并让超时消息区分"fork 没建完"和"reap 没回收干净"：
原消息只说 "tree is incomplete"，把两种完全不同的缺陷混成一句话。

**证明race修好而不是藏起来，用"同一注入、两种实现"对照。** 只跑一遍通过什么都不能
说明——它本来在本机就通过。做法：在 fixture 的 `run_sleeping_child`（PTY 模式）
`spawn_grandchild()` 之前注入 `sleep(3s)`，确定性地制造 31/33 的欠额，然后
- 轮询版：2 passed，**耗时 9.67s**（正常 0.3s）——它真的等了；
- 把超时上界临时改成 `Duration::ZERO` 复现旧的立即断言：**2 FAILED，0.31s**，
  消息为 "31 PIDs … incomplete fork, not an incomplete reap"。
两个注入都要撤销，且撤销前先 `cp` 备份（不要 `git checkout --`，见 Task 上一节）。
补充独立验证：不改代码、用 96 个忙循环把 32 核的 load average 压到 105，测试
5.27s 通过（正常 0.3s），轮询把真实竞争吸收掉了。

**`ZUNO_ENV_NAME_MAP` 只接受 `OPENCODE_* → ZUNO_*`，`OC_*` 不能进这张表。**
`crates/zuno-paths/src/env.rs` 的 pin 测试
`every_project_owned_name_accepts_only_its_zuno_spelling` 断言
`internal.starts_with("OPENCODE_")` 且 `external.starts_with("ZUNO_")`，共 66 条。
那张表的语义是"上游继承来的名字，旧拼写要被拒绝"；`OC_*` 是本项目自己发明的
开发期开关，仓库外无人设置（`.github/`、`Makefile`、`scripts/` 全部 grep 干净），
所以是**直接改名、不留兼容路径**，不是加映射。判断依据不是前缀长得像，而是
"这个名字有没有外部设置者"。

**`OC_MODEL → ZUNO_MODEL` 是一次真实的命名碰撞，不能机械替换。**
`ZUNO_MODEL` 已经是 `OPENCODE_MODEL` 的对外拼写（env.rs:127），而 `OC_MODEL`
在 `zuno-config` 里只是变量替换测试的**样本变量名**，还出现在
`variable.rs:126` 的公开 rustdoc 例子里。机械改名会让 doctest 读成
"Zuno 通过 `{env:ZUNO_MODEL}` 读取模型"——两套机制被缝成一句半真话。
改为 `ZUNO_SAMPLE_MODEL`。**批量 env 改名前先把目标名字与既有 env 全集对一遍碰撞。**

**fixture 与 host 必须同一个提交改完，否则得到一个"通过但不再测试任何东西"的测试。**
`OC_EXAMPLE_*` 6 个名字横跨 `examples/rust_plugin.rs`（被 spawn 的 fixture 二进制）
与 `crates/zuno-plugin/tests/{jsonrpc,integration}.rs`（设置 env 的 host）；
`OC_JS_HOST_{PORT,TOKEN}` 横跨 `js/host.rs` 与 `js/shim.mjs`。只改一侧，fixture
会静默走默认分支，测试照样绿。

**改名会让"为什么用这个前缀"的旧注释变成假话，要一并重写。**
`zuno-log-probe.rs` 原注释说 `OC_PROBE_*` "deliberately not in the `OPENCODE_*`
namespace… configure this test fixture, not the product"——改成 `ZUNO_PROBE_*`
后它就与产品共享前缀了，理由的前半段不再成立。改名清单之外还要扫一遍"解释前缀选择"
的散文。同理 `oracle.rs:844` 提到的 `OC_TESTKIT_ALLOW_MISSING_ORACLE` 是**已删除**
机制的历史拼写（无任何代码读它），改名会伪造历史记录，所以保留原拼写并在散文里
标注"since-removed / 不再指向本 crate 读取的任何东西"。

**清理泄漏的 fixture 进程树：只杀树根，让 guard 链自己收尾；全量 kill 会制造收不掉的孤儿。**
`zuno-process` 的测试会 spawn 真实进程树，异常中断后可能残留。66 个残留
= 2 棵 fixture 树 × 33 个进程，正是
`parent_sigkill_reaps_every_host_process_tree` 断言的拓扑
（`1 + HOST_SESSION_COUNT * HOST_KIND_COUNT * CONTAINED_PROCESSES_PER_HOST`）。

**guard 链本身就是 reaper**（`crates/zuno-process/src/lib.rs`）：
`supervise` → `monitor` → `exec`。其中 `monitor`（:215 起）先给自己挂
`set_parent_process_death_signal(TERM)`，再 spawn 的 `exec` 子进程通过
`setpgid(None, None)`（:225）成为**进程组组长**；`monitor` 的轮询循环在
子进程退出（:202）或父进程消失（:206-208）时调用
`terminate_process_group(child_pid)`，即 `kill_process_group(pid, KILL)`
（:292-295）——**这一步才是把整组 payload 一次扫掉的动作**。
`supervise` 只发 `terminate_process`（TERM，单进程，不带组）。

所以"收集全部匹配 PID 逐个 `kill -9`"是错的：先杀掉 `monitor` 就摧毁了那个
会发组 KILL 的角色，剩下的 payload 成为没人回收的孤儿。实测连跑 6 轮全量 kill，
计数始终钉在 66。正确做法是**只杀树根**（argv 含 `parent` 的进程），
让 PDEATHSIG 逐级向下触发 guard 链自己回收，66 个一次全消失：

```sh
ps -eo pid=,args= | awk '/[z]uno-reaping-fixture/ && /parent/ {print $1}' | xargs -r kill -9
```

**测量陷阱一：`pgrep -x` 对这个二进制静默返回 0。**
Linux 把 `comm` 截断到 15 字符，而 `zuno-reaping-fixture` 有 20 字符，
所以 `pgrep -c -x zuno-reaping-fixture` 在 66 个进程存在时返回 `0`。
能匹配的名字是截断后的 `zuno-reaping-fi`。`pgrep -f zuno-reaping-fixture`
可用但会多出 1（命令行含该模式的 shell 自身）。
**凡二进制名超过 15 字符都有这个坑，本仓库有多个 crate 超长。**

**测量陷阱二：不同计数写法自匹配行为不同，会给出互相矛盾的数字。**
同一批残留先后数出 101 / 99 / 66，差异来自
`ps -eo args | grep -c '[z]uno...'` 与 `awk '/pattern/ && !/awk/'` 的
自匹配处理不同。本二进制**唯一可靠的计数形式**是
`pgrep -c -x zuno-reaping-fi`。

**`__oc_child_guard` → `__zuno_child_guard` 是改名清单里最后一个 `oc` 字面量，
漏掉的原因是同一个名字存在第二份拷贝。**
`GUARD_MARKER` 常量在 `lib.rs:10`，但 `tests/containment.rs:183` 把字面量
硬编码了一遍，不跟随常量。改名安全的依据：该 marker 是二进制 re-exec 成 guard 时
传给**自己**的 argv token，spawner 与 receiver 恒为同一个二进制，不落盘、
不入库、无跨版本契约。修法是把常量 `pub` 出去让测试引用它，消掉可再次漂移的第二份拷贝。
这是"编译器不会把两份同名字符串链接起来"这一主题的第三个变体
（前两个：insta 把 crate 名编码进**快照文件名**、`\b` 在下划线后不成立导致
11 处 `CARGO_BIN_EXE_oc-*` 被漏）。

## CodeBuild 上的非 Linux 发布腿：能力边界与四个连环缺陷（task r14，2026-08-16）

### AWS 侧的硬边界（全部实测，不是读文档推断）

* **macOS 只能预留容量**：`create-project` 用 `MAC_ARM` + 不带 fleet 直接报
  `MAC_ARM is not supported for on-demand compute`。
* **MAC_ARM 只接受一种规格**：`BUILD_GENERAL1_LARGE`。`BUILD_GENERAL1_XLARGE` 报
  `Compute type BUILD_GENERAL1_XLARGE is not supported for MAC_ARM`；
  `ATTRIBUTE_BASED_COMPUTE` 报 `Attribute based compute is not supported for MAC_ARM`；
  `update-fleet` 改 computeType 报 `Updating environmentType, vpcConfig, imageId, or
  computeType is not allowed for Mac Arm`（即 macOS fleet 建完就不能改规格，只能重建）。
* **fleet 的 statusCode 会骗人**：新建的 us-east-2 / us-east-1 macOS fleet 是
  `status.statusCode = ACTIVE`，但 `status.context = INSUFFICIENT_CAPACITY`，
  `status.message = "We currently do not have sufficient capacity…"`。**判断可用性要看
  context，不能看 statusCode**；只看 statusCode 会得到"fleet 正常"的错误结论，然后作业
  永远停在 QUEUED（实测卡了 40+ 分钟无任何报错）。us-west-2 的 context 是 null，可用。
* **`list-fleets` 不可信**：us-east-2 只返回了 `test-mac-us-east-2` 一个，但
  `web-test-genie-macos` 项目实际引用着另一个同区 fleet `web-test-genie-macos`。要盘点
  fleet，得从项目的 `environment.fleet.fleetArn` 反查。
* **fleet ARN 必须带 ID 后缀**：`create-project` 传 `…:fleet/zuno-macos` 报
  `Invalid fleet ARN`，必须是 `…:fleet/zuno-macos:b75653d8-…`。
* **新建 IAM role 后要等**：立刻 `create-project` 报
  `CodeBuild is not authorized to perform: sts:AssumeRole on service role`，等约 25 秒
  再建就成功。这个报错文本会把人引向信任策略，实际只是传播延迟。
* **`create-webhook` 的 buildType**：GitHub Actions runner 项目**不要**传
  `--build-type`（传 `RUNNER_BUILDKITE_BUILD` 报 `Invalid build type provided`），
  只传 `--filter-groups '[[{"type":"EVENT","pattern":"WORKFLOW_JOB_QUEUED"}]]'` 即可。
* **枚举探测法**：想知道某参数接受哪些值，用一个明显非法的值调用一次，服务端会回
  `Invalid environment type provided` 之类；配合 `service-quotas` 查不到 fleet 配额的
  情况，这是唯一能问出真相的办法。注意 fleet 名只允许字母数字/短横/下划线。

### CodeBuild Windows 镜像根本没有 C 工具链

实测三个环境全都没有 MSVC、没有 Windows SDK，连 mingw 的 gcc/dlltool 也没有：

* `aws/codebuild/windows-base:2022-1.0`（WINDOWS_SERVER_2022_CONTAINER）
* `aws/codebuild/windows-base:2019-3.0`（WINDOWS_SERVER_2019_CONTAINER）
* `aws/codebuild/ami/windows-base:2022`（WINDOWS_EC2 预留容量）

2022 容器里唯一的 `link.exe` 是 `C:\tools\msys64\usr\bin\link.exe`——MSYS2 的 coreutils
`link`。rustc 找不到 MSVC 时会**静默回退**到它，于是每个 build script 都报
`link: extra operand`，看起来像链接器参数问题，其实是链接器本身是错的程序。
`vswhere.exe` 不存在也是个快速判据。

**可行解**：镜像自带 Chocolatey，
`choco install visualstudio2022buildtools --package-parameters "--add
Microsoft.VisualStudio.Component.VC.Tools.x86.x64 --add
Microsoft.VisualStudio.Component.Windows11SDK.22621 --quiet --norestart"`
约 6 分钟装好，之后 vswhere 能报出 BuildTools 路径，MSVC link.exe 在
`VC\Tools\MSVC\14.44.35207\bin\Hostx64\x64`。**装完不需要 vcvars**：rustc 自己的 MSVC
探测会在链接时把真 linker 排到 MSYS2 前面。
**被排除的方案**：`x86_64-pc-windows-gnu` 同样不可行，gcc 不在 PATH、
`dlltool.exe` 不存在（`error: error calling dlltool 'dlltool.exe': program not found`）。

### Windows 容器上 rustup 的两个坑

1. **默认 profile 不可写**：`rustup toolchain install stable` 报
   `could not create component directory:
   'C:\Users\ContainerAdministrator\.rustup\toolchains\...'` +
   `The specified path is invalid. (os error 161)`。同一处写入失败还有个更隐蔽的表现：
   `rustup target add` 看起来成功，随后 rustc 报 `can't find crate for std`。
   解法：`RUSTUP_HOME=C:\rustup`、`CARGO_HOME=C:\cargo`（系统盘短路径可写）。
2. **镜像自带旧 cargo 抢 PATH**：Chocolatey 装的 cargo 1.84.1 在 PATH 上胜出，
   于是 `feature `edition2024` is required`，而刚装好的 1.97.1 完全没被用到。解法：显式把
   `C:\rustup\toolchains\stable-x86_64-pc-windows-msvc\bin` 前置，并在该目录缺
   cargo.exe 时直接 throw——静默回退正是上面那个费解报错的根源。

### macOS ARM 上可以真实执行 x86_64 产物

CodeBuild 的 `macos-arm-base:15`（实测 macOS 26.2 / Xcode 26.2）**自带 Rosetta 2**：
`/usr/libexec/rosetta/{oahd,oahd-helper,oahd-root-helper}` 存在，`arch -x86_64` 可用。
在 ARM 主机上 `rustc --target x86_64-apple-darwin` 产出的
`Mach-O 64-bit executable x86_64` 直接运行并输出 `ran on x86_64`。
注意 `pgrep -l oahd` 返回空不代表没有 Rosetta——oahd 是按需拉起的守护进程，
要判断能力应该用 `arch -x86_64 /usr/bin/true` 的退出码。

意义：`x86_64-apple-darwin` 不需要 Intel 机器就能既构建又**执行**，所以它满足"不得发布
从未被执行过的产物"这条规则，不必降覆盖。诚实的限制：Rosetta 翻译的是同一份机器码，
能证明链接与逻辑可运行，不能证明在原生 Intel 芯片上行为一致。

### 一个平台从未运行 ⇒ 该平台的缺陷全部积压

Windows leg 从来没真正跑过，于是四个独立缺陷叠在一起，每修一个才暴露下一个：

1. `version` 作业没装 Rust 工具链 → `cargo: command not found`
   （它此前跑在 GitHub 托管 ubuntu 镜像上，cargo 自带，所以这个缺失一直隐形）。
2. rustup 写不进容器默认 profile（os error 161）。
3. 旧 cargo 抢 PATH（edition2024）。
4. 镜像没有 MSVC（link: extra operand）。
再往后还有两个**项目自身**的 Windows 缺陷（见 issues.md）。

教训：一条从未在 CI 里执行过的路径，它的"支持"是声明而不是事实。四个环境相关缺陷 + 两个
代码缺陷，全都是在第一次真正运行时一次性掉出来的。

## 「机制齐备但无人触发」的用户侧变体，以及退出意图为何不能按 action 名判断（task r15，2026-08-17）

### 第 12 次「不会失败的门」：这次是用户先发现的

`TerminalEvent::Shutdown` 有枚举成员、有文档、`Dispatch::Shutdown` 有两处
`return Ok(())`、`app.rs` 还专门算了 `let shutdown = matches!(event, ...)`——整条
关闭链路建好且被测试覆盖，但**没有任何按键会产出这个事件**。`app.rs` 的
`other => TerminalEvent::Input(other)` 把 Ctrl+C / Ctrl+D 一律当输入下发，而 TUI
处于 raw mode，SIGINT 已被拿走，于是用户被困死。

这是本项目重复 11 次的「门不会失败」主题的第 12 例，也是**第一次从用户侧暴露**：
前 11 次都是测试绿着而生产无调用者（`zuno-goal` 整个 crate 零依赖者最典型），
这次是关闭机制零触发者。教训不变：**一条从未被真实入口执行过的路径，它的"支持"是
声明而不是事实**；单元测试断言「按键映射到某个枚举成员」完全不能替代真实终端验证。

同一趟里还抓到一个同族新鲜实例（**由我自己新写出来的**）：新增的
`Config::javascript_plugins_enabled()` 带 `#[must_use]`、文档写得很足，但零调用者——
真正的决策点 `JsPluginPolicy::resolve` 里我把同一段查找**内联抄了一遍**。
处置是让 resolve 调用它，把"缺省即关闭"的定义收敛到一处，而不是删掉其中一份。
**新增一个"看起来该被调用"的 helper，必须当场 grep 它的调用者**；同样重要的是，
写完一个 helper 后不要在别处内联同样的逻辑——那会同时制造零调用者和两个真相。

### 退出意图属于「和弦」，不属于「action 名」

修复过程中真正难的不是发 Shutdown，而是**哪个组件能看到按键**。链条：

1. permission 对话框聚焦时 scope 链前置 `session`，于是 `ctrl+d` 解析成
   `session_delete`、`ctrl+c` 解析成 `input_clear`，**都不是 `app_exit`**。
2. permission 对话框不认这两个 action，落到 `DialogStep::Ignored`。
3. `DialogHost` 对 `Ignored` **明确不转发给 base**（这条本身是对的，否则
   `session_new` 会在权限提示后面偷偷触发）。

结论：**同一个物理和弦会被多个 scope claim（`input_clear` / `input_delete` /
`session_delete` / `stash_delete`），所以 action 名无法告诉组件"用户想退出"**。
正确的判据是问键位表：该 chord 是否是 `app_exit` 的单和弦拼写之一
（`keybind::is_exit_chord`，从 `DEFINITIONS` 派生，重新生成上游表时自动跟随）。
反向收益：`input_delete` 的默认拼写含 `delete,shift+delete`，按 action 名判断会让
空 prompt 上的裸 `Delete` 也退出应用——按 chord 判断顺手修掉了这个。

### 「取消还是退出」不能从状态条反推

turn 运行中首次按退出键应当**取消**而非拆掉应用。但**第二次按键必须无条件退出**，
且「是否已取消过」必须显式记住，不能重读状态条的 running：**停在 permission ask
上的 turn 永远走不到引擎的中断检查点**，`abort` 之后状态条仍是 running，靠状态条
判断就会永远取消、永远不退——同一个陷阱换了个更礼貌的形式。
另一条守则：取消 sink 缺失或 `try_send` 拒收时**直接落到 shutdown**。坏掉的协作者
只该损失一次取消，不该损失退出能力。

### JS 插件是启动耗时的 98%，且「关掉」和「你没开」必须可区分

实测本机 3 个 JS 插件，各取 3 次，**两种构建都记**：

| 状态 | debug | release（`dist/unpacked/zuno`） |
|---|---|---|
| 默认（JS host 关） | 0.10 / 0.11 / 0.13 s | 0.05 / 0.04 / 0.04 s |
| `ZUNO_ENABLE_JS_PLUGINS=1` | 1.65 / 1.35 / 1.32 s | 1.28 / 1.21 / 1.18 s |
| 配置键开启 | 1.28 s | — |
| `--pure` | 0.11 s | 0.03 s |

即约 1.2 s ≈ 96–97% 花在 JS 插件加载，每个插件 spawn 一个 Node 进程；关掉之后
与 `--pure` 同量级，说明门确实落在插件路径上而非顺带砍了别的东西。
**记录耗时必须写清构建配置**：同一台机器上 debug 与 release 的默认启动差 2–3 倍
（0.11 vs 0.04 s），只写"实测 0.04 s"会让下一个人拿 release 数字去对 debug 观测，
得出"回归了"的错误结论。
门要下在**发现之前**：扫四个目录 + 读 package manifest 也是关掉的 runtime 不该做的活。

两个设计要点：

* **`source` 要能回答"为什么没加载"**，因为「关掉了」和「你没开」从外面看一模一样，
  而这正是"一行配置就能修"与"提一个 bug"的分界。落地为
  `JsPluginPolicy { enabled, source }`，`source` 取 `pure` / `environment` /
  `config` / `default` 四值（当前是 `&'static str`；若将来要给某个取值挂行为——
  例如 `--pure` 下不该提示如何开启，因为那条路径原先是安静的——就该升成 enum，
  字符串比较表达同样的意思但没有任何检查）。`debug config` 无条件输出它，
  并与 `plugin_origins` **并列而非取代**：「发现了这些插件但 host 关着」才是要看的状态。
* **`plugin_runtime` 不叫 `plugins`**。与既有的 `plugin` 列表构成近似同形词时，
  打错一个字母会静默命中另一个键。

优先级 `--pure` > 环境变量 > 配置 > 默认关：`--pure` 是既有 kill switch，
不能被一个用户没写的配置文件推翻；环境变量胜过配置，一次性运行不必改文件。

### 库不能自己往终端写：`eprintln!` 在 alternate screen 下等于「延迟污染」

`zuno-tools/src/registry.rs` 的工具遮蔽诊断此前是无条件 `eprintln!`——这个"无条件"
是上一次修复**故意**加的（安静的 log 曾把真实缺陷藏掉），所以不能改回安静。
但它在 TUI 下会写进 raw mode 终端：这些行躺在 primary screen 上，被 alternate
screen 盖住，**直到用户退出才连同上下文丢失地一起继承出来**。

修法是分层而不是消音：库侧 `tracing::warn!` + 结构化 `diagnostics` 上抛，
由 host 决定落到哪个 surface——headless `run` 打 stderr（原文一字不改，
既有断言的意图原样存活），TUI 画进 transcript。附带发现：`StatusDetail` 事件此前
**只有 `--json` 会渲染**，所以 prelude 报告的一切（被遮蔽的工具、跳过的 internal）
对普通 `run` 是隐形的。

TUI 侧还有个细节：状态条只存**一条** detail，下一条会覆盖上一条，所以警告只放状态条
等于「闪一下就没了」。必须进 transcript，并且需要一个 `Role::System`——把会话自己的
警告挂在 user 或 assistant 名下是**对"谁说的"撒谎**。

### 真实终端验证的最小可信形态（tmux）

```
tmux send-keys -t S 'stty -a | tr " " "\n" | grep -E "^-?(isig|icanon|echo)$" | tr "\n" " "; \
  echo BEFORE; ./zuno tui; echo "TUI-EXIT=$?"; stty -a | ... ; echo AFTER' Enter
tmux send-keys -t S C-c   # 或 C-d
tmux capture-pane -t S -p
```

三个必须同时成立的证据：`TUI-EXIT=0`、退出后 `stty` 报 `isig icanon echo`
（raw mode 与 SIGINT 都已归还）、capture-pane 里没有继承的 `warning:` 行。
注意首次 `send-keys` 常被 zsh 的 fancy prompt 初始化吃掉（本次只剩一个 `c`），
所以要先发一条无害命令确认 shell 已就绪，再发真正的验证命令。

### 把默认值反转时，「原本安静的路径」是最容易被忽略的回归面（task r15 补记）

JS 插件改成 opt-in 之后，`models` 会在「配了插件但 host 关着」时打一行提示。
这条提示本身是对的（否则用户看不到模型会以为是数据问题），但它的守卫写成
`!policy.enabled`，于是 **`--pure` 也命中了**——把用户刚刚显式拒绝的开关再推荐一遍。
`--pure models` 原本一个字都不打，改完多出一行 note，这是行为回归。

判据：**反转一个默认值时，要枚举所有「原本走 disabled 分支」的入口**，
其中显式 kill switch（`--pure`）与「你没开」是两种语义完全不同的 disabled，
不能共用一个 `!enabled` 判断。修法是让 `source` 带类型（`JsPluginSource`）而不是
字符串，把「该不该解释」变成一个可被类型检查的谓词 `should_explain_absence()`。
字符串比较能表达同样的规则，但没有任何检查。

顺带确认 `make smoke-artifact` 的 `13 tools offered` 未变，原因是
`zuno-smoke.rs:458` 本来就设了 `OPENCODE_PURE=1`——它从来没加载过 JS 插件，
所以这个数字钉的是**内置工具面**，与插件门控无关，断言依然有意义。
不查这一行就无法回答「数字为什么没变」，只能猜。

### 共享 worktree 上有并发写者：`git status` 干净不等于「你的改动在里面」

本次在 `oc-wt/r15` 上工作时，另一个进程在同一个 worktree 里持续提交（00:54、
00:55、01:00、01:22 四次），其中一次把我未提交的 `JsPluginSource` 重构连同 4 个
测试一起丢弃，另一次把我的 notepad 追加与两处编辑**替我提交了**。
最迷惑的一步是：`git status` 变成干净，看上去像「工作已保存」，实际是被别人
选择性提交后我的一部分改动消失了。

可复现的检测手法：

* `ls -la /proc/*/cwd | grep <worktree>` 找出把 cwd 设在该目录的进程；
* `git log --format="%h %ad %s" --date=format:"%H:%M:%S"` 把提交时间和自己的动作
  时间轴对齐——提交时间落在自己两次工具调用之间，就是并发写者；
* 采样 `git rev-parse HEAD` + `git status --porcelain | md5sum`，间隔 60–90s
  比较，判断对方是否仍在活动。

教训：**在共享 worktree 上，任何「我改完了」的结论都必须用 `git show HEAD:<file>`
或重新构建的二进制复核，不能依赖自己的编辑记忆**。本次正是靠重新读 HEAD 才发现
`--pure` 的回归还留在树上——如果按记忆汇报，就会汇报一个并不存在的修复。

## README 从「兼容说明」改为「公开介绍」（task r16）

**`1.18.13` 的解释性 prose 与常量是两回事。** `COMPATIBILITY_VERSION`
（`crates/zuno-cli/src/version.rs:14`）是冻结的插件 ABI，必须留在代码里；但 README 里
「为什么 `zuno --version` 输出 1.18.13」的那段说明，是让读者去推理 opencode 发行版而不是
推理 Zuno，属于实现细节。两个 README 各 5 处引用，全部移除后插件消息收敛成一句：支持
opencode 插件，新插件推荐用 Rust（`zuno-plugin-sdk`），并链到 `docs/plugin-authoring.md`。

**needle 从「要求写出版本号」翻转成「禁止写出版本号」，并且是派生的。**
`readme_states_the_pinned_baseline_the_binary_actually_reports` 曾要求两个 README 都出现
`COMPATIBILITY_VERSION` 及 `--version` 输出句式；它被
`both_readmes_recommend_the_rust_plugin_sdk_without_explaining_a_pinned_version` 取代：
正向 needle 钉住插件推荐语与 SDK crate 名（crate 名从
`crates/zuno-plugin-sdk/Cargo.toml` 解析，改名会红），负向 needle 用
`zuno_cli::COMPATIBILITY_VERSION` 的值作为禁止串。已实测falsification：把旧那句
「`zuno --version` 输出 `1.18.13`」加回 README，该测试立刻以
`must not mention "1.18.13"` 失败 —— 旧文本无法让新门禁通过。

**generated 块换页面必须带链接前缀，否则悄悄产出死链。** `memory-gate-measurement` 从
`README.md` 移到 `docs/resource-gates.md`（深一层），`memory_gate_block` 因此新增
`link_prefix` 参数。第一次只改了主段落，`g2_robustness_prose` 里 superseded artefact 的
链接仍是根相对 —— 是我为此新写的「每条 `../` 链接都要能从宿主页面解析、且至少 3 条」断言
把它抓出来的，不是人眼。教训：搬迁生成块时，链接解析要有断言，不能靠 review。

**中文 README 换标题 = 换锚点，必须脚本校验。** 章节从「非功能门禁 / 独立运行与插件接入」
改成「插件 / 独立运行」后，目录与 `README.en.md` 里那条
`../../README.md#g1-与-g2--峰值常驻内存` 交叉链接全部失效。用脚本重算三份文档的全部锚点
（含中文），共校验 46 条链接 0 失效；同时校验每个 `##` 标题都在目录里、目录里没有多余项。
## r17-solo TUI 重建：落地记录与可复用判据

最终形态：欢迎界面 + 环境侧栏 + 重做的对话渲染 + 四个可达的选择器。
基线 3500 passed / 213 suites，落地后 3550 / 213，+50 恰好等于新增的 50 个测试
（welcome 15、ambient 17、message +5、session +10、dialog +2、picker +1）。
注意：任务书给的 3512 passed / 3 ignored / 219 suites 在本机复现不出来 —— 我在
**同一个 commit（3b46b5f）上 stash 后重跑基线**，得到的是 3500 / 2 ignored / 213 suites。
差异不是我引入的；报数前务必自己跑一遍同 commit 基线，不要拿别人报的数字做减法。

### 这一轮抓到的四个"门禁抓不到"的缺陷（都靠实机驱动发现）

1. **模态框不独占键盘**。picker 打开时输入过滤词，字符落进背后的 prompt：
   每次按键同时做错两件事（没过滤 + 给用户没在写的消息追加文本），Enter 于是
   选中未过滤时光标所在项。根因在 `DialogHost::handle_event`：未匹配绑定的按键
   一律转给 base。441 个测试全绿也没抓到——因为没人从"host 之上"喂过普通字符。
2. **`esc cancel` 是个假承诺**。escape 解析成 `session_interrupt`，SelectDialog
   不认 → host 吸收 → 只能靠选中才能离开。页脚写着的出路不存在。
3. **scope 缺失 = 最安静的死键**。`scopes()` 原本只有 input/prompt/messages/app，
   而 model_list 在 `model` scope、agent_list 在 `agent`、theme_list 在 `theme`、
   sidebar_toggle 在 `sidebar`、tool_details 在 `tool`、display_thinking 在 `display`。
   绑定表有行、和弦拼得出、handle_action 有分支，但 KeyDispatcher 按 scope 解析，
   永远解析不到。**加了 handle_action 分支就以为通了，是本轮最容易犯的错。**
4. **多步 turn 打五个 `Assistant` 标题**。每步开一条 assistant message，逐条打标题
   把"一次回复"渲染成五段。标题应只在说话人变化时出现。

### 反向验证是必须的
`views_dialog_typed_keys_filter_the_dialog_and_never_reach_the_base` 与
`..._reject_box_can_still_be_typed_into`：把修复用 `if false &&` 注掉后**两个都失败**，
证明有牙。本仓库有过"guard 冻结了它本该抓的缺陷"的先例，新写的每个 guard 都该这么试一次。

### 视图层的两个新事实
- `ActionComponent` 新增 `drain_dialogs` / `apply_dialog_outcome`：组件在 DialogHost
  **之下**，无法自己开对话框；请求携带已构造的 dialog（待选列表在组件手里）。
- `Dialog` 新增 `handle_typed`：动作来自 KeyDispatcher，未认领按键作为普通事件到达
  host，两条路径不同。permission 拒绝框 / question 自由文本 / help 过滤框原先都只从
  动作兜底分支读字符，迁移时**必须保留各自的阶段判断**（question 的 `editing`、
  permission 的 `Stage::RejectMessage`），否则会在不该收字符的阶段收字符。

### 未达成项与原因（不要当成已完成）
- **token 统计在本机拿不到真实数字**。折叠逻辑、侧栏、百分比都有测试覆盖并通过，
  但 `myopenai` 网关在流式响应里不回 `usage`，所以实机永远显示 "no usage reported yet"。
  我试过在 `request.rs` 给 chat 请求加 `stream_options.include_usage`，网关**照旧不回**，
  于是这是一处无法验证的线路改动 → 已 revert。bedrock 会回 usage，但本机 404 无模型权限。
  结论：这是 provider/网关侧的缺口，不是 TUI 侧；UI 如实报告"未收到"而非编数字。
- `tool_details` 与 `display_thinking` 在绑定表里是 `keys: "none"`（上游 43 个 none 绑定
  之一，`keybind_tests.rs:229` 钉住这个数字），所以只能通过用户自定义绑定或命令面板触发。
  路由已打通并有测试，但实机截图只能证明路由存在，不能证明有默认键。
- 会话切换给的是空列表：无法在不丢弃可能正在运行的 turn 的前提下切会话。

## 同一个缺陷在一次修复里犯了两遍：路由与 scope 是两个独立必要条件

r17-solo 收尾自查时发现，我自己上一批提交复刻了本任务正要消除的缺陷形态：
欢迎页快捷键网格宣传 `ctrl+p commands` / `help` / `mcp`，而 `command_list`、
`help_show`、`mcp_list` 在 SessionScreen 里都没有处理分支。实机按 ctrl+p 毫无反应。

**更值得记的是修复过程本身又犯了一次**：给 `mcp_list` 加了 handle_action 分支后，
实机按 `<leader>k` 仍然把 `k` 打进了 prompt —— 因为 `scopes()` 里漏了 `"mcp"`。
`mcp_list` 的 scope 是 `mcp`，不在链里，KeyDispatcher 永远解析不到。
**"加了 handle_action 分支" ≠ "键通了"**，这两件事必须分别验证。

### 第一版 guard 为什么没抓到（比缺陷本身更重要）
我写的第一个 guard 直接调 `screen.handle_action(definition, ...)`，
**绕过了整条 scope 解析链**，所以 `mcp_list` 在 guard 里"已路由"而在实机里按不动。
一个从错误的层进入的断言，会精确地放过这一层之下的缺陷。

最终补了三层，缺一层都不够：
1. 动作被 `handle_action` 路由（能抓：忘记写分支）；
2. 动作的 scope 被 `scopes()` 覆盖（能抓：忘记加 scope）；
3. **用 `KeyDispatcher` 按真实和弦走一遍并要求被消费**（能抓前两者，是唯一
   与用户实际按键路径同构的断言）。
第 3 层写的时候踩了两个坑：`<leader>m` 必须用 `keybind::parse_sequence(spelling,
keymap.leader())` 展开，直接 `Chord::parse("<leader>m")` 会把 token 当字面量，
让一个实机验证过的键"失败"；`input_submit` 在空 prompt 上合法地什么都不做，
断言前必须先经 dispatcher 打一个字符，否则会因非缺陷原因失败。
三个 guard 都做过反向验证（移除 mcp 路由 / 移除 mcp scope 分别失败）。

### 顺带查清的一件事：TUI 不读 tui.json
`cmd/tui.rs:81` 用的是 `ResolvedTuiConfig::default()`，而 `tui.json` 的发现与多文件
合并按 `config.rs` 模块文档的 `# Scope` 明确"不在该模块范围内"，且**没有任何一层实现它**
（`grep keybinds crates/zuno-cli/src/` 为空）。所以 `ResolvedTuiConfig::keybinds`
在生产路径上永远是空的，`key_label` 的覆盖分支可达且有测试，但当前每个提示渲染的
都是内置默认拼写。已在 `key_label` 文档里写明这一点——它原来的措辞是过度承诺。
后续要做 tui.json 发现的人：`Keymap::from_config(&config)` 已经存在且可用，
缺的只是"走目录、合并文件"那一层。

### 可复用的判据
凡是"UI 宣传了某个键"的改动，必须问三个问题：动作有处理分支吗？它的 scope 在链里吗？
按真实和弦走 dispatcher 会被消费吗？只答前一个就提交，是本仓库第十二次同形缺陷。

## keys 是三个独立必要条件，不是两个（task r18）

上一轮记的是「路由与 scope 是两个独立必要条件」。实际做完六个 `keys: "none"` 动作
之后，条件是**三个**，缺任何一个都得到一个「按下去没反应」的键：

1. **有键**（`Keymap` 能把和弦解析成这个 action）；
2. **scope 在链条里**（`views::session::scopes()` 列出了该 action 的 scope）；
3. **有路由**（`handle_action` / `handle_view_action` 有一条 arm 真的做事）。

六个动作在这三项上的分布是**不均匀**的，这就是为什么必须逐个查而不能整批处理：
`mcp_list` / `help_show` / `display_thinking` / `tool_details` 早就有路由、scope 也在，
只缺键；`prompt_skills` 缺键和路由；`diff_open` 三项全缺——连整个 `diff` scope 都不在
`scopes()` 里。

### 加键有两个不显而易见的落点陷阱

**不能改 `DEFINITIONS`。** `keybind_tests::table_matches_the_upstream_fixture_row_for_row`
逐行断言 `found.keys == row.keys`，对着
`tests/fixtures/upstream-keybinds-1.18.13.tsv`。给一个 `"none"` 行填键，等于让这份表
宣称上游发布了它并不存在的键，兼容性守卫会（正确地）失败。正确做法是新增
`SHIPPED_DEFAULTS`，走与用户自定义绑定同一条覆盖路径。

**不能放在 `TuiConfig::resolve`。** 那里已经有 `terminal_suspend` 重写的先例，看起来
就是「放默认值的地方」。但宿主 `cmd/tui.rs:81` 用的是 `ResolvedTuiConfig::default()`，
**从不调用 `resolve`**。默认值放那里 = 表里声明了、运行的二进制没有 = 正是要消灭的
那种死键。唯一安全的漏斗是 `Keymap::from_config`，所有 keymap 都必经它。
判断方法：`grep -rn "ResolvedTuiConfig::default\|ResolveOptions" crates/*/src`，
看宿主走哪条。

### 选键位时被 shift 咬过一次

`?` 看起来是 help 的自然键（`diff_help` 就用它）。但 `?` 要按 shift，事件到达时带
`SHIFT` 修饰符，而拼写 `?` 声明的是无修饰符，两者**永不匹配**。功能键或 leader
序列没有这个问题。裸字母也不能用——见下条。

## 暴露一个 scope 会把它全部的裸字母一起放出来（task r18）

给 `scopes()` 加 `"diff"` 只是为了 `diff_open` 一个动作，但 `diff` scope 还带着查看器
自己的裸键：`q n p d v s b [ ]`。加进链条后，这些字母在**会话屏上**也会解析成 diff
动作，无论查看器是否打开。

输入框仍然能打字，只因为两件事**同时**成立：
- 本屏对除 `diff_open` 以外的所有 diff 动作返回 `EventResult::IGNORED`；
- `KeyDispatcher::handle_event` 在 `!result.handled` 时把事件继续交给 `inner`，
  编辑器于是把字符插进去。

**给本屏加一个 `"n"` 或 `"p"` 的 arm，这个字母立刻打不出来了。** 已加
`exposing_the_diff_scope_did_not_stop_its_bare_letters_being_typed` 守这条。

## 「未解析的和弦也会报告 handled」让可达性断言不会失败（task r18）

第一版可达性测试是：过 `KeyDispatcher::handle_event`，断言 `result.handled`。
实测变异（把 `"diff"` 从 `scopes()` 删掉）**照样通过**——因为和弦没解析成 action 时会
落到编辑器，编辑器把字符插进去并报告 `handled`。这个断言在 scope 列表为空时也成立，
即又一个「不会失败的断言」。

改成两半，各自钉一个必要条件：
- **scope 那半**：直接调 `Keymap::resolve(&chain, chord, now)`，断言拿到
  `Resolution::Action{definition}` 且 `definition.name` 就是它自己。这是
  `KeyDispatcher` 内部调的同一个函数，绕开了编辑器兜底。
- **路由那半**：调 `handle_action`，断言 `handled`。只有在上一半已经证明键能到达
  这一层之后，这一半才是有意义的。

实测两个变异各让对应的一半失败：

| 变异 | scope 半 | 路由半 | 端到端 |
|---|---|---|---|
| 删 `scopes()` 里的 `"diff"` | **FAIL** | pass | **FAIL** |
| 删 `diff_open` 的 arm | pass | **FAIL** | **FAIL** |

**教训**：断言「事件被消费」在有兜底链的系统里几乎没有鉴别力。要断言的是
**解析结果本身**。

## token usage 缺口是两个必要条件，两次修复各只做了一半（task r18）

notepad 里此前记的是「兼容层网关不回 usage（provider 侧，未修）」，而 task-r17s 的
`38a94cf` 说根因是「MessageEnd 之后的帧被丢掉」。**两个说法都只对一半。**

curl 直连该网关（`openai-us.onethinker.top/api/v1`，同一个 claude-haiku-4-5）实测：

- 不带 `stream_options`：内容帧 → 带 `finish_reason` 的帧 → `[DONE]`，
  **没有任何 usage 帧**。
- 带 `stream_options.include_usage=true`：`finish_reason` 之后多出一帧
  `choices: [], usage: {prompt_tokens, completion_tokens, total_tokens}`，再之后
  才是 `[DONE]`。

所以：只加 `include_usage` → 引擎在 `MessageEnd` 处 return，多出来的帧被丢掉 →
看起来「网关不回 usage」（第一次的结论）。只修引擎尾部读取 → 根本没有帧可读 →
仍然全零（第二次的结论）。**两半都必要。**

判断这类问题的最短路径是**打线**，不是从 UI 往回猜，也不是信任任一方的提交信息：
一条 curl 就把「provider 不发」和「client 不收」区分开了。

实测结果（真实回合）：状态条 `↑33,382 ↓481 ⚡25,604`，侧栏
`59,467 tokens / 33.4k in · 481 out / 25.6k cached`，数据库里 assistant 记录从
每条 `input:0 output:0` 变成 `{"cache":{"read":6401},"input":7006,"output":23}`。

## `ToolDispatchCompleted` 单独到达会被静默丢弃（task r18，未修，已登记）

`Transcript::observe` 对 `ToolDispatchStarted` 会在找不到对应 part 时 `append` 一个
（注释写明「a dispatch with no ToolUseStart seen — materialise it rather than
dropping」），但 `ToolDispatchCompleted` 只走 `update_tool`，找不到就返回 `false`
**什么都不做**。

后果：重连后只收到 completion 的那一路，工具输出连同它可能携带的补丁一起消失。
写测试 fixture 时也会踩到——只发 completion 的 transcript 是空的，`latest_diff()`
返回 `None`，于是 `diff_open` 报「nothing to choose from」，看起来像 diff 功能坏了。

本次没改渲染行为（超出范围），fixture 改为按真实顺序先发 Started 再发 Completed。

## `0 lsp` 是对的，但要能证明非零（task r18）

欢迎页的 `0 lsp` 不是缺陷：`ResolvedLsp::resolve(config.lsp.as_ref())` 在配置里没有
`lsp` 段时返回空列表，而本机 cwd 确实没配。造一个带 `lsp.rust.command=["rust-analyzer"]`
的项目后，欢迎页变 `1 lsp`、侧栏出现 `▾ LSP 0/1 · ◐ rust`，真实 rust-analyzer 的诊断
进入 transcript：`⌁ src/lib.rs: 1 error, 2 others (rust)` 并带 1-based 位置。

**「计数为 0」和「计数坏了」必须用配置一个真实服务器来区分**，光看 UI 分不出来。
## TUI 重建续篇：通道接不上消费者，是"建好但打不到"的第二种形态

前一轮把"路由 + scope 两个独立必要条件"记了下来。这一轮遇到的四个缺陷里有三个是同一
类的第三种形态：**通道有生产者、有容量、有 try_send，消费者却永远不跑**。它比缺 scope
更难发现，因为编译器、clippy、单元测试全都通过——每一端单独看都是对的。

### 形态一：事件在最后一个事件之后到达

`SessionScreen::drain_reports` 在 `handle_event` 里 try_recv。看起来没问题，直到发现
**已完成的回合是渲染循环能看到的最后一个事件**：LSP 报告在回合结束后 20 秒才产生，此时
没有任何事件会再来触发 drain。"下一个事件会顺手取走"不是兜底，是永远。

修法是已有的 `TerminalEvent::Wake`——但这暴露了形态二。

### 形态二：中间层吞掉了唤醒，理由在写下时是对的

`PermissionBridge::handle_event` 有一行注释："a wake carries no state of its own, so
there is nothing below to hand it to"，然后 return，不转发给 base。在 broker 是唯一
nudge 生产者时这句话完全正确。出现第二个生产者的那一刻它变成错的，而注释仍然读起来像
对的——这是最难删掉的一类注释。

实测症状：rust-analyzer 已经拉起（pgrep 看得到进程）、报告已经入队、屏幕上什么都没有。
三个组件各自都"正确"。

### 形态三：两个消费者排空同一个队列

`DialogHost::drain_outcomes` 取走整个队列。router 消费 picker 的选择，permission bridge
消费权限决定。谁先跑，谁就把另一个的答案丢掉——表现为 5 个测试在等一个**已经作出的**决定
上超时。修法是 `requeue_outcomes` 把不认领的按原顺序放回，并用 `OWNED_DIALOGS` 显式声明
所有权，而不是靠"我打开的我消费"（权限提示是通过 `host_mut()` 打开的，栈是真共享的）。

### 可复用的判据（扩展前一轮的三问）

对任何 `mpsc` / 队列接线，除了容量与背压策略（backpressure.rs 已强制），还要问：
1. 消费者的**触发条件**是什么？如果是"另一个事件到达"，那个事件一定会到吗？
2. 生产者入队之后有没有主动唤醒消费者？唤醒会被中间层吞掉吗？
3. 这个队列有几个消费者？第一个排空它时，会不会丢掉第二个要的东西？

第 1 条和第 3 条都不会被"每一端单独测一遍"发现，只能靠端到端：真实终端 + 真实
rust-analyzer，或 bridge → host → screen → drain → transcript 走完整条路的断言。

## 断言用错单位，等于没有断言

`padded` 用 `chars().count()` 补齐，CJK 字形占两格，于是每个宽字形让行多出一列——实测
skill 描述冲出右边界、折行、把整帧下推。

真正的教训在测试上：既有断言是 `padded("日本", 4)` 有 4 个**字符**。它的名字叫
`views_padded_counts_characters_not_bytes`，抓得住"按字节数"，恰好放过"按字符数"。
一个用错单位的断言比没有断言更危险，因为它占着位置、看起来在防这件事。改成断言**终端
列数**后，两种错都能抓到，原意（不要按字节）完整保留。

## usage 在 finish_reason 之后到达，整条记账链路因此为空

OpenAI 兼容端点把 `usage` 放在带 finish_reason 的 chunk **之后**的一帧里
（`stream_options.include_usage` 的约定：choices 为空、usage 有值），再之后才是 `[DONE]`。
`loop.rs` 在 `MessageEnd` 处 return，这一帧连同 `StreamEvent::TokenUsage` 一起被丢掉。

被丢掉的不只是状态条显示：`StepAccumulator` 的四个 token 字段、`update_usage` 写
`message.tokens` 的实现，全都测过而永远收不到数据。实测本机 9 个会话的数据库，每一条
assistant 记录都是 `input: 0, output: 0`；下游还有成本统计与压缩阈值。

**为什么测试没抓到**：既有 projector 测试在 `MessageEnd` **之前** apply `TokenUsage`。
这个顺序没有任何真实 provider 会产生，所以它证明了 projector 正确，同时盖住了上游正在
丢事件。写 stream 层测试时，事件顺序本身就是被测契约的一部分，不能选一个方便的顺序。

修法：`MessageEnd` 不结束这一步，而是开启 8 帧的有上限尾部读取。有上限是因为"说完了却
继续发"的 provider 不能把回合拖住；尾部阶段的错误不再让回合失败——答案已经完整落库，
此时只剩记账。

## 并发写者：同一个 worktree 被两个 agent 同时改

本轮开局在 r17 里写了 5 个文件，随后发现 `ambient.rs`、`key_label`、`SIDEBAR_MIN_WIDTH`
不是我写的——另一个 agent 正在同一个 worktree 做同一个任务。更糟的是我们都创建了
`views/welcome.rs`，我的 `cp` 覆盖了对方的文件。

**自保手法**（本轮验证有效）：
1. 发现异常文件后立刻新建自己的 worktree，`cp` 走自己的新文件；
2. 用 python 精确字符串替换把自己的改动从共享文件里**逐块回退**，而不是 `git checkout`
   ——后者会连对方的改动一起丢；
3. 对每一块 assert `count(old)==1`，找不到就报错而不是静默跳过。

另一个教训：对方的成果最后进了 main（8 个提交），而我的分支基线落后。此时正确动作是
**rebase 到 main 并只移植 main 没有的部分**，而不是强推自己的版本。实际只有 6 项是
additive 的：engine 的 usage 修复、列宽度量、命令面板、picker 按 value 搜索、LSP 诊断、
全屏权限提示取高。其余（欢迎屏、四个选择器、思考折叠）main 的实现更好，直接用它的。

判断"谁的更好"要看运行结果而不是代码：main 的欢迎屏 49/50 行且带侧栏，我的 23/50 行。

## 「守卫钉住缺陷」的第四种形态：把假话钉成契约（task r19）

前三种记过了：门禁不可能失败、断言比实现弱、测试改成迁就实现。第四种是
**守卫本身断言的就是那句假话**，于是修复看起来像是在破坏契约：

- `permission_tests.rs:250` 钉着 `until OpenCode is restarted`
- `ambient_tests.rs:97` 钉着空 LSP 区显示 `starts as files are read`

第二个尤其隐蔽：那句话在**非空**列表下是对的，只有在空列表下是假的
（没有服务器启用，读什么文件都不会有东西起来）。守卫写的时候没区分这两种情形，
于是把"空列表也这么说"固化成了契约。

**判法**：断言的是"当前行为"还是"应有行为"？如果注释只解释了机制、没解释
**为什么这个措辞对用户是真的**，它很可能只是把现状抄了一遍。

## 一个事实两个采集器 —— 这次是同一个 config 的两种"解析结果"

`ResolvedLsp::servers()` 名字看起来就是"启用的服务器"，实际是"逐服务器 override"。
`lsp: true` 下它是空的，而真正的启用集合在 `ServerRegistry` 里（38 个内置）。
census 用了前者 → 欢迎屏报 `0 lsp`，同时诊断探针用后者 → 一直正常工作。

**教训**：同一个配置有多个"解析结果"类型时，先问**功能实际消费的是哪一个**，
然后让展示层消费同一个。判据不是"哪个名字更像"，是"哪个是真的在跑的那个"。

已知同型：token 统计（sidebar 曾想自己累加，改成和状态条共用 `TokenUsage`）、
`latest_diff`（刻意从 transcript 反读而不是另设收集器）。

## 修一个"用户看到的状态"缺陷时，先数清有几个 surface 在说这件事

缺陷记录写的是"transcript 显示 `⠹ working`"。改完 transcript、写集成测试时才发现
**状态条也在说 `working`**，而且第一条消息产生之前 transcript 区域被欢迎屏占着 ——
那时候状态条是屏幕上唯一说状态的东西。只修 transcript，用户真正在看的那个 surface
上原缺陷完好无损。

**这是被一个失败的测试逼出来的，不是想出来的**：我断言 `!frame.contains("working")`，
它在一个"看起来该过"的帧上失败了，打出整屏才看到 `working` 来自最后一行。
**判据写成对整屏的断言（而不是对某个组件的方法调用），才有机会撞上第二个来源。**

反过来也有代价：那个集成测试第一版是在**空 transcript** 上跑的，对话框正好盖住
状态条那一行，于是 `!contains("working")` **空洞地通过**了。改成先流一条真实消息
让 transcript 占住区域，才是在断言用户看得到的帧。**"断言通过"和"断言观察到了东西"
是两件事。**

## 派生 vs 推送：跨层状态告知选派生，且在渲染前一刻

`observe_modal` 的实现方式有两条路：对话框 open/close 时推送，或渲染时从栈派生。
选了派生，理由是**通知点的数量**：派生只有一个调用点，不可能和屏幕不一致；
推送有 `open` 和每一处 `pop`（正常关闭、esc、外部 dismiss），漏掉任何一处
就得到一个活得比对话框久的横幅。

同理放在 `self.base.render()` **之前**：base 要在知道自己上面盖了什么的情况下画。

## tmux 驱动 leader 键：两次 send-keys 之间的工具往返会超时

`ctrl+x d` 打不开 diff，我一度以为是绑定没生效。**错了** ——
`DEFAULT_LEADER_TIMEOUT = 2000ms`（`config.rs:47`），而我把 `send-keys C-x` 和
`send-keys d` 分成两次工具调用，中间隔了好几秒，chord 早过期了。
未解析的 chord 落到编辑器、把 `d` 插进输入框（`d▏`），看起来完全像"键没绑"。

**正确做法**：`tmux send-keys -t <s> C-x d` 一次发完。
**教训**：判定"键没绑"之前，先确认自己的输入方式满足了这个键的时序前提。

## 模型不同、工具集不同：验收"某个工具"要先确认这个模型有它

想在真机上验 `edit`，用 `myopenai/gpt-5.4` 一直失败，模型自己说
"this environment doesn't provide an `edit` tool—only `apply_patch`"。
不是缺陷 —— `uses_apply_patch()` 对 gpt-5/codex 系返回 true，这些模型被换成
`apply_patch`，欢迎屏的 `12 tools`（而非 13）就是这个交换的可见证据。

换 `claude-sonnet-4-6`（13 tools）才验到 edit。
**教训**：真机验收某个工具前，先看欢迎屏的 tools 计数对不对得上。
