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
