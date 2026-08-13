# F3 — Real Manual QA — ROUND 2 (delta-only)

- **Audited HEAD: `647a2d64d2b34a602f59b0189e613d957b40882b`**
- Worktree: `/config/workspace/ProdDir/AI/oc-wt/tF3`, branch `task-F3`
- Round: **2** of the Final Verification Wave (hard cap 7). Governed by
  `## Final Verification Wave — convergence protocol` in `.omo/plans/opencode-rust.md`.
- Scope: **delta-only.** One closed question per frozen ledger entry — "is this Blocker
  closed: yes / no". No hunting for new defects. A new Blocker is admissible only for a
  regression directly introduced by one of the six fixes.

## Verdict

# APPROVE

All six frozen ledger entries closed; no admissible new Blocker. Round converges from my
side per iron rule 6. Gate: **3473 passed / 0 failed / 2 ignored**. Four non-blocking
Follow-ups recorded. Full detail and the verdict rationale are at the end of this report.

## Ledger answers (summary — filled in as each is exercised)

| # | Entry | Closed? | How it was proven |
|---|-------|---------|-------------------|
| 1 | v1 plugin SDK routes returned 501 (todos 175/169/171) | **YES** | credential persisted to disk and read by a second process; plugin `authorize()`/`callback()` really invoked and their tokens stored; `slug` present; `tool_result` 200 with the body's model dialed; summarize dialed the provider (delta=1) |
| 2 | F3-W13-07 `tool.definition` unusable by any JS plugin (todo 172) | **YES** | four-line no-op runs clean; hook invoked for all 10 definitions and survives past `todowrite`; mutation reaches the provider; genuine plugin truncation still caught, correctly and unforgeably attributed |
| 3 | F3-W13-01 `auth.loader` failure kills `run`/`models`/HTTP (todo 173) | **YES** | all three surfaces work on my exact trigger; loader provably invoked 0× with no credential and handed a real `Auth` with one; a throwing loader disables only the plugin |
| 4 | F2-B7 version gate loads incompatibles (todo 174) | **YES** | excluding + invalid ranges never evaluate the module body; satisfying + absent both reach factory; documented message verbatim with range and running version |
| 5 | F3-W13-05 top-level config `model` ignored (todo 177) | **YES** | both directions on both `run` and the TUI; full precedence chain flag > agent > config > catalog-first |
| 6 | F4 w13 `PluginInput.client` unprojected (todo 176) | **YES** | `provider.list()` returns real `{all, default, connected}`; `session.create` has a real effect; epoch-ms round-trips correctly |
| — | todo 178 oracle re-pin to 1.18.18 | **YES** | host `opencode --version` = 1.18.18, `PINNED_RELEASE = "1.18.18"`, and `the_declared_pin_equals_the_version_the_resolved_binary_reports` passes |

**No admissible new Blocker.** Two non-blocking Follow-ups found while exercising the six
(port-0 `serverUrl` on the turn-scoped plugin instance; `tool_use_id` still dropped), plus
one pre-existing flaky timing test. None is a regression I can attribute to one of the six
fixes, so per iron rules 4 and 5 none blocks.

---

## Scenario log

### Gate — run 1 hit one load-induced timing flake; deferred full re-run to the end

`cargo test --workspace --offline`, run 1:

```
transport::remote_timeout_bounds_both_transport_attempts ... FAILED
thread '…' panicked at crates/oc-mcp/tests/remote/transport.rs:150:5:
assertion failed: started.elapsed() < Duration::from_millis(500)
test result: FAILED. 7 passed; 1 failed
```

`cargo test` is fail-fast across targets, so the run stopped there: **1565 passed / 1
failed**, not a full sweep.

This is a wall-clock assertion, and the host was under concurrent load — `ps` shows
**another reviewer's** `cargo test --workspace` started at 08:15 while mine was still
going (F1/F2/F4 audit this same HEAD concurrently, as my prompt states). Isolated retry:

```
$ cargo test --offline -p oc-mcp --test remote transport::remote_timeout_bounds_both_transport_attempts
test transport::remote_timeout_bounds_both_transport_attempts ... ok
test result: ok. 1 passed; 0 failed; 7 filtered out; finished in 0.13s
```

0.13 s against a 500 ms bound — a ~4× margin, so the failure was contention, not a real
regression. **Both runs recorded**, per my instructions. Full-count re-run deferred to
the end of QA: a second concurrent workspace sweep would contend with the 30 s plugin
connect-back timeout my manual scenarios depend on and could manufacture false
`FailedToLoad` results.

Follow-up (non-blocking, pre-existing, not caused by any of the six fixes): this test
asserts on wall-clock elapsed time with no slack for loaded CI, so it will flake again.

### Entry 2 (F3-W13-07) — `tool.definition` unusable by any JS plugin — **CLOSED**

Environment: isolated `XDG_CONFIG_HOME=/tmp/opencode/f3r2/noop/config`, one fake
OpenAI-compatible provider (`aaatool/tool-model`, python SSE server on :47401, `tool`
mode → emits a tool call then text), real `bun` on `PATH`
(`/config/.local/share/mise/installs/bun/1.3.14/bin`). No `--pure`, no `--print-logs`.

