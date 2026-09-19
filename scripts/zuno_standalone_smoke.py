#!/usr/bin/env python3
"""Smoke-test a standalone Zuno binary in strict approval mode.

The script needs only the Python standard library so it can run inside a
minimal container next to the binary. It checks that:

1. the binary is a static ELF executable (no dynamic loader) and reports its
   version;
2. with ``approval_policy = "untrusted"`` (strict mode) the App Server asks the
   user before running even a harmless ``ls`` proposed by the model, and a
   declined approval ends the turn without executing anything.

The model is a local mock of the OpenAI Responses API that first asks to run
``ls`` and then, once told the command was declined, answers with a message.
"""

from __future__ import annotations

import argparse
import json
import os
import re
import shutil
import struct
import subprocess
import sys
import tempfile
import threading
import time
from http.server import BaseHTTPRequestHandler
from http.server import ThreadingHTTPServer
from pathlib import Path

APPROVAL_METHOD = "item/commandExecution/requestApproval"
PT_INTERP = 3


class SmokeError(RuntimeError):
    pass


def check_static_elf(binary: Path, allow_dynamic: bool) -> dict[str, object]:
    if not binary.is_file():
        raise SmokeError(f"binary not found: {binary}")
    with binary.open("rb") as handle:
        header = handle.read(64)
        if header[:4] != b"\x7fELF":
            raise SmokeError(f"{binary} is not an ELF executable")
        is_64 = header[4] == 2
        little = header[5] == 1
        endian = "<" if little else ">"
        if is_64:
            phoff = struct.unpack(f"{endian}Q", header[32:40])[0]
            phentsize, phnum = struct.unpack(f"{endian}HH", header[54:58])
        else:
            phoff = struct.unpack(f"{endian}I", header[28:32])[0]
            phentsize, phnum = struct.unpack(f"{endian}HH", header[42:46])
        handle.seek(phoff)
        table = handle.read(phentsize * phnum)
    has_interp = False
    for index in range(phnum):
        entry = table[index * phentsize : (index + 1) * phentsize]
        p_type = struct.unpack(f"{endian}I", entry[:4])[0]
        if p_type == PT_INTERP:
            has_interp = True
    if has_interp and not allow_dynamic:
        raise SmokeError(f"{binary} requests a dynamic loader (PT_INTERP); it is not standalone")
    return {"elf64": is_64, "dynamic_loader": has_interp}


def sse(events: list[dict[str, object]]) -> bytes:
    out = []
    for event in events:
        out.append(f"event: {event['type']}\n")
        out.append(f"data: {json.dumps(event)}\n\n")
    return "".join(out).encode("utf-8")


def function_call(call_id: str, name: str, arguments: dict[str, object]) -> dict[str, object]:
    return {
        "type": "function_call",
        "call_id": call_id,
        "name": name,
        "arguments": json.dumps(arguments),
    }


def assistant_message(text: str) -> dict[str, object]:
    return {
        "type": "message",
        "role": "assistant",
        "id": "msg-final",
        "content": [{"type": "output_text", "text": text}],
    }


SESSION_PLACEHOLDER = "$SESSION_ID"


def resolve_session_id(item: dict[str, object], body: dict[str, object]) -> dict[str, object]:
    """Fill the terminal session id the server reported into a scripted write_stdin call."""
    arguments = item.get("arguments")
    if not isinstance(arguments, str) or SESSION_PLACEHOLDER not in arguments:
        return item
    session_id = None
    for entry in body.get("input") or []:
        if not isinstance(entry, dict) or entry.get("type") != "function_call_output":
            continue
        output = entry.get("output")
        text = output if isinstance(output, str) else json.dumps(output)
        match = re.search(r"session ID (\d+)", text)
        if match:
            session_id = int(match.group(1))
    if session_id is None:
        raise RuntimeError("no terminal session id in the model request")
    resolved = dict(item)
    resolved["arguments"] = arguments.replace(f'"{SESSION_PLACEHOLDER}"', str(session_id))
    return resolved


