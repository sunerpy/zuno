# Divergences

Thirteen deliberate differences from upstream `opencode` 1.18.13. Each one is a
**decision**, not an omission: a surface that is merely unimplemented is a gap,
recorded in the compatibility report's `known_gaps` and listed in the
[compatibility matrix](compatibility-matrix.md), never here.

This page is the **single place** a behavioural difference is declared. Four of the
thirteen arrived late, in plan todo 119: they had been recorded in
`compat_suite.rs::nominated_divergences`, a second structure that asserted they
stayed *out* of the allow-list, so a reader consulting this page could not learn
about them and no gate could fail while they went undeclared. The thirteenth
arrived in plan todo 133, which declared what success criterion 2's narrowing to
pure mode leaves out — because a narrowing nothing declares is a waiver.

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

### cross-session-resident-memory

**Surface.** system-prompt resident blocks; model-facing `memory` tool; post-response reflection

**Why.** Upstream opencode 1.18.13 has no cross-session memory subsystem; this implementation carries character-capped global notes and project rules into later sessions and can reflect after delivery, so `memory: false` is the single strict-parity switch that removes all three surfaces and preserves the original prompt bytes. Absorbs the former `memory-subsystem` nomination, which named these same three surfaces.

### session-subpath-is-applied

**Surface.** HTTP `GET /api/session?project=…&subpath=…`; `oc-db` session listing in project scope

**Why.** Upstream declares `subpath` in the v2 list union (`packages/core/src/session.ts:68-76`), the HTTP query schema (`packages/protocol/src/groups/session.ts:98-110`), the generated client and the SDK, and the handler even forwards it (`packages/server/src/handlers/session.ts:23-37`) — but `session.list` builds its conditions from `directory`, `workspaceID`, `project` and `search` only (`packages/core/src/session.ts:268-277`), so the parameter changes nothing upstream. This port applies it, which narrows the result set for a request upstream answers unfiltered. It is matched as a literal tree prefix (`path = s OR path LIKE s || '/%'` with the pattern bound, not interpolated), so a subpath containing `_` or `%` selects that directory and not a wildcard family; upstream's un-escaped `LIKE '${path}/%'` lives on the LEGACY `/session?path=` handler (`packages/opencode/src/session/session.ts:969-980`), a route this port does not serve, so literal matching is a property of the applied filter rather than a second divergence. Absorbs the former `subpath-matches-literally` nomination.

### context-md-excluded

**Surface.** project instruction cascade — the filename list probed by `findUp`

**Why.** Upstream's `instructionFiles` still lists `CONTEXT.md` behind a `// deprecated` marker (`packages/opencode/src/session/instruction.ts:60-68`) and genuinely loads it: the cascade probes each name in order and the first project-level match wins (`:122-132`), then every resolved path is read and injected as `Instructions from: …` (`:155-168`). This port stops at `AGENTS.md` and `CLAUDE.md`, so a repository whose only instruction file is `CONTEXT.md` contributes one instruction block under the TypeScript binary and zero here.

### malformed-auth-json-is-an-error

**Surface.** `$XDG_DATA_HOME/opencode/auth.json` — reading the credential store

**Why.** Upstream funnels every read and parse failure into an empty store: `readJson(file).pipe(Effect.orElseSucceed(() => ({})))` (`packages/opencode/src/auth/index.ts:58-67`), and the next `set` writes `{ ...data, [norm]: info }` over the file (`:73-80`) — so one truncated `auth.json` silently destroys every other credential in it. This port returns a typed `Malformed` error naming the file instead, which fails the read a user can retry rather than losing data they cannot recover. An empty or whitespace-only file still reads as an empty store, because that is a crash mid-create rather than corruption.

### failed-format-restores-pre-format-bytes

**Surface.** post-edit formatter execution — the file's bytes after a formatter exits non-zero

