# F3 Real Manual QA Report

Date: 2026-08-08  
Worktree: `/config/workspace/ProdDir/AI/oc-wt/tF3`  
Branch / HEAD: `task-F3` / `70114aa5cbce2946d95abea2c0d6b4e8007d0e6b`  
Role: Final Verification Wave, F3 Real Manual QA

## Execution journal

Environment snapshot:

- Runtime: `rustc 1.96.0 (ac68faa20 2026-05-25)`, `cargo 1.96.0 (30a34c682 2026-05-25)`.
- Required launchers: `target/debug/opencode-rust`, `target/debug/oc-server`.
- Isolation root: `/tmp/opencode/f3-opencode-rust-20260808`.
- References read: `references/runtimes/rust.md`, `references/methodology/00-setup.md`, `references/methodology/08-qa.md`, `references/methodology/09-cleanup.md` from oh-my-openagent 4.21.0.
- Initial `git status --short`: clean.

Artifacts to remove or stop before final verification:

- [x] `/tmp/opencode/f3-opencode-rust-20260808` — removed after QA.
- [x] tmux session `f3-opencode-tui` — absent after QA.
- [x] local test HTTP servers and `oc-server` processes — exact spawned processes terminated; ports 39831–39835 verified free.
- [x] `.opencode/` — isolated memory QA created a project-memory artifact in the worktree; both untracked files were removed after capture.
- [x] `target/` changes — build artifacts are ignored; final Git verification found no tracked source/test/doc drift.

## Results

Results are populated below from actual commands and observed output.

### 0. Required build — PASS

Command:

```sh
cargo build --workspace --offline
```

Observed output (trimmed):

```text
Compiling oc-server v0.1.0 (/config/workspace/ProdDir/AI/oc-wt/tF3/crates/oc-server)
Compiling oc-tui v0.1.0 (/config/workspace/ProdDir/AI/oc-wt/tF3/crates/oc-tui)
Compiling oc-cli v0.1.0 (/config/workspace/ProdDir/AI/oc-wt/tF3/crates/oc-cli)
Finished `dev` profile [unoptimized + debuginfo] target(s) in 26.58s
```

Judgement: **PASS**. The required offline workspace build completed and produced the debug binaries.

### 1. First contact — PASS

All invocations used the same isolation envelope (abbreviated as `$ISO` below):

```sh
env -i \
  HOME=/tmp/opencode/f3-opencode-rust-20260808/first/home \
  XDG_CONFIG_HOME=/tmp/opencode/f3-opencode-rust-20260808/first/config \
  XDG_DATA_HOME=/tmp/opencode/f3-opencode-rust-20260808/first/data \
  XDG_CACHE_HOME=/tmp/opencode/f3-opencode-rust-20260808/first/cache \
  XDG_STATE_HOME=/tmp/opencode/f3-opencode-rust-20260808/first/state \
  TMPDIR=/tmp/opencode/f3-opencode-rust-20260808/first/tmp \
  PATH=/usr/bin:/bin TERM=xterm-256color
```

Commands and observed output:

```console
$ $ISO ./target/debug/opencode-rust --version
1.18.13
[exit=0]

$ $ISO ./target/debug/opencode-rust --version --long
opencode-rust 0.1.0 (Rust package 0.1.0; plugin compatibility 1.18.13)
[exit=0]

$ $ISO ./target/debug/opencode-rust definitely-not-a-command
error: unrecognized subcommand 'definitely-not-a-command'

Usage: opencode-rust [OPTIONS] [COMMAND]

For more information, try '--help'.
[exit=2]

$ $ISO ./target/debug/opencode-rust --definitely-not-a-flag
error: unexpected argument '--definitely-not-a-flag' found

Usage: opencode-rust [OPTIONS] [COMMAND]

For more information, try '--help'.
[exit=2]

$ $ISO ./target/debug/opencode-rust --log-level NOPE
error: invalid value 'NOPE' for '--log-level <LOG_LEVEL>'
  [possible values: DEBUG, INFO, WARN, ERROR]

For more information, try '--help'.
[exit=2]
```

