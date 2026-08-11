# F3 Manual QA Report — Wave 9

**Audited HEAD:** `c251665a`
**Worktree:** `/config/workspace/ProdDir/AI/oc-wt/tF3` (branch `task-F3`)
**Role:** real manual QA — run the built artifact, report observed output only.

**VERDICT: CHANGES REQUESTED** — 2 defects (1 needs a decision, 1 blocker-adjacent),
7 observations. See the summary table at the end.

---

## Planned scenarios

### Mandatory — wave-8 follow-ups
- S1. Build the artifact (`cargo build --offline`) and run the gate once.
- S2. **SEAM #20 re-test** (was F3-W8-D2 blocker): TUI permission dialog must show the actual
  command/URL and must be answerable by a keypress, unblocking the turn. Test `bash`,
  `webfetch`, `edit`.
- S3. **F3-W8-D1 re-test**: `docs/plugin-authoring.md` `plugin/` + `plugins/` auto-discovery vs
  production reading only `config.plugin`; `discover_plugins` production caller.
- S4. **O1 re-test**: internal title generation bypassing `chat.params` / `chat.headers`.
- S5. **O2 re-test**: deeply nested valid JSON config reported "not valid JSON"; `models`
  printing the error but exiting 0.
- S6. **O3 re-test**: TUI not repainting on resize until next keystroke.
- S7. The five previously-unexercised hooks: `command.execute.before`,
  `experimental.session.compacting`, `experimental.compaction.autocontinue`, `dispose`
  beyond shutdown, `tool` beyond registration.

### Mandatory — what landed since wave 8
- S8. **Todo 151**: plugin hook write-back rejects bounded-encoder truncation on *every*
  mutable hook. Over-deep payload → clear error naming the plugin and the JSON Pointer path,
  never silent corruption.
- S9. **Todo 152**: provider identity preserved through selection — OpenRouter, Azure,
  GitHub Copilot profiles, and `@ai-sdk/groq` / `@ai-sdk/mistral` transports reachable.
  Verify against local mocks.
- S10. **Todo 153**: config differential fixture asserts against live file (not user-facing;
  note anything odd only).

### Exploratory (only after the above are written up)
- S11. CLI abuse: bad input, missing config, wrong flags, `--help`, empty state.
- S12. Server / SSE with bounded reads, concurrent clients, interrupted streams.

---

## Results

### QA rig

Isolation root `/tmp/opencode/f3w9`. Every product run goes through
`/tmp/opencode/f3w9/run.sh`, which is `env -i` with a temporary `HOME` and the full
XDG set, `OPENCODE_DISABLE_MODELS_FETCH=1`, and nothing else inherited. A Python mock
OpenAI-compatible provider on `127.0.0.1:43881` logs every inbound request (path,
model, sampling params, header subset, message roles, system text, advertised tools)
as one JSON line, and is *scripted* by env var so I can make the assistant emit a
specific tool call on demand: `text`, `bash`, `webfetch`, `edit`. Long-lived
processes (mock, TUI, `serve`) run in their own tmux session.

Project config `/tmp/opencode/f3w9/project/opencode.json` sets
`"permission": {"bash":"ask","webfetch":"ask","edit":"ask"}` and one provider
`test` (`@ai-sdk/openai-compatible`) with model `test-model`.

### S1. Build — PASS

```
$ cargo build --offline
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 30.50s
$ ls -la target/debug/opencode-rust
-rwxrwxr-x 2 abc abc 155970736 Aug 11 17:54 target/debug/opencode-rust
```

`--help` lists 22 subcommands. The eight "explain why this is excluded" commands
(`console`, `web`, `stats`, `github`, `pr`, `upgrade`, `uninstall`, `generate`,
plus `completion`) are present as commands rather than silently missing, which is
the declared divergence behaviour.

Rig smoke test — a plain turn against the mock printed the mock's scripted answer
and exited 0:

```
$ run --model test/test-model "hello F3 wave9"
F3_ANSWER_OK
EXIT=0
```

### S2. SEAM #20 (was F3-W8-D2, my wave-8 blocker) — FIXED for `bash`; one residual footer/keymap mismatch

**Part 1 — the dialog now shows what it is approving. FIXED.**

Same setup as wave 8 (`"permission": {"bash":"ask"}`, TERM=xterm-256color, tmux pane
170x44). Mock scripted to call `bash` with
`{"command":"touch /tmp/opencode/f3w9/TUI_BASH_SENTINEL","intent":"f3 probe"}`.
Verbatim pane, blank lines stripped:

```
> You
  F3W9 bash permission probe
* Assistant
  … $ bash
… working
│ Permission required
│△ Permission required
│  # Shell command
│  $ touch /tmp/opencode/f3w9/TUI_BASH_SENTINEL
│
│ Allow once   Allow always   Reject
│⇆ select  enter confirm  ctrl+f fullscreen
```

The command line `$ touch /tmp/opencode/f3w9/TUI_BASH_SENTINEL` is present. In wave
8 that row was the border glyph and nothing else. The `Value::Null` tool-input path
is gone from the surface I can see.

**Part 2 — a key answers it and the turn unblocks. FIXED.**

Selection read from the real highlight attribute (`tmux capture-pane -e`, selected
background `48;2;250;178;131`), not guessed.

`Enter` on `Allow once`:

