# Enterprise preview

This directory owns the independent enterprise preview channel. The approved
[implementation plan](PLAN.zh.md) covers the shared Zuno kernel, PostgreSQL
runtime state, resumable workers, isolated execution environments, and the Web
and ACP clients.

## Delivery status

The preview starts from Zuno v0.10.29 at
`d1212860dba6a6b420ce81d444ace58feaf0adb5`. Enterprise features are not advertised
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