`--help` exited 0 and listed these real command groups: `run`, `tui`, `serve`, `session`, `agent`, `models`, `providers`, `mcp`, `db`, `debug`, `completion`, `export`, `import`, and the documented explanatory commands. It also explained the danger of `--auto` rather than presenting it as an innocuous switch.

Judgement: **PASS**. Version identity is explicit, parse failures are specific and non-zero, malformed enumerated values list valid alternatives, and help exposes the implemented surface.

### 2. Config-only provider, empty cache, external model fetch disabled — PASS

Config written only under the isolated XDG config directory; no `OPENCODE_MODELS_PATH` was set:

```json
{
  "$schema": "https://opencode.ai/config.json",
  "provider": {
    "localqa": {
      "name": "F3 Local QA",
      "npm": "@ai-sdk/openai-compatible",
      "options": {
        "baseURL": "http://127.0.0.1:39831/v1",
        "apiKey": "local-test-key"
      },
      "models": {
        "f3-model": { "name": "F3 Local Model" }
      }
    }
  },
  "model": "localqa/f3-model",
  "memory": false
}
```

Commands and observed output:

```console
$ env -i HOME=... XDG_CONFIG_HOME=... OPENCODE_DISABLE_MODELS_FETCH=1 \
    ./target/debug/opencode-rust models
localqa/f3-model
[exit=0]

$ # local HTTP server listening on 127.0.0.1:39831, then:
$ env -i HOME=... XDG_CONFIG_HOME=... OPENCODE_DISABLE_MODELS_FETCH=1 \
    ./target/debug/opencode-rust run --model localqa/f3-model \
    --format json "Reply with exactly LOCAL_OK"
{"detail":"session titled: LOCAL_OK","step":0,"type":"status_detail"}
{"modelID":"f3-model","providerID":"openai-compatible","step":1,"type":"model_resolved"}
{"messageCount":2,"step":1,"type":"provider_request_started"}
{"step":1,"text":"LOCAL_OK","type":"text"}
{"step":1,"stopReason":"Stop","type":"message_end"}
{"finishReason":"Stop","step":1,"type":"step_completed"}
{"steps":1,"type":"turn_completed",...}
[exit=0]
```

Local server evidence (trimmed):

```text
READY
REQUEST /v1/chat/completions ... "model":"f3-model","stream":true ...
"POST /v1/chat/completions HTTP/1.1" 200 -
REQUEST /v1/chat/completions ... "Reply with exactly LOCAL_OK" ...
"POST /v1/chat/completions HTTP/1.1" 200 -
```

Judgement: **PASS**. With an empty cache, fetch disabled, and no model-path injection, the configured model was listed and an actual turn reached the endpoint supplied exclusively through `options.baseURL`.

### 3. Provider error diagnostics and credential handling — DEFECT

Each case used an isolated config with the same `@ai-sdk/openai-compatible` provider/model shape. The exact run form was:

```sh
env -i \
  HOME=/tmp/opencode/f3-opencode-rust-20260808/errors/CASE/home \
  XDG_CONFIG_HOME=/tmp/opencode/f3-opencode-rust-20260808/errors/CASE/config \
  XDG_DATA_HOME=/tmp/opencode/f3-opencode-rust-20260808/errors/CASE/data \
  XDG_CACHE_HOME=/tmp/opencode/f3-opencode-rust-20260808/errors/CASE/cache \
  XDG_STATE_HOME=/tmp/opencode/f3-opencode-rust-20260808/errors/CASE/state \
  TMPDIR=/tmp/opencode/f3-opencode-rust-20260808/errors/CASE/tmp \
  PATH=/usr/bin:/bin TERM=xterm-256color \
  OPENCODE_DISABLE_MODELS_FETCH=1 \
  ./target/debug/opencode-rust run --model failureqa/f3-model \
  --format json "hello from F3"
```

#### 3a. Dead port — PASS

`options.baseURL` was `http://127.0.0.1:39999/v1` with no listener.

```text
transient provider failure (status=None): error sending request for url
(http://127.0.0.1:39999/v1/chat/completions): client error (Connect):
tcp connect error: Connection refused (os error 111)
[exit=1]
```

Judgement: **PASS**. The diagnostic includes the full attempted URL and the OS-level refusal.

#### 3b. Misspelled hostname — PASS

