# F3 — Manual QA Report, Wave 12

- **Audited HEAD:** `79ea3c3c`
- **Worktree:** `/config/workspace/ProdDir/AI/oc-wt/tF3` (branch `task-F3`)
- **Artifact:** `target/debug/opencode-rust`, built with `cargo build --offline` (Finished dev profile in 33.27s)
- **Verdict:** **CHANGES REQUESTED** — I withdraw my wave-10/11 approval at this HEAD.

## Verdict summary

The three merged todos I was asked to re-verify all hold up under real use:

| todo | surface | result |
| --- | --- | --- |
| 165 | pre-`/api` v1 SDK routes | 11 served / 9 honest 501, shapes converted, **one required field dropped (F3-W12-01)** |
| 166 | stalled-provider idle bound | **PASS** — my wave-9 finding is genuinely closed |
| 167 | plugin SDK `providerID` shape | **PASS** — both shapes work, precedence correct both directions |

All four wave-11 re-checks also pass: plugin auto-discovery from all four directories, the `edit`
permission subject, the migration ceiling through the real `db` command, and `kiro-auth` in `models`.
The gate is exactly 3433 / 0.

I am nevertheless **not approving**, on the strength of one finding I hit while re-checking:

- **F3-W12-03 (high)** — with this machine's real plugin configuration, **every turn fails** with
  `plugin hook failed: plugin oh-my-openagent failed in hook tool.definition`. Proven by removing
  that one config entry and watching the identical turn succeed. It contradicts
  `docs/plugin-authoring.md:88` ("rather than taking the turn down") and contradicts the port's own
  log line in the same run, which says the plugin did not load. It hits the HTTP server as well as
  the CLI, and the message names no cause and no remedy. A default install is unusable for its
  primary purpose.
- **F3-W12-01 (medium, undeclared)** — the v1 `Session` projection drops `slug`, which is `required`
  in both the committed oracle schema and the port's **own** runtime OpenAPI at `/doc`. Undeclared in
  `docs/divergences.*`. Lossy projection; the data exists one layer down.

Two lower-severity observations (F3-W12-02, and the `/agent` schema drift) are recorded in place.

Sequencing note for whoever picks this up: F3-W12-03 is the blocker; F3-W12-01 is a small, contained
fix. Everything else in this report is a pass and needs no action.

## Scope

I approved at waves 10 and 11. This is a re-verification, not a fresh audit. Three todos merged
since (165, 166, 167), each touching a surface I exercise. I re-check those three first, then
re-confirm my wave-11 priority checks, then explore.

## Planned scenarios

### Mandatory — the three merged todos
1. **Todo 165** — pre-`/api` v1 plugin SDK routes now served by adapters. Hit each with `curl`
   against a live server. Do they answer in the *pre-`/api` SDK* shape, or does the `/api` shape
   leak through? Do the still-unbacked routes give the honest 501 hint verified in wave 11?
2. **Todo 166** — stalled-provider idle bound proven at the turn level. Rebuild both socket
   servers: a stall must end the turn with partial text preserved + visible error; a
   slow-but-progressing stream must survive. Third fix to this feature — verify behavior, not
   test names.
3. **Todo 167** — plugin returning the SDK shape (`providerID`) previously had models silently
   dropped. Write a real JS plugin returning the SDK shape; confirm models appear in `models`,
   and endpoint precedence works both directions through that plugin path.

### Mandatory — wave-11 re-confirmation (tree moved)
4. Plugin auto-discovery from all four directories.
5. `edit` permission subject + diff.
6. Migration-ceiling refusal through the real `db` command.
7. `kiro-auth` provider present in `models`.
8. My open wave-11 observations — state which still reproduce.

### Gate
9. `cargo test --workspace --offline` once for context (expect 3433 passing / 0 failed).

### Exploration (after verdict)
10. Break attempts: bad input, missing config, wrong flags, `--help`, empty state, permission
    denial, interrupted streams, concurrent clients.

