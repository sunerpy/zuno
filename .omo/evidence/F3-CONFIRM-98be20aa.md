# F3 — Round 3 Narrow Confirmation

- **Audited HEAD:** `98be20aa`
- **Previously approved at:** `647a2d64` (Round 2)
- **Worktree:** `/config/workspace/ProdDir/AI/oc-wt/tF3` (branch `task-F3`)
- **Reviewer:** F3 (real manual QA)
- **Verdict:** **APPROVE**

## Scope

Narrow same-HEAD confirmation. Single question: does the product still behave at
`98be20aa` the way I verified at `647a2d64`?

Product delta between those two shas is todo 179 only (288 insertions / 29
deletions): a build script generating an arrival enum from the pinned OpenAPI
capture, an exhaustive match over it, and generated path constants bound in
`crates/oc-server/src/{api/mod.rs,compat_v1.rs}`.

Assigned checks (regression-only):

1. v1 routes exercised in Round 2 — `PUT /auth/{providerID}`, both provider
   OAuth operations, and `/provider` returning the typed legacy document.
2. A plugin calling `client.provider.list()` still observes correct data,
   including `release_date` (the field that had been double-converted).
3. One ordinary turn completes end to end — CLI and HTTP.
4. Gate: `cargo test --workspace --offline` (expect 3473 / 0).

## Checks

<!-- appended as completed -->
### Check 0 — artifact actually built and run

`cargo build --offline` with `CARGO_TARGET_DIR=/tmp/opencode/f3c`:

```
Finished `dev` profile [unoptimized + debuginfo] target(s) in 30.42s
-rwxrwxr-x 2 abc abc 159432832 /tmp/opencode/f3c/debug/opencode-rust
```

Server launched from that binary and used for every check below:

```
opencode server listening on http://127.0.0.1:42731
```

Generated arrival constants (from the build script's real output at
`/tmp/opencode/f3c/debug/build/oc-plugin-sdk-*/out/generated_client_arrivals.rs`)
resolve byte-identically to the literals they replaced:

```
Self::ProviderList   => "/provider"
Self::V2ModelList    => "/api/model"
Self::V2ProviderGet  => "/api/provider/{providerID}"
Self::V2ProviderList => "/api/provider"
```

**Result: no drift.**

### Check 1 — Round-2 v1 routes still perform their real effect

`PUT /auth/{providerID}` — real effect confirmed on disk, not just a 200:

```
PUT /auth/f3check   -> HTTP 200   body: true
PUT /auth/f3check2  -> HTTP 200   body: true
```

`/config/.local/share/opencode/auth.json` afterwards contains both new entries:

```
['alb-openai', 'amazon-bedrock', 'awsopenai', 'dev-openai', 'f3check',
 'f3check2', 'google', 'kiro-auth', 'myopenai', 'newapi', 'nwcdai',
 'openai', 'zhipuai', 'zhipuai-coding-plan']
```

(Both scratch entries removed afterwards — see Cleanup.)

Note: `GET /auth` answers with the deliberate `unimplemented_v1_route`
diagnostic (`"/auth" is not part of the measured pre-/api surface`). That is
designed behavior, unchanged, not a regression.

Both provider OAuth operations still dispatch into the real plugin OAuth
machinery — argument validation first, then a genuine provider lookup, with no
canned/stub response:

```
POST /provider/anthropic/oauth/authorize {}            -> HTTP 400
  "provider OAuth authorize requires a JSON body with an integer `method`"
POST /provider/anthropic/oauth/authorize {"method":0}  -> HTTP 502
  "provider OAuth failed: plugin provider `anthropic` has no OAuth method 0"

POST /provider/anthropic/oauth/callback {}             -> HTTP 400
  "provider OAuth callback requires a JSON body with an integer `method`"
POST /provider/anthropic/oauth/callback {"method":0,…}  -> HTTP 502
  "provider OAuth failed: plugin provider `anthropic` method 0 has no active OAuth callback"
```

`GET /provider` still returns the typed legacy document, now served through
`GeneratedClientArrival::ProviderList.path()`:

```
HTTP 200  bytes=120393
top-level: ['all', 'connected', 'default']
all:       list len=6
connected: ["amazon-bedrock","awsopenai","cloudflare-ai-gateway","google",
            "myopenai","nwcdai","openai","zhipuai","zhipuai-coding-plan"]
all[0] keys: ['env','id','models','name','npm']
all[0] id=amazon-bedrock name="Amazon Bedrock" npm="@ai-sdk/amazon-bedrock"
  models: dict of 114
  model "amazon.nova-2-lite-v1:0" keys:
    ['attachment','cost','headers','id','limit','modalities','name',
     'options','reasoning','release_date','status','temperature','tool_call']
    release_date = "2024-12-01"
    cost  = {"input":0.33,"output":2.75,"cache_read":0,"cache_write":0}
    limit = {"context":128000,"output":4096}
```

