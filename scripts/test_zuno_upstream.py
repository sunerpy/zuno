#!/usr/bin/env python3

import importlib.util
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest


SCRIPT = Path(__file__).with_name("zuno_upstream.py")
SPEC = importlib.util.spec_from_file_location("zuno_upstream", SCRIPT)
assert SPEC and SPEC.loader
zuno_upstream = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = zuno_upstream
SPEC.loader.exec_module(zuno_upstream)


def git(repo: Path, *args: str) -> str:
    return subprocess.run(
        ["git", *args],
        cwd=repo,
        text=True,
        check=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    ).stdout.strip()


class Repository:
    def __init__(self, root: Path) -> None:
        self.root = root
        git(root, "init", "-q")
        git(root, "config", "user.email", "zuno-test@example.invalid")
        git(root, "config", "user.name", "Zuno Test")

    def write(self, name: str, value: str) -> None:
        path = self.root / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(value, encoding="utf-8")

    def commit(self, message: str) -> str:
        git(self.root, "add", "--all")
        git(self.root, "commit", "-q", "-m", message)
        return git(self.root, "rev-parse", "HEAD")

    def tag_baseline(self, tag: str) -> tuple[str, str]:
        git(self.root, "tag", tag)
        return git(self.root, "rev-parse", "HEAD"), git(
            self.root, "rev-parse", "HEAD^{tree}"
        )

    def manifest(self, tag: str, commit: str, tree: str) -> None:
        self.write(
            "UPSTREAM_CODEX.toml",
            "\n".join(
                [
                    "schema_version = 1",
                    "[baseline]",
                    f'release_tag = "{tag}"',
                    f'release_commit = "{commit}"',
                    f'release_tree = "{tree}"',
                    "[sync]",
                    'remote = "upstream"',
                    "candidate_only = true",
                    "automatic_merge = false",
                    "",
                ]
            ),
        )


