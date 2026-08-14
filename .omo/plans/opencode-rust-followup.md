# opencode-rust — Follow-up backlog

Created 2026-08-13, after the main plan reached **183/183 with four-reviewer approval** at
product tree `crates=1b37eafb1e14` (HEAD `d2eb383f`).

## What this file is, and is not

Every item here was **explicitly ruled non-blocking** by the reviewer who raised it, under
the convergence protocol's Blocker admission threshold (`.omo/plans/opencode-rust.md`,
section `## Final Verification Wave — convergence protocol`). None of them blocked
acceptance, and closing them is **not** a precondition for anything already delivered.

They are recorded here because a non-blocking finding that lives only in a review report
is a finding that gets lost — which is exactly what happened in wave 8, when three of F2's
non-blocking observations went unactioned until F2 escalated them to Blockers in wave 9.
The orchestrator called that a scheduling failure of its own. This file is the fix.

**Correction to the record**: the orchestrator twice told the user this backlog held
"four items". It holds **five** from the backlog paragraph plus **one** F4 ruled Follow-up
in Round 2 — **six**. The undercount was an orchestrator error, not a reviewer omission.

## Status

None started. No item has an owner. Nothing here is in progress.

---

## FU-1. `ProviderRetryPolicy` has no production caller

**Status: CLOSED (2026-08-13)** — `run_turn` now performs bounded typed provider retries with rollback, terminal-error propagation, and permanent/idle-partial guards.

**Source**: F2, non-blocking observation F2-O1, raised in Round 1 (wave 13) and confirmed
still true in Round 2.

**Verified location**: `crates/oc-engine/src/retry.rs:140` (`ProviderRetryPolicy`), `:187`
(`retry_provider`). Callers exist only in tests and testkit.

**Why it matters.** This is the **zero-non-test-caller shape** this project has now found
five times — seam #17 (`HookInvocation::Auth`/`Tool`), `PluginKind::Tui` (todo 160),
`discover_plugins` (todo 163), `MODEL_ENDPOINTS_OPTION` (todo 162), and now this. In each
prior case a reviewer required it to be either wired or declared. A public retry policy
that production never invokes is a capability the tests prove and users cannot get.

F4's Round-2 note is relevant: todo 166 established that `run_turn` reads
`provider.stream(...)` and returns the first `ProviderError` **without** calling
`retry_provider`, so there is no retry amplification of the idle-timeout path. That is why
it is not a defect today — but it also means a transient provider 503 ends the turn.

**Two honest options.**
1. **Wire it**: a retryable transient failure emits `RetryRollback`, clears attempt-local
   accumulation, replays within a finite budget, and surfaces the final failure. F4's
   Round-2 report sketched exactly this in its required-closure list.
2. **Remove it and declare the narrowing**, as todo 160 did for `PluginKind::Tui`.

**Do NOT** leave it as a public API only tests can reach. That is the state five reviewers
have rejected.

**Acceptance if wired**: a production-path test drives a transient 503 through a real turn
and asserts bounded replay then a surfaced failure; removing the retry call fails it by
name; a permanent failure is not retried.

---

## FU-2. Malformed plugin-model diagnostic is unguarded

**Status: CLOSED (2026-08-13)** — `production_js_malformed_model_diagnostic_names_the_plugin_model_and_decode_reason` pins plugin, model id and decode reason on one emitted line; deleting the emission or blanking any single field fails it by name.

**Source**: F2, non-blocking observation F2-O2, Round 1 and Round 2.

**What exists**: todo 167 correctly isolates a malformed model sibling — one bad model does
not discard the whole provider — and emits a debug event naming plugin, model id, and decode
error.

**The gap**: deleting that event leaves **both** production JavaScript boundary tests green.
So the diagnostic is behavior nothing pins.

**Why it matters.** The silent-drop branch is deliberate and correct (discarding a whole
provider over one bad model is worse). But that makes the diagnostic the *only* way a plugin
author learns their model was rejected. An unguarded diagnostic is one refactor from
silence, and this project's worst defects have all been silent.

