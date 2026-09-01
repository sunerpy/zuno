# Contributing to Zuno

Thanks for your interest. This page covers what a change needs before it can be
merged. Design rules that govern the runtime itself live in
[`AGENTS.md`](./AGENTS.md) and in the architecture pages under [`docs/`](./docs/README.md).

## Contents

- [Getting set up](#getting-set-up)
- [The gates](#the-gates)
- [Commit messages](#commit-messages)
- [Pull requests](#pull-requests)
- [What a change owes](#what-a-change-owes)
- [Reporting a bug](#reporting-a-bug)
- [Security](#security)

## Getting set up

Zuno builds with the toolchain pinned in
[`rust-toolchain.toml`](./rust-toolchain.toml). Bumping that pin is a deliberate
change, not routine maintenance: the workspace sets `clippy::all = "warn"` and CI
runs `-D warnings`, so any lint a new toolchain adds becomes a build failure.

A C compiler is required. SQLite is compiled from the bundled amalgamation and the
TLS stack builds `aws-lc-sys`, so this is expected rather than incidental.

```sh
git clone https://github.com/sunerpy/zuno.git
cd zuno
make build          # debug build, staged at dist/zuno
./dist/zuno --version --long
make hooks          # pre-commit formatting, pre-push fast tests
```

`scripts/preflight.sh` reports whether the optional tooling the gates use — oxfmt,
cargo-deny, and the rest — is present.

Gates run with `--offline` by default because this project's registry is often a
mirror. Set `OFFLINE=` to permit a fetch:

```sh
make test              # offline
make test OFFLINE=     # allowed to fetch
```

## The gates

`make ci` is the host-side source gate. Before opening or updating a pull request
from Linux, run `make pre-ci`: it adds the same packaged host smoke used by CI and
a Zig-backed Windows GNU Clippy and test-link pass that catches `cfg(windows)`
and linker failures without waiting for a hosted Windows runner.

| Command          | What it checks                                              |
| ---------------- | ----------------------------------------------------------- |
| `make fmt`       | Writes `cargo fmt` and oxfmt formatting                     |
| `make fmt-check` | Verifies formatting without writing                         |
| `make lint`      | `cargo clippy --workspace --all-targets -- -D warnings`     |
| `make test`      | Serial Cargo compatibility path; reports every failed suite |
| `make test-par`  | Same non-ignored tests, concurrent across test binaries     |
| `make deny`      | Licence and advisory checks via `cargo-deny`                |
| `make check`     | `cargo check --workspace --all-targets`                     |
| `make ci`        | Host metadata, format, lint, parallel tests, supply chain   |
| `make pre-ci`    | `make ci`, check, packaged smoke, Windows Clippy/test link  |

Run the smallest command that covers your change first — `cargo test -p <crate>` —
then `make pre-ci` before opening a pull request. Linux CI installs pinned
nextest. Native Windows CI uses the in-tree binary-level scheduler instead:
Cargo compiles once, then a bounded worker pool runs test binaries concurrently,
with progress and per-suite timeouts. ACP stdio, startup timing, and ConPTY
lifecycle binaries run exclusively with one harness thread; a timeout terminates
the complete descendant process tree. Windows Clippy and this test job start in
parallel. Locally `make test-par` uses nextest when available and the same
measured scheduler otherwise. The cross pass is predictive: native Windows/MSVC
tests, ConPTY, Job Object, and hosted-runner loopback behavior still require CI.
Install its one-time prerequisites with `rustup target add
x86_64-pc-windows-gnu`, a `zig` binary on `PATH`, and `cargo-zigbuild`. Do not
report that a workspace-wide gate passed unless its command reached successful
completion.

`make fmt-check` and `make lint` fail closed. A missing formatter is a failure, not
a skip.

## Commit messages

[Conventional Commits](https://www.conventionalcommits.org/) with a scope, written
in the imperative:

```
feat(tui): add sticky reply identity and live work footer
fix(auth): list only usable login providers
docs(harness): record native lifecycle adoption
refactor(runtime): enforce quiescent component lifecycle
```

Common types are `feat`, `fix`, `refactor`, `docs`, `test`, `perf`, and `chore`.
The scope is usually the crate without its `zuno-` prefix.

## Pull requests

- One coherent change per pull request. Unrelated fixes are separate pull requests.
- Update every internal caller in the same change. This project prefers a corrected
  foundation over a compatibility shim.
- A non-trivial runtime change updates the relevant page in `docs/` in the same
  commit. Changing the default agent loop means updating
  [`docs/harness-runtime.md`](./docs/harness-runtime.md).
- Tests describe Zuno's behaviour. Do not add a test whose only purpose is parity
  with another product.
- Say what you actually ran. A reviewer trusts an honest "I ran `cargo test -p
zuno-tui`, not the full workspace" far more than an unverifiable claim.

## What a change owes

Three properties are enforced by tests rather than by review habit, so a change
that breaks one fails `cargo test` rather than reaching a reviewer:

- **No `unsafe`.** `unsafe_code` is `forbid` workspace-wide and
  `crates/zuno-cli/tests/release_surface.rs` scans first-party sources directly.
  Every crate must carry `[lints] workspace = true`; omitting it silently drops the
  guarantee and is treated as a defect.
- **Every lint suppression carries a reason.** `#[allow(...)]` without a stated
  reason fails the same suite.
- **One dependency version per workspace.** Third-party versions live once in the
  root `Cargo.toml`; member crates write `dep = { workspace = true }` and never
  restate a version. Extra cargo features are added additively alongside the
  inheritance.

The release surface is also asserted from inside the test suite: every target
must build, package, execute, attest, and upload in one candidate leg; publication
must consume one immutable candidate run without compiling; and `zuno/pr-gate`
must require every job in `ci.yml`. If you add a CI job or release target, that
suite will tell you what else to update. See
[`docs/operate/release-pipeline.md`](./docs/operate/release-pipeline.md).

## Reporting a bug

Open an issue using the bug template. The three things that make a report
actionable are the exact command, `zuno --version --long`, and the observed versus
expected behaviour. `RUST_LOG=debug` output helps; redact credentials first.

## Security

Do not open a public issue for a vulnerability. See [`SECURITY.md`](./SECURITY.md).
