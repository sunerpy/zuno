import unittest
import io
from unittest.mock import patch

import cargo_surface
from cargo_surface import personal_arguments
from ci_platform_scope import requires_personal


def metadata(extra=()):
    packages = [{"id": name, "name": name, "metadata": {}} for name in
                ["zuno", "zuno-tui", "zuno-acp", "zuno-engine", "zuno-server", "zuno-types"]]
    packages.extend(extra)
    return {"workspace_members": [package["id"] for package in packages], "packages": packages}


class PersonalSurfaceTests(unittest.TestCase):
    def test_argument_stream_is_lf_only_even_with_windows_stdout_translation(self):
        raw = io.BytesIO()
        translated = io.TextIOWrapper(raw, encoding="utf-8", newline="\r\n")
        with patch.object(cargo_surface, "metadata_at", return_value=metadata()), \
                patch("sys.argv", ["cargo_surface.py", "args"]), \
                patch("sys.stdout", translated):
            cargo_surface.main()
            translated.flush()
        self.assertEqual(raw.getvalue(), b"--workspace\n")

    def test_enterprise_only_paths_do_not_require_personal_windows(self):
        self.assertFalse(requires_personal([
            "crates/zuno-postgres/src/operation.rs", "enterprise/STATUS.md",
            "crates/zuno-server/src/enterprise_state.rs",
        ]))

    def test_shared_or_unknown_changes_keep_personal_native_gates(self):
        for path in ["Cargo.lock", "Cargo.toml", "crates/zuno-engine/src/loop.rs",
                     "crates/zuno-tui/src/lib.rs", "crates/zuno-acp/src/lib.rs",
                     "crates/zuno-server/Cargo.toml", "new-directory/file.rs"]:
            self.assertTrue(requires_personal([path]), path)
        self.assertTrue(requires_personal([]))

    def test_enterprise_service_roots_are_excluded_and_shared_packages_remain(self):
        data = metadata([{
            "id": "gateway", "name": "zuno-gateway",
            "metadata": {"zuno": {"distribution": "enterprise"}},
        }])
        self.assertEqual(personal_arguments(data), ["--workspace", "--exclude", "zuno-gateway"])

    def test_an_enterprise_only_cli_or_unknown_marker_fails_closed(self):
        for distribution in ["enterprise", "unsupported"]:
            data = metadata()
            data["packages"][0]["metadata"] = {"zuno": {"distribution": distribution}}
            with self.assertRaises(ValueError):
                personal_arguments(data)

    def test_third_party_metadata_cannot_change_workspace_selection(self):
        data = metadata()
        data["packages"].append({
            "id": "external", "name": "external", "metadata": {"zuno": {"distribution": "enterprise"}},
        })
        self.assertEqual(personal_arguments(data), ["--workspace"])

    def test_cargo_null_metadata_defaults_to_personal(self):
        data = metadata()
        data["packages"][0]["metadata"] = None
        self.assertEqual(personal_arguments(data), ["--workspace"])


if __name__ == "__main__":
    unittest.main()
