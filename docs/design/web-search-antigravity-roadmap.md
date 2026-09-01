# Web search and Antigravity authentication roadmap

Status: approved for implementation
Date: 2026-08-25

## Objective

Make internet research useful on a fresh Zuno install while keeping authenticated Google grounding explicit and auditable:

- expose `web_search` by default through Exa's anonymous hosted MCP endpoint;
- never send an unauthenticated request to Parallel;
- add a native `antigravity` login integration;
- expose a separate `google_search` tool only when a usable Antigravity credential exists;
- keep search authentication distinct from model-provider transport and authentication;
- inherit Zuno's proxy, cancellation, permission, logging, and lifecycle guarantees.

This roadmap is an implementation handoff. No runtime source change was successfully applied before this document was created.

## Verified starting point

Repository snapshot when this roadmap was written:

- repository: `/config/workspace/ProdDir/AI/zuno`;
- branch: `main`;
- HEAD: `181b2d8bfdddd429da5101dc0e8251c0c593a1c2`;
- only worktree: the main worktree above;
- CodeGraph: current, 659 files, 24,213 nodes, 78,447 edges;
- preserve these unrelated user edits:
  - `.omo/notepads/opencode-rust/learnings.md`;
  - `.omo/notepads/opencode-rust/problems.md`.

A no-key request to `https://mcp.exa.ai/mcp` was exercised on 2026-08-25. Both `tools/list` and a `web_search_exa` call returned HTTP 200 and a real result. The live tool schema accepted the portable fields `query` and `numResults`; Zuno currently also sends stale `type` and `livecrawl` fields.

## Current implementation gaps

### Search gate

`crates/zuno-tools/src/websearch/gating.rs` currently:

- defaults both `enable_exa` and `enable_parallel` to `false`;
- treats any explicit provider as usable, even `parallel` without a key;
- accepts `ZUNO_WEB_SEARCH_ENABLE_PARALLEL=1` without a credential;
- cannot distinguish an explicit `false` override from an absent value;
- has no profile-level master switch.

### Exa and Parallel transport

`crates/zuno-tools/src/websearch/provider.rs` currently:

- sends `query`, `numResults`, `type`, and `livecrawl` to Exa;
- conditionally adds a Parallel bearer token, but still sends the request when no token exists;
- exposes only the provider-neutral `web_search` tool.

### Authentication

`crates/zuno-auth/src/provider.rs` currently stores one credential per provider id. Its OAuth shape has OpenAI-specific `enterprise_url` metadata and no general metadata for email, project id, managed project id, scopes, or integration identity.

`crates/zuno-cli/src/cmd/providers.rs` currently assumes every login target is a model provider and dispatches a small hard-coded `LoginMethodKind` match. An Antigravity search consumer is an integration before it is a model provider, so it must not be faked as a catalog model provider.

### Reference implementation findings

The installed reference is:

`/config/.cache/opencode/packages/opencode-antigravity-auth@1.6.0/node_modules/opencode-antigravity-auth`

Relevant files:

- `dist/src/antigravity/oauth.js`: Google OAuth, PKCE, token exchange, user info, initial project discovery;
- `dist/src/plugin/token.js`: refresh-token rotation and invalid-grant handling;
- `dist/src/plugin/project.js`: `loadCodeAssist`, onboarding, managed-project caching;
- `dist/src/plugin/search.js`: separate Antigravity `generateContent` request with `googleSearch` and optional `urlContext`;
- `dist/src/plugin.js`: login integration and `google_search` registration;
- `README.md`: multi-account behavior and explicit service/account-risk warning.

Do not copy the implementation blindly:

- version 1.6.0 can pass `projectId = "unknown"` into `google_search`, producing a known 404;
- it carries a hard-coded fallback project id that Zuno must not borrow;
- its `thinking` argument is accepted but not actually wired into the request;
- it writes a second account file instead of using Zuno's credential store;
- it uses undocumented Antigravity internal endpoints and must remain explicitly labelled experimental.

## Architectural decisions

### 1. Keep two tools

`web_search` remains provider-neutral and batch-oriented:

```json
{"queries":["query one","query two"]}
```

`google_search` is a distinct authenticated grounding tool:

```json
{
  "query": "question to research",
  "urls": ["https://example.com/reference"],
  "thinking": true
}
```

The separation is intentional. It prevents a Google OAuth dependency from disabling ordinary search and avoids mutating the main model request to inject Gemini-native tools.

### 2. Minimal configuration surface

Extend the existing top-level `web_search` object instead of adding another top-level subsystem:

```json
{
  "web_search": {
    "enabled": true,
    "provider": "exa",
    "max_queries": 4,
    "max_results": 8,
    "timeout_ms": 60000,
    "google": {
      "enabled": true,
      "search_model": "gemini-2.5-flash"
    }
  }
}
```

Resolution rules:

- absent `web_search.enabled` means `true`;
- absent provider means `exa`;
- Exa works with no key; `EXA_API_KEY` only raises upstream limits;
- `ZUNO_WEB_SEARCH_ENABLE_EXA` is a tri-state override: absent uses the profile/default, true enables, false disables;
- an explicit false is never overridden merely because an Exa key exists;
- Parallel is usable only when `PARALLEL_API_KEY` is non-empty;
- `provider: "parallel"` without a key is a startup/configuration error;
- remove the redundant keyless `ZUNO_WEB_SEARCH_ENABLE_PARALLEL` path;
- absent `google` config means auto: register only when an Antigravity credential exists;
- `google.enabled=false` always disables it;
- `google.enabled=true` without a credential produces a clear startup diagnostic.

Because the project is unreleased, implement this directly without compatibility aliases or migration code.

### 3. Authentication targets are not all model providers

Introduce an explicit authentication target kind:

```rust
enum AuthTargetKind {
    Provider,
    Integration,
}
```

The login registry must join three facts before showing a target:

1. a native login implementation exists;
2. a native credential consumer exists;
3. a reachable provider or integration route exists.

Register `antigravity` as an Integration consumed by `google_search`. Bare `zuno auth login` should list it, while the model catalog must not invent Antigravity models.

### 4. Generalize OAuth metadata once

Replace provider-specific optional fields in the generic OAuth credential with a metadata map owned by the provider/integration implementation. A target shape is:

```rust
struct OauthCredential {
    refresh: Secret,
    access: Secret,
    expires: u64,
    account_id: Option<String>,
    metadata: BTreeMap<String, Secret>,
}
```

Antigravity metadata includes email, project id, managed project id, and granted scopes. OpenAI enterprise URL moves into the same metadata mechanism. No second `antigravity-accounts.json` is created.

First release supports one active Antigravity identity. Keep the target/identity boundary clean enough for later multi-account support, but do not implement quota scraping, account rotation, or PID-based account selection in this task.

### 5. Native component ownership

Provider/auth/search ownership remains in native in-process Rust components. Process, WASI, or MCP plugins must not own credential refresh, login UI, or request replay. Register every search/auth component through the existing lifecycle so unload/recomposition removes its tools and cancels in-flight requests.

## Implementation phases

## Phase A: anonymous Exa and provider validation

Start with failing tests, then implement:

1. Add `enabled: Option<bool>` to `WebSearchConfig`.
2. Make `SearchConfig::default()` select anonymous Exa.
3. Parse boolean overrides as `Option<bool>`, not truthy-only values.
4. Replace `web_search_enabled` with a usability check for the selected provider.
5. Add `WebError::MissingSearchCredential { provider }`.
6. Reject explicit Parallel selection without `PARALLEL_API_KEY` during tool-runtime assembly.
7. Remove `type` and `livecrawl` from Exa arguments; send only `query` and `numResults`.
8. Keep Exa key placement in the encoded `exaApiKey` query parameter.
9. Preserve batching, stable result order, cancellation, timeout, bounded response size, proxy inheritance, and read-only permission classification.

Required tests:

- default config exposes `web_search` and selects Exa;
- explicit Exa false hides it;
- profile `enabled=false` hides it;
- an Exa key does not override explicit false;
- Parallel key auto-enables Parallel;
- explicit Parallel without key fails before any HTTP request;
- the exact Exa JSON-RPC arguments contain only the live schema fields;
- existing batch ordering, cancellation, timeout, error, and permission tests remain green.

Commit boundary:

`fix(search): enable anonymous Exa by default`

## Phase B: Antigravity authentication integration

Implement in `zuno-auth` and the CLI composition root:

1. Add a native Antigravity OAuth module with browser PKCE.
2. Keep the verifier in process memory; use a random state nonce instead of embedding the verifier in state.
3. Bind the registered localhost callback and support manual redirect/code paste when callback delivery fails.
4. Request offline access and persist refresh/access/expiry atomically in Zuno's protected auth file.
5. Fetch user identity without logging tokens.
6. Refresh with clock skew and preserve project metadata when Google does not replace it.
7. Treat `invalid_grant` as reauthentication-required and retire stale access state.
8. Implement managed-project resolution through `loadCodeAssist` and onboarding.
9. Never send `unknown`, an empty project, or a borrowed hard-coded project id.
10. Cache project resolution per credential generation and invalidate it after refresh/logout.
11. Use the shared proxy-aware HTTP client and Zuno user agent.
12. Add `zuno auth login antigravity`, `auth methods antigravity`, list, and logout behavior.
13. Require a one-time experimental/service-risk acknowledgement; non-interactive login uses an explicit acceptance flag.
14. Redact access tokens, refresh tokens, authorization codes, OAuth state, and client credentials from logs and errors.

Do not place literal OAuth client credentials in documentation, fixtures, snapshots, or logs. Review the reference package's MIT code and service constraints before choosing whether Zuno ships a client identity or requires an operator-provided one.

Required tests:

- browser callback and manual-code flows;
- PKCE/state mismatch rejection;
- token exchange and refresh rotation;
- missing refresh token;
- invalid grant;
- user-info failure with otherwise valid token;
- load-project success;
- onboarding success;
- project discovery failure prevents consumer activation;
- secrets stay redacted in Debug, Display, tracing, snapshots, and auth-list output;
- auth file remains mode 0600;
- bare interactive login lists Antigravity only when both login and consumer are registered.

