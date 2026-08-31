#!/usr/bin/env python3
"""Write one target's immutable release-candidate evidence."""

from __future__ import annotations

import argparse
import hashlib
import json
from pathlib import Path


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--target", required=True)
    parser.add_argument("--archive", required=True, type=Path)
    parser.add_argument("--runner", required=True)
    parser.add_argument("--attestation-id", required=True)
    parser.add_argument("--attestation-url", required=True)
    parser.add_argument("--output", required=True, type=Path)
    args = parser.parse_args()

    if not args.archive.is_file():
        raise SystemExit(f"archive does not exist: {args.archive}")

    evidence = {
        "target": args.target,
        "archive": args.archive.name,
        "size": args.archive.stat().st_size,
        "sha256": sha256(args.archive),
        "build_conclusion": "success",
        "smoke_conclusion": "success",
        "runner": args.runner,
        "attestation_id": args.attestation_id,
        "attestation_url": args.attestation_url,
    }
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(
        json.dumps(evidence, indent=2, sort_keys=True) + "\n",
        encoding="utf-8",
    )


if __name__ == "__main__":
    main()
