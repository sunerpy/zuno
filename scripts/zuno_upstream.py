#!/usr/bin/env python3
"""Plan or prepare a candidate-only Codex upstream sync for Zuno.

This command never merges into the active Zuno branch. ``prepare`` creates a
new branch in a new worktree and rebases only that candidate from the recorded
Codex baseline onto an exact upstream release tag.
"""

from __future__ import annotations

import argparse
from dataclasses import asdict, dataclass
import json
from pathlib import Path
import re
import subprocess
import sys
from typing import Sequence

sys.path.insert(0, str(Path(__file__).resolve().parent))
import zuno_rebrand  # noqa: E402


STABLE_TAG = re.compile(
    r"^rust-v(?P<major>0|[1-9][0-9]*)\.(?P<minor>0|[1-9][0-9]*)\.(?P<patch>0|[1-9][0-9]*)$"
)
STRING_FIELD = re.compile(r'^([A-Za-z_][A-Za-z0-9_]*)\s*=\s*("(?:[^"\\]|\\.)*")\s*$')

# Derived artifacts are regenerated from source after the replay, so a textual
# conflict in them carries no information. They are reset to the upstream side
# and reported so the caller regenerates them (see docs/zuno-upstream-sync.md).
GENERATED_PATH_PREFIXES = (
    "codex-rs/app-server-protocol/schema/",
    "codex-rs/core/config.schema.json",
    "codex-rs/Cargo.lock",
)


def is_generated_path(path: str) -> bool:
    return any(
        path == prefix.rstrip("/") or path.startswith(prefix)
        for prefix in GENERATED_PATH_PREFIXES
    )


class SyncError(RuntimeError):
    """A sync precondition failed; ``details`` carries machine-readable context."""

    def __init__(self, message: str, details: dict[str, object] | None = None) -> None:
        super().__init__(message)
        self.details = details or {}


def stable_tag_version(tag: str) -> tuple[int, int, int]:
    match = STABLE_TAG.fullmatch(tag)
    if match is None:
        raise SyncError(f"not an exact stable Codex tag: {tag}")
    return tuple(int(match.group(name)) for name in ("major", "minor", "patch"))


@dataclass(frozen=True)
class Baseline:
    release_tag: str
    release_commit: str
    release_tree: str


@dataclass(frozen=True)
class SyncPlan:
    source: str
    source_commit: str
    baseline_tag: str
    baseline_commit: str
    target_tag: str
    target_commit: str
    target_tree: str
    relationship: str
    merge_base: str
    baseline_only_commits: list[str]
    upstream_changed_files: int
    zuno_changed_files: int
    overlapping_files: list[str]
    source_dirty: bool
    candidate_only: bool = True


@dataclass(frozen=True)
class CurrentStatus:
    """The recorded baseline already is the newest stable Codex release."""

    status: str
    source: str
    source_commit: str
    baseline_tag: str
    baseline_commit: str
    latest_stable_tag: str
    source_dirty: bool
    candidate_only: bool = True


@dataclass(frozen=True)
class FinalizedCandidate:
    branch: str
    worktree: str
    commit: str
    tree: str
    source_commit: str
    target_tag: str
    target_commit: str


SOURCE_REF_PREFIX = "refs/zuno-upstream-source/"
CONFLICT_MARKERS = ("<<<<<<< ", "=======", ">>>>>>> ")


@dataclass(frozen=True)
class RebrandReplay:
    """What the rebrand replay changed in a candidate (see FORK_REBRAND.toml)."""

    # Conflicted paths whose every hunk was the rebrand of its base; rewritten as
    # the rebrand of the upstream text.
    resolved: list[str]
    # Paths upstream deleted that Zuno had only rebranded; removed.
    deleted: list[str]
    # Zuno-rebranded paths that merged cleanly but whose replayed text differs
    # (new upstream wording, or the ${version} placeholder); rewritten.
    refreshed: list[str]
    # Paths taken from a previous candidate of the same release: its conflict
    # resolutions, adaptations in cleanly merged files, and deletions.
    reused: list[str]
    # Conflicted paths where only some hunks were rebrand; markers remain.
    partial: list[str]
    # Files upstream added inside an `[[added]]` scope of FORK_REBRAND.toml
    # (rendered-text snapshots); rebranded without a predicate.
    added: list[str]
    # Upstream-changed paths outside the Zuno delta that the rules would alter;
    # reported for review, never rewritten.
    drift: list[str]


def empty_replay() -> RebrandReplay:
    return RebrandReplay(
        resolved=[], deleted=[], refreshed=[], reused=[], partial=[], added=[], drift=[]
    )


@dataclass(frozen=True)
class PreparedCandidate:
    branch: str
    worktree: str
    target_tag: str
    target_commit: str
    candidate_base_commit: str
    manifest_updated: bool
    regenerate_paths: list[str]
    rebrand: RebrandReplay


def run(
    repo: Path, args: Sequence[str], *, check: bool = True
) -> subprocess.CompletedProcess[str]:
    completed = subprocess.run(
        ["git", *args],
        cwd=repo,
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        check=False,
    )
    if check and completed.returncode != 0:
        rendered = " ".join(["git", *args])
        detail = completed.stderr.strip() or completed.stdout.strip()
        raise SyncError(f"{rendered} failed: {detail}")
    return completed


def repo_root(start: Path) -> Path:
    result = run(start, ["rev-parse", "--show-toplevel"])
    return Path(result.stdout.strip()).resolve()


