# F3 — Real Manual QA, Wave 10

- **Audited HEAD**: `2e742986206d5a8707508b4008d2b56d651f0864`
- **Worktree**: `/config/workspace/ProdDir/AI/oc-wt/tF3` (branch `task-F3`)
- **Method**: build the real artifact, drive it as a user. No source-reading substituted for running.
- **VERDICT**: **PASS WITH DEFECTS** — no blocker. 2 defects (both doc-vs-production), 3 defect-adjacent, 7 observations, 1 withdrawn.

## Planned scenarios

| # | Scenario | Origin | Status |
|---|---|---|---|
| S0 | Build artifact + run full test gate | mandatory | PASS (3421/0) |
| S1 | Stalled provider (no FIN) now bounded by idle timeout; partial preserved | F3-W9-D2 / todo 154 | FIXED |
| S2 | Slow-but-progressing stream survives (idle bound, not total deadline) | todo 154 | PASS |
| S3 | Permission dialog subject + diff + fullscreen, all kinds | F3-W9-D1 / todo 157 | FIXED |
| S4 | Permission footer advertised keys actually work | F3-W9-O1 / todo 157 | FIXED |
| S5 | Deeply nested config "not valid JSON"; `models` exit code | F3-W9-O2(O2) | HALF FIXED |
| S6 | docs/plugin-authoring.md "a path" claim | F3-W9-O3 | DOC CORRECT |
| S7 | Plugin write-back rejection JSON Pointer | F3-W9-O4 | NOT VERIFIED |
| S8 | Un-prefixed POST/GET /session → not_implemented | F3-W9-O2 | STILL OPEN |
| S9 | plugin/ + plugins/ auto-discovery vs config.plugin only | F3-W8-D1 | STILL OPEN |
| S10 | Migration journal future id refused before serving queries | todo 155 | VERIFIED |
| S11 | Azure/Copilot Responses surface sends `input`, decodes `response.*` | todo 156 | VERIFIED (mocks) |
| S12 | Plugin `parts` mutation reaches model request; chat.message identity fields | todo 158/159 | VERIFIED |
| S13 | TUI loads Tui-kind plugins; tui() executes; declared boundary vs reality | todo 160 | VERIFIED |

## Results

### S0 — build + artifact identity

`cargo build --offline` → `Finished \`dev\` profile ... in 26.24s`. Binary at
`target/debug/opencode-rust` (157 MB). Every scenario below drives that binary.

```
$ target/debug/opencode-rust --version
1.18.13
```

**F3-W10-O1 — WITHDRAWN.** I first flagged `--version` reporting `1.18.13` against a
1.18.15 port target as an undeclared mismatch. It **is** declared:
`docs/divergences.md` → `split-version-identity` states the short version reports the
pinned `1.18.13` deliberately, because npm plugins gate on it, with the real identity
behind `--version --long`. Verified:

```
$ opencode-rust --version
1.18.13
$ opencode-rust --version --long
opencode-rust 0.1.0 (Rust package 0.1.0; plugin compatibility 1.18.13)
```

Correctly declared, correctly implemented. Not a defect.

---

### S1 — stalled provider, no FIN (F3-W9-D2 / todo 154) — **FIXED**

Fixture: a raw-socket server that completes the request, sends `200 OK` +
`text/event-stream`, emits two SSE deltas, then **holds the socket open forever** —
no further bytes, no FIN, no RST. This is byte-for-byte the wave-9 hang fixture.
Idle bound shortened to 5s via `OPENCODE_STREAM_IDLE_TIMEOUT_SECS=5` so the run is
observable in seconds rather than minutes.

```
$ opencode-rust run --model test/test-model "hello"
exit=1 elapsed=10s
--- stdout ---
PARTIAL_ONE PARTIAL_TWO
--- stderr ---
transient provider failure (status=None): provider `test` response stream idle timeout after 5s; raise OPENCODE_STREAM_IDLE_TIMEOUT_SECS for slower providers
```

All three wave-9 complaints are addressed, observed not inferred:

- **It terminates.** 10s, not the 200s-and-counting of wave 9. (10s = two attempts
  × 5s; the server log shows exactly two accepted connections, so the retry is real
  and each attempt is independently bounded.)
- **Partial content is preserved.** `PARTIAL_ONE PARTIAL_TWO` reaches stdout.
- **The error names the mechanism and the remedy** — "response stream idle timeout
  after 5s", plus the env var to raise it. A user who hits this can act on it.

Exit status is 1, so scripts see the failure.

### S2 — slow-but-progressing stream survives (todo 154) — **PASS**

Fixture: same server, `slow` mode — one SSE delta every **1.5s** for 8 ticks, i.e.
a 12s stream whose every inter-chunk gap (1.5s) is under the 5s idle bound.

```
exit=0 elapsed=24s   (idle bound was 5s)
--- stdout ---
SLOW0 SLOW1 SLOW2 SLOW3 SLOW4 SLOW5 SLOW6 SLOW7
--- stderr ---
(empty)
```

This is the discriminating result: total wall time (24s) is ~5x the idle bound and
the run still succeeded with every chunk delivered. The bound is genuinely **idle**,
not a total deadline — which was the design requirement, and is what stops the fix
from becoming a new class of failure for legitimately slow providers.

