# Agent backend plugin example

This directory is a documentation-only plugin skeleton. Zuno does not install,
enable, or discover it automatically.

`bin/external-agent-acp` is an executable, dependency-free ACP v1 echo provider
used to make the package shape and lifecycle concrete. It is a protocol
skeleton, not a production coding agent; replace it with a pinned provider
binary while keeping the package-relative path.

After explicitly installing the package through the normal plugin flow, a
user-owned workflow may reference:

- `example-agent-backends/native-build`
- `example-agent-backends/claude-review`
- `example-agent-backends/external-acp`

Pair every route with a user-owned `executionProfile`; do not add models,
providers, credentials, prompts, or permission bypasses to
`agent-backends.json`. See
[`docs/zuno-plugin-agent-backends.md`](../../../docs/zuno-plugin-agent-backends.md).
