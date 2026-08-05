---
slug: opencode-rust
status: review-converged
intent: clear
review_required: true
review_outcome: CONVERGED at Round 3 of 3 (hard cap). 10 admitted blockers + 1 admitted regression, all closed. Final plan SHA-256 b0277b6bbb04d30d47e97c2210dee8395f6d65475a5228226ed41412bc74a101, 1011 lines, 97 implementation todos + F1-F4.
round3:
  momus:
    verdict: NOT CONVERGED (on revision 1a53b450)
    result: "B2 CLOSED. B5 CLOSED. R1 STILL OPEN - todo 1 retained stale '31-crate roster' text on its References line and 'scaffold 25-crate rust workspace' in its commit message. Fixed immediately: both lines now read 33."
  codex:
    verdict: CONVERGED (on the post-fix revision b0277b6b, 1011 lines)
    result: "ROUND 2 VERDICT header aside, R3 output verbatim: 'ROUND 3 VERDICT: CONVERGED / B2 CLOSED / B5 CLOSED / R1 CLOSED'. No admitted regression. Exit code 0."
    independence: "Round 3 explicitly forbade reading .omo/drafts/opencode-rust.md, correcting a Round-2 contamination where Codex had seen momus's verdict inside the draft before answering."
    sandbox_note: "This container's bubblewrap is broken (bwrap: loopback: Failed RTM_NEWADDR: Operation not permitted; setting up uid map: Permission denied), so Codex's default Linux sandbox fails every file read. Resolved by keeping --sandbox read-only and switching the enforcement backend via --disable use_linux_sandbox_bwrap --enable use_legacy_landlock. The read-only constraint was never relaxed and no bypass flag was used."
  ordering_note: "momus judged B2/B5 on revision 1a53b450; the subsequent fix touched only todo 1's References and Commit lines, which cannot affect B2 (todos 38/40/72) or B5 (todo 86 / SC17). Codex then re-verified all three on the final revision, so the closure set holds for the delivered artifact."
  convergence_basis: "/dual-review rule 6 - a round that produces no new admission-gate-passing blocker converges. Round 3 produced none from either link; both links agree all three items are closed on the final revision."
plan_path: .omo/plans/opencode-rust.md
review_round_id: round-2
round2:
  momus:
    verdict: NOT CONVERGED
    closed: [B1, B3, B4, B6, B7, B8, B9, B10]
    still_open:
      B2: "Todos 38 and 40 still required truncation and ordinary-timeout killing, contradicting Todo 72's refusal and background-promotion semantics."
      B5: "Todo 86's executable allow-list still omitted the execute parameter-contract divergence that Success Criterion 17 requires."
    admitted_regression:
      R1: "Todos 95-96 target crates oc-provider-bedrock and oc-provider-google, but Todo 1 enforced an exact 31-crate roster excluding both - the roster gate would have blocked the very todos the B4 fix created. Passes all three admission clauses (specific, in scope, not a preference)."
  codex:
    status: running
  revisions_applied_after_round2:
    B2: "Todo 38 now owns size DETECTION + overflow persistence only and explicitly asserts nothing about what the model receives; Todo 40 now owns cancellation + the hard kill ceiling only and explicitly asserts nothing about the ordinary foreground timeout; Todo 72 is named as sole owner of both user-visible policies, in all three todos."
    B5: "The allow-list became a machine-readable docs/divergences.toml with seven declared entries; the compat suite loads it, asserts the exact count, and asserts the execute tool's live parameter schema matches its divergence entry."
    R1: "Crate roster corrected 31 -> 33 (oc-provider-bedrock, oc-provider-google added), with the count updated in the Crate layout section, Todo 1's title, its What-to-do, and its acceptance criterion; the acceptance now compares against a committed crates.expected fixture."
review_mechanism: /dual-review (strict convergence - 3 round hard cap, Round 1 full then delta-only, blocker admission gate, disputes default to pass)
frozen_baseline:
  plan_sha256: 207c8b187f9336ed0170e76a5c7d0524501ff8bda99fb12ccf33a8a3a5505159
  plan_lines: 955
  plan_bytes: 223293
  draft_sha256: caa92cc463094037fc898c51c0cb613f88acd3e19ee57287af9003c7a7412d09
  codex_cli: 0.146.0 - all four mandated flags present at mandated positions; read-only sandbox smoke test returned SANDBOX_OK exit 0
  disk: / 238G avail (18% used), /config 282G avail - the earlier "disk full" risk is definitively stale
  refs: /tmp/ulw-refs/{jcode 366M, claw-code 21M, codex 136M, omo-slim 49M} all present
pending-action: collect the independent review, merge into the Round 1 ledger, minimally revise, then run Round 2 delta-only with momus + codex CLI
review:
  momus:
    status: complete
    verdict: BLOCK
    round_id: round-1
    blockers_admitted: 3
    result: |
      B-R1-1 HTTP compatibility gate is both unsatisfiable and insufficient.
        Todo 52 / 86 / Success Criterion 4 require the /api path+method set to EQUAL upstream's,
        while Todo 85 ADDS C8 maintenance endpoints that are absent from the divergence allow-list -
        so the gate can never pass. Inversely the gate only compares paths and schemas, so all 61
        handlers could return stubs with wrong side effects and still pass.
        Fix: upstream endpoints become a required SUBSET; C8 extensions explicitly allow-listed and
        documented; add an oracle-driven request/status/body/side-effect contract matrix per group.
      B-R1-2 Wave 7 and Wave 12 mandate opposite tool behavior.
        Todo 38 requires oversized output to be TRUNCATED with overflow persisted; Todo 72 requires
        it to be REFUSED pending accept_large_output. Todo 40 requires a timed-out child to be
        KILLED; Todo 72 requires an ordinary foreground timeout to leave it RUNNING in background.
        Both pairs of acceptance tests cannot be green against one implementation.
        Fix: Todo 38 tests only size detection + overflow persistence; Todo 40 tests cancellation and
        the hard kill ceiling only; the user-visible refusal and timeout-promotion semantics belong
        exclusively to Todo 72, and the output-policy divergence from upstream gets recorded.
      B-R1-3 G1-G4 thresholds can be fitted to the finished implementation.
        The TS fraction, absolute ceilings, soak turn count/duration, slope bound, and watchdog
        timeout are all chosen in Wave 14 AFTER Rust is measured; "demonstrates a real reduction" is
        not an executable bound. A run can observe its own results and pick values that pass, leaving
        memory exhaustion and hangs unproven - i.e. the motivating problem.
        Fix: freeze the FORMULAS and workload parameters before measuring Rust (fraction, aggregation
        method, repetitions, minimum soak duration and turn count, slope formula and bound, watchdog
        timeout). Wave 14 may only substitute the independently measured TS baseline into them.
      Non-blocking notes: spot-checked >10 citations across waves 2,3,4,7,9,12,13,14 - config merge,
      permission findLast/visibility, session FK structure, turn loop, tool registry, MCP transport
      order, plugin closure contract, /api/session/active, and theme layers all support their claims.
      The real-counterpart requirements for MCP/LSP/ACP do correctly prevent fixture-only validation.
      G5 and G6 are concrete; the loophole is specifically G1-G4's unfrozen pass criteria.
  round2:
    momus:
      status: complete
      verdict: NOT CONVERGED (2 still open, 1 admitted regression)
      plan_sha256_reviewed: 4b8ba36c9d7f5464d34117077741d31c712b870d0161ee20b5e6d03eae480a66
      result: |
        CLOSED: B1, B3, B4, B6, B7, B8, B9, B10 (8 of 10).
        STILL OPEN B2 - todos 38 and 40 still asserted truncation and ordinary-timeout killing,
          contradicting todo 72's refusal and background-promotion semantics. I had rewritten todo 72
          to claim ownership but had not stripped the contradicting assertions from 38 and 40.
          FIXED after this round: todo 38 now owns size DETECTION + overflow persistence only and
          explicitly asserts nothing about what the model receives; todo 40 now owns cancellation +
          the hard kill ceiling only and explicitly asserts nothing about the ordinary timeout.
        STILL OPEN B5 - success criterion 17 listed the `execute` divergence but todo 86's
          executable allow-list did not, so the gate itself never checked it.
          FIXED after this round: the allow-list is now a machine-readable `docs/divergences.toml`
          with seven enumerated entries, an asserted count, and a test comparing the `execute` tool's
          LIVE parameter schema against its divergence entry.
        ADMITTED REGRESSION (self-inflicted by the B4 fix) - splitting the provider work created
          todos 95 and 96 targeting `oc-provider-bedrock` and `oc-provider-google`, but todo 1's
          exact-roster gate listed 31 crates excluding both, so those todos could never build.
          FIXED after this round: roster is 33 crates with both named; todo 1's count and its
          `crates.expected` fixture updated; the "31 crates" phrasing purged.
  independent:
    status: complete
    verdict: DO-NOT-START-AS-WRITTEN
    round_id: round-1
    result: |
      Found 7 in-scope findings Momus missed, 3 of which I independently verified against the real
      tree (see "Round 1 verification" below). Its scope-cut recommendation (headless-first) is
      REJECTED - the user explicitly ruled out MVP subsetting - but its ordering insight is admitted.
      Named the plan's genuine strengths: contract-first differential testing (schema/journal
      round-trip against the real binary), the single event-emitting turn loop as the core boundary,
      and whole-process-tree memory/liveness validation. All three are to be preserved in any rework.

## Round 1 verification — I checked the three load-bearing subagent claims myself
Subagent output is a CLAIM until verified. Three mattered enough to check, and all three hold:

1. **`middlewareStack.add` is NOT on `PluginInput.client` — my own draft was wrong.** `sdk-client.js:18` constructs `new CodeWhispererStreamingClient({...})` and `:33`/`:40` call `client.middlewareStack.add` on **that AWS client**, to inject `x-amzn-kiro-agent-mode` and the per-family effort wire shape. Meanwhile the plugin's only use of the opencode SDK client is `client.tui.showToast` (`plugin.js:46`). So the Metis-sourced claim I recorded — that the compat host must hand plugins an SDK client carrying `middlewareStack` — is **false**, and Todo 60's acceptance criterion built on it would have sent an implementer chasing a property that does not exist on that object. Corrected below.
2. **Provider coverage: `provider.ts:107-134` bundles 23 distinct SDK factories**, including `@ai-sdk/amazon-bedrock` (+`/mantle`), `@ai-sdk/anthropic`, `@ai-sdk/azure`, `@ai-sdk/google`, `@ai-sdk/google-vertex` (+`/anthropic`), `@ai-sdk/openai`, `@ai-sdk/openai-compatible`, openrouter, xai, mistral, groq, deepinfra, cerebras, cohere, gateway, togetherai, perplexity, vercel, alibaba, gitlab, github-copilot, venice — plus per-provider `custom()` loaders with genuinely different behavior (`azure` model selection `:154-160`, bedrock-mantle routing `:162-166`, copilot endpoint choice `:225-239`). Bedrock needs SigV4 + EventStream, Google its own wire format, Vertex GCP auth. **A single "OpenAI-compatible profile" cannot cover them**, so Todo 30 as written could not deliver what C3 promises.
3. **`execute` really is `{ code: string }`.** `code-mode.ts:12-20`: id `execute`, description "Run a confined orchestration script with access to connected MCP tools.", one parameter `code` = "Script body executed by the confined interpreter." The jcode-shaped replacement is therefore a **model-facing contract change** and must be allow-listed as an intentional divergence, not left implicit.

## Round 1 ledger — 10 admitted blockers, 3 downgraded
Admission gate applied per `/dual-review`: specific+falsifiable AND in-scope AND not a selection preference. Source noted; "both" means both links converged.

