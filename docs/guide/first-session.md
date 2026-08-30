# Your first session

This page walks one real session and explains what the runtime does at each step. The
task is deliberately small: fix a failing test. What matters is the machinery behind it,
because that is what you will be reasoning about when something goes wrong.

## Start the terminal application

```sh
cd /srv/projects/api
zuno
```

Opening the welcome screen creates no durable session. An interactive new session is
prepared in process memory with a stable identity, and browsing models, agents, or themes
does not write a row. The `session` row and its first user message are inserted in one
transaction on the first model-bound submission.

That is why a session you opened and abandoned does not appear in
`zuno session list`.

## Submit the first prompt

```text
The test users_list_is_paginated fails. Find out why and fix it.
```

Pressing Enter while idle starts a turn. Before the provider request leaves, several
things happen in order:

1. The prompt is committed to the durable event log and inbox in one SQLite transaction.
2. Instruction files are discovered: the global `AGENTS.md`, then project files from the
   worktree root down to the current directory.
3. The Skill catalog is assembled as bounded metadata, not as every `SKILL.md` body.
4. The host generates developer instruction sections from the final provider-visible tool
   set, with stable section identifiers.
5. The complete post-hook prompt is persisted with a content digest.

Only then is the request sent. Everything the model can see is reconstructable
afterwards:

```sh
zuno debug prompt --step 1
```

Add `--show-sensitive` to include instruction, AGENTS, skill, and memory content
verbatim. Treat that output as sensitive before pasting it anywhere.

## Watch the tool calls

A typical first step is read-only investigation: `grep` for the test name, `read` the
file, `read` the handler. These are classified `ReadOnly`, so they do not trigger the
side-effect gates and may overlap when the model issues them together. Results are
persisted in the model's call order regardless of which finished first.

Then the agent wants to run the test. `shell` is side-effecting, so two independent
things happen:

- The **permission** gate decides whether the call is admitted at all. Under the default
  `standard` mode, configured rules and the normal risk gates apply.
- The **sandbox** decides what the admitted process can reach. Under the default
  `workspace-write`, the host root is read-only while the workspace is writable.

Allowing the call does not widen the sandbox, and a permissive sandbox does not skip the
permission gate. See [Permissions and sandboxing](/guide/permissions).

## Answer a permission prompt

When a rule says `ask`, the composer region is replaced by the prompt rather than a new
transcript card. Left and Right move between choices, Enter selects, and mouse selection
works. Cancelling resolves the tool as a typed denial; it never fabricates an approval.

A denial is a lifecycle outcome, not a crash. Durable tool state keeps
`outcome: "blocked"` with a block kind of `invalid_arguments`, `unavailable`, or
`denied`, so the transcript can state that the effect never ran instead of implying the
tool failed halfway.

## Steer without waiting

You do not have to wait for the turn to end.

| Key | Effect |
| --- | --- |
| `Enter` while a turn is active | Admit a FIFO queue item for the next turn |
| `Ctrl+Enter` | Steer: request a soft interrupt at the nearest safe step boundary |
| `Shift+Enter`, `Alt+Enter`, `Ctrl+J` | Insert a newline |
| `Escape` | Interrupt the turn; a second press confirms |

An item is reported as queued only after SQLite commits it. Pending items can be edited
or cancelled by revision and survive a process restart. Steering can wake a provider
stream or a retry delay, but an already-executing tool is not abandoned: its result
reaches the next tool-safe point first.

## Let it edit and verify

The agent proposes a patch through `apply_patch`, then re-runs the test. The default
model editing surface is `apply_patch` plus `write` for new files or an intentional full
replacement.

`apply_patch` verifies every section against current file bytes before the first
filesystem change, so a stale context fails the whole patch rather than applying half of
it. That is what makes "read the file again and retry with a smaller patch" the correct
recovery, rather than replaying the same diff.

## Read the outcome

The final rows report the resolved agent, the catalog model display name, and the
configured reasoning effort. Context occupancy is the most recent complete provider
prompt divided by the catalog context limit, replaced on each provider report rather than
accumulated over the session.

Cumulative token buckets stay in the usage projection and the sidebar.

## Resume it later

The session is durable, so continuing is not a new conversation:

```sh
zuno run --continue "now cap the page size at 100"
```

```sh
zuno tui --continue
zuno tui --session ses_1a2b3c
```

Resuming rebuilds the durable transcript, the plan and todo state, pending inbox inputs,
and any child sessions from delegation. A retry deadline that was pending is
reconstructed from SQLite rather than lost.

## What to inspect when it misbehaves

```sh
zuno debug permissions
zuno debug prompt --session ses_1a2b3c --step 3
zuno debug agent build
zuno session list --no-roots
```

Those four answer most questions: what the effective policy is, what the model actually
received, what the agent's resolved capability surface is, and whether delegation created
child sessions you have not looked at.

## See also

- [Sessions and turns](/guide/sessions)
- [The terminal application](/guide/tui)
- [Tools](/guide/tools)
- [Diagnosing a failure](/operate/diagnostics)
