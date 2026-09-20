#!/usr/bin/env python3
"""Live end-to-end check of a standalone Zuno binary against a real model provider.

Complements ``zuno_standalone_smoke.py`` (which uses a scripted mock model): here a
real OpenAI Responses-compatible provider (for example the local Kiro Provider)
drives the turn, and strict approval mode must still gate everything:

1. ask the model to run ``ls -la`` -> an ``item/commandExecution/requestApproval``
   arrives, we accept, the command runs, the turn completes with an answer;
2. ask the model to create a file -> an approval arrives (file change or shell),
   we decline, the file must not exist afterwards.

Only the standard library is used so the script runs next to the binary inside a
minimal container (`docker run --network host ...` reaches a provider on the host).
"""

from __future__ import annotations

import argparse
import json
import os
import shutil
import sys
import tempfile
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import zuno_standalone_smoke as smoke  # noqa: E402

COMMAND_APPROVAL = "item/commandExecution/requestApproval"
FILE_APPROVAL = "item/fileChange/requestApproval"


def write_config(home: Path, base_url: str, model: str, api_key_env: str) -> None:
    home.mkdir(parents=True, exist_ok=True)
    (home / "config.toml").write_text(
        f"""# Strict server troubleshooting profile against a live provider.
model_provider = "live"
model = "{model}"
approval_policy = "untrusted"
approvals_reviewer = "user"
sandbox_mode = "danger-full-access"

[model_providers.live]
name = "Live provider under test"
base_url = "{base_url}"
env_key = "{api_key_env}"
wire_api = "responses"
requires_openai_auth = false
request_max_retries = 2
stream_max_retries = 2
stream_idle_timeout_ms = 300000
""",
        encoding="utf-8",
    )


def run_prompt(
    app: smoke.AppServer,
    workspace: Path,
    prompt: str,
    decision_for: dict[str, str],
    timeout: float,
) -> dict[str, object]:
    start = len(app.transcript)
    thread = app.request("thread/start", {"cwd": str(workspace)})
    thread_id = thread["thread"]["id"]
    app.request(
        "turn/start",
        {"threadId": thread_id, "input": [{"type": "text", "text": prompt}]},
    )
    approvals: list[dict[str, object]] = []
    deadline = time.monotonic() + timeout
    while True:
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            raise smoke.SmokeError("live turn did not complete in time")
        app.timeout = remaining
        message = app.read_until(
            lambda m: m.get("method") in (COMMAND_APPROVAL, FILE_APPROVAL, "turn/completed")
        )
        method = message.get("method")
        if method == "turn/completed":
            completed = message
            break
        params = message.get("params") or {}
        decision = decision_for.get(method, "decline")
        approvals.append(
            {
                "method": method,
                "kind": params.get("kind"),
                "command": params.get("command"),
                "reason": params.get("reason"),
                "decision": decision,
            }
        )
        app.send({"id": message["id"], "result": {"decision": decision}})
    status = ((completed.get("params") or {}).get("turn") or {}).get("status")
    items = [
        (message.get("params") or {}).get("item") or {}
        for message in app.transcript[start:]
        if message.get("method") == "item/completed"
    ]
    commands = [
        {"command": item.get("command"), "status": item.get("status"), "exit": item.get("exitCode")}
        for item in items
        if item.get("type") == "commandExecution"
    ]
    answer = " ".join(
        str(item.get("text") or "") for item in items if item.get("type") == "agentMessage"
    ).strip()
    return {
        "status": status,
        "approvals": approvals,
        "commands": commands,
        "answer": answer[:400],
    }


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--base-url", required=True, help="Responses API base URL, e.g. http://127.0.0.1:8787/v1")
    parser.add_argument("--model", required=True)
    parser.add_argument("--api-key-env", default="KIRO_PROVIDER_API_KEY", help="env var holding the bearer token")
    parser.add_argument("--timeout", type=float, default=300.0, help="seconds per turn")
    parser.add_argument("--allow-dynamic", action="store_true")
    parser.add_argument("--keep", action="store_true")
    args = parser.parse_args()
    if not os.environ.get(args.api_key_env):
        print(json.dumps({"ok": False, "error": f"{args.api_key_env} is not set"}))
        return 1

    binary = args.binary.resolve()
    report: dict[str, object] = {"binary": str(binary), "provider": args.base_url, "model": args.model}
    root = Path(tempfile.mkdtemp(prefix="zuno-live-check-"))
    try:
        report["elf"] = smoke.check_static_elf(binary, args.allow_dynamic)
        home = root / "zuno-home"
        workspace = root / "workspace"
        workspace.mkdir()
        (workspace / "README.txt").write_text("live check workspace\n", encoding="utf-8")
        (workspace / "data.csv").write_text("a,b\n1,2\n", encoding="utf-8")
        write_config(home, args.base_url, args.model, args.api_key_env)
        app = smoke.AppServer(binary, home, workspace, args.timeout)
        try:
            app.request(
                "initialize",
                {"clientInfo": {"name": "zuno_live_check", "title": "Zuno live check", "version": "0.1.0"}},
            )
            app.send({"method": "initialized"})

            approved = run_prompt(
                app,
                workspace,
                "Run the shell command `ls -la` in the current directory, then reply with the "
                "number of entries it listed. Do nothing else.",
                {COMMAND_APPROVAL: "accept"},
                args.timeout,
            )
            report["command_turn"] = approved
            if approved["status"] != "completed":
                raise smoke.SmokeError(f"command turn ended with status {approved['status']}")
            if not any(a["method"] == COMMAND_APPROVAL for a in approved["approvals"]):
                raise smoke.SmokeError("strict mode did not ask before the model's shell command")
            if not any(c["status"] == "completed" for c in approved["commands"]):
                raise smoke.SmokeError(f"approved command did not run: {approved['commands']}")
            if not approved["answer"]:
                raise smoke.SmokeError("model returned no answer after the approved command")

            declined = run_prompt(
                app,
                workspace,
                "Create a new file named NOTE.txt in the current directory containing the single "
                "line `hello`. Do nothing else.",
                {},
                args.timeout,
            )
            report["edit_turn"] = declined
            if declined["status"] != "completed":
                raise smoke.SmokeError(f"edit turn ended with status {declined['status']}")
            if not declined["approvals"]:
                raise smoke.SmokeError("strict mode did not ask before the model's file edit")
            if (workspace / "NOTE.txt").exists():
                raise smoke.SmokeError("declined edit still created NOTE.txt")
            if any(c["status"] == "completed" for c in declined["commands"]):
                raise smoke.SmokeError(f"a command ran despite the decline: {declined['commands']}")
        finally:
            app.close()
    except smoke.SmokeError as error:
        report["ok"] = False
        report["error"] = str(error)
        print(json.dumps(report, indent=2, ensure_ascii=False))
        return 1
    finally:
        if args.keep:
            report["kept"] = str(root)
        else:
            shutil.rmtree(root, ignore_errors=True)
    report["ok"] = True
    print(json.dumps(report, indent=2, ensure_ascii=False))
    return 0


if __name__ == "__main__":
    sys.exit(main())
