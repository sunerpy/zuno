#!/usr/bin/env python3
"""Require one exact SemVer patch increment during rapid development."""

from __future__ import annotations

import re
import sys


SEMVER = re.compile(r"^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$")


def parse(value: str) -> tuple[int, int, int]:
    match = SEMVER.fullmatch(value)
    if match is None:
        raise SystemExit(f"version must be plain major.minor.patch SemVer, got {value!r}")
    return tuple(int(group) for group in match.groups())


def main() -> None:
    if len(sys.argv) != 3:
        raise SystemExit("usage: require-patch-release.py CURRENT CANDIDATE")
    current_text, candidate_text = sys.argv[1:]
    current = parse(current_text)
    candidate = parse(candidate_text)
    expected = (current[0], current[1], current[2] + 1)
    if candidate != expected:
        expected_text = ".".join(str(part) for part in expected)
        raise SystemExit(
            "rapid-development releases must increment only the patch component: "
            f"{current_text} -> {expected_text}, got {candidate_text}"
        )
    print(f"patch-only release verified: {current_text} -> {candidate_text}")


if __name__ == "__main__":
    main()
