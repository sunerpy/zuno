#!/usr/bin/env python3
"""Fail if Zuno can reach inherited Codex updater or publisher entrypoints."""

from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]


def text(path: str) -> str:
    return (ROOT / path).read_text(encoding="utf-8")


def require(path: str, needle: str) -> None:
    if needle not in text(path):
        raise SystemExit(f"{path} is missing required Zuno boundary: {needle}")


def reject(path: str, needle: str) -> None:
    if needle in text(path):
        raise SystemExit(f"{path} still contains reachable Codex update target: {needle}")


def runtime_text(path: str) -> str:
    source = text(path)
    marker = "\n#[cfg(test)]\nmod tests {"
    return source.split(marker, 1)[0]


def reject_runtime(path: str, needle: str) -> None:
    if needle in runtime_text(path):
        raise SystemExit(f"{path} still contains reachable inherited behavior: {needle}")


def function_body(path: str, signature: str) -> str:
    source = text(path)
    start = source.index(signature)
    opening = source.index("{", start)
    depth = 0
    for index in range(opening, len(source)):
        if source[index] == "{":
            depth += 1
        elif source[index] == "}":
            depth -= 1
            if depth == 0:
                return source[opening + 1 : index]
    raise SystemExit(f"{path} has an unterminated function body for {signature}")


