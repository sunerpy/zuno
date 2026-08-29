# Trusted process plugin development

This guide describes how to build a static Zuno extension whose executable
tools run in a contained child process. It covers the package contract, the
line-delimited JSON-RPC protocol, lifecycle and failure semantics, security
review, testing, and the operator-local OpenCode Antigravity search bridge.

Use this runtime only when the tool needs a normal host executable or an API
that a bounded WASI component cannot expose. A process plugin is a trusted
tool adapter, not a general replacement for native Zuno components.

Start with the built-in `/develop-zuno` Skill when authoring from an interactive
session. It selects among configuration, Agent Markdown, Skills, declarative
packages, WASI, trusted process tools, and native Rust, then points back to this
guide. The Skill does not grant any runtime capability.

## Choose the correct extension tier

Before writing a process host, decide who must own the behavior:

| requirement | implementation |
| --- | --- |
| Agents, slash workflows, or skills with no guest code | Declarative extension package |
| A tool that can run with explicit filesystem, environment, and network grants | WASI component |
| A tool that needs a normal host runtime, an installed SDK, or an existing local application account | Trusted process plugin |
| Provider transport, login method, credential store, `AgentDriver`, approval service, or sub-turn orchestration | Native Rust `Component`, typed service, or `AgentDriver` |

Process and WASI plugins export tools. They cannot register a provider,
credential authority, interaction surface, or hidden child-turn scheduler.
Do not move a complete provider or authentication lifecycle into a tool merely
to avoid implementing the native interface.

## Package layout

A process package is an ordinary directory:

```text
remote-search/
├── extension.json
├── plugin.mjs
├── adapter.mjs
├── plugin.test.mjs
└── README.md
```

`extension.json` is required. The remaining names are conventions:

- keep the JSON-RPC loop small and protocol-only in `plugin.mjs`;
- put service discovery, credentials, HTTP calls, and result shaping in
  `adapter.mjs`;
- unit-test the adapter without starting Zuno;
- document every inherited environment variable, credential source, and
  external side effect in the package README.

The directory name must equal the manifest `id`. Static installation rejects
symbolic links and special filesystem entries, so packages remain complete,
copyable directory trees.

## Complete manifest

The following manifest exports one trusted search tool:

```json
{
  "apiVersion": "zuno.extension/v1",
  "id": "remote-search",
  "description": "Queries an operator-authorized external search service.",
  "runtime": {
    "kind": "process",
    "command": "bun",
    "args": ["plugin.mjs"],
    "capabilities": ["host.full"],
    "timeoutMs": 75000
  },
  "tools": {
    "remote_search": {
      "description": "Search an operator-authorized external service.",
      "parameters": {
        "type": "object",
        "properties": {
          "query": {
            "type": "string",
            "minLength": 1,
            "maxLength": 8000
          },
          "urls": {
            "type": "array",
            "maxItems": 20,
            "items": {
              "type": "string",
              "format": "uri",
              "maxLength": 2048
            }
          }
        },
        "required": ["query"],
        "additionalProperties": false
      },
      "effect": "sideEffecting",
      "replay": "never",
      "concurrency": "exclusive",
      "uiIntent": "generic"
    }
  }
}
```

The process runtime has intentionally strict rules:

- `capabilities` must be exactly `["host.full"]`;
- `timeoutMs` is in `1..=300000` and defaults to `30000`;
- a runtime must export at least one tool, and tools require a runtime;
- process tools must use `effect: "sideEffecting"` and `replay: "never"`;
- protocol v1 serializes one runtime instance, so concurrency must remain
  `exclusive`;
- runtime plugins cannot claim `userMediated` or `delegating` effects;
- the package id is at most 64 lowercase ASCII letters, digits, `.`, `_`, or
  `-`, and begins with a letter or digit;
- contribution names are at most 96 ASCII alphanumeric or `-_.:/` characters
  and cannot begin with `/`.

Use a closed JSON Schema with `additionalProperties: false`. The schema is the
first boundary, not the only boundary: the adapter must repeat semantic
validation before it touches credentials, the network, or the filesystem.

## Process environment

Zuno starts the executable with the package directory as its current working
directory. The child inherits the Zuno process environment and host authority,
including:

- filesystem and network access available to Zuno;
- credential-related environment variables;
- `HTTP_PROXY`, `HTTPS_PROXY`, `ALL_PROXY`, and `NO_PROXY`;
- the parent process locale and executable search path.

There is no process-runtime environment allowlist. If the package is not
trusted with all inherited authority, do not install it directly. Prefer a WASI
component or launch the entire Zuno deployment inside an operating-system or
container sandbox.

An adapter should treat inherited proxy variables as deployment policy. Do not
rewrite process-global proxy state per tool call. If an SDK accepts a typed
proxy or `no_proxy` option, derive it from the inherited environment and keep
the setting local to that client.

## JSON-RPC protocol

The protocol is JSON-RPC 2.0 over standard input and standard output, with
exactly one JSON object per line. The process must write no banners, progress
messages, or debug logs to stdout.

### Initialize

Zuno sends:

```json
{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"zuno.plugin/1","packageId":"remote-search","packageRoot":"/path/to/remote-search","workspace":"/path/to/worktree","capabilities":["host.full"]}}
```

The plugin must return:

```json
{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"zuno.plugin/1"}}
```

Reject an unsupported version with a JSON-RPC error. Do not start background
workers or open durable resources before the initialization checks have
succeeded.

### Tool call

For each invocation Zuno sends:

```json
{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"tool":"remote_search","arguments":{"query":"Rust language official site"},"sessionId":"ses_123","messageId":"msg_123","callId":"call_123","agent":"orchestrator"}}
```

A successful result is:

```json
{"jsonrpc":"2.0","id":2,"result":{"title":"Search results","output":"# Results\n\n...","metadata":{"provider":"example","latencyMs":412,"sourceCount":3}}}
```

`title` and `output` are required strings. `metadata` is optional, but when
present it must be a JSON object or `null`. Metadata is diagnostic and
presentation data; adding an arbitrary `usage` field does not make it part of
Zuno's native provider usage accounting.

Return expected service, validation, and authentication failures as JSON-RPC
errors:

```json
{"jsonrpc":"2.0","id":2,"error":{"code":-32000,"message":"no enabled account with a managed project is available"}}
```

Never include access tokens, refresh tokens, authorization headers, OAuth
client secrets, or unredacted upstream response bodies in the message.

### Shutdown

Zuno requests graceful cleanup:

```json
{"jsonrpc":"2.0","id":3,"method":"shutdown","params":{}}
```

Reply before exiting:

```json
{"jsonrpc":"2.0","id":3,"result":{}}
```

Close sockets, stop workers, flush only non-secret diagnostics, and release
locks. Zuno applies a bounded termination and reaping sequence if the process
does not exit.

### Protocol loop

A minimal JavaScript entrypoint is:

```js
import readline from "node:readline"
import { callTool, initialize, sanitizeError, shutdown } from "./adapter.mjs"

function send(id, result, error) {
  const message = { jsonrpc: "2.0", id }
  if (error) message.error = { code: -32000, message: error.message }
  else message.result = result
  process.stdout.write(`${JSON.stringify(message)}\n`)
}

const lines = readline.createInterface({
  input: process.stdin,
  crlfDelay: Infinity,
})

for await (const line of lines) {
  let request
  try {
    request = JSON.parse(line)
    if (request.method === "initialize") {
      send(request.id, await initialize(request.params))
    } else if (request.method === "tools/call") {
      send(request.id, await callTool(request.params))
    } else if (request.method === "shutdown") {
      send(request.id, await shutdown())
      break
    } else {
      throw new Error(`unknown method: ${request.method}`)
    }
  } catch (error) {
    send(request?.id ?? null, undefined, sanitizeError(error))
  }
}
```

