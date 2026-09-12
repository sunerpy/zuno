#!/usr/bin/env python3
"""Select the personal Cargo surface without Linux enterprise service roots."""

import argparse
import json
import subprocess
from pathlib import Path


def personal_arguments(metadata):
    members = set(metadata["workspace_members"])
    excluded = []
    personal = set()
    for package in metadata["packages"]:
        if package["id"] not in members:
            continue
        distribution = (package.get("metadata") or {}).get("zuno", {}).get("distribution", "personal")
        if distribution == "enterprise":
            excluded.append(package["name"])
        elif distribution == "personal":
            personal.add(package["name"])
        else:
            raise ValueError(f"unknown distribution for {package['name']}")
    if not {"zuno", "zuno-tui", "zuno-acp", "zuno-engine"}.issubset(personal):
        raise ValueError("personal surface must retain CLI, TUI, ACP and shared engine")
    arguments = ["--workspace"]
    for package in sorted(excluded):
        arguments.extend(["--exclude", package])
    return arguments


def metadata_at(repository):
    result = subprocess.run(
        ["cargo", "metadata", "--no-deps", "--format-version", "1"],
        cwd=repository, check=True, capture_output=True, text=True,
    )
    return json.loads(result.stdout)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("mode", choices=["args", "run", "verify"])
    parser.add_argument("arguments", nargs=argparse.REMAINDER)
    options = parser.parse_args()
    repository = Path(__file__).resolve().parents[1]
    metadata = metadata_at(repository)
    selection = personal_arguments(metadata)
    if options.mode == "args":
        print("\n".join(selection))
        return
    if options.mode == "verify":
        result = subprocess.run(
            ["cargo", "tree", *selection, "--edges", "normal,build,dev", "--prefix", "none"],
            cwd=repository, check=True, capture_output=True, text=True,
        )
        dependencies = {line.split()[0] for line in result.stdout.splitlines() if line.strip()}
        enterprise = set(selection[2::2])
        if dependencies & enterprise:
            raise SystemExit(f"personal dependency graph includes enterprise services: {sorted(dependencies & enterprise)}")
        print("Personal CLI/TUI/ACP dependency graph excludes enterprise services.")
        return
    if not options.arguments or options.arguments[0] not in ["check", "clippy", "test", "build"]:
        parser.error("run requires an explicit supported Cargo command")
    if any(value == "--all-features" or value.startswith("--features") or value.startswith("-F")
           for value in options.arguments):
        parser.error("personal CI cannot enable enterprise features through extra arguments")
    command = ["cargo", options.arguments[0], *selection, *options.arguments[1:]]
    raise SystemExit(subprocess.run(command, cwd=repository).returncode)


if __name__ == "__main__":
    main()
