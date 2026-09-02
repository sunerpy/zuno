# Instructions and AGENTS.md

Instruction files are injected into every prompt, which is why Zuno's discovery is
narrow and ordered rather than convenient. There are exactly two implicit filenames,
`AGENTS.md` and `AGENTS.local.md`, plus one explicit configuration array. Zuno does
not read `CLAUDE.md`, `CONTEXT.md`, `.opencode`, or any other product's instruction
file, and it never will implicitly: a file that silently entered every prompt without
appearing in a documented list would be impossible to audit.

## Three mechanisms, deliberately separate

| Mechanism | Source | Loaded |
| --- | --- | --- |
| Global instructions | `$XDG_CONFIG_HOME/zuno/AGENTS.md` | Always |
| Project instructions | `AGENTS.local.md` or `AGENTS.md` per directory, worktree root to current directory | Always |
| Configured instructions | The `instructions` array in `zuno.json` | Always |
| Nearby instructions | Upward walk from a file read mid-session | On demand |

Nearby discovery is the one that surprises people. When the model reads a file
partway through a session, Zuno walks upward from that file and attaches instruction
files not already accounted for. Each canonical file is charged once: the system set,
the paths already read in the session, and the current message's claims are all
consulted before an attachment happens.

## Order and priority

Zuno loads native instruction files in this order:

1. `$XDG_CONFIG_HOME/zuno/AGENTS.md`;
2. `ZUNO_CONFIG_DIR/AGENTS.md`, when a profile directory supplies one;
3. project directories from the worktree root down to the current directory.

Later entries are appended later, so nearer directories carry higher priority.
Within one directory, `AGENTS.local.md` replaces `AGENTS.md` rather than joining it,
which makes `AGENTS.local.md` the right place for machine-specific or uncommitted
rules.

A profile file selected by `ZUNO_CONFIG_DIR` does not replace the base global file.
It appends narrower, higher-priority guidance. That matters when a profile switches
provider or team: the base rules stay active, so a profile only needs to state its
differences.

```text
~/.config/zuno/AGENTS.md          global, lowest priority
$ZUNO_CONFIG_DIR/AGENTS.md        profile overlay
<worktree>/AGENTS.md              project root
<worktree>/crates/AGENTS.md       narrower
<worktree>/crates/x/AGENTS.local.md   narrowest, replaces AGENTS.md here
```

## The `instructions` key

| Key | Type | Default | Description |
| --- | --- | --- | --- |
| `instructions` | `string[]` \| `null` | none | Additional instruction files or glob patterns |

```json
{
  "instructions": [
    "docs/house-style.md",
    ".zuno/rules/*.md",
    "https://example.com/org-policy.md"
  ]
}
```

Entries may be paths or glob patterns, and a remote URL is accepted. This array is a
separate explicit source; it neither replaces the implicit cascade nor changes its
priority.

Remember that arrays replace rather than merge across configuration layers. A project
`zuno.json` that sets `instructions` discards the global array entirely. See
[Files and precedence](/config/files) for the merge rule.

A remote instruction that hangs, returns 404, or fails DNS produces a warning and is
dropped from the result. It never fails the load, because a flaky URL in a config file
must not make the agent unusable. Local reads and remote fetches are both bounded and
concurrent, with a per-URL timeout.

## When a rule file stops the turn

A rule file is admitted whole or not at all. Zuno never truncates one, because a rule
cut mid-sentence states something else: "do X unless Y" trimmed after "do X" is a
different instruction, and the user goes on believing the original is in force.

Two conditions fail the turn before the first provider request, each naming the file,
its size, and the remedy:

- the file exists but cannot be read, for example a permission error or bytes that are
  not valid UTF-8;
- the file does not fit the instruction prompt budget, which is the smaller of 64 KB
  and a quarter of the model's context window.

Neither is a warning. Sending the request anyway would put the model to work under
rules it never received, and its answer would read as confident while being wrong for a
reason nothing in the transcript shows. A failed remote fetch is the documented
exception: it reports which rules are not in force and the turn proceeds, because a
network must not decide whether the agent runs at all.

The budget is per model, so the same file can be admitted by a large-window model and
refused by a small one. The refusal states the byte count and the budget, which makes
the fix either a shorter file or a model with more room.

## First run

On the first ordinary discovery, Zuno creates a missing global `AGENTS.md` from its
own starter guidance using exclusive new-file semantics. An existing file is never
overwritten. The starter covers ownership, verification, scoped Git operations, and
safe worktree decisions; detailed procedures stay in the built-in `git-workflow` and
`worktree` Skills so they load only when relevant.

Launching with an explicit `ZUNO_CONFIG`, `ZUNO_CONFIG_DIR`, or `ZUNO_CONFIG_CONTENT`
does not materialize defaults. Run one ordinary launch first if you want the starter
file.

## What belongs in an instruction file

Instructions are unconditional: they cost prompt budget on every request, in every
session, for every agent. Content that applies only sometimes belongs in a Skill,
which is loaded on match instead. See [Authoring Skills](/config/authoring-skills)
for that boundary.

Good candidates are project invariants — build commands, ownership boundaries, review
requirements, language conventions. Poor candidates are long procedures, reference
tables, and anything a single task needs occasionally.

## Verifying what the model received

Instruction content is model-visible, so it is durably logged as part of the prompt:

```sh
zuno debug prompt --show-sensitive
```

`--show-sensitive` prints instruction, AGENTS, skill, and memory content verbatim.
Treat that output as sensitive before pasting it into a ticket. Without the flag the
prompt sections are still listed, which is usually enough to answer "did this file
get in".

To confirm which roots this executable resolved in the first place:

```sh
zuno debug paths
```

## See also

- [Files and precedence](/config/files)
- [Authoring Skills](/config/authoring-skills)
- [Diagnostics](/operate/diagnostics)
- [Configuration reference](/reference/configuration)