```
* Assistant
  ✓ $ touch /tmp/opencode/f3w9/TUI_BASH_SENTINEL
      (no output)
```
and the sentinel exists: `ls` → `/tmp/opencode/f3w9/TUI_BASH_SENTINEL`.

`Down Down` to `Reject`, then `Enter`:

```
* Assistant
  ✗ $ bash error
      tool bash was denied by the permission layer
```
and the sentinel is absent. So both the allow and the deny path are driveable from
the keyboard, and the decision reaches the tool layer. The wave-8 deadlock is gone.

**Residual — F3-W9-O1 (observation): the footer advertises keys that do nothing.**

Navigation keys, each pressed once, selection re-read after each:

```
Right    -> Allow once      (no change)
Left     -> Allow once      (no change)
Tab      -> Allow once      (no change)
BTab     -> Allow once      (no change)
Down     -> Allow always    (moved)
Up       -> Allow once      (moved back)
j        -> Allow once      (no change)
l        -> Allow once      (no change)
h        -> Allow once      (no change)
Space    -> Allow once      (no change)
```

The buttons are laid out **horizontally** and the footer says `⇆ select`, but the
only keys that move the selection are `Up`/`Down`. A user following the on-screen
hint presses Left/Right or Tab, sees nothing move, and has no way to know
`Allow always` and `Reject` are one arrow-down away — which is close to the wave-8
symptom from the user's side, even though the dialog is now functional. Minor and
cosmetic-adjacent rather than a blocker, because `Enter` alone completes the common
case and `Down` does reach the other two.

**`webfetch` — FIXED.** Mock scripted to fetch `https://example.invalid/f3w9-probe?x=1`:

```
│ Permission required
│△ Permission required
│  % WebFetch https://example.invalid/f3w9-probe?x=1
│  URL: https://example.invalid/f3w9-probe?x=1
│
│ Allow once   Allow always   Reject
```

The URL is present, twice (title and detail — redundant, not wrong). `Enter` on
`Allow once` unblocked the turn and the tool actually ran:

```
* Assistant
  ✗ % webfetch error
      tool webfetch failed: request to https://example.invalid/f3w9-probe?x=1 failed
```
(the fetch failing is expected — `.invalid` does not resolve; the point is that the
tool executed rather than the turn stalling.)

---

### DEFECT F3-W9-D1 — the `edit` permission dialog still shows nothing about what it will edit; fullscreen shows nothing at all

This is SEAM #20 **still open on the `edit` tool**. `bash` and `webfetch` were fixed;
`edit` was not.

Mock scripted to call `edit` on `/tmp/opencode/f3w9/project/edit-target.txt`
(`oldString: "ORIGINAL"` → `newString: "F3_EDITED"`), same TUI, pane 170x44, same
`"permission": {"edit":"ask"}`:

```
> You
  F3W9 edit probe
* Assistant
  … → edit
… working
│ Permission required
│△ Permission required
│  → Edit
│
│ Allow once   Allow always   Reject
│⇆ select  enter confirm  ctrl+f fullscreen
```

The file path is nowhere on screen. No diff, no old/new string, no filename. Verified
byte-exactly with `cat -A` that the detail row is empty — border glyph then
end-of-line, nothing between:

```
M-bM-^TM-^B Permission required$
M-bM-^TM-^BM-bM-^VM-3 Permission required$
M-bM-^TM-^B  M-bM-^FM-^R Edit$          <- title is the bare word "Edit"
M-bM-^TM-^B$                            <- detail row: border + EOL only
M-bM-^TM-^B Allow once   Allow always   Reject$
M-bM-^TM-^BM-bM-^GM-^F select  enter confirm  ctrl+f fullscreen$
```

**`ctrl+f` (fullscreen) makes it worse, not better.** The advertised escape hatch for
"I need to see more" renders an entirely empty panel — every row is the border glyph
alone, and even the `Edit` title is gone:

```
│
│
│      (20 rows, all empty)
│
│⇆ select  enter confirm  ctrl+f minimize
```

**The data exists — this is the view, not the permission layer.** The identical
`edit` permission request, observed on the HTTP event stream
(`GET /api/event`, `POST /api/session/{id}/prompt`, same project config, same mock):

```json
{
 "action": "edit",
 "id": "per_a23bd0cca3cc4a019e1fef05ba6b20bf",
 "metadata": {
  "arguments": {
   "filePath": "/tmp/opencode/f3w9/project/edit-target.txt",
   "intent": "f3 probe",
   "newString": "F3_EDITED",
   "oldString": "ORIGINAL"
  }
 },
 "resources": ["/tmp/opencode/f3w9/project/edit-target.txt"],
 "save": ["/tmp/opencode/f3w9/project/edit-target.txt"],
 "sessionID": "ses_aebc5bfb24f442d49dbf7df6d8c2f289"
}
```

`metadata.arguments.filePath` and `resources` both carry the path. The TUI is handed
the request and drops it.

**Why this matters more than a cosmetic gap.** The user is invited to press
**Allow always** on a file modification whose target they were never shown, which
persists a standing rule for an unseen path. `bash` had exactly this shape in wave 8
and was treated as a blocker. Severity is a shade lower than the wave-8 finding
because the dialog is now *answerable* (`Enter` works, so no deadlock), but the
approve-what-you-cannot-see problem is unchanged, and the fullscreen panel being
blank means there is no workaround from the keyboard.

