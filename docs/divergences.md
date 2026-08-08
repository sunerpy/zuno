# Divergences

Seven deliberate differences from upstream `opencode` 1.18.13. Each one is a
**decision**, not an omission: a surface that is merely unimplemented is a gap,
recorded in the compatibility report's `known_gaps` and listed in the
[compatibility matrix](compatibility-matrix.md), never here.

## How this page cannot drift

The entries below are generated from [`docs/divergences.toml`](divergences.toml),
which is the same file `crates/oc-testkit/tests/compat_suite.rs` loads and counts
against `oc_testkit::divergence::DECLARED_COUNT`. Adding an entry without a count
bump fails the compatibility suite; adding an entry without documenting it fails
`crates/oc-cli/tests/docs.rs`. Regenerate this page with:

```sh
OC_DOCS_REGENERATE=1 cargo test -p oc-cli --test docs
```

Everything between the generated markers comes from the allow-list, including
each entry's stated reason. Do not edit it by hand.

<!-- generated:BEGIN divergence-detail -->
### session-list-default-sort

**Surface.** CLI `session list`; HTTP `GET /api/session`; `oc-db` session listing

**Why.** Upstream is self-inconsistent — `/api/session` sorts `time_created` while the legacy and experimental global listings sort `time_updated` — so one default was chosen: `time_updated DESC, id DESC`, with `--sort created` / `?sort=created` to opt out.

### tool-output-filename-carries-session

**Surface.** on-disk `$XDG_DATA_HOME/opencode/tool-output/tool_<session>_<uuidv7>`

**Why.** Upstream's `bound()` takes a `sessionID` that `write()` never uses, so a filename cannot be attributed to a session; the prune in plan todo 83 needs that attribution, so the session id is encoded in the name and the mtime sweep is kept as the backstop for foreign files.

### no-eager-directory-creation

**Surface.** process startup; `oc-paths` layout getters

**Why.** Upstream's `global.ts:35-43` creates seven directories at module import, before any command has decided it needs them — observable as `TMPDIR=/ opencode debug paths` exiting 1 with `EACCES … mkdir '/opencode'`; here every getter is a pure computation and creation happens only in `Layout::ensure`.

### split-version-identity

**Surface.** CLI `--version` and `--version --long`; the npm plugin compatibility gate

**Why.** Two peers need different answers: `plugin/loader.ts:123-130` skips an npm plugin whose declared range excludes the running version, so the short version reports the pinned `1.18.13`, while the real build identity is exposed separately by `--version --long` rather than being hidden.

### execute-parameter-contract

**Surface.** tool `execute` — the model-facing parameter schema

**Why.** Upstream takes `{ code: string }` and runs a confined `acorn`+`typescript` interpreter (`packages/opencode/src/tool/code-mode.ts:12-20`); a binary that forbids `unsafe` and bundles no JavaScript runtime instead takes jcode-shaped structured sub-calls, which changes what the model sees and so is declared rather than implied.

### c8-maintenance-endpoints

**Surface.** HTTP `GET /api/session/prune`, `POST /api/session/prune`

**Why.** Session retention is added scope that upstream lacks entirely, so the `/api` relation to upstream is a required subset and never an equality claim; these two operations are the whole extension and the suite asserts that set exactly.

### provider-coverage-by-wire-family

**Surface.** provider selection; `oc-provider-compatible` family routing and its diagnostics

**Why.** Upstream bundles 23 SDK factories behind one config surface; SigV4+EventStream, Gemini's wire format and Vertex auth cannot share an OpenAI-compatible request builder, so coverage is stated per wire-protocol family and an id no family claims is named in an error instead of being silently routed through the compatible profile.
<!-- generated:END divergence-detail -->

## What is deliberately not on this page

Six further deliberate differences are declared in code but sit outside the
count the plan asserts, and the compatibility report emits them as
`nominated_divergences` with a citation for each rather than laundering them into
this file. See `crates/oc-testkit/tests/compat_suite.rs::nominated_divergences`
and `.omo/notepads/opencode-rust/issues.md`.
