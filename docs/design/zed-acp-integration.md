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
| [`agentclientprotocol/codex-acp`](https://github.com/agentclientprotocol/codex-acp) | [`50f69e57ca761ccafd2ca29de7fb591068277516`](https://github.com/agentclientprotocol/codex-acp/commit/50f69e57ca761ccafd2ca29de7fb591068277516) | Capability negotiation, model/approval/sandbox configuration, tool and subagent projection, file changes, terminal events, usage, and durable thread loading |
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
| Session lifecycle | Implements `session/new`, `load`, `resume`, `list`, `delete`, and `close`. Loading and resuming create a dormant session without constructing a `TurnHost` or connecting configured MCP servers; the first prompt activates it. Loading replays once while that in-process session remains open, while new and resumed sessions treat the client transcript as already owned. |
| Session configuration | Implements build/plan modes and transactional agent or model replacement through stable config options. Reconfiguration is rejected while a prompt is active and rolls back on failure. |
| Prompt execution | Admits input through the durable Zuno turn path, streams projections while the turn runs, and projects the final durable plan before returning. Concurrent prompts for one session are rejected. |
| Prompt content | Accepts text and native `resource_link` blocks. Every resource-link field is persisted. On replay, remote links remain typed, while a local file link remains actionable only when it still resolves to a regular file inside the active worktree; stale or external links become explanatory text. Providers without a native link type receive one stable text projection. Prompt image, audio, and embedded-resource capabilities remain `false`. |
| Assistant and tool projection | Streams assistant text and reasoning, tool start/update/completion, accumulated raw input, raw output, JSON/text content, written-file locations, and usage. |
| File edits and diffs | Zuno's native file tools produce stable ACP `diff` content with an absolute path and exact `oldText`/`newText`. A unified-diff text fallback remains only for tools that cannot provide typed file state. |
| Human input | Routes tool permission through `session/request_permission` and the question tool through stable form elicitation when the client advertises it. Unknown, malformed, declined, and cancelled outcomes fail closed. |
| Cancellation | `session/cancel`, request cancellation, stdin EOF, and process shutdown abort active work and settle pending agent-to-client requests instead of leaving the transport hung. A clean stdin EOF gives already accepted, ready-to-complete requests a bounded 25 ms drain before cancelling the remainder. Session close covers the prompt-to-run handoff without leaving an interrupt armed for a later mount. |
| Durable load replay | Reconstructs a bounded retained suffix of user/assistant content, reasoning, tools, raw input/output, safe typed diffs and locations, resource links and image output, followed by the current plan and latest-context usage. Omitted history is reported explicitly. |
| ACP-provided MCP | HTTP and SSE MCP capabilities remain `false`. Zuno may still mount MCP servers from its own validated native configuration. |
| Client filesystem RPC | Not advertised. Agent file reads and writes use Zuno tools, sandbox/permission policy, and durable events; they do not masquerade as ACP client filesystem handlers. |
| Terminal RPC | Not advertised. Zuno will not emit terminal references until create/output/wait/kill/release ownership and cancellation are implemented as one lifecycle. |

`resource_link` remains typed through ingress, SQLite, compaction, goal
continuation, steering, and load replay. Text-only provider protocols lower it
only at the provider boundary. This prevents Zed from reopening a thread with
lossy prose in place of the original URI metadata. The replay safety filter is
separate: it prevents historical local URIs from remaining actionable after a
workspace moves, a file disappears, or the restored thread belongs to another
worktree.

## Restore and teardown safety

Zed may retain an external-Agent process and its last selected thread after the
Agent panel is hidden. Hiding the panel is therefore not treated as
`session/close`. Zuno contains that client behavior at the ACP boundary:

- `session/load` and `session/resume` resolve validated session configuration
  but keep the session dormant. They do not create a `TurnHost` or connect
  configured MCP servers until the first `session/prompt`.
- A second `session/load` for the same open ACP session is idempotent. A
  `session/resume` also marks replay as already satisfied, so a later load
  cannot duplicate a transcript the client already owns.
- One stdio connection retains at most 32 open ACP sessions. A request that
  would exceed the bound fails explicitly instead of growing an unbounded host
  and MCP registry.
- Transcript hydration starts after the durable compaction boundary and keeps
  at most the newest 512 retained messages. Transcript projection is limited to
  4 MiB total and 1 MiB per update; an explicit `replay_omission` message
  reports how many durable messages were left out. SQLite sizes stored part
  blobs before Rust JSON hydration, so an oversized newest message is omitted
  without first allocating its tool output or attachment payload.
- Replayed diff paths, written-file locations, and local file links are
  canonicalized. Only existing regular files inside the active worktree remain
  actionable. Missing files, external paths, and symlink escapes are filtered;
  omitted local resource links become non-actionable text.
- `session/close` first makes the in-process session terminal, stops any active
  or just-admitted prompt, serializes against replay and activation, then shuts
  down the host and MCP runtime. A teardown-only pending interrupt is cleared
  so reopening the same durable session is not immediately cancelled.

These controls prevent a restored thread from eagerly recreating every runtime
or replaying an unbounded set of stale actionable paths. They do not mutate
Zed's workspace database, clear Zed's `last_active_thread`, or remove watchers
that Zed created independently. An activated idle session currently remains
mounted until Zed sends `session/close` or the stdio process ends; there is no
timer-based idle demotion.

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

1. Start a Zuno thread and confirm the mode, agent, and model selectors are
   populated from Zuno.
2. Ask for a read-only inspection and verify reasoning and tool details stream
   without corrupting stdout JSON-RPC.
3. Request a file creation under strict permission policy, answer the Zed
   permission card, and verify the native creation diff has `oldText: null`.
4. Ask a structured question and verify the elicitation form returns the answer
   to the same turn.
5. Cancel a running prompt and verify the thread returns to an idle state.
6. Close and reopen or import the thread, then verify content, tools, diff,
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

Those tests drive initialization, lifecycle, mode/model replacement, prompt
streaming, strict permission, form elicitation, typed creation diff,
resource-link roundtrip, cancellation transport, durable load replay, plan,
and usage. A release is not allowed to claim Zed UI acceptance until the manual
sequence above has also been run against the intended Zed build and platform.

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