`options.baseURL` was `http://f3-host-does-not-exist.invalid/v1`.

```text
transient provider failure (status=None): error sending request for url
(http://f3-host-does-not-exist.invalid/v1/chat/completions): client error
(Connect): dns error: failed to lookup address information: Name or service not known
[exit=1]
```

Judgement: **PASS**. The diagnostic names the exact hostname/URL and the DNS cause.

#### 3c. Unset `${VAR}` — DEFECT

Reproduction config fragment:

```json
"options": {
  "baseURL": "${F3_MISSING_BASE_URL}",
  "apiKey": "unset-variable-test-key"
}
```

`F3_MISSING_BASE_URL` was deliberately absent from `env -i`. Observed output:

```text
{"detail":"title: transient provider failure (status=None)","step":0,"type":"status_detail"}
...
{"messageCount":2,"step":1,"type":"provider_request_started"}
transient provider failure (status=None): builder error: relative URL without a base
[exit=1]
```

Judgement: **DEFECT**. The program silently substituted the missing variable with an empty string and emitted only a downstream URL-builder category. It did not name `F3_MISSING_BASE_URL`, so the required cause-specific diagnostic is absent. A user can inspect the config and set the variable, so this is recoverable rather than a data-risk blocker.

#### 3d. HTTP 401 and secret leakage — PASS

The config contained `"apiKey": "${F3_401_KEY}"`; only the process environment contained `F3_401_KEY=sk-SUPERSECRET-DO-NOT-ECHO`. A local fixture at `http://127.0.0.1:39832/v1` returned HTTP 401 for both requests.

Exact client invocation:

```sh
env -i \
  HOME=/tmp/opencode/f3-opencode-rust-20260808/errors/auth401/home \
  XDG_CONFIG_HOME=/tmp/opencode/f3-opencode-rust-20260808/errors/auth401/config \
  XDG_DATA_HOME=/tmp/opencode/f3-opencode-rust-20260808/errors/auth401/data \
  XDG_CACHE_HOME=/tmp/opencode/f3-opencode-rust-20260808/errors/auth401/cache \
  XDG_STATE_HOME=/tmp/opencode/f3-opencode-rust-20260808/errors/auth401/state \
  TMPDIR=/tmp/opencode/f3-opencode-rust-20260808/errors/auth401/tmp \
  PATH=/usr/bin:/bin TERM=xterm-256color \
  OPENCODE_DISABLE_MODELS_FETCH=1 \
  F3_401_KEY=sk-SUPERSECRET-DO-NOT-ECHO \
  ./target/debug/opencode-rust run --model failureqa/f3-model \
  --format json "hello from F3 auth"
```

Observed output:

```text
authentication rejected by provider failureqa: provider `failureqa` returned HTTP 401:
{"error": {"message": "unauthorized by F3 fixture", "type": "authentication_error"}};
set `provider.failureqa.options.apiKey`, or run `opencode auth login failureqa`
[exit=1]
```

Leak scans:

```console
$ printf '%s' "$captured_output" | grep -F 'sk-SUPERSECRET-DO-NOT-ECHO'
[no match]
$ grep -R -F 'sk-SUPERSECRET-DO-NOT-ECHO' \
    /tmp/opencode/f3-opencode-rust-20260808/errors/auth401
[no match]
```

Judgement: **PASS**. The message gives both valid key-placement actions, names the provider and status, exits non-zero, and does not echo or persist the distinctive key anywhere in the isolated run tree.

### 4. `oc-server` runtime — PASS

Server launch:

```sh
env -i \
  HOME=/tmp/opencode/f3-opencode-rust-20260808/server/home \
  XDG_CONFIG_HOME=/tmp/opencode/f3-opencode-rust-20260808/server/config \
  XDG_DATA_HOME=/tmp/opencode/f3-opencode-rust-20260808/server/data \
  XDG_CACHE_HOME=/tmp/opencode/f3-opencode-rust-20260808/server/cache \
  XDG_STATE_HOME=/tmp/opencode/f3-opencode-rust-20260808/server/state \
  TMPDIR=/tmp/opencode/f3-opencode-rust-20260808/server/tmp \
  PATH=/usr/bin:/bin \
  OPENCODE_SERVER_USERNAME=f3user \
  OPENCODE_SERVER_PASSWORD=f3-server-password \
  ./target/debug/oc-server serve --hostname 127.0.0.1 --port 39833
```

