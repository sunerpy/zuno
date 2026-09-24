#!/usr/bin/env python3
"""Tests for the parts of smoke_zuno_package.py that run without a real Zuno build."""

import importlib.util
import json
from pathlib import Path
import subprocess
import sys
import tarfile
import tempfile
import unittest


SCRIPT = Path(__file__).with_name("smoke_zuno_package.py")
SPEC = importlib.util.spec_from_file_location("smoke_zuno_package", SCRIPT)
assert SPEC and SPEC.loader
smoke = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = smoke
SPEC.loader.exec_module(smoke)


def pe(machine: int) -> bytes:
    header = bytearray(b"MZ" + b"\0" * 62)
    header[0x3C:0x40] = (0x80).to_bytes(4, "little")
    return bytes(header) + b"\0" * (0x80 - 64) + b"PE\0\0" + machine.to_bytes(2, "little") + b"\0" * 20


def macho(cputype: int) -> bytes:
    return b"\xcf\xfa\xed\xfe" + cputype.to_bytes(4, "little") + b"\0" * 56


def elf(machine: int) -> bytes:
    return b"\x7fELF" + bytes([2, 1, 1]) + b"\0" * 9 + (2).to_bytes(2, "little") + machine.to_bytes(2, "little") + b"\0" * 44


class MachineTest(unittest.TestCase):
    def test_headers_are_classified_per_target(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            cases = {
                "aarch64-pc-windows-msvc": pe(0xAA64),
                "x86_64-pc-windows-msvc": pe(0x8664),
                "aarch64-apple-darwin": macho(0x0100000C),
                "x86_64-apple-darwin": macho(0x01000007),
                "aarch64-unknown-linux-gnu": elf(0xB7),
                "x86_64-unknown-linux-gnu": elf(0x3E),
            }
            for target, payload in cases.items():
                path = root / target
                path.write_bytes(payload)
                self.assertEqual(smoke.binary_machine(path), smoke.expected_machine(target), target)
            # A binary built for the wrong architecture does not match its target.
            self.assertNotEqual(smoke.binary_machine(root / "x86_64-pc-windows-msvc"), smoke.expected_machine("aarch64-pc-windows-msvc"))
            (root / "text").write_bytes(b"#!/bin/sh\n")
            with self.assertRaises(RuntimeError):
                smoke.binary_machine(root / "text")


class LayoutOnlyTest(unittest.TestCase):
    def test_layout_only_verifies_machines_and_reports_no_execution(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            package = root / "pkg"
            for relative in ("bin/zuno.exe", "bin/codex-code-mode-host.exe", "codex-path/rg.exe",
                             "codex-resources/codex-command-runner.exe", "codex-resources/codex-windows-sandbox-setup.exe"):
                path = package / relative
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_bytes(pe(0xAA64))
            (package / "codex-package.json").write_text(json.dumps(
                {"target": "aarch64-pc-windows-msvc", "variant": "zuno", "entrypoint": "bin/zuno.exe"}))
            archive = root / "zuno-package-aarch64-pc-windows-msvc.tar.gz"
            with tarfile.open(archive, "w:gz") as tar:
                for path in sorted(package.rglob("*")):
                    if path.is_file():
                        tar.add(path, arcname=str(path.relative_to(package)))
            report_path = root / "smoke.json"
            result = subprocess.run(
                [sys.executable, str(SCRIPT), "--archive", str(archive), "--target", "aarch64-pc-windows-msvc",
                 "--report", str(report_path), "--layout-only"],
                capture_output=True, text=True, check=False,
            )
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
            report = json.loads(report_path.read_text())
            self.assertFalse(report["executed"])
            self.assertFalse(report["nativeAcp"])
            self.assertIn("no binary was run", report["executionSkipped"])
            self.assertEqual(set(report["binaryMachines"].values()), {"pe:0xaa64"})
            # A wrong-architecture binary fails even without execution.
            (package / "bin/zuno.exe").write_bytes(pe(0x8664))
            with tarfile.open(archive, "w:gz") as tar:
                for path in sorted(package.rglob("*")):
                    if path.is_file():
                        tar.add(path, arcname=str(path.relative_to(package)))
            result = subprocess.run(
                [sys.executable, str(SCRIPT), "--archive", str(archive), "--target", "aarch64-pc-windows-msvc",
                 "--report", str(report_path), "--layout-only"],
                capture_output=True, text=True, check=False,
            )
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("expected pe:0xaa64", result.stderr)


if __name__ == "__main__":
    unittest.main()
