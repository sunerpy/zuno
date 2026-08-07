# Smoke-test cassettes

## What is here, and why it is committed

`openai-chat/drives-a-tool-loop-end-to-end.json` is a **verbatim byte-for-byte
copy** of the upstream `opencode` recording at

    packages/llm/test/fixtures/recordings/openai-chat/drives-a-tool-loop-end-to-end.json

recorded by upstream's `@opencode-ai/http-recorder` against the real OpenAI Chat
Completions API on 2026-05-06 (`metadata.recordedAt`). Upstream `opencode` is
MIT-licensed, as is this workspace.

SHA-256 of the copy, which is also the SHA-256 of the upstream file:

    fab3c2b9991544004e02a101c4bbe5843f887d2084d96353a684fba4f0e5acd4

## Why a copy rather than reading the oracle tree

`oc_testkit::cassette::recordings_root` locates the oracle's recordings by
walking up from the workspace looking for a sibling `opencode` checkout. That
works on a developer machine and it is how `crates/oc-cli/tests/tool_turn.rs`
gets this same cassette. **It cannot work on a CI runner**, which has this
repository and nothing else — and the artifact smoke test has to run there,
because "must not ship an artifact that was never executed" is the whole point of
the release pipeline.

So the one cassette the smoke test needs is committed here, and
`crates/oc-cli/tests/release_surface.rs::committed_smoke_cassette_matches_the_oracle_recording`
compares it against the oracle's copy byte for byte whenever an oracle tree is
reachable, printing a named skip when it is not. The copy therefore cannot drift
silently: on any machine that has both, a divergence is a test failure.

## Why this particular recording

Its recorded assistant turn calls `get_weather` — a tool this runtime
deliberately does not have. That makes it a **better** smoke fixture than a
matching one, for the reason `tool_turn.rs` documents: it proves the binary put
the assembled tool registry on the wire and that an unknown tool call still
produces a tool result the turn loop sends back, using nothing but recorded
provider bytes. Zero authored bytes, so `MockProvider::authored_scenarios()` is
empty and the smoke test asserts that.

## Adding another

Copy it verbatim, record its SHA-256 above, and add it to the drift test's list.
Do not hand-edit a cassette: an authored response proves nothing about a wire
format, and `oc-testkit` exists partly to keep that distinction visible.
