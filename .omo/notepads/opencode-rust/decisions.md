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