def read_baseline(manifest: Path) -> Baseline:
    if not manifest.is_file():
        raise SyncError(f"upstream manifest does not exist: {manifest}")
    section = None
    values: dict[str, str] = {}
    for number, raw_line in enumerate(
        manifest.read_text(encoding="utf-8").splitlines(), 1
    ):
        line = raw_line.split("#", 1)[0].strip()
        if not line:
            continue
        if line.startswith("[") and line.endswith("]"):
            section = line[1:-1].strip()
            continue
        if section != "baseline":
            continue
        match = STRING_FIELD.fullmatch(line)
        if match:
            values[match.group(1)] = json.loads(match.group(2))
        elif any(
            line.startswith(f"{field} ") or line.startswith(f"{field}=")
            for field in (
                "release_tag",
                "release_commit",
                "release_tree",
            )
        ):
            raise SyncError(f"invalid baseline field at {manifest}:{number}")
    missing = [
        field
        for field in ("release_tag", "release_commit", "release_tree")
        if not values.get(field)
    ]
    if missing:
        raise SyncError(f"missing baseline fields in {manifest}: {', '.join(missing)}")
    return Baseline(
        release_tag=values["release_tag"],
        release_commit=values["release_commit"],
        release_tree=values["release_tree"],
    )


def resolve_commit(repo: Path, revision: str) -> str:
    return run(repo, ["rev-parse", "--verify", f"{revision}^{{commit}}"]).stdout.strip()


def tree_for(repo: Path, revision: str) -> str:
    return run(repo, ["rev-parse", "--verify", f"{revision}^{{tree}}"]).stdout.strip()


def stable_tags(repo: Path) -> list[str]:
    lines = run(repo, ["tag", "--list", "rust-v*"]).stdout.splitlines()
    tagged: list[tuple[tuple[int, int, int], str]] = []
    for tag in lines:
        tag = tag.strip()
        if STABLE_TAG.fullmatch(tag):
            tagged.append((stable_tag_version(tag), tag))
    tagged.sort()
    return [tag for _, tag in tagged]


TARGET_POLICIES = ("next", "newest")


def exact_target(
    repo: Path,
    requested: str | None,
    baseline: Baseline | None = None,
    policy: str = "next",
    open_candidates: Sequence[str] = (),
) -> str:
    """Pick the stable Codex tag to sync.

    An explicit ``requested`` tag always wins. Otherwise ``policy`` decides:
    ``next`` (default) returns the oldest stable tag newer than the recorded
    baseline so every release is replayed in order and gets its own Zuno
    version; ``newest`` returns the newest stable tag and skips intermediate
    releases. ``open_candidates`` are the stable tags of candidates that are
    already open (unmerged) for this source: an open candidate newer than the
    policy's pick wins, so a release someone already resolved by hand is never
    regressed to an older tag; releases between the baseline and that candidate
    are superseded by it. When no tag is newer than the baseline, the newest tag
    is returned so the caller can report an up-to-date baseline.
    """
    if requested:
        if not STABLE_TAG.fullmatch(requested):
            raise SyncError(
                f"target must be an exact stable Codex tag like rust-v0.154.0: {requested}"
            )
        resolve_commit(repo, requested)
        return requested
    if policy not in TARGET_POLICIES:
        raise SyncError(f"unknown target policy {policy!r}; expected one of {TARGET_POLICIES}")
    tags = stable_tags(repo)
    if not tags:
        raise SyncError("no stable Codex rust-vX.Y.Z tag is available locally")
    chosen = tags[-1]
    if policy == "next" and baseline is not None:
        floor = stable_tag_version(baseline.release_tag)
        for tag in tags:
            if stable_tag_version(tag) > floor:
                chosen = tag
                break
    for candidate in open_candidates:
        if not STABLE_TAG.fullmatch(candidate):
            raise SyncError(f"open candidate must be an exact stable tag like rust-v0.154.0: {candidate}")
        if candidate in tags and stable_tag_version(candidate) > stable_tag_version(chosen):
            chosen = candidate
    return chosen


CANDIDATE_BRANCH_PREFIX = "upstream-sync/"


def open_candidates_on_remote(repo: Path, remote: str, source_commit: str) -> list[str]:
    """Stable tags of candidate branches on ``remote`` that are not yet merged into ``source_commit``.

    A candidate branch ``upstream-sync/X.Y.Z`` counts as open while its tip is
    not an ancestor of the source: merged candidates are ancestors and drop out,
    so only unmerged work influences target selection. Abandoned candidates must
    be deleted from the remote (the watcher deletes the ones it supersedes).
    """
    listing = run(repo, ["ls-remote", "--heads", remote, f"refs/heads/{CANDIDATE_BRANCH_PREFIX}*"])
    open_tags: list[str] = []
    for line in listing.stdout.splitlines():
        parts = line.split()
        if len(parts) != 2:
            continue
        sha, ref = parts
        version = ref.removeprefix(f"refs/heads/{CANDIDATE_BRANCH_PREFIX}")
        tag = f"rust-v{version}"
        if not STABLE_TAG.fullmatch(tag):
            continue
        if run(repo, ["cat-file", "-e", f"{sha}^{{commit}}"], check=False).returncode != 0:
            if run(repo, ["fetch", "--quiet", remote, ref], check=False).returncode != 0:
                continue
        if is_ancestor(repo, sha, source_commit):
            continue
        open_tags.append(tag)
    return sorted(open_tags, key=stable_tag_version)


