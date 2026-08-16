# Compatibility matrix

This is the retained verification inventory against upstream `opencode` 1.18.13,
not a promise that Zuno is a drop-in binary or session-store replacement. Zuno is
independent; only the plugin ABI remains a supported compatibility layer.

That decision has since been taken. The whole-surface differential suites that
byte-compared Zuno's output against a released `opencode` binary are **gone**, and
the success criteria that required them are retired. What this page still
describes is live and asserted from code: the `/api` surface the plugin ABI is
served over, the declared divergences, the CLI disposition table, and the known
gaps. Five states are used throughout:

- **implemented** — registered and backed by a handler that does the work.
- **explicit gap (503 backend unavailable)** — the path and method exist, but
  the operation-specific response names the unavailable backend. It remains a
  compatibility gap and is never counted as parity; a `501` fails the matrix.
- **diverged / added** — present here and deliberately different from, or absent
  in, upstream. Every one is an entry in [divergences.md](divergences.md).
- **rejected** — registered so that invoking it produces a migration message
  instead of a silent failure.
- **not-registered** — deliberately absent, with a named owner.

Every table on this page is generated from the code it describes. Regenerate with:

```sh
ZUNO_DOCS_REGENERATE=1 cargo test -p zuno-cli --test docs
```

## Declared divergences