## Results

### Scenario 1 — Todo 165: pre-`/api` v1 SDK routes (PASS with one defect)

Server: `./target/debug/opencode-rust serve --port 47121 --print-logs` under an isolated
`XDG_DATA_HOME`/`XDG_CONFIG_HOME` in `/tmp/opencode/f3w12/`. Startup printed, correctly:

```
Warning: OPENCODE_SERVER_PASSWORD is not set; server is unsecured.
opencode server listening on http://127.0.0.1:47121
```

I probed all 20 routes from `docs/v1-surface-capture.md` with `curl`. **11 are served, 9 answer 501**
(11+9 = 20, no route unaccounted for):

| verb + path | code | body (truncated) |
| --- | --- | --- |
| `GET /agent` | 200 | array of 7 agents |
| `GET /provider` | 200 | `{"all":[…],"default":…,"connected":…}` |
| `GET /session` | 200 | `[]` then `array[3]` |
| `POST /session` | 200 | unwrapped Session |
| `GET /session/{id}` | 200 | unwrapped Session |
| `POST /session/{id}/abort` | 200 | `true` |
| `GET /session/{id}/message` | 200 | `[]` |
| `POST /session/{id}/message` | 200 | `{"info":{…},…}` |
| `POST /session/{id}/prompt_async` | 204 | (empty) |
| `POST /session/{id}/summarize` | 500 | `mutation_failed … NoCompactableHistory` (legitimate domain error on an empty session) |
| `POST /tui/show-toast` | 200 | `true` |
| `GET /config` | 501 | honest hint |
| `GET /session/status` | 501 | honest hint |
| `PUT /auth/{providerID}` | 501 | honest hint |
| `POST /log` | 501 | honest hint |
| `POST /provider/{id}/oauth/authorize` | 501 | honest hint |
| `POST /provider/{id}/oauth/callback` | 501 | honest hint |
| `PATCH /session/{id}` | 501 | honest hint |
| `GET /session/{id}/children` | 501 | honest hint |
| `GET /session/{id}/todo` | 501 | honest hint |

**Does the `/api` shape leak through? Mostly no — the conversion is real.** Compare the same
create call on both surfaces:

```
POST /api/session -> {"data":{"id":"ses_…","projectId":"global","parentId":null,"slug":"ses_…","workspaceId":null,"path":"", …}}
POST /session     -> {"id":"ses_…","projectID":"global","parentID":null, …}
```

The v1 answer is correctly **unwrapped** (no `{"data":…}` envelope) and correctly renames
`projectId`/`parentId` to the oracle's `projectID`/`parentID`. Boolean-returning routes return bare
`true`, and `prompt_async` returns `204`. That is the pre-`/api` SDK shape, not the `/api` shape.

**The honest 501 hint I verified in wave 11 is intact and has improved.** Full body:

```json
{"error":{"apiAlternative":null,"backing":"not-implemented",
  "callers":["@sunerpy/oh-my-openagent@4.21.0"],"code":"not_implemented",
  "hint":"this pre-/api route is registered but has no local backend, and `client.session.status` has no served /api equivalent here; there is no alternative call that works today",
  "message":"`client.session.status` has no local backend in this build",
  "route":"GET /session/status","sdkMethod":"client.session.status",
  "surfaceCoverage":"9 of 20 measured pre-/api routes have no local backend (0 of those can name a served /api alternative); 11 served locally"}}
```

The `surfaceCoverage` counter (9 unbacked / 11 served) matches exactly what I measured by hand, so
it is computed, not hardcoded. It names the calling plugin and the SDK method. Good.

#### FINDING F3-W12-01 (defect, undeclared) — the v1 Session adapter drops the required `slug` field

The v1 Session projection omits `slug`, `path` and `workspaceID`, all three of which the port's own
`/api` handler already produces for the same session. Measured key sets:

```
v1   POST /session      -> ['directory','id','parentID','projectID','time','title','version']
/api POST /session.data -> ['directory','id','parentId','path','projectId','slug','time','title','version','workspaceId']
```

`slug` is not optional. It is **required** in two independent places:

- the committed oracle schema — `Session.required = ['id','slug','projectID','directory','title','version','time']`, with `additionalProperties: false`;
- the port's **own** runtime OpenAPI served at `/doc` — `Session required= ['id','projectId','slug','directory','title','version','time']`.

So this is not a fixture-version artifact: the build contradicts its own published schema. A
pre-`/api` SDK client that reads `session.slug` gets `undefined` on `GET /session`,
`POST /session` and `GET /session/{id}`. `grep -n -i slug docs/divergences.md docs/divergences.toml
docs/v1-surface-capture.md` returns nothing, so the omission is **undeclared**. The data is present
one layer down, so this is a lossy projection rather than a missing capability.

#### Minor observations (not defects)

- `GET /agent` returns `builtIn`, `maxSteps`, `tools`, which are absent from the oracle `Agent`
  schema (`additionalProperties: false`), and omits the oracle's `hidden`, `native`, `steps`,
  `temperature`, `topP`, `variant`. All four oracle-**required** keys (`name`, `mode`, `permission`,
  `options`) are present, and the fixture is 1.18.12 while the port targets 1.18.15, so this is
  plausibly legitimate drift. I could not verify 1.18.15's `Agent` schema — no oracle for that
  version is in the tree. Reporting as unconfirmed, low severity; `maxSteps` vs `steps` looks like a
  rename worth declaring.
- The v1 routes do not appear in the runtime OpenAPI document (`/doc` lists 52 paths, all `/api`).
  Defensible for a plugin compat shim, but it means a plugin author cannot discover them from the
  published doc.
- An unknown route (`GET /totally/bogus`) returns `404` with an **empty body**, whereas the
  registered-but-unbacked routes return a richly diagnostic 501. Cosmetic asymmetry only.

### Scenario 2 — Todo 166: stalled-provider idle bound at the turn level (PASS)

This is my own wave-9 finding and the third fix to it, so I verified the user-visible behavior
end-to-end rather than reading tests. I rebuilt both socket servers as fake OpenAI-compatible
SSE endpoints (`/tmp/opencode/f3w12/fake/srv.py`):

- **stall server** (`:47201`) — emits two text deltas, then goes silent forever without closing;
- **slow-but-progressing server** (`:47202`) — emits a delta every 2s for 6 ticks, then
  `finish_reason:"stop"` + `[DONE]` and a clean chunked close.

Both were confirmed with a bounded `curl --max-time -N` before use. They were wired in as two real
providers via config `provider.fakestall` / `provider.fakeslow` with
`options.baseURL` pointing at each, and both appeared in the CLI:

```
$ opencode-rust models | grep fake
fakeslow/fake-model
fakestall/fake-model
```

The bound is `OPENCODE_STREAM_IDLE_TIMEOUT_SECS` (default 300s), which I set low to keep runs short.

**Stall — the turn ends, the partial text survives, the error is visible:**

```
$ OPENCODE_STREAM_IDLE_TIMEOUT_SECS=3 opencode-rust run --model fakestall/fake-model "say hello"
REAL EXIT=1  elapsed=7s
--- stdout ---
PARTIAL-ONE PARTIAL-TWO
--- stderr ---
transient provider failure (status=None): provider `fakestall` response stream idle timeout after 3s; raise OPENCODE_STREAM_IDLE_TIMEOUT_SECS for slower providers
```

Everything I needed to see is here:

- the turn **terminates** (7s) instead of hanging forever, which was the original wave-9 defect;
- the **partial text is preserved** and emitted on **stdout**;
- a **visible, actionable error** goes to **stderr** — it names the provider, the elapsed bound, and
  the exact environment variable to raise;