(Fixture note: my `slow` mode's terminal `finish_reason` chunk is malformed, so the
agent legitimately took a second turn — hence 24s for a 12s stream. That is my
fixture's fault, not the product's, and it does not weaken the claim above: both
turns' streams progressed at 1.5s gaps and neither was cut off.)

### S1b — the **production default** is bounded (the owner's follow-up concern) — **PASS**

The owner noted the first fix left the shipped default unguarded and pinned it with
a test. I verified the shipped default by running the same stall fixture with
`OPENCODE_STREAM_IDLE_TIMEOUT_SECS` **unset**:

```
exit=1 elapsed=241s
--- stdout ---
PARTIAL_ONE PARTIAL_TWO
--- stderr ---
transient provider failure (status=None): provider `test` response stream idle timeout after 120s; raise OPENCODE_STREAM_IDLE_TIMEOUT_SECS for slower providers
```

241s = two attempts × ~120s. The out-of-the-box default really is 120s per attempt,
so the wave-9 unbounded hang cannot recur for a user who sets nothing. Partial text
still preserved, exit still 1.

One thing a user should know and currently cannot learn from the docs: **the
user-visible wall time is ~2x the configured idle bound**, because the bound is
per-attempt and the failure is classified `transient` and retried once. A user who
sets 180s (the cap) waits ~360s. That is defensible behaviour, but see O2.

**Observation F3-W10-O2 (low).** `OPENCODE_STREAM_IDLE_TIMEOUT_SECS` is named in the
error message the user sees, and it is the only lever they have, yet `grep -rn "idle"
docs/*.md` returns nothing but unrelated `perf-methodology.md` hits. The variable,
its 120s default, its 180s cap, and the retry doubling are undocumented. The error
message is good enough to act on blindly, so this is documentation debt, not a defect.

---

### S3 — permission dialog subject + diff + fullscreen (F3-W9-D1 / todo 157) — **FIXED**

Fixture: a mock openai-compatible provider that emits one permission-raising tool
call per turn, plus a config with `permission` set to `ask` for edit/write/bash/
webfetch/grep/todowrite. Driven in a real 200x50 tmux TUI, keys sent as keystrokes.

Fixture note worth recording, because it cost me a false lead: **opencode's first
request to the provider carries no `tools` array** — it is the title-generation call
(`"You are a title generator..."`). A counter-based mock therefore mis-attributes the
whole script by one. I dispatch on the presence of `tools` instead. Also every tool
schema requires an `intent` property, and `edit` requires the file to have been read
first, so a naive script silently produces argument errors rather than dialogs.

The wave-9 defect is gone. The `edit` dialog:

```
│ Permission required
│△ Permission required
│  → Edit /tmp/opencode/f3w10/cwd4/tui_target.txt
│  Path: /tmp/opencode/f3w10/cwd4/tui_target.txt
│
│@@ -1,1 +1,1 @@
│   1-ORIGINAL_LINE_BRAVO                             1+REPLACED_LINE_CHARLIE
│
│ Allow once   Allow always   Reject
│↑↓ select  enter confirm  ctrl+f fullscreen
```

Both halves of F3-W9-D1 are fixed: the **path** is present twice (title line and
explicit `Path:` row), and the **diff renders** with real unified-diff framing and
the old/new lines side by side. `filePath` vs `filepath` was indeed the root cause
and the wire name really is camelCase `filePath` — confirmed from the captured
request schema, `edit required= ['filePath','oldString','newString','intent']`.

**`ctrl+f` fullscreen now renders** (wave 9: completely empty):

```
│ Permission required
│△ Permission required
│  → Edit /tmp/opencode/f3w10/cwd4/tui_target.txt
│  Path: /tmp/opencode/f3w10/cwd4/tui_target.txt
│
│@@ -1,1 +1,1 @@
│   1-ORIGINAL_LINE_BRAVO                             1+REPLACED_LINE_CHARLIE
│
│ Allow once   Allow always   Reject
   ... (padding)
│↑↓ select  enter confirm  ctrl+f minimize
```

The footer correctly flips to `ctrl+f minimize`, and `ctrl+f` again returns inline.

**Every kind I could raise shows its own subject** — this is the table-driven fix
working in the product, not just in a test:

| kind | dialog title | subject row |
|---|---|---|
| `edit` | `→ Edit <abs path>` | `Path: <abs path>` + unified diff |
| `bash` | `# Shell command` | `$ echo F3_BASH_SUBJECT_MARKER` |
| `webfetch` | `% WebFetch https://example.invalid/f3-webfetch-subject` | `URL: https://example.invalid/f3-webfetch-subject` |
| `write` | `→ Edit <abs path>.new2` | `Path: <abs path>.new2` |
| `grep` | `✱ Grep "ORIGINAL"` | `Pattern: ORIGINAL` |
| `todowrite` | `⚙ Call tool todowrite` | `Tool: todowrite` |

No dialog was blank. Every one named what it was about to do.

**Denial works and is enforced, not just displayed.** Selecting `Reject` on the
`write` produced `tool write was denied by the permission layer`, and
`ls` confirms `tui_target.txt.new2` was **never created**. The allowed `edit`
did land — `cat` shows `REPLACED_LINE_CHARLIE` in place of `ORIGINAL_LINE_BRAVO`.

### S4 — advertised keys actually work (F3-W9-O1 / todo 157) — **FIXED**

The footer now reads `↑↓ select  enter confirm  ctrl+f fullscreen`. Wave 9's lie
(`⇆ select` with only Up/Down implemented) is gone. Verified each claim by keystroke,
reading the actual SGR highlight out of the pane rather than trusting the glyphs:

- `Down` → highlight moves `Allow once` → `Allow always` → `Reject`. Confirmed.
- `Up` → moves back. Confirmed.
- `enter` → confirms the highlighted option. Confirmed (single press, clean run).
- `ctrl+f` → toggles fullscreen both ways. Confirmed.

`Right`, `Left` and `Tab` are all no-ops — the highlight does not budge. Since the
footer no longer advertises them, the footer is now **honest**, which is what O1
asked for. They chose to correct the advertisement rather than add the keys; that is
a legitimate resolution.

**Observation F3-W10-O3 (cosmetic).** The dialog renders its title **twice**:
`│ Permission required` immediately followed by `│△ Permission required`. Present in
both inline and fullscreen, for every kind. Looks like a panel title plus a heading
row that duplicate each other.

**Observation F3-W10-O4 (low).** A `write` of a brand-new file is titled
`→ Edit <path>` and shows only the path — no content preview, in inline or in
fullscreen. The permission *kind* really is `edit` (there is no `write` key in the
permission schema), so the title is defensible, but a user approving the creation of
a file cannot see what will be written. `edit` gets a diff; `write` gets nothing.

**Observation F3-W10-O5 (low).** The inline dialog silently drops the diff when the
transcript grows tall enough to squeeze the panel — the same `edit` dialog showed its
diff at 10 rows and, after the transcript grew, showed only the path at 7 rows, with
no indication content had been elided. `ctrl+f` recovers it, so a user who knows the
key is fine; one who does not may approve an edit believing there is no diff to see.

**Unreproduced anomaly (recorded, not claimed as a defect).** On my first confirm,
one `Enter` did not take effect and a second was needed; every later dialog confirmed
on a single `Enter`. I could not reproduce it, and my keystroke sequence just before
it included `Up`/`Left`/`Right`/`Tab` probes, so I cannot attribute it to the product.

---

### S5 — valid-but-deep config reported "not valid JSON" (was F3-W9 obs O2) — **HALF FIXED**

**The exit code is fixed.** Wave 9's complaint that `models` printed the error while
exiting 0 no longer holds. All three surfaces now exit 1 and write to **stderr**:

```
$ opencode-rust debug config   → rc=1, stderr: config file OPENCODE_CONFIG_CONTENT is not valid JSON
$ opencode-rust models         → rc=1, stderr: (same), stdout empty
$ opencode-rust run "hi"       → rc=1, stderr: (same), stdout empty
```

(My first pass appeared to show rc=0; that was `head` in the pipeline eating the
status. Re-measured directly, it is 1.)

**The misleading message still reproduces.** JSON that `python3 -m json.load`
accepts is reported as "not valid JSON". Bisected the boundary exactly:

```
depth=120 rc=0  (accepted)
depth=121 rc=0
...
depth=125 rc=0  (accepted)
depth=126 rc=1  config file OPENCODE_CONFIG_CONTENT is not valid JSON
depth=127 rc=1
depth=200 rc=1
```

The cliff is between **125 and 126** nested objects, which is a parser recursion
limit, not a syntax property of the document. Reproduces identically for an on-disk
config, so it is not an env-var artifact:

```
$ (./opencode.json = 200-deep valid JSON)
config file /tmp/opencode/f3w10/cwd5/opencode.json is not valid JSON
```

The message names the wrong cause. A user is told their file is malformed and will
go looking for a missing brace that is not there; nothing points at nesting depth.
`grep -in "recursion|depth|nest" docs/rejected-inputs.md` → no matches, and
`docs/divergences.md` has nothing on nesting either, so the limit is **undeclared**.

Severity is low — 126 levels of nesting is not a realistic config — but the fix is a
message, not a limit. Ranked as observation, not defect.

### S6 — `docs/plugin-authoring.md` "a path" (was F3-W9 obs O3) — **DOC IS CORRECT**

The line still reads:

> A bare entry is an npm specifier, a `file://` URL, or a path

Wave 9 doubted the "a path" clause. **Tested it, and it works.** A bare absolute
filesystem path in `config.plugin` is accepted and reaches the JS host — proven by
the host naming that exact path in its own diagnostics
(`plugin=/tmp/opencode/f3w10/cwd6/f3plugin.js`) and, once the environment was
complete, by the plugin actually running (see S12). The doc is accurate; I withdraw
the wave-9 doubt.

### S7 — plugin write-back rejection JSON Pointer (was F3-W9 obs O4) — **NOT REACHED**

I could not construct a config write-back rejection in this environment, so I have
no observation to report either way. Recording it as unverified rather than guessing.

### S9 — `plugin/` + `plugins/` auto-discovery (F3-W8-D1) — **STILL AN UNDECLARED DEFECT**

`docs/plugin-authoring.md` still promises, verbatim:

> Beyond the config array, both `plugin/` and `plugins/` directories are scanned for
> `*.ts` and `*.js` in the global and project trees, and provenance is retained
> (`oc_plugin::PluginOrigin`) so a diagnostic can name the file that contributed a
> plugin.

I placed the **same plugin file I had already proven loads and runs** (plugA.js,
rc=0 when named in `config.plugin`) into **all four** advertised locations:

```
./plugin/discovered_project_plugin.js
./plugins/discovered_project_plugins.js
$XDG_CONFIG_HOME/opencode/plugin/discovered_global_plugin.js
$XDG_CONFIG_HOME/opencode/plugins/discovered_global_plugins.js
```

and ran a real turn with **no `plugin` key in the config**:

```
rc=0  stdout='SCRIPT_EXHAUSTED_DONE'
--- any plugin mentioned in logs (--log-level DEBUG)? ---
(none)
--- did any discovered plugin run? ---
NO — plugin.log absent, nothing auto-discovered
```

Not one of the four loaded. Not one was mentioned even at DEBUG. Using a
known-good plugin file removes the "your plugin was broken" explanation: the
scanning does not happen.

Still **undeclared** — `grep -in "discover|plugin/|plugins/" docs/divergences.md`
returns only the unrelated `split-version-identity` entry. So this remains what I
called it in wave 8: documentation promising a capability production does not have.
Two review waves have now passed with the doc unchanged.

**Severity: defect (documentation).** No user data is at risk, but a plugin author
following the official authoring guide gets silence — no plugin, no warning, no
diagnostic naming the file. Either wire the scan, or delete the paragraph.

### S12 — plugin `parts` mutation reaches the model request (todo 158) — **VERIFIED**
### S12b — `chat.message` identity fields (todo 159) — **VERIFIED**

Getting a JS plugin to run at all took real work, and the obstacles are findings in
themselves (see O6/O7 below). Once the environment was complete — `bun` on PATH and
a real `@opencode-ai/sdk` with its `cross-spawn` dependency in `node_modules` — the
plugin loaded and both todos check out.

**Todo 159 — `chat.message` carries real identity.** The hook input, logged from
inside the plugin:

```json
{
  "agent": "build",
  "messageID": "msg_25cf52dbabe24f279481b8324d334b7a",
  "model": { "modelID": "test-model", "providerID": "test" },
  "sessionID": "ses_7a352218b4c54244b60086b91c44cc53",
  "variant": null
}
```

Real ids, not placeholders, and the field set matches upstream's published type
(`@opencode-ai/plugin/dist/index.d.ts:187` — `sessionID`, `agent?`, `model?`,
`messageID?`, `variant?`) exactly. `output.message` likewise carries `id`, `agent`,
`model`, `role`, `sessionID`, `time.created`.

**Todo 158 — a canonical `parts` mutation reaches the wire.** A plugin that appends
a **fully-formed** part (carrying `id`/`messageID`/`sessionID` alongside `type` and
`text`) succeeds, and the injected text appears in the request my mock provider
captured off the socket:

```
rc=0
$ grep -l F3_PLUGIN_INJECTED_PART /tmp/opencode/f3w10/plain-*.json
/tmp/opencode/f3w10/plain-8.json
$ (decode that request's messages)
role= user  content= "hello pluginF3_PLUGIN_INJECTED_PART"
```

That is the claim proven end to end: plugin mutation → provider HTTP request body.
Verified against a captured request, not against a log line.

**Observation F3-W10-O6 (defect-adjacent — opaque, turn-killing plugin failure).**
The word "canonical" in todo 158 is load-bearing, and nothing tells the plugin author
so. Three variants, same hook, same plugin:

| plugin `chat.message` body | rc | user-visible output |
|---|---|---|
| logs only, no mutation | 0 | normal assistant reply |
| appends **fully-formed** part | 0 | normal reply, injection on the wire |
| appends `{type:"text", text:"..."}` | **1** | **no reply at all**; `plugin <path> failed in hook chat.message` |

The natural, minimal part — the shape a plugin author would write first — **aborts
the entire turn** and yields no assistant output. The diagnostic is one line naming
the plugin and the hook and *nothing else*: no field name, no "missing id", no
rejected JSON. I re-ran at `--log-level DEBUG` and the only matching line was still
`plugin ... failed in hook chat.message`. This is exactly the gap wave 9's O4 raised
about write-back rejections (plugin + hook named, no pointer to the offending
member), reappearing on a different surface.

Strictly, rejecting a part with no `id` is defensible: upstream's `Part` type does
require those fields. Two things still make this worth an owner's time — a
recoverable validation failure takes the whole turn down rather than dropping the
mutation, and the message gives the author no way to find their mistake.

**Observation F3-W10-O7 (defect-adjacent — a plugin that cannot load costs 30s of
total silence).** Before I fixed the environment, the JS host could not start (`bun`
absent). Measured, same config, same prompt, only the `plugin` key differing:

```
WITH broken plugin: rc=0 elapsed=30s stdout='SCRIPT_EXHAUSTED_DONE' stderr_bytes=0
WITHOUT plugin:     rc=0 elapsed=0s  stdout='SCRIPT_EXHAUSTED_DONE'
```

**30 seconds of dead wall time, zero bytes on stderr, exit 0.** The real reason
exists —

```
WARN oc_cli::cmd::plugin_runtime: JavaScript plugin did not fully load
  plugin=... kind=FailedToLoad plugin `...` did not connect back within 30000 ms
```

— but only at `--print-logs`. At default verbosity the user sees a healthy run that
is inexplicably 30s slower, on **every invocation**. A `run` that would take 0s takes
30s forever, silently. Compare this against S1, where the team did surface a bound
and its remedy in the error text; the same treatment here would cost one line.

The second failure mode is equally quiet and more likely in the wild: with `bun`
present but the SDK missing, the host reports `could not locate a real
@opencode-ai/sdk with createOpencodeClient; refusing to substitute a hand-rolled
client` and lists six probed paths. That message is genuinely good — but again only
at `--print-logs`, and `docs/plugin-authoring.md` never mentions that a JS plugin
needs `bun` and a real `@opencode-ai/sdk` resolvable from the project. Both times I
had to read DEBUG logs to learn why my plugin was inert.

---

### S10 — future migration journal refused before serving queries (todo 155) — **VERIFIED**

Took a database the binary had itself created, injected a journal row with an id far
in the future, and drove the real `db` command against it.

```
$ opencode-rust db "select 1 as one"
rc=1
stdout: (empty)
stderr: database migration journal is newer than this binary (known ceiling 20260622202450_simplify_session_input, observed 29991231235959_from_the_future)
```

Everything todo 155 promised is observable:

- **Both ids are named** — the ceiling `20260622202450_simplify_session_input` and the
  observed `29991231235959_from_the_future`. A user can tell instantly that their
  binary is older than their data, which is the actionable fact.
- **Refused before serving any query.** I tried the query that matters:
  `db "delete from session"` → same refusal, rc=1, and the data survived
  (`select count(*) from session` = 1 before and after, journal still 39 rows). An
  older binary cannot corrupt a newer database through this door.
- **The guard is not `db`-local.** `session list` and `run "hi"` both refuse with the
  identical message, so the check sits under the shared database open rather than in
  one command. That is the right placement.

Baseline sanity: the same `db "select 1 as one"` against the un-tampered database
returns `one / 1`, rc=0 — so the refusal is caused by the injected row, not by my
environment.

### S8 — un-prefixed v1 routes still `not_implemented` (F3-W9-O2) — **STILL REPRODUCES, and the hint is now stale**

```
$ curl -s http://127.0.0.1:18899/session
HTTP 501
{"error":{"callers":["@sunerpy/oh-my-openagent@4.21.0"],"code":"not_implemented",
  "hint":"the route is measured and registered; its backend lands in todos 57-62",
  "message":"`client.session.list` has no local backend in this build",
  "route":"GET /session","sdkMethod":"client.session.list"}}

$ curl -s -X POST -d '{}' http://127.0.0.1:18899/session      → HTTP 501 (same shape, client.session.create)
```

Two things changed the reading of this since wave 9, both making it worse:

**1. The hint promises work that is already finished.** It says the backend "lands in
todos 57-62". Per my brief, **all 161 implementation todos are checked**. A user or
plugin author reading this error is told to wait for something that has shipped.

**2. The backend demonstrably exists, one prefix away.** The same capability is fully
served under `/api`:

```
$ curl -s http://127.0.0.1:18899/api/session
HTTP 200  {"data":[],"cursor":{"previous":null,"next":null}}

$ curl -s -X POST -d '{}' http://127.0.0.1:18899/api/session
HTTP 200  {"data":{"id":"ses_3903c03b...","projectId":"global",...}}
```

So `client.session.list` "has no local backend in this build" is not accurate — the
backend is there and works; only the v1 alias is unrouted.

**Scope: 17 of the 18 v1 routes I probed answer `not_implemented`.** Only
`/tui/show-toast` is genuinely wired (HTTP 400 on my empty body, i.e. it reached a
handler that validated input):

```
PUT  /auth/openai                    -> 501 not_implemented
POST /log                            -> 501 not_implemented
GET  /agent                          -> 501 not_implemented
GET  /config                         -> 501 not_implemented
GET  /provider                       -> 501 not_implemented
GET  /session                        -> 501 not_implemented
POST /session                        -> 501 not_implemented
GET  /session/status                 -> 501 not_implemented
GET  /session/{id}                   -> 501 not_implemented
PATCH /session/{id}                  -> 501 not_implemented
GET  /session/{id}/children          -> 501 not_implemented
GET  /session/{id}/todo              -> 501 not_implemented
POST /session/{id}/abort             -> 501 not_implemented
POST /session/{id}/summarize         -> 501 not_implemented
GET  /session/{id}/message           -> 501 not_implemented
POST /session/{id}/message           -> 501 not_implemented
POST /session/{id}/prompt_async      -> 501 not_implemented
POST /tui/show-toast                 -> 400 SERVED
TOTALS: not_implemented=17  served=1
```

**Why this matters beyond a stale string.** `docs/compatibility-matrix.md` presents
these under the heading **"v1 plugin compatibility routes"**, describes them as "the
set of routes the installed JavaScript plugins were **measured calling**", and each
error body names its real caller (`@sunerpy/oh-my-openagent@4.21.0`). By the
project's own evidence, a real installed plugin calls `client.session.list` — and
gets a 501. So the v1 surface as shipped does not deliver the plugin compatibility
its section title claims, for 17 of 18 routes.

To be fair to the doc: it does say the set is "registered", and that
`compat_v1.rs` "asserts every route has a recorded callsite and that **none answers
404**". 501-not-404 is literally satisfied. The gap is between that careful wording
and the section's framing as a compatibility surface, plus a hint that now points at
completed work. I found nothing in `docs/divergences.md` declaring the v1 surface as
stubs (`grep -in "v1\b"` → no matches).

**Severity: defect (undeclared divergence / stale diagnostic).** Not data-threatening.
The minimum fix is honesty: drop the "lands in todos 57-62" promise, and either route
v1 to the `/api` handlers that already work or declare the stub status in
`divergences.md`.

**Incidental positive.** `serve` warns unprompted:
`Warning: OPENCODE_SERVER_PASSWORD is not set; server is unsecured.` Good default —
the unauthenticated surface announces itself.

---

### S11 — Azure/Copilot Responses surface sends `input`, decodes typed `response.*` (todo 156) — **VERIFIED against local mocks**

No Azure or Copilot credentials here, so I did what the brief asks: built local mocks
that record the request **path** and **body** and reply with typed `response.*` SSE
events (`response.created`, `output_item.added`, `content_part.added`,
`output_text.delta` ×3, `output_text.done`, `content_part.done`,
`output_item.done`, `response.completed`). Nothing about live Azure/Copilot
authentication is claimed.

**First attempt failed and the failure was mine.** I declared the provider with
`npm: "@ai-sdk/openai-compatible"` and got `/chat/completions` + `messages`. The
transport id matters: `azure` must declare `@ai-sdk/azure` and `github-copilot`
`@ai-sdk/github-copilot`. Recording this because it is a genuine trap — a config
that names the generic compatible npm silently loses the Responses surface.

**Azure — correct.** `provider.azure` on `@ai-sdk/azure`:

```
paths:      /responses
body keys:  ['input', 'model', 'stream', 'tools']     ← input, NOT messages
stdout:     RESPONSES_SURFACE_OK
rc:         0
```

`input` is the typed array shape, not a flattened string:

```json
[{"content":"","role":"system"},
 {"content":[{"text":"hello azure","type":"input_text"}],"role":"user"}]
```

And the typed events really are decoded — `RESPONSES_SURFACE_OK` on stdout was
assembled from three `response.output_text.delta` events, so the decoder is
consuming `response.*` rather than falling back to chat-chunk parsing.

**Copilot — correct, including the per-model-id rule.** Same mock, three model ids:

| model | path | body | stdout |
|---|---|---|---|
| `gpt-5` | `/responses` | `input` | `RESPONSES_SURFACE_OK` |
| `gpt-5-mini` | `/chat/completions` | `messages` | (empty — mock speaks only Responses) |
| `gpt-4o` | `/chat/completions` | `messages` | (empty — same) |

That is exactly the documented `/^gpt-(\d+)/`, `major >= 5`, minus the explicit
`gpt-5-mini` exclusion — reproduced in the shipped binary against a real socket, per
model id. The two `messages` rows produce empty output only because my mock answers
Responses events on every path; the surface selection is the measured claim.

**Observation F3-W10-O8 (defect-adjacent — the title generator ignores the resolved
surface).** On the Azure run, two requests were made and they did **not** agree:

```
azure-path-2 -> /chat/completions   body keys ['messages','model','stream']
azure-path-3 -> /responses          body keys ['input','model','stream','tools']
```

Request 2 is the title generator — its first message is
`"You are a title generator. You output ONLY a thread title..."`. Same provider,
same model, same config, yet the internal title call goes to the **chat** surface
while the user's turn goes to **responses**. Same split on Copilot `gpt-5`.

This is invisible against my mock, which answers on any path. Against a real Azure
deployment that exposes only the Responses API, the user's turn would work and title
generation would 404 on `/chat/completions`. I could not test that consequence
without credentials, so I am reporting the measured surface split as fact and the
consequence as inference, explicitly labelled.

---

### S13 — TUI runtime loads `Tui`-kind plugins, shim executes `tui()` (todo 160) — **VERIFIED, and the declared boundary matches reality**

Wrote a real v1 TUI plugin (default export carrying `id` + `async tui()`), whose
`tui()` returns an object with `status()`, `render()` and `keybind()`, each logging
when called. Then drove the real TUI in tmux.

**The kind gate works in both directions.** On the **turn** surface the same plugin is
rejected with the oracle's own wording, and the diagnostic names the surface:

```
kind=Protocol plugin `.../f3tuiplugin.js` failed `init`:
  Plugin /tmp/opencode/f3w10/cwd6/f3tuiplugin.js must default export an object with server()
  surface="turn"
```

On the **TUI** surface it loads and `tui()` is executed:

```
MODULE_EVALUATED {"when":1786498835420}
MODULE_EVALUATED {"when":1786498835474}
TUI_FACTORY_CALLED {"argType":"object","argKeys":["client","project","directory","worktree","serverUrl","experimental_workspace","$"]}
```

`tui()` is genuinely invoked and handed the full plugin context — the same seven-key
object the server-side hook receives.

**The declared boundary — "the returned object is not wired into Rust view hooks" —
is exactly what an author observes.** I sent a prompt, completed a turn, pressed
`Escape`, `ctrl+f`, `Up` and `Down`, and after all of it the plugin's log still had
only the three lines above:

- `STATUS_CALLED` — never
- `RENDER_CALLED` — never
- `KEYBIND_CALLED` — never
- `F3_TUI_STATUS_MARKER` / `F3_TUI_RENDER_MARKER` on screen — 0 occurrences

So the capability is precisely as declared: the plugin loads, its `tui()` runs, it
gets a context it can use (it has `client` and `serverUrl`, so it can drive the
server API), and nothing it *returns* affects the Rust-rendered view. The
declaration is accurate — no undeclared over- or under-delivery. Good.

**Observation F3-W10-O9 (cosmetic).** The plugin module is evaluated **twice**, 54 ms
apart, on a single TUI start. Presumably once to classify the kind and once to
execute. Harmless for a pure module, but a plugin doing work at module scope (opening
a file, connecting a socket, incrementing a counter) does it twice per start. Worth
either a note in the authoring doc or a single evaluation.

---

## Trying to break it — error paths, empty state, concurrency, streams

### Bad and hostile input — mostly excellent

| input | result |
|---|---|
| `{"formatter":` (truly malformed) | rc=1, `config file ... is not valid JSON` — correct |
| `{"totallyBogusKey": 1}` | rc=1, `failed validation (1 issue(s))` / `totallyBogusKey: unrecognized key` |
| `{"formatter": "yes"}` | rc=1, `formatter: data did not match any variant of untagged enum FormatterConfig at line 1 column 20` |
| `{"permission": 5}` | rc=1, `permission: invalid type: integer 5, expected one of "ask", "allow", "deny", or an object of permission rules at line 1 column 16` |
| `{"provider": "string-not-object"}` | rc=1, `provider: invalid type: string ..., expected an object at line 1 column 32` |
| `run --nonsense` | rc≠0, `error: unexpected argument '--nonsense' found` + a `tip:` |
| `--model no-such-provider/no-such-model` | rc≠0, and the message names **all three** ways to fix it |

Those validation messages are genuinely good — key path, expected type, and a line/column.
The unavailable-model error is the best string I saw all wave:

```
model `no-such-provider/no-such-model` is not available: no `provider` block in your
configuration defines it, OPENCODE_DISABLE_MODELS_FETCH is set so no fetch from
`https://models.opencode.ai` was attempted, and no cached catalog exists at
`/tmp/.../models.json`. Define the provider and model under `provider` in your config,
or unset OPENCODE_DISABLE_MODELS_FETCH to fetch the catalog, or set
OPENCODE_MODELS_PATH to a catalog file on disk
```

**Observation F3-W10-O10 (low) — `theme` is the one config key with no type validation.**
Every other key rejects a wrong type with a precise message; `theme` accepts anything:

```
{"theme":12345}                -> rc=0   (no error)
{"theme":"no-such-theme-xyz"}  -> rc=0   (no error)
{"permission":5}               -> rc=1   invalid type: integer 5, expected ...
```

An integer, and a theme name that cannot exist, are both accepted silently. Given how
precise the neighbouring messages are, a user who typos their theme gets no signal.
(I could not tell from `debug config` whether the value is dropped or retained — the
command does not echo `theme` even when it is a valid `"system"` — so I claim only the
missing validation, not the downstream handling.)

**Empty state is clean.** With an isolated HOME, no config, no catalog and fetch
disabled: `models` → rc=0, zero lines, no error (reasonable — an empty list is not a
failure), while `run "hi"` correctly refuses with the full three-way remedy above.

**Methodology correction, recorded against myself.** My first error-path pass wrote
`export HOME=X XDG_CONFIG_HOME=$HOME/.config` as a **single** statement, where `$HOME`
still holds the old value — so that run read my real `/config/.config/opencode` and
`models` listed 47 real models. `debug paths` is what caught it (`config` pointed
outside my sandbox). Re-run with separate exports, `debug paths` showed every path
inside the sandbox. The product was correct; my isolation was not. No finding here, but
the earlier "47 models with no config" line in my scratch notes was an artifact.

### Concurrency — passes

12 simultaneous `POST /api/session` requests:

```
     12 200          (all twelve HTTP 200)
distinct ids: 12     (no id collisions, no lost writes)
sessions listed: 13  (12 new + 1 pre-existing)
```

### SSE — bounded reads, and it genuinely streams

Always bounded with `--max-time`, as instructed; `curl` rc=28 is the expected outcome
for a stream that never ends on its own.

```
$ curl -sN --max-time 8 /api/event
data: {"data":{},"id":"evt_...","type":"server.connected"}
```

And a mutation made by a *different* client mid-read arrives on the open stream:

```
data: {"data":{},"id":"evt_...","type":"server.connected"}
data: {"data":{"info":{...,"id":"ses_79c0dfe3...","title":"New session - ses_79c0dfe3..."},
       "sessionID":"ses_79c0dfe3..."},
       "durable":{"aggregateID":"ses_79c0dfe3...","seq":0,"version":1},
       "id":"evt_...","type":"session.created"}
```

Real event fan-out across clients, with durable sequencing attached. Working.


## Test gate

```
cargo test --workspace --offline
TOTAL passed=3421  failed=0  ignored=2   (212 `test result` lines, 0 FAILED blocks)
```

Matches the expected **3421 passing / 0 failed** exactly. No `EAGAIN`, no compile
errors, single run, no retry needed.

Worth stating plainly: this suite was green for **every** finding below. The four
still-open items were all found by using the product, not by reading it.

## Scenario ledger

| # | Scenario | Verdict |
|---|---|---|
| S0 | Build + gate (3421/0) | PASS |
| S1 | Stalled provider now bounded, partial preserved, error names the lever | **FIXED** |
| S1b | Production default (unset env) bounded at 120s | **PASS** |
| S2 | Slow-but-progressing stream survives (idle, not total) | **PASS** |
| S3 | Permission dialog subject + diff + fullscreen, 6 kinds | **FIXED** |
| S4 | Advertised keys work; footer honest | **FIXED** |
| S5 | Deep-config exit code fixed; misleading message remains | **HALF FIXED** |
| S6 | docs "a path" claim | **DOC CORRECT** (wave-9 doubt withdrawn) |
| S7 | Plugin write-back JSON Pointer | **NOT VERIFIED** — could not construct |
| S8 | Un-prefixed v1 routes | **STILL OPEN**, hint now stale |
| S9 | `plugin/`+`plugins/` auto-discovery | **STILL OPEN** (3rd wave) |
| S10 | Future migration journal refused | **VERIFIED** |
| S11 | Azure/Copilot Responses (`input`, typed `response.*`) | **VERIFIED** (local mocks) |
| S12 | Plugin `parts` mutation reaches the wire | **VERIFIED** |
| S12b | `chat.message` identity fields | **VERIFIED** |
| S13 | TUI loads Tui plugins, `tui()` runs, boundary accurate | **VERIFIED** |

## Findings

### Defects (undeclared divergence — documentation/diagnostics, no data at risk)

- **F3-W10-D1 — `plugin/` and `plugins/` auto-discovery is documented but does not
  happen** (S9). Four advertised locations, a plugin file already proven to load,
  zero of four loaded, nothing logged even at DEBUG, undeclared in
  `divergences.md`. Third wave carrying this. Either wire it or delete the paragraph.
- **F3-W10-D2 — the v1 surface is 17/18 stubs, and its `not_implemented` hint points
  at completed work** (S8). `"lands in todos 57-62"` when all 161 are checked, while
  `/api/session` serves the same capability. Presented under a heading that calls
  these "plugin compatibility routes" measured from real plugin callsites; those
  plugins get 501. Undeclared.

### Defect-adjacent (an owner should decide; both are silent-failure ergonomics)

- **F3-W10-O6 — a non-canonical `parts` mutation kills the whole turn with a
  one-line diagnostic.** The minimal, natural part shape a plugin author writes first
  produces rc=1, no assistant output, and `plugin <path> failed in hook chat.message`
  with no field name even at DEBUG. Same "names the hook, not the member" gap wave 9
  raised for write-backs.
- **F3-W10-O7 — a plugin that cannot load costs 30s of total silence on every run.**
  Measured 30s vs 0s, `stderr_bytes=0`, rc=0. The real reason exists only at
  `--print-logs`. Also: `docs/plugin-authoring.md` never says a JS plugin needs `bun`
  and a resolvable `@opencode-ai/sdk`.
- **F3-W10-O8 — the title generator ignores the resolved API surface.** On Azure and
  on Copilot `gpt-5`, the user's turn goes to `/responses` while the internal title
  call goes to `/chat/completions`, same provider and model. Consequence on a
  Responses-only deployment is inference, not measurement — no credentials.

### Observations (low / cosmetic)

- **O2** — `OPENCODE_STREAM_IDLE_TIMEOUT_SECS`, its 120s default, 180s cap and the
  retry doubling (user waits ~2x the bound) are entirely undocumented.
- **O3** — permission dialog renders its title twice, every kind, both modes.
- **O4** — a `write` permission prompt shows no content preview; `edit` gets a diff.
- **O5** — the inline dialog silently drops the diff when the panel is squeezed, with
  no elision marker; `ctrl+f` recovers it.
- **O9** — a plugin module is evaluated twice per TUI start, 54 ms apart.
- **O10** — `theme` is the one config key with no type validation (`12345` and a
  nonexistent theme name both rc=0) while its neighbours give precise messages.
- **S5** — valid JSON nested ≥126 deep is reported as "not valid JSON"; boundary
  bisected at 125/126; undeclared. Exit code is now correct (1, on stderr).

### Withdrawn

- **O1** (`--version` 1.18.13 vs 1.18.15) — declared in `divergences.md` as
  `split-version-identity`, correctly implemented. Not a defect.
- **Wave-9 O3** ("a path" in plugin-authoring.md) — the doc is accurate; a bare
  filesystem path does work.

## Could not verify

- **S7 / wave-9 O4** — plugin config write-back rejection and its JSON Pointer. I
  could not construct a write-back rejection through the CLI, so I have no
  observation. Not reported as fixed or broken.
- **Live Azure / GitHub Copilot** — no credentials. S11 is verified against local
  mocks that record path + body and emit typed `response.*` events; nothing about
  live authentication, real endpoint URL assembly, or the deployment-name path is
  claimed. The O8 consequence is labelled inference.
- **`azure-cognitive-services`** — it is in the surface profile table but not in the
  CLI's transport table, so I could not route a request to it the way I did for
  `azure`. Untested; I make no claim about it.

## Verdict

**VERDICT: PASS WITH DEFECTS — no blocker.**

The wave-9 work landed and it landed properly. My stall finding (F3-W9-D2) is fixed
in the way that matters: bounded on **idle** rather than by a total deadline, so the
hang is gone (10s vs 200s+) *and* a legitimately slow provider still completes — I
verified both halves against real sockets, plus the shipped 120s default with the
override unset. The permission dialog (F3-W9-D1) went from blank to showing a
subject for every one of the six kinds I could raise, with a real diff, working
fullscreen, and a footer that no longer advertises keys it does not implement;
Reject provably blocked a write. Todos 155, 156, 158, 159 and 160 all check out
against the product, and the todo-160 capability boundary is *exactly* what a plugin
author observes — declared honestly, neither over- nor under-sold.

Nothing I found threatens data or blocks a release. The two defects are both
**documentation promising more than production delivers** — directory plugin
discovery that never runs, and a v1 route surface that is 17/18 stubs behind a hint
pointing at finished work. Undeclared, which by this project's own standard makes
them defects rather than divergences, but the fix for either could be a doc edit.

The theme running through my defect-adjacent findings is worth naming, because it is
one theme and not three: **this build is excellent at explaining failures the user
caused and quiet about failures inside itself.** Config validation names the key, the
expected type and the column; the unavailable-model error lists three remedies; the
new idle-timeout error names the env var to raise. But a plugin that cannot load
costs 30 silent seconds forever at rc=0, a plugin that mutates `parts` the obvious way
loses the whole turn to a one-line message with no field name, and a diff silently
vanishes from a permission dialog when the panel is short. The S1 error message is the
model to copy; these three are where it has not been applied yet.

---

*Every result above was produced by running `target/debug/opencode-rust` at
`2e742986` — real sockets, real tmux TTY, real curl, captured request bodies. Where I
could not run something I said so instead of inferring it.*
