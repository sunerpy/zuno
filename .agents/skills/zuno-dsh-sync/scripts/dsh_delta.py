#!/usr/bin/env python3
"""Report DeepSeek Harness changes since Zuno's reviewed baseline."""

from __future__ import annotations

import argparse
import json
import os
import subprocess
import sys
from collections import Counter
from pathlib import Path
from typing import Any


CATEGORIES: tuple[tuple[str, tuple[str, ...]], ...] = (
    (
        "harness-composition",
        (
            "packages/boot/",
            "packages/bundle/",
            "packages/core/",
            "packages/extensions/",
            "packages/preset/",
            "docs/architecture",
            "docs/capability-seams",
        ),
    ),
    (
        "goal-recovery",
        (
            "packages/goal/",
            "packages/guard/",
            "packages/llm/llm-retry/",
        ),
    ),
    (
        "tools-web-subagent-workflow",
        (
            "packages/jobs/",
            "packages/subagent/",
            "packages/tools/",
            "packages/web/",
            "packages/workflow/",
        ),
    ),
    (
        "prompt-session-context",
        (
            "packages/compaction/",
            "packages/context/",
            "packages/core/session/",
            "packages/core/system-prompt/",
            "packages/plan/",
            "packages/session/",
        ),
    ),
    (
        "security-permissions-runtime",
        (
            "packages/credentials/",
            "packages/fs/",
            "packages/hooks/",
            "packages/interaction/",
            "packages/sandbox/",
            "packages/shell/",
            "packages/subprocess/",
            "packages/terminal/",
        ),
    ),
    (
        "providers-attachments",
        (
            "packages/attachment/",
            "packages/llm/",
        ),
    ),
    (
        "ui-client",
        (
            "apps/web/",
            "packages/client/",
            "packages/host/",
        ),
    ),
    (
        "docs-process",
        (
            ".agents/",
            ".github/",
            "docs/",
            "scripts/",
            "website/",
        ),
    ),
)


def run(repo: Path, *args: str, check: bool = True) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        ["git", "-C", str(repo), *args],
        check=check,
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )


def git_output(repo: Path, *args: str) -> str:
    return run(repo, *args).stdout.strip()


def classify(path: str) -> str:
    for category, prefixes in CATEGORIES:
        if path.startswith(prefixes):
            return category
    return "other"


def default_zuno_root() -> Path:
    return Path(__file__).resolve().parents[4]


def parse_args() -> argparse.Namespace:
    skill_root = Path(__file__).resolve().parents[1]
    zuno_root = default_zuno_root()
    parser = argparse.ArgumentParser(
        description="Compare a recorded DSH commit with the latest cached or fetched tracking ref."
    )
    parser.add_argument(
        "--baseline",
        type=Path,
        default=skill_root / "references" / "dsh-baseline.json",
    )
    parser.add_argument(
        "--repo",
        type=Path,
        default=Path(os.environ.get("DSH_REPO", zuno_root.parent / "deepseek-harness")),
    )
    parser.add_argument("--no-fetch", action="store_true")
    parser.add_argument("--json", action="store_true", dest="as_json")
    parser.add_argument("--limit", type=int, default=80)
    return parser.parse_args()


def load_baseline(path: Path) -> dict[str, Any]:
    with path.open(encoding="utf-8") as handle:
        value = json.load(handle)
    required = {
        "schema_version",
        "repository",
        "tracking_ref",
        "reviewed_commit",
        "reviewed_tag",
        "reviewed_at",
        "reviewed_against_zuno",
    }
    missing = sorted(required.difference(value))
    if missing:
        raise ValueError(f"{path} is missing keys: {', '.join(missing)}")
    if value["schema_version"] != 1:
        raise ValueError(f"{path} has unsupported schema_version {value['schema_version']!r}")
    return value


def fetch(repo: Path, tracking_ref: str) -> tuple[bool, str | None]:
    remote = tracking_ref.split("/", 1)[0]
    result = run(repo, "fetch", remote, "--prune", check=False)
    if result.returncode == 0:
        return True, None
    detail = result.stderr.strip() or result.stdout.strip() or "git fetch failed"
    return False, detail


def tags_at(repo: Path, commit: str) -> list[str]:
    output = git_output(repo, "tag", "--points-at", commit)
    return sorted(line for line in output.splitlines() if line)