def main() -> int:
    daemon = "codex-rs/app-server-daemon/src/lib.rs"
    require(daemon, "fn zuno_managed_daemon_disabled()")
    lifecycle_body = function_body(daemon, "pub async fn run(")
    guard = lifecycle_body.split("ensure_supported_platform()?", 1)[0]
    for variant in ["Start", "Restart", "Stop"]:
        if f"LifecycleCommand::{variant}" not in guard:
            raise SystemExit(f"public daemon {variant} does not fail closed before filesystem access")
    if "LifecycleCommand::Version" in guard:
        raise SystemExit("read-only daemon Version was accidentally blocked as a mutation")
    require(daemon, "zuno_blocks_managed_daemon_mutations_before_filesystem_access")
    for signature in [
        "pub async fn bootstrap(",
        "pub async fn ensure_remote_control_ready(",
        "pub async fn start_remote_control_pairing(",
        "pub async fn set_remote_control(",
        "pub async fn run_pid_update_loop(",
    ]:
        body = function_body(daemon, signature)
        disabled = body.find("zuno_managed_daemon_disabled()")
        mutation = min(
            (position for token in ["ensure_supported_platform()?", "Daemon::from_environment()?", "update_loop::run("] if (position := body.find(token)) >= 0),
            default=len(body),
        )
        if disabled < 0 or disabled > mutation:
            raise SystemExit(f"{signature} does not fail closed before daemon mutation")

    app_cmd = "codex-rs/cli/src/app_cmd.rs"
    app_body = function_body(app_cmd, "pub async fn run_app(")
    if "ZUNO_DESKTOP_APP_DISABLED" not in app_body:
        raise SystemExit("zuno app does not return the fixed disabled-state error")
    for forbidden in ["canonicalize", "run_app_open_or_install", "crate::desktop_app"]:
        if forbidden in runtime_text(app_cmd):
            raise SystemExit(f"zuno app can still reach inherited installer behavior: {forbidden}")
    require(app_cmd, "zuno_app_refuses_paths_and_download_urls_before_side_effects")
    reject("codex-rs/cli/src/main.rs", "mod desktop_app;")

    require("scripts/install/install.sh", "ZUNO_ALLOW_UPSTREAM_CODEX_INSTALLER")
    require("scripts/install/install.ps1", "ZUNO_ALLOW_UPSTREAM_CODEX_INSTALLER")
    for workflow in [
        ".github/workflows/rust-release.yml",
        ".github/workflows/rust-release-zsh.yml",
        ".github/workflows/rusty-v8-release.yml",
    ]:
        require(workflow, "if: github.repository == 'openai/codex'")

    require("codex-rs/tui/src/update_action.rs", "pub(crate) fn from_install_context(_context")
    require("codex-rs/tui/src/update_action.rs", "None")
    require("codex-rs/cli/src/main.rs", "Automatic Zuno update is disabled")
    tui_updates = "codex-rs/tui/src/updates.rs"
    require(tui_updates, "pub fn get_upgrade_version(_config: &Config)")
    require(tui_updates, "None")
    reject(tui_updates, "tokio::spawn")
    reject(tui_updates, "api.github.com/repos/")
    reject(tui_updates, "RouteAwareClientPool")

    doctor_updates = "codex-rs/cli/src/doctor/updates.rs"
    require(doctor_updates, "latest version probe: disabled")
    reject(doctor_updates, "GITHUB_LATEST_RELEASE_URL")
    reject(doctor_updates, "RouteAwareClientPool")
    reject(doctor_updates, "api.github.com/repos/")

    tooltips = "codex-rs/tui/src/tooltips.rs"
    for forbidden in [
        "ANNOUNCEMENT_TIP_URL",
        "RouteAwareClientPool",
        "announcement::prewarm",
        "codex app",
        "chatgpt.com/codex",
        "learn.chatgpt.com",
    ]:
        reject_runtime(tooltips, forbidden)
    reject("codex-rs/tui/src/lib.rs", "tooltips::announcement::prewarm")
    for forbidden in [
        "codex app",
        "chatgpt.com/codex",
        "learn.chatgpt.com",
        "discord.gg/openai",
        "community.openai.com/c/codex",
    ]:
        reject("codex-rs/tui/assets/tooltips.txt", forbidden)

    doctor_boundaries = {
        "codex-rs/cli/src/doctor.rs": ["rerun zuno doctor", "Run zuno login", "no Zuno credentials"],
        "codex-rs/cli/src/doctor/background.rs": ["Run zuno app-server daemon version"],
        "codex-rs/cli/src/doctor/runtime.rs": ["bundled Zuno package"],
        "codex-rs/cli/src/doctor/desktop.rs": ["rerun zuno doctor"],
        "codex-rs/cli/src/doctor/sandbox.rs": ["run zuno sandbox setup", "Zuno sandbox rules"],
        "codex-rs/cli/src/doctor/network.rs": ["`zuno features enable respect_system_proxy`"],
        "codex-rs/cli/src/doctor/git.rs": ["Zuno can inspect repository metadata"],
        "codex-rs/cli/src/doctor/security.rs": ["Zuno exclusions", "Zuno executable and compatibility-helper exclusions"],
        "codex-rs/cli/src/doctor/thread_inventory.rs": ["Start Zuno with no state DB present"],
        "codex-rs/cli/src/doctor/output.rs": ["Zuno Doctor", "Run zuno doctor"],
    }
    for path, required in doctor_boundaries.items():
        for needle in required:
            if needle not in runtime_text(path):
                raise SystemExit(f"{path} is missing Zuno Doctor guidance: {needle}")
        for forbidden in [
            "Run codex",
            "run codex",
            "rerun codex",
            "`codex doctor",
            "`codex login",
            "`codex sandbox",
            "`codex features",
            "Codex CLI",
            "Start Codex",
        ]:
            reject_runtime(path, forbidden)

    for path in [
        "codex-rs/tui/src/update_prompt.rs",
        "codex-rs/tui/src/history_cell/notices.rs",
    ]:
        require(path, "sunerpy/zuno")
        reject(path, "github.com/openai/codex")
        reject(path, "api.github.com/repos/openai/codex")

    zuno_ci = ".github/workflows/zuno-ci.yml"
    require(zuno_ci, "name: zuno/pr-gate")
    reject(zuno_ci, "workflow_dispatch:")
    reject(zuno_ci, "  push:")
    require(
        zuno_ci,
        "tests::zuno_blocks_managed_daemon_mutations_before_filesystem_access",
    )
    require(
        zuno_ci,
        "doctor::updates::tests::preview_update_check_is_explicitly_offline_and_non_mutating",
    )
    require(zuno_ci, "timeout-minutes: 180")
    require(zuno_ci, "bubblewrap")
    require(zuno_ci, '--entrypoint-dir "${CARGO_TARGET_DIR}/${TARGET}/release"')
    require(zuno_ci, "Smoke external Agent host sandbox")
    require(
        zuno_ci,
        "one_shot::claude_code::tests::workspace_profile_confines_the_external_process_on_linux",
    )
    release = ".github/workflows/zuno-release.yml"
    require(release, "Download exact sealed candidate artifact")
    require(release, "git merge-base --is-ancestor \"$EXPECTED_HEAD_SHA\"")
    require(release, "candidate_run_attempt:")
    require(release, "candidate_artifact_id:")
    require(release, '[[ "${#merge_parents[@]}" -eq 2 ]]')
    require(release, '[[ "${merge_parents[0]}" == "$base_sha" ]]')
    require(release, '[[ "${merge_parents[1]}" == "$EXPECTED_HEAD_SHA" ]]')
    upstream_sync = ".github/workflows/zuno-upstream-sync.yml"
    require(upstream_sync, 'plan_path="${RUNNER_TEMP}/upstream-plan.json"')
    reject(upstream_sync, "> upstream-plan.json")
    reject(upstream_sync, "base_branch:")
    require(upstream_sync, "ref: main")
    require(upstream_sync, "persist-credentials: false")
    require(upstream_sync, "Push exact candidate branch")
    require(upstream_sync, "-c credential.helper= \\")
    require(upstream_sync, 'credential.helper="store --file=${credential_file}"')
    if text(upstream_sync).count("GH_TOKEN: ${{ github.token }}") != 2:
        raise SystemExit(
            "upstream sync must expose GH_TOKEN only to the final push and PR steps"
        )

    smoke = text("scripts/smoke_zuno_package.py")
    isolation = smoke.index('runtime_env["HOME"]')
    if isolation > smoke.index('[str(binary), "--version"]') or isolation > smoke.index(
        '[str(binary), "acp"]'
    ):
        raise SystemExit("Zuno package smoke runs before HOME/ZUNO_HOME isolation")
    acp_start = smoke.index('[str(binary), "acp"]')
    if "env=runtime_env" not in smoke[acp_start : acp_start + 500]:
        raise SystemExit("Zuno ACP smoke does not use the isolated environment")
    print("ZUNO_UPDATE_BOUNDARIES_OK")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
