#!/usr/bin/env python3
"""Validate and seal the independent enterprise preview release channel."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import tomllib
import urllib.error
import urllib.request
import zipfile

CHANNEL = "enterprise-preview"
BRANCH = "codex/enterprise-preview"
TAG_PREFIX = "enterprise-v"
TARGETS = {
    "x86_64-unknown-linux-gnu": "ubuntu-24.04",
    "aarch64-unknown-linux-gnu": "ubuntu-24.04-arm",
}
BASE = re.compile(r"(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)")
PREVIEW = re.compile(r"(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)-preview\.([1-9]\d*)")
SHA = re.compile(r"[0-9a-f]{40}")


class InvalidPreview(ValueError):
    """A source, channel or artifact identity cannot be certified."""


def require(condition: bool, message: str) -> None:
    if not condition:
        raise InvalidPreview(message)


def git(root: Path, *args: str) -> str:
    return subprocess.check_output(
        ["git", "-C", str(root), *args], text=True, stderr=subprocess.PIPE
    ).strip()


def read_request(root: Path) -> dict:
    data = json.loads((root / "enterprise/preview.json").read_text(encoding="utf-8"))
    require(type(data.get("schemaVersion")) is int and data["schemaVersion"] == 1,
            "unknown preview manifest schema")
    require(set(data) == {
        "schemaVersion", "channel", "branch", "baseVersion", "baseSha", "version",
        "enabled", "binary", "capabilities", "targets",
    }, "unknown or missing preview manifest fields")
    require(data.get("channel") == CHANNEL, "incorrect release channel")
    require(data.get("branch") == BRANCH, "incorrect preview integration branch")
    require(type(data.get("enabled")) is bool, "enabled must be a boolean")
    require(data.get("binary") == "zuno-enterprise", "incorrect preview binary")
    require(bool(SHA.fullmatch(data.get("baseSha", ""))), "invalid stable baseline SHA")
    base = BASE.fullmatch(data.get("baseVersion", ""))
    require(base is not None, "stable baseline must be a release without a suffix")
    targets = data.get("targets")
    require(isinstance(targets, list), "targets must be an array")
    observed = {}
    for entry in targets:
        require(isinstance(entry, dict), "invalid target entry")
        triple = entry.get("triple")
        require(triple not in observed, "duplicate preview target")
        observed[triple] = entry.get("runner")
    require(observed == TARGETS, "preview targets must match the native Linux matrix")
    capabilities = data.get("capabilities")
    require(isinstance(capabilities, list), "capabilities must be an array")
    require(
        all(isinstance(item, str) and re.fullmatch(r"[a-z][a-z0-9-]*", item)
            for item in capabilities),
        "invalid capability identifier",
    )
    require(len(set(capabilities)) == len(capabilities), "duplicate capability")
    version = data.get("version")
    if version is not None:
        match = PREVIEW.fullmatch(version) if isinstance(version, str) else None
        require(match is not None, "preview version must end in -preview.N")
        require(
            tuple(map(int, match.groups()[:3]))
            == (int(base[1]), int(base[2]), int(base[3]) + 1),
            "preview core version must be the next stable patch",
        )
    if data["enabled"]:
        require(version is not None, "publication requires a preview version")
        require("root-task-resume" in capabilities, "first preview requires root-task-resume")
        cargo = tomllib.loads((root / "Cargo.toml").read_text(encoding="utf-8"))
        require(cargo["workspace"]["package"]["version"] == version,
                "Cargo and preview versions differ")
        require((root / "crates/zuno-enterprise/Cargo.toml").is_file(),
                "the enterprise binary has no implemented crate")
    return data


def validate_release(root: Path, ref: str, sha: str) -> dict:
    require(ref == f"refs/heads/{BRANCH}", "publication must run on the preview branch")
    require(bool(SHA.fullmatch(sha)), "publication requires a full source SHA")
    require(git(root, "rev-parse", "HEAD") == sha, "checkout does not match source SHA")
    data = read_request(root)
    require(git(root, "rev-parse", f"v{data['baseVersion']}^{{commit}}") == data["baseSha"],
            "stable tag and baseline SHA differ")
    baseline_cargo = tomllib.loads(git(root, "show", f"{data['baseSha']}:Cargo.toml"))
    require(baseline_cargo["workspace"]["package"]["version"] == data["baseVersion"],
            "stable baseline Cargo version differs from its tag")
    require(
        subprocess.run(
            ["git", "-C", str(root), "merge-base", "--is-ancestor", data["baseSha"], sha],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        ).returncode == 0,
        "source is not descended from the declared stable baseline",
    )
    return data


def archive_name(data: dict, triple: str) -> str:
    return f"{data['binary']}-{data['version']}-{triple}.tar.gz"


def seal(root: Path, dist: Path, ref: str, sha: str) -> dict:
    data = validate_release(root, ref, sha)
    require(data["enabled"], "publication is disabled")
    expected = {archive_name(data, triple) for triple in TARGETS}
    found = {path.name for path in dist.iterdir() if path.name.endswith(".tar.gz")}
    require(found == expected, "candidate archives are missing or unexpected")
    docs = dist / "enterprise-docs.zip"
    with zipfile.ZipFile(docs, "w", compression=zipfile.ZIP_DEFLATED) as archive:
        for name in ["PLAN.zh.md", "README.md", "STATUS.md", "preview.json"]:
            info = zipfile.ZipInfo("enterprise/" + name, date_time=(1980, 1, 1, 0, 0, 0))
            info.compress_type = zipfile.ZIP_DEFLATED
            archive.writestr(info, (root / "enterprise" / name).read_bytes())
    expected.add(docs.name)
    assets = []
    for name in sorted(expected):
        path = dist / name
        require(path.is_file() and not path.is_symlink(), "archive must be a regular file")
        size = path.stat().st_size
        require(size > 0, "archive is empty")
        assets.append({
            "name": name,
            "bytes": size,
            "sha256": hashlib.sha256(path.read_bytes()).hexdigest(),
        })
    manifest = {
        "schemaVersion": 1,
        "channel": CHANNEL,
        "branch": BRANCH,
        "tag": TAG_PREFIX + data["version"],
        "version": data["version"],
        "sourceSha": sha,
        "sourceTree": git(root, "rev-parse", "HEAD^{tree}"),
        "baseSha": data["baseSha"],
        "workflowSha": os.environ.get("GITHUB_WORKFLOW_SHA", sha),
        "runId": os.environ.get("GITHUB_RUN_ID"),
        "runAttempt": os.environ.get("GITHUB_RUN_ATTEMPT"),
        "capabilities": data["capabilities"],
        "assets": assets,
    }
    (dist / "SHA256SUMS").write_text(
        "".join(f"{item['sha256']}  {item['name']}\n" for item in assets),
        encoding="utf-8",
    )
    (dist / "candidate-manifest.json").write_text(
        json.dumps(manifest, indent=2, sort_keys=True) + "\n", encoding="utf-8"
    )
    return manifest


def verify(root: Path, dist: Path, ref: str, sha: str) -> dict:
    data = validate_release(root, ref, sha)
    require(data["enabled"], "publication is disabled")
    manifest = json.loads((dist / "candidate-manifest.json").read_text(encoding="utf-8"))
    require(manifest.get("channel") == CHANNEL and manifest.get("branch") == BRANCH,
            "candidate channel mismatch")
    require(manifest.get("sourceSha") == sha
            and manifest.get("sourceTree") == git(root, "rev-parse", "HEAD^{tree}"),
            "candidate source mismatch")
    require(manifest.get("tag") == TAG_PREFIX + data["version"]
            and manifest.get("version") == data["version"],
            "candidate version mismatch")
    require(manifest.get("schemaVersion") == 1
            and manifest.get("baseSha") == data["baseSha"]
            and manifest.get("capabilities") == data["capabilities"],
            "candidate baseline or capability mismatch")
    if os.environ.get("GITHUB_RUN_ID"):
        require(manifest.get("runId") == os.environ["GITHUB_RUN_ID"],
                "candidate came from a different workflow run")
        require(manifest.get("workflowSha") == os.environ["GITHUB_WORKFLOW_SHA"],
                "candidate workflow identity mismatch")
        require(0 < int(manifest.get("runAttempt", "0"))
                <= int(os.environ["GITHUB_RUN_ATTEMPT"]),
                "candidate workflow attempt mismatch")
    expected = {archive_name(data, target) for target in TARGETS} | {"enterprise-docs.zip"}
    assets = manifest.get("assets", [])
    require(len(assets) == len(expected)
            and {item.get("name") for item in assets} == expected,
            "candidate target set mismatch")
    canonical = ""
    for item in sorted(assets, key=lambda value: value["name"]):
        path = dist / item["name"]
        require(path.is_file() and not path.is_symlink(), "archive must be a regular file")
        require(path.stat().st_size == item["bytes"], "candidate archive size mismatch")
        require(hashlib.sha256(path.read_bytes()).hexdigest() == item["sha256"],
                "candidate archive digest mismatch")
        canonical += f"{item['sha256']}  {item['name']}\n"
    require((dist / "SHA256SUMS").read_text(encoding="utf-8") == canonical,
            "candidate checksum manifest mismatch")
    require({path.name for path in dist.iterdir()}
            == expected | {"SHA256SUMS", "candidate-manifest.json"},
            "candidate contains unexpected files")
    return manifest


class GitHub:
    """Bounded GitHub REST requests; mutating failures are never retried."""

    def __init__(self) -> None:
        self.token = os.environ.get("GH_TOKEN", "")
        self.repository = os.environ.get("GITHUB_REPOSITORY", "")
        require(bool(self.token), "GH_TOKEN is required for publication")
        require(bool(re.fullmatch(r"[\w.-]+/[\w.-]+", self.repository)),
                "GITHUB_REPOSITORY is required")
        self.base = os.environ.get("GITHUB_API_URL", "https://api.github.com")

    def request(self, path: str, method: str = "GET", data: dict | None = None):
        request = urllib.request.Request(
            self.base + "/repos/" + self.repository + "/" + path,
            data=None if data is None else json.dumps(data).encode(),
            method=method,
            headers={
                "Authorization": "Bearer " + self.token,
                "Accept": "application/vnd.github+json",
                "Content-Type": "application/json",
                "User-Agent": "zuno-enterprise-preview",
                "X-GitHub-Api-Version": "2022-11-28",
            },
        )
        try:
            with urllib.request.urlopen(request, timeout=30) as response:
                return json.load(response)
        except urllib.error.HTTPError as error:
            if error.code == 404 and method == "GET":
                return None
            raise InvalidPreview(f"GitHub {method} {path} failed with HTTP {error.code}") from error

    def upload(self, tag: str, path: Path) -> None:
        subprocess.run(
            ["gh", "release", "upload", tag, str(path), "--repo", self.repository], check=True
        )


def publish(root: Path, dist: Path, ref: str, sha: str, tag: str) -> dict:
    manifest = verify(root, dist, ref, sha)
    require(tag == manifest["tag"] and tag.startswith(TAG_PREFIX),
            "publication tag does not match the sealed candidate")
    github = GitHub()
    existing_tag = github.request("git/ref/tags/" + tag)
    if existing_tag is not None:
        target = existing_tag["object"]
        for _ in range(16):
            if target["type"] != "tag":
                break
            target = github.request("git/tags/" + target["sha"])["object"]
        require(target["type"] == "commit" and target["sha"] == sha,
                "existing preview tag points to different source")
    release = github.request("releases/tags/" + tag)
    if release is None:
        release = github.request("releases", "POST", {
            "tag_name": tag,
            "target_commitish": sha,
            "name": "Zuno enterprise " + manifest["version"],
            "body": (
                "Independent enterprise preview. Stable installations and data are unaffected.\n\n"
                "Source: `" + sha + "`\n\n"
                "Verified capabilities: " + ", ".join(manifest["capabilities"]) + "\n\n"
                "The attached candidate manifest, checksums and documentation bind this release "
                "to its source and packaged artifacts."
            ),
            "draft": True,
            "prerelease": True,
            "make_latest": "false",
        })
    require(release["draft"] is True and release["prerelease"] is True,
            "published or non-preview releases cannot be modified")
    expected = {}
    for path in dist.iterdir():
        require(path.is_file() and not path.is_symlink(), "unsafe release asset")
        expected[path.name] = (path.stat().st_size, "sha256:" + hashlib.sha256(path.read_bytes()).hexdigest())
    observed = {item["name"]: item for item in release["assets"]}
    require(set(observed) <= set(expected), "draft contains unexpected assets")
    require(len(observed) == len(release["assets"]), "draft contains duplicate assets")
    for name, asset in observed.items():
        require(expected[name] == (asset["size"], asset.get("digest")),
                "existing draft asset differs; use the original candidate or a new version")
    for name in sorted(expected):
        if name not in observed:
            github.upload(tag, dist / name)
    uploaded = github.request("releases/" + str(release["id"]))
    require(uploaded["draft"] is True and uploaded["prerelease"] is True,
            "release state changed during upload")
    require(len(uploaded["assets"]) == len(expected), "release asset count mismatch")
    for asset in uploaded["assets"]:
        require(expected.get(asset["name"]) == (asset["size"], asset.get("digest")),
                "GitHub asset size or digest mismatch")
    result = github.request("releases/" + str(release["id"]), "PATCH", {
        "draft": False, "prerelease": True, "make_latest": "false",
    })
    require(result["draft"] is False and result["prerelease"] is True,
            "GitHub did not publish the requested prerelease")
    return {"tag": tag, "sourceSha": sha, "releaseId": result["id"], "prerelease": True}


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("command", choices=["check", "identify", "seal", "verify", "publish"])
    parser.add_argument("--root", type=Path, default=Path(__file__).resolve().parents[1])
    parser.add_argument("--ref", default=os.environ.get("GITHUB_REF", ""))
    parser.add_argument("--sha", default=os.environ.get("GITHUB_SHA", ""))
    parser.add_argument("--dist", type=Path, default=Path("dist/enterprise-preview"))
    parser.add_argument("--tag", default="")
    args = parser.parse_args()
    if args.command == "check":
        result = read_request(args.root)
    elif args.command == "identify":
        result = validate_release(args.root, args.ref, args.sha)
        output = os.environ.get("GITHUB_OUTPUT")
        if output:
            with Path(output).open("a", encoding="utf-8") as handle:
                handle.write(f"publish={str(result['enabled']).lower()}\n")
                handle.write(f"version={result['version'] or ''}\n")
                handle.write(f"tag={TAG_PREFIX + result['version'] if result['enabled'] else ''}\n")
                handle.write("matrix=" + json.dumps({"include": result["targets"]}) + "\n")
    elif args.command == "seal":
        result = seal(args.root, args.dist, args.ref, args.sha)
    elif args.command == "verify":
        result = verify(args.root, args.dist, args.ref, args.sha)
    else:
        result = publish(args.root, args.dist, args.ref, args.sha, args.tag)
    print(json.dumps(result, sort_keys=True))


if __name__ == "__main__":
    try:
        main()
    except (InvalidPreview, OSError, ValueError, KeyError, TypeError, subprocess.CalledProcessError) as error:
        print(f"enterprise preview refused: {error}", file=sys.stderr)
        sys.exit(1)
