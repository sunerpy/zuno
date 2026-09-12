#!/usr/bin/env python3
"""Run the PostgreSQL backend contract against an isolated local TLS cluster."""

import json
import os
from pathlib import Path
import secrets
import shutil
import socket
import subprocess
import tempfile


def main():
    repository = Path(__file__).resolve().parents[1]
    configured_bindir = os.environ.get("ZUNO_POSTGRES_BINDIR")
    bindir = Path(configured_bindir or subprocess.check_output(["pg_config", "--bindir"], text=True).strip())
    for binary in ["postgres", "initdb", "pg_ctl"]:
        if not (bindir / binary).is_file():
            raise SystemExit(f"PostgreSQL server binary missing: {bindir / binary}")
    if not shutil.which("openssl"):
        raise SystemExit("OpenSSL is required to create the isolated test certificate")
    with tempfile.TemporaryDirectory(prefix="zuno-enterprise-pg-") as temporary:
        root = Path(temporary)
        root.chmod(0o700)
        setup = (root / "setup.log").open("wb")

        def run(command, **kwargs):
            return subprocess.run(command, check=True, stdout=setup, stderr=setup, **kwargs)

        def private(path, content):
            path.write_text(content, encoding="utf-8")
            path.chmod(0o600)

        admin_password = secrets.token_hex(24)
        runtime_password = secrets.token_hex(24)
        migration_password = secrets.token_hex(24)
        private(root / "admin.password", admin_password)
        run([
            str(bindir / "initdb"), "-D", str(root / "data"), "--no-locale", "--encoding=UTF8",
            "--username=zuno_preview_admin", "--auth=scram-sha-256",
            f"--pwfile={root / 'admin.password'}",
        ])
        run([
            "openssl", "req", "-x509", "-newkey", "rsa:2048", "-nodes",
            "-keyout", str(root / "ca.key"), "-out", str(root / "ca.pem"),
            "-subj", "/CN=ZunoPreviewTestCA", "-days", "1",
            "-addext", "basicConstraints=critical,CA:TRUE",
        ])
        run([
            "openssl", "req", "-newkey", "rsa:2048", "-nodes",
            "-keyout", str(root / "server.key"), "-out", str(root / "server.csr"),
            "-subj", "/CN=localhost",
        ])
        private(root / "extensions.cnf", (
            "basicConstraints=critical,CA:FALSE\n"
            "keyUsage=critical,digitalSignature,keyEncipherment\n"
            "extendedKeyUsage=serverAuth\nsubjectAltName=DNS:localhost,IP:127.0.0.1\n"
        ))
        run([
            "openssl", "x509", "-req", "-in", str(root / "server.csr"),
            "-CA", str(root / "ca.pem"), "-CAkey", str(root / "ca.key"), "-CAcreateserial",
            "-out", str(root / "server.crt"), "-days", "1", "-extfile", str(root / "extensions.cnf"),
        ])
        (root / "server.key").chmod(0o600)
        with socket.socket() as probe:
            probe.bind(("127.0.0.1", 0))
            port = probe.getsockname()[1]
        quote = lambda text: str(text).replace("'", "''")
        with (root / "data/postgresql.conf").open("a", encoding="utf-8") as config:
            config.write(
                f"\nlisten_addresses='127.0.0.1'\nport={port}\n"
                f"unix_socket_directories='{quote(root)}'\nssl=on\n"
                f"ssl_cert_file='{quote(root / 'server.crt')}'\n"
                f"ssl_key_file='{quote(root / 'server.key')}'\n"
            )
        started = False
        try:
            run([
                str(bindir / "pg_ctl"), "-D", str(root / "data"), "-l", str(root / "postgres.log"),
                "-w", "-t", "15", "start",
            ])
            started = True
            private(root / "pgpass", f"127.0.0.1:{port}:postgres:zuno_preview_admin:{admin_password}\n")
            environment = os.environ.copy()
            environment.update({
                "PGPASSFILE": str(root / "pgpass"),
                "PGSSLMODE": "verify-full", "PGSSLROOTCERT": str(root / "ca.pem"),
            })
            run(
                [
                    "psql", "-h", "127.0.0.1", "-p", str(port), "-U", "zuno_preview_admin",
                    "-d", "postgres", "-v", "ON_ERROR_STOP=1",
                ],
                input=(
                    "CREATE ROLE zuno_preview_runtime LOGIN NOSUPERUSER NOBYPASSRLS "
                    f"NOCREATEDB NOCREATEROLE PASSWORD '{runtime_password}';\n"
                    "CREATE ROLE zuno_preview_migrator LOGIN NOSUPERUSER NOBYPASSRLS "
                    f"NOCREATEDB NOCREATEROLE PASSWORD '{migration_password}';\n"
                    "GRANT CREATE ON DATABASE postgres TO zuno_preview_migrator;\n"
                ).encode(),
                env=environment,
            )
            private(root / "fixture.json", json.dumps({
                "admin_url": f"postgresql://zuno_preview_admin:{admin_password}@127.0.0.1:{port}/postgres",
                "runtime_url": f"postgresql://zuno_preview_runtime:{runtime_password}@127.0.0.1:{port}/postgres",
                "migration_url": f"postgresql://zuno_preview_migrator:{migration_password}@127.0.0.1:{port}/postgres",
                "root_certificate": str(root / "ca.pem"),
                "runtime_role": "zuno_preview_runtime",
            }))
            environment["ZUNO_POSTGRES_TEST_CONFIG"] = str(root / "fixture.json")
            # The Rust tests provide trust explicitly. Do not let libpq defaults
            # accidentally make the negative certificate-verification case pass.
            for variable in ["PGSSLROOTCERT", "PGSSLMODE", "PGPASSFILE"]:
                environment.pop(variable, None)
            subprocess.run(
                ["cargo", "test", "-p", "zuno-postgres", "--lib", "--", "--include-ignored"],
                cwd=repository, env=environment, check=True,
            )
            subprocess.run(
                ["cargo", "test", "-p", "zuno-server", "--features", "enterprise", "--test", "enterprise_state", "--", "--include-ignored"],
                cwd=repository, env=environment, check=True,
            )
        except subprocess.CalledProcessError as error:
            setup.flush()
            if not error.cmd or error.cmd[0] != "cargo":
                # Never echo connection URLs or generated passwords.
                text = (root / "setup.log").read_text(errors="replace")
                for password in [admin_password, runtime_password, migration_password]:
                    text = text.replace(password, "<redacted>")
                print("\n".join(text.splitlines()[-20:]))
            raise
        finally:
            if started or (root / "data/postmaster.pid").is_file():
                subprocess.run(
                    [str(bindir / "pg_ctl"), "-D", str(root / "data"), "-m", "fast", "-w", "-t", "15", "stop"],
                    stdout=setup, stderr=setup, check=False,
                )
            setup.close()


if __name__ == "__main__":
    main()
