# Developer and CI entry points.
#
# `make ci` runs the host-side source gates that CI invokes. `make pre-ci` adds
# the packaged-host smoke and a Linux-to-Windows cross Clippy/link pass so
# avoidable platform failures are found before the native GitHub-hosted jobs.
#
# OFFLINE BY DEFAULT. This repository's gates run with `--offline` throughout
# because the registry here is a mirror that cannot
# always be reached. Every target below therefore passes `$(OFFLINE)`, which is
# `--offline` unless you set `OFFLINE=` to allow a fetch:
#
#     make test              # offline
#     make test OFFLINE=     # allowed to fetch
#
# `--locked` is separate and always on for the metadata gate: a lock file that
# cannot be reproduced makes CI fail in a way that looks like a code problem.

.PHONY: all help \
		fmt fmt-check fmt-rust fmt-rust-check fmt-oxfmt fmt-oxfmt-check \
		lint lint-windows-cross check test test-nextest test-zed-acp-schema test-par test-fast \
		hook-fmt hook-test hooks ci pre-ci \
		deny metadata \
        build release release-target package smoke smoke-artifact \
        clean

CARGO       := cargo
OXFMT      ?= oxfmt
CLI_CRATE   := zuno
BINARY_NAME := zuno
TARGET_DIR  := target
DIST_DIR    := dist
HOST_SUFFIX := $(if $(filter Windows_NT,$(OS)),.exe,)
HOST_BINARY := $(BINARY_NAME)$(HOST_SUFFIX)
CARGO_OUTPUT_DIR := $(if $(CARGO_TARGET_DIR),$(CARGO_TARGET_DIR),$(TARGET_DIR))
DIST_BINARY := $(DIST_DIR)/$(HOST_BINARY)

# Set `OFFLINE=` on the command line to permit network access.
OFFLINE ?= --offline

OXFMT_FILES := \
  .oxfmtrc.json \
  .pre-commit-config.yaml \
  README.md \
  CONTRIBUTING.md \
  SECURITY.md \
  CODE_OF_CONDUCT.md

# Cross-compilation target for `release-target` / `package`, e.g.
#   make package TARGET=x86_64-unknown-linux-musl
TARGET ?=

# The targets the release pipeline ships. Listed here so `make help` can show
# them next to the command that builds one; the authoritative matrix lives in
# `.github/workflows/release-candidate.yml` and
# `crates/zuno-cli/tests/release_surface.rs` asserts the two agree.
RELEASE_TARGETS := \
  x86_64-unknown-linux-musl \
  aarch64-unknown-linux-musl \
  x86_64-apple-darwin \
  aarch64-apple-darwin \
  x86_64-pc-windows-msvc \
  aarch64-pc-windows-msvc

all: build

# ─── Gates ──────────────────────────────────────────────────────────────────

fmt: fmt-rust fmt-oxfmt

fmt-rust:
	$(CARGO) fmt --all

fmt-check: fmt-rust-check fmt-oxfmt-check

fmt-rust-check:
	$(CARGO) fmt --all --check

fmt-oxfmt:
	@command -v $(OXFMT) > /dev/null 2>&1 \
	  || { echo "oxfmt is required; install it from https://oxc.rs/docs/guide/usage/formatter.html"; exit 1; }
	$(OXFMT) --ignore-path .oxfmtignore $(OXFMT_FILES)

fmt-oxfmt-check:
	@command -v $(OXFMT) > /dev/null 2>&1 \
	  || { echo "oxfmt is required; install it from https://oxc.rs/docs/guide/usage/formatter.html"; exit 1; }
	$(OXFMT) --check --ignore-path .oxfmtignore $(OXFMT_FILES)

# `--all-targets` so tests and examples are linted too; `-D warnings` because the
# workspace lint table sets `clippy::all = "warn"` and a warning nobody fails on
# is a warning nobody fixes.
lint:
	$(CARGO) clippy --workspace --all-targets $(OFFLINE) -- -D warnings

lint-windows-cross:
	ZUNO_CARGO_OFFLINE=$(if $(strip $(OFFLINE)),1,0) \
		.github/scripts/lint-windows-cross.sh

check:
	$(CARGO) check --workspace --all-targets $(OFFLINE)

test:
	$(CARGO) test --workspace --no-fail-fast $(OFFLINE)
	$(MAKE) test-zed-acp-schema

# The cross-platform CI runner. nextest compiles the workspace once and schedules
# test binaries concurrently; `.config/nextest.toml` reserves the complete worker
# pool for the startup wall-clock suite. Cargo still owns doctests because nextest
# does not execute them.
test-nextest:
	@$(CARGO) nextest --version > /dev/null 2>&1 \
	  || { echo "cargo-nextest is required; install cargo-nextest 0.9.103"; exit 1; }
	$(CARGO) nextest run --workspace --no-fail-fast --no-tests=warn $(OFFLINE)
	$(CARGO) test --workspace --doc --no-fail-fast $(OFFLINE)
	$(MAKE) test-zed-acp-schema

