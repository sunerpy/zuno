#!/usr/bin/env bash
# One-shot generator for the 33 crate skeletons. Kept in the tree as the record
# of how the roster was materialised; it is idempotent and safe to re-run.
set -euo pipefail

cd "$(dirname "$0")/.."

# name<TAB>one-line purpose
ROSTER=$(
  cat <<'EOF'
oc-error	Typed error taxonomy shared by every crate; recovery decisions read data, never rendered messages.
oc-types	Wire and domain types shared across the workspace (sessions, messages, parts, tool payloads).
oc-paths	Filesystem layout resolution: project root, data dir, cache dir, and per-worktree state paths.
oc-observability	Tracing subscriber setup, log file rotation, and structured span conventions.
oc-config	Configuration schema, discovery, merge order, variable substitution, and legacy rejection.
oc-permission	Permission engine: pattern matching over tool calls and the ask/allow/deny decision.
oc-catalog	Catalog of agents, skills, commands, and references discovered from disk and config.
oc-db	SQLite storage layer with schema parity against the TypeScript `opencode.db`.
oc-snapshot	Workspace snapshots and diffs used for revert and for tool-edit provenance.
oc-auth	Credential storage and refresh for provider authentication (API keys and OAuth).
oc-llm	Provider-agnostic model interface, provider registry, streaming event vocabulary.
oc-provider-anthropic	Anthropic Messages wire protocol: streaming, tool use, reasoning, and cache control.
oc-provider-openai	OpenAI Chat Completions and Responses wire protocols.
oc-provider-compatible	OpenAI-compatible endpoints reached through a configurable base URL.
oc-provider-bedrock	Amazon Bedrock: SigV4 signing and the binary EventStream framing.
oc-provider-google	Google Gemini and Vertex AI, including the Vertex-hosted Anthropic publisher.
oc-tool	The tool trait, argument schemas, and the result shape every tool returns.
oc-tools	Built-in tool implementations: file, shell, search, web, and task tools.
oc-search	Content and path search over a project, honouring ignore semantics.
oc-mcp	Model Context Protocol client: stdio and remote transports, tools, resources, prompts.
oc-lsp	Language server client pool used for diagnostics and symbol lookup.
oc-pty	Pseudo-terminal management for interactive shell sessions with OS-level child containment.
oc-watch	Filesystem watcher publishing coalesced, bounded change events.
oc-engine	The turn engine: the agent loop, tool dispatch, compaction, retry, and cancellation.
oc-agent	Agent definitions, presets, and the sub-agent task boundary.
oc-goal	Goal store and continuation board that survive across sessions.
oc-plugin	Plugin host: the hook bus and the plugin lifecycle.
oc-plugin-sdk	The surface a plugin is written against, shared by hosts and plugin authors.
oc-server	HTTP server exposing the `/api` surface and the server-sent event stream.
oc-tui	Terminal user interface: views, keybindings, themes, and the render loop.
oc-acp	Agent Client Protocol adapter for external editor clients.
oc-cli	Command-line entry point and subcommand dispatch.
oc-testkit	Shared test fixtures, cassette replay, and differential-oracle helpers.
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

  if [ "$name" = "oc-cli" ]; then
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
name = "opencode-rust"
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
