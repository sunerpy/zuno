"""Publication isolation and immutable candidate contract tests."""

from copy import deepcopy
import hashlib
import io
import json
import os
from pathlib import Path
import subprocess
import struct
import tarfile
import tempfile
import unittest
import zipfile
from unittest.mock import patch

import enterprise_preview as preview


class PreviewTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.root = Path(self.directory.name)
        self.manifest = {
            "schemaVersion": 1,
            "channel": preview.CHANNEL,
            "branch": preview.BRANCH,
            "baseVersion": "0.10.29",
            "baseSha": "a" * 40,
            "version": None,
            "enabled": False,
            "binary": "zuno-enterprise",
            "capabilities": [],
            "targets": [{"triple": key, "runner": value} for key, value in preview.TARGETS.items()],
        }
        (self.root / "enterprise").mkdir()
        for name in ["PLAN.zh.md", "README.md", "STATUS.md"]:
            (self.root / "enterprise" / name).write_text("document\n")
        self.write()

    def write(self):
        (self.root / "enterprise/preview.json").write_text(json.dumps(self.manifest))

    def candidate(self):
        self.manifest.update(enabled=True, version="0.10.30-preview.1",
                             capabilities=["root-task-resume"])
        (self.root / "crates/zuno-enterprise").mkdir(parents=True)
        (self.root / "crates/zuno-enterprise/Cargo.toml").write_text("[package]\n")
        (self.root / "Cargo.toml").write_text(
            '[workspace.package]\nversion="0.10.29"\n'
        )
        for args in [
            ["init", "-q"],
            ["config", "user.email", "preview-test@example.invalid"],
            ["config", "user.name", "Preview fixture"],
            ["add", "."],
            ["commit", "-qm", "test(preview): 建立版本基线"],
            ["tag", "v0.10.29"],
        ]:
            subprocess.run(["git", "-C", str(self.root), *args], check=True)
        self.manifest["baseSha"] = preview.git(self.root, "rev-parse", "HEAD")
        (self.root / "Cargo.toml").write_text(
            '[workspace.package]\nversion="0.10.30-preview.1"\n'
        )
        self.write()
        subprocess.run(["git", "-C", str(self.root), "add", "."], check=True)
        subprocess.run(["git", "-C", str(self.root), "commit", "-qm",
                        "test(preview): 创建预览候选"], check=True)
        sha = preview.git(self.root, "rev-parse", "HEAD")
        dist = self.root / "dist"
        dist.mkdir()
        for target in preview.TARGETS:
            binary = bytearray(64)
            binary[:6] = b"\x7fELF\x02\x01"
            struct.pack_into("<H", binary, 18, preview.artifact_smoke.TARGETS[target][1])
            binary.extend(("fixture " + target).encode())
            archive = dist / preview.archive_name(self.manifest, target)
            with tarfile.open(archive, "w:gz") as package:
                member = tarfile.TarInfo("zuno-enterprise")
                member.size = len(binary)
                member.mode = 0o755
                package.addfile(member, io.BytesIO(binary))
            evidence = {
                "schemaVersion": 1, "kind": "enterprise-native-processes",
                "binarySha256": hashlib.sha256(binary).hexdigest(),
                "version": self.manifest["version"], "sourceSha": sha, "target": target,
                "archive": archive.name, "archiveSha256": preview.artifact_smoke.sha256(archive),
                "roles": ["control", "gateway", "gateway-peer", "worker-a", "worker-b"], "modelRequests": 42,
                "runId": "42", "runAttempt": "1",
                **{name: True for name in ["userIsolation", "humanApproval", "workflow",
                                          "council", "workspaceMerge", "contentReview", "workspaceTransfer", "shutdown"]},
            }
            (dist / preview.smoke_name(self.manifest, target)).write_text(json.dumps(evidence))
        return dist, "refs/heads/" + preview.BRANCH, sha

    def test_disabled_channel_is_valid_but_cannot_publish(self):
        self.assertFalse(preview.read_request(self.root)["enabled"])
        with self.assertRaisesRegex(preview.InvalidPreview, "preview branch"):
            preview.validate_release(self.root, "refs/heads/main", "b" * 40)

    def test_rejects_stable_versions_minor_bumps_and_noncanonical_previews(self):
        for version in ["0.10.30", "0.11.0-preview.1", "0.10.29-preview.1",
                        "0.10.30-preview.0", "0.10.30-preview.01", "v0.10.30-preview.1"]:
            with self.subTest(version=version):
                self.manifest["version"] = version
                self.write()
                with self.assertRaises(preview.InvalidPreview):
                    preview.read_request(self.root)

    def test_rejects_duplicate_and_foreign_targets(self):
        original = deepcopy(self.manifest)
        self.manifest["targets"].append(self.manifest["targets"][0])
        self.write()
        with self.assertRaisesRegex(preview.InvalidPreview, "duplicate"):
            preview.read_request(self.root)
        self.manifest = original
        self.manifest["targets"][0]["runner"] = "windows-2022"
        self.write()
        with self.assertRaises(preview.InvalidPreview):
            preview.read_request(self.root)

    def test_enabled_release_requires_real_entrypoint_and_first_capability(self):
        self.manifest.update(enabled=True, version="0.10.30-preview.1")
        self.write()
        with self.assertRaisesRegex(preview.InvalidPreview, "root-task-resume"):
            preview.read_request(self.root)

    def test_sealed_candidate_verifies_and_detects_tampering(self):
        dist, ref, sha = self.candidate()
        with patch.dict(os.environ, {}, clear=True):
            manifest = preview.seal(self.root, dist, ref, sha)
            self.assertEqual(manifest, preview.verify(self.root, dist, ref, sha))
            asset = dist / manifest["assets"][0]["name"]
            asset.write_bytes(asset.read_bytes() + b"modified")
            with self.assertRaisesRegex(preview.InvalidPreview, "size mismatch"):
                preview.verify(self.root, dist, ref, sha)

    def test_preview_docs_include_tracked_guides_templates_and_licenses(self):
        files = {
            "MEMORY.md": "Memory guide\n",
            "MEMORY.zh.md": "Memory 中文\n",
            "examples/worker.json": '{"example":true}\n',
            "licenses/example.txt": "license\n",
        }
        for name, text in files.items():
            path = self.root / "enterprise" / name
            path.parent.mkdir(exist_ok=True)
            path.write_text(text, encoding="utf-8")
        dist, ref, sha = self.candidate()
        (self.root / "enterprise" / "untracked-private.md").write_text("never publish")
        (self.root / "enterprise" / "MEMORY.md").write_text("unreviewed local edit")
        with patch.dict(os.environ, {}, clear=True):
            preview.seal(self.root, dist, ref, sha)
        with zipfile.ZipFile(dist / "enterprise-docs.zip") as archive:
            for name, text in files.items():
                self.assertIn("enterprise/" + name, archive.namelist())
                self.assertEqual(archive.read("enterprise/" + name), text.encode())
            self.assertNotIn("enterprise/untracked-private.md", archive.namelist())

    def test_candidate_requires_complete_matrix_and_exact_source(self):
        dist, ref, sha = self.candidate()
        with patch.dict(os.environ, {}, clear=True):
            with self.assertRaisesRegex(preview.InvalidPreview, "checkout"):
                preview.seal(self.root, dist, ref, "b" * 40)
            next(dist.glob("*.tar.gz")).unlink()
            with self.assertRaisesRegex(preview.InvalidPreview, "missing or unexpected"):
                preview.seal(self.root, dist, ref, sha)

    def test_promotion_rejects_wrong_run_and_unsealed_extra_files(self):
        dist, ref, sha = self.candidate()
        environment = {
            "GITHUB_RUN_ID": "42", "GITHUB_RUN_ATTEMPT": "1", "GITHUB_WORKFLOW_SHA": sha,
        }
        with patch.dict(os.environ, environment, clear=True):
            preview.seal(self.root, dist, ref, sha)
            with patch.dict(os.environ, {"GITHUB_RUN_ID": "43"}):
                with self.assertRaisesRegex(preview.InvalidPreview, "different workflow"):
                    preview.verify(self.root, dist, ref, sha)
            (dist / "unexpected.txt").write_text("unsealed")
            with self.assertRaisesRegex(preview.InvalidPreview, "unexpected files"):
                preview.verify(self.root, dist, ref, sha)

    def test_preview_workflow_never_publishes_latest_or_main(self):
        root = Path(__file__).resolve().parents[1]
        workflow = (root / ".github/workflows/enterprise-preview-release.yml").read_text()
        self.assertIn("branches: [codex/enterprise-preview]", workflow)
        self.assertIn("refs/heads/codex/enterprise-preview", workflow)
        self.assertNotIn("--latest\n", workflow)
        self.assertNotIn("pull_request_target:", workflow)
        self.assertIn("needs.build.result == 'success'", workflow)
        self.assertNotIn("unpacked/zuno-enterprise smoke", workflow)
        self.assertIn("scripts/enterprise_artifact_smoke.py", workflow)
        self.assertIn('--archive "dist/$archive"', workflow)
        self.assertIn("--source-sha \"$GITHUB_SHA\"", workflow)
        self.assertIn("dist/*.smoke.json", workflow)
        self.assertIn("--signer-workflow", workflow)
        for name in ["release.yml", "release-candidate.yml", "publish-docs.yml"]:
            stable = (root / ".github/workflows" / name).read_text()
            self.assertIn("reject_preview:", stable)
            self.assertIn("!startsWith(github.ref, 'refs/heads/codex/enterprise-')", stable)

    def test_publication_checks_remote_digests_and_never_sets_latest(self):
        dist, ref, sha = self.candidate()
        with patch.dict(os.environ, {}, clear=True):
            manifest = preview.seal(self.root, dist, ref, sha)
            remote = FakeGitHub()
            with patch.object(preview, "GitHub", return_value=remote):
                result = preview.publish(self.root, dist, ref, sha, manifest["tag"])
        self.assertTrue(result["prerelease"])
        self.assertEqual(len(remote.release["assets"]), len(list(dist.iterdir())))
        for method, payload in remote.writes:
            self.assertIn(method, ["POST", "PATCH"])
            self.assertTrue(payload["prerelease"])
            self.assertEqual(payload["make_latest"], "false")

    def test_native_evidence_is_required_and_binds_packaged_binary_and_role_execution(self):
        dist, ref, sha = self.candidate()
        target = next(iter(preview.TARGETS))
        report = dist / preview.smoke_name(self.manifest, target)
        original = report.read_text()
        with patch.dict(os.environ, {}, clear=True):
            report.unlink()
            with self.assertRaisesRegex(preview.InvalidPreview, "smoke evidence"):
                preview.seal(self.root, dist, ref, sha)
            for change in [{"binarySha256": "f" * 64}, {"sourceSha": "f" * 40},
                           {"workspaceMerge": False}, {"roles": ["control"]}]:
                value = json.loads(original)
                value.update(change)
                report.write_text(json.dumps(value))
                with self.assertRaises(preview.InvalidPreview):
                    preview.seal(self.root, dist, ref, sha)
            report.write_text(original)
            preview.seal(self.root, dist, ref, sha)

    def test_public_release_and_mismatched_uploaded_assets_are_immutable(self):
        dist, ref, sha = self.candidate()
        with patch.dict(os.environ, {}, clear=True):
            manifest = preview.seal(self.root, dist, ref, sha)
            for public in [True, False]:
                remote = FakeGitHub()
                remote.release = {
                    "id": 1, "draft": not public, "prerelease": True,
                    "assets": [{"name": "SHA256SUMS", "size": 1, "digest": "sha256:wrong"}],
                }
                with patch.object(preview, "GitHub", return_value=remote):
                    with self.assertRaises(preview.InvalidPreview):
                        preview.publish(self.root, dist, ref, sha, manifest["tag"])
                self.assertEqual(remote.writes, [])
                self.assertEqual(remote.uploads, [])

    def test_server_asset_digest_mismatch_leaves_draft_unpublished(self):
        dist, ref, sha = self.candidate()
        with patch.dict(os.environ, {}, clear=True):
            manifest = preview.seal(self.root, dist, ref, sha)
            remote = FakeGitHub(corrupt_upload=True)
            with patch.object(preview, "GitHub", return_value=remote):
                with self.assertRaisesRegex(preview.InvalidPreview, "digest mismatch"):
                    preview.publish(self.root, dist, ref, sha, manifest["tag"])
        self.assertTrue(remote.release["draft"])
        self.assertEqual([method for method, _ in remote.writes], ["POST"])


class FakeGitHub:
    def __init__(self, corrupt_upload=False):
        self.repository = "example/zuno"
        self.release = None
        self.writes = []
        self.uploads = []
        self.corrupt_upload = corrupt_upload

    def request(self, path, method="GET", data=None):
        if path.startswith("git/ref/tags/"):
            return None
        if method == "POST":
            self.writes.append((method, data))
            self.release = {"id": 1, **data, "assets": []}
        elif method == "PATCH":
            self.writes.append((method, data))
            self.release.update(data)
        return deepcopy(self.release)

    def upload(self, tag, path):
        self.uploads.append((tag, path.name))
        self.release["assets"].append({
            "name": path.name,
            "size": path.stat().st_size,
            "digest": "sha256:" + (
                "wrong" if self.corrupt_upload else hashlib.sha256(path.read_bytes()).hexdigest()
            ),
        })


if __name__ == "__main__":
    unittest.main()
