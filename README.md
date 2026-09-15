# Zuno

Zuno is a local, plugin-oriented agent runtime built as a full-source fork of
[OpenAI Codex](https://github.com/openai/codex). It keeps Codex's Rust runtime,
thread state, approvals, sandboxing, model providers, TUI, and App Server while
adding native ACP, user-owned workflow engines, and a typed Agent-backend
factory boundary.
The project and executable are named **Zuno**; Codex remains the upstream source
baseline.

> Zuno next is under active development on the `zuno-next` line. Do not replace
> an installed legacy Zuno binary until the candidate has passed the platform,
> migration, ACP, workflow, and Provider acceptance gates.

## Design rules

- Workflows are user/project/plugin resources using `zuno.workflow/v1`; no
  product-specific workflow or model routing is built into the application.
- Native Codex subagents share the in-process thread runtime; Claude Code and
  ACP are bounded external backends. Enabled plugins may declaratively mount
  namespaced backend factories, while execution profiles retain model and
  policy ownership.
- ACP is a client projection over the same Codex App Server state, not another
  agent loop.
- Model/provider/profile selection remains configuration. Workflows use logical
  routes instead of embedding credentials or mandatory model IDs.
- Upstream Codex updates are replayed into an isolated candidate and never
  merged automatically.

See [Zuno next architecture](ZUNO_ARCHITECTURE.md),
[plugin-owned Agent backends](docs/zuno-plugin-agent-backends.md),
[model profile examples](examples/zuno-config/README.md),
[optional workflow templates](examples/zuno-workflows/README.md),
[`UPSTREAM_CODEX.toml`](UPSTREAM_CODEX.toml), and
[`FORK_DELTA.toml`](FORK_DELTA.toml) for the exact boundary and baseline.

## Build from source

The Rust workspace is under `codex-rs`. Use the pinned toolchain and the
repository's `just` commands:

```sh
rustup toolchain install 1.95.0
cd codex-rs
rustup run 1.95.0 cargo build -p codex-cli --bin zuno
target/debug/zuno --help
```

The final command path may also be exercised directly from the workspace:

```sh
rustup run 1.95.0 cargo run -p codex-cli --bin zuno -- --help
```

Create a local platform package with the Zuno entrypoint and Codex-derived
companion binaries:

```sh
just assemble-zuno-package --target x86_64-unknown-linux-gnu
```

Release automation can build `//codex-rs/cli:zuno_release_binaries` (or run
`just build-zuno-for-release`) without replacing the upstream Codex
`release_binaries` compatibility target. The package currently retains internal
layout names such as `codex-package.json` and `codex-resources`; these are stable
runtime contracts, while the archive and executable are named `zuno-package-*`
and `zuno`.

## Upstream update candidate

```sh
# Fetch and report the newest stable Codex release without changing this branch.
python3 scripts/zuno_upstream.py --json check

# After the current Zuno delta is reviewed and committed, apply its exact tree
# delta to a new Codex tag in an isolated candidate worktree. Conflicts stay
# there for review; legacy-main bridge ancestry is never replayed.
python3 scripts/zuno_upstream.py prepare \
  --target rust-vX.Y.Z \
  --worktree ../zuno-upstream-X.Y.Z
```

Maintainers can run **Prepare Codex upstream sync** in GitHub Actions to create a
candidate branch and pull request. The workflow never merges the candidate.

Zuno pull requests use the repository-owned `zuno/pr-gate`; OpenAI-specific
Codex CI remains manual because it depends on upstream private runners and
publishing credentials. The PR gate builds each of six platform packages once,
runs native ACP/layout smoke, emits provenance attestations, and seals the exact
bytes to the PR head and tree. After a merge-method-only cutover preserves both
histories, the promotion workflow accepts an explicit candidate run ID, verifies
tree equality, creates `zuno-vX.Y.Z`, rechecks downloaded release bytes, and
publishes a non-latest preview without rebuilding. The upstream `rust-v*`
release workflow is retained only as a manual compatibility reference and never
publishes a Zuno product release.

During this first source-migration release, `zuno update` deliberately fails
closed with the Zuno Releases URL. It never invokes the inherited Codex npm,
Homebrew, or chatgpt.com installers. The compatibility `zuno app` command,
including `--download-url`, fails before it inspects a workspace path or reaches
the inherited Codex Desktop launcher. Automatic replacement stays disabled
until a Zuno-owned atomic installer passes the same six native platform gates.
The persistent managed app-server daemon and its hidden updater loop are also
disabled for this preview; Start, Restart, and Stop fail before daemon state is
read or changed, while Version stays read-only. Foreground App Server, ACP, and
remote-control transports remain available without installing or executing a
Codex package. TUI startup tips are bundled locally and never fetch inherited
announcements or advertise the Codex Desktop app.

## Codex attribution and license

Zuno contains and modifies Codex source. Preserve OpenAI and third-party notices
when redistributing builds. This repository remains licensed under the
[Apache-2.0 License](LICENSE); see [NOTICE](NOTICE) for attribution.

Upstream Codex build and contributor documentation remains available in
[`docs/`](docs/) and at <https://developers.openai.com/codex>.