Scope of my claim: reproduced on the `edit` tool at pane width 170 (so not
truncation), TERM=xterm-256color under tmux. I did not test every remaining tool —
only `bash`, `webfetch`, `edit` were in my mandate. Two of three now render; one does
not, so I would not assume the other tools are uniformly fine.

---

### F3-W9-O2 (observation) — two `serve` routes answer `not_implemented` citing todos that are all closed

While setting up the HTTP comparison above, `POST /session` and `GET /session`
(the un-prefixed paths) returned:

```json
{"error":{"code":"not_implemented",
 "hint":"the route is measured and registered; its backend lands in todos 57-62",
 "message":"`client.session.create` has no local backend in this build",
 "route":"POST /session","sdkMethod":"client.session.create",
 "callers":["@sunerpy/oh-my-openagent@4.21.0"]}}
```

The real routes are under `/api/` and work (`POST /api/session` created a session,
`POST /api/session/{id}/prompt` ran a full turn). So functionality is present. But
all 153 implementation todos are checked, and this message tells a user their build
is incomplete and points at work that is finished. Either the un-prefixed paths
should redirect/404 like any other unknown route, or the hint text is stale. I am
filing it as an observation, not a defect, because no working surface is affected —
it is a misleading error string on a path that is not the documented one.

---

### S3. F3-W8-D1 re-test — STILL AN UNDECLARED DIVERGENCE. Directory auto-discovery does not happen.

`docs/plugin-authoring.md:29` still says, unchanged:

> Beyond the config array, both `plugin/` and `plugins/` directories are scanned for
> `*.ts` and `*.js` in the global and project trees

Tested empirically. Two JS plugins that append a line to a log on `$load` and on the
`config` hook, one at `project/plugin/singular.js`, one at `project/plugins/plural.js`,
with **no** `plugin` key in `opencode.json`:

```
$ run --model test/test-model "F3W9 discovery test"
F3_ANSWER_OK
=== disco.log (expect BOTH if docs true) ===
cat: /tmp/opencode/f3w9/disco.log: No such file or directory
=== files present ===
project/plugin:  singular.js
project/plugins: plural.js
```

Neither loaded — the log file was never created, and at `--log-level DEBUG` the run
emitted **zero** lines matching `plugin`, so nothing was even attempted.

Global tree too: `$XDG_CONFIG_HOME/opencode/plugin/global.js` — same result, no log
file, zero plugin log lines.

**Control, which is what makes the above meaningful.** The identical file listed
explicitly in `config.plugin` as a `file://` URL loads and its hooks fire:

```
=== disco.log ===
{"from":"plugin-dir-singular","hook":"$load"}
{"from":"plugin-dir-singular","hook":"config"}
```

So the plugin, the JS runtime and the hook plumbing are all fine; only discovery is
absent. `discover_plugins` (`crates/oc-plugin/src/discovery.rs:83`) still has callers
only in `crates/oc-plugin/tests/discovery.rs` — no production caller.

`docs/divergences.md` has no entry mentioning discovery (grepped for `discover` —
no match). So this remains an **undeclared** documentation-vs-production gap, in the
same state as wave 8. A user who drops a plugin into `plugin/` because the authoring
guide told them to gets silence: no warning, no error, exit 0.

**Additional finding while establishing the control — F3-W9-O3: the accepted plugin
spec shapes do not match the doc either.** `docs/plugin-authoring.md:26-27` says
"A bare entry is an npm specifier, a `file://` URL, or a path". Four forms of the
same existing file:

```
spec=/tmp/opencode/f3w9/project/plugin/singular.js   loaded=0  Protocol plugin `/tmp/.../singular.js` failed
spec=./plugin/singular.js                            loaded=0  FailedToLoad ... is not a valid file URL or path
spec=plugin/singular.js                              loaded=0  FailedToLoad ... is an npm spec with no pinned …
spec=file:///tmp/.../plugin/singular.js              loaded=2  (works)
```

Only the `file://` URL works. An absolute path is routed to the *protocol* plugin
loader instead of the JS loader and fails there. A `./`-relative path is rejected
outright. A bare relative path is guessed to be an npm specifier. The doc's "or a
path" is not implemented for any of the three path spellings I tried. The failure
messages are at least honest and specific — nothing loads silently — but a user
following the documented syntax will not get a working plugin.

---

### S4. F3-W8-O1 re-test — STILL REPRODUCES. Internal title generation bypasses `chat.params` and `chat.headers`.