def collect(args: argparse.Namespace) -> dict[str, Any]:
    baseline = load_baseline(args.baseline.resolve())
    repo = args.repo.resolve()
    if not (repo / ".git").exists():
        raise ValueError(f"{repo} is not a Git worktree")

    tracking_ref = str(baseline["tracking_ref"])
    fetched = False
    fetch_error = None
    if not args.no_fetch:
        fetched, fetch_error = fetch(repo, tracking_ref)

    reviewed = str(baseline["reviewed_commit"])
    run(repo, "cat-file", "-e", f"{reviewed}^{{commit}}")
    head = git_output(repo, "rev-parse", f"{tracking_ref}^{{commit}}")
    ancestor = run(repo, "merge-base", "--is-ancestor", reviewed, head, check=False).returncode == 0

    files: list[str] = []
    commits: list[dict[str, str]] = []
    shortstat = ""
    if ancestor and reviewed != head:
        files = [
            line
            for line in git_output(repo, "diff", "--name-only", f"{reviewed}..{head}").splitlines()
            if line
        ]
        shortstat = git_output(repo, "diff", "--shortstat", f"{reviewed}..{head}")
        raw_log = git_output(
            repo,
            "log",
            "--reverse",
            "--format=%H%x1f%cs%x1f%s",
            f"{reviewed}..{head}",
        )
        for line in raw_log.splitlines():
            commit, date, subject = line.split("\x1f", 2)
            commits.append({"commit": commit, "date": date, "subject": subject})

    categories = Counter(classify(path) for path in files)
    return {
        "baseline_path": str(args.baseline.resolve()),
        "repo": str(repo),
        "repository": baseline["repository"],
        "tracking_ref": tracking_ref,
        "fetch": {
            "attempted": not args.no_fetch,
            "fresh": fetched,
            "error": fetch_error,
        },
        "reviewed_commit": reviewed,
        "reviewed_tag": baseline["reviewed_tag"],
        "head_commit": head,
        "head_tags": tags_at(repo, head),
        "baseline_is_ancestor": ancestor,
        "commit_count": len(commits),
        "commits": commits,
        "changed_file_count": len(files),
        "changed_files": files,
        "categories": dict(sorted(categories.items())),
        "shortstat": shortstat,
    }


def print_markdown(report: dict[str, Any], limit: int) -> None:
    fetch_state = "fresh" if report["fetch"]["fresh"] else "cached"
    print("# DeepSeek Harness delta")
    print()
    print(f"- Repository: `{report['repo']}`")
    print(f"- Tracking ref: `{report['tracking_ref']}` ({fetch_state})")
    print(f"- Reviewed: `{report['reviewed_commit']}` ({report['reviewed_tag']})")
    tags = ", ".join(f"`{tag}`" for tag in report["head_tags"]) or "none"
    print(f"- Head: `{report['head_commit']}` (tags: {tags})")
    if report["fetch"]["error"]:
        print(f"- Fetch warning: {report['fetch']['error']}")
    print()

    if not report["baseline_is_ancestor"]:
        print("The reviewed commit is not an ancestor of the tracking head. Stop and investigate rewritten history.")
        return
    if report["commit_count"] == 0:
        print("No unreviewed commits.")
        return

    print(f"{report['commit_count']} commits and {report['changed_file_count']} changed files.")
    if report["shortstat"]:
        print(f"Git summary: {report['shortstat']}.")
    print()
    print("## Categories")
    print()
    for category, count in report["categories"].items():
        print(f"- `{category}`: {count} files")
    print()
    print("## Commits")
    print()
    for item in report["commits"][:limit]:
        print(f"- `{item['commit'][:12]}` {item['date']} {item['subject']}")
    omitted = report["commit_count"] - min(report["commit_count"], limit)
    if omitted:
        print(f"- ... {omitted} more commits; rerun with a larger `--limit` or `--json`.")


def main() -> int:
    args = parse_args()
    try:
        report = collect(args)
    except (OSError, ValueError, subprocess.CalledProcessError) as error:
        print(f"dsh-delta: {error}", file=sys.stderr)
        return 1

    if args.as_json:
        json.dump(report, sys.stdout, indent=2)
        print()
    else:
        print_markdown(report, args.limit)
    return 0 if report["baseline_is_ancestor"] else 2


if __name__ == "__main__":
    raise SystemExit(main())