# The exact schema crate consumed by current Zed enables serde_json/preserve_order.
# Keep that feature out of the default workspace graph so it cannot change
# production type layouts or unrelated Clippy results; exercise it in one
# isolated, explicitly gated contract build instead.
test-zed-acp-schema:
	$(CARGO) test -p zuno-acp --features zed-schema-contract --test contract $(OFFLINE)

# Same non-ignored test surface as `test`, run concurrently. Prefer the maintained
# cross-platform runner used by CI; retain the measured in-repository scheduler
# as an offline fallback for developer machines without cargo-nextest.
#
# Historical baseline: `cargo test` built in parallel but ran 224 suites one at
# a time, taking 219.9s with a warm target; the in-tree scheduler reached the
# same 4280 passed / 0 failed / 8 ignored in 53.2s median. The current pinned
# nextest validation is recorded separately in docs/perf-methodology.md.
test-par:
	@if $(CARGO) nextest --version > /dev/null 2>&1; then \
		$(MAKE) test-nextest; \
	else \
		echo "cargo-nextest unavailable; using scripts/test-parallel.sh"; \
		./scripts/test-parallel.sh; \
	fi

test-fast:
	$(CARGO) test -p $(CLI_CRATE) --test docs --test release_surface $(OFFLINE)
	sh -n scripts/install.sh

hook-fmt: fmt

hook-test: test-fast

hooks:
	@command -v pre-commit > /dev/null 2>&1 \
	  || { echo "pre-commit is required; install it from https://pre-commit.com"; exit 1; }
	pre-commit install --hook-type pre-commit --hook-type pre-push

# Run FIRST in `ci`: a lock file that cannot be reproduced makes every later
# failure look like a code problem, and a plain `cargo build` silently repairs it
# instead of reporting it.
metadata:
	$(CARGO) metadata --locked $(OFFLINE) --format-version 1 > /dev/null

# The supply-chain gate. Offline here so `make ci` works without network; CI runs
# it online so the RUSTSEC advisory database is current. `cargo-deny` is optional
# for a build, so its absence is a named skip rather than a hard failure — but the
# CI job that runs it online is not skippable.
deny:
	@if command -v cargo-deny > /dev/null 2>&1; then \
		$(CARGO) deny $(OFFLINE) check; \
	else \
		echo "SKIP  cargo-deny is not installed; install with: cargo install cargo-deny --locked"; \
		echo "      the CI 'Supply chain' job runs it regardless, so this is a local convenience only"; \
	fi

# Host-side source gates, in the order that makes a failure easiest to read.
ci: metadata fmt-check lint test-par deny
	@echo "OK    metadata + fmt + clippy + parallel tests + cargo-deny"

# Local predictive gate before opening or updating a pull request. Native
# Windows/MSVC execution and hosted-runner networking remain CI-only evidence.
pre-ci: ci check smoke-artifact lint-windows-cross
	@echo "OK    host CI + check + packaged smoke + Windows cfg/Clippy/link preflight"

# ─── Build ──────────────────────────────────────────────────────────────────

build:
	$(CARGO) build -p $(CLI_CRATE) --bin $(BINARY_NAME) $(OFFLINE)
	@mkdir -p "$(DIST_DIR)"
	@rm -f "$(DIST_BINARY).tmp"
	@cp "$(CARGO_OUTPUT_DIR)/debug/$(HOST_BINARY)" "$(DIST_BINARY).tmp"
	@mv -f "$(DIST_BINARY).tmp" "$(DIST_BINARY)"
	@ls -l "$(DIST_BINARY)"

release:
	$(CARGO) build --release -p $(CLI_CRATE) --bin $(BINARY_NAME) $(OFFLINE)
	@mkdir -p "$(DIST_DIR)"
	@rm -f "$(DIST_BINARY).tmp"
	@cp "$(CARGO_OUTPUT_DIR)/release/$(HOST_BINARY)" "$(DIST_BINARY).tmp"
	@mv -f "$(DIST_BINARY).tmp" "$(DIST_BINARY)"
	@ls -l "$(DIST_BINARY)"

# Cross-compile one target. The two musl targets go through `cargo zigbuild`,
# which is what makes them need no per-target C cross-toolchain: this workspace
# compiles C (bundled SQLite, aws-lc-sys) and Zig supplies a hermetic C
# cross-compiler as a single download. The other four are native-only — building
# an Apple or MSVC target from Linux is out of scope, and the release workflow
# uses runners of the right architecture instead.
release-target:
ifndef TARGET
	$(error TARGET is not set. Try one of: $(RELEASE_TARGETS))
endif
	@case "$(TARGET)" in \
	  *-linux-musl) \
	    command -v cargo-zigbuild > /dev/null 2>&1 \
	      || { echo "cargo-zigbuild is required for $(TARGET); install with: cargo install cargo-zigbuild --locked"; exit 1; }; \
	    command -v zig > /dev/null 2>&1 \
	      || { echo "zig is required for $(TARGET); see https://ziglang.org/download/"; exit 1; }; \
	    $(CARGO) zigbuild --release -p $(CLI_CRATE) --bin $(BINARY_NAME) --target $(TARGET) $(OFFLINE) ;; \
	  *) \
	    $(CARGO) build --release -p $(CLI_CRATE) --bin $(BINARY_NAME) --target $(TARGET) $(OFFLINE) ;; \
	esac
	@ls -l $(TARGET_DIR)/$(TARGET)/release/$(BINARY_NAME)*