**Acceptance**: a test asserts the diagnostic names plugin, model id, and decode reason;
deleting the emission fails it by name.

---

## FU-3. Antigravity recovery's `tool_use_id` is not covered end to end

**Status: CLOSED (2026-08-13)** — The v1 adapter now resolves only the pending calls named by submitted ids and rejects unknown ids without writes.

**Source**: F2, non-blocking observation F2-O3, Round 2.

**Verified location**: `crates/oc-server/src/compat_v1.rs:1354-1357` — the route reads and
validates `tool_use_id`, requiring a string.

**The gap**: the route **discards** it after validation. Changing the fixture to a wrong id
leaves the route regression green. A separate engine test proves session-wide unfinished-call
behavior, but nothing connects the submitted id to that behavior.

**Why it matters.** Todo 169 fixed the case where an adapter *discarded the body* — F2 found
that by noticing a body extractor could be removed with the test still passing. This is the
same shape one field down: validated, then dropped.

**Acceptance**: a route-level test submits a `tool_result` whose `tool_use_id` names a
specific pending call and asserts *that* call is the one resolved; a wrong id produces an
observable difference.

---

## FU-4. The published OpenAPI binds no request or response bodies

**Status: CLOSED (2026-08-13)** — Every body already described by a Rust `JsonSchema` type is now bound in `/doc`; the remaining 48 operations are frozen by operation and reason in the generated compatibility inventory.

**Source**: F3, finding F3-W13-03, Round 1.

**What F3 measured**: the document served at `/doc` binds **0 of 60** operations to any
request or response body.

**Why it matters.** Todo 171 fixed a case where the port served a response its own published
schema rejected, and did it by *deriving* the required-key set from `/doc` and the oracle
rather than hard-coding it. That technique only works where `/doc` actually describes bodies.
A schema that describes no bodies cannot catch the next projection defect — and projection
defects are what F4 spent eight layers on.

The oracle's own capture has **188 operations** for comparison
(`.omo/fixtures/oracle-openapi-1.18.12.json`).

**Scope caution.** This is potentially large. Bind bodies where the types already exist and
the binding is mechanical; do **not** invent schemas to raise the count. If most operations
cannot be bound without new type work, say so and record the remainder as a frozen gap with
its reason — the registry rule (`docs/divergences.toml:11-14`) forbids laundering an
unimplemented surface into a divergence.

**Acceptance**: every operation whose Rust types already describe its body binds that body in
`/doc`; a test asserts a bound operation's response validates against its own published
schema; unbound operations are enumerated in the frozen gap inventory with reasons.

---

## FU-5. `/agent` schema drift is unverifiable against the available capture

**Status: CLOSED (2026-08-13)** — The sole OpenAPI fixture now names pinned release 1.18.18 and is byte-verified against its live `/doc`; the exact residual optional-key drift remains a witnessed known gap.

**Source**: F3, lower-severity observation, Round 1; recorded by todo 171 as the frozen gap
`v1-agent-projection-drift` (`crates/oc-testkit/src/compat_report.rs:491`) with a witness
test.

**Why it was honestly blocked, not neglected.** Todo 171 established three facts: all four
oracle-required keys (`name`, `mode`, `permission`, `options`) are returned; this build
publishes **no** `Agent` schema at `/doc`, so there is no self-contradiction to detect; and
the tree held only a **1.18.12**-named capture while the oracle pinned **1.18.18**. FU-5
replaced that name with the executable pin after proving the live bytes identical, so the
optional-key drift (extra `builtIn`/`maxSteps`/`tools`, missing `hidden`/`native`/`steps`/
`temperature`/`topP`/`variant`) is now characterised without fabricating a capture.

**What would unblock it**: a `/doc` capture from the currently pinned release, taken the way
`compat_suite.rs` takes its others — refetched from the running binary so provenance is an
executable assertion rather than a claim. Then the drift becomes measurable and is either
matched or declared.

