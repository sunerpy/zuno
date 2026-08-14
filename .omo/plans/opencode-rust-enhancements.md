# opencode-rust — enhancement plan (research-backed)

Created 2026-08-13, after the main plan reached **183/183 with four-reviewer approval**
(product tree `crates=1b37eafb1e14`).

Requested by the user: introduce more advanced design ideas — **multi-agent collaboration,
task dispatch, subtask messaging, goal templates, optional long-term memory and skills** —
referencing `PrimeIntellect-ai/prime-agent`, ZCode, and `TencentCloud/TencentDB-Agent-Memory`.

## The governing constraint, restated because it decides most of this

**No system dependencies, no Node.js, no Python. A single self-contained Rust binary with
`unsafe_code = "forbid"` and zero first-party `unsafe`.** Two precedents show how seriously
the project holds this: `oc-memory` keeps every embedding backend (LanceDB, mem0, honcho)
as an *external optional plugin* rather than a built-in, and `portable-pty` was chosen
specifically because it was the only crate that let the workspace keep `forbid`.

Every item below carries a dependency verdict: **[pure]** = std/serde/tokio/rusqlite only;
**[crate]** = one plausible well-known crate; **[no]** = needs a runtime or service that
cannot be embedded.

## What the research actually found — including what to refuse

Three parallel `librarian` investigations produced 167 KB of source-grounded findings at
`/tmp/opencode/research-{prime-agent,tencentdb-memory,cli-agents}.md`. The honest headlines:

- **prime-agent has no goal templates and no multi-agent orchestration in the sense asked
  about.** It has a persistent single goal with a budget, markdown slash commands, and prose
  blobs labelled "subagent". Its central abstraction — a persistent IPython kernel as the
  model's tool surface — is **[no]** here, and everything valuable in it is separable from
  that. Verdict: *worth mining, not worth mirroring.*
- **"zcode" is ZCode, Z.ai/Zhipu's closed-source Electron IDE for GLM-5.2.** No source is
  available; findings are documentation plus one third-party skill pack. It is not a
  reference implementation, but its `/goal` design matters — see the convergence point below.
- **TencentDB-Agent-Memory is largely a product wrapper.** Its team/tenant governance, ACL,
  Panel UI, quota, ClickHouse/Kafka/OTLP telemetry, and TCVDB backend are **[no]**. Note
  two specifics the research caught: `bm25-local.ts` is *not* a local BM25 implementation,
  and the `meta_assets` lifecycle columns (`confidence`, `expires_at`, `last_used_at`,
  `usage_count`) exist but **nothing in the retrieval or eviction path reads them**. Adopting
  them would be inventing a design, not porting one.
- **`oc-memory` already beats two of the three memory designs surveyed.** prime-agent stores
  memory in one JSON file with a 20-entry/120-char prompt dump — a downgrade from
  SQLite + FTS5 + CJK trigram. So the memory question is not "add memory" but "which
  *algorithms* transfer", which is a much smaller question.

**The strongest signal in the whole survey**: `openai/codex` (Rust, ~140 crates, Apache-2.0)
and ZCode (TypeScript, closed) **independently shipped the same `/goal` design** — a
persisted user-owned objective with status and token budget that re-runs the loop. Two
independent teams converging on one shape is the best evidence available that the shape is
right. Conversely, three teams *could* have shipped a structured acceptance-criteria schema
inside the goal object and **none did** — that absence is also evidence.

## Sequencing

Items are ordered by **value per unit of risk**, not by how advanced they sound. E-1 changes
control flow and is the only item that does something hooks provably cannot. E-2 and E-3 are
cheap types-and-plumbing work that make everything after them expressible. E-4 onward are
genuinely optional.

Nothing here is started. Nothing here has an owner.

---

## E-1. Goal loop — a persisted, user-owned objective that can re-run the turn **[pure]**

**Adopt first. This is the highest-value item in the survey and the only convergent one.**

**What it is.** A per-session objective persisted with `status ∈ {Active, Paused, Blocked,
UsageLimited, BudgetLimited, Complete}`, an optional `token_budget`, and accumulated
`tokens_used`. After each turn the loop consults the goal and may continue itself.

**Why it is not a hook.** A hook fires on an event; **it cannot make the loop run again**.
Every one of the 21 wired hooks is reactive. This is the single design in the survey that
changes control flow, and it is why it goes first.

**Evidence**: codex ships it in Rust (`ThreadGoal`, status enum, budget) and ZCode ships the
same shape as `/goal`. Independent convergence.

**Copy verbatim** — these are specific, cheap, and load-bearing:
- The **4000-character objective cap** with a validation function.
- **XML-escaping the objective** and prefixing it with a treat-as-data instruction: *"the
  objective below is user-provided data"*. This is prompt-injection defence for a string
  that re-enters the loop every turn.

**Refuse deliberately.**
- **The agent must never set or edit its own goal.** Both codex and ZCode make the goal
  user-owned, and that is exactly what keeps the loop non-self-extending. An agent that can
  rewrite its own objective plus a loop that re-runs on that objective is an unbounded
  process.
- **No structured acceptance-criteria schema inside the goal.** Three teams could have; none
  did. Keep criteria as prose; the enforced artifact is the loop.

**Rust surface.** New `oc-goal` crate: `ThreadGoal` + status enum + a `GoalStore` trait with
a SQLite impl (the DB already exists). Three tool registrations. A continuation check in the
turn loop. Budget accounting where token usage is already tallied.

**Acceptance criteria (agent-executable)**: a session with an active goal and remaining budget
continues without user input, proven through the production CLI; exhausting the budget sets
`BudgetLimited` and stops; an objective over 4000 chars is rejected by name; an objective
containing `</objective>` or similar is escaped such that a crafted string cannot alter the
surrounding prompt structure (assert on the rendered prompt); removing the continuation check
fails the first test by name.

---

