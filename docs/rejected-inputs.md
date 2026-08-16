# Rejected inputs

Ten configuration forms upstream still accepts, or once accepted, are rejected
here with a message naming the modern replacement and the exact file. Rejecting
is the deliberate choice: silently accepting a deprecated form leaves a
configuration that behaves differently from what it says.

Each rejection is reported as a `ConfigIssue` inside a `ConfigError::Invalid`
whose own path is the *scanned root*, which for a directory scan is not the
offending file. The message therefore names its own path.

## How this page cannot drift

Every message below is **rendered by the detector**, not transcribed:
`crates/zuno-cli/tests/docs.rs` calls `zuno_config::legacy`'s `inspect_*` functions
and compares `Deprecation::message()` against this page byte for byte. Rewording a
replacement in `crates/zuno-config/src/legacy.rs` fails the documentation gate.
`<file>` stands in for the absolute path of the offending file, which is the only
part of a message that is not a constant. Regenerate with:

```sh
ZUNO_DOCS_REGENERATE=1 cargo test -p zuno-cli --test docs
```

<!-- generated:BEGIN rejected-inputs -->
### AgentMaxSteps

- Rejected: `agent.build.maxSteps` — use `steps`

  ```text
  deprecated key `agent.build.maxSteps` at /example/opencode.json; use `steps`
  ```

- Rejected: `maxSteps` — use `steps`

  ```text
  deprecated key `maxSteps` at /example/agent/build.md; use `steps`
  ```


### AgentTools

- Rejected: `agent.build.tools` — use `permission` — `write`, `edit`, and `patch` all collapse to `permission.edit`

  ```text
  deprecated key `agent.build.tools` at /example/opencode.json; use `permission` — `write`, `edit`, and `patch` all collapse to `permission.edit`
  ```

- Rejected: `tools` — use `permission` — `write`, `edit`, and `patch` all collapse to `permission.edit`

  ```text
  deprecated key `tools` at /example/agent/build.md; use `permission` — `write`, `edit`, and `patch` all collapse to `permission.edit`
  ```


### AuthPromptCondition

- Rejected: `methods.0.condition` — use `when` — a `{ key, op, value }` rule, not a predicate

  ```text
  deprecated key `methods.0.condition` at /example/auth.json; use `when` — a `{ key, op, value }` rule, not a predicate
  ```

- Rejected: `prompts.0.condition` — use `when`

  ```text
  deprecated key `prompts.0.condition` at /example/auth.json; use `when`
  ```


### Autoshare

- Rejected: `autoshare` — use `share` — `autoshare: true` is `share: "auto"`

  ```text
  deprecated key `autoshare` at /example/opencode.json; use `share` — `autoshare: true` is `share: "auto"`
  ```


### ContextFile

- Rejected: `CONTEXT.md` — rename it to `AGENTS.md`

  ```text
  deprecated instruction file `CONTEXT.md` at <file>/CONTEXT.md; rename it to `AGENTS.md`
  ```


### Layout

- Rejected: `layout` — removed — delete it; the layout is always stretched

  ```text
  deprecated key `layout` at /example/opencode.json; removed — delete it; the layout is always stretched
  ```


### ModeBlock

- Rejected: `mode.build` — use `agent.build` with `mode: "primary"`

  ```text
  deprecated key `mode.build` at /example/opencode.json; use `agent.build` with `mode: "primary"`
  ```


### ModeDirectory

- Rejected: `mode/plan.md` — move it to `agent/plan.md`

  ```text
  deprecated agent definition `mode/plan.md` at <file>/mode/plan.md; move it to `agent/plan.md`
  ```


### Reference

- Rejected: `reference` — use `references`

  ```text
  deprecated key `reference` at /example/opencode.json; use `references`
  ```


### TomlConfig

- Rejected: `config` — migrate it to `config.json`

  ```text
  deprecated TOML config file `config` at <file>/config; migrate it to `config.json`
  ```
<!-- generated:END rejected-inputs -->

## Two spellings for one form

`AuthPromptCondition` appears twice with different replacements, and that is the
code's behaviour rather than a documentation error. The descriptor detector
(`auth_prompt_deprecation`) names the shape of the modern rule because a plugin
author needs it; the JSON convenience detector (`inspect_auth`) names only the key
because the surrounding document already shows the shape.

## What is not rejected

The legacy global TOML `config` file is reported and **never rewritten or
removed** — `crates/zuno-config/src/legacy/tests.rs::the_toml_config_file_is_never_rewritten_or_removed`
asserts that. Rejecting an input is a refusal to interpret it, not a licence to
modify the user's files.