def merge_base(repo: Path, left: str, right: str) -> str | None:
    result = run(repo, ["merge-base", left, right], check=False)
    if result.returncode != 0:
        return None
    return result.stdout.strip() or None


def baseline_only_commits(repo: Path, baseline: str, target: str) -> list[str]:
    """Commits reachable from the baseline tag whose patches are absent from the target.

    Codex cuts every release on a short branch off main (a release commit plus
    any cherry-picks), so a newer tag normally does not descend from the older
    one. Whatever only exists on the old release branch is not carried into the
    candidate, so it is surfaced for review.
    """
    entries = []
    for line in run(repo, ["cherry", target, baseline]).stdout.splitlines():
        marker, _, commit = line.partition(" ")
        if marker != "+" or not commit:
            continue
        subject = run(repo, ["log", "-1", "--format=%s", commit]).stdout.strip()
        entries.append(f"{commit} {subject}")
    return entries


def is_ancestor(repo: Path, older: str, newer: str) -> bool:
    return (
        run(repo, ["merge-base", "--is-ancestor", older, newer], check=False).returncode
        == 0
    )


def changed_files(repo: Path, older: str, newer: str) -> set[str]:
    result = run(repo, ["diff", "--name-only", "--no-renames", older, newer])
    return {line for line in result.stdout.splitlines() if line}


def dirty(repo: Path) -> bool:
    return bool(run(repo, ["status", "--porcelain=v1", "--untracked-files=all"]).stdout)


def validate_baseline(repo: Path, baseline: Baseline) -> None:
    actual_commit = resolve_commit(repo, baseline.release_tag)
    if actual_commit != baseline.release_commit:
        raise SyncError(
            f"baseline tag {baseline.release_tag} resolves to {actual_commit}, "
            f"manifest records {baseline.release_commit}"
        )
    actual_tree = tree_for(repo, baseline.release_commit)
    if actual_tree != baseline.release_tree:
        raise SyncError(
            f"baseline commit {baseline.release_commit} has tree {actual_tree}, "
            f"manifest records {baseline.release_tree}"
        )


def make_plan(repo: Path, baseline: Baseline, source: str, target_tag: str) -> SyncPlan:
    validate_baseline(repo, baseline)
    source_commit = resolve_commit(repo, source)
    target_commit = resolve_commit(repo, target_tag)
    target_tree = tree_for(repo, target_commit)
    baseline_version = stable_tag_version(baseline.release_tag)
    target_version = stable_tag_version(target_tag)
    if target_version <= baseline_version:
        relation = "same" if target_version == baseline_version else "older"
        raise SyncError(
            f"target {target_tag} must be strictly newer than baseline "
            f"{baseline.release_tag}; relationship is {relation}"
        )
    if target_commit == baseline.release_commit:
        raise SyncError(
            f"target {target_tag} does not advance baseline commit "
            f"{baseline.release_commit}"
        )
    if is_ancestor(repo, baseline.release_commit, target_commit):
        relationship = "descendant"
        common = baseline.release_commit
    else:
        # Upstream release tags live on sibling release branches cut from main,
        # so the normal case is a shared ancestor rather than direct descent.
        found = merge_base(repo, baseline.release_commit, target_commit)
        if found is None:
            raise SyncError(
                f"target {target_tag} shares no history with baseline "
                f"{baseline.release_tag}; unrelated release lines require explicit manual recovery"
            )
        relationship = "release-branch"
        common = found
    upstream_files = changed_files(repo, baseline.release_commit, target_commit)
    zuno_files = changed_files(repo, baseline.release_commit, source_commit)
    return SyncPlan(
        source=source,
        source_commit=source_commit,
        baseline_tag=baseline.release_tag,
        baseline_commit=baseline.release_commit,
        target_tag=target_tag,
        target_commit=target_commit,
        target_tree=target_tree,
        relationship=relationship,
        merge_base=common,
        baseline_only_commits=baseline_only_commits(
            repo, baseline.release_commit, target_commit
        ),
        upstream_changed_files=len(upstream_files),
        zuno_changed_files=len(zuno_files),
        overlapping_files=sorted(upstream_files & zuno_files),
        source_dirty=dirty(repo),
    )


def replace_manifest_baseline(
    manifest: Path, target_tag: str, target_commit: str, target_tree: str
) -> None:
    replacements = {
        "release_tag": target_tag,
        "release_commit": target_commit,
        "release_tree": target_tree,
    }
    section = None
    found: set[str] = set()
    output: list[str] = []
    for raw_line in manifest.read_text(encoding="utf-8").splitlines(keepends=True):
        line = raw_line.split("#", 1)[0].strip()
        if line.startswith("[") and line.endswith("]"):
            section = line[1:-1].strip()
        match = STRING_FIELD.fullmatch(line) if section == "baseline" else None
        if match and match.group(1) in replacements:
            key = match.group(1)
            ending = "\n" if raw_line.endswith("\n") else ""
            output.append(f"{key} = {json.dumps(replacements[key])}{ending}")
            found.add(key)
        else:
            output.append(raw_line)
    if found != replacements.keys():
        missing = sorted(replacements.keys() - found)
        raise SyncError(
            f"cannot update missing manifest baseline fields: {', '.join(missing)}"
        )
    manifest.write_text("".join(output), encoding="utf-8")


MERGE_TREE_STAGE = re.compile(r"^[0-7]{6} [0-9a-f]{40,64} [123]\t(?P<path>.+)$")


