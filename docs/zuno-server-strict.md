# Standalone binary and strict approval mode

Zuno can run on a server as one static executable and stop for a human decision
before every command it proposes. This covers the common troubleshooting shape:
copy one file to a host, log in, and let the model suggest diagnostics while a
person approves each step.

## Standalone binary

Each release publishes, next to the six platform packages, a statically linked
Linux executable:

- `zuno-standalone-x86_64-unknown-linux-musl`

It has no dynamic loader and no shared-library dependencies, so it runs on any
x86_64 Linux kernel regardless of the distribution's glibc, including a
`FROM scratch` container. Copy it anywhere on `PATH`, `chmod +x`, and run it as
`zuno`.

What the single file does not carry, and how that affects a server:

- **Sandbox helpers.** The packages bundle a `bwrap` companion for the Linux
  sandbox; the standalone binary does not. Use `sandbox_mode = "danger-full-access"`
  (or install the distribution's `bubblewrap`, which Zuno picks up from `PATH`).
  In strict approval mode the approval prompt, not the sandbox, is the safety
  boundary.
- **Code Mode host.** Code Mode is off by default and stays off; it needs the
  `codex-code-mode-host` companion from the full package.
- **TLS roots.** HTTPS uses the host's certificate store (`/etc/ssl/certs` or
  `SSL_CERT_FILE`). A `scratch` container therefore needs the CA bundle mounted
  when it talks to a remote provider.

Local build of the same artifact:

```sh
rustup target add x86_64-unknown-linux-musl
cd codex-rs
CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER=musl-gcc \
CC_x86_64_unknown_linux_musl=musl-gcc \
AWS_LC_SYS_NO_JITTER_ENTROPY=1 \
cargo build --locked --release --target x86_64-unknown-linux-musl -p codex-cli --bin zuno
```

## Strict approval mode

`approval_policy = "untrusted"` requires approval for every shell command and
every file edit unless an explicit `allow` rule in `$ZUNO_HOME/rules/*.rules`
matches it. No rules ship by default, so nothing runs unprompted. Codex retired
this value as a user-selectable policy; Zuno accepts it again in `config.toml`,
in profile files, and on the command line:

```sh
# one-off
zuno -a untrusted -s danger-full-access

# as a reusable profile: copy examples/zuno-config/server-strict.config.toml to
# $ZUNO_HOME/server-strict.config.toml, set your provider, then
zuno --profile server-strict
```

The profile also sets `approvals_reviewer = "user"` so an automatic reviewer can
never approve on your behalf. "Approve for this session" in the prompt still
lets you stop being asked for an identical command you already reviewed.

Persistent terminals are covered too. When the model keeps a shell open with
`exec_command` and later types into it with `write_stdin`, strict mode reviews
every such input as a new command (only a bare Ctrl-C is exempt), regardless of
the experimental `write_stdin_approval` feature and even when the terminal's
permissions did not change.

What can still skip the prompt is only what the operator wrote down: an
`allow` rule in `$ZUNO_HOME/rules/*.rules`, or a `PermissionRequest` hook in
`hooks.json` that answers `allow`. The strict profile ships neither; do not add
them on a host where every command must be reviewed.

`zuno exec` is headless and always runs with `approval_policy = "never"`; strict
mode applies to the interactive TUI, `zuno app-server`, and `zuno acp`.

## Smoke test

`scripts/zuno_standalone_smoke.py` needs only the Python standard library. It
verifies the ELF has no dynamic loader, runs `zuno --version`, then drives
`zuno app-server` against a local mock model in three phases: strict mode must
emit `item/commandExecution/requestApproval` for a proposed `ls` and declining
must execute nothing; `approval_policy = "never"` must run the same `ls` without
a prompt; and after an approved `/bin/sh -i`, a follow-up `write_stdin` must be
reviewed again (kind `writeStdin`) and, once declined, leave no trace.

```sh
python3 scripts/zuno_standalone_smoke.py --binary ./zuno-standalone-x86_64-unknown-linux-musl

# the same check inside containers that have nothing but the binary
docker run --rm -v "$PWD/zuno:/zuno:ro" scratch /zuno --version
docker run --rm -v "$PWD:/work:ro" -w /work python:3.12-alpine \
  python3 scripts/zuno_standalone_smoke.py --binary ./zuno
```

The PR gate runs the build and the smoke test on every candidate, and the
release promotion publishes the binary with the packages.