Commit boundary:

`feat(auth): add native Antigravity login integration`

## Phase C: authenticated google_search

Add a native tool module with a narrow credential seam so `zuno-tools` does not own login storage:

```rust
#[async_trait]
trait GoogleSearchCredentialProvider {
    async fn resolve(&self) -> Result<GoogleSearchCredential, GoogleSearchAuthError>;
}
```

Tool contract:

- id: `google_search`;
- effect: `ReadOnly`;
- concurrency: `ParallelSafe`;
- replay policy: `Safe`;
- hidden when no usable Antigravity credential exists;
- parameters: non-empty `query`, optional validated HTTP(S) `urls`, optional `thinking` defaulting true;
- one separate non-streaming Antigravity `generateContent` request;
- `googleSearch` always enabled for the request;
- `urlContext` added only when URLs are present;
- no function declarations in the search request;
- configurable search model with a proven default;
- `thinking=true/false` must actually map to tested thinking budgets, or the argument must be removed;
- request cancellation uses the turn interrupt;
- timeout and body size are bounded;
- response parser extracts text, grounding sources, search queries, URL retrieval statuses, finish state, and usage metadata;
- search-model token usage is recorded as tool/external usage and never overwrites main-session context usage.

Error taxonomy must distinguish authentication, refresh, unresolved project, rate limit, retryable transport, malformed response, cancellation, and fatal protocol failure. Error bodies are bounded and sanitized before persistence.

Required tests:

- tool absent before login and present after a valid credential fixture;
- exact request body for query-only and query-plus-URLs;
- thinking budget is real, not a dead argument;
- missing/empty/`unknown` project never sends HTTP;
- expired credentials refresh once and persist once;
- 401/403, 429 with retry-after, 5xx, malformed JSON, empty candidates, timeout, cancellation, oversized body;
- grounding citations and URL metadata normalize deterministically;
- concurrent calls share refresh/project resolution without duplicate refresh storms;
- proxy and `NO_PROXY` inheritance;
- strict authorization does not ask for this read-only search tool.

Add a credential-gated ignored E2E test for a real Antigravity account. Never run it in normal CI and never record its token or raw response headers.

Commit boundary:

`feat(search): add authenticated Google grounding`

## Phase D: product surfaces and documentation

CLI/TUI behavior:

- `auth list` identifies Antigravity as an integration, masks the account, and reports active, expired, or reauthentication-required state;
- the tool/activity metadata shows `Exa · anonymous`, `Exa · API key`, `Parallel · API key`, or `Google · Antigravity`;
- authenticated Google search failures include the exact corrective command without exposing credentials;
- the sidebar/integration status updates after login/logout without restarting the TUI;
- transcript details retain provider, latency, source count, and external usage without dumping raw OAuth or internal headers.

Documentation:

- explain that Exa receives search text by default and show the opt-out;
- distinguish `web_search` from `google_search`;
- distinguish search integration auth from model-provider auth;
- document proxy behavior;
- state that Antigravity uses undocumented internal services and is experimental;
- clarify that the Consumer Gemini Code Assist Google-login entitlement ended, not that the Gemini CLI codebase stopped being maintained;
- do not promise that Antigravity login automatically creates a Zuno Gemini model provider in this task.

Commit boundary:

`docs(search): document search providers and Antigravity risks`

## Acceptance gates

Run targeted gates after every phase and record complete command output before claiming success:

```text
cargo test -p zuno-tools --test websearch
cargo test -p zuno-tools
cargo test -p zuno-auth
cargo test -p zuno
cargo test -p zuno-config
```

Final gates:

```text
cargo test --workspace
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo fmt --all -- --check
git diff --check
cargo build --release
```

Also run:

- a live no-key Exa MCP probe;
- CLI PTY coverage for Antigravity login/list/logout and cancellation;
- a proxy/no-proxy fixture;
- a credential-gated real Antigravity search E2E when the user explicitly authorizes account use;
- CodeGraph status/index refresh after source changes;
- `git status --short` and `git worktree list` before each commit;
- staged-diff review proving `.omo/notepads` changes were not included.

## Explicit non-goals

This task does not:

- add an Antigravity/Gemini chat model provider;
- implement multi-account rotation or quota polling;
- copy OpenCode's JavaScript plugin ABI;
- move authentication into a process/WASI/MCP plugin;
- silently fall back from Google search to Exa after an authenticated call fails;
- add migration or compatibility code for the old unreleased search/auth schema.

## Handoff checklist

Before editing in the next task:

1. Open the repository directly at `/config/workspace/ProdDir/AI/zuno`.
2. Read this roadmap completely.
3. Read any repository `AGENTS.md` instructions.
4. Confirm CodeGraph is current.
5. Confirm only the two `.omo/notepads` files are pre-existing modifications.
6. Begin Phase A with failing tests.
7. Use patch-based edits and preserve unrelated changes.
8. Stop and report if OAuth client/service authorization cannot be implemented without copying an unapproved third-party identity.