A plugin that sets `output.temperature = 0.42` in `chat.params` and adds
`x-f3-hook: fired` in `chat.headers`. One `run` turn, observed at the provider
(my mock's own log — this is the wire, not an inference):

```
req 32  temp=None  top_p=None  hdr={'authorization': 'Bearer sk-f3'}
        system: "You are a title generator. You output ONLY a thread title. N…"
req 33  temp=0.42  top_p=0.0   hdr={'authorization': 'Bearer sk-f3', 'x-f3-hook': 'fired'}
        system: ""
```

Request 32 is the internal title-generation call. It carries neither the plugin's
temperature nor the plugin's header. Request 33, the user-visible turn, carries both.
The plugin's own log confirms each hook fired exactly **once**, not twice:

```
{"hook":"$load"}
{"hook":"config"}
{"hook":"chat.params","agent":"build"}
{"hook":"chat.headers"}
```

Unchanged from wave 8. Whether it should be considered a defect depends on whether
upstream applies the hooks to internal agents; I have no upstream to compare against
here, so I am leaving it as an observation as before. The practical consequence is
that a plugin adding a required header (auth proxy, tenant routing, cost tagging)
would have it silently missing on the title request, which is a real provider call
that can be rejected or billed.

### S5. F3-W8-O2 re-test — HALF FIXED. Exit code corrected; the misleading message remains.

Config files with a valid but deeply nested `experimental` object, each confirmed
valid by a reference parser (`python3 -c "json.load(open(...))"`), run through
`models` from that directory:

```
depth=40   python=valid  exit=0  (no error)
depth=80   python=valid  exit=0  (no error)
depth=100  python=valid  exit=0  (no error)
depth=128  python=valid  exit=1  config file /tmp/opencode/f3w9/deep/opencode.json is not valid JSON
depth=130  python=valid  exit=1  config file /tmp/opencode/f3w9/deep/opencode.json is not valid JSON
depth=200  python=valid  exit=1  config file /tmp/opencode/f3w9/deep/opencode.json is not valid JSON
```

**Fixed:** the exit code. Wave 8 recorded `models` printing this error while exiting
**0**, so a script wrapping `opencode-rust models` saw success with empty output.
It now exits **1**, matching `run`. That half of the observation is resolved.

**Not fixed:** the wording. The file is valid JSON; what was exceeded is a parser
recursion limit (boundary between 100 and 128, consistent with serde_json's default
128). The user is told their file is malformed when it is not, and gets nothing
actionable — no mention of nesting depth, no limit value. Still an observation rather
than a defect: rejecting the input is correct behaviour, only the diagnosis is wrong.

### S6. F3-W8-O3 re-test — STILL REPRODUCES, and the recovery is narrower than I described in wave 8.

TUI with a completed turn on screen, pane 170x44, then `tmux resize-window` to
110x30. Non-blank row count from `capture-pane`:

```
before resize                          -> 6 non-blank rows
2.5 s after resize, no keystroke       -> 0
5.5 s after resize, no keystroke       -> 0
after Escape                           -> 0     <- a keystroke, still blank
after printable 'a'                    -> 6     <- restored
after Enter                            -> 10
after C-l                              -> 10
```

The screen goes **completely** blank on resize and stays blank indefinitely. New in
this wave: it is not "any keystroke" that recovers it. `Escape` — which is a no-op in
the idle state — left the pane blank. Only a key that actually changes editor state
(`a` into the composer) forced the repaint. So a user who resizes and then presses a
navigation or dismiss key sees a still-blank terminal and has good reason to think the
app has died. `SIGWINCH` is evidently not driving a redraw.

---

### S1 (cont). Gate — PASS, matches the expected numbers exactly

`cargo test --workspace --offline`, single run, no retry needed:

```
TOTAL PASSED: 3404
TOTAL FAILED: 0
TOTAL IGNORED: 2
"test result: FAILED" occurrences: 0
EXIT=0
```
No `EAGAIN` / `Resource temporarily unavailable` this run.

---

### S8. Todo 151 — plugin write-back truncation rejection: WORKS on every mutable hook, but the promised JSON Pointer path is not in the message

A plugin that mutates its `output` with an over-deep object, one hook per run
(`F3_TARGET` selects which). Depth 300 nested `{"n": …}`:

```
TARGET=chat.params    EXIT=1  plugin hook failed: plugin file:///…/deep.js failed in hook chat.params
TARGET=chat.message   EXIT=1  plugin file:///…/deep.js failed in hook chat.message
TARGET=config         EXIT=1  plugin file:///…/deep.js failed in hook config
TARGET=chat.headers   EXIT=1  plugin hook failed: plugin file:///…/deep.js failed in hook chat.headers
```
and the plugin's own log confirms the hook really ran before being rejected, e.g.
`{"hook":"$load","depth":300,"target":"config"}  {"hook":"config"}`.

**The core guarantee holds.** Four different mutable hooks — not just the auth
loader — all reject rather than truncate, the turn fails loudly with exit 1, and
nothing corrupted reaches the provider. That is the substance of todo 151 and it is
delivered.

**Control that the mechanism is depth-triggered and not "any mutation fails":**
depth 5 succeeds and the mutation reaches the wire, observed at the provider —
`keys=['f3deep','messages','model','stream','temperature','tools','top_k','top_p']`.

**Gap — F3-W9-O4: the error does not name the JSON Pointer path.** The stated
behaviour was "a clear error naming the plugin **and the JSON Pointer path**". The
message names the plugin URL and the hook. I could find no path anywhere: greping the
full `--log-level DEBUG --print-logs` output for `truncat`, `pointer`, `/options`,
`bounded`, `depth`, `f3deep` returned **nothing** — at DEBUG the single line is the
same short message. A plugin author with a large config object is told which hook
failed but not which field, and not that depth is the problem at all.

**Boundary — F3-W9-O5: the accepted depth is much shallower than the config parser's,
and unstated.** Bisecting `chat.params`:

```
depth=5   -> accepted (reached the wire)
depth=6   -> rejected
depth=7,8,9,10,20,28,30,32,40,60,120,126,130,300 -> rejected
```

The limit sits at 5-6 levels of nesting inside a hook's output value. The config file
parser tolerates ~127 (S5 above). Neither number is documented and the message
mentions neither, so a plugin returning a moderately structured object — six levels is
not exotic for a nested provider-options blob — gets a flat "failed in hook
chat.params" with no hint that it is a depth ceiling or what the ceiling is. Rejecting
is the right call; the diagnosis is the gap. Same shape as F3-W8-O2: correct
behaviour, unhelpful message.

---

### S9. Todo 152 — provider identity preserved through selection: PASS for all five

Five providers configured in one config, each pointed at a **different path** on one
multi-protocol mock (`127.0.0.1:43882`) with a **different API key**, so any collapse
of identity during selection shows up as the wrong path or the wrong credential.
The mock answers with a family-specific marker so the reply proves which branch ran.

`models` lists all five:

```
azure/mock-model
github-copilot/mock-model
groq/mock-model
mistral/mock-model
openrouter/mock-model
```

One `run` per provider — each returned its own family's marker:

```
openrouter      -> F3_OPENROUTER_OK
azure           -> F3_AZURE_OK
github-copilot  -> F3_COPILOT_OK
groq            -> F3_GROQ_OK
mistral         -> F3_MISTRAL_OK
```

Provider-side observation of the same runs (the mock's own log — path and credential
as they arrived on the wire):

```
openrouter  path=/openrouter/v1/chat/completions  authorization=Bearer sk-or-f3
azure       path=/azure/chat/completions          authorization=Bearer az-f3
azure       path=/azure/responses                 authorization=Bearer az-f3
copilot     path=/copilot/v1/chat/completions     authorization=Bearer gho-f3
groq        path=/groq/v1/chat/completions        authorization=Bearer gsk-f3
mistral     path=/mistral/v1/chat/completions     authorization=Bearer ms-f3
```

Every provider's own key went to its own endpoint. No cross-contamination, no
fallback to a shared generic path. The OpenRouter, Azure and GitHub Copilot profiles
and the `@ai-sdk/groq` / `@ai-sdk/mistral` transports are all reachable and distinct.
The Azure profile is visibly a *distinct* profile rather than an alias: it is the only
one of the five that used the **Responses** API (`/azure/responses`) for the main turn
while using `chat/completions` for the internal title call.

**F3-W9-O6 (observation, uncertain — cannot verify without real Azure).** The Azure
requests authenticated with `Authorization: Bearer az-f3` and hit
`{baseURL}/chat/completions` and `{baseURL}/responses` — no `api-key:` header, no
`/openai/deployments/{deployment}/` path segment, and no `api-version` query string,
even though I set `resourceName` and `apiVersion` in the provider options. Real Azure
OpenAI expects `api-key: <key>` for a plain key and the deployment path with
`api-version`. It is entirely possible that supplying an explicit `baseURL` is
*meant* to override the derived deployment URL, in which case this is correct. I have
no Azure credentials and would not use real ones here, so I am reporting the observed
shape and explicitly **not** claiming it is wrong.

### S10. Todo 153 — config differential fixture

Not a user-facing surface, and I touched config behaviour only through S5 and S8.
Nothing odd observed beyond what those two sections already record. Not independently
exercised.

---

### S7. The five hooks I could not exercise in wave 8 — three now exercised and proven authoritative, two still not reached

**`command.execute.before` — EXERCISED, and authoritative.**

The reason I missed it in wave 8: slash commands are **not** expanded on the `run`
surface. Sending `/f3cmd hello-args` as a message text passes the literal string
straight through to the provider, verified at the wire:

```
req 69 | last_user: '/f3cmd hello-args'
```
even though the command is present in the merged config
(`debug config` → `command tree: {"f3cmd": {"template": "echo the command template ran with $ARGUMENTS", …}}`).
The expansion path is the `run --command <name>` flag. With that:

```
$ run --command f3cmd "hello-args"
{"hook":"command.execute.before","input":["arguments","command","sessionID"],
 "output":["parts"],
 "inputJson":"{\"arguments\":\"hello-args\",\"command\":\"f3cmd\",\"sessionID\":\"ses_7a8c…\"}"}
```
and the expanded template reached the provider:
`req 71 | last_user: 'echo the command template ran with hello-args'`.

Authoritative, proven by mutation. The hook receives the fully-formed part:

```json
{"parts":[{"$ocTimeCreated":1786475870316,"$ocTimeUpdated":1786475870316,
 "id":"prt_b24ae…","messageID":"msg_af9b…","sessionID":"ses_9bda…",
 "text":"echo the command template ran with hello-args","type":"text"}]}
```
Rewriting `output.parts[0].text` changed what the provider actually received:

```
req 73 | last_user: 'F3_HOOK_REWROTE_THE_COMMAND'
req 74 | last_user: 'F3_HOOK_REWROTE_THE_COMMAND'
```

Good adjacent finding: replacing `output.parts` with a *bare* `[{type,text}]` — no
id, no messageID, no timestamps — is **rejected**, not silently accepted:

```
plugin file:///…/hooks5.js failed in hook command.execute.before
```
So the write-back validates the shape rather than letting a malformed part through.

**`experimental.session.compacting` — EXERCISED, and authoritative.**

Not reachable from the CLI (there is no `compact` subcommand; `session` has only
`list`/`prune`/`delete`). It is reachable over HTTP:
`POST /api/session/{id}/compact`. With too little history the route refuses honestly
rather than doing nothing:

```
HTTP=500 {"error":{"code":"mutation_failed","message":"manual compaction failed:
 Reason(\"NoCompactableHistory: session has no compactable history before the preserved tail\")"}}
```

After six turns in the same session, `HTTP=204` and the hook fires:

```
{"hook":"compacting","before":"{\"context\":[],\"prompt\":null}"}
{"hook":"compacting","after":"{\"context\":[],\"prompt\":\"F3_COMPACT_PROMPT_FROM_HOOK\"}"}
```
and the plugin's prompt is what the provider was actually asked to summarise with:

```
req 22 | user: F3_COMPACT_PROMPT_FROM_HOOK
```

**`dispose` beyond shutdown — EXERCISED (as shutdown only).** `dispose` fired on
every run I made this wave, including runs that ended in a *failure*
(`plugin hook failed …`, exit 1) and the `serve` process's per-turn plugin runtime.
So it is reached on the error path and not only on a clean exit. I found no lifecycle
event other than runtime teardown that invokes it, and I do not know whether one is
intended to exist, so "beyond shutdown" remains unverified rather than absent.

**`experimental.compaction.autocontinue` — STILL NOT REACHED.** Documented as the
"overflow decision before automatic compaction" (`docs/plugin-authoring.md:179`).
I could not construct an automatic overflow:

- `limit: {"context": 400, "output": 100}` on the model, then a single ~13 KB message:
  no hook, exit 0.
- Six accumulating turns of ~13 KB each in one session via `run --continue`. History
  really did accumulate — the provider saw `n_msgs` grow 2 → 4 → 6 → 8 → 10 → 12 —
  and still no hook.
- Same again with the mock reporting `usage.prompt_tokens: 380` against a declared
  400-token context, so the runtime had a real token count near the ceiling: no hook.
- `--log-level DEBUG --print-logs` greped for `compact|token|usage|context|limit|overflow`
  produced **no** output at all on those runs.

Manual compaction works (above), so the compaction machinery exists; what I could not
trigger is the *automatic* overflow decision. I cannot say whether the trigger needs a
condition I did not find or is not wired. Stating it plainly rather than inferring.

**`tool` beyond registration — PARTIALLY EXERCISED. Registration and advertisement
work; execution fails with a message that says nothing.**

I registered a plugin tool `f3_probe_tool` and scripted my mock to call it by name.

Registration and advertisement are confirmed at the wire — the plugin's tool is in
the provider's tool list alongside the built-ins:

```
req 2 tools= ['invalid','bash','read','glob','grep','edit','write','webfetch',
              'todowrite','f3_probe_tool','memory']
```

Dispatch is attempted, and fails:

```
[f3_probe_tool] started
[f3_probe_tool] failed: f3_probe_tool error
```

The plugin's `execute` never ran — its log has only `{"hook":"$load"}` and
`{"hook":"dispose"}`, no `tool.execute` line. The turn then burned its whole
100-step budget retrying, ending with
`agent \`build\` exhausted its 100-step turn budget`.

I tried two plausible definition shapes: `args: { note: { type: "string", … } }`, and
a full JSON Schema `parameters: { type: "object", properties: {…}, required: […] }`.
Both registered, both advertised, both failed identically at execute.
`docs/plugin-authoring.md:163` lists the `tool` hook as "executable tool-registry
assembly" but gives no example of the expected object shape, so **I cannot tell
whether my definition was malformed or the execution path is broken** — and that is
itself the finding worth recording:

**F3-W9-O7 (observation): `[f3_probe_tool] failed: f3_probe_tool error` is a
zero-information error.** It repeats the tool name twice and adds the word "error".
At `--log-level DEBUG --print-logs` there is nothing more — greping the full output
for `oc_plugin`, `oc_tools`, `dispatch`, `invoke` returned nothing. Compare this with
the quality of the rest of the product's diagnostics (the bad-model message names
three fixes and a cache path; the plugin spec failures say exactly which form was
rejected and why). A plugin author whose tool fails here has no way forward, and the
failure costs a full 100-step budget rather than aborting.

