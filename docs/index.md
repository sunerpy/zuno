---
layout: home

hero:
  name: Zuno
  text: Zero code. Any task.
  tagline: >
    A single-binary coding agent CLI in Rust. No runtime dependency, sessions that
    survive a restart, and an OS sandbox that fails closed by default while keeping
    native execution behind an explicit trusted choice.
  actions:
    - theme: brand
      text: Quick start
      link: /guide/quick-start
    - theme: alt
      text: What is Zuno?
      link: /guide/what-is-zuno
    - theme: alt
      text: GitHub
      link: https://github.com/sunerpy/zuno

features:
  - title: One binary, nothing beside it
    details: >
      Static musl builds for Linux, native builds for macOS and Windows. No Node,
      no Python, no runtime to keep aligned with the agent's version.
    link: /guide/installation
    linkText: Install

  - title: A sandbox that fails closed by default
    details: >
      Read-only and workspace-write both require a proved OS confinement backend.
      When none is available Zuno refuses to start the session instead of silently
      running your code unconfined, unless trusted policy explicitly selects native
      execution or unavailable-only fallback.
    link: /guide/permissions
    linkText: Permissions and sandboxing

  - title: Durable by construction
    details: >
      Every prompt section, tool result and subagent report is persisted before the
      provider request. A restart reconstructs the work, including retry deadlines
      read back from SQLite.
    link: /guide/durable-state
    linkText: Goals, plans and todos

  - title: Native extensions, not a plugin ABI
    details: >
      WebAssembly components under explicit WASI grants, or contained child
      processes speaking line-delimited JSON-RPC. Capabilities are declared and
      checked, never inherited by accident.
    link: /plugins
    linkText: Plugins and extensions

  - title: Delegation with real boundaries
    details: >
      Hand a bounded objective to a specialist agent. Child reports are evidence the
      parent verifies, and a child can never obtain a tool its parent lacks.
    link: /orchestration
    linkText: Orchestration

  - title: Bring your own provider
    details: >
      Anthropic, OpenAI, Google, Bedrock, and any OpenAI-compatible endpoint.
      Credentials stay in a store you control; model routing is configuration.
    link: /reference/providers
    linkText: Providers and credentials
---

## Install

::: code-group

```sh [Linux / macOS]
curl -fsSL https://raw.githubusercontent.com/sunerpy/zuno/main/scripts/install.sh | sh
```

```powershell [Windows]
irm https://raw.githubusercontent.com/sunerpy/zuno/main/scripts/install.ps1 | iex
```

```sh [Cargo]
cargo install --git https://github.com/sunerpy/zuno zuno-cli --locked
```

:::

Then start the terminal application:

```sh
zuno
```

## What a turn actually is

Zuno is not a chat window that happens to run commands. A turn is a durable unit of
work: the assembled prompt is written to SQLite before the request leaves, every
tool result is logged as an event, and the session can be replayed or resumed after
the process dies.

```sh
# Make a change, with the workspace-write sandbox and the tests as the gate.
zuno run "add pagination to the /users endpoint and run the tests"

# Continue the most recent session rather than starting a new one.
zuno run --continue "now cap the page size at 100"

# Read-only investigation. No write tool is registered at all.
zuno run --agent plan "why does the retry budget start before the first attempt?"
```

## Where to go next

| If you want to | Read |
| --- | --- |
| Understand the design before installing | [What is Zuno?](/guide/what-is-zuno) |
| Get running in a few minutes | [Quick start](/guide/quick-start) |
| Connect a provider | [Providers and credentials](/reference/providers) |
| Look up a configuration key | [Configuration reference](/reference/configuration) |
| Look up a command | [CLI reference](/cli/) |
| Work inside an editor | [Editors and ACP](/reference/zed-acp) |
| Diagnose a failure | [FAQ](/faq) |