## E-2. Typed subtask message envelope **[pure]**

**Adopt the type even if teams are never built.** This is the item the user named directly.

**What it is.** A typed envelope for parent↔child and sibling messages: `author` and
`recipient` as validated `AgentPath` newtypes, message id, parent linkage, status, and
artifacts — replacing an XML string.

**What it fixes here.** The current shape is an XML string (`<task id state><task_result>`).
Typed `author`/`recipient` make sibling and child→parent messaging expressible **without
re-parsing prose**, and give errors somewhere structured to live.

**Also adopt now, near-zero cost.** codex's `AgentGraphStore` trait shape — four methods, an
`Open|Closed` edge status — as the persistence boundary. Small, and it makes the session tree
queryable instead of implied.

**Keep** the XML rendering as the *serialisation for the model*. The type is for the host; the
model still reads text. Do not conflate them.

**Refuse.** `encrypted_content` on inter-agent messages — it solves a hosted-product problem
with no local analogue.

**Rust surface.** A protocol/type module; the task tool's result rendering; an edge table in
session storage.

**Acceptance criteria (agent-executable)**: a child result round-trips through the typed
envelope with author, recipient, status and artifacts preserved; a malformed `AgentPath` is
rejected at construction, not at use; the model-facing XML is byte-identical to today's for an
unchanged scenario (proving this is a refactor, not a behaviour change); removing the newtype
validation fails a named test.

---

## E-3. Split `send` / `wake`, and make `wait` return no content **[pure]**

**A lesson already paid for by someone else** — this is codex's v1→v2 refactor.

**What it is.** Three verbs instead of one: `send_message {target, message}` queues without
running a turn; `followup_task {target, message}` queues and wakes if idle; `wait` blocks and
returns **no content**.

**Why the content-free `wait` is the important half.** It stops a coordinator from
accumulating every child's output in its own context — which is the mechanism by which
multi-agent designs quietly destroy the context budget they were adopted to protect.

**Refuse.** The three-valued delivery mode (`auto|steer|follow_up`) prime-agent shipped for
agent messages is now **dead code marked "accepted and ignored"** in their own source. Ship
steer-only for agent-to-agent.

**Acceptance criteria (agent-executable)**: `send_message` to an idle child does not start a
turn; `followup_task` does; `wait` returns a completion signal carrying no child output, proven
by asserting the coordinator's context does not grow by the child's result size; collapsing the
three verbs into one fails a named test.

---

## E-3b. Code-form delegation — the user's question, investigated properly **[crate], optional**

**Raised by the user 2026-08-13**: *"prime-agent 的任务委派、多 agent 协作是否可以参考？它应该是
使用代码的形式而不是工具 tool 调用"*. The user is **right about the mechanism**, and the summary
above understated it. This section records what investigation found, including the part that
reverses half the conclusion.

### The user is correct: prime-agent delegates in code, not tool calls

```python
# prime-agent-runtime/src/rlm/__init__.py:143
async def run(prompt: str, **kwargs: Any) -> RLMSpawnHandle:
    """Spawn a recursive Prime Agent child and return once its task is admitted."""
```

The model writes Python in a persistent IPython kernel; `rlm.run`, messaging, skills and
compaction are all Python-callable functions bridged over a Jupyter comm channel. Delegation is a
**function call in a language**, not a structured tool invocation. Calling it "the model's tool
surface" earlier was accurate but obscured that distinction.

### But prime-agent's code form does not deliver the main benefit of a code form

`rlm.run` **resolves when the child accepts the task, not when it finishes**, and returns only a
handle (`rlm_child_id`, `name`, `session_dir`, `model` — `rlm-runtime.ts:14`). So results
**cannot** flow back as return values; they arrive as agent-to-agent messages.

That matters because the theoretical prize of code delegation is result-side composition:

```python
results = [spawn(p) for p in prompts]
summary  = reduce(r.output for r in results)   # raw child output NEVER enters parent context
```

The second line is the real win — child output stays in the interpreter and only the reduction
costs context. **prime-agent cannot write it.** The research reached the same verdict
independently: the async-only result path is *"a design regression that should not be copied —
the absence of join, typed results, result timeouts, and error envelopes is a real defect,
visible in the prompt-level nagging that compensates for it."*

So the code form buys prime-agent **spawn-side** composition only: `[await rlm.run(p) for p in
prompts]` is fan-out with no scheduler, and loops or conditionals can decide *whether* to spawn.

### Where that leaves this project

Two facts reframe the question:

1. **`oc-tools/src/task.rs` is already closer to results-as-values than prime-agent is.** It is
   synchronous by default and returns the result (`task.rs:31`: *"foreground is a blocking wait
   the caller must opt out of"*). The thing prime-agent gave up to get code form, this project
   already has.
2. **The real gap is spawn-side composition**: fanning out N subtasks, or deciding what to spawn
   from a prior result, currently costs N model round-trips.

**Convergence evidence runs 4:1 against the code form.** All four other surveyed agents dispatch
through **one tool** — opencode `task`, codex `spawn_agent`, Claude Code `Agent`, ZCode `Agent`
(`research-cli-agents.md:518`). `openai/codex` is the most relevant comparison: Rust, ~140
crates, Apache-2.0, and it shipped **typed tools with an envelope**, not a code substrate. It had
the engineering budget to choose either.

### Substrate reality here

- **No embedded script engine exists.** Verified.
- **JS plugins discover an external `bun`/`node` on PATH** (`oc-plugin/src/js/runtime.rs:141`,
  `:35-36`). Unusable for a *core* control-flow feature, which must work without an external
  runtime.
- **wasmtime is a dependency but feature-gated**, deliberately kept out of every default build
  and enforced by `wasmtime_feature_gate.rs::wasm_runtime_is_absent_from_the_default_dependency_graph`.
  So an embedded execution substrate is **precedented but gated** — which is the shape any code
  substrate here would have to take.

### Two ways to close the real gap, with the honest tradeoff

**Cheap — batch fan-out tool [pure].** One tool call carrying N task specs, returning N typed
results. Closes parallel fan-out, which is the dominant composition case, with no interpreter, no
sandbox, no new failure modes. Composes with E-2's envelope. This gets most of the value.

**Expensive — embedded pure-Rust script engine [crate], feature-gated.** `rhai`, `rune`, or
`starlark-rust` (starlark is deterministic and I/O-free by design, which is the right safety
posture for model-authored code). Mirrors the wasmtime precedent: optional, off by default,
guarded. This buys what batching cannot — **data-dependent orchestration**: spawn based on a
prior child's result, loop until a condition holds, filter before returning.

**If the expensive path is taken, do the part prime-agent did not.** Make spawn return results
as values so reduce-in-code works, keeping raw child output out of the parent's context. That is
the actual prize, and it is available precisely because this project's `task()` is already
synchronous. Adopting the code form while copying prime-agent's fire-and-forget handle would take
on the cost and leave the benefit behind.

**Recommendation**: build the batch fan-out tool first and see whether data-dependent
orchestration is genuinely wanted before embedding an interpreter. The 4:1 convergence is not
proof that code delegation is wrong — CodeAct-style results are real — but it is evidence that
four teams solving this problem did not need it, and one team that did needed a Python runtime to
get there.

**Acceptance criteria (agent-executable), cheap path**: one tool call spawning N subtasks returns
N typed results with per-child status and errors; a partial failure does not discard successful
siblings; the parent's context grows by the reductions, not by the raw child outputs, asserted by
byte comparison; collapsing the batch into sequential single dispatches fails a named test.

**Acceptance criteria, expensive path (only if pursued)**: the engine is absent from the default
dependency graph, enforced the way `wasmtime_feature_gate.rs` enforces wasmtime; a script can
spawn, await results as values, reduce them, and return only the reduction, proven by asserting
parent context size; script execution cannot perform I/O outside the declared tool surface.

## E-4. Autonomous completion gates, with the livelock breaker **[pure]**

**The best single idea in prime-agent**, and independently the thing this project has been
doing by hand for fourteen review waves.

**What it is.** Completion is decided by **machine-checked shell commands**, not by the model
declaring done. Plus prime-agent's **git-worktree-unchanged livelock breaker**: if a
continuation produces no worktree change, stop rather than loop.

**Why it belongs here.** This project's entire review history is evidence for it — a subagent
claiming success while `cargo test` disagreed was the single most common failure across 183
todos. Encoding "done means the gate passed" removes a class of lying.

**Rust surface.** Config for gate commands; a continuation guard that runs them; a
worktree-dirty check between continuations. Composes directly with E-1's continuation.

**Acceptance criteria (agent-executable)**: a session whose gate command fails does not report
completion; a continuation that leaves the worktree unchanged terminates instead of looping,
asserted by a bounded test; removing the livelock check makes that test hang or exceed a bound.

---

## E-5. Skill catalog with a hard description cap and path-glob gating **[pure]**

**The hard part of skills is keeping unused ones out of context** — that is where the surveyed
designs actually differ, and Claude Code's answer is the most developed.

**What it is.** A skill = manifest (`id`, `title`, capped prose description, argument schema)
plus an executable reference. Discovery is a catalog of *descriptions only*; the body loads on
selection. Gate availability by path glob so irrelevant skills never enter the prompt.

**Adopt prime-agent's validation rule**: a skill entry is **invalid** without both a callable
reference and an `arguments` schema describing required fields, defaults, and constraints.
That check is worth keeping regardless of backend.

**Refuse.**
- **Python-package skills** — **[no]**. Bind a skill to a shell command or an in-binary tool.
- **Separate "commands" and "skills" concepts.** ZCode maintains both; Claude Code merged
  commands into skills and kept the old directory as an alias. **Build one mechanism.**

**Note on overlap**: this project already has a plugin system with 21 hooks and a governed tool
registry. Establish first whether a skill is a *new* concept here or a manifest over the
existing tool registry. The research does not settle that — the codebase does.

**Acceptance criteria (agent-executable)**: an unselected skill contributes only its capped
description to the prompt, asserted on the rendered prompt bytes; a skill whose path glob does
not match the session is absent entirely; a manifest lacking an `arguments` schema is rejected
at load with a named error; the cap is enforced by a test that would fail if a description grew.

---

## E-6. `/refine` — a reviewed, versioned, rollback-able diff over a supplemental prompt layer **[pure]**

**What it is.** CRUD over a supplemental prompt/memory layer where every mutation is a
reviewed diff with a versioned entry and a refinement audit log
(`RefinementEvent {id, trigger, changes[]}`), and every write re-reads before writing because
another process may have changed the file.

**Why it fits.** The backing store here is the existing SQLite, which is **better than
prime-agent's single JSON file**. The valuable part is the *write protocol* — reviewed,
versioned, reversible — not the store.

**Relationship to `oc-memory`**: this is the write-path discipline `oc-memory`'s batch-atomic
apply already gestures at. Check whether it is an extension of that crate rather than a new one.

**Acceptance criteria (agent-executable)**: every mutation produces an audit entry naming
trigger and changes; a rollback restores the prior version exactly; a concurrent modification is
detected rather than silently overwritten, proven by a test that mutates from two handles.

---

## E-7. Memory algorithms worth porting from TencentDB — **[pure]**, selectively

Only the parts expressible over SQLite/FTS5. Ranked by value; the first two carry most of it.

1. **A recall scoring formula** combining lexical match with recency and usage, rather than
   raw match order. Portable to FTS5 with no vector store.
2. **Watermark-based incremental consolidation**: a `last_processed` cursor per scope so
   consolidation cost is proportional to *new* content, not store size. Needs a composite
   index `(scope, updated_at)`. Conditional: only if a consolidation pass is added at all.
3. **LLM-judged dedup with a `store | update | merge | skip` verdict.** Architecturally
   SQLite-compatible: recall top-K≈5 candidates via FTS5, batch all pending writes plus
   candidates into **one** LLM call, apply the verdicts. **Only worth it if capture is
   automatic** — if the user authors memories explicitly, items 1-2 give most of the benefit
   without an LLM call on the write path.

**Refuse**: `sqlite-vec`/`vec0` — architecturally closer than a service, but a **C SQLite
extension that must be compiled or loaded**, which conflicts with a single self-contained
binary. **[no]** as a built-in; it belongs behind the existing embedding-plugin boundary if
anywhere. Also refuse the `embedding_meta` provenance table as a built-in — it matters only to
the optional embedding plugins, and belongs *inside* each plugin.

**Acceptance criteria (agent-executable)**: recall order changes measurably and correctly when
recency or usage changes, with a test pinning the formula; the watermark makes a second
consolidation pass touch only new rows, asserted by row counts.

---

## E-9. antigravity's `google_search` as a standalone optional plugin **[pure], self-contained**

**Raised by the user 2026-08-13**, then corrected by the user twice. Both corrections were right
and both are recorded here, because the second one changes the design.

### Correction 1 — I investigated the wrong version

My first pass grepped `opencode-antigravity-auth@1.2.8` and `@1.3.0`, found no `google_search`,
and concluded it did not exist. **The user pushed back**: *"我让你执行联网搜索时你就会调用这个工具"*.
They were right. `opencode.json`'s `plugin` array names **`opencode-antigravity-auth@1.6.0`**, and
`google_search` is there in full.

**Why I got it wrong is the part worth keeping.** I listed what the *cache* contained
(`ls .../*antigravity*`) and never read which version the *config* enables. Cache contents are not
runtime state. That is the same error shape as the twenty-three seams found in the main plan:
**I measured something adjacent to the truth rather than the truth.** Rule: to learn what is
loaded, read the config that loads it.

### What it actually is — a real registered tool, and a clever one

```js
// dist/src/plugin.js:1140
tool: { google_search: googleSearchTool }
```

Args are `query`, `urls?`, `thinking?` (default true) — the signature is identical to the tool
this session actually calls, which is what the user recognised.

The implementation is a deliberate workaround, and the reason is in the source:

```js
// dist/src/plugin/search.js:4
 * Due to Gemini API limitations, native search tools (googleSearch, urlContext)
 * cannot be combined with function declarations. This module implements a
 * wrapper that makes separate API calls with only the grounding tools enabled.
```

So it makes a **separate Gemini call** whose `tools` array holds only `{googleSearch:{}}` (plus
`{urlContext:{}}` when URLs are supplied) and **no function declarations**, then renders the
grounding metadata into Markdown with source citations.

Concrete facts, all verified:
- Endpoint `https://cloudcode-pa.googleapis.com` (`constants.js:30`), with daily/autopush
  sandboxes and a fallback list (`:28-35`).
- `SEARCH_MODEL = "gemini-2.5-flash"` (`:184`), `SEARCH_TIMEOUT_MS = 60000` (`:196`), and a
  dedicated `SEARCH_SYSTEM_INSTRUCTION` (`:200`).
- Auth is the plugin's cached OAuth token; on expiry it calls `refreshAccessToken`
  (`plugin.js:1117-1126`). **No API key.**
- Failure returns a plain string telling the user to run `opencode auth login`.

**The constraint that must be copied, not just noted**: grounding tools cannot coexist with
function declarations. Injecting `googleSearch` into the main conversation request would collide
with this project's tool declarations. **It has to be a separate request.** That is not an
implementation detail; it is why the wrapper exists.

### Correction 2 — the user wants it as a standalone plugin, and that is the right call

> *"如果实现可以作为一个独立的 plugin 进行实现不要影响主进程，后续也可以废弃"*

My earlier draft proposed folding grounding into `oc-provider-google`'s request path. **That was
wrong on two counts**: it would put a Gemini-only, OAuth-bound feature in the core request
builder, and it would be hard to remove later. A standalone plugin is better because:

- **The main process is unaffected.** No core code path changes; the binary works identically with
  the plugin absent.
- **It is discardable.** If Google changes the endpoint or the OAuth flow — plausible for a
  `cloudcode-pa` sandbox surface — the plugin is deleted, not untangled.
- **Gating already exists.** `websearch/mod.rs:3-8`: an unconfigured tool is **absent** from the
  tool list rather than present-and-failing. Without a Google OAuth credential this tool simply
  does not appear.

### The auth question the user raised — mostly already answered

> *"可能需要实现 gemini 或者反重力的认证登录方式，看看有没有相关 sdk 实现"*

**No SDK is needed, and no new login flow is needed.** Verified:

- `crates/oc-auth/src/provider.rs:63` already has `Credential::OAuth { refresh, access, expires }`
  — the exact shape antigravity stores.
- `/config/.local/share/opencode/auth.json` already holds a `google` entry of type `oauth` with
  `access`, `refresh`, `expires` (124-char refresh token, `1//0e…` — a Google refresh token).
  **This project already reads that store.**

So the plugin **consumes an existing credential**; it does not mint one. What remains is narrow:
- **Token refresh** when `expires` has passed — a standard Google OAuth refresh POST. Whether
  `oc-auth` already refreshes or only reads must be checked before assuming.
- **Project id extraction** from the refresh token's parts (antigravity's `parseRefreshParts`
  yields `managedProjectId || projectId`); the Cloud Code endpoint needs it.

