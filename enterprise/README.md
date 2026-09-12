# Enterprise preview

This directory owns the independent enterprise preview channel. The approved
[implementation plan](PLAN.zh.md) covers the shared Zuno kernel, PostgreSQL
runtime state, resumable workers, isolated execution environments, and the Web
and ACP clients.

## Delivery status

The preview originally started from Zuno v0.10.29 and now incorporates v0.10.31 at
`5619205d60aef572e484646dd9ab0563e1ef8066`. Enterprise features are not advertised
until their provider, consumer, entry point, and acceptance tests are present.
See [STATUS.md](STATUS.md) for completed work and remaining gates.

The PostgreSQL persistence adapter and its isolated verification procedure are
documented in [English](POSTGRES.md) and [中文](POSTGRES.zh.md).
The generic OAuth2 adapter and Entra specialization are described in
[English](AUTHENTICATION.md) and [中文](AUTHENTICATION.zh.md). This implements the
2026-09-11 scope amendment: Entra is a provider adapter, not the application API's
authentication abstraction.

## Channel contract

- Integration branch: `codex/enterprise-preview`.
- Feature PRs target that branch, never `main`.
- Tags: `enterprise-vX.Y.Z-preview.N`; the core version is the next patch after
  the incorporated stable baseline.
- Releases are prereleases and never become GitHub's Latest release.
- The preview has its own installation, configuration, database, object, image,
  documentation, and compiler-cache namespaces.
- Released tags and source history are immutable. Incorporate later stable
  fixes through a synchronization PR.

`preview.json` is the release request. `enabled: false` disables publication,
not validation. Enabling it requires a runnable capability set, matching Cargo
versions, the complete target matrix, packaged-binary execution, checksums, and
attestations. A branch name or successful unit test alone is not release proof.

The release job packages a single executable in a GNU-format archive and invokes
`scripts/enterprise_artifact_smoke.py`. The driver verifies the native ELF target,
archive/binary SHA and version, then runs the unpacked binary for the isolated
control plane, gateway and both Workers. The fixture checks Linux process
executable identities and records successful runtime behavior before emitting
evidence. A per-target `.smoke.json` binds that evidence to source SHA, archive
bytes and workflow run. Sealing and promotion require matching evidence and
checksums; release publishes the same attested artifacts without rebuilding.
Preview PR CI also runs this archive driver against each native Linux test binary,
so the amd64/arm64 execution path is exercised before publication is enabled.

预览发布会直接运行归档中解包的二进制，检查 ELF 架构、版本、SHA 和四个独立服务的
实际进程路径。每平台 `.smoke.json` 将运行证据绑定到源码、归档字节和工作流 run；
seal 与 promotion 检查一致性，发布阶段不重新构建，也不依赖 tag 隐式触发其他流程。

## Architecture ownership

The control plane owns authentication, authorization, approvals, durable state,
scheduling and client projections. Agent Workers execute the shared kernel.
The environment gateway owns external operation admission, receipts and Docker
environments. Commands cannot access control-plane credentials or the Docker
socket from inside their execution container.

Local and enterprise profiles share behavior contracts. SQLite remains a local
adapter. Cross-table mutations that assert one fact stay in one transaction.
Model-visible output remains reconstructable from durable state.

## Chinese plan

完整计划、阶段范围和验收标准见 [PLAN.zh.md](PLAN.zh.md)。本目录的实现及发布仅属于
独立预览通道，不更新正式安装、正式数据库或稳定文档站点。

Organization policy and durable approval contracts: [English](AUTHORIZATION.md), [中文](AUTHORIZATION.zh.md).

Rootless environments and operation recovery: [English](ENVIRONMENTS.md), [中文](ENVIRONMENTS.zh.md).

Durable invocation waiting and consumption: [English](WAITING.md), [中文](WAITING.zh.md).

Browser login and BFF session contracts: [English](BROWSER.md), [中文](BROWSER.zh.md).

Authenticated gateway transport: [English](GATEWAY.md), [中文](GATEWAY.zh.md).

Durable operation results and acknowledgement: [English](OPERATION_RESULTS.md), [中文](OPERATION_RESULTS.zh.md).

Enterprise Linux and personal cross-platform boundaries: [English](PLATFORMS.md), [中文](PLATFORMS.zh.md).

Bounded Worker execution and renewal: [English](WORKERS.md), [中文](WORKERS.zh.md).

Enterprise session, Job and approval API: [English](APPLICATION.md), [中文](APPLICATION.zh.md).

Executable roles and deployment templates: [English](DEPLOYMENT.md), [中文](DEPLOYMENT.zh.md).

Scoped private Memory, consent and source validity: [English](MEMORY.md), [中文](MEMORY.zh.md).

Native child admission, pending results and completion: [English](CHILDREN.md), [中文](CHILDREN.zh.md).

Child workspace forks and executable target configuration: [English](WORKSPACES.md), [中文](WORKSPACES.zh.md).

Durable job/tree cancellation and observed process stops: [English](CONTROL.md), [中文](CONTROL.zh.md).

Public activity enums, history and TypeScript SDK: [English](ACTIVITY.md), [中文](ACTIVITY.zh.md).

React session workbench, browser validation and static deployment: [English](WEB.md), [中文](WEB.zh.md).

Durable Workflow coordination, dependency inputs and node approvals: [English](WORKFLOW.md), [中文](WORKFLOW.zh.md).

Preview documentation archives contain the tracked guides, example configurations
and license notices from the exact release commit. Local edits and untracked files
are excluded; these archives do not update the stable documentation site.
