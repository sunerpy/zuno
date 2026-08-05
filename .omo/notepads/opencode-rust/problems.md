# problems — opencode-rust

## Task 1 — what actually went wrong, and the fix

1. **16 cargo warnings the moment the profile pins landed.** The todo asks for
   `opt-level = 3` on `ratatui` / `crossterm` / `unicode-*` in dev and test, but
   no crate depended on any of them yet, so cargo emitted
   `warning: profile package spec 'ratatui' in profile 'dev' did not match any
   packages` — eight specs x two profiles. Against a zero-warning bar that is a
   hard failure, not noise. Fixed by giving `oc-tui` its real `ratatui` +
   `crossterm` dependencies now; that pulls the six transitive packages and
   satisfies every spec. Those two deps look unused and are not: deleting them
   reintroduces all 16 warnings. A comment in `crates/oc-tui/Cargo.toml` says so.

2. **`cargo add reqwest --features rustls-tls` fails outright** on reqwest 0.13:
   "unrecognized feature for crate reqwest: rustls-tls". Every 0.12-era snippet
   uses that name. The 0.13 spelling is `rustls`. Cost one round-trip; recorded
   in learnings.md so it costs nobody else one.

3. **`cargo add <crate>@latest` is not a thing.** It fails with
   "invalid version requirement `latest`". Bare `cargo add <crate>` already
   resolves to the newest compatible version.

4. **zsh's `$PIPESTATUS` is 1-indexed**, so `${PIPESTATUS[0]}` after a
   `cmd | tee` pipeline reads empty and an early exit-code check silently
   reported nothing. The evidence capture script runs under `bash` explicitly to
   avoid this; do not assume `$?` after a `| tee` is the compiler's status.

5. **Non-blocking, left as-is:** `.omo/plans/opencode-rust.md` and
   `.omo/drafts/opencode-rust.md` do contain the strings "25 crates" and
   "31 crates". Task 1 forbids *writing* those counts, and both occurrences are
   the plan's own record of why those earlier counts were wrong. Both files are
   also explicitly off-limits to edit. No action taken; flagged so a later
   grep-based check does not read them as drift.