**Only if a fresh login is ever wanted** would a device-code or loopback flow be needed — and that
is a separate, larger item. Do not build it as part of this.

### Dependency cost

**[pure]** — one HTTPS POST and JSON parsing, both already in the workspace. No SDK. No new crate.
The `oauth2` crate would only be justified if a full login flow is built later, and even then it
should be weighed against the existing hand-rolled auth code.

### Acceptance criteria (agent-executable)

- With a `google` OAuth credential present, `google_search` appears in the tool list and a real
  query returns cited results; with the credential absent the tool is **absent**, not failing.
- The outgoing request carries `{googleSearch:{}}` and **no** function declarations, asserted on
  the captured request body — the Gemini constraint is the thing most likely to be broken by a
  later refactor.
- `urls` supplied adds `{urlContext:{}}`; omitted, it does not.
- An expired token is refreshed once and the search retried; a refresh failure returns an
  actionable message naming the login command rather than a raw error.
- Removing the plugin leaves the binary and its test suite unchanged — proving the main process is
  genuinely unaffected.
- The 60s timeout is enforced and bounded, consistent with `websearch`'s existing bounded fetch.

### Substrate: DECIDED — native Rust behind a cargo feature

**User decision 2026-08-13**: *"我倾向于原生 rust 方式"*. Settled. The JS and WASM options are
dropped. A JS plugin would have reintroduced the external `bun`/`node` discovery
(`oc-plugin/src/js/runtime.rs:141`) that this project exists to avoid.

