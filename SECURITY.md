# Security Policy

## Reporting a vulnerability

Report privately. Do not open a public issue, pull request, or discussion for a
suspected vulnerability.

Use GitHub's private vulnerability reporting:
[**Report a vulnerability**](https://github.com/sunerpy/zuno/security/advisories/new).
If that is unavailable to you, email <nkuzhangshn@gmail.com> with `zuno security` in
the subject.

Please include the affected version (`zuno --version --long`), the platform, a
minimal reproduction, and the impact you believe it has. Redact API keys, tokens,
and session contents before sending anything.

Expect an acknowledgement within seven days. Once a fix is ready it ships in a
release and the advisory is published with credit unless you ask otherwise.

## Supported versions

Zuno is pre-1.0. Fixes land on `main` and in the next release; older releases are
not patched in place.

| Version        | Supported |
| -------------- | --------- |
| Latest release | Yes       |
| Older releases | No        |

## What is in scope

Zuno is an agent that executes tools on the user's machine with the user's
credentials. That makes the following interesting, and all of it in scope:

- **Permission bypass.** Any path by which a tool call runs a side effect that the
  configured permission set should have denied or held for approval — including an
  extension manifest that claims read-only or safe-replay policy for a `host.full`,
  WASI `network`, or WASI `workspace.write` tool.
- **Containment escape.** A WASI component reaching outside its granted workspace,
  network, or environment; a contained process surviving its declared lifecycle; or
  fuel, memory, and wall-time bounds failing to apply.
- **Prompt injection with real consequence.** Repository content, tool output, or a
  fetched page steering the agent into an unauthorized side effect, or into writing
  a durable memory entry that persists that instruction.
- **Credential disclosure.** Provider keys or session tokens reaching logs, durable
  session events, snapshots, exported sessions, subagent reports, or a model
  request that should not have carried them.
- **Update integrity.** Any way `zuno self-update` or `scripts/install.sh` /
  `scripts/install.ps1` can be made to install bytes that do not match the
  `SHA256SUMS` published with the same release.
- **Durable state corruption** that survives a restart, including a crafted session
  database or extension manifest that causes unbounded resource use on load.

## What is not in scope

- A tool doing exactly what an explicitly granted permission allows. Granting
  `bash: allow` permits arbitrary commands by design; that is a configuration
  choice, not a vulnerability.
- Behaviour that requires an attacker who already has local code execution as the
  same user, or write access to the user's Zuno configuration.
- Model output quality, hallucination, or cost.
- Vulnerabilities in a third-party model provider's service. Report those to the
  provider.
- Findings from an automated scanner with no demonstrated impact on this codebase.

## Hardening this project already relies on

Useful context when assessing a report:

- `unsafe_code` is `forbid` workspace-wide and asserted by the test suite.
- TLS is rustls only. `crates/zuno-cli/tests/release_surface.rs` asserts the shipped
  binary's dependency graph contains no OpenSSL or `native-tls`.
- No JavaScript ABI and no Rust dynamic-library plugin loader; both absences are
  asserted rather than remembered.
- Tool execution is at-most-once by default. A timeout or lost response around a
  side effect is recorded as uncertain and is never mechanically replayed.
- CI runs `cargo deny --all-features check` against the RUSTSEC advisory database on
  every push and pull request.
- Release archives are published only after the binary has been executed on a runner
  of its own architecture, and `SHA256SUMS` is generated from the same bytes the
  release attaches.