def parse_merge_tree(output: str) -> tuple[str, list[str], list[str]]:
    """Split ``git merge-tree --write-tree`` output into tree, conflicted paths, messages."""
    lines = output.splitlines()
    if not lines or not re.fullmatch(r"[0-9a-f]{40,64}", lines[0].strip()):
        raise SyncError(f"unexpected git merge-tree output: {output[:200]!r}")
    tree = lines[0].strip()
    conflicting: list[str] = []
    messages: list[str] = []
    in_messages = False
    for line in lines[1:]:
        if not in_messages:
            match = MERGE_TREE_STAGE.match(line)
            if match:
                path = match.group("path")
                if path not in conflicting:
                    conflicting.append(path)
                continue
            if not line.strip():
                in_messages = True
                continue
            in_messages = True
        if line.strip():
            messages.append(line.rstrip())
    return tree, sorted(conflicting), messages


def prepare(
    repo: Path,
    manifest_name: str,
    plan: SyncPlan,
    branch: str,
    worktree: Path,
    *,
    replay_rebrand: bool = True,
    reuse: str | None = None,
) -> PreparedCandidate:
    if plan.source_dirty:
        raise SyncError(
            "source worktree is dirty; commit the reviewed Zuno delta before preparing a sync candidate"
        )
    if (
        run(
            repo,
            ["show-ref", "--verify", "--quiet", f"refs/heads/{branch}"],
            check=False,
        ).returncode
        == 0
    ):
        raise SyncError(f"candidate branch already exists: {branch}")
    if worktree.exists():
        raise SyncError(f"candidate worktree path already exists: {worktree}")
    run(repo, ["worktree", "add", "-b", branch, str(worktree), plan.target_commit])
    # Three-way merge with the recorded baseline as the explicit base: ours is
    # the exact upstream release, theirs is the reviewed Zuno source. Unlike
    # `git apply --3way`, the ort merge follows upstream renames and reports
    # modify/delete pairs as conflicts instead of aborting. Requires git >= 2.40.
    # diff3 markers keep the baseline text of every hunk, which is what lets
    # the rebrand replay prove a hunk is rename-only before resolving it.
    merge = run(
        repo,
        [
            "-c",
            "merge.conflictStyle=diff3",
            "merge-tree",
            "--write-tree",
            f"--merge-base={plan.baseline_commit}",
            plan.target_commit,
            plan.source_commit,
        ],
        check=False,
    )
    if merge.returncode not in (0, 1):
        detail = merge.stderr.strip() or merge.stdout.strip()
        raise SyncError(f"git merge-tree failed while preparing the candidate: {detail}")
    tree, conflicting, messages = parse_merge_tree(merge.stdout)
    run(worktree, ["read-tree", "--reset", "-u", tree])
    regenerate = [path for path in conflicting if is_generated_path(path)]
    source_conflicts = [path for path in conflicting if not is_generated_path(path)]
    if regenerate:
        # Derived artifacts are rebuilt from the merged source; keep the exact
        # upstream bytes as the placeholder instead of conflict markers.
        run(worktree, ["checkout", plan.target_commit, "--", *regenerate])
    # The new baseline and the source commit are recorded even when conflicts
    # remain, so a manually resolved candidate still finalizes and promotes
    # against the exact upstream release it was built from.
    candidate_manifest = worktree / manifest_name
    replace_manifest_baseline(
        candidate_manifest,
        plan.target_tag,
        plan.target_commit,
        plan.target_tree,
    )
    run(worktree, ["add", "--", manifest_name])
    run(repo, ["update-ref", f"{SOURCE_REF_PREFIX}{branch}", plan.source_commit])
    replay = empty_replay()
    if replay_rebrand:
        source_conflicts = replay_rebrand_into(repo, worktree, plan, source_conflicts, replay)
    if reuse is not None:
        source_conflicts = reuse_resolutions(
            repo, worktree, plan, manifest_name, tree, source_conflicts, reuse, replay
        )
    if source_conflicts:
        raise SyncError(
            "candidate merge has conflicts; source branch is untouched and the candidate "
            f"was retained at {worktree} with conflict markers; resolve them, regenerate "
            "derived artifacts, then run `zuno_upstream.py finalize --worktree` to commit",
            details={
                "status": "conflict",
                "candidate_branch": branch,
                "candidate_worktree": str(worktree),
                "source_commit": plan.source_commit,
                "target_tag": plan.target_tag,
                "target_commit": plan.target_commit,
                "conflicting_files": source_conflicts,
                "generated_conflicts": regenerate,
                "conflict_messages": messages,
                "manifest_updated": True,
                "rebrand": asdict(replay),
            },
        )
    return PreparedCandidate(
        branch=branch,
        worktree=str(worktree),
        target_tag=plan.target_tag,
        target_commit=plan.target_commit,
        candidate_base_commit=resolve_commit(worktree, "HEAD"),
        manifest_updated=True,
        regenerate_paths=regenerate,
        rebrand=replay,
    )


WORKSPACE_VERSION = re.compile(r'^version = "(?P<version>[0-9]+\.[0-9]+\.[0-9]+)"$', re.M)


def blob(repo: Path, revision: str, path: str) -> str | None:
    """Return the UTF-8 text of ``path`` at ``revision``; ``None`` when absent or binary."""
    result = subprocess.run(
        ["git", "show", f"{revision}:{path}"],
        cwd=repo,
        capture_output=True,
        check=False,
    )
    if result.returncode != 0:
        return None
    try:
        return result.stdout.decode("utf-8")
    except UnicodeDecodeError:
        return None