**Do NOT** hand-author a capture to make the comparison possible. A capture no test
re-derives is a claim, not a fixture — the rule `no_pinned_oracle_paths.rs:20-26` states.

**Acceptance**: a provenance-asserted capture from the pinned release exists; the `/agent`
comparison either passes or its residual difference is declared with a reason; the witness
test for the gap is updated or retired accordingly.

---

## FU-6. Legacy reverse-projection `release_date` restoration is unguarded

**Status: CLOSED (2026-08-13)** — Pinned, and the gap was four fields wide, not one: `release_date`, `variants`, `capabilities.interleaved` and `limit.input` were all unobservable because `kiro_model()` held their defaults; two new tests drive the real JS auth loader over a non-default canonical model so all five restorations fail by name.

**Source**: the orchestrator found it while mutation-verifying todo 176; **F4 independently
ruled it Follow-up** in Round 2 after `git blame` showed it predates todo 176
(commit `35cda9514`), so it is not a regression that todo introduced.

**Verified location**: `crates/oc-plugin/src/js/projection.rs:267` —
`model.release_date.clone_from(&original.release_date);` in the legacy reverse-projection
branch.

**The gap**: commenting that line out and running the **entire** workspace suite produces
**zero** failures. The orchestrator confirmed this directly.

**Worth recording precisely because of how it was found.** The orchestrator's first mutation
of the epoch fix targeted this line, saw nothing fail, and briefly mistook it for the real
guard's absence — the actual guard is at `crates/oc-server/src/api/provider.rs:812`, pinned by
`compat_v1_provider_projection_preserves_catalog_model_semantics`. Two separate paths handle
`release_date`; only one is guarded.

**Acceptance**: a test pins the legacy reverse-projection's `release_date` restoration such
that removing it fails by name; or, if the line is genuinely redundant now that the forward
projection is typed, remove it and prove the removal changes nothing observable.

---

## FU-7. Cap the retry worst case at 3 minutes and make retrying visible

**Status: CLOSED (2026-08-13)** — the engine now enforces a shared 180-second absolute
provider deadline independent of attempt and idle-timeout factors. Plain `run` and the TUI render
`RetryRollback` as a red human-readable attempt notice, while non-TTY output remains escape-free
and the JSON shape remains unchanged. Paused-time regressions, partial-output checks, changed-file
diagnostics, and controlled ceiling/visibility/factor mutations all passed; the final workspace
gates are recorded in `.omo/evidence/task-fu7-opencode-rust.txt`.

**Source**: the user, 2026-08-13, on reviewing FU-1's measured timing bound:
*"需要优化 6 分钟的重试，可以选择性增加一些必要输出（比如红色的重试中），最长 3 分钟的时间是极限了"*.

**The measurement that prompted it.** FU-1 wired `ProviderRetryPolicy` into `run_turn` with
`PROVIDER_RETRY_MAX_ATTEMPTS = 3` (`crates/oc-engine/src/loop.rs:45`). That composes with todo
154/166's 120-second idle timeout, so a provider that stalls **without a status code and before
emitting any output** can consume 3 × 120s = **360 seconds**. FU-1 recorded this honestly rather
than hiding it. After any generated output the cost is one idle window, because generated text is
not rolled back and the error surfaces immediately.

**Two requirements, and they are independent.**

### (a) Hard ceiling: 180 seconds

The user's limit is explicit. **Do not simply set attempts to 2** and call it done — 2 × 120s is
still 240s. The ceiling must hold as a property, not as an arithmetic coincidence of two constants
that a later change to either one can break.

Prefer a **deadline** the retry loop honours: record the first attempt's start, and refuse a further
replay once elapsed + the next idle window would exceed the ceiling. That survives someone later
changing `PROVIDER_RETRY_MAX_ATTEMPTS` or the idle timeout. Whatever mechanism is chosen, a test
must pin the **total**, not the factors.

