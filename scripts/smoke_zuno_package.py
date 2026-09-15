#!/usr/bin/env python3
"""Smoke a built Zuno package archive without relying on an installed Zuno."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path
import subprocess
import tarfile
import tempfile


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--archive", type=Path, required=True)
    parser.add_argument("--target", required=True)
    parser.add_argument("--report", type=Path, required=True)
    args = parser.parse_args()

    is_windows = "windows" in args.target
    is_linux = "linux" in args.target
    suffix = ".exe" if is_windows else ""
    with tempfile.TemporaryDirectory(prefix="zuno-package-smoke-") as directory:
        root = Path(directory)
        isolated_home = root / "home"
        zuno_home = root / "zuno-home"
        isolated_home.mkdir()
        zuno_home.mkdir()
        runtime_env = os.environ.copy()
        runtime_env.pop("CODEX_HOME", None)
        runtime_env["HOME"] = str(isolated_home)
        runtime_env["ZUNO_HOME"] = str(zuno_home)
        with tarfile.open(args.archive, "r:gz") as archive:
            archive.extractall(root, filter="data")

        metadata = json.loads((root / "codex-package.json").read_text())
        expected = {
            "target": args.target,
            "variant": "zuno",
            "entrypoint": f"bin/zuno{suffix}",
        }
        for field, value in expected.items():
            if metadata.get(field) != value:
                raise RuntimeError(
                    f"invalid package metadata {field}: {metadata.get(field)!r} != {value!r}"
                )

        binary = root / f"bin/zuno{suffix}"
        host = root / f"bin/codex-code-mode-host{suffix}"
        ripgrep = root / f"codex-path/rg{suffix}"
        required = [binary, host, ripgrep]
        if is_linux:
            required.append(root / "codex-resources/bwrap")
        if is_windows:
            required.extend(
                [
                    root / "codex-resources/codex-command-runner.exe",
                    root / "codex-resources/codex-windows-sandbox-setup.exe",
                ]
            )
        missing = [str(path.relative_to(root)) for path in required if not path.is_file()]
        if missing:
            raise RuntimeError(f"package is missing required files: {missing}")

        companion_checks = []
        for name, command in (
            ("codeModeHost", [str(host), "--help"]),
            ("ripgrep", [str(ripgrep), "--version"]),
        ):
            result = subprocess.run(
                command,
                text=True,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                timeout=30,
                env=runtime_env,
                check=False,
            )
            if result.returncode != 0:
                raise RuntimeError(
                    f"packaged {name} executable smoke failed: "
                    f"{result.stdout}{result.stderr}"
                )
            companion_checks.append(name)
        if is_linux:
            bwrap = root / "codex-resources/bwrap"
            result = subprocess.run(
                [str(bwrap), "--version"],
                text=True,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                timeout=30,
                env=runtime_env,
                check=False,
            )
            if result.returncode != 0:
                raise RuntimeError(
                    f"packaged bwrap executable smoke failed: {result.stdout}{result.stderr}"
                )
            companion_checks.append("bwrap")

        version = subprocess.run(
            [str(binary), "--version"],
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            timeout=30,
            env=runtime_env,
            check=True,
        ).stdout.strip()
        if not version.startswith("zuno "):
            raise RuntimeError(f"unexpected product version output: {version!r}")

        initialize = json.dumps(
            {
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": {
                    "protocolVersion": 1,
                    "clientInfo": {"name": "zuno-package-smoke", "version": "1"},
                },
            },
            separators=(",", ":"),
        )
        acp = subprocess.run(
            [str(binary), "acp"],
            input=initialize + "\n",
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            timeout=30,
            env=runtime_env,
            check=True,
        )
        frames = [json.loads(line) for line in acp.stdout.splitlines() if line.strip()]
        response = next(frame for frame in frames if frame.get("id") == 1)
        if response.get("result", {}).get("agentInfo", {}).get("name") != "Zuno":
            raise RuntimeError(f"native ACP identity mismatch: {response}")

        if is_windows:
            for name, helper in (
                ("commandRunner", root / "codex-resources/codex-command-runner.exe"),
                (
                    "sandboxSetup",
                    root / "codex-resources/codex-windows-sandbox-setup.exe",
                ),
            ):
                result = subprocess.run(
                    [str(helper)],
                    text=True,
                    stdout=subprocess.PIPE,
                    stderr=subprocess.PIPE,
                    timeout=30,
                    env=runtime_env,
                    check=False,
                )
                if result.returncode == 0:
                    raise RuntimeError(
                        f"packaged {name} unexpectedly accepted a missing protocol payload"
                    )
                companion_checks.append(name)
        update = subprocess.run(
            [str(binary), "update"],
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            timeout=30,
            env=runtime_env,
            check=False,
        )
        update_output = update.stdout + update.stderr
        if update.returncode == 0 or "github.com/sunerpy/zuno/releases" not in update_output:
            raise RuntimeError(f"Zuno update did not fail closed: {update_output}")
        if "@openai/codex" in update_output or "chatgpt.com/codex" in update_output:
            raise RuntimeError(f"Zuno update exposed a Codex installer: {update_output}")

        daemon = subprocess.run(
            [str(binary), "remote-control", "start"],
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            timeout=30,
            env=runtime_env,
            check=False,
        )
        daemon_output = daemon.stdout + daemon.stderr
        if daemon.returncode == 0 or "managed app-server daemon mutation is disabled" not in daemon_output:
            raise RuntimeError(f"managed daemon did not fail closed: {daemon_output}")

        report = {
            "archive": str(args.archive),
            "archiveSha256": sha256(args.archive),
            "target": args.target,
            "version": version,
            "nativeAcp": True,
            "automaticUpdateBlocked": True,
            "companionExecutableChecks": companion_checks,
            "managedDaemonBlocked": True,
            "requiredFiles": [str(path.relative_to(root)) for path in required],
        }
        args.report.parent.mkdir(parents=True, exist_ok=True)
        args.report.write_text(json.dumps(report, indent=2, sort_keys=True) + "\n")
        print(json.dumps(report, indent=2, sort_keys=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