The server was stopped after each probe by its exact recorded PID.

#### 4a. Basic authentication — PASS

```console
$ curl -i http://127.0.0.1:39833/global/health
HTTP/1.1 401 Unauthorized
www-authenticate: Basic realm="Secure Area"
content-length: 0

$ curl -i -u f3user:wrong http://127.0.0.1:39833/global/health
HTTP/1.1 401 Unauthorized
www-authenticate: Basic realm="Secure Area"
content-length: 0

$ curl -i -u f3user:f3-server-password \
    http://127.0.0.1:39833/compat/v1/diagnostics
HTTP/1.1 200 OK
content-type: application/json
...
{"toasts":{"accepted":0,...},"unknownRoutes":{"total":0,...},"v1Surface":{...}}
```

Judgement: **PASS**. Missing and wrong credentials are challenged; the configured credentials reach a real 200 endpoint.

#### 4b. Known event route behavior — PASS

```console
$ curl -i http://127.0.0.1:39833/api/event
HTTP/1.1 401 Unauthorized
www-authenticate: Basic realm="Secure Area"

$ curl -i -u f3user:f3-server-password http://127.0.0.1:39833/api/event
HTTP/1.1 404 Not Found
content-length: 0

$ curl -i --max-time 1 -u f3user:f3-server-password \
    'http://127.0.0.1:39833/event?sessionID=ses-does-not-exist'
HTTP/1.1 200 OK
content-type: text/event-stream
cache-control: no-cache
x-content-type-options: nosniff
...
curl: (28) Operation timed out after 1002 milliseconds with 0 bytes received
```

The one-second timeout was intentional for a live SSE stream. `README.md:88-91` explicitly says `/api/event` and `/api/session/{sessionID}/event` are unregistered and that the equivalent stream is at `/event`.

Judgement: **PASS**. `/api/event` is the documented 404, not an unexpected regression; `/event` establishes the declared SSE response.

#### 4c. Non-loopback safety gate — PASS

```console
$ env -i HOME=... XDG_CONFIG_HOME=... XDG_DATA_HOME=... \
    PATH=/usr/bin:/bin \
    ./target/debug/oc-server serve --hostname 0.0.0.0 --port 39834
refusing --hostname `0.0.0.0`: a non-loopback listener would expose the
unauthenticated server to the network; set OPENCODE_SERVER_PASSWORD to a
non-empty value before using this --hostname
[exit=1]
```

Judgement: **PASS**. An unauthenticated non-loopback bind is refused before listening, with an actionable condition for deliberately enabling authenticated remote access.

### 5. Interactive TUI — PASS, with first-launch prerequisite observed

The TUI was launched in the required `interactive_bash`/tmux session `f3-opencode-tui` at 120x35. The wrapper kept the pane alive only to expose the binary's exit code; the binary itself owned the terminal while running.

Exact binary invocation inside tmux:

```sh
env -i \
  HOME=/tmp/opencode/f3-opencode-rust-20260808/tui/home \
  XDG_CONFIG_HOME=/tmp/opencode/f3-opencode-rust-20260808/tui/config \
  XDG_DATA_HOME=/tmp/opencode/f3-opencode-rust-20260808/tui/data \
  XDG_CACHE_HOME=/tmp/opencode/f3-opencode-rust-20260808/tui/cache \
  XDG_STATE_HOME=/tmp/opencode/f3-opencode-rust-20260808/tui/state \
  TMPDIR=/tmp/opencode/f3-opencode-rust-20260808/tui/tmp \
  PATH=/usr/bin:/bin TERM=xterm-256color \
  /config/workspace/ProdDir/AI/oc-wt/tF3/target/debug/opencode-rust tui
```

With a truly empty config, the process exited cleanly with an actionable prerequisite rather than rendering a broken frame:

```text
no available model; configure a provider credential or provider block
F3_TUI_EXIT=1
```

After adding only the same isolated local provider/model used in check 2, the first frame rendered:

```text



 idle
▏
```

Typing `hello` updated the composer in place:

```text
 idle
hello▏
```

The first `Ctrl-C` cleared the composer. The second `Ctrl-C` exited the application. Post-exit capture:

```text
F3_TUI_EXIT=0
```

The tmux pane remained a normal `sh` process only because the QA wrapper intentionally slept after the binary returned; the display was reset and no escape-sequence debris or stuck alternate screen remained. The exact tmux session was then killed.

Judgement: **PASS**. The configured TUI renders, accepts editing keystrokes, uses an intuitive two-stage cancel/quit sequence, returns 0, and restores the terminal. The no-model first-launch message is direct and actionable.

### 6. Session list and prune preview — PASS

I copied the isolated database created by check 2, never the 62 GB user database:

```sh
cp /tmp/opencode/f3-opencode-rust-20260808/provider/data/opencode/opencode-local.db \
   /tmp/opencode/f3-opencode-rust-20260808/sessions/throwaway.db
```

All commands explicitly set:

```sh
OPENCODE_DB=/tmp/opencode/f3-opencode-rust-20260808/sessions/throwaway.db
```

#### 6a. Project-local and global listing — PASS

```console
$ env -i HOME=... XDG_CONFIG_HOME=... OPENCODE_DB="$THROWAWAY" \
    ./target/debug/opencode-rust session list --format json
[
  {
    "agent": "build",
    "directory": "/config/workspace/ProdDir/AI/oc-wt/tF3",
    "id": "ses_738026eec17c4c33ba2fe3bfc90d8b01",
    "model": {"modelID":"f3-model","providerID":"localqa"},
    "title": "LOCAL_OK",
    "version": "1.18.13",
    ...
  }
]
[exit=0]

$ env -i HOME=... XDG_CONFIG_HOME=... OPENCODE_DB="$THROWAWAY" \
    ./target/debug/opencode-rust session list --all-projects --format json
[
  {
    "id": "ses_738026eec17c4c33ba2fe3bfc90d8b01",
    "project":{"worktree":"/config/workspace/ProdDir/AI/oc-wt/tF3",...},
    "title":"LOCAL_OK",
    ...
  }
]
[exit=0]
```

Judgement: **PASS**. Both scopes locate the known throwaway session and expose useful project/model metadata.

#### 6b. Preview with an actual selected candidate is non-mutating — PASS

`--include-recent` deliberately made the fresh fixture eligible. No `--archive`, `--delete`, `--yes`, or `--force` was passed.

```console
$ sha256sum "$THROWAWAY"
eb8e6c3ee78a575d548d752a419264f9f542aeb2b5fe734583ddb77cb4328813  .../throwaway.db

$ env -i HOME=... XDG_CONFIG_HOME=... OPENCODE_DB="$THROWAWAY" \
    ./target/debug/opencode-rust session prune --older-than 0 \
    --all-projects --include-recent --format json
{"action":"preview",
 "selected_session_ids":["ses_738026eec17c4c33ba2fe3bfc90d8b01"],
 "excluded":[],
 "database":{"total_rows":5,"total_bytes":1403,...},
 "changed_sessions":0,
 "warnings":[]}
[exit=0]

$ sha256sum "$THROWAWAY"
eb8e6c3ee78a575d548d752a419264f9f542aeb2b5fe734583ddb77cb4328813  .../throwaway.db
byte-identical=yes
```

A subsequent `session list --all-projects --format json` returned the same session ID and title.

Judgement: **PASS**. Preview reports a real selected session and estimated rows/bytes but changes zero sessions; the database remains byte-for-byte identical and readable afterward.

### 7. Memory kill switch — PASS

Both modes used separate isolated config/data/cache/state trees and the same local OpenAI-compatible capture server.

#### 7a. `memory: false` — PASS

Command:

```sh
env -i \
  HOME=/tmp/opencode/f3-opencode-rust-20260808/memory/off/home \
  XDG_CONFIG_HOME=/tmp/opencode/f3-opencode-rust-20260808/memory/off/config \
  XDG_DATA_HOME=/tmp/opencode/f3-opencode-rust-20260808/memory/off/data \
  XDG_CACHE_HOME=/tmp/opencode/f3-opencode-rust-20260808/memory/off/cache \
  XDG_STATE_HOME=/tmp/opencode/f3-opencode-rust-20260808/memory/off/state \
  TMPDIR=/tmp/opencode/f3-opencode-rust-20260808/memory/off/tmp \
  PATH=/usr/bin:/bin TERM=xterm-256color \
  OPENCODE_DISABLE_MODELS_FETCH=1 \
  ./target/debug/opencode-rust run --model memoryqa/f3-model \
  --format json "My deployment color is saffron."
```

