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
metadata index, search keeps the larger model-discoverable catalog available on demand, and
a body is loaded only after selection.

Identity is the second reason. A Skill has a source, so the same name from two roots stays
independently addressable and no hidden precedence winner is chosen. That is what makes a
project Skill and a global Skill with the same name a visible ambiguity rather than a
silent surprise.

## Discovery order

Zuno discovers Skills in this scope order:

1. project `.zuno/skill` roots, from the current directory to the worktree;
2. project `.agents/skills` roots over the same walk;
3. `$XDG_CONFIG_HOME/zuno/skill` (normally `~/.config/zuno/skill`) and
   `ZUNO_CONFIG_DIR/skill` when that override is set;
4. global `~/.agents/skills`;
5. explicit `skills.paths`;
6. configured remote indexes.

Project scope is therefore advertised before user-global scope. Zuno never scans
`.claude`, `.opencode`, or another product's config directory implicitly. A shared
directory can still be selected deliberately through `skills.paths`. The same canonical
source path is de-duplicated, including symlink aliases.

Zuno does not implicitly scan `~/.zuno`, `~/.config/zuno/skills`, or project
`.zuno/skills`. Add any non-canonical directory explicitly through `skills.paths`.

```sh
ZUNO_DISABLE_EXTERNAL_SKILLS=1 zuno
```

This disables implicit `.agents` roots. Zuno-native `.zuno` roots, configured Zuno roots,
and explicit `skills.paths` stay enabled.

## Changes during a running session

A session uses one shared, atomically published Skill catalog generation for
prompt discovery, required Skills, the `skill` tool, slash commands, TUI, and
ACP. Installing, editing, deleting, or renaming a Skill in an already-effective
root is visible without restarting the session. A malformed edit retains the
last valid entry and exposes a warning until the file is repaired.

The canonical user root and explicit configured paths may be created while a
session is running. Zuno watches only their nearest existing ancestor
non-recursively, narrows the subscription as directories appear, and enables
recursive watching only at the exact configured root. A shared
`~/.agents/skills` root is watched when it already exists; create it before
starting Zuno or select it through `skills.paths` when hot installation is
required.

The remote download cache is private state. It is created only when
`skills.urls` actually downloads a file and is never installed as a filesystem
watch root.

A `skills.urls` index decides what goes into that cache, and it is remote input, so
Zuno bounds what it may name. An index entry's
`name` must be a single directory segment: an absolute name, one containing `..`, and one
containing a path separator are all skipped with a warning, and nothing is downloaded for
them. The rule matters because a versioned entry is refreshed by staging the download
beside the target directory and then renaming the directory's current contents away and
deleting them — an unchecked name could have aimed that at your own Skill directory
instead of the cache. See
[Remote Skill indexes](/config/authoring-skills#remote-skill-indexes).

If a caller tries to load an old exact source after a rename, Zuno refreshes
once and then returns `CatalogStale` with the current exact locators. It never
guesses between duplicate names.

## How a Skill reaches the model

The prompt gets the catalog, not the bodies:

```json
{
  "skills": {
    "includeInstructions": true,
    "maxContextTokens": 8000,
    "maxSelectedContextTokens": 16000,
    "config": [
      {
        "path": "~/.config/zuno/skill/powerapps",
        "recursive": true,
        "exposure": "search"
      }
    ]
  }
}
```

`maxContextTokens` explicitly bounds the compact catalog in approximate tokens. Without an
override, Zuno uses a character budget equal to roughly two percent of a known model
context. If the context is unknown, the fallback is approximately 8,000 characters rather
than a 2,000-character fixed cap. The explicit override is converted to characters and
capped at 10,000 tokens.
Unique Skill names omit their absolute source path from this prompt index. A source
locator is included only for same-named entries that would otherwise be ambiguous.

Every enabled Skill has one catalog exposure:

| Exposure | Initial index | `skill search` / `list` | Exact load, `$name`, `/<name>`, `requiredSkills` |
| --- | --- | --- | --- |
| `index` | yes | yes | yes |
| `search` | no | yes | yes |
| `explicit` | no | no | yes |

`index` is the default. Use `search` for large vendor or domain packs that should remain
discoverable without occupying every initial prompt. Use `explicit` for instructions that
must never be selected from a capability match. The model is told how many search-only
sources exist, but explicit-only names are intentionally absent.

Fully selected bodies share a separate aggregate budget. Its default is ten percent of a
known context, with a 2,000-token floor and a 32,000-token ceiling.
`maxSelectedContextTokens` overrides that but stays capped at 32,000. If selected bodies do
not fit, loading or restoring the session fails before a provider request rather than
silently dropping instructions.

`includeInstructions: false` removes both the trigger policy and the initial index from
prompts. The `skill` tool still supports paged `list` and `search` over `index` and `search`
entries.

A large personal library therefore does not inject every `SKILL.md` body into every
request. A vendor or domain pack can remain in a normal discovery root while one recursive
`skills.config` entry makes it search-only. Do not delete or merge distinct Skills merely
to reduce the catalog count.

An exact `skills.config` entry can also set `"enabled": false`. Paths may name a Skill
directory or its `SKILL.md`; `"recursive": true` applies the entry to descendants. Entries
are evaluated in order and the last matching entry wins, so a later exact entry can
re-enable or reclassify one Skill below a broader subtree rule. Existing paths are
canonicalized, including symlink aliases.

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

Zuno compiles eleven first-party Skills into the `zuno-orchestration` pack:
`customize-zuno`, `develop-zuno`, `deepwork`, `codemap`, `verification-planning`,
`reflect`, `worktree`, `git-workflow`, `github-delivery`, `ui-design`, and
`bedrock-model-capability-review`.

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

`git-workflow` owns local repository and commit preparation. It keeps user changes
separate, verifies the staged diff, and applies Zuno's default
`zuno-agent <zuno-agent@firlab.app>` identity with command-scoped Git configuration.
Explicit current-user instructions, repository rules, and selected Skills take
precedence over that fallback.

The built-in trigger descriptions are deliberately narrower than their bodies.
`git-workflow` is for material Git decisions such as staging, commits, branches,
worktrees, delivery handoff, or preserving a dirty repository; an isolated disposable
fixture or ordinary uncommitted edit does not select it. `verification-planning` is for
high-risk, multi-surface, release, migration, security, or explicitly requested evidence
design; a bounded change whose acceptance commands are already clear proceeds under the
runtime verification contract without loading a second workflow.

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
preserved, and the summary reports source, indexed, searchable, explicit, disabled, and
unique counts plus ambiguous names. `debug agent` reports the agent-filtered view with
budgets and coverage.

## See also

- [Authoring Skills](/config/authoring-skills)
- [Workflows and commands](/config/workflows)
- [Agents](/guide/agents)
- [Configuration reference](/reference/configuration)
