# Codex and Claude Code product agents

Status: 2026-08-22.

Zuno can expose host-installed Codex and Claude Code as bounded subagent tools. This is a native Rust adaptation of the product-subagent design reviewed in DeepSeek Harness, not a compatibility layer for its TypeScript API.

## Capability ownership

The capability is split across three lifecycles:

1. `zuno-product-agent` owns the native process protocols and returns only a final answer or a typed failure.
2. `zuno-tools::product_agent` owns the static model-visible tool contract and normal Zuno permission request.
3. The CLI composition root owns configured instances, durable background jobs, parent reports, cancellation, and restart reconciliation.

Each enabled instance registers one immutable tool name. Instances do not appear in the native agent catalog and do not create fake child sessions.

## Configuration

Product agents are disabled unless an instance sets `enabled: true`:

```json
{
  "productAgent": {
    "codex": {
      "kind": "codex",
      "enabled": true,
      "command": "codex",
      "toolName": "subagent_codex",
      "permissionMode": "never"
    },
    "claude-code": {
      "kind": "claude-code",
      "enabled": true,
      "command": "claude",
      "toolName": "subagent_claude_code",
      "permissionMode": "dontAsk"
    }
  }
}
```

`toolName` must be unique across enabled instances and cannot replace a native Zuno tool. Multiple instances of the same product are supported when they use distinct names. `command` may be an executable name or path.

Every product tool accepts the same arguments:

| argument | meaning |
| --- | --- |
| `prompt` | required bounded task |
| `description` | optional client-facing label |
| `background` | return a durable job immediately |
| `reportDelivery` | `nextStep` by default or `quiet`; valid only in background mode |

The tool requests the normal `task` permission with the pattern `product:<instance>`. External product calls use `ToolReplayPolicy::Never`.

## Native authentication and model ownership

Zuno does not authenticate these products. It does not read or copy their tokens into `auth.json`, does not select their model, and does not translate a Zuno provider credential into a product login.

- Codex is launched through its app-server stdio JSON-RPC protocol. Zuno initializes the server, creates an ephemeral thread, starts one turn, answers unattended approval requests according to the configured native permission mode, and retains only the final agent message. Current app-server enum spellings are attempted first; an `Invalid params` response before thread creation permits one legacy-spelling retry, while every other protocol failure remains terminal.
- Claude Code is launched in one-shot `--print --output-format stream-json --no-session-persistence` mode. Interactive user questions are disabled and the configured native `--permission-mode` is passed through.

Both commands inherit their own installation, native configuration and login state, the Zuno session working directory, and the process environment. Standard `HTTP_PROXY`, `HTTPS_PROXY`, `ALL_PROXY`, and `NO_PROXY` variables therefore reach the child product. An instance `env` map overlays inherited variables without mutating the parent process.

## Permission modes

Codex accepts `never`, `unlessTrusted`, and `onRequest`. `dangerouslyBypassApprovals` additionally selects a full-access sandbox and must be written explicitly.

Claude Code accepts `manual`, `auto`, `acceptEdits`, `dontAsk`, and `plan`. `bypassPermissions` is the explicit dangerous bypass. Zuno disables `AskUserQuestion` in every mode and also disables `ExitPlanMode` in plan mode so an unattended invocation cannot wait for interactive input or silently leave the requested policy.

The safe defaults are `never` for Codex and `dontAsk` for Claude Code. A permission value from one product is rejected for the other during configuration validation.

## Jobs, cancellation, and recovery

A foreground invocation has a one-shot `run_*` id and waits for the final answer. A background invocation additionally creates a durable `job_*` row whose `JobSubject` is `ProductAgent { run_id, product, instance, tool }`.

Product jobs can be `running`, `completed`, `failed`, `cancelled`, or `uncertain`. `nextStep` commits the terminal job and parent inbox report atomically before waking the parent; `quiet` stores the result without parent input.

`job_cancel` is itself non-replayable. It verifies that the current session owns a running job, asks the live supervisor to cancel it, and leaves settlement to the executor after the process tree has actually stopped. The TUI exposes the same action with `x` twice.

Codex protocol loss and Claude Code stream loss after execution may have started are `uncertain`, not retryable failures. A process restart converts any still-running product job to `uncertain`, admits its promised report, and never launches the product again. Authoritative external state must be inspected before a user chooses another invocation.

## Client projection

`ToolUiIntent::Subagent` is persisted with tool events, so clients recognize native and product subagents without hard-coding tool names. The TUI subagent view shows product or native target, objective, state, elapsed time, run/session id, job id, report delivery, result, and a safety diagnostic.

Enter toggles details. Pressing `x` twice cancels a running job and keeps the list mounted so several jobs can be cancelled in succession. Internal Codex or Claude Code reasoning and tool streams are deliberately not projected as Zuno events.
