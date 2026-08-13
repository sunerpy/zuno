# The measured pre-`/api` (v1) surface

This document is the evidence behind `crates/oc-server/src/compat_v1.rs`. Every
route that file serves appears here with at least one recorded plugin callsite.
A route with no callsite is scope creep; a callsite with no route is a gap. Both
are reportable, and the executable form of that rule is
`crates/oc-server/tests/compat_v1.rs`, which asserts the same properties against
`V1_SURFACE`.

## How this was measured

1. `/config/.config/opencode/opencode.json:87-92` names the installed plugins.
   The file contains a raw control character, so it was read as text rather than
   parsed as strict JSON.
2. Each package was located in opencode's versioned plugin cache under
   `/config/.cache/opencode/packages/`, and its runtime entry point taken from
   that package's own `package.json`.
3. Each entry point and its bundled modules were scanned for SDK namespace calls
   (`client.<group>.<method>`), including the aliased form
   `const app = _client?.app; app.log(...)`.
4. Each captured SDK method was mapped to an HTTP verb and path from the oracle's
   route groups, then cross-checked against the committed OpenAPI document
   `.omo/fixtures/oracle-openapi-1.18.18.json`. All 20 verb/path pairs are
   present there; none was invented.

Re-run this capture when the plugin set changes, when a plugin is upgraded, or
whenever an unknown-route 404 tells you to (see "Unknown-route accounting").

## Installed plugins

| plugin | evidence | runtime entry point |
| --- | --- | --- |
| `opencode-antigravity-auth@1.6.0` | `/config/.config/opencode/opencode.json:88` | `…/opencode-antigravity-auth@1.6.0/node_modules/opencode-antigravity-auth/dist/index.js` (`package.json:5`) |
| `@sunerpy/opencode-kiro-auth@0.20.6` | `/config/.config/opencode/opencode.json:90` | `…/@sunerpy/opencode-kiro-auth@0.20.6/node_modules/@sunerpy/opencode-kiro-auth/dist/index.js` (`package.json:6`) |
| `@sunerpy/oh-my-openagent@4.21.0` | `/config/.config/opencode/opencode.json:91` | `…/@sunerpy/oh-my-openagent@4.21.0/node_modules/@sunerpy/oh-my-openagent/dist/index.js` (`package.json:5`) |

Cache paths are rooted at `/config/.cache/opencode/packages/`. Line 89 of the
config is a commented-out `file://` spec and is not an enabled plugin.

All three sources were located and readable, so no entry in the table below is
unconfirmed for want of a plugin.

## Route → plugin → callsite

Paths are written exactly as the oracle declares them, **without an `/api`
prefix**: `InstanceHttpApi` composes the provider, session and TUI groups with no
`.prefix("/api")` (`packages/opencode/src/server/routes/instance/httpapi/api.ts:61-76`),
and the generated SDK requests those bare paths directly
(`packages/sdk/js/src/gen/sdk.gen.ts:437,553,607,617,725,743,759,1120`).

Plugin short names: **AG** = `opencode-antigravity-auth@1.6.0`,
**KIRO** = `@sunerpy/opencode-kiro-auth@0.20.6`,
**OMO** = `@sunerpy/oh-my-openagent@4.21.0`.

| # | verb + path | SDK method | plugin | callsite |
| --- | --- | --- | --- | --- |
| 1 | `PUT /auth/{providerID}` | `client.auth.set` | AG | `dist/src/plugin.js:1400,2319,2337,2366` |
| 2 | `POST /log` | `client.app.log` | AG | `dist/src/plugin/logger.js:45-50` (aliased through `_client?.app`) |
| 3 | `GET /agent` | `client.app.agents` | OMO | `dist/index.js:135963` |
| 4 | `GET /config` | `client.config.get` | OMO | `dist/index.js:136416,137080,171644` |
| 5 | `GET /provider` | `client.provider.list` | OMO | `dist/index.js:26958,84674` |
| 6 | `POST /provider/{providerID}/oauth/authorize` | `client.provider.oauth.authorize` | KIRO | `dist/core/request/request-handler.js:783-786` |
| 7 | `POST /provider/{providerID}/oauth/callback` | `client.provider.oauth.callback` | KIRO | `dist/core/request/request-handler.js:787-790` |
| 8 | `GET /session` | `client.session.list` | OMO | `dist/index.js:128645,128654` |
| 9 | `POST /session` | `client.session.create` | OMO | `dist/index.js:131233,132341,135030,143073` |
| 10 | `GET /session/status` | `client.session.status` | OMO | `dist/index.js:10581,123210,131119,132235,133654` |
| 11 | `GET /session/{sessionID}` | `client.session.get` | OMO | `dist/index.js:90497,96292,96646,116276,131215` |
| 12 | `PATCH /session/{sessionID}` | `client.session.update` | OMO | `dist/index.js:138043` |
| 13 | `GET /session/{sessionID}/children` | `client.session.children` | OMO | `dist/cli/index.js:106371,106539` (see gap G1) |
| 14 | `GET /session/{sessionID}/todo` | `client.session.todo` | OMO | `dist/index.js:89318,89712,90912,119229,143674` |
| 15 | `POST /session/{sessionID}/abort` | `client.session.abort` | AG, OMO | AG `dist/src/plugin/recovery.js:293`; OMO `dist/index.js:106808,120119,131421` |
| 16 | `POST /session/{sessionID}/summarize` | `client.session.summarize` | OMO | `dist/index.js:94259,119806,119913` |
| 17 | `GET /session/{sessionID}/message` | `client.session.messages` | AG, OMO | AG `dist/src/plugin/recovery.js:295`; OMO `dist/index.js:28404,85143,87664` |
| 18 | `POST /session/{sessionID}/message` | `client.session.prompt` | AG | `dist/src/plugin/recovery.js:126,198`; `dist/src/plugin.js:1077` |
| 19 | `POST /session/{sessionID}/prompt_async` | `client.session.promptAsync` | OMO | `dist/index.js:138443` |
| 20 | `POST /tui/show-toast` | `client.tui.showToast` | AG, KIRO, OMO | AG `dist/src/plugin.js:1086,1183,1254,2476`; KIRO `dist/plugin.js:46-47`; OMO `dist/index.js:89478,93846,94061` |