Note there is already a precedent for capping a bound rather than trusting its inputs: todo 154
capped user overrides of the idle timeout at 180s. Reuse that thinking, and check whether the same
constant should be shared rather than duplicated.

### (b) Make retrying visible in the human-facing surfaces

**Verified current state**: `StreamEvent::RetryRollback { attempt, max }` is already emitted before
every sleep and replay (`crates/oc-engine/src/retry.rs:248`), and the JSON surface already renders
it as `{"type":"retry_rollback","attempt":…,"max":…}` (`crates/oc-cli/src/cmd/run.rs:288`).

**But no human-readable surface shows anything.** The TUI consumes the event only to *discard*
partial parts (`crates/oc-tui/src/views/message.rs:415`) — correct behaviour, since keeping them
would render the answer twice — and prints nothing. So a user watching a stalled turn sees silence
for up to the full ceiling.

Show it, in red, on the surfaces a person reads: the plain `run` output and the TUI. The event
already carries `attempt` and `max`, so the message can say which attempt is in flight.

**Check for an existing colour idiom before adding one.** A grep for `Color::Red` found nothing in
`oc-cli`, so establish how this workspace already styles warnings — and if it does not, whether
adding a styling dependency is justified for one message, or whether the TUI's own renderer already
provides colour. Do not introduce a new dependency casually; this project's dependency discipline
is the reason it has zero first-party `unsafe`.

**Respect non-TTY output.** A red escape sequence written into a pipe or a log is noise. Follow
whatever the workspace already does for TTY detection; if nothing exists, plain text on non-TTY.

**Acceptance criteria (agent-executable)**:
- A test drives repeated status-less transient stalls and asserts the **total** elapsed bound is at
  or under 180s — not that attempts equal some number. Changing either `PROVIDER_RETRY_MAX_ATTEMPTS`
  or the idle timeout alone must not be able to breach it; prove that by mutating each and observing
  the bound test still holds or fails loudly.
- A retry emits a visible, red, human-readable notice naming the attempt and the max on the plain
  `run` surface and in the TUI; a test asserts the rendered bytes contain the notice.
- On a non-TTY sink the notice carries no escape sequences.
- The JSON surface's `retry_rollback` shape is unchanged — it is a public output contract.
- Todo 154/166's behaviour still holds: a stall after generated output preserves the partial text
  and surfaces the idle error immediately, with no replay.
- Removing the notice fails the visibility test by name; removing the ceiling fails the bound test
  by name.

## FU-8. `run` cannot complete a turn against a real provider — two distinct defects

**Status: CLOSED (2026-08-14)** — Defect A routes provider-level custom
`@ai-sdk/openai` URLs through the compatible transport, defaulting to Chat unless
the model advertises an endpoint; Defect B rejects empty completed turns.

**Source**: the orchestrator's first genuine end-to-end verification with real credentials,
2026-08-14, prompted by the user asking how to configure and validate the project. **Nothing in the
3488-test suite covers this path**, because every test supplies its own mock provider. This is the
project's twenty-fourth seam and the first one found by simply trying to use the product as shipped.

### Defect A — `@ai-sdk/openai` is routed to a Responses endpoint that many gateways do not serve

**Measured directly against the user's configured gateway** (`myopenai`, an OpenAI-compatible relay
at `https://openai-us.onethinker.top/api/v1`, `npm: "@ai-sdk/openai"`):

```
POST /api/v1/chat/completions  -> HTTP 200
POST /api/v1/responses         -> HTTP 400
```

And the product's own behaviour on the same model:

```
$ opencode-rust run --model myopenai/global.anthropic.claude-haiku-4-5-20251001-v1:0 "hi"
EXIT=1
unrecoverable provider failure (status=Some(404))
```

The chain: `turn.rs:1100` maps `"@ai-sdk/openai"` to the `openai` identity; `family.rs:250` binds
that identity to `Family::OpenAi`; the surface walk in `surface.rs:160-212` prefers
`ApiSurface::Responses` whenever `support.responses` holds — and the comment at `:162` is explicit:
*"without it the walk always prefers `responses`"*.