def workspace_version(repo: Path, revision: str) -> str | None:
    text = blob(repo, revision, zuno_rebrand.WORKSPACE_MANIFEST)
    if text is None:
        return None
    match = WORKSPACE_VERSION.search(text)
    return match.group("version") if match else None


def read_worktree_text(worktree: Path, path: str) -> str | None:
    candidate = worktree / path
    if not candidate.is_file():
        return None
    try:
        return candidate.read_text(encoding="utf-8")
    except UnicodeDecodeError:
        return None


def load_rebrand(repo: Path, revision: str) -> zuno_rebrand.Rebrand | None:
    """Load the rebrand rules recorded in the Zuno source being replayed."""
    text = blob(repo, revision, zuno_rebrand.MANIFEST_NAME)
    if text is None:
        return None
    import tempfile

    with tempfile.NamedTemporaryFile("w", suffix=".toml", delete=False, encoding="utf-8") as handle:
        handle.write(text)
        temporary = Path(handle.name)
    try:
        return zuno_rebrand.Rebrand.load(temporary)
    except zuno_rebrand.RebrandError as error:
        raise SyncError(f"{zuno_rebrand.MANIFEST_NAME} at {revision} is invalid: {error}") from error
    finally:
        temporary.unlink(missing_ok=True)


def replay_rebrand_into(
    repo: Path,
    worktree: Path,
    plan: SyncPlan,
    conflicts: list[str],
    replay: RebrandReplay,
) -> list[str]:
    """Resolve rename-only conflicts and refresh rename-only files in ``worktree``.

    A path is touched only when applying the rules of ``FORK_REBRAND.toml`` (as
    recorded in the Zuno source) to the Codex baseline reproduces the Zuno text
    exactly; the same rules are then applied to the upstream release text.
    Returns the conflicts that remain.
    """
    rebrand = load_rebrand(repo, plan.source_commit)
    if rebrand is None:
        return conflicts
    source_version = workspace_version(repo, plan.source_commit)
    target_version = plan.target_tag.removeprefix("rust-v")
    remaining: list[str] = []
    for path in conflicts:
        current = read_worktree_text(worktree, path)
        base = blob(repo, plan.baseline_commit, path)
        zuno = blob(repo, plan.source_commit, path)
        upstream = blob(repo, plan.target_commit, path)
        if current is not None and zuno_rebrand.has_conflict_markers(current):
            resolved, report = zuno_rebrand.resolve_conflicts(
                current,
                rebrand,
                path=path,
                source_version=source_version,
                target_version=target_version,
            )
            if report.remaining == 0:
                (worktree / path).write_text(resolved, encoding="utf-8")
                replay.resolved.append(path)
                continue
            if report.resolved:
                (worktree / path).write_text(resolved, encoding="utf-8")
                replay.partial.append(path)
            remaining.append(path)
            continue
        # No markers: a modify/delete pair (the merge kept the Zuno copy) or a
        # rename/add collision. Only the rename-only modify/delete case is safe.
        if (
            base is not None
            and zuno is not None
            and upstream is None
            and current == zuno
            and rebrand.apply(base, version=source_version, path=path) == zuno
        ):
            (worktree / path).unlink()
            replay.deleted.append(path)
            continue
        remaining.append(path)
    conflicted = set(conflicts)
    zuno_delta = sorted(changed_files(repo, plan.baseline_commit, plan.source_commit))
    for path in zuno_delta:
        if path in conflicted or is_generated_path(path):
            continue
        current = read_worktree_text(worktree, path)
        if current is None:
            continue
        base = blob(repo, plan.baseline_commit, path)
        zuno = blob(repo, plan.source_commit, path)
        if base is None or zuno is None:
            continue
        if rebrand.apply(base, version=source_version, path=path) != zuno:
            continue
        upstream = blob(repo, plan.target_commit, path)
        expected = rebrand.apply(upstream if upstream is not None else base, version=target_version, path=path)
        if expected != current:
            (worktree / path).write_text(expected, encoding="utf-8")
            replay.refreshed.append(path)
    zuno_touched = set(zuno_delta) | conflicted
    baseline_paths = set(run(repo, ["ls-tree", "-r", "--name-only", plan.baseline_commit]).stdout.split("\n"))
    for path in sorted(changed_files(repo, plan.baseline_commit, plan.target_commit)):
        if path in zuno_touched or is_generated_path(path):
            continue
        upstream = blob(repo, plan.target_commit, path)
        if upstream is None:
            continue
        rebranded = rebrand.apply(upstream, version=target_version, path=path)
        if rebranded == upstream:
            continue
        if path not in baseline_paths and rebrand.applies_to_added(path):
            current = read_worktree_text(worktree, path)
            if current is not None and current != rebranded:
                (worktree / path).write_text(rebranded, encoding="utf-8")
                replay.added.append(path)
            continue
        replay.drift.append(path)
    return remaining


def blob_object_id(content: bytes) -> str:
    """Git blob id of ``content`` (sha1 over the ``blob <len>\\0`` header and bytes)."""
    import hashlib

    digest = hashlib.sha1(f"blob {len(content)}\0".encode() + content)
    return digest.hexdigest()


def tree_entries(repo: Path, revision: str) -> dict[str, str]:
    """Map every path in ``revision`` to its blob id."""
    output = run(repo, ["ls-tree", "-r", "-z", revision]).stdout
    entries: dict[str, str] = {}
    for record in output.split("\0"):
        if not record:
            continue
        meta, path = record.split("\t", 1)
        entries[path] = meta.split(" ")[2]
    return entries