- **exit code is 1**, so a script can detect the failure;
- stdout/stderr separation is clean, so piping the transcript does not swallow the error.

The fake server log shows two connections per run, and elapsed ≈ 2× the bound, so the idle timeout
is classified transient and retried once before the turn gives up. The partial text is **not**
duplicated across the two attempts — stdout contains exactly one copy.

**Slow-but-progressing — survives, as it must:**

```
$ OPENCODE_STREAM_IDLE_TIMEOUT_SECS=3 opencode-rust run --model fakeslow/fake-model "say hello"
REAL EXIT=0  elapsed=24s
--- stdout ---
tick0 tick1 tick2 tick3 tick4 tick5
--- stderr ---  (empty)
```

All six deltas arrived with 2s gaps under a 3s bound, exit 0, no error. So the bound measures the
**gap between chunks**, not total wall-clock duration — a slow provider is not killed merely for
being slow. That is the correct distinction and it is the thing the previous two fixes got wrong.

The server log showed the 6-tick stream twice (24s ≈ 2×12s), which I chased down rather than assume:
it is auto-title generation, confirmed by `session list` showing the title
`tick0 tick1 tick2 tick3 tick4 t…` derived from the model output. Expected behavior, not a defect.

**Verdict: my wave-9 finding is closed at the user-visible level.** I consider this fix real.

### Scenario 3 — Todo 167: plugin returning the SDK `providerID` shape (PASS)

I wrote a real JavaScript plugin, auto-discovered from `$XDG_CONFIG_HOME/opencode/plugin/`, whose
`provider` hook returns a model map in the **SDK shape** — each model carrying `providerID`, the key
that was previously dropped. To make the outcome unambiguous I had the same plugin return four
models at once:

| model id | owner key supplied | expectation |
| --- | --- | --- |
| `sdk-shape-a` | `providerID` (SDK shape) | must appear — this is the fix |
| `internal-b` | `provider_id` (host-internal shape) | must appear — must not regress |
| `no-owner-c` | neither | must be rejected |
| `bad-cost-d` | `providerID` + `cost: "not-an-object"` | must be rejected |

Result from the shipped binary:

```
$ opencode-rust models | grep f3plug
f3plug/internal-b
f3plug/sdk-shape-a
```

**The SDK shape is converted and both shapes survive.** `sdk-shape-a` appears, so `providerID` →
`provider_id` conversion happens at the plugin boundary, and `internal-b` still appears, so the
conversion did not break the pre-existing internal shape. The two genuinely malformed models are
rejected, and at `--log-level DEBUG` the rejection names the model and the precise reason:

```
DEBUG oc_plugin::js::bridge: skipped a plugin model this host could not decode model=bad-cost-d error=invalid type: string "not-an-object", expected struct ModelCost
DEBUG oc_plugin::js::bridge: skipped a plugin model this host could not decode model=no-owner-c error=missing field `provider_id`
```

One behavior worth knowing: the hook **replaces** rather than merges. My config declared
`f3plug.models = {"placeholder-from-config": …}`; after the plugin loaded, that placeholder was gone
and only the plugin's two models remained. Also, the hook only *enriches a provider that already
exists in the catalog* — a `provider` hook for an id absent from config contributes nothing and is
skipped without a message. Both match the documented upstream contract, but the second cost me time
before I declared the provider in config, and a plugin author would hit the same wall.

#### Endpoint precedence through the plugin path — verified in both directions

I gave the two fake servers from Scenario 2 a second job here: they are a **physical discriminator**.
Their outputs are distinguishable (`PARTIAL-ONE…` + idle error vs `tick0…tick5`), so I can tell
which socket was actually dialled rather than inferring it from config.