So **a provider that declares `@ai-sdk/openai` but serves only Chat Completions is unusable**. That
describes most self-hosted relays and proxies, and it describes the user's working configuration.

**Why the existing tests missed it.** Todo 156 made request bodies and decoders surface-aware and
todo 162 carried the advertised endpoint into selection — both correct, both verified. But their
tests assert that *a declared surface is honoured*, never that *an undeclared surface degrades to
one the server actually serves*. The `SurfaceSupport` for a custom gateway is assumed, not probed.

**Do NOT fix this by hard-coding a host pattern.** Options, to be weighed with evidence:
- honour an explicit per-provider surface override in config (does one already exist? check before
  adding);
- treat a `404`/`400` on `/responses` as a signal to fall back to `/chat/completions` **once** per
  provider, cached for the session — note this must not mask a genuine `400` from a well-formed
  Responses request;
- default a *custom* `baseURL` to Chat unless Responses is explicitly declared, on the reasoning that
  `@ai-sdk/openai` against a non-OpenAI host cannot imply OpenAI's newest surface.

Whichever is chosen, **check what released 1.18.18 does with this exact configuration** — the user's
gateway works under upstream, so upstream's behaviour is the oracle.

### Defect B — `kiro-auth` turns exit 0 with no output, no error, and no log line

Separate from A and worse, because it is **silent**:

```
$ opencode-rust run --model kiro-auth/claude-haiku-4-5 "say ok"
EXIT=0   stdout 0 bytes   stderr 0 bytes   --print-logs 0 lines
```

The turn is not a no-op. Verified through the database, bypassing every rendering layer:

```
session ses_8abf8f57…  (created 03:00:06, four minutes before the query)
  role=user       parts=1     <- the prompt was persisted
  role=assistant  parts=0     <- the assistant message exists and is empty
```

So a session is created, both messages are written, the assistant message has **zero parts**, and the
CLI exits **0** having printed nothing at all. A user cannot distinguish this from success.

This must be characterised before it is fixed. Candidate causes, none confirmed:
- the provider returned an empty stream and the turn treated it as a completed turn;
- authentication failed in a way that produced no error path;
- content was produced but dropped between the stream and the renderer.

**The most important requirement here is not the fix but the observability**: exiting 0 with no
output is the failure mode this project has hunted twenty-three times. Whatever the cause, an empty
assistant turn must be reported, not silently accepted.

### Both defects share one root cause in the test strategy

**No test exercises a real provider with real credentials.** Every one of the 3488 supplies a mock.
That is defensible for an offline gate — determinism and no cost — but it means the suite can be
entirely green while the product cannot hold a conversation. That gap is the actual finding.

The fix is not "add a live test to the default gate". It is an **opt-in** live smoke test, excluded
from the offline gate, that a maintainer can run before a release:

- one turn, one cheap model, a minimal prompt (cost was a stated user concern; `hi` against a Haiku
  is the right size);
- asserts a non-empty assistant part reaches stdout **and** the process exits 0;
- asserts the inverse honestly: if the provider is unreachable or unauthenticated, the command exits
  non-zero with a message naming the cause.

### Acceptance criteria (agent-executable)

- Defect A: a provider declaring `@ai-sdk/openai` with a custom `baseURL` that serves only
  `/chat/completions` completes a turn. A test pins the chosen mechanism; reverting it fails that
  test by name. Upstream 1.18.18's behaviour on the same configuration is recorded in the evidence.
- Defect B: an empty assistant turn is **never** reported as success — the command exits non-zero
  with a diagnostic naming the provider and the emptiness. A test drives a provider that returns an
  empty stream and asserts the non-zero exit and the message.
- A live smoke test exists, is **excluded from `cargo test --workspace --offline`** by an explicit
  mechanism (feature or ignore attribute), is documented with its cost, and passes against the
  configured provider.
