# Compatibility matrix

Every surface this port claims, and its state against upstream `opencode`
1.18.13. Four states are used throughout:

- **implemented** — registered and backed by a handler that does the work.
- **registered (501 stub)** — the path and method exist so a client binds, but
  the handler answers `501 Not Implemented`. A route in this state is a
  compatibility seam, not a capability.
- **diverged / added** — present here and deliberately different from, or absent
  in, upstream. Every one is an entry in [divergences.md](divergences.md).
- **rejected** — registered so that invoking it produces a migration message
  instead of a silent failure.
- **not-registered** — deliberately absent, with a named owner.

Every table on this page is generated from the code it describes. Regenerate with:

```sh
OC_DOCS_REGENERATE=1 cargo test -p oc-cli --test docs
```

## Declared divergences

<!-- generated:BEGIN divergence-index -->
| # | id | surface |
|---:|---|---|
| 1 | [`session-list-default-sort`](divergences.md#session-list-default-sort) | CLI `session list`; HTTP `GET /api/session`; `oc-db` session listing |
| 2 | [`tool-output-filename-carries-session`](divergences.md#tool-output-filename-carries-session) | on-disk `$XDG_DATA_HOME/opencode/tool-output/tool_<session>_<uuidv7>` |
| 3 | [`no-eager-directory-creation`](divergences.md#no-eager-directory-creation) | process startup; `oc-paths` layout getters |
| 4 | [`split-version-identity`](divergences.md#split-version-identity) | CLI `--version` and `--version --long`; the npm plugin compatibility gate |
| 5 | [`execute-parameter-contract`](divergences.md#execute-parameter-contract) | tool `execute` — the model-facing parameter schema |
| 6 | [`c8-maintenance-endpoints`](divergences.md#c8-maintenance-endpoints) | HTTP `GET /api/session/prune`, `POST /api/session/prune` |
| 7 | [`provider-coverage-by-wire-family`](divergences.md#provider-coverage-by-wire-family) | provider selection; `oc-provider-compatible` family routing and its diagnostics |
| 8 | [`cross-session-resident-memory`](divergences.md#cross-session-resident-memory) | system-prompt resident blocks; model-facing `memory` tool; post-response reflection |
<!-- generated:END divergence-index -->

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
| `memory.project_char_limit` | `3000` | cap `<worktree>/.opencode/RULES.md` in Unicode scalar values |
| `memory.nudge_interval` | `10` | periodic reflection cadence in delivered turns; `0` disables only that trigger |

Reflection must not learn any of these negative cases:
- Environment-dependent failures: missing binaries, fresh-install errors, post-migration path mismatches, 'command not found', unconfigured credentials, uninstalled packages. The user can fix these — they are not durable rules.
- Negative claims about tools or features ('browser tools do not work', 'X tool is broken', 'cannot use Y from execute_code'). These harden into refusals the agent cites against itself for months after the actual problem was fixed.
- Session-specific transient errors that resolved before the conversation ended. If retrying worked, the lesson is the retry pattern, not the original failure.
- One-off task narratives. A user asking 'summarize today's market' or 'analyze this PR' is not a class of work that warrants a skill.
- Unresolved failures: if the session ended WITHOUT actually finding a working method — you tried several things, none worked, and told the user to check manually — do NOT write those attempts up as a 'reliable workflow' or 'recommended approach'. That presents an untested sequence of failures as validated guidance a future session will trust and repeat. Either say 'Nothing to save', or, only if you are independently confident of a real working alternative (not something you are merely guessing might work), capture ONLY that alternative — never the dead ends, and never dressed up as best practice.
<!-- generated:END cross-session-memory -->

## CLI commands

Derived from `oc_cli::dispositions()` — the same table
`crates/oc-cli/tests/surface.rs::surface_registered_commands_match_their_dispositions`
asserts against the registered `clap` tree, and
`surface_every_upstream_command_has_exactly_one_disposition` asserts against a
committed capture of upstream 1.18.13's command symbols. So a command that gains
or loses a registration cannot pass while this table says otherwise.

Of upstream's 23 commands: 12 implemented, 8 rejected, 3 not-registered.
A rejected command is still registered, so invoking it explains
the replacement instead of reporting an unknown command. A not-registered one
names the owner of the work it waits on.

The `why` column is the reason recorded in the code, reproduced verbatim rather
than paraphrased. `todo N` references are this port's own build plan; treat them
as identifiers for the work that owns a surface, not as anything a user needs.

<!-- generated:BEGIN cli-disposition -->
| upstream symbol | command | disposition | why |
|---|---|---|---|
| `AcpCommand` | `acp` | not-registered | todo 78 owns the oc-acp protocol adapter; registering it before that handler exists would advertise a server that cannot speak ACP |
| `AgentCommand` | `agent` | implemented | registered through the headless-command seam for todo 56 |
| `AttachCommand` | `attach` | not-registered | attach requires the TUI client and terminal lifecycle owned by the TUI wave; no headless substitute is equivalent |
| `ConsoleCommand` | `console` | rejected | the hosted OpenCode Console is excluded from this Rust port's local-agent scope; use `providers` (alias `auth`) for local credentials instead |
| `DbCommand` | `db` | implemented | registered through the headless-command seam for todo 56 and the maintenance extensions in todo 84 |
| `DebugCommand` | `debug` | implemented | registered through the headless-command seam for todo 56 |
| `ExportCommand` | `export` | implemented | prints one session's whole transcript as JSON, byte-compared against the released binary's own export, with `--sanitize` redacting the same fields |
| `GenerateCommand` | `generate` | rejected | the command is a TypeScript source-tree SDK/OpenAPI generator that depends on Prettier and is excluded from the runtime binary; use the server's `/openapi.json` document instead |
| `GithubCommand` | `github` | rejected | the hosted GitHub agent is outside the local-agent scope; run `opencode-rust run` from the CI workflow instead |
| `ImportCommand` | `import` | implemented | reads a document `export` produced back into this checkout's database; share-URL imports are not accepted because the hosted share service is outside this port's scope |
| `McpCommand` | `mcp` | implemented | registered through the headless-command seam for todo 56 |
| `ModelsCommand` | `models` | implemented | registered through the headless-command seam for todo 56 |
| `PluginCommand` | `plugin` | not-registered | plugin installation must wait for todo 60's resident JavaScript host and compatibility gate; accepting installs before plugins can load would corrupt configuration |
| `PrCommand` | `pr` | rejected | the GitHub checkout helper is excluded from the local-agent runtime; use `gh pr checkout <number>` and then `opencode-rust run` instead |
| `ProvidersCommand` | `providers` | implemented | registered with the upstream `auth` alias through the headless-command seam for todo 56 |
| `RunCommand` | `run` | implemented | registered through the headless-command seam for todo 56 |
| `ServeCommand` | `serve` | implemented | registered through the headless-command seam; todo 56 wraps oc-server's public builder rather than duplicating its server logic |
| `SessionCommand` | `session` | implemented | registered through the headless-command seam for todo 56 and session maintenance todos 80-85 |
| `StatsCommand` | `stats` | rejected | upstream stats reads the excluded stats package's session SQL directly; use `db stats` from todo 84 instead |
| `TuiThreadCommand` | `tui` | implemented | registered as `tui` and as the bare invocation upstream spells `$0`; it boots oc-tui's application over the terminal lease from todo 73 and the views from todo 76 |
| `UninstallCommand` | `uninstall` | rejected | self-uninstallation is excluded from the runtime; remove `opencode-rust` with the package manager or installer that placed it |
| `UpgradeCommand` | `upgrade` | rejected | the TypeScript self-updater cannot safely replace this Rust artifact and is excluded; install the desired release through the Rust release installer instead |
| `WebCommand` | `web` | rejected | the bundled hosted web application is excluded from this headless Rust scope; use `serve` and connect a supported client instead |
<!-- generated:END cli-disposition -->

## HTTP `/api` operations

Derived by set-differencing the document `oc_server::api::openapi()` serves
against the committed capture of the 1.18.12 release's document
(`.omo/fixtures/oracle-openapi-1.18.12.json`), then probing each served route
through the real router and recording which answer `501`.

**56 of the 58 upstream operations are served**, plus **2 operations added** for
session retention (the declared `c8-maintenance-endpoints` divergence). Of the
served set, **45 registered as a 501 stub**: the path and method exist so an SDK
binds and does not 404, but no handler does the work yet. Read the stub count as
the honest size of the remaining work, not as coverage.

The two absent operations are both SSE event streams — `GET /api/event` and
`GET /api/session/{sessionID}/event`. An equivalent stream is served at `/event`,
so the capability exists while the upstream paths do not. That is a gap, and
`crates/oc-testkit/tests/compat_suite.rs::api_operations_are_a_superset_of_upstream_minus_the_two_known_gaps`
asserts the absent set is *exactly* those two, so a third absence fails rather
than quietly widening the exemption.

<!-- generated:BEGIN api-operations -->
| method | path | state |
|---|---|---|
| GET | `/api/agent` | registered (501 stub) |
| GET | `/api/command` | registered (501 stub) |
| DELETE | `/api/credential/{credentialID}` | registered (501 stub) |
| PATCH | `/api/credential/{credentialID}` | registered (501 stub) |
| GET | `/api/event` | not-registered |
| GET | `/api/fs/find` | registered (501 stub) |
| GET | `/api/fs/list` | registered (501 stub) |
| GET | `/api/fs/read/*` | registered (501 stub) |
| GET | `/api/health` | implemented |
| GET | `/api/integration` | registered (501 stub) |
| DELETE | `/api/integration/attempt/{attemptID}` | registered (501 stub) |
| GET | `/api/integration/attempt/{attemptID}` | registered (501 stub) |
| POST | `/api/integration/attempt/{attemptID}/complete` | registered (501 stub) |
| GET | `/api/integration/{integrationID}` | registered (501 stub) |
| POST | `/api/integration/{integrationID}/connect/key` | registered (501 stub) |
| POST | `/api/integration/{integrationID}/connect/oauth` | registered (501 stub) |
| GET | `/api/location` | implemented |
| GET | `/api/model` | registered (501 stub) |
| GET | `/api/permission/request` | registered (501 stub) |
| GET | `/api/permission/saved` | registered (501 stub) |
| DELETE | `/api/permission/saved/{id}` | registered (501 stub) |
| GET | `/api/provider` | registered (501 stub) |
| GET | `/api/provider/{providerID}` | registered (501 stub) |
| GET | `/api/pty` | implemented |
| POST | `/api/pty` | implemented |
| DELETE | `/api/pty/{ptyID}` | implemented |
| GET | `/api/pty/{ptyID}` | implemented |
| PUT | `/api/pty/{ptyID}` | implemented |
| GET | `/api/pty/{ptyID}/connect` | registered (501 stub) |
| POST | `/api/pty/{ptyID}/connect-token` | registered (501 stub) |
| GET | `/api/question/request` | registered (501 stub) |
| GET | `/api/reference` | registered (501 stub) |
| GET | `/api/session` | implemented |
| POST | `/api/session` | implemented |
| GET | `/api/session/active` | implemented |
| GET | `/api/session/prune` | added |
| POST | `/api/session/prune` | added |
| GET | `/api/session/{sessionID}` | implemented |
| POST | `/api/session/{sessionID}/agent` | registered (501 stub) |
| POST | `/api/session/{sessionID}/compact` | registered (501 stub) |
| GET | `/api/session/{sessionID}/context` | registered (501 stub) |
| GET | `/api/session/{sessionID}/event` | not-registered |
| GET | `/api/session/{sessionID}/history` | registered (501 stub) |
| POST | `/api/session/{sessionID}/interrupt` | registered (501 stub) |
| GET | `/api/session/{sessionID}/message` | registered (501 stub) |
| GET | `/api/session/{sessionID}/message/{messageID}` | registered (501 stub) |
| POST | `/api/session/{sessionID}/model` | registered (501 stub) |
| GET | `/api/session/{sessionID}/permission` | registered (501 stub) |
| POST | `/api/session/{sessionID}/permission` | registered (501 stub) |
| GET | `/api/session/{sessionID}/permission/{requestID}` | registered (501 stub) |
| POST | `/api/session/{sessionID}/permission/{requestID}/reply` | registered (501 stub) |
| POST | `/api/session/{sessionID}/prompt` | registered (501 stub) |
| GET | `/api/session/{sessionID}/question` | registered (501 stub) |
| POST | `/api/session/{sessionID}/question/{requestID}/reject` | registered (501 stub) |
| POST | `/api/session/{sessionID}/question/{requestID}/reply` | registered (501 stub) |
| POST | `/api/session/{sessionID}/revert/clear` | registered (501 stub) |
| POST | `/api/session/{sessionID}/revert/commit` | registered (501 stub) |
| POST | `/api/session/{sessionID}/revert/stage` | registered (501 stub) |
| POST | `/api/session/{sessionID}/wait` | registered (501 stub) |
| GET | `/api/skill` | registered (501 stub) |
<!-- generated:END api-operations -->

## v1 plugin compatibility routes

Derived from `oc_server::V1_SURFACE`. This is **not** upstream's full v1 surface:
it is the set of routes the installed JavaScript plugins were measured calling,
each carrying its callsite evidence. `crates/oc-server/tests/compat_v1.rs`
asserts every route has a recorded callsite and that none answers 404.

**20 v1 routes**, measured against the plugins installed at capture time. A route
with no recorded callsite is scope creep, and a test fails on it.

<!-- generated:BEGIN v1-routes -->
| method | path | SDK method |
|---|---|---|
| PUT | `/auth/{providerID}` | `client.auth.set` |
| POST | `/log` | `client.app.log` |
| GET | `/agent` | `client.app.agents` |
| GET | `/config` | `client.config.get` |
| GET | `/provider` | `client.provider.list` |
| POST | `/provider/{providerID}/oauth/authorize` | `client.provider.oauth.authorize` |
| POST | `/provider/{providerID}/oauth/callback` | `client.provider.oauth.callback` |
| GET | `/session` | `client.session.list` |
| POST | `/session` | `client.session.create` |
| GET | `/session/status` | `client.session.status` |
| GET | `/session/{sessionID}` | `client.session.get` |
| PATCH | `/session/{sessionID}` | `client.session.update` |
| GET | `/session/{sessionID}/children` | `client.session.children` |
| GET | `/session/{sessionID}/todo` | `client.session.todo` |
| POST | `/session/{sessionID}/abort` | `client.session.abort` |
| POST | `/session/{sessionID}/summarize` | `client.session.summarize` |
| GET | `/session/{sessionID}/message` | `client.session.messages` |
| POST | `/session/{sessionID}/message` | `client.session.prompt` |
| POST | `/session/{sessionID}/prompt_async` | `client.session.promptAsync` |
| POST | `/tui/show-toast` | `client.tui.showToast` |
<!-- generated:END v1-routes -->

## Surfaces compared differentially

The compatibility suite records, per surface, whether it was compared against the
real binary, compared with a declared exception, or never compared — and writes
that as `target/compat/compat-report.json`. Read the artifact rather than trusting
a summary here:

```sh
cargo test -p oc-testkit --test compat_suite
cat target/compat/compat-report.json
```

Three surfaces are deliberately **never** compared and say so in the report:
live provider wire bytes (the harness has no HTTP client by construction), TUI
rendering byte-for-byte (an equivalent interface was the goal, not a pixel-exact
reproduction of OpenTUI), and the ACP transport (validated against the real SDK
instead of against the binary).
