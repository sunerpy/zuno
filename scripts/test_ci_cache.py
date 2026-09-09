"""Filesystem and authority-boundary tests for optional compiler snapshots."""

import copy
import json
import os
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

import ci_cache as cache
import ci_cache_publish as publish


class SnapshotTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.compiler = self.root / "compiler"
        self.compiler.mkdir()
        (self.compiler / "a").mkdir()
        (self.compiler / "a/object").write_bytes(b"compiled object")
        self.slot = "release-x86_64-unknown-linux-musl"
        self.expected = {
            "repository": "example/zuno", "head_sha": "a" * 40,
            "tree_sha": "b" * 40, "run_id": 123, "run_attempt": 2,
        }
        files, size = cache.inventory(self.compiler)
        self.metadata = {
            "schema_version": 1, "slot": self.slot,
            "prefix": f"{cache.NAMESPACE}{self.slot}-" + "c" * 24,
            "sccache_version": cache.SCCACHE_VERSION,
            **self.expected, "files": files, "size": size,
        }
        self.save()

    def save(self):
        (self.root / "metadata.json").write_text(
            json.dumps(self.metadata), encoding="utf-8"
        )

    def validate(self):
        return cache.validate_snapshot(self.root, self.slot, self.expected)

    def test_verified_bytes_and_source_produce_an_immutable_key(self):
        self.assertEqual(
            self.validate(),
            self.metadata["prefix"] + "-" + "a" * 40 + "-123-2",
        )

    def test_changed_or_additional_cache_objects_are_rejected(self):
        (self.compiler / "a/object").write_bytes(b"tampered")
        with self.assertRaises(ValueError):
            self.validate()
        (self.compiler / "a/object").write_bytes(b"compiled object")
        (self.compiler / "extra").write_bytes(b"unrecorded")
        with self.assertRaises(ValueError):
            self.validate()

    def test_each_source_identity_is_checked(self):
        for field in self.expected:
            with self.subTest(field=field):
                expected = dict(self.expected)
                expected[field] = "wrong" if isinstance(expected[field], str) else 99
                with self.assertRaises(ValueError):
                    cache.validate_snapshot(self.root, self.slot, expected)

    def test_paths_are_portable_and_cannot_escape(self):
        for value in ["../outside", "/outside", "a\\b", "a/../b", "a//b",
                      "C:/outside", "a:stream", "NUL", "a/CON.txt",
                      "a/end.", "a/end ", "a/new\nline"]:
            with self.subTest(path=value), self.assertRaises(ValueError):
                cache.checked_path(value)

    def test_links_and_devices_are_not_read_as_cache_objects(self):
        outside = self.root / "outside"
        outside.write_bytes(b"private")
        link = self.compiler / "link"
        try:
            link.symlink_to(outside)
        except OSError as error:
            self.skipTest(f"symlink creation unavailable: {error}")
        with self.assertRaises(ValueError):
            cache.inventory(self.compiler)

    def test_snapshot_size_and_count_are_bounded(self):
        with patch.object(cache, "MAX_BYTES", 2), self.assertRaises(ValueError):
            cache.inventory(self.compiler)
        (self.compiler / "another").write_bytes(b"x")
        with patch.object(cache, "MAX_FILES", 1), self.assertRaises(ValueError):
            cache.inventory(self.compiler)

    def test_wrong_namespace_and_boolean_identity_fail_closed(self):
        self.metadata["prefix"] = "some-other-cache"
        self.save()
        with self.assertRaises(ValueError):
            self.validate()
        self.metadata["prefix"] = f"{cache.NAMESPACE}{self.slot}-" + "c" * 24
        self.metadata["run_attempt"] = True
        self.save()
        with self.assertRaises(ValueError):
            self.validate()

    def test_key_tracks_profile_and_compiler_but_never_credentials(self):
        base = cache.key_prefix("pr-linux-tests", "rustc 1", {"test": {}}, "", {})
        self.assertNotEqual(
            base, cache.key_prefix("pr-linux-tests", "rustc 2", {"test": {}}, "", {})
        )
        self.assertNotEqual(
            base, cache.key_prefix("pr-linux-tests", "rustc 1", {"test": {"debug": 0}}, "", {})
        )
        secret_env = {"GITHUB_TOKEN": "sentinel-secret", "CARGO_REGISTRIES_TOKEN": "secret"}
        self.assertEqual(
            base, cache.key_prefix("pr-linux-tests", "rustc 1", {"test": {}}, "", secret_env)
        )

    def test_version_only_manifest_change_keeps_dependency_restore_prefix(self):
        original = Path.cwd()
        os.chdir(self.root)
        try:
            with patch("ci_cache.subprocess.check_output", return_value="rustc 1.98"):
                Path("Cargo.toml").write_text('[workspace.package]\nversion = "0.10.23"\n')
                first = cache.current_prefix("pr-linux-tests")
                Path("Cargo.toml").write_text('[workspace.package]\nversion = "0.10.24"\n')
                self.assertEqual(first, cache.current_prefix("pr-linux-tests"))
        finally:
            os.chdir(original)

    def test_retention_only_deletes_older_main_entries_in_our_slot(self):
        prefix = f"{cache.NAMESPACE}pr-linux-tests-"
        entries = [
            {"id": 1, "key": prefix + "old", "ref": "refs/heads/main", "created_at": "2026-01-01"},
            {"id": 2, "key": prefix + "current", "ref": "refs/heads/main", "created_at": "2026-01-02"},
            {"id": 3, "key": prefix + "newer", "ref": "refs/heads/main", "created_at": "2026-01-03"},
            {"id": 4, "key": prefix + "pr", "ref": "refs/pull/7/merge", "created_at": "2026-01-01"},
            {"id": 5, "key": "unrelated", "ref": "refs/heads/main", "created_at": "2026-01-01"},
        ]
        self.assertEqual(cache.obsolete_caches(entries, "pr-linux-tests", prefix + "current"), [1])
        self.assertEqual(cache.obsolete_caches(entries, "pr-linux-tests", prefix + "not-visible"), [])


