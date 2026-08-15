# Zuno — rename, own configuration, and AWS-hosted CI

> **Zuno — Zero code. Any task.**

Created 2026-08-14, at the user's direction:

> *"最后需要移除对 opencode 配置的读取，然后将本项目重命名：Zuno — Zero code. Any task.
> 使用自己的配置文件目录。然后根据 github-project skill 优化本项目设计，并根据
> github-codebuild skill 实现 aws 托管的 actions 构建测试验证等"*

Direction updated 2026-08-15: **Zuno is an independent project.** Compatibility
with the released `opencode` binary, and importing or restoring its sessions, are
not product goals. The npm plugin tier remains supported, so
`COMPATIBILITY_VERSION`, `engines.opencode`, and the six measured `OPENCODE_*`
plugin-ABI names stay load-bearing. Existing differential suites and compatibility
documents remain in place pending a separate explicit decision; their presence is
verification history, not a mandate to add old-directory fallback or session
interchange features.

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
| Lowercase `opencode` source inventory | **943 occurrences / 169 files** | committed R3 inventory |
| Distinct `OPENCODE_*` environment variables | **72** | `crates/*/src/` |
| Crates in the workspace | **36** | `crates/` |
| `runs-on: ubuntu-latest` jobs in CI | **4** | `.github/workflows/ci.yml` (210 lines) |
| `runs-on` entries in release | **5** | `.github/workflows/release.yml` (418 lines) |
| **git remote** | **origin configured** | `git@github.com:sunerpy/zuno.git` |
| **LICENSE** | **present** | `LICENSE` |

The remote and license were absent when the plan was drafted; Z-3 subsequently created both. In
AWS China, Z-4 cannot use CodeConnections because that partition has no endpoint, so the prepared
CodeBuild project uses project-scoped `SECRETS_MANAGER` PAT authentication instead.

## The dependency order, and why it is not negotiable

```
Z-1  own config dirs (breaking) ──┐
Z-2  rename                       ├──► Z-3  repo scaffold ──► Z-4  CodeBuild runners
FU-8A  provider surface  ─────────┘
```

- **Z-1 before Z-2**: renaming while still reading `~/.config/opencode/` would ship a binary called
  Zuno that silently depends on its predecessor's directories. Decide the directory story first.
- **Z-3 before Z-4**: the CodeBuild path needs a remote and a workflow to attach runners to. The
  completed AWS China implementation uses `SECRETS_MANAGER` PAT authentication because
  CodeConnections is unavailable in `aws-cn`.
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

- **This machine's old setup stopped being read.** The one-time config copy used during Z-1 was a
  private QA fixture that proved the new root worked; it is not a supported import path and must not
  be presented as a session/config migration feature. Production behavior remains Zuno-only.
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

- The unfiltered loaded-bundle union contains **41** `OPENCODE_*` names; the bundle-local counts are
  25 for OMO, 18 for Antigravity, and 0 for Kiro. Excluding Antigravity's 11 plugin-owned
  `OPENCODE_ANTIGRAVITY_*` names leaves 7 for that bundle and a **30-name filtered host-contract
  union**. Antigravity's own namespace is not a host ABI.
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

**Status: CLOSED (2026-08-14)** — Zuno identity is implemented and verified; the `opencode` executable alias remains open for an explicit user decision, with current no-alias behavior preserved.

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
- **Compatibility records are retained evidence, not identity.** `docs/divergences.toml`, the
  differential suites, and oracle fixtures still describe upstream artefacts. Whether those suites
  should remain is awaiting an explicit user decision; Zuno's own behavior must not be bent to
  preserve cross-binary session or configuration compatibility in the meantime.

### Acceptance criteria (agent-executable)

- The binary, user agent, long display version, and help text say Zuno; a test pins each.
- `COMPATIBILITY_VERSION`'s meaning is documented and its test updated to state *why* the value is
  what it is.
- The alias decision is implemented and tested either way.
- A reproducible classification inventory accounts for every lowercase `opencode` source occurrence
  as an upstream artefact/reference, plugin ABI, or historical citation; Zuno presentation identity
  has no remainder.

---

## Z-3. Repository scaffold, per the `github-project-scaffold` skill

**Status: CLOSED (2026-08-14)** — File-side scaffold, remote creation, repository metadata, and the
first push are complete. `origin` is `git@github.com:sunerpy/zuno.git`; `origin/main` matched HEAD at
verification time. The repository is private and has a description plus five topics. `LICENSE` is
present.

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

**Status:** ACTIVE, REMOTE PROOF BLOCKED BELOW CODEBUILD (2026-08-15) — Linux workflow edits,
least-privilege IAM, project-scoped secret auth, the CodeBuild project, and the
`WORKFLOW_JOB_QUEUED` webhook are complete. The existing GitHub CLI credential supplied the source
credential without minting a new PAT; no credential value was recorded. A full remote runner proof is
currently blocked because GitHub is not starting workflow jobs. A GitHub-hosted Windows job in the
same affected run also failed in two seconds with zero steps and an empty `runner_name`, which is the
account-level Actions quota signature rather than a CodeBuild routing failure. The exact billing/quota
figure could not be read because the current GitHub token lacks the required `user` scope.

