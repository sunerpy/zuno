# Changelog

## [0.1.0](https://github.com/sunerpy/zuno/compare/v0.0.1...v0.1.0) (2026-08-30)


### Features

* initial release ([2129c5c](https://github.com/sunerpy/zuno/commit/2129c5c29e8384a64ff3aa5e2139bdd3c2100735))

## 0.0.1

First published release. Zuno is in pre-release development: configuration, data
formats, tool arguments, and the extension protocol may change between versions.

- Local coding agent shipped as a single native Rust executable, with a built-in
  terminal interface, headless runs, an ACP server, and an HTTP server over one
  runtime.
- Durable sessions in SQLite. Prompts, tool results, retry notices, goals, plans,
  todos, jobs, and child-agent reports survive a restart.
- Agent roles with fixed tool ceilings: `orchestrator` decomposes and verifies,
  `build` delivers, `plan` is read-only, and `deep` handles cross-cutting work
  without recursive delegation.
- Shell authority split into permission policy, command-risk checks, and OS
  confinement. Restricted modes fail closed unless trusted policy selects native
  execution; Linux confinement uses bubblewrap, capability dropping, and seccomp.
- Native provider transports for OpenAI, Anthropic, Google, Bedrock, and
  OpenAI-compatible endpoints.
- Extensions contribute agents, workflows, Skills, WASI components, and contained
  process tools through the same profile lifecycle.
