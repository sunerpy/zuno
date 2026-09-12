#!/usr/bin/env python3
"""Verify the gateway against an isolated rootless Docker daemon."""

import json
import os
from pathlib import Path
import shutil
import signal
import subprocess
import tempfile
import time

IMAGE = "public.ecr.aws/docker/library/alpine@sha256:14358309a308569c32bdc37e2e0e9694be33a9d99e68afb0f5ff33cc1f695dce"


def main():
    repository = Path(__file__).resolve().parents[1]
    environment = os.environ.copy()
    supplied = environment.get("ZUNO_ROOTLESS_DOCKER_SOCKET")
    daemon = None
    log = None
    root = None
    try:
        if supplied:
            socket = Path(supplied)
        else:
            for command in ["docker", "dockerd-rootless.sh", "newuidmap", "newgidmap"]:
                if not shutil.which(command):
                    raise SystemExit(f"rootless Docker prerequisite missing: {command}")
            runtime = Path(environment.get("XDG_RUNTIME_DIR", f"/run/user/{os.getuid()}"))
            if not (runtime / "bus").exists():
                raise SystemExit("rootless Docker requires the user's systemd D-Bus and delegated cgroups")
            root = Path(tempfile.mkdtemp(prefix="zuno-preview-docker-", dir=environment.get("RUNNER_TEMP")))
            root.chmod(0o700)
            control = Path(tempfile.mkdtemp(prefix="zuno-docker-", dir=runtime))
            control.chmod(0o700)
            socket = control / "docker.sock"
            environment["XDG_RUNTIME_DIR"] = str(runtime)
            environment["DBUS_SESSION_BUS_ADDRESS"] = f"unix:path={runtime / 'bus'}"
            environment["DOCKERD_ROOTLESS_ROOTLESSKIT_STATE_DIR"] = str(control / "rootlesskit")
            log = (root / "daemon.log").open("wb")
            daemon = subprocess.Popen(
                [
                    "dockerd-rootless.sh", "--host", f"unix://{socket}",
                    "--data-root", str(root / "data"), "--exec-root", str(control / "exec"),
                    "--pidfile", str(control / "docker.pid"),
                ],
                env=environment, stdout=log, stderr=log, start_new_session=True,
            )
            deadline = time.monotonic() + 45
            while not socket.exists():
                if daemon.poll() is not None or time.monotonic() >= deadline:
                    log.flush()
                    print("\n".join((root / "daemon.log").read_text(errors="replace").splitlines()[-25:]))
                    raise SystemExit("isolated rootless Docker did not become ready")
                time.sleep(0.2)
        docker = ["docker", "--host", f"unix://{socket}"]
        info = json.loads(subprocess.check_output(docker + ["info", "--format", "{{json .}}"], text=True))
        if "name=rootless" not in info.get("SecurityOptions", []) or info.get("CgroupDriver") != "systemd":
            raise SystemExit("refusing a non-rootless or unconfined Docker test backend")
        subprocess.run(docker + ["pull", IMAGE], check=True)
        environment["ZUNO_ROOTLESS_DOCKER_SOCKET"] = str(socket)
        if environment.get("ZUNO_ENTERPRISE_ARTIFACT_SMOKE") != "1":
            subprocess.run(
                ["cargo", "test", "-p", "zuno-environment", "--", "--include-ignored"],
                cwd=repository, env=environment, check=True,
            )
        # Require the complete authenticated gateway path in this native gate.
        # The PostgreSQL-only gate separately exercises the control API without
        # starting Docker; it cannot stand in for this execution evidence.
        environment["ZUNO_GATEWAY_TEST_REQUIRED"] = "1"
        subprocess.run(
            ["python3", str(repository / "scripts/check_enterprise_postgres.py")],
            cwd=repository, env=environment, check=True,
        )
    finally:
        if daemon is not None:
            if daemon.poll() is None:
                os.killpg(daemon.pid, signal.SIGTERM)
                try:
                    daemon.wait(timeout=15)
                except subprocess.TimeoutExpired:
                    os.killpg(daemon.pid, signal.SIGKILL)
                    daemon.wait(timeout=10)
            if log is not None:
                log.close()
            # The isolated daemon's image layers may contain subordinate UID
            # ownership. Do not use a host-root recursive deletion here.
            print(f"Rootless test evidence and task-owned data: {root}")


if __name__ == "__main__":
    main()
