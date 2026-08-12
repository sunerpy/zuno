# F3 — Manual QA Report, Wave 11

- **Audited HEAD**: `b20ecbc9497cb820929c0cdb9f0507a0b425c9c9`
- **Worktree**: `/config/workspace/ProdDir/AI/oc-wt/tF3`, branch `task-F3`
- **Role**: real manual QA — drive the built binary, report observed output.
- **Verdict**: APPROVE WITH FINDINGS (1 new defect, 1 doc-contradiction defect, 3 defect-adjacent, 2 could-not-verify)

## Method

Build `cargo build --offline`, run `target/debug/opencode-rust`. Real terminal, tmux for
long-lived surfaces, curl for HTTP with bounded reads. Observed output quoted verbatim.

## Planned scenarios (mandatory first, in order)

1. **S1** — Re-run the four-location plugin auto-discovery test (F3-W10-D1 / todo 163):
   `plugin/`, `plugins/`, `.opencode/plugin/`, `.opencode/plugins/`. Does each load now?
   Is a rejected file's reason discoverable at some log level?
2. **S2** — Hit a v1 `not_implemented` stub route and read the 501 body (F3-W10-D2 / todo 164):
   does it name a currently-valid alternative, and has it stopped citing plan todos?
3. **S3** — Re-check the non-canonical `parts` mutation diagnostic (F3-W10-O6): does it
   name the offending member, not just the hook?
4. **S4** — Re-check plugin load failure latency + silence (F3-W10-O7), and whether
   `docs/plugin-authoring.md` now states the `bun` / `@opencode-ai/sdk` prerequisites.
5. **S5** — Re-check title-generator API surface vs. the user turn's surface (F3-W10-O8)
   on Responses-dispatching providers.
6. **S6** — Todo 162: Copilot model advertising `responses` with heuristic-hostile id
   (`mai-code-1-flash-picker`) → Responses; explicit `chat` endpoint on Responses-heuristic
   id → Chat. Against local mocks.
7. **S7** — Regression re-confirmation on the moved tree:
   - stalled-provider idle bound (wave-9 finding)
   - `edit` permission subject + diff
   - migration-ceiling refusal through the real `db` command
   - `kiro-auth` provider appears in `models`
8. **S8** — Gate run: `cargo test --workspace --offline` (expect 3426 passing / 0 failed).
9. **S9** — Break-it pass: bad input, missing config, wrong flags, `--help`, empty state,
   permission denial, interrupted streams, concurrent clients.

## Results

### S1 — plugin auto-discovery (F3-W10-D1 / todo 163) — **FIXED. All eight advertised locations load.**

I carried this defect for three waves. It is resolved, and the fix is broader than the
log line I asked for.

Two things changed. First, `docs/plugin-authoring.md` was rewritten to name the scanned
locations precisely — it no longer says "project trees", it says **configuration
directories**:

> Beyond the config array, every configuration directory is scanned for
> `plugin/*.{ts,js}` and `plugins/*.{ts,js}`. This includes `$XDG_CONFIG_HOME/opencode`,
> project `.opencode` directories, `$HOME/.opencode`, and `OPENCODE_CONFIG_DIR`, in that
> configuration-directory order

That is four config dirs x two subdirectory names = **eight** locations, not the four I
tested in wave 10. My wave-10 test used a bare project-root `./plugin/`, which the
current doc does not advertise. I re-tested against what the doc claims today.

Second, the scan now actually runs. I placed a copy of the known-good plugin file in all
eight locations, with **no `plugin` key in the config at all**, and gave each copy a tag
so I could tell which file executed:

```
$ find $W/d2 -name a.js | sed 's#.*/d2/##'
ocdir/plugin/a.js          ocdir/plugins/a.js
xdg/opencode/plugin/a.js   xdg/opencode/plugins/a.js
proj/.opencode/plugin/a.js proj/.opencode/plugins/a.js
home/.opencode/plugin/a.js home/.opencode/plugins/a.js

$ env -i PATH=.../bin:/usr/bin:/bin HOME=$W/d2/home XDG_CONFIG_HOME=$W/d2/xdg \
    OPENCODE_CONFIG_DIR=$W/d2/ocdir opencode-rust run --log-level DEBUG --print-logs \
    -m test/test-model "hello four locations"
rc=0
SCRIPT_EXHAUSTED_DONE

--- LOADED tags (which files actually EXECUTED) ---
"tag":"HOME_plugin"        "tag":"HOME_plugins"
"tag":"OCDIR_plugin"       "tag":"OCDIR_plugins"
"tag":"PROJ_dotoc_plugin"  "tag":"PROJ_dotoc_plugins"
"tag":"XDG_plugin"         "tag":"XDG_plugins"
```

**Eight of eight ran** — this is the plugin's own `appendFileSync` from inside the JS
process, not a host log line, so it proves execution and not merely enumeration.
Provenance is reported too, with the right scope per directory:

```
source=xdg/opencode/plugin/a.js    scope=Global
source=xdg/opencode/plugins/a.js   scope=Global
source=proj/.opencode/plugin/a.js  scope=Local
source=proj/.opencode/plugins/a.js scope=Local
source=home/.opencode/plugin/a.js  scope=Global
source=ocdir/plugin/a.js           scope=Global
```

**A rejected file's reason is now discoverable, and it names both the file and the
cause.** I broke two of the discovered files — one syntactically, one exporting a
non-function — and the WARN is specific:

```
WARN oc_cli::cmd::plugin_runtime: JavaScript plugin did not fully load
  plugin=file:///.../xdg/opencode/plugin/a.js  kind=Protocol
  plugin `...plugin/a.js` failed `init`: Expected identifier but found end of file
WARN oc_cli::cmd::plugin_runtime: JavaScript plugin did not fully load
  plugin=file:///.../xdg/opencode/plugins/a.js kind=Protocol
  plugin `...plugins/a.js` failed `init`: Plugin export is not a function
```

That answers the question in my brief: yes, at WARN, with the offending path and a real
parser/loader reason. `kind=Protocol` also distinguishes it from `kind=FailedToLoad`.

**One residual, unchanged from O7 and now the only part left.** At the default log level
a broken discovered plugin is still completely silent:

```
###### DEFAULT (no flags) ######
rc=0 elapsed=0s stderr_bytes=0
```

Two plugins failed to initialise and the run exits 0 with zero bytes on stderr. The
reason exists only once you pass `--print-logs`. Since discovery is now automatic, this
matters more than it did in wave 10: a file dropped into `~/.opencode/plugin/` can fail
and the user has no signal at all. See O7 below for my current read on severity.