Callsite line lists are representative where a method is called many times from
one bundle; the full sets are recorded in
`.omo/evidence/task-54-opencode-rust.txt`.

### The toast path is `/tui/show-toast`

Confirmed twice: server side at
`packages/opencode/src/server/routes/instance/httpapi/groups/tui.ts:45,140-149`,
and SDK side at `packages/sdk/js/src/gen/sdk.gen.ts:1115-1126`. It is not
`/tui/showToast`. Three of three installed plugins call it, which is why a
literal reading of "no pre-`/api` endpoints" would have broken every plugin
toast.

Its request body, from the committed fixture, is
`{ title?: string, message: string, variant: "info"|"success"|"warning"|"error", duration?: integer }`
and its success response is a bare JSON `true`.

## How this differs from the plan's six

The plan recorded six SDK methods. Re-running the capture confirmed five of them
verbatim, corrected one, and added fourteen more — twenty in total.

Confirmed as written: `client.auth.set`, `client.session.abort`,
`client.session.messages`, `client.session.prompt`, `client.tui.showToast`.

Corrected: `client.provider.oauth` is not a callable method. It is a namespace
object (`Provider.oauth = new Oauth(...)`,
`packages/sdk/js/src/gen/sdk.gen.ts:715-750,753-774`) whose two methods are
actually called: `client.provider.oauth.authorize` and
`client.provider.oauth.callback`. One plan entry therefore becomes two routes.

Added by the re-run (14): `client.app.agents`, `client.app.log`,
`client.config.get`, `client.provider.list`, `client.session.children`,
`client.session.create`, `client.session.get`, `client.session.list`,
`client.session.promptAsync`, `client.session.status`,
`client.session.summarize`, `client.session.todo`, `client.session.update`,
plus the second half of the `provider.oauth` split.

The plan measured only the two auth plugins. The third installed plugin,
`@sunerpy/oh-my-openagent@4.21.0`, is a session-orchestration plugin and is
responsible for 13 of the 14 additions. `client.app.log` is the one addition
attributable to an auth plugin the plan already counted, and it was missed
because the callsite aliases the namespace (`const app = _client?.app`) instead
of writing `client.app.log`.

## Gaps and unverified items

**G1 — `client.session.children` has no line-numbered callsite in OMO's plugin
entry bundle.** The call is present in the same package's CLI bundle
(`dist/cli/index.js:106371,106539`) and the plugin bundle reuses that
implementation, but the plugin-entry line numbers were not captured. The route
is served because the method is demonstrably reachable from the package; the
precise plugin-entry callsite is UNVERIFIED. If a stricter reading is wanted,
this is the one route in the table whose justification rests on a CLI-bundle
citation.

**No captured SDK method lacks a route.** All 20 map to a verb/path present in
`.omo/fixtures/oracle-openapi-1.18.18.json`.

**No route in `V1_SURFACE` lacks a callsite.** Asserted by
`compat_v1_every_route_has_a_recorded_callsite`.

**Not implemented, deliberately.** The oracle serves 111 pre-`/api` paths; 20 are
implemented here. The remaining 91 are covered by the accounting mechanism below.
`DELETE /auth/{providerID}` is the interesting shape: the *path* is measured but
the *verb* is not, so it is accounted as an operation gap rather than a path gap
— `405` with the same actionable body and the same counter, keyed
`DELETE /auth/anthropic`.

## Backends: what is real and what is a seam

Eleven of the 20 routes have local backends in this build. Ten are thin wire-format
adapters over the corresponding `/api` implementation:

- `GET /agent` and `GET /provider`;
- `GET|POST /session` and `GET /session/{sessionID}`;
- `POST /session/{sessionID}/abort` and
  `POST /session/{sessionID}/summarize`;
- `GET|POST /session/{sessionID}/message`; and
- `POST /session/{sessionID}/prompt_async`.

The adapters preserve the published SDK's unprefixed paths and response envelopes,
while session persistence, event publication, prompt execution, compaction, and
catalog resolution remain owned by the shared `/api` handlers.

