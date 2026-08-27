# Codex GitHub tooling and system Skills

Status: design decision only. As of 2026-08-27, Zuno does not ship a typed
GitHub provider, GitHub App integration, or privileged `merge`/`release`
command. This document defines the boundary for future work without claiming
that those capabilities already exist.

## Decision summary

Codex does not expose one universal GitHub core tool. Its GitHub workflows are
composed from several independently authorized layers:

- a Skill supplies task-specific instructions and sequencing;
- local `git`, `gh`, and shell tools inspect or mutate a checkout;
- an App Connector or MCP server supplies negotiated remote GitHub operations;
- Codex cloud can perform hosted GitHub code review;
- the sandbox, network policy, credentials, and approval policy remain the
  runtime authorities.

Zuno should adopt this separation, not Codex-specific Skill bodies or product
configuration. In particular:

- do not add an all-powerful `github` tool;
- do not build `dual-review`, `auto-release`, merge policy, or release policy
  into Zuno;
- register only capabilities that an authenticated provider has actually
  negotiated for the current repository;
- keep local review separate from publishing a GitHub review;
- treat every remote write as at-most-once and non-replayable.

## Local evidence

The CodeGraph index was refreshed with `codegraph sync` and was `current` with
zero pending files when this document was written.

The current Zuno implementation establishes these facts:

- [`BuiltinSlot`](../../crates/zuno-tools/src/registry.rs) contains generic
  shell, file, delegation, web, Skill, patch, execution, LSP, and plan slots.
  It contains no GitHub, pull-request, Actions, review-thread, merge, or release
  slot.
- Tool sources are assembled as built-in, Harness, then MCP contributions.
  This is already the correct extension seam for a future GitHub provider.
- The compiled [`BuiltinSkillDescriptor`](../../crates/zuno-orchestration/src/lib.rs)
  records a stable source identity, pack version, content SHA-256, provenance,
  allowed Agent profiles, and required tools.
- The built-in `git-workflow` Skill covers local repository hygiene. It does not
  grant remote GitHub authority.
- [`SkillTool`](../../crates/zuno-tools/src/skill.rs) loads a selected Skill body
  and its resources on demand. Initial discovery is metadata-only, so Zuno
  already has progressive disclosure rather than eagerly injecting every Skill
  body.
- Extension manifests are typed and validated by
  [`Package`](../../crates/zuno-extension/src/manifest.rs). Dynamic activation
  already separates desired and committed state and supports prepare, commit,
  abort, and rollback.
- [`ToolReplayPolicy`](../../crates/zuno-tool/src/lib.rs) defaults to `Never`;
  only explicitly read-only or idempotent tools may opt into `Safe`.

These existing mechanisms are sufficient foundations. They do not mean a
GitHub provider has been implemented.

## Codex capability layers

### Skill recipe

A Skill describes when and how to perform a workflow. For example, a PR
monitoring Skill may instruct an agent to inspect checks, read failed logs,
apply a fix, and monitor the next run. The Skill does not itself provide GitHub
credentials, network access, repository permissions, or merge authority.

Codex's own repository follows this pattern with repository-local Skills such
as `babysit-pr`. The recipe composes existing tools; it is not a privileged
GitHub subsystem.

### Local Git and GitHub CLI

Local `git` can inspect the checkout, commits, branches, and diffs. The GitHub
CLI can additionally access remote PR, Actions, merge, and release APIs when
the executable is installed and its authenticated identity is authorized.

The presence of `gh` is not sufficient capability evidence. A host must also
resolve:

- the repository and GitHub host;
- the authenticated account or App identity;
- token scopes or App installation permissions;
- branch protections and repository rules;
- the exact operation supported by the installed CLI/API version.

### App Connector or MCP provider

An App Connector or MCP server can expose GitHub operations without routing
them through an unrestricted shell. Its advertised tool list is still only an
input to negotiation. Zuno must map the server's concrete schemas and
authenticated permissions into its own typed capability set before registering
consumer tools.

An MCP connection must not silently inherit capabilities merely because a
server is named `github`.

### Hosted GitHub review

Codex cloud can review a GitHub pull request and publish findings as a standard
GitHub review. That is a distinct product surface from local review and is an
external write.

## Operation boundaries

| Operation | Meaning | Mutates remote state | Proposed Zuno capability |
| --- | --- | ---: | --- |
| Repository inspection | Read refs, commits, branches, status, and diffs | no | `repo.read` |
| PR metadata and check rollup | Read PR state, head SHA, labels, approvals, and check conclusions | no | `pr.read` |
| Create a PR | Publish a new PR for an existing branch | yes | `pr.create` |
| Read review threads | Read inline threads, comments, resolution state, and reviewer identity | no | `review_thread.read` |
| Write review threads | Publish, reply to, resolve, or unresolve review discussion | yes | `review_thread.write` |
| Read Actions logs | Read workflow-run and job logs, including failed-step output | no | `actions.logs.read` |
| Merge a PR | Merge, squash, rebase, enqueue, or enable auto-merge | yes | `pr.merge` |
| Publish a release | Create or update a release, tag, notes, and artifacts | yes | `release.publish` |