**Test 1 — the literal four-line no-op from my wave-13 report:**

```js
export const NoOp = async () => ({
  "tool.definition": async () => {},
});
```

```
$ opencode-rust run --model aaatool/tool-model "hello"
EXIT=0
--- stdout ---
DONE-AFTER-TOOL
--- stderr ---
(empty)
```

Wave 13 produced, on this identical plugin, a `disabled plugin … after hook
`tool.definition` failed … truncated … at /parameters/properties/todos/items/properties/priority/oneOf/0`
on stderr. That diagnostic is **gone**, and stderr is now completely empty.

**Test 2 — proving the plugin actually loaded and stayed enabled** (empty stderr alone
could mean the plugin silently never loaded, so this is the load-bearing check). Same
plugin, hook body now records each invocation:

```
--- hook invocations (10 lines) ---
{"toolID":"invalid","hasParams":true,"oneOfPriority":null}
{"toolID":"bash","hasParams":true,"oneOfPriority":null}
{"toolID":"read","hasParams":true,"oneOfPriority":null}
{"toolID":"glob","hasParams":true,"oneOfPriority":null}
{"toolID":"grep","hasParams":true,"oneOfPriority":null}
{"toolID":"edit","hasParams":true,"oneOfPriority":null}
{"toolID":"write","hasParams":true,"oneOfPriority":null}
{"toolID":"webfetch","hasParams":true,"oneOfPriority":null}
{"toolID":"todowrite","hasParams":true,"oneOfPriority":3}
{"toolID":"memory","hasParams":true,"oneOfPriority":null}
```

Three independent confirmations in that output:

1. The hook **ran** — 10 invocations, one per tool definition, so the plugin genuinely
   loaded and registered hook 21.
2. `todowrite` — the exact tool that killed the plugin in wave 13 — is reached, and the
   `oneOf` beneath `items` arrives **fully populated (3 entries)**. This is the pointer
   the host used to blame the plugin.
3. **The plugin survived past the failure point.** `memory` is invoked *after*
   `todowrite`. In wave 13 the plugin was disabled at `todowrite`, so no later definition
   was ever handed to it. Reaching tool 10 is positive proof the plugin was not disabled.

My wave-13 conclusion is **vindicated**: `@sunerpy/oh-my-openagent@4.21.0` was never at
fault. The failure was the host's own encoder truncating a built-in schema at
`MAX_DEPTH = 8` and attributing the loss to the plugin. With `MAX_DEPTH = 16` the
built-in `todowrite` schema survives the round trip.

**Test 3 — is the mutation actually applied, or merely not rejected?** A hook that runs
but whose result is discarded would be a hollow fix. Plugin rewrites `read`'s description
to a unique marker; the fake provider logs its full request body:

```
$ grep -o '.\{80\}F3R2-MUTATION-MARKER-9134.\{40\}' fake/tool-47401.log
…,"type":"function"},{"function":{"description":"F3R2-MUTATION-MARKER-9134","name":"read","parameters":{"additiona…
```

The mutation reaches the provider, bound to the right tool. Hook 21 works end to end.

**Test 4 — did the fix simply delete the truncation detector, and can a plugin forge
host attribution?** Hostile plugin: deletes a `todowrite` subtree, injects
`"$source": "host"` at two levels to forge host attribution, and nests `read`'s schema
40 levels deep to provoke a real loss.

