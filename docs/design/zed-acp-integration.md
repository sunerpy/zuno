# Zed ACP integration

## Decision

Zuno targets the latest reviewed stable ACP V1 schema for its Zed integration.
The V2 schema remains an explicitly labeled preview input until it is stable and
Zed negotiates it in production. Protocol implementation work must start from
the audited files under `docs/upstream/acp`, not from a floating branch or an
unversioned web page.

The 2026-08-26 baseline is:

| Input | Pin | Commit | Role |
| --- | --- | --- | --- |
| ACP stable schema | `schema-v1.21.0` | `272bf799f35a258c6a4107a0410ed361e83683d3` | Production wire contract |
| ACP Rust schema crate | `v1.7.0` | `272bf799f35a258c6a4107a0410ed361e83683d3` | Typed Rust schema baseline |
| ACP V2 preview | `schema-v2.0.0-alpha.3` | `272bf799f35a258c6a4107a0410ed361e83683d3` | Design tracking only |
| Zed | `ac099b4a809a564f06907125e7a536c33cb60084` | same | Client behavior observation |

The exact annotated tag objects, release asset IDs, URLs, sizes, hashes, fetch
time, and upstream main commits observed during capture are recorded in
`docs/upstream/acp/manifest.json`.

## Official adapter behavior references

Zuno also reviews the official ACP adapters as executable design evidence. They
are not wire-contract authorities and Zuno does not copy their implementation:

| Adapter | Reviewed commit | What Zuno reviews |
| --- | --- | --- |
| [`agentclientprotocol/codex-acp`](https://github.com/agentclientprotocol/codex-acp) | [`2b48e9822330fc09f3a94a81563e5c4bb779601a`](https://github.com/agentclientprotocol/codex-acp/commit/2b48e9822330fc09f3a94a81563e5c4bb779601a) | Capability negotiation, `thought_level` configuration, command publication, rich prompt conversion, cancellation, tool and subagent projection, file changes, usage, and durable thread loading |
| [`agentclientprotocol/claude-agent-acp`](https://github.com/agentclientprotocol/claude-agent-acp) | [`8710ce1cbccf562cb04b4bcc30e053e960aee05f`](https://github.com/agentclientprotocol/claude-agent-acp/commit/8710ce1cbccf562cb04b4bcc30e053e960aee05f) | Permission and elicitation lifecycle, edit review, TODO projection, client MCP, terminal ownership, and cancellation races while user input is pending |

For every ACP change, review the stable schema first, then compare the same
behavior in these pinned adapters and Zed. Classify the result as `adopt`,
`adapt`, or `reject` for Zuno's durable runtime. Adapter-specific extensions,
authentication identities, hidden prompts, and runtime assumptions are not
inherited merely because an adapter implements them.

The adapter commits are intentionally recorded separately from the vendored
schema assets. A future review must choose new exact 40-character commits and
update this table; it must not replace the pins with floating `main` links.

## Upstream authorities

- Repository: <https://github.com/agentclientprotocol/agent-client-protocol>
- Stable protocol documentation:
  <https://agentclientprotocol.com/protocol/v1/overview>
- Zed external agents documentation:
  <https://zed.dev/docs/ai/external-agents>
- Zed evidence at the pinned commit:
  <https://github.com/zed-industries/zed/blob/ac099b4a809a564f06907125e7a536c33cb60084/crates/agent_servers/src/acp.rs#L993>

At the pinned Zed commit, the production connection path constructs
`InitializeRequest::new(ProtocolVersion::V1)`. Zed also declares V1 as its
minimum supported version in the same file. This is why Zuno's production
target is stable V1 even while V2 exists as an alpha schema.

Zed's repository is GPL-3.0-or-later. Zuno may use public behavior and protocol
observations as design evidence, but this snapshot does not copy Zed source.
The ACP repository and copied schema assets are distributed under Apache-2.0;
the exact upstream license is stored at `docs/upstream/acp/LICENSE`.

## Integration boundary

ACP is a client projection over Zuno's durable runtime, not a second agent
loop. Zed requests must enter the same durable inbox and session lifecycle as
the TUI, CLI, server, and future clients. ACP notifications must be projections
of durable events, and disconnecting Zed must not silently discard durable
work.

The stable schema snapshot is the acceptance source for these surfaces:

- initialization, version negotiation, implementation metadata, and honest
  client/agent capabilities;
- authentication methods only when a real handler exists;
- session creation, loading, listing, resuming, deletion, closing, mode/config
  changes, prompting, and cancellation where present in the stable schema;
- text, image, embedded-resource, and resource-link content only when the
  negotiated capabilities and runtime implementation both support them;
- streaming assistant and reasoning chunks;
- tool-call start/update/completion, raw input/output, locations, terminal
  content, and file diff content;
- human-in-the-loop permission requests with typed outcomes and cancellation;
- client file reads/writes and terminal operations only through negotiated
  handlers, with errors preserved rather than replaced by successful stubs;
- elicitation, plan, usage, configuration, mode, model, and session information
  updates where defined by the pinned schema;
- durable replay on load, live event ordering, backpressure, process
  cancellation, and exact session teardown.

Capabilities are promises. Zuno must not advertise an ACP method, content type,
MCP transport, filesystem operation, terminal operation, renderer, or
authentication flow until its provider and runtime consumer are wired and
tested.

## Implemented production surface

`zuno acp` is the production stdio entry point. It uses the same `TurnHost`,
durable SQLite sessions, provider configuration, tools, permission policy, MCP
runtime, work state, and lifecycle as the CLI and TUI; it is not a second agent
loop.

| Area | Production behavior |
| --- | --- |
| Initialization | Negotiates stable protocol V1 and reports schema `1.21.0`. Authentication methods are empty because Zuno uses its own configured provider credentials and has no ACP login handler. |
| Session lifecycle | Implements `session/new`, `load`, `resume`, `list`, `delete`, and `close`. Loading replays durable client-visible history; resuming continues without duplicating that replay. |
| Session configuration | Implements build/plan modes plus transactional Agent, model, and `reasoning_effort` replacement. The reasoning selector uses ACP category `thought_level` and only publishes levels supported by the active catalog model. Reconfiguration is rejected while a prompt is active and rolls back on failure. |
| Commands and Skills | Publishes executable Catalog commands and unambiguous slash-invokable Skills through `available_commands_update` after new/load/resume and successful reconfiguration. `/name arguments` reuses the same command or Skill driver as other Zuno surfaces; adapter-specific commands are not invented. |
| Prompt execution | Admits input through the durable Zuno turn path, streams projections while the turn runs, and projects the final durable plan before returning. Concurrent prompts for one session are rejected. |
| Prompt content | Advertises and accepts text, inline image, native `resource_link`, embedded text resource, and embedded image resource content. Audio remains `false`. Resource links stay typed; images use typed durable file parts; embedded text keeps URI, MIME, and body in one stable persisted envelope. Selection, diagnostics, fetched context, and branch diff use the generic embedded-resource path rather than Zed-specific prompt branches. |
| Assistant and tool projection | Streams assistant text and reasoning, tool start/update/completion, accumulated raw input, raw output, JSON/text content, written-file locations, and usage. |
| File edits and diffs | Zuno's native file tools produce stable ACP `diff` content with an absolute path and exact `oldText`/`newText`. A unified-diff text fallback remains only for tools that cannot provide typed file state. |
| Human input | Routes tool permission through `session/request_permission` and the question tool through stable form elicitation when the client advertises it. Unknown, malformed, declined, and cancelled outcomes fail closed. |
| Cancellation | `session/cancel`, JSON-RPC `$/cancel_request`, stdin EOF, and process shutdown abort active work and settle pending agent-to-client requests instead of leaving the transport hung. Request-id cancellation calls back into the Agent with the original method and params before dropping the handler future, allowing Zuno to abort the matching session process tree. |
| Durable load replay | Reconstructs user/assistant content, reasoning, tools, raw input/output, typed diffs, locations, resource links and image output, plan, and latest-context usage from durable session state. |
| ACP-provided MCP | HTTP and SSE MCP capabilities remain `false`. Zuno may still mount MCP servers from its own validated native configuration. |
| Client filesystem RPC | Not advertised. Agent file reads and writes use Zuno tools, sandbox/permission policy, and durable events; they do not masquerade as ACP client filesystem handlers. |
| Terminal RPC | Not advertised. Zuno will not emit terminal references until create/output/wait/kill/release ownership and cancellation are implemented as one lifecycle. |

`resource_link` remains typed through ingress, SQLite, compaction, goal
continuation, steering, and load replay. Text-only provider protocols lower it
only at the provider boundary. This prevents Zed from reopening a thread with
lossy prose in place of the original URI metadata.

Embedded text is bounded to 50 KiB and 2,000 lines per resource. Inline or
embedded images must be valid base64 PNG, JPEG, GIF, or WebP payloads no larger
than 5 MiB; the ACP transport also enforces its 8 MiB frame ceiling. Binary
embedded resources other than images are rejected instead of injecting opaque
base64 into model context.

## Zed setup and verification

The canonical operator guide is
[Use Zuno in Zed through ACP](../reference/zed-acp.md). It contains the current
Zed `agent_servers` entry, Linux/macOS/Windows executable discovery, Zuno
configuration overlays, `deep` and other Agent selection, permission ownership,
logging, troubleshooting, and a copyable acceptance sequence.

This design document remains the protocol and architecture authority. Provider
keys, OAuth state, model routing, Agents, Skills, extensions, permissions, and
MCP configuration remain Zuno-owned rather than Zed-owned.

Run this acceptance sequence from a Zed External Agent thread:

1. Start a Zuno thread and confirm the mode, Agent, model, and reasoning
   selectors are populated from Zuno.
2. Open `/` completion and verify configured commands and unambiguous Skills
   appear, then execute one command.
3. Add an image, selection, and branch diff; verify the selected model receives
   the supported content without a protocol error.
4. Ask for a read-only inspection and verify reasoning and tool details stream
   without corrupting stdout JSON-RPC.
5. Request a file creation under strict permission policy, answer the Zed
   permission card, and verify the native creation diff has `oldText: null`.
6. Ask a structured question and verify the elicitation form returns the answer
   to the same turn.
7. Cancel a running prompt and verify the thread returns to an idle state.
8. Close and reopen or import the thread, then verify content, tools, diff,
   plan, resource link, and usage are replayed once.

Use Zed's `dev: open acp logs` command when diagnosing framing, ordering, or
capability problems. ACP stdout is protocol-only; diagnostics belong on stderr
or in Zuno's configured logs.

The repository covers the same path without a GUI through production stdio
tests:

```sh
cargo test -p zuno-acp
cargo test -p zuno-cli --test acp_stdio
```

Those tests drive initialization, lifecycle, dynamic reasoning configuration,
command publication, mode/model replacement, rich prompt parsing, prompt
streaming, strict permission, form elicitation, typed creation diff,
resource-link roundtrip, request-to-session cancellation, durable load replay,
plan, and usage. A release is not allowed to claim Zed UI acceptance until the
manual sequence above has also been run against the intended Zed build and
platform.

## Audited snapshot

The snapshot layout is:

```text
docs/upstream/acp/
├── LICENSE
├── README.md
├── SHA256SUMS
├── manifest.json
└── assets/
    ├── stable/
    │   ├── meta.json
    │   ├── meta.unstable.json
    │   ├── schema.json
    │   └── schema.unstable.json
    └── v2-preview/
        ├── meta.json
        ├── meta.unstable.json
        ├── schema.json
        └── schema.unstable.json
```

`meta.json` and `schema.json` are the stable release outputs.
`meta.unstable.json` and `schema.unstable.json` are retained because they make
preview/stable movement auditable; retaining them does not enable unstable
features in Zuno. The V2 directory is similarly reference-only.

The manifest separates:

- immutable release identity: tag, annotated tag object, and peeled commit;
- release artifacts: asset ID, exact URL, byte size, and SHA-256;
- observations: fetch timestamp and upstream main commits at that time;
- Zed evidence: exact commit, source path, line, and requested protocol
  version;
- licensing: SPDX identifier, source URL, snapshot path, and checksum.

## Refresh contract

The two update entry points implement the same contract:

```sh
./scripts/update-acp-spec.sh --verify
./scripts/update-acp-spec.sh --check-upstream
./scripts/update-acp-spec.sh --refresh
```

```powershell
pwsh -File scripts/update-acp-spec.ps1 -Mode Verify
pwsh -File scripts/update-acp-spec.ps1 -Mode CheckUpstream
pwsh -File scripts/update-acp-spec.ps1 -Mode Refresh
```

They fail closed when:

- a pin is malformed;
- a tag is absent, is not annotated, or peels to an invalid commit;
- a GitHub release is missing or names a different tag;
- any required asset is missing or duplicated;
- an asset URL leaves the expected official repository;
- GitHub omits a SHA-256 release digest;
- downloaded size or SHA-256 differs from release metadata;
- the pinned commit does not contain the Apache-2.0 license;
- the pinned Zed commit cannot be read or no longer shows the V1 minimum and
  initialization request;
- the checked-in file set, checksum list, manifest, or upstream bytes differ.

`Refresh` stages every input under a temporary directory and copies files into
the repository only after all remote checks pass. It does not resolve
`latest`. By default it reproduces the pins already stored in the manifest. A
version review supplies explicit `ACP_STABLE_TAG`, `ACP_CRATE_TAG`,
`ACP_PREVIEW_TAG`, and `ZED_COMMIT` values, runs `Refresh`, reviews the manifest
and schema diff, and then runs `CheckUpstream`.

The POSIX script supports Linux and macOS and requires `curl`, `jq`, and
standard POSIX utilities. The PowerShell script supports Windows with
PowerShell 7 or later. `GITHUB_TOKEN` is optional and is used only as an
authorization header for GitHub rate limits.

## Review checklist for a future pin change

1. Read the ACP release notes and protocol documentation for every stable
   addition, removal, or stabilization.
2. Review the schema diff between the old and candidate stable assets.
3. Inspect the candidate Zed commit and confirm which protocol version its
   production connection requests.
4. Review exact candidate commits for `codex-acp` and `claude-agent-acp`, then
   update the behavior-reference table without using floating branches.
5. Run `Refresh` with all reviewed schema and Zed pins explicitly provided.
6. Inspect `manifest.json`, `SHA256SUMS`, and all schema diffs.
7. Classify each protocol change as implemented, intentionally unsupported, or
   preview-only in the ACP implementation plan.
8. Run both platform scripts in offline verification mode; run at least one
   online `CheckUpstream` on each release platform before publishing.