Observed client output (trimmed):

```text
{"toolIDs":["invalid","bash","read","glob","grep","edit","write","webfetch","todowrite"],"type":"tool_snapshot_locked",...}
{"messageCount":2,"step":1,"type":"provider_request_started"}
{"step":1,"text":"MEMORY_DISABLED","type":"text"}
{"steps":1,"type":"turn_completed",...}
[exit=0]
```

Captured main provider request:

```text
message[0]: role=system, content=""
message[1]: role=user, content="My deployment color is saffron."
tools: invalid,bash,read,glob,grep,edit,write,webfetch,todowrite
```

The local server received exactly the title request and one main request; there was no reflection/write request, no `memory` tool, and no memory block.

Judgement: **PASS**. `memory: false` removes both the prompt injection and the write mechanism.

#### 7b. `memory: true` — PASS

The same user turn under the `memory: true` isolation exposed `memory` in the tool snapshot. The local provider issued one real `memory` tool call:

```text
{"id":"call_memory_f3","name":"memory","step":1,"type":"tool_use_start"}
{"callID":"call_memory_f3","name":"memory","step":1,"type":"tool_dispatch_started"}
{"callID":"call_memory_f3","isError":false,"name":"memory",
 "output":"{\"current\":31,\"done\":true,\"entry_count\":1,
 \"limit\":3000,\"message\":\"Applied 1 operation(s).\",
 \"scope\":\"project\",\"success\":true,...}",
 "title":"memory project updated","type":"tool_dispatch_completed"}
{"step":2,"text":"MEMORY_ENABLED","type":"text"}
{"steps":2,"type":"turn_completed",...}
[exit=0]
```

The operation saved `F3 deployment color is saffron.` in project scope. A new session was then run:

```sh
env -i HOME=.../memory/on/home XDG_CONFIG_HOME=.../memory/on/config \
  XDG_DATA_HOME=.../memory/on/data XDG_CACHE_HOME=.../memory/on/cache \
  XDG_STATE_HOME=.../memory/on/state TMPDIR=.../memory/on/tmp \
  PATH=/usr/bin:/bin TERM=xterm-256color OPENCODE_DISABLE_MODELS_FETCH=1 \
  ./target/debug/opencode-rust run --model memoryqa/f3-model \
  --format json "What deployment color did I tell you?"
```

Captured main provider request from that new session:

```text
system:
══════════════════════════════════════════════
MEMORY (project rules) [1% — 31/3,000 chars]
══════════════════════════════════════════════
F3 deployment color is saffron.

user: What deployment color did I tell you?
tools: invalid,bash,read,glob,grep,edit,write,webfetch,todowrite,memory
```

The turn returned `MEMORY_RECALL_OK` and exited 0.

Judgement: **PASS**. Enabling memory exposes the write tool, executes and persists a successful reflection/write, and injects the saved project memory into a future session's actual provider prompt.

### 8. Side-by-side with released TypeScript 1.18.12 — BLOCKER

Both binaries used the same isolated XDG directories and the same compatible config (the Rust-only `memory` key was intentionally removed for this parity check).

#### 8a. Versions and model listing

```console
$ env -i HOME=... XDG_CONFIG_HOME=... OPENCODE_DISABLE_MODELS_FETCH=1 \
    ./target/debug/opencode-rust --version
1.18.13
[exit=0]

$ env -i HOME=... XDG_CONFIG_HOME=... OPENCODE_DISABLE_MODELS_FETCH=1 \
    /config/.local/share/mise/installs/opencode/1.18.12/opencode --version
1.18.12
[exit=0]

$ env -i HOME=... XDG_CONFIG_HOME=... OPENCODE_DISABLE_MODELS_FETCH=1 \
    ./target/debug/opencode-rust models
localqa/f3-model
[exit=0]

$ env -i HOME=... XDG_CONFIG_HOME=... OPENCODE_DISABLE_MODELS_FETCH=1 \
    /config/.local/share/mise/installs/opencode/1.18.12/opencode models
opencode/big-pickle
opencode/deepseek-v4-flash-free
opencode/laguna-s-2.1-free
opencode/ling-3.0-flash-free
opencode/mimo-v2.5-free
opencode/nemotron-3-ultra-free
opencode/north-mini-code-free
localqa/f3-model
[exit=0]
```

