# Enterprise implementation status

Baseline: `v0.10.29`, `d1212860dba6a6b420ce81d444ace58feaf0adb5`.

| Phase | State | Evidence required before completion |
| --- | --- | --- |
| P0 | Complete | PR #176 merged into preview; native CI run 34553054820 succeeded |
| P1 | In progress | Principal propagation implemented; ownership migration validated; local bounded driver validated; application/store interfaces remain |
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
- Current phase branch: `codex/enterprise-p1-driver`.
- The primary checkout and pre-existing worktrees remain owner-controlled.

## Validation

- Preview publisher tests: 11 passed.
- Documentation and existing release-surface contracts: 99 passed.
- `cargo fmt --all --check`: passed.
- `cargo check --workspace --all-targets --offline`: passed.
- `cargo clippy --workspace --all-targets --offline -- -D warnings`: passed.
- Actionlint and README formatting: passed.
- P0 merged commit: `c85c03cfe9871d2c19cd57b1d8cb2223882af28e`.
- P1 SQLite ownership and migration: 426 tests passed.
- P1 engine/type/tool suite: 496 tests passed (one documented doctest ignored).
- P1 English/Chinese documentation contracts: 48 passed.
- P1 shared workspace check, Clippy, formatting and diff checks: passed.
- Ownership PR #177 merged at `b56a2aeeeb32b3891ad0830ea4d129e8fb33432e`; CI run 34557423275 succeeded.
- Bounded driver: 689 engine/LLM/tool tests and 48 documentation contracts passed.
- Bounded driver workspace check, Clippy, formatting and diff checks: passed.
- The driver remains a local SQLite boundary; it is not a remote worker or an environment lease.

No enterprise runtime or preview release has been certified yet. Publication
stays disabled in `preview.json`.
