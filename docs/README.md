# Zuno documentation

This directory is organized so it can be projected into a documentation site without moving facts between pages.

## Learn

- [Configuration](reference/configuration.md): files, precedence, JSON Schema, and TUI settings.
- [Providers and credentials](reference/providers.md): provider setup, credential storage, `myopenai`, and native request transports.
- [Provider authentication](design/provider-authentication.md): credential precedence, login-method registration, OpenAI API keys, and ChatGPT OAuth.
- [Codex and Claude Code product agents](design/product-agents.md): native product protocols, permissions, background jobs, cancellation, and credential ownership.
- [Harness runtime](harness-runtime.md): components, profiles, durable delivery, goals, and recovery.
- [Plugins, custom agents, and workflows](plugins.md): package installation, WASI/process runtimes, capabilities, protocols, and examples.
- [Client interfaces](design/client-interfaces.md): TUI, HTTP, ACP, and future GUI ownership.
- [Memory learning](design/memory-learning.md): durable candidates, reflection, review, promotion, and undo.

## Operate

- [Session retention](session-retention.md)
- [Resource gates](resource-gates.md)
- [Performance methodology](perf-methodology.md)

## Design

- [Harness comparison](design/harness-comparison.md)
- [Provider authentication](design/provider-authentication.md)
- [Product agents](design/product-agents.md)
- [Memory learning](design/memory-learning.md)
- [Migration and durable schema](migration.md)

Future site navigation should keep the same three user-facing groups: **Learn**, **Operate**, and **Design**. Generated API and JSON Schema artifacts belong under **Reference**, not in tutorials.