**Shape, copied from the `wasmtime` precedent** — which is the project's own proven pattern for an
optional heavyweight capability:

```toml
# crates/oc-plugin/Cargo.toml:12-14 — the pattern to follow
[features]
default = []
wasm = ["dep:wasmtime"]
```

So: a new module (or small crate) with `default = []` and a `google-search` feature, **absent from
every default build**, enforced by a structural test written the way
`crates/oc-plugin/tests/wasmtime_feature_gate.rs::wasm_runtime_is_absent_from_the_default_dependency_graph`
enforces wasmtime's absence. That test is the thing that makes "does not affect the main process"
checkable rather than asserted.

**No new dependencies.** `reqwest` (`Cargo.toml:90`) and `serde_json` (`:103`) are already
workspace dependencies. One HTTPS POST, one JSON parse. No SDK, no `oauth2` crate.

### One thing I verified that changes the work estimate

**`oc-auth` stores provider OAuth credentials but does not refresh them.** `Credential::OAuth
{ refresh, access, expires }` exists (`provider.rs:63-74`) and `expires` is stored, but the only
refresh machinery in the crate is on the **MCP** path (`mcp.rs:69` `refresh_token`), not the
provider path. So the plugin must perform its own refresh:

- POST to Google's token endpoint with the stored `refresh` token when `expires` has passed.
- Persist the new `access`/`expires` back through `AuthStore` so the next process does not re-refresh.
- Antigravity's own flow is the reference (`plugin.js:1117-1126` → `refreshAccessToken`), including
  extracting the project id from the refresh token's parts (`parseRefreshParts` →
  `managedProjectId || projectId`), which the Cloud Code endpoint requires.

This is the largest single piece of the item and it was invisible in the first pass. **Do not
assume `oc-auth` will hand back a fresh token.**

### Build order

1. **Read the credential and refresh it.** Prove a refresh round-trip persists through `AuthStore`
   before any search code exists — if this does not work, nothing else matters.
2. **The bounded POST and the grounding request shape**, with the no-function-declarations
   assertion, since that constraint is the one a later refactor will silently break.
3. **Response rendering** — sources and `urlsRetrieved`, matching antigravity's Markdown so output
   is recognisable to anyone who has used it.
4. **Registration and gating** — appear only when the credential exists, absent otherwise, using
   the existing `websearch` gating rather than a parallel mechanism.
5. **The feature-absence structural test**, last, so it pins a real surface.

## E-10. Keyless search backend — **already implemented; verify, do not build** **[pure]**

**Raised by the user 2026-08-13**: how does opencode's context7 work when the user has never
entered an API key, and can this project do the same?

### The answer: this project already does it

`crates/oc-tools/src/websearch/mcp.rs:55` takes the key as an **`Option`** and returns the bare
endpoint when it is absent:

```rust
pub fn exa_url(api_key: Option<&str>) -> String {
    let Some(key) = api_key else {
        return EXA_URL.to_owned();          // no key → plain https://mcp.exa.ai/mcp
    };
    ...append_pair("exaApiKey", key)
}
```

