#!/usr/bin/env python3

import importlib.util
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest


SCRIPT = Path(__file__).with_name("zuno_rebrand.py")
SPEC = importlib.util.spec_from_file_location("zuno_rebrand", SCRIPT)
assert SPEC and SPEC.loader
zuno_rebrand = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = zuno_rebrand
SPEC.loader.exec_module(zuno_rebrand)

REPO_MANIFEST = Path(__file__).resolve().parent.parent / zuno_rebrand.MANIFEST_NAME


def rules() -> "zuno_rebrand.Rebrand":
    return zuno_rebrand.Rebrand.load(REPO_MANIFEST)


def git(repo: Path, *args: str) -> str:
    return subprocess.run(
        ["git", *args], cwd=repo, text=True, check=True, stdout=subprocess.PIPE, stderr=subprocess.PIPE
    ).stdout.strip()


class ApplyTest(unittest.TestCase):
    def test_repository_manifest_loads(self) -> None:
        rebrand = rules()
        self.assertGreater(len(rebrand.rules), 5)
        self.assertIn("Codex Desktop", rebrand.protect)

    def test_product_name_and_home_are_rebranded(self) -> None:
        text = "Restart Codex after enabling it. Config lives in CODEX_HOME or ~/.codex/config.toml.\n"
        self.assertEqual(
            rules().apply(text, version=None, path="codex-rs/tui/src/x.rs"),
            "Restart Zuno after enabling it. Config lives in ZUNO_HOME or ~/.zuno/config.toml.\n",
        )

    def test_identifiers_and_protected_phrases_are_kept(self) -> None:
        text = (
            "use codex_login::CodexAuth;\n"
            "let op = AppEvent::CodexOp(Codex::new());\n"
            "// Codex Desktop and Codex Cloud stay; codex-rs and gpt-5-codex too; .codex/ as well\n"
            "OAI-Product-Sku: codex\n"
            "Zuno CLI is an open source project led by OpenAI.\n"
        )
        self.assertEqual(
            rules().apply(text, version=None, path="codex-rs/core/src/x.rs"),
            "use codex_login::CodexAuth;\n"
            "let op = AppEvent::CodexOp(Codex::new());\n"
            "// Codex Desktop and Codex Cloud stay; codex-rs and gpt-5-codex too; .codex/ as well\n"
            "OAI-Product-Sku: codex\n"
            "Zuno CLI is an open source project forked from OpenAI's Codex CLI.\n",
        )

    def test_product_name_after_escaped_newline_is_rebranded(self) -> None:
        text = 'push_str("\\nThis may restart.\\nCodex exits to update.");\n'
        self.assertEqual(
            rules().apply(text, version=None, path="codex-rs/tui/src/app/x.rs"),
            'push_str("\\nThis may restart.\\nZuno exits to update.");\n',
        )

    def test_command_examples_are_rebranded(self) -> None:
        text = "Run `codex login`, then codex resume <id> or `codex`.\n$ codex\nopenai/codex stays\n"
        self.assertEqual(
            rules().apply(text, version=None, path="docs/x.md"),
            "Run `zuno login`, then zuno resume <id> or `zuno`.\n$ zuno\nopenai/codex stays\n",
        )

    def test_width_is_preserved_for_padded_lines(self) -> None:
        text = '"› Ask Codex to do anything      "\n│  >_ OpenAI Codex (v0.0.0)        │\n'
        rebranded = rules().apply(text, version="0.156.1", path="codex-rs/tui/src/status/snapshots/a.snap")
        self.assertEqual(
            rebranded,
            '"› Ask Zuno to do anything       "\n│  >_ Zuno (v0.156.1)              │\n',
        )
        for before, after in zip(text.splitlines(), rebranded.splitlines()):
            self.assertEqual(zuno_rebrand.display_width(before), zuno_rebrand.display_width(after))

    def test_version_rule_is_path_scoped_and_needs_a_version(self) -> None:
        text = "(v0.0.0)\n"
        self.assertEqual(rules().apply(text, version="1.2.3", path="codex-rs/tui/src/status/snapshots/a.snap"), "(v1.2.3)\n")
        self.assertEqual(
            rules().apply("package v0.0.0 from x, CLI v0.0.0. Not v0.0.0.1 nor av0.0.0\n", version="1.2.3", path="codex-rs/tui/src/a.snap"),
            "package v1.2.3 from x, CLI v1.2.3. Not v0.0.0.1 nor av0.0.0\n",
        )
        self.assertEqual(rules().apply(text, version="1.2.3", path="codex-rs/core/src/other.snap"), "(v0.0.0)\n")
        self.assertEqual(rules().apply(text, version="1.2.3", path="codex-rs/tui/src/app/tests.rs"), "(v0.0.0)\n")
        self.assertTrue(rules().applies_to_added("codex-rs/tui/src/app/snapshots/new.snap"))
        self.assertFalse(rules().applies_to_added("codex-rs/tui/src/app/new.rs"))
        self.assertFalse(rules().applies_to_added("codex-rs/core/src/snapshots/new.snap"))
        self.assertEqual(rules().apply(text, version=None, path="codex-rs/tui/src/status/snapshots/a.snap"), "(v0.0.0)\n")

    def test_replacement_text_is_not_matched_again(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            manifest = Path(directory) / "rules.toml"
            manifest.write_text(
                'schema_version = 1\nprotect = []\n[[rule]]\nfind = "Codex"\nreplace = "Zuno Codex"\n'
                '[[rule]]\nfind = "Zuno"\nreplace = "X"\n',
                encoding="utf-8",
            )
            rebrand = zuno_rebrand.Rebrand.load(manifest)
        self.assertEqual(rebrand.apply("Codex Zuno", version=None, path=None), "Zuno Codex X")

    def test_manifest_validation(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            manifest = Path(directory) / "rules.toml"
            manifest.write_text('schema_version = 1\n[[rule]]\nfind = "a"\nregex = "b"\nreplace = "c"\n', encoding="utf-8")
            with self.assertRaises(zuno_rebrand.RebrandError):
                zuno_rebrand.Rebrand.load(manifest)
            manifest.write_text('schema_version = 2\n[[rule]]\nfind = "a"\nreplace = "c"\n', encoding="utf-8")
            with self.assertRaises(zuno_rebrand.RebrandError):
                zuno_rebrand.Rebrand.load(manifest)


def hunk(ours: str, base: str, theirs: str) -> str:
    return f"<<<<<<< up\n{ours}||||||| base\n{base}=======\n{theirs}>>>>>>> zuno\n"


class ResolveTest(unittest.TestCase):
    def test_rename_only_hunk_is_replayed_onto_upstream(self) -> None:
        text = "a\n" + hunk("Run `codex login` now.\n", "Run `codex login`.\n", "Run `zuno login`.\n") + "c\n"
        resolved, report = zuno_rebrand.resolve_conflicts(
            text, rules(), path="docs/x.md", source_version=None, target_version=None
        )
        self.assertEqual(resolved, "a\nRun `zuno login` now.\nc\n")
        self.assertEqual((report.resolved, report.remaining), (1, 0))

    def test_semantic_hunk_keeps_its_markers_and_labels(self) -> None:
        text = "a\n" + hunk("new logic\n", "old\n", "zuno logic\n") + "c\n"
        resolved, report = zuno_rebrand.resolve_conflicts(
            text, rules(), path="x.rs", source_version=None, target_version=None
        )
        self.assertEqual(resolved, text)
        self.assertEqual((report.resolved, report.remaining), (0, 1))

    def test_mixed_file_resolves_only_rename_hunks(self) -> None:
        text = hunk("Codex v2\n", "Codex\n", "Zuno\n") + "mid\n" + hunk("x\n", "y\n", "z\n")
        resolved, report = zuno_rebrand.resolve_conflicts(
            text, rules(), path="x.md", source_version=None, target_version=None
        )
        self.assertEqual(resolved, "Zuno v2\nmid\n" + hunk("x\n", "y\n", "z\n"))
        self.assertEqual((report.resolved, report.remaining), (1, 1))

    def test_upstream_deletion_of_rename_only_hunk_deletes_it(self) -> None:
        text = "a\n" + hunk("", "old Codex line\n", "old Zuno line\n") + "c\n"
        resolved, _ = zuno_rebrand.resolve_conflicts(
            text, rules(), path="x.md", source_version=None, target_version=None
        )
        self.assertEqual(resolved, "a\nc\n")

    def test_workspace_version_hunk_takes_upstream(self) -> None:
        text = hunk('version = "0.156.1"\n', 'version = "0.154.0"\n', 'version = "0.154.3"\n')
        resolved, _ = zuno_rebrand.resolve_conflicts(
            text, rules(), path="codex-rs/Cargo.toml", source_version=None, target_version=None
        )
        self.assertEqual(resolved, 'version = "0.156.1"\n')
        elsewhere, report = zuno_rebrand.resolve_conflicts(
            text, rules(), path="codex-rs/other/Cargo.toml", source_version=None, target_version=None
        )
        self.assertEqual(report.remaining, 1)
        self.assertEqual(elsewhere, text)

    def test_two_way_hunks_without_base_are_left_alone(self) -> None:
        text = "<<<<<<< up\nCodex\n=======\nZuno\n>>>>>>> zuno\n"
        resolved, report = zuno_rebrand.resolve_conflicts(
            text, rules(), path="x.md", source_version=None, target_version=None
        )
        self.assertEqual(resolved, text)
        self.assertEqual(report.remaining, 1)

    def test_unterminated_hunk_is_an_error(self) -> None:
        with self.assertRaises(zuno_rebrand.RebrandError):
            zuno_rebrand.split_conflicts("<<<<<<< up\nx\n")


class AuditTest(unittest.TestCase):
    def test_audit_counts_reproduced_and_diverged_files(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            repo = Path(directory)
            git(repo, "init", "-q")
            git(repo, "config", "user.email", "t@example.invalid")
            git(repo, "config", "user.name", "t")
            (repo / "pure.md").write_text("Restart Codex.\n", encoding="utf-8")
            (repo / "mixed.md").write_text("Codex\nlogic\n", encoding="utf-8")
            (repo / "added.md").write_text("", encoding="utf-8")
            (repo / "added.md").unlink()
            git(repo, "add", "-A")
            git(repo, "commit", "-q", "-m", "base")
            git(repo, "tag", "base")
            (repo / "pure.md").write_text("Restart Zuno.\n", encoding="utf-8")
            (repo / "mixed.md").write_text("Zuno\nother\n", encoding="utf-8")
            (repo / "added.md").write_text("new\n", encoding="utf-8")
            git(repo, "add", "-A")
            git(repo, "commit", "-q", "-m", "zuno")
            report = zuno_rebrand.audit(repo, REPO_MANIFEST, "base", "HEAD", None)
            self.assertEqual(report["reproduced"], 1)
            self.assertEqual(report["diverged"], [{"path": "mixed.md", "differing_lines": 1}])
            self.assertEqual(report["added"], ["added.md"])
            output = subprocess.run(
                [sys.executable, str(SCRIPT), "--repo", str(repo), "--manifest", str(REPO_MANIFEST), "audit", "--baseline", "base", "--json"],
                text=True,
                check=True,
                stdout=subprocess.PIPE,
            ).stdout
            self.assertEqual(json.loads(output)["modified"], 2)


if __name__ == "__main__":
    unittest.main()
