# Codex upstream release sync

Zuno is a source-level fork of [openai/codex](https://github.com/openai/codex).
Every stable Codex release (`rust-vX.Y.Z`) is folded into Zuno by replaying the
reviewed Zuno delta onto that exact release, building the six platform packages
once, and promoting those exact bytes to `zuno-vX.Y.Z`. Zuno versions therefore
track Codex versions.

The pipeline has exactly one manual step: merging the candidate pull request.
Everything before and after it is automated.

```text
openai/codex tag rust-vX.Y.Z
        │  scheduled watcher (every 6 h) or manual dispatch
        ▼
zuno-upstream-sync.yml ── replay clean ──▶ branch upstream-sync/X.Y.Z + PR (label upstream-sync)
        │                                          │
        └── replay conflicts ──▶ issue (label       ▼
            upstream-sync-conflict)        zuno-ci.yml: zuno/pr-gate builds, smokes,
            resolve locally, push branch   attests and seals the six packages
                                                   │
                                                   ▼
                                        human review + "Create a merge commit"
                                                   │
                                                   ▼
                                        zuno-release.yml (pull_request: closed)
                                        verifies merge shape, downloads the sealed
                                        candidate, tags zuno-vX.Y.Z, publishes the
                                        preview release without rebuilding
```

## Watcher: `zuno-upstream-sync.yml`

Runs on a six-hour schedule and on `workflow_dispatch` (optional exact `target`
tag, optional `refresh`). Each run:

1. Fetches Codex tags and runs `scripts/zuno_upstream.py check --allow-current`.
   When the newest stable tag equals the recorded baseline in
   `UPSTREAM_CODEX.toml`, the run ends with a summary and no side effects.
2. Looks for an open candidate PR on `upstream-sync/X.Y.Z` or an open conflict
   issue for the tag. If either already records the current `main` commit
   (`Zuno-Source-Commit` trailer), the run is a no-op. This makes the schedule
   idempotent.
3. Otherwise prepares the candidate in an isolated worktree:
   `git worktree add <tmp> rust-vX.Y.Z`, then a three-way apply of
   `git diff <baseline> <main>`, then rewrites the `[baseline]` section of
   `UPSTREAM_CODEX.toml`. `main` is never modified. Derived artifacts
   (`codex-rs/Cargo.lock`, `codex-rs/app-server-protocol/schema/**`,
   `codex-rs/core/config.schema.json`) are never merged as text: a conflict in
   them is reset to the upstream bytes, and after the replay the watcher
   regenerates all three from the merged source (`cargo update --workspace`,
   `write_schema_fixtures.py` with and without `--experimental`, and
   `codex-write-config-schema`) before committing.
4. On a clean replay it force-pushes `upstream-sync/X.Y.Z` (with
   `--force-with-lease`) and opens or refreshes the PR. Whenever `main` moves,
   the next run re-prepares the candidate so the PR always replays the current
   reviewed delta. Older open candidates and any conflict issue for the same tag
   are closed as superseded.
5. On conflicts it opens or updates one issue listing the conflicting paths and
   the local commands to resolve them. Nothing is pushed.

Only one release is tracked at a time: the newest stable tag. Alpha tags are
ignored.

## Resolving conflicts locally

```sh
git fetch --tags --prune upstream
python3 scripts/zuno_upstream.py --no-fetch prepare \
  --source main --target rust-vX.Y.Z \
  --branch upstream-sync/X.Y.Z \
  --worktree ../zuno-upstream-X.Y.Z
# fix conflict markers in ../zuno-upstream-X.Y.Z, then regenerate derived artifacts:
cd ../zuno-upstream-X.Y.Z/codex-rs
cargo update --workspace
python3 app-server-protocol/scripts/write_schema_fixtures.py
python3 app-server-protocol/scripts/write_schema_fixtures.py --experimental
cargo run -p codex-config-schema --bin codex-write-config-schema
cd .. && git add --all && git commit && git push -u origin upstream-sync/X.Y.Z
```

Open the PR from that branch and add the `upstream-sync` label so promotion
recognises it. The watcher closes the conflict issue when a later replay of
`main` becomes clean.

## Promotion: `zuno-release.yml`

Promotion runs automatically when a PR from a same-repository
`upstream-sync/*` branch is merged into `main`. It derives every input from the
merged PR: the head SHA, the version in `codex-rs/Cargo.toml` at that head, the
latest successful `zuno-ci.yml` run for that SHA, and its `zuno-candidate`
artifact. `workflow_dispatch` with explicit inputs remains available for other
releases.

Before anything is published it verifies that:

- the PR is merged into `main` with a two-parent merge commit whose parents are
  exactly the PR base and the certified head (squash and rebase merges are
  rejected because they change the certified bytes);
- the merged tree equals the certified head tree, so the released bytes are
  the ones the PR gate built;
- the head retains the Codex baseline recorded in its `UPSTREAM_CODEX.toml`;
- the sealed `candidate-manifest.json` and every archive match the run,
  attempt, PR, head, parents, tree, and version, and carry valid provenance
  attestations from `zuno-ci.yml` on GitHub-hosted runners.

It then creates the immutable tag `zuno-vX.Y.Z` on the merge commit, uploads
the six archives, `zuno-package_SHA256SUMS`, smoke reports, and the manifest,
re-downloads them to compare bytes, and publishes a non-latest prerelease.

## Manual controls

- Run the watcher immediately: **Actions → Prepare Codex upstream sync → Run
  workflow** (optionally with an exact tag).
- Re-prepare an already open candidate: dispatch with `refresh = true`.
- Pause the automation: disable the two workflows in the Actions UI; nothing
  else needs to change.

## Invariants

- `main` and the active worktree are never mutated by synchronization.
- Candidates are always prepared from a clean `main`; a dirty source aborts.
- The exact upstream tag, commit, and tree are verified before replay. Codex
  cuts each release on its own short branch, so the target normally is a sibling
  of the baseline rather than a descendant; the two must share history, and
  commits that exist only on the old release branch are listed in the PR.
- Nothing is merged automatically. The merge is the review gate.
- Released bytes are never rebuilt after review; promotion only republishes the
  sealed PR-gate artifacts.
