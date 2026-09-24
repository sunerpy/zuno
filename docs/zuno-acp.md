# Native ACP

Zuno exposes the Agent Client Protocol (ACP) as a native stdio frontend:

```sh
zuno acp
```

ACP is a protocol projection over Zuno's in-process App Server. It does not run a
second agent loop: ACP sessions map to the same durable threads and turns used by
App Server, so cancellation, approvals, sandbox policy, history, and persisted
state keep their existing owners.

## Configuration

The command accepts the normal runtime configuration layers. Common examples:

```sh
# Select a user profile.
zuno --profile kiro acp

# Override the default model and working directory for sessions that do not
# provide their own values.
zuno acp --model gpt-5.6-sol -c model_provider='"kiro-local"' --cd /work/project

# Apply explicit permission defaults and an additional writable root.
zuno acp --sandbox workspace-write --add-dir /work/shared

# Reject unknown configuration fields.
zuno acp --strict-config
```

Arbitrary configuration overrides use the standard global `-c key=value`
syntax. Provider credentials and provider definitions belong in configuration;
`zuno acp` deliberately rejects `--oss` and `--local-provider`. ACP clients may
select a model, mode, or session working directory through the protocol when the
client supports those operations.

The ACP transport owns stdout. Logs and diagnostics must not be written to the
protocol stream.

## Protocol surface

The adapter currently implements stable ACP v1 methods for initialization,
session create/load/resume/fork/list, prompt and same-process steering, model,
mode and configuration updates (`session/set_mode`, `session/set_model`, and
`session/set_config_option` all reach the same thread settings), cancellation,
close, and delete. Permission requests and turn/session updates are translated to
and from App Server events.

Prompt blocks map to App Server input the same way the reference `codex-acp`
adapter maps them, so a prompt means the same thing to the model whichever Codex
ACP agent a client talks to: `text` and `image` pass through (an `http(s)` or
`data:` image URI is used as-is), `resource_link` becomes a `[@name](uri)` link,
an embedded text `resource` becomes that link followed by a
`<context ref="uri">` block, an embedded `image/*` blob becomes an image, and any
other blob becomes a base64 `<context>` block. Block types the `initialize`
response does not advertise (for example `audio`) are rejected with
`-32602`.

The bridge reports two outcomes of its own with implementation-defined JSON-RPC
codes chosen not to alias anything either side already uses: `-32010` when a
`session/prompt` was admitted as steering into the session's live turn (the
`data` field carries the durable admission), and `-32011` when a `session/steer`
request cannot target the live turn. ACP's `-32000` (authentication required)
and `-32002` (resource not found) and App Server's `-32001` (overloaded) pass
through unchanged.

## Protocol version and client compatibility

The bridge speaks ACP wire protocol **1**, the stable version. ACP versions
three things independently: the `protocolVersion` integer negotiated in
`initialize` (1 stable, 2 draft), the JSON schema release (1.x), and the SDK
packages (the Rust `agent-client-protocol` crate is at 2.x while still speaking
wire protocol 1). A client that upgrades its SDK, such as Zed 1.21 moving to
`agent-client-protocol` 2.1 with schema 1.7, still sends `protocolVersion: 1`;
nothing in Zuno changes for it. Protocol 2 will be added behind explicit
negotiation once it leaves draft status, keeping protocol 1 served.

Within protocol 1 the bridge tracks the schema additions clients rely on:

- `tool_call` updates carry the first-class `name` (schema 1.8) alongside
  `title` and `kind`: `shell`, `apply_patch`, `web_search`, `spawn_agent`
  (which Zed uses to recognise sub-agents), the MCP or dynamic tool's own name,
  `view_image`, `image_generation`, `sleep`.
- App Server lifecycle items (`contextCompaction`, review-mode markers,
  `functionCallOutput`) are not projected as tool calls.
- When the client advertises `clientCapabilities.elicitation.form` (schema
  1.7), tool questions from the model (`item/tool/requestUserInput`) are asked
  through one `elicitation/create` form. The App Server tool always allows a
  free-form "Other" answer (`isOther`), so such questions stay `string`
  properties that list the suggested options in their description; only
  questions without `isOther` become closed `enum` properties. Secret questions
  are refused because form mode must not carry credentials. Clients without form
  elicitation keep the previous behaviour: the listed options through
  `session/request_permission`, no free-form answer.

Zuno-specific capabilities continue to evolve behind explicit protocol
metadata. An ACP SDK package version alone does not opt a connection into an
unstable wire protocol.