| # | config `options` | plugin model `api.url` | observed stdout | STALL hits | SLOW hits | winner |
| --- | --- | --- | --- | --- | --- | --- |
| A | `baseURL` = STALL `:47201` | SLOW `:47202` | `PARTIAL-ONE PARTIAL-TWO` + idle error, exit 1 | 2 | 0 | config `baseURL` |
| B | `baseURL` = SLOW `:47202` | STALL `:47201` | `tick0 … tick5`, exit 0 | 0 | 2 | config `baseURL` |
| C | `endpoint` = SLOW `:47202`, `baseURL` = STALL `:47201` | STALL `:47201` | `tick0 … tick5`, exit 0 | 0 | 2 | config `endpoint` |

Precedence is **consistent and correct in both directions**: config `options.endpoint` beats config
`options.baseURL`, which beats the plugin model's `api.url`. Rows A and B are the same config key
winning against opposite plugin values, which rules out coincidence — in each case the losing
server's log shows **zero** connections, so the request genuinely never went there.

#### OBSERVATION F3-W12-02 — a rejected plugin model is invisible at default verbosity

The class of bug todo 167 fixed is closed for the SDK shape, but the *mechanism* that hid it is
still in place for every other decode failure. At default log level:

```
$ opencode-rust models | grep -i "f3plug\|skip\|warn\|error"
f3plug/internal-b
f3plug/sdk-shape-a
```

`no-owner-c` and `bad-cost-d` are gone with **no warning, no diagnostic, no exit-code change**. The
reason is only reachable via `--log-level DEBUG`. The *skip* policy itself is deliberate and I agree
with it — the source comment says dropping a whole provider over one bad model is the worse outcome.
The problem is purely diagnosability: a plugin author whose model silently fails to appear gets no
hint that the host rejected it or why, which is exactly the debugging dead-end that made todo 167
take a full cycle to find. A single `WARN` naming the model and reason would close it. Not a
correctness defect in the fixed path; I am reporting it as the residual of one.

#### Harness note (not a product defect)

My first attempts at this scenario failed with the JS host reporting
`mise ERROR bun is not a valid shim` and then `plugin … did not connect back within 30000 ms`. I
tracked that to **my own test harness**: the JS plugin host runs `bun`, this machine's `bun` is a
mise shim, and mise resolves its installs under `$XDG_DATA_HOME` and its global config under
`$XDG_CONFIG_HOME` — both of which I had redirected to scratch directories for isolation. Pointing
`PATH` at the real binary (`/config/.local/share/mise/installs/bun/1.3.14/bin`) fixed it. **The
product did nothing wrong here.** Two things it did right are worth recording: a JS runtime that
cannot start is a `WARN` that names the plugin and the reason, and the `models` command still
completed and printed all 321 models rather than failing the whole invocation. The only rough edge
is that the 30-second connect-back timeout is paid on *every* invocation while a runtime is broken,
which makes the CLI feel hung with no message at default verbosity.

### Scenario 4 — wave-11 re-check: plugin auto-discovery from all four directories (PASS)

Built five plugin files across every documented location and ran `models` from inside the project
directory with `XDG_CONFIG_HOME`, `HOME`, and `OPENCODE_CONFIG_DIR` all pointed at scratch trees.
All five were found, in the documented order, with correct provenance:

```
auto-discovered … /disc/xdg/opencode/plugin/d1_xdg.js            scope=Global
auto-discovered … /disc/xdg/opencode/plugins/d1b_xdg_plugins.js  scope=Global
auto-discovered … /disc/proj/.opencode/plugin/d2_project.js      scope=Local
auto-discovered … /disc/home/.opencode/plugin/d3_home.js         scope=Global
auto-discovered … /disc/ocdir/plugin/d4_ocdir.js                 scope=Global
```

Both `plugin/` and `plugins/` are scanned, `plugin/` before `plugins/`, and the project `.opencode`
directory is correctly the only one marked `scope=Local`. **My three-wave finding stays closed.**

### Scenario 5 — wave-11 re-check: `edit` permission subject and diff (PASS on subject)

With `"permission": {"edit": "ask"}` and a provider that issues a real `edit` tool call in a
non-interactive `run`, the refusal is the best error message I have seen in this product:

```
denied `edit`: permission `edit` resolves to ask for /tmp/opencode/f3w12/permproj/target.txt, and
this non-interactive run has nobody to ask; add
`"permission": {"edit": {"/tmp/opencode/f3w12/permproj/target.txt": "allow"}}` to your
configuration to authorize it
```

The **subject is the concrete file path**, not a generic `*`, and the message hands over a
paste-ready config fragment scoped to exactly that path. The file was verified unchanged, and the
model received `tool edit was denied by the permission layer` — so the model is told the truth too.

Getting to this point required a two-step `read` → `edit` flow, because `edit` correctly refuses to
touch a file the session has not read:

```
"output": "tool edit received invalid arguments: File must be read before editing. Use the read tool on /tmp/…/target.txt, then retry the edit."
```

Control with `"edit": "allow"` completed the whole loop and the file really changed
(`line two` → `LINE TWO EDITED`), so the deny path is a genuine block rather than a broken tool.

With `"edit": "deny"` the tool is instead **removed from the exposed registry**, and the model is
told `Unknown tool: edit. Available tools: glob, grep, invalid, memory, read, todowrite, webfetch.`
(`write` is withdrawn too). I checked before reporting this: `crates/oc-tools/src/exposure.rs`
documents deny-means-hide as measured off the real 1.18.12 binary, so it is upstream-consistent, not
a defect. Worth noting only that the *user* sees no statement that their own `deny` policy caused it.

**I could not verify the rendered diff.** The diff belongs to the interactive approval prompt, and I
could not reach an interactive approval: the CLI `run` path has no TTY by construction, and the
server route needs a live approval client. I am not going to claim it works from reading the code.
Unverified, not failed.

### Scenario 6 — wave-11 re-check: migration ceiling through the real `db` command (PASS)

Created a real database with the shipped binary (39 journal rows, newest
`20260622202450_simplify_session_input`), then injected a future migration id and re-ran the real
commands:

```
$ opencode-rust db "select 1"
database migration journal is newer than this binary (known ceiling 20260622202450_simplify_session_input, observed zzzz_from_the_future)
$ echo $?
1
$ opencode-rust session list      # same message, exit 1
```

The refusal names both the ceiling and the offending id, fires before any SQL runs, and applies to
every command that opens the database rather than just `db`. The journal still had exactly 39 rows
afterwards, so the refusal is non-destructive. Correct behavior.

### Scenario 7 — wave-11 re-check: `kiro-auth` provider (PASS)

```
$ opencode-rust models | grep -i kiro
kiro-auth/auto
kiro-auth/claude-haiku-4-5
kiro-auth/claude-opus-4-5
kiro-auth/claude-opus-4-6
kiro-auth/claude-opus-4-7
kiro-auth/claude-opus-4-7-high
…
```

Present, with a full model family. Nine providers resolve in total (`amazon-bedrock`, `awsopenai`,
`google`, `kiro-auth`, `myopenai`, `nwcdai`, `openai`, `zhipuai`, `zhipuai-coding-plan`), 321 models.

### Scenario 8 — FINDING F3-W12-03 (defect, high) — an installed plugin's hook failure kills every turn

This is the most serious thing I found. **With this machine's real configuration, every turn fails**
unless `--pure` is passed:

```
$ opencode-rust run --model faketool/tool-model "edit the file"
plugin hook failed: plugin oh-my-openagent failed in hook tool.definition
$ echo $?
1
```

Nothing else is printed. No assistant text, no tool activity, no remediation.

**Causality is proven, not inferred.** I copied the real `/config/.config/opencode/opencode.json`
into an isolated `XDG_CONFIG_HOME` and ran the identical turn twice, changing exactly one thing:

| plugin list | outcome |
| --- | --- |
| `[antigravity-auth, kiro-auth, oh-my-openagent]` | `plugin hook failed: … hook tool.definition`, exit 1, turn dead |
| `[antigravity-auth, kiro-auth]` | full turn: `read` → `edit` → permission refusal → `DONE-AFTER-TOOL`, exit 0 |