**Why.** Upstream inspects the exit code and only logs: `if (result && result.exitCode !== 0) yield* Effect.logError("failed", …)`, with a spawn failure mapped to `undefined` and the loop continuing (`packages/opencode/src/format/index.ts:73-114`). Nothing is snapshotted or written back, so whatever a failing formatter left on disk — including a truncated file — stands. This port keeps the bytes the edit wrote and restores them when a formatter exits non-zero, reports `editRestored` in the tool metadata and says so in the tool output. The cost is deliberate: useful partial work from a formatter that exits non-zero after reformatting is discarded.

### non-pure-plugin-generated-trees

**Surface.** `debug config` without `OPENCODE_PURE` — the `agent` and `command` trees a third-party JS plugin synthesises

**Why.** Success criterion 2 was NARROWED on 2026-08-09 to require byte-identical merged configuration in pure mode (`OPENCODE_PURE=1`), where neither binary loads external plugins. Without pure mode the released 1.18.15 binary's own plugin set writes generated entries into the merged config that this port does not reproduce: measured on the user's real `/config/.config/opencode/opencode.json`, a 221818-byte `agent` tree and a 17970-byte `command` tree, against empty `agent` and `command` objects here. Reproducing them means re-implementing third-party plugin output rather than the config contract, so it is a decision and not a gap — and declaring it with its measured sizes is what makes a *new* non-pure difference a failure instead of one absorbed into a vague inequality.

### plain-cli-presentation

**Surface.** every CLI command's stdout and stderr — colour, the `Error: ` prefix, the prompt gutter, and JSON object key order

**Why.** The released binary decorates its output in four ways this port does not, all four measured on 1.18.15 under `NO_COLOR=1` and `TERM=dumb` in `.omo/evidence/task-135-opencode-rust.txt`. (1) It emits SGR colour regardless: `import probe.json` writes `ESC[91m ESC[1m Error: ESC[0m` — `NO_COLOR` is honoured for the TUI theme, not for these writers. (2) It prefixes a top-level failure with a line-leading `Error: `; this port prints the message alone, because the shell already reports the non-zero status and the prefix costs a line of terminal width in every failure. (3) It renders `mcp list` and `providers list` through `@clack/prompts`, drawing a box gutter of `┌ │ ▲ └` glyphs plus a trailing blank line; this port writes plain lines, which is what makes the output usable in a pipe. (4) It serializes embedded JSON with keys in insertion order, so `agent list`'s permission arrays spell `{action, pattern}` where this port spells `{pattern, action}`, and JavaScript's single number type writes an integral double as `1024` where `serde_json` writes `1024.0`. All four are presentational: the same values, the same order, the same lines. `crates/oc-testkit/src/cli_normalize.rs` neutralizes exactly these four and nothing else — each rule has a negative control asserting a renamed key, a changed value, a dropped line, a non-SGR escape and a `\r\n` all still diverge — and `crates/oc-cli/tests/cli_parity.rs::the_declared_presentation_divergences_are_live` re-derives all four from the two running binaries, so a reverted decision fails rather than widening what is forgiven.

### diagnostics-name-their-cause

**Surface.** CLI failure messages on paths where upstream reports an opaque error — `serve` on an unavailable port, `run` with no message, `run` with an unresolvable model

**Why.** Upstream reports several failures without naming what failed. Measured on 1.18.15: binding an occupied port prints the two-line `Unexpected error` / `ServeError` with neither the address nor the reason, and `run --model bogus/model hi` prints a JSON `UnknownError` carrying `Unexpected server error. Check server logs for details.` and a `ref` — a message whose only actionable content is an instruction to read a log the user does not have. This port names the cause and the input at the point of failure: `could not bind HTTP server to 127.0.0.1:<port>: Address already in use (os error 98)`, and for an unresolvable model the provider, the catalogue state and the three ways to fix it. The consequence is that the two texts are not two renderings of one message and cannot be normalized into one without deleting the information that makes this port's version useful, so `crates/oc-cli/tests/cli_parity.rs` exempts those two stderr streams **by name, with this reason**, while still comparing their exit status and stdout and still asserting both sides refuse. The same decision covers the wording of `run`'s argv refusal (`a message is required` against `You must provide a message or a command`).