**Good news on the latency half of O7**: the failure is now fast. `elapsed=0s` at
default and `1s` at DEBUG for two broken plugins. The 30s stall I measured in wave 10
reproduces only when `bun` itself is unusable — I hit it accidentally at the start of
this scenario when a mise shim was stale:

```
DEBUG oc_plugin::js::host: javascript plugin stderr ... mise ERROR bun is not a valid
  shim. This likely means you uninstalled a tool and the shim does not point to anything.
WARN oc_cli::cmd::plugin_runtime: JavaScript plugin did not fully load
  plugin=file:///...  kind=FailedToLoad  plugin `...` did not connect back within 30000 ms
```

So the 30s is the connect-back timeout for a runtime that never starts, and the WARN now
explains it in those words. Still a 30s wait, but no longer unexplained.

**Verdict: F3-W10-D1 is closed.** Discovery happens, all eight documented locations
work, provenance is right, and rejections are diagnosable. This was the oldest open item
I was carrying.


---

### S2 — v1 `not_implemented` hint (F3-W10-D2 / todo 164) — **FIXED. The hint now names a working alternative and cites no todos.**

Hitting the stub route myself:

```
$ curl -s -w '\nHTTP %{http_code}\n' http://127.0.0.1:18931/session
HTTP 501
{"error":{
  "apiAlternative":"GET /api/session",
  "backing":"not-implemented",
  "callers":["@sunerpy/oh-my-openagent@4.21.0"],
  "code":"not_implemented",
  "hint":"this pre-/api route is registered but has no local backend; call `GET /api/session`
          instead, which serves the same capability in this build",
  "message":"`client.session.list` has no local backend in this build",
  "route":"GET /session","sdkMethod":"client.session.list",
  "surfaceCoverage":"19 of 20 measured pre-/api routes have no local backend
    (10 of those can name a served /api alternative); 1 served locally"}}
```

Both halves of my finding are addressed:

**The stale todo promise is gone.** `"lands in todos 57-62"` no longer appears. I swept
every v1 route I know of and grepped all bodies for a todo citation: **0 occurrences**.

**The named alternative is real and works.** Not a claim — I called it:

```
$ curl http://127.0.0.1:18931/api/session
{"data":[],"cursor":{"previous":null,"next":null}}   HTTP 200
$ curl -X POST -d '{}' http://127.0.0.1:18931/api/session  -> HTTP 200
GET /api/agent    -> HTTP 200
GET /api/provider -> HTTP 200
```

**The coverage string is accurate.** It claims 10 routes can name an alternative. My
independent sweep found exactly 10:

```
GET /session                     501  alt=GET /api/session
POST /session                    501  alt=POST /api/session
GET /session/{id}                501  alt=GET /api/session/{sessionID}
POST /session/{id}/abort         501  alt=POST /api/session/{sessionID}/interrupt
POST /session/{id}/summarize     501  alt=POST /api/session/{sessionID}/compact
GET /session/{id}/message        501  alt=GET /api/session/{sessionID}/message
POST /session/{id}/message       501  alt=POST /api/session/{sessionID}/prompt
POST /session/{id}/prompt_async  501  alt=POST /api/session/{sessionID}/prompt
GET /agent                       501  alt=GET /api/agent
GET /provider                    501  alt=GET /api/provider
GET /session/status              501  alt=-
PATCH /session/{id}              501  alt=-
GET /session/{id}/children       501  alt=-
GET /session/{id}/todo           501  alt=-
GET /config                      501  alt=-
POST /log                        501  alt=-
PUT /auth/openai                 501  alt=-
POST /tui/show-toast             400  (served — reached a validating handler)
GET /event                       400  (served)
```

**The routes that cannot name an alternative say so honestly**, rather than inventing
one:

```
$ curl http://127.0.0.1:18931/config
"apiAlternative": null,
"hint": "this pre-/api route is registered but has no local backend, and
         `client.config.get` has no served /api equivalent here; there is no
         alternative call that works today"
```

I checked that "no served /api equivalent" is truthful rather than a shrug, by probing the
obvious candidates for every alt-less route:

```
GET /api/config -> 404   GET /api/session/status -> 404   GET /api/log -> 404
GET /api/auth/openai -> 404   GET /api/session/{id}/children -> 404
GET /api/session/{id}/todo -> 404   POST /api/log -> 404
PATCH /api/session/{id} -> 405  (path exists, method genuinely not served)
```

All confirm the claim. The `backing:"not-implemented"` field and the `surfaceCoverage`
summary together mean a plugin author who gets a 501 can now tell, from the response
alone, whether to switch calls or stop trying. That is the honesty I asked for, and it
went further than the minimum.

**Verdict: F3-W10-D2 is closed.**

**Incidental positive, still present.** `serve` continues to announce its own
insecurity unprompted:

```
Warning: OPENCODE_SERVER_PASSWORD is not set; server is unsecured.
opencode server listening on http://127.0.0.1:18931
```

---

### S3 — non-canonical `parts` mutation diagnostic (F3-W10-O6) — **UNCHANGED. Still one line, still names the hook and not the member.**

I widened the test from wave 10's three variants to five distinct malformations, each a
plausible first attempt by a plugin author, all through the same hook and plugin:

| `parts.push(...)` | rc | stdout | diagnostic |
|---|---|---|---|
| *(no mutation)* | 0 | `SCRIPT_EXHAUSTED_DONE` | — |
| `{type:"text", text:"BARE_PART"}` | 1 | *(empty)* | `plugin <path> failed in hook chat.message` |
| `{text:"NO_TYPE"}` — no `type` | 1 | *(empty)* | `plugin <path> failed in hook chat.message` |
| `{hello:"world"}` — nothing valid | 1 | *(empty)* | `plugin <path> failed in hook chat.message` |
| `{type:"nonexistent_kind",text:"x"}` | 1 | *(empty)* | `plugin <path> failed in hook chat.message` |
| `"just a string"` — not an object | 1 | *(empty)* | `plugin <path> failed in hook chat.message` |

Five different mistakes — a missing discriminant, an unknown discriminant, a wholly
foreign object, a non-object, and the merely-incomplete-but-well-shaped part — produce
**byte-identical** output. The turn dies, no assistant text is emitted, and the one line
the user gets does not say which member was rejected or why.

The reason is not recoverable at any verbosity I can reach. Full stderr at `DEBUG`, all
five lines:

```
DEBUG oc_cli::cmd::plugin_runtime: auto-discovered JavaScript plugin plugin="file:///.../mut.js" ...
DEBUG rustls_platform_verifier::verification::others: Loaded 122 CA root certificates ...
DEBUG rustls_platform_verifier::verification::others: Loaded 122 CA root certificates ...
DEBUG rustls_platform_verifier::verification::others: Loaded 122 CA root certificates ...
plugin file:///tmp/opencode/f3w11/mut/xdg/opencode/plugin/mut.js failed in hook chat.message
```

Three of the five lines are TLS root-store noise. `RUST_LOG=trace` is not an escape hatch
either — `rc=1 lines=1`, i.e. it produced *less*. And `--log-level` caps at DEBUG:

```
$ opencode-rust run --log-level TRACE ...
error: invalid value 'TRACE' for '--log-level <LOG_LEVEL>'
  [possible values: DEBUG, INFO, WARN, ERROR]
```

(That message itself is good — it enumerates the valid values. I note the cap only
because it closes off the last place the reason might have been hiding.)

**Severity: unchanged, defect-adjacent.** No data loss; the failure is loud in the sense
that it stops the turn. But contrast it with what S1 now produces for a *load* failure —
`failed 'init': Plugin export is not a function` — which names the actual cause. The
load path got a real diagnostic this wave; the hook path did not. It is the same
"names the hook, not the member" gap wave 9 raised for config write-backs, now the third
wave I have reported it.

---

### S4 — plugin load failure: latency + silence + doc prerequisites (F3-W10-O7) — **latency PARTLY FIXED, doc gap UNCHANGED**

I split the failure into its two real causes, which wave 10 conflated.

**Case A — no JS runtime at all. FIXED, and the message is now actionable.** With `bun`
absent from PATH entirely:

```
rc=0 elapsed=0s
WARN oc_cli::cmd::plugin_runtime: JavaScript plugin did not fully load
  plugin=file:///.../mut.js  kind=MissingRuntime
  no JavaScript runtime found on PATH or in user tool directories (looked for bun, node);
  these plugins cannot load: file:///.../mut.js. Install `bun` (preferred) or `node`.
```

0s instead of 30s, a distinct `kind=MissingRuntime`, the searched runtimes named, the
affected file named, and an explicit remedy. This is exactly the information wave 10 said
was missing — and it means the product now tells you the prerequisite even though the
docs still do not (below).

**Case B — a runtime that exists but cannot serve. STILL 30s, still silent by default.**
I planted a `bun` on PATH that exits 1 immediately:

```
###### C) with --print-logs ######
rc=0 elapsed=30s stdout='SCRIPT_EXHAUSTED_DONE'
WARN ... kind=FailedToLoad plugin `file:///.../mut.js` did not connect back within 30000 ms

###### D) DEFAULT level — what a user actually sees ######
rc=0 elapsed=31s stderr_bytes=0 stdout='SCRIPT_EXHAUSTED_DONE'
```

31 seconds, zero bytes on stderr, exit 0. The child process was dead within milliseconds
— it exits 1 unconditionally — yet the host waits out the full 30 000 ms window. The wait
is not adaptive to child exit. `kind=FailedToLoad` vs `kind=MissingRuntime` does at least
distinguish the two cases once you pass `--print-logs`.

**This matters more this wave than last, because of S1.** Discovery is now automatic. In
wave 10 you only paid this cost if you had explicitly listed a plugin in `config.plugin`;
now a stale file in `~/.opencode/plugin/` — a directory the docs tell you to use — adds
30 silent seconds to *every* `run` invocation, on a machine whose `bun` broke. I hit
precisely this by accident at the start of S1, via a stale mise shim:

```
DEBUG oc_plugin::js::host: javascript plugin stderr ... mise ERROR bun is not a valid shim.
  This likely means you uninstalled a tool and the shim does not point to anything.
```

A real user's `mise`-managed `bun` going stale is not a contrived scenario.

**Doc prerequisites — UNCHANGED, still nothing.** The whole `docs/` tree:

```
$ grep -rn -iw "bun" docs/            -> (no matches)
$ grep -rn "opencode-ai/sdk" docs/    -> (no matches)
$ grep -n -i -E "\bnode\b" docs/plugin-authoring.md  -> (no matches)
```

`docs/plugin-authoring.md` describes the JS plugin surface in detail — hook table,
discovery order, provenance — and never states that a JS plugin needs a `bun` or `node`
runtime, nor that it needs a resolvable `@opencode-ai/sdk` in `node_modules`. Those two
prerequisites cost me the bulk of wave 10's plugin work and cost me the first 20 minutes
of this wave again. The runtime half is now at least discoverable at runtime via
`kind=MissingRuntime`; the SDK half is not documented anywhere and not named by any
diagnostic.

**Severity: defect-adjacent (documentation + one non-adaptive timeout).** The remedy is
two sentences in the authoring guide, and treating child exit as a load failure instead
of waiting for the timeout.

---

### S5 — title generator vs. the resolved API surface (F3-W10-O8) — **UNCHANGED, and I have now MEASURED the consequence I could only infer last wave**

Built a **dual-surface mock** that answers `/chat/completions` in chat-chunk dialect and
`/responses` in typed `response.*` dialect, and records path + body shape per request.
That lets me attribute each request to the user turn or the internal title call.

**The split reproduces exactly, on both providers.** Same provider, same model, same
config, one `run` invocation:

```
$ opencode-rust run -m azure/test-model "F3W11 azure surface probe"
rc=0  stdout='RESPONSES_SURFACE_OK'