So: `tool` is proven to reach registration and advertisement, and proven to reach
*dispatch*. A plugin tool's `execute` actually running remains **unverified**.

---

## Exploration (started only after the mandatory sections above were written)

### S11. CLI failure modes and empty state — PASS

Each run separately so `$?` is the product's, not a pipeline's. No config present
unless noted:

```
models                exit=0  (empty output)
session list          exit=0  Session ID  Project  Title  Agent  Last activity …  (header, no rows)
debug paths           exit=0  home       /tmp/opencode/f3w9/home
mcp list              exit=0  MCP Servers
stats                 exit=1  `stats` is not available: upstream stats reads the excluded stats package's session SQL directly; use `db stats` …
console               exit=1  `console` is not available: the hosted OpenCode Console is excluded from this Rust port's local-agent scope; …
export (no arg)       exit=1  session selection is interactive upstream; pass the session id, which `session list` prints
run (no message)      exit=1  a message is required
run --nope hi         exit=2  error: unexpected argument '--nope' found
frobnicate            exit=2  error: unrecognized subcommand 'frobnicate'
```

Config abuse:

```
{ "provider":                      exit=1  config file …/opencode.json is not valid JSON
not json at all                    exit=1  config file …/opencode.json is not valid JSON
{"permission":{"bash":"nonsense"}} exit=1  config file …/opencode.json failed validation (1 issue(s))
                                             permission.bash: data did not match any variant of untagged enum PermissionRule at line 1 column 39
{"provider":{"x":{"npm":123}}}     exit=1  … failed validation (1 issue(s))
run --model x/nope hi              exit=1  model `x/nope` is not available: no `provider` block in your configuration defines it, OPENCODE_DISABLE_MODELS_FETCH is set so no fetch … was attempted, and no cached catalog exists at …
```

