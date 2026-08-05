# issues — opencode-rust

## Task 1 — unresolved tension: SQLite needs a C toolchain

**For todo 19 (SQLite parity) and todo 91 (packaging) to resolve together, not
separately.** Recording it here so neither discovers it late.

Todo 91 requires: "no OpenSSL and no C-toolchain-dependent dependency in the
default feature set … Must NOT require a C compiler for a default build."

Todo 19 requires byte-level schema parity with the TypeScript `opencode.db`,
which is real SQLite. Every production-grade SQLite binding for Rust compiles C:

- `rusqlite` → `libsqlite3-sys` → bundles and compiles the SQLite amalgamation.
- `sqlx` with `sqlite` → same `libsqlite3-sys`.
- `libsql` / `limbo` (`turso`) → pure Rust, but not production-grade for a
  file-format-compatible store today.
- `rusqlite` with `--features sqlcipher` or a system `libsqlite3` only moves the
  C dependency from build time to link time; it does not remove it.

So the two constraints as written cannot both hold. Todo 1 pins **no** SQLite
crate — that choice belongs to todo 19 — but the decision must be made
knowingly. The options, none of them free:

1. Accept `cc` as a build requirement and drop "no C toolchain" from todo 91,
   keeping only "no OpenSSL". Cheapest, and `cc` is present on every platform
   the release matrix targets. Cost: `aarch64` and `musl` cross-builds need a
   working cross C toolchain, which is the actual reason the constraint was
   written.
2. Ship SQLite behind a non-default feature. Contradicts todo 19, since the DB
   is not optional for a drop-in replacement.
3. Use a pure-Rust engine and give up on reading an existing `opencode.db`.
   Contradicts the parity requirement outright.
4. Statically link a prebuilt SQLite per target in CI. Keeps a C artifact but
   removes the compiler from the build; adds real release-pipeline complexity.

**Same tension, already live in TLS.** `reqwest` 0.13's `rustls` feature selects
the `aws-lc-rs` crypto provider, whose `aws-lc-sys` crate builds C — verified in
the resolved graph. Todo 91's reference line says "rustls only, no OpenSSL",
which this satisfies; the "no C compiler" half it does not. If a C-free default
is genuinely required, the TLS fallback is `rustls` with the `ring` provider
(`rustls-no-provider` plus an explicit `rustls` dependency), and `ring` still
ships pre-generated assembly and needs `cc`. There is no production-grade C-free
TLS stack in Rust today. Todo 91 should reconcile the wording with reality
rather than have a build silently violate it.

Both `cc` and `gcc` are present on this machine (`/usr/bin/cc`, `/usr/bin/gcc`);
`cmake` is not, which matters because `aws-lc-sys` prefers cmake and falls back
to its own build script.
