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

`make ci` is the local gate and runs exactly what CI enforces, so "green locally"
and "green in CI" cannot drift into meaning different things:

| Command          | What it checks                                          |
| ---------------- | ------------------------------------------------------- |
| `make fmt`       | Writes `cargo fmt` and oxfmt formatting                 |
| `make fmt-check` | Verifies formatting without writing                     |
| `make lint`      | `cargo clippy --workspace --all-targets -- -D warnings` |
| `make test`      | `cargo test --workspace`                                |
| `make deny`      | Licence and advisory checks via `cargo-deny`            |
| `make check`     | `cargo check --workspace --all-targets`                 |
| `make ci`        | All of the above, in the order CI runs them             |

Run the smallest command that covers your change first — `cargo test -p <crate>` —
then `make ci` before opening a pull request. Do not report that a workspace-wide
gate passed unless its command reached successful completion.

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

The release surface is also asserted from inside the test suite: the build and
smoke matrices must name the same targets, publication must depend on the smoke
job, and `CI Success` must require every job in `ci.yml`. If you add a CI job or a
release target, that suite will tell you what else to update.

## Reporting a bug

Open an issue using the bug template. The three things that make a report
actionable are the exact command, `zuno --version --long`, and the observed versus
expected behaviour. `RUST_LOG=debug` output helps; redact credentials first.

## Security

Do not open a public issue for a vulnerability. See [`SECURITY.md`](./SECURITY.md).