The mechanism generalises: both backends are **MCP servers reached over HTTP**
(`mcp.rs:3-4`: *"Both providers expose their search as an MCP `tools/call`, so one bounded POST
serves both"*), at `https://mcp.exa.ai/mcp` and `https://search.parallel.ai/mcp`. A public MCP
endpoint that accepts unauthenticated calls is exactly how context7 works too — the same shape,
a different host. **So there is nothing to design here.**

Two details already right, worth not breaking:
- **Gating, not failing** (`websearch/mod.rs:3-8`): an unconfigured backend makes the tool
  **absent** from the tool list rather than present-and-erroring. `web_search_enabled`
  (`gating.rs:199`) returns true for the hosted provider id even with no keys configured, so a
  hosted-provider session gets search without any user setup.
- **Deterministic per-session backend choice** (`gating.rs:209`) so a session does not alternate
  backends mid-conversation.

### What is actually left to do — verification, and it is small

The keyless path exists in code; what I did **not** verify is that it *works against the live
endpoint today*, and that is the only open question. Concretely:

1. **Does a keyless POST to `https://mcp.exa.ai/mcp` still succeed?** A public endpoint can start
   requiring a key at any time, and the failure would be a silent capability loss.
2. **If a keyless call is rejected, does the tool degrade honestly** — absent, or a clear error —
   rather than appearing available and failing mid-turn? That is the seam shape this project has
   found twenty-three times.
3. **Adding context7 itself** would then be config plus one endpoint constant, if its MCP server
   is public. Worth doing only if a keyless Exa call is confirmed working, since it is the same
   mechanism.

**Dependency cost [pure]** — no new code paths, no new dependencies.

**Acceptance criteria (agent-executable)**: a live keyless call is exercised and its outcome
recorded with the date (a network test must be opt-in, not part of the offline gate); if keyless
access is rejected, the tool is absent rather than failing, asserted by a test; any added backend
reuses `mcp.rs`'s single bounded POST rather than a second client.

---

## E-11. deepseek-harness findings — one principled contradiction, and a cheaper gate **[pure]**

**Requested by the user 2026-08-13**: analyse `deepseek-ai/deepseek-harness` (`dsh`) and its paper.
Full report: `.omo/research/deepseek-harness.md` (51 KB, source-grounded with file:line).

### First correction: it is not what the name suggests

**`dsh` is a production agent runtime, not an evaluation harness.** "Harness" here means *the thing
that wraps a model to make it act*. Not a SWE-bench rig, not an RL environment. I had assumed
otherwise when dispatching, and that assumption would have made the whole report unusable had the
researcher not established it first.

### The headline: a principled contradiction of "machine-checked completion gates"

E-4 and the Zuno strategy both rank machine-checked completion gates highly, on the strength of two
independent implementations (prime-agent, claw-code). **`dsh` is a fifth independent goal-loop
implementation that deliberately does NOT machine-check completion**, and what it builds instead is
better than nothing and cheaper than a test runner.

`packages/goal/tool-goal/src/index.ts:298-305` — the terminal transition runs no test, evaluates no
acceptance criteria, and calls no judge model. Completion is model self-report. **What is machine-
enforced is *who may make the claim*, not *whether it is true*:**

- **Compare-and-set on the goal revision** (`tool-goal/src/index.ts:145-154`): the model must pass
  the exact `revision` it read from `get_goal`; a stale revision is rejected. This defeats completing
  a goal the model has not re-read.
- **Authority tiers** (`authority.ts:19-22, 90-93, 101-108`): `create_goal`, `edit`, `pause`,
  `resume` require **direct human input** — a `user/message` whose `source.kind === 'user'` inside the
  current *root-agent* turn. **A model may never create, re-scope, or re-arm its own goal.**
  Subagents are structurally excluded (`authority.ts:71`).
- The doc comment names the attack it defends: *"An omitted `Agent.followup()` / `steer()` source
  resolves to `user`, so non-human producers must supply their own source rather than inheriting this
  authority."* That is a real hole, found and closed.

**This is the same philosophy as this project's "modes as permission rulesets a model cannot argue
with", applied to goal lifecycle** — and it extends it somewhere E-1 did not cover. E-1 already says
the agent must not set its own goal; `dsh` shows *how to enforce that structurally* rather than by
convention.

### The cheaper gate worth adopting first

**A structural completion contract**: require a terminal status to be *self-consistent with its own
fields*. `complete` demands ≥1 evidence item and zero remaining steps; `continue` demands ≥1 next
step and an empty blocker; `blocked` demands a non-empty concrete blocker. Reject the call otherwise.

Why this goes **before** E-4's evidence-execution gate rather than instead of it:
- It needs **no test runner, no acceptance criteria, no per-project config**, so it works on day one
  and on tasks that have no runnable test.
- It deterministically kills bare "Done!" and self-contradicting completions.
- The two compose. The researcher's own note: *"this is not a substitute for running tests. Adopt
  both: structure gate first (always applicable), evidence-execution gate second (applicable when a
  verify command exists)."*

**This is directly relevant to this project's own history.** Across 183 todos the single most common
failure was a subagent reporting success while `cargo test` disagreed — and several reported success
having produced *nothing at all* (FU-2's first dispatch: 5m46s, zero commits, zero evidence). A
structural contract would have caught the empty-handed ones without running anything.

### Corroboration ledger — what changes and what does not

| Prior finding | `dsh` verdict |
|---|---|
| Persisted user-owned goal with status and budget | **CORROBORATES — 5th independent implementation** (`GoalSnapshot { objective, phase, blockedReason?, maxGoalRounds }`) |
| Machine-checked completion gates | **PARTIALLY CONTRADICTS**, and supplies a cheaper gate that composes with it |
| Livelock breaker | **CORROBORATES — 3rd implementation, best design seen**: `repeat-tool-reminder`, thresholds `[3,5,8]`, canonicalised-arg keying, advisory, counts denials |
| Typed subtask envelopes | **CORROBORATES**: `RalphRoundReport { status, summary, evidence[], nextSteps[], blocker }`, schema-enforced, double-validated across the boundary |
| `wait` returns no content | **CORROBORATES from the other direction**: the parent never sees the child transcript; only ≤16 KB of validated JSON crosses |
| Collaboration modes as prompt text — REFUSED | **CONFIRMS THE REFUSAL**: `dsh`'s `plan-mode` is prompt text only, and its own source admits sandbox and approval are unenforced there |
| Python/Node substrate — REFUSED | No new evidence; `dsh` *is* Node, and its Code Mode needs a JS realm |
| Vector-DB memory — REFUSED | No new evidence; `dsh` ships no vector memory at all — the workspace filesystem is its long-term memory |

**Five independent implementations of goal-with-status.** E-1 stays first. But its justification
changes: the strongest reason is no longer "everyone gates completion" — it is that everyone
persists a user-owned objective, and the *gating* designs diverge.

### Ideas with no prior counterpart, ranked

1. **Structural completion contract** — above. **[pure]**. Adopt before E-4.
2. **Monotonic tool guards**: deny is absorbing and "allow" is **not representable**
   (`packages/core/tools/README.md:25,51`). A guard that cannot be widened by construction is
   stronger than one that must be checked. **[pure]**, and it matches this project's taste for
   compile-time impossibility — see todo 179's `E0004` arrival guard.
3. **`armed | disarmed` activation split** on the goal loop. **[pure]**. A persisted goal that is not
   currently driving is a distinct state from paused; E-1's status enum should account for it.
4. **Escalating repeat-call reminder** as livelock breaker: thresholds `[3,5,8]`, keyed on
   canonicalised arguments, advisory rather than blocking, and it **counts denials** so a model
   cannot spin against a permission wall. Better than claw-code's or prime-agent's. **[pure]**.
5. **Block-threshold floor**: a machine-enforced minimum round count before a model may declare
   itself `blocked` (default 3, `tool-goal/src/index.ts:290-297`). **Premature surrender treated as a
   first-class failure mode distinct from livelock** — nothing else surveyed had this, and this
   project has seen it (subagents stopping at their verification limit with work incomplete).
   **[pure]**.
6. **`SandboxEnforcement: full | partial`** — the sandbox **reports the limits of its own
   guarantee** (`docs/subsystems/sandbox.md`). Honest capability reporting rather than an implied
   promise. **[crate]**, and philosophically close to this project's gating-not-failing rule.

### Not worth adopting

- `dsh`'s completion model **as a replacement** for evidence execution. Its authority tiers are
  worth taking; its willingness to trust a self-reported `complete` is not, in a project whose
  review history is a catalogue of exactly that failure.
- Anything requiring the Node realm: Code Mode, workflow worker-threads.
- `plan-mode` as prompt text — its own source concedes the enforcement gap this project already
  closed with permission rulesets.

## E-12. "Everything is a plugin" — agent-authored plugins instead of skill guidance **[crate], feature-gated**

**Raised by the user 2026-08-13**: *"其设计理念类似『一切皆插件』是否可以参考，比如可以让 agent
自行创建插件加载执行相关工作流、loop 等而不是 skill 引导"*.

The distinction the user is drawing is real and sharper than E-5's skill catalogue: **a skill is
text that guides the model; a plugin is code that executes.** Asking the agent to author executable
capability is a different proposition from giving it better instructions.

### What the field actually does — six implementations, one paper, all Python

Web search surfaced a coherent cluster, and every one of them converges on the same pipeline:

| Project | Substrate | Validation before registration |
|---|---|---|
| `CorvinLabs/CorvinOS` (forge / skill-forge) | Python + bwrap sandbox, MCP | linter, policy, scope ladder, hash-chain audit |
| `tomasmihalyi/claude-forge` | FastMCP servers, Python | **11-check AST validator**, 3 retries, atomic registry |
| `kai-linux/hydra` | PydanticAI + MCP | validate, hot-load, self-healing retry on traceback |
| `OneManCrew/self-extending-agent` | Python | AST + banned patterns + subprocess import test |
| `ms1963/Codegen` | MCP, in-process | generate → register → invoke → inspect → remove |
| **SelfEvolve** (arXiv:2604.16314, SEAMS 2026) | Python, `importlib.reload()` | **TDD tests → intermediate adjudicator → unit tests → final adjudicator** |

**The convergent pipeline is: detect gap → generate → validate in a sandbox → register → use.**
Six independent teams built the same shape. That is a real signal about the *architecture*.

**The paper's evidence is thinner than its abstract implies.** 92.7% Pass@1 sounds strong, but it is
**11 tasks / 55 attempts**, in a **6-page** SEAMS short paper, on function-scale generation
("compute eigenvalues of this matrix"). It is preliminary feasibility evidence, not a validated
production design. Its genuinely transferable contribution is the **two-adjudicator TDD loop**:
generate tests first, run them, adjudicate; then generate *implementation-specific* unit tests, run
those, adjudicate again. Tests come before code and both gates execute.

### Two facts about this codebase decide the design

**1. The plugin substrate is already there and needs no runtime.** `PluginProcessSpec::new(name,
program)` (`crates/oc-plugin/src/jsonrpc.rs:72`) takes a name and an **executable path** — that is
the whole contract. Newline-delimited JSON-RPC over stdio, process-tree containment via
`oc_process::guarded_argv` (`jsonrpc.rs:203`). **An agent that writes an executable script has
written a plugin.** No Node, no Python, no MCP server needed. This is the one place where Zuno is
structurally *better positioned* than all six references, every one of which needs a Python realm.

**2. The tool registry is built once and cannot be extended mid-turn.** `RegistryCore` is assembled
in a single pass (`crates/oc-tools/src/registry.rs:270-288`): builtins, then
`config_directory_tools`, then `plugin_tools`, then `mcp_loader.tools()`. There is **no
`tools/list_changed`, no reload, no refresh** — I grepped for all three and found nothing.

That second fact is the load-bearing constraint. Claude-forge works around the same problem with a
`--call` CLI wrapper so a freshly forged tool is usable "in the same turn without waiting for MCP
registration". **Zuno has a cheaper answer available**: a forged plugin is an executable, and
`execute` already runs executables under permission governance. **Same-turn use needs no registry
mutation at all** — it is a governed process launch. Registration for *subsequent* turns is then a
config write, which the existing loader already picks up.

### Why this must not be built as an unguarded capability

This project's entire review history is the argument. Across 183 todos, the most common failure was
a subagent claiming success against evidence — and **E-11 just established that DeepSeek's
production runtime deliberately forbids a model from creating or re-arming its own goal**, enforcing
it structurally through authority tiers (`authority.ts:90-93`). Letting a model author *executable
code that then runs* is strictly more dangerous than letting it set its own objective.

So the design constraints are not negotiable:

- **Feature-gated and off by default**, following the `wasmtime` precedent — absent from the default
  dependency graph and enforced by a structural test, the way
  `wasmtime_feature_gate.rs::wasm_runtime_is_absent_from_the_default_dependency_graph` does it.
- **Validation before registration, and the validation must execute**, not merely lint. SelfEvolve's
  two-adjudicator loop and claude-forge's 11-check AST validator are the two references worth
  copying. A generated plugin that has not run its own tests is not a plugin, it is a guess.
- **Governed like any other tool.** A forged plugin's tools must pass the same permission layer as
  builtins — the property todo 144 established and a named test already guards
  (`a_plugin_tool_is_hidden_by_the_same_permission_layer_as_builtins`).
- **Human authority on promotion.** Session-scoped by default; persisting a plugin beyond the
  session requires explicit user action. This is E-11's authority-tier lesson applied to capability
  rather than to goals. CorvinOS's scope ladder and claude-forge's `/tool save` are both this idea.
- **Auditable.** CorvinOS feeds every forge event into a hash chain. Zuno has SQLite; an append-only
  record of what was forged, from what prompt, and what it was allowed to do is cheap here.

### Where the substrate choice lands

**The generated artifact should be a JSON-RPC plugin executable, not a script for an embedded
interpreter.** Reasons, in order:

1. It requires **no new runtime** — the strongest fit with the single-binary rule.
2. Process-tree containment already exists (`oc-process`).
3. It is language-agnostic: the agent can emit whatever the host can execute, and on a host with no
   interpreter it can emit a shell script.

The alternative — an embedded pure-Rust script engine (`rhai`, `rune`, `starlark-rust`) — was already
weighed in **E-3b** for code-form delegation and deferred there. The same reasoning applies, with one
addition: `starlark-rust` is deterministic and I/O-free by design, which is the right safety posture
for model-authored code, and would let a forged *workflow* (the user's "loop") run without spawning
a process. **That is the stronger option specifically for loops and workflows**, where spawning a
process per iteration is wasteful. Worth revisiting if forged workflows become the dominant use.

### Relationship to E-5 (skills) — the user's actual question

The user asked whether plugin authoring should replace skill guidance. **Answer: they solve different
problems and both are worth having, but plugin authoring is the more valuable of the two here.**

- A **skill** is text in the context window. It costs context on every turn it is active, cannot be
  tested, and its effect is advisory. CorvinOS's own comparison table is honest about this: skills are
  "markdown knowledge" that "runs in LLM context", guarded only by a "linter".
- A **plugin** is code. It costs nothing until invoked, **can be tested before it is trusted**, and
  its effect is deterministic.

For a coding agent whose failures are overwhelmingly "claimed something without checking", the
capability that can be *verified before use* is worth more than the one that can only be *read*.
E-5 stays, at lower priority; this item supersedes the ambition behind it.

### Sequencing — not now, and here is the honest reason

This is a **large** item with a real blast radius: code generation, sandboxed execution, a validation
pipeline, a promotion ladder, and an audit trail. It depends on machinery that does not exist yet:

- **E-1's goal loop** — a forge that cannot be bounded by a goal and budget is an unbounded process.
- **E-11's structural completion contract** — the cheap gate that would reject a forge claiming
  success with no evidence.
- **E-4's evidence-execution gate** — the thing that actually runs the generated tests.

**Build E-1, E-11's structure gate, and E-4 first.** They are individually valuable, each `[pure]`,
and together they are the substrate that makes agent-authored plugins safe rather than exciting.
Attempting this before them would produce the most dangerous shape available: a model writing and
running code with no bounded objective and no evidence requirement.

### Acceptance criteria, when it is built (agent-executable)

- The forge capability is **absent from the default dependency graph and default builds**, enforced
  by a structural test in the `wasmtime_feature_gate.rs` style.
- A forged plugin **cannot be registered until its generated tests execute and pass**; a plugin whose
  tests fail is rejected with the failure surfaced, and a test proves the rejection.
- A forged plugin's tools pass the **same permission layer** as builtins, proven by extending the
  existing `a_plugin_tool_is_hidden_by_the_same_permission_layer_as_builtins` guard to a forged tool.
- Promotion beyond session scope **requires direct user action**; a test proves a model cannot promote
  its own forge.
- Every forge event is recorded append-only with prompt provenance, and a test proves the record
  cannot be silently skipped.
- Removing any one of these guards fails a named test.

## E-8. Deferred, with reasons

- **Agent teams (peer topology, shared task list)** — the only shipped peer-to-peer design is
  file-based, and Claude Code's teammate display needs tmux **[no]**. Adopt the *hook surface*
  only, if anything. Defer until parallel *writers* prove to be a real workflow rather than a
  demo.
- **`isolation: worktree` for writing subagents** — worth wanting, expensive to do right.
- **Cheap lexical skill pre-selection (BM25 / char-ngram / LRU)** — build the harness, ship it
  dark until there are enough skills for selection to matter.
- **Explicit team create/delete tools** — deleted by the only project that shipped them.
- **A general memory/context "blackboard"** — not present in any surveyed design, and
  prime-agent's family-reach + no-broadcast policy is evidence they consciously avoided it.
  Do not invent it.
- **Collaboration modes as prompt text** — this project already encodes modes as permission
  rulesets (`plan` denies `edit`), which cannot be talked out of by a model. Prompt-text modes
  are strictly weaker.
- **`meta_assets` lifecycle columns** — the columns exist in TencentDB's schema but nothing
  reads them. Adopting `last_used_at`/`usage_count` for eviction is reasonable, but it would be
  *designing*, not porting. Treat as an original idea and justify it on its own merits.

---

## Working rules, carried from the main plan

- **Mutate to verify, and mutate the right thing.** A registry deletion and a behavioural break
  catch different things. Beware equivalent mutants.
- **Back up with `&&`, never `;`.** Check `df -h /`; stop above 90%.
- **Never `git add -A`; never combine `-A` with `-f`.**
- **Prose nothing derives from goes stale.** If you write a number, make something check it.
- **Question test inputs, not only assertions.**

## Research artifacts

`/tmp/opencode/research-prime-agent.md` (46 KB), `research-tencentdb-memory.md` (41 KB),
`research-cli-agents.md` (79 KB). Source-grounded with file:line citations; each states its
own evidence class (source vs documentation) and each project's language, licence and activity.
**Copy them into `.omo/research/` before `/tmp` is cleared** if any of this is pursued.
