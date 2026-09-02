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
  agents/
    openai.yaml
    zuno.yaml
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

A Skill with no `description` at all is loaded but hidden from model-driven discovery
unless a recognized sidecar supplies `interface.short_description`. That is occasionally
what you want for a Skill only invoked explicitly by name, and never what you want
otherwise.

Without a sidecar, `description` is the primary trigger surface. It should state both what
the Skill does and when to use it. Search also considers the invocation name and optional
sidecar display metadata; the initial index uses `interface.short_description` when present
and otherwise falls back to the frontmatter description. A description reading "dependency
helper" is weak; "Use when adding, upgrading, or reviewing a third-party package" is
actionable.

## Optional sidecar metadata

`agents/openai.yaml` is the shared Agent Skills metadata surface. Zuno consumes only the
fields with active runtime behavior and ignores unknown fields:

```yaml
interface:
  display_name: Dependency Audit
  short_description: Audit third-party packages before adding or upgrading them
policy:
  allow_implicit_invocation: false
```

- `interface.display_name` is a human-facing search and diagnostic title.
- `interface.short_description` replaces the longer frontmatter description in bounded
  catalog output while both remain searchable.
- `policy.allow_implicit_invocation: false` makes the Skill `explicit`.

`agents/zuno.yaml` overlays the shared file field-by-field and may additionally set native
catalog exposure:

```yaml
policy:
  exposure: search
```

The supported values are `index`, `search`, and `explicit`. A native exposure overrides
`allow_implicit_invocation`; a matching user `skills.config` entry overrides both
sidecars. A malformed recognized field produces a discovery warning but does not drop an
otherwise valid `SKILL.md`.

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

Project `.zuno/skill` roots and Zuno's canonical user Skill root stay enabled
under the broad external switch.

## Live catalog generations

Each running session owns one immutable
`SkillCatalogSnapshot { generation, digest, skills, warnings }`. `zuno-watch`
observes project scope, the canonical user root, existing shared Agent Skills,
and explicit configured paths. It does not observe `~/.zuno` or the private
remote download cache. When a canonical or explicit root does not exist yet,
the watcher observes the nearest safe existing parent **non-recursively**. The
subscription moves toward the logical root as missing directories are created
and becomes recursive only at the exact root. Relevant events are debounced,
watcher overflow forces a complete rescan, and the next generation is
published atomically.

Prompt metadata, `requiredSkills`, slash commands, the `skill` tool, TUI, and
ACP all read that same snapshot. Adding, editing, deleting, or renaming a Skill
therefore becomes visible to an existing session without restarting Zuno. A
malformed or temporarily unreadable `SKILL.md` keeps the previous valid source
in the catalog and publishes a warning instead of replacing the whole snapshot
with partial state.

`load` and `read_resource` force one refresh when given a locator that is absent
from the current generation. If the source reappeared, it loads normally. If it
was deleted or renamed, the tool returns typed `CatalogStale` with the currently
available exact locators. Zuno does not scan an arbitrary caller-supplied path
or fuzzy-load a same-named Skill.

Changing discovery configuration itself, such as adding a new `skills.paths`
root, still requires session reconfiguration or restart because that changes
which directories are watched.

## Configuration

| Key | Type | Default | Description |
| --- | --- | --- | --- |
| `includeInstructions` | `boolean` \| `null` | enabled | Whether turns receive the skill trigger policy and metadata catalog |
| `maxContextTokens` | positive integer \| `null` | 2% of model context; about 8,000 characters when unknown | Explicit approximate-token limit for the catalog. Values above 10,000 are clamped by the runtime |
| `maxSelectedContextTokens` | positive integer \| `null` | 10% of known context, floor 2,000, ceiling 32,000; 8,000 when unknown | Maximum approximate tokens used by all fully selected Skill bodies in one session prompt. Values above the runtime ceiling are clamped |
| `paths` | `string[]` \| `null` | none | Additional paths to skill folders |
| `urls` | `string[]` \| `null` | none | URLs to fetch skills from |
| `config` | object[] \| `null` | none | Ordered per-path enablement and exposure overrides |

```json
{
  "skills": {
    "includeInstructions": true,
    "maxContextTokens": 8000,
    "maxSelectedContextTokens": 16000,
    "paths": ["~/work/shared-skills"],
    "config": [
      {
        "path": "~/.agents/skills/private-release",
        "enabled": false
      },
      {
        "path": "~/.config/zuno/skill/powerapps",
        "recursive": true,
        "exposure": "search"
      }
    ]
  }
}
```

The default metadata character budget is two percent of a known model context. When the
context is unknown, Zuno uses approximately 8,000 characters. `maxContextTokens` remains
an explicit approximate-token override; Zuno converts it to characters and caps it at
10,000 tokens.

Each `config` object accepts:

| Key | Type | Meaning |
| --- | --- | --- |
| `path` | string | Skill directory, exact `SKILL.md`, or subtree root |
| `enabled` | boolean | Load or exclude matching Skills; omission means enabled |
| `exposure` | `index` \| `search` \| `explicit` | Override model-discovery exposure |
| `recursive` | boolean | Apply the entry to every Skill below `path` |

Entries are evaluated in order and the last matching entry wins. Existing paths are
canonicalized, so a symlink alias matches the same source. A missing configured path is
not an error because the Skill may be installed later.

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
| `list` | Paged `index` and `search` catalog; `source` appears only for ambiguous names |
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
unique counts plus ambiguous names. Each command performs a fresh discovery; a running
TUI or ACP session updates its own snapshot automatically.

`zuno debug agent <name>` gives the agent-filtered view instead, including metadata and
selected-body budgets, rendered/omitted/truncated coverage, and a bounded preview.
That is the one to use when a Skill exists but a specific agent does not see it.

## See also

- [Skills](/guide/skills)
- [Workflows and commands](/config/workflows)
- [Custom agents](/config/custom-agents)
- [Diagnostics](/operate/diagnostics)
