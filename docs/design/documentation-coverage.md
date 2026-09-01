# Documentation architecture and coverage

Zuno treats documentation as part of each public contract. A capability is not
complete until users, extension authors, operators, and maintainers can find the
page that owns its behavior, boundaries, failure modes, and verification path.

“Everything documented” does not mean documenting every private helper. It
means every public command, configuration field, protocol, durable state
transition, security boundary, extension interface, operational procedure, and
release artifact has one canonical page and no contradictory copy.

## Ownership rules

Every behavior-changing change must:

1. name the public surface and its canonical page;
2. update that page in the same change as the implementation;
3. document defaults, authority, durability, failure, recovery, and platform
   differences where they apply;
4. link from the nearest task-oriented guide or index;
5. add a documentation contract test when a safety or compatibility boundary
   must not drift silently.

Reference pages own exact fields and protocols. Guides own task sequences.
Design records own rationale and rejected alternatives. Operational pages own
diagnosis, migration, rollback, and evidence. README files provide routes into
those pages rather than duplicating their full contracts.

## Coverage map

| Public surface | Canonical documentation |
| --- | --- |
| Product scope and execution model | [What is Zuno?](/guide/what-is-zuno), [Harness Runtime](/harness-runtime) |
| Installation and platform prerequisites | [Installation](/guide/installation), [Quick start](/guide/quick-start) |
| Configuration, providers, models, and credentials | [Configuration reference](/reference/configuration), [Providers and credentials](/reference/providers) |
| Agents, permissions, Skills, and delegation | [Agents](/guide/agents), [Custom agents](/config/custom-agents), [Permissions](/guide/permissions), [Orchestration](/orchestration) |
| Agent and extension implementation | [Developing agents and extensions](/guide/extension-development), [Plugins](/plugins), [Process plugins](/process-plugin-development) |
| Native components, profiles, drivers, and lifecycle | [Harness Runtime](/harness-runtime), [Developing agents and extensions](/guide/extension-development) |
| Tools, MCP, LSP, web, Shell, and sandbox | [Tools](/guide/tools), [Permissions](/guide/permissions), [Shell sandbox roadmap](/design/shell-sandbox-roadmap) |
| Sessions, prompts, inbox, goals, plans, retries, and recovery | [Sessions](/guide/sessions), [Durable state](/guide/durable-state), [Harness Runtime](/harness-runtime) |
| TUI, headless, ACP, HTTP, and client projections | [CLI reference](/cli/), [Zed and ACP](/reference/zed-acp), [Client interfaces](/design/client-interfaces) |
| Images, file references, import, and export | [Attachments](/reference/attachments), [Portable bundles](/reference/portable-bundles) |
| SQLite schema, migrations, retention, and continuity | [Database lifecycle](/migration), [Session retention](/session-retention), [History and Notes](/config/continuity) |
| Logging, diagnostics, resource gates, and performance | [Logging](/logging), [FAQ](/faq), [Diagnostics](/operate/diagnostics), [Resource gates](/resource-gates), [Performance](/perf-methodology) |
| Product agents, memory, and learning | [Product agents](/design/product-agents), [Resident memory](/design/memory-learning), [Learning flywheel](/design/user-learning-flywheel) |
| Self-update, CI, release assets, and rollback | [Self-update](/reference/self-update), [Release pipeline](/operate/release-pipeline) |

The English page is canonical when a generated schema or exhaustive protocol
reference is not translated. Chinese task guides must still state the usable
workflow, safety boundary, and link to the exact reference.

## Change checklist

Before merging a public change, verify:

- the command help, config schema, runtime behavior, and docs agree;
- new states and errors describe operator action and recovery;
- durable schema changes include a migration and preservation evidence;
- supported OS and architecture differences are explicit;
- extension changes identify the interface, provider, and consumer;
- examples exercise the shipped artifact rather than only source-tree code;
- removed behavior and obsolete compatibility claims are removed from search
  results and navigation;
- English and Chinese entry points reach the updated contract.

## Site publication

Markdown under `docs/` is source-owned by the Zuno repository. After a docs
change reaches `main`, `.github/workflows/publish-docs.yml` checks out the
Firlab repository and runs `docs/scripts/sync-zuno-docs.sh`. The sync copies the
owned documentation tree into Firlab, records the exact Zuno commit, and the
Firlab VitePress workflow publishes it at `zuno.firlab.app`.

Publication is complete only when:

1. the Zuno docs workflow succeeds for the merged commit;
2. the corresponding Firlab commit and deployment succeed;
3. the English and Chinese routes render from the public site;
4. links and code blocks are usable in the deployed page.

Local checks catch structure and rendering failures before CI:

```sh
cargo test -p zuno --test docs
git diff --check

# In a disposable Firlab checkout:
docs/scripts/sync-zuno-docs.sh /path/to/zuno
pnpm --dir docs build
```

## Maintaining this map

Add a row when a new public surface has no existing owner. Split a page when
different audiences or lifecycles make one owner ambiguous. Do not create a new
page merely to repeat an existing contract; link to the canonical owner and
document only the task-specific context.
