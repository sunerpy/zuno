# Customizing Zuno

Zuno validates its own config strictly and refuses to start when a field
is wrong. The forms below cover the supported configuration surface.

## Schema ownership

Zuno does not currently publish a JSON Schema. Omit `$schema` unless the user
already owns a Zuno-specific schema reference; schemas for other products include
fields Zuno rejects.

If a field is not documented here, inspect the installed Zuno version's
configuration documentation or its `zuno-config` schema before editing. Unknown
top-level keys fail startup.

## Applying changes

Config is loaded once when Zuno starts and is not hot-reloaded. After
saving changes to `zuno.json`, an agent file, a skill, or any other
config-time file, **tell the user to quit and restart Zuno** for
the changes to take effect. The running session will keep using the
already-loaded config until then.

## Where files live

| Scope                         | Path                                                                                                                      |
| ----------------------------- | ------------------------------------------------------------------------------------------------------------------------- |
| Project config                | `./zuno.json`, `./zuno.jsonc`, or `.zuno/zuno.json` (Zuno walks up from the cwd to the worktree root)                      |
| Global config                 | `~/.config/zuno/zuno.json` (NOT `~/.zuno/`)                                                                               |
| Project agents                | `.zuno/agent/<name>.md` or `.zuno/agents/<name>.md`                                                                       |
| Global agents                 | `~/.config/zuno/agent(s)/<name>.md`                                                                                       |
| Project commands              | `.zuno/command/<name>.md` or `.zuno/commands/<name>.md`                                                                   |
| Global commands               | `~/.config/zuno/command(s)/<name>.md`                                                                                     |
| Project skills                | `.zuno/skill(s)/<name>/SKILL.md`                                                                                          |
| Global skills                 | `~/.config/zuno/skill(s)/<name>/SKILL.md`                                                                                 |
| External skills (auto-loaded) | `~/.claude/skills/<name>/SKILL.md`, `~/.agents/skills/<name>/SKILL.md`                                                    |

Configs from each scope are deep-merged. Project overrides global. Unknown
top-level keys in `zuno.json` are rejected with `ConfigInvalidError`.

## zuno.json

Every field is optional.

```json
{
  "username": "string",
  "model": "provider/model-id",
  "small_model": "provider/model-id",
  "default_agent": "agent-name",
  "shell": "/bin/zsh",
  "logLevel": "DEBUG" | "INFO" | "WARN" | "ERROR",
  "share": "manual" | "auto" | "disabled",
  "autoupdate": true | false | "notify",
  "snapshot": true,
  "instructions": ["AGENTS.md", "docs/style.md"],

  "skills": {
    "paths": [".zuno/skills", "/abs/path/to/skills"],
    "urls": ["https://example.com/.well-known/skills/"]
  },

  "references": {
    "docs": {
      "path": "../docs",
      "description": "Use for product behavior and documentation conventions"
    },
    "sdk": {
      "repository": "owner/sdk",
      "branch": "main",
      "description": "Use for SDK implementation details",
      "hidden": true
    }
  },

  "agent": {
    "my-agent": {
      "model": "anthropic/claude-sonnet-4-6",
      "mode": "subagent",
      "description": "...",
      "permission": { "edit": "deny" }
    }
  },

  "command": {
    "deploy": { "description": "...", "template": "..." }
  },

  "provider": {
    "anthropic": { "options": { "apiKey": "..." } }
  },
  "disabled_providers": ["openai"],
  "enabled_providers": ["anthropic"],

  "mcp": {
    "playwright": {
      "type": "local",
      "command": ["npx", "-y", "@playwright/mcp"],
      "enabled": true,
      "environment": {}
    },
    "remote-thing": {
      "type": "remote",
      "url": "https://...",
      "headers": { "Authorization": "Bearer ..." }
    }
  },

  "permission": {
    "edit": "deny",
    "bash": { "git *": "allow", "*": "ask" }
  },

  "formatter": false,
  "lsp": false,

  "experimental": {
    "primary_tools": ["edit"],
    "mcp_timeout": 30000
  },

  "tool_output": { "max_lines": 200, "max_bytes": 8192 },

  "compaction": { "auto": true, "tail_turns": 15 },

  "web_search": {
    "provider": "exa",
    "max_queries": 4,
    "max_results": 12,
    "timeout_ms": 30000
  }
}
```

