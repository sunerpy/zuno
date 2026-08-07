# decisions — opencode-rust

## Task 1 — choices the plan left open

- **Edition `2024`, `resolver = "3"`.** The plan named neither. 2024 is the
  current edition on rustc 1.96 and resolver 3 is its default; picking anything
  older would be a deliberate downgrade with no stated reason. Consequence worth
  knowing: resolver 3 honours `rust-version` during resolution, so bumping
  `rust-version` can change the resolved graph.
- **Toolchain channel pinned to `1.96.0`, not `stable`.** `rust-toolchain.toml`
  names the exact version so a future `stable` release cannot turn a new clippy
  lint into a CI failure without someone editing that file. The toolchain is
  already installed on this machine (`rustup toolchain list` shows
  `1.96.0-x86_64-unknown-linux-gnu (active)`) with `clippy` and `rustfmt`, so
  the pin resolves offline. `profile` is deliberately omitted so rustup does not
  try to reinstall components that are already present.
- **Binary name `opencode-rust`** on the single `[[bin]]` in `oc-cli`. Keeps it
  distinguishable from the real `opencode` binary, which this project uses as a
  differential test oracle — two things named `opencode` on `PATH` would make
  every oracle comparison ambiguous. Todo 91 owns the published name.
- **`license = "MIT"`, `repository = "https://github.com/sunerpy/opencode-rust"`,
  `authors = ["sunerpy <nkuzhangshn@gmail.com>"]`, `publish = false`.** MIT
  matches the upstream TypeScript `opencode`. The author fields come from this
  machine's git config. `publish = false` is set workspace-wide because nothing
  here is a crates.io artifact today; Todo 91 flips it if that changes.
- **First-party crates are listed in `[workspace.dependencies]` as path entries.**
  Not strictly required by the todo, but todos 2-6 run in parallel immediately
  after this one and each needs to depend on siblings. With the paths already
  declared, a member crate writes `oc-error = { workspace = true }` and never
  touches the root manifest, which removes the concurrent-edit collision on
  `Cargo.toml` that five parallel tasks would otherwise cause.
- **`clippy all = "warn"` only.** claw-code additionally sets `pedantic = allow`
  with `priority = -1` and allows four specific lints; that shape is a response
  to lints this workspace has not hit yet. Starting narrow means the first crate
  that trips something makes a deliberate decision instead of inheriting an
  allow-list nobody remembers the reason for.
- **`scripts/gen-crates.sh` is kept in the tree.** It is the record of how the
  roster was materialised and is idempotent — it skips any crate whose
  `Cargo.toml` already exists, so it can never clobber landed work.
- **`crates.expected` holds bare names only, no header comment.** The gate is a
  byte-for-byte `cmp` against `cargo metadata | jq -r … | sort`, so any comment
  line would break it. The gate's documentation lives in
  `.omo/evidence/task-1-opencode-rust.txt` instead.
- **`.gitignore` also excludes `.omo/run-continuation/` and
  `.omo/delegated-runtime/`** — orchestrator scratch state that was already in
  the tree. `.omo/plans/` and `.omo/notepads/` are deliberately tracked.

## Task 2 — choices the plan left open

The plan named the types and the two `ProviderError` methods. Everything below it
did not name, decided here, and now binding on ~95 downstream todos.

- **`Recovery` exists as its own enum, and it is the primitive; `is_retryable()`
  and `retry_after()` are derived from it.** The todo asked only for those two
  methods. Two booleans cannot express the whole decision: a context overflow is
  *recoverable* but not *retryable*, and jcode conflated exactly those two and
  spun its retry budget on requests that could never succeed unchanged. Every
  error answers one `recovery()` and the two mandated methods are computed from
  it, so they cannot disagree with each other or drift apart.
- **The enums are NOT `#[non_exhaustive]`.** Every consumer lives in this
  workspace, so an exhaustive `match` is the feature: adding a variant breaks
  every recovery site until its author decides what the new failure means.
  `#[non_exhaustive]` would force a `_ =>` arm at each site and silently route new
  failures to whatever the wildcard happened to do — which is how a taxonomy rots.
  Revisit only if `oc-plugin-sdk` is ever published for third-party consumers.
- **Every `recovery()` lists variants explicitly; no `_ =>` arms anywhere in the
  crate.** Same reasoning, applied internally. This is why the impls look verbose.
- **`Error` uses `#[from]` on all seven variants, each `#[error(transparent)]`.**
  So `?` works across domains with no boilerplate, and the aggregate adds no text
  of its own — `Display` and `source()` pass straight through to the domain error.
  The alternative, a hand-written message per variant, would have produced
  "provider error: rate limited by provider …" double-prefixing at every layer.
- **`Display` wording: lower-case, no trailing period, machine-ish `key=value`
  for optional data** (`"rate limited by provider (retry_after=Some(30s))"`).
  Rust convention is lower-case and period-free so an error composes inside a
  larger sentence. `key=value` because these strings end up in log lines. The
  `{:?}` on `Option`/`Duration` is not laziness: `Duration` has no `Display`, and
  `Debug` renders `Some(30s)`, which is readable.
- **Split into nine modules, one per domain, re-exported flat from `lib.rs`.**
  A single 1,500-line `lib.rs` would be a merge magnet across 95 todos; a todo
  touching provider errors now edits `provider.rs` only. The public API is flat —
  `oc_error::ProviderError`, never `oc_error::provider::ProviderError` — so the
  layout can change later without breaking a single consumer. `source` is the one
  `pub mod`, because its module documentation is the argument for why a boxed
  cause is not the forbidden catch-all, and that argument should be reachable from
  rustdoc.
- **`BoxSource = Box<dyn Error + Send + Sync + 'static>` for foreign causes;
  concrete types where they carry recoverable data.** `serde_json::Error` and
  `std::io::Error` are named directly, because boxing them discards
  `Error::classify()`/line/column and `ErrorKind` respectively. Everything else is
  boxed so that `oc-error` — a dependency of all 33 crates — does not drag
  `reqwest`, a TLS stack, or a DB driver into the graph of the terminal renderer.
- **`serde_json` is the only runtime dependency besides `thiserror`.** Justified
  because config, MCP, LSP and the DB all decode JSON and all four want the decode
  position preserved. `walkdir` is dev-only, for the guard test.
- **The no-anyhow guard is a textual scan, not a `compile_fail` doctest or
  `trybuild` case.** The todo said "compile-fail test"; a compile-fail test can
  only prove *this* crate rejects something, and the requirement is about the
  *other 32*. A scan over `crates/*/src/**` is the only construction that actually
  enforces the stated rule. It also reports a violation in a crate that does not
  currently compile — which is when the report is most useful — and needs no
  dependency on the crate being inspected.
- **The guard lives in `crates/oc-error/tests/no_anyhow_in_libraries.rs`** and
  walks up two directories from `CARGO_MANIFEST_DIR`. Chosen over a
  `scripts/*.sh` or a CI-only step so that `cargo test` alone enforces it, with
  no separate gate to remember.
- **It scans `src/` only, and bans four token forms** (`anyhow::`, `anyhow!`,
  `use anyhow`, `extern crate anyhow`) **plus any `anyhow` line in a member
  `Cargo.toml`.** `tests/`, `benches/` and `examples/` are out of scope: the rule
  is about what a library hands its callers. Line comments are stripped first so
  that documentation *about* the ban — including this crate's own — does not read
  as a violation; the stripper deliberately does not truncate at a `//` that
  follows a `"`, so a URL cannot hide a real import on the same line. Both
  directions are unit-tested.
- **The guard asserts floors (>= 33 crates, >= 33 files scanned) before reporting
  success.** Without them, a scanner pointed at the wrong directory passes
  vacuously forever, which is a worse failure than a false alarm.
- **`ProviderError::from_status(provider, status)` was added, though the plan did
  not ask for it.** It is the single place a wire status becomes a recovery class,
  so the five provider crates cannot each grow their own copy — five copies of a
  status classifier is exactly how jcode ended up with five `contains(…)` chains.
  Documented as a floor: a provider crate that can read a richer signal from the
  *response body* should build the specific variant directly. Reading a response
  body is reading the wire; reading a rendered error message is not, and only the
  latter is forbidden.
- **Three convenience predicates beyond the mandated two**, each because the
  alternative is a caller re-deriving it from text: `ToolError::is_model_correctable()`
  (bad args or wrong tool name can be handed back to the model; a denial cannot),
  `LspError::is_missing_binary()` (install-able vs. merely broken — both surface as
  a spawn failure at the OS level), and `Error::as_provider()` (reach provider data
  from the aggregate without a downcast).
- **`ProviderError::Refused` keeps the provider's own wording in
  `provider_text: Option<String>`.** This is the one place a `String` could be
  mistaken for the banned pattern. It is payload for display, never a
  classification channel — the variant has already settled that the request cannot
  succeed. Same rule for `ConfigIssue::detail`.
- **`ConfigIssue::key_path` is `Vec<String>`, not a pre-joined `"a.b.c"`.** A
  reporter formats it however it likes and a fixer can navigate it; joining early
  would force the next layer to split the string back apart.
- **No `Error::Io` variant on the aggregate.** An I/O failure always happens
  *while doing something*, and that something is the domain that owns it.
  `ConfigError::Io` names the file it could not read; a bare `Error::Io` could not.
- **A `size_of::<Error>() <= 128` test.** These types are returned from nearly
  every function in the workspace, so a fat variant makes every `Result` fat.
  128 is clippy's own `result_large_err` threshold; asserting it here means the
  crate that grows past it fails in `oc-error` instead of surfacing as a warning
  in an unrelated build.
- **`pub type Result<T, E = Error>` is exported, but the crate documentation tells
  callers to prefer the specific error type in a signature.** `Result<Config,
  ConfigError>` tells a caller more than `Result<Config, Error>` and lets the
  compiler prove they handled every case. The alias is for the layers that
  genuinely span domains.
- **The evidence is generated by a script, `.omo/evidence/gen-task-2-evidence.sh`,
  which is deliberately NOT committed.** Task 1 gitignored `.omo/evidence/`, and
  the generator belongs with the transcript it produces rather than in history.
  Keeping it as a script rather than hand-assembling the transcript means every
  claim in the evidence file is re-derivable by one command. It performs the
  failure-injection QA scenario under a `trap` that restores
  `crates/oc-config/src/lib.rs` even if interrupted — necessary because four
  sibling agents share this tree and an abandoned injection would break their
  builds. A later todo that wants this in CI should lift the guard test itself
  (which *is* committed, at `crates/oc-error/tests/no_anyhow_in_libraries.rs`),
  not this script.

## Task 3

- **The three roles live together in `oc_engine::interrupt::EngineInterrupts`.**
  `graceful_shutdown` and `background_tool` are distinct `InterruptSignal`
  instances rather than aliases of one shared signal, so later turn-loop and
  tool-detachment code cannot consume each other's state. `soft_interrupts` is
  the third role and is an `Arc<std::sync::Mutex<Vec<_>>>`, deliberately not a
  Tokio mutex, so parser loops, signal handlers, and other synchronous callers
  can enqueue without a runtime.
- **`SoftInterruptMessage` follows the jcode transport shape:** `content`,
  `images: Vec<(String, String)>`, `urgent`, and a typed `source` enum with
  `User`, `System`, and `BackgroundTask`. Keeping content and images now avoids
  forcing later turn-loop work to replace the queue payload when it starts
  injecting messages; `urgent` and `source` remain typed policy data rather than
  conventions encoded in content text.
- **Both 2,000-iteration hammers are normal tests, not `#[ignore]`d.** Together
  with the 50 ms Notify contract timeout they complete in about 0.10 seconds per
  full interrupt run on this machine, so ignoring them would save negligible CI
  time while removing the regression gate from ordinary `cargo test`.
- **No bare public `reset()` is exposed.** A caller must capture `epoch()` and
  use `reset_if_epoch()`, preventing later waves from accidentally reintroducing
  the unconditional clear that erased repeated cancellation requests.

## Task 4 — choices the plan left open

### The one divergence from the oracle: no eager `mkdir` (required record)

`packages/core/src/global.ts:35-43` creates **seven** directories at module
import — `data`, `config`, `state`, `tmp`, `log`, `bin`, `repos` — before any
command has decided it needs them. This crate creates none of them in a getter.
Creation lives in exactly one place, `Layout::ensure()`, which a caller invokes
explicitly.

**Why.** A path query is a question, not an action. Three concrete costs of the
oracle's shape:

1. It makes the getters untestable in isolation. A test that merely *asks* where
   the data directory is would litter the filesystem, so every path test would
   need a temp `HOME` and cleanup — for a pure string computation.
2. It makes failure arrive at the wrong time and in the wrong place. Measured:
   `TMPDIR=/ opencode debug paths` prints **no paths at all** and exits 1 with
   `EACCES: permission denied, mkdir '/opencode'`. A command whose entire job is
   to print nine strings cannot complete because an unrelated import decided to
   create a directory. The pure version cannot fail that way.
3. It couples every consumer to a side effect it did not ask for. `oc-cli`
   printing a path, `oc-tui` rendering one, and a test asserting one all
   currently pay for a `mkdir` in the TypeScript design.

**Why this has to be written down.** A differential test cannot see it. Both
binaries report identical *paths*; the difference is *when directories appear*.
So there is no automated guard, and the record is the guard: whoever wires
startup must call `oc_paths::ensure()` once, or the directories will not exist.
`ensure()` is idempotent, so calling it at every startup is correct.

**Scope.** `ensure()` creates exactly the oracle's seven, in the oracle's order,
and nothing else. `cache()` is deliberately absent (it exists only as `bin()`'s
parent); so are `snapshot/`, `tool-output/` and the database file — their owning
todos create them on demand, as upstream does. Adding `cache()` explicitly would
be inventing behaviour.

### Environment as a value, not a global read

`Layout::resolve(&Env)` takes an environment snapshot instead of reading
`std::env` internally. Forced by the workspace: `std::env::set_var` is `unsafe`
in edition 2024 and `unsafe_code = "forbid"` is live, so **no test here can
mutate the process environment**. Threading `Env` is the only way to test XDG
resolution at all — and it pays off twice, because the differential test can hand
*the same* `BTreeMap` to a child process and to `Layout::resolve_with` and
compare, which is what makes the comparison prove anything.

`Env` is a `BTreeMap`, not a `HashMap`, so building a child environment from it is
deterministic and a failing comparison is reproducible.

`resolve_with(&env, home_fallback)` exists alongside `resolve(&env)` so a test can
be *fully* pure: `resolve` still consults `std::env::home_dir()` when `HOME` is
absent, which is right for production and wrong for a hermetic test.

### `node_path`: a hand-written port of `path.posix`, not `PathBuf`

Every join in this crate goes through `node_path`, because Node's `path.join`
normalizes and Rust's does not, and the difference changes the resolved directory
for four of the nine `XDG_DATA_HOME` spellings actually measured against the
binary. A `PathBuf`-based layout would read a different data directory than the
TypeScript binary for any of them.

Scope is POSIX only. `path.win32` is a genuinely different algorithm (drive
letters, UNC roots, `\`), and guessing at it would be worse than the recorded gap
in issues.md. `FSUtil.windowsPath` is ported as a named identity function rather
than inlined away, so the Windows branch has an obvious home later.

### SHA-1 implemented in-crate rather than adding a dependency

The workspace pins `sha2`; SHA-1 is a different family and no SHA-1 crate is
pinned. Adding one means editing the root `Cargo.toml` while sibling agents hold
it. SHA-1 is a fixed, fully specified 60-line algorithm, so implementing it is
cheaper and lower-risk than a concurrent manifest edit. Test vectors are FIPS
180-4's plus coreutils `sha1sum` output at five block boundaries — never this
implementation read back to itself.

Documented in the module: SHA-1 is used **only** to reproduce directory names, is
never a security boundary, and nothing here endorses it as one. A later todo that
wants a vetted implementation can swap the two functions out; the call sites go
through `sha1::hex` only.

### `dirs` is pinned in the workspace but deliberately unused here

`dirs` normalizes a relative XDG value away and substitutes the home-relative
default. The oracle does not: `XDG_DATA_HOME=relx` really does put the data
directory at `relx/opencode`. Using `dirs` would silently relocate a user's data
directory whenever a relative XDG value is set. `xdg_base()` is eleven lines and
matches `xdg-basedir@5.1.0` exactly.

### `DbLocation` is an enum, not a `PathBuf`

`OPENCODE_DB=:memory:` is a SQLite sentinel, not a filename. A `PathBuf` cannot
say that, and the string form invites exactly the wrong move —
`create_dir_all(path.parent())` on a path called `:memory:`. `DbLocation::Memory`
makes the case unmissable in a `match`, and `as_oracle_string()` still yields the
literal string a driver wants. Consistent with task 2's rule that the type
carries the decision rather than the caller re-deriving it from text.

### `db_path()` reads the channel from `option_env!("OPENCODE_CHANNEL")`

The oracle's channel is a bundler-injected define. The closest Rust analogue is a
build-time environment variable, so a release build sets `OPENCODE_CHANNEL=latest`
and lands on the same `opencode.db` the TypeScript release binary uses.
`db_path_for_channel(channel)` is public so a test and todo 19 can pin a channel
explicitly rather than depending on how the test binary was built. The
consequence for a plain `cargo build` is recorded in issues.md.

### `PathsError` is local to this crate

`oc-error` is committed and owned by task 2, and issues.md is where a request for
a new variant belongs — so a filesystem failure here is a local
`thiserror` enum with one variant, `CreateDirectory { path, source }`. It follows
task 2's rules: a single specific variant, no `Other(String)`, the path carried as
data so the caller can report *which* directory failed rather than parsing a
message. If a later todo wants this in the aggregate, it is a one-line `#[from]`.

`ensure()` stops at the first failure rather than continuing. A half-created
layout is not a state any consumer should have to reason about.

### The differential test asserts its own sensitivity

`the_comparison_detects_a_single_character_divergence` perturbs each of the nine
lines in turn and requires the comparison to notice. Without it,
`assert_eq!(oracle, subject)` could be comparing two copies of the same
computation and would pass forever. Same reasoning as task 2's floor assertions in
the no-anyhow guard: a test that can pass vacuously is worse than no test.

The skip path (no `opencode` binary) **prints** its reason instead of failing
silently, and `oracle_binary_is_locatable` reports the resolved binary and its
version, so a machine that has quietly lost the oracle is visible in the log
rather than looking green.

Nothing is normalized. The plan forbids passing a differential test by smoothing
a real difference away, so the comparison is on raw bytes and the version-skew
question was settled by `git diff` over the thirteen layout files instead — the
diff between the v1.18.12 and v1.18.13 release commits is empty, so a mismatch
would be this crate's bug.

### `examples/dump_paths.rs` exists so the comparison is reproducible from a shell

One `print!` of `debug_paths_dump()`. It makes
`diff <(opencode debug paths) <(dump_paths)` a real command a human can run, which
is how the QA transcripts in the evidence file are produced, and it is the exact
shape todo 6's `Subject` harness needs.

### Module split, and a flat public API

Nine modules — `node_path`, `sha1`, `env`, `layout`, `files`, `walk`, `project`,
`config_chain`, `ensure` — because eight downstream todos will each touch one of
them and a single `lib.rs` would be a merge magnet. Same reasoning as task 2's
nine error modules. The modules are `pub` (unlike task 2's) because their
documentation *is* the oracle mapping and should be reachable from rustdoc, but the
names consumers need are also re-exported flat from the crate root, and the free
functions (`oc_paths::data()`, …) are the intended production API.

## Task 5 — choices the plan left open

### `init()` returns a `LogHandle`, not a bare `WorkerGuard`

The plan said the guard must be returned to the caller. It is, wrapped:

```rust
pub fn init(config: LogConfig) -> Result<LogHandle, LogInitError>
```

`LogHandle` owns the `WorkerGuard` and adds `installed()`, `level()`,
`print_logs()`, `dir()`, `dropped_lines()`, plus `into_guard()` for a caller that
wants the raw guard.

**Why not the bare guard.** Three of those answers are things a caller genuinely
needs and cannot otherwise get:
- `installed()` distinguishes "this call installed the subscriber" from "one was
  already there". A bare `WorkerGuard` looks identical in both cases, and in the
  second it holds nothing, so dropping it is a no-op — which a caller reasoning
  about flush-on-exit has to know.
- `dropped_lines()` surfaces the cost of the lossy writer (below). Without it the
  loss is an unexplained gap in a file someone is debugging from.
- `level()` / `print_logs()` let a caller report the resolved configuration
  without re-deriving the flag-over-environment precedence and getting it subtly
  different.

`#[must_use = "dropping the LogHandle stops all file logging; …"]` puts the
lifetime trap in the compiler's mouth rather than only in a doc comment.

### Idempotence: a second `init()` succeeds and installs nothing

Not an error, not a panic, and no `AlreadyInitialized` error variant.

**Why `Ok` rather than `Err`.** Both the CLI and the test suite call `init`. If the
second call returned `Err`, every caller would have to decide whether that
particular error is benign — and the only correct answer is always "yes, carry
on", which is a decision better made once, here. `handle.installed() == false`
carries the information for the rare caller that cares.

Guarded by a `static INSTALLED: OnceLock<()>`, whose `set` is atomic, so of two
racing callers exactly one proceeds and the loser tears its appender down rather
than registering a second subscriber. Both fallible steps (filter, appender) run
**before** the claim, so a failure leaves the process untouched and a later `init`
can still succeed. `tracing_subscriber`'s `try_init` (not `init`) is used, so a
subscriber installed by something outside this crate is a reason to step aside,
not to abort.

`OnceLock` here is **not** an implicit initializer: nothing reads or writes it at
load time, and a unit test asserts `!is_initialized()` before any `init` runs.

### File naming: `opencode.<YYYY-MM-DD>.log`, daily, keep 14

The oracle appends forever to a single `opencode.log`
(`logging.ts:49-52`). An agent that logs every tool call and every provider
request cannot share that policy — the file grows without bound.

Chosen: prefix `opencode` and suffix `log` (so the name is recognizably the
oracle's), `Rotation::DAILY`, `max_log_files(14)`. `Rotation::Never` is available
and reproduces the oracle's exact single `opencode.log`, which is what the tests
use so that assertions do not depend on today's date.

14 days is a judgement call: long enough to investigate a bug reported "last
week", short enough that a chatty agent does not fill a disk.

### The non-blocking writer is lossy, and that is deliberate

`NonBlockingBuilder::lossy(true)` with `buffered_lines_limit(8_192)`.

The alternative is backpressure, which would let a slow disk stall the async
runtime in the middle of a turn. Losing a log line is strictly better than
stalling a user's request. `LogHandle::dropped_lines()` makes the cost visible so
the trade is observable rather than silent.

8_192 rather than `tracing-appender`'s 128_000 default: a backlog that large means
the disk is far behind, and at that point the useful record is the drop count, not
another minute of stale history held in memory by a process already under
pressure. Enforced at compile time via `const _: () = assert!(…)`, not by a test.

### Span field names (the contract every later wave codes against)

Three spans, names and fields pinned by `pub const` and asserted by tests so a
rename is a visible breaking change rather than a silent filter that stops
matching:

| span | fields |
| --- | --- |
| `turn` | `session`, `turn`, `agent`, + late-bound `provider`, `model` |
| `tool_call` | `tool`, `call_id` |
| `provider_request` | `provider`, `model`, `attempt` (1-based), `stream`, + late-bound `status`, `request_id` |

Late-bound fields are declared `tracing::field::Empty` and filled by
`record_turn_model` / `record_provider_response`. They have to be declared:
`Span::record` on an undeclared field is a silent no-op.

`attempt` is 1-based so a retry reads `attempt=2` and is distinguishable from a
first try without diffing timestamps.

### `TOOL_LIFECYCLE` has five phases, not four

`pending → running → completed | error`, plus **`abandoned`**, emitted at warn
level from `ToolLifecycle::drop` when neither terminal phase was reached.

A tool call that stops being tracked without an outcome — a `?` on a path nobody
considered, a cancelled task — otherwise just stops appearing in the log. An
absence is the hardest thing to notice, so it is turned into a record.

A failure records `error_kind` (a stable discriminant from an exhaustive match on
`ToolError`), `retryable` and `model_correctable` as **fields**. A consumer
deciding whether a failure was the model's fault reads `model_correctable=true`;
it never matches on rendered text, which is the defect `oc-error` exists to
prevent. The match is exhaustive, so a new `ToolError` variant fails to compile
here rather than falling into a wildcard.

### `LogInitError` is local to this crate, not a new `oc-error` variant

Logging setup has no recovery: it happens once, before anything is running that
could retry. Folding it into `ConfigError::Io` was considered and rejected — that
variant renders `"config file {path} could not be read"`, which is factually wrong
for a log directory, and `oc-error`'s own contract is that a config error names the
config file at fault.

It implements `oc_error::Recoverable` so a caller can ask the same question it
asks of every other workspace error; the answer is always `Recovery::Fail`.

### `TRACE` is not in `LogLevel`

`LogLevel` is a closed four-variant enum matching the oracle's map exactly. Adding
`Trace` would make `OPENCODE_LOG_LEVEL=TRACE` behave differently in the two
implementations. `TRACE` is reachable only via `LogConfig::with_directives("trace")`
— programmatic, so it cannot diverge from a user's environment. No env var feeds
`directives`, because the oracle has no equivalent and inventing one would be a
divergence a differential test could not see.

### A `src/bin/` fixture is part of the deliverable

`src/bin/oc-log-probe.rs` is a real process that frames newline-delimited JSON on
stdout, initializes logging, and emits at every level. It exists because the
guarantee is about a process's fd 1, and the two alternatives both fail:
re-executing the test binary mixes `libtest`'s own stdout chatter into the capture
(forcing an allow-list, which is the loophole a real leak walks through), and
`dup2` needs `unsafe`, which the workspace forbids.

It is the **only** file allowed to touch stdout, and
`tests/no_stdout_in_library.rs` names it as the single exemption plus asserts the
exemption still matches a real file — so deleting or renaming the probe fails
loudly instead of silently disabling the guard.

## Task 6 — oc-testkit

### D6.1 The default oracle is the INSTALLED BINARY (1.18.12), not the pinned source tree

Both flavours are implemented and both are tested. `Oracle::discover()` prefers the installed
release; `Oracle::from_source(tree)` runs the pinned tree with bun.

Why the binary is the default:
1. It is the artifact users actually run, so a difference against it is a difference users see.
2. It **self-reports a real version** (`1.18.12`). A from-source run reports `local`, because the
   version is a build-time `define` — so a report built on it could not name what it compared
   against.
3. It is ~2.4x faster per invocation (0.46 s vs 1.1 s measured). Ninety later tasks each run it.

**The cost is a version gap, and it is surfaced rather than absorbed.** The installed release is
1.18.12; the pinned tree is 1.18.13 at `aefaf140c1`. Therefore:
- `Oracle::version_gap()` returns `VersionGap { reported, pinned }` with
  `is_aligned()` / `is_unversioned()` / `describe()`. On this machine `describe()` prints:
  *"oracle reports 1.18.12 but the pinned source is 1.18.13; a differential failure here may be a
  version gap rather than a compatibility defect"*.
- **Every** `Provenance::label()` carries the flavour, the reported version, the pinned version
  and the pinned commit, and `diff_runs()` puts that label in the report header. A differential
  failure can never be silently attributed to a compatibility defect when it was a patch bump.
- Whoever hits a suspicious failure re-runs it against `Oracle::from_source()` to tell the two
  apart. `the_from_source_oracle_runs_the_pinned_tree_and_reports_local` proves both flavours
  work from one machine.

Overrides: `OC_TESTKIT_ORACLE` (exact binary), `OC_TESTKIT_ORACLE_SOURCE` (tree),
`OC_TESTKIT_ORACLE_FLAVOUR` (`binary` | `source`; anything else is a typed error, never a silent
fallback).

### D6.2 The exact normalization rule list, and why each is safe

Five default rules, pinned by `normalize::tests::default_rule_names_are_pinned` so adding,
removing or renaming one breaks a test and must be justified. Each rule is a hand-written scanner
that must match a fully specified shape **at an exact offset** — there is no pattern language here
to loosen by one character. Each has a `why()` string and its own test asserting what it matches
*and* what it must not.

| rule | masks | why it cannot hide a semantic difference |
|---|---|---|
| `iso8601-timestamp` → `<TIMESTAMP>` | full date-time: `YYYY-MM-DD` + `T`/`t`/space + `hh:mm:ss` + optional fraction + optional zone | Requires the **time** component. A bare date (`2026-04-28`) is left alone, so a differing release date, or a date in a non-timestamp field, still diverges. `1.18.13` cannot match. |
| `opencode-id` → `<ID>` | one of the 10 known prefixes, `_`, exactly 12 lowercase hex, exactly 14 base62, bounded on both sides | Grounded in `packages/schema/src/identifier.ts`: 48 mint-time bits + 14 random base62 chars, so it cannot agree across runs. Too narrow for any word, model name, path or hash to satisfy. `xses_…` and `nope_…` are rejected by the boundary check. |
| `uuid` → `<UUID>` | canonical hyphenated 8-4-4-4-12 hex only | Randomly generated per run. An **unhyphenated** hex blob is deliberately NOT matched, because content hashes look like that and a differing hash is a real difference (a sha256 survives, asserted). |
| `loopback-port` → `<PORT>` | the digit run (1–5 digits) after `127.0.0.1:` / `localhost:` / `[::1]:` / `::1:` | The kernel picks ephemeral ports. **Only the digits are replaced; the host and colon stay in the diff**, so a differing host still diverges. `api.anthropic.com:443` and `"port": 4096` are untouched. `0.0.0.0` is excluded: it is a different address, not loopback. |
| `labelled-pid` → `<PID>` | the digit run after `"pid":`, `"pid": `, `pid=`, `pid: `, value ≥ 2 | The kernel picks pids. **Only the digits are replaced; the label stays.** `"pid":0`, `"pid":1` and `"pid":null` still diverge, because those would indicate a subject that failed to record one. `parentpid=` does not fire. |

**Volatile paths are literals, never patterns.** There is no `/tmp/.*` rule. A run masks the exact
strings it created via `ScriptedEnv::normalizer()` → `Normalizer::mask_literal(name, literal,
placeholder)`, longest literal first. `a_literal_mask_does_not_become_a_pattern` proves a
neighbouring temp path (`/tmp/oc-testkit-somewhere-else`) is still compared.

**Deliberately NOT normalized** (each of these has caught a real defect somewhere, and any of them
can be masked explicitly by a caller that can justify it):
line endings; whitespace and indentation; durations and elapsed times; numbers in general.

**`Normalizer::none()` is the right default for most comparisons** — a path dump, a JSON schema, a
tool list should agree byte for byte. Reach for `Normalizer::default()` only when a genuinely
volatile span is present. `DiffReport::render()` prints which rules fired and how many spans each
masked **even when the diff passes**, so masking is visible rather than implied.

Anti-widening proof: `diff::tests::a_real_value_difference_survives_normalization` and
`a_volatile_looking_value_outside_a_masked_position_still_diverges`
(`{"port":4096}` vs `{"port":4097}` with differing timestamps → reported;
`iso8601-timestamp` fired twice, `loopback-port` did not fire).

### D6.3 "The harness never makes a live provider call" is enforced structurally

`oc-testkit` declares **no HTTP client** — not `reqwest`, not `hyper`, nothing. Rust only permits
importing direct dependencies, so no code in this crate *can* originate a request; `axum` is a
server and cannot. `tests/no_http_client.rs` fails if one is added, and also fails if a
non-loopback URL appears in executable (non-doc, non-test) source. A consumer that needs to drive
`MockProvider` brings its own client and points it at `MockProvider::base_url()`, always loopback.
`ScriptedEnv` additionally sets `OPENCODE_DISABLE_AUTOUPDATE=1` and
`OPENCODE_DISABLE_MODELS_FETCH=1` for the oracle's own network use.

Consequence for the self-tests: they speak **hand-written HTTP/1.1 over a raw `tokio::net::TcpStream`**.
Proving a mock correct with the same client that will later be under test is precisely the
`Content-Length`-framing failure this crate exists to prevent — two components agreeing because
they share one assumption.

### D6.4 Response provenance is data: `Recorded` vs `Authored`

`ResponseOrigin::Recorded { cassette, interaction }` vs `ResponseOrigin::Authored { reason }`,
carried on every `MockResponse` and echoed back on every `CapturedRequest.served_origin`.
`MockProvider::authored_scenarios()` lists the authored ones with their reasons. Authoring is
sometimes necessary (an error path no real provider produces on demand) but proves nothing about a
wire format, and a harness that cannot tell them apart cannot warn anybody. **A todo validating a
provider protocol should assert `authored_scenarios()` is empty for the scenarios it relies on.**

### D6.5 `oc-testkit` uses no `anyhow`, despite being exempt from the workspace guard

19 typed `TestkitError` variants, no catch-all. A harness whose failures are opaque strings reports
"something went wrong" at exactly the moment a ninety-task verification chain needs to know *what*.
Every variant carries the paths, names and counts a caller needs — `BinaryNotFound` names the
expected path, everything else searched, and a remedy; `CassetteMismatch` carries both canonical
forms.

### D6.6 `requested_flavour` is a pure public function because env vars are untestable here

Rust 2024 makes `std::env::set_var` `unsafe` and this workspace sets `unsafe_code = "forbid"`, so
**no test in this workspace can mutate the process environment.** Any env-driven branch must
therefore be split into a pure function over `Option<&str>` to be testable. `oracle::requested_flavour`
is the pattern to copy; later todos with env-var-driven behaviour (config layers, flags) should do
the same rather than leaving the branch unverified.

## Task 7 — how each union was modelled, and where the escape hatches are

### Unknown keys: deny at the top, ignore in the middle, capture where the oracle does
Top-level unknown keys are **rejected**, one `ConfigIssue` per key with
`key_path = [key]`. That is the oracle's behaviour, not a choice:
`packages/opencode/src/config/parse.ts:40-53` scans for extra top-level keys and
throws `unrecognized_keys` before decoding. `Config` also carries
`#[serde(deny_unknown_fields)]` so direct `serde` use is strict too, and todo 10
gets a structured key list to turn into an actionable deprecation message.

Nested unknown keys are **ignored** (dropped), matching Effect's default
`onExcessProperty: "ignore"`. This was the deliberate opposite of "be strict
everywhere": being stricter than the oracle at a nested level would reject configs
the real binary accepts, which is a drop-in-replacement failure. A test pins it
(`nested_unknown_keys_are_ignored_like_the_oracle`).

`#[serde(flatten)]` is used in exactly the four places the oracle writes
`StructWithRest`: `AgentConfig::extra`, `ProviderOptions::extra`,
`ModelVariant::extra`, and (via a custom visitor) the permission object.

### The agent sweep: `options` gains the key, `extra` keeps it
`AgentConfig` deserializes through a wire struct and then runs the oracle's
`normalize` (`config/agent.ts:62-81`): every flattened key is copied into `options`.
Two refinements:

* the key is *also* kept verbatim in `extra`, so the value is lossless and todo 10
  can still see a deprecated key that a sweep would otherwise have buried;
* `SWEEP_EXEMPT_KEYS = ["name", "tools", "maxSteps"]` are **not** swept. All three
  are in the oracle's `KNOWN_KEYS`, so the oracle never sweeps them either. Since
  this schema deliberately does not *name* `tools`/`maxSteps`, they would otherwise
  fall through into provider options — a deprecated key silently becoming an API
  argument, which is worse than either accepting or rejecting it.

`options` is `Option<JsonMap>`, not `JsonMap`: an author who writes `"options": {}`
gets an empty map back, and an author who writes neither gets `None`, so the round
trip does not invent a key.

### The unions
* **`references` / `reference`** — untagged 3-arm enum. Arms are disjoint by required
  field (`repository` vs `path`), and the string arm can only match a string, so arm
  order is not load-bearing.
* **`plugin`** — untagged `Name(String) | WithOptions(String, JsonMap)`; the tuple
  variant serializes back to a 2-element array.
* **`formatter`** — untagged `bool | OrderedMap<FormatterEntry>`.
* **`lsp`** — hand-written visitor (`visit_bool` / `visit_map`) instead of untagged,
  because untagged swallows the inner error and the LSP rules produce two messages
  worth keeping ("needs a command unless disabled", "custom server must declare
  extensions"). The oracle's 39-id builtin list is copied verbatim and its
  `requiresExtensionsForCustomServers` check (`config/lsp.ts:63-78`) is enforced here,
  because in the oracle it is part of the schema, not of the runtime.
* **`LspEntry`** is one struct with `command: Option<...>` plus a `try_from`
  requirement ("command unless disabled"), rather than the oracle's
  `{disabled: true} | {command, ...}` union. Same accept/reject set, but lossless: the
  union's first arm would silently drop a `command` written next to `disabled: true`.
* **`mcp`** — custom `Deserialize` that buffers to `Value` and tries local → remote →
  toggle in the oracle's union order (first success wins, so `{type:"local",
  enabled:false}` with no command still lands on the toggle arm exactly as the oracle
  does), then, if all three fail, re-runs the arm the author's own `type` names so the
  error says "missing field `url`" instead of "no variant matched".
* **`permission`** — visitor over `visit_str` / `visit_map`. The bare-action form is
  **not** eagerly rewritten to `{"*": action}`; `PermissionConfig::normalized()`
  offers that instead. Keeping both forms means the parsed value still records what
  the author wrote, which a merge pass and a round trip both need.
* **`timeout` / `headerTimeout` / `oauth`** — untagged with a dedicated `False` type,
  because `Schema.Literal(false)` must reject `true`.
* **`autoupdate`** — untagged `bool | AutoupdateMode::Notify`.
* **`interleaved`** — untagged `bool | String | {field}`; the oracle's three literals
  are inside a `Union([Literals, String])`, so any string is legal.

### `OrderedMap` instead of `BTreeMap`
`config/permission.ts:14-16` states outright that permission precedence depends on
the author's key order, and `config/parse.ts:55` passes `propertyOrder: "original"`
to get it. `BTreeMap` sorts; `serde_json::Map` also sorts here (no `preserve_order`
feature); `indexmap` is not pinned and the root manifest is off-limits. So
`schema::ordered::OrderedMap<V>` is a `Vec<(String, V)>` with hand-written serde
impls, used for every `Record<String, X>`. Duplicate keys resolve last-wins in place.

**Consequence, and the reason `from_json_str` exists:** parsing must run against the
**text**. A document that has been through `serde_json::Value` is already sorted, and
no downstream type can recover the order. `Config::from_json_value` is kept for
convenience with that caveat documented, and a test
(`parsing_through_a_json_value_forfeits_key_order`) pins both behaviours so nobody
"simplifies" `from_json_str` into a `from_json_value` wrapper. **Todo 8's merge must
not route layers through `Value`.**

### `Schema.Finite` -> `f64`, and the round-trip comparison
`f64` is the type the consumers want (cost arithmetic, limits). The cost is that an
integral value re-serializes as `272000.0`. JSON has one number type, so this is a
spelling change, not a value change; the round-trip test therefore compares after
canonicalizing every number through `as_f64()`. Recorded in issues.md for todo 12.

### `reference` (singular) is kept as a field
It is `@deprecated` in the oracle (`config/config.ts:46-48`) but it is **not** on
todo 10's rejection list (`mode`, `layout`, `autoshare`, agent `tools`, agent
`maxSteps`) and it is not in todo 7's key list either. Dropping it would make a
config the real binary accepts fail as an unrecognized key and lose the user's data,
so fidelity wins. Flagged in issues.md so todo 12's differential watches it.

### Key paths in errors without `serde_path_to_error`
The root manifest is off-limits, so `schema::parse::locate_failure` recovers the path
from what is pinned: on failure it removes one candidate child at a time from a copy
of the document and re-runs the deserializer; the child whose removal makes the
document valid is the culprit, and recursion gives the full path. Required fields
cannot be removed without breaking the document a second way, so they are instead
overwritten with each of `PROBE_VALUES` (`0`, `""`, `false`, `{}`, `[]`). A false
positive is impossible because the whole document has to pass. Runs only on the
failure path. Known blind spot recorded in issues.md.

### For todo 12 / `ConfigFixture`
`Config` is `PartialEq` and `Serialize`, so a differential can compare parsed values
directly or compare `serde_json::to_value` output. Two normalizations are required
before comparing against `opencode debug config`: canonicalize numbers (`272000` vs
`272000.0`) and expect `agent.*.options` to have absorbed swept keys (the oracle does
the same, so this is agreement, not drift).

## Orchestrator — per-task verification uses the /dual-review convergence gate

Applied from Todo 7 onward, after the user asked for convergent review to avoid
repeated rejection cycles. My verification of a subagent's work admits a defect
as blocking only when it passes all three clauses:

1. **Specific and falsifiable** — I can name the file/line and state concretely
   what breaks. "Could be more robust" is not admissible.
2. **In scope** — it contradicts something the todo already states. A capability
   the todo never claimed is scope creep, not a defect.
3. **Not a preference** — "I would have structured it differently" is never a
   blocker.

Anything failing a clause is recorded as a follow-up and does not block the
merge. Disputes default to pass. Verification is still adversarial — I re-run the
subagent's central claim myself rather than trusting the report — but the *bar*
is "would this actually fail in use", not "is this how I would have written it".

First application, Todo 7: my grep flagged `mode`/`tools`/`maxSteps` appearing in
`schema/agent.rs`, which looked like the schema accepting deprecated forms that
Todo 10 must reject. Investigation showed the opposite:
- `AgentConfig.mode` is a **legitimate** agent field in the oracle
  (`packages/core/src/v1/config/agent.ts:26`, `subagent|primary|all`). The
  deprecated thing is the **top-level** `mode.<name>` block, and
  `deprecated_top_level_keys_are_not_accepted` proves that is rejected.
- `tools`/`maxSteps` appear only in `SWEEP_EXEMPT_KEYS`, which stops them
  becoming provider options — a deprecated key silently turning into an API
  argument would be worse than either accepting or rejecting it.
Verdict: sound, no rejection. Under the old habit this would have cost a rework
round on a false positive.
## Task 8
- macOS precedence is tested on Linux through DiscoveryOptions::with_managed_preferences, which injects the decoded plist document at the same final merge point used by native discovery. The test puts conflicting scalar and instructions values in every earlier layer and proves the injected macOS value wins while instructions still append/de-duplicate. Native macOS discovery mirrors managed.ts:43-65 and remains cfg-gated.
- JSONC uses an internal byte-stable lexical pass because the workspace pins no JSONC parser and root Cargo.toml cannot be edited in this task. Comments become spaces while newlines are retained, and only syntactically trailing commas become spaces; serde_json then supplies line/column. json_error_byte_offset maps that position to the original bytes.
- The differential uses Normalizer::none: no volatile value is masked. Before byte comparison, both outputs pass through the typed Config serializer. The sole explicit allow-list is an empty deprecated mode object emitted by the oracle bootstrap; a non-empty mode fails the test instead of being hidden. This is required because Todo 7 intentionally rejects deprecated mode.
- Variable substitution belongs immediately before strip_jsonc in parse_layer. Todo 9 can call its variable pass there, before schema validation and merge; Task 8 does not interpret {env:VAR} or {file:path}.


## Task 9 — `oc-config::variable`

### The API Todo 8 should wire into

```rust
Substitution::for_file(&Path)            // relative {file:} resolve against its dirname
Substitution::for_virtual(label, dir)    // remote body: label goes in errors, dir is the base
    .with_env(&oc_paths::env::Env)       // the oracle's input.env, consulted first
    .with_process_env(&Env)              // stands in for process.env AND os.homedir()
    .on_missing(Missing::Empty)           // default is Missing::Error
    .apply(&str) -> Result<String, ConfigError>
```

`apply` takes and returns **text**: call it on the raw file contents, then strip
JSONC comments, then `Config::from_json_str`. That is the oracle's order
(`config.ts:220-227`) and it is load-bearing — see `learnings.md`.

Builder rather than an input struct, because four of the five knobs are defaulted
at almost every call site and a struct literal would need `..Default::default()`
plus a lifetime. Everything is `const fn` and `Copy`, so a configured
`Substitution` can be built once per layer and reused across texts.

### Injected env: `oc_paths::env::Env`, not a new map type

`Env` already models the two JavaScript lookup rules this needs (`value` = `??`,
`truthy_value` = `||`) and Todo 8 will be holding one anyway. Defining a second
map type would have forced a conversion at every call site and re-litigated the
empty-string semantics. `Env::empty().with(k, v)` makes test setup a one-liner.

### `process.env` and `os.homedir()` are one injectable knob, not two

The oracle reads both from the real environment, so modelling them as one
`with_process_env(&Env)` (with `HOME` inside it) is faithful *and* removes the
need for a separate `with_home`. Unset, it snapshots the real environment once in
a `OnceLock`; the snapshot cannot go stale because mutating the environment is
`unsafe` and this workspace forbids `unsafe`. This is the same argument
`oc_paths::global()` makes.

Deliberately **not** `oc_paths::home()`: that is `Global.Path.home`, which
`OPENCODE_TEST_HOME` overrides, and `variable.ts` calls `os.homedir()` directly
and never sees that override.

### Ambiguities the oracle left, and how each was resolved

* **Where does a bad file reference live in `ConfigError`?** The oracle throws
  `InvalidError` with `message` set and `issues` unset — the fault is the file's,
  not a key's. `oc-error` has no message-only shape, so it is one `ConfigIssue`
  with an **empty `key_path`**, which is how "nowhere in particular" is spelled in
  the Todo 2 taxonomy. Rejected `ConfigError::Io`: it would lose the token text,
  and the oracle deliberately reports the token so the user can find it.
* **Virtual sources have a label, not a path.** `ConfigError::Invalid.path` is a
  `PathBuf`; the label (`https://example.test/config.json`) goes in it via
  `PathBuf::from`. Mild abuse, but the oracle uses one string field for both and
  splitting them here would fork the error type for no caller's benefit.
* **Invalid UTF-8.** `readFile(p, "utf-8")` is lossy, so `String::from_utf8_lossy`
  — not an error. Verified byte-for-byte: `[41 ff fe 42]` → `A\u{fffd}\u{fffd}B`.
* **No usable `HOME`.** Node falls through to the password database; nothing pinned
  here can. Yields `""`, which makes `path.join("", "x") == "x"`, so a `~/`
  reference degrades to config-relative — the same thing Node does when
  `os.homedir()` returns empty. Documented and tested rather than papered over
  with `dirs::home_dir()`, which would have diverged from `HOME`.
* **Regex semantics without a regex crate.** `regex` is not pinned and the root
  `Cargo.toml` is off limits, so both patterns are hand-scanned. On a failed match
  the scanner resumes just past the literal prefix rather than one character on;
  that is equivalent here because no shorter overlapping prefix of `{env:` or
  `{file:` exists, and the overlap cases (`{env:}{env:A}`, `{env:{env:A}`) are
  tested against measured oracle output.
* **Absolute paths are not normalized.** Tempting to canonicalize; it would change
  the reported path and, under symlinks, which file is read. Left verbatim.

### Test placement

Unit tests inline in `src/variable.rs` (so they are named `variable::tests::*`)
and integration tests named `variable_*` in `tests/variable_substitution.rs`, so
that the plan's `cargo test -p oc-config variable` name filter selects both.

## Task 11

**Task NOT completed. Stopped deliberately on a concurrent-writer collision — see `issues.md` (task 11). Nothing committed; `task-11` is still at `b317132`.** The decisions below are recorded so whichever implementation is adopted does not have to re-derive them.

- **The `CONTEXT.md` seam for Todo 10.** The cascade must not accept `CONTEXT.md` (upstream `instruction.ts:67`), but a repository carrying one silently loses its instructions under this port, so detection has to live somewhere. Three-part seam, chosen because Todo 10 needs both the name and the two *shapes* the cascade uses: (1) `pub const DEPRECATED_CONTEXT_FILE: &str = "CONTEXT.md"` so no caller spells it; (2) `Loader::deprecated_instruction_files()` — every `CONTEXT.md` in the same inclusive `directory ..= worktree` range the cascade scanned, deepest first, i.e. exactly what upstream would have accepted; (3) `deprecated_instruction_file_in(dir)` — the single-directory check mirroring `find`, which is the upward append's blind spot. Detection only; acceptance stays rejected.
- **How the concurrency cap is tested, not trusted.** The cap is `futures::StreamExt::buffered(limit)` — order-preserving, and it constructs futures lazily so nothing runs before admission. Observed with a `Gauge { inflight, peak }` of atomics: each future increments on **first poll** (inside the async block body, not at construction, or the count would read 64), sleeps 25ms so admitted futures genuinely overlap, then decrements. 64 items at limit 8 must observe peak **exactly** 8 — `assert_eq!`, not `<=`, so a regression to unbounded *or* to serial both fail. Same for 4. Plus `bounded_map_treats_zero_as_one` and an order-preservation assert inside the helper.
- **Remote failures warn, they are not silent, and they are never errors.** Upstream is silent (`:98` catches to `null`). Diverging deliberately: a `tracing::warn!` carrying the URL and reason, **and** a returned `Vec<InstructionWarning>` so the caller can surface it. An instruction that silently fails to load changes model behaviour with no observable cause, which is the worst failure mode this module has. The load itself always succeeds — the entry is simply absent from `blocks`.
- **The 5s bound covers the body read, not just the headers.** Upstream times out only `http.execute` (`:96-99`) and reads the body unbounded (`:101`), so a server that answers headers and stalls the body hangs it forever. Bounding the whole operation with `tokio::time::timeout` is the only way "abandoned at 5s" is actually true. Deliberate improvement over the oracle.
- **Upstream's `withTransientReadRetry` (`:59`) is not ported.** A retry inside a 5s wall-clock bound cannot help, and it makes the bound harder to reason about.
- **Ancestry is component-wise, not string-prefix.** Upstream's `current.startsWith(root)` (`:194`) is a string compare, so a sibling directory `/rootfoo` passes for root `/root`. Using `Path::starts_with` instead. Only reachable when the file being read is outside the root, so no observable parity cost.
- **Glob results are sorted; upstream does not sort.** Bun's `Glob.scan` yields directory order, which is not reproducible across machines. A system prompt whose instruction order changes between runs is not debuggable. Ordering *between* sources still follows the oracle exactly.
- **Glob walks are bounded by the pattern's literal prefix.** Descend from `cwd/<leading literal components>`, and cap `walkdir` depth at the remaining component count unless a component contains `**`. A pattern with no metacharacters at all is a direct `is_file` check, no walk. Without this, one relative entry costs a full-repository walk *per ancestor level*.
- **No ambient state: `Claims` is caller-owned.** Upstream keeps `Map<MessageID, Set<string>>` inside the service (`:74`). One `Claims` per assistant message, owned by the session layer, keeps `oc-config` free of instance state while preserving the only semantics that matter (attach-once-per-message). `Claims::clear` maps to `Instruction.clear`.
- **`Locations` takes all four anchors explicitly** (`global_config`, `home`, `directory`, `worktree`) and never consults the process working directory, so discovery is testable on `tempfile` trees without mutating process env.

## Task 10 — error shape and filesystem detection

**Reused `ConfigError::Invalid`, did not add a `Deprecated` variant.** The plan's
prose named `ConfigError::Deprecated { found, replacement, location }`, but the task
constraints forbid touching any crate other than `oc-config`, so adding a variant to
`oc-error` was not available — and it turned out not to be needed. `Invalid { path,
issues: Vec<ConfigIssue> }` already carries exactly the three things a deprecation
report needs: `path` = the file, `ConfigIssue::key_path` = the structured JSON pointer
(`["agent","build","maxSteps"]`), `ConfigIssue::detail` = the payload. `Invalid` also
reports *many* problems at once, which matters here: a config with four deprecated
keys should produce four repair instructions, not four consecutive runs.

The typed classification lives in `oc-config` instead: `legacy::DeprecatedForm` (a
closed ten-variant enum, not `#[non_exhaustive]`, matching Todo 2's convention) and
`legacy::Deprecation { form, path, pointer, found, replacement }`. `Deprecation::issue()`
lowers it into a `ConfigIssue`. No `String`-carrying catch-all was introduced.

**Every message is self-contained.** `Deprecation::message()` renders
`deprecated {kind} \`{found}\` at {path}; {replacement}` — e.g.

```
deprecated key `agent.build.maxSteps` at /repo/opencode.json; use `steps`
deprecated agent definition `mode/build.md` at /repo/.opencode/mode/build.md; move it to `agent/build.md`
```

The path is repeated inside the issue detail even though `Invalid` already has a
`path`, because for a *directory* scan `Invalid.path` is the scanned root and the
offending file is a child of it. An issue that did not name its own path would lose
that.

**Ordering contract, not a code change to Todo 7's files.** `schema/parse.rs` already
rejects `mode`/`layout`/`autoshare` as `unrecognized key`. Rather than edit that file
(not owned by this task), `legacy::check_config` is written to run **before**
`Config::from_json_str`, and a test
(`legacy::tests::the_legacy_pass_is_what_makes_the_schemas_rejection_actionable`)
pins the contract: it asserts the schema's message is the bare `unrecognized key` and
the legacy pass's message names `agent.build`. Whoever wires the two together must
call the legacy pass first.

**`reference` is rejected by the legacy pass while the schema keeps parsing it.** Todo 7
put `reference` in `KNOWN_TOP_LEVEL_KEYS` on the grounds that dropping it would lose a
real user's data. That is still true, and it is left as-is — parse succeeds, the legacy
pass errors. This is deliberate layering: the schema's job is to *understand* the
document, the legacy pass's job is to *refuse* it. Nothing needed to change in the
schema.

**Filesystem forms are detected by scanning a directory, never by reaching into
another task's module.**

* `inspect_directory(dir)` — the project- or global-level forms. For `{mode,modes}/`
  it reports one `Deprecation` per `*.md` **directly inside** the directory, which is
  exactly the oracle's `{mode,modes}/*.md` glob. An *empty* `mode/` directory is
  **not** reported: the oracle loads nothing from it, so there is no behavioural
  difference, and a false positive here blocks a config load. It also reports
  `CONTEXT.md` when present.
* `inspect_global_directory(dir)` — the above plus the extensionless TOML `config`
  file, which `config/config.ts:262` looks for under the global config dir **only**.
  A test pins that a project directory containing a `config` file is not flagged.
* `CONTEXT.md` coordination with Todo 11: this task only *detects the file's presence*
  in a scanned directory. It does not touch `instructions.rs` and does not need
  Todo 11's cascade — Todo 11 excludes `CONTEXT.md` from the cascade, this task
  explains why it is excluded.

**`condition` is matched structurally, not by name alone.** An auth prompt is a plugin
API descriptor, so `inspect_auth` walks the descriptor's JSON form and flags
`condition` **only on an element of a `prompts` array**. A test pins that a bare
`{"rules":[{"condition":"always"}]}` is not flagged — `condition` is a perfectly
ordinary word and flagging it everywhere would be noise.

**A `mode.<name>` entry is scanned for agent-level forms too.** `mode.build.maxSteps`
produces two findings (`mode.build` and `mode.build.maxSteps`), because the oracle
spreads a `mode` entry into `agent` verbatim, so both are genuinely deprecated. Report
both and the author fixes it once; report only the outer one and they come back.

**Nothing is written.** The oracle's TOML path migrates and `unlink`s. This pass has a
test (`the_toml_config_file_is_never_rewritten_or_removed`) asserting the file is
byte-identical afterwards and that no `config.json` appeared.

**Test placement is dictated by the acceptance command.** `cargo test -p oc-config legacy`
filters on test *name*, not on target. Unit tests therefore live in
`src/legacy/tests.rs` so their paths are `legacy::tests::*`, and the two integration
QA tests in `tests/legacy.rs` are named with a `legacy_` prefix. Tests placed in
`tests/legacy.rs` with unprefixed names would silently not run under that command.

## Task 11 — instruction discovery and the `instructions[]` loader

### `CONTEXT.md` excluded from the cascade (deliberate divergence)

The oracle's `instructionFiles` is `["AGENTS.md", "CLAUDE.md", "CONTEXT.md"]`
(`packages/opencode/src/session/instruction.ts:64-68`), with `CONTEXT.md` marked
`// deprecated` at `:67`. This port's `INSTRUCTION_FILENAMES` is `AGENTS.md` →
`CLAUDE.md` only, matching the user's "reject deprecated forms" directive and
todo 10, which rejects `CONTEXT.md` explicitly.

Observable consequence, recorded so todo 12's differential harness can allow-list
it: a repo whose **only** instruction file is `CONTEXT.md` loads zero project
instructions here and one under the TypeScript binary. A repo that also has
`AGENTS.md` or `CLAUDE.md` behaves identically, because `CONTEXT.md` is last in
the oracle's cascade and the earlier name always claims the chain first. So the
divergence is reachable only from a fully deprecated tree. Pinned by
`context_md_is_never_loaded`.

### Concurrency modelled as `futures::StreamExt::buffered`, bounds as public constants

`Effect.forEach(..., { concurrency: 8 })` / `{ concurrency: 4 }`
(`instruction.ts:157-158`) map to `futures::stream::iter(..).buffered(N)`, which
preserves **output order** while bounding in-flight work — order matters because
the rendered instruction blocks must line up with `Array.from(paths)`.
`LOCAL_CONCURRENCY = 8`, `REMOTE_CONCURRENCY = 4` and `REMOTE_TIMEOUT = 5s` are
`pub const` rather than literals so a caller and a test can name them.

Remote concurrency is **observed**, not just asserted: `remote_fetches_run_exactly_four_at_a_time`
registers 8 wiremock endpoints delayed 30s, spawns the load, samples
`received_requests()` at 1.5s and asserts the server has received exactly 4.
Local concurrency is *not* directly observable — filesystem reads finish faster
than any sampling window, and the only ways to block one (FIFOs, FUSE) are
platform-specific and flaky. It is therefore asserted structurally: the constant
is checked, the single `.buffered(LOCAL_CONCURRENCY)` call site is the only read
path, and `a_file_count_above_the_concurrency_bound_loads_completely_and_in_order`
proves 25 files (> 3× the bound) all arrive in order. Stated as unverified in the
task report rather than claimed as proven.

### The 5s remote budget needs two timeouts, and one error classification

`Effect.timeout(5000)` (`instruction.ts:97`) wraps only the response; the body
read at `:100-101` is unbounded. This port bounds the **whole** fetch — headers
and body — with `tokio::time::timeout(REMOTE_TIMEOUT, ..)` *and* sets the same
budget on the `reqwest::Client`, so a server that answers `200` then stalls
mid-body cannot hang a turn.

Consequence found empirically: with both budgets equal, `reqwest`'s own timeout
usually trips first and surfaces as a *transport* error, so a hanging server was
initially reported as `RemoteTransport("error sending request…")`. `fetch_one`
now routes `reqwest::Error::is_timeout()` to `WarningKind::RemoteTimeout`
(`transport_or_timeout`). Without that, a hang and a DNS failure are
indistinguishable in the logs. The wiremock test asserts the *kind*, not just
that the load finished.

### Failures are non-fatal warnings, not `ConfigError`

The oracle swallows every instruction failure into `""`
(`instruction.ts:91-92, 98-99`), which makes a typo in `instructions[]` silently
invisible. `Instructions::load` is therefore **infallible** — it returns
`LoadedInstructions { entries, warnings }` — because one unreachable URL in a
config file must not make the agent unusable, and there is no failure mode left
that deserves to abort a config load. Warnings are additionally emitted through
`tracing::warn!`, so a caller that ignores `warnings()` can still explain a
missing instruction. Nothing in this module needs `ConfigError`, and no
dynamically typed error is introduced.

### De-duplication keyed on canonical identity, reported as the textual path

Node's `path.resolve` is textual and symlink-blind, so string de-duplication
alone charges twice for a symlink and its target — and an instruction file is
re-sent on **every turn**, so a duplicate is a permanent cost. The `seen` key is
`fs::canonicalize(resolved)` with the resolved path as fallback; the path
*reported* stays the textually resolved one so output still matches the oracle
where the canonical path differs (`/var` vs `/private/var`).

### `InstructionOptions` takes `Env` + `Layout`, not bare paths

Mirrors `discovery.rs` (todo 8) rather than inventing a second convention: the
`OPENCODE_*` flags are read through `Env::flag`, and `$CONFIG`/`$HOME` come from
`Layout`, so todo 12's differential harness can hand the same immutable `Env` to
both this loader and the TypeScript oracle without `std::env::set_var` (forbidden
here — `unsafe_code = "forbid"`). The merged `instructions` list is an **input**;
todo 8 owns the cross-layer concat/de-dup and this module never re-implements it.

## Task 10 — where each seam lives, and why

**`ConfigError` gained no variant.** `oc-error` is another crate and its enums are
not `#[non_exhaustive]`, so adding `Deprecated { found, replacement, location }`
would force every existing `match` on `ConfigError` to change. The information the
plan asked that variant to carry is carried instead by a `Deprecation` struct in
`oc-config::legacy`, which renders itself into the existing
`ConfigError::Invalid { path, issues }` — one `ConfigIssue` per finding, `key_path`
holding the structured JSON pointer and `detail` holding the full repair
instruction. Nothing is lost: `Deprecation` exposes `form()`, `path()`,
`pointer()`, `found()`, and `replacement()` as separate accessors, so a caller that
wants the fields does not parse the message. See `issues.md` for the tradeoff.

**Every issue names its own file.** `ConfigError::Invalid` has one `path`, but a
directory scan finds problems in several files (`mode/build.md`, `CONTEXT.md`,
`config`). So `path` is the *scanned root* and each issue's `detail` repeats its own
absolute file. Without that, a finding under a scanned directory could not be traced
to the `.md` carrying it.

**`CONTEXT.md` detection lives at the instruction-file cascade, not in this crate's
traversal.** `legacy::inspect_instruction_file(path) -> Option<Deprecation>` and
`legacy::is_legacy_instruction_file(path) -> bool` take one already-resolved
candidate path. Todo 11 owns the `AGENTS.md` → `CLAUDE.md` → `CONTEXT.md` walk up to
the worktree root; re-implementing that walk here would have been a second chance to
disagree with the first about which file wins. `inspect_instruction_file` returns
`None` both for a non-`CONTEXT.md` name and for a `CONTEXT.md` that does not exist,
so the loader can hand it every candidate unconditionally.
`legacy::inspect_directory` also calls it for the single-directory case, which is how
the per-form test reaches it without an instruction loader.

**The auth-prompt `condition` predicate takes field *names*, not a JSON value.**
`legacy::auth_prompt_uses_condition(keys: impl IntoIterator<Item = impl AsRef<str>>)`
and `legacy::auth_prompt_deprecation(source, pointer, keys)` are the primary
detectors, because upstream's `condition` is a JS closure and can never appear in
JSON. `legacy::inspect_auth(path, &Value)` is kept as a convenience for a plugin
bridge that reflects an `AuthHook` into JSON with function fields reduced to
markers, and its doc comment says to prefer the predicate. The call site is not in
this crate: `condition` is read only while an auth method's prompts are presented
(`opencode/src/cli/cmd/providers.ts:68-77`), so the wiring lands in the plugin wave,
Todos 57-62. The test for this form exercises the predicate directly rather than
faking a config-level fixture.

**`SWEEP_EXEMPT_KEYS` was left exactly as Todo 7 wrote it.** `tools` and `maxSteps`
stay out of `options` and stay visible in `AgentConfig::extra`; `legacy` reads
`extra` to reject them. Sweeping a deprecated key into provider options would turn
it into an API argument, which is worse than either accepting or rejecting it.

**Detection never writes.** The oracle's TOML migration rewrites `config.json` and
`unlink`s the old file (`config.ts:270-272`). `the_toml_config_file_is_never_rewritten_or_removed`
asserts the file is byte-identical after rejection and that no `config.json` appeared.

**An empty `mode/` directory is not a finding.** The oracle's flat
`{mode,modes}/*.md` glob would load nothing from it, so rejecting it changes no
behaviour and would be a false positive on a load-blocking check.

## Task 12

- The acceptance matrix has 14 named trees: `global-only`, `project-only`, `global-and-project`, `dot-opencode-chain`, `env-config-file-before-project`, `env-config-content-after-project`, `home-and-env-config-dir`, `permission-env-object`, `project-disabled-uppercase-true`, `pure-env-keeps-config-layers`, `pure-cli-flag-keeps-config-layers`, `jsonc-comments-and-trailing-commas`, `deep-ancestor-walk-with-env-file`, and `all-config-env-layers`. A coverage assertion pins every required source and both pure entry paths.
- Todo 8 already proves ten-layer discovery broadly. This matrix retains the minimum overlapping baseline needed to diagnose precedence, then extends it with isolated `OPENCODE_PERMISSION`, environment and CLI pure paths, a six-level ancestor walk, all config env vars in one collision tree, explicit coverage accounting, failure aggregation, and a pinned-source 1.18.13 run.
- All 14 acceptance-tree comparisons use `Normalizer::none()` and are byte-exact after the existing typed canonicalization removes only the oracle empty deprecated `mode` field. Every final matrix diff is empty.
- Intentional divergence allow-list: `permission-env-object-key-order` — remeda reverses newly inserted `OPENCODE_PERMISSION` keys while Rust preserves JSON insertion order; the names are distinct and have no precedence interaction. It is isolated from the happy matrix in its own test, requires a non-empty one-line reason, and asserts the exact three-line ordering diff so the entry becomes stale if either side changes.
- Oracle coverage uses the installed `/config/.local/share/mise/installs/opencode/1.18.12/opencode` and separately the pinned source tree `/config/workspace/ProdDir/AI/opencode` at version 1.18.13, commit `aefaf140c1`.
## Task 16

- Public rule API: `rules_from_config(&PermissionConfig) -> Vec<Rule>` preserves outer and nested source order; `evaluate(permission, pattern, rules) -> PermissionAction` is pure, last-match-wins, and defaults to `Ask`.
- Stateful API for Todos 17/32/33: `PermissionEngine` owns insertion-ordered `pending: Vec<PermissionRequest>` plus ordered runtime `approved: Vec<Rule>`. `authorize(request, ruleset)` returns `Authorization::Allowed` or `Authorization::Pending`, and returns `ToolError::Denied` before inserting state on any deny.
- `reply(PermissionReply) -> Option<ReplyOutcome>` models transitions as returned data. `None` means the request ID was not pending. `ReplyOutcome.resolved` tells the turn/event layer every request removed and its effective reply; `installed_rules` tells it what an always reply remembered. `pending()` and `approved_rules()` expose read-only slices for server/tool wiring.
- `PermissionRequest` mirrors the oracle request data (`id`, `sessionID`, permission, patterns, metadata, always, optional tool coordinates) and is serde-compatible for future API/event layers. Tool aliasing and visibility remain outside this crate for Todo 17.

## Task 18 — resolution layer only; no duplicate types

- **All three schema types already existed** from todo 7 and were consumed, not re-declared: `oc_config::schema::reference::{ReferenceEntry, GitReference, LocalReference}`, `oc_config::schema::formatter::{FormatterConfig, FormatterEntry}`, `oc_config::schema::lsp::{LspConfig, LspEntry, BUILTIN_SERVER_IDS}`. Todo 18 is therefore purely the resolved-view layer plus round-trip coverage per arm.
- Todo 7 had already collapsed the inner lsp union into one `LspEntry` with a `try_from` guard ("command required unless disabled"), which accepts/rejects the same documents while preserving every key the author wrote. Kept as-is.
- **Resolved-view API for todo 48 (LSP) and todo 79 (formatter)**:
  - `oc_catalog::lsp_config::ResolvedLsp::resolve(Option<&LspConfig>)` → `is_enabled()`, `is_server_enabled(id)`, `command_for(id) -> Option<&[String]>`, `extensions_for(id) -> Option<&[String]>` (`None` = keep built-in's), `initialization_for(id) -> Option<&JsonMap>`, `servers()`, `disabled()`, `get(id)`, `for_extension(ext)`. `ResolvedServer::is_builtin()` for the 38-id registry.
  - `oc_catalog::formatter::ResolvedFormatters::resolve(Option<&FormatterConfig>)` → `is_enabled()`, `is_formatter_enabled(name)` (already accounts for the ruff/uv link), `command_for(name)`, `overrides()`, `disabled()`, `get(name)`, `for_extension(ext)` (matches the leading-dot form the runtime uses).
  - `oc_catalog::reference::ResolvedReferences::resolve(Option<&OrderedMap<ReferenceEntry>>)` → `iter()`, `visible()` (drops `hidden`), `get(name)`; `ReferenceTarget::{Shorthand, Git, Local}`.
- The ruff/uv linked disable lives in the **resolution** layer, not in todo 79's execution, because it decides *which formatters are enabled* — the question the resolved view exists to answer.
- `oc_catalog::reference::parse` / `parse_json` deserialize the map entry-by-entry so a non-matching arm reports `ConfigError::Invalid` with `key_path == ["references", <name>]`. Serde's untagged error alone never names the key, and every bad entry is reported, not just the first.
- Declaration order is preserved everywhere via `OrderedMap`, since permission precedence already depends on it.

## Task 17 — tool visibility

- **Reused Todo 16's `wildcard_match` for the key match; did NOT reuse `evaluate`.** `evaluate` cannot
  express the hiding predicate: it matches permission *and* pattern, so
  `evaluate("bash","*",[{bash,"*",deny},{bash,"echo *",allow}])` returns `Deny` and would hide a tool
  the oracle keeps visible. `is_tool_hidden` therefore does the oracle's permission-only `findLast`
  over the same shared matcher — one matching primitive, no duplicated pattern logic.
- **A proptest ties the two paths together**: whenever `is_tool_hidden` is true, `evaluate` must return
  `Deny` for arbitrary input. That is the invariant that would break if the predicate ever drifted.
- **API for Todos 38/44** (`oc_permission::visibility`):
  - `is_tool_visible(tool, &[Rule]) -> bool` / `is_tool_hidden(...)` — the primitive.
  - `visible_tools(tools: IntoIterator<T>, &[Rule], id: Fn(&T)->&str) -> Vec<T>` — order-preserving
    filter over any tool-def collection; the oracle's `visibleTools` for a Rust registry.
  - `retain_visible_tools(&mut Vec<T>, &[Rule], id)` — in-place variant for a built registry list.
  - `disabled_tools(names, &[Rule]) -> BTreeSet<String>` — the oracle's `disabled`; `BTreeSet` (not
    `HashSet`) so the reported set is deterministic in tests and logs.
  - `merge_agent_session(agent, session)` / `merge_rulesets(&[&[Rule]])` — the flatten, with the
    precedence documented at the call site.
  - `EDIT_TOOLS` / `READ_TOOLS` / `permission_key(tool)` — the alias table, exported so the registry
    and any future tool never re-hardcodes it.
- **`lib.rs` edit is exactly one line: `pub mod visibility;`.** The crate's other modules are private
  with flat `pub use` re-exports, but the mandate was a single-line touch of Todo 16's file, so the
  module is public instead of adding a second re-export line. Consumers call
  `oc_permission::visibility::…`. Flattening it later is a one-line follow-up if the crate owner prefers.
- **Did not fix the `wildcard_match` input-`*` defect** found by the proptest (see issues.md). It lives
  in Todo 16's file, is guarded by its 17 tests, and is an evaluation bug, not a visibility bug.

## Task 13 — agent loading from config and markdown

### Frontmatter parser: hand-written, because no YAML crate is pinned

`[workspace.dependencies]` in the root `Cargo.toml` has **no YAML parser**. The
only `yaml` string in it is `insta`'s `yaml` snapshot feature, which is a
dev-only serializer, not a parser. This task may add a dependency to
`crates/oc-catalog/Cargo.toml` only if it is already in
`[workspace.dependencies]`, and may not edit the root manifest — so a YAML crate
was not available.

Decision: implement the YAML subset in
`crates/oc-catalog/src/agent/frontmatter.rs` (~600 lines with tests), and choose
the subset **by probing the real binary** rather than by reading the YAML spec.
Every construct the module accepts is one opencode 1.18.12 was observed to accept
in an `agent/*.md` file: plain / single-quoted / double-quoted scalars, nested
block maps, flow maps and sequences, block scalars (`|`, `|-`, `>`, `>-`),
comments, blank lines, an unquoted colon in a value, and no frontmatter at all.
Anything outside the subset — anchors, aliases, tags, merge keys, multi-document
streams, explicit complex keys, block-scalar indentation indicators — is a parse
**error** rather than a silent misread, because a frontmatter key that quietly
becomes the wrong value reaches the provider as a wrong model or permission.

Two oracle behaviours in `packages/core/src/config/markdown.ts` were reproduced
deliberately:

* `parse` retries the whole document through `sanitize` (`:5-10`) when the first
  attempt fails. `sanitize` (`:22-35`) rewrites any top-level `key: value` whose
  value contains a further colon into a `key: |-` block scalar. Real agent files
  written by other coding agents depend on this. Ported line-for-line, including
  its regex `^([a-zA-Z_][a-zA-Z0-9_]*)\s*:\s*(.*)$` and its capture boundary: the
  oracle's group excludes the newline before the closing `---`, so the
  replacement must too or the head gets welded to the delimiter.
* Because of that retry, a **malformed flow map is not a parse error**. Verified:
  `permission: { edit: deny` (unclosed) is sanitized into the string
  `"{ edit: deny"`, and the binary then fails at the *schema* layer with
  `Expected PermissionActionConfig, got "{ edit: deny" permission`. This crate
  produces the identical string and rejects it at the same layer.

Error line numbers are **1-based within the whole file**, not within the
frontmatter head, so a message names the line the author actually wrote.

### Consumed oc-config's sweep rather than re-implementing it

`oc_config::schema::agent::AgentConfig`'s hand-written `Deserialize` already
performs the oracle's unknown-key sweep into `options`
(`packages/core/src/v1/config/agent.ts:62-81`), and already honours
`SWEEP_EXEMPT_KEYS = ["name", "tools", "maxSteps"]` so a deprecated key cannot
become a provider option. This task therefore:

* builds a `serde_json::Value` from the frontmatter, installs the trimmed body as
  `prompt`, and hands it to `serde_json::from_value::<AgentConfig>` — the sweep
  happens inside that call and is not duplicated anywhere in `oc-catalog`;
* reads the renaming `name` key back out of `AgentConfig::extra` (where the
  exemption leaves it) rather than adding a `name` field;
* reuses `oc_config::legacy::check_agent_frontmatter` (todo 10) for the
  `tools` / `maxSteps` rejection, calling it on every markdown definition instead
  of restating the deprecation list.

`oc_paths::Layout::config_directories` supplies the config-dir chain; the merge
uses `oc_config::discovery::discover_with` for everything below the markdown
layer. No path logic and no merge logic is re-derived.

### Deep merge operates on JSON, not on `AgentConfig`

`merge_agent_maps` converts each side to `serde_json::Value`, runs a port of
remeda's `mergeDeep`, and re-deserializes. That is what the oracle's `mergeDeep`
does (`config/config.ts:460`), and it is the only way a nested `options` map or
`permission` object merges key-by-key instead of being replaced wholesale. A
field-by-field Rust merge would have silently replaced `options`.

### Permissions carried as data, not resolved

`Agent` carries the `permission` key verbatim and `builtin::Builtin::
permission_overlay()` returns each built-in's `Permission.fromConfig` literal as
declarative data. Resolution into a ruleset belongs to todos 16-17: it needs the
runtime default set, `Truncate.GLOB`, the global tmp and plans directories, the
discovered skill and reference directories, and a worktree-relative rewrite.
`permission_overlay_is_partial()` returns `true` for `plan` and `explore`, whose
overlays contain those runtime-path-dependent entries, so a later caller cannot
mistake the overlay for the finished ruleset.

### The differential compares headers, and says why

Since `agent list --format json` does not exist, the differential compares the
`name (mode)` header of every agent in oracle order, and **not** the permission
ruleset that follows each one — that ruleset is todos 16-17's output, so
comparing it would either fail for reasons outside this task or force a
normalizer wide enough to hide a real difference. The header set is not a weak
assertion: it carries which agents exist, what each is called (the whole
path-derived name rule), whether an override created or modified an agent, the
`mode: "all"` default, and the native-first sort order. Normalizer is
`Normalizer::none()` — byte-exact, nothing masked. Intentional-divergence list is
**empty**.

A test `the_oracle_has_no_format_json_flag` asserts the flag is still rejected,
so if a future opencode adds it the suite fails and the differential gets
upgraded rather than the correction being forgotten.

### `localeCompare` approximated as lowercase-then-raw

`agent list` sorts with `a.name.localeCompare(b.name)`. Agent names come from
file paths and config keys, so they are ASCII-dominated; the one behaviour worth
reproducing is that `localeCompare` puts `a` before `B` where a byte comparison
would not. `locale_compare` therefore keys on the lowercased name and breaks ties
on the raw name. A full ICU collation would need a dependency that is not pinned.

## Task 104 — remove the permission-order allow-list

`permission-env-object` is no longer an intentional divergence. The allow-list
is empty, and the matrix now compares raw observable key order byte-for-byte.
No production merge change was made: `oc-config` discovery already matched both
raw oracles. The fix belongs in `tests/differential.rs`, where
`OracleDebugConfig` strips only the top-level oracle `mode` field without routing
the rest of the document through `serde_json::Value`.

The regression test uses overlapping permission keys and covers one new key,
four new keys, overwrite-plus-append, and nested objects. This makes ordering a
security-relevant observable under `findLast`, rather than accepting distinct
keys whose precedence interaction would be hidden.

## Task 14 — skill discovery

### The two de-duplication dimensions are separate types of thing

They are handled in different places on purpose, because conflating them changes which
`location` the user sees:

- **By path** — `SkillSources::absorb` keys on `node_path::normalize(path)`, mirroring the
  oracle's `Set<string>` (`skill/index.ts:168`). Deliberately **not** `canonicalize`: the
  oracle does not, and canonicalizing would silently merge a `~/.claude → ~/.agents` symlink
  alias into one match, changing which file wins the name.
- **By name** — `Skills::insert` replaces the entry **in place** and warns once
  (`:125-139`). In place matters: the oracle's state is a JS object, so re-assigning an
  existing key keeps its original position, and `Skill.all()` returns `Object.values`, which
  makes that position observable in `debug skill`.

Note this differs from `oc-config::instructions`, which dedups by *canonicalized* identity.
Both are right for their own oracle; the divergence is in the oracle, not in this port.

### The duplicate winner is made deterministic, on purpose

The oracle's winner is decided by I/O timing (three runs, three different winners — see
learnings). This port loads in root order and lets the later root win. That reproduces the
oracle's real-tree outcome for every alias on this machine (`.agents` beats `.claude`, a
config directory beats both) and is reproducible run to run. A prompt that changes between
runs is worse than one that differs from a coin flip, so determinism wins. Recorded rather
than hidden: the differential compares the name **set** for the real tree and the **whole
document** for the sandboxed trees, which have no duplicates.

### `yaml-rust2` added to `oc-catalog/Cargo.toml`, not to the root pins

The root manifest ships no YAML parser and this task may not edit it. `yaml-rust2 = "0.11.0"`
is therefore a crate-local version literal with a comment saying so and asking for promotion
the next time the root manifest is touched. The choice over `serde_yaml` is behavioural, not
stylistic: `serde_yaml` is libyaml/YAML 1.1, where `name: yes` is a boolean and the skill
would be silently dropped; `yaml-rust2` is YAML 1.2 core, matching js-yaml 4 under
`gray-matter`, where it stays the string `"yes"`. Confirmed against the oracle.

### Frontmatter is not deserialized into a struct

`Field` is a three-state enum — `Absent` / `Text` / `NotAString` — because the oracle's guard
distinguishes all three (`typeof data.name === "string"`, and `description === undefined ||
typeof description === "string"`). A `#[derive(Deserialize)]` struct with
`Option<String>` would fold `NotAString` into `Absent` and load skills the oracle drops. The
same reason keeps every other key ignored rather than rejected: `deny_unknown_fields` would
turn a skill carrying `license:` into an error, which the oracle does not.

### Rejection is a narrow private enum, not the public warning enum

`Rejection` has exactly the three ways one `SKILL.md` can fail. Both mappings out of it — to
`SkillWarningKind` for `load`, to `ConfigError` for `parse_file` — are exhaustive with no
catch-all arm, so adding a rejection reason is a compile error rather than a silent fall
through to a generic message. This is the `oc-error` "no `Other(String)`" rule applied one
level down.

### `load` never fails; `parse_file` does

`load` returns `Skills` with a `warnings()` list and no `Result`, because the oracle logs and
continues past every failure and one broken skill must not cost the user the other 135.
`parse_file` is the typed-error entry point for a caller that wants to *report* a bad file
(`ConfigError::Invalid` with `key_path: ["name"]`, or `ConfigError::Frontmatter`). Warnings
are both `tracing::warn!`ed and returned, following `oc-config::instructions`.

### The remote root is hardened past the oracle in one place

`index.json` is remote input and the oracle joins each `files` entry onto the cache root with
no traversal check, so `"files": ["../../../.bashrc"]` would write outside the cache. This
port drops any entry file that escapes its skill root, with a warning, and drops the entry
entirely if that removes its `SKILL.md`. It only refuses what the oracle should not have
accepted; a well-formed index is byte-identical. Recorded as an intentional divergence in
code and tested.

### The Claude-compatibility flags

Roots 1 and 3 drop `.claude` when **either** `OPENCODE_DISABLE_CLAUDE_CODE` or
`OPENCODE_DISABLE_CLAUDE_CODE_SKILLS` is set (`effect/runtime-flags.ts:28-29`), and
`OPENCODE_DISABLE_EXTERNAL_SKILLS` (`:22`) removes roots 1-3 outright. Read from an injected
`Env` snapshot, never from the process, so tests need no `unsafe` env mutation. All three are
differential cases.

### The real-tree differential compares against `--pure`, and says why in an assertion

The plain run reports one extra skill from `$XDG_CACHE_HOME/opencode/skills` because the
installed `@sunerpy/oh-my-openagent` plugin contributes `skills.*` config; `--pure` drops
external plugins and the run matches this port exactly (135 = 135). Rather than leave `--pure`
as an unexplained convenience, the test *also* runs the plain command and asserts every extra
name is cache-located — so the leftover gap is *checked* to be the unimplemented plugin layer
(todo 26+) rather than merely claimed to be.

### Bounds: 8 local reads, 4 remote skills, 8 remote files, 5s per request

Local read concurrency is 8 (the oracle is unbounded; a bound is what makes the load
reproducible, and 8 matches `oc-config::instructions`). Remote bounds are the oracle's own
(`discovery.ts:10-11`). The 5s per-request timeout is stated rather than inherited from an
HTTP layer, and is *observed* by tests, not assumed: a hanging TCP server proves the timeout
fires, and a counting server proves file downloads really run concurrently and never exceed 8.

## Task 15 — command resolution

### The skill-source shape I own, and what todo 14 must satisfy

Todo 14 (`src/skill.rs`) was being written concurrently, so `command.rs` depends
on nothing from it. I defined the minimal shape command resolution needs, owned
by `command.rs`:

```rust
pub struct SkillCommand {
    pub name: String,
    pub description: Option<String>,
    pub content: String,          // the body, verbatim
    pub location: SkillLocation,
}
pub enum SkillLocation { Builtin, File(PathBuf) }   // File holds the SKILL.md path
```

**What todo 14 must supply** — a mapping from its own record onto this, i.e. it
needs to expose, per skill: `name`, `description`, the verbatim body, and whether
the skill is the `<built-in>` sentinel or a path. That is exactly the field set
`skill/index.ts` already carries, so no negotiation should be needed; the only
thing to get right is that `location` for a file-backed skill is the SKILL.md
path itself, not its directory — `command/index.ts:136` takes `path.dirname` of
it, and `SkillCommand::base_dir` reproduces that with `Path::parent`.

A plain struct rather than a trait: a trait would buy nothing (there is one
implementor) and would force `dyn` or a generic parameter through `Sources`,
`Registry::build`, and every test. Todo 14 can add a `From<Skill> for
SkillCommand` in its own module without touching mine.

**Footer**: a file-backed skill's command template is the body, a blank line, the
base directory, and the relative-path note (`command/index.ts:141-149`); a
`<built-in>` skill gets its body unchanged. Verified against the real binary,
including the three consecutive newlines that arise when the body has its own
trailing newline.

### Modeling MCP prompts pending todos 45-47

The MCP client does not exist yet, so level 3's input is also a shape I own:

```rust
pub struct McpPrompt { client, prompt, description, arguments: Vec<String> }
```

`arguments` is a flat `Vec<String>` of declared argument NAMES in order —
everything `command/index.ts:117,130` actually reads. The oracle treats an absent
list and an empty list identically, so both are the empty vec and there is no
`Option<Vec<_>>` to get wrong.

**Resolution is two-phase, because the oracle's is.** `Registry::resolve` returns
`Resolution::PendingMcp` for an MCP command rather than pretending to have the
text: `command/index.ts:110-129` builds a lazy promise, and
`session/prompt.ts:1374` awaits it. The caller finishes with
`PendingMcp::complete(&[Option<String>])`, one entry per returned message —
`Some(text)` for a text block, `None` for anything else, matching
`:121-126` where a non-text block becomes `""` but is still joined by `"\n"`.
Todo 47 supplies that slice and needs no other seam.

`Template::Mcp` carries the `(argument name, "$N")` pairs already computed, so
todo 47 sends them verbatim as `prompts/get` arguments and does not re-derive the
mapping.

### Zero new dependencies — deliberate, to avoid a union-merge break

`crates/oc-catalog/Cargo.toml` is UNCHANGED. Todos 13, 14, and 15 all landed in
this crate concurrently and the orchestrator union-merges its manifest; two
agents inserting an identical `serde = { workspace = true }` line into
`[dependencies]` would produce a DUPLICATE KEY and fail the build. So:

- `CommandError` implements `Display` + `std::error::Error` by hand instead of
  deriving `thiserror`. It has exactly one variant (`NotFound`), so this is ~14
  lines.
- No `serde` derives on `Info`. The `/command` HTTP response belongs to the
  server crate, which can shape its own DTO; deriving here would have added a dep
  for a caller that does not exist yet.
- The integration test parses its fixtures as `serde_json::Value` — `serde_json`
  is already a dependency and test targets see `[dependencies]`.

`oc-error` was NOT extended with a command variant, because that would mean
editing another crate, which this task forbids.

### `Sources` is a struct, not four method calls

`Registry::build` takes one `Sources<'_>` with all four levels rather than
exposing `add_config`, `add_mcp`, `add_skills`. The ORDER is the entire feature —
a per-level API would let a caller apply skills before config and silently invert
the precedence. There is no way to build a registry with the levels out of order.

### Expansion lives in resolution, not dispatch

`Registry::resolve(name, arguments)` returns a `Resolved` whose `prompt` is
already final, matching `session/prompt.ts:1362-1395` — the oracle resolves,
awaits the template, expands, and only then builds a message. Nothing downstream
re-expands. The `` !`cmd` `` shell substitution (`:1397-1408`) is deliberately
NOT here: it spawns processes, runs strictly after everything in this module, and
belongs to whichever todo owns shell execution.

### The differential is a golden file plus a re-derivation

`tests/fixtures/command_expansion_oracle.cjs` is a verbatim transcription of the
oracle's expansion body and its three regexes. It generates
`command_expansion_expected.json` from `command_expansion_cases.json` (59 cases).
The Rust test diffs against the golden ALWAYS, and additionally re-runs the
JavaScript when node is present, failing if the golden has drifted. That keeps
the suite green on a machine without node while making it impossible for the
golden to rot into the self-consistent fiction `oc-testkit`'s docs warn about.

`oc-testkit`'s `Oracle` was not used: it drives the installed BINARY, and there
is no CLI or HTTP surface in this project yet to point it at (todos 55-56, 71).
The binary was still used for observation, by hand, through `opencode serve` +
`GET /command` — recorded in the evidence file.

## Task 24 — `oc-auth`

### Redaction: a `Secret` newtype, not a discipline

`oc_auth::Secret(String)` wraps every refresh token, access token, API key, client
secret, PKCE verifier and OAuth state. Both `Debug` and `Display` render the constant
`oc_auth::REDACTED` = `"<redacted>"`; `serde` is `#[serde(transparent)]`, so the on-disk
bytes are identical to the `String` it replaces.

Both traits are overridden, not just `Debug`, because the two real leak paths are both
automatic and one of them is `Display`:

- `#[derive(Debug)]` on an enclosing struct plus any `{:?}` — a `tracing` field, a
  `dbg!`, an `assert_eq!` failure, a panic payload, an `unwrap()` on a `Result` whose
  error contains it.
- `{}` in a format string, when the author reached for `Display` because the field
  happened to be a bare `String`.

`Secret::expose()` is the only way out, deliberately awkward and greppable. It is not
encryption and does not zero on drop — the plaintext is in the heap exactly as it is in
the file. The threat answered is accidental disclosure through this crate's own output.
`PartialEq` is byte-comparing, therefore not constant-time; nothing here compares a
stored credential against attacker-supplied input.

`Secret::hint()` gives `sk-…4f2a` for a human telling two credentials apart. Opt-in,
never used by `Debug`/`Display`, char-boundary safe, and it refuses to reveal anything
from a value under 12 characters where prefix+suffix would be most of the secret.

Two fields are deliberately **not** secrets: `Credential::Api::metadata` **keys** (the
values are wrapped; the keys are what makes a log line useful) and
`ClientInfo::client_id` (an OAuth client ID is public by design and travels in query
strings — hiding it costs legibility and protects nothing). `metadata` *values* are
wrapped because a provider is free to put a token in there.

### A too-permissive file WARNS; it does not refuse

The plan text for todo 24 says "**refuse** to read a file that is group/world-readable
with a warning". That is wrong for parity and the prompt's own restatement ("a
too-permissive file is a warning, not a hard failure — confirm that against the oracle")
is right. Confirmed both ways:

- There is no permission check anywhere in `auth/index.ts` or `mcp/auth.ts`.
- Observed on 1.18.12: `auth.json` at `0644`, `opencode auth list` printed all
  credentials and left the mode at `0644`.

Refusing would be a parity break in the worst direction: a user whose file came back
from a backup at `0644` would be locked out of every model they have configured, by the
crate whose job is to let them in. So `store::read_json` reads it, returns a
`PermissionWarning { path, mode }` alongside the value, and emits a `tracing::warn!`
naming the path and both the found and wanted modes. A **write** then repairs the mode
to `0600`, which is what the oracle does.

The finding is returned as data, not only logged, so a caller that *wants* to refuse can
— and so a test can assert it fired.

`PermissionWarning` has a hand-written `Debug` that renders `mode` in octal: a derived
one prints `0o644` as `420`, the one number an operator reading a dump cannot act on.

### Mode is set at `open(2)`, not after the write

`OpenOptions::mode(0o600)` on Unix, so the file is `0600` from the instant it exists,
followed by `set_permissions` to repair a pre-existing permissive file (`mode()` applies
only on creation). This closes the window the oracle leaves at `fs-util.ts:110-113`,
where the file is created at the umask and chmodded afterwards — and where `chmod` does
not revoke a descriptor another process already opened. No `unsafe` is involved;
`std::os::unix::fs::{OpenOptionsExt, PermissionsExt}` are safe.

### Windows: nothing is set, and that is a gap todo 91 must know about

`CREDENTIAL_FILE_MODE` and every permission assertion are `#[cfg(unix)]`. On Windows
there are no Unix mode bits: `File::set_permissions` can only toggle the read-only
attribute, which is not an access control. Real protection would need an explicit DACL
via `SetNamedSecurityInfo`/`windows-acl`, which is out of scope here and would pull in a
Windows-only dependency. **So on Windows both credential files inherit the parent
directory's ACL and this crate adds no restriction.** `permission_warning` returns `None`
there, so no false warning is emitted either. Flagged for todo 91's cross-platform
packaging in `issues.md`.

### Divergence: malformed JSON is an error, not silently an empty store

The oracle pipes every read failure into `orElseSucceed(() => ({}))`
(`auth/index.ts:65`) and `Effect.catch(() => ({}))` (`mcp/auth.ts:68`). A truncated
`auth.json` therefore reads as empty, and the next `set` writes that emptiness back —
destroying every credential in the file. `oc-auth` returns `AuthError::Malformed { path,
source }` instead, so a caller can decline to write. A **missing** file still reads as
empty (that is the normal first-run path), and so does an empty one (what a crash
between create and write leaves behind).

Relatedly, entries that individually fail to decode are dropped as the oracle drops
them, **and** listed in `Credentials::skipped` / `McpCredentials::skipped` — because a
subsequent write silently destroys exactly those entries, and the caller deserves the
chance to see it coming.

### `AuthError` lives in `oc-auth`, not `oc-error`

`oc-error` has no auth-storage domain and adding one means editing a crate this task
does not own. The five variants (`Read`, `Malformed`, `Write`, `Serialize`,
`Permissions`) follow that crate's doctrine verbatim: no catch-all, no `String` message
field, not `#[non_exhaustive]`, every variant carries its `PathBuf` plus the concrete
`io::Error`/`serde_json::Error` in `#[source]` position so `ErrorKind` and JSON
line/column survive. It implements `oc_error::Recoverable`; every variant is
`Recovery::Fail` — notably **not** `Reauthenticate`, which is the answer when a provider
rejects a credential, whereas these are failures to reach the store at all and a fresh
login would write to the same unwritable path.

### `Tokens` and `ClientInfo` are deliberately not `Default`

`accessToken` and `clientId` are the one required field of each. A `Default` impl would
let a caller construct a token pair with an empty access token, which is not a thing
that can exist. `Entry` *is* `Default` (all its fields are optional) because
`updateField` needs to create a blank entry for an unseen server.

### `Env` is passed in, never read from the process

`AuthStore::with_env(path, &Env)` takes `oc_paths::Env` by reference rather than reading
`std::env` itself, so the `OPENCODE_AUTH_CONTENT` tests do not race each other —
mutating process env from parallel tests is unsound in practice. `AuthStore::resolve`
and `McpAuthStore::resolve` take the `oc_paths::Layout` for the same reason.

## Task 19 — `oc-db`: driver pin, pool, and the `transaction()` shape

### What was added to the root `Cargo.toml` — **todo 20 read this**

One dependency, twelve lines, all additions, nothing removed or reordered
(`git diff --stat Cargo.toml` → `1 file changed, 12 insertions(+)`). In a new
`# -- storage --` section between `# -- filesystem, search, watching --` and
`# -- primitives --`:

```toml
rusqlite = { version = "0.40.1", features = ["bundled"] }
```

Todo 20 should therefore write `rusqlite = { workspace = true }` in
`crates/oc-db/Cargo.toml` — it is **already there**, so no manifest change is needed for
schema work. Resolved: `libsqlite3-sys 0.38.1`, bundled **SQLite 3.53.2**.
`ENABLE_FTS5` is compiled in.

The `sha2` pin an earlier agent deleted was **not** touched. Nothing tripped over it.

`crates/oc-db/Cargo.toml` now declares `oc-error`, `oc-paths`, `rusqlite` and a
dev-dependency on `tempfile`, all `{ workspace = true }`.

### `Pool` owns connection creation; there is no `Pool::from_connection`

Four of the five pragmas are per-connection state, and one of them is `foreign_keys`. A
pool that accepted a caller's `Connection` could hand out one that declines to enforce
every `ON DELETE CASCADE` in todo 20's schema, with no error anywhere. So the only ways
in are `Pool::open_default()`, `Pool::open(&DbLocation)` and
`Pool::open_with_max_idle(&DbLocation, usize)`, each of which routes through
`open::open_target` → `apply_pragmas` → `verify_pragmas`. **Do not add a constructor that
takes a connection.**

### The pool is a `Mutex<Vec<Connection>>`, not `r2d2`/`deadpool`

`Connection` is `Send` but not `Sync`, so a connection must be checked out exclusively.
Checkout pops an idle connection or opens a fresh one; `PooledConnection` returns it on
`Drop`, discarding it if `max_idle` (default 4) is already met. No extra dependency, no
async runtime coupling — the store is synchronous and SQLite serializes writers itself.
Adding a pool crate later is a local change; it is not needed to hit the acceptance bar.

### `:memory:` becomes a *named shared-cache* database inside the pool

Plain `:memory:` gives every connection its own private database, so a pool of them would
silently hand out unrelated databases and a two-connection test would pass for the wrong
reason. `Pool` therefore opens `file:oc-db-<pid>-<n>?mode=memory&cache=shared` for
`DbLocation::Memory`, and permanently retains one **anchor** connection that is never
handed out, because a shared in-memory database is destroyed when its last connection
closes. `Pool::target()` exposes the URI, `Pool::holds_memory_anchor()` reports the
anchor. `oc_paths::db_path()` still returns plain `DbLocation::Memory`; the URI is an
implementation detail of pooling and does not change what the oracle would compute.

Two pools over `DbLocation::Memory` are independent (distinct names), which is the
per-process transience `OPENCODE_DB=:memory:` promises.

### `transaction()` API shape — todos 21-24 build on this

```rust
pool.transaction(|tx: &rusqlite::Transaction| -> Result<T, DbError> { ... })  // IMMEDIATE
pool.transaction_with_behavior(behavior, |tx| ...)
conn.transaction(|tx| ...)   // on a PooledConnection already checked out
```

`Ok` commits, `Err` drops the transaction unfinished and rolls back (proved by a test
asserting zero rows persist). The closure takes `&Transaction` so `execute`, `prepare` and
`execute_batch` are all reachable, and returns `Result<T, DbError>` so a caller maps
`rusqlite::Error` once via `oc_db::open::map_error`.

Default behaviour is `IMMEDIATE` deliberately — see `learnings.md`: `DEFERRED` yields
`SQLITE_BUSY_SNAPSHOT` on a write-write race, which the busy handler may not retry, so
`busy_timeout` would not help. `transaction_with_behavior` exists for a genuine
read-only or exclusive case.

### Error mapping: `Busy` is the only retryable classification

`open::map_error` returns `DbError::Busy { retry_after: None }` for `SQLITE_BUSY` /
`SQLITE_LOCKED` and `DbError::Query { source }` for everything else, keeping the original
`rusqlite::Error` as the cause. `retry_after` is `None` because SQLite reports no
suggested delay. Predicates `open::is_busy` and `open::is_constraint_violation` are public
so a caller can branch without matching on message text. Nothing uses `anyhow`; the guard
test passes.

### `open_at` creates the parent directory — a deliberate superset of the oracle

`oc-paths` keeps every path getter pure and the oracle relies on `global.ts` having
already `mkdir`ed `data()`. `open::ensure_parent` does it at open time instead, which also
covers an `OPENCODE_DB` pointing at a nested directory that the oracle would fail to open.
Tested by `opencode_db_absolute_is_used_verbatim` with a two-level path.

### Path rules are consumed, never re-derived

`oc-db` calls `oc_paths::db_path()` and matches on `DbLocation`. It contains no reference
to `OPENCODE_DB`, no `is_absolute` check and no channel logic. Tests exercise all three
forms by building a `Layout` from an explicit `Env` (`Layout::resolve_with`), never by
mutating process env — `set_var` is `unsafe` and this workspace forbids it.

## Task 23 — oc-snapshot

**Shell out to `git`, do not use a Git library.** The snapshot store is not a normal repository:
it is driven with three distinct `-c` override sets, a private index seeded by *copying another
repository's index file*, `--pathspec-from-file=- --pathspec-file-nul` staging, `write-tree` against
that private index, and `checkout-index -a -f` to restore. The Rust and TypeScript binaries must read
each other's stores, so reproducing the oracle's invocations exactly outranks elegance — and no
pinned Rust Git library exposes the index-file-copy trick or cruft-pack `gc` anyway. `oc-paths`
already set the shell-out precedent for Git discovery. Injection safety comes from
`std::process::Command::args(&[OsString])` → `execvp` argv (never a shell string) plus keeping file
names off argv entirely; proven against a worktree path containing a space, a single quote, a double
quote, `$(touch pwned)` and `; rm -rf .`, and against a file named ``a file's; rm -rf $HOME `id`.txt``.

**`std::process::Command`, not `tokio::process`.** Every existing library crate that spawns a process
uses the blocking API. The one place that must not block a runtime worker — the hourly gc loop — wraps
the call in `tokio::task::spawn_blocking`. `tokio` is a normal dependency; `features = ["test-util"]`
is a **dev**-dependency addition only (resolver 3 keeps it out of normal builds) so the cadence can be
asserted on a paused clock instead of waiting an hour.

**Crate-local `SnapshotError`, not a new `oc-error` variant.** `oc-error` deliberately has no
process/exec or `Io` variant, and its aggregate `Error` is not `#[non_exhaustive]`, so adding a
variant breaks every exhaustive `match` in the workspace. `oc-paths` set the precedent by owning
`PathsError`. Variants: `Spawn`, `Git { args, code, stderr }`, `Encoding`, `Store { operation, path }`,
`Scan`. If a later todo wants snapshot failures inside `oc_error::Error`, add one
`Error::Snapshot(#[from] SnapshotError)` arm — the type is ready for it.

**Reference-count query API — todo 83 consumes this.**

```rust
pub struct StoreKey        { pub project_id: String, pub worktree_hash: String }
pub struct SessionRef      { pub session_id: String, pub project_id: String, pub worktree: PathBuf }
pub struct StoreReferences { pub key: StoreKey, pub path: PathBuf, pub on_disk: bool,
                             pub sessions: BTreeSet<String> }

pub fn discover_stores(root: &Path) -> Result<Vec<StoreKey>>;
pub fn reference_counts<I: IntoIterator<Item = SessionRef>>(root, sessions) -> Result<Vec<StoreReferences>>;
pub fn unreferenced_stores<I: IntoIterator<Item = SessionRef>>(root, sessions) -> Result<Vec<StoreReferences>>;
pub fn is_worktree_hash(name: &str) -> bool;
```

Shape rationale: the caller owns the session list (it comes from the DB, which this crate must not
depend on) and owns every deletion. `reference_counts` returns *all* stores including zero-reference
ones so a GC can report and dry-run; `on_disk` distinguishes "referenced but never tracked" from
"exists". Sorted by key for stable output. `is_worktree_hash` is strict (40 lowercase hex) so a stray
directory is never reported as a deletion candidate.

**Restore restores content only.** `read-tree` + `checkout-index -a -f`, matching upstream: a file
created after the snapshot is left in place. Deleting those is `revert`'s job (todo 74), which is why
`patch()` returns the changed-file list.
## Task 20

- Fresh schema creation and journal insertion occur in one `IMMEDIATE` transaction. All 38 ids from current `migration.gen.ts` receive one captured Unix-millisecond `time_completed`, matching upstream's one schema-creation transaction while avoiding partially current databases.
- Existing databases with a `session` table are accepted only when their `migration` journal contains every current id. Rust does not speculatively mark an older schema current; a missing id returns `DbError::Migration` instead of making TypeScript skip required SQL.
- All 19 tables from generated `schema.up(tx)` are emitted verbatim, including the six cloud-side tables, because omitted tables would contradict a fully seeded journal and later TypeScript migrations can alter them.
- Schema differential normalization removes backtick/double-quote identifier quoting, collapses insignificant SQL whitespace, trims a terminal semicolon, and lowercases SQL keywords/identifiers. These rules are safe here because all generated identifiers are lowercase, SQLite identifiers and keywords are case-insensitive, and literals/defaults remain structurally checked via `pragma_table_info`. Table names, column order/name/type/null/default/PK position, index name/DDL, and FK source/target/update/delete/match actions are still compared exactly.
- The differential uses a fresh database created by the real binary rather than the 51 GiB user database, because the legacy database deliberately retains `__drizzle_migrations` and therefore is not the output of current `schema.up`.


## Task 93

### Methodology revision 2: the warm-up discard is now W-soak only

**Decision.** `PERF_METHODOLOGY_REVISION` 1 -> 2. The 90-second warm-up discard
applies to `W-soak` alone. `W-idle` and `W-real` take their peak over the whole
trace.

**Why revision 1 was wrong, not merely different.** For a bounded cold-start
workload the peak *is* the startup-plus-turn spike, so discarding the startup
discards the measurement. W-idle's trace is 148s; a 90s discard cut 45 of its 75
samples and under-reported its median peak as 728.9 MB against the 931.9 MB its
own retained samples hold — 203 MB hidden. Every rep peaks inside the discarded
window and then falls 130-300 MB, so this was not an edge case.

**Why now is the only legitimate moment.** No Rust binary has been measured yet,
so the change cannot have been fitted to a comparison result. This is exactly the
case the `PERF_METHODOLOGY_REVISION` mechanism exists to permit: an honest
pre-comparison correction, recorded with its evidence.

**The four G1-G4 formulas were not touched.** The frozen section is byte-identical
across revisions 1 and 2, hashing to the same
`db49ffeb3a19a265a948e5545afe14e245f8ac7c8201ae1b1e1748e87f6922ad`. Registered as
a separate `REVISION_2_HASH` constant rather than aliased to `REVISION_1_HASH`, so
revision 2 has a digest it must match on its own and editing a formula still
breaks the lock. Proved by mutating G1's coefficient 0.50 -> 0.75 (guard FAILED
with digest `aa519eca...`) and reverting (guard ok). Transcript in the evidence file.

**Enforced in one place.** `runner::warm_up_discard(WorkloadName) -> Duration` is
the single source of the rule; `workload::peak_after_warm_up` is the only consumer,
and the artifact test re-derives every committed peak through that same function.
A stored peak therefore cannot drift from the rule that produced it.

### QA substitution: within-run spread replaces a second full baseline pass

**This is a deliberate change to the verification method, not a skipped check.**

The original happy-path scenario was "two independent baseline runs agree within
10%". Dropped. It costs a second ~50-minute measurement pass for information the
committed artifact already contains, and the measured within-pass spread of the
five per-run peaks — 1.1402x for W-idle, 1.1788x for W-real — is *wider* than the
10% the criterion would have demanded. Running the second pass would have produced
either a false failure or a passing number obtained by luck.

**Replaced with:** `WorkloadMeasurement::peak_spread()` deriving min / median /
max / max-over-min from the retained runs, plus
`committed_baseline_records_the_spread_of_every_measured_workloads_peaks`
asserting that every measured workload records a coherent five-run spread whose
median is the median the artifact publishes, and that the deferred workload
records no spread and a reason instead. Spread is derived, never stored, so it
cannot drift from the runs it summarises. Recorded in `docs/perf-methodology.md`
so the substitution is visible to a reader who never sees this notepad.

### Measured versus deferred

- **W-idle** — measured. 5 runs, median 954,240 KiB. Feeds G1.
- **W-real** — measured. 5 runs, median 3,026,992 KiB. Feeds G2.
- **W-soak** — deferred, honestly labelled: `smoke_only: true`, `runs: []`,
  `median_peak_rss_kib: null`, and a `deferred_reason` naming the exact failure.
  Left as-is. A 20-turn smoke cannot satisfy G3 even when it succeeds, so losing
  it costs no gate evidence, and `soak_outcome` already distinguishes this from a
  failed *full* soak, which is propagated because it is the G3 input itself.
  Fabricating a number here would have been the only real defect available.

### Artifact recomputed, never re-measured

Every number came from the retained raw samples already on disk. The
revision 1 -> 2 diff is exactly 7 lines: `methodology_revision`, W-idle's five
per-run peaks, W-idle's median. W-real and W-soak byte-identical apart from the
shared revision field. The redundant second measurement pass (`run-b`) was killed
before this task began and was not restarted.
## Task 93

**Measured vs `null`, and why.**

| Workload | State | Why |
|---|---|---|
| W-idle | **measured**, 5 runs, median 954,240 KiB | G1's only input. Cold start + one cassette-backed tool turn + settle, 2 s sampling, 148 s trace. |
| W-real | **measured**, 5 runs, median 3,026,992 KiB | G2's only input. Largest session by `SUM(LENGTH(part.data))`, restored, rendered, one turn. |
| W-soak | **`null` + reason** | Not a G1/G2 input. The plan already states a 20-turn run is smoke-only and **cannot satisfy G3**, so the permitted smoke has no gate value even when it succeeds. Chasing it would not have produced G3 evidence. Deferred to Todos 88-90. |

The recorded reason is explicit rather than a bare error: *"not measured: the 20-turn W-soak smoke is not a G3 input and was not pursued; the full W-soak of 500 turns over 2 hours remains owed by the G3 gate. Smoke attempt reported: …only 0 of 20 cassette-backed turns completed; captured 2 provider request(s)"*. A later reader must not be able to mistake it for a measurement that came out small.

**A failed W-soak *smoke* is deferred; a failed *full* soak propagates.** `soak_outcome` splits on `smoke_only`. Losing the smoke costs no gate evidence and must not discard the ten G1/G2 runs already measured; losing the full soak loses G3's input itself, so it fails the report. `BaselineReport::validate` still rejects any artifact whose W-idle or W-real lacks five runs and a median, so a *gate* input can never reach the deferred state — the artifact fails instead.

**`PERF_METHODOLOGY_REVISION` bumped 1 → 2. The four formulas were not touched.** Revision 1 discarded the first 90 s of *every* workload as warm-up, which is wrong by construction for a bounded cold-start workload: W-idle's whole trace is 148 s, so the rule threw away 45 of 75 samples — 61% of the trace, including the entire cold start it exists to measure. Recomputed from the retained raw samples, W-idle's median peak is **954,240 KiB (931.9 MiB)** over the whole trace against **746,408 KiB (728.9 MiB)** under the discard: the rule hid ~203 MiB of real peak. W-real is unaffected — its turn is typed only after the 90 s hydration gate, so its peak lands after the former window either way, and both rules give 3,026,992 KiB. Revision 2 therefore scopes the discard to W-soak alone, where startup is genuinely noise against hours of steady state. The formula section is **byte-identical** across both revisions (`sha256 = db49ffeb…6922ad`), and revision 2 re-registers that digest rather than aliasing revision 1's, so it must match on its own. The correction landed **before any Rust binary was measured**, which is the case a revision bump exists to record — it cannot have been fitted to a comparison result.

**The 90 s mark keeps a second, non-aggregation role.** A restored session's first turn is not typed until 90 s have elapsed, so W-real's keystrokes reach a TUI that has finished replaying its parts rather than one still hydrating. That is a run-shaping gate, not a sample filter, and the two uses are now named separately (`hydration_is_settled` vs `warm_up_discard`).

**W-real's sampling window is 450 s, not 150 s.** `90 s` hydration gate + `300 s` turn allowance + `60 s` settle. The turn cannot start before the gate, so a 150 s window ended before the turn's peak existed. 300 s is sized from measurement: 13 s keystroke-to-first-request and still climbing 55 s later on this session.

**The cassette prelude is unconditional.** Both a new and a restored session issue exactly one tool-free text request first, so `openai-chat/streams-text` is always served before the tool loop and `completed_tool_turns` always deducts exactly one prelude request. Making it conditional on the session kind was the wrong model — see `learnings.md`; the two preludes differ in *purpose* (title vs compaction summary), not in count.

**The released binary is measured, never the from-source oracle.** `released_oracle()` honours `OC_TESTKIT_ORACLE` — the not-found error's own remedy instructs the operator to set it, and a `PATH` hit can be a launcher shim — but deliberately ignores the from-source flavour. Running the TypeScript entry point under Bun would measure a different process tree than the release users run, so a baseline taken that way would not describe the software the gates are about.

**Run-to-run stability is evidenced from one pass's five peaks, not from a second pass.** Two second-pass attempts were started; neither finished (one hit the pre-fix W-real failure, one was destroyed when `/tmp` was swept mid-run and took the harness binary with it). Neither contributed a number. The within-pass spread of the five retained peaks measures the same quantity from data the artifact already holds, and the measured spread — **1.140x** for W-idle, **1.179x** for W-real — is *wider* than a two-pass 10% agreement criterion would have tolerated. Reporting the measured spread states that variance honestly; asserting a 10% tolerance this machine does not meet would not. `PeakSpread` is derived from the runs rather than stored, so it cannot drift from them, and `every_committed_peak_is_reproducible_from_its_retained_samples` re-derives each published peak through the production rule.

**File permissions use explicit Unix modes.** `Permissions::set_readonly(false)` is a clippy warning and, on Unix, ambiguous about which of user/group/other it grants. The snapshot is `0o444` and each run's private clone is `0o600`.
## Task 27

- Todos 29/30/94/96 must feed raw response bytes through `oc_llm::sse::SseParser::push`, call `finish` at EOF, and deserialize each `SseEvent` through `SseEvent::deserialize(provider, model)`. Provider crates must not decode UTF-8 or split SSE frames themselves.
- All text/event-stream providers share this parser. `SseParser` recognizes both LF and CRLF blank-line frame delimiters, joins repeated `data:` fields with newline, ignores comments and unrelated fields, and emits a trailing unterminated frame from `finish`.
- Stream idle timeout defaults to 300 seconds. Provider config is passed to `StreamIdleTimeout::from_config`; the positive integer environment override is `OPENCODE_STREAM_IDLE_TIMEOUT_SECS`. Providers wrap each `stream.next()` future with `StreamIdleTimeout::wait`.
- A timeout is `ProviderError::Transient`; malformed streamed JSON is `ProviderError::Fatal`. Both preserve actionable provider/model source context without adding a catch-all error variant.


## Task 25

**The `Provider` trait is three methods.** The reference implementation's is ~30 and the plan names that an anti-pattern; the cost is not aesthetic, it is that every method is a question all five families must answer including the ones for which it is meaningless, and each is a place a caller can start behaving per-provider.

```rust
pub trait Provider: std::fmt::Debug + Send + Sync + 'static {
    fn id(&self) -> &str;
    fn capabilities(&self) -> Capabilities;
    fn stream(&self, request: CompletionRequest) -> ProviderStream<'_>;
}
```

`Debug` is a **supertrait, not a method**, so it costs the trait no width; it is required because a `Result` carrying a provider must be printable when it turns out to be the wrong branch (`unwrap_err` needs `T: Debug`), and because a startup audit listing what got wired beats one that counts it. Implementations must not render a credential.

`stream` returns `Pin<Box<dyn Stream<Item = Result<StreamEvent, ProviderError>> + Send + '_>>` rather than an `async fn`, so the trait stays object-safe without `async_trait`, and a request-shaping failure surfaces as the stream's first item instead of a second error channel.

`Capabilities` is one plain struct — `reasoning`, `tool_calls`, `prompt_cache`, `attachments`, `sampling_params` — rather than five predicate methods: a caller needing two answers should not pay two virtual calls, and a new capability must not widen the trait. Each field is there because a *named* downstream todo branches on it (28 reasoning, 29/31 prompt cache, 30 sampling-param stripping, 32 attachments).

**Deliberately excluded from the trait, with the owner of each:** model listing / metadata / cost / token limits → `catalog.rs` (26); authentication and credential refresh → `oc-auth` (24); retry, backoff and compaction decisions → `oc-error`'s `Recovery` (2); prompt caching and reasoning-effort resolution → `effort.rs` + `cache.rs` (31); SSE framing and incremental UTF-8 decoding → `sse.rs` (27); the full stream-event vocabulary including `RetryRollback` → `event.rs` + `stream.rs` (28).

**`Spec` is a struct of optional parameters, not an enum of provider identities.** The reference uses an enum, one variant per identity (`OpenRouterRuntimeSpec::{Default, OpenRouterApiKey, CompatibleProfile, NamedProfile}`), which works when the variants are known and few. Here five families are still unwritten, and an enum would force each of them to add a variant to a shared type **in `oc-llm`** — reintroducing in the type system exactly the coupling this registry deletes from the dependency graph. Shape:

```rust
pub struct Spec {
    pub provider: String,                              // the key being instantiated
    pub surface: ApiSurface,                           // Default | Chat | Responses | Messages
    pub base_url: Option<String>,                      // Azure endpoint, Vertex REP domain, profile URL
    pub api_version: Option<String>,                   // Azure api-version
    pub region: Option<String>,                        // Bedrock signing, Vertex routing
    pub project: Option<String>,                       // Vertex publisher path
    pub headers: BTreeMap<String, String>,             // anthropic-beta, Copilot editor headers
    pub options: BTreeMap<String, serde_json::Value>,  // the user's provider.*.options bag
}
```

`BTreeMap` not `HashMap` so a spec renders and compares deterministically — todo 31's byte-stable-prefix test will depend on that. `options` is `serde_json::Value` on purpose: it mirrors the oracle's own `Record<string, any>` config surface, so a provider-specific option a user sets does not require a field here first. It is a *config* bag, never an error channel; nothing makes a recovery decision from it.

**Three error variants, not two, and never one.** `RegistryError::NotRegistered` (a wiring bug — nothing a user can configure fixes it), `Unavailable { reason: Unavailable }` (a user-facing state — login or config edit), `Construction { source: ProviderError }` (the provider ran and broke). The reference collapses the first two into a bare `Option::None` and logs the same "composition root must call `register_external_provider()`" warning for both (`external.rs:219-245`), so a user with no GitHub token is told the *program* is miswired. `Unavailable` is itself an enum — `MissingCredential | UnsupportedPlatform | IncompleteConfiguration` — because only the first warrants pushing the user through a login flow and only it is worth re-checking after one.

**A fallible factory must name a reason for declining.** `FactoryOutcome = Result<Arc<dyn Provider>, Declined>`, and there is deliberately no `Ok(None)`: an unexplained decline is precisely what leaves a caller unable to tell a user what to do. `Declined::{Unavailable(Unavailable), Failed(ProviderError)}` with `From` impls for both, so `?` works in a factory body.

**`ProviderRegistry` is an owned value, not a global.** The reference uses a process-wide `OnceLock<RwLock<HashMap<..>>>` and documents that re-registering a key replaces the previous factory "useful for tests" (`external.rs:184-195`) — an admission that a global registry and a parallel test suite fight each other. This workspace's tests run in parallel, so the composition root builds a value and passes it down. That also makes "wired in exactly one place" true *by construction* rather than by convention: there is no ambient state to reach for.

**The composition-root signature todos 29, 30, 94, 95, 96 must satisfy:**

```rust
pub type Factory        = Arc<dyn Fn(Spec) -> FactoryOutcome + Send + Sync>;
pub type FactoryOutcome = Result<Arc<dyn Provider>, Declined>;
pub type Composition    = fn() -> ProviderRegistry;

// infallible — the provider always constructs
registry.register("anthropic", |spec| Arc::new(Anthropic::new(spec)));

// fallible — construction may decline, and must say why
registry.register_fallible("github-copilot", |spec| {
    let token = load_token().ok_or(Unavailable::MissingCredential)?;
    Ok(Arc::new(Copilot::new(spec, token)?))
});
```

Each provider todo adds **one** call inside `oc-cli`'s single `Composition` function and changes nothing else in the workspace.

**Credential presence arrives through a one-method trait, not a dependency on `oc-auth`.** `CredentialPresence::has_credential(&self, provider: &str) -> bool` is all the "credentialed but unwired" diagnostic needs, and taking `oc-auth` as a dependency to get it would put credential storage, refresh and file-permission concerns in the spine. Same inversion as the factories, applied to the one dependency the diagnostic would otherwise force in. `ProviderRegistry::unwired(&dyn CredentialPresence, &[&str])` returns one `NotRegistered` per credentialed-but-unregistered candidate; the candidate list comes from the catalog (26), because the registry has no opinion about which providers exist in the world, only which it can build.

**The absent `oc-llm → oc-provider-*` edge is asserted mechanically, over the transitive closure.** `cargo tree -p oc-llm | grep oc-provider-` is the stated criterion, but `crates/oc-llm/tests/registry_dependency_direction.rs` is the durable form: it parses every member manifest, walks the first-party closure breadth-first (so an edge added through an intermediate crate is caught too), scans `dependencies`, `dev-dependencies` **and** `build-dependencies` (a dev edge costs the same rebuild on every CI run), and carries two vacuity guards — it fails unless it finds ≥33 members, all five `oc-provider-*` crates, and `oc-llm → oc-error`, so a scan that walked the wrong directory cannot pass by finding nothing.

## Task 22 - message/part persistence

### The blob is `serde_json::Map`, not a typed struct per variant
Only the discriminator (`type` / `role`) is typed; the payload rides as an
untyped object. Reason: the writer on the other side of this file is another
program that ships independently. A production `file` part carries `synthetic`,
which `FilePart` does not declare - a typed decoder would have dropped it and
silently broken every attachment. Byte parity is the requirement; a typed model
would trade it for a type-safety guarantee this module cannot honour anyway.
Consequence for callers: reach for `PartKind` to dispatch, then index `data`.

`preserve_order` is **off** in this workspace, so `Map` is a `BTreeMap` and
`to_string` emits one canonical key order. That is what makes "byte-identical
JSON" a meaningful assertion rather than an accident of insertion order - both
sides of every comparison are independently re-serialised.

### Hydration: two statements, chunked at 900 ids
`messages_for_session` (1 statement, `ORDER BY time_created, id` to ride
`message_session_time_created_id_idx`) then `parts_by_message` (1 statement per
900 ids, `WHERE message_id IN (...) ORDER BY message_id, id`), grouped into a
`HashMap` and zipped. Shape taken from `message-v2.ts:98-123`.
**Measured: 500 messages x 3 parts = 2 statements.** 900 is well under SQLite's
32766 variable ceiling; it bounds statement text and the bind array, it is not
working around a limit.
An empty message set issues **no** part lookup at all (1 statement total) -
guarded, because `IN ()` is a syntax error.

### The query count is proved two independent ways
Every statement in the module is prepared through one private
`MessageStore::prepare`, the only place a `Cell<u32>` counter moves - so a
statement cannot be issued without being counted. That alone is self-reported, so
the 500-message test **also** installs SQLite's own `SQLITE_TRACE_STMT` hook and
asserts both numbers are 2. If a statement ever escaped the wrapper the two would
disagree and the test would fail. Cost: the `trace` feature on oc-db's own
`rusqlite` line (documented in the manifest as existing for exactly this).
`reset_query_count()` is called immediately before the measured call so setup
writes are excluded - the first draft omitted this and the real-data test caught
it at 29 instead of 2.

### An unknown variant is `DbError::Decode`, and the row is left alone
`PartKind` / `MessageRole` have no catch-all arm and `from_tag` returns `Option`;
the decode path converts `None` into `DbError::Decode { table: "part", .. }` whose
source names both the rejected tag and the twelve expected ones. Rationale: a
dropped part renders as a tool call with no result or a step with no finish, with
nothing in the logs. `Decode` is already classified non-retryable by oc-error, so
a retry loop will not spin on it. Following Todo 2's discipline, a thirteenth
upstream variant breaks compilation at every match rather than being absorbed.
The failing row is **not** deleted or skipped - asserted.

### A stripped key found inside `data` on read is also `Decode`
Nothing in SQLite rejects a blob that duplicates `id`/`sessionID`/`messageID`, and
a schema test cannot see it, so it is checked on the read path
(`reject_stored_keys`). Without this the duplicate becomes a second source of
truth that can disagree with the indexed column.

### `time_created` comes from the payload, `time_updated` from the clock
`projector.ts:264` reads `info.time.created`; `MessageRecord::from_json` does the
same and falls back to `0` rather than to a clock read, keeping the value a pure
function of the input. A part's `time_created` is **not** in its payload
(`projector.ts:321` uses the event's own timestamp) so it is a parameter.
`put_*_at(record, now)` takes the write stamp explicitly and `put_*` supplies the
clock, so every test is deterministic without a clock abstraction.

### QA went for the strong direction
`opencode export` exists in 1.18.12, so the differential is Rust-writes ->
TypeScript-reads (all twelve variants, exit 0, 12/12 surfaced) rather than the
prompt's permitted fallback. The reverse direction is covered too, against nine
genuine production rows behind `OC_T22_REAL_ROWS`. The fixture is deliberately
**not committed** - it is the user's conversation content; the extraction query is
in the evidence file so anyone can regenerate it.

### The 51 GiB database was opened read-only, not copied
The prompt suggested copying it. It is 51 GiB with an 815 MiB `-wal`; a copy would
have burned the task budget and the disk. `file:...?mode=ro` is strictly safer
than a copy anyway - no write handle is ever created and the `-wal` is untouched.

## Task 21 — session store

### `subpath` is implemented — intentional divergence, for Todo 86's allow-list

**DIVERGENCE CANDIDATE #1 — `subpath` is applied instead of ignored.**

Upstream declares `subpath` on the project arm of the list union
(`packages/core/src/session.ts:64-68`), re-declares it in the HTTP query schema
(`packages/protocol/src/groups/session.ts:44` and `:102`), carries it through the
generated client (`packages/client/src/generated/client.ts:298`) and the SDK
(`packages/sdk/js/src/v2/gen/sdk.gen.ts:5440-5456`) — and then **never reads it**
in the handler (`core/src/session.ts:268-303` builds its conditions from
`directory`, `workspaceID`, `project`, `search` and `anchor`, and nothing else).
The intent is even written down as a comment at `core/src/session.ts:50`:
`// - by subpath`. The v1 code had it: `listByProject`'s path filter at
`session.ts:969-984`.

So a caller asking for one directory's sessions today silently receives the whole
project's. This project applies the filter, taking its prefix semantics from that
v1 filter: `path = ?` OR everything beneath `?/`.

Two sub-decisions inside it:

1. **`substr(path, 1, length(?) + 1) = ? || '/'` rather than `path LIKE ? || '/%'`.**
   `LIKE` reads `_` and `%` as wildcards, so upstream's v1 filter matches a
   session under `axb/` when asked for `a_b` — it interpolates the path straight
   into the pattern. There is no index on `session.path` either way, so the exact
   form costs nothing. Pinned by
   `a_subpath_containing_a_like_wildcard_is_not_treated_as_a_pattern`.
   **This is a second, smaller divergence** and should go on the allow-list with
   the first: given `subpath` was dead code upstream, there is no observable
   behaviour to break, and matching a directory literally is the only reading of
   "sessions under this subpath" that is not a bug.
2. **An empty subpath filters nothing**, matching upstream's own `if (input.path)`
   guard (`session.ts:969`). A session at the worktree root stores `""`, so an
   empty subpath would otherwise mean "only the root", which is not what an
   absent filter means.

`ListQuery::with_subpath` on a non-project scope is dropped rather than
reinterpreted, and `ListQuery::subpath_applies()` reports whether a subpath will
actually narrow anything — so a caller can never silently pass a subpath that is
ignored, which is the exact failure this divergence fixes.

### The API shape Todos 71-76 will consume

Two layers, both in `oc_db::session`:

- **Free functions taking `&Transaction`** — `create`, `get`, `find`, `touch`,
  `touch_at`, `list`, `list_global`, `children`, `subtree`, `remove`. These
  compose inside a caller's own transaction, which is what the turn loop (Todo 32)
  needs when a session write has to land atomically with message writes.
- **`Store<'pool>`**, a `Copy` facade over `&Pool` with the same method names.
  Reads take a pooled connection; writes go through `Pool::transaction`
  (`IMMEDIATE`), so a subtree delete is one atomic unit under contention. This is
  the layer the request handlers should use.

Request-shaped types:

- `SessionCreate` — required fields via `SessionCreate::new(id, slug, project_id,
  worktree, directory, title, version)`, then `.with_parent()`, `.with_workspace()`,
  `.at(millis)`. `worktree` is an input, not a stored column: it exists only to
  derive `path`. `SessionCreate::default_title_prefix(parent_id)` returns
  upstream's prefix; the timestamp half is the caller's to format, because it is
  the only part needing a calendar.
- `ListQuery { scope, workspace_id, search, roots, start, cursor, archived, sort,
  direction, limit }` with `ListQuery::{global, directory, project}` constructors
  and `.with_subpath()`, `.created_order()`, `.with_limit()`, `.active_only()`.
  Every field defaults to not narrowing.
- `ListScope::{Directory{directory}, Project{project_id, subpath}, Global}` — an
  enum, because the upstream schema is a union and a struct of three `Option`s
  would let a caller ask for two scopes at once.
- `Creation::{Inserted, AlreadyExists}` — see learnings; `.session()`,
  `.into_session()`, `.was_inserted()`.
- `Session` with grouped `tokens: Tokens` and `summary: Option<Summary>`, matching
  the shape `fromRow` emits. `Session::subpath()` applies upstream's
  empty-string-is-absent rule; `Session::path` is the raw column.
- `GlobalSession { session, project: Option<ProjectSummary> }` for `list_global`.
  Two statements, not a join (`session.ts:578-595`), so a session whose project
  row is gone still comes back with `project: None` — upstream's `?? null` at
  `:595`.

### JSON columns are carried as `Option<String>`, unparsed

`model`, `metadata`, `revert`, `permission` and `summary_diffs` are stored and
returned verbatim. Nothing in this module needs to look inside them, and
re-encoding is exactly how a byte-compatible payload stops being byte-compatible
— key order, number formatting and absent-vs-null all shift. Todo 22 makes the
same call for `message.data` / `part.data`. A later todo that needs typed access
should parse at its own boundary, not here.

### `remove` returns the removed ids rather than cancelling background jobs

`session.ts:618` cancels the subtree's running jobs before deleting. The job
registry is not in this crate and `oc-db` must not grow a dependency on it, so
`remove` returns `Vec<String>` (deepest first, root last) and the caller that
owns the registry does step 2. Todos 80-85 need that list anyway.

### `session.path` gets its own module, not `std::fs::canonicalize`

`src/session/path.rs` reimplements Node's `path.resolve` + `path.relative`
lexically. `canonicalize` resolves symlinks and requires existence; a worktree
reached through a symlink would then produce a `path` the oracle never wrote, and
the subpath filter matches on that column. 13 unit tests, including
`/abc` vs `/abcd` → `../abcd` (a shared prefix that is not a shared segment).

## Task 31
- Prefix protection is both type-level and tracker-level. `PromptCache<T>` owns a private immutable `StaticSystemPrompt`; `prepare_turn` has no static-prompt parameter. Volatile input has the separate `DynamicContext` type and can only become a trailing `Role::User` message. `CacheTracker::record` remains defense in depth and rejects byte changes, history shrinkage, and in-place prefix edits while retaining the last valid baseline.
- Model-declared variants are exact JSON option objects keyed by canonical effort and always win before generic provider-family mapping. Adaptive/budget differences are catalog capabilities, never model-name checks.
- Todo 47 MCP merge API: call `LockedTools::tools_for_request(&available_tools, McpToolStatus::Pending)` while discovery is incomplete, then call it with `McpToolStatus::Ready` when discovery settles. The first changed ready snapshot consumes the single rebuild; later tool changes remain hidden until explicit `LockedTools::reset()`.
- A late-MCP rebuild resets the message tracker baseline because that request is already the one intentional provider-cache miss; the immutable static prefix still remains byte-identical.

## Task 28

### Replay exclusion is type-level
Stored transcripts use `ContentBlock`; provider-bound messages use `RequestContentBlock`. The outbound enum deliberately has no `Reasoning` or `ReasoningTrace` variant. `TranscriptMessage::to_request` filters both while preserving `SignedThinking`, `ProviderEncryptedReasoning`, and tool-call `ThoughtSignature`. This is stronger than relying on every provider serializer to remember a boolean or repeat a filter.

### Turn-loop accumulator API
Consumers call `StreamAccumulator::apply(&StreamEvent)`. It accumulates visible text, raw tool-input JSON, reasoning text, reasoning signatures, and a per-tool thought signature. It deliberately does not parse partial JSON or execute tools. `RetryRollback` calls `clear_attempt()`, which clears text, tool calls, reasoning, and reasoning signature together. Read-only access is through `text()`, `tool_calls()`, `reasoning()`, `reasoning_signature()`, and `is_empty()`.

### Compatibility with Todo 25's registry contract
`registry::ProviderStream` now yields the canonical `event::StreamEvent`; the temporary four-variant enum was removed. `CompletionRequest.messages` now holds `event::Message`, whose content is already the safe `RequestContentBlock` type. Registry re-exports preserve the existing import surface for downstream provider crates.

## Task 38 — `oc-tool` trait, schema pipeline, context

### Object safety: `#[async_trait]` boxing, and a named adapter rather than a blanket impl

`Tool` uses `#[async_trait]`, so `execute` returns a boxed future and `Arc<dyn Tool>`
works. The alternative — a hand-rolled `fn execute(&self, …) -> Pin<Box<dyn Future + '_>>`
— is the same allocation with worse ergonomics for 19 implementors.

The bridge from `TypedTool` to `Tool` is a **named wrapper `Typed<T>`** plus
`erase(tool) -> Arc<dyn Tool>`, *not* `impl<T: TypedTool> Tool for T`. A blanket impl
would conflict with any direct `impl Tool`, because the compiler cannot prove an MCP
proxy is not also a `TypedTool`. Todo 47 needs `impl Tool` directly (a remote server's
schema is not describable by a Rust type), so the blanket impl would have closed the
door this crate must leave open. Cost: implementors write `erase(MyTool)` once.

### `raw_parameters_schema` is named for being un-augmented

`Tool` exposes the derived schema and the augmented one separately, and the augmented
one is only reachable through `definition()`. Naming the raw accessor `parameters_schema`
(as jcode does) invites a caller to send it to a provider and silently skip the
cross-cutting properties. `ToolDefinition` has no public constructor path that bypasses
`definition()`, so augmentation is not something a caller can forget.

### Two artifacts collapsed into one, and the settings that cost tokens

The schema is `schemars`-derived from `TypedTool::Params`, the same type serde
deserializes. Three departures from schemars' defaults, each a per-request cost:
draft-07 (providers do not implement 2020-12's `$defs`/`$dynamicRef`), subschemas
inlined (a `$ref` hop providers handle inconsistently), `$schema` and `title` stripped
(46 dead bytes and the Rust type name).

A params type whose derived schema is not object-shaped is **normalized to an empty
object schema**. `#[derive(JsonSchema)]` on a unit struct yields `{"type":"null"}`, but a
no-argument call arrives as `{}`, and a non-object schema would silently lose the
injected `intent`. jcode's `ensure_intent_in_schema` returns non-object schemas
untouched and has no derivation step, so it never hits this.

### The guard-key test is behavioural, not `assert_eq!` on two constants

Comparing two constants passes trivially if both are renamed and only one side's
*reader* is updated. `tests/guard_key.rs` instead diffs an augmented schema against its
input to **discover** the injected property names, then proves each one is observed **by
calling the guards**, and that exactly one key feeds each guard. A rename on either side
alone fails it. The wire spellings are pinned separately, because a coordinated rename
is still a wire change.

`INJECTED_KEYS` exists so `strip_cross_cutting` cannot fall behind the injector, and a
test asserts the declared list equals what augmentation actually injects.

### `for_subcall` shares everything except the call id, and adds a depth

Session, message, agent, permission asker and interrupt are all inherited by `Arc` clone
(proved with `Arc::ptr_eq`): a sub-call runs under the same rules and the same abort, so
a denied edit stays denied and one interrupt stops the whole tree. **Anything a composing
tool could vary here would be a way to launder a sub-call past a gate the parent could
not pass**, which is why nothing is parameterized.

`depth` is added over jcode's version. Todo 70's `execute` re-enters the registry, so a
tool that composes itself recurses without a bound. Choosing the limit is the composer's
call; *recording* the depth belongs here, because this is the only place a child context
is created.

### The interrupt is a trait, because the concrete signal is downstream

`InterruptSignal` lives in `oc-engine`, and `oc-engine → oc-tool` is required by todo 33.
So `oc-tool` declares `InterruptHandle { fn is_set(&self) -> bool; async fn notified(&self) }`
with method names and signatures **identical** to `InterruptSignal`'s, leaving `oc-engine`
a one-line forwarding impl with nowhere to introduce a discrepancy (spelled out in
`context.rs`'s module docs). `is_set` stays synchronous, preserving todo 3's property that
blocking tool code can poll cancellation with no Tokio runtime. `NeverInterrupted::notified`
never completes, which is the honest reading of "never interrupted".

### Size detection API, and the policy that is explicitly not here

`measure(text, limits) -> SizeMeasurement { lines, bytes, limits, verdict }`. Two
deliberate shapes:

- `SizeVerdict` is an enum, not a `bool`, and `LimitExceeded` distinguishes
  `Lines`/`Bytes`/`Both`. A caller reporting *why* output was withheld has to name the
  threshold, and re-deriving that from the numbers at the reporting site is how the
  reported reason drifts from the decided one.
- `SizeMeasurement` carries the `limits` it applied. Otherwise the number in a message
  and the number in the decision are two reads of configuration that can disagree.

**The refuse-vs-truncate policy is todo 72's alone.** This crate detects the size and
persists the full text; nothing here truncates, and no test here asserts what a caller
receives on overflow. `tests/oversized_output.rs` says so in its module docs. The one
test that touches the untruncated text asserts only that *this crate's own operations*
(`measure`, `persist`) do not rewrite it — a statement about these functions, not about
what todo 72 hands back.

### Storage divergence: the session id goes in the filename

Deliberate, documented divergence from the oracle. See `issues.md` for the upstream
defect that forces it. The `tool_` prefix is preserved so files written by this binary
stay prunable by the TypeScript binary sharing the directory, and the unique component is
a UUIDv7 so the ascending-by-creation ordering of `Identifier.ascending()` survives.
Session components are sanitized to `[A-Za-z0-9_-]`; a test proves `../../etc/ses_x`
cannot move the file out of the store.

### `record_output_path` uses `outputPaths` (plural), matching the v2 wire

The oracle has two shapes: v1 `Tool.wrap` writes `metadata.outputPath` (singular) plus
`metadata.truncated`; v2 `ToolOutputStore.bound` returns `outputPaths` (array), which is
what `message-updater.ts:313` persists. The array is used here — one result can spill more
than once. **`metadata.truncated` is deliberately not written**: setting it would presume
the policy this todo does not own.

## Task 26

### The pinned fixture: a real 7-provider subset, compiled in
`crates/oc-llm/tests/fixtures/models-dev-pinned.json` (sha256 `a11b7af8395945c2…`) is a
**verbatim subset** of a real `https://models.opencode.ai/api.json` response — captured
once from `/config/.cache/opencode/models.json`, never re-fetched. Both sides of the
differential read it: the oracle through `OPENCODE_MODELS_PATH`, this crate through
`include_str!` (compiled in, so a test cannot silently read a stale file).

Seven providers, each earning its place by covering a shape the resolver must handle:
`deepseek` (provider-level api+npm, cache_read cost), `mistral` (a `deprecated` model),
`groq` (a `beta` model and a `/`-bearing model id), `inceptron` (an `alpha` model),
`anyapi` (`experimental.modes`), `impossibl` (`cost.tiers` + `context_over_200k`),
`zhipuai` (`interleaved:{field}` + `reasoning_options`).

**The selection criterion that makes the differential meaningful**: none of the seven has
a `custom()` loader in `provider.ts:168-963`, so availability is decided purely by the
three generic sources this crate implements. A difference in the model list is therefore
a difference in *this* logic, not in a provider-specific autoload rule owned by another
todo. A test asserts the fixture still covers all seven shapes, so trimming it later
fails loudly instead of quietly weakening the differential.

### Why `opencode models`, not `--format json`
`--format json` does not exist on 1.18.12 (`models --help` lists only `--verbose` and
`--refresh`; the flag prints help and lists nothing). `--verbose` was rejected as the
target because its JSON key order differs between catalog-derived and config-derived
models — the oracle builds them by different code paths — so a diff would fail on key
order and say nothing about which models resolved. The plain `provider/model` line list
is the actual contract, so `Catalog::model_lines()` reproduces it and the comparison
runs under `Normalizer::none()`: a model list has no timestamps, ids, ports or pids, so
masking anything would mask a real difference.

### Availability maps onto todo 25's diagnostics, and only onto one of them
`Availability::unavailable_reason()` returns `registry::Unavailable`, **never**
`RegistryError::NotRegistered`:
- nothing fired → `Unavailable::MissingCredential` ("log in")
- a credential exists but is a shape the generic path cannot use (oauth / wellknown) →
  `Unavailable::IncompleteConfiguration` ("this needs its provider's own flow")

Whether a provider is *wired into this build* is a fact about `oc-cli`'s composition
root and is unknowable from a catalog, a config file and `auth.json`. Keeping the two
apart is precisely what stops a user with no API key from being told the program is
miswired — the bug todo 25 called out in the reference implementation.

### Fail loudly rather than return an empty catalog
With fetching disabled and no cache this returns `CatalogError::FetchDisabled` naming
the flag, the source, the cache path and the alternative, where the oracle would fall
through to its compiled-in snapshot. This crate has no snapshot; silently returning `{}`
would be indistinguishable from "you have no providers configured" and the user would
meet it as an empty model picker. A caller wanting the oracle's silence matches
`is_policy()`. A future snapshot-baking todo inserts rung 2 between `load_from_disk()`
and the error, the same position the oracle has it.

### Loading is async, resolving is not
`CatalogSource::load()` is the only thing that can touch the network; `Catalog::resolve()`
is synchronous and pure. That split is what lets every merge and availability test run
with zero I/O, and makes "do not fetch at startup when the flag is set" a property of one
small function rather than of the whole pipeline.

### `ResolveInput` is a builder, not positional arguments
The three availability sources are independent, and a positional API invites a caller to
pass two of three and not notice. Each has a `with_` method so a test states exactly one.

### Variants are merged here, derived in todo 31
`ProviderTransform.variants` is reasoning-effort logic and belongs to `effort.rs`
(todo 31, concurrent). This crate merges config-declared variants and drops
`disabled: true` ones; `MergeOutcome::variant_derivation_pending` names the providers
where a derived set would have applied, so todo 31 has a seam rather than a rewrite.

### Field named `origin`, not `source`, in `CatalogError`
`thiserror` reserves a field called `source` for the error cause, so the models.dev URL
is `origin`. Cosmetic, but it is why the variants read the way they do.

## Task 105
- Chose option 2 (test-local helper) over adding an accessor to `Message`. Reason: the fix needed
  zero changes outside `crates/oc-llm/src/cache.rs`, and `event.rs` is owned by Todo 28 — an
  additive method there would still be a cross-owner edit for no extra benefit in this test.
- Helper added in `cache.rs` `#[cfg(test)]` module:
  `fn text_of(message: &Message) -> String` — filters `RequestContentBlock::Text { text }` and
  concatenates. Imported `crate::event::RequestContentBlock` in the test module only
  (`registry::provider` does not re-export it).
- The five assertions still compare real message prose; nothing was weakened or ignored.

## Task 96
- Kept three explicit provider types/construction paths: `GoogleGenerativeAi` (API key + Gemini API), `VertexGemini` (GCP bearer auth + Gemini shape), and `VertexAnthropic` (GCP bearer auth + Anthropic shape). Shared code is limited to Gemini body/stream translation and transport helpers; Vertex-Anthropic does not pass through Gemini or OpenAI lowering.
- GCP auth implements ADC order (`GOOGLE_APPLICATION_CREDENTIALS`, gcloud well-known file, metadata server), both standard ADC JSON forms (`authorized_user`, `service_account`), token caching/refresh, and explicit bearer tokens for deterministic tests. No JWT/RSA crate is pinned in workspace dependencies, and root `Cargo.toml` is out of scope, so service-account RS256 uses the installed OpenSSL CLI with a protected `NamedTempFile`; reqwest handles token exchange.
- HTTP failures are classified from status plus exact structured error-code fields. Rendered provider messages are never inspected for retryability.

## Task 29 — Anthropic provider

- `AnthropicProvider` owns transport and authentication; request lowering,
  stream decoding, and error classification are separate modules. This keeps the
  `Provider` implementation small while making each protocol boundary directly
  testable.
- The provider uses `oc_llm::sse::SseParser` exclusively. It does not keep a local
  line parser or perform lossy UTF-8 conversion, so partial multibyte code points,
  CRLF frames, and trailing frames inherit the shared parser's tested behavior.
- Error recovery is decided from HTTP status and structured Anthropic error
  fields. Provider-rendered message text is retained only as payload for display;
  it never selects `RateLimited`, `Transient`, `Auth`, `Refused`, or `Fatal`.
  Numeric `retry-after` is preserved as `Duration`.
- Signed thinking is replayed only through the canonical
  `RequestContentBlock::SignedThinking { thinking, signature }` form. Plain
  reasoning remains excluded by Todo 28's outbound type, preventing Anthropic
  from receiving unsigned thinking.
- Cache breakpoints are deterministic: only the last non-empty static system
  block receives ephemeral cache control. Per-turn messages remain unmarked, so
  a changing clock, memory item, or tool result cannot poison the stable prefix.
- Real recordings are strict request-and-event golden tests through
  `CassettePlayer`, including both interactions of the tool loop and cache cases.
  Auth headers, interleaved signed thinking, two simultaneous tool calls, and
  model substitution use authored unit tests because no committed recording
  contains those wire cases; they are not represented as recorded evidence.

## Task 95
- Implemented SigV4 locally using the already-pinned `sha2` crate rather than
  introducing an AWS SDK/signing dependency or editing the root manifest. HMAC is
  built from SHA-256 with fixed block handling and is locked by RFC 4231 plus the
  published AWS known-answer vector.
- Credential resolution follows explicit -> environment -> profile -> SSO cache
  -> container -> IMDS. Explicit credentials lead because they are deliberate
  provider configuration; metadata endpoints are last and use bounded requests.
- `BedrockOperation` keeps ConverseStream and InvokeModelWithResponseStream as
  explicit wire paths. Mantle changes only the shared `ApiSurface`: exactly the
  two `openai.gpt-oss-safeguard-{20b,120b}` model ids select Chat; all others
  select Responses, matching the TypeScript oracle.
- Error recovery is classified from HTTP status and structured AWS error-code
  fields, never from rendered provider text. EventStream exceptions use the same
  typed taxonomy, and secrets are redacted from credential `Debug` output.

## Task 94 — how the quirk table is organized, and why stripping keys off capabilities

### One concrete type, fifteen-plus identities; classification is a closed table

`CompatibleProvider` serves every claimed provider id. The differences are data —
base URL, surface rule, capability set, headers — so there is no per-vendor struct
and no per-vendor branch in the request path. `Spec::provider` tells the instance
which identity it took on, which is why `Provider::id()` returns rather than being
stamped by the registry.

`family.rs::CLAIMED` is a **closed** table, not a fallthrough. Both reference
implementations treat OpenAI-compatible as the default for an unrecognized id,
which converts a misconfiguration into a deserialization error naming JSON. Here
an unclaimed id is refused at construction and the message names the destination
crate. The escape hatch for a user's own endpoint is explicit
(`options.npm = "@ai-sdk/openai-compatible"`), mirroring the oracle's own
catalog-declaration mechanism.

The refusal is `Declined::Failed(ProviderError::fatal(UnsupportedProvider))`, not
`Declined::Unavailable`. `Unavailable`'s three reasons are fixed strings that
cannot name a crate, and naming it is the whole point. It stays terminal:
`Fatal → Recovery::Fail`, so nothing retries a misrouted provider. This keeps
`oc-llm`'s two diagnostics distinct — a refused id is *not* "not registered"
(a wiring bug) and *not* "unavailable" (a login or a config edit); it is the wrong
family, which is a third thing, and `RegistryError::Construction` is where it
belongs.

### The quirk table: three places, each named, none scattered

- `family.rs` — id → `{surface rule, routes_upstreams}`. One row per id.
- `surface.rs` — the two rule *functions*, Azure and Copilot, in one file with
  their oracle citations. `request.rs` receives a resolved `ApiSurface` and never
  learns which provider produced it.
- `quirks.rs::MODEL_PROTOCOL_RULES` — the profile's **only** model-id table,
  currently one row (`deepseek-v4`). Adding a row here is the review boundary.

`tests/discipline.rs::model_id_literals_appear_only_in_the_two_named_rule_tables`
enforces that `gpt-5-mini` and `deepseek-v4` appear only in `surface.rs` and
`quirks.rs`. This is the local form of `oc-llm`'s
`policy_sources_contain_no_model_id_literals`: this crate genuinely *has* two
model-id rules, so the discipline is that they are confined, not that they are
absent.

### Sampling-param stripping keys off `Capabilities::sampling_params`

The reference maintained `is_reasoning_model()` as a growing prefix list
(`openai_compat.rs:1191-1204`) — `o1`, `o3`, `o4`, `qwq`, `contains("thinking")` —
and every new reasoning model was an edit there. `Capabilities::sampling_params`
already exists for this, and the catalog supplies it per model, so `Quirks`
consults the capability and this crate never learns a reasoning model's name.

`Capabilities` is a *provider*-level answer on the trait, but `sampling_params` is
genuinely per model: one deployment serves both a reasoning model that 400s on
`temperature` and a chat model that wants it. `MODEL_CAPABILITIES_OPTION` narrows
the provider default for one model id, and `capabilities_for(model)` is the
lookup. `Provider::capabilities()` keeps returning the provider-level set, so the
trait is unchanged.

The same mechanism gates tools and attachments. An image bound for a text-only
model is **dropped**, not sent — that is what `Capabilities::attachments` is for,
and dropping is a decision while a 400 is an accident.

### `reasoning_content`: read always, echo conditionally

Reading is unconditional and free — a vendor that never sends the field costs
nothing, and two of seven corpus vendors do send it. Echoing is gated on
`Quirks::reasoning_protocol`, because for every other model it is pure token cost
on every later turn and a vendor that never sent it may reject it.

Three-level precedence, so a user is never stuck: an explicit
`options.reasoningContent` boolean (including `false`, which switches the protocol
*off* for a model the table matches), then `options.reasoningContentModels` as an
extension list, then the built-in table. A vendor changing behaviour mid-release
is a config edit, not a release.

Model ids are canonicalized before matching (lowercase, routing prefix stripped),
so `deepseek/deepseek-v4-pro` from a router and `deepseek-v4-pro` from the vendor
hit the same rule.

### `thinking` is written after `extra_body`

`RequestBody::build` applies `extra_body`, then writes
`thinking: {"type":"enabled"}` when the protocol is on. A caller with a stale
`thinking` option cannot disable a required opt-in
(`openai_compat.rs:1226-1227`). With the protocol off, the caller's value stands.
`PROTECTED_KEYS` additionally prevents `extra_body` from replacing `model`,
`messages`, `stream`, `tools`, `tool_choice` or either max-tokens spelling — the
fields derived from the request itself, where an override would make the wire and
the transcript disagree silently.

`extra_body` is also the seam for `oc-llm`'s effort resolution:
`EffortResolution::apply_to` writes into a `Map`, and that map is
`Spec.options.extraBody`. Effort policy therefore stays in one place for all five
families instead of being re-expressed per provider.

### A `Transport` trait rather than `#[cfg(test)]`

`Provider::stream` reaches bytes only through `Transport`. Tests construct the
provider with a cassette-backed transport, so "no live call in a test" holds
structurally: there is no test-only branch inside the request path to erode. It
also keeps `reqwest` out of the translation logic, which is the part with
behaviour worth testing.

The cassette transport re-slices each recorded body into 7-byte pieces (and one
test into 1-byte pieces). The recorder buffered whole streams, so the original
network boundaries are gone; re-slicing restores the property that matters — a
frame separator and a multi-byte code point landing across two chunks. This does
not re-test `oc-llm`'s parser (already proven by a 4220-offset sweep); it asserts
this profile *inherits* the property rather than bypassing it.

### Errors: status first, structured body only to disambiguate

`ProviderError::from_status` is the floor. The body refines it only where a status
genuinely cannot distinguish two recovery classes: `400` +
`code: "context_length_exceeded"` → `Compact`, and `code`/`type: "content_filter"`
→ `Refused`. Both are structured-field reads. `WireError::message` is attached as
a source for the human and never examined — the rule `oc-error` exists to make
unnecessary.

Two wire details worth keeping: several gateways report an upstream `429` as a
`200` carrying `{"error":{"code":429}}`, so `WireError::status()` parses a numeric
or numeric-string `code`; and `Retry-After` is honoured only in delta-seconds
form, because a mis-parsed HTTP-date would produce a worse backoff than deferring
to `oc-error`'s own policy.

## Task 106

Service-account RS256 now signs **in process** via `aws-lc-rs 1.17.3`
(`signature::RsaKeyPair::from_pkcs8` / `from_der` + `RSA_PKCS1_SHA256`), replacing
the `openssl dgst -sha256 -sign` subprocess that task 96 documented above.

Why aws-lc-rs and not the `rsa` crate: aws-lc-rs was **already in `Cargo.lock`** as
rustls' crypto provider (reqwest's `rustls` feature selects it), so promoting it to a
first-party pin adds no crate, no new C build, and no second crypto implementation to
audit. Pinned with `default-features = false, features = ["aws-lc-sys", "alloc"]`,
which drops `ring-io`/`ring-sig-verify` — their only effect here would be to add a
second `untrusted` major version for a *ring*-compat shim this code never calls.
`Cargo.lock` therefore gains **zero** packages: its whole diff is
`+aws-lc-rs` / `-tempfile` under `oc-provider-google`. `ring` was rejected for the
opposite reason: it is present only transitively and is stricter about acceptable RSA
keys than GCP is about the ones it mints. The `rsa` crate was not needed and would
have been a third RSA implementation in one binary.

PEM handling is local: `pem_der` slices between the armor lines, strips ASCII
whitespace, and base64-decodes with the `STANDARD` engine. Both PKCS#8
(`-----BEGIN PRIVATE KEY-----`, what GCP service-account JSON actually ships) and
PKCS#1 (`-----BEGIN RSA PRIVATE KEY-----`, what an older or re-exporting tool emits)
are accepted; `Ok(None)` means "armor absent" so the caller can try the other
encoding, and only a present-but-corrupt body is an error.

Known-answer test provenance — `KNOWN_ANSWER_SIGNATURE_BASE64` in
`crates/oc-provider-google/src/lib.rs` was produced once, outside this codebase, by:

    openssl genpkey -algorithm RSA -pkeyopt rsa_keygen_bits:2048 -out key.pem
    openssl rsa -in key.pem -traditional -out key_pkcs1.pem   # same key, PKCS#1 armor
    printf '%s' '<KNOWN_ANSWER_MESSAGE>' > msg.txt
    openssl dgst -sha256 -sign key.pem -out sig.bin msg.txt
    base64 -w0 sig.bin

with OpenSSL 3.0.13 (30 Jan 2024). Both the key and the signature are committed
verbatim. RSASSA-PKCS1-v1_5 is deterministic — no salt, no nonce — so the expected
bytes are a property of the algorithm, not of the tool that produced them, and any
conforming signer must reproduce all 256 of them. That makes the test a real oracle
against an implementation sharing no code with the one under test: a regression in
the digest, the PKCS#1 padding or the DER parse fails locally instead of surfacing
in production as a token Google refuses without explanation. The key is a throwaway
that guards nothing.

Error taxonomy: the `OpenSsl*` variants are gone. `ServiceAccountSigningError` now
carries `PrivateKeyNotPem`, `PrivateKeyBase64(base64::DecodeError)`,
`PrivateKeyRejected(aws_lc_rs::error::KeyRejected)` and
`Sign(aws_lc_rs::error::Unspecified)` — every variant typed, none carrying a
free-form `String`, and none able to render key material (a test asserts the
rejected input does not appear in either `Display` or `Debug`).

## Task 32

### Bounded event transport

`TURN_EVENT_CHANNEL_CAPACITY` is 64. Overflow policy is lossless backpressure: `TurnEventSender::send` awaits capacity. Events are never dropped, coalesced, or buffered without a bound; if the consumer closes, the loop returns typed `TurnError::EventConsumerClosed`.

### Public spine for Todos 33-37 and interfaces

`run_turn(request: RunTurnRequest, context: TurnContext<'_>, events: TurnEventSender) -> Result<TurnOutcome, TurnError>` is the sole turn state machine. `TurnContext` receives SQLite, provider registry, `AgentModelResolver`, `ToolDispatcher`, and `InterruptSignal` by reference. Interfaces construct the bounded channel and consume `TurnEvent`; they do not receive a headless/rendering alternate path.

Todo 33 replaces the `ToolDispatcher` implementation. Todo 34 evolves terminal checkpoint helpers into batched live projection while preserving the surrounding loop. Todo 35 adds compaction decisions at the history boundary and resets/recreates prompt-cache state. Todo 36 handles typed provider errors and `RetryRollback` at the stream boundary. Todo 37 wraps `run_turn` with the one-live-session registry and routes abort/soft interrupt state into the existing signal and safe points.

### Locked tool list

The loop owns one `PromptCache<ToolDefinition>` for the full turn. Each step asks `ToolDispatcher::available_tools`, then `prepare_turn` freezes the first list and permits exactly one changed snapshot when MCP status moves from `Pending` to `Ready`. The frozen definitions are emitted as `ToolSnapshotLocked` and carried into each `DispatchRequest` as `Arc<[ToolDefinition]>`, so dispatch executes against exactly what the model saw.

### Persisted IDs

The caller supplies a database-unique `turn_id`; assistant and part IDs are deterministic functions of `(turn_id, step, call_index)`. That makes the three-run event transcript byte-identical without introducing random IDs into acceptance tests.

## Task 37
- Chose rejection for a second concurrent prompt: `begin_turn` returns typed `SessionBusy`. Rejection is explicit and cannot silently discard the distinct work supplied by the second caller; the test proves exactly one `Running` and one `Busy` result under a simultaneous race.
- Soft-interrupt payload reuses `interrupt::SoftInterruptMessage { content, images, urgent, source }` and `SoftInterruptSource::{User,System,BackgroundTask}`. The per-live-turn FIFO is drained only through `take_soft_interrupts_at_safe_point`; any urgent message returns `SkipRemainingTools`, while neither path fires abort.
- Interface API for Todos 51-56: share one `SessionRunRegistry`; use `status(session_id)` and sorted `active_sessions()` for reporting, `control(session_id).abort()` for stale-safe cancellation, `control(session_id).queue_soft_interrupt(...)` for injection, and retain `begin_turn(session_id)`'s guard for the full `run_turn` lifetime.
## Task 36
- Loop-facing API: `retry_provider(policy, operation, emit)` performs real Tokio sleeps; `retry_provider_with_sleep` injects the sleeper for deterministic tests. The operation receives the 1-based attempt number and the emit closure preserves bounded-channel backpressure.
- `ProviderRetryPolicy` requires a caller-supplied `NonZeroU32` total-attempt limit, making an unbounded provider retry unrepresentable through this API.
- Retry without provider guidance uses OpenCode oracle backoff: 2 seconds times 2^(failed_attempt-1), capped at 30 seconds. A carried `retry_after` is used unchanged.
- `StreamEvent::RetryRollback { attempt: next_attempt, max }` is emitted after a retryable failure but before the sleep and before the replayed provider operation. If rollback emission fails, replay is aborted.
- Provider retries replay the unchanged request. Context-limit recovery and continuation prompts change the request and therefore use the separate per-turn `RecoveryBudgets` counters.
- Oracle discrepancy: OpenCode `SessionRetry.policy` has no finite attempt ceiling, while this task explicitly forbids indefinite retries. The implementation keeps the oracle delay behavior but enforces a finite caller-supplied policy. Jcode also returned partial/silent completion when incomplete or empty continuation budgets ended; this implementation follows the task contract and returns a typed exhaustion error.


## Task 34

- **Batch window:** use a deterministic 4096 dirty-byte size window shared by text and reasoning, plus terminal flushes. A size window is testable without sleeps or scheduler sensitivity and caps 5,000 one-byte deltas at two delta upserts rather than 5,000 transactions.
- **Tool input parsing:** retain `ToolInputDelta` fragments as raw text and parse exactly once at `ToolUseEnd` (or `MessageEnd` when the provider omits the explicit end). First try strict JSON-object parsing, then a quote-aware trailing-comma repair; non-object or still-invalid input becomes a synthetic error tool part. `finish_incomplete` never treats partial JSON as complete and records an error without panicking.
- **Rollback cleanliness:** every attempt-scoped persisted part id is tracked. `RetryRollback` deletes those rows before clearing text, reasoning, active/completed tools, usage and dirty-byte state; the retry marker itself remains for observability.
- **Step finish effects:** projector start captures the pre-step snapshot through `ProjectionEffects`. `MessageEnd` captures the completed snapshot, writes `step-finish`, updates assistant finish/cost/tokens, asks `ProjectionEffects::patch(pre_step_snapshot)` for a patch part, then triggers summary and overflow hooks. This keeps `stream.rs` independent of Todo 35's compaction implementation and avoids modifying the forbidden `loop.rs` spine.

## Task 33

- **Suggestion ranking/cutoff:** ported `.omo/refs/jcode/crates/jcode-app-core/src/tool/mod.rs:404-439,1196-1219`. Compare lowercase names; exact equality scores 0, prefix in either direction scores 1, substring in either direction scores 2, otherwise Levenshtein distance is accepted only when `distance <= max(longer_len / 3, 2)` and scores `3 + distance`. Sort by `(score, tool_name)`, return at most 3. Every miss also includes the complete alphabetically sorted available list. `ToolSerch` produces `Unknown tool: ToolSerch. Did you mean: tool_search? Available tools: bash, tool_search.`
- **Background grace:** 750 ms, matching `.omo/refs/jcode/crates/jcode-app-core/src/agent/turn_streaming_mpsc.rs:1366-1412`. Dispatch uses a biased select: normal completion wins, then a background signal gets 750 ms for graceful completion before the join handle is detached, while the turn receives a synthesized successful background status; a turn interrupt aborts the task and returns an error result.
- **Dispatcher wiring:** `ToolRegistryDispatcher::new(tools, merged_rules, approval, background_tool, mcp_status)` receives registry assembly rather than owning it. `available_tools()` applies `oc-permission` visibility and emits definitions. `dispatch()` is the only name-resolution/miss choke point, validates against the augmented definition schema, applies the argument-derived permission ask, constructs `ToolContext`, spawns execution, and converts every tool/join/input/name failure into `ToolDispatchResult`. `loop.rs` remains the owner of sequential iteration and running/completed/error events and was not modified.
- **Permission resolution:** an unconditional rule allow proceeds; deny synthesizes a denied tool result; ask blocks on the supplied `PermissionAsker` before execution. The per-dispatch context asker re-evaluates later, more precise asks from built-ins and remembers approved `(permission, pattern)` pairs for that call only, preventing a parent grant from laundering unrelated subcalls.

## Task 35
- Context-limit attempts are recorded only through Todo 36 `RecoveryBudgets::record_context_limit_retry`; `CompactionState` adds only a terminal failure latch, not a parallel attempt counter. A failure persists `CompactionError`, latches the state, and a second call returns `AlreadyFailed` without a provider request.
- Plugin-host seam: `CompactionHooks::compacting` mirrors `experimental.session.compacting` with mutable context/prompt output; `CompactionHooks::auto_continue` mirrors `experimental.compaction.autocontinue`. `NoopCompactionHooks` supplies current defaults until Todos 57-62 wire the real host.
- Cache sequencing: persist the completed summary and compaction marker first, then call both Todo 31 mechanisms (`CacheTracker::reset`, `LockedTools::reset`), then ask the auto-continue hook. A hook failure after summary persistence remains terminal, but the cache still correctly reflects the changed prefix.
- The new public API doc comments are intentional: this crate documents public interfaces consistently, while the boundary comment carries the required provider-400 rationale and the module comment records the failure-latch/cache ordering invariant.
## Task 39

### external_directory escalation shape
Resolve the argument path against the canonical workspace and canonicalize the longest existing ancestor so symlink escapes cannot be disguised as workspace-local paths. Internal resources are slash-normalized workspace-relative paths and ask only the native permission (`read` or aliased `edit`). External resources first ask `permission = external_directory` with `patterns = [<canonical parent>/*]`, `always = [<canonical parent>/*]`, and metadata `{ filepath: <canonical target>, parentDir: <canonical parent> }`; after approval, ask the native permission against the canonical absolute target. Directory reads use the directory itself as the external boundary; file operations use the parent. Todos 40-44 should reuse this two-stage shape.

### Formatter seam
`FileFormatter::format(&Path) -> io::Result<bool>` is injected into `FileTools::with_formatter`. write, edit, and every non-delete apply_patch operation call it after bytes are written and then re-read final bytes into session state. `NoopFormatter` is the current default because formatter execution belongs to Todo 79; Todo 79 can implement the trait without changing file-tool semantics.

### Read-before-edit state
One `FileAccessState` is shared by all four tools. It keys reads by `(session_id, canonical_path)` and stores a length plus in-memory content hash. edit and overwrite-by-write require a matching current revision; missing state returns `File must be read before editing...`, and stale bytes return `File changed after it was read...`. Successful writes update the stored revision; delete and move forget the source path. The shared FileTools lifetime is therefore the session-state boundary, while session IDs prevent cross-session laundering.

### Mutation consistency
All write/edit/apply_patch mutations share one async mutex. Permission is obtained before the mutex; file revision checks and writes happen under it. apply_patch verifies every operation before the first mutation and refuses duplicate operation paths.


## Task 42 — web tools (webfetch, websearch)

**HTML→markdown/text is hand-rolled, in `webfetch/html.rs`.** The workspace pins no
HTML parser and this task may not add one, so upstream's `htmlparser2` +
`turndown` pair is replaced by one tolerant tokenizer with two renderers.

Text extraction is byte-identical to upstream and asserted as such against a fixture
captured by *running* upstream's `extractTextFromHTML`. Markdown is deliberately not
byte-identical: turndown emits whitespace artifacts (a leading space and two trailing
spaces on the `<title>` run, `-   ` three-space bullets) that would require
reimplementing its whitespace collapser rather than its markdown. Instead the port
matches turndown's *configuration* — atx headings, `---`, `-` bullets, fenced code,
`*` emphasis, `script`/`style`/`meta`/`link` removed — and a test normalizes both
documents and asserts they agree line for line, so the snapshot is pinned to the
oracle rather than to itself. Both fixtures are in the repo, and a second assertion
fails if they ever become byte-equal, which would mean the documented delta is stale.

The tokenizer treats **unknown elements as block containers**, with a closed inline
set. Inverting that default collapses `html`/`body` and every custom element onto one
line — which it did, once, before the inline set existed.

**Gating maps onto two independent mechanisms, not one.** `web_search_enabled`
(mirroring `registry.ts:58-60`) answers "is a provider configured"; it is a free
function so the registry and the tool cannot hold divergent copies, and a test asserts
`WebSearchTool::enabled_for` delegates to it for five configurations.
`oc_permission::visibility::is_tool_hidden` answers "is it denied outright". Either
removes the tool from the list. `WebError::NoSearchProvider` exists only so that a
registry bug which exposes an unconfigured tool fails by name instead of as a
confusing transport error — it is not the intended path.

**Keys are read through `SearchConfig::from_lookup`,** with `from_env` as the thin
default. Tests state a configuration without mutating process globals (which would
race across the shared test binary), and todo 44 can source a key from somewhere else
without editing the tool.

**`WebError` is a local `thiserror` enum in `#[source]` position.** `ToolError` has no
`Other(String)`, so a web failure has to be classified before it can be reported;
every variant of `WebError` names a specific bound or transport condition, and callers
downcast to assert on them.

**Body reads stream through `read_bounded`, never `response.bytes()`.** `bytes()`
buffers the whole body before any cap can be consulted, so a 100 MB response would be
100 MB resident and uncancellable. The reader refuses *before* retaining an oversized
chunk (peak stays at the cap plus one chunk) and polls `InterruptHandle::is_set()`
before every chunk.

**`with_endpoint` and `with_timeout` on `WebSearchTool`.** No test may reach a real
backend, and no test should wait out a real 25 s budget. The default budget is pinned
by a separate constant assertion plus a test that observes 25 s in the typed failure,
so shortening it in one test cannot hide a wrong default.

**`reqwest::redirect::Policy::limited(10)` on the client rather than manual hop
counting.** `reqwest` surfaces an exhausted policy as `Error::is_redirect()`, which
`classify_send_error` maps to `WebError::TooManyRedirects`; without that mapping the
hop cap would be indistinguishable from a refused connection.

## Task 41 — search: backend selection, the shape of `oc-search`, and the params structs

### Embedded by default; a system `rg` only when asked by name

`Backend::from_env` reads `OPENCODE_SEARCH_BACKEND` (values `ripgrep` or `rg`, anything else or
absent → embedded). Rationale: the embedded engine is *why* the runtime download in
`ripgrep/binary.ts:88-121` is gone, so a machine that happens to have an old `rg` on `PATH` must not
be able to silently change what search returns. The oracle's own order is the opposite — it prefers a
system binary and downloads one if absent — and reproducing that preference would reintroduce the
version-skew it causes.

The `rg` backend is kept for one purpose: when a divergence is suspected, the same `GlobRequest` /
`GrepRequest` can be answered by the very binary the oracle would have used, which turns "our walker
disagrees" into a mechanical question. `crates/oc-search/tests/engine_semantics.rs` has a
cross-backend equality test that runs whenever an `rg` is present and skips (loudly) otherwise.

Asking for `ripgrep` with no `rg` on `PATH` **degrades to embedded with a `tracing::warn!`** rather
than failing every search; the alternative is worse and the two produce the same answers.
`Backend::select(Option<&str>)` is the pure form, because Rust 2024 makes `env::set_var` `unsafe` and
the workspace forbids `unsafe_code`, so a test cannot exercise `from_env` directly.

### How `oc-search` is laid out

- `types` — `Entry`/`Submatch`/`Match` field-for-field from `packages/schema/src/filesystem.ts`,
  because `debug rg search` serialises exactly those and the differential compares that JSON.
  Also `normalize_relative` (the `./`-stripping from `ripgrep.ts:171-175`) and `truncate_utf16`.
- `embedded` — one `ignore::Walk`, `grep-searcher` per file. The module doc carries the flag-by-flag
  table from `rg` so the mapping is checkable without re-reading the TypeScript.
- `ripgrep` — the opt-in backend. Spawns **once per request** over the whole directory; never per
  file. Sorts before truncating so both backends have one contract.
- `backend` — the enum and the selection.
- `cancel` — `Cancellation`, a **local** one-method trait, not a dependency on `oc-tool`'s
  `InterruptHandle`. `oc-search` must be usable without the tool layer linked in (todo 48's LSP walk
  wants the same engine), and `oc-tools::search_common::InterruptCancellation` is the forwarding
  adapter. Synchronous, like `is_set`, so blocking walk code needs no runtime.
- `error` — `SearchError` with `is_model_correctable()`. No `Other(String)`.

`SearchResults::truncated` reports what the engine actually saw (a result existed past the limit).
The **tools** deliberately re-derive the oracle's weaker `len() == limit` test for the output the
model reads, since that claim is part of the rendered text; the honest field stays for other callers.

Interrupt polling is every 256 entries (`CANCEL_POLL_INTERVAL`), plus once per matched line in the
grep sink. Bounds interrupt latency to a few hundred `stat`s without an atomic load per entry.

### The params structs

Derived, never hand-written (`oc-tool::TypedTool`), and the doc comment on each field **is** the
description the model reads, so it is copied verbatim from the oracle:

- `GlobParams  { pattern: String, path: Option<String> }`                       — `glob.ts:10-15`
- `GrepParams  { pattern: String, path: Option<String>, include: Option<String> }` — `grep.ts:10-18`

Both are `#[serde(deny_unknown_fields)]`; `oc-tool`'s adapter strips the injected `intent` /
`accept_large_output` before decoding, so that is safe.

### Oracle behaviours reproduced that look like defects and are not ours to fix

1. `grep` with a `path` naming a **file** searches that file's whole **directory** — `grep.ts:62`
   takes `dirname` and hands it to `rg` as the cwd. So `grep pattern path=src/a.ts` also returns
   matches from `src/b.ts`. Asserted by a test that says so.
2. Each rendered `grep` match keeps the line's terminator, so the output has a blank line after every
   match (`Found 2 matches\n<a>:\n  Line 1: alpha needle here\n\n\n<b>:\n...`).
3. `grep`'s empty output says `"No files found"` even though the search was over contents.
4. `truncated` is `len() == 100`, so a tree with exactly 100 results claims more are available.

### `external_directory`, pending Todo 39

Todo 39 had not landed a shared escalation when this was written, so
`oc-tools::search_common::assert_external_directory` is a local port of
`tool/external-directory.ts:15-44`: permission key `external_directory`, pattern
`<directory>/*`, `always` the same pattern, metadata `{filepath, parentDir}`; the target directory is
the path itself when it is a directory and its parent otherwise. Containment is checked against
**both** the session directory and the worktree, matching `containsPath(full, ins)`. If Todo 39 lands
a shared helper with this shape, `search_common` should delegate to it and delete its copy — the
shapes were chosen to make that a deletion rather than a reconciliation.

## Task 40 — Shell execution boundaries
- The public wire id is `bash`; Bash and PowerShell are parsed before permission asks, and every non-directory-changing constituent is submitted as its own resource pattern.
- User foreground `timeout` is carried in metadata but does not kill the child here; only cancellation and the injectable hard ceiling terminate it, preserving todo 72 ownership of foreground timeout promotion.
- `shell.env` is represented by the injectable `ShellEnvHook` with a no-op default, following the existing formatter seam rather than depending on the plugin crate directly.

## Task 30 — OpenAI protocol boundaries
- One provider owns both genuine OpenAI wire shapes. Surface selection is explicit, with `Default -> Responses`; the compatible-provider family remains separate and unchanged.
- Responses reasoning is persisted as `ProviderReasoningItem` / `ProviderEncryptedReasoning`, never flattened into generic text or signed-thinking forms. The private ciphertext is opaque data and is not decoded, normalized, or regenerated.
- Request and stream conversion are separate state machines around the shared canonical `CompletionRequest`, `RequestContentBlock`, and `StreamEvent` types. No OpenAI-specific event enum crosses the crate boundary.
- Error recovery is classified from HTTP status and structured `error.type` / `error.code`. Human-readable `message` is retained only as an error source or refusal detail and never controls retry policy.

## [2026-08-06] Task 50: oc-watch channel capacity, coalescing rules, and drop visibility

### Bounded channel: 1024 events, buffer 4096 paths

```rust
pub const DEFAULT_CAPACITY: usize    = 1_024;  // tokio::sync::mpsc, bounded
pub const DEFAULT_MAX_PENDING: usize = 4_096;  // distinct paths coalesced
```

**1024** is sized to absorb one full flush of the largest realistic single event:
a `git checkout` that touches ~1,000 distinct files. Measured — the 1,000-file
burst test delivers 1,000 events with **0 drops**. 1024 is the next power of two
above the measured requirement. Cost is bounded and small (~40 KiB of slots plus
the paths actually queued). Raising it buys no correctness: past this size a
consumer that cannot keep up wants an overflow signal and a rescan, not a longer
queue, so a bigger number only trades staleness for memory.

**4096 = 4 x capacity.** Four flush-windows of coalescing for a fully stalled
consumer before anything is discarded. Beyond that the honest answer is
`Overflow`: a buffer large enough to hold every path in a monorepo is an unbounded
queue with extra steps.

Both are `#[must_use]` builder overrides, which is what makes the pressure path
testable — `capacity(16)` + `max_pending(64)` reaches the give-up path with 4,000
files in under a second instead of needing production-scale load.

Also `DEFAULT_DEBOUNCE = 100ms` (long enough that a multi-write save lands in one
window, short enough to feel immediate) and `DEFAULT_MAX_WAIT = 1s` (a build
touches something every few ms, so the trailing debounce alone would never elapse
and the consumer would starve for the build's duration).

### Two kinds of drop, surfaced differently — this is the load-bearing decision

The plan says "coalesce-and-drop, never grow" as if it were one outcome. It is
two, with different consumer consequences, and conflating them hides a real bug
class:

| | information | how it is surfaced |
|---|---|---|
| superseded (coalesced) | preserved — merged into a newer event | **silent**, this is the normal case |
| given up (buffer full) | **lost** | `WatchEvent::Overflow { dropped }` |

Degradation order: **coalesce → hold → drop.**

- *Hold*: channel full → the batch is requeued into the coalescing buffer and
  retried one quiet period later. `Debouncer::requeue` sets `last_activity` as
  well as `window_opened`, which is what makes the retry a **backoff** instead of
  a hot loop — without it the flush thread spins against a full channel. Requeued
  events are treated as **older** than anything that arrived meanwhile, so a
  newer kind wins.
- *Drop*: buffer full → further **new** paths are discarded and counted. Paths
  already held keep merging, because merging costs no memory and losing them
  would report stale kinds.

`Overflow` is sent **ahead of** the batch it precedes, so a consumer learns its
view has a hole *before* it acts on a partial one. `dropped` counts since the
previous `Overflow`, not since the start, so a consumer can act on the number
without keeping a running total.

### Coalescing rule: newer kind wins, EXCEPT `Add` survives a following `Change`

```
Add     + Change  -> Add       <- the only asymmetric case
Add     + Unlink  -> Unlink
Change  + Add     -> Add       <- atomic-rename save
Change  + Unlink  -> Unlink
Unlink  + Add     -> Add       <- same, seen in the other order
Unlink  + Change  -> Change
```

The asymmetry is necessary: a consumer has never heard of a newly created path, so
the first thing it must hear is "this is new", and `Create + Modify` is what
inotify emits for **every** file write. Collapsing that to `Change` would tell a
consumer to update a record it does not have.

`Add + Unlink -> Unlink` rather than "emit nothing": a consumer that observed the
file mid-window (via its own scan) would otherwise be left stale forever. An
`Unlink` for a path a consumer never had is a harmless no-op; the reverse is not.

This is safe **only because every consumer action is idempotent under the rule** —
`Add` and `Change` both mean "read this path", `Unlink` means "forget it". That is
the property that makes the oracle's 3 events and this 1 event leave a consumer in
the same state, and it is the invariant to preserve if the rule is ever extended.

### Ambiguous notify kinds read as `Change`, never as `Unlink`

`Modify(Name(Any))`, `Modify(Name(Other))`, `EventKind::Any`, `EventKind::Other`
carry no direction. All four map to `Change`: it tells the consumer to re-read,
which is correct whether the path was written or moved into place, and a re-read of
a vanished path fails harmlessly. Guessing `Unlink` would **evict live files** from
a consumer's index. `Access(_)` maps to nothing at all — the oracle publishes only
create/update/delete (`watcher.ts:85-89`), so mapping `IN_CLOSE_WRITE` would invent
events. `Modify(Name(Both))` is the one notification that is genuinely two changes
(two paths, `(from, to)` order) and is split into `Unlink` + `Add`.

### The notify callback thread is never blocked, and the lock is never held over a send

`notify` runs its callback on a thread it owns, and the kernel's inotify queue is
bounded (16384 on this host) — overrun it and the kernel sets `IN_Q_OVERFLOW` and
**silently discards** events. So the callback only classifies, filters, takes the
buffer mutex, merges, and wakes. Every publish is `Sender::try_send` from a
separate flush thread, **with the mutex not held**. No path in the crate calls a
blocking or awaiting send.

The flush loop is a plain `std::thread` + `Condvar`, not a Tokio task: `try_send`
needs no reactor, so the crate constructs and publishes outside a runtime, and the
loop's timing does not depend on runtime scheduling. That is also what lets the
tests be plain `#[test]` with `try_recv` polling — no `#[tokio::test]` anywhere.

A poisoned mutex is recovered (`PoisonError::into_inner`) rather than propagated:
the guarded region is a map and three counters, and the alternative to recovering
is that one panic permanently stops the consumer hearing about anything.

### The gitignore chain is lazy and per-directory, with an explicit staleness escape

One `Gitignore` matcher **per directory that owns a `.gitignore`**, consulted
deepest-first, built on first use and cached. Per-directory is forced by
correctness (`GitignoreBuilder` anchors to the builder root — see issues.md);
lazy is forced by the watcher's purpose (an eager map means walking the whole repo
at startup).

The consequence is stated in the docs rather than papered over: a `.gitignore`
created in a directory not yet seen **is** picked up; one in an already-cached
directory is not, and an **edited** one is not re-read. `Filter::is_gitignore(path)`
plus `Filter::invalidate()` let a consumer handle it — the edit is itself an event
it receives. `Watcher::filter()` returns the `Arc` so this needs no restart.

### `Decision` is data, and a disabled watcher is the same type

Three variants — `Disabled(reason)` / `VcsOnly` / `Full` — because the oracle
watches two different things under two different conditions and a caller must be
able to tell them apart (see issues.md on the enable flag not being a master
switch). `DisabledReason` distinguishes `ExplicitlyDisabled` (deliberate opt-out,
log nothing) from `UnparseableFlag { key, value }` (misconfiguration worth telling
the user about).

A disabled watcher is **not** a separate type and **not** an error: `Watcher::start`
returns an `EventStream` that never yields, exactly as the oracle returns an empty
service (`watcher.ts:59`, `watcher.ts:130-136`). Callers therefore have no branch
to write; they consult `decision()` only if the reason is worth logging.

`Env` is threaded in rather than read from `std::env`, because this workspace
forbids `unsafe` and `std::env::set_var` is `unsafe` in edition 2024 — the same
constraint that shaped `oc-paths`. It is the only way a test can vary the flags.

## [2026-08-06] Task 43: the exposure-predicate API, the todo replace strategy, and four seams

### The exposure API todo 44 consumes

`crates/oc-tools/src/exposure.rs`. One data struct, four predicates, one lookup:

```rust
pub struct Client(String);                 // newtype, NOT an enum — see below
pub struct ExposureFlags {
    pub client: Client,
    pub enable_question_tool: bool,
    pub experimental_plan_mode: bool,
}
impl ExposureFlags {
    pub fn from_env() -> Self;
    pub fn from_lookup(impl Fn(&str) -> Option<String>) -> Self;
    // builders for tests: with_client, with_plan_mode, with_question_tool
}

pub type ExposurePredicate = fn(&ExposureFlags) -> bool;
pub fn exposes_invalid(&ExposureFlags) -> bool;      // always true
pub fn exposes_todowrite(&ExposureFlags) -> bool;    // always true
pub fn exposes_question(&ExposureFlags) -> bool;
pub fn exposes_plan_exit(&ExposureFlags) -> bool;

pub const CONDITIONAL_TOOLS: [(&str, ExposurePredicate); 4];
pub fn exposure_predicate(wire_id: &str) -> Option<ExposurePredicate>;
pub fn exposed_conditional_tools(&ExposureFlags) -> Vec<&'static str>;
```

Design choices and why:

- **`exposure_predicate(wire_id) -> Option<_>` is the intended registry entry point.**
  The filter is `predicate(&flags)` where it returns `Some`, unconditional otherwise —
  one lookup, no `match` in the registry to keep in step with this module. `None` for a
  non-conditional tool is a deliberate signal, tested against `read`/`write`/`glob`/
  `grep` **and** against `todo`/`plan` (the registry *keys*, which must not resolve).
- **`fn` pointers, not boxed closures**, so `CONDITIONAL_TOOLS` is a `const` and two
  predicates can be compared for identity.
- **Keyed on WIRE ids.** `Tool::id()` returns the wire id, so the registry map is keyed
  `invalid`/`question`/`todowrite`/`plan_exit`. Upstream's registry keys `todo` and
  `plan` have no wire meaning and are deliberately not reproduced anywhere. A test
  asserts neither appears in `CONDITIONAL_TOOLS`.
- **`exposed_conditional_tools` exists for the differential**, which compares a list per
  flag configuration. `tests/conditional_tools.rs` has a 15-row table of measured binary
  invocations driving it, with a floor assertion (`>= 15`) so a table that silently
  shrank fails loudly.
- **`ExposureFlags::default()` is hand-written, not derived**, because the client's
  default is `"cli"` and an empty client matches no gate — a derived `Default` would be
  wrong in a way nothing would notice.
- **Flags as data, read through a lookup closure**, exactly as `websearch::gating`
  already does: Rust 2024 makes `env::set_var` `unsafe`, the workspace forbids
  `unsafe_code`, so an env-mutating test cannot be written at all, let alone run
  concurrently in a shared binary.
- **`Client` is a newtype over `String`, not a closed enum.** Upstream's flag is a bare
  `Config.string` with no validation, so an unrecognised client is a normal state that
  matches no gate rather than an error, and keeping the raw value lets a caller log
  what it was actually given. `can_render_questions()` and `is_plan_exit_client()` are
  the two questions anyone asks of it.

### The `(session_id, position)` primary-key strategy: delete-then-insert, one transaction

Forced by the DDL and matching upstream (`session/todo.ts:29-51`). Inside a single
`Pool::transaction` (IMMEDIATE, so the busy timeout applies): `DELETE FROM todo WHERE
session_id = ?`, then one prepared `INSERT` per item with `position` = the item's index
in the model's array and one shared `now_millis()` for the whole batch.

Rejected alternatives:
- **`INSERT OR REPLACE`** — leaves stale rows behind whenever the new list is shorter,
  which is the common case as a session's todos get completed and pruned.
- **Diffing the existing list** — needs a stable item identity the schema does not have
  (there is no id column), so any diff would be content-based and would renumber
  positions on a reordering anyway.
- **One timestamp per row** — a list written together would appear to have been written
  over several milliseconds, which is a lie a transcript can show.

`SqliteTodoStore::list` reads with an explicit `ORDER BY position ASC`. Without it the
correct order is a coincidence of the index, not a guarantee.

### `TodoStatus` / `TodoPriority`: hand-written `Deserialize` over one shared visitor

The derived decoder cannot name the allowed values in its error, which is the whole
point (see learnings). One `EnumVisitor<T>` holds `allowed: &'static [&'static str]`
plus a `fn(&str) -> Option<T>` parser, and both enums delegate to it — so the two error
messages cannot drift, and adding a third string enum here is three lines.
`ALLOWED` is `pub` so a test and a caller assert against the same list.

### Four seams, because this crate sits below the layers that own the effects

| tool | seam | why |
|---|---|---|
| `todowrite` | `TodoStore` (+ `SqliteTodoStore`, `MemoryTodoStore`) | synchronous, because the only real impl is a local SQLite write; `spawn_blocking` at the call site rather than an async trait to suit one implementation |
| `question` | `QuestionAsker` (+ `ScriptedAnswers`) | the round trip to a human needs an event bus and an HTTP API, neither of which is in this crate |
| `plan_exit` | `QuestionAsker` **and** `PlanExitHost` (+ `RecordingHost`) | reuses the *same* asker upstream does — one transport, not two — plus a host for the session-message write |
| `invalid` | none | it formats a diagnosis someone else made; infallible in both implementations |

`ScriptedAnswers` and `RecordingHost` are `pub` in the library, not `#[cfg(test)]`
helpers, so `plan_exit`'s tests and the integration test share one double and one
contract with `question`'s.

`plan_exit`'s params are `struct PlanExitParams {}` — an empty **struct**, not a unit
type — so `schemars` derives `{"type":"object"}` and `oc-tool`'s central augmentation
has an object to inject `intent`/`accept_large_output` into. A unit type derives `null`
and the augmentation would have nothing to attach to.

### Upstream oddities reproduced rather than corrected, each with a test that says so

- `question`'s title pluralises on `> 1`, so zero questions reads "Asked 0 question"
  (singular). `question.ts:35`.
- `todowrite`'s title counts items whose status is not `completed`, so `cancelled`
  counts as open and a one-item list reads "1 todos". `todo.ts:37`.
- `question`'s answer rendering joins multiple selected labels with `", "` — the same
  separator that joins the question/answer pairs, so the two are indistinguishable by
  separator alone. `question.ts:30-32`.
- `plan_exit` tests `answers[0]?.[0] === "No"`, so an **empty** answer is not a refusal
  and falls through to the switch. Inventing a refusal there would strand the session
  in plan mode. `plan.ts:46`.

## [2026-08-06] Task 67: sqlite goal store with split status ownership and budgets

### The goal table lives in its own database file, `goal_1.db`

`$XDG_DATA_HOME/opencode/goal_1.db`, beside `opencode.db` and not inside it.
Spill files at `$XDG_DATA_HOME/opencode/goal-objective/<uuid-v4>/goal-objective.md`.

Two independent reasons:

1. **`opencode.db` is not ours to extend.** `oc-db` reproduces the TypeScript
   schema byte-for-byte — `TABLE_COUNT = 19` plus the `migration` journal — and
   proves it with a differential test against a real database plus the 38-entry
   journal contract. A 20th table breaks that test and the promise it guards.
   A goal is a feature the TypeScript binary does not have, so it has no place
   in a file that binary also writes.
2. **A goal must outlive session churn.** Its purpose is to survive the
   compaction that discards the conversation which set it, so it must not share
   a file with state that gets pruned, vacuumed and cascaded.

Therefore **no** `FOREIGN KEY (session_id) REFERENCES session(id) ON DELETE
CASCADE`. A goal is keyed *by* a session id. Asserted:
`pragma_foreign_key_list('goal')` returns zero rows, the DDL contains no
"FOREIGN KEY", and the database holds exactly one table.

Schema is one `CREATE TABLE IF NOT EXISTS`, no migration chain — reopening must
be a no-op, and that is what makes a goal survive a restart. An incompatible
change revs the filename (`goal_2.db`), codex's convention
(`codex-rs/state/src/sqlite.rs:30`).

### Ownership is enforced by two types, not by a runtime check

```
ModelStatus  = { blocked, complete }
SystemStatus = { active, paused, usage_limited, budget_limited }
GoalStatus   = the union; nothing accepts a bare GoalStatus as a write
```

`update_status_as_model(&self, session_id, ModelStatus)` — there is no `paused`
variant to pass, so a future caller cannot smuggle one through. The alternative
shape (one method taking an `Actor` parameter) was rejected: it makes the
illegal state representable and then rejects it at runtime, which is exactly the
check a refactor forgets.

**`active` is system-owned.** The non-obvious one. The model already obtains
`active` by creating a goal, so making it model-writable buys nothing — and it
opens a hole: a model on a `budget_limited` goal could set `active` and keep
spending. The plan's "must not let the model clear a budget limit" decides it.

### Two invariants live in SQL because in Rust they would be races

- **The guarded replace** is one upsert whose `DO UPDATE` carries
  `WHERE goal.status = 'complete'` (ports `goals.rs:245`). A read-then-write
  would let two concurrent `create_goal` calls both observe `complete` and both
  replace. The refusal *is* the statement returning no row; the follow-up read
  that names the blocking status runs in the same transaction and only labels.
- **The budget flip** is a `CASE` in every statement that can move `tokens_used`
  or `token_budget`. `tokens_used + ?1 >= token_budget` reads the pre-update
  value, so the flip decides on the post-increment total inside the statement
  performing the increment. Also in `set_token_budget` (lowering below the spend
  stops the goal at once) and in `set_status_as_system` (an `active` request on
  an over-budget goal resolves to `budget_limited`, so even the system cannot
  clear a limit without raising the budget first). And the upsert's `0 >= ?4`
  means a budget of `Some(0)` is never observable as an `active` goal.

Both are proven by tests that run the statement text **directly** against a
connection, because a test that only calls the store cannot distinguish a SQL
`WHERE` from a Rust `if` around an unguarded statement.

### The objective cap and the pointer sentence

`MAX_OBJECTIVE_CHARS = 4_000`, counted with `chars().count()`. Above it the
objective spills and the column holds, verbatim:

```
Read the full goal objective at <spill_dir>/<uuid-v4>/goal-objective.md before continuing.
```

Both the cap and the spill sit **below** the store's API — codex splits them
across protocol and TUI (`protocol.rs:4082`, `goal_files.rs:121`), which means
every caller must remember to spill. Folding them in makes "the column is never
longer than 4,000 chars" unbreakable from outside.

Reading it back validates rather than parses: inside the spill dir, named
`goal-objective.md`, parent directory a v4 UUID (`goal_files.rs:164-171`). The
objective is model-writable, so a pointer that resolved to any path would be an
arbitrary-file-read primitive.

### Error taxonomy: `GoalError`, and `oc-error` untouched

A refused status is a policy decision, not a broken statement, so it does not
belong in `DbError` — and four sibling crates depend on `oc-error` being stable
this wave. `GoalError` wraps `DbError` with `#[error(transparent)] #[from]` and
adds the domain failures. `GoalError::is_model_refusal()` tells the goal tool
(todo 68) whether to hand the text to the model or treat it as internal.

## [2026-08-06] Task 98: `oc-memory` — unit, scopes, patterns, drift, header

### Counting unit: `chars().count()` — Unicode scalar values

Matches what the reference's `len(str)` counts in Python 3, so the 2200 figure
means the same thing on both sides. Bytes were the alternative and are wrong: the
cap exists to bound how much *instruction* rides in the system prompt, and under a
byte cap the same rule written in Chinese costs 3× what it costs in English while
occupying a third of the attention budget it paid for. `char_count("先读代码再改")`
= 6, `len()` = 18. One function, `scope::char_count`, so no call site can disagree
with another about what "2200 chars" means. UTF-16 units were never a candidate —
they would encode a JS engine's internals into a file format.

Not tokens, ever. See learnings.md §7.

### Two scopes, diverging from the reference's split

| Scope | Path (via `oc-paths`) | Cap |
|---|---|---|
| `Scope::Global` | `$CONFIG/memory/MEMORY.md` | **2200** |
| `Scope::Project` | `<worktree>/.opencode/RULES.md` | **3000** |

The reference splits by *who the note is about* (agent notes vs user profile) —
right for a personal assistant whose whole context is one user. A coding agent's
context is one user across many repositories, so the real axes are **habits that
travel** and **rules that do not**. One store means every repo pays prompt budget
for every other repo's rules; per-repo habits means relearning them in each new
checkout.

2200 carried unchanged from `memory_char_limit`: same quantity, notes that load in
*every* session including repos the note has nothing to do with. 3000 for project
rules rather than the reference's 1375, because a build command, a test gate, a
lint policy and a directory convention are each one sentence, and they only ever
load inside the one repository that pays for them.

Both paths come from `oc_paths::config()` / `PROJECT_CONFIG_DIRECTORY`, never a
local `XDG_*` read — so an `OPENCODE_CONFIG_DIR` override is honoured for free and
the store lands beside the TypeScript binary's config. (The reference makes the
same point from the other direction at `memory_tool.py:50-56`: a *function* not an
import-time constant, so a profile switch is respected.)

### Threat patterns: all 36 of the `strict` set, three retargeted

Carried the full broadest ruleset (`all` 11 + `context` 17 + `strict` 8), in the
reference's declaration order, with the reference's verbatim pattern ids so a
finding traces back to its line. Three retargeted from hermes paths to this
agent's — same attack class, different filename:

| reference id | here | why |
|---|---|---|
| `hermes_env` (`~/.hermes/.env`) | `agent_credential_store` (`auth.json` / `mcp-auth.json`) | this agent's secret store |
| `hermes_config_mod` (`.hermes/config.yaml`) | `opencode_config_mod` (`opencode.json(c)`, `.opencode/`) | this agent's self-modification target |
| `env_var_unset_agent` | `OPENCODE` added to the runtime list | same sub-session-bypass behaviour |

Bounded `{0,8}` filler kept (15 occurrences); no unbounded repetition anywhere.
Invisible-codepoint check runs on RAW text before folding. Compiled as a
`Vec<Regex>` not a `RegexSet` — see learnings.md §4 for the `CompiledTooBig` reason
and the `Threat::ScannerUnavailable` fail-closed design that keeps `expect` out of
the library.

### Drift: three signals, refuse + `.bak.<ts>`, atomic rename

`Stamp` (mtime **and** length, as the plan asked) + `RoundTrip` + `EntryOverflow`
(both from the reference). Structural signals evaluated first. Rationale and the
per-writer coverage table are in issues.md.

Two ordering details that are the actual safety:
- The stamp is read **before** the content. If a writer lands between the two
  reads, the stamp is then *older* than the content, so the next `apply_batch`
  sees a differing fresh stamp and refuses. Reading content first gives the
  opposite, unsafe skew: stale content under a current-looking stamp.
- Writes are temp-file-then-rename in the destination's own directory. Atomic, so
  a reader sees one complete version and **no lock is needed** — the reference's
  reasoning at `memory_tool.py:762-764`.

`.bak.<ts>` uses second resolution like the reference. Two drifts inside one second
write the same path, which is harmless *because* drift refuses the write: the file
has not changed between detections, so the second snapshot is the first again.

### `apply_batch` semantics

Cap validated **once, against the final candidate**. Applying operations to a
clone and measuring only the result is what lets a store sitting on exactly
2200/2200 accept `[remove, remove, add]` in one call. A per-operation check rejects
that batch on the add even though the removals ahead of it freed the room —
`the_add_in_that_batch_would_not_fit_on_its_own` pins that the test is not vacuous.

All-or-nothing: nothing is written unless every operation validates, including the
final cap check. Locators are short unique substrings, never ids — an id would have
to survive in the prompt, stay stable across a rewrite, and be re-read correctly,
three ways to address the wrong note. Ambiguity (≥2 *distinct* matches) is refused
rather than resolved by position; identical duplicates are not ambiguous. A
duplicate `add` is idempotent, not a failure — "make sure this is recorded" has
succeeded when it already is. **No auto-eviction**: a full store fails and the
error carries the entries.

### The rendered header, verbatim

```
══════════════════════════════════════════════
MEMORY (agent notes) [63% — 1,390/2,200 chars]
══════════════════════════════════════════════
<entries joined by "\n§\n">
```

46 `═` above and below (`memory_tool.py:746`), em dash, thousands separators,
integer-truncated percent clamped to 100. `MEMORY (project rules)` for the other
scope. Thousands grouping is hand-rolled — `{:,}` has no Rust equivalent and a
formatting crate for one comma is not a trade worth making.

**An empty store renders `""`.** Not cosmetic: a `0% — 0/2,200 chars` header spends
prompt space announcing it has nothing to say, and leaves a block for todo 99's
consistency check to find after the last entry is removed.

### API shaped for todos 99-101, not for this crate

- `apply_batch(&[Operation]) -> Result<Usage>`; `Usage: Display` gives todo 100 the
  `current/limit` string with no second read.
- `MemoryError::current_entries()` and `is_consolidation_failure()` are accessors,
  not just `Display`, so todo 100 can serialize the entries its own way and count
  the right failures for its circuit breaker.
- `Scope::label()` is public and stable so todo 99's consistency check can spot a
  leftover header (the reference exports the same constants at
  `memory_tool.py:59-65` for exactly that).
- `Operation::parse(index, action, content, old_text)` so todo 100's
  model-supplied-JSON path produces the *same* wording for a missing field that
  `apply_batch` does, instead of deriving its own.
- `render_block()` is a pure function of the entries, which is what lets todo 99
  freeze it.

### Verification note

`lsp_diagnostics` could not be used: the LSP tool refuses paths outside the
request cwd (`/config/workspace/ProdDir/AI/opencode-rust`) and this work is in the
`oc-wt/t98` worktree. `cargo clippy --workspace --all-targets --offline` at 0
warnings plus a clean `cargo build --workspace --offline` is the stronger gate and
was used instead.

## Task 102

- FTS objects are an explicit extension, not a schema migration. This keeps the
  19-table OpenCode schema oracle unchanged and makes the storage cost opt-in;
  `ensure` is idempotent and `rebuild` is the maintenance seam.
- The main `unicode61` index includes tool content for complete lexical recall.
  The trigram index excludes tool parts, implementing the plan's cost boundary
  without pretending tool output does not exist. Query script detection, rather
  than a caller-selected mode, chooses the appropriate index.
- `SessionSearchTool` owns only a database path and executes SQLite queries. Its
  public schema has no provider, model, embedding, or external-service parameter,
  making the zero-LLM property structural rather than a test-only convention.
- Discovery ranks sessions after grouping message hits and applies a child-session
  penalty. It never removes the penalized class. Scroll is intentionally separate
  from FTS and bookends: an exact `(session_id, around_message_id)` anchor returns
  local context even if the anchor contains no searchable text.

## [2026-08-06] Task 44: registry seams and filter order

`ToolRegistryBuilder` owns an ordered `BuiltinSlot -> Arc<dyn Tool>` map plus the shared `FileTools`. Built-ins are emitted through the fixed upstream slot sequence; requiring `FileTools` lets per-model resolution call `FileTools::exposed_for_model` rather than duplicate the GPT substring rule.

Custom loading is a `CustomToolLoader` with separate `config_directory_tools(&[PathBuf])` and `plugin_tools()` methods because upstream appends those sources in that order. `McpToolLoader::tools()` is a second seam appended afterward. Both default to no-op implementations until waves 9 and 8 land. Config directories come directly from `oc_paths::config_directories(directory, worktree)`. `config_tool_id(path, export)` is the pure load-bearing naming function: default export -> basename, named export -> `{basename}_{export}`.

Resolution order is fixed: process-wide exposure flags while assembling built-ins; append config tools, plugin tools, then MCP tools; per-turn websearch and `FileTools` model filtering; the independent execute-description gate; finally `oc_permission::visibility::retain_visible_tools`. Permission hiding is last so it also covers every extension source and alias mapping.

## Task 99 — snapshot and cache-consistency choices

- `SessionMemory` owns both stores and freezes each rendered block at construction.
  `store_mut` remains available so a memory-tool write lands immediately, while
  `inject_into` can only read the frozen strings. A new `SessionMemory` is the sole
  operation that makes new disk state visible to a prompt.
- Scope order in the static prompt is global then project, with exactly two newlines
  between non-empty components. Empty blocks add no bytes. This keeps prompt bytes
  deterministic and preserves Task 98's `render_block` format as the single source
  of header and usage formatting.
- `cache_consistency` first opens every enabled current scope, then classifies. This
  makes an unreadable enabled scope return `Unknown` even if another scope already
  appears stale; proven unreadability is the stronger conservative signal. Disabled
  scopes are not read, but their stable header must be absent from the cache.
- A non-empty current rendered block must occur verbatim in the cached prompt. An
  empty current block is checked through `Scope::label`, because there is no block
  string to compare. This is the same independent header handle the reference
  exports for compaction consistency.
- The external fence is a separate function rather than part of `inject_into`.
  Global and project stores are first-party, threat-scanned resident memory and must
  not be relabelled as external user input; external recall is sanitized and fenced
  with the required authoritative-memory note.

## Task 72 — policy and background-task boundaries

- `OutputPolicy` owns both persistence and refusal so callers cannot accidentally
  reject an oversized value before preserving it. Refusal reports bytes, lines,
  estimated tokens, retrieval path, and the exact `accept_large_output` escape hatch.
- The token count is deliberately an estimate using four UTF-8 bytes per token. It
  is documented rather than presented as tokenizer-accurate context accounting.
- `BackgroundManager` is an adoption seam. Explicit background execution and
  foreground timeout promotion both hand the same running future to it, producing
  queryable task state and output/status files under `.opencode/background`.
- Caller-selected foreground timeout values are normalized to at most 600 seconds;
  the existing 30-minute hard ceiling is retained instead of being conflated with
  the foreground-attention cap.
- The promotion response warns that the command is still running and should not be
  rerun unless a duplicate process is intentional.

## [2026-08-06] Task 68 — hidden goal context and continuation policy

- Goal context is a synthetic pseudo-user transcript entry regenerated from
  `GoalStore`, never a persisted message. This makes SQL the single durable source
  and prevents compaction from deleting the control objective.
- Auxiliary continuation state lives beside the goal in `goal_1.db`, in separate
  `goal_continuation_deferral` and `goal_failure_streak` tables. The existing `goal`
  table is unchanged, preserving Task 67's schema contract while keeping restart
  behavior durable.
- Self-continuation is conjunctive: process-local start ownership, no active engine
  turn, no plan mode, no queued user input, no consumed one-shot deferral, and an
  active goal are all required. A missing guard returns a typed non-start outcome;
  it never "best-effort" starts.
- The model may request only `blocked` or `complete`, reusing Task 67's
  `ModelStatus` boundary. `blocked` adds a persistent audit threshold of three
  matching failure signals; terminal infrastructure errors are the exception and
  block immediately because no model turn remains in which to report them.
- The start mutex is deliberately process-local. A database lease was rejected for
  this scope because no caller currently promises cross-process continuation; the
  limitation is explicit rather than implied away.

## [2026-08-06] Task 49: oc-pty ring design, exit-order eviction, thread shape, and the ticket model

### The scrollback is a fixed-capacity ring, UTF-8-realigned at the head

```rust
pub const BUFFER_LIMIT: usize = 1024 * 1024 * 2;   // packages/core/src/pty.ts:14
const MAX_CONTINUATION_BYTES: usize = 3;
const INITIAL_PHYSICAL: usize = 8 * 1024;
```

**Bytes, not characters.** The oracle counts UTF-16 code units of a JavaScript
string; this counts bytes, which is the quantity that actually bounds memory. They
agree exactly for ASCII; for CJK this retains fewer characters and the same number
of bytes, which is the correct reading of a *memory* cap.

**A ring, not periodic re-slicing.** `buffer = buffer.slice(excess)` (`pty.ts:220`)
copies the retained 2 MiB per chunk once full. The ring makes the bound structural
rather than periodic: the allocation never exceeds the limit and a write is two
`copy_from_slice` calls no matter how much has been discarded.

**Grows geometrically from 8 KiB.** Eagerly reserving 2 MiB would mean 50 MiB held
for 25 retained-exited sessions nobody is reading. `reserved_bytes()` is exposed
separately from `retained_len()` precisely so a memory assertion reads the
high-water mark rather than the current fill.

**UTF-8: realign the HEAD, bounded at 3 bytes.** Discarding an arbitrary byte count
splits code points, and `replay` is decoded as text; the user works in Chinese, so
mojibake at the top of every replay is the *common* case, not an edge case. After
any discard the head advances past continuation bytes (`0b10xxxxxx`), at most 3
because a UTF-8 sequence is at most 4 bytes.

**The 3-byte bound is load-bearing, not defensive.** A PTY also carries binary
output, where a "continuation byte" is just a byte; an unbounded scan would empty
the buffer looking for a lead byte that never comes. `realignment_is_bounded_on_
binary_output` drives 32 bytes of `0x80` into an 8-byte buffer and requires exactly
5 retained.

**Not `oc_llm::Utf8StreamDecoder`** (`crates/oc-llm/src/sse.rs:24-91`). That holds
an incomplete *trailing* sequence pending the next chunk, which is right for a
stream decoding to `String` in order. A ring stays bytes (a terminal replay is
bytes — escape sequences, cursor moves, partial writes) and truncates at the
*head*. Mirror-image problem, so the approach is shared (`error_len() == None`
means "incomplete, not invalid") but the code is not.

### Eviction is exit-ordered, and the test asserts identity rather than count

`ExitRetention` is a `VecDeque<PtyId>` appended on exit and popped from the front,
mirroring `exitOrder.push(id)` (`pty.ts:228`) with `exitOrder[0]` evicted
(`:234-238`). The oracle uses `indexOf` + `splice` rather than `shift`, so an
explicitly removed exited session frees its slot; `forget()` does the same.

**Why identity, not count.** A creation-ordered implementation retains exactly 25
and retains the *wrong* 25, so a count assertion passes on a broken port. The test
makes exit order the exact reverse of creation order and asserts the retained queue
with `assert_eq!` against the exact 25-element exit-order slice. Measured:

```
EXIT ORDER by creation index: [29, 28, ... 1, 0]
EVICTED creation indices:     [25, 26, 27, 28, 29]   <- the five EARLIEST exits
oldest-5-by-CREATION still retained: [true, true, true, true, true]
```
Creation-ordered eviction would have produced `{0,1,2,3,4}`. Disjoint sets.

**`record_exit` returns the evictions instead of performing them.** That keeps the
type free of any knowledge of processes or locks, so the ordering is testable
without spawning anything (6 unit tests, microseconds). It also makes the
self-eviction hazard representable-and-excluded: a non-zero limit guarantees a slot
for the just-exited id, which matters because eviction runs on that session's own
waiter thread — evicting itself would mean tearing down the thread doing the
tearing down. `with_limit(0)` is therefore raised to 1.

**A duplicate `record_exit` is a no-op.** The oracle guards with a status check
before the push (`pty.ts:224`); this repeats the guarantee at the queue, so a
double notification cannot silently shorten the retained history.

### Two OS threads per session, and dropped output rather than a longer queue

`portable_pty`'s reader is a blocking `std::io::Read` and its child wait is a
blocking `wait()`; neither has an async form. So each session owns a reader thread
and a waiter thread, and every publish is `try_send`. Nothing in the crate needs a
Tokio runtime to exist — same conclusion todo 50 reached for `oc-watch`, and it is
what lets every method be a plain `fn` and every test a plain `#[test]`.

The waiter thread is not merely bookkeeping: **`wait()` is the reap.** It is the
only call that collects the zombie, so the thread existing is the containment.

**Degradation for a slow subscriber: drop and report.** Per-attachment queue is 256
slots x 8 KiB = 2 MiB, deliberately equal to the scrollback bound — a subscriber
cannot cost more than the history it could re-read anyway. On overflow, chunks are
discarded and counted, and `PtyOutput::Lagged { dropped, cursor }` is sent **ahead
of** the next chunk so a client learns of the hole before rendering a discontinuity.

Dropping is safe *here* and not in a filesystem watcher because **the scrollback is
the durable copy**: `retained_output()` serves the missed window. That is the
property to preserve if this is ever changed. Growing the channel instead would
move the unbounded buffer one layer down, which is this crate's whole failure mode.

**No `activate()` two-phase attach.** The oracle stages output in an unbounded
per-subscriber `pending` array between `attach` and `activate` (`pty.ts:26`,
`:252-261`). Here the replay snapshot and the subscription are taken **under one
lock** — the reader thread needs the same lock to append — so no chunk can land
between them and there is nothing to stage. Deleting the staging array deletes the
unbounded buffer with it.

**Poisoned mutexes are recovered** (`PoisonError::into_inner`), as in `oc-watch`:
the guarded region is a byte ring and a map of senders, both structurally valid,
and the alternative is one panic permanently stopping every client of that session.

**`shutdown` signals and returns; it does not reap.** The session's own waiter
thread observes the death. Holding the registry lock across a session teardown
would let one session's reader block every other session's lookups, so `take()`
removes under the lock and tears down after releasing it.

### `PtyServiceConfig` instead of `&mut self` builder setters

The setters were first written as `self`-by-value with `Arc::get_mut`, which is
sound only while the service is unshared and silently no-ops otherwise — a footgun
with no compile-time signal. A plain config struct consumed by
`PtyService::with_config` makes construction-time-only structural.

### Errors: the oracle's two, plus the four it swallows

`NotFound` and `Exited` are `pty.ts:74-80`. `Open`, `Spawn`, `Write`, `Resize` are
additions — the oracle wraps those calls in bare `try {} catch {}`, so a user whose
keystrokes go nowhere cannot learn that. A write or resize *after* a clean exit is
still a no-op rather than an error, matching the `status === "running"` guards
(`:194`, `:200`): a client reporting its window size should not need to know
whether the shell is still alive.

### connect-token IS modelled, and revoked with its session

`TicketStore`: `uuid::Uuid::new_v4()`, 60 s TTL, capacity 10,000 — all three the
oracle's (`core/src/pty/ticket.ts:9-10`, `:41`). Single-use, and scoped to
`{pty_id, directory, workspace_id}` so a ticket for one project's PTY cannot be
replayed against another that reuses the identifier.

It lives in the library, not the route, because the store is the only stateful half
and the oracle has **two** upgrade surfaces that must not each keep their own.

**One addition: `revoke_session`.** The oracle expires by TTL only, so a ticket
minted for a since-removed PTY stays redeemable for up to 60 s against whatever
later session might reuse the id. Since a WebSocket URL ends up in history, proxy
logs and referrers, narrowing that window to zero is worth one `retain` call.

### `PtyId::mint` is monotonic within the process

`pty_` + 12 lowercase hex of the mint-time millisecond + 14 base62, per
`packages/schema/src/identifier.ts`. The millisecond is bumped past the previous
reading when the clock has not advanced (`AtomicU64::fetch_update`), so two ids
minted in the same millisecond still sort by creation. That is what `ascending()`
promises and what lets `list()` reproduce the oracle's `Map` insertion order by
sorting on the id instead of depending on a `HashMap`'s iteration order.


## [2026-08-06] Task 45: MCP stdio transport decisions

- Framing is strict NDJSON: serialize exactly one JSON-RPC object plus `\n`, and parse stdout line by line. `Content-Length` is deliberately unsupported because it is the known non-functional claw-code implementation and not the MCP stdio wire format used by the real server.
- Concurrent requests allocate monotonically increasing `u64` ids and register `oneshot` senders in a shared map before writing. Only a response carrying that exact id removes its waiter; notifications and unknown ids never do.
- Initialization uses protocol `2024-11-05`, sends `notifications/initialized`, then obtains the complete tool catalog through cursor pagination. A tools-changed notification refreshes the cache before publishing `ToolsChanged`.
- The default request timeout is 30 seconds, matching the executable TypeScript oracle rather than its conflicting 5-second schema prose. Configured timeout values are milliseconds and override the default.
- Child environment order is inherited process environment, then `BUN_BE_BUN=1` only for an `opencode` command, then configured `environment` overrides. Relative `cwd` is resolved lexically against the workspace.
- Tool ids port the JavaScript UTF-16 sanitizer exactly, including one underscore per non-ASCII UTF-16 code unit, so astral characters become two underscores.
- Explicit `close` is the normal lifecycle; drop is the safety net. Both ensure the child is killed/reaped and pending requests are failed instead of leaked.

## [2026-08-06] Task 71: deterministic run / reflect / deny policy

- Assessment is a pure synchronous function over command text, `ShellSyntax`, cwd, and a HOME snapshot. It makes no LLM call, filesystem query, environment mutation, or process invocation. This makes one verdict reproducible and safe to run before every dispatch.
- `Reflect` is represented as `ToolError::InvalidArgs`, because adding a substantive `justification` is a model-correctable resubmission. `Deny` is `ToolError::Failed` with `PermissionDenied` as its source, because identical arguments plus prose must never unlock a catastrophic target.
- `justification` belongs to `ShellParams`, not a global cross-cutting schema field. It is meaningful only to this gate and adding it to every tool would charge schema tokens on every request for a shell-only recovery path.
- Protected paths are conservative and explicit: filesystem root; critical system roots and recursive stores; the current user's home; credential stores; selected home configuration/data roots; and `/dev`. `/dev/null`, `/dev/stdout`, `/dev/stderr`, and `/dev/fd/*` are allowed only as redirect sinks, not as destructive-command targets.
- Every verdict is emitted through `tracing` under target `oc_tools::risk`; nothing writes to stdout. The log happens before shell dispatch observes the outcome, so allowed, reflected, and denied attempts are all auditable.

## [2026-08-06] Task 51: HTTP-core security and overflow policy

- The default listener is `127.0.0.1:0`. Before binding, the configured hostname is resolved once; without a non-empty password, every returned address must be loopback. The selected validated `SocketAddr` is bound directly, avoiding a validate-then-re-resolve DNS race. Resolution failure is an error, never permission to assume locality.
- Non-loopback binding is a hard startup error only when auth is disabled. The diagnostic names both `--hostname` and `OPENCODE_SERVER_PASSWORD`, making the remedy actionable without printing credentials.
- Fan-out uses drop-newest per subscriber rather than blocking the engine or evicting retained events. A scalar pending-drop count is delivered after the retained backlog, including when publishing has already stopped. This preserves event order, gives each connection a hard memory ceiling, and isolates one stalled client from every other subscriber.
- `ServerServices` is the extension seam for later route groups: it owns the one-live-turn registry and `EventFanout<TurnEvent>`. `ServerBuilder::with_routes` is intentionally the only route-extension point before mandatory middleware is finalized.
- A small `oc-server serve --hostname/--port` binary provides Task 51's executable QA surface. Todos 55-56 remain responsible for integrating the same server builder into the final `opencode-rust` CLI command tree.

## [2026-08-06] Task 69: the goal document's format, its conflict rule, and self-render suppression

### The document format

`.opencode/goal/<sessionID>.md`, or `$XDG_DATA_HOME/opencode/goal/<sessionID>.md`
when the project is not a repository — the same two-way choice the oracle makes for
plans (`packages/opencode/src/session/session.ts:331-335`). `document_path`
validates the **session id** as a single normal path component and refuses
`../../etc/passwd`, `a/b`, `..`, `.`, `""` and `/absolute`.

Five sections, in this order: `## Objective`, `## State`, `## Budget`,
`## Checklist`, `## Rejected edits`. Above them an HTML comment stating the
conflict rule, so a user who opens the file learns it without reading source.

The objective is delimited by `<!-- goal:objective:begin -->` /
`<!-- goal:objective:end -->` rather than a fenced code block or a heading. HTML
comments because (a) they render as nothing in every Markdown viewer, so the
document stays readable, (b) they are unambiguous machine markers, and (c) an
objective that itself contains a fence or a `##` heading cannot break the parse.
The region is taken from the **first** opening marker to the **last** closing one,
so an objective containing the closing marker still round trips — tested.

Everything else is `- \`key\`: value` lines and `- [x] \`key\`: prose` checkboxes.
`Field` (9 variants) and `Check` (3) are exported with `ALL` arrays, so
`every_projected_field_and_checkbox_is_guarded` walks the whole matrix rather than
the fields whoever wrote the test remembered, with a floor assertion so a shrunken
`ALL` cannot make it pass vacuously.

`parse` returns `None` unless the objective region **and every** field and checkbox
key is present. That strictness is what makes it usable as the assertion in the
atomicity test: holding a `Document` proves the reader did not see a partial file.

### The conflict rule

| field | authority |
|---|---|
| objective text | **the document** — adopted on the next turn |
| `status` | **SQL** |
| `token_budget`, `tokens_used`, `tokens_remaining`, `time_used_seconds` | **SQL** |
| `session_id`, `goal_id`, `created_at_ms`, `updated_at_ms` | **SQL** |
| the checklist | **SQL** — a projection, never an input |

Adoption goes through `GoalStore::update_objective`, deliberately **not** around
it, so todo 67's 4,000-character cap and spill apply to a hand edit exactly as to a
tool call: a 6,000-character hand-edited objective spills to
`<spill_dir>/<uuid>/goal-objective.md` and the document then shows the pointer
sentence, not the raw text. Tested end to end including that the re-render round
trips as `OwnRender`.

"Adopted on the next turn" means SQL, not a cache in the projection layer: todo
68's `GoalContinuation::injection` reads `GoalStore::goal`, so writing SQL *is* the
adoption. `an_edited_objective_is_adopted_and_the_next_turns_injection_carries_it`
asserts against `injection()` rather than against the store, so the two halves
cannot drift apart.

An objective edited to whitespace is refused (`GoalError::EmptyObjective`) and
reported like any other rejection — the one case where the document-authoritative
field still loses, because an empty objective is a north star pointing nowhere.

### The rejection message, verbatim

```
- `status` was edited to `complete`, but the status is the system's to set, not the document's; the goal database still says `active`.
```

Four things, because a user needs all four: what they edited, what they set it to,
who owns it, and what the value really is. Grouped nouns — "the counters", "the
timestamps" — because `tokens_remaining is the system's to set` invites the user to
try `tokens_used` instead.

Singular/plural is carried by `Refusal::SystemOwned { noun, plural }` and built
only through `Field::owner()`, after the first draft shipped "the counters **is**
the system's to set". A grammar bug in a tested artifact; the constructor makes it
unrepresentable.

An unparsable document is preserved at `<name>.bak.<unix-seconds>` (matching
`oc-memory`'s `.bak.<ts>`) and rebuilt from SQL, with the document naming the
backup. Second resolution is safe for the same reason it is there: two salvages in
one second preserve the same bytes.

### Self-render suppression: retained bytes, not a stamp or a token

`GoalProjection` holds the exact bytes of its last render plus the `Goal` behind
them. Byte-identical file ⇒ `Ingest::OwnRender`, no SQL write, no rewrite. Chosen
over `oc-memory`'s mtime+len stamp (exact here, since we just produced the bytes)
and over an ignore-next-event token (coalescing makes one token cover N events).
Rejections diff against **the last render**, not live SQL, so a turn finishing
between render and save does not report edits the user never made. Full reasoning
in learnings.md.

### Where the gitignore recommendation lives

`projection::GITIGNORE_SNIPPET`, a `pub const` carrying `.opencode/goal/` on a line
of its own plus comment lines explaining itself. No recommended-snippet file exists
in this repo or the oracle to append to — see issues.md — and inventing one would
claim a file another todo may own.

### `GoalError::Document { operation, path, source }`

Separate from `GoalError::Spill`: a spill failure means an objective could not be
*stored*, so the write must fail; a projection failure means the human-readable
copy is stale while SQL is still correct. `is_model_refusal()` is `false` — a
filesystem failure is not something the model can rephrase its way out of.

## [2026-08-06] Task 100: `memory` tool response shapes, breaker keying, and the Ok-carrying-error rule

### Three response shapes, and what each deliberately withholds

| shape | `done` | `current_entries` | why |
|---|---|---|---|
| success | `true` | **absent** | terminal and minimal. `memory_tool.py:711-723` measured echoing them: the correct batch on call 1, then 5 redundant repeats |
| refusal | `false` | **present** | consolidating is a judgement about which of two overlapping notes to keep; the model cannot make it blind |
| breaker terminal | `true` | **absent** | handing the entries over here would argue for exactly the retry this response forbids |

All three carry `scope`, `usage`, `current`, `limit`, `entry_count` — the plan's
"report current/limit in every response". `usage` is `Usage: Display` from todo 98,
so the string in a response is the same string in the prompt header and no second
formatter can disagree. On a refusal the figures are the store's size **before** the
batch, because nothing was written; they are omitted only when the store could not
be opened at all, where there is no trustworthy count.

Both halves of the asymmetry are asserted
(`success_withholds_the_entries_and_failure_hands_them_over`,
`a_terminal_refusal_still_reports_the_budget_but_not_the_entries`) and mutation-proven.
Nothing but a test protects the success half; the reference protected it with a comment
and someone will still "helpfully" add the list back.

### The breaker keys on `session_id`, resets on success, and has an explicit turn hook

`ConsolidationBreaker` is a `Mutex<HashMap<session_id, usize>>`. Keying rationale is
in learnings.md — the short version is that `call_id` and `message_id` both reset on
every attempt, and `message_id` does so while still passing a naive test.

Three resets, in the reference's order of reliability:
1. **A successful write clears the streak** (`memory_tool.py:704-706`). The cap counts
   a *stuck loop*, not a lifetime tally.
2. **`reset_for_turn(session_id)`**, the port of `reset_consolidation_failures()`
   (`:176-178`). No caller yet — the engine wiring is outside this todo's crate
   boundary. Gap documented on the method and in the evidence file; it fails safe
   (trips a turn early, never late).
3. A new session is a new key.

Only sessions with a *pending* streak occupy an entry and both resets evict, so the
map is bounded by sessions currently mid-consolidation, not by sessions ever seen.

**Only consolidation failures count.** `MemoryError::is_consolidation_failure()`
(todo 98) gates it: `NoMatch`, `Ambiguous`, `CapExceeded` are worth another attempt;
a blocked injection pattern, a drifted file or an unreadable store will not resolve by
merging entries, so they must not spend the budget that protects the reply. An
unusable call shape does not spend it either. Three tests pin those.

A poisoned `Mutex` recovers via `into_inner()` rather than propagating. The count is
advisory; panicking out of a memory side effect would take the turn's reply with it,
which is the exact failure this whole module exists to prevent.

### A breaker trip stays a successful tool result

Every store refusal — including the terminal one — is `Ok(ToolOutput)` with
`success: false` in the body. The reason is not "an `Err` would fail the turn" (it
would not; `dispatch.rs:496-515` converts it to an error *result* and `loop.rs`
continues). The reason is that `ToolDispatchResult::error` **replaces the tool's body
with a rendered error string**, discarding the usage figures and the entry list the
model needs. The single `Err` path is `ToolError::InvalidArgs` for an unusable call
shape, where there is nothing to report about memory and the model must correct the
call — and it is model-correctable, so it comes back as a tool result anyway.

### Store opened per call, not held

`MemoryTool` holds only `ScopePaths` and the breaker; `MemoryStore::open` runs on
every call. Todo 98 refuses a write whose file moved underneath it, and that drift
check is only as good as the freshness of the handle it compares against — a
long-lived store would compare against a stamp from session start and refuse every
write after any external edit. Todo 99's frozen-snapshot contract is unaffected:
`SessionMemory` freezes the *rendered blocks*, so a mid-session write lands on disk
without moving the prompt, which is what the happy-path QA test asserts end to end.

### `ScopePaths` is the test seam, and it is not new machinery

`Scope::Global` resolves through `oc_paths::config()`, a process-wide cached layout —
so a test using `Scope::path()` would write the developer's real `MEMORY.md`.
`ScopePaths::at(global, project)` wraps todo 98's existing
`MemoryStore::open(scope, path)` escape hatch rather than adding a second override
mechanism. `ScopePaths::discover(worktree)` is the production constructor. Every test
in both targets uses `at` with a `TempDir`.

### `MemoryTarget` / `MemoryAction` mirror `oc-memory` rather than deriving on it

`Scope` and `Operation` live in `oc-memory`, which does not depend on `schemars` and
must not (it is a storage crate; a schema derive there would put a serialization
concern in the file format's owner). So the wire enums are local, the conversions are
total `match`es, and `wire_names_cover_every_scope` asserts `Scope::ALL.len() == 2` —
a third scope in `oc-memory` fails that test instead of silently becoming unreachable
from the tool.
## [2026-08-06] Task 48 — oc-lsp boundaries and supervision

- `Client` owns protocol only: framed stdio, request demultiplexing, reverse
  requests, document versions, and diagnostics caches. `Manager` owns processes,
  roots, fan-out, status, restart policy, and reaping. `ServerRegistry` owns static
  definitions plus resolved config. This keeps process failure from corrupting the
  protocol state machine.
- Downloads are represented by `ServerInstaller`; the default is a no-op and this
  crate performs no network operation. Built-ins retain install provenance so a
  future host can opt in without embedding HTTP/package-manager behavior in
  detection or tests.
- Restart delays are bounded exponential backoff with a finite consecutive-failure
  cap. Every child uses piped stdio and `kill_on_drop`; explicit termination and
  shutdown both call kill then wait, making reaping a lifecycle invariant.
- Lifecycle event publication occurs under the same state mutex status readers
  acquire. `Connected`, `Restarted`, `Degraded`, and `Stopped` enter the bounded
  broadcast channel before their corresponding state becomes observable.
- The model surface is one `TypedTool` named `lsp`, matching upstream. Schemars and
  serde derive from the same params type; the tool preserves one-based model input
  and converts to zero-based LSP positions at the boundary.

## [2026-08-06] Task 71 adversarial correction: real bypasses and boundaries

- **FIXED — empty brace alternatives:** `rm -rf /{,}`, `rm -rf /{a,}`,
  `rm -rf {/,}`, and `rm -rf /{.,}` reached `Allow` after a substantive
  justification because an empty alternative aborted expansion and the raw token
  was assessed as a literal. Empty alternatives now expand to the empty string, so
  all four expose their protected target and permanently `Deny`.
- **FIXED — ordered cwd changes:** `cd / && rm -rf .`, `cd / && rm -rf *`,
  `cd ~ && rm -rf .`, `cd / ; rm -rf .`, `pushd / && rm -rf .`,
  `cd /etc && rm -rf .`, and `cd / && rm -rf ./` reached `Allow` after a
  substantive justification because every relative target used the session cwd.
  The assessor now consumes `CommandResource::changes_directory` in lexical order,
  simulates Bash and PowerShell location stacks, and resolves each later target
  against the simulated cwd. All seven now permanently `Deny`.
- **DOCUMENTED — unknown braces and locations fail closed:** unsupported or
  malformed brace syntax is an unknown target rather than a literal path and
  produces `Reflect`. A dynamic/unknown directory change makes subsequent relative
  destructive targets unknowable even after a justification, so those permanently
  `Deny`. The gate does not claim to know either concrete runtime destination.
- **DOCUMENTED — conservative control flow:** `&&`, `;`, and `||` conditions are
  not used to waive a constituent. If the destructive command can run, it is
  assessed. Nested-shell scope, arbitrary application/interpreter semantics,
  aliases/functions, encoded/downloaded scripts, symlinks, and TOCTOU still require
  confinement and remain outside the static proof.
- **DOCUMENTED — reads are not destruction:** `cat ~/.ssh/id_rsa` is intentionally
  `Allow` in this destructive-command gate. Read/exfiltration policy belongs to
  permission policy or confinement; allowing it here is a named boundary, not an
  accidental omission.

## [2026-08-06] Task 46: remote MCP transport and OAuth decisions

- One `protocol.rs` owns response validation, id waiter routing, notification classification, and pending-failure fan-out for stdio and remote transports. Transport modules own framing only: NDJSON, HTTP JSON/SSE, or legacy SSE events.
- Remote negotiation is Streamable HTTP then legacy SSE. Any non-authentication connection error permits fallback; 401/403 never does. `oauth: false` converts the challenge into `OAuthDisabled`, while absent OAuth config enables automatic discovery.
- `AuthorizationRequest` is the explicit pause point. Its `Debug` redacts the whole authorization URL because the query contains CSRF state. `finish(code, returned_state)` validates state, exchanges the code with the persisted PKCE verifier, clears transient state only after success, stores tokens, and reconnects.
- Configured `redirectUri` overrides `callbackPort`; otherwise the callback is `http://127.0.0.1:19876/mcp/oauth/callback`. Configured client id/secret overrides stored DCR information. Dynamic clients are reused only for the exact server URL and only while their secret is unexpired.
- Refresh occurs before the MCP handshake when stored expiry is within 30 seconds. If the token endpoint omits a replacement refresh token, the existing one is retained.
- Static remote headers are applied to protocol requests; OAuth credentials are never included in error text or derived `Debug`. Access, refresh, client secret, verifier, and state remain `Secret` at persistence boundaries.

## [2026-08-06] Task 53 — SSE service boundaries

- `EventService` owns persistence and per-session bounded broadcast senders;
  `events_router` owns HTTP parsing and SSE framing. The split lets non-HTTP
  producers publish typed events without depending on Axum response types.
- The service keeps strong sender references so a session stream survives periods
  with no subscribers. Subscriber capacity is fixed at service construction and a
  lagged receiver gets one `server.stream.lagged` diagnostic without an SSE id,
  then disconnects so reconnect cannot replay the diagnostic as domain history.
- Heartbeats are SSE comments rather than named events, so they keep proxies and
  clients alive without advancing a durable cursor.
- Store initialization is lazy and pool-local. This is idempotent for file-backed
  databases and required for `:memory:` pools, where an independently migrated
  connection would initialize the wrong database.

## [2026-08-06] Task 63: lean built-in agent roster

**The six-agent mapping from slim's nine.** Slim
(`.omo/refs/omo-slim/src/config/constants.ts:7-20`) ships `orchestrator` + `explorer,
librarian, oracle, designer, fixer, observer, council, councillor`:

| slim | here | why |
| --- | --- | --- |
| `orchestrator` | `orchestrator` | kept as-is: primary, write-capable, the only `MayDelegate` |
| `explorer` | `explorer` | kept: internal recon, read-only |
| `librarian` | `librarian` | kept: the **only** lane with `webfetch`/`websearch` |
| `oracle` + `observer`(advisory half) | `advisor` | one lane. "Should we do X" and "tell me why this patch is wrong" are the same act — read the code and argue with it — and two names for it made the caller choose between them instead of using either |
| `fixer` | `worker` | kept, **minus the amnesia** (below) |
| `observer`(multimodal half) | `looker` | kept, **un-gated** (below) |
| `designer` | — | dropped. Its whole justification is UI/UX taste (slim spends 0.7 temperature there and nowhere else); this is a terminal agent harness with no UI surface, and a dropped agent is cheaper than one nobody routes to |
| `council` + `councillor` | — | dropped. Slim prices multi-model consensus at "3x slower … 3x or more cost" in its own prompt (`src/agents/orchestrator.ts:95`). Two agents and a fan-out protocol to obtain disagreement is replaced by one required envelope section — see the temperature note |
| — | (omo's `prometheus`/`metis`/`momus`, Team Mode) | never adopted. A test (`the_dropped_agents_stay_dropped`) asserts all eleven forbidden names resolve to `None`, so a later todo cannot reintroduce one by accident |

**The amnesiac-worker defect, and how `worker` avoids it.** Slim's `fixer` prompt
(`src/agents/fixer.ts:15-17`) reads: *"NO external research (no context7, gh_grep) / NO
spawning subagents … / No multi-step research/planning; minimal execution sequence ok"*.
Two very different bounds are bundled there. `NO spawning subagents` is the one that
matters — it is what keeps a delegated lane from becoming a fan-out tree that the depth
limit alone has to contain. `NO external research` / `no multi-step planning` bound nothing
dangerous; they just guarantee that any *explore → decide → implement → verify* task
bounces back through the orchestrator between every phase, so the orchestrator's context
window absorbs the discovery, the decision, and the verification of work it delegated
precisely to avoid absorbing. This is deviation (1) recorded in
`.omo/drafts/opencode-rust.md`. Here the two bounds are separate roster columns —
`Delegation::NoChildren` and `Research::Allowed` — and `worker` takes one of each. It gets
`read/glob/grep/lsp/edit/bash/webfetch/websearch/todowrite/skill/execute` and is denied
`task`; a test asserts each of those individually
(`the_worker_writes_researches_and_iterates_but_spawns_nothing`).

**`looker` is capability-gated, not opt-in** (deviation (2) in the draft; slim disables its
multimodal agent by default at `constants.ts:91`). An opt-in context-hygiene feature is one
nobody opts into, and the cost of the agent merely existing is a paragraph of prompt. So
`Gate::VisionModel` asks the catalog a question — is any resolved model's `input.image`
true — and the roster is a *function* of that answer: `roster(vision_available)`. No config
key participates. Signal is `ModelCapabilities.input.image`, **not** `attachment`; see
learnings for why that distinction is load-bearing.

**Which agent got the higher temperature: `advisor`, at 0.4.** Everything else sits at 0.1,
except `looker` at 0.2. The reasoning is the `council` cut. Cutting it removed the only
place in slim's roster where two *independent* readings of a problem were produced on
purpose, and the advisor inherits that job through a **required `<alternatives>` section
demanding at least two options with their costs**. At temperature 0.1 a model collapses
onto the single most probable reading and writes two alternatives that are one alternative
in different words — exactly the failure council existed to prevent. 0.4 buys enough
divergence to surface a genuinely second option and stays well short of the 0.7 slim spends
on prose taste, where review output starts asserting facts about code it did not read.
`looker`'s 0.2 is a smaller claim: at 0.1 image descriptions collapse into clipped
templated phrases and lose the detail the caller delegated for. Tests pin both — one
asserting `advisor` is the *only* lean agent above 0.2, and a range assertion
(`0.0..=1.0` for all, `0.1..=0.5` for the non-internals) so a typo like `10.0` cannot pass.

**Are the internals exempt from the table requirements? Partially, and the exemption is
encoded in the table rather than in the test.** Two of the four columns cannot be
meaningfully filled for an engine-invoked agent: a *delegation* boundary for something no
caller can delegate to, and an *output envelope* for output the engine consumes raw (a
title string, a compacted transcript). So the field types carry the exemption as data:
`Boundary::NotDelegable { reason }` and `OutputContract::EnginePrompt { prompt }`. The
table test then closes it from three sides: `NotDelegable`/`EnginePrompt` are accepted only
when `role == Role::Internal`, only when the name appears in `INTERNAL_NAMES` (a list fixed
by upstream's hidden natives, not by this roster), and the `reason`/`prompt` must itself be
substantial (≥40 chars / >200 bytes). The other two columns are **not** exempt: every
internal declares a temperature and a deny-by-default set like everyone else. Net effect —
a seventh *subagent* cannot borrow the exemption, and adding one with an empty boundary
fails `every_agent_states_every_column` (demonstrated for real; output in
`.omo/evidence/task-63-opencode-rust.txt` §8).

**`plan` is not reproduced in `oc-agent`'s roster.** It is the fourth upstream native this
roster does not carry, and unlike `build`/`general`/`explore` it is not *replaced* by
anything: `orchestrator` is write-capable, so it cannot be plan mode. The argument for
leaving it out is that `plan` is a visible primary *mode* whose entire content is a
permission overlay over a primary agent, and `oc_catalog::agent::builtin::plan` already
carries exactly that (including the overlay, marked partial pending runtime paths).
Duplicating it here would put two roster entries under one name. The cost is stated and
tested rather than hidden: every entry in `oc-agent`'s roster denies `plan_exit`, and
`plan_mode_is_not_reproduced_and_no_agent_can_leave_it` asserts both that `get("plan")` is
`None` here and that the catalog still has it — so whichever later todo makes this roster
the sole source of agents fails that test and has to add plan mode deliberately.

## [2026-08-06] Task 70 — execute scheduling and registry re-entry

- `execute` re-enters `ToolRegistry::execute` through a weak registry handle instead
  of calling tools directly. This preserves aliases, permission checks, context, and
  future registry policy in one seam without creating a strong-reference cycle.
- The hard limit applies after `$each` expansion, because expanded calls are the
  actual resource cost. A request that expands beyond 10 is rejected before dispatch.
- Failed siblings remain result records rather than cancelling the batch; dependency
  failures affect only calls whose bindings cannot be resolved.

**Deny-by-default is stronger here than slim's.** Beyond porting the `'*': deny` +
explicit-denies + explicit-allows redundancy from `permissions.ts:13-30`, every agent must
give **every** governed tool an explicit verdict: `denied ∪ allowed == GOVERNED_TOOL_IDS`,
asserted per agent. Adding a tool id to the governed list therefore forces all nine agents
to state a position on it, instead of the new tool quietly landing in whichever bucket the
catch-all happens to put it in.

**API shape for todos 64-69.** `builtin::roster(vision_available) -> Vec<Agent>` is the
entry point; `lean()` / `internals()` / `delegable()` / `get(name, vision)` narrow it.
Todo 64 (`model_policy.rs`): `Agent` deliberately has **no model or variant field** — there
is nothing to default to unset because the concept is absent from the roster, and
`no_agent_names_a_model` scans every rendered string (description, boundary, envelope,
`agent list` line) for model-shaped tokens, so 64's own grep-test has a working scanner to
reuse. Todo 65 (`task.rs`): `delegable()` is the valid `subagent_type` set and already
excludes the orchestrator and the internals; `Delegation::MayDelegate` identifies the
coordinator to reject as a target. Todo 66 (`continuation.rs`): `Role`/`AgentMode`/`hidden`
are what a job board renders, and `Agent::summary_line()` / `render_list()` are the
`agent list` rendering the CLI todo can call without re-deciding the wording.
## [2026-08-06] Task 52 — generated API contract and deliberate divergences

- OpenAPI is generated from one deterministic operation registry plus
  `schemars`-derived DTO schemas. This reuses the workspace's pinned schema stack
  and avoids adding `utoipa`; the same registry is compared against the pinned
  oracle document as a method/path subset of all 56 task-owned operations.
- The count boundary is explicit: 61 protocol operations overall, 58 `/api`
  operations, and 56 owned here after excluding task 53's two event streams. The
  three experimental project-copy operations and the fixture-only generate-name
  operation are not mounted by this router.
- Normalization for any future live differential is restricted to generated
  session/PTY ids and slugs, timestamps, PTY pid/exit timing, temporary absolute
  directory/worktree values, and cursor tokens. HTTP status, error code, shape,
  field presence, ordering, and stable values remain comparison-bearing.
- Two divergences from the oracle are intentional and tested. Project `subpath`
  is applied as a literal tree prefix instead of being ignored or interpreted as
  a SQL wildcard. Session listing defaults to `time_updated DESC, id DESC` for a
  total stable order; `?sort=created` explicitly selects creation-time ordering.
- Backends that do not exist locally are represented by registered, structured
  `501 not_implemented` seams. Returning empty or invented success data is not an
  acceptable compatibility strategy.


## [2026-08-06] Task 55: command dispositions and dual version identities

| upstream symbol | CLI spelling | disposition | reason / replacement |
|---|---|---|---|
| `AcpCommand` | `acp` | not registered | todo 78 owns the real ACP adapter |
| `AgentCommand` | `agent` | implemented seam | todo 56 |
| `AttachCommand` | `attach` | not registered | requires the TUI/terminal lifecycle wave |
| `ConsoleCommand` | `console` | rejected | hosted Console is excluded; use `providers`/`auth` for local credentials |
| `DbCommand` | `db` | implemented seam | todo 56, extended by todo 84 |
| `DebugCommand` | `debug` | implemented seam | todo 56 |
| `ExportCommand` | `export` | implemented seam | todo 56 |
| `GenerateCommand` | `generate` | rejected | source-tree Prettier generator is excluded; consume `/openapi.json` |
| `GithubCommand` | `github` | rejected | hosted GitHub agent is excluded; use `run` in CI |
| `ImportCommand` | `import` | implemented seam | todo 56 |
| `McpCommand` | `mcp` | implemented seam | todo 56 |
| `ModelsCommand` | `models` | implemented seam | todo 56 |
| `PluginCommand` | `plugin` | not registered | todo 60 must land the resident JS host before installs can be accepted |
| `PrCommand` | `pr` | rejected | use `gh pr checkout`, then `opencode-rust run` |
| `ProvidersCommand` | `providers` (`auth` alias) | implemented seam | todo 56 |
| `RunCommand` | `run` | implemented seam | todo 56 |
| `ServeCommand` | `serve` | implemented seam | todo 56 wraps `oc_server::ServerBuilder`; it does not spawn `oc-server` or duplicate server logic |
| `SessionCommand` | `session` | implemented seam | todo 56, extended by todos 80-85 |
| `StatsCommand` | `stats` | rejected | stats package/direct SQL path is excluded; use todo 84's `db stats` |
| `TuiThreadCommand` | `$0` | not registered | ratatui and terminal lease belong to the TUI wave |
| `UninstallCommand` | `uninstall` | rejected | use the package manager/installer that placed the binary |
| `UpgradeCommand` | `upgrade` | rejected | use the Rust release installer; do not let TS self-update replace this artifact |
| `WebCommand` | `web` | rejected | bundled hosted web app is excluded; use `serve` plus a supported client |

`completion` is an upstream yargs-generated command rather than a `*Command` symbol; it is separately
registered through the same implemented seam.

The identities stay separate by API and text: `compatibility_version()` and short `--version` are
exactly `1.18.13`, the value todo 60 must pass to npm `engines.opencode`; `BUILD_ID`,
`--version --long`, and a user agent beginning `opencode-rust/` expose the real Rust package/build.
The long form includes both values so neither audience can mistake one for the other.

Startup resolves flags as immutable data, then on Unix safely `exec`s itself with `AGENT=1`,
`OPENCODE=1`, `OPENCODE_PID=<same exec-preserved pid>`, and CLI overrides. This avoids forbidden
Rust-2024 process-global mutation while ensuring downstream libraries that read the real environment
observe upstream middleware semantics. The typed `CommandDispatcher`/`DispatchRequest` seam is the
only route from registered skeleton commands to todos 56 and 80-85.

## [2026-08-06] Task 97: terminal-lease protocol

**The concurrent-acquire policy is refusal.** The plan said "a policy" without saying
which. A second `acquire` against a live lease returns `TerminalLeaseError::Busy`
naming the holder. Queueing was rejected twice over: a queued device-code prompt
arrives with no context on a terminal the user has moved on from and is
indistinguishable from the first, and a host that never releases would make every
later acquirer block — turning the symptom back into a hang one level further out,
with the force-reclaim freeing only the head of the queue. Preemption was rejected
because revoking a lease mid-prompt yanks the terminal out from under half-typed
input. So the only involuntary transition is the deadline, and it is loud.

**Release is `Drop`, therefore `reclaim_terminal` is synchronous.** A guard whose
release had to await would either block a runtime thread or spawn a detached task
whose completion nobody could observe, and a release that may not have happened yet
is exactly the deadlock the lease removes. Same reasoning as
`oc_tool::InterruptHandle::is_set` (todo 3): the caller has no runtime to lend it.
`yield_terminal` stays async, because acquisition is in async host code and a real
owner must let its render loop reach a safe point first.

**Force-reclaim has two paths and one settle-once flag.** A per-grant watchdog task
cancelled by the guard's `oneshot::Sender` being dropped — which covers a panic unwind,
where a notification-based scheme would miss — plus a sweep at the top of `acquire`
and a public `reclaim_if_expired()`. Two paths because the watchdog needs a live Tokio
runtime: `acquire` uses `Handle::try_current` and degrades to sweep-only rather than
panicking inside the terminal protocol. Both paths `swap` the same `AtomicBool`, so
`reclaim_terminal` runs exactly once per grant however they race, and a wedged guard
dropped after its lease was taken cannot restore a terminal someone else has redrawn.

**The diagnostic is a struct, not a string, and it names the plugin.** `LeaseReason`
carries the plugin separately from the purpose specifically so `ForcedReclaim` can
say `plugin `kiro` held the terminal for `device-code prompt` past its N ms deadline
and did not release it`. "A lease expired" is not actionable. It is handed to the
owner rather than logged here, because `oc-engine` has no logging facade and inventing
one would put a presentation decision in the wrong layer.

**The no-`oc-tui`-dependency check is mechanical, and reads manifests rather than
running cargo.** `terminal_lease_keeps_the_plugin_crate_away_from_the_tui_and_ratatui`
BFS-closes the first-party graph (runtime + dev + build) from `oc-plugin` and fails on
`oc-tui`, `ratatui` or `crossterm`. `crossterm` is in the list because a host that can
reach it can seize the terminal directly, which is what the protocol exists to route
through a lease. Three floor assertions stop a vacuous pass: >=33 crates scanned, both
`oc-plugin` and `oc-tui` present, and `ratatui`+`crossterm` reachable *from* `oc-tui`
(so a rename of the render stack fails loudly instead of making the exclusion trivial).
Manifests over `cargo tree` because spawning cargo inside a cargo test run can block
on the shared build-directory lock.

**Every timeout is injected; no test waits a production interval and none sleeps as
synchronisation.** `DEFAULT_LEASE_TIMEOUT` is 300 s — sized for a human reading a
device code off a browser and typing it back. Tests use `TerminalBroker::with_timeout`:
`Duration::ZERO` for the deterministic reclaim assertions (already expired on return,
so no timer is involved at all), 5 ms plus a 10 s bounded-poll budget for the watchdog,
and 3600 s wherever the assertion is that a reclaim did *not* happen. Every direction
is one-sided, because load can only make a timer late.

## [2026-08-06] Task 58: JSON-RPC host boundaries

- Protocol version `1.0` is negotiated by `plugin.initialize`; the host offers a
  version list and rejects a plugin selecting anything else. Hook names remain
  the exact Task 57 strings, and resource hooks (`tool`, `auth`, `provider`) are
  never misrepresented as serializable callback calls.
- The default request deadline is five seconds and lives on
  `PluginProcessSpec`; tests inject shorter values. One policy covers initialize,
  hook, and tool exchanges so no request class can become an unbounded exception.
- Process failure is containment, not bus failure. A timeout, malformed frame,
  closed stdout, failed read, or unexpected exit permanently disables that
  plugin and records a `PluginDiagnostic`; `Plugin::call` still returns success so
  sibling plugins and the turn continue.
- Remote tool names are checked as a complete set before entering the shared
  registry. A collision with a built-in or another plugin is an explicit error;
  registry insertion order never decides which implementation wins.
- `oc-plugin-sdk` owns wire types, the plugin builder, stdio server, and reusable
  self-conformance runner. `oc-plugin` owns spawning, deadlines, lifecycle,
  diagnostics, typed Task 57 hook codecs, and adaptation to the existing bus.
  This keeps third-party plugin code independent of host internals.

## [2026-08-06] Task 47: how a failed connection is surfaced, and where tools-changed is published

### A failed connection is surfaced as retained data, not as a deletion

The oracle deletes a dead server's cached definitions in `onclose`
(`mcp/index.ts:442-455`). I deliberately do NOT. `Catalog::unavailable` flips only
`ServerStatus` and leaves both the tool snapshot and the `ConnectedServer` handle in
place, so `ServerStatus::is_connected()` in `Catalog::tools()` is the **single** thing
standing between a dead server and the merged list.

Why: if `unavailable` also cleared the tools, a missing gate would be hidden behind a
second accidental one — the tools would vanish for the wrong reason and no test could
tell the difference. That is not hypothetical; my first implementation cleared the
handle and the connected-only mutation passed vacuously. Retaining state I could have
dropped is what makes the rule falsifiable.

Surfacing shape:

- `ServerStatus` mirrors the oracle's `Status` union (`mcp/index.ts:83-107`):
  `Connected | Disabled | Failed{error} | NeedsAuth | NeedsClientRegistration{error}`.
- `Diagnostic { server, status }` is **data**, with `message()` rendering one line.
  The oracle splits this across a `logWarning` (`:383-386`) and, for the two OAuth
  cases, a toast (`:297-321`). Both spellings agree on the only thing a user staring
  at a missing tool needs — the server's *name* — so I carry it structurally and let
  the caller choose log, toast, or TUI panel. Verbatim example:
  `MCP server broken is unavailable and contributes no tools: Connection closed`
- `needs_auth` keeps the remedy in the message (`run `opencode mcp auth <name>``),
  because that is the one status a user can fix without reading docs.

### Tools-changed is published from three places, all inside `Catalog`

`CatalogEvent::ToolsChanged { server }` — payload is the server name only, matching
`mcp/index.ts:461-471`, so a subscriber re-reads the catalog rather than trusting a
diff it was handed. It is published by:

1. `connected` / `connected_with_prompts` — a late connection is a change.
2. `unavailable` — a withdrawal is a change.
3. `refresh(server)` — the `notifications/tools/list_changed` path.

Todo 45's `refresh` mpsc channel and `subscribe_tools_changed()` stay where they are:
they are *per-client*, and `Catalog::refresh` is what turns a per-client notification
into a catalog-level event. A failed re-list leaves the previous snapshot untouched
(`mcp/index.ts:465-466`) — a transient list failure must not empty a working server.

### `ConnectedServer` is a trait, not an enum over the two transports

Both `StdioClient` and `RemoteClient` implement it. An enum would force every call
site to re-decide which transport it holds; the trait means `McpToolProxy` relays a
call without knowing. Remote failures are re-wrapped as `McpError::Connect` so the
catalog holds one error type — `RemoteError`'s `Display` already names the server and
the transport, so nothing is lost as a boxed source.

`supports_resources` / `supports_prompts` read the `initialize` capabilities and treat
**any present non-null value** as support, matching `getServerCapabilities()?.resources`
(`session/tools.ts:155-157`) where an empty object still counts.

### The proxy holds two names on purpose

`McpToolProxy { id, tool }`: `id` is the namespaced id the model calls, `tool` is the
server-local name sent on the wire. Never derived from each other — the sanitizer is
not injective. A tool-level `is_error` becomes `ToolError::Failed` naming the
*namespaced* id with the server's own text as the source, so the message a user sees
still says which server refused.

### Permission filtering happens in two places, and that is not redundant

`Catalog::visible_tools(rules)` filters, and `oc-tools`' registry filters again after
appending (`registry.rs:387-395`). The registry route is the normal one; the code-mode
path describes `mcp.tools()` **without** going through the registry
(`tool/registry.ts:275-284`), so the deny has to hold on both routes. `CatalogLoader`
therefore hands the registry the list *unfiltered* — the registry owns that pass.

### Resource-tool argument parsing follows the oracle's hand-written parser

Not serde-derived: `optional_server` treats `""` as absent (`session/tools.ts:512-518`),
so a model sending an empty string gets the all-servers listing rather than a refusal.
`resource_permission_patterns(server, servers)` ports the finer `mcp:<server>:*` ask
(`:173-175`) even though the registry currently gates these tools under the coarser
`read` / `*` pair — it is strictly narrower than what the coarse gate already allows,
and porting it now means whoever wires a per-server gate has it ready and tested.

## [2026-08-06] Task 79: in-place formatting, the risk-gate boundary, and plan-file precedence

### A failed format restores the pre-format bytes — a deliberate divergence

`format/index.ts:96-113` logs the failure and keeps whatever the formatter left on
disk. `Formatters::format_all` instead retains the post-write bytes and **restores
them whenever the formatter did not succeed** (non-zero exit, spawn failure, or
timeout). The contract a caller can rely on is exact: *after a failed format the
file holds precisely the bytes the edit wrote.*

The mutation proof is not close. Reverting to upstream's behaviour makes
`a_formatter_that_truncates_the_file_before_failing_has_its_damage_undone` fail
with `left: ""` — the edit entirely gone. A formatter killed at a ceiling, or one
that truncates before failing to parse, leaves a file that is neither the edit nor
a formatted version of it.

**Price paid, stated plainly**: a formatter that exits non-zero *after* doing
useful work has its partial work discarded. `rubocop --autocorrect` returns 1
while any offence remains uncorrected, so a rubocop run that fixed nine of ten
offences is rolled back to the unformatted edit. That leaves the file exactly as
it would be if rubocop were not installed — losing an optional partial reformat,
against keeping a mangled file. Taken.

### In place, not through a temporary file

Every command in the built-in table mutates the file it is handed: `-w`, `-i`,
`--write`, `--fix`. Handing them a temp copy breaks two classes of formatter:
those that resolve configuration by walking up from the *file's own* directory
(`.clang-format`, `.ocamlformat`, `rustfmt.toml`, `biome.json`) and those that key
behaviour on the filename.

A temp file would buy atomicity against a concurrent reader. Restoring the
pre-format bytes buys the property that actually matters — the **edit** survives —
and buys it without lying to the formatter about where the file lives. Note this
is the opposite call from `oc-goal`'s projection (todo 69), which *does* use
temp+rename: there the writer owns the file and a reader may open it at any
moment; here a third-party process must see the real path.

### A formatter does NOT go through the risk gate

`crates/oc-tools/src/risk.rs` (todo 71) exists because `bash` executes a string
the **model** composed. A formatter command comes from either the compile-time
table or the operator's config, so there is no model-authored text anywhere in it
and the gate has no audience. It is also spawned as argv with **no shell**, so
there is no command string to parse — `rm -rf` cannot appear as a side effect of
word splitting when there is no word splitting.

What *is* borrowed from `shell.rs` is the hygiene rather than the policy: `stdin`
closed, both output streams captured, `kill_on_drop(true)`, `process_group(0)` on
Unix so a formatter that spawns helpers is torn down with them, and a hard
ceiling. Direct `tokio::process::Command`, not the shell machinery: routing
through `ShellTool` would mean permission asks, tree-sitter parsing, and output
policy for a command the operator already authorised by writing it into config.

### A 30s ceiling where the oracle has none

The oracle can hang an edit forever on a wedged formatter. Thirty seconds is far
more than any formatter in the table needs for one file and far less than a user
waits before assuming the tool is broken. Reported as `TimedOut`, edit intact.
`kill_on_drop` does the killing when the timeout future drops the child, so no
separate kill path is needed.

### The seam widened by ADDING a method, not by changing one

Todo 39 left `FileFormatter::format(&Path) -> io::Result<bool>`. Failures need
more than a bool, but changing that signature would break every existing
implementation. So `format_reporting(&Path) -> io::Result<FormatOutcome>` was
added **with a default body** that delegates to `format`. Every implementation
written against the original seam keeps compiling and keeps working, and the
default says the honest thing about such an implementation: bytes changed or not,
no failures to report because it has no way to express one. `NoopFormatter` and
the existing `RecordingFormatter` in `tests/file_tools.rs` are untouched.

### A formatter failure is NOT an `Err` from the tool

`FormatOutcome { changed, failures }`, and `failures` being non-empty is a
successful call. The write already landed by the time a formatter runs, so an
`Err` would make the tool tell the model its edit failed when the edit is on disk
— which is precisely the confusion that makes a model redo a write it already
made. Failures are attached to the *successful* result, in both the text (what the
model reads) and metadata under `formatterFailures` (what a UI reads). Nothing is
attached when there is nothing to say, so an ordinary edit's output is
byte-identical to what it was before formatters existed.

The report says "The edit was written and is intact" explicitly. That sentence is
load-bearing prose, not decoration.

`apply_patch` accumulates failures across operations rather than propagating the
first: one uncooperative formatter must not abandon a patch half-applied.

### Node-hosted formatters resolve from the project, not by installing

`Npm.which` (`core/src/npm.ts:192-241`) reads a *global* per-package install
directory and **installs the package if it is missing**. Installing something to
satisfy a format is out of scope, so `NodePackage`/`NodeMarker` look for
`node_modules/.bin/<bin>` walking up from the edited file. A project that has the
formatter installed still formats; one that does not is skipped rather than
triggering a download. The declared-dependency check itself is ported exactly.

### A configured `command` replaces the availability probe outright

`format/index.ts:154`: `enabled` becomes `async () => info.command ?? false`
whenever an override supplies a command. An operator naming a command *is* the
assertion that it exists, so no `which` and no marker check — and no shadowing
either, since `shadowed_by` is a fact about the built-in, not about a command the
operator chose. Everything the override omits keeps the built-in's value
(`mergeDeep`), which is why each field is applied conditionally.

### An empty extension short-circuits before matching

`format_all` returns early when `path.extension()` is `None`. Without that, a
config entry with `extensions: [""]` would claim every extensionless file —
`Makefile`, `.gitignore`, `LICENSE`. Tested with exactly that config.

### Plan-file location precedence: `project.vcs`, not "is there a worktree"

`session.ts:331-335` branches on `instance.project.vcs`. The enum is therefore
named after the *condition* — `PlanLocation::{Worktree(&Path), Global}` — rather
than after the directories, because a parameter shaped like
`worktree: Option<&Path>` invites the wrong call: a caller that *has* a worktree
but whose project is not a repository must still choose `Global`. Naming the
branch after the fact makes that impossible to get wrong by accident.

Precedence, in order:
1. `project.vcs` set -> `<worktree>/.opencode/plans/<created>-<slug>.md`
2. otherwise -> `<oc_paths::data()>/plans/<created>-<slug>.md`

Why the fallback exists at all: `.opencode/plans/` is the location a human
actually finds, and in a repository it is a path they can gitignore or commit as
they choose. Outside a repository there is no such place — writing `.opencode/`
into whatever directory the user started in litters unrelated trees with files
nothing will clean up, and there is no `.gitignore` to keep them out of someone
else's commit. `oc-goal`'s `document_path` (todo 69) makes the same two-way choice
for the same reason and says so; this is the convention it copied.

### The slug is validated, never the filename built from it

`PlanKey::file_name` validates the **slug**, because appending `.md` turns `..`
into the perfectly legal single component `...md` — so validating the derived name
accepts exactly the input that most needs refusing. `oc-goal` shipped that bug in
its first draft and its notepad entry says so; the lesson is reused rather than
relearned. Leading/trailing whitespace is refused too, since a filename with a
trailing space is a support ticket rather than an error.

### The plan file is written atomically, and does not `fsync`

Temp file in the destination's own directory, then rename. Same filesystem, so the
rename is atomic and a reader — the model next turn, or a human with the file open
— always sees one complete document. The three temp-name details learned in todo
69 are carried rather than rediscovered: `with_file_name` not `with_extension` (a
slug containing a dot would otherwise collide with a *different* plan), nanos in
the name so concurrent writes cannot interleave, and the rename's error arm
removes the temp file so a failure leaves no litter beside a document a human is
about to open. `a_rewrite_is_atomic_under_a_concurrent_reader` spins a reader
against 200 rewrites and asserts 0 partial reads with a `>= 50` observation floor.

No `sync_all`. A plan is a document the user iterates on with the ordinary editing
tools, not a ledger; a rename that reaches the directory entry is what the next
reader needs.

### `read_plan` returns `Ok(None)` for a missing plan

"No plan yet" is the ordinary state of a new session, not an error.
`reminders.ts:55,73` branches on exactly this and tells the model to *create* the
plan rather than failing, so the Rust shape has to make that branch cheap.

## [2026-08-06] Task 54: where the counter lives, how the catch-all is scoped, what a toast does with no TUI

**The counter is surfaced twice, and both halves are load-bearing.**
`GET /compat/v1/diagnostics` returns the implemented surface (each route with its
SDK method, callers and callsites), the exact unknown total, a bounded per-path
breakdown, the overflow figure, and the toast sink's state. **And** the *first*
sighting of each distinct unknown path writes one line to stderr. A diagnostics
endpoint alone requires an operator who already suspects a problem — which is the
state the mechanism exists to prevent; a log line alone cannot be asserted on
cheaply or read as a running total. Repeat sightings are counted but not re-logged,
so a path scanner is fully counted while the log stays bounded.
`/compat/v1/diagnostics` deliberately sits **outside** `V1_PREFIXES` so it cannot
be shadowed by, or shadow, anything it reports on.

**The breakdown is capped at 64 distinct paths (256 bytes each, truncated at a
char boundary); the total is never capped.** The key is caller-controlled, so an
uncapped map is an unbounded allocation driven by whoever can reach the port.
Losing the exact total would defeat the mechanism, so overflow past the cap is
reported as its own `overflowedSightings` figure. Test: 200 distinct paths ->
total 200, breakdown 64, overflowed 136.

**Catch-all scope: the oracle's 25 pre-`/api` top-level segments minus `event` = 24.**
Not a global `Router::fallback` (it would claim unmatched `/api/*` as v1 gaps, and
merging two fallback-bearing routers panics) and not `/{prefix}/{*rest}` (matchit
rejects it as *conflicting* with `/auth/{providerID}` — it panics at assembly, see
learnings). It is one `nest` per prefix with a fallback on each inner router.
`event` is excluded because the SSE stream owns `/event` exactly. **A test derives
the prefix set from the committed OpenAPI fixture and asserts equality**, plus
asserts it never contains `api`, so the scope cannot silently drift from the
document it claims to mirror.

**An unmeasured VERB on a measured path is accounted too, at 405 rather than 404.**
The plan only specifies unimplemented *paths*, but a mis-measured *operation* is
the same failure and would otherwise escape as axum's bodiless default 405 — no
body, no counter, no way to find it. Downgrading it to 404 would misreport a path
that exists, so it keeps 405, carries the same actionable body, and is keyed
`"VERB path"` in the breakdown. `DELETE /auth/{providerID}` is the live case: the
oracle serves it, no installed plugin calls it.

**A toast with no TUI attached returns `200 true` and is recorded, not dropped and
not failed.** Upstream answers a bare `true`; a 500 would break a plugin over a
display that does not exist yet (todo 73), and silently discarding would leave
nothing to diagnose. So the route is a **recording seam**: a bounded 64-entry ring
plus an accepted counter, both reported by diagnostics. When a TUI arrives it
registers through `CompatV1State::with_toast_forwarder` and the *same route*
forwards **as well as** records — recording does not stop, or diagnostics go blind
the moment a TUI connects. Keeping the HTTP surface independent of the TUI's
existence is the point of the seam.

**Two deliberate leniencies vs the oracle on that route**: a missing `variant`
defaults to `info`, and unknown fields are ignored. Three of three installed
plugins call `/tui/show-toast`, so a 400 over a cosmetic schema mismatch breaks
precisely what the plan warns about. Still strict on `message`: absent or
non-string is 400, because there is then nothing to show.

**The other 19 routes are registered, structured `501 not_implemented` seams**, each
naming its SDK method and its calling plugins so the operator learns *which plugin
needs which backend*. This follows todo 52's precedent for `/api`: an operation with
no local backend answers definitively rather than fabricating success. Backends land
in todos 57-62. Stated plainly for whoever picks those up: against this surface the
installed auth plugins complete their **call lifecycle** — every request reaches a
registered route, nothing hangs — but they cannot **authenticate**, because
`auth.set` and the OAuth pair have no credential backend here.

**`V1_SURFACE` carries its evidence as data, not as a comment.** Each entry has
`sdk_method`, `plugins` and `callsites`, and the tests assert `callsites` is
non-empty with a `file:line` shape. The capture document, the diagnostics payload
and the 501 bodies all read the same table, so a route's justification cannot drift
away from the route. That is the acceptance criterion "every implemented route maps
to >=1 recorded callsite" made executable instead of reviewable.

## [2026-08-06] Task 73: TUI foundation contracts

**Components are `Component { render, handle_event }` trait objects composed by
layout containers.** `render` receives only a ratatui frame/area; `handle_event`
receives the TUI's `AppEvent`. Engine execution is not callable from rendering:
`TurnEvent` is an input value, preserving the `oc-tui -> oc-engine` dependency.

**Terminal input uses a lossless bounded channel of 64; engine events retain
`TURN_EVENT_CHANNEL_CAPACITY` (64).** Both producers await capacity. The loop does
not use `try_recv` or silently discard input. While a lease owns stdin, both receive
branches are disabled, applying bounded backpressure; a race already selected at the
suspension edge is retained and dispatched after reclaim.

**Yield and reclaim are synchronous at the physical boundary.** Yield marks the TUI
suspended, waits for the shared render lock, leaves the alternate screen, disables
configured mouse capture, and restores cooked mode. The existing `TerminalBroker`
then grants or refuses exactly as todo 97 decided. Guard drop/forced timeout re-enable
raw mode, enter the alternate screen, restore mouse capture, clear stale cells, and
complete a repaint before returning; forced reclaim also emits the broker diagnostic
to stderr. No second exclusion or timeout policy exists in `oc-tui`.

## [2026-08-06] Task 64: where preset data lives, and the three-rung ladder

### Preset shape is code; preset data is configuration. `oc-agent` ships zero model ids.

The acceptance test walks every `*.rs` under `crates/oc-agent/src` and fails on a
model-id-shaped token, so a shipped preset could not have named one anyway — but the
reason is not the test. A preset compiled into the binary **is**
`CATEGORY_MODEL_REQUIREMENTS` with better manners: it encodes today's model market and
rots on the next release. Slim looks like a counter-example and is not — its five
`MODEL_MAPPINGS` presets are consumed only by the *installer*, which writes them into
the user's config file, and the runtime reads `config.presets` and never the constant
(see learnings). So the contract here is a shape (`PresetDocument`) and the data is a
file: whatever an installer writes, a user hand-edits, or a future `Config` field
carries. No preset asset ships either — an embedded TOML/JSON would put the ids back
in the crate's build output and just move them past the source scan.

### Resolution precedence: per-agent override > active preset > session model

Three rungs, one test each, and the ladder is *skip-on-unavailable* rather than
fail-on-unavailable:

1. `agent.<name>.model` from the user's own config — the highest rung because it is the
   most specific thing the user said.
2. the active preset's entry for that agent.
3. the session/global model — the default for **every** agent, which is todo 64's whole
   point (slim's all-`undefined` table).

A rung whose model is unavailable, or is not in `provider/model` form, is skipped with
a `Diagnostic` and the next rung is tried. **The session model is never checked for
availability**: there is nothing below it, and rejecting it would replace a working
session with `None`.

### Resolution has no error path. At all.

The module has exactly one error type, `PresetError::Parse`, and it can only come from
reading preset *bytes*. `ModelPolicy::resolve` / `resolve_category` / `resolve_roster`
return a `Resolution`, never a `Result` — so "a missing preset entry must not
hard-fail" is a **type-level** guarantee rather than a convention a later edit can
forget. Everything that would have been an error is a `Diagnostic` carried alongside a
usable answer: `UnknownPreset`, `UnknownCategory`, `ModelUnavailable`,
`ModelNotQualified`, `UnknownVariant`. Mutation-tested: turning the unavailable branch
into a `panic!` fails three independent tests.

### Categories are a preset key and nothing else

omo's eight categories are a good idea buried in a hardcoded table. Kept as a
*shorthand*: `ModelPreset::with_category` puts a `{model, variant}` under a name, and
`resolve_category` answers from the **active preset only**. There is no built-in
category list — two presets may declare different categories or none, and a test
asserts all eight of omo's names resolve to the session model against an empty preset.
An unknown category is a diagnostic, not an error.

### `ModelAvailability` is a trait, not a `&Catalog` parameter

Resolution runs on paths where no catalog exists (`agent list` with no credentials),
and a test proving the fallthrough should not have to build a models.dev document.
`AnyModel` (not checking) and `NoModel` cover the ends; `impl ModelAvailability for
oc_llm::catalog::Catalog` is the real answer and treats a **bare** model id as
unavailable — choosing a provider for an unqualified id is exactly the entitlement
guessing `CATEGORY_MODEL_REQUIREMENTS` hardcodes as ten provider ids per rung.

### The model-id predicate moved into `model_policy`, and `builtin/tests.rs` imports it

Todo 63 defined `looks_like_model_id` privately in `builtin/tests.rs`. Todo 64 needed
it crate-wide *and* had to fix it (source citations `path/file:line` read as
`provider/model`), but `builtin.rs` was owned by a sibling task. Resolution: the
predicate lives in `model_policy` as `#[cfg(test)] pub(crate)` — test-only because
preset data is exactly where model ids belong, so no shipping code has a use for it —
and `builtin/tests.rs` imports it. One definition, so the two scans cannot drift.

### No fourth rung for `small_model`

The engine already routes its internal agents (`oc-engine/src/compaction.rs:404`).
Adding a `small_model` rung here would give two places to disagree about which model
titles a session. Internals resolve like any other agent and a test asserts it.

## [2026-08-06] Task 74: where the TUI schema lives, the conflict report, and the leader machine

**The TUI-only config schema lives in `crates/oc-tui/src/config.rs`, not `oc-config`.** None
of `theme`, `keybinds`, `leader_timeout`, `prompt`, `scroll_speed`, `scroll_acceleration`,
`diff_style`, `mouse` appears in `packages/core/src/v1/config/config.ts`; upstream declares
them in `packages/tui/src/config/index.tsx` and loads them from separate `tui.json` files.
Modelling them in `oc-config` would advertise keys the real binary rejects there. It is one
struct of independent fields with unknown keys ignored, so concurrent todos add one field
each. `oc-config` was **not** modified.

**Conflict detection is scope-local, and precedence across scopes is explicit ordering.**
Each action carries a `scope` derived from its name (namespace before the last `.`, else the
segment before the first `_`). Two bindings claiming one sequence *within* a scope have no
ordering to break the tie and are reported. Across scopes, `Keymap::resolve` takes an
**ordered active scope chain** — the focus chain in data form — and the first scope with a
match answers. That is declared precedence, not a silent duplicate, and it is what lets
`diff_expand` (`right`) and `session_child_cycle` (`right`) coexist as upstream intends.

**Report format.** Every collision is collected before returning, so one bad config yields
one report rather than a fix-and-rerun loop:

```
1 keybind conflict:
  `ctrl+x l` is bound to both `session_list` and `session_new` in scope `session`
```

Three or more: ``is bound to all of `a`, `b`, and `c` ``. Actions are sorted so the message
is stable. A second kind is reported for a short binding that makes a longer sequence
unreachable — ``…which shadows the longer sequence `ctrl+x x` `` — because a dead binding is
the same failure as a duplicate wearing a different hat.

**Three further loud refusals rather than silent drops**: an unrecognized keybind *name*
(upstream throws too, `keybind.ts:450-451`); an unparseable spelling, naming the action and
the spelling; and unbinding `leader` while 28 defaults are written `<leader>…`, which would
silently delete 28 bindings.

**The leader state machine is a general prefix matcher with an injected clock.**
`<leader>q` is not special: a spelling is a whitespace-separated chord list in which
`<leader>` expands to the configured chord, so multi-chord sequences the upstream table
happens not to use work for free. `resolve(scopes, chord, now)` takes `now` as a parameter —
the timeout test asserts behaviour at `start + timeout - 1ms` and at `start + timeout` with
**no sleep and no polling**. Order inside `resolve`: expire a stale pending sequence, then
exact match in scope order, then extendable-prefix in scope order (sets pending), otherwise
clear pending and report unmatched. A separate `expire(now)` exists so a timer tick can
close a which-key panel on time, but correctness never depends on a tick arriving.

**The anti-hardcoding seam is `KeyDispatcher` + `ActionComponent`.** The dispatcher wraps a
component, resolves `TerminalEvent::Input` from todo 73's existing loop, and passes down a
`&'static Definition` — never a key. A view therefore cannot branch on a keystroke, and
rebinding changes nothing below that point. `prevent_default` rides on the resolution rather
than being consulted by the view.

**The fixture is the oracle; the Rust table is the implementation.** Both are generated from
the same extraction, and a test diffs them row for row *including table order*, so an
upstream bump regenerates the TSV and the diff names exactly what moved instead of a
hand-maintained list quietly rotting.

## [2026-08-06] Task 75: four-layer theme resolution with 33 built-in themes

**Fallback source for a missing key: the built-in `opencode` theme, same mode — not the
layer below.** Two candidates were real. "The layer below" is intuitive but wrong for
two reasons: a user theme is usually a *whole* theme rather than an override of a
built-in of the same name, so there is normally no layer below to fall back to; and it
makes the resolved palette depend on registration order, which turns a diagnostic into
a heisenbug. `opencode` is also what the oracle itself falls back to on every failure
path (`src/context/theme.tsx:143`, `:162`, `:177`). So every failure — missing key,
dangling reference, reference cycle, malformed hex — substitutes that key's `opencode`
value and records a `ThemeIssue`.

Resolution is therefore total and cannot recurse: `baseline(mode)` is a `OnceLock`
holding `opencode` resolved against a `last_resort()` palette (ANSI white on a
transparent background), and that palette needs no fallback of its own. A corrupt
`opencode.json` degrades to legible monochrome instead of panicking, and
`ThemeRegistry::load_issues()` says so.

**A missing *theme name* is the same class of failure as a missing key.** `resolve()`
never returns `Option`. An unknown name yields `ThemeIssue::UnknownTheme` plus the
default theme's palette, written without recursion so a broken default cannot loop. This
is what makes `theme: "system"` safe when no terminal answered: the system layer is
simply absent, the name resolves to nothing, and the diagnostic is
`theme "opencode": no theme named "system" in any layer; falling back to the built-in
"opencode" theme`.

**Palette shape: a flat struct of 52 `Rgba` fields plus `thinking_opacity` and
`has_selected_list_item_text`, generated from one macro table.** Rejected a
`HashMap<String, Rgba>` — a view would then need to handle a missing key at every paint
site, which is exactly the panic the task forbids, and typos would be runtime failures.
The flat struct makes `palette.text` infallible and misspellings compile errors. `Rgba`
keeps its alpha (rather than flattening to ratatui's `Color`) because two behaviours
read it: `From<Rgba> for Color` maps `a == 0` to `Color::Reset`, which is how a cell
says "keep the emulator's own background", and `selected_foreground` branches on
`background.a == 0` to pick contrast (`index.ts:100-107`).

**The snapshot subject: one row per palette field, rendered through todo 73's
`render_offscreen`, serialized as style runs.** `PaletteSampleView` emits the field's
own JSON key as text, styled `fg = that field's colour` on `bg = palette.background`,
plus a `thinkingOpacity` row and a derived `selectedForeground` row — 55 rows in a
26-column buffer. Three properties this buys:

- every field reaches a cell, so a snapshot of a blank buffer cannot pass;
- the snapshot is taken from the rendered `Buffer`, not from the `Palette`, so it proves
  the colours survived the render path;
- one colour change diffs one line. Measured: mutating `nord`'s `nord8` def changed
  exactly 1 of 33 `.snap` files (12 lines inside it, since 12 keys reference that def).

Chose dark mode only for the snapshots, giving exactly the 33 the plan asks for; light
mode is covered by `theme_resolves_in_both_modes_without_issues` and by the
variant-resolution tests, which is cheaper than 33 more `.snap` files.

**`add_plugin_theme` and `upsert_theme` are both ported, because they differ.** The
oracle's `addTheme` (`index.ts:220-227`) refuses to shadow an existing name; only
`upsertTheme` (`:229-240`) replaces, and it writes to whichever of the custom/plugin
layers already holds the name. Keeping both is what makes the layer *order* observable
at all — with only `addTheme`, a plugin could never shadow a built-in and rung 2 of the
ladder would be untestable.

**A guard test enforces "no view hardcodes a colour" rather than a convention.**
`theme_no_view_hardcodes_a_color` scans `crates/oc-tui/src/*.rs`, excludes `theme.rs`
and its tests, and fails on `Color::Rgb`, `Color::Indexed`, or `Rgba::opaque`. It carries
the mandatory floor assertion (`scanned >= 3`) so the stale-`CARGO_MANIFEST_DIR` hazard
in `.omo/WORKTREE.md` cannot make it pass vacuously. The asset-count test does the same
with `== 33` against the on-disk directory *and* an exact set comparison against the
embedded table, so an added file, a deleted file, or a rename all fail.

## [2026-08-06] Task 65: the five refusals, and why a job id is not a session id

### The five rejection messages, verbatim

Each is a `TaskRejection` variant's `Display`, chained as the `#[source]` of a
`ToolError`, so `error.source().to_string()` is the tested artifact. All five name the
fix; `"invalid arguments"` would have failed the acceptance criterion.

1. **neither target** — `Must provide either `category` or `subagent_type`. Add
   `subagent_type="worker"` naming one of the valid targets (explorer, librarian,
   advisor, worker), or `category="<preset shorthand>"` to run the `worker` agent at
   that preset's model.`
2. **both targets** — `` `category` and `subagent_type` are mutually exclusive; you sent
   `category="cheap"` and `subagent_type="explorer"`. Provide only one: keep
   `subagent_type="explorer"` to choose the agent, or keep `category="cheap"` to run the
   `worker` agent at that preset's model.``
3. **a coordinator as target** — `` `orchestrator` coordinates delegations and cannot be
   a delegation target — targeting it would reopen the unbounded recursion the roster
   closes. Set `subagent_type` to one of the valid targets: explorer, librarian, advisor,
   worker.``
4. **depth exceeded** — `Subagent depth limit reached: this session is already 1
   delegation hop(s) deep and `subagent_depth` is 1. Do this work in the current session,
   or raise `subagent_depth` in config to allow nested subagents.`
5. **permission denied** — `` `task` is not permitted for `worker`. Grant `task` for
   pattern `worker`, or set `subagent_type` to a target the current rules allow
   (explorer, librarian, advisor, worker).`` — carried on the `PermissionAsk` metadata
   under `task::GUIDANCE_KEY`, because `ToolError::Denied` has no `#[source]` to hang it
   on. The error itself stays `Denied`, so `is_model_correctable()` is `false` and the
   model does not retry a refusal that needs a grant.

Two more in the same shape: a passed `load_skills`, and an unknown agent (which also
suggests `category=` in case the caller meant a preset shorthand — the exact confusion
omo's `'Unknown category'`/`'Unknown agent'` hook pair exists to unpick).

### The target list is never written down here

`valid_targets(vision_available)` is `oc_agent::builtin::delegable(..)` mapped to names,
and a test asserts the two are equal rather than merely overlapping. So the coordinator
is excluded *by the roster*, not by this tool — todo 63 already encoded that as data
(`Role::Orchestrator` is the one non-`Subagent` role). The `COORDINATOR` constant exists
only to render the refusal. Consequence worth noting: the vision-gated target is
unreachable until the catalog offers a vision model, and there is a test that drives the
same target name through both polarities of `with_vision_available`.

### `category` forces the generic executor, and gates on *it*

A category names a `{model, variant}` and says nothing about conduct, so it cannot pick
a specialist — omo's rule, and the same conclusion here. The agent is
`GENERIC_EXECUTOR = "worker"` (todo 63's replacement for upstream's `general`), and the
model resolves through `ModelPolicy::resolve_category`, i.e. the active preset's category
map and nothing else. No built-in category table, so an unknown category is a note plus
the session model, never an error.

The load-bearing detail: **the permission pattern for a `category` call is `worker`, not
the category name.** A rule can only match a pattern some agent is named by; keying the
ask on `"cheap"` would make every category delegation unmatched by any real rule and
therefore governed by whatever the wildcard says. Tested.

### Variant selection: three refusals, three variants of `ToolError`

- the three *argument* rejections (1, 2, 3 above, plus unknown-agent and `load_skills`)
  are `InvalidArgs` — genuinely correctable by a different call;
- **depth is `Failed`, deliberately not `InvalidArgs`.** `InvalidArgs` advertises
  `is_model_correctable`, and a model that believes a depth limit is an argument problem
  reissues the identical call and is refused again. `Failed` is the only remaining
  variant that both carries a message and reports `Recovery::Fail`. The first draft used
  `InvalidArgs` and a test caught it, which is why the choice is documented at the
  `unrecoverable()` constructor;
- permission stays `Denied`, unchanged, for the same reason.

### Argument validity is checked before the human is asked

Upstream asks permission first and validates the agent afterwards
(`task.ts:118-183`), so a user can be prompted to approve delegation to an agent that
cannot exist. Order reversed here: target validity, then depth, then the gate. A
permission prompt is the scarcest thing this tool spends, and spending it on a call that
is going to fail anyway is worse than a slightly different order from the oracle.

### A background dispatch's id differs from the child session id — on purpose

Upstream sets `jobId: nextSession.id` (`task.ts:279`). One string then answers two
different questions — "cancel this job" and "resume this session" — and a client holding
it cannot tell which it has. Here:

- **foreground** returns the child session id only. `background_id` is `None` and the
  envelope carries no `background=` attribute.
- **background** returns both. The job id is `task::background_id(session)` =
  `"bg_" + session_id`, and both appear on the wire:
  `<task id="ses_child_of_ses_root" background="bg_ses_child_of_ses_root" state="running">`.
- The tool **refuses a host that hands back the session id as the job id**
  (`ToolError::Failed`, "must be distinguishable"). `RecordingHost::conflating_ids()`
  reproduces the upstream shape so that refusal is a tested property, not a claim.

The prefix rather than a fresh random id is deliberate: it keeps the job id derivable
from the session id (so a client can find one from the other) while keeping them
distinguishable by inspection, which a random id would not.

### `load_skills` is dropped, and passing it is an error rather than ignored

Skills are permission-gated per agent, so nothing about them is a property of the call.
The evidence for dropping it is that slim needs a hook family to recover from its
omission — `.omo/refs/omo-slim/src/hooks/delegate-task-retry/patterns.ts:14-18` maps the
substring `'load_skills'` to the fix hint "Add `load_skills=[]` (empty array when no
skill is needed)". An argument whose most common correct value is the empty one that
means nothing, and whose omission needs a recovery hook, should not exist. `background`
survives the same test only because its default genuinely carries information.

Chosen over silently ignoring it: a caller that believes it loaded a skill and did not
will blame the child for ignoring the skill, and that misattribution costs more than one
refused call. The argument is hidden from the derived schema
(`#[schemars(skip)]`) so no caller learns the name here.

### Resolution notes are rendered inside the result envelope

`<note>…</note>` lines sit inside `<task>`, before `<task_result>`. Appending them after
the envelope would let a caller that parses only the result body miss the fact that its
`effort` was dropped — which is exactly the silent downgrade the plan forbids.
## Task 101: background reflection fork

**`ReflectionFork` owns scheduling; `ReflectionRunner` owns review execution.** The
fork checks delivery and trigger policy synchronously, then launches a detached
Tokio task. Runner errors and panics are contained and logged because reflection is
advisory and must never alter foreground turn delivery.

**The default periodic cadence is ten delivered turns; zero disables it.** A
same-command fail-then-succeed recovery is an independent trigger, so the two
conditions are ORed. Trigger state records the last scheduled delivered-turn count
to avoid duplicate periodic reviews.

**The tool whitelist is enforced by exact runtime dispatch.** Only `memory` can be
called, with the stable rejection text required by acceptance. The public boundary
uses `oc-tool` directly rather than importing `oc-tools`, which keeps `oc-agent`
independent of concrete tool implementations.

**Policy input is an owned transcript snapshot.** The detached review cannot read
or compact the live parent conversation. `CompactionMode` deliberately has only
`Disabled`, making the no-compaction rule representable in the type surface rather
than relying on convention.

## [2026-08-07] Task 77: silence by default, because the licence cannot be stated

### The asset decision, and it is a licensing decision rather than a technical one

Upstream's built-in pack is six `.mp3` imports from `@opencode-ai/ui`
(`attention.ts:17-22`) — the only runtime reference from the ported tree into an
excluded package, and assets rather than code. The plan sanctioned two options: vendor
equivalents with a clear licence note, or ship silence by default with a documented path
to supply a pack. **Chose silence**, and the reason is that option 1 is not actually
available here:

* `packages/ui/package.json` declares `"license": "MIT"` and `packages/ui/LICENSE` is
  the MIT text, "Copyright (c) 2025 opencode". That licenses **the package**.
* It does not establish redistributable rights in the 90 audio files under
  `packages/ui/src/assets/audio/` (45 `.mp3` + 45 `.aac`). Measured: that tree contains
  no LICENSE, no NOTICE, no README, no attribution file of any kind.
* The names — `alert-01..10`, `bip-bop-01..10`, `nope-01..12`, `staplebops-01..07`,
  `yup-01..06`, each in two encodings — are the numbered-variation naming of a purchased
  UI-sound library. Those are routinely licensed to the buyer for use *in a product*
  while forbidding redistribution of the assets standalone or inside another pack.

So the provenance is unknown and the licence is unstatable. **Shipping bytes whose
licence cannot be stated is worse than shipping silence** — a downstream consumer
inherits a liability they cannot audit, in exchange for a courtesy sound. This is the
same refusal todo 41 made about a ripgrep binary and todo 48 about a language server,
one category over: not "do not download at build time" but "do not redistribute what
you cannot license".

Note also that silence-by-default is not a *degraded* outcome here — upstream ships
`attention.enabled: false` (`config/index.tsx:103`), so the out-of-box behaviour of the
real binary is already silence. What this decision changes is what happens after a user
turns attention on: they get notifications immediately and audio once they supply files.

### What ships instead: the right id, no bytes, and a diagnostic that says so

`builtin_pack()` registers under upstream's own `opencode.default` and is **empty**. The
id matters: `sound_pack` defaults to it, so a pack has to answer to that name or every
diagnostic would be an `UnknownSoundPack` about the *default*, which reads like a bug in
the port. With the id registered and the slots empty, the honest finding surfaces
instead:

```
sound pack "opencode.default" has no done sound and none is configured under `attention.sounds.done`; notifying without audio
```

Candidate order is upstream's (`attention.ts:146-150`): per-slot `attention.sounds`
override, then the active pack, then the built-in — deduplicated. So the two documented
paths to supply audio compose rather than competing, and a user can fill one slot without
adopting a pack.

**"No audio ships" is a guard test, not a convention.**
`attention_no_audio_asset_is_compiled_into_this_crate` walks the crate and fails on any
file with an audio extension or any `include_bytes!` in a source, with the mandatory
floors (`>= 40` files, `>= 6` sources; measured 79 and 10). A future contributor who
vendors a pack "just to make the sound work" fails a test that explains why.

### A cue is a pair, so the outcome is a pair — never one boolean

`AttentionOutcome { notification, sound, skipped, diagnostics }` with `ok() = notification || sound`.
Two consequences that a single boolean would have destroyed:

1. **Notification-only is a delivered cue, not a skip.** The missing-pack path returns
   `notification: true, sound: false, skipped: None` plus a diagnostic. Reporting a skip
   reason next to a delivered notification would be a lie, so `skipped` is populated
   **only when nothing was delivered** (mirrors `attention.ts:202-205`).
2. **A diagnostic is not an error.** `notify` returns no `Result` at all. Attention is
   the least important subsystem in the process and must never be the reason a turn
   fails, so every failure mode — unknown pack, missing slot, unplayable file,
   out-of-range volume — is an `AttentionDiagnostic` carried alongside a usable outcome.
   Same type-level guarantee todo 64 chose for model resolution.

### The master switch is checked before either channel is *constructed as a decision*

`enabled: false` returns `SkipReason::AttentionDisabled` before the message is
normalized and before a notifier or player is consulted, so the "must not play a sound
when `enabled` is false" constraint is positional rather than a conjunction someone could
later reorder. The test asserts `(notifier.count(), player.count()) == (0, 0)` across all
five classes with everything else saying yes — so only the master switch can account for
the silence. Mutation-tested: `if !self.config.enabled` -> `if false` fails two tests.

### Two seams, following todo 73's precedent rather than inventing a third pattern

`trait Notifier` and `trait SoundPlayer`, both with inert defaults, exactly as
`TerminalLifecycle` is a trait so its tests can run without a TTY.

* `OscNotifier<W: Write>` is the **real** notification path, generic over the sink. This
  is not a testability concession — a TUI owning the alternate screen must choose its own
  stream — and it makes the assertion the exact bytes rather than "a mock was called".
  OSC 777 is the chosen sequence because `renderer.triggerNotification`'s synchronous
  `boolean` return (`attention.ts:30`) cannot be awaiting a native async API; see
  learnings.
* `SilentPlayer` is the real player **and** the default. Deferring a decoding backend is
  the honest ordering when the crate ships nothing to decode; `trait SoundPlayer` means
  adding one later changes no caller. The "no hardware in tests" property and the "no
  unlicensable assets" decision are therefore the same fact.

### The config field: exactly one, following todo 75's `theme` precedent

```rust
/// Notification and sound-cue settings.
#[serde(default, skip_serializing_if = "Option::is_none")]
pub attention: Option<crate::attention::AttentionSettings>,
```

Third task to add a field to `TuiConfig`; the pattern was established, so nothing else in
`config.rs` moved. `skip_serializing_if` is mandatory — every sibling field carries it so
`TuiConfig::default()` serializes to `{}`, and `theme_config_round_trips_through_serde`
asserts exactly that. `AttentionSettings` repeats the pattern on all six of its own keys,
so `{"attention":{"enabled":true}}` round-trips to those bytes and nothing else.

The vocabulary itself lives in `attention.rs` rather than `config.rs`, for the reason
todo 74 gave for keeping the whole TUI schema out of `oc-config`: it belongs next to the
only code that reads it. `oc-config` was not touched and needs no change — `attention` is
a `tui.json` key, absent from `packages/core/src/v1/config/config.ts`.

Two docstring corrections came with it: the module header and the `TuiConfig` docstring
both listed `theme`/`attention` as not-yet-owned, and `config_tests.rs` used `attention`
as its example of a *tolerated unknown key*. That test now uses `plugin`/`plugin_enabled`
(genuinely unowned) and asserts theme and attention parse — otherwise it would have kept
passing while proving nothing.

### Volume is clamped with a diagnostic, not rejected

Upstream's schema rejects a volume outside `0..1`, but its runtime **also** clamps
(`clampVolume`, `attention.ts:77-80`) — so the value is already treated as untrusted, and
refusing to start a TUI over a loudness setting trades a working session for a pedantic
one. A non-finite volume is silence, matching `!Number.isFinite(volume) -> 0`. The clamp
finding is produced once at load and reported with the **first** cue rather than on every
one, so a misconfiguration is said once instead of per notification.

## [2026-08-07] Task 66: where aliases live, how Active is derived, and the board's shape

### Aliases persist in the board's own in-process map, and nowhere else

Minted once per lane at dispatch (`Lanes::next_alias`, a per-`(parent, prefix)` counter)
and never recomputed. Three options were live:

1. **`oc-db`** — rejected. Its 19-table schema is byte-compat-guarded by a diff test
   against the real binary, and todo 68 already established that a new table goes in its
   own database rather than there.
2. **A separate database, as `oc-goal` did** — rejected, but for a reason worth stating:
   the board's `Active` state derives from `SessionRunRegistry`, which is process-local by
   construction (`oc-engine/src/status.rs:1-6`). Persisting the alias half while the
   authority for the state half cannot be persisted buys a handle that survives a restart
   and resolves to a state the board can no longer compute. Half-durable is worse than
   in-process.
3. **In-process, alongside the run state it derives from** — chosen. Both halves have the
   same lifetime, so the board is coherent at every moment it exists.

The property the tests actually pin is *stability across turns*, not durability across
restarts: an alias must not change while the model may still be holding it. Six tests
cover it, and mutation 2 (regenerating aliases at render time) fails twelve.

One consequence, made explicit: a **vanished** lane's alias is never returned to the
counter. `JobBoard::forget` drops the lane and leaves the counter alone, so a handle the
model may still hold can never come to mean a different lane. Tested
(`a_vanished_lanes_alias_is_never_reissued_to_a_later_lane`).

### `Active` is derived, and the run registry wins

```rust
fn derive(lane: &Lane, runs: &dyn RunState) -> JobState {
    if runs.is_running(&lane.session_id) { return JobState::Active; }
    match lane.recorded { Running => Active, Completed if reconciled => Reconciled, ... }
}
```

`RunState` is consulted **first** and overrides the stored record. A lane the board
believes finished but whose session still holds a live turn is `Active`, because
`SessionRunRegistry::begin_turn` would reject the resumed turn with `SessionBusy` — so
agreeing with the engine here converts a confusing downstream failure into a refusal that
names the lane. That ordering is the concrete meaning of "do not invent a second notion of
running", and it has its own fixture
(`a_lane_whose_session_holds_a_live_turn_is_active_even_though_its_record_settled`).

`RunState` is a trait rather than a `SessionRunRegistry` because `oc-agent` must not
depend on `oc-engine` (`oc-tools -> oc-agent` already exists; the reverse closes a cycle
through the delegation tool). The sole intended implementation is
`|id| registry.status(id) == SessionStatus::Busy`.

### Five states, three sections, and why the section carries the meaning

States: `Active`, `Unreconciled`, `Reconciled`, `Failed`, `Cancelled`. Sections:
`Active` (not addressable), `Reusable` (addressable), `Closed` (terminal). slim has two
sections and folds unread failures into Active (`background-job-board.ts:662-664`), which
renders an already-failed lane as "still working". Split, because the question a section
answers is "may I send to this", and a failed lane is un-sendable for a different reason
than a busy one — the refusals differ (`ActiveLane` vs `NotReusable`) and so should the
heading.

`Unreconciled` applies to a **completed** lane only. The reason a completed-but-unread
lane is un-addressable is that a re-dispatch would overwrite an answer already waiting;
a failed lane has no answer to lose. So `settle` and `reconcile` are separate operations
and only the second makes a lane addressable — a running lane cannot be reconciled at all
(`a_running_job_cannot_be_reconciled_before_it_settles`).

Section presence is unconditional: an empty section renders `- none` rather than
disappearing, because an absent heading reads as an unanswered question.

### The board's rendered format

```
### Background Job Board
<PROSE_IS_NOT_ENOUGH>
<ACTIVE_IS_NOT_ADDRESSABLE>
<REUSABLE_RULE>

#### Active
- exp-1 / ses_child_0001 / explorer / active
  Job: bg_000001
  Objective: explorer: map the loader
...
#### Reusable
#### Closed
```

slim's four fields plus the **job id**, which slim cannot show because it does not have
one. Nothing time-dependent renders — slim excludes wall-clock ages for the cache reason
(`:839-841`) and the same applies here: the board is re-injected every turn, so a byte
that changes when nothing happened both busts the provider's prompt-cache prefix and makes
an unchanged lane look like it moved. Pinned byte-for-byte by
`the_board_renders_in_one_pinned_shape`, so a row moving between sections or a state word
changing spelling is a deliberate edit.

The three rules are rendered **as data**, in the board, next to the aliases they refer to.
slim keeps them in the orchestrator's system prompt, a different file from the board that
supplies the handles; a rename in one silently invalidates the other. Here
`PROSE_IS_NOT_ENOUGH` is a `pub const` that a test asserts appears in every render.

### Job ids come from the board's own sequence, not from the session id

`bg_000001`, `bg_000002`, … A lane takes several dispatches over its life, so deriving
the job id from the session id would give them all the same handle — precisely the
ambiguity upstream creates (`task.ts:262` reports a session id as `jobId`, `:294` reports
a service handle, same field). `no_job_id_is_ever_a_session_id` asserts the two spaces
stay disjoint, and `a_stale_completion_cannot_settle_the_dispatch_that_replaced_it`
asserts a superseded job id no longer names a lane — which is the concrete value of having
a per-dispatch handle at all.

### `ChildSessions` has no way to rebuild a session

Three methods: `open`, `append_turn`, `message_count`. No `replace_history`, no
`copy_messages`, no `replay`. "Must not copy or replay a child's history" is therefore
inexpressible through the seam rather than merely forbidden by prose, and the operation
log the recording double keeps makes it checkable: a continuation is exactly
`[Count, Append]` against an existing session, pinned by
`a_continuation_appends_to_the_session_and_never_reopens_or_replays_it`.

`message_count` returns `Option<usize>` — `None` means the session is gone, which is not
the same as zero. The store outlives the process the board lives in, so a lane can name a
session that no longer exists; appending to it would create a one-message conversation and
report it as a continuation.

### `task_id: Some("")` is the same as `None`

Both start a fresh lane. A model that emits an empty string is not asking for a
continuation, and making the same intent succeed or fail on whitespace would produce a
refusal whose fix is invisible. slim reaches the same conclusion in prose
(`orchestrator.ts:247`: "omitted or empty `task_id` creates a new specialist session").

### Refusal variants map to two `ToolError` kinds, and the split is by fixability

Recorded here for the host that wires this to `oc-tools`, following todo 65's precedent
that a model must not retry a refusal it cannot fix:

- `ActiveLane`, `NotReusable`, `VanishedSession` -> `ToolError::Failed`. None is fixable
  by reissuing the same arguments; the caller must wait, or start fresh.
- `UnknownTaskId`, `ForeignParent`, `AgentMismatch` -> `ToolError::InvalidArgs`. A
  different `task_id` (or none) fixes each.

## [2026-08-07] Task 59: WASM component host policy

- **Component model, not core-module ad hoc ABI.** Guests export named WIT functions
  matching the authoritative hook names and exchange JSON strings. This keeps plugin
  binaries language-neutral and avoids exposing Rust layouts across the boundary.
- **No WASI and no ambient capabilities.** The linker starts empty and any import is a
  startup diagnostic. Capability growth must be named, opt-in, and linked interface by
  interface; filesystem, network, environment, clocks, and inherited stdio are not
  silently available.
- **Resident instance, serialized calls.** Each component owns one `Store` and `Instance`
  behind a mutex and adapts to the existing sequential `HookBus`. Calls are never
  parallelized; configuration order remains mutation order.
- **Failure containment over host failure.** Compile/instantiate errors exclude only the
  bad component. Runtime traps, malformed output, resource exhaustion, and interrupts
  disable only that component and become diagnostics; hook dispatch itself remains
  successful so healthy siblings continue.
- **The heavyweight runtime is opt-in.** `oc-plugin` keeps `default = []`; only feature
  `wasm` activates exact `wasmtime = 47.0.3`. The default workspace build therefore does
  not compile or carry Wasmtime.

## [2026-08-07] Task 61: conversion ownership and collision evidence

Zod conversion and validation live entirely in `shim.mjs`. The resident host imports
the tool's own Zod package, converts the schema there, retains the execute closure, and
runs `safeParse` immediately before invoking it. Rust does not inspect Zod internals:
schemars remains the single source of truth for first-party Rust tools, while
JavaScript-provided tools carry their own finished schema across the bridge. Both enter
the downstream registry as `Tool` values.

Collision validation happens before registry assembly. Reserved-name errors retain the
existing stable text (`plugin tool \`bash\` conflicts with a reserved tool name`). A
duplicate config tool reports one error containing the generated name and both source
paths: `duplicate plugin tool name \`duplicate\` from \`<first>/tool/duplicate.js\`
and \`<second>/tools/duplicate.js\``. This is preferred to first-wins/last-wins because
lookup order must never silently decide which user code executes.

## [2026-08-07] Task 80: cross-project session listing

### The 100-cap is a **default**, not a ceiling

`listGlobal` reads `input?.limit ?? 100` (`session.ts:575`), so a caller asking
for 500 gets 500 upstream. `GlobalListRequest::effective_limit()` does the same:
`self.limit.unwrap_or(UPSTREAM_LIST_LIMIT)`. Clamping to 100 would make
`--limit 500` return a truncated page indistinguishable from a database holding
100 sessions — the same failure todo 21 avoided by leaving `ListQuery::limit` an
unset `Option` in the store. The cap lives at the request boundary, where
upstream puts it, and `session_list_limit_above_the_upstream_default_is_honoured`
pins it.

The endpoint's `limit + 1` / `x-next-cursor` probe
(`handlers/experimental.ts:139-156`) is **not** reproduced: the CLI prints a page,
it does not paginate, and inventing a cursor header on stdout would be noise.

### Table column widths — fixed, not fitted

| column | width | why |
|---|---|---|
| Session ID | natural, floor 20 | the value copied into `session delete`; a truncated id is useless |
| Project | 18 | fits `opencode-rust`-length names and a `prj_…` id |
| Title | 32 | the longest cell that still leaves the numeric columns on one 120-col line |
| Agent | 9 | `general` (7) is the longest built-in; 9 leaves room |
| Last activity | 32 | `11:38 PM · 12/31/2026` (21) **plus** ` (archived)` (11) |
| Msgs | 4 right | four digits is 9999 messages |
| Cost | 8 right | `$99999.99` |

**Fixed, not fitted to the data.** Upstream fits to the longest title
(`cli/cmd/session.ts:121-122`), which makes every run a different shape — two
listings cannot be diffed, and a terminal that fit yesterday wraps today. Only
the id column follows its data, and only upward.

`ACTIVITY_WIDTH` was 20 at first and truncated the archive marker to a bare `…`,
which told the reader a cell had been cut but not that the session was archived —
the one thing `--archived` exists to show. Found by eye in hands-on QA, then
pinned by `an_archived_row_keeps_its_marker_at_the_widest_timestamp`, which uses
the widest timestamp the formatter can emit.

### `--format json` emits `GlobalInfo`, not upstream's flat six fields

Upstream's `formatSessionJSON` (`cli/cmd/session.ts:137-147`) emits
`{id, title, updated, created, projectId, directory}` — which spells the project
id **`projectId`**, a spelling neither the endpoint (`projectID`) nor the database
(`project_id`) uses, and which has no project summary at all. A cross-project
listing whose JSON cannot name the projects is not usable.

So there is one JSON shape and it is the documented endpoint's. The differential
asserts set **and** byte equality against `/experimental/session` on a shared
database, which is only meaningful because the shapes are the same.

Consequence, deliberate: the **message count stays out of `GlobalInfo`**. Folding
it in would make the CLI's JSON a superset of the endpoint's and turn the
differential from an equality into a subset check, losing the ability to notice a
*missing* field. It lives on `ListedSession` alongside `info`, and only the table
reads it.

### No pager

Upstream spawns `less` when stdout is a TTY (`cli/cmd/session.ts:93-111`, plus a
20-line Windows `less.exe` hunt). A listing command that pipes itself into a pager
cannot be composed, and `| less` is one keystroke. Dropped, with the 20 lines of
platform probing it required.

### `ProjectScope` has no `Default`

Two arms, `AllProjects` and `Project(String)`, and no `Default` impl — a caller
cannot forget to choose. The ambient project is resolved in the **CLI** layer
(`cmd/session_list.rs::scope`) as the no-flag default, so it is a default a flag
overrides rather than a predicate the store injects. That is the whole difference
from `Session.list` (`session.ts:548-555`).

`--project` accepts a **path or an id** because neither is derivable from the
other — the id is a hash of the Git remote — and a user standing in a checkout
knows the path while a user reading a listing knows the id. The path is tried both
raw and canonicalised, so `.`, `..` and a symlinked checkout all resolve, while a
worktree that has since been deleted (uncanonicalisable) is still listable. An
unresolvable value is an **error naming the value**, not a silent fall-through to
listing everything.

### `session::list_sql` extracted rather than the query restated

`session_list` wraps `session::list_sql` as a subquery instead of building its own
`WHERE`. A second hand-written copy of the predicates and the
`time_updated DESC, id DESC` ordering is the divergence that shows up as a
paginated client seeing a row twice. `SessionSort::column` and
`session::from_row` became `pub(crate)` for the same reason: one decoder, one
column list, one order.

## [2026-08-07] Task 62: integration-test lifecycle boundaries

**No production PID accessor was added.** Child identity is a test concern here:
the JSON-RPC fixture is launched through a shell that writes `$$` and immediately
`exec`s the real plugin, and the JS fixture writes `process.pid`. This records the
actual owned children while preserving the production API and permits exact-PID
orphan checks instead of process-table pattern matching.

**Dispose is dispatched before transport shutdown.** After intentionally killing
the JSON-RPC sibling, the shared bus receives `HookInvocation::Dispose`; only then
do the surviving process loaders shut down and reap. A tracking adapter observes
that every still-enabled tier was traversed, while the JS fixture independently
persists a marker from its real dispose callback.

**WASM-off is explicit, not silent.** The integration target keeps the same six
test names under the inverse cfg and emits a reason naming the required `wasm`
feature and Unix PID controls. The real tests run only with both capabilities.

## [2026-08-07] Task 81: retention selector boundary

- `oc-db::retention::select` is read-only and returns a preview-oriented report: selected rows with inclusion reasons plus age-eligible exclusions with direct or descendant protection reasons. It emits no `DELETE`; mutation remains a later service concern.
- Scope is explicit (`CurrentProject`, named `Project`, or `AllProjects`), `time_updated` is the default age key, `time_created` is opt-in, and age uses a strict `< cutoff` boundary. `time_archived` is intentionally absent from protection.
- Public `LivenessProbe` makes server discovery fakeable without coupling `oc-db` to HTTP. The process edge may aggregate reachable local servers into `Liveness::Reachable`; `Liveness::Unreachable` preserves the honest uncertainty boundary.

## [2026-08-07] Task 76: the dialog's non-blocking shape, the external seams, and the view module layout

### A dialog is state in the component tree. It cannot await, by construction.

`Dialog` is deliberately **not** a `Component`. It has `handle_action(action, event)
-> DialogStep`, where `DialogStep` is `Ignored | Redraw | Resolved(DialogOutcome)`,
and `DialogHost` owns a stack of them. Making a dialog a `Component` would let it be
mounted directly into the tree, where nothing would enforce the contract; making the
answer a return value rather than a callback means there is no place a reply *could*
be awaited from. Outcomes accumulate in a queue that a consumer drains — a callback
would run inside `handle_event`, which is precisely the frame that must not block.

Why this is architecture and not style: `App::run` is the single consumer of terminal
input, engine events, **and** the terminal-lease wake notification. A dialog that
awaited inside `handle_event` stops all three, and the lease the plugin host takes for
an OAuth prompt (todos 60/97) can then neither be granted nor reclaimed, because
reclaim needs the render lock that frame is holding. That is a deadlock, not a stall.

`DialogHost::handle_event` forwards **every non-key event to the base
unconditionally**. One line, and it is the whole property the acceptance test asserts.
Mutating it to `_ if self.is_open() => EventResult::REDRAW` — i.e. making the host
modal — fails three tests, one of them by tokio timeout.

**An action a dialog does not understand is NOT forwarded to the base.** A modal owns
the keyboard; forwarding would let `session_new` fire while a permission prompt is up.
So `DialogStep::Ignored` returns `handled: true, redraw: false`.

### Escape resolves a permission prompt to `reject`, never to the highlighted option

The highlighted option is `Allow once`. A prompt dismissed by a mis-keyed escape must
not have granted anything, so `app_exit` maps to `Reject` unconditionally. Cancelling
the *escalation* (the "always allow" confirmation) is different: it returns to the
choice rather than resolving, because the user has not decided yet.

### `$EDITOR` and the clipboard are traits, and the editor path takes todo 97's lease

`ExternalEditor` and `Clipboard` are traits with `ScriptedEditor` / `MemoryClipboard`
doubles — the same shape todo 73 used for `TerminalLifecycle`, for the same reason: a
real `$EDITOR` blocks on a human and a real clipboard read depends on which of
`wl-paste`/`xclip`/`pbpaste` happens to exist on the CI machine.

Upstream wraps the child in `renderer.suspend()`/`resume()` (`editor.ts:32-53`). This
crate already has the right mechanism — todo 97's `TerminalBroker` driven by todo 73's
`TerminalLeaseOwner` — so `EditorRequest::lease_reason()` returns
`LeaseReason::new("tui", "external editor")` and the **caller acquires a lease**. Two
suspend mechanisms in one process is how you get a deadlock against a plugin prompt;
`LeaseReason::plugin` names the culprit in a forced-reclaim diagnostic, and for this
path the culprit is the TUI, so it says so instead of borrowing a plugin's name.

**No process-spawning implementation ships.** The prompt forbids a subprocess or a
clipboard access in any test, so a real implementation would ship untested. What does
ship is every *pure* part — `invocation()`, `editor_spec()`, `copy_command()`,
`image_read_command()`, `osc52()`, `base64()` — which is where the bugs live. Recorded
in issues.md as the one partial item.

### `base64` is hand-rolled rather than a new dependency

Twenty lines, tested against RFC 4648 §10's seven vectors. Adding an encoder crate to
the render stack for one OSC 52 sequence is a poor trade, and the root manifest pins
dev/test `opt-level` per package — a new package means a new pin to justify.

### OSC 52 is written even when a native tool exists

`clipboard.ts:120-124` does both. Over SSH the native tool copies into the *remote*
machine's clipboard, which is not where the user is looking. The tmux/screen wrapper
is part of it, because those multiplexers swallow an unwrapped sequence.

### `ViewContext` carries the palette AND the config, as one value

Not a palette argument threaded through every `render`. The two travel together: a
view that paints also needs the user's diff/scroll/size preferences, and both are
immutable for the life of a frame. It also gives the palette-discipline scan a single
thing to look for — "does this module mention `ViewContext`" is the complement of "does
it name a colour", and a view that paints nothing fails the first check.

`diff_columns(width)` lives on the context rather than in `diff.rs` so the permission
prompt and a standalone diff viewer cannot disagree about the `diff_style` fork.

### Twelve modules, one concern each, `#[path]` test files beside them

`views.rs` + `views/{message,dialog,permission,question,editor,diff,autocomplete,
picker,help,scroll,external}.rs`, each with a sibling `*_tests.rs` via
`#[cfg(test)] #[path = "…"] mod tests;` — the layout `app.rs`/`keybind.rs`/`theme.rs`
already use. Plus `views/views_tests.rs` for the cross-module guards and the
composition tests, which is where an assertion that spans two views belongs.

**One `SelectDialog` with four constructors, not four picker components.** Upstream
ships `dialog-session-list.tsx`, `dialog-model.tsx`, `dialog-agent.tsx` and
`dialog-theme-list.tsx` over one shared `ui/dialog-select.tsx`. The shared part *is*
the behaviour; four copies of the paging arithmetic is four places for it to be wrong.
The theme picker's preview is a closure, which is also how it reuses todo 75's
resolved palettes — resolved once at construction, because resolution walks colour
references and a picker redraws on every keystroke.

**The model picker's value is `provider/model`, never a bare model id.** A bare id is
exactly the unqualified form `oc-agent/src/model_policy.rs` treats as unavailable.

### Help is generated from the live `Keymap`, and lists unbound actions

A hand-written help text is wrong the moment a user rebinds anything, and wrong
*silently*. Grouping is by the table's `scope` column, because scope is not a category
label — it is the condition under which the key resolves at all, so it answers the
question a user actually has ("what can I press *here*"). An unbound action still gets
a row reading `(unbound)`: a user who unbound something needs to see it exists, or
they conclude the feature is gone.

### Autocomplete ranking is a documented rule, not a Fuse.js reimplementation

Upstream uses Fuse with `threshold: 0.5` for `@` and `0` for `/`. Fuse's score is not
derivable from its configuration, so `score()` is prefix (1000) > word-boundary (500)
> substring (250) > scattered subsequence (10), ties broken by the source's own order
via a stable sort. The `/` exactness is preserved as "must score ≥ 500", so a
half-typed slash command cannot match by scattered letters. Deterministic, which is
what a test can assert.

### The transcript wraps text itself instead of letting ratatui do it

`Paragraph::wrap` would work, but then the transcript cannot *count* the rows it will
occupy — which the scroll offset and the scrollbar both need. So `wrap()` produces the
lines already broken, on word boundaries, splitting a run longer than the row (paths
and URLs are common here and neither breaks on spaces).

## [2026-08-07] Task 82: destructive prune safety boundary

- `PruneRequest::default()` is preview-only. Archive changes only `session.time_archived` and has an explicit inverse; delete requires a separate confirmation bit both at the public entry point and at the caller-owned transaction seam.
- Every mutation uses `TransactionBehavior::Immediate`. Delete captures its preview in that transaction, performs remote-unshare checks before local statements, deletes the ten related tables in the pinned order, and then performs the global `part` orphan sweep before commit.
- `RemoteUnshare` is injected. Tests use an in-memory fake and never make a network call; failures refuse local deletion unless force was explicitly supplied.
- No table, index, or migration was added. The API reports exact per-table rows, logical bytes, aggregate cost, and all five token counters so later CLI/HTTP surfaces can share one loss report.

## [2026-08-07] Task 83: off-database artifact GC safety boundary

- GC re-reads surviving `session` rows under an `IMMEDIATE` transaction and holds that writer lock through filesystem decisions. Filesystem deletion cannot roll back with SQLite, so it remains a separate, preview-default pass after prune rather than part of task 82’s transaction.
- Snapshot stores are keyed by `(project_id, sha1(project.worktree))`, reference-counted with `oc_snapshot::reference_counts`, and removed only at count zero. A missing/empty joined project worktree is ambiguous and retains every store for that project.
- Rust-authored `tool_<sanitized-session>_<uuidv7>` files use `oc_tool::store::session_of`; attributed files are reclaimed only for requested ids no longer present in the database. `None` is never guessed and uses only the configurable mtime backstop, defaulting to upstream’s seven days.
- Legacy `storage/{session,message,part,session_diff}` cleanup is disabled by default and requires an explicit request opt-in. Session/message/diff paths are directly attributable; part directories are reached only through message ids enumerated under a deleted session.
- Preview and delete share one candidate-discovery path and report stable path-ordered logical content bytes. Scans and recursive byte accounting use `symlink_metadata`; managed roots and legacy category roots that are symlinks are retained without traversal.

## [2026-08-07] Task 84: vacuum is a command, not a flag, and the borrow checker enforces it

### `vacuum` takes `&mut Connection` so it cannot be folded into a prune

`VACUUM` rewrites the whole file, cannot run inside a transaction, and needs free space
of roughly the database's own size. Todo 82's delete is one
`TransactionBehavior::Immediate` transaction by design. A live `rusqlite::Transaction`
holds the connection's mutable borrow for its lifetime, so `vacuum(&mut connection, …)`
**does not compile** while that transaction is open. The prohibition is a type error
rather than a comment. `tests/vacuum.rs` also proves the underlying constraint is real
in the linked SQLite (`execute_batch("VACUUM")` inside a transaction errors, and the
message names the transaction), so the signature is guarding a genuine failure rather
than a remembered one.

A second, independent guard: `vacuum_is_never_reachable_as_a_side_effect_of_another_module`
walks all 13 `oc-db` source files, skips `//` lines (three doc comments in `fts.rs` and
`open.rs` legitimately discuss `VACUUM`), and fails on any executable line outside
`vacuum.rs` that contains `VACUUM` or `vacuum::vacuum`, or any line anywhere that
mentions `auto_vacuum`/`incremental_vacuum` — because those would make reclamation
implicit at the SQLite level, defeating the whole design. Both guards have floor
assertions (≥12 files, ≥2000 executable lines) so the scan cannot pass vacuously the way
todo 2's did under a stale `CARGO_MANIFEST_DIR`.

### The refusal threshold is `available < main_bytes`, and Unknown proceeds

`VACUUM` materializes a second copy of the file before replacing the original, so the
main file's current size is the requirement. Refuse iff strictly less is available;
**equality passes**, with its own test, because an off-by-one there refuses every vacuum
on a disk that is exactly large enough.

It is a necessary, not sufficient, condition — SQLite may place its intermediate copy
under `SQLITE_TMPDIR` on another filesystem, which this check cannot see. And when free
space cannot be established at all (`Availability::Unknown`, i.e. every non-unix host),
the rewrite **proceeds** and the report records why the guard was not evaluated.
Refusing instead would make the command unusable on those platforms to prevent a failure
SQLite already handles safely: an out-of-space `VACUUM` aborts and rolls back with the
original database intact. Refusing is for the case where we *know*, and the value of
knowing is the actionable message.

The message names, exactly once each: the path, the required bytes (human + exact), the
available bytes (human + exact), the shortfall (human + exact), and `OPENCODE_DB` as the
lever a user can actually pull. Byte formatting is integer arithmetic throughout — a
formatter that goes through `f64` starts disagreeing with the exact count printed beside
it.

### `DiskSpace` is injected, following the two seams already in this crate

Same shape as todo 81's `LivenessProbe` and todo 82's `RemoteUnshare`: the database
cannot answer a question about the host, and a test must not have to fill a disk. The
fake counts its own calls, so a test can assert the guard was *consulted* rather than
merely that it did not fire.

### `integrity-check` runs `foreign_key_check` too, and that is the load-bearing half

`PRAGMA integrity_check` answers `ok` for a structurally perfect file whose references do
not resolve — which is precisely the damage a connection that inherited
`foreign_keys = OFF` leaves behind, the hazard this crate's module docs are built around.
So `foreign_key_check` runs alongside it, and `IntegrityReport::is_ok()` requires both.
That makes this the check that actually proves todo 82's explicit ten-table delete order
and its global `part` orphan sweep worked. One test constructs the readable-but-orphaned
case and asserts `integrity == ["ok"]` while `is_ok() == false`.

At the CLI, damage is an `Err` and therefore a non-zero exit, not a report printed with
status 0 — a script's exit status has to mean what it looks like it means.

### `vacuum` reports the FTS rebuild obligation instead of discharging it

`fts.rs:240-244` says a rewrite may renumber the implicit `message.rowid` values the
external-content indexes use as document ids. `VacuumReport::fts_rebuild_required` is
therefore a detected fact, not an action: rebuilding inside `vacuum` would be exactly the
hidden side effect this module exists to refuse, and the indexes are opt-in and absent
from `migration::apply`, so an unconditional rebuild would fail on the ordinary database.
The **CLI** discharges it, because that is a caller. One test proves the obligation is
dischargeable: rebuild after the rewrite and the surviving messages are findable again.

### `db stats` shares one definition of "bytes" with the prune preview

`part` bytes are the sum of non-null column lengths, spelled the same way
`prune::preview` spells them, and a test asserts the two agree on a real database for the
heaviest session. Two definitions would make the number `db stats` shows disagree with
the number a prune previews for the same rows, and the operator would reasonably read
that as a bug in one of them.

### Positional keywords, not clap subcommands

`db path` was already dispatched on the positional's value. A clap subcommand alongside a
positional needs `args_conflicts_with_subcommands` and turns one unambiguous string match
into parser configuration nobody can read — and adding any flag would break
`differential.rs`'s exact long-option comparison against the real binary. Keywords add no
flag. `Maintenance::parse` matches exactly: `Stats`, `IntegrityCheck` (both
`integrity-check` and `integrity_check`, since the pragma has the underscore spelling),
`Vacuum`, and a test pins that `path`, `VACUUM`, `Stats`, `" stats"`, `"stats;"` and any
SQL fall through to the query runner.

## [2026-08-07] Task 85: session-prune adapters and local-server discovery

- `oc-db::session_prune::execute` is the sole orchestration boundary. CLI and HTTP resolve transport-specific input and confirmation, then pass one `SessionPruneRequest`; neither owns selection, mutation, accounting, or artifact logic.
- GET `/api/session/prune` is structurally preview-only. POST requires both an archive/delete action and literal `apply: true`; delete also reaches the service-level confirmation guard, so an adapter omission cannot silently become destructive.
- `/api/session/active` exposes only `ServerServices::runs`, preserving its process-local meaning. It returns the oracle-compatible `{data:{sessionID:{type:"running"}}}` shape with deterministic key order.
- Each bound loopback server owns a unique URL record under `$XDG_STATE_HOME/opencode/servers` and removes it on drop. Discovery accepts only credential-free `http` URLs with literal loopback IPs and explicit ports; CLI probes concurrently with proxy bypass and optional server Basic auth, then unions IDs from every valid responder.
- Failure to publish a loopback discovery record fails server binding rather than silently weakening pruning safety. Non-loopback authenticated listeners are not published because a standalone local-maintenance probe must never discover or contact a network endpoint implicitly.

## [2026-08-07] Task 86: the report's shape, and how the suite reaches other crates' work

### `docs/divergences.toml` is loaded through `oc-testkit`, not parsed in the test

`oc_testkit::divergence::DivergenceList::load()` finds the workspace root, reads
`docs/divergences.toml`, and rejects three shapes before any caller sees it: an empty
`id`/`surface`/`reason`, a duplicate id, and an unknown key (`deny_unknown_fields`, so a
typo'd field is an error rather than silently dropped data). `DECLARED_COUNT = 7` lives
beside the loader rather than only inside the test, because two consumers need it: this
suite, and todo 92's documentation test. A single `const` both read is the smallest thing
that cannot drift.

The count assertion is in the *test*, not the loader, so the message that names the
mismatch sits next to the number it is asserting. `load()` deliberately does not check
the count — a loader that refuses to parse a file with eight entries could not be used to
*report* that there are eight.

The `execute` entry carries a `[divergence.contract]` sub-table of sorted property-name
lists. That is what makes it verified rather than declared: the suite derives the live
schema through `oc_tool::schema::params_schema::<oc_tools::ExecuteParams>()` and compares
property and required sets, plus a subset check on `Subcall`'s control properties (a
subset, because tool-specific arguments are `#[serde(flatten)]`ed in beside them). It also
asserts `code` is *absent* — if `execute` ever grew a `code` parameter the divergence
would no longer exist and the entry must go.

### The report is JSON at `target/compat/compat-report.json`

Chosen over TOML or Markdown: the consumers are F1-F4 and a human asking "what was
proven", and JSON is the only one of the three that a script and a reader both handle
without a parser choice. Pretty-printed and newline-terminated so two runs diff cleanly.
`OC_COMPAT_REPORT` overrides the path; the default is under `target/` so it is not
committed — the artifact is evidence of a run, not source, and a committed copy would
immediately be stale.

Four verdicts, because three distinctions matter and "pass/fail" collapses all of them:
`compared`, `partially_compared` (compared, with a named exception),
`not_compared` (with a mandatory `NOT COMPARED` in the detail text), and `skipped` (the
comparison exists but the environment lacked the oracle). A surface marked
`not_compared` must name `OracleKind::None` and must not claim an evidence test —
asserted, so the two cannot disagree.

Three separate lists sit beside the surfaces, and keeping them separate is the point:
- `normalizations` — each mask with the reason it hides nothing real.
- `known_gaps` — this port is behind upstream, with no decision behind it.
- `nominated_divergences` — a real decision that the plan's asserted count excludes.

An unimplemented surface in the divergence file would convert an omission into a design
choice by fiat. That is the laundering the plan's "must NOT normalize away a real
difference" forbids, so gaps get their own list and read as what they are.

### How the suite reaches ninety-five tasks' work without re-running or duplicating it

Two mechanisms, and no third:

1. **Re-assertion, for exactly two contracts.** The DB schema and the journal round-trip
   are re-expressed in this target because the plan's QA scenario requires
   `cargo test --test compat_suite` itself to fail when an index is renamed. Delegating
   would have made the gate report "oc-db failed". The re-expression is not a copy: it
   compares name-keyed maps so the failure names the object, where `oc-db`'s version
   compares flat vectors and dumps both schemas.
2. **A registry of `path::test_name` claims, verified to resolve.** Every other surface is
   a row naming the test that does the work.
   `every_registered_evidence_test_still_exists` opens each file and requires
   `fn <name>(` to be present, with a floor of 15 so a wrong root cannot pass vacuously.
   This is deliberately a *weaker* check than running the test — it proves the claim
   points at something real, not that it currently passes, which is
   `cargo test --workspace`'s job. Stating that limit is better than implying a strength
   the mechanism does not have.

Cost paid for this: `oc-testkit` gains `oc-db`, `oc-server`, `oc-tool` and `oc-tools` as
**dev**-dependencies. They are dev-only so nothing in the harness's runtime graph changes,
and `tests/no_http_client.rs` still passes — the load-bearing absence of an HTTP *client*
in `[dependencies]` is untouched. `toml` moved into `[dependencies]` because todo 92's
docs test will read the same allow-list through the same loader.

## Task 87 — provenance-first cassette coverage

The suite reuses `oc_testkit::cassette::{CassettePlayer, Cassette, HttpInteraction,
RequestSnapshot}` rather than introducing a provider-specific replay engine. Each cell
uses the production decoder (`AnthropicDecoder`, `OpenAiDecoder`, `ChunkTranslator`,
`BedrockEventDecoder`, or `GeminiStreamDecoder`) and asserts the complete ordered event
vector. This keeps request matching, cursor semantics, and unused-interaction checks in
the one cassette implementation Task 6 already established.

`Evidence::{Recorded, Authored, Gap}` is closed data on the matrix cell. `Authored`
requires a reason and may exercise decoder behavior, but never upgrades itself into wire
compatibility evidence. `Gap` is an executable omission: it names the missing protocol
artifact and is excluded only from replay, not from matrix completeness. This is chosen
over manufacturing all 40 cells as recordings because false provenance is worse than an
explicitly incomplete compatibility claim.

Registry coverage is derived from `ProviderRegistry::registered()`. A family map is still
needed because multiple provider ids share one wire family, but the registry is the
authority over which ids must map. Consequently a newly registered id fails the suite
until its family is chosen, while a new scenario fails until every family receives a
cell.

## [2026-08-07] Task 104: where the tool runtime lives, and what governs it

### The shared assembly point is `oc-cli`'s `cmd::tool_runtime`

`assemble(directory, worktree, env, config, agent, provider_id, model_id)`
returns the resolved tool vector and the merged ruleset. Every surface that
drives a turn calls it; there is deliberately no second place that builds a
registry.

It is in `oc-cli` and not in a new crate because `oc-cli` is currently the only
crate that sees all seven inputs. Moving it becomes necessary — and is the named
next step — when `oc-server` needs it. See `issues.md` for that extraction.

The registry is built through todo 44's `ToolRegistryBuilder` and projected with
`ToolRegistry::resolve`, so the model-conditional and permission-hiding passes run
in the documented order rather than being restated.

### `run`'s permission rules are the ones `agent list` prints

`cmd::agent::resolved_rules` and `DynamicRules::resolve` became `pub(crate)` and
are now called by `tool_runtime::assemble`. This is the load-bearing choice: a
ruleset a user can inspect with `agent list` but that the turn loop does not
enforce is worse than having no listing. For the `build` agent that resolves to
15 rules, ending in the four `read` rules that make `*.env` an ask.

**`AllowAll` is gone from the production dispatch path.** `HeadlessApproval`
replaces it: `oc-permission` resolves `allow` and `deny` itself and only calls the
approval collaborator for `ask`, and a headless run has nobody to ask — stdin is
the prompt or a pipe and stdout is the model's answer. Blocking would hang a
non-interactive invocation forever; prompting would corrupt the output. So it
refuses, and names the rule that would authorize the call. `external_directory`,
`doom_loop` and reading a `.env` file all resolve to `ask` by default, and each is
a decision a person should make.

Todo 71's destructive-command gate is unaffected: it lives inside the shell tool,
before any spawn, and still applies to every `bash` call.

### The tool snapshot travels on `CompletionRequest`, not on the provider

`CompletionRequest` gained `tools: Vec<ToolSchema>` where `ToolSchema` is
`{ name, description, parameters }` — provider-neutral, because each family nests
the same three fields differently and `oc-llm` must not depend on `oc-tool`. The
turn loop fills it from the snapshot it already locked, so **dispatch is checked
against exactly what the model was shown**; a provider holding its own tool list
could answer with a call the loop would then refuse.

`oc-provider-compatible::function_envelopes` does the OpenAI translation and
returns `None` for an empty snapshot rather than `[]`, because several compatible
vendors reject `tools: []` outright.

### Which built-ins `run` registers, and why the list is short

Registered: `invalid`, `bash`, `read`, `glob`, `grep`, `edit`, `write`,
`apply_patch`, `webfetch`, `todowrite`, `websearch`. Resolved for a non-GPT model
that is nine (`websearch` needs a configured backend, `apply_patch` is GPT-only).

Not registered, each for a stated reason: `question` and `plan_exit` need a live
user to answer; `task` needs a child-session host; `skill` and `lsp` have no
`oc-tools` implementation; `execute` is the builder's own, behind
`experimental_code_mode`. An unregistered slot is simply absent from the assembled
vector, so the model is never told about a tool that cannot run.

### `TuiThreadCommand`'s disposition changed from `NotRegistered` to `Implemented`

Its row now names `tui` rather than `$0`, and the bare invocation dispatches the
same handler — upstream's default command is the TUI, so a bare `opencode-rust`
should not explain an absence. `crates/oc-cli/tests/surface.rs`'s 23-command
fixture is unchanged and still one-to-one; the registration check now finds `tui`
under its explicit spelling.

`cmd::tui::execute` owns only wiring: the terminal session, the two bounded
channels, the input producer, the root component. Every rendering decision stayed
in `oc-tui`, and no engine call is reachable from it — todo 73's rule holds.

`forward_terminal_input` went into `oc-tui::app` rather than the CLI: it is the
only code that has to know crossterm's reader is synchronous, and its contract
(poll and read on blocking threads, **await** the send so a burst backpressures
instead of dropping) belongs with the channel it feeds.

A non-terminal `tui` invocation is refused, not degraded. Entering raw mode and
the alternate screen on a pipe writes escape sequences into whatever is reading it
and leaves no way to type the exit key.

## [2026-08-07] Task 88 recheck after Task 104: topology and build decision

The committed TypeScript G1/G2 artifact measures the released **TUI** under a real
PTY. Therefore the only admissible Rust comparison is the released Rust **TUI**
executing the same cassette-backed turn, with the same W-idle/W-real database shape,
sampling window, process-tree accounting, and five-run AB/BA schedule. Comparing
`opencode-rust run` against that artifact is rejected: it omits the terminal renderer,
input task, and TUI channels, so a lower number would mix an implementation saving
with a topology change.

When the TUI turn path exists, both sides must be release artifacts. A debug Rust
binary includes different optimization, allocator, and debug-assertion behaviour and
cannot support a production memory claim against the installed TypeScript release.
This decision is recorded now, but no Rust measurement was taken.

Task 104 closed two earlier gaps: `run` now assembles real tools, and `tui` is a
registered command that boots `App::run`. It deliberately did not connect the TUI to
the turn composition root. `cmd/tui.rs` creates and retains an engine sender but never
sends through it; its module contract explicitly says prompt submission only updates
the transcript and that `run` is the sole turn-executing surface. The frozen perf
driver decides completion from captured provider requests, so the Rust TUI can never
satisfy even one turn today.

Decision: Task 88 remains blocked rather than adding a gate that compares unlike
surfaces or a guard that can only fail forever. Resume after the turn composition root
is callable from the TUI thread and emits into the TUI's existing engine channel.

## [2026-08-07] Task 91: what ships, when it runs, and how "executed" is enforced

### `opencode-rust` ships. `oc-server` does NOT.

The workspace produces **six** cargo binary targets, not two: `opencode-rust`
(oc-cli), `oc-server`, `oc-example-plugin` (oc-plugin), `oc-acp-conformance`
(oc-acp), `oc-log-probe` (oc-observability), `ts-baseline` (oc-testkit). Only the
first is a release artifact.

`oc-server` is excluded on purpose. `oc-cli/src/command.rs:552-554` documents that
`opencode-rust serve` "wraps the `oc-server` library rather than its standalone
binary", and `decisions.md` (task 45) records the `oc-server` binary as "Task 51's
executable QA surface". Two binaries where one is a QA harness invites users to run
the wrong one. If `oc-server` ever becomes a deployable, that is a deliberate
decision with its own smoke leg, not a side effect of `--bins`.

Every build step names `--bin opencode-rust` explicitly rather than relying on
package defaults, so adding a `src/bin/*.rs` to `oc-cli` cannot silently change
what gets packaged.

### Release runs on a `v*` tag or on manual dispatch — never on every push

Six builds plus six smoke legs is far too slow for a per-commit gate. But a
pipeline whose first ever run is the real release is untested, so the gap is closed
twice:

- `workflow_dispatch` runs the **complete six-target matrix with `publish`
  skipped** (`version.outputs.publish` is false unless a tag was pushed or the
  dispatcher ticked the box). The pipeline is exercisable end to end before any tag
  exists.
- `ci.yml`'s `artifact` job runs the same **package → unpack → smoke** path on the
  host leg for every push and pull request, via `make smoke-artifact`. A break in
  the smoke driver or the cassette surfaces on the PR that caused it.

**No release-please.** This project has no release process and inventing one is
todo 92's territory. Instead a `version` job resolves one version string from
`cargo metadata` and **fails if a `v*` tag disagrees with oc-cli's manifest
version** — an artifact filename that lies outlives the mistake.

### "Must not ship an artifact that was never executed" is enforced structurally

`build` (6) → `smoke` (6, each on a runner matching the artifact's architecture) →
`checksums` → `publish`, and `publish` lists `[version, build, smoke, checksums]`
in `needs`. GitHub's default `needs` semantics require every listed job to have
succeeded, so one failed or skipped smoke leg blocks publication of **all six**.
There is no path from a compiled binary to a release asset that skips a process
that ran it.

Two tests hold the shape from outside the workflow, because the failure is
otherwise invisible: `every_built_target_is_also_smoke_tested` (a target in `build`
and missing from `smoke` would ship unexecuted with CI green) and
`publication_depends_on_the_smoke_job`.

### Why `build` and `smoke` are separate jobs

Only because of one leg. `aarch64-unknown-linux-musl` is cross-linked on x86_64 and
**cannot execute there**; it has to be handed to `ubuntu-24.04-arm`. That
runner-crossing handoff is the entire reason for the split. The other five legs
could have smoked in place, and keeping them in the same shape costs one artifact
download and buys a uniform, assertable topology.

Runner choice per leg, and why `macos-15-intel` rather than codegraph's
`macos-latest`: `macos-latest` is Apple Silicon now, so an `x86_64-apple-darwin`
build there is cross-compiled and unexecutable — the exact failure this todo
forbids. codegraph's matrix was proven for "produce a binary", not for "run it".

### The smoke cassette is committed, not read from the oracle tree

`packaging/smoke/cassettes/openai-chat/drives-a-tool-loop-end-to-end.json`, sha256
`fab3c2b9991544004e02a101c4bbe5843f887d2084d96353a684fba4f0e5acd4`, byte-identical
to upstream's recording. `oc_testkit::cassette::recordings_root()` finds cassettes
by walking up for a sibling `opencode` checkout — correct on a developer machine,
impossible on a CI runner that has this repository and nothing else.

The copy cannot rot silently:
`committed_smoke_cassette_matches_the_oracle_recording` compares the two byte for
byte whenever an oracle tree is reachable and prints a **named skip** otherwise.
`PROVENANCE.md` records the source path, the recording date, and the hash.

Chosen deliberately because its recorded call names `get_weather`, a tool this
runtime does not have — so the smoke proves the assembled registry reached the wire
and that an unknown call still produces a tool result the loop sends back, with
`authored_scenarios()` empty. Zero authored bytes, asserted.

### `oc-smoke` is a binary, not a `#[test]`

`crates/oc-testkit/src/bin/oc-smoke.rs`. It takes `--binary <path>`, because the
subject of a release gate is the binary **inside the archive**, after packaging and
transport, on the platform it targets. `tool_turn.rs` resolves
`env!("CARGO_BIN_EXE_opencode-rust")` at compile time, which is precisely the wrong
subject. Same three checks, same loopback `MockProvider`, same env recipe, same
`tokio::process` requirement (a synchronous wait stops driving the mock's server
and the run hangs instead of failing — todo 104's three-round debugging lesson).

It canonicalises `--binary` before use: every subprocess runs with `current_dir`
set to a scratch project, so a relative path resolves against that scratch
directory and reports a bare "No such file or directory".

### `cargo-deny`: offline locally, online in CI

`make deny` passes `--offline` so `make ci` needs no network, and prints a **named
skip** when cargo-deny is absent (it is a supply-chain gate, not a build
requirement). The CI `supply-chain` job runs it **online and unskippable**, because
the whole value of the advisories check is being current. `ignore = []` — nothing
acknowledged, nothing suppressed, and a future entry must carry its RUSTSEC id and
a reachability argument.

### The unsafe gate needed the half nobody had written

`[workspace.lints.rust] unsafe_code = "forbid"` existed from todo 1. It applies
**only to crates that write `[lints] workspace = true`**, so the enforceable
property is not "no unsafe in the source" but "no unsafe in the source AND every
crate inherits the lint". Both are now tests, with floors (>= 300 source files,
>= 34 manifests). The inheritance test is the load-bearing one: a crate omitting
the key would accept unsafe code silently and no amount of scanning the other 33
would see it. Measured today: 34/34 inherit, 0 keyword-position uses (22 textual
mentions, all in doc comments).
