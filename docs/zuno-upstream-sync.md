# Codex upstream release sync

Zuno is a source-level fork of [openai/codex](https://github.com/openai/codex).
Every stable Codex release (`rust-vX.Y.Z`) is folded into Zuno by replaying the
reviewed Zuno delta onto that exact release, building the six platform packages
once, and promoting those exact bytes to `zuno-vX.Y.Z`. Zuno versions therefore
track Codex versions, and releases are replayed in order so every Codex release
becomes a Zuno release.

The pipeline has one review gate: merging the candidate pull request. With
`[sync].automatic_merge = true` in `UPSTREAM_CODEX.toml` (the default) a
candidate whose replay was fully automatic is queued for GitHub auto-merge and
merges as soon as the PR gate passes, so a clean Codex release becomes a Zuno
release without a person (see *Hands-off mode* below for the repository secret
and the two settings this needs). A candidate that needed manual conflict
resolution waits for a person. Everything before and after the merge is
automated.

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
                                    review gate: hands-off auto-merge (fully automatic
                                    replay, [sync].automatic_merge) or a person's
                                    "Create a merge commit"
                                                   │
                                                   ▼
                                        zuno-release.yml (pull_request: closed)
                                        verifies merge shape, downloads the sealed
                                        candidate, tags zuno-vX.Y.Z, publishes the
                                        preview release without rebuilding
```

## Watcher: `zuno-upstream-sync.yml`

Runs on a six-hour schedule and on `workflow_dispatch` (optional exact `target`
tag, optional `refresh`, optional `policy`). Each run:

1. Fetches Codex tags and runs `scripts/zuno_upstream.py check --allow-current`.
   With the default policy `next` the target is the oldest stable tag newer
   than the baseline recorded in `UPSTREAM_CODEX.toml`, so releases are synced
   one by one in order; `policy: newest` jumps to the latest tag instead. An
   open candidate for a newer release (for example one resolved by hand) is
   never regressed: the run continues with that release. When no stable tag is
   newer than the baseline, the run ends with a summary and no side effects.
2. Looks for an open candidate PR on `upstream-sync/X.Y.Z` or an open conflict
   issue for the tag. If either already records the current `main` commit
   (`Zuno-Source-Commit` trailer), the run is a no-op. This makes the schedule
   idempotent.
3. Otherwise prepares the candidate in an isolated worktree:
   `git worktree add <tmp> rust-vX.Y.Z`, then `git merge-tree --merge-base=<baseline>
   rust-vX.Y.Z main` (ort merge with rename detection; needs git 2.40+) checked
   out into that worktree, then rewrites the `[baseline]` section of
   `UPSTREAM_CODEX.toml`. `main` is never modified. Derived artifacts
   (`codex-rs/Cargo.lock`, `codex-rs/app-server-protocol/schema/**`,
   `codex-rs/core/config.schema.json`) are never merged as text: a conflict in
   them is reset to the upstream bytes, and after the replay the watcher
   regenerates all three from the merged source (`cargo update --workspace`,
   `write_schema_fixtures.py` with and without `--experimental`, and
   `codex-write-config-schema`) before committing. The commit is created by
   `scripts/zuno_upstream.py finalize`: a merge whose **first parent is the
   `main` commit** and whose **second parent is the exact release commit**. The
   candidate therefore contains `main`, the PR diff is exactly the upstream
   change, the GitHub merge is trivial (so the promoted tree equals the certified
   head tree), and the release commit stays reachable for the next sync.
4. Conflicts are merged with `diff3` markers and then run through the **rebrand
   replay** (see below), which resolves every hunk that exists only because Zuno
   renamed Codex text. If a finalized candidate for the same release already
   exists on `origin`, every post-merge edit it carries (conflict resolutions,
   adaptations to changed upstream APIs in files that merged cleanly, deleted
   orphans) is **reused** for every path that `main` has not changed since, so
   a moving `main` does not undo work a reviewer already did.
5. On a clean merge it force-pushes `upstream-sync/X.Y.Z` (with
   `--force-with-lease`) and opens or refreshes the PR. Whenever `main` moves,
   the next run re-prepares the candidate so the PR always replays the current
   reviewed delta. The PR body lists what the rebrand replay resolved,
   refreshed, deleted, and reused. Older open candidates and conflict issues
   (for this tag and for older tags) are closed as superseded.
6. On remaining conflicts it opens or updates one issue listing the conflicting
   paths, the replay summary, and the local commands to resolve them. Nothing is
   pushed.

One candidate is open at a time. Alpha tags are ignored.

## Hands-off mode

`automatic_merge = true` under `[sync]` in `UPSTREAM_CODEX.toml` (the default)
lets fully automatic candidates merge without a human: after opening or
refreshing the PR the watcher runs `gh pr merge --auto --merge`, GitHub merges
it with a merge commit as soon as `zuno/pr-gate` succeeds, and
`zuno-release.yml` promotes the sealed bytes. "Fully automatic" means the
replay needed no manual conflict resolution and copied nothing from a
hand-resolved candidate (`--reuse`); such candidates, and every candidate when
the flag is `false`, wait for a manual merge. Every refresh first withdraws any
auto-merge queued by an earlier run and re-decides, so a candidate that stops
being fully automatic (or a flag flipped to `false`) never rides a stale queue
into `main`; a replay that hits conflicts also withdraws it. The queue is tied
to the exact head the watcher pushed (recorded as `Zuno-Candidate-Commit` in the
PR body): GitHub keeps auto-merge when a collaborator pushes to the branch, so
every run, including one that otherwise skips, withdraws the queue as soon as
the PR head is not that commit and says so on the PR. The watcher only queues
while the `main` ruleset still requires the `zuno/pr-gate` check, because
without a pending required check `--auto` would merge at once, and only when
the `ZUNO_UPSTREAM_SYNC_TOKEN` secret is configured: a merge that GitHub
completes for a queue raised with the default `GITHUB_TOKEN` fires a
`pull_request: closed` event that starts no workflow, so `zuno-release.yml`
would never promote it and the release would silently not happen. Three
repository decisions gate the hands-off path and belong to the owner: the
secret (see *Repository secret for a fully automatic gate*), **Allow
auto-merge** enabled, and a `main` ruleset that requires no human review (a
required code-owner review makes auto-merge wait for that approval). When any
of them is missing, or GitHub refuses to queue the merge, the watcher leaves a
comment on the PR and the candidate waits for a manual merge.

Delete a candidate branch you abandon (`git push origin --delete
upstream-sync/X.Y.Z` after closing its PR): the watcher treats every unmerged
`upstream-sync/*` branch on `origin` as an open candidate and will keep
preferring it over older releases.

## Rebrand replay: `FORK_REBRAND.toml`

Most of the Zuno delta is the rename of user-visible Codex text (product name,
`ZUNO_HOME`, `~/.zuno`, command examples, repository links). Every upstream
release touches some of those lines, which used to surface as dozens of trivial
conflicts per sync. `FORK_REBRAND.toml` records that rename as ordered rules
(literal or regex substitutions, a protect list for identities Zuno keeps such as
`Codex Desktop` or `Codex Apps`, and a path-scoped `${version}` rule for the TUI
status snapshots that upstream renders as `v0.0.0`). `prepare` applies them
with one safety predicate:

> A hunk is resolved by automation only when applying the rules to its Codex
> baseline text reproduces the Zuno text **byte for byte**. The resolution is
> then the same rules applied to the new upstream text.

Concretely, per conflicted path:

- **Content conflict** (`diff3` hunks with the baseline in the middle): each
  rename-only hunk is replaced by the rebranded upstream text; a hunk the rules
  cannot reproduce keeps its markers. A file is reported as *resolved* only
  when no marker remains, otherwise as *partial*.
- **Modify/delete** (upstream deleted a file Zuno only renamed): the file is
  deleted with upstream.
- **Workspace version** (`codex-rs/Cargo.toml`): a hunk consisting solely of the
  `version = "…"` line takes the release version, because Zuno versions track
  Codex versions.

Rename-only files that merged cleanly are also **refreshed**: if the upstream
release added new wording to a file Zuno had already rebranded, the new wording
is rebranded too, and the `${version}` placeholder is moved to the new release.
Files upstream **adds** inside an `[[added]]` scope of the manifest (today: TUI
`.snap` files) are rebranded without a predicate, because they are rendered
text whose source already says Zuno. Note that `zuno/pr-gate` runs only a few
targeted TUI tests, so a mis-rebranded snapshot surfaces in the full
`cargo test -p codex-tui` run (see *Resolving conflicts locally*), not in the
gate. Source code is never renamed: in a `.rs` file the lines upstream added
(which no predicate has vouched for) are rebranded only inside string literals
and `//` comments, so a new bare `Codex` type or `CODEX_HOME` constant is left
as upstream wrote it instead of becoming code the gate might not compile; such
files are listed as `guarded` in the PR. Lines that already existed in the
baseline are rebranded in full, exactly as the predicate proved. Every other
file outside the Zuno
delta is never rewritten; paths whose new upstream text the rules would change
are listed as `drift` in the JSON report for review. A rule that did reach an
identifier inside a refreshed `.rs` file would fail to compile and be caught by
the gate.

Rules never run without the predicate, so an incomplete rule set costs coverage,
not correctness. Measure coverage with

```sh
python3 scripts/zuno_rebrand.py audit --baseline rust-vX.Y.Z --source main \
  --version "$(sed -n 's/^version = "\(.*\)"$/\1/p' codex-rs/Cargo.toml | head -n1)" --list-diverged
```

which lists the Zuno-changed files the rules do not reproduce. When a rebrand
decision is made consistently across the tree, add it to `FORK_REBRAND.toml` so
the next sync resolves it. `scripts/test_zuno_rebrand.py` pins the rule
semantics; `prepare --no-rebrand` keeps every conflict for manual work.

## Resolving conflicts locally

```sh
git fetch --tags --prune upstream
# Add `--reuse origin/upstream-sync/X.Y.Z` when a finalized candidate for this
# release already exists: its post-merge edits (resolutions, adaptations,
# deletions) are copied for every path main has not changed since.
python3 scripts/zuno_upstream.py --no-fetch prepare \
  --source main --target rust-vX.Y.Z \
  --branch upstream-sync/X.Y.Z \
  --worktree ../zuno-upstream-X.Y.Z
# Everything below runs from the repository root. Only the paths the command
# lists still carry markers (diff3 style: upstream, then the Codex baseline,
# then Zuno); rename-only hunks were already replayed from FORK_REBRAND.toml
# and modify/delete pairs that were rename-only were deleted. Other
# modify/delete pairs keep the Zuno version. UPSTREAM_CODEX.toml already
# records the new release. Fix the markers in ../zuno-upstream-X.Y.Z, then
# regenerate derived artifacts:
(cd ../zuno-upstream-X.Y.Z/codex-rs \
  && cargo update --workspace \
  && python3 app-server-protocol/scripts/write_schema_fixtures.py \
  && python3 app-server-protocol/scripts/write_schema_fixtures.py --experimental \
  && cargo run -p codex-config-schema --bin codex-write-config-schema)
# finalize refuses leftover conflict markers, uses the main commit recorded by
# prepare (a different --source is rejected; re-run prepare if main moved), and
# commits the merge with the Zuno-Source-Commit / Zuno-Upstream-* trailers:
python3 scripts/zuno_upstream.py finalize --worktree ../zuno-upstream-X.Y.Z
git -C ../zuno-upstream-X.Y.Z push -u origin upstream-sync/X.Y.Z
```

Before finalizing, run the TUI suite (`cargo test -p codex-tui`), which the PR
gate does not run in full: upstream text that the rules did not reach (see the
drift list in the conflict issue) shows up there as snapshot mismatches, and a
rebranded word that is one character shorter re-wraps lines and shifts cursor
columns in rendered snapshots. Review the pending changes, accept the ones that
are only rename, width, or version effects (`INSTA_UPDATE=always` for `.snap`
files; edit inline `@"…"` snapshots by hand), and rebrand the source of any new
user-visible wording rather than the snapshot alone.

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
- a finalized upstream-sync head is itself a merge whose first parent is the PR
  base and whose second parent is the release commit recorded in its
  `UPSTREAM_CODEX.toml`, with the recorded release tree;
- the merged tree equals the certified head tree, so the released bytes are
  the ones the PR gate built;
- the head retains the Codex baseline recorded in its `UPSTREAM_CODEX.toml`;
- the sealed `candidate-manifest.json` and every archive match the run,
  attempt, PR, head, parents, tree, and version, and carry valid provenance
  attestations from `zuno-ci.yml` on GitHub-hosted runners.

It then creates the immutable tag `zuno-vX.Y.Z` on the merge commit, uploads
the six archives, `zuno-package_SHA256SUMS`, smoke reports, and the manifest,
re-downloads them to compare bytes, and publishes a non-latest prerelease.

## Repository secret for a fully automatic gate

Events raised by the default `GITHUB_TOKEN` start no workflows: a pull request
it opens does not run `zuno/pr-gate`, and a merge GitHub completes for an
auto-merge it queued does not run `zuno-release.yml`. Create a fine-grained
personal access token scoped to this repository with **Contents: read and
write** and **Pull requests: read and write** (no issues permission is needed;
issues are always handled with the default token), and store it as the
repository secret `ZUNO_UPSTREAM_SYNC_TOKEN`. The watcher uses it only for the
push and PR steps. Without the secret the candidate PR still opens, the watcher
leaves a comment, auto-merge is not queued, and closing and reopening the PR
(or pushing to the branch) starts the gate by hand; the merge and the promotion
dispatch then stay manual.

The token is exposed to steps that run after the candidate tree, including the
upstream release's build scripts, has been executed on the same runner (the
derived artifacts are regenerated with `cargo`). A compromised Codex release
could therefore reach the token; it is scoped to this repository, and the same
release would reach `main` through the gate anyway, so the residual risk is
push access to non-protected branches. Splitting the watcher into a
token-free prepare job and a token-holding publish job would remove it.

## Manual controls

- Run the watcher immediately: **Actions → Prepare Codex upstream sync → Run
  workflow** (optionally with an exact tag).
- Re-prepare an already open candidate: dispatch with `refresh = true`.
- Pause the automation: set `[sync].automatic_merge = false` on `main` (or run
  `gh pr merge <candidate> --disable-auto` on every open candidate) **before**
  disabling the two workflows in the Actions UI. Disabling a workflow does not
  cancel an auto-merge that is already queued: GitHub would still merge the
  candidate when its gate passes, and with the promotion workflow disabled no
  release would follow.
- Offline dry runs: `--trust-local-tags` skips the check that the chosen tag
  is published by openai/codex. The watcher never passes it.
- The watcher always checks out and replays `main`, even when dispatched from
  another branch, so changes to the watcher itself only take effect once they
  are merged.

## Invariants

- `main` and the active worktree are never mutated by synchronization.
- Candidates are always prepared from a clean `main`; a dirty source aborts.
- The exact upstream tag, commit, and tree are verified before replay, and the
  chosen tag must point at the commit `openai/codex` publishes for it: tags are
  fetched into one namespace, so a `rust-vX.Y.Z` tag that exists only on this
  fork's `origin` is refused instead of being merged and released as a Codex
  release. Codex cuts each release on its own short branch, so the target
  normally is a sibling of the baseline rather than a descendant; the two must
  share history, and commits that exist only on the old release branch are
  listed in the PR.
- Releases are replayed in order (`policy: next`), one candidate at a time, so
  every Codex release gets a Zuno release; `policy: newest` is an explicit
  choice to skip intermediate releases.
- The merge is the review gate. Hands-off mode only ever queues candidates
  whose replay was fully automatic, only with the automation token, and only
  for the exact head the watcher pushed; anything a person touched waits for a
  person.
- Automation resolves a conflict hunk only when `FORK_REBRAND.toml` reproduces
  the Zuno side from the Codex baseline byte for byte, or when the hunk is
  solely the workspace `version` line of `codex-rs/Cargo.toml` (Zuno versions
  track Codex versions); every other hunk waits for a human. Source code is
  never renamed. Reviewed post-merge edits are reused, never re-derived, when
  `main` moves; when `FORK_REBRAND.toml` itself changed on `main` in between,
  the reused paths reflect the rules of the earlier candidate and count as
  hand-resolved, so the candidate waits for a person.
- Released bytes are never rebuilt after review; promotion only republishes the
  sealed PR-gate artifacts.