class MockResponses(BaseHTTPRequestHandler):
    """Scripted Responses mock: each model request pops the next output item."""

    calls: list[dict[str, object]] = []
    script: list[dict[str, object]] = []
    lock = threading.Lock()

    def log_message(self, format: str, *args: object) -> None:  # noqa: A002 - BaseHTTPRequestHandler API
        return

    def do_POST(self) -> None:  # noqa: N802 - BaseHTTPRequestHandler API
        length = int(self.headers.get("Content-Length") or 0)
        body = json.loads(self.rfile.read(length) or b"{}")
        if not self.path.endswith("/responses"):
            self.send_response(404)
            self.end_headers()
            return
        with MockResponses.lock:
            MockResponses.calls.append(body)
            call_number = len(MockResponses.calls)
            if MockResponses.script:
                item = MockResponses.script.pop(0)
            else:
                item = assistant_message("Done.")
        item = resolve_session_id(item, body)
        response_id = f"resp-{call_number}"
        events = [
            {"type": "response.created", "response": {"id": response_id}},
            {"type": "response.output_item.done", "item": item},
        ]
        events.append(
            {
                "type": "response.completed",
                "response": {
                    "id": response_id,
                    "usage": {
                        "input_tokens": 0,
                        "input_tokens_details": None,
                        "output_tokens": 0,
                        "output_tokens_details": None,
                        "total_tokens": 0,
                    },
                },
            }
        )
        payload = sse(events)
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Content-Length", str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)

    def do_GET(self) -> None:  # noqa: N802 - BaseHTTPRequestHandler API
        self.send_response(404)
        self.end_headers()


def write_config(home: Path, base_url: str, approval_policy: str) -> None:
    home.mkdir(parents=True, exist_ok=True)
    (home / "config.toml").write_text(
        f"""# Server troubleshooting profile exercised by the smoke test.
model = "mock-model"
model_provider = "mock_provider"
approval_policy = "{approval_policy}"
approvals_reviewer = "user"
sandbox_mode = "danger-full-access"

[model_providers.mock_provider]
name = "Mock provider for the standalone smoke test"
base_url = "{base_url}/v1"
wire_api = "responses"
request_max_retries = 0
stream_max_retries = 0
""",
        encoding="utf-8",
    )


class AppServer:
    def __init__(self, binary: Path, home: Path, cwd: Path, timeout: float) -> None:
        env = {
            key: value
            for key, value in os.environ.items()
            if key not in {"CODEX_HOME", "ZUNO_HOME", "OPENAI_API_KEY"}
        }
        env["ZUNO_HOME"] = str(home)
        env.setdefault("HOME", str(home))
        # stderr goes to a file so verbose logging can never block the server.
        self.stderr_path = home / "app-server.stderr.log"
        self._stderr = self.stderr_path.open("w", encoding="utf-8")
        self.process = subprocess.Popen(
            [str(binary), "app-server"],
            cwd=cwd,
            env=env,
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=self._stderr,
            text=True,
            bufsize=1,
        )
        self.timeout = timeout
        self.next_id = 1
        self.transcript: list[dict[str, object]] = []
        self._lines: list[str] = []
        self._lines_lock = threading.Lock()
        self._reader = threading.Thread(target=self._pump, daemon=True)
        self._reader.start()

    def _pump(self) -> None:
        assert self.process.stdout is not None
        for line in self.process.stdout:
            with self._lines_lock:
                self._lines.append(line)

    def send(self, message: dict[str, object]) -> None:
        assert self.process.stdin is not None
        self.process.stdin.write(json.dumps(message) + "\n")
        self.process.stdin.flush()

    def request(self, method: str, params: dict[str, object]) -> dict[str, object]:
        request_id = self.next_id
        self.next_id += 1
        self.send({"id": request_id, "method": method, "params": params})
        message = self.read_until(lambda m: m.get("id") == request_id and "method" not in m)
        if "error" in message:
            raise SmokeError(f"{method} failed: {message['error']}")
        return message.get("result") or {}

    def read_until(self, predicate) -> dict[str, object]:
        deadline = time.monotonic() + self.timeout
        while time.monotonic() < deadline:
            line = None
            with self._lines_lock:
                if self._lines:
                    line = self._lines.pop(0)
            if line is None:
                if self.process.poll() is not None:
                    raise SmokeError(
                        f"app-server exited with {self.process.returncode}: {self.stderr_tail()}"
                    )
                time.sleep(0.02)
                continue
            line = line.strip()
            if not line:
                continue
            try:
                message = json.loads(line)
            except json.JSONDecodeError:
                continue
            self.transcript.append(message)
            if predicate(message):
                return message
        raise SmokeError(
            f"timed out waiting for app-server message; last messages: "
            f"{json.dumps(self.transcript[-5:], ensure_ascii=False)[:2000]}; stderr: {self.stderr_tail()}"
        )

    def stderr_tail(self) -> str:
        self._stderr.flush()
        try:
            return self.stderr_path.read_text(encoding="utf-8", errors="replace")[-2000:]
        except OSError:
            return ""

    def close(self) -> None:
        if self.process.poll() is None:
            try:
                self.process.stdin.close()  # type: ignore[union-attr]
            except Exception:  # noqa: BLE001
                pass
            try:
                self.process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                self.process.kill()
        self._stderr.close()