These capabilities are deliberately separate:

- `gh pr checks` reports a PR or commit check rollup. It does not fetch complete
  workflow logs and does not authorize merge.
- `gh run view --log` and `--log-failed` read logs for a workflow run. Log access
  does not imply permission to rerun or cancel it.
- `gh pr merge` changes protected remote state and may interact with a merge
  queue, auto-merge, or branch deletion.
- `gh release create` may create a tag, publish release notes, and upload
  artifacts. It is a stronger and different authority than merging a PR.
- Creating or replying to a review thread is external communication. It must
  not be treated as a side effect of a local code review.

Future rerun, cancel, label, branch-delete, or release-delete operations should
receive their own capability identifiers instead of being hidden under the
entries above.

## Local review is not PR publication

Codex's local review mode and its `review-agent` Skill are read-only:

- inspect a specified diff or commit;
- report actionable findings;
- do not modify files;
- do not create commits or push branches;
- do not post GitHub comments.

Publishing findings to a PR is a separate provider call requiring
`review_thread.write`. Zuno must preserve the same boundary:

1. a reviewer produces a durable local report;
2. a user or project workflow decides whether the report should be published;
3. the runtime verifies the remote provider and capability;
4. permission is requested for the external write;
5. the provider publishes and returns an authoritative remote identifier.

A generic Council or user-defined review Skill may produce the report. Neither
the Council nor the Skill grants publication authority.

## Proposed provider contract

A future GitHub integration should use provider negotiation, not static
assumptions:

```text
provider discovery
  -> credential resolution
  -> repository identity resolution
  -> authenticated capability probe
  -> typed capability registration
  -> consumer tool registration
  -> disposer on logout, revocation, replacement, or shutdown
```

Possible providers include:

- a local Git provider that can supply only `repo.read`;
- a `gh` provider that maps the installed CLI and authenticated scopes;
- a GitHub App or MCP provider that maps its negotiated tool schemas and App
  installation permissions.

Each provider must publish:

- provider and authenticated identity;
- GitHub host and immutable repository identity;
- exact capability identifiers;
- capability-specific schema/version information;
- permission and policy constraints;
- lifecycle state and a disposer.

Consumers register only after the capability probe succeeds. Logout, token
revocation, repository replacement, provider shutdown, or capability loss must
withdraw exactly the tools that provider registered.

## Authorization, replay, and secrets

### Read operations

`repo.read`, `pr.read`, `review_thread.read`, and `actions.logs.read` may declare
`ToolReplayPolicy::Safe` only when the concrete provider operation is
side-effect-free and idempotent. Rate limits, pagination, cancellation, and
stale-head handling remain explicit provider concerns.

### Write operations

`pr.create`, `review_thread.write`, `pr.merge`, and `release.publish` must use
`ToolReplayPolicy::Never`.

A timeout or lost response is an uncertain outcome. Zuno must persist that
state and inspect authoritative GitHub state before any user-directed retry:

- PR creation: look for a matching head/base and existing PR;
- review publication: look for the returned or matching comment/thread;
- merge: inspect merged state and the exact head SHA;
- release: inspect the tag, release record, and uploaded artifact digests.

The runtime must never mechanically repeat a remote write after a timeout.

### Credentials and logs

Tokens, refresh tokens, App private keys, installation tokens, and authorization
headers must not enter:

- tracing fields or rendered errors;
- durable session events;
- tool arguments visible to the model;
- subprocess command lines;
- cached Skill or extension metadata.

Providers should receive opaque credential handles or redacted secret values
from the credential service. A Skill must never contain a token or instruct the
model to print one.

Sandboxing and approval remain independent. A Skill or provider capability
cannot bypass filesystem, network, process, or HITL policy.

## System Skill comparison

Codex system Skills are preinstalled packages, not privileged core tools. Their
instructions and helper resources still depend on the host's tools,
permissions, sandbox, and external credentials.

| Codex system Skill | Product-specific dependency | Zuno decision |
| --- | --- | --- |
| `review-agent` | Codex review format and delegated review behavior | Do not copy. Keep local review as a user/project workflow or a Zuno-native reviewer/Council contract. |
| `plugin-creator` | `.codex-plugin/plugin.json`, Codex marketplace files, cachebuster, and reinstall commands | Do not copy. Use Zuno's typed extension manifest and lifecycle. |
| `openai-docs` | OpenAI documentation domains, Codex manuals, models, and OpenAI product routing | Do not copy. It is useful only as an external user-installed Skill when OpenAI work requires it. |
| `skill-creator` | Codex packaging conventions such as `agents/openai.yaml` | Adapt only general progressive-disclosure and validation principles. |
| `skill-installer` | `$CODEX_HOME/skills`, `openai/skills`, and Codex restart/loading behavior | Do not copy. A Zuno installer needs Zuno-native destinations, schema, provenance, and activation. |

