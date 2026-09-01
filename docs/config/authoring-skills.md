# Authoring Skills

A Skill is reusable guidance with its own identity, loaded when it matches instead of
on every request. That distinction is the whole reason Skills exist: instruction files
cost prompt budget unconditionally, so anything that applies only sometimes belongs
here instead of in `AGENTS.md`.

[Skills](/guide/skills) covers using them. This page covers writing one and configuring
discovery.

## File layout

A Skill is a directory containing `SKILL.md`, plus whatever resources it references:

```text
my-skill/
  SKILL.md
  references/
    ci.md
    architecture.md
  scripts/
    check.sh
```

Resources are read on demand through the `skill` tool's `read_resource` action with a
path relative to the Skill directory. Referencing a file from `SKILL.md` does not load
it, which is what keeps a large Skill cheap until a task actually needs the detail.

## Frontmatter: exactly two keys

```markdown
---
name: dependency-audit
description: Audit a dependency for supply-chain risk. Use when adding, upgrading, or reviewing a third-party package.
---

# Dependency audit

Steps and criteria go here.
```

| Key | Type | Required | Description |
| --- | --- | --- | --- |
| `name` | `string` | yes | The catalog key the Skill is addressed by |
| `description` | `string` | no | Trigger and purpose advertised in the catalog |

Nothing else is read. `license`, `version`, `allowed-tools` and similar keys are
ignored rather than rejected, so their presence is harmless but has no effect.

Two failure modes are worth knowing because they are silent-looking:

- a `name` that is a number, boolean, or null drops the Skill entirely;
- a `description` that is present but not a string also drops it — including a bare
  `description:` with no value, which YAML resolves to null.

A Skill with no `description` at all is loaded but hidden from the model-facing
catalog. That is occasionally what you want for a Skill only invoked explicitly by
name, and never what you want otherwise.

`description` is the trigger surface. It should state both what the Skill does and when
to use it, because the model matches a request against this text and nothing else. A
description reading "dependency helper" will not fire; "Use when adding, upgrading, or
reviewing a third-party package" will.

## Discovery order

Zuno discovers Skills in this scope order:

| # | Root | Pattern |
| --- | --- | --- |
| 1 | Every project `.zuno` from the current directory up to the worktree | `{skill,skills}/**/SKILL.md` |
| 2 | Every project `.agents` | `skills/**/SKILL.md` |
| 3 | Zuno's global and configured config directories | `{skill,skills}/**/SKILL.md` |
| 4 | `$HOME/.agents` | `skills/**/SKILL.md` |
| 5 | Each `skills.paths` entry | `**/SKILL.md` |
| 6 | Each cache directory a `skills.urls` index produced | `**/SKILL.md` |

Project scope is advertised before user-global scope. Zuno never scans `.claude`,
`.opencode`, or another product's configuration directory implicitly. Add one through
`skills.paths` only when that sharing is intentional.

The same canonical source path is de-duplicated, including symlink aliases. Same-named
Skills from different sources remain independently addressable and no hidden winner is
selected. The compact prompt index omits source paths for unique names and reports a
`source` locator only for ambiguous names; the model must supply one in that case. An
ambiguous name also disables the direct `/<skill-name>` slash form.

| Variable | Effect |
| --- | --- |
| `ZUNO_DISABLE_EXTERNAL_SKILLS=1` | Disable implicit `.agents` roots |

Zuno-native `.zuno` roots stay enabled under the broad external switch.

## Configuration

| Key | Type | Default | Description |
| --- | --- | --- | --- |
| `includeInstructions` | `boolean` \| `null` | enabled | Whether turns receive the skill trigger policy and metadata catalog |
| `maxContextTokens` | positive integer \| `null` | 2% of model context; 8,000 approximate tokens when unknown | Maximum approximate tokens used by the catalog. Values above 10,000 are clamped by the runtime |
| `maxSelectedContextTokens` | positive integer \| `null` | 10% of known context, floor 2,000, ceiling 32,000; 8,000 when unknown | Maximum approximate tokens used by all fully selected Skill bodies in one session prompt. Values above the runtime ceiling are clamped |
| `paths` | `string[]` \| `null` | none | Additional paths to skill folders |
| `urls` | `string[]` \| `null` | none | URLs to fetch skills from |

```json
{
  "skills": {
    "includeInstructions": true,
    "maxContextTokens": 8000,
    "maxSelectedContextTokens": 16000,
    "paths": ["~/work/shared-skills"]
  }
}
```

The two budgets are separate on purpose. `maxContextTokens` bounds the compact
metadata catalog — names and descriptions. `maxSelectedContextTokens` bounds the
aggregate of fully loaded bodies in one session prompt. If selected bodies do not fit,
loading or restoring the session fails before a provider request rather than silently
dropping instructions, because a partially loaded Skill is worse than none.

`includeInstructions: false` removes both the trigger policy and the catalog from
model prompts. The `skill` tool still supports paged `list` and `search`, so explicit
invocation keeps working; only implicit matching stops.

## Progressive disclosure

The model never receives every `SKILL.md` body. It sees a bounded catalog and then
pulls what it needs:

| Action | Returns |
| --- | --- |
| `list` | Paged catalog of model-visible Skills with exact `source` locators |
| `search` | Metadata matches for a capability query |
| `load` | One Skill body, paged with a continuation cursor |
| `read_resource` | One referenced text resource, by relative path |

`load` and `read_resource` return content-bound continuation cursors. The caller must
read through `complete: true` before applying the instructions — a partial `SKILL.md`
is not usable guidance, and a Skill whose steps are split across a cursor boundary can
otherwise be acted on half-read.

Write for this consumption model. Put the decision-relevant content early in the body,
and push long tables and background into `references/` so the first page is
actionable.

## What a Skill cannot do

A Skill provides instructions. It does not grant tools, permissions, filesystem
access, network access, or environment access. The active runtime capability snapshot
remains authoritative, and selecting a Skill can never widen it.

This means a Skill telling the model to run a command does not authorize that command.
If the tool is not in the agent's allowlist or a permission rule denies it, the
instruction simply fails at call time. Design Skills to state what to do and let the
capability model decide whether it is allowed.

For a child agent that must always receive a particular Skill, use
`agents.<name>.requiredSkills` — see [Custom agents](/config/custom-agents). Child
turns run discovery independently, so loading a Skill in the parent does not inject its
body into a delegated child.

## Built-in Skills

Zuno compiles first-party Skills into the `zuno-orchestration` pack with stable
`builtin://zuno-orchestration/...` sources, content hashes, provenance, allowed agent
profiles, and required-tool declarations. They are compiled into the executable and are
not copied into the user configuration directory, so they update with the binary.

Do not copy one into a user Skill directory to "override" it. That creates a same-name
source ambiguity, which disables the direct slash form for that name.

## Inspecting discovery

```sh
zuno debug skill
zuno debug agent build
```

`zuno debug skill` reports raw discovery: `view.kind: "raw_discovery"`,
`agentFiltered: false`, `extensionOverlayApplied: false`, the `skills` array preserving
same-name entries from different sources, and a `summary` with source, described, and
unique counts plus ambiguous names. Restart before reading it, since it reflects the
process's discovery.

`zuno debug agent <name>` gives the agent-filtered view instead, including metadata and
selected-body budgets, rendered/omitted/truncated coverage, and a bounded preview.
That is the one to use when a Skill exists but a specific agent does not see it.

## See also

- [Skills](/guide/skills)
- [Workflows and commands](/config/workflows)
- [Custom agents](/config/custom-agents)
- [Diagnostics](/operate/diagnostics)