def reuse_resolutions(
    repo: Path,
    worktree: Path,
    plan: SyncPlan,
    manifest_name: str,
    merge_tree: str,
    conflicts: list[str],
    reuse: str,
    replay: RebrandReplay,
) -> list[str]:
    """Carry the previous candidate's post-merge edits into this candidate.

    ``reuse`` must be a finalized candidate for the same release: a merge commit
    whose second parent is the release commit. Everything that differs between
    that candidate and the raw merge of its own source is a human (or replay)
    edit made after the merge: a conflict resolution, an API adaptation in a file
    that merged cleanly, or a deletion. For every path whose Zuno side is
    unchanged since that candidate's source the raw merge result is identical,
    so the edit is copied as-is; paths Zuno changed since are left to this run.
    Returns the conflicts that remain.
    """
    candidate = run(repo, ["rev-parse", "--verify", "--quiet", f"{reuse}^{{commit}}"], check=False)
    if candidate.returncode != 0:
        raise SyncError(f"--reuse {reuse} does not name a commit")
    candidate_commit = candidate.stdout.strip()
    parents = run(repo, ["rev-list", "--parents", "-n", "1", candidate_commit]).stdout.split()[1:]
    if len(parents) != 2 or parents[1] != plan.target_commit:
        raise SyncError(
            f"--reuse {reuse} is not a finalized candidate for {plan.target_tag}; "
            "its second parent must be the release commit"
        )
    previous_source = parents[0]
    old_candidate = tree_entries(repo, candidate_commit)
    merged = tree_entries(repo, merge_tree)
    old_source = tree_entries(repo, previous_source)
    new_source = tree_entries(repo, plan.source_commit)
    manifest_path = Path(manifest_name).as_posix()
    remaining = set(conflicts)
    for path in sorted(set(old_candidate) | set(merged)):
        if path == manifest_path or is_generated_path(path):
            continue
        if old_candidate.get(path) == merged.get(path):
            continue
        if old_source.get(path) != new_source.get(path):
            # Zuno changed this path after the candidate was reviewed; the old
            # edit may no longer apply, so this run's merge result stands.
            continue
        target = worktree / path
        blob_id = old_candidate.get(path)
        if blob_id is None:
            if target.exists():
                target.unlink()
                remaining.discard(path)
                replay.reused.append(path)
            continue
        if target.is_file() and blob_object_id(target.read_bytes()) == blob_id:
            # The rebrand replay already produced this exact text.
            remaining.discard(path)
            continue
        raw = subprocess.run(
            ["git", "cat-file", "blob", blob_id],
            cwd=repo,
            capture_output=True,
            check=True,
        ).stdout
        try:
            if zuno_rebrand.has_conflict_markers(raw.decode("utf-8")):
                continue
        except UnicodeDecodeError:
            pass
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_bytes(raw)
        mode = run(repo, ["ls-tree", candidate_commit, "--", path]).stdout.split(" ")[0]
        if mode == "100755":
            target.chmod(target.stat().st_mode | 0o111)
        remaining.discard(path)
        replay.reused.append(path)
    return sorted(remaining)


def finalize(
    worktree: Path,
    manifest_name: str,
    source: str | None,
    message: str | None,
) -> FinalizedCandidate:
    """Commit the prepared candidate as a merge of the Zuno source and the upstream release.

    The commit's first parent is the reviewed Zuno source (so the candidate PR
    merges into it trivially and the promoted tree equals the certified head
    tree) and its second parent is the exact upstream release commit recorded in
    the candidate's manifest, which keeps the baseline reachable for later syncs.
    """
    if not (worktree / ".git").exists():
        raise SyncError(f"candidate worktree is not a git worktree: {worktree}")
    branch = run(worktree, ["branch", "--show-current"]).stdout.strip()
    if not branch:
        raise SyncError(f"candidate worktree {worktree} is not on a branch")
    baseline = read_baseline(worktree / manifest_name)
    validate_baseline(worktree, baseline)
    recorded = run(
        worktree,
        ["rev-parse", "--verify", "--quiet", f"{SOURCE_REF_PREFIX}{branch}^{{commit}}"],
        check=False,
    )
    recorded_commit = recorded.stdout.strip() if recorded.returncode == 0 else ""
    if source:
        source_commit = resolve_commit(worktree, source)
        # The candidate tree was merged against the source that `prepare`
        # recorded; attaching it to a different (for example newer) commit would
        # silently drop whatever that commit changed.
        if recorded_commit and recorded_commit != source_commit:
            raise SyncError(
                f"--source {source} resolves to {source_commit}, but this candidate was "
                f"prepared from {recorded_commit}; re-run prepare against the new source "
                "instead of finalizing a stale merge"
            )
    elif recorded_commit:
        source_commit = recorded_commit
    else:
        raise SyncError(
            f"no recorded source commit for {branch}; pass --source <commit> explicitly"
        )
    if is_ancestor(worktree, baseline.release_commit, source_commit):
        raise SyncError(
            f"source {source_commit} already contains {baseline.release_tag}; nothing to merge"
        )
    changed = [
        line
        for line in run(
            worktree, ["diff", "--name-only", "--no-renames", baseline.release_commit]
        ).stdout.splitlines()
        if line
    ]
    unresolved = []
    for path in changed:
        candidate_path = worktree / path
        if not candidate_path.is_file():
            continue
        try:
            text = candidate_path.read_text(encoding="utf-8")
        except UnicodeDecodeError:
            continue
        if any(
            line.startswith(CONFLICT_MARKERS[0]) or line.startswith(CONFLICT_MARKERS[2])
            for line in text.splitlines()
        ):
            unresolved.append(path)
    if unresolved:
        raise SyncError(
            "candidate still contains conflict markers",
            details={"status": "conflict", "conflicting_files": sorted(unresolved)},
        )
    run(worktree, ["add", "--all"])
    tree = run(worktree, ["write-tree"]).stdout.strip()
    version = baseline.release_tag.removeprefix("rust-v")
    subject = message or f"chore(upstream): merge Codex {version} into Zuno"
    body = (
        f"{subject}\n\n"
        f"Merges exact Codex release {baseline.release_tag} into the reviewed Zuno source "
        f"{source_commit}.\n\n"
        f"Zuno-Source-Commit: {source_commit}\n"
        f"Zuno-Upstream-Tag: {baseline.release_tag}\n"
        f"Zuno-Upstream-Commit: {baseline.release_commit}\n"
    )
    commit_tree = subprocess.run(
        [
            "git",
            "commit-tree",
            tree,
            "-p",
            source_commit,
            "-p",
            baseline.release_commit,
        ],
        cwd=worktree,
        input=body,
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        check=False,
    )
    if commit_tree.returncode != 0:
        raise SyncError(f"git commit-tree failed: {commit_tree.stderr.strip()}")
    commit = commit_tree.stdout.strip()
    run(worktree, ["update-ref", f"refs/heads/{branch}", commit])
    run(worktree, ["reset", "-q"])
    run(worktree, ["update-ref", "-d", f"{SOURCE_REF_PREFIX}{branch}"], check=False)
    return FinalizedCandidate(
        branch=branch,
        worktree=str(worktree),
        commit=commit,
        tree=tree,
        source_commit=source_commit,
        target_tag=baseline.release_tag,
        target_commit=baseline.release_commit,
    )


