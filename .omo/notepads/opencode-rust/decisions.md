# decisions — opencode-rust

## Task 1 — choices the plan left open

- **Edition `2024`, `resolver = "3"`.** The plan named neither. 2024 is the
  current edition on rustc 1.96 and resolver 3 is its default; picking anything
  older would be a deliberate downgrade with no stated reason. Consequence worth
  knowing: resolver 3 honours `rust-version` during resolution, so bumping
  `rust-version` can change the resolved graph.
- **Toolchain channel pinned to `1.96.0`, not `stable`.** `rust-toolchain.toml`
  names the exact version so a future `stable` release cannot turn a new clippy
  lint into a CI failure without someone editing that file. The toolchain is
  already installed on this machine (`rustup toolchain list` shows
  `1.96.0-x86_64-unknown-linux-gnu (active)`) with `clippy` and `rustfmt`, so
  the pin resolves offline. `profile` is deliberately omitted so rustup does not
  try to reinstall components that are already present.
- **Binary name `opencode-rust`** on the single `[[bin]]` in `oc-cli`. Keeps it
  distinguishable from the real `opencode` binary, which this project uses as a
  differential test oracle — two things named `opencode` on `PATH` would make
  every oracle comparison ambiguous. Todo 91 owns the published name.
- **`license = "MIT"`, `repository = "https://github.com/sunerpy/opencode-rust"`,
  `authors = ["sunerpy <nkuzhangshn@gmail.com>"]`, `publish = false`.** MIT
  matches the upstream TypeScript `opencode`. The author fields come from this
  machine's git config. `publish = false` is set workspace-wide because nothing
  here is a crates.io artifact today; Todo 91 flips it if that changes.
- **First-party crates are listed in `[workspace.dependencies]` as path entries.**
  Not strictly required by the todo, but todos 2-6 run in parallel immediately
  after this one and each needs to depend on siblings. With the paths already
  declared, a member crate writes `oc-error = { workspace = true }` and never
  touches the root manifest, which removes the concurrent-edit collision on
  `Cargo.toml` that five parallel tasks would otherwise cause.
- **`clippy all = "warn"` only.** claw-code additionally sets `pedantic = allow`
  with `priority = -1` and allows four specific lints; that shape is a response
  to lints this workspace has not hit yet. Starting narrow means the first crate
  that trips something makes a deliberate decision instead of inheriting an
  allow-list nobody remembers the reason for.
- **`scripts/gen-crates.sh` is kept in the tree.** It is the record of how the
  roster was materialised and is idempotent — it skips any crate whose
  `Cargo.toml` already exists, so it can never clobber landed work.
- **`crates.expected` holds bare names only, no header comment.** The gate is a
  byte-for-byte `cmp` against `cargo metadata | jq -r … | sort`, so any comment
  line would break it. The gate's documentation lives in
  `.omo/evidence/task-1-opencode-rust.txt` instead.
- **`.gitignore` also excludes `.omo/run-continuation/` and
  `.omo/delegated-runtime/`** — orchestrator scratch state that was already in
  the tree. `.omo/plans/` and `.omo/notepads/` are deliberately tracked.