Judgement: **OBSERVATION**. Both resolve the config-only model, but the TypeScript release additionally exposes bundled `opencode/*` models while model fetching is disabled and the cache is empty. The Rust binary does not claim those models in this condition.

#### 8b. `OPENCODE_DB` selects a TypeScript-created database — PASS

The TypeScript binary created a session in its isolated default `opencode.db` by completing a real local-server turn (`SIDE_OK`, 8 non-zero tokens). It then listed:

```json
[
  {
    "id": "ses_01c6c19c2ffeiz599h0hAmkxYL",
    "title": "SIDE_OK",
    "updated": 1786229550187,
    "created": 1786229548605,
    "projectId": "e847d413559b8a19c26d5eb9a68151d3b90c9fe4",
    "directory": "/config/workspace/ProdDir/AI/oc-wt/tF3"
  }
]
```

Pointing Rust at that exact file worked:

```sh
OPENCODE_DB=/tmp/opencode/f3-opencode-rust-20260808/side/data/opencode/opencode.db \
  ./target/debug/opencode-rust session list --all-projects --format json
```

Observed Rust output included:

```json
{
  "id": "ses_01c6c19c2ffeiz599h0hAmkxYL",
  "title": "SIDE_OK",
  "model": {"id":"f3-model","providerID":"localqa","variant":"default"},
  "tokens":{"input":7,"output":1,"reasoning":0,...},
  "version":"1.18.12"
}
```

Judgement: **PASS**. `OPENCODE_DB` overrides channel database selection and Rust can read a TypeScript-created session.

#### 8c. Rust-created session makes the same database unreadable to TypeScript — BLOCKER

This reproduction mutates only a throwaway copy. Before the Rust turn, TypeScript listed the copied TypeScript-only database successfully:

```console
$ OPENCODE_DB=/tmp/opencode/f3-opencode-rust-20260808/side/mixed.db \
    /config/.local/share/mise/installs/opencode/1.18.12/opencode \
    session list --format json
[
  {
    "id": "ses_01c6c19c2ffeiz599h0hAmkxYL",
    "title": "SIDE_OK",
    ...
  }
]
[exit=0]
```

Rust then wrote one real session to that same explicit database:

```console
$ OPENCODE_DB=/tmp/opencode/f3-opencode-rust-20260808/side/mixed.db \
    ./target/debug/opencode-rust run --model localqa/f3-model \
    --format json "Reply MIXED_OK"
{"detail":"session titled: MIXED_OK","step":0,"type":"status_detail"}
{"sessionID":"ses_e8bcdee5a4e846af96470ea89002bab5","type":"turn_started"}
{"step":1,"text":"MIXED_OK","type":"text"}
{"finishReason":"Stop","step":1,"type":"step_completed"}
{"steps":1,"type":"turn_completed",...}
[exit=0]
```

Rust could list both `MIXED_OK` (1.18.13) and `SIDE_OK` (1.18.12). The released TypeScript binary could no longer list the same database:

```console
$ OPENCODE_DB=/tmp/opencode/f3-opencode-rust-20260808/side/mixed.db \
    /config/.local/share/mise/installs/opencode/1.18.12/opencode \
    session list --format json
Error: Unexpected error

Expected string, got undefined
[exit=1]
```

Judgement: **BLOCKER**. A normal Rust turn writes a session record the released TypeScript binary cannot decode. This defeats the documented rollback/side-by-side path: after using Rust, a user cannot list sessions with the released binary even when `OPENCODE_DB` selects the intended file. `README.md` states that the released binary can keep using a Rust-created database; actual runtime behavior contradicts that claim.

### 9. Exploratory user flow: export a known session — BLOCKER