`POST /tui/show-toast` is fully served. No server entry point attaches a display
— `crates/oc-server/src/main.rs` and `crates/oc-cli/src/cmd/serve.rs` both build a
bare `CompatV1State::new()` — so in every shipped server the route is a
**recording seam**: each accepted toast is appended to a bounded
in-process ring, counted, and reported by the diagnostics endpoint, and the call
returns `200 true`. A toast that no one sees is a degraded UX; a `500` would be a
broken plugin, so the seam never fails on a well-formed toast. When a TUI
attaches it registers a forwarder through
`CompatV1State::with_toast_forwarder`, and the same route forwards instead of
only recording — no route change.

Two deliberate leniencies protect the toast mandate. The oracle marks `variant`
required and forbids additional properties; this seam defaults a missing
`variant` to `info` and ignores unknown fields. Rejecting either case would turn
a cosmetic mismatch into the exact failure the plan warns about. A missing or
non-string `message` is still a `400`, because there is then nothing to show.

The other nine routes are **registered, structured `501 not_implemented` seams**.
This follows the precedent set by the `/api` surface: an operation with no local
backend is registered explicitly and answers definitively rather than fabricating
success data. Each `501` body names the SDK method, the plugins that call it, and
the `/api` route to call instead when one is served here, so the operator learns
which plugin needs which backend and the caller learns what works today.

This is a **gap, not a decision**: it is recorded
as `v1-surface-unbacked` in `oc_testkit::compat_report::known_gaps`, rendered into
`docs/compatibility-matrix.md`, and never declared in `docs/divergences.toml`,
whose own rule is that a merely unimplemented surface must not be laundered into a
divergence. The nine remaining seams — `auth.set`, `app.log`, `config.get`, the two
`provider.oauth` calls, `session.status`, `session.update`, `session.children` and
`session.todo` — have no served `/api` spelling in this build, so a plugin needing
them has no working call today. `crates/oc-server/tests/compat_v1.rs` asserts both
halves against the routes the server really answers, so this paragraph cannot go
stale without a red test.

The practical consequence, stated plainly: against this surface alone the
installed auth plugins complete their **call lifecycle** — every request they
issue reaches a registered route and receives a definitive answer, and their
toasts are delivered to the sink — but they cannot yet authenticate, because
`auth.set` and the OAuth pair have no credential backend here.

## Unknown-route accounting

The point of a measured minimum is that the measurement can be wrong. When it is,
the failure must be a visible number rather than a mysterious plugin hang.

Any request to a pre-`/api` path that is **not** in the table above, but sits
under one of the v1 top-level prefixes, gets:

- HTTP **404**;
- a body naming the exact path, and instructing the operator to re-run this
  capture and extend `V1_SURFACE`;
- an increment of a process-local counter.

The counters are surfaced two ways: `GET /compat/v1/diagnostics` returns the
implemented surface, the total unknown-request count, and the per-path
breakdown; and the **first** sighting of each distinct unknown path also writes
one line to stderr, so an operator watching the server sees it without polling.
Repeat sightings are counted but not re-logged, so a path scanner cannot flood
the log.

A request whose **path** is measured but whose **verb** is not gets the same
treatment at `405` instead of `404`, keyed by `"VERB path"` in the breakdown.
Downgrading it to `404` would misreport a path that exists; leaving it as axum's
bodiless default `405` would hide the gap. `DELETE /auth/{providerID}` is the
real case.

The prefix set is exactly the oracle's pre-`/api` top-level path segments minus
`event`, which the SSE stream owns; a test derives that set from the committed
OpenAPI document and asserts equality, so it cannot drift. Scoping the accounting
this way is what keeps it from shadowing the `/api/*` surface, the `/event`
stream, `/health`, `/doc`, or `/openapi.json` — and there are tests asserting
each of those still answers with the accounting mounted.

### Mechanism, and the one edge it does not cover

The accounting is **not** a single `Router::fallback`, which would be global and
would claim the API surface's misses as v1 gaps. Nor is it a `/{prefix}/{*rest}`
wildcard: `matchit` rejects `/auth/{*rest}` as *conflicting* with the already
registered `/auth/{providerID}` rather than ordering one above the other, so that
shape panics at assembly on the first prefix with a parameterised route.

What it is: one `Router::nest` per prefix, each inner router carrying its own
fallback. axum grafts an inner fallback into the outer router at the nest prefix
(`axum-0.8.9/src/routing/mod.rs:227-229`), which is exactly the scoping needed.
Prefixes with no measured root route also get an explicit bare route, because a
nest at `/foo` matches `/foo` but the grafted fallback is registered one segment
deeper.

Known edge: a **trailing-slash** v1 path such as `/session/` is matched by
neither the nest nor the bare route, so it falls through to the plain outer 404
without a body or a counter bump. No SDK method generates a trailing slash, so
nothing in the capture reaches it.

The per-path breakdown is bounded (64 distinct paths, each truncated) so that
unbounded distinct junk paths cannot grow the process. Overflow beyond that is
still counted in the total and reported as an explicit overflow figure, because
losing the exact total would defeat the mechanism.
