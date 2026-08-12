# F3 — Real Manual QA, Wave 13

- **Audited HEAD:** `67167fe681e6bd954a5a6fd5e2e6dd8384a74364`
- **Branch / worktree:** `task-F3` @ `/config/workspace/ProdDir/AI/oc-wt/tF3`
- **Artifact under test:** `target/debug/opencode-rust`, built with `cargo build --offline`
- **Role:** reviewer. No product source, test, plan, doc or other evidence file modified.
- **Verdict:** **BLOCK** — 3 high, 2 medium, 2 low. All seven undeclared. See [Verdict](#verdict).

## Summary

The four todos my wave-12 findings drove all do what they claim. Todo 168's hook isolation is real and
its diagnostic is the best failure message in this product; todo 171's `slug` fix is complete and its
"unverifiable" `/agent` classification is one I independently reached and agree with; todo 169's
recorded `tool_result` contract now returns `200`; todo 170's `providerID` projection works both
directions through a real plugin I wrote.

I am blocking on three high-severity defects found by using the product:

- **F3-W13-07** — `tool.definition` (hook 21 of 21) is unusable by **any** JS plugin. A four-line
  no-op hook is disabled on every turn. The plugin receives the data intact and touches nothing; the
  host then reports that the plugin truncated it. **This is the root cause of my wave-12 F3-W12-03**,
  and it means `@sunerpy/oh-my-openagent@4.21.0` was never at fault.
- **F3-W13-05** — the top-level config `model` key is parsed, echoed back by `debug config`, and then
  ignored. The turn silently uses the **catalog's first** model. Proven in both directions with two
  distinguishable fake providers. Affects `run` and the TUI.
- **F3-W13-01** — a plugin `auth.loader` failure is fatal: it kills `run`, kills **`models`** (zero
  output, exit 1), and kills turns through the HTTP server. Triggered by the ordinary "provider
  configured but not logged in" state, because the host resolves `getAuth()` to `null` against the
  `Promise<Auth>` type the plugin compiles against. Same shape as the finding todo 168 just fixed, on
  a path it did not cover.

Plus **F3-W13-03** (the published OpenAPI binds 0 of 60 operations to any request or response body,
while the port points users at it as the replacement for the excluded generator) and **F3-W13-02** (the
documented "version-incompatible plugins are skipped" contract is not what happens). Gate: 3443
passing / 0 failed, as expected — none of the above is visible to it.

## Planned scenarios

### Mandatory — the four todos my wave-12 findings drove

| # | Scenario | Todo | Status |
|---|----------|------|--------|
| S1 | Re-run my wave-12 reproduction: real global plugin config (`@sunerpy/oh-my-openagent@4.21.0`) + a turn. Does the turn complete? Is the diagnostic visible without `--print-logs`? Does it name plugin + hook + actionable cause? | 168 | **FIXED** — turn completes, exit 0, diagnostic visible without `--print-logs`, names plugin+hook+JSON-Pointer cause |
| S2 | Version-incompatible plugin: skipped (per `docs/plugin-authoring.md:37-40`) or loaded-with-warning (`VersionGate::Unsatisfied`)? What does a user see? | 168 | **F3-W13-02** — loaded-with-a-warning, not skipped; warning invisible at default verbosity |
| S3 | v1 `Session` projection: `POST /session`, `GET /session`, `GET /session/{id}` vs the schema the same process publishes at `/doc`. Confirm `slug` present and required-key set satisfied. | 171 | **FIXED** — `slug` present; conformant vs oracle (closed schema) and vs `/doc` on the surface each governs |
| S4 | The `/agent` drift todo 171 recorded as *unverifiable* — do I agree with that classification? | 171 | **AGREE** with "unverifiable"; + **F3-W13-03** found while probing `/doc` |
| S5 | Three v1 routes with recorded plugin contracts — send the real recorded shapes, incl. Antigravity's `tool_result` prompt part. Confirm effect, not just status code. | 169 | `tool_result` now **200** (was 400); 3 caveats: `tool_use_id` dropped, body model ignored, failed turn reported 200 |
| S6 | Real JS plugin reading a model/provider from a hook argument: does it see SDK spelling (`providerID`)? Is a model it supplies accepted? | 170 | **CONFIRMED** both directions; + **F3-W13-06**, and **F3-W13-07** (high) found here |

### Mandatory — re-confirm, since the tree moved

| # | Scenario | Status |
|---|----------|--------|
| S7 | Stalled-provider idle bound (my wave-9 finding, turn-level via todo 166) | PASS — identical to wave 12 |
| S8 | Plugin auto-discovery from all four directories | PASS — all five locations, correct scope |
| S9 | `edit` permission subject + diff | subject PASS; diff now **measured and absent** (detail route is a declared 503 gap) |
| S10 | Migration-ceiling refusal through the real `db` command | PASS — every DB-opening command refuses, non-destructive |
| S11 | `kiro-auth` provider in `models` | PASS — present with full model family in a 313-model catalog |

### Gate

| # | Scenario | Status |
|---|----------|--------|
| S0 | `cargo test --workspace --offline` — expect 3443 passing / 0 failed | PASS — 3443 passed / 0 failed / 2 ignored |

### Exploratory (after the mandatory set is written up)

- Bad input / missing config / wrong flags / `--help` / empty state
- Permission denial, interrupted streams, concurrent HTTP clients

## Results

### Environment note — `bun` had to be put on `PATH` for plugins to load at all

Worth recording, because it changed what I saw. `bun` on this host is a **mise shim** that is broken
in a non-login shell:

```
$ bun --version
mise ERROR bun is not a valid shim. This likely means you uninstalled a tool and the shim does not
  point to anything. Run `mise use <TOOL>` to reinstall the tool.
```

With that broken shim on `PATH`, all three real plugins fail identically and *silently*:

```
WARN … JavaScript plugin did not fully load plugin=opencode-antigravity-auth@1.6.0 hook=None
  kind=FailedToLoad plugin `opencode-antigravity-auth@1.6.0` did not connect back within 30000 ms
```

Two user-visible consequences of that state, neither of which is a hook or auth defect:

1. **Every turn pays 30 s of dead wait**, three times over, before running. Measured:
   `WALL=30.25s` for a turn whose provider answers in milliseconds. Nothing is printed at default
   verbosity — the user sees a 30-second hang with no explanation.
2. The real cause (`bun` is not executable) is only visible at `--log-level DEBUG`, and even there it
   appears as raw plugin stderr, not as a host diagnostic:
   `DEBUG oc_plugin::js::host: javascript plugin stderr plugin=… mise ERROR bun is not a valid shim`.

**OBSERVATION F3-W13-04 (low)** — a missing/broken JS runtime is reported as `FailedToLoad … did not
connect back within 30000 ms`, i.e. as a timeout, after a 30 s stall per plugin, rather than as
"the configured JS runtime could not be executed". The information needed to fix it is captured
(plugin stderr names the exact mise error) but is two verbosity levels below where the user is.

For every scenario below I put the real bun on `PATH`
(`/config/.local/share/mise/installs/bun/1.3.14/bin`) so plugins genuinely load. Everything I report
about hooks and auth loaders is from that state.

### Scenario 1 — Todo 168, the hook path of my wave-12 finding: FIXED

Exact wave-12 reproduction: the real `/config/.config/opencode/opencode.json` copied verbatim into an
isolated `XDG_CONFIG_HOME`, real plugin list intact
(`opencode-antigravity-auth@1.6.0`, `@sunerpy/opencode-kiro-auth@0.20.6`,
`@sunerpy/oh-my-openagent@4.21.0`), real `node_modules` symlinked so `@sunerpy/oh-my-openagent@4.21.0`
resolves exactly as it does on this machine, plus one added fake OpenAI-compatible provider so the
turn actually calls a tool.

**The turn now completes.** No `--print-logs`, no `--pure`:

```
$ opencode-rust run --model faketool/tool-model "read the file"
$ echo $?
0
--- stdout ---
DONE-AFTER-TOOL
--- stderr ---
disabled plugin `oh-my-openagent` after hook `tool.definition` failed: plugin
`@sunerpy/oh-my-openagent@4.21.0` truncated `tool.definition` hook argument 1 at
`/parameters/properties/todos/items/properties/priority/oneOf/0`; refusing to apply any hook mutation
```

Checked against each thing I asked for in wave 12:

| requirement | observed |
| --- | --- |
| turn completes | yes — `DONE-AFTER-TOOL`, exit **0** (was exit 1, no output) |
| diagnostic visible **without** `--print-logs` | yes — printed unconditionally on stderr |
| names the plugin | yes — `oh-my-openagent`, and the full specifier with version |
| names the hook | yes — `tool.definition` |
| actionable cause | yes — `truncated … argument 1 at /parameters/properties/todos/items/properties/priority/oneOf/0`. A JSON Pointer to the exact spot. This is the first time I have been told *what* the plugin did wrong. |
| blast radius contained | yes — only that plugin is disabled; the tool call still executed |
| stream hygiene | clean. `cat -A` confirms stdout is `DONE-AFTER-TOOL$` and the diagnostic is entirely on stderr, so piping the transcript does not swallow it and does not corrupt it |

Causality re-proven by varying only the plugin list, four ways:

| plugin list | outcome |
| --- | --- |
| all three | fails — but for a *different* reason, see Scenario 1b |
| kiro + oh-my-openagent | `DONE-AFTER-TOOL`, exit 0, disable diagnostic on stderr |
| oh-my-openagent only | `DONE-AFTER-TOOL`, exit 0, disable diagnostic on stderr |
| none | `DONE-AFTER-TOOL`, exit 0, no diagnostic |

**The hook-failure half of F3-W12-03 is genuinely closed.** A plugin whose hook fails is now disabled
with a diagnostic and the turn survives, exactly as `docs/plugin-authoring.md:88` promises. I consider
this fix real, and better than I asked for — the JSON-Pointer cause is actionable.

### Scenario 1b — FINDING F3-W13-01 (defect, high) — the *auth-loader* path is still fatal, and now kills `models` too

Same class of failure as F3-W12-03, on a path todo 168 did not cover. With the full real plugin list
and **no stored credential for the provider the plugin claims**, every command dies:

```
$ opencode-rust run --model faketool/tool-model "read the file"
plugin auth loader `google` failed: plugin `opencode-antigravity-auth@1.6.0` failed `call`:
  null is not an object (evaluating 'auth.type')
$ echo $?
1
```

Nothing else on stdout. `WALL=1.01s` — it dies before reaching the provider.

**It is worse than wave 12 in one specific way: `models` dies too.**

```
$ opencode-rust models > out 2> err ; echo $?
1
$ wc -l < out
0
$ cat err
plugin auth loader `google` failed: plugin `opencode-antigravity-auth@1.6.0` failed `call`:
  null is not an object (evaluating 'auth.type')
```

Zero lines of output, exit 1. `models` is the command a user reaches for to diagnose "why won't it
run", and it is taken out by the same fault. `providers list` and `--help` still work.

**The HTTP server is hit as well**, so it is not CLI-only. The prompt is *admitted* with `200`, then
the turn dies:

```
POST /api/session/{id}/prompt   -> 200
  {"data":{"admittedSeq":0,"id":"msg_38bfd…","sessionID":"ses_147344…","prompt":{"text":"read the file",…}}}

server log: session prompt execution failed: plugin auth loader `google` failed: plugin
  `opencode-antigravity-auth@1.6.0` failed `call`: null is not an object (evaluating 'auth.type')
```

Bounded SSE read (`curl -sN --max-time 20 /api/event`) shows the client does get told, which is the
one good part:

```
data: {"data":{"message":"plugin auth loader `google` failed: plugin `opencode-antigravity-auth@1.6.0`
        failed `call`: null is not an object (evaluating 'auth.type')","sessionID":"ses_147344…"},
        "durable":{"aggregateID":"ses_147344…","seq":2,"version":1},…,"type":"session.error"}
```

Event census over that window: exactly `server.connected` ×1 and `session.error` ×1 — no
`assistant.message.created`, no text. And `GET /api/session/{id}/message` returns `{"data":[],"cursor":{}}`
**after two admitted prompts**, so an admitted prompt that dies this way leaves no user message
behind either.

**Root cause is narrowed by experiment, not inferred.** The variable is whether a credential exists
for the provider the plugin declares (`auth.provider === "google"`):

| `auth.json` state | `run` outcome |
| --- | --- |
| no `google` entry | exit 1, `null is not an object (evaluating 'auth.type')` |
| `google` present but malformed (`type` not a real kind) | exit 1, identical error |
| `google` = well-formed `{"type":"oauth","refresh","access","expires"}` | **exit 0**, `DONE-AFTER-TOOL` |

So the host calls the plugin's `auth.loader` and the `getAuth()` callback it hands over **resolves to
`null`** when nothing is stored. The plugin does `auth.type` and throws.

**That violates the SDK type the plugin compiles against.** From the `@opencode-ai/plugin` typings
shipped inside this very plugin's dependency tree:

```
dist/index.d.ts:23:  loader?: (auth: () => Promise<Auth>, provider: Provider) => Promise<Record<string, any>>;
```

`Promise<Auth>`, not `Promise<Auth | undefined>`. A plugin author reading that type is entitled to
dereference the result. The host resolving it to `null` is a contract violation on the host side.

Two distinct defects, either of which alone would be enough:

1. **`getAuth()` resolves to `null`** where the declared SDK type is `Promise<Auth>`. This is what
   makes a correct-by-the-types plugin throw.
2. **The throw is not isolated.** `docs/plugin-authoring.md:88` — "a plugin that crashes or times out
   is disabled with a `PluginDiagnostic` **rather than taking the turn down**." Here it takes down
   the turn, and additionally takes down `models`, on the CLI and through the server. Todo 168 made
   this true for hooks; the auth-loader path did not get the same treatment.

Practical impact: a user with a plugin-provided auth provider configured but **not yet logged in** —
a normal state, and the state a fresh machine is in after copying a config across — has a product
that cannot list models or run a turn, and the message names no remedy. It does not say the plugin is
version-incompatible, does not say "no credential stored for `google`", does not mention
`providers login`, and does not mention `--pure`.

I am not claiming the plugin is blameless; it should guard. But the host hands it `null` against its
own published type and then lets the throw be fatal.

**Not verified:** whether upstream `opencode` 1.18.15 also invokes the loader when no credential is
stored, or skips it. I have no upstream to run here. What I can state from the artifact is the type
declaration above and this port's observed behaviour.

### Scenario 2 — FINDING F3-W13-02 (documentation defect, medium) — "skipped" vs loaded-with-a-warning

`docs/plugin-authoring.md:37-40`:

> An npm plugin whose `engines.opencode` range excludes the running version is **skipped**, upstream's
> behaviour.

Observed, with the real bun on `PATH`:

```
WARN oc_cli::cmd::plugin_runtime: JavaScript plugin did not fully load
  plugin=opencode-antigravity-auth@1.6.0 hook=None kind=Compatibility
  plugin declares @opencode-ai/plugin ^0.15.30; host reports 1.18.13 surface="turn"
WARN oc_cli::cmd::plugin_runtime: JavaScript plugin did not fully load
  plugin=@sunerpy/oh-my-openagent@4.21.0 hook=None kind=Compatibility
  plugin declares @opencode-ai/plugin 1.15.13; host reports 1.18.13 surface="turn"
```

Both plugins are declared incompatible for `surface="turn"`. **Both then participate anyway**, which
I can prove from behaviour rather than from log text:

- `@sunerpy/oh-my-openagent@4.21.0` — its `tool.definition` hook runs; that is the only way the
  `disabled plugin … after hook 'tool.definition' failed` diagnostic in Scenario 1 can exist.
- `opencode-antigravity-auth@1.6.0` — its `auth.loader` runs; that is the whole of Scenario 1b.

So the user-visible behaviour is **loaded-with-a-warning, hooks and auth loaders live**, not
"skipped". `kind=Compatibility` + `surface="turn"` reads as a *partial* gate. The contradiction I
flagged in wave 12 (a log line saying the plugin did not load, followed by that plugin's hook firing)
is therefore still present in the same form; todo 168 fixed the *consequence* (the turn survives) but
not the inconsistency between the two statements.

**What a user sees:** at default verbosity, **nothing at all**. The `Compatibility` warning only
appears with `--print-logs`. So a user running an incompatible plugin is never told, and if that
plugin then misbehaves they get a disable diagnostic (good) with no hint that a version mismatch is
the underlying reason (not good).

`docs/divergences.md` has no entry for this; `grep -in "engines.opencode|compatib|skipped"` returns
only the unrelated `split-version-identity` reference and boilerplate. So it is an **undeclared**
difference between the documented contract and the shipped behaviour. Either the doc should say
"loaded, with the `turn` surface withheld and a warning", or the gate should actually skip.


### Scenario 3 — Todo 171, the v1 `Session` projection: FIXED

I hit all three routes on the running server and measured the returned key sets against the
**committed oracle** schema (`.omo/fixtures/oracle-openapi-1.18.12.json`) and against the schema the
**same process** publishes at `/doc`.

```
POST /session      -> 200
{"directory":"/tmp/opencode/f3w13/proj","id":"ses_9c1ae107…","parentID":null,
 "projectID":"global","slug":"ses_9c1ae107…",
 "time":{"created":1786556207412,"updated":1786556207412},
 "title":"New session - ses_9c1ae107…","version":"0.1.0"}
```

`slug` is present. Same key set on all three:

| route | keys | oracle required-missing |
| --- | --- | --- |
| `POST /session` | `directory, id, parentID, projectID, slug, time, title, version` | **NONE** |
| `GET /session` (15 rows, row 0) | identical | **NONE** |
| `GET /session/{id}` | identical | **NONE** |

Oracle `Session.required` = `['id','slug','projectID','directory','title','version','time']` — every one
served. The oracle schema is also `additionalProperties: false`, and the v1 response contains **no key
outside** the oracle's declared properties, so the projection is conformant in both directions, not
merely required-key-complete. **F3-W12-01 is closed.**

On "the same schema the process publishes at `/doc`" — that needs care, and the naive check is
misleading. `/doc` publishes `Session.required = ['id','projectId','slug','directory','title','version','time']`
with camelCase `projectId`, while v1 serves SDK-spelled `projectID`. Taking the union of both required
sets makes v1 look like it is missing `projectId`. It is not a defect, because **`/doc` describes only
the `/api` surface** — I confirmed programmatically that `/doc` contains **zero** non-`/api` paths, so
it makes no claim about `/session`. The two surfaces legitimately spell it differently. Verified the
`/api` side separately, against `/doc`'s own `Session`:

| route | required-missing | keys not in schema |
| --- | --- | --- |
| `POST /api/session` | NONE | NONE |
| `GET /api/session` row 0 | NONE | NONE |
| `GET /api/session/{id}` | NONE | NONE |

Both surfaces satisfy the schema that governs them. `/doc`, `/openapi.json` and `/api/doc` all return
the byte-identical document (16742 bytes); `/api/openapi.json` is `404`.

### Scenario 4 — the `/agent` drift classified "unverifiable": I AGREE

I measured it myself before reading how todo 171 classified it, so this is an independent check.

```
v1 GET /agent [0] keys: builtIn, color, description, maxSteps, mode, model, name, options,
                        permission, prompt, tools
oracle Agent.required:  name, mode, permission, options          -> required-missing: NONE
oracle Agent.properties: color, description, hidden, mode, model, name, native, options,
                         permission, prompt, steps, temperature, topP, variant
served keys not in oracle properties: builtIn, maxSteps, tools
oracle properties not served:         hidden, native, steps, temperature, topP, variant
oracle Agent.additionalProperties:    false
```

`docs/compatibility-matrix.md:95` records exactly this, key for key — the three extra keys against a
closed schema, the six omitted optional keys, `maxSteps`-vs-`steps` reading as a rename, and no
required key dropped. Two further facts I confirmed by running the artifact, both of which support the
classification:

- **This build publishes no `Agent` schema of its own.** `components.schemas` at `/doc` contains only
  `Session, SessionActive, SessionActiveResponse, SessionCreate, SessionListResponse,
  SessionPruneMutation` — six schemas, no `Agent`. So unlike the `Session`/`slug` case there is no
  self-contradiction to convict the build with.
- **The only oracle in the tree is 1.18.12**, and this port targets 1.18.15
  (`find . -name '*1.18.15*'` → nothing; the newest captures are `upstream-keybinds-1.18.13.tsv` and
  `upstream-commands-1.18.13.txt`). Three patch releases of legitimate schema movement sit between the
  oracle and the target.

**I agree with "unverifiable".** Settling it requires a 1.18.15 capture; asserting either way would be
inventing evidence. The distinction drawn against the `Session`/`slug` case — required-and-
self-contradicted versus optional-and-unclaimed — is the right line, and the guard test that fails if
this build ever starts publishing its own `Agent` schema is the right way to keep the reason honest.

### Scenario 4b — FINDING F3-W13-03 (defect, medium, undeclared) — the published OpenAPI binds no request or response body at all

Found by using the API, not by reading it: to send a prompt I had to guess the payload, and `/doc`
could not tell me. Every attempt was rejected on a *different* missing field, and the document I was
supposed to consult declares nothing:

```
POST /api/session/{id}/prompt  {"model":{"providerID":"faketool","modelID":"tool-model"},"parts":[…]}
  -> 422 Failed to deserialize the JSON body into the target type: model: missing field `id` at line 1 column 57
POST … {"model":{"providerID":"faketool","id":"tool-model"},"parts":[…]}
  -> 422 … missing field `prompt` at line 1 column 102
POST … {"prompt":{"text":"read the file"},"model":{"providerID":"faketool","modelID":"tool-model"}}
  -> 422 … model: missing field `id` at line 1 column 91
POST … {"prompt":{"text":"read the file"},"model":{"providerID":"faketool","id":"tool-model"}}
  -> 200
```

What `/doc` says about that route, in full:

```json
{"operationId":"post_api_session__sessionID__prompt",
 "responses":{"200":{"description":"Success"},
              "503":{"description":"Operation is known but its local backend is explicitly unavailable"}}}
```

No `requestBody`, no response content. I only got to `200` by reading
`crates/oc-server/src/api/session.rs:216-224` for `PromptBody`. A published OpenAPI exists precisely
to make that unnecessary.

It is not one route. Census over both documents:

| document | operations | with `requestBody` | with a `200` content schema | `components.schemas` |
| --- | --- | --- | --- | --- |
| oracle 1.18.12 | 188 | 60 | 165 | 472 |
| this build's `/doc` | 60 | **0** | **0** | **6** |

**Zero of 60 operations declare a request or response body.** The six schemas that exist are
orphans — no path references them, so `Session` is published but never bound to the routes that
return it. The document is not usable for client generation, response validation, or contract
testing; it is a route inventory with an unreferenced type appendix.

Two things make this worth reporting rather than shrugging at:

1. **The port points users at it.** `docs/compatibility-matrix.md:151` rejects the `generate` command
   with "use the server's `/openapi.json` document instead" — as the *replacement* for the excluded
   SDK/OpenAPI generator. That document cannot generate a client.
2. **It is undeclared.** `grep -in "requestBody|response schema|components/schemas|no schema"` across
   `docs/` returns nothing. `docs/divergences.md` has 17 entries, none about this. The matrix's own
   OpenAPI section (lines 168-190) carefully accounts for operation *registration* — "58 of the 58
   upstream operations are registered" — and for 503 backends, and says nothing about bodies. So the
   thing that is measured and gated is route presence; the thing a consumer actually needs is absent
   and unmentioned.

Being precise about severity: no runtime behaviour is wrong, the 422 messages are specific and name
the missing field, and `components.schemas.Session` is accurate where it exists. This is a defect in
the *published contract's* completeness plus a missing declaration, not a broken endpoint.

### Scenario 5 — Todo 169, the three recorded plugin contracts: the `tool_result` blocker is FIXED, with two caveats

I read Antigravity's own shipped bundle first so I sent the byte-shape it sends, not an approximation
(`…/opencode-antigravity-auth/dist/src/plugin/recovery.js:120-130`):

```js
const toolResultParts = toolUseIds.map((id) => ({
    type: "tool_result", tool_use_id: id,
    content: "Operation cancelled by user (ESC pressed)",
}));
try { await client.session.prompt({ path: { id: sessionID },
        // @ts-expect-error - SDK types may not include tool_result parts
        body: { parts: toolResultParts } });
      return true; } catch { return false; }
```

`client.session.prompt` is v1 route **18**, `POST /session/{sessionID}/message`
(`docs/v1-surface-capture.md:74`). Sent verbatim:

```
POST /session/{id}/message
  {"parts":[{"type":"tool_result","tool_use_id":"call_abc123",
             "content":"Operation cancelled by user (ESC pressed)"}]}
-> 200
{"info":{"agent":"build","id":"msg_5bf435…_0001","role":"assistant",…},"parts":[]}
```

**200, not the 400 F2 recorded.** The recorded contract executes. That part of todo 169 is real.

I also confirmed the sibling route is honestly refused rather than half-served — `POST /session/{id}/prompt`
is *not* in the measured surface, and says so with the best 404 body I have seen in this product:

```json
{"error":{"code":"unimplemented_v1_route",
 "message":"`/session/ses_3e78…/prompt` is not part of the measured pre-/api surface",
 "action":"re-run the plugin capture documented in docs/v1-surface-capture.md, add the route to
           V1_SURFACE with its callsite, then rebuild",
 "diagnostics":"/compat/v1/diagnostics","path":"…","unaccountedRequests":1}}
```

**Effect, not status code — caveat 1: `tool_use_id` is dropped.** I read back what was persisted, on
both surfaces. The `tool_result` part becomes a plain **text** part:

```
GET /session/{id}/message  ->  user message parts:
  {"id":"prt_bdbb20…","messageID":"msg_bcfd0a…","text":"Operation cancelled by user (ESC pressed)",
   "type":"text"}
```

Same on `/api/session/{id}/message`. So `type` is rewritten to `text` and **`tool_use_id`
(`call_abc123`) is not preserved anywhere in the stored part**. The whole point of this recovery call
is to pair an orphaned `tool_use` with a matching `tool_result` so the provider stops rejecting the
history; a text part carrying the human-readable content does not do that. The route now *accepts*
the contract, and Antigravity now gets `true` instead of `false`, but I cannot confirm the repair the
plugin is asking for actually occurs — the correlation id needed to perform it is discarded. I did not
verify what the *next* provider request contains, because I could not steer this route onto my fake
provider (caveat 2), so I state the persistence observation and stop there rather than infer.

**Caveat 2: the route ignores `providerID`/`modelID` in the body.** Sent
`{"providerID":"faketool","modelID":"tool-model","parts":[…tool_result…]}`; the response reported
`"providerID":"amazon-bedrock","modelID":"amazon.nova-2-lite-v1:0"` and **zero requests arrived at my
fake server**. That is the same shape as F2's contract-2 finding (summarize discarding its body),
on route 18.

**Caveat 3 — a failed turn is reported as `200`.** The server log for that exact request says:

```
session prompt execution failed: unrecoverable provider failure (status=Some(404)):
  Bedrock service error status=404 code=None: None
```

The HTTP response was **`200`** with an assistant message envelope whose `parts` is `[]` and whose
token counts are all zero. Antigravity's recovery does `try { … ; return true } catch { return false }`
— so on a turn that failed at the provider it returns **`true`**, reporting a successful repair.
Earlier, in a state where no assistant record was created at all, the same route returned
`500 {"error":{"code":"mutation_failed","message":"prompt completed without an assistant message"}}`.
So the status depends on whether an empty assistant row happened to be written, not on whether the
turn succeeded.

I could not exercise contracts 2 (OMO summarize `{providerID, modelID, auto}`) and 3 (OMO session
create `{id, providerID, variant?}`) as end-to-end *effects* — both need a real turn on a provider I
can observe, and caveat 2 blocks steering this surface onto my fake provider. **Not verified**, stated
rather than assumed. F2 owns those two and re-tested them this wave.

### Scenario 5b — FINDING F3-W13-05 (defect, high) — the config `model` key is parsed, reported, and then ignored

Found while trying to steer a turn: I wrote `"model": "faketool/tool-model"` in the config and the
product used `amazon-bedrock/amazon.nova-2-lite-v1:0`. `debug config` confirms the key is read:

```
$ opencode-rust debug config | jq .model
"faketool/tool-model"
$ opencode-rust run "read the file"          # no --model
transient provider failure (status=None): error sending request for url
  (https://bedrock-runtime.cn-northwest-1.amazonaws.com/model/amazon.nova-2-lite-v1%3A0/converse-stream)
```

So parsing is fine; **selection ignores it**. `--model` is honoured, which is why every earlier
scenario in this report worked and why this went unnoticed.

**Proven with a physical discriminator, in both directions, with no AWS involved.** Two fake providers
whose outputs are distinguishable — `DONE-AFTER-TOOL` (tool server, :47301) vs `tick0 … tick5` (slow
server, :47302) — declared under names that control catalog order, in a minimal config:

| config `model` | catalog order (`models` output) | observed stdout | model actually used |
| --- | --- | --- | --- |
| `zzfaketool/tool-model` | `aaaslow/slow-model`, `zzfaketool/tool-model` | `tick0 tick1 tick2 tick3 tick4 tick5` | **`aaaslow`** — not the configured one |
| `zzslow/slow-model` | `aaatool/tool-model`, `zzslow/slow-model` | `DONE-AFTER-TOOL` | **`aaatool`** — not the configured one |
| `zzslow/slow-model` + `--model zzslow/slow-model` | same | `tick0 … tick5` | `zzslow` — the flag works |

Rows 1 and 2 are the same rule winning against **opposite** configured values, so it is not
coincidence: the turn takes the **catalog's first** model and the config key has no effect. Row 3 is
the control proving both providers are reachable and the requested one is dialable.

This also explains the `amazon-bedrock` selection: with `AWS_REGION` and `AWS_BEARER_TOKEN_BEDROCK`
present in the ambient environment, an env-detected `amazon-bedrock` sorts first in the catalog and
wins. Clearing those two variables made the same command run the configured provider — which looks
like the config working but is really the catalog's first entry coinciding with it. That coincidence
is exactly what would hide this in a hand test.

The code comment agrees with the observed behaviour, which is why I am confident this is the rule and
not a fluke of my config — `crates/oc-cli/src/cmd/turn.rs:99`:

```rust
/// `provider/model`, defaulting to the agent's and then to the catalog's first.
```

Agent, then catalog-first. The top-level `Config::model` field
(`crates/oc-config/src/schema.rs:203`) is not in that chain.

**Why this is high severity.** `model` is the single most-used key in an opencode config; it is what
upstream documents for choosing a default model. A user setting it gets silently routed to whatever
sorts first — potentially a different vendor, at a different price, with different capabilities — with
no warning that the key was ignored. And `debug config` actively reassures them it was read. It is
undeclared: `docs/divergences.md` has no entry, and nothing in `docs/` mentions the top-level `model`
key being inert.

**Not verified:** whether upstream honours the key. I have no upstream binary here. What I verified is
that this build parses it, reports it, and does not use it.

### Scenario 6 — Todo 170, SDK spelling at the JS model/provider boundary: CONFIRMED both directions

I wrote a real plugin, auto-discovered from `$XDG_CONFIG_HOME/opencode/plugin/f3probe.js`, that logs
the hook arguments it receives to a file I read afterwards, and supplies a model of its own.

**Outbound (host → plugin) carries SDK spelling.** From the `experimental.provider.small_model` hook
argument, exactly as the plugin saw it:

```json
{"provider":{"env":[],"id":"aaatool","name":"aaatool","source":"config",
  "options":{"apiKey":"x","baseURL":"http://127.0.0.1:47301/v1"},
  "models":{"f3-sdk-model":{
     "id":"f3-sdk-model","name":"f3-sdk-model","providerID":"aaatool",
     "api":{"id":"aaatool","npm":"@ai-sdk/openai-compatible","url":"http://127.0.0.1:47301/v1"},
     "capabilities":{"attachment":false,"reasoning":false,"temperature":true,"toolcall":true,
                     "input":{…},"output":{…},"interleaved":false},
     "cost":{"cache":{"read":0,"write":0},"input":0,"output":0},
     "limit":{"context":8192,"input":null,"output":2048},
     "family":"","headers":{},"options":{},"release_date":"2026-01-01",
     "status":"active","variants":{}}}}}
```

**`providerID`, not `provider_id`.** A plugin reading `model.providerID` from a hook argument gets a
value. That is todo 170's claim and it holds.

**Inbound (plugin → host) accepts SDK spelling too.** The model above is one my plugin supplied via
its `provider` descriptor using `providerID`, and it reached the catalog:

```
$ opencode-rust models
aaatool/f3-sdk-model
zzslow/slow-model
```

So the `providerID` → `provider_id` conversion works in both directions through a real plugin. **The
narrow claim of todo 170 is verified.** Three caveats from using it, though, and the first two cost me
real time:

**Caveat A — only `providerID` is SDK-spelled; everything else is internal.** Getting that model
accepted took five iterations, each rejected on a different field, all visible only at `DEBUG`:

```
missing field `status`      -> added status:"active"
missing field `api`         -> added api:{npm:…}
missing field `id`   (api)  -> added api:{id,npm,url}
missing field `toolcall`    -> flat attachment/reasoning/temperature/tool_call had to become
                               capabilities:{…, toolcall:…, input:{…}, output:{…}, interleaved:…}
```

The final accepted object is this build's **internal** `ResolvedModel` shape, not the SDK's `Model`:
`toolcall` (SDK: `tool_call`), a nested `capabilities` object (SDK: flat booleans), `cost.cache.read`
(SDK: `cache_read`), plus mandatory `status`, `api.{id,npm,url}`, `headers`, `variants`. The code's own
comment at `crates/oc-plugin/src/js/bridge.rs:482-485` asserts "A plugin's model map is the SDK's
`Model` shape, which is a superset of `ResolvedModel`'s serialized form for the fields that matter" —
measured against the artifact, that is not true for `V2`: `plugin_model_value`
(`crates/oc-plugin/src/js/projection.rs:226-255`) maps `providerID` and then only fills defaults for
`family`/`release_date`/`variants`/`interleaved` on the **`Legacy`** path. `V2` gets the rename and
nothing else. `CatalogStatus`'s own doc comment says "The catalog omits the field for a normal model;
the oracle then treats it as `active`" — yet a plugin omitting it is rejected. **OBSERVATION
F3-W13-06 (low/medium):** todo 167 and 170 fixed one key; the remaining internal-vs-SDK field
mismatches still silently drop a plugin's model, and the diagnostic is two verbosity levels down.

**Caveat B — a rejected plugin model destroys the provider's configured models.** Before my plugin
existed, `models` listed `aaatool/tool-model` (declared in config). While my plugin's single model was
being rejected, `models` listed **only** `zzslow/slow-model` — `aaatool` had *zero* models and
vanished. The `provider` hook **replaces** the model map rather than merging it, so one undecodable
plugin model silently removes models the user configured by hand. I flagged the replace semantics as
"matches the documented upstream contract" in wave 12; combined with a silent per-model rejection it is
a data-loss path, not just a surprise.

**Caveat C — the host's own diagnostics for a malformed plugin are excellent.** Two of my mistakes
produced messages I could act on immediately, which is worth saying because it contrasts with
F3-W13-01:

```
kind=Protocol plugin `file:///…/f3probe.js` returned an auth hook with no `provider` string
kind=Protocol plugin `file:///…/f3probe.js` returned a provider hook with no `id` string
```

Both name the plugin, the hook, and the missing field. Both are `WARN`-level and therefore invisible
without `--print-logs` — same visibility gap as F3-W13-04 — but the content is right.

### Scenario 6b — FINDING F3-W13-07 (defect, HIGH) — `tool.definition` is unusable by any JS plugin, and the host blames the plugin for its own loss

This is the real root cause of my wave-12 F3-W12-03, and it is bigger than that finding described.

**A `tool.definition` hook that does literally nothing is disabled on every turn.** The entire plugin:

```js
export const NoOp = async () => ({
  "tool.definition": async () => {},
});
```

```
$ opencode-rust run --model aaatool/tool-model "hello"
DONE-AFTER-TOOL
disabled plugin `file:///…/noop.js` after hook `tool.definition` failed: plugin `file:///…/noop.js`
  truncated `tool.definition` hook argument 1 at
  `/parameters/properties/todos/items/properties/priority/oneOf/0`; refusing to apply any hook mutation
```

An empty function body cannot truncate anything. `async (input, output) => { return undefined; }`
behaves identically. **Control:** the same plugin with the hook renamed to `chat.message` and nothing
else changed →

```
$ opencode-rust run --model aaatool/tool-model "hello"
DONE-AFTER-TOOL
```

Clean, no diagnostic. So registering `tool.definition` at all is sufficient to be disabled.

**The plugin receives the data intact, so the loss is on the host's read-back.** I had the hook dump
what it was handed, then return nothing. The pointer the host later complains about is fully populated
on arrival:

```json
output.parameters.properties.todos.items.properties.priority =
  {"description":"Priority level of the task: high, medium, low",
   "oneOf":[{"const":"high","description":"Highest urgency.","type":"string"},
            {"const":"medium","description":"Default urgency.","type":"string"},
            {"const":"low","description":"Lowest urgency.","type":"string"}]}
```

`oneOf[0]` is present with `const`, `description`, `type`. The plugin touches nothing. The host then
reports that `oneOf/0` was truncated **by the plugin**. Whatever is lost is lost inside the host's own
JS round-trip, and the diagnostic attributes it to the wrong party.

**Which tools trigger it.** I walked every definition the hook is handed and recorded each `oneOf`:

| tool | invocation order | `oneOf` locations as the plugin sees them |
| --- | --- | --- |
| invalid, bash, read, glob, grep, edit, write | 1-7 | none |
| `webfetch` | 8 | `/parameters/properties/format/oneOf` (3 entries, each `const`+`description`+`type`) |
| `todowrite` | 9 | `/parameters/properties/todos/items/properties/priority/oneOf` (3), `…/status/oneOf` (4) |

Nine definitions, two contain `oneOf`, and the one named is `todowrite`. `webfetch`'s `oneOf` sits
directly under a property; `todowrite`'s sits under an **array's `items`**. `webfetch` passes and
`todowrite` fails, so the loss correlates with `oneOf` nested beneath `items`, not with `oneOf` as
such. I am reporting the correlation I measured, not asserting the mechanism.

**Consequences, in order of severity:**

1. **Hook 21 is unusable by every JS plugin.** `docs/plugin-authoring.md:183` advertises
   `tool.definition` as a supported hook; in practice registering it guarantees the plugin is disabled
   on its first turn. That is an undeclared, total loss of one of the 21 advertised hooks.
2. **`@sunerpy/oh-my-openagent@4.21.0` was probably never at fault.** In wave 12 I wrote "I am not
   claiming the third-party plugin is blameless". I now am: my four-line no-op reproduces the identical
   failure, at the identical JSON Pointer. F3-W12-03's cause was this defect, not OMO.
3. **The diagnostic, which I praised in Scenario 1, points at the wrong party.** It is precise about
   *where* and blames the plugin for *what*. A plugin author following it would search their own code
   for a mutation that does not exist. This one message is the difference between a 10-minute fix and
   the four review waves this has survived.
4. **Every install with any `tool.definition` plugin loses that plugin, every turn**, and the
   real-config repro is exactly this.

What todo 168 fixed is still fixed: the turn completes (`exit 0`), the blast radius is one plugin, and
the message is loud. The underlying defect is untouched, and it now has a four-line reproduction.

## Re-confirmation of my earlier priority checks (the tree moved)

### Scenario 7 — stalled-provider idle bound (my wave-9 finding): still correct

Rebuilt both fake socket servers and re-ran both halves against the freshly built binary.

**Stall** (two deltas then silence forever, never closes), `OPENCODE_STREAM_IDLE_TIMEOUT_SECS=3`:

```
REAL EXIT=1  elapsed=7s
--- stdout ---  PARTIAL-ONE PARTIAL-TWO
--- stderr ---  transient provider failure (status=None): provider `fakestall` response stream idle
                timeout after 3s; raise OPENCODE_STREAM_IDLE_TIMEOUT_SECS for slower providers
```

**Slow-but-progressing** (a delta every 2s for six ticks, under the same 3s bound):

```
REAL EXIT=0  elapsed=24s
--- stdout ---  tick0 tick1 tick2 tick3 tick4 tick5
--- stderr ---  (empty)
```

Identical to wave 12: the turn terminates instead of hanging, partial text is preserved on stdout, the
error names the provider/bound/env var on stderr, exit code is 1, and a merely slow stream is not
killed — so the bound still measures the **gap between chunks**, not total duration. Closed.

### Scenario 8 — plugin auto-discovery from all four directories: PASS, five locations

Built one trivial plugin in each documented location plus the second `plugins/` spelling, then ran a
real turn:

```
DEBUG auto-discovered JavaScript plugin plugin="file://config/opencode/plugin/xdg_plugin.js"    scope=Global
DEBUG auto-discovered JavaScript plugin plugin="file://config/opencode/plugins/xdg_plugins.js"   scope=Global
DEBUG auto-discovered JavaScript plugin plugin="file://proj/.opencode/plugin/project_plugin.js"  scope=Local
DEBUG auto-discovered JavaScript plugin plugin="file://home/.opencode/plugin/home_plugin.js"     scope=Global
DEBUG auto-discovered JavaScript plugin plugin="file://ocdir/plugin/ocdir_plugin.js"             scope=Global
```

All five found, `plugin/` and `plugins/` both scanned, and provenance is right: the project-local one
is `scope=Local`, the other four `scope=Global`. Matches `docs/plugin-authoring.md:29-35` exactly.
(Discovery is `DEBUG`-only, same visibility caveat as elsewhere.)

### Scenario 9 — `edit` permission subject and diff: subject PASS; diff now MEASURED and absent

The subject reproduces verbatim on the CLI with `{"permission":{"edit":"ask"}}` and a provider that
issues a real `edit` call:

```
denied `edit`: permission `edit` resolves to ask for /tmp/opencode/f3w13/permproj/target.txt, and this
non-interactive run has nobody to ask; add `"permission": {"edit":
{"/tmp/opencode/f3w13/permproj/target.txt": "allow"}}` to your configuration to authorize it
```

Concrete path, paste-ready fragment, file verified unchanged afterwards.

**New this wave: I reached a live pending permission**, which I could not do in wave 12. Driving the
same turn through `POST /api/session/{id}/prompt` and polling `GET /api/session/{id}/permission`:

```json
{"data":[{"id":"per_357a313caae64aa585b63cfc61b876a3","sessionID":"ses_9be5fdcb…",
  "action":"edit",
  "resources":["/tmp/opencode/f3w13/permproj/target.txt"],
  "save":["/tmp/opencode/f3w13/permproj/target.txt"],
  "metadata":{"arguments":{"filePath":"/tmp/opencode/f3w13/permproj/target.txt",
     "intent":"F3 wave13 permission probe","newString":"LINE TWO EDITED","oldString":"line two"}}}]}
```

The subject is right here too — `resources` and `save` are the concrete file, and `intent` is carried
through. **There is no rendered diff.** `metadata.arguments` gives the raw `oldString`/`newString`; no
`diff`, `patch`, `preview`, `before` or `after` key exists anywhere in the payload (I grepped the
response). The route that might have carried more is an explicit gap:

```
GET /api/session/{id}/permission/{requestID}
  -> {"error":{"code":"backend_unavailable",
       "message":"backend unavailable for GET /api/session/{sessionID}/permission/{requestID}"}}
```

So an approval client must render its own diff from the two strings. That is a defensible design and
`compatibility-matrix.md` accounts for the 503 as one of its ten declared backend gaps — I am
recording it as **measured**, not as a defect, and closing my wave-12 "unverified" item: the diff is
not served by the API.

`POST …/permission/{id}/reply {"reply":"once"}` returns `204`. **What I could not verify cleanly:**
whether the edit then applies. My fake provider is stateful (`read` on request 1, `edit` on request 2)
and its process was reaped several times by harness timeouts, so the retries desynchronised the
sequence and the file stayed unchanged in a run where a prior `read` had not happened. One run logged
`Tool.execute{tool.name="edit"} … close time.busy=300s` after a granted reply, which would be a 300 s
hang, but the same run also shows my fake server had died (`Connection refused`) — the confound is
mine, so I am not reporting it as a finding. Reply **acceptance** is verified; reply **effect** is not.

### Scenario 10 — migration ceiling through the real `db` command: PASS

Real database built by the shipped binary — 38 rows in `migration`, newest
`20260622202450_simplify_session_input`. Injected a future id, then ran the real commands:

```
$ opencode-rust db "select 1"
database migration journal is newer than this binary (known ceiling
  20260622202450_simplify_session_input, observed zzzz_from_the_future)
$ opencode-rust session list        # identical message
$ opencode-rust run --model faketool/tool-model "hi"   # identical message
```

Every command that opens the database refuses, not just `db`. The message names both the ceiling and
the offending id. **Non-destructive**: the journal still had 39 rows (38 + my injection) afterwards —
nothing was rewritten or dropped. Restored to 38 after the test.

### Scenario 11 — `kiro-auth` provider in `models`: PASS

Against this machine's real configuration and credential store:

```
$ opencode-rust models | grep -i kiro
kiro-auth/auto
kiro-auth/claude-haiku-4-5
kiro-auth/claude-opus-4-5
kiro-auth/claude-opus-4-6
kiro-auth/claude-opus-4-7
kiro-auth/claude-opus-4-7-high
…
$ opencode-rust models | wc -l
313
```

Present, with its full model family, inside a 313-model catalog. `providers list` also shows it as
`kiro-auth api` under the credential store. (Note: this only works when the antigravity auth-loader
crash of F3-W13-01 is avoided; with no `google` credential stored, `models` prints nothing at all.)

## Scenario 0 — the gate

```
$ cargo test --workspace --offline
PASSED=3443  FAILED=0  IGNORED=2
```

Exactly the expected 3443 passing / 0 failed, aggregated over 212 `test result:` lines. Zero
occurrences of `EAGAIN`, `error[` or `panicked` in the log, so no retry was needed and I have no host
transient to report. One run, as instructed.

## Exploration — attempts to break it

**Malformed config is rejected precisely, and the three failure modes are distinguished:**

```
{ "model": "a/b",                    -> config file …/opencode.json is not valid JSON
{ "totallyBogusKey": 1, … }          -> failed validation (1 issue(s))
                                          totallyBogusKey: unrecognized key
[1,2,3]                              -> failed validation (1 issue(s))
                                          <document>: invalid type: integer `1`, expected a string
                                          at line 1 column 2
```

Syntax error, unknown key, and wrong type each get their own message; the last two carry the JSON path
and, where applicable, line and column.

**Bad CLI input** — every message names the specific problem:

```
run --model nosuch/model "hi"   -> Model not found: nosuch/model
run --model garbage "hi"        -> model must be provider/model, got "garbage"
run                             -> a message is required
export ses_nope                 -> Exporting session: ses_nope / Session not found: ses_nope
import /tmp/nope.json           -> File not found: /tmp/nope.json
session delete ses_nope         -> Session not found: ses_nope
frobnicate                      -> error: unrecognized subcommand 'frobnicate' + usage
serve --port <occupied>         -> could not bind HTTP server to 127.0.0.1:47601:
                                     Address already in use (os error 98)
```

Misspellings get did-you-mean help (`db`/`debug`, `pr`/`export`). Nothing panicked; no Rust backtrace
ever surfaced.

**A broken plugin does not break the product.** A file containing deliberate garbage
(`export const Bad = async () => ({ this is not javascript`) let the turn run to completion, with the
cause available at `--print-logs`:

```
WARN … kind=Protocol plugin `file:///…/broken.js` failed `init`:
  3 errors building "/tmp/opencode/f3w13/min/config/opencode/plugin/broken.js"
```

**Concurrency and SSE are clean.** 12 simultaneous `POST /api/session` → twelve `200`s. Three
simultaneous bounded SSE subscribers each got their own `server.connected` frame with a **distinct**
event id (`evt_019ff759c97f77c1921eeae0…`, `…eeadde5f…`, `…eeac63dc…`), and `GET /api/session`
answered `200` afterwards. Every SSE read was bounded with `--max-time`; I never left a stream open.

**HTTP error shapes are consistent**, and the unaccounted-route counter is real state, not decoration:

```
POST /session  '{not json'  -> 400 Failed to parse the request body as JSON: key must be a string
                                  at line 1 column 2
GET /totally/bogus          -> 404 (empty body)
DELETE /auth/anthropic      -> 404 {"code":"unimplemented_v1_operation",
                                    "message":"`DELETE /auth/anthropic` is not part of the measured
                                               pre-/api surface","unaccountedRequests":2, …}
GET /compat/v1/diagnostics  -> 200 {"unknownRoutes":{"total":1,"paths":{"DELETE /auth/anthropic":1},
                                     "action":"a non-zero total means the capture is incomplete;
                                               re-run it and extend V1_SURFACE"}, …}
```

`unaccountedRequests` incremented 1 → 2 across my two probes and the diagnostics route reports the
same sighting with its path. The verb-vs-path distinction described in
`docs/v1-surface-capture.md:135-140` is live: `DELETE /auth/{providerID}` is
`unimplemented_v1_operation`, not `unimplemented_v1_route`.

**TUI** — started under tmux at 200×50, came up with an `idle` strip and an input caret, accepted
typed input, and streamed a real turn **live**:

```
> You
  read the file
* Assistant
  tick0 tick1 tick2            (mid-turn, status: "… working"; footer: build · fakeslow/slow-model)
…
* Assistant
  tick0 tick1 tick2 tick3 tick4 tick5        (status: idle)
```

Two things worth recording. The TUI streams text **live** where `run` prints it at end-of-turn — a
defensible difference, not a defect. And the footer read `build · fakeslow/slow-model` while that
environment's config said `"model": "faketool/tool-model"`: **F3-W13-05 reaches the TUI too**, so the
config key is inert on at least two of the three entry points.

**Ctrl-C** — with a real controlling TTY, `Ctrl-C` interrupts a stalled turn immediately (`^C` echoed,
shell reports `INT`). Recording a harness confound so nobody re-chases it: when I first launched the
same turn as a **background** job from a non-interactive shell, two `SIGINT`s were ignored and only
`SIGTERM` killed it (exit 143). That is POSIX shell behaviour — a background job in a shell without
job control inherits `SIG_IGN` for `SIGINT` — **not** a product defect. The TTY test is the valid one.

**Empty state** — `session list` prints a real header-and-rule table (`Session ID | Project | Title |
Agent | Last activity | Msgs | Cost`), which differs from the bare-lines behaviour I recorded in wave
12; the `session-list-output-shape` divergence is declared. `mcp list` on an empty config is exemplary:
`MCP Servers / No MCP servers configured / Add servers with: opencode-rust mcp add`.

## Verdict

**BLOCK.** Three defects I would not ship, all found by running the product and none of which the
3443-test suite detects.

| id | severity | what | declared? |
| --- | --- | --- | --- |
| F3-W13-07 | **high** | `tool.definition` is unusable by any JS plugin — a four-line no-op hook is disabled on every turn, and the diagnostic blames the plugin for a loss inside the host | no |
| F3-W13-05 | **high** | the top-level config `model` key is parsed and reported by `debug config`, then ignored; the turn silently uses the catalog's first model. Affects `run` and the TUI | no |
| F3-W13-01 | **high** | a plugin `auth.loader` failure is fatal — kills `run`, kills `models` (zero output, exit 1), and kills turns through the server. Triggered by the normal "configured but not logged in" state, because the host resolves `getAuth()` to `null` against its own `Promise<Auth>` SDK type | no |
| F3-W13-03 | medium | the published OpenAPI binds **0 of 60** operations to a request or response body, while the port points users at it as the replacement for the excluded generator | no |
| F3-W13-02 | medium | `docs/plugin-authoring.md:37-40` says a version-incompatible npm plugin is *skipped*; it is loaded and its hooks and auth loaders run | no |
| F3-W13-04 | low | a missing/broken JS runtime is reported as a 30 s-per-plugin `FailedToLoad … did not connect back` timeout; the real cause is only in raw plugin stderr at `DEBUG` | n/a |
| F3-W13-06 | low/med | plugin models must match the **internal** `ResolvedModel` shape, not the SDK `Model` shape the code comment claims; a rejected model silently wipes the provider's configured models | no |

**What the four todos got right.** Todo 168's hook isolation is real and its JSON-Pointer diagnostic is
the best failure message in this product — the turn now completes, exit 0, on CLI, server and TUI.
Todo 171's `slug` fix is complete and conformant in both directions against a closed oracle schema, and
its "unverifiable" classification of the `/agent` drift is one I independently reached and agree with.
Todo 169's `tool_result` route now returns `200` where F2 measured `400`. Todo 170's `providerID`
projection works in both directions through a real plugin.

**Why I am still blocking.** My wave-12 finding was that a default install could not complete a turn.
Todo 168 fixed the *symptom* — and in doing so uncovered the cause, which is F3-W13-07: the host loses
data in its own `tool.definition` round-trip and reports the plugin as the truncator. That is why this
survived twelve waves. A four-line no-op plugin reproduces it. And two of the three high-severity items
here are the same shape as the one I filed last wave: a fault in a plugin-adjacent path taking down a
command the user needs to diagnose it, with a message that names no remedy.

## What I could NOT verify, and why

- **Whether the `tool_result` repair actually happens.** The route accepts the recorded contract and
  returns `200`, but the stored part is a plain `text` part with `tool_use_id` discarded, and I could
  not steer route 18 onto my observable fake provider (it ignores `providerID`/`modelID` in the body),
  so I could not read the next provider request. Acceptance verified; repair not.
- **The permission reply's effect.** `POST …/permission/{id}/reply {"reply":"once"}` returns `204`, but
  my stateful fake provider was desynchronised by harness timeouts and the edit did not land in a run
  whose `read` had not happened. Reply acceptance verified; reply effect not. One run logged a 300 s
  busy `edit` after a granted reply, but the same run's provider had died — my confound, so not filed.
- **Contracts 2 and 3 of todo 169** (OMO summarize, OMO session create) as end-to-end effects — same
  steering limitation. F2 owns those.
- **Whether upstream 1.18.15 honours the config `model` key, or invokes an auth loader with no stored
  credential.** No upstream binary here. I verified this build's behaviour and quoted its own type
  declaration and code comment, nothing more.
- **The `/agent` drift's legitimacy** — needs a 1.18.15 OpenAPI capture; the newest in the tree is
  1.18.12. Same conclusion as todo 171.
- **Real provider traffic.** No working credentials in this sandbox (Bedrock returned 404 and TLS
  handshake EOF), so every turn used my own fake OpenAI-compatible servers. Sufficient for stream
  lifecycle, tool dispatch, permission, plugin and precedence behaviour; it does not exercise real
  provider wire quirks.
- **The exact mechanism of F3-W13-07.** I measured that `webfetch`'s top-level `oneOf` survives and
  `todowrite`'s `oneOf` under an array's `items` does not, and that the plugin receives it intact. I
  report that correlation, not a root cause in the host's serialization.

## Environment notes

- `bun` on this host is a **broken mise shim** in non-login shells. Left as-is, all JS plugins fail
  with a 30 s timeout each and the product silently pays 90 s per turn (F3-W13-04). Every plugin
  scenario here was run with `/config/.local/share/mise/installs/bun/1.3.14/bin` prepended to `PATH`.
- `AWS_REGION` and `AWS_BEARER_TOKEN_BEDROCK` are set in the ambient environment, which makes an
  env-detected `amazon-bedrock` sort first in the catalog. Combined with F3-W13-05 that silently
  hijacks any turn without `--model`. I cleared both for the model-selection scenarios.
- Background-shell `SIGINT` inheritance (POSIX `SIG_IGN` for background jobs) is a harness artifact,
  not a product defect — see the Ctrl-C note above.

## Cleanup

All scratch lives under `/tmp/opencode/f3w13/`. Every fake server (ports 47301-47304), every `serve`
process (47401, 47501, 47601, 47701), the TUI, and every tmux session I created were killed; the
injected future migration row was removed and the journal restored to 38 rows. No product source,
test, plan, documentation or evidence file other than this report was modified, and nothing was
committed, branched, pushed or merged.
