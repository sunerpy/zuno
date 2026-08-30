# Agent Client Protocol upstream snapshot

This directory is Zuno's auditable, vendored snapshot of the official Agent
Client Protocol (ACP) release assets. The machine-readable source of truth is
[`manifest.json`](manifest.json); [`SHA256SUMS`](SHA256SUMS) covers the copied
Apache-2.0 license and every release asset.

The snapshot fetched on 2026-08-26 pins:

| Track | Tag | Annotated tag object | Peeled commit |
| --- | --- | --- | --- |
| Stable schema | `schema-v1.21.0` | `fe2db5aa7c7f5565424515075c00a66f8f6715d8` | `272bf799f35a258c6a4107a0410ed361e83683d3` |
| Rust schema crate | `v1.7.0` | `d46e61d959c356e86468ee7bf87544cfa0933b3a` | `272bf799f35a258c6a4107a0410ed361e83683d3` |
| V2 preview schema | `schema-v2.0.0-alpha.3` | `0e76bb95191f54961301464dc93cf8d4d53071ec` | `272bf799f35a258c6a4107a0410ed361e83683d3` |

The official sources are:

- [agentclientprotocol/agent-client-protocol](https://github.com/agentclientprotocol/agent-client-protocol)
- [ACP V1 protocol documentation](https://agentclientprotocol.com/protocol/v1/overview)
- [Zed external agents documentation](https://zed.dev/docs/ai/external-agents)

The Zed observation is pinned to commit
[`ac099b4a809a564f06907125e7a536c33cb60084`](https://github.com/zed-industries/zed/commit/ac099b4a809a564f06907125e7a536c33cb60084).
At that commit Zed initializes an ACP connection with
[`ProtocolVersion::V1`](https://github.com/zed-industries/zed/blob/ac099b4a809a564f06907125e7a536c33cb60084/crates/agent_servers/src/acp.rs#L993).
This repository stores only that source reference and observation. It does not
copy Zed's GPL-licensed source.

## Snapshot contents

- `assets/stable/`: the four official `schema-v1.21.0` release assets.
- `assets/v2-preview/`: the four official
  `schema-v2.0.0-alpha.3` release assets.
- `LICENSE`: the ACP repository's Apache-2.0 license at the peeled release
  commit.
- `manifest.json`: repository URLs, documentation URLs, tag objects, commits,
  release asset IDs, sizes, source URLs, SHA-256 values, fetch time, and the
  pinned Zed observation.
- `SHA256SUMS`: a platform-neutral checksum list for all copied upstream files.

## Verify or refresh

Linux and macOS require `curl`, `jq`, and standard POSIX utilities:

```sh
./scripts/update-acp-spec.sh --verify
./scripts/update-acp-spec.sh --check-upstream
./scripts/update-acp-spec.sh --refresh
```

Windows uses PowerShell 7 or later:

```powershell
pwsh -File scripts/update-acp-spec.ps1 -Mode Verify
pwsh -File scripts/update-acp-spec.ps1 -Mode CheckUpstream
pwsh -File scripts/update-acp-spec.ps1 -Mode Refresh
```

`Verify` is offline. `CheckUpstream` resolves the checked-in annotated tags,
downloads the pinned release assets again, validates GitHub's release digests,
and compares the result without modifying the checkout. `Refresh` performs all
checks in a temporary directory and updates the snapshot only after every check
passes. Set `GITHUB_TOKEN` only when a higher GitHub API rate limit is needed.

To move to reviewed upstream versions, pass explicit pins to `Refresh`; the
scripts never follow a floating latest release:

```sh
ACP_STABLE_TAG=schema-v1.x.y \
ACP_CRATE_TAG=v1.x.y \
ACP_PREVIEW_TAG=schema-v2.0.0-alpha.n \
ZED_COMMIT=<40-character-commit> \
./scripts/update-acp-spec.sh --refresh
```

```powershell
$env:ACP_STABLE_TAG = "schema-v1.x.y"
$env:ACP_CRATE_TAG = "v1.x.y"
$env:ACP_PREVIEW_TAG = "schema-v2.0.0-alpha.n"
$env:ZED_COMMIT = "<40-character-commit>"
pwsh -File scripts/update-acp-spec.ps1 -Mode Refresh
```

Review the resulting manifest and schema diff, update
[`docs/design/zed-acp-integration.md`](../../design/zed-acp-integration.md) if
the protocol boundary changed, then run both offline verification commands.