### session-list-output-shape

**Surface.** CLI `session list` and `session list --format json` with at least one session

**Why.** The empty listing is identical on both binaries. A non-empty one is not, and this is a difference in content rather than presentation, so nothing normalizes it. Upstream's table has three columns — `Session ID`, `Title`, `Updated` — and its JSON object has six flat fields: `id`, `title`, `updated`, `created`, `projectId`, `directory`. This port's table has seven columns, adding `Project`, `Agent`, `Msgs` and `Cost`, and its JSON nests `time.created` / `time.updated`, `tokens.*` and `project.{id,worktree}` while spelling the project reference `projectID` rather than `projectId`. The added columns exist because the listing this port serves is not project-scoped by construction — see the `session-list-default-sort` entry and `--all-projects` — so a row has to say which project it belongs to, and the nesting matches the shape `/api/session` returns so one decoder serves both. `crates/oc-cli/tests/cli_parity.rs::the_session_list_output_shape_difference_is_live` seeds one session into a database both binaries open and asserts the two field sets differ in exactly the `projectId` / `projectID` spelling this reason names, so the declaration carries a measurement rather than a memory.

### non-vcs-plan-glob-is-absolute

**Surface.** `agent list` — the `plan` agent's `edit` allow-rule for the global plans directory, in a directory that is not a repository

**Why.** Upstream builds that rule as `path.relative(ctx.worktree, path.join(Global.Path.data, 'plans', '*.md'))` (`packages/opencode/src/agent/agent.ts:174`), and assigns a non-VCS project the worktree `/` (`packages/opencode/src/project/project.ts:217`). Relativising an absolute path against `/` strips the leading separator, so outside a repository the released binary emits a **relative** glob — measured on 1.18.15 as `tmp/.../data/opencode/plans/*.md` — which matches nothing from any working directory and is the shape a `worktree` sentinel produced rather than a path anyone intended. This port emits the absolute path there, and matches upstream exactly inside a repository, where the relative form is meaningful. `crates/oc-cli/tests/cli_parity.rs::the_non_vcs_plan_glob_difference_is_live` runs both binaries in an unmarked directory and asserts upstream's glob is relative while this port's is absolute; it is also why every parity probe marks its project with a `.git` directory, so that a probe never silently compares two answers to different questions.
<!-- generated:END divergence-detail -->

## What is deliberately not on this page

**Gaps.** A surface that is merely unimplemented is not a decision, so writing it
here would convert an omission into a divergence by fiat. Those are reported as
`known_gaps` by the compatibility report and listed in the
[compatibility matrix](compatibility-matrix.md).

Nothing else. Every behavioural difference is on this page. The compatibility
report's `behavioural_differences` section indexes them: each record names the
entry above that declares it, the upstream file and lines that make it a
difference, and the test that proves the behaviour is live. The compatibility
suite resolves each `declared_as` against this file and **fails when one is
undeclared** — the assertion that used to run the other way. Two of the six
records share an entry rather than being declared twice, because neither was an
independent difference from upstream:

- `subpath-matches-literally` belongs to `session-subpath-is-applied`. Upstream's
  un-escaped `LIKE '${path}/%'` is on the legacy `/session?path=` handler
  (`packages/opencode/src/session/session.ts:969-980`), which this port does not
  serve; the v2 `/api/session?subpath=` surface ignores `subpath` entirely
  (`packages/core/src/session.ts:268-277`), so there is no upstream pattern-match
  to differ from — literal matching is a property of applying the parameter at all.
- `memory-subsystem` belongs to `cross-session-resident-memory`, which already
  declares the same three surfaces.
