# Zuno documentation

This directory is organized so it can be projected into a documentation site without moving facts between pages.

## Learn

- [Configuration](reference/configuration.md): files, precedence, JSON Schema, and TUI settings.
- [Portable environment bundles](reference/portable-bundles.md): cross-platform export/import, exclusions, credentials, validation, and rollback.
- [Images and file references](reference/attachments.md): TUI image paste, `@file`, headless attachments, durability, and model capability checks.
- [Providers and credentials](reference/providers.md): provider setup, credential storage, `myopenai`, and native request transports.
- [Provider authentication](design/provider-authentication.md): credential precedence, login-method registration, OpenAI API keys, and ChatGPT OAuth.
- [Web search and Antigravity roadmap](design/web-search-antigravity-roadmap.md): default anonymous Exa, authenticated Google grounding, integration auth, risks, and acceptance gates.
- [Codex and Claude Code product agents](design/product-agents.md): native product protocols, permissions, background jobs, cancellation, and credential ownership.
- [Harness runtime](harness-runtime.md): components, profiles, durable delivery, goals, and recovery.
- [Agent orchestration and model routing](orchestration.md): orchestrator delegation, child-agent models and reasoning, presets, workflows, and Council.
- [Plugins, custom agents, and workflows](plugins.md): package installation, WASI/process runtimes, capabilities, protocols, and examples.
- [Trusted process plugin development](process-plugin-development.md): package and JSON-RPC contracts, security, lifecycle, tests, and the OpenCode Antigravity bridge case study.
- [Client interfaces](design/client-interfaces.md): TUI, HTTP, ACP, and future GUI ownership.
- [Zed ACP integration](design/zed-acp-integration.md): stable protocol pins, official adapter references, implemented capabilities, Zed setup, and acceptance tests.
- [Memory learning](design/memory-learning.md): durable candidates, reflection, review, promotion, and undo.

## Operate

- [Self-update](reference/self-update.md)
- [Operational logging](logging.md)
- [Session retention](session-retention.md)
- [Resource gates](resource-gates.md)
- [Performance methodology](perf-methodology.md)

## Design

- [Harness comparison](design/harness-comparison.md)
- [Build-agent prompt and request comparison](design/agent-prompt-request-comparison.md)
- [Provider authentication](design/provider-authentication.md)
- [Product agents](design/product-agents.md)
- [Memory learning](design/memory-learning.md)
- [Migration and durable schema](migration.md)

Future site navigation should keep the same three user-facing groups: **Learn**, **Operate**, and **Design**. Generated API and JSON Schema artifacts belong under **Reference**, not in tutorials.
