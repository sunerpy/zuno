# Enterprise implementation status

Baseline: `v0.10.29`, `d1212860dba6a6b420ce81d444ace58feaf0adb5`.

| Phase | State | Evidence required before completion |
| --- | --- | --- |
| P0 | Hosted validation pending | Plan and channel implemented; native preview PR CI still required |
| P1 | Pending | Scoped application/storage interfaces, bounded driver, SQLite behavior |
| P2 | Pending | PostgreSQL, identity/approval, leases/checkpoints/completion |
| P3 | Pending | Two workers, Docker gateway, root-task takeover |
| P4 | Pending | Durable child/Workflow/Council waits, merges and cancellation |
| P5 | Pending | Web, SDK, remote ACP, common projections |
| P6 | Pending | Fault injection, native artifacts, preview publication |

Publication remains disabled until the first runnable root-task preview has
completed its acceptance gates. This file records actual completion, not planned
capabilities.

## Workspace

- Integration branch: `codex/enterprise-preview`.
- Current phase branch: `codex/enterprise-p0-foundation`.
- The primary checkout and pre-existing worktrees remain owner-controlled.

## Validation

- Preview publisher tests: 11 passed.
- Documentation and existing release-surface contracts: 99 passed.
- `cargo fmt --all --check`: passed.
- `cargo check --workspace --all-targets --offline`: passed.
- `cargo clippy --workspace --all-targets --offline -- -D warnings`: passed.
- Actionlint and README formatting: passed.

No enterprise runtime or preview release has been certified yet. Publication
stays disabled in `preview.json`.
