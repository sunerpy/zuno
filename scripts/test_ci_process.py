"""Exercise real descendants and output handles, including the CI hang regression."""

import ctypes
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import threading
import time
import unittest
from unittest.mock import patch

import ci_process

SCRIPTS = Path(__file__).resolve().parent


def alive(pid):
    if os.name == "nt":
        from ctypes import wintypes
        api = ctypes.WinDLL("kernel32", use_last_error=True)
        api.OpenProcess.argtypes = [wintypes.DWORD, wintypes.BOOL, wintypes.DWORD]
        api.OpenProcess.restype = wintypes.HANDLE
        api.GetExitCodeProcess.argtypes = [wintypes.HANDLE, ctypes.POINTER(wintypes.DWORD)]
        api.CloseHandle.argtypes = [wintypes.HANDLE]
        handle = api.OpenProcess(0x1000, False, pid)
        if not handle:
            return False
        try:
            code = wintypes.DWORD()
            return bool(api.GetExitCodeProcess(handle, ctypes.byref(code))) and code.value == 259
        finally:
            api.CloseHandle(handle)
    stat = Path(f"/proc/{pid}/stat")
    if Path("/proc").is_dir():
        try:
            return stat.read_text(encoding="utf-8", errors="replace").rsplit(")", 1)[1].split()[0] not in ("Z", "X")
        except FileNotFoundError:
            return False
    try:
        os.kill(pid, 0)
        return True
    except ProcessLookupError:
        return False


class ProcessTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.log = self.root / "output.log"
        self.env = os.environ.copy()
        self.env["PYTHONPATH"] = str(SCRIPTS)
        self.env["PYTHONUTF8"] = "1"
        self.env["PYTHONIOENCODING"] = "utf-8"

    def run_code(self, code, timeout=10, cancelled=None):
        return ci_process.run(
            [sys.executable, "-c", code], self.root, self.env,
            timeout, self.log, cancelled,
        )

    def tree_code(self, parent_wait):
        leaf = self.root / "leaf.pid"
        parent = self.root / "parent.pid"
        child_code = (
            "import os,time;from pathlib import Path;"
            f"Path({str(leaf)!r}).write_text(str(os.getpid()));time.sleep(120)"
        )
        code = (
            "import os,subprocess,sys,time\nfrom pathlib import Path\n"
            f"Path({str(parent)!r}).write_text(str(os.getpid()))\n"
            f"subprocess.Popen([sys.executable,'-c',{child_code!r}])\n"
            f"ready=Path({str(leaf)!r})\n"
            "while not ready.exists(): time.sleep(0.01)\n"
            "print('parent ready',flush=True)\n"
            f"time.sleep({parent_wait})\n"
        )
        return code, (parent, leaf)

    def assert_reaped(self, paths):
        pids = [int(path.read_text(encoding="utf-8", errors="replace")) for path in paths]
        deadline = time.monotonic() + 5
        while any(alive(pid) for pid in pids) and time.monotonic() < deadline:
            time.sleep(0.02)
        self.assertFalse(any(alive(pid) for pid in pids), pids)

    def test_preserves_exit_status_and_both_unicode_streams(self):
        result = self.run_code(
            "import sys;print('stdout 中文');print('stderr 中文',file=sys.stderr);sys.exit(7)"
        )
        self.assertEqual(result.code, 7)
        self.assertIsNone(result.reason)
        self.assertEqual(self.log.read_text(encoding="utf-8").splitlines(),
                         ["stdout 中文", "stderr 中文"])

    def test_exited_parent_with_inherited_output_does_not_wait_for_eof(self):
        code, pids = self.tree_code(0)
        result = self.run_code(code)
        self.assertEqual(result.code, 0, self.log.read_text(encoding="utf-8", errors="replace"))
        self.assertLess(result.elapsed, 10)
        self.assertIn("parent ready", self.log.read_text(encoding="utf-8", errors="replace"))
        self.assert_reaped(pids)

    def test_timeout_terminates_the_entire_running_tree(self):
        code, pids = self.tree_code(120)
        result = self.run_code(code, timeout=3)
        self.assertEqual(result.code, ci_process.EXECUTION_TIMEOUT, self.log.read_text(encoding="utf-8", errors="replace"))
        self.assertLess(result.elapsed, 10)
        self.assert_reaped(pids)

    def test_cancellation_before_launch_starts_no_program(self):
        cancelled = threading.Event()
        cancelled.set()
        marker = self.root / "must-not-exist"
        result = self.run_code(
            f"from pathlib import Path;Path({str(marker)!r}).touch()", cancelled=cancelled
        )
        self.assertEqual(result.code, ci_process.CANCELLED)
        self.assertFalse(marker.exists())

    def test_cancellation_reaps_a_running_tree(self):
        code, pids = self.tree_code(120)
        cancelled = threading.Event()
        stop = threading.Event()

        def cancel_when_started():
            deadline = time.monotonic() + 10
            while not stop.wait(0.01):
                if pids[1].exists() or time.monotonic() >= deadline:
                    cancelled.set()
                    return

        watcher = threading.Thread(target=cancel_when_started)
        watcher.start()
        try:
            result = self.run_code(code, timeout=15, cancelled=cancelled)
        finally:
            stop.set()
            watcher.join(timeout=2)
        self.assertFalse(watcher.is_alive())
        self.assertEqual(result.code, ci_process.CANCELLED, self.log.read_text(encoding="utf-8", errors="replace"))
        self.assert_reaped(pids)

    def test_capture_larger_than_a_pipe_buffer_does_not_deadlock(self):
        result = self.run_code(
            "import sys;sys.stdout.write('a'*2000000);sys.stderr.write('b'*2000000)"
        )
        self.assertEqual(result.code, 0)
        self.assertEqual(self.log.stat().st_size, 4_000_000)

    def test_log_limit_is_a_failure_instead_of_silent_truncation(self):
        with patch.object(ci_process, "MAX_LOG_BYTES", 1024):
            result = self.run_code("print('x'*8192)")
        self.assertEqual(result.code, ci_process.SUPERVISION_FAILURE)
        self.assertIn("output exceeded", self.log.read_text(encoding="utf-8", errors="replace"))
        self.assertLess(self.log.stat().st_size, 2048)

    def test_missing_executable_is_a_recorded_failure(self):
        result = ci_process.run(
            [str(self.root / "absent.exe")], self.root, self.env, 5, self.log
        )
        self.assertNotEqual(result.code, 0)
        self.assertTrue(self.log.read_text(encoding="utf-8", errors="replace"))

    @unittest.skipUnless(os.name == "nt", "Windows nested Job Object")
    def test_nested_supervisors_work_with_an_existing_parent_job(self):
        inner_log = self.root / "inner.log"
        inner = (
            "import os,sys;from ci_process import run;"
            f"r=run([sys.executable,'-c','print(42)'],os.getcwd(),os.environ.copy(),5,{str(inner_log)!r});"
            "assert r.code==0,r"
        )
        result = self.run_code(inner)
        self.assertEqual(result.code, 0, self.log.read_text(encoding="utf-8", errors="replace"))
        self.assertEqual(inner_log.read_text(encoding="utf-8", errors="replace").strip(), "42")

    @unittest.skipUnless(os.name == "nt", "Windows kill-on-close")
    def test_killing_the_supervisor_reaps_its_owned_job(self):
        code, pids = self.tree_code(120)
        driver = (
            "import os,sys;from ci_process import run;"
            f"run([sys.executable,'-c',{code!r}],os.getcwd(),os.environ.copy(),120,{str(self.log)!r})"
        )
        with (self.root / "driver.log").open("wb") as output:
            process = subprocess.Popen(
                [sys.executable, "-c", driver], cwd=self.root, env=self.env,
                stdout=output, stderr=output,
            )
            try:
                deadline = time.monotonic() + 10
                while not pids[1].exists() and time.monotonic() < deadline:
                    time.sleep(0.02)
                self.assertTrue(pids[1].exists())
            finally:
                process.kill()
                process.wait(timeout=5)
        self.assert_reaped(pids)


if __name__ == "__main__":
    unittest.main()
