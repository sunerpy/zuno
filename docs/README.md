# Zuno documentation

This directory is organized so it can be projected into a documentation site without moving facts between pages.

## Learn

- [Configuration](reference/configuration.md): files, precedence, JSON Schema, and TUI settings.
- [Providers and credentials](reference/providers.md): provider setup, credential storage, `myopenai`, and native request transports.
- [Harness runtime](harness-runtime.md): components, profiles, durable delivery, goals, and recovery.
- [Client interfaces](design/client-interfaces.md): TUI, HTTP, ACP, and future GUI ownership.

## Operate

- [Session retention](session-retention.md)
- [Resource gates](resource-gates.md)
- [Performance methodology](perf-methodology.md)

## Design

- [Harness comparison](design/harness-comparison.md)
- [Migration and durable schema](migration.md)

Future site navigation should keep the same three user-facing groups: **Learn**, **Operate**, and **Design**. Generated API and JSON Schema artifacts belong under **Reference**, not in tutorials.
