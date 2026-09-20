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
- **Code Mode host.** Built in. Models whose catalog entry says
  `tool_mode = "code_mode_only"` (the gpt-5.6 families, which is everything a
  Kiro provider serves) run every tool call through `codex-code-mode-host`. The
  packages ship it as a sibling binary; the standalone binary carries it inside
  and reaches it through an arg0 alias in the per-session helper directory under
  `$ZUNO_HOME/tmp/arg0`. Release builds refuse to create helpers when
  `$ZUNO_HOME` sits under the system temp directory, so keep the home elsewhere
  (the default `~/.zuno` is fine); otherwise these models answer
  "the shell tool failed to start" after the approval prompt.
- **TLS roots.** HTTPS uses the host's certificate store (`/etc/ssl/certs` or
  `SSL_CERT_FILE`). A `scratch` container therefore needs the CA bundle mounted
  when it talks to a remote provider.

Local build of the same artifact. The embedded host links the prebuilt V8
archive that Codex publishes for musl; download the archive, the bindgen output
and the checksum manifest from the `rusty-v8-v<version>` release of
`openai/codex` (the `.github/actions/setup-rusty-v8` action shows the exact
file names and checks the manifest against `third_party/v8/`):

```sh
rustup target add x86_64-unknown-linux-musl
cd codex-rs
RUSTY_V8_ARCHIVE=/path/to/librusty_v8_ptrcomp_sandbox_release_x86_64-unknown-linux-musl.a.gz \
RUSTY_V8_SRC_BINDING_PATH=/path/to/src_binding_ptrcomp_sandbox_release_x86_64-unknown-linux-musl.rs \
CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER=musl-gcc \
CC_x86_64_unknown_linux_musl=musl-gcc \
AWS_LC_SYS_NO_JITTER_ENTROPY=1 \
cargo build --locked --release --target x86_64-unknown-linux-musl -p codex-cli --bin zuno
```

The musl target is what selects the embedded host: `codex-cli` lists
`codex-code-mode-host` as a musl-only dependency (next to the jemalloc
allocator), so no cargo feature is involved and the glibc, macOS and Windows
packages are unaffected.

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
`zuno app-server` against a local mock model in four phases: strict mode must
emit `item/commandExecution/requestApproval` for a proposed `ls` and declining
must execute nothing; `approval_policy = "never"` must run the same `ls` without
a prompt; after an approved `/bin/sh -i`, a follow-up `write_stdin` must be
reviewed again (kind `writeStdin`) and, once declined, leave no trace; and with
a `code_mode_only` model slug the JavaScript program the mock sends must reach
the embedded host, its `exec_command` must stop at the approval prompt, and its
output must travel back to the model.

```sh
python3 scripts/zuno_standalone_smoke.py --binary ./zuno-standalone-x86_64-unknown-linux-musl

# the same check inside containers that have nothing but the binary
docker run --rm -v "$PWD/zuno:/zuno:ro" scratch /zuno --version
docker run --rm -v "$PWD:/work:ro" -w /work python:3.12-alpine \
  python3 scripts/zuno_standalone_smoke.py --binary ./zuno
```

The PR gate runs the build, the smoke test on the runner and the same smoke
inside a `python:3.13-slim` container on every candidate, and the release
promotion publishes the binary with the packages.

`scripts/zuno_standalone_live_check.py` repeats the strict-mode checks against a
real Responses-compatible provider (for example a local Kiro provider), which
is the only way to exercise a genuine `code_mode_only` model end to end:

```sh
export KIRO_PROVIDER_API_KEY=...   # bearer token for the provider
python3 scripts/zuno_standalone_live_check.py \
  --binary ./zuno-standalone-x86_64-unknown-linux-musl \
  --base-url http://127.0.0.1:8787/v1 --model gpt-5.6-sol-low
```

It asks the model to run `ls -la` (the approval must arrive, the command must run
after acceptance) and then to create a file (the approval must arrive, declining
must leave no file). Inside a container add `--network host` so the provider on
the host stays reachable.