Shape notes worth being explicit about:

- `model` always carries a provider prefix: `"anthropic/claude-sonnet-4-6"`.
- `skills` is an object with `paths` and/or `urls`, not an array.
- `references` is an object keyed by alias. Each value is a local path, Git repository, or string shorthand.
- `agent` is an object keyed by agent name, not an array.
- `command` is an object keyed by command name, not an array.
- `mcp[name].command` is an array of strings, never a single string. `type` is required.
- `permission` is either a string action or an object keyed by tool name.
- `web_search.provider` is `"exa"` or `"parallel"`; limits are positive integers.

## Skills

Zuno's skill loader scans for `**/SKILL.md` inside skill directories. The
file is named `SKILL.md` exactly, and lives in its own folder named after the
skill:

```
.zuno/skills/my-skill/SKILL.md
```

Frontmatter:

```markdown
---
name: my-skill
description: One sentence covering what this skill does AND when to trigger it. Front-load the literal keywords or filenames the user is likely to say.
---

# My Skill

(skill body in markdown: instructions, examples, references)
```

- `name` is required, lowercase hyphen-separated, up to 64 chars, and matches the folder name.
- `description` is effectively required: skills without one are filtered out and never surfaced to the model. Cover both _what_ the skill does and _when_ to use it. Write in third person ("Use when...", not "I help with..."). Front-load concrete trigger keywords and filenames; gate with "Use ONLY when..." if the skill should stay quiet on adjacent topics.
- Optional: `license`, `compatibility`, `metadata` (string-string map).

Register skills from non-default locations via `skills.paths` (scanned
recursively for `**/SKILL.md`) and `skills.urls` (each URL serves a list of
skills).

## References

References make local directories and Git repositories outside the active
project available as supporting context. Configure them under `references`,
keyed by the alias used in `@` autocomplete:

```json
{
  "references": {
    "docs": {
      "path": "../product-docs",
      "description": "Use for product behavior and terminology"
    },
    "effect": {
      "repository": "Effect-TS/effect",
      "branch": "main",
      "description": "Use for Effect implementation details"
    }
  }
}
```

Local `path` values may be relative to the declaring config, absolute, or use
`~/`. Git `repository` values accept Git URLs, host/path references, and GitHub
`owner/repo` shorthand; `branch` is optional. Both forms support optional
`description` and `hidden` fields.

- Only references with a `description` are advertised to agents in system context.
- `hidden: true` removes a reference from TUI `@` autocomplete only. It remains available to agents and by direct path.
- Reference directories are automatically allowed through the external-directory boundary; normal read/edit/tool permissions still apply.
- String shorthand is supported: use `"docs": "../docs"` for local paths or `"effect": "Effect-TS/effect"` for Git repositories.

## Agents

Two ways to define an agent. Use the file form for anything non-trivial.

### Inline (in `zuno.json`)

```json
{
  "agent": {
    "my-reviewer": {
      "description": "Reviews PRs for style violations.",
      "mode": "subagent",
      "model": "anthropic/claude-sonnet-4-6",
      "permission": { "edit": "deny", "bash": "ask" },
      "prompt": "You are a strict PR reviewer..."
    }
  }
}
```

### File

```
.zuno/agent/my-reviewer.md      OR     .zuno/agents/my-reviewer.md
```

```markdown
---
description: Reviews PRs for style violations.
mode: subagent
model: anthropic/claude-sonnet-4-6
permission:
  edit: deny
  bash: ask
---

You are a strict PR reviewer. Focus on...
```

The file body becomes the agent's `prompt`. Do not also put `prompt:` in the
frontmatter.

`mode` is one of `"primary"`, `"subagent"`, `"all"`.

Allowed top-level frontmatter fields: `name, model, variant, description, mode,
hidden, color, steps, options, permission, disable, temperature, top_p`. Any
unknown field is silently routed into `options`.

To disable a built-in agent: `agent: { build: { disable: true } }`, or in a
file, `disable: true` in frontmatter.

`default_agent` must point to a non-hidden, primary-mode agent.

### Built-in agents

Zuno ships with primary agents `build` and `plan`, plus `deep`, `explorer`,
`librarian`, `advisor`, `worker`, and `looker` specialist entries. Hidden
internal agents are `compaction`, `title`, and `summary`. To override a native
agent's fields, define the same key in `agent: { <name>: { ... } }`.