| id | source | blocker | minimal fix |
| --- | --- | --- | --- |
| B1 | momus | HTTP gate is simultaneously unsatisfiable and insufficient: Todo 85 adds C8 `/api` endpoints while Todo 52/86/SC4 demand the path+method set **equal** upstream's, and C8 is not allow-listed; inversely the gate compares only paths+schemas, so all 61 handlers could be stubs and still pass | upstream endpoints become a required **subset**; C8 extensions allow-listed; add a per-group request/status/body/side-effect contract matrix driven by the real binary |
| B2 | momus | Waves 7 and 12 mandate **opposite** behavior: Todo 38 truncates oversized output, Todo 72 refuses it; Todo 40 kills a timed-out child, Todo 72 leaves it running. Both test pairs cannot be green against one implementation | Todo 38 tests size detection + overflow persistence only; Todo 40 tests cancellation + the hard kill ceiling only; refusal and timeout-promotion semantics belong solely to Todo 72, recorded as a divergence |
| B3 | both | G1-G4 thresholds AND their derivation formulas are chosen in Wave 14 **after** Rust is measured; "demonstrates a real reduction" is not an executable bound, so a run can fit the gate to its own results — leaving the motivating failure unproven | freeze methodology and formulas **before** measuring: TS fraction, aggregation, warm-up, sampling rate, repetitions, sliding window, slope estimator and bound, watchdog timeout, hard turn deadline. Wave 14 may only substitute the measured TS baseline into them |
| B4 | oracle (verified) | C3 promises upstream's full provider set but Todo 30 covers Bedrock/Google/Vertex through one "OpenAI-compatible profile" — impossible for SigV4+EventStream, Gemini's wire format, or Vertex auth | split into provider **families** with their own todos (Bedrock, Google/Vertex, Azure) or narrow C3's claim; do not leave the contradiction |
| B5 | oracle (verified) | The `execute` contract change (`{code}` → structured sub-calls) is a model-facing divergence absent from Todo 86's allow-list, so the compat suite should fail by definition | add it to the allow-list with its reason; keep the jcode design (user's directive) but declare it |
| B6 | oracle (verified) | Todo 60's acceptance demands `client.middlewareStack.add` succeed on the opencode SDK client — that property belongs to the plugin's internal AWS client, not `PluginInput.client` | replace with: trigger a real Kiro provider request and assert the AWS middleware took effect; correct the draft's claim |
| B7 | oracle | Todo 60 must prove an interactive `readline` prompt does not deadlock "a running TUI", but the TUI arrives at Todo 73 — the todo cannot pass under the one-green-commit rule | W10 depends only on an abstract terminal-lease interface, verified with a fake owner (acquire/return/timeout); the real `bun`/`node` readline ↔ ratatui pty integration test moves to Todo 73 |
| B8 | oracle | The rollback promise is unbounded: the journal round-trip proves parity with TS `1.18.13` only. A newer TS binary applying newer migrations leaves the Rust binary unable to open its own database | add a max-known-migration guard that refuses (with a message) rather than corrupting; scope the documented rollback promise to pinned version pairs; recommend a pre-switch backup |
| B9 | oracle | G6's "zero orphans after abnormal termination" cannot be delivered by Rust destructors — `SIGKILL` runs none. As written the test would measure the harness cleaning up, not the product | require OS-level containment: `prctl(PR_SET_PDEATHSIG)`/cgroup on Linux, process groups, Job Objects on Windows; test the guarantee, not the harness |
| B10 | oracle | Four todos violate the plan's own "one commit, compiles green, implementation+tests together" rule: Todo 30 (two OpenAI APIs + several non-OpenAI providers), Todo 52 (61 endpoints + OpenAPI parity), Todo 60 (npm install + dual entrypoint + closures + native addons + TTY + loopback + SDK), and Wave 12's TUI todos (184 keybinds, 33 themes, all views) | split by provider family, HTTP group, plugin-lifecycle stage, and TUI route/view |

