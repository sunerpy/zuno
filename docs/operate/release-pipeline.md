# Release pipeline

Zuno builds release bytes once. A release-please pull request is certified on its
exact head commit, and the resulting archives are promoted after merge only when
the release tag has the same Git tree.

## Workflow ownership

- `ci.yml` is the required pull-request gate for ordinary contributions. It runs
  on standard GitHub-hosted Linux and Windows runners, including untrusted public
  forks with read-only permissions and no repository secrets.
- `release.yml` owns release-please, exact candidate dispatch, release identity,
  and GitHub asset publication. Its candidate-promotion path never installs Rust
  or recompiles a binary; crates.io publication is delegated after the GitHub
  release is public.
- `release-candidate.yml` owns the full test gate and the six release targets.
  Each target builds `zuno` and `zuno-smoke` together, packages and unpacks the
  archive, verifies the exact executable architecture, runs the packaged binary,
  generates provenance, and only then uploads its bytes. Linux, Windows, and
  arm64 macOS execute natively; x86_64 macOS executes through Rosetta 2 on the
  `macos-15` Arm64 runner. Windows x86_64 uses `windows-2022`; Windows ARM64
  uses the standard `windows-11-arm` hosted runner.

GitHub may mark the ordinary `pull_request` workflow for a `GITHUB_TOKEN`-authored
release PR as `action_required` under the repository's native Actions approval
policy. This is an intentional human gate, not a test failure and not something
the release automation may bypass. The repository keeps
`actions/permissions/fork-pr-contributor-approval` at
`all_external_contributors`; it does not add a CI skip marker, switch to
`pull_request_target`, or give release-please a privileged token.

After release-please creates or updates its PR, the controller does not trust the
first mutable PR API response. It waits for the PR base SHA to equal the `main`
commit that started the controller, verifies that the bot-authored release head
has exactly that commit as its sole parent, and confirms the same base/head pair
with a second PR read. A stale API view is retried for a bounded period and is
never dispatched as the expected candidate SHA.

A maintainer reviews the exact head SHA and, when GitHub marks the ordinary `CI`
run as `action_required`, approves that exact run. Once GitHub admits the run,
`ci.yml` deliberately ignores `github.actor`: the initiator may be the
maintainer who approved or retriggered the run, not the identity that authored
the PR. The lightweight route instead requires the release-please bot PR author,
a same-repository head, `main` base, the release-please branch prefix, and the
`autorelease: pending` label. It uses a non-protected check name and skips every
duplicate build job; the exact-head candidate workflow remains the sole owner of
`zuno/pr-gate`. Ordinary and fork PRs still run the complete CI matrix. An
`action_required` run that has not yet been approved is waiting for operator
action and must not be reported as a completed release. Leaving it unattended
until GitHub expires it produces the misleading failed `chore: release ...`
history that this procedure prevents.

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
`RELEASE_CANDIDATE_AUTOMATION=true` lets the controller dispatch and certify the
exact release-PR head. Certification does not imply approval or merge:
`RELEASE_CANDIDATE_AUTO_MERGE=true` is a separate opt-in. When that second switch
is absent or false, a maintainer must revalidate the exact head and merge the
certified PR manually. That user-authored merge push wakes release finalization.
When the second switch is explicitly enabled, the candidate workflow enables
squash auto-merge for the exact head, waits for GitHub to report the merge, and
explicitly dispatches finalization because a `GITHUB_TOKEN` merge does not start
another workflow.

## Candidate identity

The sealed `release-candidate` Actions artifact is retained for seven days and is
addressed only by workflow run ID. It contains six archives, `SHA256SUMS`,
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

After GitHub publication, `.github/workflows/publish-crates.yml` packages the complete
first-party crates.io dependency closure in topological order. Every local dependency
must retain an explicit registry version, every normalized `.crate` manifest must be
free of path dependencies, and an existing version is skipped only when crates.io
reports the exact expected checksum. A checksum mismatch is permanent and stops the
run; a partial publication can be resumed safely with the same tag.

The first crates.io publication is an explicit bootstrap:

1. create the protected `crates-io` GitHub environment and add a crates.io API token as
   `CRATES_IO_TOKEN`;
2. dispatch `publish-crates.yml` for the already-public release tag with
   `auth_mode=bootstrap`;
3. register this repository, `.github/workflows/publish-crates.yml`, and the
   `crates-io` environment as a Trusted Publisher for every published Zuno crate;
4. set the repository variable `CRATES_IO_TRUSTED_PUBLISHING=true`.

Subsequent releases exchange GitHub's OIDC identity for a short-lived crates.io token;
the long-lived bootstrap token is no longer used. Leaving the repository variable
absent or false keeps GitHub Releases functional while crates.io remains deliberately
disabled.

Automatic failure never falls back to recompilation. Recovery is explicit:

1. dispatch `release-candidate.yml` with `mode=backfill` on the exact release tag;
2. record the successful run ID;
3. dispatch `release.yml` with `mode=promote`, that run ID, the merged release PR,
   its candidate source SHA, and the existing tag.

For crates.io-only recovery, rerun `publish-crates.yml` against the same public tag.
The immutable version/checksum check resumes missing packages and refuses to overwrite
different bytes.

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