Still the strongest part of the product. The 2-vs-1 exit split (usage error vs runtime
failure) is conventional and consistent. Schema validation names the exact key, the
reason, and the line/column. `session list` on empty state prints its header and exits
0, which is defensible. Nothing bluffs success.

### S12. Server, SSE, concurrent clients — PASS

Three concurrent bounded observers on `GET /api/event` while one turn ran:

```
observer1: bytes=2915 events=13
observer2: bytes=2915 events=13
observer3: bytes=2915 events=13
```

Byte-identical, and the event multiset agrees exactly across all three:

```
{'agent.resolved':1, 'assistant.checkpointed':1, 'assistant.message.created':1,
 'model.resolved':1, 'provider':3, 'provider.request.started':1,
 'server.connected':1, 'step.completed':1, 'tool.snapshot.locked':1,
 'turn.completed':1, 'turn.started':1}
```

No fan-out starvation, no per-observer divergence. (All SSE reads bounded with
`--max-time`; nothing left dangling.)

### DEFECT F3-W9-D2 — a provider that stalls mid-stream hangs the CLI forever, with no output and no error

Two variants of an interrupted provider stream, each a purpose-built socket server
that emits one partial SSE chunk (`content: "PARTIAL_"`) and then stops.

**Variant A — connection closed mid-stream (real FIN). Handled correctly:**

