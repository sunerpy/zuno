---
layout: home

hero:
  name: Zuno
  text: Durable work. Explicit boundaries.
  tagline: >
    A local Rust coding agent that keeps goals, tool results, retries, and
    delegation state across restarts.
  image:
    src: /zuno-logo.svg
    alt: Zuno logo
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
  - title: Work that resumes
    details: >
      Prompts, tool results, plans, retries, and child reports are stored as
      durable session state. A restarted process can continue from recorded work.
    link: /guide/durable-state
    linkText: Goals and work state

  - title: Roles with fixed authority
    details: >
      Build, planning, deep implementation, and specialist agents expose different
      tool ceilings. Configuration can narrow those ceilings, but cannot widen them.
    link: /guide/agents
    linkText: Agents

  - title: Controlled command execution
    details: >
      Permission rules, command-risk checks, and OS confinement are separate gates.
      Restricted modes fail closed unless trusted policy explicitly selects native execution.
    link: /guide/permissions
    linkText: Permissions and sandboxing

  - title: One native runtime
    details: >
      TUI, headless, ACP, and HTTP clients share the same Rust runtime, durable
      events, tools, and extension lifecycle.
    link: /harness-runtime
    linkText: Harness Runtime
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

Ripgrep 14 or newer is only the backend for the `glob` and `grep` tools; it is not
required to start Zuno or use the core runtime. Bubblewrap 0.8.0 or newer is only
the Linux backend for confined `read-only` and `workspace-write` Shell modes.
Explicit `danger-full-access` and an eligible trusted `workspace-write`
`run-unconfined` fallback execute natively. macOS and Windows currently have no
confined backend. The installer verifies the selected release archive against its
`SHA256SUMS`.

## Start a run

Configure a provider and verify its model catalog first:

```sh
zuno debug config
zuno models myopenai --verbose
```

On Linux with working confinement, use the read-only `plan` agent to verify the full
model and tool path before allowing writes:

```sh
zuno run --agent plan "summarize this repository's architecture"
```

On macOS or Windows, or on a trusted Linux host without confinement, follow the
[Quick start](/guide/quick-start) native-execution guidance. For delivery work:

```sh
zuno run "add pagination to the users endpoint and run the tests"
```

Bare `zuno` opens the terminal application. See [Quick start](/guide/quick-start) for
provider configuration, credentials, and sandbox checks.

## Find the right page

| Task | Page |
| --- | --- |
| Understand the execution model | [What is Zuno?](/guide/what-is-zuno) |
| Configure a provider | [Providers and credentials](/reference/providers) |
| Look up a setting | [Configuration reference](/reference/configuration) |
| Enable or switch History and Notes | [History and Notes continuity](/config/continuity) |
| Choose an agent | [Agents](/guide/agents) |
| Configure Shell authority | [Permissions and sandboxing](/guide/permissions) |
| Use Zuno from an editor | [Editors and ACP](/reference/zed-acp) |
| Look up a command | [CLI reference](/cli/) |
| Diagnose a failure | [FAQ](/faq) |