<!-- generated:BEGIN divergence-index -->
| # | id | surface |
|---:|---|---|
| 1 | [`session-list-default-sort`](divergences.md#session-list-default-sort) | CLI `session list`; HTTP `GET /api/session`; `zuno-db` session listing |
| 2 | [`tool-output-filename-carries-session`](divergences.md#tool-output-filename-carries-session) | on-disk `$XDG_DATA_HOME/zuno/tool-output/tool_<session>_<uuidv7>` |
| 3 | [`no-eager-directory-creation`](divergences.md#no-eager-directory-creation) | process startup; `zuno-paths` layout getters |
| 4 | [`split-version-identity`](divergences.md#split-version-identity) | CLI `--version` and `--version --long`; the npm plugin compatibility gate |
| 5 | [`execute-parameter-contract`](divergences.md#execute-parameter-contract) | tool `execute` — the model-facing parameter schema |
| 6 | [`c8-maintenance-endpoints`](divergences.md#c8-maintenance-endpoints) | HTTP `GET /api/session/prune`, `POST /api/session/prune` |
| 7 | [`provider-coverage-by-wire-family`](divergences.md#provider-coverage-by-wire-family) | provider selection for a model whose resolved `api.npm` transport is unknown to this build |
| 8 | [`cross-session-resident-memory`](divergences.md#cross-session-resident-memory) | system-prompt resident blocks; model-facing `memory` tool; post-response reflection |
| 9 | [`session-subpath-is-applied`](divergences.md#session-subpath-is-applied) | HTTP `GET /api/session?project=…&subpath=…`; `zuno-db` session listing in project scope |
| 10 | [`context-md-excluded`](divergences.md#context-md-excluded) | project instruction cascade — the filename list probed by `findUp` |
| 11 | [`malformed-auth-json-is-an-error`](divergences.md#malformed-auth-json-is-an-error) | `$XDG_DATA_HOME/zuno/auth.json` — reading the credential store |
| 12 | [`failed-format-restores-pre-format-bytes`](divergences.md#failed-format-restores-pre-format-bytes) | post-edit formatter execution — the file's bytes after a formatter exits non-zero |
| 13 | [`non-pure-plugin-generated-trees`](divergences.md#non-pure-plugin-generated-trees) | `debug config` without `OPENCODE_PURE` — the `agent` and `command` trees a third-party JS plugin synthesises |
| 14 | [`plain-cli-presentation`](divergences.md#plain-cli-presentation) | every CLI command's stdout and stderr — colour, the `Error: ` prefix, the prompt gutter, and JSON object key order |
| 15 | [`diagnostics-name-their-cause`](divergences.md#diagnostics-name-their-cause) | CLI failure messages on paths where upstream reports an opaque error — `serve` on an unavailable port, `run` with no message, `run` with an unresolvable model |
| 16 | [`session-list-output-shape`](divergences.md#session-list-output-shape) | CLI `session list` and `session list --format json` with at least one session |
| 17 | [`non-vcs-plan-glob-is-absolute`](divergences.md#non-vcs-plan-glob-is-absolute) | `agent list` — the `plan` agent's `edit` allow-rule for the global plans directory, in a directory that is not a repository |
<!-- generated:END divergence-index -->

## Known gaps

A surface that is merely **unimplemented** is not a decision, so it is never an
entry on [divergences.md](divergences.md). It is recorded here and in the
compatibility report's `known_gaps` section, which this table is generated from —
`zuno_testkit::compat_report::known_gaps`.

Each entry below names what a consumer loses and, where one exists, the test that
fails if the gap closes or goes stale.

<!-- generated:BEGIN known-gaps -->
### api-backends-unavailable

**Surface.** 10 of the 58 upstream /api operations

**What is missing.** Every upstream operation is registered here and probed through the real router in-process; no second process is executed. 48 operations have local backends. The remaining 10 return an operation-specific 503 backend_unavailable response and are never counted as coverage. Any 501 fails the gate outright. This remains a gap in this server's own capability, not a declared behavioral difference.

### permission-evaluation-semantics

**Surface.** permission resolution (`findLast` wildcard matching)

**What is missing.** The merged permission CONFIG is compared against the real binary; the evaluation order that turns it into an allow/ask/deny decision is verified against the upstream source by unit tests, not differentially, because the binary exposes no command that prints a resolved decision.

### channel-dependent-database-filename

**Surface.** $XDG_DATA_HOME/zuno/zuno-<channel>.db

**What is missing.** A Zuno source build resolves zuno-local.db while an installed release resolves zuno.db, so a `cargo build` does not see the release database. Zuno retains the oracle's channel filename rule (packages/core/src/database/database.ts:45-55) exactly, so it is FAITHFUL BEHAVIOUR inside Zuno's own data root and not a divergence — recorded here because it presents as a missing-data bug the first time anyone tries it. Plan todo 92 owns documenting it.

### assistant-turn-step-parts

**Surface.** the `part` rows one assistant turn persists — the step-boundary parts

**What is missing.** For one plain single-step turn the release persists [step-start, text, step-finish] and this port persists [text], so [step-start, step-finish] is never written. Measured on the `run` path at 1.18.18 in .omo/evidence/task-178-opencode-rust.txt, inside a git repository and outside one; the user's production database holds 280,859 step-start rows, so the release's shape is the normal one rather than an artefact. This is a GAP and not a declared divergence because nothing chose it: `zuno-db` already models both types as first-class wire tags (crates/zuno-db/src/message.rs:139-142,181-182) and `zuno-engine::stream::StreamProjector` already writes upstream's exact shape including the snapshot hashes (crates/zuno-engine/src/stream.rs:211-265,869-977), but no production caller reaches it — the live turn path accumulates and then checkpoints only text, reasoning and tool parts (crates/zuno-engine/src/loop.rs:1547-1588). An unwired implementation is work outstanding, so declaring it in docs/divergences.toml would dress an omission up as a decision. What a consumer loses: upstream reads `step-finish.cost`/`tokens` to aggregate session usage (packages/core/src/session/projector.ts:36-42,90-108) and takes the first `step-start.snapshot` and last `step-finish.snapshot` as the bounds of a turn's diff (packages/opencode/src/session/summary.ts:82-99), which `revert` then refreshes (packages/opencode/src/session/revert.ts:70-77). Interoperability is unaffected and was measured to be: every assertion in crates/zuno-testkit/tests/session_interop.rs holds across this difference in both directions. Witnessed by crates/zuno-testkit/tests/session_interop.rs::the_recorded_turn_part_gap_matches_what_a_turn_actually_persists.

### v1-surface-unbacked

**Surface.** 6 of the 20 measured pre-/api (v1) routes the installed plugins actually call

**What is missing.** The pre-/api surface exists because the published SDK sends unprefixed paths, so every resident plugin talks to it. It registers 20 routes, each with a recorded plugin callsite, and 14 do real local work. Ten adapters reuse the corresponding /api implementations for app.agents, provider.list, session.list, session.create, session.get, session.abort, session.summarize, session.messages, session.prompt and session.promptAsync. Three local authentication backends persist auth.set credentials and invoke the installed provider OAuth authorize/callback closures. POST /tui/show-toast remains a recording sink rather than a display — no server entry point attaches a forwarder (crates/zuno-server/src/main.rs and crates/zuno-cli/src/cmd/serve.rs both build a bare CompatV1State::new). 6 of the 20 answer `501 not_implemented`. 0 of those 6 name a served /api alternative; the other 6 have no served /api spelling at all — app.log, config.get, session.status, session.update, session.children and session.todo — so a plugin that needs one has no working call today. The installed auth plugins' measured authentication routes are served; the remaining gaps are non-authentication operations. This is a GAP and not a declared divergence because nothing chose it, and docs/divergences.toml:11-14 forbids recording an unimplemented surface as a decision. Witnessed by crates/zuno-server/tests/compat_v1.rs::compat_v1_declared_backing_matches_what_the_router_answers, which drives every route and fails if a declared status disagrees with what the router answers.

### v1-agent-projection-drift

**Surface.** the `Agent` body shape `GET /agent` serves the pre-/api (v1) SDK

**What is missing.** The projection serves three keys the oracle `Agent` schema does not declare — builtIn, maxSteps and tools, against a schema with additionalProperties:false — and omits six it declares as optional: hidden, native, steps, temperature, topP and variant. maxSteps against the oracle's steps reads as a rename. What is NOT missing is any required key: all four of name, mode, permission and options are served, so no v1 caller reads a promised field and gets nothing. That is the line between this and the `Session` slug omission the same review wave found, which was a defect because the dropped key was required by the oracle AND by the OpenAPI this build publishes at /doc, making the build contradict itself. Here the build publishes no `Agent` schema at all. The committed 1.18.18 oracle capture is byte-identical to the live `/doc` recapture, so this optional-key drift is confirmed against the current executable pin. It remains a gap, not a declared divergence, because no implementation decision chose the difference; docs/divergences.toml:11-14 forbids recording an omission as a decision. Witnessed by crates/zuno-server/tests/compat_v1.rs::compat_v1_agent_projection_residual_drift_matches_pinned_capture_and_drops_no_required_key, which measures the served key set against the oracle schema and fails if a required key is ever dropped or if this build starts publishing an `Agent` schema of its own — either event ends the reason recorded here.

### openapi-body-schema-bindings

**Surface.** 48 published /api operations whose request or success-response body schema is not fully bound

**What is missing.** The remaining type work is frozen by operation and reason: GET `/api/health` — the successful response is an untyped Json<Value>; GET `/api/location` — LocationInfo does not derive JsonSchema; GET `/api/event` — the successful response is an SSE stream, not a modeled JSON body; GET `/api/agent` — LocationEnvelope<Vec<AgentInfo>> and its nested catalog types do not derive JsonSchema; GET `/api/model` — LocationEnvelope<Vec<ModelInfo>> and its nested provider types do not derive JsonSchema; GET `/api/command` — LocationEnvelope<Vec<CommandInfo>> does not derive JsonSchema; GET `/api/skill` — LocationEnvelope<Vec<SkillInfo>> does not derive JsonSchema; GET `/api/reference` — LocationEnvelope<Vec<ReferenceInfo>> does not derive JsonSchema; GET `/api/provider` — LocationEnvelope<Vec<ProviderInfo>> and its nested types do not derive JsonSchema; GET `/api/provider/{providerID}` — LocationEnvelope<ProviderInfo> and its nested types do not derive JsonSchema; GET `/api/integration` — LocationEnvelope<Vec<IntegrationInfo>> and its nested types do not derive JsonSchema; GET `/api/integration/{integrationID}` — OptionalEnvelope<IntegrationInfo> and its nested types do not derive JsonSchema; POST `/api/integration/{integrationID}/connect/key` — the unsupported handler has neither a typed request extractor nor a modeled success response; POST `/api/integration/{integrationID}/connect/oauth` — the unsupported handler has neither a typed request extractor nor a modeled success response; GET `/api/integration/attempt/{attemptID}` — the unsupported handler has no modeled success response; POST `/api/integration/attempt/{attemptID}/complete` — the unsupported handler has neither a typed request extractor nor a modeled success response; DELETE `/api/integration/attempt/{attemptID}` — the unsupported handler has no modeled response contract; PATCH `/api/credential/{credentialID}` — the unsupported handler has neither a typed request extractor nor a modeled success response; DELETE `/api/credential/{credentialID}` — the unsupported handler has no modeled response contract; GET `/api/fs/read/*` — the response is content-type-dependent raw bytes with no schema type; GET `/api/fs/list` — LocationEnvelope<Vec<Entry>> does not derive JsonSchema; GET `/api/fs/find` — LocationEnvelope<Vec<Entry>> does not derive JsonSchema; GET `/api/pty` — PtyInfo is imported without a JsonSchema implementation; POST `/api/pty` — CreateInput and PtyInfo are imported without JsonSchema implementations; GET `/api/pty/{ptyID}` — PtyInfo is imported without a JsonSchema implementation; PUT `/api/pty/{ptyID}` — UpdateInput and PtyInfo are imported without JsonSchema implementations; POST `/api/pty/{ptyID}/connect-token` — ConnectTokenResponse and its nested types do not derive JsonSchema; GET `/api/pty/{ptyID}/connect` — the response upgrades to WebSocket frames and has no JSON body model; GET `/api/permission/request` — the opaque Json<impl Serialize> envelope has no nameable JsonSchema type; GET `/api/permission/saved` — Data<Vec<Value>> leaves each saved permission payload untyped; POST `/api/session/{sessionID}/permission` — the unsupported handler has neither a typed request extractor nor a modeled success response; GET `/api/session/{sessionID}/permission` — PermissionRequest does not derive JsonSchema; GET `/api/session/{sessionID}/permission/{requestID}` — the unsupported handler has no modeled success response; POST `/api/session/{sessionID}/permission/{requestID}/reply` — PermissionReplyBody does not derive JsonSchema; GET `/api/question/request` — the opaque Json<impl Serialize> envelope has no nameable JsonSchema type; GET `/api/session/{sessionID}/question` — QuestionRequest does not derive JsonSchema; POST `/api/session/{sessionID}/question/{requestID}/reply` — QuestionReplyBody does not derive JsonSchema; GET `/api/session/prune` — SessionPruneReport does not derive JsonSchema; POST `/api/session/prune` — the request is bound, but SessionPruneReport does not derive JsonSchema for the response; GET `/api/session/{sessionID}/event` — the successful response is an SSE stream, not a modeled JSON body; POST `/api/session/{sessionID}/agent` — AgentBody does not derive JsonSchema; POST `/api/session/{sessionID}/model` — ModelBody and ModelRefBody do not derive JsonSchema; POST `/api/session/{sessionID}/prompt` — PromptBody, PromptAdmitted, and their nested types do not derive JsonSchema; POST `/api/session/{sessionID}/revert/stage` — RevertStageBody does not derive JsonSchema and Data<Value> leaves the response untyped; GET `/api/session/{sessionID}/context` — Data<Vec<Value>> leaves context items untyped; GET `/api/session/{sessionID}/history` — HistoryResponse does not derive JsonSchema; GET `/api/session/{sessionID}/message/{messageID}` — the unsupported handler has no modeled success response; GET `/api/session/{sessionID}/message` — MessagesResponse and MessageCursor do not derive JsonSchema. These are gaps, not declared divergences: each entry must be removed when its Rust body types gain JsonSchema and the operation is bound.
<!-- generated:END known-gaps -->

## Cross-session resident memory

<!-- generated:BEGIN cross-session-memory -->
Persistent memory is **enabled by default**. With both non-empty scopes, the default resident prompt budget is up to **5200 stored characters** (`2200` global + `3000` project), plus two rendered scope headers. The model-facing tool schema also adds request metadata while enabled. No embedding model, vector database, or external memory service is used.

`memory: false` is the only supported strict-parity mode: resident files are not opened, the `memory` tool is not advertised, reflection cannot spawn, and the original system-prompt bytes are returned unchanged.

| key | default | effect |
|---|---:|---|
| `memory` | `true` | master switch for all three surfaces |
| `memory.resident` | `true` | inject session-frozen global and project blocks |
| `memory.tool` | `true` | advertise the model-facing `memory` tool |
| `memory.reflection` | `true` | permit post-response reflection tasks |
| `memory.global_char_limit` | `2200` | cap `$CONFIG/memory/MEMORY.md` in Unicode scalar values |
| `memory.project_char_limit` | `3000` | cap `<worktree>/.zuno/RULES.md` in Unicode scalar values |
| `memory.nudge_interval` | `10` | periodic reflection cadence in delivered turns; `0` disables only that trigger |

Reflection must not learn any of these negative cases:
- Environment-dependent failures: missing binaries, fresh-install errors, post-migration path mismatches, 'command not found', unconfigured credentials, uninstalled packages. The user can fix these — they are not durable rules.
- Negative claims about tools or features ('browser tools do not work', 'X tool is broken', 'cannot use Y from execute_code'). These harden into refusals the agent cites against itself for months after the actual problem was fixed.
- Session-specific transient errors that resolved before the conversation ended. If retrying worked, the lesson is the retry pattern, not the original failure.
- One-off task narratives. A user asking 'summarize today's market' or 'analyze this PR' is not a class of work that warrants a skill.
- Unresolved failures: if the session ended WITHOUT actually finding a working method — you tried several things, none worked, and told the user to check manually — do NOT write those attempts up as a 'reliable workflow' or 'recommended approach'. That presents an untested sequence of failures as validated guidance a future session will trust and repeat. Either say 'Nothing to save', or, only if you are independently confident of a real working alternative (not something you are merely guessing might work), capture ONLY that alternative — never the dead ends, and never dressed up as best practice.
<!-- generated:END cross-session-memory -->

## CLI commands

Derived from `zuno_cli::dispositions()` — the same table
`crates/zuno-cli/tests/surface.rs::surface_registered_commands_match_their_dispositions`
asserts against the registered `clap` tree, and
`surface_every_upstream_command_has_exactly_one_disposition` asserts against a
committed capture of upstream 1.18.13's command symbols. So a command that gains
or loses a registration cannot pass while this table says otherwise.

Of upstream's 23 commands: 12 implemented, 8 rejected, 3 not-registered.
A rejected command is still registered, so invoking it explains
the replacement instead of reporting an unknown command. A not-registered one
names the owner of the work it waits on.

The `why` column is the reason recorded in the code, reproduced verbatim rather
than paraphrased. `todo N` references are Zuno's own build plan; treat them as
identifiers for the work that owns a surface, not as anything a user needs.

<!-- generated:BEGIN cli-disposition -->
| upstream symbol | command | disposition | why |
|---|---|---|---|
| `AcpCommand` | `acp` | not-registered | todo 78 owns the zuno-acp protocol adapter; registering it before that handler exists would advertise a server that cannot speak ACP |
| `AgentCommand` | `agent` | implemented | registered through the headless-command seam for todo 56 |
| `AttachCommand` | `attach` | not-registered | attach requires the TUI client and terminal lifecycle owned by the TUI wave; no headless substitute is equivalent |
| `ConsoleCommand` | `console` | rejected | the hosted OpenCode Console is excluded from Zuno's local-agent scope; use `providers` (alias `auth`) for local credentials instead |
| `DbCommand` | `db` | implemented | registered through the headless-command seam for todo 56 and the maintenance extensions in todo 84 |
| `DebugCommand` | `debug` | implemented | registered through the headless-command seam for todo 56 |
| `ExportCommand` | `export` | implemented | prints one session's whole transcript as JSON, byte-compared against the released binary's own export, with `--sanitize` redacting the same fields |
| `GenerateCommand` | `generate` | rejected | the command is a TypeScript source-tree SDK/OpenAPI generator that depends on Prettier and is excluded from the runtime binary; use the server's `/openapi.json` document instead |
| `GithubCommand` | `github` | rejected | the hosted GitHub agent is outside the local-agent scope; run `zuno run` from the CI workflow instead |
| `ImportCommand` | `import` | implemented | reads a local `export` document into Zuno's database; share-URL imports are not accepted because Zuno does not integrate with the hosted share service |
| `McpCommand` | `mcp` | implemented | registered through the headless-command seam for todo 56 |
| `ModelsCommand` | `models` | implemented | registered through the headless-command seam for todo 56 |
| `PluginCommand` | `plugin` | not-registered | the resident host loads configured plugins, but Zuno does not own an npm installer; declare plugins in opencode.json so compatibility is checked before import |
| `PrCommand` | `pr` | rejected | the GitHub checkout helper is excluded from the local-agent runtime; use `gh pr checkout <number>` and then `zuno run` instead |
| `ProvidersCommand` | `providers` | implemented | registered with the upstream `auth` alias through the headless-command seam for todo 56 |
| `RunCommand` | `run` | implemented | registered through the headless-command seam for todo 56 |
| `ServeCommand` | `serve` | implemented | registered through the headless-command seam; todo 56 wraps zuno-server's public builder rather than duplicating its server logic |
| `SessionCommand` | `session` | implemented | registered through the headless-command seam for todo 56 and session maintenance todos 80-85 |
| `StatsCommand` | `stats` | rejected | upstream stats reads the excluded stats package's session SQL directly; use `db stats` from todo 84 instead |
| `TuiThreadCommand` | `tui` | implemented | registered as `tui` and as the bare invocation upstream spells `$0`; it boots zuno-tui's application over the terminal lease from todo 73 and the views from todo 76 |
| `UninstallCommand` | `uninstall` | rejected | self-uninstallation is excluded from the runtime; remove `zuno` with the package manager or installer that placed it |
| `UpgradeCommand` | `upgrade` | rejected | the TypeScript self-updater cannot safely replace this Rust artifact and is excluded; install the desired release through the Rust release installer instead |
| `WebCommand` | `web` | rejected | the bundled hosted web application is excluded from this headless Rust scope; use `serve` and connect a supported client instead |
<!-- generated:END cli-disposition -->

## HTTP `/api` operations

Derived by set-differencing the document `zuno_server::api::openapi()` serves
against the committed capture of the pinned 1.18.18 release's document
(`.omo/fixtures/oracle-openapi-1.18.18.json`), then probing each served route
through the real router and recording which explicitly answer
`503 backend_unavailable`. Any `501` fails the gate.

**58 of the 58 upstream operations are registered**, plus **2 operations added**
for session retention (the declared `c8-maintenance-endpoints` divergence).
Forty-eight have local backends; **10 explicit 503 backend gaps** name the missing
capability and remain reported as gaps rather than compatibility.

The two SSE operations are implemented: `GET /api/event` immediately emits
`server.connected`, while `GET /api/session/{sessionID}/event` replays durable
events after `?after=<sequence>` and continues live. All 58 operations are probed
through the real router in-process, as described above; **no second binary is
executed**, and any `501` fails the gate outright rather than being exempted. The
whole-surface differential that once compared both processes was deleted when
behavioural equivalence with `opencode` stopped being a promise. The ten
session-read, request-state, and PTY-attach operations added in task 128 have
their status and operation-scoped normalized bodies asserted against recorded
expectations. Session message pages default to 50 entries and cap at 200; durable
history defaults to 50 and caps at 100. PTY attach credentials expire after 60
seconds, are single-use and scope-bound, and are never included in error
responses.

<!-- generated:BEGIN api-operations -->
| method | path | state |
|---|---|---|
| GET | `/api/agent` | implemented |
| GET | `/api/command` | implemented |
| DELETE | `/api/credential/{credentialID}` | explicit gap (503 backend unavailable) |
| PATCH | `/api/credential/{credentialID}` | explicit gap (503 backend unavailable) |
| GET | `/api/event` | implemented |
| GET | `/api/fs/find` | implemented |
| GET | `/api/fs/list` | implemented |
| GET | `/api/fs/read/*` | implemented |
| GET | `/api/health` | implemented |
| GET | `/api/integration` | implemented |
| DELETE | `/api/integration/attempt/{attemptID}` | explicit gap (503 backend unavailable) |
| GET | `/api/integration/attempt/{attemptID}` | explicit gap (503 backend unavailable) |
| POST | `/api/integration/attempt/{attemptID}/complete` | explicit gap (503 backend unavailable) |
| GET | `/api/integration/{integrationID}` | implemented |
| POST | `/api/integration/{integrationID}/connect/key` | explicit gap (503 backend unavailable) |
| POST | `/api/integration/{integrationID}/connect/oauth` | explicit gap (503 backend unavailable) |
| GET | `/api/location` | implemented |
| GET | `/api/model` | implemented |
| GET | `/api/permission/request` | implemented |
| GET | `/api/permission/saved` | implemented |
| DELETE | `/api/permission/saved/{id}` | implemented |
| GET | `/api/provider` | implemented |
| GET | `/api/provider/{providerID}` | implemented |
| GET | `/api/pty` | implemented |
| POST | `/api/pty` | implemented |
| DELETE | `/api/pty/{ptyID}` | implemented |
| GET | `/api/pty/{ptyID}` | implemented |
| PUT | `/api/pty/{ptyID}` | implemented |
| GET | `/api/pty/{ptyID}/connect` | implemented |
| POST | `/api/pty/{ptyID}/connect-token` | implemented |
| GET | `/api/question/request` | implemented |
| GET | `/api/reference` | implemented |
| GET | `/api/session` | implemented |
| POST | `/api/session` | implemented |
| GET | `/api/session/active` | implemented |
| GET | `/api/session/prune` | added |
| POST | `/api/session/prune` | added |
| GET | `/api/session/{sessionID}` | implemented |
| POST | `/api/session/{sessionID}/agent` | implemented |
| POST | `/api/session/{sessionID}/compact` | implemented |
| GET | `/api/session/{sessionID}/context` | implemented |
| GET | `/api/session/{sessionID}/event` | implemented |
| GET | `/api/session/{sessionID}/history` | implemented |
| POST | `/api/session/{sessionID}/interrupt` | implemented |
| GET | `/api/session/{sessionID}/message` | implemented |
| GET | `/api/session/{sessionID}/message/{messageID}` | explicit gap (503 backend unavailable) |
| POST | `/api/session/{sessionID}/model` | implemented |
| GET | `/api/session/{sessionID}/permission` | implemented |
| POST | `/api/session/{sessionID}/permission` | explicit gap (503 backend unavailable) |
| GET | `/api/session/{sessionID}/permission/{requestID}` | explicit gap (503 backend unavailable) |
| POST | `/api/session/{sessionID}/permission/{requestID}/reply` | implemented |
| POST | `/api/session/{sessionID}/prompt` | implemented |
| GET | `/api/session/{sessionID}/question` | implemented |
| POST | `/api/session/{sessionID}/question/{requestID}/reject` | implemented |
| POST | `/api/session/{sessionID}/question/{requestID}/reply` | implemented |
| POST | `/api/session/{sessionID}/revert/clear` | implemented |
| POST | `/api/session/{sessionID}/revert/commit` | implemented |
| POST | `/api/session/{sessionID}/revert/stage` | implemented |
| POST | `/api/session/{sessionID}/wait` | implemented |
| GET | `/api/skill` | implemented |
<!-- generated:END api-operations -->

## v1 plugin compatibility routes

Derived from `zuno_server::V1_SURFACE`. This is **not** upstream's full v1 surface:
it is the set of routes the installed JavaScript plugins were measured calling,
each carrying its callsite evidence. `crates/zuno-server/tests/compat_v1.rs`
asserts every route has a recorded callsite and that none answers 404.

<!-- generated:BEGIN v1-summary -->
**20 v1 routes** are registered from measured installed-plugin callsites. A route with no recorded callsite is scope creep, and a test fails on it.

Registering a route is not the same as backing it: **14 of the 20 do real local work**, while **6 of the 20 answer `501 not_implemented`**. 0 of those 6 name a served `/api` alternative; the other 6 have no served `/api` spelling here. The generated route table below names every backing. The installed auth plugins' `auth.set` and provider OAuth routes are served; the remaining gaps are non-authentication operations. These figures come from `zuno_server::v1_coverage()`, which counts the same route and backend tables the server mounts.
<!-- generated:END v1-summary -->

<!-- generated:BEGIN v1-routes -->
| method | path | SDK method | backing | `/api` alternative |
|---|---|---|---|---|
| PUT | `/auth/{providerID}` | `client.auth.set` | local-auth-store | none served here |
| POST | `/log` | `client.app.log` | not-implemented | none served here |
| GET | `/agent` | `client.app.agents` | api-adapter:agent | `GET /api/agent` |
| GET | `/config` | `client.config.get` | not-implemented | none served here |
| GET | `/provider` | `client.provider.list` | api-adapter:provider | `GET /api/provider` |
| POST | `/provider/{providerID}/oauth/authorize` | `client.provider.oauth.authorize` | local-provider-oauth | none served here |
| POST | `/provider/{providerID}/oauth/callback` | `client.provider.oauth.callback` | local-provider-oauth | none served here |
| GET | `/session` | `client.session.list` | api-adapter:session-list | `GET /api/session` |
| POST | `/session` | `client.session.create` | api-adapter:session-create | `POST /api/session` |
| GET | `/session/status` | `client.session.status` | not-implemented | none served here |
| GET | `/session/{sessionID}` | `client.session.get` | api-adapter:session-get | `GET /api/session/{sessionID}` |
| PATCH | `/session/{sessionID}` | `client.session.update` | not-implemented | none served here |
| GET | `/session/{sessionID}/children` | `client.session.children` | not-implemented | none served here |
| GET | `/session/{sessionID}/todo` | `client.session.todo` | not-implemented | none served here |
| POST | `/session/{sessionID}/abort` | `client.session.abort` | api-adapter:session-abort | `POST /api/session/{sessionID}/interrupt` |
| POST | `/session/{sessionID}/summarize` | `client.session.summarize` | api-adapter:session-summarize | `POST /api/session/{sessionID}/compact` |
| GET | `/session/{sessionID}/message` | `client.session.messages` | api-adapter:session-messages | `GET /api/session/{sessionID}/message` |
| POST | `/session/{sessionID}/message` | `client.session.prompt` | api-adapter:session-prompt | `POST /api/session/{sessionID}/prompt` |
| POST | `/session/{sessionID}/prompt_async` | `client.session.promptAsync` | api-adapter:session-prompt-async | `POST /api/session/{sessionID}/prompt` |
| POST | `/tui/show-toast` | `client.tui.showToast` | local-toast-sink | none served here |
<!-- generated:END v1-routes -->

## Surfaces compared differentially

`crates/zuno-testkit/tests/compat_suite.rs` used to write a per-surface verdict to
`target/compat/compat-report.json`. That suite is **deleted** and nothing writes
that file any more, so this section no longer points at it — a page telling a
reader to run a test that does not exist is worse than a page that says what is
actually asserted.

The `known_gaps` half of that report was moved into
`zuno_testkit::compat_report::known_gaps`, which renders the generated
[Known gaps](#known-gaps) table above. So a gap is now published in a committed
document rather than in an artifact nobody commits, and it is checked by:

```sh
cargo test -p zuno-cli --test docs
```

What remains of the comparison is per-surface and named. Every implemented CLI
command's normalized exit status, stdout and stderr are compared against the
installed pinned release by `cargo test -p zuno-cli --test cli_parity`, whose
exemption floor is frozen by name and whose every exemption must keep a two-sided
witness. Three surfaces are deliberately **never** compared, and each says so
where it lives: live provider wire bytes (the harness has no HTTP client by
construction), TUI rendering byte-for-byte (an equivalent interface was the goal,
not a pixel-exact reproduction of OpenTUI), and the ACP transport (validated
against the real SDK instead of against the binary).