```
EXIT=1  elapsed=1s
PARTIAL_
transient provider failure (status=None): error decoding response body:
  error reading a body from connection: unexpected EOF during chunked size line
```
One second, exit 1, the partial text it did receive is shown, and the error names the
exact wire-level cause. This is the behaviour I would want.

**Variant B — socket held OPEN, no further bytes, no FIN. Hangs indefinitely:**

```
EXIT=124  elapsed=200s
(no output at all)
```

`124` is my `timeout 200` killing it. In 200 seconds the product produced **zero
bytes** — not the `PARTIAL_` text it had already received, not a warning, not a
progress indicator. There is no read/idle timeout. I first hit this accidentally at
a 90 s bound and confirmed it deliberately at 200 s.

It is also not interruptible in the obvious way: with the run in a tmux pane, `Ctrl-C`
after 8 s did not terminate it — the process was still alive afterwards and I had to
`SIGKILL` it.

Why this is a realistic condition rather than a contrived one: a stalled TCP
connection with no FIN is exactly what a dropped VPN, a silently-dead load balancer,
a suspended laptop, or an overloaded proxy produces. It is the *normal* network
failure mode, and arguably more common than a clean close. The user sees a hung
terminal with no diagnostic and must discover `kill -9`.

Distinguishing this from a rig artifact: variant A proves the product's stream error
handling works and my harness is sound; the only difference in variant B is the
absent FIN. So the gap is specifically the missing idle timeout, not stream handling
in general.

Scope of my claim: measured on the `run` surface with an `@ai-sdk/openai-compatible`
provider. I did not test whether `serve` or the TUI behave the same way under a
stalled provider, so I am not claiming it for those surfaces.


---

## Summary

| # | Scenario | Result |
|---|---|---|
| S1 | `cargo build --offline`, artifact runs, `--help` lists 22 subcommands | PASS |
| S1 | gate: 3404 passed / 0 failed / 2 ignored, no EAGAIN | PASS |
| S2 | SEAM #20 — `bash` dialog shows the command, `Enter` allows, `Down Down`+`Enter` rejects, tool honours both | **FIXED** |
| S2 | SEAM #20 — `webfetch` dialog shows the URL, answerable, tool runs | **FIXED** |
| S2 | SEAM #20 — `edit` dialog shows nothing; fullscreen entirely blank | **DEFECT F3-W9-D1** |
| S3 | `plugin/` + `plugins/` auto-discovery promised by docs | still absent, still undeclared |
| S4 | title generation bypasses `chat.params` / `chat.headers` | still reproduces (O1) |
| S5 | deep valid JSON called "not valid JSON" | message unchanged; **exit code fixed** to 1 |
| S6 | TUI resize repaint | still reproduces, recovery narrower than I thought |
| S7 | `command.execute.before` | exercised, authoritative |
| S7 | `experimental.session.compacting` | exercised, authoritative |
| S7 | `dispose` | fires on success, failure and `serve`; "beyond shutdown" unverified |
| S7 | `experimental.compaction.autocontinue` | NOT reached despite 4 attempts |
| S7 | `tool` beyond registration | registers + advertises + dispatches; `execute` never runs |
| S8 | todo 151 — truncation rejected on 4 mutable hooks, exit 1, nothing corrupt on the wire | PASS |
| S8 | todo 151 — error names plugin + hook but **no JSON Pointer path**; depth ceiling ~5 | O4, O5 |
| S9 | todo 152 — 5 providers, distinct paths + distinct keys, no cross-contamination | PASS |
| S10 | todo 153 | not independently exercised |
| S11 | CLI failure modes, exit codes, schema validation, empty state | PASS |
| S12 | 3 concurrent SSE observers byte-identical | PASS |
| S12 | provider closes mid-stream (FIN) | PASS — 1 s, exit 1, precise error |
| S12 | provider stalls mid-stream (no FIN) | **DEFECT F3-W9-D2** — 200 s, no output, not Ctrl-C-able |