def parser() -> argparse.ArgumentParser:
    result = argparse.ArgumentParser(description=__doc__)
    result.add_argument("--repo", type=Path, default=Path.cwd())
    result.add_argument("--manifest", default="UPSTREAM_CODEX.toml")
    result.add_argument("--remote", default="upstream")
    result.add_argument("--no-fetch", action="store_true")
    result.add_argument("--json", action="store_true")
    subparsers = result.add_subparsers(dest="command", required=True)
    for name in ("check", "prepare"):
        command = subparsers.add_parser(name)
        command.add_argument("--source", default="HEAD")
        command.add_argument("--target")
        command.add_argument(
            "--policy",
            choices=TARGET_POLICIES,
            default="next",
            help=(
                "which stable tag to sync when --target is omitted: 'next' replays releases "
                "in order (the oldest tag newer than the baseline), 'newest' jumps to the latest"
            ),
        )
        command.add_argument(
            "--open-candidate",
            action="append",
            metavar="TAG",
            help=(
                "stable tag of a candidate that is already open for this source; an open "
                "candidate newer than the policy's pick is kept instead of regressing to an older tag"
            ),
        )
        command.add_argument(
            "--open-candidates-remote",
            metavar="REMOTE",
            help=(
                "discover open candidates from this remote's upstream-sync/* branches whose tip "
                "is not yet merged into --source"
            ),
        )
        if name == "check":
            command.add_argument(
                "--allow-current",
                action="store_true",
                help=(
                    "exit successfully with status=current when no stable Codex tag "
                    "is newer than the recorded baseline (for scheduled runs)"
                ),
            )
        if name == "prepare":
            command.add_argument("--branch")
            command.add_argument("--worktree", type=Path, required=True)
            command.add_argument(
                "--no-rebrand",
                action="store_true",
                help="keep every conflict for manual resolution instead of replaying FORK_REBRAND.toml",
            )
            command.add_argument(
                "--reuse",
                help=(
                    "finalized candidate commit for the same release whose post-merge edits "
                    "(conflict resolutions, adaptations, deletions) are copied for every path "
                    "the Zuno source has not changed since"
                ),
            )
    finalize_command = subparsers.add_parser(
        "finalize",
        help="commit a prepared candidate worktree as a merge of the Zuno source and the upstream release",
    )
    finalize_command.add_argument("--worktree", type=Path, required=True)
    finalize_command.add_argument("--source", help="Zuno source commit (defaults to the one recorded by prepare)")
    finalize_command.add_argument("--message", help="commit subject (trailers are appended automatically)")
    describe_command = subparsers.add_parser(
        "describe-rebrand",
        help="render the rebrand replay recorded in a prepare report as Markdown",
    )
    describe_command.add_argument("--report", type=Path, required=True, help="JSON written by `prepare --json`")
    return result


