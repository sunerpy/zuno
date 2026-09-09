#!/usr/bin/env python3
"""Compiler-cache snapshots, identities, diagnostics, and bounded retention."""

import argparse
import hashlib
import json
import os
from pathlib import Path, PurePosixPath
import re
import stat
import subprocess
import tomllib

NAMESPACE = "zuno-compiler-v1-"
CACHE_PATH = Path("target/ci-cache/compiler")
METADATA_PATH = Path("target/ci-cache/metadata.json")
SCHEMA = 1
SCCACHE_VERSION = "0.16.0"
MAX_BYTES = 800 * 1024 * 1024
MAX_FILES = 50_000
TARGETS = (
    "aarch64-apple-darwin",
    "aarch64-pc-windows-msvc",
    "aarch64-unknown-linux-musl",
    "x86_64-apple-darwin",
    "x86_64-pc-windows-msvc",
    "x86_64-unknown-linux-musl",
)
SLOTS = (
    "pr-linux-tests", "pr-windows-tests", "pr-host-release",
    *(f"release-{target}" for target in TARGETS),
)
SHA = re.compile(r"[0-9a-f]{40}\Z")


def checked_slot(slot):
    if slot not in SLOTS:
        raise ValueError(f"unknown compiler-cache slot: {slot}")
    return slot


def checked_sha(value):
    if not isinstance(value, str) or not SHA.fullmatch(value):
        raise ValueError("expected a full commit/tree SHA")
    return value


def positive(value):
    if isinstance(value, bool) or not str(value).isdecimal() or int(value) < 1:
        raise ValueError("expected a positive run identifier")
    return int(value)


def canonical(value):
    return json.dumps(value, sort_keys=True, separators=(",", ":")).encode()


def git(*arguments):
    return subprocess.check_output(["git", *arguments], text=True).strip()


def identity():
    repository = os.environ["GITHUB_REPOSITORY"]
    if not re.fullmatch(r"[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+", repository):
        raise ValueError("invalid repository identity")
    head = checked_sha(git("rev-parse", "HEAD"))
    if head != checked_sha(os.environ["GITHUB_SHA"]):
        raise ValueError("checked-out source differs from the workflow head")
    return {
        "repository": repository,
        "head_sha": head,
        "tree_sha": checked_sha(git("rev-parse", "HEAD^{tree}")),
        "run_id": positive(os.environ["GITHUB_RUN_ID"]),
        "run_attempt": positive(os.environ["GITHUB_RUN_ATTEMPT"]),
    }


def key_prefix(slot, compiler, profiles, configuration, environment):
    """Version-only releases can reuse dependencies; rustc still hashes all inputs."""
    checked_slot(slot)
    flags = {
        key: value for key, value in environment.items()
        if key in ("RUSTFLAGS", "CARGO_ENCODED_RUSTFLAGS", "CARGO_BUILD_TARGET")
        or key.startswith("CARGO_PROFILE_")
    }
    digest = hashlib.sha256(canonical({
        "compiler": compiler,
        "profiles": profiles,
        "configuration": configuration,
        "flags": flags,
        "sccache": SCCACHE_VERSION,
    })).hexdigest()[:24]
    return f"{NAMESPACE}{slot}-{digest}"


def current_prefix(slot):
    manifest = tomllib.loads(Path("Cargo.toml").read_text(encoding="utf-8"))
    configuration = Path(".cargo/config.toml")
    return key_prefix(
        slot,
        subprocess.check_output(["rustc", "-vV"], text=True),
        manifest.get("profile", {}),
        configuration.read_text(encoding="utf-8") if configuration.exists() else "",
        os.environ,
    )


def snapshot_key(prefix, source):
    return (
        f"{prefix}-{checked_sha(source['head_sha'])}-"
        f"{positive(source['run_id'])}-{positive(source['run_attempt'])}"
    )


def emit(name, value, destination="GITHUB_OUTPUT"):
    value = str(value)
    if "\n" in value or "\r" in value:
        raise ValueError("workflow outputs must be single-line values")
    path = os.environ.get(destination)
    if path:
        with open(path, "a", encoding="utf-8") as stream:
            stream.write(f"{name}={value}\n")


