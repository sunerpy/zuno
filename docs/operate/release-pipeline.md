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
  or recompiles a binary.
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

release-please reads the squash commit of every merged pull request with a
conventional-commits parser, and one unparsable commit is skipped silently: the
release run still succeeds, logs `commit could not be parsed` and `Considering:
0 commits`, and opens no release PR. A squash merge without an explicit body
lets GitHub concatenate every branch commit into the commit body, where a
footer-shaped line with an unbalanced parenthesis is enough to abort the parse
(the 51-commit batch-3 merge produced an 1,891-line body and no 0.10.1 PR). Merge
a multi-commit pull request with `gh pr merge --squash --subject '<conventional
subject>' --body '<short body>'`, or carry the intended message in a
`BEGIN_COMMIT_OVERRIDE` … `END_COMMIT_OVERRIDE` block, and after every merge to
`main` confirm that a `chore: release X.Y.Z` pull request appeared. When a merge
was already skipped, land a small follow-up pull request whose squash body
carries the override block for the missed change; release-please then records it.

If a documentation-only or otherwise non-releasable commit advances `main`,
release-please may leave the existing release PR head on its older parent. After
observing that stable stale identity repeatedly, the controller fetches the
same-repository release branch, independently rechecks its single parent, bot
author, and `chore: release` subject, and verifies that the old parent is an
ancestor of the triggering `main`. It then cherry-picks that one release commit
onto the exact triggering SHA in a temporary worktree and updates the existing
branch with a lease bound to the previously observed head. The refreshed head is
again confirmed through the PR API before candidate dispatch. A conflict,
identity mismatch, non-ancestor, or lease failure stops the controller without
changing the PR branch. The workflow never creates a two-parent “update branch”
merge and never closes and recreates the PR as routine recovery.

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

Installing that backend is not the same as proving it confines anything, so both
Linux gate jobs then run `make test-sandbox-e2e`. The target builds the `zuno`
executable the sandbox needs as its helper and runs the boundary test, which
executes a real process under `bwrap` and requires that the workspace write
succeeds while writes to `.git`, `.zuno`, `.agents`, an outside directory, and a
symlink pointing out of the workspace all fail, that the effective capability set
and `NoNewPrivs` are as declared, that `AF_INET` sockets, `ptrace`, and
`process_vm_readv` return `EPERM` while `AF_UNIX` socketpair IPC still works, and
that a `read-only` policy refuses the same write.

The test reports a named skip when the host has no bubblewrap or no helper
executable, which keeps it harmless in `make test` on a developer machine. Both
gate jobs set `ZUNO_SANDBOX_E2E_REQUIRE=1`, which turns that skip into a failure.
Run the same gate locally with `make test-sandbox-e2e`; `make pre-ci` includes it.

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