### Defects

- **F3-W9-D1 (needs a decision)** — the `edit` permission dialog shows neither the
  file path nor a diff, and `ctrl+f` fullscreen renders a completely empty panel. The
  data is present on the HTTP event stream (`metadata.arguments.filePath`,
  `resources`), so this is the TUI view. This is SEAM #20 unfixed on the `edit` tool
  after being fixed on `bash` and `webfetch`. Lower severity than the wave-8 blocker
  because the dialog is answerable, but the user is still asked to press **Allow
  always** on a file change they were never shown.
- **F3-W9-D2 (blocker-adjacent)** — a provider that stalls mid-stream without closing
  the socket hangs `run` indefinitely: 200 s measured with zero output, no error, no
  partial text, and `Ctrl-C` did not terminate it. The clean-close variant is handled
  correctly in 1 s with an exact error, which isolates the gap to a missing idle
  timeout. This is the ordinary network failure mode (dead LB, dropped VPN, suspended
  laptop), and the only escape is `kill -9`.

### Observations

- **F3-W9-O1** — permission dialog footer advertises `⇆ select`, but only `Up`/`Down`
  move the selection; `Left`/`Right`/`Tab`/`BTab`/`j`/`h`/`l`/`Space` do nothing on a
  horizontal button row.
- **F3-W9-O2** — un-prefixed `POST /session` / `GET /session` answer `not_implemented`
  with a hint pointing at "todos 57-62", all of which are closed. The real `/api/`
  routes work.
- **F3-W9-O3** — `docs/plugin-authoring.md` says a plugin entry may be "a path", but
  only a `file://` URL loads; absolute path, `./`-relative and bare relative all fail
  (each with an honest, distinct message).
- **F3-W9-O4** — plugin write-back rejection names the plugin and hook but no JSON
  Pointer path, at any log level.
- **F3-W9-O5** — that rejection triggers at ~6 levels of nesting, versus ~127 for the
  config file parser; neither limit is documented or mentioned in the message.
- **F3-W9-O6** — Azure requests used `Authorization: Bearer` and `{baseURL}/chat/completions`
  / `{baseURL}/responses` with no `api-key:` header, deployment path or `api-version`,
  despite `resourceName`/`apiVersion` being set. May be correct when `baseURL` is
  explicit; **not claiming it is wrong** — no Azure credentials to check against.
- **F3-W9-O7** — `[f3_probe_tool] failed: f3_probe_tool error` carries no information
  at any log level, and the failure consumes the entire 100-step turn budget.

### Carried forward from wave 8, still open

- **F3-W8-D1** — `plugin/`/`plugins/` auto-discovery documented, not implemented,
  not declared in `docs/divergences.md`. `discover_plugins` still has test-only callers.
- **F3-W8-O1** — internal title generation bypasses `chat.params` / `chat.headers`.
- **F3-W8-O2 (half)** — "not valid JSON" wording for valid-but-deep JSON. Exit code
  half is fixed.
- **F3-W8-O3** — TUI blank after resize until a state-changing keystroke; `Escape` is
  not enough.

### What I could NOT verify, and why

- **`experimental.compaction.autocontinue`** — could not construct an automatic
  context overflow. Tried a 400-token declared context with a 13 KB message; six
  accumulating turns (provider saw `n_msgs` 2→12); and a mock reporting
  `usage.prompt_tokens: 380` against that 400 limit. No hook, and nothing in DEBUG
  logs matching `compact|token|usage|context|limit|overflow`. Manual compaction works,
  so the machinery exists; the automatic trigger I could not reach.
- **A plugin tool's `execute` body running** — two definition shapes both failed at
  dispatch with an opaque error, and the docs give no shape example, so I cannot
  separate my error from the product's.
- **`dispose` on any lifecycle event other than teardown** — I saw it on clean exit,
  on failed runs, and under `serve`; I do not know whether another trigger is intended.
- **Provider families against live cloud endpoints** — no credentials, and I would not
  use real ones here. All five in S9 were proven against a local mock. The Azure
  request shape (O6) specifically needs a real endpoint to judge.
- **F3-W9-D1 / O1 / F3-W8-O3 outside tmux** — no physical tty available in this
  environment. tmux with `TERM=xterm-256color` at 170x44 is an ordinary user setup, so
  I consider the findings valid, but a terminal-capability dependency cannot be ruled
  out for the key-handling and repaint items.
- **Todo 153** — not independently exercised (not a user-facing surface).
- **F3-W9-D2 on the `serve` and TUI surfaces** — measured on `run` only.

### Cleanup

All tmux sessions killed (`tmux ls` → `no server running`), all mock providers
(`mock.py`, `multi.py`, `cut2.py`, `stall.py`), the `serve` instances and every
`opencode-rust` process stopped and confirmed gone via `pgrep`. All scratch under
`/tmp/opencode/f3w9/`. No product source, test, plan, doc or other evidence file was
modified — this report is the only file I wrote.
