"""Windows process-tree containment for the native test runner.

The launcher waits for a handshake before starting a test. This makes assigning
the launcher to its Job Object happen before any test-controlled child exists.
"""

import ctypes
from ctypes import wintypes
import subprocess
import sys
import time


class _BasicLimits(ctypes.Structure):
    _fields_ = [
        ("process_time", ctypes.c_longlong),
        ("job_time", ctypes.c_longlong),
        ("flags", wintypes.DWORD),
        ("minimum_working_set", ctypes.c_size_t),
        ("maximum_working_set", ctypes.c_size_t),
        ("active_process_limit", wintypes.DWORD),
        ("affinity", ctypes.c_size_t),
        ("priority", wintypes.DWORD),
        ("scheduling", wintypes.DWORD),
    ]


class _IoCounters(ctypes.Structure):
    _fields_ = [(name, ctypes.c_ulonglong) for name in (
        "read_operations", "write_operations", "other_operations",
        "read_bytes", "write_bytes", "other_bytes",
    )]


class _ExtendedLimits(ctypes.Structure):
    _fields_ = [
        ("basic", _BasicLimits),
        ("io", _IoCounters),
        ("process_memory", ctypes.c_size_t),
        ("job_memory", ctypes.c_size_t),
        ("peak_process_memory", ctypes.c_size_t),
        ("peak_job_memory", ctypes.c_size_t),
    ]


class _Accounting(ctypes.Structure):
    _fields_ = [
        ("user_time", ctypes.c_longlong),
        ("kernel_time", ctypes.c_longlong),
        ("period_user_time", ctypes.c_longlong),
        ("period_kernel_time", ctypes.c_longlong),
        ("page_faults", wintypes.DWORD),
        ("total_processes", wintypes.DWORD),
        ("active_processes", wintypes.DWORD),
        ("terminated_processes", wintypes.DWORD),
    ]


def _api():
    api = ctypes.WinDLL("kernel32", use_last_error=True)
    api.CreateJobObjectW.argtypes = [ctypes.c_void_p, wintypes.LPCWSTR]
    api.CreateJobObjectW.restype = wintypes.HANDLE
    api.SetInformationJobObject.argtypes = [
        wintypes.HANDLE, ctypes.c_int, ctypes.c_void_p, wintypes.DWORD,
    ]
    api.AssignProcessToJobObject.argtypes = [wintypes.HANDLE, wintypes.HANDLE]
    api.OpenProcess.argtypes = [wintypes.DWORD, wintypes.BOOL, wintypes.DWORD]
    api.OpenProcess.restype = wintypes.HANDLE
    api.QueryInformationJobObject.argtypes = [
        wintypes.HANDLE, ctypes.c_int, ctypes.c_void_p,
        wintypes.DWORD, ctypes.c_void_p,
    ]
    api.TerminateJobObject.argtypes = [wintypes.HANDLE, wintypes.UINT]
    api.CloseHandle.argtypes = [wintypes.HANDLE]
    return api


class WindowsJob:
    """Own a Job Object and refuse to run a payload before assignment succeeds."""

    def __init__(self):
        self.api = _api()
        self.handle = self.api.CreateJobObjectW(None, None)
        if not self.handle:
            raise ctypes.WinError(ctypes.get_last_error())
        limits = _ExtendedLimits()
        limits.basic.flags = 0x2000  # JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE
        if not self.api.SetInformationJobObject(
            self.handle, 9, ctypes.byref(limits), ctypes.sizeof(limits)
        ):
            error = ctypes.WinError(ctypes.get_last_error())
            self.close()
            raise error

    def spawn(self, command, cwd, env, stdout, stderr):
        launcher = (
            "import subprocess,sys;"
            "message=sys.stdin.buffer.readline();"
            "sys.stdin.close();"
            "assert message==b'start\\n';"
            "child=subprocess.Popen(sys.argv[1:],stdin=subprocess.DEVNULL,"
            "stdout=sys.stdout,stderr=sys.stderr);"
            "sys.exit(child.wait())"
        )
        process = subprocess.Popen(
            [sys.executable, "-c", launcher, *command],
            cwd=cwd, env=env, stdin=subprocess.PIPE, stdout=stdout, stderr=stderr,
            creationflags=subprocess.CREATE_NEW_PROCESS_GROUP,
        )
        # Open the still-waiting launcher with the rights documented for
        # AssignProcessToJobObject; do not depend on Popen's private _handle.
        handle = self.api.OpenProcess(0x0100 | 0x0001, False, process.pid)
        try:
            if not handle:
                raise ctypes.WinError(ctypes.get_last_error())
            if not self.api.AssignProcessToJobObject(self.handle, handle):
                raise ctypes.WinError(ctypes.get_last_error())
            process.stdin.write(b"start\n")
            process.stdin.flush()
        except BaseException:
            # The launcher cannot have spawned a payload before the handshake.
            process.kill()
            process.wait(timeout=5)
            raise
        finally:
            if handle:
                self.api.CloseHandle(handle)
            process.stdin.close()
        return process

    def terminate(self, deadline):
        if not self.api.TerminateJobObject(self.handle, 1):
            raise ctypes.WinError(ctypes.get_last_error())
        while True:
            accounting = _Accounting()
            if not self.api.QueryInformationJobObject(
                self.handle, 1, ctypes.byref(accounting),
                ctypes.sizeof(accounting), None,
            ):
                raise ctypes.WinError(ctypes.get_last_error())
            if accounting.active_processes == 0:
                return
            if time.monotonic() >= deadline:
                raise TimeoutError("test Job Object still contains active processes")
            time.sleep(0.01)

    def close(self):
        if self.handle:
            self.api.CloseHandle(self.handle)
            self.handle = None
