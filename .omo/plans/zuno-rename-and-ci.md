# Zuno — rename, own configuration, and AWS-hosted CI

> **Zuno — Zero code. Any task.**

Created 2026-08-14, at the user's direction:

> *"最后需要移除对 opencode 配置的读取，然后将本项目重命名：Zuno — Zero code. Any task.
> 使用自己的配置文件目录。然后根据 github-project skill 优化本项目设计，并根据
> github-codebuild skill 实现 aws 托管的 actions 构建测试验证等"*

## Status of the work that precedes this

- Main plan **183/183**, four reviewers approved on one product tree.
- Follow-up **7/7 closed**, plus **FU-8 defect B closed** — an empty assistant turn now exits
  non-zero naming the provider, verified by the orchestrator against a real provider.
- `main` at **3491 tests passing / 0 failed**, 0 clippy warnings, zero first-party `unsafe`.
- **FU-8 defect A remains open**: a provider declaring `@ai-sdk/openai` with a custom `baseURL` is
  routed to `/responses`, which many gateways answer 400/404. Measured on the user's own gateway.

## Measured scope — the numbers that shape the plan

| Fact | Count | Where |
|---|---:|---|
| Source files mentioning `opencode` | **181** | `crates/*/src/` |
| Distinct `OPENCODE_*` environment variables | **72** | `crates/*/src/` |
| Crates in the workspace | **36** | `crates/` |
| `runs-on: ubuntu-latest` jobs in CI | **4** | `.github/workflows/ci.yml` (210 lines) |
| `runs-on` entries in release | **5** | `.github/workflows/release.yml` (418 lines) |
| **git remote** | **none** | — |
| **LICENSE** | **absent** | — |

Two of those decide the ordering. There is **no git remote**, and CodeBuild-hosted runners require
a GitHub repository with a CodeConnections App authorisation against it. And `LICENSE` is missing,
which the project-scaffold skill treats as table stakes for a public repo.

## The dependency order, and why it is not negotiable

```
Z-1  own config dirs (breaking) ──┐
Z-2  rename                       ├──► Z-3  repo scaffold ──► Z-4  CodeBuild runners
FU-8A  provider surface  ─────────┘
```

- **Z-1 before Z-2**: renaming while still reading `~/.config/opencode/` would ship a binary called
  Zuno that silently depends on its predecessor's directories. Decide the directory story first.
- **Z-3 before Z-4**: the CodeBuild path needs a remote, a CodeConnections authorisation bound to
  that remote, and a workflow to attach runners to. Skill §1 makes the connection **region-scoped**
  and its GitHub App handshake **console-only** — it cannot be done from the CLI.
- **FU-8A should land before Z-3** so the first CI run on the new repo is not red for a known cause.

---

## Z-1. Own the configuration and data directories

**Status: CLOSED (2026-08-14)** — Zuno-only config/data/project directories are implemented with no migration, dual-read, or fallback; 66 project-owned environment names moved to `ZUNO_*`, the six measured plugin ABI names remain `OPENCODE_*`, matrix and mutation tests pin the hard cut, and a single real non-`--pure` run discovered the copied Zuno configuration and plugins before the selected Bedrock endpoint returned HTTP 404.

**This is the one genuinely breaking change in the set**, and I will not guess at it.

### What exists today

`crates/oc-paths/src/config_chain.rs:257` uses the literal `"opencode"` for the config file's
directory, and `PROJECT_CONFIG_DIRECTORY = ".opencode"` (asserted at `oc-paths/src/lib.rs:285`).
`config_directories()` (`:202`) already returns a **chain**, and the tool registry consumes it as a
chain (`oc-tools/src/registry.rs:129`) — so *adding* sources is cheap. Removing the old one is not.

### What breaks if `~/.config/opencode/` and `~/.local/share/opencode/` stop being read

Concretely, on this machine, today:

- **Three working plugins** declared in `opencode.json`: `opencode-antigravity-auth@1.6.0`,
  `@sunerpy/opencode-kiro-auth@0.20.6`, `@sunerpy/oh-my-openagent@4.21.0`.
- **`auth.json` with live credentials**, including the `google` OAuth token that E-9's
  `google_search` plugin depends on and the `api` keys for `myopenai` / `kiro-auth`.
- **Seven configured providers** and 404 resolved models.
- **The session database** — every existing conversation.

A user who upgrades and finds an empty model list and no history has been broken by us, silently.

### DECIDED: hard cut (user, 2026-08-14)

> *"Z-1 的话硬切就行  因为还没有发布投产"*

**The project has never been released, so there is no installed base to protect.** That removes the
entire compatibility question: read Zuno paths only, write Zuno paths only, no dual-read, no
migration, no deprecation window. The three options previously listed are moot.

**What this simplifies, concretely**: no migration atomicity to get right, no "both present"
precedence rule, no fallback path to test, and no announcement cycle. The work becomes a rename of
literals plus the tests that pin them.

**What it still costs, and must be handled rather than discovered:**

- **This machine's own setup stops being read.** `~/.config/opencode/opencode.json` (7 providers, 404
  models, 3 plugins) and `~/.local/share/opencode/auth.json` (the live `google` OAuth plus the
  `myopenai` / `kiro-auth` API keys) will no longer be found. That is correct behaviour under a hard
  cut, but it means **the orchestrator's own end-to-end verification breaks until a Zuno-located
  config exists**. Provide a documented one-liner that copies the existing config into the Zuno
  location so hands-on QA — including FU-8A's verification — remains possible.
- **`.omo/fixtures/` and the compat suite** may reference `opencode` config paths. The oracle is the
  released `opencode` binary and **its** paths must not be renamed; only *this* project's paths move.
  Confirm that distinction test by test rather than by global replace.
- **The 72 environment variables** are a separate decision from the directories — see below. A hard
  cut on directories does not automatically imply a hard cut on variable names.

### Environment variables: hard cut too, with one exception to verify

The same reasoning applies — nothing is deployed, so `ZUNO_*` can be the only accepted spelling.

**The exception to check before assuming**: variables that the **plugins** set or read are not ours
to rename. `OPENCODE_CONFIG_CONTENT` and `OPENCODE_AUTH_CONTENT` are used by this project's own
tests, which is fine to rename. But if any of the three installed plugins reads an `OPENCODE_*`
variable from the host environment, renaming it breaks that plugin regardless of release status.

**Established empirically, so this is settled rather than open.** The original
`/config/.bun/install/cache/` measurement used stale manifests
(`oh-my-openagent@4.10.0`, `opencode-antigravity-auth@1.2.8`) and is retained only as historical
context. The decision was re-measured against the exact loaded bundles under
`/config/.cache/opencode/packages/`: `@sunerpy/oh-my-openagent@4.21.0`,
`opencode-antigravity-auth@1.6.0`, and `@sunerpy/opencode-kiro-auth@0.20.6`.

- The loaded bundle union contains **30** `OPENCODE_*` names; the bundle-local counts are 25 for
  OMO, 7 for Antigravity, and 0 for Kiro. Antigravity's own namespace is not a host ABI.
- This project uses **72**.
- **The intersection is exactly 6**, and these are the ones a rename would break:

```
OPENCODE_CLIENT              OPENCODE_CONFIG_CONTENT      OPENCODE_CONFIG_DIR
OPENCODE_DISABLE_CLAUDE_CODE OPENCODE_SERVER_PASSWORD     OPENCODE_SERVER_USERNAME
```

`OPENCODE_CONFIG` and `OPENCODE_VERSION` were artifacts of the stale measurement and are not in the
loaded-bundle intersection.

