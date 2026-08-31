# Changelog

## [0.0.2](https://github.com/sunerpy/zuno/compare/v0.0.1...v0.0.2) (2026-08-31)


### Bug Fixes

* **agent:** preserve direct-answer interaction boundaries ([357f5ce](https://github.com/sunerpy/zuno/commit/357f5ceb66f30ce813505f7ccbb34cfefc834e0e))
* **agent:** preserve direct-answer interaction boundaries ([dccd8c4](https://github.com/sunerpy/zuno/commit/dccd8c49317c2035fc57512c27cb7cb3c4e249f7))
* **runtime:** harden autonomous work recovery ([181c335](https://github.com/sunerpy/zuno/commit/181c33574de6e014d71261c3239498dc835043f1))

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
