# Develop Zuno

Use this Skill when the user asks to create or change a Zuno Agent, Skill,
configuration field, provider integration, MCP integration, extension plugin,
or other documented extension point.

## Choose the native extension point

1. Use configuration for deployment choices, models, permissions, providers,
   MCP servers, and tunables already represented by the generated schema.
2. Use Markdown under `agent/` or `agents/` for a user-defined Agent whose
   behavior can be expressed through an existing profile and tool surface.
3. Use a directory containing `SKILL.md` for reusable guidance, scripts,
   references, or assets. A Skill guides an existing Agent; it does not register
   tools, grant permissions, or own a process lifecycle.
4. Keep user-specific workflows such as release policy or review policy in
   user-owned Skills or Markdown commands. Do not add those workflows to the
   first-party pack.
5. Use an `extension.json` package when a separately versioned plugin must
   contribute Agents, slash workflows, Skills, static tools, or process-backed
   tools. Provider transports, login methods, credential stores, hooks, and
   host lifecycle services remain native Rust extension points.
6. Use a native `Component`, typed service, or `AgentDriver` only when the
   capability belongs to Zuno's product runtime and its lifecycle cannot be
   expressed through an extension.

The resources in `crates/zuno-orchestration` are embedded Skills. The catalog
may expose an unshadowed Skill as `/develop-zuno`. This is not a CLI command.
Register a real command only when a concrete host-side handler,
help text, dispatch path, permissions, and failure behavior all exist.

## Authoring workflow

1. Read the live schema and the relevant architecture document before editing.
   Zuno is unreleased: change the native shape and update every internal caller
   instead of adding an OpenCode compatibility or migration layer.
2. Trace the interface, provider, consumer, registration, disposer, and profile
   replacement paths. A capability is incomplete if any lifecycle edge is
   missing.
3. Start with a failing test at the public boundary. Cover malformed config,
   missing runtime artifacts, permissions, cancellation, rollback, durable
   events, and client presentation as applicable.
4. Keep model-visible prompts, external inputs, and tool results reconstructable
   from durable session state. Do not add client-private agent-loop behavior.
5. Update the relevant reference and design documentation in the same change,
   then run the smallest crate tests followed by the repository gates.

## User-owned file layout

- Global configuration and resources live below the resolved Zuno config root.
- Project configuration and resources live below `.zuno/`.
- Agent definitions use `agent/**/*.md` or `agents/**/*.md`.
- Skills use `skill/<name>/SKILL.md` or `skills/<name>/SKILL.md`; keep supporting
  `scripts/`, `references/`, and `assets/` inside the same Skill directory.
- Shared Skill display and implicit-invocation metadata belongs in
  `agents/openai.yaml`; Zuno-specific field overrides and `policy.exposure`
  belong in `agents/zuno.yaml`. Keep `SKILL.md` frontmatter limited to its
  native `name` and `description`.
- User Markdown commands use `command/**/*.md` or `commands/**/*.md`.
- Static and process extension packages live below `extensions/<id>/` and must
  contain a validated `extension.json`.

Never infer compatibility from another product's config, plugin, hook, OAuth,
database, or tool protocol. Adapt the design to Zuno's typed interfaces and
permission system.

## Repository references

- Configuration: `docs/reference/configuration.md` and
  <https://github.com/sunerpy/zuno/blob/main/docs/reference/configuration.md>
- Plugin model: `docs/plugins.md` and
  <https://github.com/sunerpy/zuno/blob/main/docs/plugins.md>
- Process plugin development: `docs/process-plugin-development.md` and
  <https://github.com/sunerpy/zuno/blob/main/docs/process-plugin-development.md>
- Agent and Skill orchestration: `docs/orchestration.md` and
  <https://github.com/sunerpy/zuno/blob/main/docs/orchestration.md>
- Runtime lifecycle: `docs/harness-runtime.md` and
  <https://github.com/sunerpy/zuno/blob/main/docs/harness-runtime.md>