**Downgraded (not gating), with the gate clause that downgraded each:**
- *Three-tier → two-tier plugin architecture; defer WASM.* Selection preference (gate clause 3). WASM is already behind an off-by-default cargo feature, so it costs nothing in the default build. Recorded as Follow-up.
- *Headless-first vertical slice; defer TUI/ACP/omo/goal/C8.* Out of scope (gate clause 2) — the user explicitly rejected MVP subsetting, and I asked the question flagging that. Its **ordering** insight is admitted inside B3: the TS baseline and a staged memory gate move earlier so the architecture is validated before the most expensive waves.
- *JS sidecar needs an RSS cap, timeouts, restart policy, and a supported-plugin matrix.* Real but already largely covered: G1/G2 count the **whole process tree**, so a bloated sidecar fails those gates. Folding the explicit cap in anyway as a non-gating improvement.
approval: granted - user replied "按你推荐" (adopt all recommended options) and ADDED scope: session maintenance (global session listing + age-based prune, global and per-project). Q1=A, Q2=A, Q3=A. Approval authorizes writing the plan file only.
plan_progress: COMPLETE - 92 implementation todos across 14 waves, plus F1-F4. All 16 Metis blockers applied. TL;DR, commit strategy, and success criteria filled.
open_questions_round2: RESOLVED - user replied "可以按你推荐" plus two scope refinements. Q4=A (report 1.18.13 for the plugin compatibility gate, real build identity exposed separately). Q5=A with a refinement: omo is treated as BUILT-IN and ported natively, but SLIMMED, using oh-my-opencode-slim as the reference for what to drop. Q6 overridden by the user: do NOT skip the execute tool and do NOT copy the TS acorn+typescript interpreter - model it on jcode's implementation instead.
approach: Build a new Rust workspace at /config/workspace/ProdDir/AI/opencode-rust that is a drop-in replacement for the opencode CLI/agent/server pinned to v1.18.13 @ aefaf140c1 - same config/agent/skill/command/MCP/permission file contracts, same CLI command surface, same /api HTTP surface, same on-disk state (auth.json / opencode.db / snapshots) - implementing every contract in its CURRENT form only (no deprecated aliases; they are rejected with an error naming the replacement, per the user's 无需兼容老特性 directive) - with (a) an unsafe-free Rust-first plugin system plus a JS compat host so the user's existing auth plugins keep working, (b) omo's agent + category->model/reasoning-effort delegation ported natively, and (c) a codex-/goal-style durable goal tool with a human-editable Markdown projection. Cloud/web/desktop/console/enterprise/stats packages are explicitly out of scope.
---

# Draft: opencode-rust

## Components (topology ledger)
<!-- Lock the SHAPE before depth. One row per top-level component that can succeed or fail independently. -->
<!-- id | outcome (one line) | status: active|deferred | evidence path -->
| id | outcome (one line) | status | evidence path |
| --- | --- | --- | --- |
| C1-compat-core | Config/agent/skill/command/permission/instructions resolution byte-compatible with opencode | active | packages/core/src/v1/config/config.ts:32-190; packages/opencode/src/config/config.ts:39-584; packages/opencode/src/config/paths.ts:10-40 |
| C2-session-engine | Agentic turn loop + message/part model + SQLite persistence + compaction, same DB schema | active | packages/opencode/src/session/prompt.ts:1052-1347; session/processor.ts:98-683; packages/core/src/session/sql.ts:22-176 |
| C3-provider-llm | Multi-provider streaming (SSE), reasoning/thinking effort mapping, prompt-cache stability, auth.json | active | packages/opencode/src/provider/provider.ts:101-134; provider/transform.ts:721-1410; auth/index.ts:10-89 |
| C4-tools-integrations | 17 built-in tools + MCP client (stdio/HTTP/SSE) + 39 LSP servers + ripgrep/PTY/tree-sitter replacements | active | packages/opencode/src/tool/registry.ts:96-247; mcp/index.ts:212-370; lsp/server.ts |
| C5-extensibility | Rust-native plugin ABI + cross-language JSON-RPC plugins + JS compat host covering all ~20 Hooks | active | packages/plugin/src/index.ts:56-80,222-335; packages/opencode/src/plugin/loader.ts:76-235 |
| C6-agents-goal | omo-style built-in agents, category->model/effort delegation, task continuation, codex-style goal tool | active | omo dist/index.js:24659-24818 (category table), :136363-136410 (task schema), codex ext/goal/* |
| C7-interfaces | CLI command surface + HTTP server (61 /api/* + compat routes) + ratatui TUI + ACP | active | packages/opencode/src/index.ts:45-103; protocol/src/groups/*.ts; packages/opencode/src/acp/agent.ts:32 |
| C8-session-maintenance | NEW capability opencode lacks: cross-project global session listing + age-based retention prune (global and per-project) with cascade to every on-disk artifact | active | packages/core/src/session/sql.ts:22-177; packages/core/src/project/sql.ts:6-36; packages/core/src/database/schema.sql.ts:3-10 |

## Open assumptions (announced defaults)
<!-- Record any default you adopt instead of asking, so the user can veto it at the gate. -->
<!-- assumption | adopted default | rationale | reversible? -->
| assumption | adopted default | rationale | reversible? |
| --- | --- | --- | --- |
| Compat baseline | Pin to opencode 1.18.13 @ commit aefaf140c1, branch dev | Verified shipping version in packages/opencode/package.json; a moving target makes "compatible" untestable | yes |
| Which generation to mirror | External behavior of packages/opencode (the shipping bin); internal semantics informed by core/llm/schema (Effect) | packages/opencode/package.json bin.opencode is the released binary and it *depends on* core/llm/schema; core has not replaced it (still has src/v1/) | yes |
| Async runtime + HTTP | tokio 1 multi-thread + reqwest 0.12 with rustls only (no OpenSSL) | Both jcode and claw-code converged here; rustls avoids native TLS build pain | yes |
| Error handling | Typed domain errors (thiserror) at layer boundaries, NOT anyhow-everywhere | jcode's anyhow-everywhere forces recovery decisions by string-matching error text (turn_loops.rs:132, anthropic lib.rs:1884); claw-code has the same defect via classify_error_kind | yes |
| Tool parameter schemas | schemars derive on a typed params struct as single source of truth, + central schema augmentation pass | Both references hand-write JSON schemas separately from the deserialize struct and silently drift; claw-code has 55 such pairs | yes |
| Turn loop count | ONE event-emitting loop; CLI/TUI/server are all consumers | jcode's two parallel loops (turn_loops.rs 1193 lines vs turn_streaming_mpsc.rs 1720) have already diverged on mid-stream cancel | no (structural) |
| UI decoupling | Core emits events over a channel; no rendering inside provider/tool code | claw-code welds stdout writes into ApiClient::stream and ToolExecutor::execute, which is why it has no server mode and no ACP | no (structural) |
| Storage engine | SQLite (rusqlite or sqlx) reusing opencode's exact schema | Required for session-resume compatibility; also gives conversation search that jcode had to hand-roll (session_search_index.rs) | no (compat-bound) |
| Cancellation primitive | Port jcode's epoch-guarded InterruptSignal (sync-readable + async-awaitable), not tokio_util CancellationToken | CancellationToken cannot express reset-without-erasing-a-newer-cancel; jcode issue #428 documents the exact bug, with 2000-iteration race hammers | yes |
| SSE UTF-8 | One incremental Utf8StreamDecoder used on EVERY provider path | jcode fixed this (issue #609) but left String::from_utf8_lossy per chunk on Anthropic + Copilot, corrupting CJK/emoji at TCP boundaries - directly relevant for a Chinese-language user | yes |
| unsafe policy | `unsafe_code = "forbid"` at workspace level | User requirement; claw-code proves it is achievable at 100k LOC for exactly this domain (rust/Cargo.toml:15) | no (requirement) |
| Compaction | LLM-summarized, with tool-pair-aware boundary walking | claw-code's statistical summary ("3 user / 4 assistant, tools: bash") destroys usable context; its boundary logic is still worth porting (compact.rs:123-166) | yes |
| Credentials file mode | auth.json / mcp-auth.json written 0600 | opencode already does this (auth/index.ts:65-80); claw-code forgot it (oauth.rs:283-285, no set_permissions) | no (security) |
| Tool concurrency | Sequential by default + explicit parallel batch tool, capped | Matches opencode's current behavior and avoids write-conflict hazards; jcode caps at 10 (batch.rs:10) | yes |
| Repo layout | New standalone Rust workspace in this directory; upstream TS repo untouched, consumed read-only as the oracle | opencode-rust/ currently contains only .omo/; no git repo yet | yes |
| Language of code artifacts | Code/commits/docs in English; all user-facing reporting in Chinese | Global AGENTS.md constraint | yes |
| Legacy/deprecated surface (user directive: 无需兼容老特性) | Implement ONLY the current, non-deprecated form of every contract. Deprecated aliases are rejected with an actionable error naming the modern replacement — never silently ignored | User explicitly waived legacy compat. Erroring beats ignoring: a silently-dropped `mode.*` block would look like a working config that changes nothing | yes |
| v1 HTTP/SDK surface | Serve `/api/*` only, EXCEPT the minimum v1 endpoints the JS plugin compat host actually calls (measured, not guessed) | Dropping v1 wholesale would break the `client` object handed to `antigravity-auth`/`kiro-auth`; measuring the real call set keeps the surface honest | yes (depends on Q2) |
| Legacy data migration | No reader for the old JSON `storage/**` layout; SQLite `opencode.db` only | 1.18.13 already writes SQLite as the primary store; the JSON layout exists for migration from much older versions | yes |

## Findings (cited - path:lines)

### Scale of the thing being replaced
- 32 packages, **3,213 source files, 562,493 LOC** of TS/TSX (generated code excluded). Largest: `opencode` 668 files/176,094 LOC, `app` 558/130,013, `core` 475/67,227, `tui` 204/31,729, `llm` 105/20,526, `schema` 71/3,644.
- Runtime-required closure for a local CLI/agent: `opencode`, `core`, `llm`, `schema`, `plugin`, `protocol`, `server`, `sdk`, `tui`, `codemode`, `effect-drizzle-sqlite`, `effect-sqlite-node`, `http-recorder` — **~370K LOC**.
- Not required for a local CLI (cloud/UI): `app`, `desktop`, `session-ui`, `ui`, `storybook`, `web`, `console`, `enterprise`, `function`, `slack`, `stats`, `containers`, `docs`, `identity` — **~385K LOC**, cleanly severable.
- Baseline: branch `dev`, `git log -1` = `aefaf140c1 sync release versions for v1.18.13`, version `1.18.13` in `packages/opencode/package.json`.

### v1/v2 generation question — settled
- `packages/opencode/package.json` is `name: "opencode"`, `version: 1.18.13`, `bin.opencode = ./bin/opencode` and **depends on** `@opencode-ai/core`, `llm`, `schema`, `server`, `tui`.
- `packages/core/src/tool/registry.ts:40` names its service `@opencode/v2/ToolRegistry`; `core` still ships `src/v1/` and `opencode` still imports `PermissionV1`. Migration is in-flight, not complete.
- `packages/cli/package.json` publishes bin **`lildax`**, not `opencode`, and its command surface (`$`, `api`, `debug agents`, `migrate`, `service`, `serve`) is not a wrapper of the current CLI. It is a separate next-gen shell.
- Conclusion: mirror the **external behavior of `packages/opencode` 1.18.13**; borrow internal semantics from `core`/`llm`/`schema`.

### Config contract (the compat brain)
- Source of truth is **Effect Schema, not Zod**: `packages/core/src/v1/config/config.ts:32-190` — 30+ top-level keys (`$schema, shell, logLevel, server, command, skills, references, reference, watcher, snapshot, plugin, share, autoshare, autoupdate, disabled_providers, enabled_providers, model, small_model, default_agent, subagent_depth, username, mode, agent, provider, mcp, formatter, lsp, instructions, layout, permission, tools, attachment, enterprise, tool_output, compaction, experimental`).
- **10-layer merge order**, later overrides earlier, `instructions` de-dup-concatenates instead (`packages/opencode/src/config/config.ts:39-50`): well-known remote → global (`config.json` → `opencode.json` → `opencode.jsonc`) → `OPENCODE_CONFIG` → ancestor-to-cwd `opencode.json(c)` → config dirs incl. `.opencode` per level → `OPENCODE_CONFIG_CONTENT` → hosted org config → system managed dir → macOS managed preferences (overrides everything) → `OPENCODE_PERMISSION` + legacy `tools` mapping.
- Variable substitution: `{env:VAR}` and `{file:path}` (absolute, config-relative, or `~/`), file content trimmed + JSON-escaped, tokens inside comment lines skipped (`config/variable.ts:33-90`).
- Instructions: global `$GLOBAL_CONFIG/AGENTS.md` then optional `~/.claude/CLAUDE.md`; project tries `AGENTS.md` → `CLAUDE.md` → deprecated `CONTEXT.md`, stopping at the first filename class; `instructions[]` accepts globs, `~/`, and HTTP(S) URLs with local concurrency 8 / remote 4 / 5s timeout (`session/instruction.ts:60-168`).

### Agents
- Defined via config `agent.<name>` (legacy `mode.<name>` → `mode:"primary"`) or markdown under `{agent,agents}/**/*.md` and deprecated `{mode,modes}/*.md` in every config dir; name derives from the relative path, body becomes `prompt` (`packages/opencode/src/config/agent.ts:11-58`).
- Fields (`packages/core/src/v1/config/agent.ts:7-40`): `model, variant, temperature, top_p, prompt, tools (deprecated), disable, description, mode(subagent|primary|all), hidden, options, color, steps, maxSteps (deprecated), permission` — **plus any unknown key, which is swept into `options` and passed through to the provider**. So `reasoningEffort`/`thinking` are not first-class fields; they ride in `options` (`agent.ts:43-80`).
- Legacy boolean `tools` maps to permissions, with `write`/`edit`/`patch` all collapsing to `edit`; explicit `permission` applies after and wins.
- Built-ins: `build, plan, general, explore, compaction, title, summary` (`packages/opencode/src/agent/agent.ts:140-265`). New user agents default to `mode:"all"`.
- `task` tool params (`packages/opencode/src/tool/task.ts:43-62`): `description, prompt, subagent_type, task_id?, command?, background?` — depth-checked against `subagent_depth` (default 1), gated by `permission:"task"` with the pattern = `subagent_type`, and `background` requires `OPENCODE_EXPERIMENTAL_BACKGROUND_SUBAGENTS=true`.

### Skills / commands / permissions
- Skill discovery, in order: `~/.claude/skills/**/SKILL.md`, `~/.agents/skills/**`, project `.claude`/`.agents` walked upward, `{skill,skills}/**/SKILL.md` in every config dir, `skills.paths[]`, `skills.urls[]` (fetch `<url>/index.json`, each entry `{name,files,version?}` and must list `SKILL.md`). Frontmatter recognizes only `name` (required) and `description` (`skill/index.ts:21-25,53-59,173-227`; `skill/discovery.ts:13-131`).
- Command resolution order: built-in `init`/`review` → `cfg.command` → MCP prompts → skills (only if the name is free). So config overrides built-ins, MCP overrides config, skills never override (`command/index.ts:65-152`). Templates use `$1..$N` and `$ARGUMENTS`.
- Permissions evaluate with **`findLast`** — the last rule whose permission *and* pattern both wildcard-match wins; no match defaults to `ask` (`permission/index.ts:28-107`). Replies: `reject` (also rejects sibling pendings), `once`, `always` (installs runtime allow rules and auto-clears covered pendings). A tool that is fully `deny` with pattern `*` is **hidden from the model's tool list** (`permission/index.ts:204-219`).

### Plugins — the hard constraint
- Public API is an in-process JS/TS function: `Plugin = (input: PluginInput, options?) => Promise<Hooks>` where `PluginInput` carries `client` (the SDK HTTP client), `project`, `directory`, `worktree`, `serverUrl`, and `$` (a **Bun shell**) (`packages/plugin/src/index.ts:56-80`).
- `Hooks` has ~20 members (`packages/plugin/src/index.ts:222-335`): `dispose, event, config, tool, auth, provider, chat.message, chat.params, chat.headers, permission.ask, command.execute.before, tool.execute.before, tool.execute.after, shell.env, tool.definition, experimental.chat.messages.transform, experimental.chat.system.transform, experimental.session.compacting, experimental.compaction.autocontinue, experimental.text.complete, experimental.provider.small_model`.
- A second-generation API exists at `packages/plugin/src/v2/{effect,promise}/**` (agent, tool, command, skill, event, catalog, aisdk, registration, integration...).
- Discovery: config `plugin: (string | [string, opts])[]` plus auto-discovered `{plugin,plugins}/*.{ts,js}` in every config dir; npm plugins are installed on demand and version-gated, file plugins skip the gate (`config/plugin.ts:18-59`; `plugin/loader.ts:76-235`).
- **THE CONSTRAINT**: this machine's `plugin` array is `opencode-antigravity-auth@1.6.0`, `@sunerpy/opencode-kiro-auth@0.18.0`, `@sunerpy/oh-my-openagent@4.21.0` (`/config/.config/opencode/opencode.json:87-92`). The first two are `auth` hooks that *create the providers the user actually runs on* (`kiro-auth/claude-opus-5-max` etc.). A Rust binary with no JS plugin path cannot authenticate to the user's primary models.

### MCP / LSP / native deps
- MCP local = stdio child (`cwd` relative to workspace, env = `process.env + environment`); remote tries Streamable HTTP then SSE; OAuth on by default unless `oauth:false`; tool names namespaced via `McpCatalog.toolName(server, tool)`; tools-changed notifications refresh the cache (`mcp/index.ts:212-370,461-471,666-688`).
- **39 built-in LSP servers** with probing, download, and process supervision (`packages/opencode/src/lsp/server.ts`); JSON-RPC over stdio via `vscode-jsonrpc` StreamMessageReader/Writer (`lsp/client.ts:123-260`).
- Native deps a Rust port must replace: ripgrep (with auto-download), tree-sitter Bash/PowerShell WASM (for shell-command permission analysis in `tool/shell.ts`), PTY (`bun-pty` / `@lydell/node-pty`), SQLite, `@parcel/watcher`, FFF native search, OpenTUI renderer, `git`, shell discovery (`bash/zsh/fish/pwsh/powershell/cmd.exe`), LSP server processes. `fzf` NOT confirmed as a dependency (UNVERIFIED).

### Storage layout (must match for session reuse)
- XDG: data `$XDG_DATA_HOME/opencode`, cache `$XDG_CACHE_HOME/opencode`, config `$XDG_CONFIG_HOME/opencode`, state `$XDG_STATE_HOME/opencode`, logs `.../log`, downloaded bins `$XDG_CACHE_HOME/opencode/bin`, repos `.../repos` (`packages/core/src/global.ts:10-43`).
- SQLite `$XDG_DATA_HOME/opencode/opencode.db` (channel-suffixed in dev; `OPENCODE_DB` override incl. `:memory:`), WAL + NORMAL sync + 5s busy timeout + FK on (`core/src/database/database.ts:22-55`). Tables `session, message, part, todo` with `message.data`/`part.data` as JSON text; v2 adds `session_message, session_input, session_context_epoch` (`core/src/session/sql.ts:22-176`).
- Snapshots are a real Git object store at `$XDG_DATA_HOME/opencode/snapshot/<projectID>/<hash(worktree)>` (`snapshot/index.ts:66-75`). Auth `auth.json` 0600, MCP auth `mcp-auth.json` 0600, models cache `models.json`, large tool output under `tool-output/`.

### HTTP + CLI + ACP surface
- Two API generations served together: **61 endpoints under `/api/*`** (from `packages/protocol/src/groups/*.ts`, OpenAPI auto-generated) plus the older compat surface (`/global/*`, `/config`, `/provider`, `/project`, `/session/*` per `SessionPaths` at `httpapi/groups/session.ts:78-105`, `/find`, `/file`, `/vcs`, `/mcp`, `/pty`, `/permission`, `/question`, 13 `/tui/*` control endpoints, `/sync/*`, `/experimental/*`, `GET /doc`, UI catch-all).
- Default TUI topology is **one Bun process, two units**: main thread renders; a `Worker` hosts the server; requests are marshalled over a custom RPC with the pseudo-URL `http://opencode.internal` — no TCP unless `--port`/`--hostname` is given (`cli/cmd/tui.ts:24-49,198-249`; `cli/tui/worker.ts:23-58`). Relevant to the memory complaint: server + renderer share one heap.
- CLI: globals `--help/-h, --version/-v, --print-logs, --log-level, --pure`, `completion`; ~23 top-level commands with subgroups (`providers|auth`, `agent`, `session`, `mcp`, `github`, `console`, `db`, `debug`) — full flag-by-flag inventory captured in research.
- **ACP** (`@agentclientprotocol/sdk`) is implemented at `packages/opencode/src/acp/agent.ts:32` with 13 methods — this is how Zed-class editors attach. It is a compat axis I had not anticipated.
- Server auth is HTTP Basic gated on `OPENCODE_SERVER_PASSWORD` (username default `opencode`); only enforced when a non-empty password is set (`server/auth.ts:17-47`).
- `Flag` reads ~37 `OPENCODE_*` env vars (`packages/core/src/flag/flag.ts:3-78`); CLI also sets `AGENT=1`, `OPENCODE=1`, `OPENCODE_PID`.

### omo agent design (to port natively)
- Agent registry (`omo dist/index.js:165979-165991`): `sisyphus, hephaestus, oracle, librarian, explore, multimodal-looker, metis, momus, atlas, sisyphus-junior`. Temperatures are explicit and low (0.1 for most subagents, 0.3 for metis); permission deny-lists are per-agent; models come from `AGENT_MODEL_REQUIREMENTS` fallback chains (`:24504-24640`), overridable by `/config/.omo/omo.jsonc`.
- **The canonical category table** is `CATEGORY_MODEL_REQUIREMENTS` at `omo dist/index.js:24659-24818` — 8 categories (`visual-engineering, ultrabrain, deep, artistry, quick, unspecified-low, unspecified-high, writing`), each a *fallback chain* of `{providers[], model, variant}` where `variant` is the provider's reasoning tier. This is the core artifact to port.
- `task` schema (`:136363-136372`): `load_skills[], description?, prompt, run_in_background?, category?, subagent_type?, task_id?, command?`. Dispatch rules (`:136191-136258`): `category` forces `subagent_type = "sisyphus-junior"`; naming `sisyphus-junior` directly is rejected; coordinator agents cannot be task targets; missing both category and subagent_type errors.
- Override precedence (`:136040-136072`): `agentOverride.model ?? agentCategoryModel`; `agentOverride.variant ?? resolution.variant ?? agentCategoryConfig.variant`.
- Claude thinking budget helper injects `{thinking:{type:"enabled",budgetTokens:32000}}` for older Claude, and deliberately returns `{}` for Opus 4.7+/Fable/Mythos so native reasoning variants take over (`:28829-28836`).
- Continuation: `task_id` is a `ses_...` OpenCode session id and reuses that session (history is NOT rebuilt or copied); background tasks additionally get a `bg_...` id. One continuation can keep the same `ses_` while getting a fresh `bg_` (`:133292-134680`).
- Prompt assembly order (`:132955-133079`): agents/plan context → skill fragments → category `prompt_append`, truncated skills-first when over `max_prompt_tokens` (24,000 for free/local models).
- `.omo/` state: `omo.jsonc`, `plans/<name>.md`, `notepads/<name>/*.md`, `boulder.json` (schema_version 2), `run-continuation/`, `rules/`.

### codex /goal (to port as the goal tool)
- **Not document-backed.** Authoritative state is SQLite `~/.codex/goals_1.sqlite`, table `thread_goals`, `thread_id` PRIMARY KEY (one goal per thread), columns `goal_id, objective, status, token_budget, tokens_used, time_used_seconds, created_at_ms, updated_at_ms`; status CHECK in `active|paused|blocked|usage_limited|budget_limited|complete`.
- Objective is one opaque string capped at 4,000 chars (`MAX_THREAD_GOAL_OBJECTIVE_CHARS`). Over the cap, the TUI spills to `~/.codex/attachments/<uuid>/goal-objective.md` and replaces the objective with the literal pointer sentence `Read the Codex goal objective file at <path> before continuing.`
- **Compaction survival works by never being in the context**: the goal is injected each turn as a hidden pseudo-user message `<codex_internal_context source="goal">...</codex_internal_context>`, and compaction's `parse_user_message` drops contextual fragments outright. After compaction the thread goes idle, `on_thread_idle` fires, and a *fresh* `continuation.md` is rendered from SQL with live counters.
- **Self-restarting loop**: idle + `status=active` → `try_start_turn_if_idle`, guarded against a running turn, Plan mode, queued user input, and held under a per-thread semaphore. A one-shot `thread_goal_continuation_deferrals` row suppresses auto-continue right after fork/resume.
- Three tools: `get_goal` (no params), `create_goal` (`objective`, `token_budget?`; SQL-guarded so it can only replace a *complete* goal), `update_goal` (`status` enum limited to `complete|blocked` — `paused/usage_limited/budget_limited` are system/user-owned only).
- Completion is model-asserted under a long **completion-audit rubric** re-injected every turn (requirement-by-requirement evidence, "treat completion as unproven", anti-scope-shrink "Fidelity" block, and a **three-consecutive-turns** threshold before `blocked`). The only hard mechanical stop is the token budget (SQL flips status to `budget_limited`).
- Safety detail worth copying: any terminal turn error → status `blocked`, explicitly to stop auto-continuation from looping on compaction failures and burning tokens.
- Companion `update_plan` is a **pure event emitter with zero persistence** — ephemeral within-turn checklist; the goal is the durable north star. `continuation.md` tells the model to use both.

### jcode (686K LOC Rust, 82 crates) — what to take
- Take: epoch-guarded `InterruptSignal` (+2000-iteration race hammers); `RetryRollback` stream event so a partial attempt is discarded; **five-way reasoning representation** (`Reasoning`, `ReasoningTrace` never replayed, `AnthropicThinking{signature}`, `OpenAIReasoning{encrypted_content}`, `ToolUse.thought_signature` for Gemini-3); split system prompt (`complete_split(system_static, system_dynamic)`) + memory as trailing user message + `CacheTracker` + `locked_tools` with one-shot MCP rebuild (four mechanisms, one goal: never invalidate the cached prefix); central `ensure_intent_in_schema` injecting a required `intent` and an `accept_large_output` escape hatch; dependency-inverted provider factory registry with all wiring in one function; `resolve_tool_name` + Levenshtein "did you mean"; `Utf8StreamDecoder`; dev-profile `opt-level=3` pinning for `ratatui`/`unicode-*`; `ToolContext::for_subcall`.
- Avoid: two parallel turn loops (already diverged); a 70-field `Agent` god struct; 82 crates where 3 hold 63% of the code; anyhow-everywhere forcing string-matched recovery; a ~30-method `Provider` trait; hand-written JSON schemas; 275-file TUI with no component model; `from_utf8_lossy` per chunk on 2 provider paths; `cfg!(test)`-conditional runtime behavior; unbounded `ServerEvent` channel.
- `unsafe`: 252 hits, all FFI/platform (Win32, objc2 menu bar, `dlsym("mallinfo2")`, libc, `mallopt`). Agent/tool/provider/TUI logic is entirely safe. A port without a macOS menu bar and jemalloc introspection can be 100% safe.

### claw-code (115K LOC Rust, 11 crates) — what to take, and the cautionary tales
- `unsafe_code = "forbid"` at workspace root, **4 grep hits and zero unsafe blocks**. Proof the requirement is achievable.
- Take: `ConversationRuntime<C: ApiClient, T: ToolExecutor>` — two tiny traits make the whole loop unit-testable with zero terminal/HTTP deps; **argument-derived permission classification** (same tool, higher privilege when the path escapes the workspace); tool-pair-aware compaction boundary; `ProviderCapabilityReport` per-model feature matrix with a `PassthroughAsTool` third state and coded diagnostics naming the env var to set; session JSONL hygiene (secret redaction + 16KB field truncation + 256KB rotation); canonicalize-first workspace-fingerprinted session store with `workspace_root` embedded; deterministic mock-provider parity harness capturing every request; multi-ecosystem command/skill discovery across `.claw/.omc/.agents/.codex/.claude`.
- **Cautionary tales that change the plan**: MCP stdio is implemented with LSP-style `Content-Length` framing instead of spec NDJSON — non-functional against every real MCP server, **with green tests, because the fixtures share the bug**. Sandbox advertises `filesystemMode`/`allowedMounts` in the tool schema and enforces neither (passes them as env vars). Hand-rolled `JsonValue` with `i64`-only numbers used for config parsing, in a crate that already depends on `serde_json` — any float in settings fails to parse. Credentials written without 0600. UI welded into transport and tools, hence no server mode and no ACP.
- Lesson folded into the plan: every wire protocol must be validated against a **real** counterpart, not self-authored fixtures.

### Severability claim — verified directly (not taken on trust)
`packages/opencode/package.json:34-95` declares exactly these first-party deps: `core`, `http-recorder`, `script`, `codemode`, `llm`, `plugin`, `protocol`, `schema`, `sdk`, `server`, `tui`. **Zero** dependencies on `app`, `ui`, `session-ui`, `web`, `console`, `desktop`, `enterprise`, `stats`, `storybook`, or `slack`. `packages/server/package.json:15-16` depends only on `core` + `protocol`. A grep for imports of any excluded package inside `packages/opencode/src` returns nothing. So the ~385K-LOC severance is real.

**One refinement**: `packages/tui/package.json:51-54` does depend on `@opencode-ai/ui` — but the only usage is six **audio asset files** (`@opencode-ai/ui/audio/*.mp3` at `packages/tui/src/attention.ts:17-22`, typed by `src/audio.d.ts:6`). No UI code crosses. The Rust TUI needs those notification sounds (or equivalents) as static assets; that is an asset copy, not a package port. Recorded so nobody "discovers" a blocking dependency in Wave 11.

### Metis gap analysis — 12 blockers, all folded into the plan
Session `ses_032961f22ffeHQV4d3MUcAfkjj`. It verified every load-bearing assertion against the real tree at `aefaf140c1` and against the user's three actually-installed plugins. What it confirmed as sound: the failure-injection shape of the Wave 1 QA scenarios; the "no self-authored-fixture-only protocol tests" gate; the typed-retry argument; the interrupt primitive choice; `opencode debug paths` really existing (`cli/cmd/debug/index.ts:79-87`, defined inline); the 12-item deprecation list being drift-free across all three places it appears; all `/tmp/ulw-refs` reference paths resolving.

**Blockers, and the resolution each forced:**

- **B1 — a real dependency cycle 51→64→57→51.** Not a typo: the user's plugins call `client.tui.showToast` (antigravity ×6, kiro ×1, omo ×25), and `/tui/*` control endpoints live in the interfaces wave. Three waves would have waited on each other. **Fix: interfaces move before extensibility; a listening server with `/tui/*` is a hard precondition of the plugin host.**
- **B2 — the `migration` table is an unrecorded on-disk contract, and it silently voids the rollback promise.** `packages/core/src/database/migration.ts:19-40`: opening a DB that already has a `session` table calls `applyOnly(db, migrations)`, which reads completed ids from a `migration` table and replays anything missing, wrapped in `Effect.orDie`. `migration.gen.ts` holds **38 migrations** (`20260127222353_familiar_lady_ursula` … `20260622202450_simplify_session_input`). A Rust-created DB without that journal prefilled makes the real TS binary replay 38 migrations onto an already-current schema **and die**. **Fix: the journal is now an explicit Wave 4 contract with a differential acceptance test — create with Rust, open with the real binary, assert it does not die and the journal row count is unchanged.**
- **B3 — there are 19 tables, six of them cloud-side.** `workspace, data_migration, account_state, account, control_account, credential, event_sequence, event, permission, project_directory, project, message, part, session_context_epoch, session_input, session_message, session, todo, session_share`. The cloud ones are created in the same `schema.up(tx)` and ALTERed by some of the 38 migrations, so "don't create them" is only safe if the journal is right. **Fix: explicit create-all decision recorded, with the reason.** (`credential` is the v2 integration path, not an `auth.json` replacement — it does not affect C3.)
- **B4 — the v1 HTTP surface is 67 routes, `/api/*` is zero of them, and `client` cannot be an HTTP shim.** The SDK bundled inside the plugins (`@opencode-ai/sdk@1.18.10`) exposes 67 unprefixed v1 routes and no `/api/*`. Worse: `client.middlewareStack.add` (`kiro/dist/plugin/sdk-client.js:33,40`) **is not a route at all** — the plugin installs request middleware on the client object it is handed. **Fix: C5 now specifies the object contract (a real SDK client instance with `middlewareStack`), and the "measured minimum" becomes its own pre-plugin todo producing a route→plugin→callsite ledger plus an unknown-route error-and-account mechanism, because each plugin bundles its own SDK and can change the needed set on upgrade.**
- **B5 — the plugin version gate.** `plugin/loader.ts:127` calls `checkPluginCompatibility(target, InstallationVersion, pkg)`; an npm plugin declaring a supported opencode range is **skipped entirely** on mismatch (file plugins bypass it). Todo 6's QA explicitly accepted a `--version` difference — which would get both auth plugins rejected. **Fix: the binary reports `1.18.13` for compatibility purposes, with its own build identity exposed separately; the Todo 6 wording is corrected.**
- **B6 — the non-functional gates were decoration.** No RSS number, no soak duration or turn count, no workload definition, **no hang/liveness assertion at all** despite "卡死" being half the motivation, and no gate against a real large session (cassettes are small and reproduce neither big context reads nor big tool output). Worse, the draft itself already identified the TS mechanism (one Bun process, renderer and server sharing a heap) that the Rust architecture eliminates by construction — so a cassette soak proves nothing about the original fault. **Fix: concrete numbers, a liveness watchdog assertion, and a gate that opens the largest session in the user's real `opencode.db`.**
- **B7 — C8 promised protection that the draft had already proven impossible.** Busy state is in-process (`session/run-state.ts:35-74`). **Fix: the draft's liveness mechanism (probe `/api/session/active`, else a `time_updated` safety window crossable only with `--include-recent`) is now in the plan, and the unachievable "running" guarantee is gone.**
- **B8 — "CLI surface identical" collides with Scope OUT on ≥5 commands.** `index.ts:87-99` registers `console`, `web`, `stats`, `github`, `pr` — all excluded — and `upgrade`/`uninstall`/`export`/`import`/`generate` had undefined semantics. **Fix: a per-command disposition table (implement / reject with a message / do not register).**
- **B9 — Todo 1's acceptance was unsatisfiable.** The crate list's 11th entry was the wildcard `oc-provider-*`, and Todo 5 adds a 26th crate. **Fix: the crate roster is now enumerated exactly, provider crates named individually.**
- **B10 — an entire second JS execution path was missing.** `tool/registry.ts` scans `{tool,tools}/*.{js,ts}` in config dirs and dynamically imports each, registering exports matching `isPluginTool`. And plugin tool args are **Zod objects** converted by `zodJsonSchema` (or `legacyJsonSchema` as fallback) — which conflicts with "schemars is the only schema source" on that branch. **Fix: both are now explicit scope items with a stated coexistence rule.**
- **B11 — nobody owned npm fetching, dual entrypoints, or native addons.** `PluginKind = "server" | "tui"` (`plugin/shared.ts:37`) means two entrypoints per plugin; kiro@0.18.0 pulls **native N-API modules** (`libsql`, `@neon-rs/load`, `node-gyp-build-optional-packages`, `detect-libc`) and uses **`node:readline/promises`** (interactive OAuth prompts on stdin) plus `node:http` (loopback listener). The readline one is a **runtime deadlock class**: the compat-host child wants the terminal while the Rust TUI holds it. **Fix: all four are named scope items with an explicit stdin/TTY ownership protocol.**
- **B12 — omo's disposition was undecided, and it uses `$`.** The draft said omo would be ported natively, but it is still in the user's `plugin` array, and its dist really does reference `input.$` (Bun shell). **Fix: decided — load it through the compat host (so `$` is load-bearing) with native-port duplicate suppression; documented as a user-visible decision rather than a silent break.**

**Moderates folded in:** W11 was hiding a whole TUI replacement in one todo (split into three waves, TUI ≥4 todos); the "Can parallelize with" column contradicted Depends/Blocks in three rows (rewritten); `debug paths` only prints 9 keys so most of Todo 4 had no oracle (extra oracles added, and `global.ts:34-42` eagerly mkdirs 7 directories — a deliberate divergence a differential test cannot catch, now recorded); `database/path.ts:5-24` normalization (`storagePath`, `absolute` which throws on non-POSIX-absolute, `toPlatform`) applies to `session.directory`/`session.path` and was missing from Wave 4; LSP servers are **38**, not 39; the four conditional tools' gating depends on a `flags.client` concept that was never named; `subpath` remains an upstream live no-op needing an explicit decision.

### Metis round 2 — four more blockers, one of which is a whole subsystem
Session `ses_0329356c0ffe33BpuoQc66la63`, run against the real tree and the installed plugin dists.

- **B13 — `codemode` is missing from the plan entirely.** `tool/registry.ts:114,221,300-303`: the `execute` tool is `codeMode.CodeModeTool`. `packages/codemode/` depends on `acorn` + `typescript` and contains `src/interpreter/`. My own draft listed `codemode` inside the runtime closure (draft:56), yet `## Scope` never mentions it and none of the 13 waves has a todo for it. **Implementing this means a JS/TS interpreter sandbox in Rust.** That is not a todo; it is a component, and it needs a scope decision (Q6 below) rather than a silent absorption.
- **B14 — the entire TUI configuration contract is outside the main config schema.** `theme` and `keybinds` do not appear in `core/src/v1/config/config.ts` at all; they live in `packages/tui/src/config/{index.tsx,keybind.ts}`: **184 keybind entries**, **33 built-in theme JSONs** (`tui/src/theme/assets/`), plus `attention{enabled,notifications,sound,volume,sound_pack,sounds}`, `leader_timeout`, `prompt`, `scroll_speed`, `scroll_acceleration`, `diff_style`, `mouse`, `max_height/max_width`. None is in todo 7's key list and none has a todo. Theme resolution itself is four-layered (built-in + custom + plugin + system, `theme/index.ts:166-185`).
- **B15 — the auth-hook closures cannot cross a process boundary, which fixes the compat host's architecture for us.** `condition` is `(inputs: Record<string,string>) => boolean` (`plugin/src/index.ts:102-103,115-116`) and its replacement `when` is serializable data `Rule { key, op, value }` (`:82-86`) — but in the same prompts object, `validate?: (value) => string|undefined` is **not** deprecated, and `loader`/`authorize` are closures too. They *are* the substance of an auth hook. So the JS compat host **cannot be a serialization bridge**; it must keep a resident JS runtime holding callable handles. Metis's own minimal fix for the B1 cycle follows from this: bind the plugin's `client` to **in-process service calls**, not loopback HTTP, and the `64(partial)` edge disappears without reordering anything. (Fallback if HTTP semantics are insisted on: split the HTTP todo into a skeleton carrying just the measured methods, placed before the plugin wave.)
- **B16 — `AuthOuathResult` cannot be rejected, and my plan contradicts itself about it.** It is a pure type alias (`plugin/src/index.ts:220`) with zero runtime presence. `## Scope` → Must NOT have claims every deprecated form is "detected and rejected with an error naming its modern replacement"; todo 10's ten-item list correctly omits it. The Must-NOT-have wording is wrong and must be narrowed to runtime-detectable forms.

**Measured, and worth more than an assumption:** the two auth plugins together use exactly **6** v1 SDK methods — `client.auth.set`, `client.session.abort`, `client.session.messages`, `client.session.prompt`, `client.provider.oauth`, `client.tui.showToast`. `condition:` appears **0 times** in either dist, so the deprecation is latent rather than immediately breaking. Note the last method is a TUI control endpoint — plan:44's "no pre-`/api/*` endpoints beyond the measured minimum" would casually delete it.

**Also newly named as missing from Scope:** formatter *execution* (todo 18 only parses the config; `packages/opencode/src/format/{index,formatter}.ts` post-edit formatting has no todo); `.opencode/plans/<created>-<slug>.md` writing (`session/session.ts:331-335`) as a deliverable rather than a prune footnote; the conditional tools' `flags.client` gating (`registry.ts:202,242`); and seven CLI commands — `export`, `import`, `upgrade`, `uninstall`, `stats`, `completion`, `github`/`pr`. Note `cli/cmd/stats.ts` reads `@opencode-ai/core/session/sql` directly, so "the `stats` package is excluded" does **not** cover it. W8's "39 built-in LSP servers" is also three subsystems (probe + download + supervise) compressed into one todo, and it is 38, not 39.

**Confirmed sound in round 2:** severability holds — the only runtime→excluded-package reference in the whole tree is those 6 `.mp3` imports; the 40 apparent cross-package hits are `packages/tui/src/ui/` (an in-package directory) and are false positives. shell/PTY and file watching are adequately covered.

**Cosmetic, and one correction to my own draft:** the disk risk is **stale** — Metis measured `/` at 12% used with 256G free and `/config` with 293G free. The earlier "38G, 100% full" was a transient condition inside a subagent's environment. The preflight gate stays (cheap insurance) but is no longer a blocking precondition. Separately: `/tmp/ulw-refs/{jcode,claw-code,codex}` is a poor home for references that 82 todos cite — one reboot or tmpreaper run dangles every References line. They move into the repo.

### `oh-my-opencode-slim` — found, but it is NOT what its name implies
`npm view oh-my-opencode-slim` → v2.2.9, repo `github.com/alvinunreal/oh-my-opencode-slim`, cloned at `0781456f`. **It is a different author's sibling fork**, forked from the shared upstream `code-yeongyu/oh-my-opencode`, not a trimmed build of `@sunerpy/oh-my-openagent`. So its roster and routing are an *independent redesign*, not deletions from omo's tree, and there is no CHANGELOG documenting removals. The diff below is reconstructed from both trees. This matters for the plan: slim is a **design reference for what to cut**, not a subset to copy.

Size: slim 447 files / 40,324 non-test TS LOC; omo 4,143 files / 488,361 bundled JS LOC. ~10× fewer files.

**Slim's roster** (`src/config/constants.ts:7-24`): `orchestrator` + `explorer, librarian, oracle, designer, fixer, observer, council, councillor`. `observer` disabled by default (`:91`). `PROTECTED_AGENTS = {orchestrator, councillor}`. Aliases keep `explore → explorer`, `frontend-ui-ux-engineer → designer` working (`:2-5`).

**The single most important inversion — no hardcoded models.** `constants.ts:26-41` sets `DEFAULT_MODELS` for all nine agents to `undefined`, with the comment "All set to undefined so agents follow the global/session model." Slim then ships 5 flat **presets keyed by agent** (`src/cli/providers.ts:11-56`), not a fallback matrix. Compare omo: `AGENT_MODEL_REQUIREMENTS` (`dist/index.js:24475`) and `CATEGORY_MODEL_REQUIREMENTS` (`:24660`) hardcode model ids, provider entitlement lists, and reasoning variants — `gpt-5.6-sol variant:"max"`, `kimi-k3`, `glm-5.2`, ten provider ids per entry. **Slim eliminated the 8-category system entirely**; the only surviving `category` strings are stale error-recovery hints in one hook (`src/hooks/delegate-task-retry/patterns.ts:20-34`).

**Slim registers no `task` tool at all** (`src/tools/index.ts` is six lines: `acp_run, ast_grep_search, ast_grep_replace, cancel_task, webfetch, wait_for_user`). It rides the host's `task` tool and only *intercepts* it via hooks (`src/hooks/task-session-manager/tool-execute-hooks.ts:52,69`). So `category` and `load_skills` are dropped; `task_id` is kept and hardened; background is `background: true`.

**Slim dropped the `.omo/` state layer entirely** — no plans, notepads, or boulder state (`grep -rli "notepad|boulder" src/` → nothing). Cross-turn memory is an in-session **Background Job Board** injected by a hook (`src/hooks/task-session-manager/board-injection.ts`) with Active tasks un-addressable (`orchestrator.ts:227-230`).

**Hooks 83 → ~20. Skills 86 → 9. Commands: omo's `goal/handoff/hyperplan/refactor/start-work` → slim's `/loop /deepwork /reflect /preset`.** Team Mode, hashline edits, and the whole Claude Code compat layer: dropped. Slim's stated principle, from `.out-of-scope/hashline.md`: *"a deep, behavior-changing modification to the fundamental edit loop — fragile to bolt onto a slim plugin that **intentionally avoids reimplementing tool plumbing**."* That one rule explains ~80% of the deltas.

**Adopted for the Rust built-in set:**
| Keep (load-bearing) | Drop (ceremony) |
| --- | --- |
| Named-agent routing with a per-agent **negative** boundary ("Don't delegate when…", `orchestrator.ts:41-113`) — this is what stops delegation theater | The 8-category → model-fallback matrix. It encodes today's model names and provider entitlements in the harness and rots every model release |
| Deny-by-default read-only permission primitive (`permissions.ts:13-30`, `'*': 'deny'` then allow-list) — in Rust this becomes a type, not a convention | `load_skills` per task call — permission-gated skills achieve the same isolation without an argument the model forgets (omo needs a whole `delegate-task-retry` hook family to recover from exactly that) |
| `task_id` session reuse with the explicit "prose is not enough" rule (`orchestrator.ts:245-247`) | The `.omo/` plan+notepad+boulder store as *agent* state (the **goal** subsystem is separate and stays — it is a user requirement) |
| Background dispatch + reconciliation board with an un-addressable Active state | Planner triad `prometheus`/`metis`/`momus` + their format-policing hooks; review folds into the advisor |
| Per-agent temperature, spent where taste lives (`designer` 0.7 vs 0.1-0.2 elsewhere) | Team Mode — a second fan-out paradigm alongside subagents |
| Structured output envelopes per agent (`explorer.ts:20-28`, `fixer.ts:23-34`) — extended to **every** agent, since a Rust harness parses these | 86 bundled skills → under a dozen the built-ins actually reference; Claude Code compat layer; hashline edits |

**Three places I deviate from slim** (recorded so they are decisions, not drift): (1) slim's `fixer` is deliberately amnesiac — no research, no multi-step planning (`fixer.ts:15-17`) — which forces every *explore → decide → implement → verify* task back through the orchestrator; the Rust set keeps **one write-capable agent allowed to research and iterate** (still forbidden from spawning children). (2) `observer`/multimodal is gated on "is a vision model configured", not disabled by default — its whole purpose is context hygiene. (3) `council`/`councillor` (multi-model consensus, "3x slower … 3x or more cost" by slim's own note at `orchestrator.ts:95`) is **cut**; the advisor covers it.

### jcode's code execution — there is no interpreter, and that IS the answer
Verified three ways: no scripting/interpreter crate anywhere in `Cargo.lock` (`quickjs|boa|deno|wasmtime|pyo3|rustpython|starlark|rhai|mlua|v8` → only false positives like `clipboard-win`); no `codemode`/`code_mode` symbol in any `.rs`; and the literal built-in roster (`crates/jcode-app-core/src/tool/mod.rs:177-278`) contains no code-eval tool. Two things run code: **`bash`** (spawns a shell) and **`batch`** (composes tool calls). There is no equivalent of exposing the tool roster as callable functions to an interpreter.

- **`bash`** — description is four words: `"Run a bash command."` (`tool/bash.rs:33-35`), with all operational guidance on the *parameters* instead (`bash_destructive_gate.rs:47-84`: `timeout` in **milliseconds** with a 600s ceiling, `run_in_background`, `notify`, `wake`, `justification`). Execution is `TokioCommand::new("bash").arg("-c")` with a `cfg(windows)` fork to `cmd.exe /D /S /C` (`bash.rs:499-523`) — 25 lines. Output capped at 30,000 bytes, UTF-8-safe (`:26`, `:539-547`). **Timeout does not kill**: the child is promoted to a background task with "…is continuing in background (not killed)" (`:885-936`) — a timeout is a decision about the model's attention, not about the process. Cancellation via `kill_on_drop` + a Unix process-group `SIGKILL` guard (`:490-497`, `setpgid` in `pre_exec` at `:1117-1125`). **No sandbox at all** — no seccomp/landlock/bubblewrap anywhere. Safety is instead a **deterministic static risk gate** with three verdicts — run / deny / **Reflect** (return a prompt "that a blind retry cannot satisfy", `bash_destructive_gate.rs:9-40`), backed by a 1,812-LOC `jcode-command-risk` crate.
- **`batch`** — `"Run tools in parallel."` (`batch.rs:183-193`). A declarative fan-out of up to 10 JSON tool calls (`:10`, `:203-208`), recursion banned (`:211-215`), per-sub-call output budget `50_000 / n` (`:309-315`), results re-sorted to submission order for determinism (`:296`). Crucially, sub-calls **re-enter the same `registry.execute`** with a derived `ctx.for_subcall` (`:259-272`), so policy, hooks, and context guards apply uniformly.
- **The capability gap, stated honestly**: `batch` has no variables, no control flow, no data dependency between sub-calls — all ten inputs must be known before it starts. codemode's actual value proposition (intermediate results staying *out of* the model's context: `const files = await grep(...); for (const f of files) await read(f)` is one turn where batch needs N) is absent.
- **Cost of the interpreter route, concretely**: the decisive factor is not the engine, it is the **tool bridge** — N async host-function bindings, each needing re-entrancy from inside a JS event loop back into the Rust registry, which is exactly where `unsafe` and engine-specific complexity concentrate. `rquickjs`/`deno_core` are FFI-heavy; QuickJS needs a C compiler on every target (a recurring tax cross-compiling to musl/aarch64); `boa` is pure Rust but slow and incomplete. `batch` gets sub-call dispatch from plain Rust async with none of it.
- **Adopted design**: implement `execute` as **jcode's shape plus the one thing jcode is missing** — a `batch`-style parallel composition tool, and then close the context-bloat gap with **variable binding**: let sub-call *N* reference sub-call *M*'s output through a small path expression evaluated **in Rust**. That recovers most of codemode's token savings for a few hundred LOC, with no new engine, no `unsafe`, and no C toolchain. Also adopted: the terse-description/rich-parameters split, the recursion ban and fan-out cap, sub-calls re-entering the one registry seam, timeout-promotes-to-background, refuse-oversized-output-rather-than-truncate with an `accept_large_output` opt-in, and the three-verdict risk gate with `Reflect`. Explicitly **not** adopted: jcode's total absence of confinement (a sandbox decision is deferred but named, not silently skipped) and its 75-line `normalize_batch_input` model-mistake repair layer (`batch.rs:105-179`) — the schema is designed tighter instead, with repair added only if drift is measured.
- Sizing: `bash.rs` 1,283 + gate 84 + `batch.rs` 348 + the `Tool`/`Registry` seam ≈ **1,630 LOC** without `swarm`/`bg`/the risk crate.

### Environment risk found while researching
- The librarian's `git fetch --deepen` on the codex clone failed with `fatal: write error: No space left on device`, reporting `/` at 100% (38G). A Rust workspace of this size plus `target/` will not build without free space. This is a **precondition**, not a plan step. (Reported by subagent; I could not independently confirm — no shell available in this planner session. UNVERIFIED.)

## Decisions (with rationale)
1. **Target the shipping binary's external contract, not the internal architecture.** Pin `opencode 1.18.13 @ aefaf140c1`. Compatibility is defined by observable behavior: files read, CLI accepted, HTTP served, DB written. This makes "compatible" testable via differential black-box tests against the real binary.
2. **Sever the cloud/UI half.** ~385K of 562K LOC is web/desktop/console/enterprise/stats/storybook — none of it is in the CLI's dependency closure. Excluding it is not a scope reduction of the user's request; it is the correct boundary of "the opencode CLI".
3. **One event-emitting core; every interface is a consumer.** Directly contradicts claw-code's welded rendering and jcode's duplicated loop. This is what makes CLI + TUI + HTTP + ACP possible from one implementation.
4. **Typed errors and schemars-derived tool schemas.** Both reference implementations lost this and paid for it in string-matched recovery logic and drifting schemas.
5. **Reuse opencode's SQLite schema and XDG paths verbatim** so existing sessions, auth, and snapshots keep working after switching binaries.
6. **The goal tool is codex's mechanism plus the user's requested document.** SQLite is authoritative (status enum split by writer, token/time budgets, one goal per session); a Markdown projection is rendered for human reading/editing and re-ingested; the continuation prompt is re-rendered from authoritative state every turn so it survives compaction by construction.
7. **Memory and stability are acceptance criteria, not aspirations.** The stated motivation is memory exhaustion and hangs. The plan carries explicit RSS ceilings, a long-run soak test, and bounded channels everywhere (jcode's unbounded `ServerEvent` channel is exactly the shape that grows unboundedly under a slow consumer).
8. **Modern-form-only, with loud rejection (user directive).** The user waived legacy compatibility, so every contract is implemented in its current form only. Deprecated inputs must **fail with a message naming the replacement**, not be silently ignored — a config whose `mode.*` block is quietly dropped presents as working while doing nothing. Concretely dropped: `mode.<name>` config, `{mode,modes}/*.md` dirs, agent `tools: Record<string,boolean>`, agent `maxSteps`, `layout`, `autoshare`, `CONTEXT.md`, the global TOML `config` file, the JSON `storage/**` layout, `AuthOuathResult`, auth-prompt `condition`, and the duplicate `reference` spelling of `references`. This removes 12 compatibility shims and their tests, and it removes the legacy-normalization pass in `packages/opencode/src/config/config.ts:536-584` and `packages/core/src/v1/config/agent.ts:68-80` entirely.
9. **One exception to #8, and it is load-bearing.** The pre-`/api/*` HTTP surface is "old generation", not "deprecated" — the SDK v1 `client` object passed to every plugin (`packages/plugin/src/index.ts:57`) is built on it. If Q2 keeps the JS compat host, the v1 endpoints those two auth plugins actually touch must stay. The plan measures that call set against the real plugins rather than guessing, and serves nothing beyond it.

## Scope IN
- C1..C7 as locked in the topology ledger, covering the full compatibility contract catalogued above.
- The four explicit user requirements: unsafe-free Rust; Rust-authorable plugins; omo-style built-in agents with per-agent and per-category model + reasoning-effort control; delegation parameters (model / reasoning effort / agent type / category); a built-in codex-style goal tool.
- A differential compatibility harness using the real `opencode` binary and the recorded provider cassettes in `packages/llm/test/fixtures/recordings/` as oracles.
- Migration/coexistence story: the Rust binary reads the same config and DB, so it can be used side by side and rolled back.

## Scope OUT (Must NOT have)
- No port of `app`, `desktop`, `session-ui`, `ui`, `storybook`, `web`, `console`, `enterprise`, `function`, `slack`, `stats`, `containers`, `identity`.
- No hosted/cloud features: opencode Console accounts, org config, billing, share links, `sync/*`, `experimental/control-plane/*`, GitHub App install flow.
- No modification of the upstream TypeScript repo. It is read-only reference and test oracle.
- No pixel-identical TUI reproduction (pending Q1 answer); no reimplementation of OpenTUI itself.
- No `unsafe` blocks in first-party crates; no OpenSSL; no bundled Node/Bun unless the JS-compat decision explicitly requires it.
- No inventing a reduced "MVP"/"v1" subset of the compatibility contract — waves are execution ordering, not scope cuts.
- No self-authored-fixture-only protocol tests for MCP, LSP, ACP, or SSE (the claw-code failure mode).
- **No legacy/deprecated compatibility** (user directive 无需兼容老特性). Specifically NOT implemented: `mode.<name>` config blocks; `{mode,modes}/*.md` agent directories; agent `tools: Record<string, boolean>`; agent `maxSteps`; the `layout` key; `autoshare`; `CONTEXT.md` as an instruction source; the global TOML `config` file; the JSON `storage/**` session/message/part layout and any migration from it; `AuthOuathResult`; auth-prompt `condition`; the duplicate `reference` spelling. Each is detected and rejected with an error naming its modern replacement — never silently ignored.
- No compatibility with opencode versions other than 1.18.13. No forward-compat shims for the in-flight v2/`lildax` refactor.
- No pre-`/api/*` HTTP endpoints beyond the measured minimum the JS plugin compat host requires (and none at all if Q2 resolves to option B).

## Session maintenance (C8) — added scope, findings

User request: 数据库会话清理功能；列出全局会话而不是项目会话；清理全局 N 天之前的会话；清理项目 N 天之前的会话；等等.

**Three corrections to my initial assumptions, from the dedicated cleanup research:**
1. **Global listing already exists at the HTTP layer.** `GET /api/session` with no `directory`/`project`/`workspaceID` param queries the whole `session` table (`packages/core/src/session.ts:268-299`), and `GET /experimental/session` → `listGlobal()` is explicitly documented "all OpenCode sessions across projects, sorted by most recently updated, archived excluded by default" (`experimental.ts:224-233` → `session/session.ts:557-596`, which also joins a `{id,name,worktree}` project summary). What is missing is the **CLI** surface: `session list` hard-injects `ctx.project.id` (`session/session.ts:548-555`, `:957-965`) and only exposes `--max-count` and `--format` (`cli/cmd/session.ts:70-85`). So C8's listing work is a CLI + query-option surface over an existing capability, not a new query.
2. **Busy/running state is NOT in the database.** `session.active` and `SessionRunState.runners` are in-process maps (`protocol/src/groups/session.ts:146-155`, `session/run-state.ts:35-74`, `session/status.ts:30-48`). A prune run from a *different* process therefore cannot know a session is busy. My draft assumption "protect any session currently running" is unimplementable as stated. Revised: probe a reachable local server's `/api/session/active` first; if unreachable, fall back to a recency guard (`time_updated` within a safety window) and require an explicit flag to cross it. This must be stated in the plan, not discovered during execution.
3. **`storage/session_diff/<sessionID>.json` is still being written today** by `session/revert.ts:68-77`, and `Session.remove()` never deletes it (`session/session.ts:608-629`). It is session-keyed leaked state. The Rust implementation will not write it (revert lives in the `session.revert` column), but a prune must still sweep pre-existing files — so the no-legacy directive removes the *writer*, not the *cleanup obligation*.

Schema facts that determine the design (all from `packages/core/src/session/sql.ts` and siblings):
- **Timestamps are unix ms integers**: `Timestamps = { time_created: integer notNull $default(Date.now), time_updated: integer notNull $onUpdate(Date.now) }` (`packages/core/src/database/schema.sql.ts:3-10`). `time_updated` auto-advances on every write, so "last activity" is available on the session row without scanning messages. Retention keys on `time_updated` by default.
- **Project binding**: `session.project_id` → `project.id` with `onDelete: "cascade"`, plus `index("session_project_idx")` (`session/sql.ts:26-29,62`). The `project` table holds `worktree` (absolute path), `name`, `vcs` (`project/sql.ts:6-18`), and `project_directory` maps extra directories. Session rows also carry `directory` (absolute, notNull) and `path` (worktree-relative), set at creation from `ctx.directory` / `relative(ctx.worktree, ctx.directory)` (`session/session.ts:513-533`).

**CORRECTION to my first pass — global listing already exists at the HTTP layer; it is the CLI that is project-scoped.**
- `GET /api/session` takes three mutually exclusive scopes: `directory=<abs>`, `project=<id>` (+`subpath`), or **neither, which means global** — the SQL adds no scope predicate when `directory`/`project`/`workspaceID` are all absent (`packages/core/src/session.ts:268-299`). Default page size 50 (`packages/server/src/handlers/session.ts:16-37`). It sorts by **`time_created`**, not `time_updated`. Note: `subpath` is in the API schema (`packages/protocol/src/groups/session.ts:41-45`) but the implementation never reads it — **a live upstream no-op the Rust port must decide about explicitly**.
- `GET /experimental/session` is explicitly global: "Get a list of all OpenCode sessions across projects, sorted by most recently updated. Archived sessions are excluded by default" (`httpapi/groups/experimental.ts:224-233`). `listGlobal()` queries the whole table, orders `time_updated DESC, id DESC`, caps at 100, adds `time_archived IS NULL` unless `archived=true` (which means *include* archived, not *only* archived), and returns `GlobalInfo` = session fields + nullable `{id,name,worktree}` project summary (`session/session.ts:557-596`, `:247-258`).
- `session list` CLI has only `--max-count/-n` and `--format table|json`, and calls `svc.list({ roots: true, limit })`, which **injects `ctx.project.id` unconditionally** (`cli/cmd/session.ts:70-88`; `session/session.ts:548-555`, `:957-965`). So the global capability exists in the API and is simply unreachable from the CLI. The Rust port's job is a CLI surface plus a unified, `time_updated`-ordered service — not a new query.
- compat `GET /session` scopes to current project **and current directory** by default; `scope=project` widens to all directories of the project (`httpapi/handlers/session.ts:64-74`).
- `GET /api/session/active` returns only the **current process's** foreground drains from an in-memory set (`protocol/src/groups/session.ts:146-155`; `packages/server/src/handlers/session.ts:80-89`). It is not persisted and does not see other opencode processes — so "is this session running?" cannot be answered DB-only. A prune guard must account for that.
- **Declared ON DELETE CASCADE from `session`**: `message` (`sql.ts:72-75`), `todo` (`:103-106`), `session_message` (`:123-126`), `session_input` (`:144-147`), `session_context_epoch` (`:169-172`). `part` cascades transitively via `part.message_id → message.id` (`:86-89`); note `part.session_id` is NOT a foreign key, only an index (`:90,96`) — so an orphan check on `part` is warranted.
- Foreign keys are actually enforced: `PRAGMA foreign_keys` is on (`packages/core/src/database/database.ts:22-33`), so the cascades fire.
- **`session.parent_id` is the subagent/child link and has NO foreign key and NO cascade** — index only (`sql.ts:31,64`). **Pruning a parent session orphans its children.** This is the single most important correctness fact for the feature: a naive `DELETE FROM session WHERE time_updated < ?` leaves child rows whose `parent_id` points at nothing.
- Fields that mark a session as special and must gate deletion: `time_archived` (an archive concept already exists, `sql.ts:59`), `share_url` (shared/published, `:37`), `time_compacting` (mid-compaction, `:58`), `parent_id` (has/is a child), `revert` (staged revert state, `:49`), `workspace_id` (`:30`).
- Cost/token columns (`cost`, `tokens_*`, `sql.ts:43-48`) make a prune preview able to report exactly what value is being discarded.

**What `Session.remove()` does today** (`session/session.ts:608-629`): get-or-error → cancel background jobs matching the session / its job ID / `metadata.sessionId` / `metadata.parentSessionId` (`:940-955`) → query `parent_id = sessionID` → **recurse into children in application code** (`:619-622`) → publish `SessionV1.Event.Deleted` → `events.remove(sessionID)` deleting `event_sequence` + `event` (`packages/core/src/event.ts:514-523`). The projector performs the single real row delete (`packages/core/src/session/projector.ts:259-261`). So the existing single-session path *does* recurse — my draft's "orphans its children" claim applies to a **naive SQL age-based prune**, which is exactly what C8 must not be: `parent_id` has no FK (`session/sql.ts:31`), so `DELETE FROM session WHERE time_updated < ?` would strand descendants whose parent matched while they did not.

**What `Session.remove()` does NOT do** — every item here is an obligation C8 inherits: it does not delete snapshot repos, does not delete tool-output files, does not delete `storage/session_diff/<sessionID>.json`, does not delete legacy JSON session/message/part files, does not remote-unshare (only the explicit unshare path calls `shareNext.remove()`, `share/session.ts:34-37`), does not delete session plan files, and **does not refuse a busy session** (it cancels some background jobs but never calls `SessionRunState.assertNotBusy`, cf. `session/run-state.ts:71-75`).

Off-database artifacts a prune must consider:
- **Snapshots**: `$XDG_DATA_HOME/opencode/snapshot/<projectID>/<Hash.fast(worktree)>/` (`snapshot/index.ts:66-75`) — keyed by **project + worktree hash, not session**; many sessions share one bare Git store and reference snapshots by tree hash. Existing cleanup is only `git gc --prune=7.days`, hourly (`:23-24`, `:300-316`, `:761-766`), which never removes a project/worktree directory. Per-session deletion is therefore impossible; the correct operation is reference-counted GC of stores whose project has no surviving sessions, plus Git GC.
- **Tool output**: `$XDG_DATA_HOME/opencode/tool-output/tool_<ascending-id>` (`core/src/tool-output-store.ts:17-18`, `:118-135`). `bound()` receives `sessionID` and `toolCallID` (`:19-23`) but `write()` **uses neither** — the filename is only an identifier (`:129-135`), so **session attribution cannot be recovered from the path**. Existing cleanup is mtime > 7 days, hourly (`:176-210`), duplicated by the legacy truncate path (`tool/truncate.ts:13-20`, `:54-72`, `:143-148`). Decision for the Rust port: **encode `sessionID` in the tool-output filename** so a prune can attribute and delete precisely, while keeping the age sweep as a backstop. This is a deliberate, documented divergence from upstream's on-disk naming — it changes no API and no DB row, and upstream's own hourly age sweep still handles foreign files.
- **Legacy JSON `storage/**`**: root `$XDG_DATA_HOME/opencode/storage/`, key `[a,b,c]` → `storage/a/b/c.json` (`storage/storage.ts:63-65`, `:222-241`). Session-keyed shapes present on disk: `storage/session/<projectID>/<sessionID>.json`, `storage/message/<sessionID>/<messageID>.json`, `storage/part/<messageID>/<partID>.json`, `storage/session_diff/<sessionID>.json`. No retention exists for any of it. The Rust port does not read or write this tree, but the prune offers an explicit opt-in sweep for the session-keyed shapes.
- **Plans**: `<worktree>/.opencode/plans/<created>-<slug>.md` or `$DATA/plans/...` (`session/session.ts:331-335`) — **not** session-ID-keyed, so it cannot be attributed. Out of scope for prune; documented as such so no one "fixes" it by guessing.
- **Remote share**: local `session.share_url` + `session_share` rows cascade, but the remote resource persists unless `shareNext.remove()` is called. C8's delete path calls remote-unshare first for shared sessions when reachable, and refuses (or warns under `--force`) when not.
- **PTY**: in-process map with a 25-exited-PTY cap (`core/src/pty.ts:14-18`) — no disk state, nothing to prune.

**Existing retention that already exists upstream** (so C8 does not duplicate it, and so the Rust port keeps parity): hourly snapshot `git gc --prune=7.days`; hourly tool-output mtime sweep at 7 days (two implementations); PTY 25-item cap. **Confirmed absent upstream**: session TTL, any age-based session deletion, any prune CLI or HTTP endpoint, any `VACUUM` call, any orphan DB/file reconciliation, any persisted pinned/starred flag, and any log rotation tied to session deletion.

**`db` CLI facts** (relevant because C8 adds sibling DB commands): `db <query>` executes **arbitrary** `sql.raw(query)` through the in-process driver — not restricted to SELECT — with `--format json|tsv` (`cli/cmd/db.ts:8-36`); bare `db` spawns the **external `sqlite3` binary** with `stdio: "inherit"` (`:38-41`); `db path` prints the DB path (`:45-51`). The Rust port keeps `db <query>` and `db path` in-process, and replaces the interactive mode with a built-in shell so no external `sqlite3` is required.

**No pinned/starred column exists** anywhere in the session schema (`session/sql.ts:22-60`); the app's "pinned store" is a transient client cache and must not be treated as a server-side prune-protection flag. If protection beyond `share_url`/`time_compacting` is wanted, it rides in the free-form `metadata` JSON (`:42`) under a key this project defines.

Adopted defaults for this component (destructive-operation safety):
| decision | default | rationale |
| --- | --- | --- |
| Dry-run posture | Preview is the DEFAULT; deletion requires an explicit confirm flag | Prune is irreversible; the safe mode must be the one you get by accident |
| Retention key | `time_updated` (last activity), with `--by created` to switch | "N 天没动过" is what users mean by stale |
| Child sessions | A parent is only deletable together with its whole subtree; the subtree is computed transitively and deleted in one transaction | `parent_id` has no cascade, so the app must do what SQL will not |
| Protected by default | `share_url IS NOT NULL`, `time_compacting IS NOT NULL`, `time_archived IS NULL` is NOT itself protection (archived sessions are the prime prune candidates); plus a liveness guard — see next row. Each overridable by an explicit flag | Deleting a published or mid-compaction session corrupts live state |
| Liveness guard (revised after research) | Probe a reachable local server's `/api/session/active` and refuse those IDs; if no server is reachable, refuse sessions whose `time_updated` is within a safety window (default 1h) and require `--include-recent` to cross it | Busy state is in-process only (`session/run-state.ts:35-74`), so a prune in another process cannot query it. A recency guard is the only sound proxy |
| Legacy leaked files | Sweep `storage/session_diff/<sessionID>.json` for pruned IDs even though the Rust implementation never writes it | `session/revert.ts:68-77` writes it today and `Session.remove()` never deletes it; the files exist on the user's disk now |
| Archive vs delete | Both: `--archive` sets `time_archived`, `--delete` removes rows. Archive is the recommended first step | The column already exists; reversible beats irreversible |
| Scope selection | `--project <path|id>` (default: current project) and `--all-projects` for global | Mirrors the two operations the user asked for |
| Cascade | One transaction: subtree → `session_context_epoch`/`session_input`/`session_message`/`todo`/`part`/`message` → `session`; then orphan sweep on `part`; then reference-counted GC of `tool-output` and unreferenced snapshot stores | Matches declared cascades and covers what they miss |
| Space reclamation | `VACUUM` offered as an explicit step, reporting bytes before/after; never implicit | VACUUM rewrites the whole DB and needs free disk (relevant given the disk-full finding) |
| Surfaces | Rust-native CLI subcommands + HTTP endpoints under `/api/session/...`, both driven by one service function | Single implementation, two surfaces; no logic in the CLI |
| Global listing shape | `session list --all-projects` returning the same `GlobalInfo` shape (session fields + nullable `{id,name,worktree}` project summary) that `listGlobal()` already produces, with `--archived`, `--roots`, `--sort created\|updated`, `--limit`, `--format table\|json` | The query and the response shape already exist server-side (`session/session.ts:557-596`); only the CLI surface is missing |
| Sort-key inconsistency to fix | Default to `time_updated DESC, id DESC` everywhere, with `--sort created` to opt out | Upstream is inconsistent: `/api/session` sorts by `time_created` (`core/src/session.ts:269-297`) while legacy and experimental global sort by `time_updated` (`session/session.ts:557-576`). Pick one default and document it |
| Snapshot GC | Reference-count snapshot stores by surviving sessions per `(projectID, worktreeHash)`; only remove a store when no session references it, plus keep the hourly `git gc --prune=7.days` | Snapshot stores are keyed by project+worktree, not session (`snapshot/index.ts:66-75`), so per-session deletion is wrong by construction |
| Tool-output GC | Keep the mtime-based 7-day sweep and make the retention window configurable; do NOT attempt per-session attribution | `bound()` accepts a sessionID but `write()` never uses it, so the filename cannot be reverse-mapped to a session (`tool-output-store.ts:19-23`, `:129-135`) |
| Cascade completeness | The transaction must also clear `session_share` (already cascades) and the durable `event_sequence`/`event` rows (explicitly deleted upstream, not cascaded from session) | `share/sql.ts:5-13`; `core/src/event.ts:514-523` |

## Open questions
**Round 1 — RESOLVED** ("按你推荐"): Q1 = A (protocol/file/CLI/API/state identical; ratatui TUI, equivalent not pixel-identical; ACP implemented). Q2 = A (three-tier plugins: JSON-RPC primary + optional WASM + JS compat host so `antigravity-auth`/`kiro-auth` keep working). Q3 = A (SQLite authoritative + Markdown projection at `.opencode/goal/<sessionID>.md`; SQL wins on status/budget, document wins on objective text). Test strategy accepted as proposed.

**Round 2 — RESOLVED** ("可以按你推荐" + two refinements):
- **Q4 = A.** The binary reports `1.18.13` where the plugin compatibility gate reads a version (`plugin/loader.ts:127` → `checkPluginCompatibility`), so npm plugins declaring an opencode range are not skipped. Its own build identity is exposed separately (a long-form version and a distinct user agent). Todo 6's QA wording that "accepted a known `--version` difference" is corrected — accepting it would have gotten both auth plugins rejected.
- **Q5 = A, refined by the user: omo is BUILT-IN, and SLIMMED.** The user's words: "omo 相当于内置了其实，可以参考 oh-my-opencode-slim 进行精简". So the omo `plugin` array entry is not loaded through the compat host; its capability is native. But the native set is **not** a 1:1 port of omo 4.21.0 — it is a lean subset modeled on `oh-my-opencode-slim`. What survives vs what is dropped is decided from that reference (research dispatched), with the load-bearing pieces named explicitly: the agent roster, the category→model/effort table, the `task` delegation surface, and the goal/plan state layer. Ceremony that a built-in set does not need gets dropped, and each drop is recorded so it is a decision rather than an omission.
- **Q6 = overridden by the user.** I recommended dropping the `execute` tool; the user instead said to build it, modeled on **jcode's** implementation rather than porting the TS `acorn` + `typescript` interpreter (`packages/codemode/`). This is the better call: a Rust binary that forbids `unsafe` and bundles no JS runtime has no business hosting a TypeScript interpreter, whereas jcode — a Rust agent facing the identical constraint — already had to solve "let the model run code that calls tools". Research dispatched to extract its mechanism. The design constraint is now fixed: **whatever jcode does, the Rust implementation follows that shape, not the TS shape.** If jcode turns out to have no interpreter at all (plausible — it may compose tools through a batch mechanism instead), then that absence is itself the answer, and `execute` is implemented as that composition rather than as an interpreter.

Original three forks, kept for the record:

**Q1 — Compatibility boundary (which surfaces must be identical).**
- **A (recommended)**: protocol + file-format + CLI + HTTP API + on-disk state identical; TUI reimplemented in ratatui with equivalent capability and keybinds but not pixel-identical; ACP implemented.
- B: A, plus a pixel/keybind-faithful reproduction of the OpenTUI interface, themes included.
- C: headless only (CLI + server + API + ACP), no Rust TUI — the existing opencode TUI attaches over HTTP.
- Why it forks: this is the single largest cost driver (`tui` 31,729 LOC + OpenTUI native renderer) and it decides whether `packages/tui` needs replacing at all.
- Note after the no-legacy directive: option C now costs more than it looks. Attaching the existing TypeScript TUI requires the 13 `/tui/*` control endpoints plus the v1 session surface — i.e. keeping most of the "old generation" API that the directive otherwise removes. A recommends itself more strongly than before.

**Q2 — Plugin ABI, and the fate of the three JS plugins you currently depend on.**
- **A (recommended)**: three tiers — (1) out-of-process JSON-RPC plugins over stdio as the primary Rust-authorable path (zero `unsafe`, crash-isolated, any language); (2) optional in-process WASM component plugins (`wasmtime`, still zero `unsafe`) for hot hooks; (3) a JS compat host that spawns system `bun`/`node` to run existing `@opencode-ai/plugin` modules, so `opencode-antigravity-auth` and `opencode-kiro-auth` keep providing your models unchanged. omo itself is ported natively rather than loaded as a plugin.
- B: Rust + JSON-RPC only. No JS host. `antigravity-auth` and `kiro-auth` must be rewritten in Rust before the binary can talk to your current models.
- C: in-process `cdylib` + stable C ABI for maximum speed — but this requires `unsafe` at the boundary, contradicting your no-unsafe requirement, and makes plugin/host version skew a segfault instead of an error.
- Why it forks: it decides whether day-one usability depends on rewriting two auth plugins, and it is the one place your no-`unsafe` rule and your plugin-performance wish actually conflict.

**Q3 — Goal document: location, ownership, and authority.**
- **A (recommended)**: SQLite authoritative + a rendered Markdown projection at `.opencode/goal/<sessionID>.md` (project-local, git-ignorable, human-editable; edits are re-ingested on the next turn, with SQL winning on conflict for status/budget and the document winning for objective text).
- B: `.omo/goal/<sessionID>.md` instead, to sit alongside the omo plan/notepad artifacts you already use.
- C: pure codex behavior — SQLite only, no document; the objective is only ever seen in the injected prompt.
- Why it forks: you asked for "文档保障" (document-guaranteed), which codex deliberately does not do; where the document lives and who wins a conflict is a product decision I should not make for you.

**Test strategy (confirming, not asking)** — recommended: **contract-first TDD** for the compatibility layers (config merge, permission `findLast`, command/skill resolution, DB schema, SSE parsing), where each test is written against behavior extracted from the TS source or observed from the real binary before the Rust code exists; **tests-after** for internal plumbing; plus a differential harness (same inputs → real binary vs Rust binary → diff), replayed provider cassettes, a long-run soak test with an RSS ceiling, and agent-executed QA on every todo. Say the word if you want a different split.

## Approval gate
status: awaiting-approval
pending-action: write .omo/plans/opencode-rust.md
approval authorizes: creating the plan file only. Not implementation. Execution starts separately in a worker session (e.g. `$start-work opencode-rust`).
next after approval: rerun the scaffold without `--draft-only`, run mandatory Metis gap analysis, APPEND todo batches into `## Todos`, fill `## TL;DR (For humans)` last, then deliver the handoff explanation and offer the dual high-accuracy review.
