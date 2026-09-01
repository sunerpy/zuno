# Release pipeline

Zuno builds release bytes once. A release-please pull request is certified on its
exact head commit, and the resulting archives are promoted after merge only when
the release tag has the same Git tree.

## Workflow ownership

- `ci.yml` is the required pull-request gate for ordinary contributions. It runs
  on standard GitHub-hosted Linux and Windows runners, including untrusted public
  forks with read-only permissions and no repository secrets.
- `release.yml` owns release-please, exact candidate dispatch, release identity,
  and publication. It never installs Rust or compiles a binary.
- `release-candidate.yml` owns the full test gate and the five release targets.
  Each target builds `zuno` and `zuno-smoke` together, packages and unpacks the
  archive, verifies the exact executable architecture, runs the packaged binary,
  generates provenance, and only then uploads its bytes. Linux, Windows, and
  arm64 macOS execute natively; x86_64 macOS executes through Rosetta 2 on the
  `macos-15` Arm64 runner.

The Linux source gate installs pinned `cargo-nextest`. Linux Clippy and tests
share one job-local target directory; native Windows Clippy and tests are
independent jobs so they start in parallel instead of placing a global serial
barrier before test execution. Windows uses `scripts/test-parallel.sh`: Cargo
compiles the test surface once, then a bounded worker pool runs test binaries
concurrently rather than paying for one Windows process per test case. Hosted
Windows runs four binaries at a time and one test at a time inside each binary.
The `startup` wall-clock benchmark runs once before that pool so unrelated
processes cannot invalidate its budget; all functional suites, including ACP
and ConPTY lifecycle coverage, remain concurrent with no serial tail. Every suite has a timeout;
timeout cleanup terminates the complete descendant tree, and the scheduler emits
progress while it runs. The scheduler captures Cargo's environment through a
native Python runner and JSON, not Git Bash's text `env` format, so Windows
`PATH` and other process variables keep their native representation. Python
discovery executes a real import, resolves the validated interpreter to an
absolute path, and uses a whitespace-free Windows short path for Cargo's runner
variable; a Windows Store application alias cannot masquerade as Python.

Native fixtures follow the same platform boundary. PTY API coverage uses
`COMSPEC` on Windows instead of assuming `sh`; LSP fixtures execute a validated
absolute Python interpreter; ancestor-walk tests use a unique marker so a
developer's real home directory cannot change the expected result.

Windows keeps Cargo's built-in `test` profile and standard `target/debug`
layout, but overrides that profile's debug and split-debug fields in the
workflow so roughly two hundred short-lived test binaries do not emit or link
debug databases. Panic messages still carry their source location, while
developer and Linux tests retain line-table backtraces. Doctests run once in the
Linux source gate; the Windows job owns native executable behavior and
explicitly sets `RUN_DOCTESTS=0` instead of repeating a platform-independent
rustdoc phase that added more than eight minutes. A failed Windows run uploads
Cargo timings, build/capture logs, and per-suite logs for diagnosis.

The shipped MSVC `zuno.exe` reserves an 8 MiB main-thread stack through a
binary-scoped build-script linker argument. Native `dumpbin` evidence showed the
1 MiB PE default overflowed during real session construction. The argument is
not placed in global `RUSTFLAGS`, so libraries and roughly two hundred test
binaries retain their normal cache identity.

Both workflows use the pinned official sccache action and its GitHub Actions
backend. `CARGO_INCREMENTAL=0` avoids CI-only incremental state, while Cargo
registry and Git downloads use a platform-scoped cache. Ordinary CI, candidate
tests, Linux release targets, and Windows release targets set
`cache-targets: false`, so they do not upload large `target/` trees. The two
macOS candidate legs alone enable the Rust dependency target cache. Their cache
key includes the exact Rust target, and `cache-workspace-crates: false` keeps
workspace outputs out of the cache, so `x86_64-apple-darwin` and
`aarch64-apple-darwin` cannot restore each other's target artifacts.