def describe_rebrand(report: dict[str, object]) -> str:
    """Markdown summary of the rebrand replay for the candidate PR or conflict issue."""
    replay = report.get("rebrand")
    if not isinstance(replay, dict):
        return "## Rebrand replay\n\nNot run (no `FORK_REBRAND.toml` in the source)."
    counts = {
        key: len(replay.get(key, []))
        for key in ("resolved", "deleted", "refreshed", "reused", "partial", "added")
    }
    lines = [
        "## Rebrand replay",
        "",
        f"`{zuno_rebrand.MANIFEST_NAME}` resolved {counts['resolved']} rename-only conflicted "
        f"paths, removed {counts['deleted']} paths that upstream deleted, refreshed "
        f"{counts['refreshed']} rebranded paths with new upstream text, rebranded "
        f"{counts['added']} new upstream snapshots, and reused {counts['reused']} edits from "
        f"the previous candidate. {counts['partial']} paths still carry markers for hunks it "
        "could not prove rename-only.",
    ]
    titles = {
        "resolved": "Resolved conflicts",
        "deleted": "Deleted with upstream",
        "refreshed": "Refreshed rebranded files",
        "added": "New upstream snapshots rebranded",
        "reused": "Reused from previous candidate",
        "partial": "Partially resolved (markers remain)",
    }
    for key, title in titles.items():
        paths = replay.get(key, [])
        if not paths:
            continue
        lines += ["", "<details>", f"<summary>{title} ({len(paths)})</summary>", ""]
        lines += [f"- `{path}`" for path in paths]
        lines += ["", "</details>"]
    drift = replay.get("drift", [])
    visible = [path for path in drift if path.startswith(DRIFT_SURFACES) and path.endswith(".rs")]
    if visible:
        lines += [
            "",
            "<details>",
            f"<summary>Possible rebrand drift in user-visible surfaces ({len(visible)} of {len(drift)} paths)</summary>",
            "",
            "Upstream changed or added these files outside the Zuno delta and the rebrand rules "
            "would alter their text. They were not rewritten; review whether the new wording is "
            "user-visible and rebrand it (with its snapshots) in the candidate.",
            "",
        ]
        lines += [f"- `{path}`" for path in visible[:DRIFT_LIST_LIMIT]]
        if len(visible) > DRIFT_LIST_LIMIT:
            lines.append(f"- … {len(visible) - DRIFT_LIST_LIMIT} more in the `prepare --json` report")
        lines += ["", "</details>"]
    return "\n".join(lines)


# Source directories whose text reaches users; drift elsewhere (workflows, docs,
# comments in backend crates) is kept in the JSON report only.
DRIFT_SURFACES = ("codex-rs/tui/", "codex-rs/cli/", "codex-rs/core/src/", "codex-rs/app-server/src/")
DRIFT_LIST_LIMIT = 80


def emit(value: object, as_json: bool) -> None:
    data = asdict(value)  # type: ignore[arg-type]
    if as_json:
        print(json.dumps(data, indent=2, sort_keys=True))
        return
    for key, item in data.items():
        if isinstance(item, list):
            print(f"{key}: {len(item)}")
            for entry in item:
                print(f"  - {entry}")
        elif isinstance(item, dict):
            for nested_key, nested in item.items():
                if isinstance(nested, list):
                    print(f"{key}.{nested_key}: {len(nested)}")
                    for entry in nested:
                        print(f"  - {entry}")
                else:
                    print(f"{key}.{nested_key}: {nested}")
        else:
            print(f"{key}: {item}")


def main(argv: Sequence[str] | None = None) -> int:
    args = parser().parse_args(argv)
    try:
        if args.command == "finalize":
            emit(
                finalize(args.worktree.resolve(), args.manifest, args.source, args.message),
                args.json,
            )
            return 0
        if args.command == "describe-rebrand":
            try:
                report = json.loads(args.report.read_text(encoding="utf-8"))
            except (OSError, json.JSONDecodeError) as error:
                raise SyncError(f"cannot read prepare report {args.report}: {error}") from error
            print(describe_rebrand(report))
            return 0
        repo = repo_root(args.repo.resolve())
        manifest = repo / args.manifest
        baseline = read_baseline(manifest)
        if not args.no_fetch:
            run(repo, ["fetch", "--tags", "--prune", args.remote])
        open_candidates = list(args.open_candidate or ())
        if args.open_candidates_remote:
            open_candidates += open_candidates_on_remote(
                repo, args.open_candidates_remote, resolve_commit(repo, args.source)
            )
        target_tag = exact_target(repo, args.target, baseline, args.policy, open_candidates)
        if (
            args.command == "check"
            and args.allow_current
            and stable_tag_version(target_tag) <= stable_tag_version(baseline.release_tag)
        ):
            validate_baseline(repo, baseline)
            emit(
                CurrentStatus(
                    status="current",
                    source=args.source,
                    source_commit=resolve_commit(repo, args.source),
                    baseline_tag=baseline.release_tag,
                    baseline_commit=baseline.release_commit,
                    latest_stable_tag=target_tag,
                    source_dirty=dirty(repo),
                ),
                args.json,
            )
            return 0
        plan = make_plan(repo, baseline, args.source, target_tag)
        if args.command == "check":
            emit(plan, args.json)
            return 0
        branch = args.branch or f"upstream-sync/{target_tag.removeprefix('rust-v')}"
        candidate = prepare(
            repo,
            args.manifest,
            plan,
            branch,
            args.worktree.resolve(),
            replay_rebrand=not args.no_rebrand,
            reuse=args.reuse,
        )
        emit(candidate, args.json)
        return 0
    except SyncError as error:
        if args.json:
            payload: dict[str, object] = {"error": str(error)}
            payload.update(error.details)
            print(json.dumps(payload, indent=2, sort_keys=True))
        else:
            print(f"error: {error}", file=sys.stderr)
            for name, value in error.details.items():
                if isinstance(value, list):
                    print(f"{name}: {len(value)}", file=sys.stderr)
                    for entry in value:
                        print(f"  - {entry}", file=sys.stderr)
                else:
                    print(f"{name}: {value}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
