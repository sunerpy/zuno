#!/usr/bin/env python3
"""Import optional compiler snapshots only after their exact release was promoted."""

import argparse
import json
import os
from pathlib import Path
import re
import subprocess
import time

from ci_cache import (
    TARGETS, canonical, checked_sha, emit, positive, validate_snapshot,
)


class GitHub:
    def get(self, path):
        return json.loads(subprocess.check_output(
            ["gh", "api", path], text=True, timeout=30
        ))

    def artifacts(self, repository, run_id):
        pages = json.loads(subprocess.check_output([
            "gh", "api", "--paginate", "--slurp",
            f"repos/{repository}/actions/runs/{run_id}/artifacts?per_page=100",
        ], text=True, timeout=60))
        return [item for page in pages for item in page["artifacts"]]


def producer_run(api, repository, run_id, attempt, head, wait_seconds=0):
    deadline = time.monotonic() + wait_seconds
    while True:
        run = api.get(f"repos/{repository}/actions/runs/{positive(run_id)}")
        if (
            run.get("path") != ".github/workflows/release.yml"
            or run.get("event") not in ("push", "workflow_dispatch")
            or run.get("head_branch") != "main"
            or run.get("run_attempt") != positive(attempt)
            or run.get("head_sha") != checked_sha(head)
            or run.get("repository", {}).get("full_name") != repository
            or run.get("head_repository", {}).get("full_name") != repository
        ):
            raise ValueError("cache handoff must come from the exact main Release run")
        if run.get("status") == "completed":
            if run.get("conclusion") != "success":
                raise ValueError("cache handoff requires a successful Release run")
            return run
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            raise TimeoutError("the originating Release run did not finish successfully in time")
        # The parent dispatches this workflow as its last job. Let that exact
        # attempt finish; do not infer success from publication or the dispatch.
        time.sleep(min(2, remaining))


def unique_artifact(artifacts, name, limit):
    matches = [item for item in artifacts if item["name"] == name and not item["expired"]]
    if not matches:
        return None
    if len(matches) != 1 or matches[0]["size_in_bytes"] > limit:
        raise ValueError(f"ambiguous or oversized cache artifact: {name}")
    return positive(matches[0]["id"])


def find_handoff(api, repository, run_id, attempt, head, wait_seconds=0):
    producer_run(api, repository, run_id, attempt, head, wait_seconds)
    return unique_artifact(
        api.artifacts(repository, run_id),
        f"compiler-cache-handoff-{attempt}", 64 * 1024,
    )


def write_handoff(path, environment):
    if environment["GITHUB_REF"] != "refs/heads/main":
        raise ValueError("cache handoffs are written by main Release workflows only")
    tag = environment["CACHE_RELEASE_TAG"]
    if not re.fullmatch(r"v\d+\.\d+\.\d+(?:[-.][0-9A-Za-z.-]+)?", tag):
        raise ValueError("invalid release tag")
    handoff = {
        "schema_version": 1,
        "repository": environment["GITHUB_REPOSITORY"],
        "release_run_id": positive(environment["GITHUB_RUN_ID"]),
        "release_run_attempt": positive(environment["GITHUB_RUN_ATTEMPT"]),
        "release_workflow_sha": checked_sha(environment["GITHUB_SHA"]),
        "release_sha": checked_sha(environment["CACHE_RELEASE_SHA"]),
        "release_tag": tag,
        "candidate_run_id": positive(environment["CACHE_CANDIDATE_RUN_ID"]),
        "candidate_run_attempt": positive(environment["CACHE_CANDIDATE_RUN_ATTEMPT"]),
        "candidate_head_sha": checked_sha(environment["CACHE_CANDIDATE_HEAD_SHA"]),
        "tree_sha": checked_sha(environment["CACHE_TREE_SHA"]),
    }
    Path(path).write_text(json.dumps(handoff, indent=2) + "\n", encoding="utf-8")


