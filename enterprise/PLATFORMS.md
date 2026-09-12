# Enterprise and personal platform boundaries

This is the user's September 12, 2026 clarification of the approved plan.
Enterprise service deployment targets Linux amd64 and arm64. Personal Zuno keeps
its existing Linux, macOS and Windows support, including TUI and ACP.

| Surface | Runtime/platform evidence |
| --- | --- |
| Enterprise control plane, Agent Worker and execution gateway | Native Linux amd64/arm64 |
| Enterprise Web deployment and ACP bridge service | Linux service deployment |
| Enterprise Web browser client | Browser protocol/UI testing; Windows browsers can access the Linux service |
| Personal TUI, ACP, local HTTP and their shared kernel | Existing personal platform matrix, including Windows |

`zuno-server`'s default feature set contains personal HTTP functionality.
The `enterprise` feature explicitly enables enterprise identity, PostgreSQL,
Worker and environment adapters. The enterprise integration test requires that
feature. Personal builds do not activate it.

Enterprise-only crates declare `package.metadata.zuno.distribution = "enterprise"`.
`scripts/cargo_surface.py` derives the personal workspace selection from those
declarations and checks that the CLI dependency graph contains no enterprise
services. New enterprise binaries/adapters must carry the same declaration.
Shared application/types/engine crates remain in personal tests.

Windows Clippy and test-binary scheduling use this personal selection. An
enterprise-preview PR that changes only explicitly recognized enterprise paths
may skip Windows. Root manifests/lockfiles, shared source, TUI, ACP, general CI
tooling and unknown paths conservatively retain personal Windows regression.
Changing a branch name cannot bypass that classification.

Enterprise PostgreSQL and Docker gates explicitly enable `zuno-server/enterprise`
and run on both Linux architectures. Enterprise release artifacts were already
limited to those two targets; the prerelease workflow now also requests only
the Linux shared gates. PRs with shared changes still receive personal Windows
checks before integration.

The stable release matrix, tags, installers and local data namespaces are not
changed by this boundary. Enterprise functionality remains unavailable through
unimplemented commands; the preview release remains disabled until its runtime
and fault acceptance are complete.

See [中文](PLATFORMS.zh.md), [plan](PLAN.zh.md) and [status](STATUS.md).
