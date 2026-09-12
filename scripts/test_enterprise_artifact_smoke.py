"""Artifact validation rejects substitutions before executing any binary."""
import hashlib
import io
from pathlib import Path
import struct
import tarfile
import tempfile
import unittest

import enterprise_artifact_smoke as smoke


class ArtifactTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.archive = self.root / "artifact.tar.gz"
        self.output = self.root / "unpacked"
        self.output.mkdir()

    def package(self, machine=62, name="zuno-enterprise", kind=tarfile.REGTYPE, extra=False):
        binary = bytearray(64)
        binary[:6] = b"\x7fELF\x02\x01"
        struct.pack_into("<H", binary, 18, machine)
        with tarfile.open(self.archive, "w:gz") as package:
            member = tarfile.TarInfo(name)
            member.type = kind
            member.mode = 0o755
            member.size = len(binary) if kind == tarfile.REGTYPE else 0
            member.linkname = "outside" if kind == tarfile.SYMTYPE else ""
            package.addfile(member, io.BytesIO(binary) if member.isfile() else None)
            if extra:
                member = tarfile.TarInfo("unexpected")
                package.addfile(member, io.BytesIO())
        return bytes(binary)

    def test_extracts_only_the_exact_native_binary(self):
        binary = self.package()
        path, digest = smoke.extract(self.archive, self.output, "x86_64-unknown-linux-gnu")
        self.assertEqual(path.read_bytes(), binary)
        self.assertEqual(digest, hashlib.sha256(binary).hexdigest())
        self.assertEqual(path.stat().st_mode & 0o777, 0o500)

    def test_rejects_wrong_architecture_paths_links_and_extra_members(self):
        for index, arguments in enumerate([
            {"machine": 183}, {"name": "../zuno-enterprise"},
            {"kind": tarfile.SYMTYPE}, {"extra": True},
        ]):
            with self.subTest(arguments=arguments):
                self.package(**arguments)
                destination = self.root / f"case-{index}"
                destination.mkdir()
                with self.assertRaises(ValueError):
                    smoke.extract(self.archive, destination, "x86_64-unknown-linux-gnu")
        self.assertFalse((self.root / "zuno-enterprise").exists())

    def test_proof_cannot_claim_another_binary_or_skip_role_execution(self):
        proof = {
            "schemaVersion": 1, "kind": smoke.PROOF, "binarySha256": "a" * 64,
            "version": "0.10.32-preview.1", "modelRequests": 40,
            "roles": ["control", "gateway", "worker-a", "worker-b"],
            **{name: True for name in ["userIsolation", "humanApproval", "workflow",
                                      "council", "workspaceMerge", "contentReview", "shutdown"]},
        }
        smoke.validate_proof(proof, "a" * 64, proof["version"])
        for changes in [{"binarySha256": "b" * 64}, {"roles": ["control"]},
                        {"contentReview": False}, {"modelRequests": 0}]:
            with self.subTest(changes=changes), self.assertRaises(ValueError):
                smoke.validate_proof({**proof, **changes}, "a" * 64, proof["version"])


if __name__ == "__main__":
    unittest.main()