class FakeGitHub:
    def __init__(self):
        repo = {"full_name": "example/zuno"}
        self.data = {
            "repos/example/zuno/actions/runs/101": {
                "path": ".github/workflows/release.yml", "event": "push",
                "head_branch": "main", "status": "completed", "conclusion": "success",
                "run_attempt": 3, "head_sha": "d" * 40,
                "repository": repo, "head_repository": repo,
            },
            "repos/example/zuno/actions/runs/202": {
                "path": ".github/workflows/release-candidate.yml",
                "event": "workflow_dispatch", "status": "completed", "conclusion": "success",
                "run_attempt": 2, "head_sha": "b" * 40,
                "repository": repo, "head_repository": repo,
            },
            "repos/example/zuno/releases/tags/v0.10.23": {
                "draft": False, "tag_name": "v0.10.23", "published_at": "2026-09-09",
            },
            "repos/example/zuno/git/ref/tags/v0.10.23": {
                "object": {"type": "commit", "sha": "a" * 40},
            },
            "repos/example/zuno/git/commits/" + "a" * 40: {"tree": {"sha": "c" * 40}},
            "repos/example/zuno/git/commits/" + "b" * 40: {"tree": {"sha": "c" * 40}},
        }
        self.artifact_data = {
            101: [{"id": 11, "name": "compiler-cache-handoff-3",
                   "expired": False, "size_in_bytes": 1000}],
            202: [{"id": 22, "name": "compiler-cache-release-x86_64-unknown-linux-musl-2",
                   "expired": False, "size_in_bytes": 1000}],
        }

    def get(self, path):
        return copy.deepcopy(self.data[path])

    def artifacts(self, _repository, run_id):
        return copy.deepcopy(self.artifact_data[run_id])


