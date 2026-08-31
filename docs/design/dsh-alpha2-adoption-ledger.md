# DeepSeek Harness `dsh-v0.1.2-alpha.2` adoption ledger

Status: reviewed 2026-08-31.

This ledger records the complete Zuno review of the DeepSeek Harness range from
`dsh-v0.1.1-rc.2` to `dsh-v0.1.2-alpha.2`. DSH is a design source, not a wire,
configuration, storage, package, or UI compatibility target.

## Verified upstream range

A fresh writable checkout was fetched before the review. The refs resolved to:

| ref | commit |
| --- | --- |
| `dsh-v0.1.1-rc.2` | `b150a551b8d465e31e418e1b2eaf5e79bbb7d28e` |
| `dsh-v0.1.2-alpha.2` | `0a53fb55bea101816fa226bb964ae2bed71c343b` |
| `origin/master` | `0a53fb55bea101816fa226bb964ae2bed71c343b` |

The baseline is an ancestor of the target. The range contains 1,313 commits,
6,808 changed files, 346,539 insertions, and 135,809 deletions.

The repository skill's delta classifier accounted for every changed path:

| classifier group | files |
| --- | ---: |
| `docs-process` | 1,594 |
| `goal-recovery` | 55 |
| `harness-composition` | 296 |
| `other` | 2,549 |
| `prompt-session-context` | 238 |
| `providers-attachments` | 134 |
| `security-permissions-runtime` | 310 |
| `tools-web-subagent-workflow` | 235 |
| `ui-client` | 1,397 |
| **total** | **6,808** |

The `other` label is only a mechanical fallback. Its paths were reviewed again
through the top-level inventory and the material decision groups below.

| top-level path group | files | disposition |
| --- | ---: | --- |
| `packages/` | 3,657 | Partitioned by the material decision table below; residual TypeScript API, SDK, package, and client transport changes are rejected as compatibility work. |
| `.agents/` | 1,212 | Reject DSH-owned prompts, skills, and agent packaging; Zuno keeps its own native roles and repository instructions. |
| `snapshots/` | 638 | Reject generated DSH fixtures as artifacts; adopted semantics receive Zuno-native Rust and black-box tests. |
| `examples/` | 473 | Reject package-specific examples; add Zuno documentation and tests only where a behavior is adopted. |
| `apps/` | 374 | Adapt the headless reasoning and browser-auth contracts; reject DSH web application and TypeScript UI ownership. |
| `docs/` | 253 | Use as design evidence; do not import prose or claim product compatibility. |
| `scripts/` | 123 | Reject DSH build, release, and repository automation. |
| `python/` | 29 | Reject the Python runtime and SDK surface. |
| `vendor/` | 11 | Reject vendored DSH release dependencies. |
| `.github/` | 8 | Watch upstream validation ideas; retain Zuno's own CI and release policy. |
| `website/` | 4 | Reject product website changes. |
| root and package metadata | 26 | Reject DSH package-manager, release, and workspace metadata. |
| **total** | **6,808** | **No unclassified path group remains.** |

## Material decisions

`Decision` is deliberately one of `adopt`, `adapt`, `already-covered`,
`reject`, or `watch`.