```
$ opencode-rust run --model aaatool/tool-model "hello"
EXIT=0
--- stdout ---
DONE-AFTER-TOOL
--- stderr ---
disabled plugin `file:///tmp/opencode/f3r2/noop/config/opencode/plugin/noop.js` after hook
`tool.definition` failed: plugin `file:///…/noop.js` truncated `tool.definition` hook argument 1 at
`/parameters/properties/deep/child/child/child/child/child/child/child/child/child/child/child/child/properties`;
refusing to apply any hook mutation
```

This is the behaviour I wanted to see:

- The detector **still exists** — the fix raised the depth and fixed attribution, it did
  not remove the safety check.
- Attribution is now **correct**: the named pointer is the plugin's *own* 40-level
  `deep/child/…` structure, not a built-in schema. This time the loss really is the
  plugin's fault, and the message says so truthfully.
- The forged `$source: "host"` **did not** suppress the diagnostic or flip the blame, so
  a plugin cannot launder its own truncation as a host fault.
- Blast radius remains one plugin; the turn still completes, exit 0, `DONE-AFTER-TOOL`.
- The pointer bottoms out after 12 `child` levels, consistent with `MAX_DEPTH = 16`.

A deliberate property deletion by itself (Test 4's `todowrite` half, and a separate
delete-only run) draws no diagnostic and is applied. That is correct: mutating a tool
definition is the hook's declared purpose, so a deliberate edit is not an error.

**Answer: closed — yes.** Four ways: the no-op runs clean, the hook is invoked for all 10
definitions and survives past `todowrite`, mutations reach the provider, and genuine
plugin-side loss is still caught and now blamed correctly and unforgeably.


### Entry 3 (F3-W13-01) — `auth.loader` failure kills `run`, `models` and HTTP turns — **CLOSED**

Two directions had to hold: my original trigger must no longer reach the loader, **and** a
loader that genuinely fails must be isolated rather than fatal. Both do.

#### Direction 1 — my exact wave-13 trigger: provider configured, no stored credential

Environment: the **real** `/config/.config/opencode/opencode.json` (JSONC comments
stripped) with its real plugin list intact —
`opencode-antigravity-auth@1.6.0`, `@sunerpy/opencode-kiro-auth@0.20.6`,
`@sunerpy/oh-my-openagent@4.21.0` — real package cache at `/config/.cache/opencode` so
they resolve exactly as on this machine, one fake provider added, and **`auth.json` = `{}`**
(no `google` credential — the wave-13 trigger).

All three surfaces, which wave 13 found dead:

| surface | wave 13 | round 2 |
|---|---|---|
| `run` | exit **1**, no stdout, `plugin auth loader `google` failed: … null is not an object (evaluating 'auth.type')`, 1.01 s | **exit 0**, `DONE-AFTER-TOOL`, **stderr empty**, 2 s |
| `models` | exit **1**, **0 lines** | **exit 0**, **397 lines**, stderr empty |
| HTTP turn | admitted 200 then `session.error` ×1, no assistant message, 0 persisted messages | **full turn**, 0 `session.error`, 2 persisted messages |

```
$ opencode-rust run --model aaatool/tool-model "read the file"
EXIT=0 WALL=2s
--- stdout ---
DONE-AFTER-TOOL
--- stderr ---
(empty)