class PublicationTests(unittest.TestCase):
    def setUp(self):
        self.api = FakeGitHub()
        self.handoff = {
            "schema_version": 1, "repository": "example/zuno",
            "release_run_id": 101, "release_run_attempt": 3,
            "release_workflow_sha": "d" * 40,
            "release_sha": "a" * 40, "release_tag": "v0.10.23",
            "candidate_run_id": 202, "candidate_run_attempt": 2,
            "candidate_head_sha": "b" * 40, "tree_sha": "c" * 40,
        }

    def plan(self):
        return publish.plan(self.api, self.handoff, "example/zuno", 101, 3, "d" * 40)

    def test_exact_published_candidate_yields_only_its_available_cache_slots(self):
        self.assertEqual(
            publish.find_handoff(self.api, "example/zuno", 101, 3, "d" * 40), 11
        )
        matrix, expected = self.plan()
        self.assertEqual(matrix, [{"slot": "release-x86_64-unknown-linux-musl", "artifact_id": 22}])
        self.assertEqual(expected, {
            "repository": "example/zuno", "head_sha": "b" * 40,
            "tree_sha": "c" * 40, "run_id": 202, "run_attempt": 2,
        })

    def test_non_release_main_run_without_handoff_is_a_noop(self):
        self.api.artifact_data[101] = []
        self.assertIsNone(publish.find_handoff(self.api, "example/zuno", 101, 3, "d" * 40))

    def test_dispatched_writer_waits_for_the_same_parent_attempt_to_finish(self):
        path = "repos/example/zuno/actions/runs/101"
        complete = self.api.data[path]
        running = {**complete, "status": "in_progress", "conclusion": None}
        with patch.object(self.api, "get", side_effect=[running, complete]), \
             patch("ci_cache_publish.time.sleep") as sleep:
            self.assertEqual(
                publish.find_handoff(
                    self.api, "example/zuno", 101, 3, "d" * 40, wait_seconds=10
                ), 11
            )
            sleep.assert_called_once()

    def test_waiting_parent_timeout_or_new_attempt_cannot_publish(self):
        path = "repos/example/zuno/actions/runs/101"
        self.api.data[path].update(status="in_progress", conclusion=None)
        with self.assertRaises(TimeoutError):
            publish.find_handoff(self.api, "example/zuno", 101, 3, "d" * 40)
        with patch("ci_cache_publish.time.sleep") as sleep:
            self.api.data[path]["run_attempt"] = 4
            with self.assertRaises(ValueError):
                publish.find_handoff(
                    self.api, "example/zuno", 101, 3, "d" * 40, wait_seconds=10
                )
            sleep.assert_not_called()

    def test_handoff_writer_preserves_the_exact_release_and_candidate_identity(self):
        environment = {
            "GITHUB_REF": "refs/heads/main",
            "GITHUB_REPOSITORY": "example/zuno",
            "GITHUB_RUN_ID": "101",
            "GITHUB_RUN_ATTEMPT": "3",
            "GITHUB_SHA": "d" * 40,
            "CACHE_RELEASE_SHA": "a" * 40,
            "CACHE_RELEASE_TAG": "v0.10.23",
            "CACHE_CANDIDATE_RUN_ID": "202",
            "CACHE_CANDIDATE_RUN_ATTEMPT": "2",
            "CACHE_CANDIDATE_HEAD_SHA": "b" * 40,
            "CACHE_TREE_SHA": "c" * 40,
        }
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "handoff.json"
            publish.write_handoff(path, environment)
            self.assertEqual(json.loads(path.read_text(encoding="utf-8")), self.handoff)
            environment["GITHUB_REF"] = "refs/pull/7/merge"
            with self.assertRaises(ValueError):
                publish.write_handoff(path, environment)

    def test_pr_fork_failure_or_other_workflow_cannot_publish_to_main(self):
        path = "repos/example/zuno/actions/runs/101"
        original = copy.deepcopy(self.api.data[path])
        for field, value in [
            ("event", "pull_request"), ("path", ".github/workflows/ci.yml"),
            ("head_branch", "feature"), ("conclusion", "failure"),
            ("run_attempt", 4), ("head_sha", "e" * 40),
            ("head_repository", {"full_name": "attacker/zuno"}),
        ]:
            with self.subTest(field=field):
                self.api.data[path] = {**original, field: value}
                with self.assertRaises(ValueError):
                    self.plan()

    def test_draft_retargeted_tag_and_different_tree_are_rejected(self):
        for path, changed in [
            ("repos/example/zuno/releases/tags/v0.10.23",
             {"draft": True, "tag_name": "v0.10.23", "published_at": None}),
            ("repos/example/zuno/git/ref/tags/v0.10.23",
             {"object": {"type": "commit", "sha": "e" * 40}}),
            ("repos/example/zuno/git/commits/" + "b" * 40,
             {"tree": {"sha": "e" * 40}}),
        ]:
            with self.subTest(path=path):
                original = self.api.data[path]
                self.api.data[path] = changed
                with self.assertRaises(ValueError):
                    self.plan()
                self.api.data[path] = original

    def test_failed_or_rerun_candidate_is_not_substituted(self):
        self.api.data["repos/example/zuno/actions/runs/202"]["conclusion"] = "failure"
        with self.assertRaises(ValueError):
            self.plan()
        self.api.data["repos/example/zuno/actions/runs/202"]["conclusion"] = "success"
        self.api.data["repos/example/zuno/actions/runs/202"]["run_attempt"] = 3
        with self.assertRaises(ValueError):
            self.plan()

    def test_duplicate_cache_artifacts_fail_instead_of_selecting_latest(self):
        self.api.artifact_data[202].append({**self.api.artifact_data[202][0], "id": 23})
        with self.assertRaises(ValueError):
            self.plan()

    def test_expired_or_absent_optional_caches_do_not_invalidate_the_release(self):
        self.api.artifact_data[202][0]["expired"] = True
        self.assertEqual(self.plan()[0], [])

    def test_handoff_producer_and_schema_cannot_be_forged(self):
        for field, value in [
            ("release_run_id", 102), ("release_workflow_sha", "e" * 40),
            ("candidate_run_attempt", True), ("schema_version", 2),
        ]:
            with self.subTest(field=field):
                original = self.handoff[field]
                self.handoff[field] = value
                with self.assertRaises(ValueError):
                    self.plan()
                self.handoff[field] = original


if __name__ == "__main__":
    unittest.main()