| Material delta | DSH evidence | Decision | Zuno result |
| --- | --- | --- | --- |
| Public web fetch target validation | `b2219bba63`, `c406560452`, `2fbe199a1c` | adapt | `PublicHttpClient` validates literals and every resolved address, rejects mixed public/private DNS answers and nested IPv6 forms, pins validated addresses, uses direct/no-proxy transport, and revalidates up to five manual redirects. Zuno permits a cross-origin redirect only when the new public HTTP(S) target independently passes the same checks. |
| Web-search endpoint diagnostics | `f55c676485`, `aa70a737ae` | adapt | Exa keeps its credential-bearing wire URL private and exposes only scheme, host, path, status, and error category in diagnostics. Reqwest causes are stripped with `without_url()` and sentinel tests cover errors and tracing. |
| Canonical image admission and object lifecycle | `bd4e4173e7`, `558f08780c`, `5c799a9520`, `4863890535`, `30704dc1df` | adapt | `zuno-attachment` normalizes PNG/JPEG/GIF/WebP, applies EXIF orientation, keeps the first animation frame, strips metadata, enforces pixel/byte limits, stores content-addressed private objects, and resolves inline provider blocks only while assembling a request. |
| Provider Files API fallback | attachment range above | reject | This cycle does not implement any provider Files API. `ImageRequestPolicy` remains the explicit future extension point. |
| ACP session-provided MCP | `511181684c`, `52bd3e1805` | adapt | ACP advertises stdio and Streamable HTTP, validates the complete declaration before publication, mounts an isolated session bundle transactionally, rolls partial startup back in reverse order, and never persists commands, environment values, URLs with credentials, or headers. SSE remains unsupported. |
| Child model and effort selection | `aefc083be7`, `7c626fb5d2`, `3a146064a4` | adapt | A disabled-by-default host allowlist is catalog-validated and frozen as a durable session policy digest. The `task` schema exposes `model` and `effort` only when enabled; continuations cannot change the frozen choice. |
| Headless reasoning progress | `937d2b3513` | adapt | `zuno run --show-reasoning` writes only provider-visible reasoning deltas to stderr between stable markers. Final answer text remains on stdout; signed/encrypted reasoning is never rendered, and JSON mode rejects the flag. |
| Loopback browser authentication | `3e24087bfa`, `ce031ddd16`, `3b3b493a96`, `5595d593d1`, `9c964848cd` | adapt | `zuno serve --browser-auth` requires a pure loopback authority, exchanges one 256-bit launch token for an authority-bound signed cookie, protects unsafe cookie requests with exact Origin matching, composes with Basic Auth, and redacts the bootstrap query from access logging. |
| Session projections and known-event reads | `42dc2a46c2`, `212df86cf8`, `a6c7c70d4f` | already-covered | Zuno already makes TUI, server, ACP, and future GUI clients consume durable events, inbox state, and shared projections. DSH Remote namespaces and snapshot wire formats are not copied. |
| Runtime component composition and relay removal | `9135a13a8b`, `1477d5b9ef`, `6e4fabdc1f` | already-covered | Native `Component`, typed service, `ProfileBundle`, and transactional `HarnessProfile` ownership already provide the intended separation and reverse-order disposal. |
| Goal, retry, interaction, and recovery refinements | classifier group `goal-recovery` | already-covered | Zuno already persists typed recovery, positive capped jittered backoff, interruptible deadlines, durable human waits, and at-most-once tool policy. DSH changes remain regression evidence rather than a second state model. |
| Process, sandbox, and cross-platform cleanup | `security-permissions-runtime` residual changes | watch | Keep following portable shutdown and Windows publication fixes. Zuno retains its typed authority, process-tree, uncertain-outcome, and fail-closed sandbox contracts; no TypeScript process layer is imported. |
| Batched search, workflow, and tool presentation | `tools-web-subagent-workflow` residual changes | already-covered | Zuno already owns batch scheduling above single-query search providers, durable Jobs and workflows, and frontend-neutral tool UI intent. |
| Browser clients, PTC/webworker, settings UI, inspector, and local echo | `ui-client` residual changes | reject | These are DSH product/client implementations. Zuno clients may reuse durable projections but may not acquire UI-private agent behavior or DSH RPC compatibility. |
| TypeScript SDK, API proxy, package graph, examples, and Python release surface | residual `packages/`, `examples/`, and `python/` paths | reject | Zuno is a native Rust harness and does not expose DSH package, SDK, Remote, Python, or repository layout compatibility. |
| Session-event migration and ignorable-event experiments | `2c6ff296af` and the reverted event-vocabulary range | reject | Zuno keeps its own fail-closed durable event vocabulary and migrations. A DSH compatibility reader would weaken that boundary. |
| Documentation, snapshots, translations, dependency refreshes, and release metadata | `docs-process` and non-runtime inventory groups | reject | They were included in range accounting but are not product capabilities. Zuno adds only documentation and tests for decisions in this ledger. |

## Resulting Zuno boundaries

- Security-sensitive network behavior lives in `zuno-network`, not in individual
  tools.
- Image lifetime belongs to `zuno-attachment`; providers still receive the
  existing inline provider-neutral image block.
- ACP client resources are process-local session effects and are supplied again
  on every load or resume.
- Child model authority is host-global at admission, durable per session, and
  immutable for a continuation.
- Reasoning and browser authentication are explicit opt-ins; existing CLI and
  server defaults do not change.
- No DSH wire, configuration, storage, UI, TypeScript, Python, Files API, or
  package compatibility is promised.

The tracked upstream baseline advances to `dsh-v0.1.2-alpha.2` in the same
documentation commit as this ledger. Before that update, the seven-capability
feature tip passed the complete workspace tests, formatting check, all-targets
check, Clippy with warnings denied, and diff check. The final shared gates are
run again after the baseline commit.
