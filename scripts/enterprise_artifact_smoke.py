#!/usr/bin/env python3
"""Run the native role fixture with the binary extracted from one release archive."""
from __future__ import annotations

import argparse
import hashlib
import gzip
import json
import os
from pathlib import Path
import platform
import re
import shutil
import struct
import subprocess
import tarfile
import tempfile

TARGETS = {"x86_64-unknown-linux-gnu": ("x86_64", 62),
           "aarch64-unknown-linux-gnu": ("aarch64", 183)}
PROOF = "enterprise-native-processes"


def require(condition: bool, message: str) -> None:
    if not condition:
        raise ValueError(message)


def sha256(path: Path) -> str:
    with path.open("rb") as source:
        return hashlib.file_digest(source, "sha256").hexdigest()


def extract(archive: Path, directory: Path, target: str) -> tuple[Path, str]:
    require(target in TARGETS, "unsupported native target")
    require(archive.is_file() and not archive.is_symlink(), "archive must be a regular file")
    require(0 < archive.stat().st_size <= 2 * 1024**3, "archive exceeds its size bound")
    with gzip.open(archive, "rb") as source:
        header = source.read(512)
    require(len(header) == 512 and header[:100].split(b"\0", 1)[0] == b"zuno-enterprise"
            and header[156:157] in {b"0", b"\0"}, "archive must start with its regular binary member")
    with tarfile.open(archive, "r:gz") as package:
        member = package.next()
        require(member is not None, "archive must contain the enterprise binary")
        require(member.name == "zuno-enterprise" and member.isfile()
                and 0 < member.size <= 2 * 1024**3 and member.mode & 0o111
                and member.mode & 0o6000 == 0, "invalid enterprise archive member")
        source = package.extractfile(member)
        require(source is not None, "binary payload is missing")
        binary = directory / "zuno-enterprise"
        with source, binary.open("xb") as output:
            shutil.copyfileobj(source, output, 1024 * 1024)
        require(binary.stat().st_size == member.size, "binary payload is truncated")
        end = member.offset_data + ((member.size + 511) // 512) * 512
        package.fileobj.seek(end)
        padding = package.fileobj.read(65537)
        require(1024 <= len(padding) <= 65536 and not any(padding),
                "archive must contain only the enterprise binary and bounded padding")
    with binary.open("rb") as source:
        header = source.read(64)
    require(len(header) == 64 and header[:6] == b"\x7fELF\x02\x01",
            "artifact must be a little-endian 64-bit ELF binary")
    require(struct.unpack_from("<H", header, 18)[0] == TARGETS[target][1],
            "artifact architecture does not match its target")
    binary.chmod(0o500)
    return binary, sha256(binary)


def validate_proof(proof: dict, binary_sha: str, version: str) -> None:
    require(proof.get("schemaVersion") == 1 and proof.get("kind") == PROOF,
            "native fixture proof is missing or unsupported")
    require(proof.get("binarySha256") == binary_sha and proof.get("version") == version,
            "fixture executed a different binary")
    require(proof.get("roles") == ["control", "gateway", "worker-a", "worker-b"],
            "all independent roles must execute the artifact")
    require(type(proof.get("modelRequests")) is int and proof["modelRequests"] > 0,
            "artifact did not execute the model fixture")
    for capability in ["userIsolation", "humanApproval", "workflow", "council",
                       "workspaceMerge", "contentReview", "shutdown"]:
        require(proof.get(capability) is True, f"native proof lacks {capability}")


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--archive", type=Path, required=True)
    parser.add_argument("--target", choices=TARGETS, required=True)
    parser.add_argument("--version", required=True)
    parser.add_argument("--source-sha", required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--profile", choices=["dev", "release"], default="release")
    options = parser.parse_args()
    root = Path(__file__).resolve().parents[1]
    require(platform.system() == "Linux" and platform.machine().lower() == TARGETS[options.target][0],
            "artifact smoke must run on its native Linux architecture")
    require(bool(re.fullmatch(r"[0-9a-f]{40}", options.source_sha)), "invalid source SHA")
    head = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=root, text=True).strip()
    require(head == options.source_sha, "fixture checkout does not match the candidate source")
    require(not options.archive.is_symlink(), "archive cannot be a symlink")
    archive = options.archive.resolve(strict=True)
    archive_sha = sha256(archive)
    with tempfile.TemporaryDirectory(prefix="zuno-enterprise-artifact-") as temporary:
        directory = Path(temporary)
        directory.chmod(0o700)
        binary, binary_sha = extract(archive, directory, options.target)
        proof_path = directory / "executed.json"
        environment = os.environ.copy()
        environment.update({
            "CARGO_INCREMENTAL": "0",
            "ZUNO_ENTERPRISE_ARTIFACT_SMOKE": "1",
            "ZUNO_ENTERPRISE_SMOKE_PROFILE": options.profile,
            "ZUNO_ENTERPRISE_TEST_BINARY": str(binary),
            "ZUNO_ENTERPRISE_TEST_BINARY_SHA256": binary_sha,
            "ZUNO_ENTERPRISE_TEST_VERSION": options.version,
            "ZUNO_ENTERPRISE_TEST_PROOF": str(proof_path),
        })
        # This release channel currently delivers the backend only.
        environment.pop("ZUNO_ENTERPRISE_WEB_DIST", None)
        subprocess.run(["python3", str(root / "scripts/check_enterprise_docker.py")],
                       cwd=root, env=environment, check=True)
        require(sha256(binary) == binary_sha and sha256(archive) == archive_sha,
                "candidate bytes changed during native validation")
        proof = json.loads(proof_path.read_text())
        validate_proof(proof, binary_sha, options.version)
        report = {
            **proof, "target": options.target, "sourceSha": head,
            "archive": archive.name, "archiveSha256": archive_sha,
            "runId": os.environ.get("GITHUB_RUN_ID"),
            "runAttempt": os.environ.get("GITHUB_RUN_ATTEMPT"),
        }
        options.output.parent.mkdir(parents=True, exist_ok=True)
        options.output.write_text(json.dumps(report, sort_keys=True, indent=2) + "\n")
    print(f"Native artifact verified: {archive.name} sha256:{archive_sha}")


if __name__ == "__main__":
    main()
