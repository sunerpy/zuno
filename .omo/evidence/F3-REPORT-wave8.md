# F3 — Manual QA Report, Wave 8

- **Audited HEAD:** `2e57e490c84224f44ff3ba8469cf9dd8dfa1b9e8`
- **Worktree:** `/config/workspace/ProdDir/AI/oc-wt/tF3` (branch `task-F3`)
- **Method:** build the real artifact (`cargo build --offline`), drive the resulting
  `target/debug/opencode-rust` binary as a user would (CLI, TUI via tmux, HTTP via curl).
  No source-reading substitutes for running the product.

## VERDICT: APPROVE WITH ONE BLOCKER TO TRIAGE (see the full verdict at the end)

## Planned scenarios

1. Plugin hooks reaching real lifecycle points (todo 149) — plugin tool execution,
   permission prompt, shell env injection, system-prompt / message transform.
2. Provider families reachable (todo 148) — non-`compatible` wire family selectable
   and dispatching in the production turn.
3. Seam #18's fix (todo 147) — plugin auth loader round-trip of real provider data
   (incl. `thinkingBudget`), and a would-be-truncated payload producing a clear
   error naming plugin + JSON Pointer path instead of silent `{$truncated:true}`.
4. Re-verify wave-6 priority four on the moved tree:
   - configured `kiro-auth` provider appears in `models`
   - HTTP answers visible via pre-opened session SSE, `/message`, `/history`
   - disconnecting the only session SSE observer rejects a pending permission
     immediately without running the tool
   - `/api/fs/*` blocks outward file **and** directory symlinks

Findings are appended below as each scenario completes.

---
## 0. Build the artifact — PASS

```
$ cargo build --offline
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 27.21s
$ ./target/debug/opencode-rust --version
1.18.13
$ ./target/debug/opencode-rust --version --long
opencode-rust 0.1.0 (Rust package 0.1.0; plugin compatibility 1.18.13)
```

The `split-version-identity` divergence behaves as declared. All QA below runs
`/config/workspace/ProdDir/AI/oc-wt/tF3/target/debug/opencode-rust` built at
`2e57e490`.

### QA rig

Isolation root `/tmp/opencode/f3w8`, every product run under `env -i` with a
temporary `HOME` and full XDG set. A Python mock OpenAI-compatible provider on
`127.0.0.1:43871` logs every request (path, headers, body) as JSON lines, so
"did the plugin's mutation reach the wire" is an observation and not an inference.
A JS QA plugin at `project/plugin/f3qa.js` appends one JSON line per hook
invocation to `hooks.log`, and makes four observable mutations
(`chat.params.temperature`, a `chat.headers` header, a system-prompt marker,
a `shell.env` variable) plus one plugin-provided tool `f3_probe`.

---

## 1. Plugin hooks reaching real lifecycle points (todo 149) — PASS

### 1a. A plain turn: which hooks actually fire

One `opencode-rust run --model test/test-model "hello F3 with plugin"` produced
this hook trace (verbatim `hooks.log`, deduplicated for length):

```json
{"hook":"$load","directory":"/tmp/opencode/f3w8/project","keys":["client","project","directory","worktree","serverUrl","experimental_workspace","$"]}
{"hook":"config","keys":["$schema","agent","command","formatter","lsp","permission","plugin","provider","username"]}
{"hook":"experimental.provider.small_model","input":["provider"],"output":["model"]}
{"hook":"chat.message","input":["agent","messageID","model","sessionID","variant"],"output":["message","parts"]}
{"hook":"event","type":"turn.started"}
{"hook":"event","type":"agent.resolved"}
{"hook":"event","type":"model.resolved"}
{"hook":"experimental.chat.system.transform","input":["model","sessionID"],"output":["system"],"before":1}
{"hook":"experimental.chat.messages.transform","input":[],"output":["messages"]}
{"hook":"tool.definition","input":["toolID"],"output":["description","id","parameters"]}   (x10, one per tool)
{"hook":"event","type":"tool.snapshot.locked"}
{"hook":"event","type":"provider.request.started"}
{"hook":"chat.params","input":["agent","message","model","provider","sessionID"],"output":["maxOutputTokens","options","temperature","topK","topP"],"temperature":0}
{"hook":"chat.headers","input":["agent","message","model","provider","sessionID"],"output":["headers"]}
{"hook":"experimental.text.complete","input":["messageID","partID","sessionID"],"output":["text"],"text":"F3_ANSWER_OK"}
{"hook":"event","type":"assistant.checkpointed"}
{"hook":"event","type":"step.completed"}
{"hook":"event","type":"turn.completed"}
{"hook":"dispose"}
```

The ordering matches the documented triggers: `config` before composition,
`small_model` at internal-agent resolution, `chat.message` before persistence,
the two `experimental.chat.*.transform` hooks before request preparation,
`tool.definition` once per advertised tool, `chat.params`/`chat.headers` after
model resolution, `experimental.text.complete` on the completed text part, and
`dispose` last at runtime shutdown. `event` carries the real turn event stream
(`turn.started`, `agent.resolved`, `model.resolved`, `assistant.message.created`,
`tool.snapshot.locked`, `provider.request.started`, `assistant.checkpointed`,
`step.completed`, `turn.completed`), i.e. it is the production stream and not a
synthetic one.

### 1b. The mutations reached the wire

