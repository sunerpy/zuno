#!/usr/bin/env python3
"""Package or publish Zuno's crates.io dependency closure deterministically."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import subprocess
import sys
import tarfile
import tempfile
import time
import tomllib
import urllib.error
import urllib.request
from collections import defaultdict
from pathlib import Path


ROOT = Path(__file__).resolve().parents[2]
REGISTRY = "crates-io"
CRATES_IO = "https://crates.io"


def run(*args: str) -> str:
    completed = subprocess.run(
        args,
        cwd=ROOT,
        check=True,
        text=True,
        stdout=subprocess.PIPE,
    )
    return completed.stdout


def metadata() -> dict:
    return json.loads(run("cargo", "metadata", "--locked", "--no-deps", "--format-version", "1"))


def publishable_packages(
    data: dict,
) -> tuple[list[dict], str, dict[str, set[str]], dict[str, set[str]]]:
    packages = {
        package["name"]: package
        for package in data["packages"]
        if package.get("publish") != []
    }
    if "zuno" not in packages:
        raise SystemExit("publishable workspace has no `zuno` package")

    versions = {package["version"] for package in packages.values()}
    if len(versions) != 1:
        raise SystemExit(f"publishable packages do not share one version: {sorted(versions)}")
    version = versions.pop()

    dependencies: dict[str, set[str]] = defaultdict(set)
    package_dependencies: dict[str, set[str]] = defaultdict(set)
    dependents: dict[str, set[str]] = defaultdict(set)
    for package in packages.values():
        for dependency in package["dependencies"]:
            name = dependency["name"]
            if name not in packages:
                continue
            package_dependencies[package["name"]].add(name)
            if dependency.get("kind") == "dev":
                continue
            requirement = dependency["req"]
            if requirement == "*":
                raise SystemExit(
                    f"{package['name']} has an unversioned publishable dependency on {name}"
                )
            dependencies[package["name"]].add(name)
            dependents[name].add(package["name"])

    remaining = {
        name: set(package_dependencies)
        for name, package_dependencies in dependencies.items()
    }
    for name in packages:
        remaining.setdefault(name, set())

    ready = sorted(name for name in packages if not remaining[name])
    ordered: list[dict] = []
    while ready:
        name = ready.pop(0)
        ordered.append(packages[name])
        for dependent in sorted(dependents[name]):
            remaining[dependent].remove(name)
            if not remaining[dependent]:
                ready.append(dependent)
                ready.sort()

    unresolved = sorted(name for name in packages if name not in {p["name"] for p in ordered})
    if unresolved:
        raise SystemExit(f"publishable workspace dependency cycle: {unresolved}")
    if ordered[-1]["name"] != "zuno":
        raise SystemExit("the installable `zuno` package is not the final dependency-ordered crate")
    if not any(
        target["name"] == "zuno" and "bin" in target["kind"]
        for target in packages["zuno"]["targets"]
    ):
        raise SystemExit("the `zuno` package does not contain the `zuno` binary")
    return ordered, version, dependencies, package_dependencies


def dependency_closure(name: str, dependencies: dict[str, set[str]]) -> set[str]:
    closure: set[str] = set()
    pending = list(dependencies[name])
    while pending:
        dependency = pending.pop()
        if dependency in closure:
            continue
        closure.add(dependency)
        pending.extend(dependencies[dependency])
    closure.discard(name)
    return closure


def patch_config(
    package: dict,
    packages: dict[str, dict],
    dependencies: dict[str, set[str]],
) -> Path | None:
    closure = dependency_closure(package["name"], dependencies)
    if not closure:
        return None

    handle = tempfile.NamedTemporaryFile(
        mode="w",
        prefix="zuno-crates-io-patch-",
        suffix=".toml",
        delete=False,
    )
    with handle:
        handle.write("[patch.crates-io]\n")
        for name in sorted(closure):
            dependency = packages[name]
            path = Path(dependency["manifest_path"]).parent.as_posix()
            handle.write(f'"{name}" = {{ path = "{path}" }}\n')
    return Path(handle.name)


def package(package: dict, allow_dirty: bool, config: Path | None) -> tuple[Path, str]:
    command = [
        "cargo",
        "package",
        "--quiet",
        "--locked",
        "--no-verify",
        "--package",
        package["name"],
    ]
    if config is not None:
        command.extend(["--config", str(config)])
    if allow_dirty:
        command.append("--allow-dirty")
    subprocess.run(command, cwd=ROOT, check=True)

    archive = ROOT / "target" / "package" / f"{package['name']}-{package['version']}.crate"
    if not archive.is_file():
        raise SystemExit(f"cargo did not create {archive}")
    digest = hashlib.sha256(archive.read_bytes()).hexdigest()

    manifest_name = f"{package['name']}-{package['version']}/Cargo.toml"
    with tarfile.open(archive, mode="r:gz") as crate:
        manifest = crate.extractfile(manifest_name)
        if manifest is None:
            raise SystemExit(f"{archive} has no normalized Cargo.toml")
        normalized = tomllib.loads(manifest.read().decode("utf-8"))
    dependency_tables = [
        normalized.get("dependencies", {}),
        normalized.get("build-dependencies", {}),
        normalized.get("dev-dependencies", {}),
    ]
    for target in normalized.get("target", {}).values():
        dependency_tables.extend(
            [
                target.get("dependencies", {}),
                target.get("build-dependencies", {}),
                target.get("dev-dependencies", {}),
            ]
        )
    for table in dependency_tables:
        for name, declaration in table.items():
            if isinstance(declaration, dict) and "path" in declaration:
                raise SystemExit(f"{archive} still contains path dependency {name}")
    return archive, digest


def published_checksum(name: str, version: str) -> str | None:
    request = urllib.request.Request(
        f"{CRATES_IO}/api/v1/crates/{name}/{version}",
        headers={"User-Agent": "zuno-release (https://github.com/sunerpy/zuno)"},
    )
    try:
        with urllib.request.urlopen(request, timeout=30) as response:
            payload = json.load(response)
    except urllib.error.HTTPError as error:
        if error.code == 404:
            return None
        raise
    return payload["version"]["checksum"]


def wait_for_published_checksum(
    name: str,
    version: str,
    expected_checksum: str,
    timeout_seconds: int,
) -> bool:
    deadline = time.monotonic() + timeout_seconds
    delay = 2.0
    while True:
        remote_checksum = published_checksum(name, version)
        if remote_checksum is not None:
            if remote_checksum != expected_checksum:
                raise SystemExit(
                    f"{name} {version} became visible with checksum {remote_checksum}, "
                    f"expected {expected_checksum}"
                )
            return True
        if time.monotonic() >= deadline:
            return False
        time.sleep(delay)
        delay = min(delay * 1.5, 15.0)


def publish(package: dict, expected_checksum: str) -> None:
    name = package["name"]
    version = package["version"]
    remote_checksum = published_checksum(name, version)
    if remote_checksum is not None:
        if remote_checksum != expected_checksum:
            raise SystemExit(
                f"{name} {version} already exists with checksum {remote_checksum}, "
                f"expected {expected_checksum}"
            )
        print(f"skip {name} {version}: identical crate already published", flush=True)
        return

    try:
        subprocess.run(
            [
                "cargo",
                "publish",
                "--locked",
                "--registry",
                REGISTRY,
                "--package",
                name,
            ],
            cwd=ROOT,
            check=True,
            env=os.environ.copy(),
        )
    except subprocess.CalledProcessError:
        # A client-side disconnect can race a successful immutable upload. Query the
        # authoritative registry for a bounded propagation window before reporting
        # failure; rerunning an already accepted version is harmless only after its
        # checksum is proven identical.
        if wait_for_published_checksum(name, version, expected_checksum, 30):
            print(
                f"accepted {name} {version}: publish command failed after registry commit",
                flush=True,
            )
            return
        raise

    if not wait_for_published_checksum(name, version, expected_checksum, 180):
        raise SystemExit(
            f"{name} {version} was accepted but did not become visible within 180 seconds"
        )
    print(f"published {name} {version}", flush=True)


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--mode", choices=["check", "publish"], default="check")
    parser.add_argument("--allow-dirty", action="store_true")
    args = parser.parse_args()

    packages, version, dependencies, package_dependencies = publishable_packages(metadata())
    packages_by_name = {package["name"]: package for package in packages}
    print(
        f"{args.mode}: {len(packages)} crates for Zuno {version}: "
        + ", ".join(package["name"] for package in packages),
        flush=True,
    )
    if args.mode == "publish" and not os.environ.get("CARGO_REGISTRY_TOKEN"):
        raise SystemExit("CARGO_REGISTRY_TOKEN is required for publish mode")

    prepared: list[tuple[dict, str]] = []
    for package_data in packages:
        config = patch_config(package_data, packages_by_name, package_dependencies)
        try:
            archive, checksum = package(package_data, args.allow_dirty, config)
            print(f"packaged {archive.name} sha256={checksum}", flush=True)
            prepared.append((package_data, checksum))
        finally:
            if config is not None:
                config.unlink(missing_ok=True)

    if args.mode == "publish":
        for package_data, checksum in prepared:
            publish(package_data, checksum)


if __name__ == "__main__":
    try:
        main()
    except subprocess.CalledProcessError as error:
        sys.exit(error.returncode)