def run_turn(
    binary: Path,
    root: Path,
    base_url: str,
    approval_policy: str,
    timeout: float,
    script: list[dict[str, object]],
    decisions: dict[str, str],
) -> dict[str, object]:
    """Drive one App Server turn; `decisions` maps approval kinds to responses."""
    with MockResponses.lock:
        MockResponses.calls.clear()
        MockResponses.script = list(script)
    home = root / "zuno-home"
    workspace = root / "workspace"
    workspace.mkdir(parents=True)
    (workspace / "README.txt").write_text("smoke workspace\n", encoding="utf-8")
    write_config(home, base_url, approval_policy)
    result: dict[str, object] = {"approval_policy": approval_policy}
    app = AppServer(binary, home, workspace, timeout)
    try:
        app.request(
            "initialize",
            {
                "clientInfo": {
                    "name": "zuno_standalone_smoke",
                    "title": "Zuno standalone smoke",
                    "version": "0.1.0",
                }
            },
        )
        app.send({"method": "initialized"})
        thread = app.request("thread/start", {"cwd": str(workspace)})
        thread_id = thread["thread"]["id"]
        app.request(
            "turn/start",
            {
                "threadId": thread_id,
                "input": [{"type": "text", "text": "List the files in this directory."}],
            },
        )
        approvals: list[dict[str, object]] = []
        while True:
            message = app.read_until(
                lambda m: m.get("method") in (APPROVAL_METHOD, "turn/completed")
            )
            if message.get("method") != APPROVAL_METHOD:
                completed = message
                break
            params = message.get("params") or {}
            kind = params.get("kind") or "command"
            approvals.append(
                {
                    "kind": kind,
                    "command": params.get("command"),
                    "reason": params.get("reason"),
                }
            )
            decision = decisions.get(kind, "decline")
            app.send({"id": message["id"], "result": {"decision": decision}})
        result["approval_requests"] = approvals
        result["approval_request"] = approvals[0] if approvals else None
        status = ((completed.get("params") or {}).get("turn") or {}).get("status")
        if status != "completed":
            raise SmokeError(f"turn did not complete cleanly: {completed}")
        result["turn_status"] = status
        with MockResponses.lock:
            result["model_requests"] = len(MockResponses.calls)
            outputs: list[dict[str, object]] = []
            for call in MockResponses.calls:
                for item in call.get("input") or []:
                    if isinstance(item, dict) and item.get("type") == "function_call_output":
                        output = item.get("output")
                        if not isinstance(output, str):
                            output = json.dumps(output)
                        outputs.append({"call_id": item.get("call_id"), "output": output[:300]})
            result["tool_outputs"] = outputs
        executed = [
            message
            for message in app.transcript
            if message.get("method") == "item/completed"
            and ((message.get("params") or {}).get("item") or {}).get("type")
            == "commandExecution"
            and ((message.get("params") or {}).get("item") or {}).get("status")
            == "completed"
        ]
        result["command_executed"] = bool(executed)
        if result["model_requests"] < 2:
            raise SmokeError("the model was not told about the command outcome")
    finally:
        app.close()
    return result


LS_SCRIPT = [function_call("call-ls", "exec_command", {"cmd": "ls", "yield_time_ms": 1000})]


def strict_ls(binary: Path, root: Path, base_url: str, timeout: float) -> dict[str, object]:
    result = run_turn(binary, root, base_url, "untrusted", timeout, LS_SCRIPT, {})
    if result["approval_request"] is None:
        raise SmokeError("strict mode did not ask for approval before running ls")
    if "ls" not in (result["approval_request"].get("command") or ""):
        raise SmokeError(f"approval request did not carry the ls command: {result}")
    if result["command_executed"]:
        raise SmokeError("a command executed despite the decline")
    return result