- No credential, key, or token appears in any test fixture, log, or evidence file.

## FU-9. A plugin tool that collides with a builtin name breaks every turn

**Source**: the orchestrator's hands-on verification of FU-8A, 2026-08-14. FU-8A fixed the surface
routing correctly — and the very next real turn failed for an entirely different reason, which the
old 404 had been masking.

### The defect

With this machine's real configuration:

```
$ opencode-rust run --model myopenai/global.anthropic.claude-haiku-4-5-... "hi"
unrecoverable provider failure (status=Some(400)): provider `myopenai` returned HTTP 400:
{"error":{"message":"The tool grep is already defined at toolConfig.tools.10.",
 "type":"invalid_request_error","code":"bad_request"}}
```

**The decisive control**: the identical command with `--pure` **succeeds and returns a real answer.**
So the collision comes from user configuration or plugins, not from the builtin set.

**Located**: `@sunerpy/oh-my-openagent@4.21.0` registers a tool named `grep`, and the builtin registry
also has `grep`. Both reach the provider request.

### Why the registry does not catch it

`crates/oc-tools/src/registry.rs` **does** guard collisions *among builtins* — `RegistryError::
DuplicateBuiltin` at `:170`, enforced by a `contains_key` check at `:239`. But the assembly path at
`:281-286` is a sequence of `tools.extend(...)` calls — config-directory tools, then plugin tools,
then MCP tools — **with no cross-source name check at all.** A plugin may therefore shadow or
duplicate a builtin name, and the duplicate is sent to the provider verbatim.

### Why no test caught it

Same root cause as all of FU-8: **no test in the 3495 loads a real plugin set alongside the builtins
and inspects the outgoing tool list for duplicates.** The plugin tests use fixture plugins with
distinctive names; the builtin tests do not load plugins.

### The question that must be answered before fixing

**What does upstream 1.18.18 do when a plugin registers a builtin name?** It runs this exact
configuration successfully, so it either de-duplicates, gives one source precedence, or namespaces
plugin tools. Establish which, with a file:line citation, and match it. Do not invent a policy —
precedence here is user-visible: if the plugin's `grep` wins, behaviour changes silently; if the
builtin wins, the plugin author's tool is silently ignored. Either is defensible, but only one
matches upstream, and *silently* is what must be avoided in both cases.

### Acceptance criteria (agent-executable)

- A registry-level test assembles builtins plus a plugin declaring a colliding name and asserts the
  outgoing tool list contains **no duplicate names**.
- The precedence rule matches upstream, is cited in the evidence, and is pinned by a test.
- The losing tool's suppression is **observable** — a diagnostic naming the tool and both sources —
  rather than silent. This project has fixed twenty-four silent defects; this must not become the
  twenty-fifth.
- A hands-on run against the real configuration (all three installed plugins active, no `--pure`)
  completes a turn.
- Reverting the de-duplication fails the registry test by name.

**Status: CLOSED (2026-08-14)** — Cross-source tool names are de-duplicated with upstream last-source-wins precedence, visible suppression diagnostics, and a successful three-plugin live turn.

## Working rules for whoever picks these up

Carried from the main plan, because each was learned the hard way:

- **Mutate to verify, and mutate the right thing.** A registry-entry deletion and a
  behavioral break catch different things (wave 59). Beware equivalent mutants — the
  orchestrator was fooled four times, twice in this session alone.
- **Back up with `&&`, never `;`.** A truncating write once ran after `cp` had already
  reported `No space left on device` and destroyed an uncommitted file. Check `df -h /`
  first; stop above 90%.
- **Never `git add -A`, never combine `-A` with `-f`.** 48,148 build-product files were
  once committed that way.
- **Prose that nothing derives from goes stale.** Seven instances were fixed in the main
  plan, one of them in the orchestrator's own notepad. If you write a number, make
  something derive or check it.
- **Test inputs hide defects as effectively as weak assertions.** Todo 162 survived four
  provider waves because its tests used only heuristic-friendly model ids.