def configure(slot):
    prefix = current_prefix(slot)
    source = identity()
    CACHE_PATH.mkdir(parents=True, exist_ok=True)
    emit("prefix", prefix)
    emit("key", snapshot_key(prefix, source))
    emit("SCCACHE_DIR", CACHE_PATH.resolve(), "GITHUB_ENV")
    emit("SCCACHE_GHA_ENABLED", "false", "GITHUB_ENV")
    emit("SCCACHE_CACHE_SIZE", "768M", "GITHUB_ENV")


def inventory(directory):
    """Describe regular files only; cache snapshots never carry links or devices."""
    directory = Path(directory)
    if _is_link(directory) or not directory.is_dir():
        raise ValueError("compiler cache is not a regular directory")
    files, total, entries = [], 0, 0
    for path in directory.rglob("*"):
        entries += 1
        if entries > MAX_FILES * 4:
            raise ValueError("compiler cache contains too many directory entries")
        if _is_link(path):
            raise ValueError("cache snapshots may not contain symbolic links")
        relative = checked_path(path.relative_to(directory).as_posix())
        if path.is_dir():
            continue
        if not path.is_file():
            raise ValueError("cache snapshots may contain regular files only")
        size = path.stat().st_size
        total += size
        if total > MAX_BYTES or len(files) >= MAX_FILES:
            raise ValueError("compiler cache exceeds its snapshot budget")
        digest = hashlib.sha256()
        with path.open("rb") as stream:
            for chunk in iter(lambda: stream.read(1024 * 1024), b""):
                digest.update(chunk)
        files.append({"path": relative, "size": size, "sha256": digest.hexdigest()})
    files.sort(key=lambda entry: entry["path"])
    return files, total


def checked_path(value):
    if not isinstance(value, str) or not value or len(value) > 1024:
        raise ValueError("invalid cache entry path")
    path = PurePosixPath(value)
    if any(ord(character) < 32 or character in '<>"|?*' for character in value):
        raise ValueError("cache entry path is not portable")
    if path.is_absolute() or str(path) != value or any(
        part in (".", "..") or ":" in part or "\\" in part
        or part.endswith((" ", ".")) for part in path.parts
    ):
        raise ValueError("cache entry path escapes its portable namespace")
    reserved = {"CON", "PRN", "AUX", "NUL", *(f"COM{i}" for i in range(1, 10)),
                *(f"LPT{i}" for i in range(1, 10))}
    if any(part.split(".", 1)[0].upper() in reserved for part in path.parts):
        raise ValueError("cache entry uses a reserved Windows path")
    return value


def _is_link(path):
    return path.is_symlink() or bool(
        getattr(path.lstat(), "st_file_attributes", 0)
        & getattr(stat, "FILE_ATTRIBUTE_REPARSE_POINT", 0x400)
    )


def export_snapshot(slot):
    checked_slot(slot)
    if not slot.startswith("release-"):
        raise ValueError("only release builds export cache artifacts")
    files, total = inventory(CACHE_PATH)
    metadata = {
        "schema_version": SCHEMA,
        "slot": slot,
        "prefix": current_prefix(slot),
        "sccache_version": SCCACHE_VERSION,
        **identity(),
        "files": files,
        "size": total,
    }
    METADATA_PATH.write_text(json.dumps(metadata, indent=2) + "\n", encoding="utf-8")


def validate_snapshot(root, slot, expected):
    root = Path(root)
    if {path.name for path in root.iterdir()} != {"compiler", "metadata.json"}:
        raise ValueError("unexpected compiler snapshot files")
    metadata_path = root / "metadata.json"
    if _is_link(metadata_path) or metadata_path.stat().st_size > 12 * 1024 * 1024:
        raise ValueError("invalid compiler snapshot metadata")
    metadata = json.loads(metadata_path.read_text(encoding="utf-8"))
    checked_slot(slot)
    if (
        type(metadata.get("schema_version")) is not int
        or metadata.get("schema_version") != SCHEMA
        or metadata.get("slot") != slot
        or metadata.get("sccache_version") != SCCACHE_VERSION
        or not re.fullmatch(
            re.escape(f"{NAMESPACE}{slot}-") + r"[0-9a-f]{24}",
            str(metadata.get("prefix", "")),
        )
    ):
        raise ValueError("compiler snapshot compatibility identity mismatch")
    checked_sha(metadata.get("head_sha"))
    checked_sha(metadata.get("tree_sha"))
    positive(metadata.get("run_id"))
    positive(metadata.get("run_attempt"))
    if type(metadata.get("size")) is not int or metadata["size"] < 0:
        raise ValueError("compiler snapshot size is invalid")
    for field in ("repository", "head_sha", "tree_sha", "run_id", "run_attempt"):
        if metadata.get(field) != expected[field]:
            raise ValueError(f"compiler snapshot source mismatch: {field}")
    files, size = inventory(root / "compiler")
    if files != metadata.get("files") or size != metadata.get("size"):
        raise ValueError("compiler snapshot contents changed")
    return snapshot_key(metadata["prefix"], metadata)