def plan(api, handoff, repository, run_id, attempt, head):
    producer_run(api, repository, run_id, attempt, head)
    expected_fields = {
        "schema_version", "repository", "release_run_id", "release_run_attempt",
        "release_workflow_sha", "release_sha", "release_tag", "candidate_run_id",
        "candidate_run_attempt", "candidate_head_sha", "tree_sha",
    }
    if set(handoff) != expected_fields or type(handoff["schema_version"]) is not int:
        raise ValueError("unsupported cache handoff schema")
    for field in (
        "release_run_id", "release_run_attempt",
        "candidate_run_id", "candidate_run_attempt",
    ):
        if type(handoff[field]) is not int:
            raise ValueError("cache handoff run identities must be integers")
    if (
        handoff["schema_version"] != 1
        or handoff["repository"] != repository
        or handoff["release_run_id"] != positive(run_id)
        or handoff["release_run_attempt"] != positive(attempt)
        or handoff["release_workflow_sha"] != checked_sha(head)
    ):
        raise ValueError("cache handoff producer identity mismatch")
    release_sha = checked_sha(handoff["release_sha"])
    source_sha = checked_sha(handoff["candidate_head_sha"])
    tree_sha = checked_sha(handoff["tree_sha"])
    candidate_id = positive(handoff["candidate_run_id"])
    candidate_attempt = positive(handoff["candidate_run_attempt"])
    tag = handoff["release_tag"]
    if not isinstance(tag, str) or not re.fullmatch(r"v\d+\.\d+\.\d+(?:[-.][0-9A-Za-z.-]+)?", tag):
        raise ValueError("invalid handoff tag")
    release = api.get(f"repos/{repository}/releases/tags/{tag}")
    if release.get("draft") is not False or release.get("tag_name") != tag or not release.get("published_at"):
        raise ValueError("compiler caches require a public release")
    reference = api.get(f"repos/{repository}/git/ref/tags/{tag}")["object"]
    for _ in range(8):
        if reference["type"] != "tag":
            break
        reference = api.get(
            f"repos/{repository}/git/tags/{checked_sha(reference['sha'])}"
        )["object"]
    if reference["type"] != "commit" or reference["sha"] != release_sha:
        raise ValueError("published tag does not match the promoted release commit")
    commit = api.get(f"repos/{repository}/git/commits/{release_sha}")
    if commit["tree"]["sha"] != tree_sha:
        raise ValueError("published release tree differs from the cache source")
    candidate = api.get(f"repos/{repository}/actions/runs/{candidate_id}")
    if (
        candidate.get("path") != ".github/workflows/release-candidate.yml"
        or candidate.get("event") != "workflow_dispatch"
        or candidate.get("status") != "completed"
        or candidate.get("conclusion") != "success"
        or candidate.get("head_sha") != source_sha
        or candidate.get("run_attempt") != candidate_attempt
        or candidate.get("repository", {}).get("full_name") != repository
        or candidate.get("head_repository", {}).get("full_name") != repository
    ):
        raise ValueError("cache source is not the exact successful release candidate")
    source = api.get(f"repos/{repository}/git/commits/{source_sha}")
    if source["tree"]["sha"] != tree_sha:
        raise ValueError("candidate source tree differs from the published tree")
    artifacts = api.artifacts(repository, candidate_id)
    matrix = []
    for target in TARGETS:
        slot = f"release-{target}"
        artifact = unique_artifact(
            artifacts, f"compiler-cache-{slot}-{candidate_attempt}",
            900 * 1024 * 1024,
        )
        if artifact is not None:
            matrix.append({"slot": slot, "artifact_id": artifact})
    expected = {
        "repository": repository,
        "head_sha": source_sha,
        "tree_sha": tree_sha,
        "run_id": candidate_id,
        "run_attempt": candidate_attempt,
    }
    return matrix, expected


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest="command", required=True)
    sub.add_parser("handoff").add_argument("--output", type=Path, required=True)
    for name in ("resolve", "plan"):
        command = sub.add_parser(name)
        command.add_argument("--run-id", type=int, required=True)
        command.add_argument("--attempt", type=int, required=True)
        command.add_argument("--head", required=True)
        if name == "plan":
            command.add_argument("--handoff", type=Path, required=True)
        else:
            command.add_argument("--wait-seconds", type=int, choices=range(301), default=0)
    validate = sub.add_parser("validate")
    validate.add_argument("--slot", required=True)
    validate.add_argument("--root", type=Path, required=True)
    args = parser.parse_args()
    if args.command == "handoff":
        write_handoff(args.output, os.environ)
        return
    if args.command == "validate":
        expected = json.loads(os.environ["CACHE_EXPECTED_JSON"])
        emit("key", validate_snapshot(args.root, args.slot, expected))
        return
    if os.environ.get("GITHUB_REF") != "refs/heads/main":
        raise ValueError("only the main workflow may publish shared compiler snapshots")
    repository = os.environ["GITHUB_REPOSITORY"]
    api = GitHub()
    if args.command == "resolve":
        artifact = find_handoff(
            api, repository, args.run_id, args.attempt, args.head, args.wait_seconds
        )
        emit("available", str(artifact is not None).lower())
        if artifact is not None:
            emit("artifact", artifact)
    else:
        matrix, expected = plan(
            api, json.loads(args.handoff.read_text(encoding="utf-8")), repository,
            args.run_id, args.attempt, args.head,
        )
        emit("ready", str(bool(matrix)).lower())
        emit("matrix", canonical({"include": matrix}).decode())
        emit("expected", canonical(expected).decode())
        emit("candidate_run_id", expected["run_id"])


if __name__ == "__main__":
    main()