Provider-side observation of the same run (my mock's own log, not the product's):

```
--- request 0: temperature=None keys=['messages', 'model', 'stream']
    x-f3 headers: {}
    system msgs: 1 | F3_SYSTEM_MARKER present: False
--- request 1: temperature=0.42 keys=['messages','model','stream','temperature','tools','top_k','top_p']
    x-f3 headers: {'x-f3-hook': 'chat-headers-fired'}
    system msgs: 1 | F3_SYSTEM_MARKER present: True
```

All three are real: `chat.params` moved `temperature` from `0` to `0.42`,
`chat.headers` added `x-f3-hook`, and `experimental.chat.system.transform`
appended `F3_SYSTEM_MARKER` to the system prompt actually sent. These are not
hook-fired-but-ignored; the provider received them.

OBSERVATION (not a defect): request 0 is the internal title generation, and it
carries neither the plugin's `temperature` nor its header. The internal agent has
its own hook (`experimental.provider.small_model`, which did fire). A plugin
author reading "chat.params — provider request preparation after model
resolution" could reasonably expect it on every provider request including the
title one. Worth one sentence in `plugin-authoring.md`; it does not change any
observable promise the docs make.

### 1c. `permission.ask` is authoritative, and `shell.env` reaches the child

A tool-calling turn (`bash` with `printf "SHELLENV=[%s]\n" "$F3_SHELL_ENV"`):

```
[bash] started
[bash] completed: printf "SHELLENV=[%s]\n" "$F3_SHELL_ENV"
F3_ANSWER_OK
```
```json
{"hook":"tool.execute.before","tool":"bash","output":["args"],"args":["command","intent"]}
{"hook":"permission.ask","input":["always","id","metadata","patterns","permission","sessionId","tool"],"output":["status"],"status":"ask"}
{"hook":"shell.env","input":["callID","cwd","sessionID"],"output":["env"]}
{"hook":"tool.execute.after","tool":"bash","output":["attachments","metadata","output","title"],"title":"printf \"SHELLENV=[%s]\\n\" \"$F3_SHELL_ENV\""}
```

The tool result the provider received on the follow-up request:

```
req2 TOOL RESULT: "SHELLENV=[injected]\n"
```

So `shell.env`'s `env.F3_SHELL_ENV = "injected"` reached a real `bash` child
process — that is the strongest possible evidence for that hook.

Re-run with the plugin flipping `permission.ask` output to `status:"deny"`, on a
command that would leave a sentinel file:

```
[bash] started
[bash] failed: bash error
F3_ANSWER_OK
$ ls -la /tmp/opencode/f3w8/MUST_NOT_EXIST
ls: cannot access '/tmp/opencode/f3w8/MUST_NOT_EXIST': No such file or directory
req2 TOOL RESULT: "Tool `bash` was denied by a plugin."
```
```json
{"hook":"tool.execute.before","tool":"bash","output":["args"],"args":["command","intent"]}
{"hook":"permission.ask","output":["status"],"status":"ask"}
```

The hook's deny is enforced: **the command never ran** (no sentinel), `shell.env`
and `tool.execute.after` correctly did **not** fire, and the model-visible error
names the plugin as the cause. `tool.execute.before` firing on an invalid-args
call in an earlier attempt also confirms its documented position "before
validation, permission, and execution".

Note on `permission.ask` input: the hook is handed `status:"ask"` even though the
project config sets `"permission": {"bash": "allow"}`. That reads as the hook
seeing the pre-resolution decision, which is the useful position for a hook that
is meant to be able to override; I could not tell from the outside whether
upstream presents the resolved value instead, so I am recording it rather than
calling it either way.

### 1d. A plugin-provided tool executes

Plugin exported `tool: { f3_probe: {...} }`. The tool is advertised to the model
in the real request as one of eleven:

```
req1 tools(11): ['invalid','bash','read','glob','grep','edit','write','webfetch','todowrite','f3_probe','memory']
   f3_probe schema: {"function":{"description":"F3 QA probe tool provided by a plugin","name":"f3_probe","parameters":{"additionalProperties":false,"properties":{"accept_large_output":{...},"intent":{...},"text":{}},"required":["intent"],"type":"object"}}}
```

and it executes end to end:

```
[f3_probe] started
[f3_probe] completed: F3 probe
F3_ANSWER_OK
```
```json
{"hook":"tool.execute.before","tool":"f3_probe","args":["intent","text"]}
{"hook":"tool.execute(f3_probe)","args":{"intent":"f3 plugin tool probe","text":"hello-from-f3"},"ctx":["sessionID","messageID","agent","directory","worktree","abort","metadata","ask"]}
{"hook":"tool.execute.after","tool":"f3_probe","title":"F3 probe"}
req2 TOOL RESULT: "F3_PLUGIN_TOOL_RAN:{\"intent\":\"f3 plugin tool probe\",\"text\":\"hello-from-f3\"}"
```

`"text": {}` in the advertised schema is the documented, deliberate gap
(`shim.mjs:describeTool` — zod schemas are not translated); the host still
injects its own `intent` / `accept_large_output` contract and enforces it, since
a call without `intent` came back as `Invalid arguments for tool `f3_probe`:
"intent" is a required property`. Nothing silent.