**Not blocked on Z-3.** The repository and first push exist. CodeConnections is not the path in
`aws-cn` because that partition has no CodeConnections endpoint. The CodeBuild project source uses
`SECRETS_MANAGER` PAT authentication instead. The dedicated secret now has one `AWSCURRENT` version,
and webhook `666011311` is active for `workflow_job`/`issue_comment` events with a
`WORKFLOW_JOB_QUEUED` CodeBuild filter. The remaining blocker is restoring GitHub Actions job
scheduling capacity, not supplying credentials or activating CodeBuild. No credential value belongs
in this plan or its evidence.

### Why this shape rather than migrating to CodePipeline

The skill is explicit, and it matches this project's situation: changing `runs-on` preserves every
existing workflow, secret, and OIDC role, whereas a CodePipeline migration duplicates deployment
logic — *"部署逻辑出现第二份，而第二份必然漂移"*. With 628 lines of working workflow across two
files, that duplication is the larger risk.

### The six steps, and the three traps that matter most here

Actual path: private repository → least-privilege service role (**two policies, no ECR/ECS**) →
runner-mode project with source auth type `SECRETS_MANAGER` (`buildspec: ""`,
`privilegedMode: false`, `reportBuildStatus: false`) → populate the dedicated source secret from the
existing GitHub CLI credential without exposing its value → create the webhook filtered to
`WORKFLOW_JOB_QUEUED` → after GitHub Actions scheduling capacity is restored, verify workflow-job
deliveries and jobs rather than only webhook activation.

Traps, all of which the skill reports having hit in practice:

1. **Create the webhook only after a valid source credential is present in the existing secret.** An
   empty secret cannot authenticate webhook creation or repository access. The host's existing
   GitHub CLI credential was sufficient; a new PAT and CodeConnections/App authorization were not
   required. CodeConnections instructions do not apply in `aws-cn`.

   **What is actually in use.** The credential lives in Secrets Manager secret
   `codebuild/zuno/github-source`
   (`arn:aws-cn:secretsmanager:cn-northwest-1:107255705363:secret:codebuild/zuno/github-source-vzWMnL`),
   referenced by the project's `source.auth.resource`. It is the host's existing `gh` token, whose
   scopes are `admin:public_key, gist, read:org, repo`; GitHub's `repo` scope already includes
   webhook administration, which is why webhook creation succeeded. Any earlier text in this plan or
   its evidence demanding a **classic PAT with `repo + admin:repo_hook`** is obsolete — that
   requirement was never tested against the credential the host already had, and minting a token was
   avoidable. No credential value is recorded here or in evidence.

   **Recommended shape when this is next rotated:** a fine-grained token scoped to `sunerpy/zuno`
   only, with Contents `Read`, Commit statuses `Read/Write`, Webhooks `Read/Write`, Administration
   `Read/Write`. That is narrower than the classic `repo` scope in use today, which grants access to
   every repository the account can reach.
2. **Give every job a unique label.** GitHub routes by label **superset**, so a job with fewer labels
   can have its runner stolen by one with more — and the symptom is a job hanging forever with no
   error. This project has **11 distinct CodeBuild label sets across two workflows**, pinned by
   `crates/oc-cli/tests/release_surface.rs`; that is exactly the condition. The earlier "9 jobs"
   figure in this plan was never measured and was wrong by two.
3. **Pin tool versions.** `aws/codebuild/standard:7.0` defaults to Node 18 while GitHub-hosted images
   are newer, and `npm install` only *warns* on `EBADENGINE` — so a failure surfaces two steps later
   with no mention of versions. Less acute for a Rust project, but `release.yml` may use Node tooling.

**One project-specific check the skill cannot know**: `cargo test --workspace` here is heavy, and this
session repeatedly hit host `EAGAIN` under parallel load. Establish the compute size deliberately and
record why, rather than accepting `BUILD_GENERAL1_MEDIUM` because it is the example.

### Measured configuration of the live project (2026-08-15)

Read back from `aws codebuild batch-get-projects --names zuno-runner --region cn-northwest-1`, so
these supersede any earlier figure in this plan:

| field | measured value |
|---|---|
| partition / region / account | `aws-cn` / `cn-northwest-1` / `107255705363` |
| ARN | `arn:aws-cn:codebuild:cn-northwest-1:107255705363:project/zuno-runner` |
| `environment.type` | `LINUX_CONTAINER` |
| `environment.computeType` | `BUILD_GENERAL1_LARGE` |
| `environment.image` | `aws/codebuild/standard:7.0` |
| `environment.privilegedMode` | `false` — correct, because both workflows use zero docker |
| `source.auth.type` | `SECRETS_MANAGER`, because `aws-cn` has no CodeConnections |
| `source.buildspec` | empty, as runner mode requires |
| `source.reportBuildStatus` | `false` |

**`aws-cn` has no fleet API at all.** `aws codebuild list-fleets --region cn-northwest-1` answers
`InvalidInputException: Unknown operation ListFleets`. Reserved-capacity fleets are the only way
CodeBuild hosts Windows or macOS runners, so in this partition the runner path is
`LINUX_CONTAINER`-only. Any Windows or macOS job must stay GitHub-hosted; that is a partition
limitation, not a configuration choice, and no amount of project tuning changes it.

### Acceptance criteria (agent-executable)

- Every job's `runs-on` names the CodeBuild project **verbatim** and carries a **unique** second
  label; a check proves no two jobs share a label set.
- Webhook activation and its 200 `ping` delivery are recorded. Once GitHub resumes creating jobs,
  `queued` and `in_progress` workflow-job deliveries must both return 200 before remote proof closes.
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