def statistics():
    data = json.loads(subprocess.check_output(
        ["sccache", "--show-stats", "--stats-format=json"], text=True
    ))
    path = Path("target/ci-cache-statistics.json")
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(data, indent=2) + "\n", encoding="utf-8")
    stats = data["stats"]
    summary = os.environ.get("GITHUB_STEP_SUMMARY")
    if summary:
        with open(summary, "a", encoding="utf-8") as stream:
            stream.write(
                "### Compiler cache\n\n"
                f"- Rust hits: {stats['cache_hits']['counts'].get('Rust', 0)}\n"
                f"- Rust misses: {stats['cache_misses']['counts'].get('Rust', 0)}\n"
                f"- Cache write errors: {stats['cache_write_errors']}\n"
                f"- Stored bytes: {data.get('cache_size', 0)}\n"
            )


def obsolete_caches(caches, slot, current_key):
    """Delete only our older main-scope snapshots after the new key is visible."""
    checked_slot(slot)
    prefix = f"{NAMESPACE}{slot}-"
    if not current_key.startswith(prefix):
        raise ValueError("current key does not belong to the requested slot")
    own = [entry for entry in caches
           if entry["ref"] == "refs/heads/main" and entry["key"].startswith(prefix)]
    current = [entry for entry in own if entry["key"] == current_key]
    if not current:
        return []
    created = max(entry["created_at"] for entry in current)
    return [entry["id"] for entry in own
            if entry["key"] != current_key and entry["created_at"] < created]


def prune(slot, key):
    if os.environ.get("GITHUB_REF") != "refs/heads/main":
        raise ValueError("only main workflows may prune shared compiler caches")
    repository = os.environ["GITHUB_REPOSITORY"]
    prefix = f"{NAMESPACE}{checked_slot(slot)}-"
    pages = json.loads(subprocess.check_output([
        "gh", "api", "--paginate", "--slurp",
        f"repos/{repository}/actions/caches?key={prefix}&ref=refs/heads/main&per_page=100",
    ], text=True, timeout=60))
    caches = [entry for page in pages for entry in page["actions_caches"]]
    for cache_id in obsolete_caches(caches, slot, key):
        subprocess.run([
            "gh", "api", "--method", "DELETE",
            f"repos/{repository}/actions/caches/{positive(cache_id)}",
        ], check=True, timeout=30)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest="command", required=True)
    for name in ("configure", "export"):
        sub.add_parser(name).add_argument("--slot", choices=SLOTS, required=True)
    sub.add_parser("statistics")
    validate = sub.add_parser("validate")
    validate.add_argument("--root", type=Path, required=True)
    validate.add_argument("--slot", choices=SLOTS, required=True)
    validate.add_argument("--expected", type=Path, required=True)
    remove = sub.add_parser("prune")
    remove.add_argument("--slot", choices=SLOTS, required=True)
    remove.add_argument("--key", required=True)
    args = parser.parse_args()
    if args.command == "configure":
        configure(args.slot)
    elif args.command == "export":
        export_snapshot(args.slot)
    elif args.command == "statistics":
        statistics()
    elif args.command == "validate":
        emit("key", validate_snapshot(
            args.root, args.slot, json.loads(args.expected.read_text(encoding="utf-8"))
        ))
    elif args.command == "prune":
        prune(args.slot, args.key)


if __name__ == "__main__":
    main()