Zuno already provides the appropriate first-party entry points:

- `customize-zuno` for configuring providers, authentication, permissions,
  Agents, workflows, Skills, MCP servers, and extensions;
- `develop-zuno` for implementing Zuno-native components, providers, Agents,
  Skills, and extension points;
- `git-workflow` for local repository hygiene.

Their compiled descriptors include version, digest, provenance, allowed
profiles, required tools, and stable `builtin://` identities. They use the same
progressive Skill loading path as external Skills.

## User-owned release and review workflows

Zuno must not ship `dual-review`, `auto-release`, merge policy, or release policy
as built-in product behavior. Those workflows contain project-owned decisions:

- reviewer topology and severity thresholds;
- required checks and Actions workflows;
- branch protection and merge strategy;
- versioning, changelog, tag, artifact, and signing policy;
- environments, rollout, rollback, and incident ownership;
- whether and where review comments are published.

Users may define these as global or project Skills. A workflow that needs
GitHub must declare the exact required typed capabilities and fail closed when
they are unavailable. A Skill may sequence operations; it cannot register a
provider or expand permissions.

## Future Skill and plugin installation

The runtime already has typed extension manifests and transactional activation.
A future installer should preserve that architecture:

1. download or copy into an isolated staging directory;
2. validate paths, package identity, schema version, manifests, digests,
   provenance, runtime declarations, and required capabilities;
3. construct and prepare the candidate composition without publishing it;
4. atomically activate the validated package;
5. commit the new generation only after all consumers start successfully;
6. roll back files and runtime state together on failure;
7. retain enough provenance to audit or remove the exact installed version.

The Rust manifest types and generated JSON Schema must remain the single source
of truth. Installer scripts, TUI forms, documentation examples, and plugin
validation must consume that schema rather than maintaining parallel field
lists.

This is a required future contract, not a claim that a complete remote
marketplace installer exists today.

## Upstream references and refresh pins

The following sources were checked on 2026-08-26. Repository commits are pins
for reproducible comparison; web documentation must also be re-read because its
content is not identified by a public commit.

### OpenAI Codex

- Skills documentation: <https://developers.openai.com/codex/skills>
- Plugins documentation: <https://developers.openai.com/codex/plugins>
- MCP documentation: <https://developers.openai.com/codex/mcp>
- GitHub code review: <https://developers.openai.com/codex/integrations/github>
- Local app review: <https://developers.openai.com/codex/app/review>
- Sandbox and approvals: <https://developers.openai.com/codex/security>
- Repository: <https://github.com/openai/codex>
- Pinned commit:
  <https://github.com/openai/codex/commit/bde9db1375667c50dcc0c2b52532a4e2672571c2>
- Relevant pinned paths:
  - <https://github.com/openai/codex/tree/bde9db1375667c50dcc0c2b52532a4e2672571c2/.codex/skills>
  - <https://github.com/openai/codex/tree/bde9db1375667c50dcc0c2b52532a4e2672571c2/codex-rs/skills>
  - <https://github.com/openai/codex/tree/bde9db1375667c50dcc0c2b52532a4e2672571c2/codex-rs/ext/skills>

### OpenAI system Skills

- Repository: <https://github.com/openai/skills>
- Pinned commit:
  <https://github.com/openai/skills/commit/49f948faa9258a0c61caceaf225e179651397431>
- System Skill tree:
  <https://github.com/openai/skills/tree/49f948faa9258a0c61caceaf225e179651397431/skills/.system>

The Codex repository also carries sample assets for `review-agent`,
`plugin-creator`, `openai-docs`, `skill-creator`, and `skill-installer` at the
Codex commit above. Those are comparison inputs, not source material for Zuno
prompts.

### GitHub CLI and MCP

- PR checks: <https://cli.github.com/manual/gh_pr_checks>
- Actions run and logs: <https://cli.github.com/manual/gh_run_view>
- PR merge: <https://cli.github.com/manual/gh_pr_merge>
- Release publication: <https://cli.github.com/manual/gh_release_create>
- GitHub CLI repository: <https://github.com/cli/cli>
- Pinned GitHub CLI commit:
  <https://github.com/cli/cli/commit/606cda4a9b1a703ad7c2e353a77bce0d93d21b0e>
- GitHub MCP Server repository:
  <https://github.com/github/github-mcp-server>
- Pinned GitHub MCP Server commit:
  <https://github.com/github/github-mcp-server/commit/a00dc319edcb5f8a10f118b1dad649c94928aac4>

## Refresh procedure

Before changing this design:

1. fetch the latest commits for the four repositories above;
2. re-read the official Codex and GitHub CLI pages;
3. compare only the relevant Skill, provider, permission, review, and lifecycle
   paths;
4. record each discovered behavior as adopt, adapt, or reject;
5. do not copy upstream prompt or Skill bodies;
6. run `codegraph sync` and re-check Zuno's current capability seams;
7. update this page only when the architectural decision or upstream pin
   changes.