def control_never(binary: Path, root: Path, base_url: str, timeout: float) -> dict[str, object]:
    result = run_turn(binary, root, base_url, "never", timeout, LS_SCRIPT, {})
    if result["approval_request"] is not None:
        raise SmokeError("approval_policy=never unexpectedly asked for approval")
    if not result["command_executed"]:
        raise SmokeError("approval_policy=never did not execute ls")
    return result


def strict_stdin(binary: Path, root: Path, base_url: str, timeout: float) -> dict[str, object]:
    """Approve opening a shell, then every later input must be reviewed again."""
    marker = root / "workspace" / "pwned"
    script = [
        function_call(
            "call-shell",
            "exec_command",
            {"cmd": "/bin/sh -i", "tty": True, "yield_time_ms": 500},
        ),
        function_call(
            "call-stdin",
            "write_stdin",
            {
                "session_id": SESSION_PLACEHOLDER,
                "chars": f"echo pwned > {marker}\n",
                "yield_time_ms": 1000,
            },
        ),
    ]
    # Accept the shell itself, decline everything typed into it afterwards.
    result = run_turn(
        binary, root, base_url, "untrusted", timeout, script, {"command": "accept"}
    )
    kinds = [approval["kind"] for approval in result["approval_requests"]]
    if kinds[:1] != ["command"]:
        raise SmokeError(f"expected the shell launch to be reviewed first, saw {kinds}")
    if "writeStdin" not in kinds:
        raise SmokeError(f"input to the approved shell was not reviewed: {kinds}")
    if marker.exists():
        raise SmokeError("declined terminal input still executed")
    result["stdin_reviewed"] = True
    return result


def run(binary: Path, timeout: float, keep: bool, allow_dynamic: bool) -> dict[str, object]:
    report: dict[str, object] = {"binary": str(binary)}
    report["elf"] = check_static_elf(binary, allow_dynamic)
    version = subprocess.run(
        [str(binary), "--version"],
        check=True,
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        timeout=timeout,
        env={"HOME": tempfile.gettempdir(), "PATH": os.environ.get("PATH", "")},
    ).stdout.strip()
    if not version.startswith("zuno "):
        raise SmokeError(f"unexpected --version output: {version!r}")
    report["version"] = version

    server = ThreadingHTTPServer(("127.0.0.1", 0), MockResponses)
    server_thread = threading.Thread(target=server.serve_forever, daemon=True)
    server_thread.start()
    base_url = f"http://127.0.0.1:{server.server_address[1]}"
    root = Path(tempfile.mkdtemp(prefix="zuno-standalone-smoke-"))
    try:
        report["strict"] = strict_ls(binary, root / "strict", base_url, timeout)
        # Control: the same model proposal under approval_policy = "never" must run
        # without any prompt, proving the prompt above came from the strict policy.
        report["control_never"] = control_never(binary, root / "control", base_url, timeout)
        # A persistent shell must not become an approval-free side channel.
        report["strict_stdin"] = strict_stdin(binary, root / "stdin", base_url, timeout)
    finally:
        server.shutdown()
        if keep:
            report["kept"] = str(root)
        else:
            shutil.rmtree(root, ignore_errors=True)
    report["ok"] = True
    return report


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True, help="path to the standalone zuno binary")
    parser.add_argument("--timeout", type=float, default=60.0, help="seconds to wait for each step")
    parser.add_argument("--keep", action="store_true", help="keep the temporary home for inspection")
    parser.add_argument(
        "--allow-dynamic",
        action="store_true",
        help="only exercise strict approval mode; accept a dynamically linked development build",
    )
    args = parser.parse_args()
    try:
        report = run(args.binary.resolve(), args.timeout, args.keep, args.allow_dynamic)
    except (SmokeError, subprocess.CalledProcessError, subprocess.TimeoutExpired) as error:
        print(json.dumps({"ok": False, "error": str(error)}, indent=2, ensure_ascii=False))
        return 1
    print(json.dumps(report, indent=2, ensure_ascii=False))
    return 0


if __name__ == "__main__":
    sys.exit(main())