## Commands

Zuno's command loader scans for `**/*.md` inside command directories. The
file is named after the command, and lives directly inside the `command` folder:

```
.zuno/command/deploy.md
```

Frontmatter:

```markdown
---
description: One sentence describing what the command does.
agent: build
model: anthropic/claude-sonnet-4-6
---

(command body in markdown: the prompt Zuno runs, with $ARGUMENTS for the user's input)
```

- `template` is the command body — everything below the frontmatter — and is required: it is the prompt Zuno runs when the command is invoked. Do not also put a `template:` key in the frontmatter.
- `$ARGUMENTS` is replaced with everything the user typed after the command; `$1`, `$2`, … pull individual positional arguments.
- Optional: `description`, `agent`, `model`, `variant`, `subtask`.

## MCP servers

`mcp:` is an object keyed by server name. Each server is discriminated by
`type`:

```json
{
  "mcp": {
    "playwright": {
      "type": "local",
      "command": ["npx", "-y", "@playwright/mcp"],
      "enabled": true,
      "environment": { "BROWSER": "chromium" }
    },
    "github": {
      "type": "remote",
      "url": "https://...",
      "enabled": true,
      "headers": { "Authorization": "Bearer {env:GITHUB_TOKEN}" }
    },
    "old-server": { "enabled": false }
  }
}
```

`command` is an array of strings. `environment` sets environment variables for
a local MCP server. `type` is required. Use `enabled: false` to
disable a server inherited from a parent config. String values such as header
tokens support `{env:VAR}` interpolation (and `{file:path}`); the shell-style
`${VAR}` is not substituted.

## Permissions

```json
"permission": {
  "edit": "deny",
  "bash": { "git *": "allow", "rm *": "deny", "*": "ask" },
  "external_directory": { "~/secrets/**": "deny", "*": "allow" }
}
```

Actions: `"allow"`, `"ask"`, `"deny"`.

Per-tool value forms: `"allow"` shorthand (treated as `{"*": "allow"}`), or an
object `{ pattern: action }`. Within an object, **insertion order matters**.
Zuno evaluates the LAST matching rule, so put broad rules first and narrow
rules last.

`permission: "allow"` (a string at the top level) is shorthand for "allow
everything" and is rarely what the user wants.

Known permission keys: `read, edit, glob, grep, list, bash, task,
external_directory, todowrite, question, webfetch, web_search, lsp, doom_loop,
skill`. Some of these (`todowrite,
question, webfetch, web_search, doom_loop`) only accept a flat
action, not a per-pattern object.

`external_directory` patterns are filesystem paths (use `~/`, absolute paths,
or globs like `~/projects/**`).

Per-agent `permission:` overrides top-level `permission:`. Plan Mode lives on
the `plan` agent's permission ruleset (`edit: deny *`).

## Escape hatches

When a user's config is broken and Zuno won't start, these env vars help:

- `ZUNO_DISABLE_PROJECT_CONFIG=1`: skip the project's local `zuno.json`
  and start from globals only. Run from the project directory, Zuno loads,
  the user edits the broken file, then they restart without the flag.
- `ZUNO_CONFIG=/path/to/file.json`: load an additional explicit config.
- `ZUNO_CONFIG_CONTENT='{}'`:
  inject inline JSON as a final local-scope merge.
- `ZUNO_DISABLE_EXTERNAL_SKILLS=1`,
  `ZUNO_DISABLE_CLAUDE_CODE_SKILLS=1`: skip the external skill scans under
  `~/.claude/` and `~/.agents/`.

## When proposing edits

- Validate the exact Zuno field type before writing. If a field is not covered
  here, inspect the installed version's configuration source rather than guessing.
- Preserve any user-authored `$schema` and fields the user did not ask to change,
  but do not introduce a schema owned by another product.
- For agent, command, and skill definitions, prefer creating new files
  in the correct location over inlining everything in `zuno.json`.
- If the user's existing config is malformed, point them at the env-var escape
  hatches above so they can edit from inside Zuno without breaking their
  session.
- After saving any config change, remind the user to quit and restart Zuno
  — running sessions keep using the already-loaded config.
