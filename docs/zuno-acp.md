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
session create/load/resume/fork/list, prompt and same-process steering, model and
configuration updates, cancellation, close, and delete. Permission requests and
turn/session updates are translated to and from App Server events.

Zuno-specific capabilities continue to evolve behind explicit protocol
metadata. An ACP SDK package version alone does not opt a connection into an
unstable wire protocol.