**So the rule is precise: those 6 keep their `OPENCODE_*` spelling as a documented plugin-facing
contract. The other 66 become `ZUNO_*` with no fallback.** Do not treat the 6 as an oversight to be
cleaned up later — they are the ABI the JS plugin tier speaks, and this project deliberately keeps
that tier for the three real installed plugins.

Note `OPENCODE_CONFIG_DIR` is in the intersection. A plugin can therefore point the host at a config
directory. Under a hard cut the *default* directory becomes Zuno's, but this override must keep
working, and a test should pin that.

### Acceptance criteria (agent-executable)

- The chosen directory strategy is implemented, and a test proves the behaviour for each of: only-old
  present, only-new present, both present, neither present.
- If migration is chosen, a test proves it is atomic and idempotent, and that a second run is a no-op.
- The environment-variable strategy is implemented and a test pins each accepted spelling.
- A test proves a config in the old location is still discoverable if the strategy says it should be —
  and provably ignored if the strategy says it should not.
- No credential value appears in any test fixture, log, or evidence file.

---

## Z-2. Rename to Zuno

**Mechanical, but 181 files and 36 crates deep.** Do it after Z-1 so the directory semantics are
already settled.

### The parts that are not mechanical

- **`COMPATIBILITY_VERSION = "1.18.13"`** (`crates/oc-cli/src/version.rs:11`) is what npm plugins see
  when they check `engines.opencode`. It is deliberately distinct from the oracle pin, and
  `crates/oc-cli/tests/surface.rs:76` asserts it. **Under Zuno, what does a plugin's
  `engines.opencode` range mean?** One fact bears on it: `antigravity@1.6.0` declares only
  `engines: {"node": ">=20.0.0"}` and no `opencode` key at all, so todo 174's gate does not fire on
  any of the three real installed plugins. The npm compatibility story is thinner than it appears.
- **Whether `opencode` remains an accepted command alias.**
- **`user_agent()`** (`version.rs:44`) — it must not masquerade as the TypeScript binary, which the
  module doc already states as a requirement.
- **Crate names** (`oc-*`): renaming 36 crates is churn with no user-visible benefit. **I would keep
  the `oc-` prefix** and rename only the binary, the user agent, the display name, and the docs. Say
  so explicitly rather than leaving it ambiguous.
- **`docs/divergences.toml`'s meaning.** Its 17 entries mean "deviation from upstream". That framing
  survives Z-1 and Z-2 only if the compatibility surface is still defined against upstream. This is
  the open question already recorded in `zuno-strategy.md`; renaming does not settle it.

### Acceptance criteria (agent-executable)

- The binary, user agent, long display version, and help text say Zuno; a test pins each.
- `COMPATIBILITY_VERSION`'s meaning is documented and its test updated to state *why* the value is
  what it is.
- The alias decision is implemented and tested either way.
- `grep -rn "opencode" crates/*/src/` returns only: upstream oracle references, historical comments,
  and the plugin-compatibility surface — each of which a test or a comment justifies.

---

## Z-3. Repository scaffold, per the `github-project-scaffold` skill

**Blocked on: there is no git remote, and no LICENSE.** Both must exist before Z-4 is even possible.

Scope from the skill: a bilingual README (Chinese root + English under `docs/readme/`, slim with a
TOC and detail pushed into `docs/`), LICENSE, repository description and topics via `gh`, commit-time
formatting and push-time test hooks, `oxfmt` for YAML/JSON/Markdown, a Makefile (one exists — audit
rather than replace), and release automation with release-please + git-cliff.

**Judgement calls to make rather than apply blindly:**

- **Release automation already exists** — `release.yml` is 418 lines with 5 `runs-on` entries.
  Audit it against the skill's release-please + git-cliff shape before rewriting; a working pipeline
  is worth more than a canonical one.
- **The skill wants changelogs split by major version and a GHCR image.** Decide whether a container
  image is wanted at all for a single-binary CLI whose entire selling point is having no runtime.
- **A one-line install script** is in scope and genuinely useful here.

### Acceptance criteria (agent-executable)

