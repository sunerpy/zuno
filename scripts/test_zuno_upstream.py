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
    def test_target_policy_next_replays_releases_in_order(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            repo = Repository(Path(directory))
            repo.write("file", "base\n")
            repo.commit("base")
            base, tree = repo.tag_baseline("rust-v0.9.0")
            for tag in ["rust-v0.10.0", "rust-v0.10.1", "rust-v0.11.0", "rust-v0.11.0-alpha.3"]:
                repo.write("file", f"{tag}\n")
                repo.commit(tag)
                git(repo.root, "tag", tag)
            repo.manifest("rust-v0.10.0", base, tree)
            baseline = zuno_upstream.read_baseline(repo.root / "UPSTREAM_CODEX.toml")
            self.assertEqual(
                zuno_upstream.exact_target(repo.root, None, baseline, "next"), "rust-v0.10.1"
            )
            self.assertEqual(
                zuno_upstream.exact_target(repo.root, None, baseline, "newest"), "rust-v0.11.0"
            )
            # Nothing newer than the baseline: both policies report the newest tag
            # so `check --allow-current` can say the baseline is current.
            repo.manifest("rust-v0.11.0", base, tree)
            current = zuno_upstream.read_baseline(repo.root / "UPSTREAM_CODEX.toml")
            self.assertEqual(
                zuno_upstream.exact_target(repo.root, None, current, "next"), "rust-v0.11.0"
            )
            self.assertEqual(
                zuno_upstream.exact_target(repo.root, "rust-v0.10.0", baseline, "next"),
                "rust-v0.10.0",
            )
            with self.assertRaises(zuno_upstream.SyncError):
                zuno_upstream.exact_target(repo.root, None, baseline, "latest")
            # An open candidate for a newer release is kept (releases between the
            # baseline and it are superseded); an older or unknown one is ignored.
            self.assertEqual(
                zuno_upstream.exact_target(
                    repo.root, None, baseline, "next", ["rust-v0.11.0"]
                ),
                "rust-v0.11.0",
            )
            self.assertEqual(
                zuno_upstream.exact_target(
                    repo.root, None, baseline, "next", ["rust-v0.9.0", "rust-v0.99.0"]
                ),
                "rust-v0.10.1",
            )
            self.assertEqual(
                zuno_upstream.exact_target(
                    repo.root, "rust-v0.10.1", baseline, "next", ["rust-v0.11.0"]
                ),
                "rust-v0.10.1",
            )
            with self.assertRaises(zuno_upstream.SyncError):
                zuno_upstream.exact_target(repo.root, None, baseline, "next", ["0.11.0"])

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
                zuno_upstream.SyncError, "shares no history.*manual recovery"
            ):
                zuno_upstream.make_plan(
                    repo.root, baseline, source, "rust-v0.2.0"
                )

    def test_plan_accepts_sibling_release_branch_and_lists_baseline_only_commits(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            repo_path = root / "repo"
            repo_path.mkdir()
            repo = Repository(repo_path)
            repo.write("shared", "base\n")
            trunk = repo.commit("trunk base")
            trunk_branch = git(repo.root, "branch", "--show-current")

            # Codex release branches: a release commit on top of trunk per tag.
            git(repo.root, "checkout", "-q", "-b", "release-0.1", trunk)
            repo.write("CHANGELOG", "0.1.0\n")
            base = repo.commit("## New Features 0.1.0")
            base_tree = git(repo.root, "rev-parse", "HEAD^{tree}")
            git(repo.root, "tag", "rust-v0.1.0")

            git(repo.root, "checkout", "-q", trunk_branch)
            repo.write("shared", "trunk change\n")
            repo.commit("trunk moves on")
            git(repo.root, "checkout", "-q", "-b", "release-0.2")
            repo.write("CHANGELOG", "0.2.0\n")
            repo.commit("## New Features 0.2.0")
            git(repo.root, "tag", "rust-v0.2.0")

            git(repo.root, "checkout", "-q", "-b", "zuno", base)
            repo.manifest("rust-v0.1.0", base, base_tree)
            repo.write("zuno", "feature\n")
            source = repo.commit("zuno delta")

            baseline = zuno_upstream.read_baseline(repo.root / "UPSTREAM_CODEX.toml")
            plan = zuno_upstream.make_plan(repo.root, baseline, source, "rust-v0.2.0")
            self.assertEqual(plan.relationship, "release-branch")
            self.assertEqual(plan.merge_base, trunk)
            self.assertEqual(
                plan.baseline_only_commits, [f"{base} ## New Features 0.1.0"]
            )
            self.assertEqual(plan.overlapping_files, [])

            worktree = root / "candidate"
            prepared = zuno_upstream.prepare(
                repo.root, "UPSTREAM_CODEX.toml", plan, "upstream-sync/0.2.0", worktree
            )
            self.assertEqual(prepared.candidate_base_commit, plan.target_commit)
            self.assertEqual((worktree / "shared").read_text(), "trunk change\n")
            self.assertEqual((worktree / "CHANGELOG").read_text(), "0.2.0\n")
            self.assertEqual((worktree / "zuno").read_text(), "feature\n")

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

    def test_cli_check_allow_current_reports_current_baseline(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            repo = Repository(Path(directory))
            repo.write("base", "base\n")
            repo.commit("base")
            base, tree = repo.tag_baseline("rust-v0.2.0")
            git(repo.root, "tag", "rust-v0.3.0-alpha.1")
            repo.manifest("rust-v0.2.0", base, tree)
            repo.commit("manifest")
            strict = subprocess.run(
                [str(SCRIPT), "--repo", str(repo.root), "--no-fetch", "--json", "check"],
                text=True,
                check=False,
                stdout=subprocess.PIPE,
            )
            self.assertEqual(strict.returncode, 1)
            self.assertIn("strictly newer", json.loads(strict.stdout)["error"])
            relaxed = subprocess.run(
                [
                    str(SCRIPT),
                    "--repo",
                    str(repo.root),
                    "--no-fetch",
                    "--json",
                    "check",
                    "--allow-current",
                ],
                text=True,
                check=True,
                stdout=subprocess.PIPE,
            )
            payload = json.loads(relaxed.stdout)
            self.assertEqual(payload["status"], "current")
            self.assertEqual(payload["baseline_tag"], "rust-v0.2.0")
            self.assertEqual(payload["latest_stable_tag"], "rust-v0.2.0")
            self.assertFalse(payload["source_dirty"])

    def test_cli_check_allow_current_still_plans_newer_release(self) -> None:
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
                    "--allow-current",
                ],
                text=True,
                check=True,
                stdout=subprocess.PIPE,
            )
            payload = json.loads(result.stdout)
            self.assertNotIn("status", payload)
            self.assertEqual(payload["target_tag"], "rust-v0.2.0")
            self.assertEqual(payload["relationship"], "descendant")

    def test_prepare_reports_conflicting_files_and_keeps_candidate(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            repo_path = root / "repo"
            repo_path.mkdir()
            repo = Repository(repo_path)
            repo.write("shared", "base\n")
            repo.write("other", "base\n")
            repo.commit("base")
            base, tree = repo.tag_baseline("rust-v0.1.0")

            git(repo.root, "checkout", "-q", "-b", "release-next")
            repo.write("shared", "upstream\n")
            repo.commit("upstream next")
            git(repo.root, "tag", "rust-v0.2.0")

            git(repo.root, "checkout", "-q", "-b", "zuno", base)
            repo.manifest("rust-v0.1.0", base, tree)
            repo.write("shared", "zuno\n")
            repo.write("other", "zuno\n")
            source = repo.commit("zuno delta")
            baseline = zuno_upstream.read_baseline(repo.root / "UPSTREAM_CODEX.toml")
            plan = zuno_upstream.make_plan(repo.root, baseline, "zuno", "rust-v0.2.0")
            self.assertEqual(plan.overlapping_files, ["shared"])
            worktree = root / "candidate"
            with self.assertRaises(zuno_upstream.SyncError) as raised:
                zuno_upstream.prepare(
                    repo.root, "UPSTREAM_CODEX.toml", plan, "upstream-sync/0.2.0", worktree
                )
            details = raised.exception.details
            self.assertEqual(details["status"], "conflict")
            self.assertEqual(details["conflicting_files"], ["shared"])
            self.assertEqual(details["candidate_branch"], "upstream-sync/0.2.0")
            self.assertEqual(details["candidate_worktree"], str(worktree))
            self.assertEqual(git(repo.root, "rev-parse", "zuno"), source)
            self.assertTrue(worktree.is_dir())
            self.assertIn("<<<<<<<", (worktree / "shared").read_text())
            self.assertEqual((worktree / "other").read_text(), "zuno\n")

    def test_cli_prepare_conflict_is_machine_readable(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            repo_path = root / "repo"
            repo_path.mkdir()
            repo = Repository(repo_path)
            repo.write("shared", "base\n")
            repo.commit("base")
            base, tree = repo.tag_baseline("rust-v0.1.0")
            git(repo.root, "checkout", "-q", "-b", "release-next")
            repo.write("shared", "upstream\n")
            repo.commit("upstream next")
            git(repo.root, "tag", "rust-v0.2.0")
            git(repo.root, "checkout", "-q", "-b", "zuno", base)
            repo.manifest("rust-v0.1.0", base, tree)
            repo.write("shared", "zuno\n")
            repo.commit("zuno delta")
            result = subprocess.run(
                [
                    str(SCRIPT),
                    "--repo",
                    str(repo.root),
                    "--no-fetch",
                    "--json",
                    "prepare",
                    "--source",
                    "zuno",
                    "--target",
                    "rust-v0.2.0",
                    "--worktree",
                    str(root / "candidate"),
                ],
                text=True,
                check=False,
                stdout=subprocess.PIPE,
            )
            self.assertEqual(result.returncode, 1)
            payload = json.loads(result.stdout)
            self.assertEqual(payload["status"], "conflict")
            self.assertEqual(payload["conflicting_files"], ["shared"])
            self.assertIn("conflicts", payload["error"])

    def test_prepare_resets_generated_conflicts_to_upstream_and_reports_them(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            repo_path = root / "repo"
            repo_path.mkdir()
            repo = Repository(repo_path)
            repo.write("codex-rs/Cargo.lock", "base\n")
            repo.write("codex-rs/app-server-protocol/schema/json/A.json", "base\n")
            repo.write("src.rs", "base\n")
            repo.commit("base")
            base, tree = repo.tag_baseline("rust-v0.1.0")

            git(repo.root, "checkout", "-q", "-b", "release-next")
            repo.write("codex-rs/Cargo.lock", "upstream\n")
            repo.write("codex-rs/app-server-protocol/schema/json/A.json", "upstream\n")
            repo.commit("upstream next")
            git(repo.root, "tag", "rust-v0.2.0")

            git(repo.root, "checkout", "-q", "-b", "zuno", base)
            repo.manifest("rust-v0.1.0", base, tree)
            repo.write("codex-rs/Cargo.lock", "zuno\n")
            repo.write("codex-rs/app-server-protocol/schema/json/A.json", "zuno\n")
            repo.write("src.rs", "zuno\n")
            repo.commit("zuno delta")
            baseline = zuno_upstream.read_baseline(repo.root / "UPSTREAM_CODEX.toml")
            plan = zuno_upstream.make_plan(repo.root, baseline, "zuno", "rust-v0.2.0")
            worktree = root / "candidate"
            prepared = zuno_upstream.prepare(
                repo.root, "UPSTREAM_CODEX.toml", plan, "upstream-sync/0.2.0", worktree
            )
            self.assertEqual(
                prepared.regenerate_paths,
                [
                    "codex-rs/Cargo.lock",
                    "codex-rs/app-server-protocol/schema/json/A.json",
                ],
            )
            self.assertEqual((worktree / "codex-rs/Cargo.lock").read_text(), "upstream\n")
            self.assertEqual(
                (worktree / "codex-rs/app-server-protocol/schema/json/A.json").read_text(),
                "upstream\n",
            )
            self.assertEqual((worktree / "src.rs").read_text(), "zuno\n")
            self.assertEqual(git(worktree, "diff", "--name-only", "--diff-filter=U"), "")

    def test_prepare_reports_source_conflicts_separately_from_generated(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            repo_path = root / "repo"
            repo_path.mkdir()
            repo = Repository(repo_path)
            repo.write("codex-rs/Cargo.lock", "base\n")
            repo.write("src.rs", "base\n")
            repo.commit("base")
            base, tree = repo.tag_baseline("rust-v0.1.0")
            git(repo.root, "checkout", "-q", "-b", "release-next")
            repo.write("codex-rs/Cargo.lock", "upstream\n")
            repo.write("src.rs", "upstream\n")
            repo.commit("upstream next")
            git(repo.root, "tag", "rust-v0.2.0")
            git(repo.root, "checkout", "-q", "-b", "zuno", base)
            repo.manifest("rust-v0.1.0", base, tree)
            repo.write("codex-rs/Cargo.lock", "zuno\n")
            repo.write("src.rs", "zuno\n")
            repo.commit("zuno delta")
            baseline = zuno_upstream.read_baseline(repo.root / "UPSTREAM_CODEX.toml")
            plan = zuno_upstream.make_plan(repo.root, baseline, "zuno", "rust-v0.2.0")
            with self.assertRaises(zuno_upstream.SyncError) as raised:
                zuno_upstream.prepare(
                    repo.root, "UPSTREAM_CODEX.toml", plan, "upstream-sync/0.2.0", root / "candidate"
                )
            self.assertEqual(raised.exception.details["conflicting_files"], ["src.rs"])
            self.assertEqual(
                raised.exception.details["generated_conflicts"], ["codex-rs/Cargo.lock"]
            )

    def test_prepare_follows_upstream_renames_and_reports_modify_delete(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            repo_path = root / "repo"
            repo_path.mkdir()
            repo = Repository(repo_path)
            repo.write("renamed_by_upstream.rs", "line1\nline2\nline3\nline4\nline5\n")
            repo.write("deleted_by_upstream.rs", "gone\n")
            repo.commit("base")
            base, tree = repo.tag_baseline("rust-v0.1.0")

            git(repo.root, "checkout", "-q", "-b", "release-next")
            git(repo.root, "mv", "renamed_by_upstream.rs", "new_name.rs")
            git(repo.root, "rm", "-q", "deleted_by_upstream.rs")
            repo.commit("upstream renames and deletes")
            git(repo.root, "tag", "rust-v0.2.0")

            git(repo.root, "checkout", "-q", "-b", "zuno", base)
            repo.manifest("rust-v0.1.0", base, tree)
            repo.write("renamed_by_upstream.rs", "line1\nline2\nzuno\nline4\nline5\n")
            repo.write("deleted_by_upstream.rs", "zuno keeps this\n")
            repo.commit("zuno delta")
            baseline = zuno_upstream.read_baseline(repo.root / "UPSTREAM_CODEX.toml")
            plan = zuno_upstream.make_plan(repo.root, baseline, "zuno", "rust-v0.2.0")
            worktree = root / "candidate"
            with self.assertRaises(zuno_upstream.SyncError) as raised:
                zuno_upstream.prepare(
                    repo.root, "UPSTREAM_CODEX.toml", plan, "upstream-sync/0.2.0", worktree
                )
            details = raised.exception.details
            self.assertEqual(details["conflicting_files"], ["deleted_by_upstream.rs"])
            self.assertTrue(
                any("modify/delete" in message for message in details["conflict_messages"]),
                details["conflict_messages"],
            )
            # The Zuno edit followed the upstream rename instead of aborting.
            self.assertFalse((worktree / "renamed_by_upstream.rs").exists())
            self.assertEqual(
                (worktree / "new_name.rs").read_text(), "line1\nline2\nzuno\nline4\nline5\n"
            )
            self.assertEqual((worktree / "deleted_by_upstream.rs").read_text(), "zuno keeps this\n")

    def test_finalize_commits_merge_of_source_and_release(self) -> None:
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
            git(repo.root, "checkout", "-q", "-b", "zuno", base)
            repo.manifest("rust-v0.1.0", base, tree)
            repo.write("zuno", "feature\n")
            source = repo.commit("zuno delta")
            baseline = zuno_upstream.read_baseline(repo.root / "UPSTREAM_CODEX.toml")
            plan = zuno_upstream.make_plan(repo.root, baseline, "zuno", "rust-v0.2.0")
            worktree = root / "candidate"
            zuno_upstream.prepare(
                repo.root, "UPSTREAM_CODEX.toml", plan, "upstream-sync/0.2.0", worktree
            )
            (worktree / "regenerated").write_text("derived\n")

            finalized = zuno_upstream.finalize(
                worktree, "UPSTREAM_CODEX.toml", None, None
            )

            self.assertEqual(finalized.branch, "upstream-sync/0.2.0")
            self.assertEqual(finalized.source_commit, source)
            self.assertEqual(finalized.target_commit, target)
            self.assertEqual(
                git(worktree, "show", "-s", "--format=%P", finalized.commit).split(),
                [source, target],
            )
            self.assertEqual(git(worktree, "rev-parse", "HEAD"), finalized.commit)
            self.assertEqual(git(worktree, "status", "--short"), "")
            self.assertEqual(git(worktree, "show", "HEAD:upstream"), "v2")
            self.assertEqual(git(worktree, "show", "HEAD:zuno"), "feature")
            self.assertEqual(git(worktree, "show", "HEAD:regenerated"), "derived")
            body = git(worktree, "show", "-s", "--format=%B", finalized.commit)
            self.assertIn("chore(upstream): merge Codex 0.2.0 into Zuno", body)
            self.assertIn(f"Zuno-Source-Commit: {source}", body)
            self.assertIn("Zuno-Upstream-Tag: rust-v0.2.0", body)
            manifest = zuno_upstream.read_baseline(worktree / "UPSTREAM_CODEX.toml")
            self.assertEqual(manifest.release_tag, "rust-v0.2.0")
            # The source branch is untouched and the release stays reachable.
            self.assertEqual(git(repo.root, "rev-parse", "zuno"), source)
            self.assertTrue(zuno_upstream.is_ancestor(worktree, target, finalized.commit))
            # A later sync from the merged result sees only the Zuno delta.
            next_baseline = zuno_upstream.read_baseline(worktree / "UPSTREAM_CODEX.toml")
            self.assertEqual(
                zuno_upstream.changed_files(worktree, next_baseline.release_commit, finalized.commit),
                {"UPSTREAM_CODEX.toml", "zuno", "regenerated"},
            )

    def test_finalize_refuses_markers_then_accepts_resolved_conflict(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            repo_path = root / "repo"
            repo_path.mkdir()
            repo = Repository(repo_path)
            repo.write("shared", "base\n")
            repo.commit("base")
            base, tree = repo.tag_baseline("rust-v0.1.0")
            git(repo.root, "checkout", "-q", "-b", "release-next")
            repo.write("shared", "upstream\n")
            repo.commit("upstream next")
            git(repo.root, "tag", "rust-v0.2.0")
            target = git(repo.root, "rev-parse", "HEAD")
            git(repo.root, "checkout", "-q", "-b", "zuno", base)
            repo.manifest("rust-v0.1.0", base, tree)
            repo.write("shared", "zuno\n")
            source = repo.commit("zuno delta")
            baseline = zuno_upstream.read_baseline(repo.root / "UPSTREAM_CODEX.toml")
            plan = zuno_upstream.make_plan(repo.root, baseline, "zuno", "rust-v0.2.0")
            worktree = root / "candidate"
            with self.assertRaises(zuno_upstream.SyncError) as raised:
                zuno_upstream.prepare(
                    repo.root, "UPSTREAM_CODEX.toml", plan, "upstream-sync/0.2.0", worktree
                )
            self.assertTrue(raised.exception.details["manifest_updated"])
            # The new baseline is already recorded despite the conflict.
            self.assertEqual(
                zuno_upstream.read_baseline(worktree / "UPSTREAM_CODEX.toml").release_tag,
                "rust-v0.2.0",
            )
            with self.assertRaises(zuno_upstream.SyncError) as unresolved:
                zuno_upstream.finalize(worktree, "UPSTREAM_CODEX.toml", None, None)
            self.assertEqual(unresolved.exception.details["conflicting_files"], ["shared"])

            (worktree / "shared").write_text("resolved\n")
            # A source that moved on since prepare must not be attached silently.
            git(repo.root, "checkout", "-q", "zuno")
            repo.write("later", "moved on\n")
            moved = repo.commit("zuno moved on")
            with self.assertRaisesRegex(zuno_upstream.SyncError, "prepared from"):
                zuno_upstream.finalize(worktree, "UPSTREAM_CODEX.toml", moved, None)
            finalized = zuno_upstream.finalize(worktree, "UPSTREAM_CODEX.toml", None, None)
            self.assertEqual(
                git(worktree, "show", "-s", "--format=%P", finalized.commit).split(),
                [source, target],
            )
            self.assertEqual(git(worktree, "show", "HEAD:shared"), "resolved")
            self.assertEqual(git(worktree, "status", "--short"), "")

    def test_cli_finalize_is_machine_readable(self) -> None:
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
            source = repo.commit("zuno delta")
            worktree = root / "candidate"
            subprocess.run(
                [str(SCRIPT), "--repo", str(repo.root), "--no-fetch", "--json", "prepare",
                 "--source", "zuno", "--target", "rust-v0.2.0", "--worktree", str(worktree)],
                text=True, check=True, stdout=subprocess.PIPE,
            )
            result = subprocess.run(
                [str(SCRIPT), "--json", "finalize", "--worktree", str(worktree),
                 "--source", source, "--message", "chore(upstream): custom subject"],
                text=True, check=True, stdout=subprocess.PIPE,
            )
            payload = json.loads(result.stdout)
            self.assertEqual(payload["source_commit"], source)
            self.assertEqual(payload["target_tag"], "rust-v0.2.0")
            self.assertEqual(
                git(worktree, "show", "-s", "--format=%s", payload["commit"]),
                "chore(upstream): custom subject",
            )


REBRAND_RULES = "\n".join(
    [
        "schema_version = 1",
        'protect = ["Codex Desktop"]',
        "[[added]]",
        'prefix = "snapshots/"',
        'suffix = ".snap"',
        "[[rule]]",
        'find = "Codex"',
        'replace = "Zuno"',
        "[[rule]]",
        'regex = \'\\(v0\\.0\\.0\\)\'',
        'replace = "(v${version})"',
        'paths = ["snapshots/"]',
        "",
    ]
)


def workspace_manifest(version: str) -> str:
    return f'[workspace.package]\nversion = "{version}"\n'


class RebrandReplayTest(unittest.TestCase):
    """``prepare`` replays FORK_REBRAND.toml onto rename-only conflicts."""

    def build(self, root: Path) -> tuple[Repository, str, str]:
        repo_path = root / "repo"
        repo_path.mkdir()
        repo = Repository(repo_path)
        repo.write("codex-rs/Cargo.toml", workspace_manifest("0.1.0"))
        repo.write("greeting.md", "Restart Codex to continue.\n")
        repo.write("semantic.rs", "fn run() { Codex::start(); }\nlet timeout = 1;\n")
        repo.write("mixed.md", "Codex docs\n\nunrelated\n\nlimit = 5\n")
        repo.write("gone.md", "Codex leaves.\n")
        repo.write("snapshots/status.snap", "│ >_ Codex (v0.0.0)   │\n")
        repo.write("clean.md", "Codex is clean.\n\n\n\nfooter\n")
        repo.commit("base")
        base, tree = repo.tag_baseline("rust-v0.1.0")

        git(repo.root, "checkout", "-q", "-b", "release-next")
        repo.write("codex-rs/Cargo.toml", workspace_manifest("0.2.0"))
        repo.write("greeting.md", "Restart Codex to continue; Codex Desktop stays.\n")
        repo.write("semantic.rs", "fn run() { Codex::start(); }\nlet timeout = 2;\n")
        repo.write("mixed.md", "Codex docs v2\n\nunrelated\n\nlimit = 6\n")
        (repo.root / "gone.md").unlink()
        repo.write("clean.md", "Codex is clean.\n\n\n\nfooter\nCodex is new.\n")
        # New upstream files: a rendered snapshot inside the `added` scope and a
        # source file outside it.
        repo.write("snapshots/new_popup.snap", "│ >_ Codex (v0.0.0) │\nAsk Codex\n")
        repo.write("new_module.rs", "// Codex keeps this\n")
        repo.commit("upstream next")
        git(repo.root, "tag", "rust-v0.2.0")
        target = git(repo.root, "rev-parse", "HEAD")

        git(repo.root, "checkout", "-q", "-b", "zuno", base)
        repo.manifest("rust-v0.1.0", base, tree)
        repo.write("FORK_REBRAND.toml", REBRAND_RULES)
        repo.write("codex-rs/Cargo.toml", workspace_manifest("0.1.3"))
        repo.write("greeting.md", "Restart Zuno to continue.\n")
        repo.write("semantic.rs", "fn run() { Codex::start(); }\nlet timeout = 9;\n")
        repo.write("mixed.md", "Zuno docs\n\nunrelated\n\nlimit = 7\n")
        repo.write("gone.md", "Zuno leaves.\n")
        repo.write("snapshots/status.snap", "│ >_ Zuno (v0.1.3)    │\n")
        repo.write("clean.md", "Zuno is clean.\n\n\n\nfooter\n")
        source = repo.commit("zuno delta")
        return repo, source, target

    def test_prepare_replays_rename_only_conflicts_and_refreshes_clean_files(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            repo, source, _ = self.build(root)
            baseline = zuno_upstream.read_baseline(repo.root / "UPSTREAM_CODEX.toml")
            plan = zuno_upstream.make_plan(repo.root, baseline, "zuno", "rust-v0.2.0")
            worktree = root / "candidate"
            with self.assertRaises(zuno_upstream.SyncError) as raised:
                zuno_upstream.prepare(
                    repo.root, "UPSTREAM_CODEX.toml", plan, "upstream-sync/0.2.0", worktree
                )
            details = raised.exception.details
            self.assertEqual(details["status"], "conflict")
            # Only the semantic conflict is left; the mixed file lost its rename hunk.
            self.assertEqual(details["conflicting_files"], ["mixed.md", "semantic.rs"])
            replay = details["rebrand"]
            self.assertEqual(replay["resolved"], ["codex-rs/Cargo.toml", "greeting.md"])
            self.assertEqual(replay["deleted"], ["gone.md"])
            self.assertEqual(replay["partial"], ["mixed.md"])
            self.assertEqual(replay["refreshed"], ["clean.md", "snapshots/status.snap"])
            self.assertEqual(replay["added"], ["snapshots/new_popup.snap"])
            self.assertEqual(replay["drift"], ["new_module.rs"])
            self.assertEqual(replay["reused"], [])
            self.assertEqual(
                (worktree / "snapshots/new_popup.snap").read_text(), "│ >_ Zuno (v0.2.0)  │\nAsk Zuno\n"
            )
            self.assertEqual((worktree / "new_module.rs").read_text(), "// Codex keeps this\n")
            self.assertEqual(
                (worktree / "greeting.md").read_text(),
                "Restart Zuno to continue; Codex Desktop stays.\n",
            )
            self.assertEqual((worktree / "codex-rs/Cargo.toml").read_text(), workspace_manifest("0.2.0"))
            self.assertFalse((worktree / "gone.md").exists())
            self.assertEqual((worktree / "clean.md").read_text(), "Zuno is clean.\n\n\n\nfooter\nZuno is new.\n")
            # The version placeholder tracks the release and the border stays aligned.
            self.assertEqual((worktree / "snapshots/status.snap").read_text(), "│ >_ Zuno (v0.2.0)    │\n")
            mixed = (worktree / "mixed.md").read_text()
            self.assertTrue(mixed.startswith("Zuno docs v2\n\nunrelated\n\n<<<<<<< "), mixed)
            self.assertIn("||||||| ", mixed)
            semantic = (worktree / "semantic.rs").read_text()
            self.assertIn("<<<<<<< ", semantic)
            self.assertIn("let timeout = 9;", semantic)
            self.assertEqual(git(repo.root, "rev-parse", "zuno"), source)

    def test_prepare_without_rebrand_keeps_every_conflict(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            repo, _, _ = self.build(root)
            baseline = zuno_upstream.read_baseline(repo.root / "UPSTREAM_CODEX.toml")
            plan = zuno_upstream.make_plan(repo.root, baseline, "zuno", "rust-v0.2.0")
            with self.assertRaises(zuno_upstream.SyncError) as raised:
                zuno_upstream.prepare(
                    repo.root,
                    "UPSTREAM_CODEX.toml",
                    plan,
                    "upstream-sync/0.2.0",
                    root / "candidate",
                    replay_rebrand=False,
                )
            self.assertEqual(
                raised.exception.details["conflicting_files"],
                ["codex-rs/Cargo.toml", "gone.md", "greeting.md", "mixed.md", "semantic.rs"],
            )
            self.assertEqual(raised.exception.details["rebrand"]["resolved"], [])

    def test_prepare_reuses_resolutions_from_a_finalized_candidate(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            repo, source, target = self.build(root)
            baseline = zuno_upstream.read_baseline(repo.root / "UPSTREAM_CODEX.toml")
            plan = zuno_upstream.make_plan(repo.root, baseline, "zuno", "rust-v0.2.0")
            first = root / "first"
            with self.assertRaises(zuno_upstream.SyncError):
                zuno_upstream.prepare(repo.root, "UPSTREAM_CODEX.toml", plan, "upstream-sync/0.2.0", first)
            (first / "semantic.rs").write_text("fn run() { Codex::start(); }\nlet timeout = 92;\n")
            (first / "mixed.md").write_text("Zuno docs v2\n\nunrelated\n\nlimit = 76\n")
            # Post-merge edits outside any conflict: an API adaptation in a file
            # that merged cleanly, and the removal of an orphaned file.
            (first / "clean.md").write_text("Zuno is clean.\n\n\n\nfooter\nZuno is new.\nadapted to v2\n")
            (first / "snapshots" / "status.snap").unlink()
            finalized = zuno_upstream.finalize(first, "UPSTREAM_CODEX.toml", None, None)
            self.assertEqual(
                git(repo.root, "show", "-s", "--format=%P", finalized.commit).split(), [source, target]
            )

            # main moves without touching the conflicted paths: the old
            # resolutions carry over and the replay is clean.
            git(repo.root, "checkout", "-q", "zuno")
            repo.write("zuno-only.md", "new zuno file\n")
            moved = repo.commit("zuno moves")
            plan = zuno_upstream.make_plan(repo.root, baseline, "zuno", "rust-v0.2.0")
            second = root / "second"
            candidate = zuno_upstream.prepare(
                repo.root,
                "UPSTREAM_CODEX.toml",
                plan,
                "upstream-sync/0.2.0-again",
                second,
                reuse=finalized.commit,
            )
            self.assertEqual(
                candidate.rebrand.reused,
                ["clean.md", "mixed.md", "semantic.rs", "snapshots/status.snap"],
            )
            self.assertIn("let timeout = 92;", (second / "semantic.rs").read_text())
            self.assertEqual((second / "mixed.md").read_text(), "Zuno docs v2\n\nunrelated\n\nlimit = 76\n")
            self.assertTrue((second / "clean.md").read_text().endswith("adapted to v2\n"))
            self.assertFalse((second / "snapshots" / "status.snap").exists())
            # Files the replay already produced identically are not reported.
            self.assertNotIn("greeting.md", candidate.rebrand.reused)
            self.assertEqual((second / "zuno-only.md").read_text(), "new zuno file\n")
            refinalized = zuno_upstream.finalize(second, "UPSTREAM_CODEX.toml", None, None)
            self.assertEqual(
                git(repo.root, "show", "-s", "--format=%P", refinalized.commit).split(), [moved, target]
            )

            # A path Zuno changed since the old candidate is not reused, whether
            # it was a conflict (semantic.rs) or a clean adaptation (clean.md).
            git(repo.root, "checkout", "-q", "zuno")
            repo.write("semantic.rs", "fn run() { Codex::start(); }\nlet timeout = 10;\n")
            repo.write("clean.md", "Zuno is very clean.\n\n\n\nfooter\n")
            repo.commit("zuno edits semantic")
            plan = zuno_upstream.make_plan(repo.root, baseline, "zuno", "rust-v0.2.0")
            with self.assertRaises(zuno_upstream.SyncError) as raised:
                zuno_upstream.prepare(
                    repo.root,
                    "UPSTREAM_CODEX.toml",
                    plan,
                    "upstream-sync/0.2.0-third",
                    root / "third",
                    reuse=finalized.commit,
                )
            self.assertEqual(raised.exception.details["conflicting_files"], ["semantic.rs"])
            self.assertEqual(
                raised.exception.details["rebrand"]["reused"], ["mixed.md", "snapshots/status.snap"]
            )
            self.assertNotIn("adapted to v2", (root / "third" / "clean.md").read_text())

    def test_prepare_rejects_reuse_of_a_foreign_commit(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            repo, source, _ = self.build(root)
            baseline = zuno_upstream.read_baseline(repo.root / "UPSTREAM_CODEX.toml")
            plan = zuno_upstream.make_plan(repo.root, baseline, "zuno", "rust-v0.2.0")
            with self.assertRaises(zuno_upstream.SyncError) as raised:
                zuno_upstream.prepare(
                    repo.root, "UPSTREAM_CODEX.toml", plan, "upstream-sync/0.2.0", root / "candidate", reuse=source
                )
            self.assertIn("not a finalized candidate", str(raised.exception))

    def test_cli_prepare_reports_rebrand_replay(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            repo, _, _ = self.build(root)
            result = subprocess.run(
                [
                    sys.executable,
                    str(SCRIPT),
                    "--repo",
                    str(repo.root),
                    "--no-fetch",
                    "--json",
                    "prepare",
                    "--source",
                    "zuno",
                    "--target",
                    "rust-v0.2.0",
                    "--worktree",
                    str(root / "candidate"),
                ],
                text=True,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                check=False,
            )
            self.assertEqual(result.returncode, 1, result.stderr)
            payload = json.loads(result.stdout)
            self.assertEqual(payload["conflicting_files"], ["mixed.md", "semantic.rs"])
            self.assertEqual(payload["rebrand"]["resolved"], ["codex-rs/Cargo.toml", "greeting.md"])
            self.assertEqual(payload["rebrand"]["deleted"], ["gone.md"])


if __name__ == "__main__":
    unittest.main()
