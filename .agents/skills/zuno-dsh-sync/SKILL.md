---
name: zuno-dsh-sync
description: Compare Zuno with new DeepSeek Harness commits and pre-releases, classify reusable design changes, port selected behavior with tests, and advance the recorded upstream baseline. Use for DSH refreshes, upstream design audits, or decisions about adopting a DSH fix or capability in Zuno.
---

# Zuno DSH Sync

Track DeepSeek Harness as a design upstream, not as a source-compatible dependency. Zuno is a native Rust harness and does not preserve OpenCode or DSH plugin ABIs.

## Start

1. Read the repository `AGENTS.md`, `docs/design/harness-comparison.md`, and [references/adoption-ledger.md](references/adoption-ledger.md).
2. Run `python3 .agents/skills/zuno-dsh-sync/scripts/dsh_delta.py` from the Zuno root. It fetches the configured DSH remote by default, compares the recorded commit with the cached remote head, and never checks out or edits the DSH worktree.
3. If fetch fails, treat the report as cache-derived and do not advance the baseline. Resolve the network failure or explicitly report that the review is stale.
4. Use CodeGraph in both repositories for every material area in the delta. Read release notes and merged PRs when they explain intent, but verify behavior in the exact source and tests at the compared commits.

## Decide

Classify each material change as one of:

- `adopt-now`: Zuno has the same user or lifecycle need and a native extension point can own it.
- `already-covered`: current Zuno code and an observable test prove the invariant.
- `adapt-later`: valuable, but a prerequisite capability or public interface is missing.
- `reject`: tied to DSH's TypeScript, Cordis, browser, or deployment choices, or conflicts with Zuno's product direction.
- `watch`: incomplete, reverted, or too unstable to turn into a Zuno obligation.

Record the decision and evidence in the adoption ledger. Compare release tags as ranges so a change later reverted in the same pre-release line is not copied accidentally.

## Port

- Port the invariant and user outcome, not DSH file structure or implementation syntax.
- Add behavior through `Component`, `ProfileBundle`, `HarnessProfile`, `AgentDriver`, tool contributions, or another documented capability service. A central loop change requires an explicit reason and an update to the architecture comparison.
- Treat a capability as provider, service interface, and consumer. Do not add a model-visible tool without the provider lifecycle and assembled application path that make it usable.
- Anything model-visible must be reconstructable from Zuno's durable event history. Prompt sections and tool schemas need provenance that tests can inspect.
- Start with a failing test through the real command, profile, server, or TUI entry path. Unit tests may supplement but do not replace assembled behavior.
- Preserve side-effect safety: retry provider requests and goal turns, but never mechanically replay an uncertain tool action.
- Update user documentation and extension examples with the implementation.

## Advance The Baseline

Advance [references/dsh-baseline.json](references/dsh-baseline.json) only after:

1. every material upstream change is classified in the ledger;
2. adopted changes pass their focused tests and relevant workspace gates;
3. deferred or rejected changes have a concrete reason and revisit trigger;
4. the recorded commit is an ancestor of the fetched tracking ref.

Update `reviewed_commit`, `reviewed_tag`, `reviewed_at`, and `reviewed_against_zuno`. Re-run the delta script; it must report no unreviewed commits.

## Report

Include the old baseline, fetched head and tag, fetch freshness, decisions by category, Zuno files changed, and exact commands run. Separate source-verified facts from recommendations.

For the DSH-to-Zuno capability map and review priorities, read [references/design-map.md](references/design-map.md).