class ZunoUpstreamTest(unittest.TestCase):
    def test_latest_stable_ignores_alpha_and_sorts_semver(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            repo = Repository(Path(directory))
            repo.write("base", "base")
            repo.commit("base")
            for tag in ("rust-v0.9.0", "rust-v0.10.0", "rust-v0.11.0-alpha.1"):
                git(repo.root, "tag", tag)
            self.assertEqual(
                zuno_upstream.stable_tags(repo.root), ["rust-v0.9.0", "rust-v0.10.0"]
            )

    def test_plan_reports_descendant_release_and_overlap(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            repo = Repository(Path(directory))
            repo.write("shared", "base\n")
            repo.write("upstream-only", "base\n")
            repo.commit("base")
            base, tree = repo.tag_baseline("rust-v0.1.0")
            repo.manifest("rust-v0.1.0", base, tree)
            repo.commit("manifest")
            source_branch = git(repo.root, "branch", "--show-current")
            repo.write("shared", "zuno\n")
            repo.commit("zuno delta")
            source = git(repo.root, "rev-parse", "HEAD")

            git(repo.root, "checkout", "-q", "-b", "release-next", base)
            repo.write("shared", "upstream\n")
            repo.write("upstream-only", "next\n")
            repo.commit("upstream delta")
            git(repo.root, "tag", "rust-v0.2.0")
            git(repo.root, "checkout", "-q", source_branch)

            baseline = zuno_upstream.read_baseline(repo.root / "UPSTREAM_CODEX.toml")
            plan = zuno_upstream.make_plan(repo.root, baseline, source, "rust-v0.2.0")
            self.assertEqual(plan.relationship, "descendant")
            self.assertEqual(plan.overlapping_files, ["shared"])
            self.assertFalse(plan.source_dirty)

    def test_plan_rejects_same_or_older_release(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            repo = Repository(Path(directory))
            repo.write("old", "old\n")
            old = repo.commit("old")
            git(repo.root, "tag", "rust-v0.1.0", old)
            repo.write("base", "base\n")
            repo.commit("base")
            base, tree = repo.tag_baseline("rust-v0.2.0")
            repo.manifest("rust-v0.2.0", base, tree)
            repo.commit("manifest")
            baseline = zuno_upstream.read_baseline(repo.root / "UPSTREAM_CODEX.toml")

            for target, relation in (
                ("rust-v0.2.0", "same"),
                ("rust-v0.1.0", "older"),
            ):
                with self.subTest(target=target), self.assertRaisesRegex(
                    zuno_upstream.SyncError,
                    rf"strictly newer.*relationship is {relation}",
                ):
                    zuno_upstream.make_plan(repo.root, baseline, "HEAD", target)

            git(repo.root, "tag", "rust-v0.3.0", base)
            with self.assertRaisesRegex(
                zuno_upstream.SyncError, "does not advance baseline commit"
            ):
                zuno_upstream.make_plan(
                    repo.root, baseline, "HEAD", "rust-v0.3.0"
                )

    def test_plan_rejects_diverged_release_line(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            repo = Repository(Path(directory))
            repo.write("base", "base\n")
            repo.commit("base")
            base, tree = repo.tag_baseline("rust-v0.1.0")
            repo.manifest("rust-v0.1.0", base, tree)
            repo.commit("manifest")
            source = git(repo.root, "rev-parse", "HEAD")
            empty_tree = subprocess.run(
                ["git", "mktree"],
                cwd=repo.root,
                input="",
                text=True,
                check=True,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
            ).stdout.strip()
            diverged = git(repo.root, "commit-tree", empty_tree, "-m", "diverged")
            git(repo.root, "tag", "rust-v0.2.0", diverged)
            baseline = zuno_upstream.read_baseline(repo.root / "UPSTREAM_CODEX.toml")

            with self.assertRaisesRegex(
                zuno_upstream.SyncError, "not a descendant.*manual recovery"
            ):
                zuno_upstream.make_plan(
                    repo.root, baseline, source, "rust-v0.2.0"
                )

    def test_prepare_replays_delta_in_candidate_and_updates_manifest(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            repo_path = root / "repo"
            repo_path.mkdir()
            repo = Repository(repo_path)
            repo.write("upstream", "v1\n")
            repo.commit("base")
            base, tree = repo.tag_baseline("rust-v0.1.0")

            git(repo.root, "checkout", "-q", "-b", "release-next")
            repo.write("upstream", "v2\n")
            repo.commit("upstream next")
            git(repo.root, "tag", "rust-v0.2.0")
            target = git(repo.root, "rev-parse", "HEAD")
            target_tree = git(repo.root, "rev-parse", "HEAD^{tree}")

            git(repo.root, "checkout", "-q", "-b", "zuno", base)
            repo.manifest("rust-v0.1.0", base, tree)
            repo.write("zuno", "feature\n")
            repo.commit("zuno delta")
            source = git(repo.root, "rev-parse", "HEAD")
            baseline = zuno_upstream.read_baseline(repo.root / "UPSTREAM_CODEX.toml")
            plan = zuno_upstream.make_plan(repo.root, baseline, "zuno", "rust-v0.2.0")
            worktree = root / "candidate"
            prepared = zuno_upstream.prepare(
                repo.root,
                "UPSTREAM_CODEX.toml",
                plan,
                "upstream-sync/0.2.0",
                worktree,
            )

            self.assertEqual(git(repo.root, "rev-parse", "zuno"), source)
            self.assertEqual((worktree / "upstream").read_text(), "v2\n")
            self.assertEqual((worktree / "zuno").read_text(), "feature\n")
            values = zuno_upstream.read_baseline(worktree / "UPSTREAM_CODEX.toml")
            self.assertEqual(
                values,
                zuno_upstream.Baseline("rust-v0.2.0", target, target_tree),
            )
            self.assertTrue(prepared.manifest_updated)
            self.assertIn("UPSTREAM_CODEX.toml", git(worktree, "status", "--short"))

    def test_prepare_ignores_unrelated_legacy_parent_after_source_bridge(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            repo_path = root / "repo"
            repo_path.mkdir()
            repo = Repository(repo_path)
            repo.write("upstream", "v1\n")
            repo.commit("base")
            base, tree = repo.tag_baseline("rust-v0.1.0")

            git(repo.root, "checkout", "-q", "-b", "release-next")
            repo.write("upstream", "v2\n")
            repo.commit("upstream next")
            git(repo.root, "tag", "rust-v0.2.0")

            git(repo.root, "checkout", "-q", "-b", "zuno", base)
            repo.manifest("rust-v0.1.0", base, tree)
            repo.write("zuno", "feature\n")
            zuno_tip = repo.commit("zuno delta")
            source_tree = git(repo.root, "rev-parse", f"{zuno_tip}^{{tree}}")
            empty_tree = subprocess.run(
                ["git", "mktree"],
                cwd=repo.root,
                input="",
                text=True,
                check=True,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
            ).stdout.strip()
            legacy = git(repo.root, "commit-tree", empty_tree, "-m", "legacy root")
            bridge = git(
                repo.root,
                "commit-tree",
                source_tree,
                "-p",
                legacy,
                "-p",
                zuno_tip,
                "-m",
                "bridge legacy and Codex histories",
            )
            git(repo.root, "branch", "migration-main", bridge)

            baseline = zuno_upstream.read_baseline(repo.root / "UPSTREAM_CODEX.toml")
            plan = zuno_upstream.make_plan(
                repo.root, baseline, "migration-main", "rust-v0.2.0"
            )
            worktree = root / "candidate"
            prepared = zuno_upstream.prepare(
                repo.root,
                "UPSTREAM_CODEX.toml",
                plan,
                "upstream-sync/bridged-0.2.0",
                worktree,
            )

            self.assertEqual((worktree / "upstream").read_text(), "v2\n")
            self.assertEqual((worktree / "zuno").read_text(), "feature\n")
            self.assertFalse((worktree / "legacy-only").exists())
            self.assertEqual(prepared.candidate_base_commit, plan.target_commit)

    def test_prepare_refuses_dirty_source(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            repo = Repository(Path(directory))
            repo.write("base", "base\n")
            repo.commit("base")
            base, tree = repo.tag_baseline("rust-v0.1.0")
            repo.manifest("rust-v0.1.0", base, tree)
            repo.commit("manifest")
            source_branch = git(repo.root, "branch", "--show-current")
            git(repo.root, "checkout", "-q", "-b", "release-next", base)
            repo.write("base", "next\n")
            repo.commit("upstream next")
            git(repo.root, "tag", "rust-v0.2.0")
            git(repo.root, "checkout", "-q", source_branch)
            repo.write("untracked", "dirty\n")
            baseline = zuno_upstream.read_baseline(repo.root / "UPSTREAM_CODEX.toml")
            plan = zuno_upstream.make_plan(repo.root, baseline, "HEAD", "rust-v0.2.0")
            with self.assertRaisesRegex(
                zuno_upstream.SyncError, "source worktree is dirty"
            ):
                zuno_upstream.prepare(
                    repo.root,
                    "UPSTREAM_CODEX.toml",
                    plan,
                    "upstream-sync/0.1.0",
                    repo.root.parent / "candidate",
                )

    def test_cli_check_is_machine_readable(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            repo = Repository(Path(directory))
            repo.write("base", "base\n")
            repo.commit("base")
            base, tree = repo.tag_baseline("rust-v0.1.0")
            repo.manifest("rust-v0.1.0", base, tree)
            repo.commit("manifest")
            source_branch = git(repo.root, "branch", "--show-current")
            git(repo.root, "checkout", "-q", "-b", "release-next", base)
            repo.write("base", "next\n")
            repo.commit("upstream next")
            git(repo.root, "tag", "rust-v0.2.0")
            git(repo.root, "checkout", "-q", source_branch)
            result = subprocess.run(
                [
                    str(SCRIPT),
                    "--repo",
                    str(repo.root),
                    "--no-fetch",
                    "--json",
                    "check",
                ],
                text=True,
                check=True,
                stdout=subprocess.PIPE,
            )
            payload = json.loads(result.stdout)
            self.assertEqual(payload["target_tag"], "rust-v0.2.0")
            self.assertTrue(payload["candidate_only"])


if __name__ == "__main__":
    unittest.main()