TITLE-GEN  path=/v1/chat/completions   shape=messages  tools=False
USER TURN  path=/v1/responses          shape=input     tools=True
```

Copilot, per model id — the surface heuristic itself is still correct, the title call is
still not:

| model | USER TURN | TITLE-GEN |
|---|---|---|
| `gpt-5` | `/v1/responses` (`input`) | `/v1/chat/completions` (`messages`) |
| `gpt-5-mini` | `/v1/chat/completions` | `/v1/chat/completions` |
| `gpt-4o` | `/v1/chat/completions` | `/v1/chat/completions` |

**Now the measurement wave 10 was missing.** I built a **Responses-only** mock — `/responses`
serves, `/chat/completions` returns `404 {"error":{"code":"404","message":"Resource not
found"}}` — which is what a real Azure deployment exposing only the Responses API looks
like. Result:

```
$ opencode-rust run -m azure/test-model "responses only deployment"
rc=0
RESPONSES_SURFACE_OK          <- the user's turn is fine
stderr: (empty)

server log:
REQ 2 path=/v1/chat/completions   <- title call, answered 404
REQ 3 path=/v1/responses          <- user turn, answered 200
```

The consequence, measured rather than inferred:

```
$ opencode-rust session list          (responses-only deployment)
Session ID                            Title
ses_1fd49d4d5d924a66887d2b29ada3952c  New session

$ opencode-rust session list          (deployment that also serves chat)
ses_1dfbd9947e7745a1bbff77c61310c1f9  CHAT_SURFACE_OK
ses_778f9d54d62b43a69e00a2fc60b7fc3c  CHAT_SURFACE_OK
```

**Every session on a Responses-only deployment is permanently titled "New session".**
The 404 is swallowed with no signal whatsoever — empty stderr, rc=0, and nothing at
`--log-level DEBUG --print-logs` either:

```
$ grep -i -E "404|title|not found|chat/completions|Resource" stderr   -> (no matches)
   (8 stderr lines total, all unrelated)
```

**Correcting my own first measurement**: I initially recorded 8s for the responses-only
run and suspected retry cost. Repeated timing shows that was first-run setup on a fresh
data dir, not the 404. The real numbers are indistinguishable:

```
dual-serves-both  mean = 222ms   (225, 219)
responses-only    mean = 232ms   (243, 222)
```

So there is no latency penalty. The defect is purely the silent loss of a feature.

**Severity: defect-adjacent, but firmer than last wave.** It is no longer inference: on a
Responses-only deployment the product silently and permanently loses session titling, and
gives the user no way to find out why. The fix is to route the internal title call through
the same surface resolution as the user turn — which is adjacent to what todo 162 just did
for model selection (S6).

**Incidental positive.** The wave-10 trap I recorded still bites and is worth repeating:
declaring `npm: "@ai-sdk/openai-compatible"` for provider `azure` silently loses the
Responses surface — I reproduced it again this wave by accident, getting
`CHAT_SURFACE_OK` where I expected Responses. The transport id (`@ai-sdk/azure`,
`@ai-sdk/github-copilot`) is what selects the surface.

---

### S6 — todo 162 dispatch decisions — **heuristic VERIFIED; the "advertised endpoint" override I was asked to test I could NOT verify; and I found a NEW provenance-dependent inconsistency**

Three separate results here. The third is a new finding.

**(a) The model-id heuristic works, and `mai-code-1-flash-picker` is handled.** Against
the dual mock, with the model declared in user config and the copilot transport id:

```
USER-CONFIG gpt-5                    -> /v1/responses
USER-CONFIG gpt-5.2                  -> /v1/responses
USER-CONFIG gpt-5.4                  -> /v1/responses
USER-CONFIG gpt-5.6-luna             -> /v1/responses
USER-CONFIG gpt-5-mini               -> /v1/chat/completions   (documented exclusion)
USER-CONFIG mai-code-1-flash-picker  -> /v1/chat/completions
```

So the heuristic-hostile id does not accidentally trip the `gpt-5+` rule. Note that a
`mai-*` id landing on Chat is the *default*, so this run does not distinguish "correctly
defaulted" from "advertisement ignored" — see (b).

**(b) COULD NOT VERIFY — the endpoint advertisement.** My brief asks me to check that a
Copilot model *advertising* `responses` dispatches Responses despite a hostile id, and
that an explicit `chat` endpoint on a Responses-heuristic id dispatches Chat. **I could
not find any way to make a model advertise an endpoint**, so I have no observation
either way. What I tried, all measured, none changed dispatch:

- user config, model level: `supported_endpoints:["responses"]`, `endpoint:"responses"`,
  `endpoints:["responses"]`, `api:"responses"` — all still `/v1/chat/completions`
- user config, inside `options`: `options.endpoint`, `options.api`,
  `options.supported_endpoints` — all still Chat
- the **models.dev catalog** on disk (`~/.cache/opencode/models.json`, 3.6 MB, real
  data), same four key names injected into the `mai-code-1-flash-picker` entry — all
  still Chat
- reverse direction: `gpt-5` + `endpoint:"chat"` and `+ options.endpoint:"chat"` — still
  `/v1/responses`, i.e. no override took effect

I confirmed the catalog file is genuinely read, so the injection point was live:

```
$ (add "f3w11-canary-model" to the catalog's github-copilot models)
$ opencode-rust models | grep -i canary
github-copilot/f3w11-canary-model
```

I also confirmed no runtime surface exposes the field name — `debug config` echoes only
keys I set, unknown model keys are silently dropped (`zzz_not_a_key` accepted, rc=0), and
the OpenAPI document at `/openapi.json` has 6 schemas and zero occurrences of
`endpoint` / `supported_endpoints` / `surface`. `grep -rn` over `docs/` finds no
documentation of an endpoint-advertisement field at all.

**So: unverified, and separately, undocumented and not reachable from any user-facing
surface I could find.** If the advertisement is only readable from an upstream catalog
field this build does not accept in config, that is worth declaring.

**(c) NEW FINDING — F3-W11-D1: identical provider + model id dispatch to different wire
surfaces depending on where the model metadata came from.**

Same provider id, same model id, same mock, same catalog on disk. The only variable is
whether the model entry is declared in `opencode.json` or sourced from the models.dev
catalog:

| provider / model | declared in user config | sourced from catalog |
|---|---|---|
| `github-copilot/gpt-5.2` | **`/v1/responses`** | **`/v1/chat/completions`** |
| `github-copilot/gpt-5.4` | `/v1/responses` | `/v1/chat/completions` |
| `github-copilot/gpt-5.6-luna` | `/v1/responses` | `/v1/chat/completions` |
| `github-copilot/gpt-5-mini` | `/v1/chat/completions` | `/v1/chat/completions` |
| `azure/gpt-5` | `/v1/responses` | `/v1/responses` |

I eliminated the obvious confounds:

1. **Not the `npm` transport id.** For a catalog-sourced model, the user-config `npm` is
   simply ignored — including a deliberately wrong one:

   ```
   provider=azure           user-npm=NONE                      gpt-5   -> /v1/responses
   provider=azure           user-npm=@ai-sdk/openai-compatible gpt-5   -> /v1/responses
   provider=github-copilot  user-npm=@ai-sdk/github-copilot    gpt-5.2 -> /v1/chat/completions
   provider=github-copilot  user-npm=@ai-sdk/azure             gpt-5.2 -> /v1/chat/completions
   ```

   (Azure ignoring `@ai-sdk/openai-compatible` and still choosing Responses is itself
   worth noting — it means my wave-10 "transport id selects the surface" reading was
   incomplete: for catalog-sourced models the **provider id** decides.)

2. **Not the richer catalog metadata.** I stripped the catalog's `gpt-5.2` entry down to
   exactly the minimal field set my user config used — same `id`, `name`, `tool_call`,
   `release_date`, `limit`, `cost`, `options`, nothing else:

   ```
   CATALOG minimal-shape gpt-5.2 -> /v1/chat/completions
   USER-CONFIG        gpt-5.2 -> /v1/responses      (same catalog file present)
   ```

   Byte-comparable model entries, opposite surfaces. Provenance is the deciding factor.

**Why this matters.** Real users do not hand-declare Copilot models; they authenticate and
take the catalog. So on the path every real Copilot user is on, the Responses surface
**never engages for any Copilot model** — I swept the whole catalog list and every one of
the 11 ids I tried went to `/v1/chat/completions`, including `gpt-5.2`, `gpt-5.4`,
`gpt-5.6-luna`, `gpt-5.2-codex`:

```
gpt-4.1 -> chat   gpt-5-mini -> chat   gpt-5.2 -> chat   gpt-5.2-codex -> chat
gpt-5.4-nano -> chat   gpt-5.6-luna -> chat   claude-opus-5 -> chat
gemini-3.5-flash -> chat   grok-4.5 -> chat   kimi-k3 -> chat
mai-code-1-flash-picker -> chat
```

The gpt-5-family Responses rule is documented and demonstrably implemented, yet it is
unreachable in the default configuration for Copilot. Azure is unaffected (catalog and
config agree).

**What I claim and what I do not.** I claim, as measured against a socket: within this
build, provenance changes the wire surface for the same provider+model. I do **not**
claim what real GitHub Copilot expects — no credentials — so I cannot say whether Chat or
Responses is the correct answer for `gpt-5.2` on the live service. The **inconsistency
inside this build** is the defect regardless of which one is right: one of the two paths
must be wrong. `grep -in "responses\|surface"` over `docs/divergences.md` finds nothing
declaring this, and `docs/` documents the surface rule in only one place
(`compatibility-matrix.md`) without mentioning provenance.

**Severity: defect (undeclared behavioural inconsistency).** Not data-threatening.

---

### S7 — regression re-confirmation on the moved tree

**(a) Stalled-provider idle bound (wave-9 finding / todo 154) — STILL HOLDS.** Hostile
provider sends two SSE deltas then holds the socket open forever, no FIN, no RST:

```
$ OPENCODE_STREAM_IDLE_TIMEOUT_SECS=5 opencode-rust run -m test/test-model "stall probe"
rc=1  elapsed=20s
stdout: PARTIAL_ONE PARTIAL_TWO
stderr: transient provider failure (status=None): provider `test` response stream idle
        timeout after 5s; raise OPENCODE_STREAM_IDLE_TIMEOUT_SECS for slower providers
```

Bounded, partial output preserved, error names the mechanism and the remedy. The 20s for a
5s bound is the documented retry doubling, consistent with wave 10 (still undocumented —
my O2).

**(b) `edit` permission subject and diff — STILL CORRECT.** Driven in the real TUI:

```
│ Permission required
│△ Permission required
│  → Edit /tmp/opencode/f3w11/perm/proj/target.txt
│  Path: /tmp/opencode/f3w11/perm/proj/target.txt
│
│@@ -1,1 +1,1 @@
│   1-ORIGINAL_LINE_BRAVO                    1+REPLACED_LINE_CHARLIE
│
│ Allow once   Allow always   Reject
│↑↓ select  enter confirm  ctrl+f fullscreen
```

Correct subject, correct path, correct unified diff of the actual replacement.

**Denial is honoured end-to-end.** Selecting Reject:

```
✗ → edit error
    tool edit was denied by the permission layer

$ cat target.txt
ORIGINAL_LINE_ALFA
ORIGINAL_LINE_BRAVO      <- untouched
ORIGINAL_LINE_DELTA
```

Same for `webfetch` and `write`. Approving instead performs the operation:
`target.txt` became `REPLACED_LINE_CHARLIE` and `target.txt.new2` was created containing
`F3_WRITE_BODY_LINE`. Subjects for the other kinds are right too —
`# Shell command / $ echo F3_BASH_SUBJECT_MARKER`, and
`% WebFetch https://example.invalid/... / URL: ...`.

**O3 (duplicate title) still reproduces** — `Permission required` renders twice on every
kind, and the always-allow confirm renders `Always allow` twice.

**(c) `write` collapses into `permission.edit` — DECLARED, so NOT a defect. Reporting it
because it surprised me and the declaration is hard to find.** I measured:

```
permission {"write":"ask"}  -> NO prompt at all; target.txt.new2 created silently
permission {"edit":"ask"}   -> the write tool DOES prompt
```

So `permission.write` is inert. Before calling that a defect I checked, and it is declared
in `docs/rejected-inputs.md`:

> use `permission` — `write`, `edit`, and `patch` all collapse to `permission.edit`

That makes the behaviour correct by design, and it also explains why the write dialog is
headed `→ Edit <path>` and why its always-allow text reads "This will allow **edit** until
OpenCode is restarted" — write really is the edit bucket. Withdrawing any defect claim.

Two residual ergonomics notes, both observations:

- The collapse is stated only inside the *deprecation message* for the old
  `agent.build.tools` / `tools` keys. A user writing `permission: {"write":"ask"}` from
  scratch never encounters that message, and gets silent file creation.
- The `permission` map accepts anything. `debug config` echoes back
  `{"zzz_bogus_tool": "ask"}` with rc=0 and no warning. Every key I tried —
  `edit write bash webfetch read grep glob patch todowrite task` — loads verbatim. So an
  inert or misspelled key is indistinguishable from a working one. This is the same
  no-validation shape as my wave-10 O10 about `theme`.

**(d) O4 re-read.** The `write` prompt shows no content preview — for
`target.txt.new2` the dialog gives subject + path and nothing else, no `F3_WRITE_BODY_LINE`.
Given (c) this is coherent (an edit-shaped dialog with no old text to diff against) but the
user still approves a file creation without seeing what goes in it.

**(e) NEW observation, F3-W11-O1 — the diff vanished on a re-rendered permission request
and `ctrl+f` did not bring it back.** Observed once, in the `edit`-only run: the first
render carried the `@@ -1,1 +1,1 @@` hunk; after my keypress the dialog re-rendered for the
same pending `edit` call with subject and path but **no diff**, and unlike wave-10's O5 the
fullscreen toggle did not recover it — fullscreen showed the subject, the path, the button
row, and ~25 blank lines:

```
│ Permission required
│△ Permission required
│  → Edit /tmp/opencode/f3w11/p_editonly/proj/target.txt
│  Path: /tmp/opencode/f3w11/p_editonly/proj/target.txt
│
│ Allow once   Allow always   Reject
│   (then ~25 empty gutter lines)
│↑↓ select  enter confirm  ctrl+f minimize
```

The dialog was still live (arrow keys moved the selection, Enter opened the always-allow
confirm, and approving it let the chain continue and the edit apply). Pane was 200x50, so
this is not the squeeze case from O5. **I saw this once and did not isolate a reproducer**,
so I report it as an observation with the conditions, not as a defect.

**(f) migration-ceiling refusal and `kiro-auth` in `models`** — see S8 below; run through
the real commands.

---

### S8 — gate run — **GREEN, matches the expected figure exactly**

```
$ cargo test --workspace --offline
passed=3426  failed=0  ignored=2
```

Summed across all 212 `test result:` lines. No `EAGAIN`, no panics, no FAILED lines — one
run, no retry needed.

---

### S7 continued — migration-ceiling refusal through the real `db` command

**The refusal itself works, and the data survives.** Both documented refusal shapes were
driven through the real command:

```
### A) non-empty db with no session table
$ OPENCODE_DB=.../nosession.db opencode-rust db "select 1"
rc=1
stderr: migration to schema version 38 failed

### B) session table but NEITHER journal
$ OPENCODE_DB=.../nojournal.db opencode-rust db "select 1"
rc=1
stderr: migration to schema version 38 failed
$ (inspect afterwards)
sessions: [('ses_precious', 'DO_NOT_LOSE_ME')]      <- intact
```

`rc=1` on stderr, and no session row lost. That is the important part and it holds.
(My first measurement read rc=0; that was my pipe to `head` masking the status. Re-measured
without the pipe: rc=1. Correcting my own error, not the product's.)

Two gaps, both worth an owner's attention:

**(f1) The specific reason documented in `docs/migration.md` is never printed.** The doc
says the refusal looks like:

> ```text
> database is not empty and has no session table
> ```

What the binary actually prints, for **both** distinct refusal causes, is the same generic
line:

```
migration to schema version 38 failed
```

No chained cause, and nothing more at `--log-level DEBUG --print-logs`. `session list`
gives the identical line. So a user cannot tell "no session table" from "session table but
no journal" — two problems with different remedies — and the message they were told to
expect does not exist. This is the same "names the operation, not the reason" pattern as
S3 and O7.

**(f2) The refused database IS modified, which the doc says it is not.**
`docs/migration.md` states a neither-journal database "is refused **without being
modified**" and "the test asserts the data is left untouched". Measured on a fresh case:

```
md5 before = e20c109cf927a4d820e63cc7fc5caa1e
md5 after  = b0a2860e61c5c211ea56f6ff381f25e2
FILE MODIFIED
tables before: ['session']
tables after : ['migration', 'session']
```

A `migration` table was created in the user's database before the refusal. The *rows* are
untouched — `('ses_p','KEEP')` survives, so the doc's "data is left untouched" is true in
the narrow sense — but "without being modified" is not true of the file. This matters
because the surrounding section is precisely the one telling users to back up first and
promising the TypeScript binary can still open the file afterwards; a stray `migration`
table in a database that was *refused* is exactly the state a rollback would encounter.
**Severity: defect (documentation contradicts observed behaviour), low impact.**

### S7 continued — `kiro-auth` in `models` — **COULD NOT VERIFY**

`kiro-auth` is not a builtin provider in this build. Checked all three places:

```
$ opencode-rust models  | grep -ci kiro     -> 0
$ opencode-rust providers list              -> "0 credentials"
$ (models.dev catalog, 183 providers)       -> providers matching "kiro": []
```

It is a **plugin** — `@sunerpy/opencode-kiro-auth`, cited as such in
`docs/plugin-authoring.md:21` and as a measured v1 caller in `docs/v1-surface-capture.md`.
The npm package is **not installed** on this host (`/config/.config/opencode/node_modules/@sunerpy/`
holds only `oh-my-openagent*`), and I cannot install it offline. So a provider it would
register cannot appear, and 0 hits here is the expected result of a missing plugin rather
than evidence of a regression. **I make no claim either way.** To verify this properly the
plugin package has to be present.

---

### S9 — break-it pass

**Bad flags and arguments — all correct, all distinguishable.**

```
run --nope "x"            rc=2  error: unexpected argument '--nope' found
frobnicate                rc=2  error: unrecognized subcommand 'frobnicate'
run -m notaslash "x"      rc=1  model must be provider/model, got "notaslash"
run -m nosuch/model "x"   rc=1  Model not found: nosuch/model
run -m "test/" "x"        rc=1  Model not found: test/
--help                    rc=0  A Rust reimplementation of the OpenCode agent
run --help                rc=0  Run OpenCode with a message
```

rc=2 for argv errors vs rc=1 for semantic errors is the right split, and the messages name
the offending input. `--log-level TRACE` enumerating its four valid values (S3) is the same
good pattern.

**Empty state.** `session list` on a fresh data dir: `rc=0`, and **no output at all** — not
even a header or a "no sessions" line. Defensible, slightly unfriendly. Observation only.

**Missing config.** `models` and `run` with no config and no catalog cache fail cleanly:
`fetching the model catalog from 'https://models.opencode.ai' failed`, rc=1. Names the URL.

**Malformed and hostile config — precise, with a path for every issue.**

```
'{ this is not json'            rc=1  config file ... is not valid JSON
''  (empty file)               rc=1  config file ... is not valid JSON
'null'                         rc=1  <document>: invalid type: null, expected struct Config at line 1 column 4
'[]'                           rc=1  <document>: invalid length 0, expected struct Config with 37 elements
'{"formatter":"yes"}'          rc=1  formatter: data did not match any variant of untagged enum FormatterConfig
'{"provider":"notanobject"}'   rc=1  provider: invalid type: string "notanobject", expected an object
'{"permission":{"edit":"maybe"}}' rc=1 permission.edit: data did not match any variant of untagged enum PermissionRule
'{"unknown_top_key":1}'        rc=1  unknown_top_key: unrecognized key
'{"theme":12345}'              rc=0  (accepted — my wave-10 O10, unchanged)
```

Line/column plus a dotted path on nearly everything. `theme` remains the lone unvalidated
key.

**This sharpens S7(c): the `permission` map validates its VALUES but not its KEYS.**

```
permission.edit="maybe"      rc=1  precise enum error
permission.zzz_bogus="ask"   rc=0  accepted silently
permission.write="ask"       rc=0  accepted silently, and inert (collapses to edit)
```

So the one key a user is most likely to reach for by analogy (`write`) is accepted, does
nothing, and is indistinguishable from a typo. A key allow-list would catch both.

**Deep nesting boundary (wave-10 S5) — reproduces at exactly the same place.** Using a
config key that accepts arbitrary JSON:

```
depth 120  ->  mcp.m.0: invalid type: sequence, expected a boolean   (parsed; type error)
depth 124  ->  mcp.m.0: invalid type: sequence, expected a boolean   (parsed)
depth 125  ->  mcp.m.0: invalid type: sequence, expected a boolean   (parsed)
depth 126  ->  config file ... is not valid JSON                     (VALID json called invalid)
depth 130  ->  config file ... is not valid JSON
```

Boundary still 125/126, still reported as "not valid JSON" when the JSON is valid — it is a
recursion-depth limit misattributed to syntax. Exit code and stream are correct. Unchanged
from wave 10, still undeclared.

**Interrupted stream — provider closes mid-stream (FIN).** Handled immediately and
distinctly from the stall case:

```
rc=1  elapsed=0s
stdout: PARTIAL_ONE PARTIAL_TWO       <- partial preserved
stderr: transient provider failure (status=None): error decoding response body:
        error reading a body from connection: unexpected EOF during chunk size line
```

0s (not the idle timeout), partial output kept, cause named.

**Ctrl-C during a long turn — WORKS. Correcting two false measurements of my own.** I first
measured SIGINT as ignored (full output delivered, rc=0), then "confirmed" it via
`/proc/<pid>/status` showing `SigIgn` bit 2. Both were **my harness, not the product**: a
backgrounded job inherits `SIG_IGN` for SIGINT, and my second attempt used `trap '' INT` in
the parent shell, which children inherit across exec. Measured cleanly, as a plain
foreground process group in a tmux pane with no trap anywhere:

```
$ (tmux pane runs the binary directly)
SigIgn [13]              <- only SIGPIPE, which is normal for Rust
SigCgt [7, 11]           <- SIGBUS, SIGSEGV; SIGINT is NOT caught
  PID     PGID    TPGID  STAT
2707247 2707247 2707247  Ssl+    <- is the foreground process group

$ (send Ctrl-C)
Pane is dead (signal 2, Wed Aug 12 06:55:37 2026)
```

SIGINT gets the default action and terminates the run. **No defect — withdrawing it before
it was ever reported.** I also measured `serve` as ignoring SIGHUP/SIGINT/SIGQUIT, but that
process was launched with `nohup ... &`, so that reading is contaminated the same way and I
draw no conclusion from it.

**Concurrent clients — clean.** 12 parallel session creations against one `serve`:

```
status codes: 200 200 200 200 200 200 200 200 200 200 200 200
distinct session ids created: 12
health after the burst: 200
sessions listed: 13
```

No duplicate ids, no 5xx, server healthy afterwards, and the count reconciles (12 new + 1
pre-existing from S2).

**Bounded SSE read.** `/api/event` emits the connect frame and then holds open, as expected:

```
$ curl -s --max-time 5 -N http://127.0.0.1:18931/api/event | head -c 400
data: {"data":{},"id":"evt_019ff4c38af37b208d701104bb2d0c4f","type":"server.connected"}
[bounded read complete]
```

---

## Summary

| # | scenario | subject | result |
|---|---|---|---|
| S1 | plugin auto-discovery, 8 documented locations | F3-W10-D1 / todo 163 | **FIXED** |
| S2 | v1 `not_implemented` hint + alternative | F3-W10-D2 / todo 164 | **FIXED** |
| S3 | non-canonical `parts` mutation diagnostic | F3-W10-O6 | UNCHANGED |
| S4a | no JS runtime: 0s + actionable message | F3-W10-O7 | **FIXED** |
| S4b | broken runtime: 30s silent | F3-W10-O7 | UNCHANGED |
| S4c | `bun` / SDK prerequisites in docs | F3-W10-O7 | UNCHANGED |
| S5 | title generator vs. resolved surface | F3-W10-O8 | UNCHANGED, consequence now measured |
| S6a | gpt-5 family heuristic, incl. `mai-code-*` | todo 162 | PASS |
| S6b | endpoint advertisement override | todo 162 | **COULD NOT VERIFY** |
| S6c | provenance changes the wire surface | new | **DEFECT (F3-W11-D1)** |
| S7a | stalled-provider idle bound | wave-9 | PASS |
| S7b | `edit` permission subject + diff + denial | wave-9/10 | PASS |
| S7c | `write` collapses to `permission.edit` | new | declared, not a defect |
| S7d | migration-ceiling refusal | wave-9 | refuses correctly; 2 doc gaps |
| S7e | `kiro-auth` in `models` | wave-9 | **COULD NOT VERIFY** |
| S8 | `cargo test --workspace --offline` | gate | 3426 / 0 |
| S9 | break-it pass | — | PASS, 1 unchanged boundary |

### Closed this wave

- **F3-W10-D1** — plugin auto-discovery. All eight documented locations load and execute,
  provenance and scope are correct, and a rejected file's reason is named at WARN. Carried
  three waves.
- **F3-W10-D2** — the v1 501 body now names a working `/api` alternative (verified by
  calling it), reports honest coverage, admits when no alternative exists, and cites no
  todos.
- **Half of F3-W10-O7** — a missing JS runtime is now 0s with `kind=MissingRuntime` naming
  the searched runtimes, the affected file, and the remedy.

### Defects

- **F3-W11-D1 (new) — the same provider+model dispatches to different wire surfaces
  depending on where the model metadata came from.** `github-copilot/gpt-5.2` declared in
  `opencode.json` → `/v1/responses`; the same id sourced from the models.dev catalog →
  `/v1/chat/completions`. Confounds eliminated: not the `npm` transport id (ignored for
  catalog models, including a deliberately wrong one), and not the richer metadata (a
  catalog entry reduced to the identical minimal field set still went to Chat). Consequence:
  on the path every real Copilot user is on, the Responses surface never engages — all 11
  catalog ids I tried, including `gpt-5.2`, `gpt-5.4`, `gpt-5.6-luna`, `gpt-5.2-codex`, went
  to Chat. Azure is consistent. Undeclared.
- **S7d — `docs/migration.md` contradicts observed behaviour on a refused database.** The
  doc says the neither-journal shape "is refused without being modified"; measured, the file
  changes (`md5` differs, a `migration` table is created) though the session rows survive.
  Separately, the documented message `database is not empty and has no session table` is
  never printed — both distinct refusal causes emit only
  `migration to schema version 38 failed`, with no chained cause at any log level.

### Defect-adjacent (an owner should decide)

- **F3-W10-O6 (3rd wave) — a bad `parts` member kills the turn with one opaque line.** Five
  different malformations produce byte-identical `plugin <path> failed in hook chat.message`.
  Nothing more at DEBUG (3 of 5 stderr lines are TLS noise), `--log-level` caps at DEBUG,
  `RUST_LOG=trace` gives less. Contrast S1, where the *load* path now names the real cause.
- **F3-W10-O7 residue — a runtime that starts but cannot serve costs 31s of total silence**
  (`stderr_bytes=0`, rc=0) because the 30 000 ms connect-back window is not adaptive to
  child exit. Now worse than in wave 10 precisely because discovery is automatic: a stale
  file in a documented directory taxes every run. And `docs/` still never mentions that a JS
  plugin needs `bun`/`node` or a resolvable `@opencode-ai/sdk` (`grep -rn -iw bun docs/` and
  `grep -rn opencode-ai/sdk docs/` are both empty).
- **F3-W10-O8 — the title generator ignores the resolved API surface, and the consequence is
  now measured rather than inferred.** On a Responses-only deployment the user's turn
  succeeds while the title call 404s, so **every session is permanently titled "New
  session"** — with empty stderr, rc=0, and nothing at DEBUG. No latency penalty (222ms vs
  232ms), so the whole cost is a silently lost feature. Adjacent to what todo 162 fixed for
  model selection.

### Observations (low / cosmetic)

- **O2** (unchanged) — `OPENCODE_STREAM_IDLE_TIMEOUT_SECS`, its 120s default, 180s cap and
  the retry doubling are undocumented; the user waits ~2x the bound.
- **O3** (unchanged) — permission dialog renders its title twice, every kind; the
  always-allow confirm does too.
- **O4** (unchanged) — a `write` prompt shows no content preview. Coherent given the
  documented collapse to `edit`, but the user approves a file creation unseen.
- **F3-W11-O1 (new)** — a re-rendered `edit` permission request lost its diff and `ctrl+f`
  did **not** recover it (unlike wave-10's O5 squeeze case); pane was 200x50. The dialog
  stayed live and the edit applied on approval. Seen once, no reproducer isolated.
- **wave-10 S5** (unchanged) — valid JSON nested >=126 deep is reported as "not valid JSON";
  boundary still bisects at 125/126; undeclared.
- **`permission` map keys are unvalidated** — `zzz_bogus` and the inert `write` both load
  with rc=0, while the map's *values* get precise enum errors. Same shape as O10 (`theme`).
- **O10** (unchanged) — `theme` is the one config key with no type validation.
- **empty `session list`** prints nothing at all, not even a header.

### Withdrawn before reporting

- **SIGINT / Ctrl-C during a run.** I measured it as ignored twice; both were harness
  artifacts (backgrounded-job `SIG_IGN`, then inherited `trap '' INT`). Measured cleanly as
  a foreground process group with no trap: `SigIgn [13]` only, SIGINT not caught, and Ctrl-C
  kills the pane with `signal 2`. Working correctly.
- **`permission.write` being inert** as a defect — declared in `docs/rejected-inputs.md`
  ("`write`, `edit`, and `patch` all collapse to `permission.edit`").

### Could not verify

- **The endpoint-advertisement override (todo 162's second half).** I found no way to make a
  model advertise an endpoint: four key names at model level, three inside `options`, and
  the same four injected into the real on-disk models.dev catalog — none changed dispatch,
  in either direction. The catalog injection point was proven live (a canary model I added
  appeared in `models`). No runtime surface exposes the field either (`debug config` drops
  unknown model keys silently; `/openapi.json` has zero occurrences of
  `endpoint`/`supported_endpoints`/`surface`), and `docs/` never mentions it. So I report no
  observation, and separately note that it is undocumented and unreachable from any
  user-facing surface I could find.
- **`kiro-auth` appearing in `models`.** It is a plugin (`@sunerpy/opencode-kiro-auth`), not
  a builtin provider; it is absent from the 183-provider catalog and the package is not
  installed on this host, and I cannot install it offline. 0 hits is the expected result of
  a missing plugin, not evidence of a regression. No claim either way.
- **Live Azure / GitHub Copilot.** No credentials. Everything in S5 and S6 is measured
  against local mocks recording path and body on a real socket. I do not claim what the live
  services expect — which is why F3-W11-D1 is framed as an internal inconsistency (one of
  the two paths must be wrong) rather than "Chat is wrong".
- **F3-W11-O1** — observed once, not isolated.

## Verdict

**APPROVE WITH FINDINGS.**

The three items I carried into this wave were addressed, including the plugin-discovery
defect I had reported for three consecutive waves — and the fix went further than asked
(eight locations, correct scope, `kind`-tagged rejection reasons). The v1 hint fix is
genuinely better than the minimum: it names an alternative, proves coverage, and admits when
no alternative exists. The gate is green at exactly the expected 3426/0, and the core
behaviours I re-checked — idle bound, permission subject/diff/denial, migration refusal,
config validation, concurrency, stream interruption — all hold.

Nothing I found threatens data. The one new defect (**F3-W11-D1**) is a behavioural
inconsistency inside the build, and the strongest carried item (**F3-W10-O8**) is now a
measured, silent feature loss rather than an inference. Both are undeclared, and my
recommendation is the same for each: declare it or fix it.

Two patterns are worth naming for whoever picks these up, because three separate findings
share them:

1. **Diagnostics name the operation, not the cause.** `failed in hook chat.message`,
   `migration to schema version 38 failed` — same shape, two subsystems. The plugin *load*
   path fixed exactly this in todo 163 (`failed 'init': Plugin export is not a function`),
   so the pattern to copy already exists in the tree.
2. **Internal calls do not inherit the user turn's resolution.** The title generator picks
   its own API surface (O8); model metadata provenance picks its own surface (D1). Both are
   the same class of bug that todo 162 addressed for model selection.

Recorded honestly: I withdrew two findings this wave after my own harness turned out to be
at fault, and I could not verify two of the things my brief asked for. Those are stated
above rather than inferred.
