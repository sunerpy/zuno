#!/usr/bin/env python3
"""Conservatively select personal native gates for an enterprise-only change."""

import argparse
import subprocess

ENTERPRISE_DIRECTORIES = (
    "enterprise/",
    "crates/zuno-identity/",
    "crates/zuno-postgres/",
    "crates/zuno-environment/",
    "crates/zuno-worker/",
    "crates/zuno-server/tests/enterprise_state/",
)
ENTERPRISE_FILES = {
    "crates/zuno-server/tests/enterprise_state.rs",
    "crates/zuno-server/src/enterprise_browser.rs",
    "crates/zuno-server/src/enterprise_gateway.rs",
    "crates/zuno-server/src/enterprise_state.rs",
    "crates/zuno-server/src/gateway_configuration.rs",
    "crates/zuno-server/src/gateway_execution.rs",
    "scripts/check_enterprise_docker.py",
    "scripts/check_enterprise_postgres.py",
    "scripts/enterprise_preview.py",
    "scripts/test_enterprise_preview.py",
}


def requires_personal(paths):
    if not paths:
        return True
    return any(not (
        path.startswith(ENTERPRISE_DIRECTORIES)
        or path in ENTERPRISE_FILES
        or path.startswith(".github/workflows/enterprise-")
    ) for path in paths)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--base", required=True)
    parser.add_argument("--head", default="HEAD")
    options = parser.parse_args()
    result = subprocess.run(
        ["git", "diff", "--no-ext-diff", "--no-textconv", "--name-only", "-z",
         options.base, options.head],
        check=True, capture_output=True,
    )
    paths = [path.decode("utf-8", errors="strict") for path in result.stdout.split(b"\0") if path]
    print("true" if requires_personal(paths) else "false")


if __name__ == "__main__":
    main()