The real implementation still needs bounded input, semantic argument
validation, signal handling, output sanitization, and tests. The canonical
small example is
[`examples/plugins/process-review`](https://github.com/sunerpy/zuno/blob/main/examples/plugins/process-review).

## Logging and output discipline

Use stderr for logs and stdout only for protocol frames:

```js
process.stderr.write(
  `${JSON.stringify({ level: "info", event: "search.completed", latencyMs })}\n`,
)
```

Follow these rules:

- never log whole environments, credential objects, headers, or raw account
  files;
- redact exact known secrets and token-shaped strings before logging or
  returning errors;
- prefer stable event names and bounded scalar fields over free-form dumps;
- cap result text well below the protocol frame limit;
- deduplicate URLs and preserve deterministic output ordering;
- strip ANSI control sequences and reject invalid Unicode or binary payloads.

Zuno bounds one response frame to 8 MiB and captured stderr to 64 KiB. These
are safety ceilings, not output targets. A search adapter should normally keep
its complete result below a few hundred KiB.

## Cancellation, timeout, and uncertain outcomes

Protocol v1 has no in-band `cancel` request. User cancellation or `timeoutMs`
retires the plugin process and terminates its owned process tree. The plugin
should handle normal termination signals promptly and must not detach
unmanaged children.

If Zuno sent `tools/call` and then lost the response, the external side effect
may already have happened. Zuno reports that outcome as `Uncertain`, retires
the process, and never mechanically replays the call. This is why every process
tool is side-effecting and replay-never even when its ordinary operation looks
like a read.

Design the upstream operation around this boundary:

- attach idempotency keys when the service supports them;
- return authoritative resource identifiers;
- provide a separate inspection tool for side effects that may need
  reconciliation;
- never interpret timeout as proof that the upstream operation did not happen.

## Credential and authorization ownership

A bridge may consume credentials owned by another installed application only
when all of the following are true:

1. the operator explicitly authorizes that use;
2. the existing application owns login, refresh, logout, and storage;
3. the account is authorized for the target service;
4. the adapter does not copy or embed another application's OAuth client
   identity;
5. project, tenant, or workspace identifiers come from the authorized account
   or service response, not a hard-coded fallback;
6. missing, logged-out, expired, or incomplete credentials fail closed.

Do not copy OAuth client ids, client secrets, internal project ids, refresh
tokens, or fallback projects from a third-party package into Zuno source or
plugin source. If a service requires an operator-owned OAuth identity, obtain
and configure that identity through the service's supported authorization
process before implementing the bridge.

When an upstream package rotates a refresh token, persist only through that
package's credential manager. Do not create a second token database whose
logout and revocation lifecycle can diverge.

## Install, update, and permission configuration

Project packages live under `.zuno/extensions`; user-global packages normally
live under `~/.config/zuno/extensions`.

```sh
# Install or validate a new project package.
zuno plugin add ./remote-search --project
zuno plugin list

# Transactionally replace an installed package.
zuno plugin update ./remote-search --project

# Remove it from future host compositions.
zuno plugin remove remote-search --project
```

`add` refuses to overwrite an existing package. `update` stages a complete
copy, swaps it, and restores the previous directory if replacement fails.
Running hosts keep their already-mounted composition until they stop or their
surface performs a supported remount; start a new host when validating an
installed runtime change.

Process tools are side-effecting. To allow one named tool without opening every
side effect, use a narrow rule:

```json
{
  "permission": {
    "mode": "standard",
    "rules": {
      "remote_search": "allow"
    }
  }
}
```

In strict mode a fresh attached-user approval is still required for every
process-tool call. Headless execution must fail closed when the effective
policy asks for approval and no approver is attached.

## Test matrix

A production-oriented process plugin should cover:

### Manifest and installation

- valid manifest installation and `zuno plugin list`;
- wrong directory/id pair;
- unknown manifest keys and invalid JSON Schema;
- missing executable or dependency;
- transactional update failure and old-package restoration;
- duplicate tool names against another active package.

### Protocol

- successful initialize, call, and shutdown;
- unsupported protocol version;
- unknown method and unknown tool;
- malformed JSON and malformed result objects;
- stdout contamination;
- oversized response and stderr bounds;
- non-zero exit before and after a call is admitted.

### Arguments and results

- required, empty, maximum, and over-limit strings;
- URL scheme, count, length, and deduplication;
- stable result ordering;
- ANSI and control-character removal;
- bounded output and metadata;
- secret and token redaction in every error path.

### Lifecycle

- foreground cancellation;
- timeout while the upstream call is pending;
- process-tree cleanup with a spawned child;
- no residual process after shutdown;
- call after a retired protocol host;
- lost response reported as uncertain and never replayed.

### Credentials and network

- no account, disabled account, logged-out account, and expired account;
- successful refresh and refresh-token rotation;
- missing authorized project or tenant id;
- `HTTP_PROXY`/`HTTPS_PROXY`/`ALL_PROXY` and `NO_PROXY`;
- upstream 401, 403, 429, timeout, malformed response, and service outage;
- a real operator-authorized end-to-end query through Zuno, followed by a
  credential-store and process-tree integrity check.

Keep the direct adapter E2E separate from the Zuno-hosted E2E. The first proves
the service adapter; the second proves manifest discovery, process protocol,
permissions, cancellation, lifecycle, and presentation.

## Case study: OpenCode Antigravity search bridge

The operator-local package `opencode-antigravity-search` exposes a
`google_search` tool by reusing an existing, enabled OpenCode Antigravity
account. It is an external bridge, not Zuno's native Antigravity integration.

The flow is:

```text
Zuno google_search tool
  -> trusted process JSON-RPC host
  -> adapter discovers the installed opencode-antigravity-auth package
  -> that package loads its own enabled account
  -> that package refreshes its own credential when required
  -> search executes with the account's managed project
  -> adapter validates, redacts, bounds, and returns the result
```

The adapter imports the installed package's own `AccountManager`,
`refreshAccessToken`, and `executeSearch` implementation. It does not copy the
package's OAuth client identity, access or refresh token, managed project id,
or fallback project into Zuno.

### Prerequisites

- `opencode-antigravity-auth` is installed in the operator's OpenCode package
  cache or configured package root;
- OpenCode has an enabled Gemini-family Antigravity account;
- the account owns a refresh token and a real managed project returned by the
  service;
- the operator has authorized using that account to send Google search
  requests;
- the current network and proxy policy can reach the service.

The adapter fails closed if the project is empty or the placeholder
`"unknown"`, if no enabled account is available, or if refresh/search fails.

### Package discovery

The bridge supports these discovery inputs:

1. `OPENCODE_ANTIGRAVITY_PACKAGE_ROOT` for an exact installed package root;
2. `OPENCODE_CONFIG_DIR` for a non-default OpenCode configuration root;
3. the normal XDG/OpenCode package cache, selecting the newest usable installed
   package.

An optional direct override is useful for development and version pinning.
Production operators should pin and test a known package version because the
bridge consumes private `dist/src/plugin/*` exports rather than a stable public
SDK.

### Operator checks

For the current operator-local package:

```sh
bun ~/.config/zuno/extensions/opencode-antigravity-search/plugin.mjs --check
zuno plugin list
```

`--check` is package-specific, not part of the Zuno protocol. It reports the
resolved upstream package version, enabled account count, managed-project
readiness, and whether the bridge can attempt a call. It must not print tokens
or OAuth identity.

Allow only the bridge tool when headless execution is intended:

```json
{
  "permission": {
    "mode": "standard",
    "rules": {
      "google_search": "allow"
    }
  }
}
```

Start a new Zuno host after installing or replacing the package so the static
extension composition includes the tool.

### Result and diagnostic contract

The bridge:

- accepts one query and up to 20 optional HTTP(S) URLs;
- normalizes and deduplicates requested URLs;
- bounds model-visible output to 512 KiB;
- strips ANSI control sequences and redacts token-shaped values;
- returns provider, credential owner, upstream package version, latency,
  source count, and requested URL count as metadata;
- reports `usageAvailable: false` because this private search path does not
  expose reliable native usage accounting.

Access-token caching is keyed by a digest of the refresh token and expires
before the token itself. Refresh-token rotation is persisted through the
upstream account manager; access tokens are not copied into a Zuno credential
store.

### Known limitations

- the upstream `dist/src/plugin/*` modules are private and may change without a
  compatible release contract;
- the tool remains registered after OpenCode logout until the Zuno host is
  rebuilt, but each invocation rechecks credentials and fails closed;
- OpenCode remains the credential owner, so Zuno cannot present native
  `auth login`, refresh status, or logout lifecycle for this bridge;
- the process has `host.full` authority and must be reviewed as trusted code;
- account and service terms may restrict automation even when credentials are
  technically usable;
- the bridge does not complete the native Antigravity phases in
  [the web search roadmap](design/web-search-antigravity-roadmap.md).

A future native implementation must use an operator-owned and service-authorized
OAuth identity, native Zuno credential management, explicit login/refresh/logout
lifecycle, and a real service-returned project id. The external bridge is useful
when an operator deliberately wants to reuse an existing OpenCode account, but
it must not become a source of copied authorization identity.
