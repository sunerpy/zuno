# Changelog

## [0.1.0](https://github.com/sunerpy/zuno/compare/v0.0.3...v0.1.0) (2026-09-01)


### Features

* **acp:** mount session-scoped MCP servers ([4bf54bc](https://github.com/sunerpy/zuno/commit/4bf54bc9b55888fed9ea1adad9d7e408d4c377e4))
* add durable user learning flywheel ([1f47591](https://github.com/sunerpy/zuno/commit/1f475914891117883e5be4d77172fd7c4b837bea))
* add overridable Zuno git attribution ([4e6b787](https://github.com/sunerpy/zuno/commit/4e6b78769fd0195fe4f5051700ac349f44e270f8))
* **attachments:** add durable normalized image objects ([c0c98f3](https://github.com/sunerpy/zuno/commit/c0c98f333f6b1c08a59c9e907bfbfd7e82a7ca32))
* **cli:** opt in to headless reasoning output ([ab6a9bf](https://github.com/sunerpy/zuno/commit/ab6a9bf2b2074ffa78d596db29dca0ec352a90ba))
* **continuity:** add native history and notes tools ([61960ed](https://github.com/sunerpy/zuno/commit/61960edd007c7c46d5166fd7f1aa9bb89d525c23))
* **server:** add loopback browser session auth ([160456a](https://github.com/sunerpy/zuno/commit/160456a9f3c3fcf2f46cf7d7e5eeab9e534a724a))
* **task:** gate child model selection by durable allowlist ([2a2b489](https://github.com/sunerpy/zuno/commit/2a2b489ab8a383c048b659bac91330304e4f6495))


### Bug Fixes

* **integration:** reconcile merged release and platform gates ([b7e1008](https://github.com/sunerpy/zuno/commit/b7e10080fb4e69cb18547ca3bfb79e8862be3f0d))
* **network:** harden public web fetch targets ([1d99b7e](https://github.com/sunerpy/zuno/commit/1d99b7e8a398ae7da2bafc8481ca4608fc7dc5e9))
* **web-search:** redact credential-bearing endpoints ([cbcf98d](https://github.com/sunerpy/zuno/commit/cbcf98decad3c876049da7f4fcaabd30e31a58a3))
* **windows:** make native validation deterministic ([dc42d2d](https://github.com/sunerpy/zuno/commit/dc42d2d51d9829d9f11c80c42871c19a8e29b4fe))


### Performance Improvements

* **release:** promote candidates and parallelize Windows CI ([0fb2bde](https://github.com/sunerpy/zuno/commit/0fb2bde1e19b11d504d01a4115e0590c6bf12454))

## [0.0.3](https://github.com/sunerpy/zuno/compare/v0.0.2...v0.0.3) (2026-08-31)


### Bug Fixes

* **release:** harden autonomous delivery and optimize CI ([a04fb26](https://github.com/sunerpy/zuno/commit/a04fb26ee71a843bf2884b3d381a957e8a066ed1))

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
