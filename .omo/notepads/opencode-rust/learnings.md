# learnings — opencode-rust

## Task 1 — workspace scaffold

Resolved versions, all pinned once in the root `[workspace.dependencies]`. These
came from `cargo add` against a throwaway crate on 2026-08-05 with rustc 1.96.0,
then verified by an actual `cargo build --workspace`, not from memory. Registry
in use is the `aliyun` mirror; `cargo build` locked 197 packages.

| crate | version | notes |
| --- | --- | --- |
| async-trait | 0.1.91 | |
| axum | 0.8.9 | default features; `http2`, `macros`, `ws`, `multipart` are OFF and each is a later todo's decision |
| base64 | 0.23.1 | |
| clap | 4.6.5 | `derive`, `env` |
| crossterm | 0.29.0 | must stay aligned with whatever `ratatui-crossterm` resolves to |
| dirs | 6.0.0 | |
| futures | 0.3.33 | |
| globset | 0.4.20 | |
| grep-regex | 0.1.14 | |
| grep-searcher | 0.1.17 | |
| ignore | 0.4.33 | |
| insta | 1.48.0 | `json`, `yaml`, `redactions` |
| notify | 8.2.0 | default features on purpose — see below |
| proptest | 1.11.0 | |
| ratatui | 0.30.2 | `serde`; 0.30 is split into `ratatui-core` / `-widgets` / `-crossterm` / `-macros` |
| reqwest | 0.13.4 | `default-features = false` + `json`, `stream`, `rustls`, `charset`, `http2` |
| schemars | 1.2.2 | `derive` + `std` are already default; `uuid1` / `url2` / `indexmap2` integrations are opt-in |
| serde | 1.0.229 | `derive` |
| serde_json | 1.0.151 | |
| sha2 | 0.11.0 | |
| tempfile | 3.27.0 | |
| thiserror | 2.0.19 | 2.x, not 1.x |
| time | 0.3.55 | `formatting`, `parsing`, `macros`, `local-offset` |
| tokio | 1.53.1 | `full` |
| tokio-util | 0.7.19 | `io`, `codec` |
| toml | 1.1.4 | 1.x — the 0.8/0.9 API notes found in older references do not apply |
| tower | 0.5.3 | `util`, `timeout` |
| tower-http | 0.7.0 | `fs`, `trace`, `cors` |
| tracing | 0.1.44 | |
| tracing-appender | 0.2.5 | |
| tracing-subscriber | 0.3.23 | `env-filter`, `fmt` |
| url | 2.5.8 | |
| uuid | 1.24.0 | `v4`, `v7`, `serde` |
| walkdir | 2.5.0 | |
| which | 8.0.5 | |
| wiremock | 0.6.5 | |

Non-obvious settings, each of which cost a build cycle to find:

1. **`reqwest` 0.13 renamed the TLS feature.** The `rustls-tls` name that every
   0.12-era snippet uses does not exist; `cargo add` rejects it outright with
   "unrecognized feature for crate reqwest: rustls-tls". The 0.13 spelling is
   plain `rustls`. Also note `charset` and `http2` are NOT in reqwest's default
   set once `default-features = false` is set, so both are listed explicitly.
2. **`unsafe_code = "forbid"` only applies to crates that opt in.** A crate
   without `[lints] workspace = true` compiles unsafe code silently. All 33
   have the key; a new crate without it is a review defect, not a style nit.
3. **A `[profile.*.package.X]` pin for a package nothing depends on is a cargo
   warning**, and 16 of them appeared the moment the profile pins landed:
   `warning: profile package spec 'ratatui' in profile 'dev' did not match any
   packages`, once per package per profile. Because the acceptance bar is zero
   warnings, `oc-tui` declares `ratatui` + `crossterm` now — that pulls
   `ratatui-core`, `ratatui-widgets`, `ratatui-crossterm`, `unicode-width`,
   `unicode-segmentation`, and `unicode-truncate` transitively and satisfies all
   eight specs in both profiles. Do not "clean up" those two unused deps.
4. **`notify` keeps its default features.** `macos_fsevent` is a default; adding
   it explicitly alongside `default-features = false` silently drops the Linux
   inotify backend. Todo 50 owns any real backend selection.
5. **`resolver = "3"`** is what makes `rust-version` participate in resolution —
   that is why `cargo build` reports "Locking 197 packages to latest Rust 1.96
   compatible versions" rather than pulling something newer that then fails.
6. Debug profile uses `debug = "line-tables-only"`: panics and backtraces keep
   file and line, but links far faster than full debuginfo. jcode goes further
   with `debug = 0`, which loses that.