Export is advertised as a real user command in top-level help:

```text
export      Export session data
```

Its own help also presents an operational command rather than an explanatory placeholder:

```console
$ OPENCODE_DB=/tmp/opencode/f3-opencode-rust-20260808/sessions/throwaway.db \
    ./target/debug/opencode-rust export --help
Export session data

Usage: opencode-rust export [OPTIONS] [ARGS]...

Arguments:
  [ARGS]...  Command-specific arguments
...
[exit=0]
```

I then exported the known session that `session list` had just returned:

```console
$ OPENCODE_DB=/tmp/opencode/f3-opencode-rust-20260808/sessions/throwaway.db \
    ./target/debug/opencode-rust export ses_738026eec17c4c33ba2fe3bfc90d8b01
`export` is registered, but its handler is pending todo 56
[exit=1]
```

The same message appears for an unknown ID, so no export lookup or output occurs. `docs/compatibility-matrix.md:89` classifies `ExportCommand` as **implemented**, despite the runtime saying its handler is pending.

Judgement: **BLOCKER**. A user following `--help` to back up or transfer a real session cannot proceed at all, and the implementation-status documentation incorrectly says the command is implemented. There is no runtime workaround in this command.

## Findings summary

| Severity | Finding |
|---|---|
| BLOCKER | A Rust-created session record makes the shared database unreadable to released TypeScript 1.18.12 (`Expected string, got undefined`), breaking the documented side-by-side/rollback workflow. |
| BLOCKER | `export` is advertised and documented as implemented but every invocation exits 1 because its handler is pending. |
| DEFECT | An unset `${VAR}` in `provider.*.options.baseURL` is silently reduced to an empty value; the eventual error does not name the missing variable. |
| OBSERVATION | With empty cache and model fetch disabled, TypeScript also lists bundled `opencode/*` models while Rust lists only the config-defined model. Both list the config-defined model. |

No credential leak was found. Preview prune was byte-stable. The config-only provider and `options.baseURL` path worked. Authenticated server behavior, the documented `/api/event` 404, loopback safety, TUI rendering/exit, memory off/on behavior, and Rust reading a TypeScript-created database all worked in the exercised cases.

## Honest test gaps

- I did not run the prohibited 100-minute memory gate or two-hour soak.
- I did not mutate or open `/config/.local/share/opencode/opencode.db`; all databases were isolated fixtures or copies created under the QA temp root.
- I did not exercise destructive prune (`--archive` or `--delete`), because the assignment required preview mode only.
- I did not test every server route; I tested authentication, a real 200 diagnostics endpoint, `/event`, the known `/api/event` 404, and non-loopback refusal.
- I did not submit the typed TUI prompt to a provider; provider turns were exercised separately through `run`, while TUI QA focused on rendering, editing, cancel, and clean exit.

## Verdict

**REJECT** — two runtime blockers remain.

Exact blocker reproductions:

1. **Rust writes a session that TypeScript 1.18.12 cannot list**

   ```sh
   export OPENCODE_DB=/tmp/opencode/f3-opencode-rust-20260808/side/mixed.db
   ./target/debug/opencode-rust run --model localqa/f3-model --format json "Reply MIXED_OK"
   /config/.local/share/mise/installs/opencode/1.18.12/opencode session list --format json
   ```

   Observed final output: `Error: Unexpected error` / `Expected string, got undefined`, exit 1. The full report above documents the isolated `HOME`/`XDG_*` envelope and the before/after control.

2. **Advertised export command has no handler**

   ```sh
   OPENCODE_DB=/tmp/opencode/f3-opencode-rust-20260808/sessions/throwaway.db \
     ./target/debug/opencode-rust export ses_738026eec17c4c33ba2fe3bfc90d8b01
   ```

   Observed output: `` `export` is registered, but its handler is pending todo 56 ``, exit 1.

## Final cleanup and verification

Cleanup output:

```text
tmux-session=absent
port-39831=free
port-39832=free
port-39833=free
port-39834=free
port-39835=free
isolation-root=removed
```

The memory QA artifact under `.opencode/` was also removed. The required build result is recorded in check 0. Final diagnostics and Git scope verification were run after this report was complete.