Artifact transfer uses commit-pinned `actions/upload-artifact` v7.0.1 and
`actions/download-artifact` v8.0.1, whose action runtimes are Node 24. The Linux
musl legs do not use a Node-based Zig setup action. Instead,
`.github/scripts/install-zig.sh` selects the official Zig 0.13.0 archive for the
Linux runner's x86_64 or aarch64 architecture and checks its hard-coded official
SHA-256 before extraction.

Linux jobs install the distribution `bwrap-userns-restrict` AppArmor profile and
prove both the user/mount/PID namespace path and the network namespace path before
running Zuno. They do not disable Ubuntu's host-wide unprivileged-user-namespace
restriction. See the [sandbox FAQ](../faq.md) for the deployment rationale.

Before dispatching CI, Linux contributors run `make pre-ci`. It executes the host
source gates, builds and smokes the packaged host archive, and uses Zig to Clippy
the complete workspace for `x86_64-pc-windows-gnu`, then links every Windows GNU
test binary through `cargo-zigbuild` without executing it. The cross pass catches Windows-only
conditional-compilation and link failures locally; it cannot prove MSVC,
ConPTY, Windows Job Object, or hosted-runner loopback behavior, so native
Windows CI remains authoritative.

Both macOS release targets remain exact Rust triples. The
`x86_64-apple-darwin` leg cross-builds on the `macos-15` Arm64 runner, verifies
both `zuno` and `zuno-smoke` with `lipo`, and runs the x86_64 smoke driver with
`arch -x86_64`, which exercises the packaged x86_64 binary through Rosetta 2.
The `aarch64-apple-darwin` leg verifies and executes with `arch -arm64`. A
translation, architecture, or smoke failure blocks attestation and upload; the
optimization never replaces execution with a static inspection.

The repository ruleset requires the `zuno/pr-gate` check with strict base-branch
freshness. The candidate workflow refuses to merge when that rule is absent.
After certification it enables squash auto-merge for the exact PR head, waits
for GitHub to report the merge, and explicitly wakes release finalization.
`RELEASE_CANDIDATE_AUTOMATION=true` is the rollout switch; without it the
controller may update the release PR but cannot dispatch or merge automatically.

## Candidate identity

The sealed `release-candidate` Actions artifact is retained for seven days and is
addressed only by workflow run ID. It contains five archives, `SHA256SUMS`,
per-target evidence, and `candidate-manifest.json`. The manifest binds:

- repository, signer workflow ref and workflow commit;
- run ID and attempt;
- release PR, source commit, release-PR head, and Git tree;
- version and expected tag;
- archive name, byte size, SHA-256, build/smoke conclusions, runner, and
  attestation identity for every target.

There is no “latest artifact” lookup. Promotion verifies the workflow path,
event, conclusion, source SHA, PR merge state, manifest fields, exact target set,
sizes, checksums, and GitHub provenance. The tag commit may differ from a
squashed PR head, but its Git tree must be byte-identical.

## Publication and recovery

release-please creates the tag and draft release. Promotion uploads assets while
the release remains draft and makes it public only after the complete asset set
has been re-read and verified. A mismatch leaves the draft unpublished.

Automatic failure never falls back to recompilation. Recovery is explicit:

1. dispatch `release-candidate.yml` with `mode=backfill` on the exact release tag;
2. record the successful run ID;
3. dispatch `release.yml` with `mode=promote`, that run ID, the merged release PR,
   its candidate source SHA, and the existing tag.

This keeps the normal path singular while allowing an operator to recover from
expired artifacts or an interrupted finalization without weakening identity
checks.

## Timing evidence

Measure from release-PR creation to public release publication, including runner
queueing. A release change is not complete until three consecutive end-to-end
runs finish within 20 minutes, publication itself finishes within three minutes,
and downloaded release assets pass checksum, provenance, and black-box smoke
verification. The timing objective does not relax per-target execution,
candidate-byte identity, or publication checks.