Verdict for scenario 1: **PASS.** 16 of 21 hooks observed firing from real
lifecycle points in ordinary use, with four independently confirmed on the wire
or in a child process, and one (`permission.ask`) proven authoritative over
whether a tool runs. `auth` and `provider` are data-shaped rather than callbacks
and are covered in scenario 3. Nothing regressed in normal operation: plain runs,
tool runs, and denials all behaved.

### 1e. Regression check — a broken JS runtime costs 30 s per run (OBSERVATION)

Before pointing `PATH` at the real `bun`, my `env -i` PATH contained the mise
shim directory, where `bun` is a shim that errors out:

```
DEBUG oc_plugin::js::host: javascript plugin stderr ... mise ERROR bun is not a valid shim. This likely means you uninstalled a tool ...
WARN oc_cli::cmd::plugin_runtime: JavaScript plugin did not fully load plugin=file:///...f3qa.js kind=FailedToLoad plugin `file:///...f3qa.js` did not connect back within 30000 ms surface=turn
F3_ANSWER_OK
```

Correct in substance — the turn still completed, the plugin was disabled with a
named diagnostic rather than taking the run down, which is the documented
contract. But the *user* sees a 30-second silent stall with no output before the
answer, on every run, and the explanation only appears at `--log-level DEBUG`.
A runtime that spawns and then never connects is indistinguishable from a hang.
Recording as an OBSERVATION on time-to-first-feedback, not a defect: the
behaviour is correct and the diagnostic exists.

Also worth stating plainly: a plugin dropped into `project/plugin/*.js` was
**not** picked up by directory scan in my rig (no hook log at all); it loaded only
after I declared it explicitly in `opencode.json`'s `plugin` array. My project
directory had no VCS marker, so this may well be project-root detection working
as intended rather than a scan defect. I did not isolate it further, so I am
reporting it as unverified rather than as a finding.

---
### 1f. DEFECT F3-W8-D1 — documented plugin auto-discovery does not exist in production

While isolating 1e I found a real, user-visible gap. `docs/plugin-authoring.md:27-33`
states:

> Beyond the config array, both `plugin/` and `plugins/` directories are scanned
> for `*.ts` and `*.js` in the global and project trees, and provenance is retained
> (`oc_plugin::PluginOrigin`) so a diagnostic can name the file that contributed a
> plugin.

I placed the same working plugin in **four** candidate locations at once —
`<project>/plugin/`, `<project>/plugins/`, `<project>/.opencode/plugin/` (upstream's
own convention), and `$XDG_CONFIG_HOME/opencode/plugin/` — in a git-initialised
project, with **no** `plugin` key in `opencode.json`:

```
$ ls  proj2/plugin/f3qa.js  proj2/plugins/f3qa.js  proj2/.opencode/plugin/f3qa.js  config/opencode/plugin/f3qa.js
(all four present)
$ opencode-rust run --model test/test-model "scan test 4 locations"
F3_ANSWER_OK
$ cat hooks2.log
(file does not exist — not one hook fired)
```

Not a load failure — a discovery no-op. At `--log-level DEBUG` the run mentions
plugins **zero** times:

```
$ opencode-rust --print-logs --log-level DEBUG run ... 2>&1 | grep -ci "plugin"
0
```

The same plugin, declared explicitly as `"plugin": ["file:///.../f3qa.js"]`, loads
and fires every hook (scenario 1 above), so the plugin itself is fine.

Cause, from the surface inward: production assembles specs in
`plugin_runtime.rs:643 configured_plugins()`, which reads `config.plugin` and
nothing else. `oc_plugin::discover_plugins` — the function that scans
`["plugin", "plugins"]` (`discovery.rs:107`) and stamps
`PluginOrigin::AutoDiscovered` — has **no production caller**; `grep` for it finds
only `crates/oc-plugin/tests/discovery.rs`.

Why this matters and why I am filing it: this is exactly the shape todo 149 was
opened to eliminate — an advertised capability whose only exerciser is its own
unit test. The docs assert it unconditionally, `docs/divergences.md` does not
declare it, and an upstream user who migrates a working `.opencode/plugin/`
tree gets total silence: no plugin, no warning, no DEBUG line. Under the project's
own rule ("an **undeclared** difference is a defect") this is a **DEFECT**, and it
is a documentation-vs-product contradiction rather than a missing nicety.

Severity: I would not block a release on it if the owner prefers to declare it,
since the workaround (declare the plugin in `opencode.json`) is one line and is
what my own scenarios used. But it must become either wiring or a declared
divergence with the sentence above removed from `plugin-authoring.md` — leaving
the doc as-is is the one option that is wrong.

---
## 2. Provider families reachable in the production turn (todo 148) — PASS, all eight

Rig: one config declaring **eight** providers, one per declared wire family,
each with a distinct `npm` id and every endpoint pointed at a single multi-family
mock on `127.0.0.1:43872`. The mock routes by request path and answers in *that
family's own streaming format* — so a decoded answer is proof the product both
selected the right family and spoke its wire protocol. Runs use `OPENCODE_PURE=1`
(no plugins) so nothing but the turn is in play.

Selection is visible first through the ordinary `models` surface:

```
$ opencode-rust models | grep fam
famanthropic/claude-f3
fambedrock/anthropic.claude-f3
famcompatible/compat-f3
famgoogle/gemini-f3
fammantle/mantle.claude-f3
famopenai/gpt-f3
famvertex/gemini-vertex-f3
famvertexanthropic/claude-vertex-f3
```

Then one real `run` per family. Observed output, verbatim:

| family (`npm`) | model | product output | exit |
|---|---|---|---|
| `@ai-sdk/openai-compatible` | `famcompatible/compat-f3` | `F3_COMPATIBLE_OK` | 0 |
| `@ai-sdk/anthropic` | `famanthropic/claude-f3` | `F3_ANTHROPIC_OK` | 0 |
| `@ai-sdk/openai` | `famopenai/gpt-f3` | `F3_OPENAI_OK` | 0 |
| `@ai-sdk/google` | `famgoogle/gemini-f3` | `F3_GOOGLE_OK` | 0 |
| `@ai-sdk/google-vertex` | `famvertex/gemini-vertex-f3` | `F3_GOOGLE_OK` | 0 |
| `@ai-sdk/google-vertex/anthropic` | `famvertexanthropic/claude-vertex-f3` | `F3_VERTEX_ANTHROPIC_OK` | 0 |
| `@ai-sdk/amazon-bedrock` | `fambedrock/anthropic.claude-f3` | `F3_BEDROCK_OK` | 0 |
| `@ai-sdk/amazon-bedrock/mantle` | `fammantle/mantle.claude-f3` | `F3_BEDROCK_OK` | 0 |

Each family reached the wire at its own endpoint with its own auth convention —
observed server-side, not inferred:

```
compatible        /chat/completions                                  authorization: Bearer f3-key
anthropic         /v1/messages                                       x-api-key: f3-key, anthropic-version: 2023-06-01
openai-responses  /v1/responses                                      authorization: Bearer f3-key
google            /models/gemini-f3:streamGenerateContent?alt=sse     x-goog-api-key: f3-key
google-vertex     /models/gemini-vertex-f3:streamGenerateContent?alt=sse  authorization: Bearer f3-key
vertex/anthropic  /claude-vertex-f3:streamRawPredict                  authorization: Bearer f3-key
bedrock           /model/anthropic.claude-f3/converse-stream          authorization: AWS4-HMAC-SHA256 Credential=.../us-east-1/bedrock/aws4_request, SignedHeaders=accept;content-type;host;x-amz-content-sha256;x-amz-date, Signature=414ad0e1...
bedrock/mantle    /model/mantle.claude-f3/converse-stream             AWS4-HMAC-SHA256 (distinct signature per request)
```

Three details worth stating because they are the ones that could have been faked
and were not:

- **Google vs Vertex-Gemini share a decoder but not a credential path.** Both
  post `:streamGenerateContent?alt=sse`, but plain Google sent `x-goog-api-key`
  while Vertex sent `authorization: Bearer`. That is the real per-family
  difference, and it is why both are separate registrations rather than an alias.
- **Vertex-Anthropic is not Vertex-Gemini.** It went to `:streamRawPredict` and
  decoded an Anthropic Messages SSE stream — the correct Vertex-Anthropic
  surface, and the one thing that proves this family is not silently collapsed
  into the Gemini path.
- **Bedrock really signs.** The request carried a full SigV4 `Authorization` with
  a correct credential scope (`.../us-east-1/bedrock/aws4_request`) and a
  different signature per request, and the answer came back only after I fed it
  genuine `application/vnd.amazon.eventstream` binary framing (prelude + CRC32 +
  typed headers + `contentBlockDelta` payloads). A stub decoder would not have
  produced `F3_BEDROCK_OK` from those bytes. `mantle` used the same transport at a
  distinct model id, which matches its being a sibling registration.

Before I taught the mock the last three protocols, those three failed **honestly**
rather than silently: `unrecoverable provider failure (status=Some(404)): provider
`google-vertex/anthropic` returned HTTP 404` and `Bedrock service error
status=404`. The error named the family. No family fell back to the compatible
transport to paper over a mismatch — which is the failure mode this todo existed
to remove.

Verdict: **PASS.** All eight declared wire-families are selectable through the
ordinary `models` surface and dispatch end to end in the production turn with
family-correct paths, auth, and response decoding. Nothing here needed a
credential I could not arrange locally, so there is no "could not verify" carve-out
for this scenario. What I did **not** verify is behaviour against the real cloud
endpoints (no live credentials, and I would not use them if I had them).

---
## 3. Seam #18's fix (todo 147) — PASS

This is the seam I suspected in wave 7. What I could see then was that
`shim.mjs` truncates at `MAX_DEPTH = 8` and `bridge.rs` writes the result straight
back over the real provider; what I could not see was whether real data crossed
the cliff. The plan owner measured it (real google sits at depth 7 of 8) and fixed
the write-back rather than the bound. Here is the user-visible behaviour now.

Rig: a provider `f3auth` with a `thinkingConfig: {thinkingBudget: 4096,
includeThoughts: true}` on its model, plus a `nest` chain I can lengthen one level
at a time to walk the payload across the depth bound. A JS plugin registers an
`auth` hook for that provider whose `loader` reports what it was handed and makes
one shallow, legitimate mutation (`provider.options.apiKey = "key-from-loader"`) —
exactly what a real auth plugin does.

### 3a. Real provider data round-trips intact, and the write-back still applies

```
$ opencode-rust run --model f3auth/deep-model "SEAM18 g-shallow"
F3_GOOGLE_OK
EXIT=0
```

What the loader received (its own log):

```json
{"event":"loader.called","providerKeys":["availability","env","id","models","name","options"],
 "deepOptions":{"thinkingConfig":{"includeThoughts":true,"thinkingBudget":4096}},
 "raw":"{\"thinkingConfig\":{\"includeThoughts\":true,\"thinkingBudget\":4096}}"}
{"event":"loader.deep.after.readback","deepAgain":{"includeThoughts":true,"thinkingBudget":4096}}
```

Byte-for-byte the configured value, with no `$truncated` marker anywhere — and
still intact on a second read after the mutation. That the write-back actually
took effect is visible on the wire, where the outbound request now authenticates
with the loader's key instead of the config's:

```
google /models/deep-model:streamGenerateContent?alt=sse | x-goog-api-key: key-from-loader
```

So the fix did not achieve safety by neutering the write-back. A legitimate
mutation is still authoritative; deep data next to it still survives.

The same holds with three extra nesting levels (`nest.l1.l2.l3`), which the loader
saw complete:

```json
"deepOptions":{"nest":{"l1":{"l2":{"l3":{"budget":4096}}}},
               "thinkingConfig":{"includeThoughts":true,"thinkingBudget":4096}}
```

### 3b. A would-be-truncated payload is refused, and the error names plugin and path

One more level (`nest.l1.l2.l3.l4`) crosses the bound. Observed output:

```
plugin auth loader `f3auth` failed: plugin `file:///tmp/opencode/f3w8/seam18/plugin/f3auth.js` truncated auth-loader provider data at `/models/deep-model/options/nest/l1/l2/l3/l4`; refusing to overwrite the provider
EXIT=1
```

This is what I wanted to see and did not get in wave 7. It names the **provider**
(`f3auth`), the **plugin** (its full resolved specifier), the **exact JSON Pointer**
of the first truncated node, and states the decision taken ("refusing to overwrite
the provider"). Nothing is silently replaced: the previous behaviour would have
written `{$truncated:true}` over the real `nest` subtree and continued, and a user
would have seen a provider misbehave with no explanation.

The refusal is a **hard stop** on that provider (`EXIT=1`), not a
continue-with-a-warning. The plan permitted either refusal or preservation, so
this is in scope, and I think refusal is the right call here — the alternative
silently discards a mutation the plugin believes it made. Worth noting the
consequence plainly: a plugin that nests deeply anywhere under the provider now
blocks the provider outright where it previously "worked" with corrupted data.
Since the trigger condition is exactly "data would have been corrupted", trading
silent corruption for a named stop is the correct direction.

Deeper payloads behave identically, and the error stays pinned to the **first**
offending path rather than degrading:

```
depth 100 → truncated auth-loader provider data at `/models/deep-model/options/nest/l1/l2/l3/l4`
depth 120 → truncated auth-loader provider data at `/models/deep-model/options/nest/l1/l2/l3/l4`
```

### 3c. The bounded-memory property the depth limit exists for still holds

The reason `MAX_DEPTH` exists is that an unbounded walk over a plugin's object
graph makes a bounded host unbounded. That property is intact: pathological input
is rejected cheaply, not absorbed. At 200 levels the config is refused before any
plugin runs, in 10 ms and 23 MB:

```
$ /usr/bin/time -f "wall=%es maxrss=%MkB" ... run --model f3auth/deep-model
config file /tmp/opencode/f3w8/seam18/opencode.json is not valid JSON
EXIT=1
wall=0.01s maxrss=23148kB
```

No hang, no growth, no stack overflow at any depth I tried (100, 120, 126, 130, 200).

Verdict for scenario 3: **PASS.** Seam #18 is closed at the surface I originally
found it: real data round-trips intact, the write-back still works, and the
formerly silent corruption is now a specific error naming the plugin and the JSON
Pointer path.

### 3d. OBSERVATION F3-W8-O2 — "not valid JSON" for JSON that is valid

Found while probing 3c. Config nesting deeper than ~125 levels is rejected as:

```
config file /tmp/opencode/f3w8/seam18/opencode.json is not valid JSON
```

The file **is** valid JSON — `python3 -c "json.load(open(...))"` parses it fine.
What was exceeded is a parser recursion limit (the boundary sits between 120 and
126, consistent with serde_json's default 128). Rejecting it is right; the wording
tells the user their file is malformed when it is not, and gives them nothing to
act on. It also swallows the real reason at any log level I tried.

Two smaller notes on the same message:

- It appears on `models` as well, but there `EXIT=0` while the error is printed
  and no models are listed. A tool scripting `opencode-rust models` sees success
  with empty output. On `run` the same condition correctly exits 1.
- Depth-limit rejection is not in `docs/rejected-inputs.md` under any of
  `depth`/`nest`/`recursion`.

Low severity — pathological input, and refusing is correct. Filed because the
message misdescribes the cause and because the exit code disagrees between two
commands about the same fatal condition.

---
## 4. Re-verification of wave-6 priority four on the moved tree — PASS (all four)

### 4a. The configured `kiro-auth` provider appears in `models` — PASS

Read-only probe against the user's real configuration (the one deliberate
exception to XDG isolation; diagnostics redirected, nothing written):

```
$ env -i PATH=/usr/bin:/bin HOME=/config XDG_{CACHE,DATA,CONFIG}_HOME=... \
    ./target/debug/opencode-rust models 2>/dev/null | cut -d/ -f1 | sort -u
amazon-bedrock
awsopenai
google
kiro-auth
myopenai
nwcdai
openai
zhipuai
zhipuai-coding-plan
PIPELINE_EXIT=0
```

`kiro-auth` is present with 59 models listed, e.g.:

```
kiro-auth/auto
kiro-auth/claude-haiku-4-5
kiro-auth/claude-opus-4-5
kiro-auth/claude-opus-4-6
kiro-auth/claude-opus-4-7
```

Nine provider prefixes, same as wave 6. No regression.

### 4b. HTTP answers visible via pre-opened SSE, `/message`, and `/history` — PASS

Real entry point (`opencode-rust serve`, not standalone `oc-server`) on
`127.0.0.1:43881` with Basic auth. Auth is enforced: `/api/app` returns `401`
unauthenticated. SSE opened **before** the prompt, bounded by `--max-time`.

```
prompt=200
wait=204
```

The pre-opened stream carried the whole turn, 2826 bytes, ending cleanly:

```
data: {... "type":"status.detail" ...}   session titled: F3_ANSWER_OK
data: {... "type":"turn.started" ...}
data: {... "type":"agent.resolved" ...}
...
data: {"data":{"assistantMessageID":"msg_...","steps":1},...,"type":"turn.completed"}
```
```
event types on the live stream:
 agent.resolved 1, assistant.checkpointed 1, assistant.message.created 1,
 message.end 1, model.resolved 1, provider 3, provider.request.started 1,
 status.detail 1, step.completed 1, text.delta 1, tool.snapshot.locked 1,
 turn.completed 1, turn.started 1
F3_ANSWER_OK occurrences in the SSE capture: 2
```

Both readback surfaces agree with it:

```
$ GET /api/session/$ID/message
  assistant -> ["F3_ANSWER_OK"]
  user -> ["HTTP_SSE_W8"]
$ GET /api/session/$ID/history
  events: 12
  F3_ANSWER_OK in history: True
```

Also checked with **no** `Accept: text/event-stream` header: same 12 events, same
bytes. A client that omits the header is not silently starved.

### 4c. Disconnecting the only session SSE observer rejects a pending permission immediately, without running the tool — PASS

Config `"permission": {"bash": "ask"}`. Sole observer opened, then a tool call
that would leave a sentinel file:

```
PERM_SESSION=ses_5b2a374df38a4d5dbea32472422411dc
prompt=200
PERMISSION_EVENT_SEEN after 200ms
```

The permission request is on the stream, fully described:

```json
{"data":{"action":"bash","id":"per_01d489012ec24c49b36b78d1f6379a15",
 "metadata":{"arguments":{"command":"touch /tmp/opencode/f3w8/SRV_MUST_NOT_EXIST","intent":"f3 probe"}},
 "resources":["touch /tmp/opencode/f3w8/SRV_MUST_NOT_EXIST"],
 "save":["touch /tmp/opencode/f3w8/SRV_MUST_NOT_EXIST"],
 "sessionID":"ses_5b2a374df38a4d5dbea32472422411dc"},"durable":{"seq":8},...}
```

Then I killed the only observer:

```
--- killing the only observer now
wait=204
ELAPSED_AFTER_KILL=0.026s
--- sentinel:
ls: cannot access '/tmp/opencode/f3w8/SRV_MUST_NOT_EXIST': No such file or directory
```

The tool result recorded on the session names the cause:

```json
"state": {"error":"tool bash was denied by the permission layer",
          "input":{"command":"touch /tmp/opencode/f3w8/SRV_MUST_NOT_EXIST","intent":"f3 probe"}}
```

**Control run**, identical except the observer stays connected — this is what makes
the above meaningful rather than "no approver, so always denied":

```
CTRL_SESSION=ses_ae4a1837e1114c82b6487c8d7bc2d500
prompt=200
PERMISSION_EVENT_SEEN
--- observer STAYS connected; waiting up to 20s for the turn
wait=000
WAIT_ELAPSED=20.01s
--- sentinel:
ls: cannot access '/tmp/opencode/f3w8/SRV_CTRL_MUST_NOT_EXIST': No such file or directory
```

With an observer present the permission stays **pending** for the full 20 s and the
tool still does not run. So the rejection is genuinely caused by losing the last
observer, it lands in 26 ms rather than after a timeout, and in neither case does
the command execute. Property confirmed on this tree.

### 4d. `/api/fs/*` blocks outward file **and** directory symlinks — PASS

Project contains `inside.txt`, a symlink `link-to-file -> /tmp/.../outside/secret.txt`,
and a symlink `link-to-dir -> /tmp/.../outside/secretdir` holding `inner.txt`.

```
/api/fs/read/inside.txt                    -> 200 | INSIDE_OK_W8
/api/fs/read/link-to-file                  -> 403 | {"error":{"code":"path_escaped_root","message":"the requested path leaves the session directory"}}
/api/fs/read/link-to-dir/inner.txt         -> 403 | {"error":{"code":"path_escaped_root","message":"the requested path leaves the session directory"}}
/api/fs/read/../outside/secret.txt         -> 404
/api/fs/list?path=link-to-dir              -> 403 | path_escaped_root
/api/fs/list?path=/tmp/.../outside         -> 403 | path_escaped_root
```

Both symlink kinds are refused with the same specific, honest error code, and
**neither** secret string appears in any response body. The control read inside the
root still works, so this is a boundary and not a blanket denial.

Two extra properties I checked because they are how a leak would actually happen:

- `/api/fs/list?path=.` does not even enumerate the links:
  `[{"path":"inside.txt","type":"file"},{"path":"opencode.json","type":"file"}]`
  — so an outward symlink is invisible, not merely unreadable.
- `/api/fs/find` does not traverse them either: `query=secret` and
  `query=link-to-file` both return `data: []`, while `query=inside` returns
  `[{"path":"inside.txt","type":"file"}]`, confirming find works and is simply not
  following the links. (`find` matches path names, not file contents — a content
  query like `INSIDE_OK_W8` correctly returns empty.)

---
## 5. The gate — PASS

```
$ cargo test --workspace --offline
aggregated across all test binaries: passed=3390 failed=0 ignored=2
```

Exactly the expected 3390 passing / 0 failed. No `EAGAIN` transient, so one run only.

---

## 6. Exploration beyond the mandatory set

Started only after sections 1-5 were written to this file.

### 6a. CLI failure modes and empty state — PASS

Exit codes and messages, observed (each run separately so `$?` is the product's,
not a pipeline's):

```
run bad model     exit=1 | model `nope/nope` is not available: no `provider` block in your configuration defines it, OPENCODE_DISABLE_MODELS_FETCH is set so no fetch from `https://models.opencode.ai` was attempted, and no cached catalog exists at `.../models.json`. Define the provider and model under `provider` in your config, or unset OPENCODE_DISABLE_MODELS_FETCH ... or set OPENCODE_MODELS_PATH ...
session list      exit=0 | (empty)
tui no tty        exit=1 | the interactive TUI requires a terminal; use `run <message>` for a non-interactive turn
session --bogus   exit=2 | error: unexpected argument '--bogus' found  Usage: ...  try '--help'
run no message    exit=1 | a message is required
export no session exit=1 | session selection is interactive upstream; pass the session id, which `session list` prints
import missing    exit=1 | File not found: /tmp/opencode/f3w8/does-not-exist.json
completion bash   exit=1 | `completion` is not available: upstream's completion script is a yargs shell function that asks the binary back for candidates over `--get-yargs-completions` ...
```

This is the best part of the CLI. The bad-model message names all three ways to
fix it and quotes the exact cache path it looked in. Empty `session list` prints
nothing and exits 0, which is correct for an empty collection. `2` for a usage
error vs `1` for a runtime failure is the conventional split. Nothing bluffs.

### 6b. DEFECT F3-W8-D2 — the TUI permission dialog cannot be answered, and does not show what it is asking about

This is the most serious thing I found this wave. Two parts, same dialog.

**Part 1 — the dialog does not say what it is approving.** A `bash` tool call under
`"permission": {"bash": "ask"}`, in the TUI:

```
> You
  TOOLCALL:touch /tmp/opencode/f3w8/TUI_SENT4
* Assistant
  … $ bash
… working
│ Permission required
│△ Permission required
│  # Shell command
│
│ Allow once   Allow always   Reject
│⇆ select  enter confirm  ctrl+f fullscreen
```

The command being approved — `touch /tmp/opencode/f3w8/TUI_SENT4` — is nowhere on
screen. The line under the title is empty (verified with `cat -A`: the row is the
border glyph and nothing else). `ctrl+f` (fullscreen) changes nothing.

Not specific to bash. A `webfetch` call renders:

```
│  % WebFetch
```

with no URL and an empty detail line. `crates/oc-tui/src/views/permission.rs:174-184`
builds `title: "Shell command"` with `detail: vec![format!("$ {command}")]`, and
`:202-208` builds `title: format!("WebFetch {url}")`. Observed output is the bare
prefix with the interpolated value missing in both, so the view is being handed
empty tool input. The data exists — the **same** permission request over HTTP
carries it in full (section 4c: `metadata.arguments.command`, `resources`) — so
this is the TUI view, not the permission layer.

Security-relevant: the user is invited to press **Allow always** on a shell
command they were never shown, which persists a rule for an unseen command.

**Part 2 — no key answers the dialog.** Once it opens, the TUI stops responding to
input entirely. Every key I tried, on two independently created fresh sessions
(no resize involved), left both the dialog and the highlighted button unchanged:

```
initial sel=[Allow once] dialog=2
Right  sel=[Allow once] dialog=2 sentinel=absent
Tab    sel=[Allow once] dialog=2 sentinel=absent
Enter  sel=[Allow once] dialog=2 sentinel=absent
```
also ignored: `Left`, `BTab`, `Down`, `j`, `Space`, `C-m`, `a`, `r`, `y`, `n`,
`Escape`, `C-f`, `C-c`, `C-d`, printable text (`ZZ` never reached the composer).

The footer advertises `⇆ select  enter confirm`, so `Tab`/`Left`/`Right` and
`Enter` are the documented keys. Selection was read from the actual highlight
attribute (`tmux capture-pane -e`, background `48;2;250;178;131`), not guessed.

It is a permanent stall, not slowness — after **60 s** the dialog is unchanged and
the tool has still not run. The process is alive and blocked in `futex_do_wait`
with stdin held on its pty (`/proc/<pid>/fd/0 -> /dev/pts/7`, `State: S`), so it
is waiting, not crashed. `C-c` does not quit; the session needs `SIGKILL`.

Input plumbing is fine, which is what makes this a dialog bug: in the same
sessions, **before** the dialog, typing and `Enter` submitted prompts normally
(`TUI_HELLO_W8` → `F3_ANSWER_OK`), and with no dialog open `C-c` is handled
correctly — it cleared the composer and returned to `idle` without exiting.

Impact: with any `ask` permission — which includes the default for `bash` in the
config I used — the TUI, the product's default command, deadlocks on the first
tool call and cannot be recovered from the keyboard. The same permission decision
over HTTP works correctly (section 4c).

Scope of my claim: reproduced twice, `TERM=xterm-256color` inside tmux, panes
170x44 and 160x42. I could not test a non-tmux physical tty from this environment,
so I cannot rule out that the dialog's key handling depends on a terminal
capability tmux does not advertise (e.g. the kitty keyboard protocol). If that is
the cause it is still a defect — tmux with `xterm-256color` is an ordinary user
setup — but the fix would be different, so I am flagging the uncertainty rather
than asserting a cause.

### 6c. OBSERVATION F3-W8-O3 — the TUI does not repaint on terminal resize

Resizing the terminal blanks the TUI completely until the next keystroke.

```
nonblank_before=6      (conversation and composer visible)
$ tmux resize-window -x 200 -y 60
nonblank_after=0       (entire pane blank; dims=200x60; proc=alive)
$ (type one character)
typed_visible=6        (fully repainted, conversation intact)
```

No data loss and it self-heals on any input, so this is cosmetic — but a user who
resizes their window sees their session vanish, and the natural reaction to a
blank screen is to kill the process. Filed as an OBSERVATION.

---

## VERDICT

**APPROVE WITH ONE BLOCKER TO TRIAGE.**

Every mandatory scenario I was given passed on the audited artifact:

| # | scenario | verdict |
|---|---|---|
| 0 | build the real binary, split-version identity | PASS |
| 1 | plugin hooks at real lifecycle points (todo 149) | PASS |
| 2 | all eight provider wire-families selectable and dispatching (todo 148) | PASS |
| 3 | seam #18 — round-trip intact, truncation refused with plugin + path (todo 147) | PASS |
| 4a | `kiro-auth` present in real-config `models` | PASS |
| 4b | HTTP answers via pre-opened SSE, `/message`, `/history` | PASS |
| 4c | last SSE observer disconnect rejects permission in 26 ms, tool never runs | PASS |
| 4d | `/api/fs/*` blocks outward file **and** directory symlinks | PASS |
| 5 | `cargo test --workspace --offline` → 3390 passed / 0 failed | PASS |

The three implementation todos this wave targeted are genuinely closed at the
surface, and I confirmed all three by running the product rather than reading it.
Scenario 3 in particular closes the seam I opened in wave 7, with the error naming
the plugin and the exact JSON Pointer.

Found, outside the mandatory set:

- **F3-W8-D2 (BLOCKER, needs a decision)** — the TUI permission dialog shows
  neither the command nor the URL it is asking about, and no key answers it, so
  any `ask` permission deadlocks the default interactive surface. Same decision
  works correctly over HTTP. I cannot rule out a tmux/terminal-capability factor,
  which is why I say "triage" rather than "reject": if it reproduces on a physical
  tty it should block, and if it is tmux-specific it is still worth fixing.
- **F3-W8-D1 (DEFECT)** — `docs/plugin-authoring.md` promises `plugin/` and
  `plugins/` directory auto-discovery; production reads only `config.plugin`, and
  `discover_plugins` has no production caller. Undeclared, therefore a defect.
  Must become wiring or a declared divergence; leaving the doc as-is is the one
  wrong option.
- **F3-W8-O1** — internal title generation bypasses `chat.params`/`chat.headers`.
- **F3-W8-O2** — deeply nested config is reported as "not valid JSON" when it is
  valid (a parser recursion limit), and `models` prints that error while exiting 0.
- **F3-W8-O3** — the TUI does not repaint on resize until the next keystroke.

### What I could not verify

- Provider families against **live** cloud endpoints — no credentials, and I would
  not use real ones for this. All eight were proven against a local mock speaking
  each family's real wire protocol, including SigV4 and Amazon event-stream framing.
- Whether F3-W8-D2 reproduces on a physical tty outside tmux — no such terminal
  available here.
- Whether the plugin-directory scan (F3-W8-D1) is reachable through some
  configuration I did not find. I tried four standard locations in a
  git-initialised project with no `plugin` key and got no discovery and no
  diagnostic at DEBUG.
- 5 of 21 hooks (`command.execute.before`, `experimental.session.compacting`,
  `experimental.compaction.autocontinue`, `dispose` beyond shutdown,
  `tool` beyond registration) were not exercised — they need command expansion or
  a context overflow I did not construct. I observed 16 firing from real triggers.
- `permission.ask` receives `status:"ask"` even where config says `allow`; I could
  not determine from outside whether upstream presents the resolved value.

### Cleanup

All tmux sessions killed, all mock providers and the `serve` instance stopped,
scratch confined to `/tmp/opencode/f3w8/`. No product source, test, plan, doc, or
other evidence file was modified; no commit, branch, push, or remote touched.
