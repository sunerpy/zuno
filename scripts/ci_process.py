"""Bounded process supervision shared by local and hosted CI test execution."""

from contextlib import contextmanager
from dataclasses import dataclass
import os
from pathlib import Path
import signal
import subprocess
import tempfile
import threading
import time

EXECUTION_TIMEOUT = 124
SUPERVISION_FAILURE = 125
CANCELLED = 130
CLEANUP_SECONDS = 5.0
MAX_LOG_BYTES = 64 * 1024 * 1024


@dataclass(frozen=True)
class Result:
    code: int
    elapsed: float
    reason: str | None = None


@contextmanager
def cancellation_signals(cancelled):
    """Let active workers clean up before the scheduler leaves its thread pool."""
    previous = {}
    for number in (signal.SIGINT, signal.SIGTERM):
        previous[number] = signal.signal(
            number, lambda _number, _frame: cancelled.set()
        )
    try:
        yield
    finally:
        for number, handler in previous.items():
            signal.signal(number, handler)


def _live_group(pgid):
    if Path("/proc").is_dir():
        # A dead orphan may briefly remain as a zombie until init reaps it.
        # Zombies cannot execute or retain the inherited output file handles.
        for entry in Path("/proc").iterdir():
            if not entry.name.isdecimal():
                continue
            try:
                fields = (entry / "stat").read_text(encoding="utf-8", errors="replace").rsplit(")", 1)[1].split()
                if int(fields[2]) == pgid and fields[0] not in ("Z", "X"):
                    return True
            except (FileNotFoundError, ProcessLookupError, PermissionError):
                continue
        return False
    try:
        os.killpg(pgid, 0)
        return True
    except ProcessLookupError:
        return False


def _terminate_group(process, deadline):
    try:
        os.killpg(process.pid, signal.SIGKILL)
    except ProcessLookupError:
        pass
    process.wait(timeout=max(0.001, deadline - time.monotonic()))
    while _live_group(process.pid):
        if time.monotonic() >= deadline:
            raise TimeoutError("test process group still contains live processes")
        time.sleep(0.01)


def _wait(process, deadline, cancelled):
    while True:
        code = process.poll()
        if code is not None:
            return code, None
        if cancelled.is_set():
            return CANCELLED, "test execution was cancelled"
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            return EXECUTION_TIMEOUT, "test execution exceeded its deadline"
        cancelled.wait(min(0.05, remaining))


def _copy_output(path, output, budget):
    size = path.stat().st_size
    remaining = min(size, budget)
    copied = remaining
    with path.open("rb") as stream:
        while remaining:
            chunk = stream.read(min(remaining, 64 * 1024))
            if not chunk:
                break
            output.write(chunk)
            remaining -= len(chunk)
    return budget - copied, size > copied


def run(command, cwd, env, timeout, log_path, cancelled=None):
    """Run once, reap descendants, and preserve output without waiting for pipe EOF.

    The execution deadline includes launcher startup. Cleanup has a separate
    five-second budget; every wait remains bounded even after termination fails.
    Cancellation never replays a test. A failed supervisor is a failing result.
    """
    if timeout <= 0:
        raise ValueError("test timeout must be positive")
    cancelled = cancelled if cancelled is not None else threading.Event()
    log_path = Path(log_path)
    log_path.parent.mkdir(parents=True, exist_ok=True)
    began = time.monotonic()
    code, reason = SUPERVISION_FAILURE, None
    process, job = None, None
    with tempfile.TemporaryDirectory(
        prefix="suite-", dir=log_path.parent, ignore_cleanup_errors=True
    ) as temp:
        stdout_path, stderr_path = Path(temp) / "stdout", Path(temp) / "stderr"
        with stdout_path.open("wb") as stdout, stderr_path.open("wb") as stderr:
            try:
                if cancelled.is_set():
                    code, reason = CANCELLED, "test was cancelled before launch"
                else:
                    if os.name == "nt":
                        from ci_windows_job import WindowsJob
                        job = WindowsJob()
                        process = job.spawn(command, cwd, env, stdout, stderr)
                    else:
                        process = subprocess.Popen(
                            command, cwd=cwd, env=env, stdin=subprocess.DEVNULL,
                            stdout=stdout, stderr=stderr, start_new_session=True,
                        )
                    code, reason = _wait(process, began + timeout, cancelled)
            except Exception as error:
                code, reason = SUPERVISION_FAILURE, f"test launch failed: {error}"
            finally:
                deadline = time.monotonic() + CLEANUP_SECONDS
                try:
                    if job is not None:
                        job.terminate(deadline)
                        if process is not None:
                            process.wait(timeout=max(0.001, deadline - time.monotonic()))
                    elif process is not None:
                        _terminate_group(process, deadline)
                except Exception as error:
                    code = SUPERVISION_FAILURE
                    reason = f"{reason or 'test ended'}; process cleanup failed: {error}"
                finally:
                    if job is not None:
                        job.close()
        with log_path.open("wb") as output:
            budget = MAX_LOG_BYTES
            for path in (stdout_path, stderr_path):
                budget, truncated = _copy_output(path, output, budget)
                if truncated:
                    if code == 0:
                        code = SUPERVISION_FAILURE
                    reason = f"{reason or 'test ended'}; output exceeded {MAX_LOG_BYTES} bytes"
            if reason:
                output.write(f"\nCI PROCESS: {reason}\n".encode("utf-8"))
    return Result(code, time.monotonic() - began, reason)