`release_date` across the whole document — the field that had been
double-converted:

```
total models=292   release_date not ISO YYYY-MM-DD: 0
```

The three `/api/*` routes rebound to generated constants all serve real data,
and a near-miss path still 404s (so the router is not blanket-matching):

```
/api/model            HTTP 200 bytes=152233  keys=['data','location']
/api/provider         HTTP 200 bytes=1011    keys=['data','location']
/api/provider/openai  HTTP 200 bytes=216     keys=['data','location']
/api/providers        HTTP 404
```

**Result: nothing looked different from `647a2d64`.**
### Check 2 — a plugin calling `client.provider.list()` still observes correct data

Method. A JS plugin was auto-discovered from a scratch project
(`/tmp/opencode/f3proj/.opencode/plugin/f3check.js`, `scope=Local`) and the real
`@opencode-ai/sdk` was made resolvable to it. The plugin built a genuine
`createOpencodeClient` and called `client.provider.list()` against a live server
built from this HEAD. Observed plugin stderr, verbatim:

```
F3PLUGIN handed-client provider.list typeof=function
F3PLUGIN own-client provider.list typeof=function
F3PLUGIN reskeys=["data","request","response"]
F3PLUGIN topkeys=["all","connected","default"]
F3PLUGIN providers=6 connected=["amazon-bedrock","awsopenai","cloudflare-ai-gateway",
                                "google","myopenai","nwcdai","openai","zhipuai",
                                "zhipuai-coding-plan"]
F3PLUGIN models=292 bad_release_date=0
F3PLUGIN sample=amazon-bedrock/amazon.nova-2-lite-v1:0 release_date="2024-12-01" typeof=string
F3PLUGIN bedrock/amazon.nova-2-lite-v1:0 release_date="2024-12-01"
```

`release_date` arrives as a plain `string` in ISO `YYYY-MM-DD` form, 0 of 292
malformed — the double conversion is still absent. Provider/model counts and the
`connected` list match the direct HTTP read in Check 1 exactly.

**Result: nothing looked different from `647a2d64`.**

Two things I hit while setting this up, both pre-existing and neither caused by
todo 179, recorded so the next reviewer does not re-derive them:

- A `file:`-specifier plugin with no resolvable `@opencode-ai/sdk` receives a
  throwing proxy for `client` (`"@opencode-ai/sdk is unavailable to this file
  plugin fixture"`, `crates/oc-plugin/src/js/shim.mjs:460`). Deliberate.
- Under `serve`, plugin load completes *before* the listener binds, so a plugin
  cannot call its own server from `init` — it times out at 60s
  (`javascript plugin host failure … hook=Some("init") … kind=TimedOut`) and the
  server then binds and serves normally. Also deliberate-looking ordering, and
  untouched by this delta; I did not pursue it.
### Check 3 — one ordinary turn still completes end to end (CLI and HTTP)

Every real upstream credential in this environment is currently unusable, which
is an environment condition and not a property of this HEAD. Recorded so nobody
mistakes it for a regression:

```
amazon-bedrock/*        unrecoverable provider failure (status=Some(404)): Bedrock service error status=404
openai/gpt-5.6          authentication rejected by provider openai: … code `"token_expired"`
google/gemini-3.6-flash unrecoverable provider failure (status=Some(400)) … Some("INVALID_ARGUMENT")
zhipuai*/glm-5.2        rate limited by provider (retry_after=None)
cloudflare-ai-gateway/… model … uses unsupported transport ai-gateway-provider
```

So the turn was driven against a local OpenAI-compatible stub in an isolated
`HOME`/config, the same fixture shape I used in wave 2.

**CLI** — `run --model localqa/f3-model --format json "Reply with exactly F3TURN-OK"`:

```
{"detail":"session titled: F3TURN-OK","step":0,"type":"status_detail"}
{"sessionID":"ses_28410c3b7ba94be98785922050f21038","type":"turn_started"}
{"agent":"build","step":1,"type":"agent_resolved"}
{"modelID":"f3-model","providerID":"localqa","step":1,"type":"model_resolved"}
{"messageID":"msg_4786…0001","step":1,"type":"assistant_message_created"}
{"rebuiltForLateMcp":false,"step":1,"toolIDs":["invalid","bash","read","glob","grep","edit","write","webfetch","todowrite"],"type":"tool_snapshot_locked"}
{"messageCount":2,"step":1,"type":"provider_request_started"}
{"step":1,"text":"F3TURN-OK","type":"text"}
{"step":1,"stopReason":"Stop","type":"message_end"}
{"interrupted":false,"messageID":"msg_4786…0001","step":1,"type":"assistant_checkpointed"}
{"finishReason":"Stop","step":1,"type":"step_completed"}
{"messageID":"msg_4786…0001","steps":1,"type":"turn_completed"}
```

The stub confirms a real Chat Completions request reached it, so the provider
transport ran rather than being short-circuited:

```
STUB path=/v1/chat/completions bytes=2409
STUB keys=['messages', 'model', 'stream'] model=f3-model msgs=3
```

**HTTP** — same binary under `serve --port 42751`:

```
POST /api/session                        -> ses_d3596957496f4e67828699249b3250e3
POST /api/session/{id}/prompt            -> HTTP 200
  {"data":{"admittedSeq":0,"id":"msg_7dbfcfe833dd4e479c327a35d5232eb3",
           "sessionID":"ses_d3596957496f4e67828699249b3250e3",
           "prompt":{"text":"Reply with exactly F3TURN-OK","files":[],"agents":[]},
           "delivery":"steer","timeCreated":1786621021030}}
POST /api/session/{id}/wait              -> HTTP 204
GET  /api/session/{id}/message           -> HTTP 200
  messages: 2
    role=assistant texts=['F3TURN-OK']
    role=user      texts=['Reply with exactly F3TURN-OK']
```

**Result: nothing looked different from `647a2d64`.**
### Check 4 — gate

`CARGO_TARGET_DIR=/tmp/opencode/f3c cargo test --workspace --offline`, run once,
to a clean finish through the doc-test suites:

```
suites=217  passed=3473  failed=0  ignored=2
FAILED occurrences: 0
error[/error: occurrences: 0
```

**3473 / 0 — matches the expected total exactly.** No retry was needed; no
`EAGAIN`, no truncated total.

## Verdict: APPROVE at `98be20aa`

Nothing I verified at `647a2d64` regressed.

Todo 179's change is structural, and the structure holds where it touches the
surfaces I exercised. The generated arrival constants resolve byte-identically to
the literals they replaced (`"/provider"`, `"/api/model"`, `"/api/provider"`,
`"/api/provider/{providerID}"`), so the routes bound through
`GeneratedClientArrival::…path()` are the same routes as before — and I confirmed
that by request, not by reading the diff: all four serve real data, a near-miss
path still 404s, `PUT /auth/{providerID}` still lands on disk, both provider OAuth
operations still dispatch into the real plugin OAuth machinery, `/provider` still
returns the typed legacy document with 6 providers / 292 models, a real
`@opencode-ai/sdk` client calling `client.provider.list()` still sees
`release_date` as an ISO string with 0 of 292 malformed, and one ordinary turn
still completes end to end on both the CLI and the HTTP path.

I raised no Blocker. I found no regression attributable to todo 179, and per the
convergence protocol I did not go looking for anything else.

### Follow-ups — explicitly non-blocking, not conditions of this approval

1. Under `serve`, JS plugin load completes before the listener binds, so a plugin
   that awaits its own server during `init` cannot succeed; it burns the full 60s
   deadline (`javascript plugin host failure … hook=Some("init") … kind=TimedOut`)
   and the server then binds and serves normally. Pre-existing, untouched by this
   delta. Worth a documented note for plugin authors at most.
2. Every upstream provider credential in this environment is currently unusable
   (bedrock 404, openai `token_expired`, google `INVALID_ARGUMENT`, zhipuai rate
   limited). Host/credential state, not product state — but it means a reviewer
   cannot exercise a live-provider turn here without a local stub.

### What I ran and cleaned up

- Built and ran the real artifact: `/tmp/opencode/f3c/debug/opencode-rust`
  (`cargo build --offline`, `Finished dev profile … in 30.42s`).
- Servers on ports 42731, 42742, 42743, 42751 — all mine, all stopped.
- Scratch confined to `/tmp/opencode/` (`f3c`, `f3c-run`, `f3proj`, `f3turn`).
- The two scratch credentials the `PUT /auth` check wrote (`f3check`,
  `f3check2`) were removed from `auth.json`; no other entry was touched.
- No tmux session was started. No repository file other than this report was
  modified. Nothing was committed, branched, pushed, or merged.