$ opencode-rust models
EXIT=0   stdout lines: 397   stderr: (empty)
aaatool/tool-model
google/antigravity-claude-opus-4-5-thinking
google/antigravity-claude-opus-4-6-thinking
google/antigravity-claude-sonnet-4-5
…
```

Note the `google/*` models are **present**, so the antigravity plugin loaded and
contributed its catalog — it simply never had its loader invoked. The plugin is working,
not bypassed.

HTTP surface (`serve --port 47501`, bounded SSE `curl -sN --max-time 35`):

```
=== SSE census ===
      3 "type":"provider"
      1 "type":"turn.started"        1 "type":"turn.completed"
      1 "type":"assistant.message.created"
      1 "type":"text.delta"          1 "type":"message.end"
      1 "type":"model.resolved"      1 "type":"agent.resolved"
      1 "type":"step.completed"      1 "type":"assistant.checkpointed"
      1 "type":"tool.snapshot.locked"  1 "type":"status.detail"
      1 "type":"server.connected"

=== session.error count === 0
=== "auth loader" occurrences in server log === 0
=== persisted messages === count: 2  (role=assistant, role=user)
```

Wave 13's census over the same window was `server.connected ×1` + `session.error ×1`,
with no assistant message and `GET /message` returning `{"data":[]}` after two admitted
prompts. Now the full turn lifecycle runs and both messages persist.

#### Direction 2 — the mechanism, measured: is the loader really *skipped*?

The fix's premise is that a configured provider with no stored credential skips
`auth.loader` entirely (upstream `provider.ts:1548-1563`). I instrumented a loader to
record every invocation and what `auth()` resolves to, and flipped only `auth.json`:

```
===== CASE A: credential STORED for badprov =====
EXIT=0
loader calls: 1
LOADER CALLED, auth() => {"key":"stored-key-so-loader-runs","type":"api"}

===== CASE B: NO credential for badprov =====
EXIT=0
loader calls: 0
stderr B: (empty)
```

This settles the root cause I reported in wave 13 at its source:

- **Case B: zero invocations.** The loader is genuinely skipped, not called-and-tolerated.
  My trigger no longer reaches it.
- **Case A: `auth()` resolves to a real `Auth` object**, `{"key":…,"type":"api"}` — not
  `null`. The SDK contract violation I quoted from
  `dist/index.d.ts:23` (`loader?: (auth: () => Promise<Auth>, …)`, i.e. `Promise<Auth>`
  and not `Promise<Auth | undefined>`) is satisfied. A plugin author who dereferences
  `auth.type`, as the antigravity plugin does, is now safe.

#### Direction 3 — a genuinely failing loader is isolated, not fatal

The fix must not be merely "never call the loader". Plugin whose loader throws
unconditionally, **with** a credential stored so it is definitely invoked:

```js
export const BadAuth = async () => ({
  auth: { provider: "badprov", methods: [],
    loader: async () => { throw new Error("F3R2-DELIBERATE-LOADER-FAILURE"); } },
});
```

```
########## SURFACE 1: run ##########
EXIT=0
--stdout--  DONE-AFTER-TOOL
--stderr--  disabled plugin `file:///…/badauth.js` after hook `auth.loader` failed:
            plugin `file:///…/badauth.js` failed `call`: F3R2-DELIBERATE-LOADER-FAILURE

########## SURFACE 2: models ##########
EXIT=0  lines=200
--stderr--  (same diagnostic)
```

HTTP surface, same failing loader:

```
=== SSE census ===  turn.started ×1, turn.completed ×1, assistant.message.created ×1,
                    text.delta ×1, message.end ×1, error ×1, provider ×4, …
=== session.error count === 0
=== persisted messages === count: 2
```

The one `error` event is the diagnostic being delivered to the HTTP client, nested in a
`provider` event — so a remote client is told, without the turn dying:

```
data: {"data":{"event":{"message":"disabled plugin `file:///…/badauth.js` after hook `auth.loader`
  failed: plugin `file:///…/badauth.js` failed `call`: F3R2-DELIBERATE-LOADER-FAILURE",
  "retryAfterMs":null,"type":"error"},"step":0},…,"type":"provider"}
```

This is exactly what `docs/plugin-authoring.md:88` promises — "disabled with a
`PluginDiagnostic` **rather than taking the turn down**" — and it is now true on the
auth-loader path, which todo 168 had not covered. The diagnostic names the plugin, the
hook (`auth.loader`), and the real underlying error, and is visible at default verbosity
on stderr without `--print-logs`.

**Answer: closed — yes.** All three surfaces verified working in both the skip case and
the genuine-failure case, with the `null`-vs-`Promise<Auth>` root cause fixed at source.

### Entry 5 (F3-W13-05) — top-level config `model` key parsed, echoed, then ignored — **CLOSED**

Re-run of my wave-13 physical-discriminator test, **in both directions on both `run` and
the TUI**, as instructed. Two fake providers with distinguishable output — the tool server
on :47401 prints `DONE-AFTER-TOOL`, the slow server on :47402 prints `tick0 … tick5` —
named so that the **configured** provider is *not* the catalog-first one. AWS env vars
cleared so no env-detected provider sorts ahead.

| dir | config `model` | catalog-first (`models` head) | `run` stdout | model actually used | TUI stdout | model actually used |
|---|---|---|---|---|---|---|
| 1 | `zzfaketool/tool-model` | `aaaslow/slow-model` | `DONE-AFTER-TOOL` | **`zzfaketool`** = configured | `DONE-AFTER-TOOL` | **`zzfaketool`** = configured |
| 2 | `zzslow/slow-model` | `aaatool/tool-model` | `tick0 tick1 tick2 tick3 tick4 tick5` | **`zzslow`** = configured | `tick0 tick1 tick2 tick3 tick4 tick5` | **`zzslow`** = configured |

In wave 13 these same two rows produced the **opposite** result — `aaaslow` and `aaatool`,
i.e. the catalog's first entry — against opposite configured values. Now the configured
value wins in both directions, so this is the config key taking effect and not a
coincidence of catalog ordering. This is why both directions matter: a single-direction
test cannot distinguish "chose correctly" from "chose the only option".

Direction 1, `run` (catalog head confirms `aaaslow` is first, so the config value is the
*last* provider alphabetically and cannot win by ordering):

```
=== catalog order ===
aaaslow/slow-model
cloudflare-ai-gateway/anthropic/claude-3-5-haiku
…
=== debug config model key ===
zzfaketool/tool-model
=== run with NO --model ===
EXIT=0
DONE-AFTER-TOOL
```

TUI, direction 2 (`opencode-rust` with no flags, prompt typed into the real TUI in tmux):

```
> You
  read the file

* Assistant
  tick0 tick1 tick2 tick3 tick4 tick5

 idle
```

TUI, direction 1 (config flipped, TUI restarted):

```
> You
  read the file

* Assistant
  DONE-AFTER-TOOL

 idle
```

#### Precedence was inserted, not inverted

Todo 177 placed the config key *between* the agent's model and the catalog fallback. A fix
that overshot — config beating the agent or the `--model` flag — would be a regression
directly caused by this fix, so I measured the whole chain. Config `model` =
`zzfaketool` (`DONE-AFTER-TOOL`); agent `slowagent` pinned to `aaaslow/slow-model`
(`tick0 …`):

```
=== A) --agent slowagent : agent should WIN over config ===
EXIT=0   tick0 tick1 tick2 tick3 tick4 tick5      → agent wins over config ✓
=== B) --model aaaslow/slow-model : flag should WIN over config ===
EXIT=0   tick0 tick1 tick2 tick3 tick4 tick5      → flag wins over config ✓
=== C) no flags : config should WIN over catalog-first ===
EXIT=0   DONE-AFTER-TOOL                           → config wins over catalog ✓
```

Resolution order measured as `--model` flag > agent model > config `model` > catalog-first
— exactly the documented order, with the config key in the slot todo 177 claims. No
inversion, no regression.

**Answer: closed — yes.** Both directions, both surfaces, plus the full precedence chain.

### Entry 4 (F2-B7) — version gate reads the wrong field and loads incompatibles — **CLOSED**

I corroborated this in wave 12 and re-found the user-visible half as F3-W13-02
("loaded-with-a-warning, not skipped"). The contract is
`docs/plugin-authoring.md:37-45`: the gate checks `package.json.engines.opencode`, "an
excluding or invalid range **skips** the plugin and reports `Plugin requires opencode
<range> but running <version>`, while a satisfying or absent range loads normally", and
local `file:` plugins bypass the gate.

`file:` plugins bypass the gate, so this needs **npm-style** packages. I hand-built four
under an isolated `XDG_CACHE_HOME` in the layout the real cache uses
(`packages/<name>@<ver>/node_modules/<name>`), each with a module body that appends to a
file the instant it is evaluated — so "skipped" is measured, not inferred from a log line.
Build reports `1.18.13`.

| `engines.opencode` | module body evaluated? | factory called? | diagnostic |
|---|---|---|---|
| `>=99.0.0` (excluding) | **NO** | NO | `kind=Compatibility` |
| `not-a-semver-range!!` (invalid) | **NO** | NO | `kind=Compatibility` |
| `>=1.0.0` (satisfying) | **YES** | **YES** | none |
| absent | **YES** | **YES** | none |

```
=== EXCLUDING (>=99.0.0) ===   NOT EVALUATED — skipped before activation
=== SATISFYING (>=1.0.0) ===   MODULE BODY EVALUATED
                               FACTORY CALLED
=== ABSENT engines ===         MODULE BODY EVALUATED
                               FACTORY CALLED
```

**Is it skipped?** Yes, and provably *before activation* — the excluding and invalid
packages' module bodies never run, so no plugin code is executed at all. This is the half
that was broken: wave 13 saw incompatible plugins loaded with a warning.

**Is the message actionable?** Yes. Verbatim as documented, with both halves a user needs:

```
WARN oc_cli::cmd::plugin_runtime: JavaScript plugin did not fully load
  plugin=f3-vgtest@1.0.0 hook=None kind=Compatibility
  Plugin requires opencode >=99.0.0 but running 1.18.13 surface="turn"
```

It names the plugin, the classification (`kind=Compatibility`, distinguishing this from
`FailedToLoad`/`Protocol`), the **declared range**, and the **running version** — enough to
act without reading source. The invalid-range case reports the same way, echoing the bad
range back (`Plugin requires opencode not-a-semver-range!! but running 1.18.13`), which is
what tells an author their range is malformed rather than merely unsatisfied.

**Regression check — did the gate become over-eager?** This is the failure mode a fix here
could plausibly introduce: skip every npm plugin. It did not. Satisfying and absent ranges
both reach module evaluation *and* factory invocation. An intermediate run of mine
accidentally proved the discrimination even more sharply: before I supplied
`@opencode-ai/sdk` to the fixtures, the excluding package still stopped at
`kind=Compatibility` while the satisfying and absent ones got all the way to `init` and
failed there with `kind=Protocol` — i.e. they had already **passed** the gate and failed
later for an unrelated, fixture-side reason. The gate is discriminating on the right field,
not rejecting broadly.

Turn completes throughout: `EXIT=0`, `DONE-AFTER-TOOL`, and a skipped plugin does not take
the turn down.

**Visibility (unchanged, and not part of this entry).** The diagnostic is `WARN`, so at
default verbosity stderr is empty and `--print-logs` is required to see it. That was my
separate wave-13 observation **F3-W13-02**, which was not admitted to the frozen ledger,
so it is out of scope for Round 2 and I record it as a **Follow-up**, explicitly
non-blocking. Entry 4's actual defect — wrong manifest field, incompatibles loaded — is
closed.

**Answer: closed — yes.**

### Entry 6 (F4 w13) — `PluginInput.client` unprojected — **CLOSED** (with one non-blocking limitation)

I wrote plugins that call `client.provider.list()`, as instructed, and checked what they
observe. Note `file:` plugins are deliberately refused a hand-rolled client —

```
{"step":"provider.list THREW","err":"Error: @opencode-ai/sdk is unavailable to this file plugin fixture"}
```

— so this needs an **npm-style** package with a real `@opencode-ai/sdk` resolvable, which is
how the installed plugin resolves it. I built one under an isolated `XDG_CACHE_HOME`.

**The projection is real and functional.** `client.provider.list()` returns a correctly
shaped, fully populated payload:

```
{"step":"deferred provider.list OK","serverUrl":"http://127.0.0.1:47505/",
 "topKeys":["all","default","connected"],
 "raw":"{\"all\":[{\"name\":\"Amazon Bedrock\",\"env\":[\"AWS_ACCESS_KEY_ID\",…],
   \"id\":\"amazon-bedrock\",\"npm\":\"@ai-sdk/amazon-bedrock\",
   \"models\":{\"amazon.nova-2-lite-v1:0\":{\"id\":\"amazon.nova-2-lite-v1:0\",
   \"name\":\"Nova 2 Lite\",\"release_date\":\"2024-12-01\",\"attachment\":false,
   \"reasoning\":true,\"tool_call\":true,\"cost\":{\"input\":0.33,\"output\":2.75,…},
   \"limit\":{\"context\":128000,\"output\":4096},…"}
```

`{all, default, connected}` is exactly the shape `@sunerpy/oh-my-openagent`'s
`updateConnectedProvidersCache` consumes (I read its `dist/index.js:26015-26023` to confirm
which keys it reads). Model metadata, cost, limits and modalities are all present, so this
is a genuine projection of the model boundary and not an empty stub.

Other client methods have **real effects**, not just responses — `session.create` through
the projected client created a session that `session.list` then returned among 7:

```
{"step":"session.create","id":"ses_a8512b9ca67b4ff8878ea802328db2b1",
 "time":{"created":1786612362316,"updated":1786612362316},…}
{"step":"session.list","n":7,"firstTime":{"created":1786612362316,…}}
```

**The epoch-millisecond double-conversion is fixed.** The value round-trips to the correct
wall clock:

```
time.created = 1786612362316  →  2026-08-13T09:12:42.316Z
reference now                 =  1786612384278   (22 s later)
createdIsEpochMs: true
```

A double conversion would land thousands of years away or in 1970; this is correct to the
second.

**Answer: closed — yes.**

#### Follow-up (non-blocking, NOT raised as a Blocker) — `serverUrl` is port 0 on the turn-scoped plugin instance

While exercising this entry I measured something worth recording. The plugin instance that
runs **hooks during a turn** is handed `serverUrl: "http://127.0.0.1:0/"`, so client calls
from a hook cannot connect. Reproducible on both surfaces:

```
# CLI `run`
{"tag":"RUN-CLI","step":"init","serverUrl":"http://127.0.0.1:0/"}
{"tag":"RUN-CLI","step":"hook fired","where":"chat.message","serverUrl":"http://127.0.0.1:0/"}
{"tag":"RUN-CLI","step":"provider.list THREW","err":"Error: Unable to connect. Is the computer able to access the url?"}

# `serve --port 47504`, while a server IS listening on 47504
{"tag":"SERVE","step":"init","serverUrl":"http://127.0.0.1:47504/"}   ← startup instance, correct
{"tag":"SERVE","step":"init","serverUrl":"http://127.0.0.1:0/"}       ← turn instance, port 0
{"tag":"SERVE","step":"hook fired","where":"chat.message","serverUrl":"http://127.0.0.1:0/"}
{"tag":"SERVE","step":"provider.list THREW","err":"Error: Unable to connect. …"}
```

The diagnosis is precise: **the projection is not the problem** — the same plugin, same
code, called from the startup instance whose `serverUrl` is correct, succeeds and returns
real data (quoted above). Only the URL injected into the turn-scoped instance is wrong.

**Why this is a Follow-up and not a Blocker**, per convergence iron rules 4 and 5:

- Entry 6's question is whether `PluginInput.client` is projected. It is, verifiably, with
  correct shape, real effects, and correct epoch-ms. That question answers *yes*.
- Rule 4 admits a new Blocker **only** for a regression **directly introduced** by one of
  the six fixes. I cannot demonstrate that. Before todo 176 the client was unprojected, so
  there was no working turn-surface client to regress *from*; port 0 looks like a
  pre-existing sentinel for "no server bound to this surface" (`run` starts no HTTP
  server at all). Attributing it to todo 176 would be speculation, and rule 1 requires
  concrete falsifiable evidence.
- Rule 5: disputes default to pass.

Practical impact is bounded, and I checked rather than assumed: the plugin the entry names
handles this. `@sunerpy/oh-my-openagent` calls `provider.list()` from a hook wrapped in a
`Promise.race` with `CACHE_UPDATE_TIMEOUT_MS = 1e4`, catches the failure, and degrades to a
toast ("Failed to build provider cache. Restart OpenCode to retry.") rather than failing the
turn. Consistent with that, my Entry 3 runs with the **real** three-plugin list completed
with exit 0 and empty stderr. A plugin that instead `await`s a client call inside its
factory stalls that surface's plugin `init` for 60 s and is then disabled
(`kind=TimedOut … did not answer 'init' within 60000 ms`) — the turn still completes, but
that is a slow and confusing path. Worth an owner's attention next cycle; it does not block
this release.

### Entry 1 (F1 w13) — measured v1 plugin SDK routes answered 501 — **CLOSED**

Hit with `curl` against a live `serve` process, checking the **effect** rather than the
status code, as instructed. All three previously-501 routes now do real work.

#### `PUT /auth/{providerID}` — persists a credential

```
$ curl -X PUT http://127.0.0.1:47506/auth/f3probe -d '{"type":"api","key":"F3R2-SECRET-KEY-AAA"}'
true
HTTP=200
```

Effect on disk, and read back by a **separate CLI process** (so this is not in-memory state):

```
$ python3 -c "…json.load(open('…/auth.json'))"
keys: ['f3probe']
f3probe entry: {"type": "api", "key": "F3R2-SECRET-KEY-AAA"}

$ opencode-rust auth list
Credentials /tmp/opencode/f3r2/cli/data/opencode/auth.json
f3probe api
1 credential
```

#### `POST /provider/{id}/oauth/authorize` — really invokes the plugin's `authorize()`

First, the routes validate rather than stub — an empty body gets a named field, not a 501:

```
HTTP=400 {"error":{"code":"invalid_request","message":"provider OAuth authorize requires a JSON body with an integer `method`"}}
```

and an unknown provider gets a genuine dispatch failure, proving lookup happens:

```
HTTP=502 {"error":{"code":"provider_oauth_failed","message":"provider OAuth failed: plugin provider `f3probe` has no OAuth method 0"}}
```

So I supplied a plugin that actually declares an OAuth method. The response is **the
plugin's own return value**, carrying my marker string:

```
$ curl -X POST …/provider/f3oauth/oauth/authorize -d '{"method":0}'
{"url":"https://example.invalid/authorize?f3=1","method":"code","instructions":"F3R2-INSTRUCTIONS-MARKER"}
HTTP=200
```

and the plugin recorded that it ran:

```
{"step":"authorize() CALLED"}
```

#### `POST /provider/{id}/oauth/callback` — invokes the plugin and **stores the tokens**

```
$ curl -X POST …/provider/f3oauth/oauth/callback -d '{"method":0,"code":"CODE-XYZ"}'
true
HTTP=200

plugin-side log:
{"step":"authorize() CALLED"}
{"step":"callback() CALLED","code":"CODE-XYZ"}
```

The authorization code was passed through to the plugin, and the credential the plugin
returned was persisted verbatim with the right kind:

```
keys: ['f3oauth', 'f3probe']
f3oauth: {"type":"oauth","refresh":"F3R2-REFRESH-CODE-XYZ","access":"F3R2-ACCESS-CODE-XYZ","expires":1900000000000}

$ opencode-rust auth list
f3oauth oauth
f3probe api
2 credentials
```

That is a complete OAuth round trip through the v1 surface with a real stored effect — the
thing F1's B1 said "closure requires". A plugin-provided auth provider can now authenticate
through this surface.

#### The recorded payload contracts

**`slug`** — my wave-12 finding — is present on v1 session create:

```
$ curl -X POST http://127.0.0.1:47507/session -d '{}'
{"directory":"…","id":"ses_08dae…","parentID":null,"projectID":"global",
 "slug":"ses_08dae10ac122467485f532bd61bbf1c4","time":{…},"title":"…","version":"0.1.0"}
slug present: True
```

**Antigravity's `tool_result` prompt part** (route 18, `POST /session/{id}/message`) — HTTP
**400** in wave 12 — is accepted, and wave-13 **caveat 2 is fixed**: the body's model is now
honoured instead of silently substituted.

```
HTTP=200
{"info":{…,"providerID":"aaatool","modelID":"tool-model","finish":"stop",…},
 "parts":[{…,"text":"DONE-AFTER-TOOL","type":"text"}]}
```

In wave 13 this same call reported `providerID: amazon-bedrock`, `modelID:
amazon.nova-2-lite-v1:0` and **zero requests reached my fake server**. Now it reports my
provider *and* the request genuinely arrived — the returned `DONE-AFTER-TOOL` is my fake
server's own output, and its log shows the content delivered:

```
MSG: {"content": "Operation cancelled by user (ESC pressed)", "role": "user"}
```

**OMO's summarize contract `{providerID, modelID, auto}`** — which wave 13 recorded as
*unverified* because caveat 2 blocked steering it onto an observable provider — now works
and I could finally measure it. On a session with no compactable history it returns a real
domain error from the compaction engine rather than discarding the body:

```
HTTP=500 {"error":{"code":"mutation_failed","message":"manual compaction failed:
  Reason(\"NoCompactableHistory: session has no compactable history before the preserved tail\")"}}
```

After driving four more turns to build history (10 messages), the same call succeeds **and
dials the model named in the body**:

```
provider requests before summarize: 48
true
HTTP=200
provider requests after: 49  (delta=1)
"model":"tool-model"
```

Body honoured, one real provider request issued, `true` returned. This closes the
"summarize's body was discarded" contract with a measured effect.

**Answer: closed — yes.**

#### Follow-up (non-blocking) — `tool_use_id` is still dropped

Wave-13 **caveat 1** persists. The `tool_result` part is stored as a plain `text` part and
the correlation id appears nowhere:

```
persisted user part:
{"id":"prt_546df6a4…","messageID":"msg_35276541…","sessionID":"ses_08dae…",
 "text":"Operation cancelled by user (ESC pressed)","type":"text"}

occurrences of "call_abc123" in the provider request log: 0
```

The route accepts and acts on the contract, but the id that would let an orphaned `tool_use`
be paired with its `tool_result` is discarded. This was a **caveat** in wave 13, not a
ledger entry — Entry 1 is specifically "routes answer 501", which is closed — so per iron
rules 3 and 4 this is a Follow-up, explicitly non-blocking. It is already recorded in the
plan's backlog as "Antigravity recovery's `tool_use_id` is not covered end to end".

### todo 178 — oracle re-pin to 1.18.18 — **CLOSED**

The environmental item: the host's upstream `opencode` was upgraded to 1.18.18 mid-session
while the oracle pinned 1.18.15.

```
$ opencode --version
1.18.18

crates/oc-testkit/src/oracle.rs:81:  pub const PINNED_RELEASE: &str = "1.18.18";
```

The pin is not merely edited — a test verifies it resolves to a real binary reporting that
version, and it passes:

```
$ cargo test --offline -p oc-testkit --lib
test oracle::tests::the_declared_pin_equals_the_version_the_resolved_binary_reports ... ok
test oracle::tests::the_resolved_pinned_oracle_reports_the_pin_from_this_working_directory ... ok
test oracle::tests::the_screen_walks_past_a_launcher_and_a_wrong_release_to_reach_the_pin ... ok
test result: ok. 139 passed; 0 failed
```

Declared pin, installed binary and the resolver all agree.

### Gate — final run: 3473 passed / 0 failed

Re-run at the end of QA, with `--no-fail-fast` so a single flake could not hide the total.
Host load at the time: three other `cargo test` processes (another reviewer's
`--workspace`, plus two unrelated projects).

```
$ cargo test --workspace --offline --no-fail-fast
TOTAL passed=3473 failed=0 ignored=2
FAILED targets: 0
error lines: 0
```

Matches the expected **3473 / 0** exactly. The earlier run-1 flake did not reproduce.

### Sanity sweep — basic surfaces after six fixes

Bounded checks that nothing obvious broke:

```
$ opencode-rust --version        → 1.18.13   (compatibility baseline, per the split-version divergence)
$ opencode-rust --help           → usage + command list
$ opencode-rust auth list        → "0 credentials" on an empty store (no crash on empty state)
$ opencode-rust --nope           → error: unexpected argument '--nope' found  + usage
$ opencode-rust run --model nosuch/model "hi"   → Model not found: nosuch/model
```

Clear, actionable errors; no panics, no stack traces, no hangs.

---

## Verdict

# APPROVE

All six frozen ledger entries are **closed**, verified by using the built binary rather than
by reading source, and **no admissible new Blocker exists**.

Per convergence iron rule 6 — "converged when a round produces no new threshold-passing
Blocker; confirming old ones closed is enough" — **this round converges from my side.**

### What I did not merely accept

For each entry I tried to find the way the fix could be hollow, because four of these were
my own findings and a passing surface check is not the same as a working product:

- Entry 2: an empty stderr could mean the plugin never loaded → I proved the hook ran 10
  times, reached the exact failure point with data intact, and continued past it; then
  proved mutations reach the provider; then proved the truncation detector still exists and
  cannot be fooled by a forged `$source: "host"`.
- Entry 3: "loader never called" would be a fake fix → I proved a genuinely throwing loader
  is isolated on all three surfaces, and that `auth()` hands over a real `Auth` rather than
  the `null` that caused the original crash.
- Entry 4: "skip everything" would pass a skip test → I proved satisfying and absent ranges
  still reach factory invocation.
- Entry 5: one direction cannot distinguish "chose correctly" from "chose the only option"
  → both directions, both surfaces, plus the whole precedence chain to rule out overshoot.
- Entry 6: a projected-but-unusable client would be hollow → I proved real data, real
  effects and correct epoch-ms, and separately isolated the one limitation that remains.
- Entry 1: status codes prove nothing → every route checked for a persisted or
  externally-observable effect, including a full OAuth round trip and a provider request
  count delta.

### Follow-ups (recorded, explicitly NON-BLOCKING)

1. **`serverUrl` is `http://127.0.0.1:0/` on the turn-scoped plugin instance**, so
   `client.*` calls from hooks cannot connect, on both `run` and `serve` — even while a
   server is listening. The projection itself is fine (proven: the same call from the
   startup instance succeeds with real data). Not attributable to todo 176 as a regression,
   since no working turn-surface client existed before it. Bounded in practice: the plugin
   the entry names guards with a 10 s race and degrades to a toast, and my real-plugin runs
   completed with exit 0. A plugin that instead awaits a client call inside its factory
   stalls `init` for 60 s and is then disabled — slow and confusing, though the turn still
   completes.
2. **`tool_use_id` is still dropped** on route 18's `tool_result` part (wave-13 caveat 1;
   already in the plan's backlog).
3. **`transport::remote_timeout_bounds_both_transport_attempts` asserts on wall-clock
   elapsed time** (`< 500 ms`) with no slack for a loaded host; it flaked for me under
   concurrent load and passed in isolation in 0.13 s. It will flake again in CI.
4. **The version-gate diagnostic is `WARN`**, so it is invisible at default verbosity and
   needs `--print-logs` (my wave-13 F3-W13-02, not admitted to the ledger).

None of these is a regression caused by one of the six fixes, so per iron rules 4 and 5
none blocks release.
