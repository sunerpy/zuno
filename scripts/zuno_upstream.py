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


STABLE_TAG = re.compile(
    r"^rust-v(?P<major>0|[1-9][0-9]*)\.(?P<minor>0|[1-9][0-9]*)\.(?P<patch>0|[1-9][0-9]*)$"
)
STRING_FIELD = re.compile(r'^([A-Za-z_][A-Za-z0-9_]*)\s*=\s*("(?:[^"\\]|\\.)*")\s*$')


class SyncError(RuntimeError):
    pass


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
    upstream_changed_files: int
    zuno_changed_files: int
    overlapping_files: list[str]
    source_dirty: bool
    candidate_only: bool = True


@dataclass(frozen=True)
class PreparedCandidate:
    branch: str
    worktree: str
    target_tag: str
    target_commit: str
    candidate_base_commit: str
    manifest_updated: bool


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


def exact_target(repo: Path, requested: str | None) -> str:
    if requested:
        if not STABLE_TAG.fullmatch(requested):
            raise SyncError(
                f"target must be an exact stable Codex tag like rust-v0.154.0: {requested}"
            )
        resolve_commit(repo, requested)
        return requested
    tags = stable_tags(repo)
    if not tags:
        raise SyncError("no stable Codex rust-vX.Y.Z tag is available locally")
    return tags[-1]


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
    if not is_ancestor(repo, baseline.release_commit, target_commit):
        raise SyncError(
            f"target {target_tag} is not a descendant of baseline "
            f"{baseline.release_tag}; divergent release lines require explicit manual recovery"
        )
    relationship = "descendant"
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


def prepare(
    repo: Path,
    manifest_name: str,
    plan: SyncPlan,
    branch: str,
    worktree: Path,
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
    delta = run(
        repo,
        [
            "diff",
            "--binary",
            "--full-index",
            "--no-renames",
            plan.baseline_commit,
            plan.source_commit,
        ],
    ).stdout
    apply = subprocess.run(
        ["git", "apply", "--index", "--3way", "--whitespace=nowarn", "-"],
        cwd=worktree,
        input=delta,
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        check=False,
    )
    if apply.returncode != 0:
        detail = apply.stderr.strip() or apply.stdout.strip()
        raise SyncError(
            "candidate delta replay has conflicts; source branch is untouched and the candidate "
            f"was retained at {worktree}: {detail}"
        )
    candidate_manifest = worktree / manifest_name
    replace_manifest_baseline(
        candidate_manifest,
        plan.target_tag,
        plan.target_commit,
        plan.target_tree,
    )
    return PreparedCandidate(
        branch=branch,
        worktree=str(worktree),
        target_tag=plan.target_tag,
        target_commit=plan.target_commit,
        candidate_base_commit=resolve_commit(worktree, "HEAD"),
        manifest_updated=True,
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
        if name == "prepare":
            command.add_argument("--branch")
            command.add_argument("--worktree", type=Path, required=True)
    return result


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
        else:
            print(f"{key}: {item}")


def main(argv: Sequence[str] | None = None) -> int:
    args = parser().parse_args(argv)
    try:
        repo = repo_root(args.repo.resolve())
        manifest = repo / args.manifest
        baseline = read_baseline(manifest)
        if not args.no_fetch:
            run(repo, ["fetch", "--tags", "--prune", args.remote])
        target_tag = exact_target(repo, args.target)
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
        )
        emit(candidate, args.json)
        return 0
    except SyncError as error:
        if args.json:
            print(json.dumps({"error": str(error)}, indent=2, sort_keys=True))
        else:
            print(f"error: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