- `LICENSE` exists and is referenced from the README and `Cargo.toml`.
- The remote exists, and `gh repo view` shows a description and topics.
- Hooks run formatting at commit and tests at push, and `make` targets exist for both; a test or a
  hook dry-run proves they fire.
- The README's structure matches the skill's shape, and no claim in it is unverified — this project
  has fixed seven "prose nothing derives from" defects, so a README asserting a test count or a
  feature must derive it or omit it.

---

## Z-4. AWS-hosted GitHub Actions runners, per the `github-actions-on-codebuild` skill

**Blocked on Z-3.** Runner-mode CodeBuild needs a repository, and the CodeConnections GitHub App
handshake is **console-only** — no CLI path exists (skill §1).

### Why this shape rather than migrating to CodePipeline

The skill is explicit, and it matches this project's situation: changing `runs-on` preserves every
existing workflow, secret, and OIDC role, whereas a CodePipeline migration duplicates deployment
logic — *"部署逻辑出现第二份，而第二份必然漂移"*. With 628 lines of working workflow across two
files, that duplication is the larger risk.

### The six steps, and the three traps that matter most here

Steps: CodeConnections → service role (**two policies, no ECR/ECS**) → runner-mode project
(`buildspec: ""`, `privilegedMode: true`, `reportBuildStatus: false`) → webhook filtered to
`WORKFLOW_JOB_QUEUED` → change `runs-on` → verify via webhook deliveries, not just job status.

Traps, all of which the skill reports having hit in practice:

1. **Create the webhook *after* the App authorisation.** Otherwise `workflow_job` arrives null and
   CodeBuild returns 400 with a message that points nowhere near the cause.
2. **Give every job a unique label.** GitHub routes by label **superset**, so a job with fewer labels
   can have its runner stolen by one with more — and the symptom is a job hanging forever with no
   error. This project has **9 jobs across two workflows**; that is exactly the condition.
3. **Pin tool versions.** `aws/codebuild/standard:7.0` defaults to Node 18 while GitHub-hosted images
   are newer, and `npm install` only *warns* on `EBADENGINE` — so a failure surfaces two steps later
   with no mention of versions. Less acute for a Rust project, but `release.yml` may use Node tooling.

**One project-specific check the skill cannot know**: `cargo test --workspace` here is heavy, and this
session repeatedly hit host `EAGAIN` under parallel load. Establish the compute size deliberately and
record why, rather than accepting `BUILD_GENERAL1_MEDIUM` because it is the example.

### Acceptance criteria (agent-executable)

- Every job's `runs-on` names the CodeBuild project **verbatim** and carries a **unique** second
  label; a check proves no two jobs share a label set.
- Webhook deliveries for `queued` and `in_progress` both return 200, recorded in the evidence.
- The service role has exactly the two documented policies and **no** ECR/ECS permissions.
- A full CI run passes on the CodeBuild runners with the same result as on `ubuntu-latest`, and both
  results are recorded.
- Compute size and its rationale are documented.

---

## Execution order (confirmed by the user)

1. **FU-8A** — the `@ai-sdk/openai` → Responses routing defect. Small, and it makes the first CI run
   on the new repo meaningful rather than red for a known cause.
2. **Z-1 hard cut** — Zuno-only config and data directories; `ZUNO_*` variables only, except any
   variable an installed plugin provably reads.
3. **Z-2 rename** — binary, user agent, display version, help text and docs. **Keep the `oc-` crate
   prefix**: renaming 36 crates is churn with no user-visible benefit.
4. **Z-3 audit-then-fill** — `release.yml` is 418 working lines; audit it against the skill rather
   than replacing it. Add LICENSE and the remote, which Z-4 depends on.
5. **Z-4 last** — CodeBuild runners, once a remote exists to authorise against.

Each step's acceptance criteria are above. FU-8A's are in
`.omo/plans/opencode-rust-followup.md` under `## FU-8`.
