---
layout: home

hero:
  name: Zuno
  text: Zero code. Any task.
  tagline: >
    A single-binary coding agent CLI in Rust. Goals with budgets and real termination
    conditions, a roster of specialist agents instead of one all-purpose prompt, and
    orchestration the model cannot rewrite at runtime.
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
  - title: Goals that converge instead of drifting
    details: >
      A goal carries an objective the model cannot narrow, success criteria it cannot
      rewrite, and a token ceiling. Marking it done needs authoritative evidence;
      reporting blocked needs a concrete condition that survived three turns.
    link: /guide/durable-state
    linkText: Goals, plans and todos

  - title: A specialist roster, not one prompt
    details: >
      Ten selectable agents with different capability ceilings. A contract can only
      narrow authority, never widen it, so choosing the read-only agent is a guarantee
      rather than a default configuration can reverse.
    link: /guide/agents
    linkText: Agents

  - title: Orchestration owned by configuration
    details: >
      Council seats, quorum, concurrency, retry policy and deadlines are configuration.
      The model supplies the question and cannot relax its own constraints under
      pressure.
    link: /orchestration
    linkText: Orchestration

  - title: Delegation with real boundaries
    details: >
      A child never obtains a tool its parent lacks, delegates names exactly who it may
      call, and its report is evidence the parent verifies rather than a conclusion to
      adopt.
    link: /orchestration
    linkText: Delegation

  - title: One binary, one external dependency
    details: >
      Static musl on Linux, native builds elsewhere. No Node, no Python, no runtime to
      keep aligned. The single requirement is ripgrep 14+, because glob and grep drive
      real ripgrep rather than a reimplementation.
    link: /guide/installation
    linkText: Install

  - title: Native components, not a plugin ABI
    details: >
      DeepSeek Harness's "everything is a plugin" made concrete as a Rust Component:
      typed services, an exact disposer per started effect, and transactional profile
      replacement. No Rust dynamic libraries, because unloading one proves nothing.
    link: /plugins
    linkText: Plugins and extensions
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

## Pick the agent, not the prompt

Most of what you configure in Zuno is *who* does the work. An agent's contract fixes
its ceiling before the turn starts, so the choice is a property of the run rather than
a request the model may reinterpret.

```sh
# Read-only investigation. No write tool is registered at all, whatever the config says.
zuno run --agent plan "why does the retry budget start before the first attempt?"

# End-to-end delivery, with the tests as the gate.
zuno run "add pagination to the /users endpoint and run the tests"

# Hard cross-cutting work that should not fan out further.
zuno run --agent deep "make session recovery survive a mid-turn provider failure"

# Continue the most recent session rather than starting a new one.
zuno run --continue "now cap the page size at 100"
```

Underneath, a turn is a durable unit of work: the assembled prompt is written to SQLite
before the request leaves, every tool result is logged as an event, and the session can
be replayed or resumed after the process dies. That is the floor the guarantees above
stand on, not the selling point.

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
