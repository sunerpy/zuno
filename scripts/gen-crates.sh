#!/usr/bin/env bash
# One-shot generator for the original crate skeletons. Kept in the tree as the
# record of how the roster was materialised; it is idempotent and safe to re-run.
#
# It is NOT the roster's source of truth: `crates.expected` is, and
# `crates/zuno-cli/tests/release_surface.rs::the_workspace_roster_matches_the_declared_crate_list`
# asserts the workspace matches it. The two crates todo 90 added (`zuno-process`,
# `zuno-reaping-fixture`) are deliberately absent below, because this script records
# what todo 1 generated, not what the workspace now contains.
set -euo pipefail

cd "$(dirname "$0")/.."

# name<TAB>one-line purpose
ROSTER=$(
  cat <<'EOF'
zuno-error	Typed error taxonomy shared by every crate; recovery decisions read data, never rendered messages.
zuno-types	Wire and domain types shared across the workspace (sessions, messages, parts, tool payloads).
zuno-paths	Filesystem layout resolution: project root, data dir, cache dir, and per-worktree state paths.
zuno-observability	Tracing subscriber setup, log file rotation, and structured span conventions.
zuno-config	Zuno configuration schema, discovery, merge order, and variable substitution.
zuno-permission	Permission engine: pattern matching over tool calls and the ask/allow/deny decision.
zuno-catalog	Catalog of agents, skills, commands, and references discovered from disk and config.
zuno-db	Zuno-native SQLite sessions, events, inboxes, jobs, and migrations.
zuno-snapshot	Workspace snapshots and diffs used for revert and for tool-edit provenance.
zuno-auth	Credential storage and refresh for provider authentication (API keys and OAuth).
zuno-llm	Provider-agnostic model interface, provider registry, streaming event vocabulary.
zuno-provider-anthropic	Anthropic Messages wire protocol: streaming, tool use, reasoning, and cache control.
zuno-provider-openai	OpenAI Chat Completions and Responses wire protocols.
zuno-provider-compatible	OpenAI-compatible endpoints reached through a configurable base URL.
zuno-provider-bedrock	Amazon Bedrock: SigV4 signing and the binary EventStream framing.
zuno-provider-google	Google Gemini and Vertex AI, including the Vertex-hosted Anthropic publisher.
zuno-tool	The tool trait, argument schemas, and the result shape every tool returns.
zuno-tools	Built-in tool implementations: file, shell, search, web, and task tools.
zuno-search	Content and path search over a project, honouring ignore semantics.
zuno-mcp	Model Context Protocol client: stdio and remote transports, tools, resources, prompts.
zuno-lsp	Language server client pool used for diagnostics and symbol lookup.
zuno-pty	Pseudo-terminal management for interactive shell sessions with OS-level child containment.
zuno-watch	Filesystem watcher publishing coalesced, bounded change events.
zuno-engine	The turn engine: the agent loop, tool dispatch, compaction, retry, and cancellation.
zuno-agent	Agent definitions, presets, and the sub-agent task boundary.
zuno-goal	Goal store and continuation board that survive across sessions.
zuno-memory	Character-capped resident memory: §-delimited entries, batch-atomic apply, injection scanning.
zuno-runtime	Transactional component scopes, typed services, and profile lifecycle.
zuno-harness	Native harness profiles combining drivers, capabilities, and tool manifests.
zuno-server	HTTP server exposing the `/api` surface and the server-sent event stream.
zuno-tui	Terminal user interface: views, keybindings, themes, and the render loop.
zuno-acp	Agent Client Protocol adapter for external editor clients.
zuno-cli	Command-line entry point and subcommand dispatch.
zuno-testkit	Shared test fixtures, cassette replay, and deterministic integration helpers.
EOF
)

while IFS=$'\t' read -r name purpose; do
  [ -n "$name" ] || continue
  mkdir -p "crates/$name/src"

  # Never clobber a crate that real work has already landed in.
  if [ -f "crates/$name/Cargo.toml" ]; then
    echo "skip $name (already exists)"
    continue
  fi

  if [ "$name" = "zuno-cli" ]; then
    cat >"crates/$name/Cargo.toml" <<EOF
[package]
name = "$name"
description = "$purpose"
version.workspace = true
edition.workspace = true
rust-version.workspace = true
license.workspace = true
repository.workspace = true
authors.workspace = true
publish.workspace = true

[[bin]]
name = "zuno"
path = "src/main.rs"

[dependencies]

[lints]
workspace = true
EOF
    cat >"crates/$name/src/main.rs" <<EOF
//! $purpose

fn main() {}
EOF
  else
    cat >"crates/$name/Cargo.toml" <<EOF
[package]
name = "$name"
description = "$purpose"
version.workspace = true
edition.workspace = true
rust-version.workspace = true
license.workspace = true
repository.workspace = true
authors.workspace = true
publish.workspace = true

[dependencies]

[lints]
workspace = true
EOF
    cat >"crates/$name/src/lib.rs" <<EOF
//! $purpose
EOF
  fi
done <<<"$ROSTER"

count=$(find crates -mindepth 1 -maxdepth 1 -type d | wc -l)
echo "generated $count crate skeletons"