# Produce the release archive for one target, named exactly as the release
# workflow names it.
package: release-target
	@mkdir -p $(DIST_DIR)
	@version=$$($(CARGO) metadata --locked $(OFFLINE) --no-deps --format-version 1 \
	  | sed -n 's/.*"name":"$(CLI_CRATE)","version":"\([^"]*\)".*/\1/p'); \
	  case "$(TARGET)" in \
	    *windows*) \
	      ( cd $(TARGET_DIR)/$(TARGET)/release && zip -q - $(BINARY_NAME).exe ) \
	        > "$(DIST_DIR)/$(BINARY_NAME)-$$version-$(TARGET).zip" ;; \
	    *) \
	      tar -czf "$(DIST_DIR)/$(BINARY_NAME)-$$version-$(TARGET).tar.gz" \
	        -C $(TARGET_DIR)/$(TARGET)/release $(BINARY_NAME) ;; \
	  esac
	@ls -l $(DIST_DIR)

# ─── Smoke ──────────────────────────────────────────────────────────────────

# Exercise an already-built binary. `BINARY=` overrides the subject, which is how
# the release workflow points this at an unpacked archive.
BINARY ?= $(TARGET_DIR)/release/$(BINARY_NAME)

smoke:
	$(CARGO) build --release -p zuno-testkit --bin zuno-smoke $(OFFLINE)
	./$(TARGET_DIR)/release/zuno-smoke --binary "$(BINARY)"

# The release pipeline's packaging + smoke path for the host, end to end: build a
# real release binary, archive it, unpack the archive, and smoke what came out.
# Unpacking is the point — it proves the archive contains a runnable binary, not
# just that the compiler produced one. This is what CI's `artifact` job runs.
smoke-artifact: release
	$(CARGO) build --release -p zuno-testkit --bin zuno-smoke $(OFFLINE)
	@rm -rf $(DIST_DIR)/host $(DIST_DIR)/unpacked
	@mkdir -p $(DIST_DIR)/host $(DIST_DIR)/unpacked
	tar -czf $(DIST_DIR)/host/$(BINARY_NAME).tar.gz -C $(TARGET_DIR)/release $(BINARY_NAME)
	tar -xzf $(DIST_DIR)/host/$(BINARY_NAME).tar.gz -C $(DIST_DIR)/unpacked
	./$(TARGET_DIR)/release/zuno-smoke --binary $(DIST_DIR)/unpacked/$(BINARY_NAME)

clean:
	$(CARGO) clean
	@rm -rf $(DIST_DIR)

help:
	@echo "Gates:"
	@echo "  ci              host metadata + fmt-check + lint + parallel tests + deny"
	@echo "  pre-ci          ci + check + packaged smoke + Windows cross-Clippy/link"
	@echo "  fmt             cargo fmt --all"
	@echo "                  + oxfmt YAML/JSON/Markdown"
	@echo "  fmt-check       verify Rust and oxfmt formatting"
	@echo "  lint            cargo clippy --workspace --all-targets -D warnings"
	@echo "  lint-windows-cross  Windows cfg/Clippy and test-link preflight with Zig"
	@echo "  check           cargo check --workspace --all-targets"
	@echo "  test            cargo test --workspace --no-fail-fast"
	@echo "  test-nextest    workspace tests concurrent across binaries + doctests"
	@echo "  test-zed-acp-schema  decode ACP updates with current Zed's pinned schema crate"
	@echo "  test-par        nextest when installed; measured in-tree fallback otherwise"
	@echo "  test-fast       focused docs/release tests + installer syntax"
	@echo "  hook-fmt        commit-time formatting gate"
	@echo "  hook-test       push-time fast test gate"
	@echo "  hooks           install pre-commit and pre-push hooks"
	@echo "  metadata        cargo metadata --locked (lock reproducibility)"
	@echo "  deny            cargo deny check (skipped with a notice if absent)"
	@echo ""
	@echo "Build:"
	@echo "  build           debug $(BINARY_NAME) -> $(DIST_BINARY)"
	@echo "  release         optimized $(BINARY_NAME) -> $(DIST_BINARY)"
	@echo "  release-target  cross-build one target; TARGET=<triple> required"
	@echo "  package         release-target + the release archive into $(DIST_DIR)/"
	@echo ""
	@echo "Smoke:"
	@echo "  smoke           run zuno-smoke against BINARY (default: the release build)"
	@echo "  smoke-artifact  release + archive + unpack + smoke (what CI runs)"
	@echo ""
	@echo "Release targets:"
	@for t in $(RELEASE_TARGETS); do echo "  $$t"; done
	@echo ""
	@echo "Every cargo invocation passes $(OFFLINE); set OFFLINE= to allow fetching."