Removing that single entry restores the product. **It is not my fake provider** — the same fake
provider drives a complete, correct turn in the second row.

**It contradicts the documented contract.** `docs/plugin-authoring.md:88` states a plugin that fails
"is disabled with a `PluginDiagnostic` **rather than taking the turn down**." Here a hook failure
takes the turn down.

**It also contradicts the port's own log line in the same run.** At `DEBUG` the binary says it did
*not* load this plugin:

```
WARN oc_cli::cmd::plugin_runtime: JavaScript plugin did not fully load
  plugin=@sunerpy/oh-my-openagent@4.21.0 kind=Compatibility
  plugin declares @opencode-ai/plugin 1.15.13; host reports 1.18.13 surface="turn"
…
plugin hook failed: plugin oh-my-openagent failed in hook tool.definition
```

So a plugin that was version-rejected for the `turn` surface still gets its `tool.definition` hook
(hook 21) invoked, and that hook's failure is fatal. Either the compatibility skip is incomplete, or
hook failures are not isolated; the two log lines cannot both be right. Per `docs/plugin-authoring.md`
an npm plugin outside its `engines.opencode` range is *skipped* — if it were truly skipped, removing
it from config could not have changed the outcome.

**Blast radius covers the server too**, so it is not a CLI-only issue. Driving the same turn through
`POST /api/session/{id}/prompt` and reading `/api/event` (bounded, `--max-time 25`) produced:

```
data: {"data":{"message":"plugin hook failed: plugin oh-my-openagent failed in hook tool.definition",
        "sessionID":"ses_9bdd…"},…,"type":"session.error"}
```

The turn aborts after `assistant.message.created` at `seq=5`, so an API consumer gets a dead session
too.

Secondary problem: the message is undiagnosable. It does not say which plugin file, that the plugin
was version-incompatible, that `--pure` bypasses it, or that removing the entry fixes it. A user with
this config has a completely non-functional product and no path from the message to the cause.

I am **not** claiming the third-party plugin is blameless — it declares an incompatible API version.
The defect is that the host neither isolates its failure nor honors its own skip decision, and the
documentation promises the opposite.

### Scenario 9 — the gate (PASS)

```
$ cargo test --workspace --offline
PASSED=3433 FAILED=0 IGNORED=2
```

Exactly the expected 3433 passing / 0 failed. No `EAGAIN` and no `error[` anywhere in the log, so no
retry was needed. (A "still running" `cargo test` I saw partway through belonged to a concurrent
reviewer in a different worktree, not to my run; my process had already exited.)

### Scenario 10 — exploration: attempts to break it (all handled correctly)

**Bad input and wrong flags** — every message is specific and actionable:

```
$ opencode-rust run --model nosuch/model "hi"   -> Model not found: nosuch/model
$ opencode-rust run --model garbage "hi"        -> model must be provider/model, got "garbage"
$ opencode-rust export ses_doesnotexist         -> Session not found: ses_doesnotexist
$ opencode-rust db "select * from nope"         -> no such table: nope
$ opencode-rust frobnicate                      -> error: unrecognized subcommand 'frobnicate' + usage
$ opencode-rust import bad.json                 -> Invalid JSON in …/bad.json: expected ident at line 1 column 2
$ opencode-rust import nope.json                -> File not found: /tmp/opencode/f3w12/nope.json
```

`import` distinguishes *missing file* from *malformed content*, and the JSON error carries line and
column. Nothing panicked and nothing printed a Rust backtrace.

**Excluded surfaces** explain themselves and point somewhere useful, rather than just failing:

```
`console` is not available: the hosted OpenCode Console is excluded from this Rust port's local-agent
  scope; use `providers` (alias `auth`) for local credentials instead
`upgrade` is not available: the TypeScript self-updater cannot safely replace this Rust artifact and
  is excluded; install the desired release through the Rust release installer instead
```

