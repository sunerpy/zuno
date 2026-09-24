#!/usr/bin/env python3
"""Fail if Zuno can reach inherited Codex updater or publisher entrypoints."""

import re
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]


def text(path: str) -> str:
    return (ROOT / path).read_text(encoding="utf-8")


def require(path: str, needle: str) -> None:
    if needle not in text(path):
        raise SystemExit(f"{path} is missing required Zuno boundary: {needle}")


def reject(path: str, needle: str) -> None:
    if needle in text(path):
        raise SystemExit(
            f"{path} still contains reachable Codex update target: {needle}"
        )


def runtime_text(path: str) -> str:
    source = text(path)
    marker = "\n#[cfg(test)]\nmod tests {"
    return source.split(marker, 1)[0]


def reject_runtime(path: str, needle: str) -> None:
    if needle in runtime_text(path):
        raise SystemExit(
            f"{path} still contains reachable inherited behavior: {needle}"
        )


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
            raise SystemExit(
                f"public daemon {variant} does not fail closed before filesystem access"
            )
    if "LifecycleCommand::Version" in guard:
        raise SystemExit(
            "read-only daemon Version was accidentally blocked as a mutation"
        )
    require(daemon, "zuno_blocks_managed_daemon_mutations_before_filesystem_access")
    for signature in [
        "pub async fn bootstrap(",
        "pub async fn ensure_remote_control_ready(",
        "pub async fn start_remote_control_pairing(",
        "pub async fn set_remote_control(",
        "pub async fn run_pid_update_loop(",
        "pub async fn update(",
    ]:
        body = function_body(daemon, signature)
        disabled = body.find("zuno_managed_daemon_disabled()")
        mutation = min(
            (
                position
                for token in [
                    "ensure_supported_platform()?",
                    "Daemon::from_environment()?",
                    "update_loop::run(",
                    "update_loop::request_manual_update(",
                ]
                if (position := body.find(token)) >= 0
            ),
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
            raise SystemExit(
                f"zuno app can still reach inherited installer behavior: {forbidden}"
            )
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

    require(
        "codex-rs/tui/src/update_action.rs",
        "pub(crate) fn from_install_context(_context",
    )
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
        "codex-rs/cli/src/doctor.rs": [
            "rerun zuno doctor",
            "Run zuno login",
            "no Zuno credentials",
        ],
        "codex-rs/cli/src/doctor/background.rs": ["Run zuno app-server daemon version"],
        "codex-rs/cli/src/doctor/runtime.rs": ["bundled Zuno package"],
        "codex-rs/cli/src/doctor/desktop.rs": ["rerun zuno doctor"],
        "codex-rs/cli/src/doctor/sandbox.rs": [
            "run zuno sandbox setup",
            "Zuno sandbox rules",
        ],
        "codex-rs/cli/src/doctor/network.rs": [
            "`zuno features enable respect_system_proxy`"
        ],
        "codex-rs/cli/src/doctor/git.rs": ["Zuno can inspect repository metadata"],
        "codex-rs/cli/src/doctor/security.rs": [
            "Zuno exclusions",
            "Zuno executable and compatibility-helper exclusions",
        ],
        "codex-rs/cli/src/doctor/thread_inventory.rs": [
            "Start Zuno with no state DB present"
        ],
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
    # Every Zuno workflow job runs on a CodeBuild-hosted runner (projects
    # zuno-runner / zuno-runner-windows / zuno-runner-macos) so the gate,
    # promotion and watcher consume no GitHub-hosted runner minutes. Each job's
    # last label is unique within its workflow so GitHub cannot hand one job's
    # runner to another.
    hosted = re.compile(
        r"^\s*(?:-\s*)?(?:runs-on:\s*|runner:\s*)?(ubuntu|windows|macos)-[a-z0-9.-]+\s*$",
        re.MULTILINE,
    )
    for path in (
        zuno_ci,
        ".github/workflows/zuno-release.yml",
        ".github/workflows/zuno-upstream-sync.yml",
    ):
        body = text(path)
        hits = [m.group(0).strip() for m in hosted.finditer(body)]
        if (
            hits
            or "runs-on: ubuntu" in body
            or "runs-on: windows" in body
            or "runs-on: macos" in body
        ):
            raise SystemExit(f"{path} still names a GitHub-hosted runner: {hits}")
        labels = re.findall(r"^\s+- (zuno-[a-z-]+)\s*$", body, re.MULTILINE)
        if not labels or len(labels) != len(set(labels)):
            raise SystemExit(
                f"{path} must give every job one unique zuno-* runner label: {labels}"
            )
        for label in re.findall(
            r"codebuild-([a-z${}. -]+?)-\$\{\{ github\.run_id", body
        ):
            if label.startswith("${{ matrix.project }}"):
                continue
            if label not in ("zuno-runner", "zuno-runner-windows", "zuno-runner-macos"):
                raise SystemExit(f"{path} names an unknown CodeBuild project: {label}")
    require(zuno_ci, "project: zuno-runner-macos")
    require(zuno_ci, "project: zuno-runner-windows")
    # aarch64-pc-windows-msvc is cross-built on x86_64 Windows and cannot run
    # there; its smoke is layout-only and the report must say so.
    require(zuno_ci, "smoke: layout")
    require(zuno_ci, 'if [[ "$SMOKE" == layout ]]; then')
    require("scripts/smoke_zuno_package.py", '"executed": False')
    require("scripts/smoke_zuno_package.py", "def binary_machine(path: Path) -> str:")
    require(".github/actionlint.yaml", "codebuild-zuno-runner-*")
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
    require(release, 'git merge-base --is-ancestor "$EXPECTED_HEAD_SHA"')
    require(release, "candidate_run_attempt:")
    require(release, "candidate_artifact_id:")
    reject(release, "[.pull_requests[].number]")
    require(release, 'assert data["pullRequest"] == int(os.environ["PR_NUMBER"])')
    require(release, '[[ "${#merge_parents[@]}" -eq 2 ]]')
    require(release, '[[ "${merge_parents[0]}" == "$base_sha" ]]')
    require(release, '[[ "${merge_parents[1]}" == "$EXPECTED_HEAD_SHA" ]]')
    require(release, "-c user.name='github-actions[bot]'")
    require(
        release,
        "-c user.email='41898282+github-actions[bot]@users.noreply.github.com'",
    )
    require(release, 'credential_file="${RUNNER_TEMP}/zuno-release-git-credential"')
    require(release, "trap 'rm -f \"${credential_file}\"' EXIT")
    require(release, 'chmod 600 "$credential_file"')
    require(release, "-c credential.helper= \\")
    require(release, '-c credential.helper="store --file=${credential_file}"')
    upstream_sync = ".github/workflows/zuno-upstream-sync.yml"
    require(upstream_sync, 'plan_path="${RUNNER_TEMP}/upstream-plan.json"')
    reject(upstream_sync, "> upstream-plan.json")
    reject(upstream_sync, "base_branch:")
    # The watcher must verify the chosen tag against openai/codex, never trust local tags.
    reject(upstream_sync, "--trust-local-tags")
    require(upstream_sync, "ref: main")
    require(upstream_sync, "persist-credentials: false")
    require(upstream_sync, "Push exact candidate branch")
    require(upstream_sync, "-c credential.helper= \\")
    require(upstream_sync, 'credential.helper="store --file=${credential_file}"')
    sync_text = text(upstream_sync)
    default_token = "GH_TOKEN: ${{ github.token }}"
    push_token = "GH_TOKEN: ${{ secrets.ZUNO_UPSTREAM_SYNC_TOKEN || github.token }}"
    if sync_text.count(default_token) != 2 or sync_text.count(push_token) != 3:
        raise SystemExit(
            "upstream sync must expose the default token only to the state and conflict-report "
            "steps and the optional automation token only to the token check, push and PR steps"
        )
    # The token health check must never create a ref: dry run only.
    require(upstream_sync, "Verify the automation token")
    require(upstream_sync, 'push --dry-run origin "HEAD:refs/heads/upstream-sync/token-check-${GITHUB_RUN_ID}"')
    # Steps that fetch or replay upstream code must never see any token: the
    # checkout/fetch/plan steps before state inspection, and the candidate
    # preparation and regeneration steps between state inspection and the push.
    inspect_at = sync_text.index("name: Inspect existing candidate and conflict report")
    prepare_at = sync_text.index("name: Prepare isolated candidate")
    push_at = sync_text.index("name: Push exact candidate branch")
    if (
        "GH_TOKEN" in sync_text[:inspect_at]
        or "GH_TOKEN" in sync_text[prepare_at:push_at]
    ):
        raise SystemExit("upstream sync exposes a token to a fetch or replay step")
    reject(upstream_sync, "persist-credentials: true")
    require(upstream_sync, "check \\\n            --allow-current")
    require(upstream_sync, "--force-with-lease=refs/heads/${HEAD_BRANCH}:")
    # The watcher itself never merges. The one permitted merge command only
    # queues GitHub auto-merge (merge commit, behind the required PR gate) and
    # only when the reviewed manifest opts in with [sync].automatic_merge = true.
    merges = [m.start() for m in re.finditer(r"gh pr merge ", sync_text)]
    queue = 'gh pr merge "${pr_url}" --auto --merge'
    if sync_text.count(queue) != 1:
        raise SystemExit(
            "upstream sync must contain exactly one guarded auto-merge queue command"
        )
    for start in merges:
        snippet = sync_text[start : start + 60]
        if not snippet.startswith(queue) and "--disable-auto" not in snippet:
            raise SystemExit(f"unexpected gh pr merge in upstream sync: {snippet!r}")
    merge_at = sync_text.index(queue)
    guard = sync_text[max(0, merge_at - 400) : merge_at]
    if 'elif [[ "${automatic_merge}" == true ]]' not in guard:
        raise SystemExit(
            "the auto-merge queue in upstream sync is not guarded by [sync].automatic_merge"
        )
    for needle in [
        '"${reused}" != 0',
        '"${gate_required}" != true',
        'elif [[ "${automatic_merge}" == true && "${USING_DEFAULT_TOKEN}" == true ]]',
        'gh pr merge "${pr_url}" --disable-auto',
        'gh pr merge "${EXISTING_PR}" --disable-auto',
        # The state step withdraws a queue whose PR head is not the commit the
        # watcher recorded, on every run including the ones that skip.
        'gh pr merge "${pr_number}" --disable-auto',
        '"${pr_head}" != "${pr_candidate}"',
        "Zuno-Candidate-Commit: ${CANDIDATE_COMMIT}",
    ]:
        require(upstream_sync, needle)
    # The PR step withdraws the earlier queue before any command that can fail
    # (closing superseded PRs and issues), so a failure cannot leave a stale queue.
    withdraw_at = sync_text.index('gh pr merge "${pr_url}" --disable-auto')
    if withdraw_at > sync_text.index(
        'gh issue close "${CONFLICT_ISSUE}"'
    ) or withdraw_at > sync_text.index('gh pr close "${stale}"'):
        raise SystemExit(
            "upstream sync must withdraw a stale auto-merge before closing superseded PRs and issues"
        )
    # A withdrawal that fails must abort the step, never be swallowed: a stale
    # queue would otherwise merge a head this run never cleared. The probe that
    # gates it must be an assignment: a command substitution inside `[[ ]]`
    # does not trip `set -e`, so a failed probe would silently skip the withdrawal.
    if (
        "--disable-auto >/dev/null" in sync_text
        or "--disable-auto || true" in sync_text
    ):
        raise SystemExit("upstream sync must not ignore a failed auto-merge withdrawal")
    if '[[ "$(gh pr view' in sync_text:
        raise SystemExit(
            "upstream sync must assign the auto-merge probe before testing it"
        )
    # The PR gate withdraws a queue whose head is not the watcher's candidate, so
    # a collaborator push cannot ride the queue into main before the next cron
    # tick; the watcher's state step is only the backstop.
    require(zuno_ci, "Withdraw auto-merge from a head the watcher did not push")
    require(zuno_ci, 'gh pr merge "${PR_NUMBER}" --disable-auto')
    require(zuno_ci, '"${head}" != "${candidate}"')
    require(zuno_ci, "startsWith(github.event.pull_request.head.ref, 'upstream-sync/')")
    require(
        zuno_ci, "github.event.pull_request.head.repo.full_name == github.repository"
    )
    if '[[ "$(gh pr view' in text(zuno_ci):
        raise SystemExit("zuno-ci must assign the auto-merge probe before testing it")
    # Issues are closed with the default token so the automation token needs no
    # issues permission; the conflict step already runs under the default token.
    if (
        sync_text.count('GH_TOKEN="${ISSUE_TOKEN}" gh issue close') != 2
        or sync_text.count("gh issue close") != 3
    ):
        raise SystemExit(
            "upstream sync must close issues from the PR step with the default token"
        )
    if (
        'tomllib.load(open("UPSTREAM_CODEX.toml", "rb")).get("sync", {}).get("automatic_merge", False)'
        not in sync_text
    ):
        raise SystemExit(
            "upstream sync must read automatic_merge from UPSTREAM_CODEX.toml"
        )
    if (
        sync_text.count("--auto ") != 1
        or "--squash" in sync_text
        or "--rebase" in sync_text
        or "--admin" in sync_text
    ):
        raise SystemExit(
            "upstream sync may only queue a merge-commit auto-merge; no squash, rebase or admin merges"
        )
    # gh infers the repo from git remotes and prefers one named `upstream`; the
    # sync checkout adds exactly that remote for openai/codex.
    require(upstream_sync, "GH_REPO: ${{ github.repository }}")

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
