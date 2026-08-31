# Skills

A Skill is reusable instructions with an identity: a name, a description used for trigger
matching, a Markdown body, and optionally bundled scripts, references, and assets. Skills
answer "how does this project want this kind of work done" without that guidance living in
every prompt.

A Skill grants nothing. It does not add tools, permissions, filesystem access, network
access, or environment access. The runtime capability snapshot remains authoritative, so a
Skill that describes a workflow does not thereby authorize it.

## Why not just put it in the prompt

Two reasons.

Loading everything is expensive: instructions that apply to one kind of work would consume
context on every turn. Progressive disclosure fixes that. The prompt receives a bounded
metadata catalog, and a body is loaded only when its name is explicit or its description
clearly matches the request.

Identity is the second reason. A Skill has a source, so the same name from two roots stays
independently addressable and no hidden precedence winner is chosen. That is what makes a
project Skill and a global Skill with the same name a visible ambiguity rather than a
silent surprise.

## Discovery order

Zuno discovers Skills in this scope order:

1. project `.zuno/skill` and `.zuno/skills` roots, from the current directory to the worktree;
2. project `.agents/skills`, then `.claude/skills`, over the same walk;
3. Zuno's global and configured config directories;
4. global `~/.agents/skills`, then `~/.claude/skills`;
5. explicit `skills.paths`;
6. configured remote indexes.

Project scope is therefore advertised before user-global scope. Zuno never scans
`.opencode` or an OpenCode config directory. The same canonical source path is
de-duplicated, including symlink aliases.

```sh
ZUNO_DISABLE_EXTERNAL_SKILLS=1 zuno
ZUNO_DISABLE_CLAUDE_CODE_SKILLS=1 zuno
```

The first disables both `.agents` and `.claude` roots; the second disables only the Claude
roots. Zuno-native `.zuno` roots stay enabled under the broad switch.

## How a Skill reaches the model

The prompt gets the catalog, not the bodies:

```json
{
  "skills": {
    "includeInstructions": true,
    "maxContextTokens": 8000,
    "maxSelectedContextTokens": 16000
  }
}
```

`maxContextTokens` bounds the compact catalog. Its default is roughly two percent of the
model context, or 8,000 approximate tokens when the context is unknown, capped at 10,000.

Fully selected bodies share a separate aggregate budget. Its default is ten percent of a
known context, with a 2,000-token floor and a 32,000-token ceiling.
`maxSelectedContextTokens` overrides that but stays capped at 32,000. If selected bodies do
not fit, loading or restoring the session fails before a provider request rather than
silently dropping instructions.

`includeInstructions: false` removes both the trigger policy and the catalog from prompts.
The `skill` tool still supports paged `list` and `search`.

## Loading is paged and must complete

`load` and `read_resource` return content-bound continuation cursors. A caller must read
through to `complete: true` before applying the instructions, because a partial `SKILL.md`
is not usable guidance. This is deliberate: half a procedure is often worse than none.

## Invoking one directly

An unambiguous Skill that does not collide with a real command is invokable as
`/<skill-name>`. Zuno resolves that exact advertised source and loads its body before the
next provider request.

Same-named Skills from multiple sources deliberately disable the ambiguous slash form. Use
the Skill picker, or the typed `skill` tool with an exact source.

Native session commands resolve before Markdown commands and Skills, so a user workflow
cannot shadow a runtime control such as `/compact` or `/plan`.

## Built-in Skills

Zuno compiles ten first-party Skills into the `zuno-orchestration` pack:
`customize-zuno`, `develop-zuno`, `deepwork`, `codemap`, `verification-planning`,
`reflect`, `worktree`, `git-workflow`, `github-delivery`, and `ui-design`.

Each has a stable `builtin://zuno-orchestration/...` source, a content hash, provenance,
allowed agent profiles, and a required-tool declaration. They are compiled into the
executable and are not copied into your configuration directory, so they update with the
binary. Copying one into a user Skill directory to "override" it creates a same-name source
ambiguity instead.

The active profile and its declared tool visibility filter the advertised set. Selecting a
Skill can never widen the runtime capability snapshot.

`github-delivery` is the generic remote-delivery method: exact commit/ref identity,
machine-readable Actions state, least-privilege workflow authoring, one durable remote
observer, required-job conclusions, artifact/checksum evidence, and consumer smoke tests.
It deliberately does not encode a repository's branch, approval, signing, or versioning
policy.

## Skills in delegated turns

Every initial or resumed child host performs discovery independently. Parent-loaded bodies
are not copied into a child prompt.

When a child role must always receive a particular instruction set, declare it:

```json
{
  "agents": {
    "explorer": {
      "requiredSkills": ["codegraph"]
    }
  }
}
```

Each name must resolve to exactly one visible source after profile and agent filtering. A
missing name or an ambiguous same-name source fails child startup rather than picking a
hidden winner.

Note the boundary carefully: this guarantees the child receives CodeGraph *instructions*,
not CodeGraph *tools*. Those still need the parent Attempt schema, role inheritance or an
exact grant, survival through the agent allowlist, and no explicit deny.

## Inspecting discovery

```sh
zuno debug skill
zuno debug agent explorer
```

`debug skill` reports raw discovery: same-name entries from different sources are
preserved, and the summary reports source, described, and unique counts plus ambiguous
names. `debug agent` reports the agent-filtered view with budgets and coverage.

## See also

- [Authoring Skills](/config/authoring-skills)
- [Workflows and commands](/config/workflows)
- [Agents](/guide/agents)
- [Configuration reference](/reference/configuration)