**HTTP error shapes are consistent across both surfaces.** A bogus session id returns the *same*
structured 404 on the v1 and `/api` routes — so the compat layer is not inventing its own error
vocabulary:

```
GET /session/ses_bogus      -> 404 {"error":{"code":"not_found","message":"session `ses_bogus` was not found"}}
GET /api/session/ses_bogus  -> 404 {"error":{"code":"not_found","message":"session `ses_bogus` was not found"}}
POST /session  (body `{not json`) -> 400 Failed to parse the request body as JSON: key must be a string at line 1 column 2
```

**Concurrency is clean.** 12 simultaneous `POST /api/session` calls all returned `200`, and three
simultaneous SSE subscribers each received their own `server.connected` frame with distinct event
ids. The server answered `GET /api/session` normally afterwards. Every SSE read was bounded with
`--max-time`; I never left a stream open.

**Empty state** — `session list` against a fresh database prints *nothing at all*, not even a header
or a "no sessions" line. Good for piping, mildly surprising interactively. Not a defect.

**TUI** — started under tmux at 200×50. It comes up showing an `idle` status strip and an input
caret, accepts typed input, and on submit renders `> You` / `* Assistant` blocks. Both error paths I
could reach were legible in the pane rather than swallowed:

```
* Assistant
 unrecoverable provider failure (status=Some(404)): Bedrock service error status=404 code=None: None
```

That 404 is a real credential/region failure in this sandbox, not a product defect — I note it only
because the TUI surfaced it clearly with its status code.

**The TUI is also the third surface hit by F3-W12-03.** With the real plugin list restored, the same
turn renders:

```
* Assistant
 plugin hook failed: plugin oh-my-openagent failed in hook tool.definition
```

So that defect reaches the **CLI, the HTTP server, and the TUI** — every entry point the product has.

## What I could not verify, and why

- **The rendered permission diff.** It belongs to the interactive approval prompt. `run` has no TTY
  by construction, and the server's `/api/session/{id}/permission` route needs a live approval client
  I could not stand up within this wave. The permission *subject* is verified (Scenario 5); the diff
  is not. I did not infer it from source.
- **Whether the `/agent` field drift is legitimate.** The only oracle in the tree is 1.18.12 and the
  port targets 1.18.15, so `builtIn`/`maxSteps`/`tools` vs `hidden`/`native`/`steps`/`temperature`/
  `topP`/`variant` could be genuine upstream movement. All oracle-*required* keys are present. Needs
  a 1.18.15 capture to settle; flagged, not asserted.
- **Real provider traffic.** No live credentials work in this sandbox (Bedrock returned 404), so every
  turn I drove used my own fake OpenAI-compatible servers. That is sufficient for stream lifecycle,
  tool dispatch, permission and precedence behavior, but it does not exercise real provider wire
  quirks.
- **The remaining 9 unbacked v1 routes' behavior when backed.** They answer 501 by design; I verified
  the refusal, not any hidden implementation.

## Environment notes

- Two harness mistakes of mine are recorded so nobody re-chases them as product bugs: redirecting
  `XDG_DATA_HOME`/`XDG_CONFIG_HOME` breaks this machine's `mise`-shimmed `bun`, which the JS plugin
  host needs; and the `edit` tool's required `intent` property is a **declared** divergence
  (`execute-parameter-contract` in `docs/divergences.toml`), not a defect.
- `cargo test` was run once and passed; per the brief I did not re-run it.

## Cleanup

All scratch lives under `/tmp/opencode/f3w12/`. Every fake server, every `serve` process and both
tmux sessions were killed; the tmux server is shut down and ports 47121/47131/47201/47202/47205/
47207/47208 are confirmed closed. No product source, test, plan, doc or evidence file other than this
report was modified, and nothing was committed, branched or pushed.

