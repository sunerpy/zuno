# F3 Real Manual QA Report — Final Verification Wave 2

Date: 2026-08-09  
Worktree: `/config/workspace/ProdDir/AI/oc-wt/tF3`  
Branch / HEAD: `task-F3` / `3d68d7a93b110f000a702537009c63f11c500122`  
Role: F3, Real Manual QA

## Execution journal

Environment snapshot:

- Runtime: `rustc 1.96.0 (ac68faa20 2026-05-25)`, `cargo 1.96.0 (30a34c682 2026-05-25)`.
- Required launchers: `target/debug/opencode-rust`, `target/debug/oc-server` (absent before this round's build).
- Isolation root: `/tmp/opencode/f3-wave2-20260809`.
- Reserved test ports: 40831–40840; all were free before QA.
- Initial tracked/untracked status: clean.
- References read: `references/runtimes/rust.md`, `references/methodology/00-setup.md`, `02-investigate.md`, `08-qa.md`, and `09-cleanup.md` from oh-my-openagent 4.21.0.

Artifacts to remove or stop before final verification:

- [x] `/tmp/opencode/f3-wave2-20260809` — all isolated homes, configs, databases, scripts, captures, and logs removed.
- [x] tmux sessions prefixed `f3w2-` — interactive TUI and PTY QA sessions absent.
- [x] local fixture servers and `oc-server` processes — exact recorded PIDs terminated.
- [x] test ports 40831–40840 — verified free.
- [x] any `.opencode/` project-memory artifact created by memory-on QA — contained under and removed with the isolation root.
- [x] `target/` build output — confirmed ignored; no tracked source drift introduced.

## Results

Results below contain only commands actually run and outputs actually observed.

### 0. Offline workspace build — PASS

Command:

```sh
cargo build --workspace --offline
```

Observed output (trimmed):

```text
Compiling oc-server v0.1.0 (/config/workspace/ProdDir/AI/oc-wt/tF3/crates/oc-server)
Compiling oc-tui v0.1.0 (/config/workspace/ProdDir/AI/oc-wt/tF3/crates/oc-tui)
Compiling oc-cli v0.1.0 (/config/workspace/ProdDir/AI/oc-wt/tF3/crates/oc-cli)
Finished `dev` profile [unoptimized + debuginfo] target(s) in 29.03s
```

Judgement: **PASS**. The complete workspace built offline and produced the real debug binaries used below.

### 1. First contact — PASS

Every invocation used this isolation shape (the scenario-specific suffix changed below):

```sh
env -i \
  HOME=/tmp/opencode/f3-wave2-20260809/first/home \
  XDG_CONFIG_HOME=/tmp/opencode/f3-wave2-20260809/first/config \
  XDG_DATA_HOME=/tmp/opencode/f3-wave2-20260809/first/data \
  XDG_CACHE_HOME=/tmp/opencode/f3-wave2-20260809/first/cache \
  XDG_STATE_HOME=/tmp/opencode/f3-wave2-20260809/first/state \
  TMPDIR=/tmp/opencode/f3-wave2-20260809/first/tmp \
  PATH=/usr/bin:/bin TERM=xterm-256color \
  ./target/debug/opencode-rust ...
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

$ $ISO ./target/debug/opencode-rust --log-level NOPE
error: invalid value 'NOPE' for '--log-level <LOG_LEVEL>'
  [possible values: DEBUG, INFO, WARN, ERROR]

For more information, try '--help'.
[exit=2]
```

`--help` exited 0. It listed `run`, `tui`, `serve`, `session`, `agent`, `models`, `providers`, `mcp`, `db`, `debug`, `completion`, `export`, and `import`, plus the explanatory rejected commands. It also retained the explicit danger explanation for `--auto`.

Judgement: **PASS**. Identity, help, unknown-command handling, malformed enum handling, and non-zero parse exits are usable and specific.

### 2. Config-only provider with no model cache/path/network — PASS

The isolated config contained only a local OpenAI-compatible provider, including `theme: "system"`. `OPENCODE_MODELS_PATH` was not set and model fetching was disabled:

```json
{
  "$schema": "https://opencode.ai/config.json",
  "theme": "system",
  "provider": {
    "localqa": {
      "name": "F3 Wave 2 Local QA",
      "npm": "@ai-sdk/openai-compatible",
      "options": {
        "baseURL": "http://127.0.0.1:40831/v1",
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
$ $ISO OPENCODE_DISABLE_MODELS_FETCH=1 ./target/debug/opencode-rust debug config
{
  "$schema": "https://opencode.ai/config.json",
  ...
  "model": "localqa/f3-model",
  "provider": { "localqa": { ... } },
  "memory": false
}
[exit=0]

$ $ISO OPENCODE_DISABLE_MODELS_FETCH=1 ./target/debug/opencode-rust models
localqa/f3-model
[exit=0]

$ $ISO OPENCODE_DISABLE_MODELS_FETCH=1 \
    ./target/debug/opencode-rust run --model localqa/f3-model \
    --format json 'Reply with exactly LOCAL_OK'
{"detail":"session titled: LOCAL_OK","step":0,"type":"status_detail"}
{"sessionID":"ses_153b5e7ca90a4e998afdbd325ab049c3","type":"turn_started"}
{"modelID":"f3-model","providerID":"openai-compatible","step":1,"type":"model_resolved"}
{"messageCount":2,"step":1,"type":"provider_request_started"}
{"step":1,"text":"LOCAL_OK","type":"text"}
{"step":1,"stopReason":"Stop","type":"message_end"}
{"steps":1,"type":"turn_completed",...}
[exit=0]
```

The local fixture log recorded two actual `POST /v1/chat/completions` requests (title plus main turn), and the main request carried `model: "f3-model"` and `stream: true`.

Judgement: **PASS**. A provider defined exclusively in config works from an empty cache without external model metadata or network access.

### 3. Config validation narrowing — PASS

The user's formerly offending key is accepted, while a genuinely unknown key remains rejected and is named:

```console
$ $ISO ./target/debug/opencode-rust debug config   # config contains theme: system
{ ... valid merged config ... }
[exit=0]

$ $ISO ./target/debug/opencode-rust debug config  # config contains definitelyBogusF3Key
config file /tmp/opencode/f3-wave2-20260809/errors/bogus/config/opencode/opencode.json failed validation (1 issue(s))
  definitelyBogusF3Key: unrecognized key
[exit=1]
```

Judgement: **PASS**. Validation was narrowed for `theme`, not disabled.

### 4. Provider failure diagnostics and credential handling — one DEFECT

All four runs used isolated homes and this command form:

```sh
env -i HOME=... XDG_CONFIG_HOME=... XDG_DATA_HOME=... \
  XDG_CACHE_HOME=... XDG_STATE_HOME=... TMPDIR=... \
  PATH=/usr/bin:/bin TERM=xterm-256color \
  OPENCODE_DISABLE_MODELS_FETCH=1 \
  ./target/debug/opencode-rust run --model failureqa/f3-model \
  --format json 'hello from F3 ...'
```

#### 4a. Dead port — PASS

With `baseURL: http://127.0.0.1:40839/v1` and no listener:

```text
transient provider failure (status=None): error sending request for url
(http://127.0.0.1:40839/v1/chat/completions): client error (Connect):
tcp connect error: Connection refused (os error 111)
[exit=1]
```

Judgement: **PASS**. It names the attempted endpoint and refusal.

#### 4b. Misspelled hostname — PASS

With `baseURL: http://f3-wave2-host-does-not-exist.invalid/v1`:

```text
transient provider failure (status=None): error sending request for url
(http://f3-wave2-host-does-not-exist.invalid/v1/chat/completions): client error
(Connect): dns error: failed to lookup address information: Name or service not known
[exit=1]
```

Judgement: **PASS**. It names the misspelled host and DNS cause.

#### 4c. Unset `${VAR}` — DEFECT

Config:

```json
"options": {
  "baseURL": "${F3_WAVE2_MISSING_BASE_URL}",
  "apiKey": "missing-key"
}
```

`F3_WAVE2_MISSING_BASE_URL` was absent from `env -i`. Observed final output:

```text
{"messageCount":2,"step":1,"type":"provider_request_started"}
transient provider failure (status=None): builder error: relative URL without a base
[exit=1]
```

Judgement: **DEFECT**. This reproduces the prior issue unchanged: the error does not name `F3_WAVE2_MISSING_BASE_URL`. A user can inspect and repair the config, so this remains recoverable rather than a data/credential blocker.

#### 4d. HTTP 401 action and secret scan — PASS

The key existed only as `F3_WAVE2_401_KEY=sk-F3-WAVE2-UNIQUE-SECRET-9xQ7`; the fixture returned HTTP 401.

```text
authentication rejected by provider failureqa: provider `failureqa` returned HTTP 401:
{"error": {"message": "unauthorized by F3 wave-2 fixture", "type": "authentication_error"}};
set `provider.failureqa.options.apiKey`, or run `opencode auth login failureqa`
[exit=1]
```

Full isolated-tree scan:

```console
$ grep -R -F 'sk-F3-WAVE2-UNIQUE-SECRET-9xQ7' \
    /tmp/opencode/f3-wave2-20260809
[no matches]
```

Judgement: **PASS**. The error gives both credential placement actions and never prints or persists the distinctive secret.

### 5. Server, authentication, and SSE — PASS

Server launch:

```sh
env -i HOME=... XDG_CONFIG_HOME=... XDG_DATA_HOME=... \
  XDG_CACHE_HOME=... XDG_STATE_HOME=... TMPDIR=... PATH=/usr/bin:/bin \
  OPENCODE_SERVER_USERNAME=f3user \
  OPENCODE_SERVER_PASSWORD=f3-server-password \
  ./target/debug/oc-server serve --hostname 127.0.0.1 --port 40833
```

Authentication probes:

```console
$ curl -i http://127.0.0.1:40833/global/health
HTTP/1.1 401 Unauthorized
www-authenticate: Basic realm="Secure Area"
content-length: 0

$ curl -i -u f3user:wrong http://127.0.0.1:40833/global/health
HTTP/1.1 401 Unauthorized
www-authenticate: Basic realm="Secure Area"
content-length: 0

$ curl -i -u f3user:f3-server-password \
    http://127.0.0.1:40833/compat/v1/diagnostics
HTTP/1.1 200 OK
content-type: application/json
...
{"toasts":{...},"unknownRoutes":{"total":0,...},"v1Surface":{...}}
```

Global SSE (the one-second timeout is deliberate):

```console
$ curl -i --max-time 1 -u f3user:f3-server-password \
    http://127.0.0.1:40833/api/event
HTTP/1.1 200 OK
content-type: text/event-stream
cache-control: no-cache
x-accel-buffering: no
x-content-type-options: nosniff

data: {"data":{},"id":"evt_019fe5d2ff0e79e0b747dbd7c17e271b","type":"server.connected"}

curl: (28) Operation timed out after 1001 milliseconds with 89 bytes received
[exit=28, intentional live-stream timeout]
```

Per-session SSE:

```console
$ curl -i --max-time 1 -u f3user:f3-server-password \
    'http://127.0.0.1:40833/api/session/ses-f3-wave2/event?after=0'
HTTP/1.1 200 OK
content-type: text/event-stream
cache-control: no-cache
x-accel-buffering: no
x-content-type-options: nosniff

curl: (28) Operation timed out after 1001 milliseconds with 0 bytes received
[exit=28, intentional live-stream timeout]
```

Non-loopback safety:

```console
$ $ISO ./target/debug/oc-server serve --hostname 0.0.0.0 --port 40834
refusing --hostname `0.0.0.0`: a non-loopback listener would expose the
unauthenticated server to the network; set OPENCODE_SERVER_PASSWORD to a
non-empty value before using this --hostname
[exit=1]
```

I also launched the server through the top-level `opencode-rust serve` command. The actual health route behaved correctly:

```console
$ curl -i http://127.0.0.1:40836/api/health
HTTP/1.1 401 Unauthorized
www-authenticate: Basic realm="Secure Area"

$ curl -i -u f3user:f3-cli-server-password http://127.0.0.1:40836/api/health
HTTP/1.1 200 OK
content-type: application/json

{"healthy":true}
```

Judgement: **PASS**. Both launchers enforce auth, the new global SSE route emits the required event, the per-session SSE route remains open correctly, and unauthenticated remote exposure is blocked.

### 6. Real TUI turn and clean exit — PASS

The configured TUI was launched in tmux at 120x35 using an isolated tree and the same local provider:

```sh
env -i HOME=... XDG_CONFIG_HOME=... XDG_DATA_HOME=... \
  XDG_CACHE_HOME=... XDG_STATE_HOME=... TMPDIR=... \
  PATH=/usr/bin:/bin TERM=xterm-256color OPENCODE_DISABLE_MODELS_FETCH=1 \
  ./target/debug/opencode-rust tui
```

Initial frame:

```text
 idle
▏
```

I typed `hello` and pressed Enter. Observed frame after the actual provider turn:

```text
> You
  hello

* Assistant
  LOCAL_OK

 idle
▏
```

I then pressed `Ctrl-C`. With tmux `remain-on-exit` enabled solely to inspect status:

```text
pane_dead=1 pane_dead_status=0
Pane is dead (status 0, Sun Aug  9 09:58:38 2026)
```

Judgement: **PASS**. This round submitted a real TUI prompt, received a real response, exited 0, and left no stuck alternate screen.

### 7. Interactive guarded PTY foreground handoff — PASS

I invoked the production binary's hidden child guard from a real tmux terminal, with the shell's own PID as the expected foreground parent. The guarded payload prints `READY`, blocks in `read`, then echoes the input:

```sh
./target/debug/opencode-rust __oc_child_guard supervise $$ -- \
  /bin/sh /tmp/opencode/f3-wave2-20260809/pty/guarded_payload.sh
```

Observed terminal transcript after typing `hello-from-real-pty` and Enter:

```text
$ ./target/debug/opencode-rust __oc_child_guard supervise $$ -- /bin/sh .../guarded_payload.sh
READY
hello-from-real-pty
READ:hello-from-real-pty
$
```

Judgement: **PASS**. The guarded child became terminal foreground, read interactive input instead of stopping under `SIGTTIN`, returned the value, and restored the shell.

I also created and deleted a real server PTY:

```console
$ curl -u f3user:f3-server-password -H 'content-type: application/json' \
    -X POST http://127.0.0.1:40833/api/pty \
    --data '{"command":"/bin/sh","args":["-i"],"cwd":".../pty","title":"F3 interactive shell","env":{"TERM":"xterm-256color"}}'
{"data":{"id":"pty_019fe5e582f4gznF1OCCCoemXl",...,"status":"running","pid":3373721}}

$ curl -i -u f3user:f3-server-password -X DELETE \
    http://127.0.0.1:40833/api/pty/pty_019fe5e582f4gznF1OCCCoemXl
HTTP/1.1 204 No Content
```

### 8. Session list, all projects, and prune preview — PASS

I copied only the isolated database created in check 2:

```sh
cp /tmp/opencode/f3-wave2-20260809/provider/data/opencode/opencode-local.db \
   /tmp/opencode/f3-wave2-20260809/sessions/throwaway.db
```

Every command set `OPENCODE_DB` to that throwaway file.

```console
$ $ISO OPENCODE_DB="$DB" ./target/debug/opencode-rust session list --format json
[
  {
    "id": "ses_153b5e7ca90a4e998afdbd325ab049c3",
    "model": {"id":"f3-model","providerID":"localqa"},
    "title": "LOCAL_OK",
    "version": "1.18.13",
    ...
  }
]
[exit=0]

$ $ISO OPENCODE_DB="$DB" ./target/debug/opencode-rust \
    session list --all-projects --format json
[
  {
    "id": "ses_153b5e7ca90a4e998afdbd325ab049c3",
    "project":{"worktree":"/config/workspace/ProdDir/AI/oc-wt/tF3",...},
    "title":"LOCAL_OK",
    ...
  }
]
[exit=0]
```

Preview only, deliberately selecting the fresh session:

```console
$ sha256sum "$DB"
c0cdf5c3a101a6d8454cc1be2d3e79a3199f0f504da0ba9e7dabbcf1dd260ed2  .../throwaway.db

$ $ISO OPENCODE_DB="$DB" ./target/debug/opencode-rust session prune \
    --older-than 0 --all-projects --include-recent --format json
{"action":"preview",
 "selected_session_ids":["ses_153b5e7ca90a4e998afdbd325ab049c3"],
 "database":{"total_rows":5,"total_bytes":1398,...},
 "changed_sessions":0,
 "warnings":[]}
[exit=0]

$ sha256sum "$DB"
c0cdf5c3a101a6d8454cc1be2d3e79a3199f0f504da0ba9e7dabbcf1dd260ed2  .../throwaway.db
```

Judgement: **PASS**. Both list scopes work, and a preview with a real selected candidate is byte-for-byte non-mutating.

### 9. Memory kill switch both ways — PASS

Both runs used isolated directories and a local fixture that inspected the actual advertised tool list. It called `memory` only when the request advertised it.

#### 9a. `memory: false`

```console
$ $ISO ./target/debug/opencode-rust run --model memoryqa/f3-model \
    --format json 'My deployment color is saffron.'
{"toolIDs":["invalid","bash","read","glob","grep","edit","write","webfetch","todowrite"],"type":"tool_snapshot_locked",...}
{"messageCount":2,"step":1,"type":"provider_request_started"}
{"step":1,"text":"MEMORY_DISABLED","type":"text"}
{"steps":1,"type":"turn_completed",...}
[exit=0]
memory_file=absent
```

Judgement: **PASS**. The request had neither a memory tool nor an injected memory block, and no memory artifact was created.

#### 9b. `memory: true`

```console
$ $ISO ./target/debug/opencode-rust run --model memoryqa/f3-model \
    --format json 'My deployment color is saffron.'
{"toolIDs":[...,"memory"],"type":"tool_snapshot_locked",...}
{"id":"call_memory_f3_wave2","name":"memory","step":1,"type":"tool_use_start"}
{"delta":"{\"target\":\"project\",\"intent\":\"save the user's durable project fact\",\"operations\":[{\"action\":\"add\",\"content\":\"F3 wave 2 deployment color is saffron.\"}]}",...}
{"callID":"call_memory_f3_wave2","isError":false,"name":"memory",
 "output":"{\"current\":38,\"done\":true,\"entry_count\":1,...,\"success\":true,...}",
 "title":"memory project updated","type":"tool_dispatch_completed"}
{"step":2,"text":"MEMORY_ENABLED","type":"text"}
{"steps":2,"type":"turn_completed",...}
[exit=0]
```

Actual persisted file:

```text
/tmp/opencode/f3-wave2-20260809/memory/on/project/.opencode/RULES.md
F3 wave 2 deployment color is saffron.
```

I then started a new session:

```console
$ $ISO ./target/debug/opencode-rust run --model memoryqa/f3-model \
    --format json 'What deployment color did I tell you?'
{"toolIDs":[...,"memory"],"type":"tool_snapshot_locked",...}
{"step":1,"text":"MEMORY_RECALL_OK","type":"text"}
{"steps":1,"type":"turn_completed",...}
[exit=0]
```

The fixture parsed that main request and printed:

```text
user='What deployment color did I tell you?'
tools=invalid,bash,read,glob,grep,edit,write,webfetch,todowrite,memory
memory_in_system=true
memory_text=F3 wave 2 deployment color is saffron.
```

Judgement: **PASS**. The switch disables both exposure and persistence when false; when true, the real tool writes and a future session receives the saved memory in its actual system prompt.

### 10. Side-by-side with released TypeScript 1.18.12 — PASS

The released binary first created a session in an isolated explicit database through a real local-provider turn:

```console
$ $ISO OPENCODE_DB="$TSDB" \
    /config/.local/share/mise/installs/opencode/1.18.12/opencode \
    run --model localqa/f3-model --format json 'Reply with exactly SIDE_OK'
{"type":"step_start",...}
{"type":"text",...,"part":{"text":"LOCAL_OK",...}}
{"type":"step_finish",...,"tokens":{"total":8,"input":7,"output":1,...}}
[exit=0]

$ $ISO OPENCODE_DB="$TSDB" opencode session list --format json
[
  {
    "id": "ses_01a06210cffe1v3f0yfHTFhSNG",
    "title": "LOCAL_OK",
    "directory": "/config/workspace/ProdDir/AI/oc-wt/tF3"
  }
]
[exit=0]
```

Rust then read the same file through the `OPENCODE_DB` escape hatch and listed the known session with model `{"id":"f3-model","providerID":"localqa","variant":"default"}` and version `1.18.12`.

#### 10a. Exact prior blocker 8c reproduction — FIX VERIFIED

I copied the TypeScript-created database to `mixed.db`, ran a real Rust turn against the copy, then invoked released TypeScript 1.18.12 on that same copy:

```console
$ $ISO OPENCODE_DB="$MIXED" ./target/debug/opencode-rust run \
    --model localqa/f3-model --format json 'Reply with exactly MIXED_OK'
{"sessionID":"ses_c47daebe13eb447481656a532c870e8c","type":"turn_started"}
{"step":1,"text":"LOCAL_OK","type":"text"}
{"steps":1,"type":"turn_completed",...}
[exit=0]

$ $ISO OPENCODE_DB="$MIXED" \
    /config/.local/share/mise/installs/opencode/1.18.12/opencode \
    session list --format json
[
  {
    "id": "ses_c47daebe13eb447481656a532c870e8c",
    "title": "LOCAL_OK",
    ...
  },
  {
    "id": "ses_01a06210cffe1v3f0yfHTFhSNG",
    "title": "LOCAL_OK",
    ...
  }
]
[exit=0]
```

Raw throwaway database check:

```console
$ sqlite3 "$MIXED" 'select id, model from session order by time_created;'
ses_01a06210cffe1v3f0yfHTFhSNG|{"id":"f3-model","providerID":"localqa","variant":"default"}
ses_c47daebe13eb447481656a532c870e8c|{"id":"f3-model","providerID":"localqa"}
[exit=0]
```

Judgement: **PASS / blocker fixed**. The released binary exits 0 and lists the Rust-written session. Rust now writes `session.model.id`; optional `variant` is omitted without breaking TypeScript.

### 11. Export and import — PASS

#### 11a. Rust vs released export

Both binaries exported the same TypeScript-created session from the same explicit database:

```console
$ $ISO OPENCODE_DB="$DB" ./target/debug/opencode-rust \
    export ses_01a06210cffe1v3f0yfHTFhSNG > rust-export.json
Exporting session: ses_01a06210cffe1v3f0yfHTFhSNG
[exit=0, bytes=3814]

$ $ISO OPENCODE_DB="$DB" \
    /config/.local/share/mise/installs/opencode/1.18.12/opencode \
    export ses_01a06210cffe1v3f0yfHTFhSNG > ts-export.json
Exporting session: ses_01a06210cffe1v3f0yfHTFhSNG
[exit=0, bytes=3814]

$ cmp rust-export.json ts-export.json
rust-export.json ts-export.json differ: byte 20, line 3
[exit=1]

$ python3 canonical_compare.py
rust_canonical_bytes=2380
ts_canonical_bytes=2380
canonical_equal=True
[exit=0]
```

Judgement: **PASS / blocker fixed**. Export performs the real lookup and emits the same document as released TypeScript; only JSON object-key order differs.

#### 11b. Import round trip

I imported Rust's export into a new empty explicit database, listed it, then re-exported it:

```console
$ $ISO OPENCODE_DB="$EMPTY_DB" ./target/debug/opencode-rust import rust-export.json
Imported session: ses_01a06210cffe1v3f0yfHTFhSNG
[exit=0]

$ $ISO OPENCODE_DB="$EMPTY_DB" ./target/debug/opencode-rust \
    session list --all-projects --format json
[
  {
    "id": "ses_01a06210cffe1v3f0yfHTFhSNG",
    "title": "LOCAL_OK",
    "model":{"id":"f3-model","providerID":"localqa","variant":"default"},
    ...
  }
]
[exit=0]

$ $ISO OPENCODE_DB="$EMPTY_DB" ./target/debug/opencode-rust \
    export ses_01a06210cffe1v3f0yfHTFhSNG > reexport.json
Exporting session: ses_01a06210cffe1v3f0yfHTFhSNG
[exit=0]

$ python3 compare.py
canonical_equal=True
messages=2
[exit=0]
```

Judgement: **PASS**. The exported transcript survives a real empty-database import and canonical re-export unchanged.

### 12. Advertised implemented surface sweep — one BLOCKER

The following non-destructive advertised paths performed real work:

```console
$ $ISO ./target/debug/opencode-rust agent list
build (primary)
...
explore (subagent)
...
[exit=0]

$ $ISO ./target/debug/opencode-rust providers list
Credentials /tmp/opencode/f3-wave2-20260809/provider/data/opencode/auth.json
0 credentials
[exit=0]

$ $ISO ./target/debug/opencode-rust mcp list
MCP Servers
No MCP servers configured
Add servers with: opencode-rust mcp add
[exit=0]

$ $ISO OPENCODE_DB="$DB" ./target/debug/opencode-rust db --format json \
    'select id, title from session order by time_created'
[
  {"id":"ses_153b5e7ca90a4e998afdbd325ab049c3","title":"LOCAL_OK"}
]
[exit=0]

$ $ISO OPENCODE_DB="$DB" ./target/debug/opencode-rust db stats
database  /tmp/opencode/f3-wave2-20260809/sessions/throwaway.db
file      240.0 KiB  245760 bytes
...
table     rows
message   2
migration 38
part      2
session   1
TOTAL     44
...
[exit=0]

$ $ISO ./target/debug/opencode-rust debug paths
home       /tmp/opencode/f3-wave2-20260809/provider/home
data       /tmp/opencode/f3-wave2-20260809/provider/data/opencode
...
tmp        /tmp/opencode/f3-wave2-20260809/provider/tmp/opencode
[exit=0]
```

The intentionally rejected commands (`console`, `web`, `stats`, `github`, `pr`, `upgrade`, `uninstall`, and `generate`) all exited 1 with a command-specific explanation and replacement where applicable. That is honest rejection, not silent success.

#### 12a. `completion` is still advertised as operational and remains a stub — BLOCKER

Top-level help says:

```text
completion  Generate shell completion output
```

Its own help also promises an operational generator:

```console
$ $ISO ./target/debug/opencode-rust completion --help
Generate shell completion output

Usage: opencode-rust completion [OPTIONS] [ARGS]...

Arguments:
  [ARGS]...  Command-specific arguments
[exit=0]
```

Actual invocations:

```console
$ $ISO ./target/debug/opencode-rust completion bash > completion.bash
`completion` is not available: upstream's completion script is a yargs shell
function that calls back into `--get-yargs-completions`, a protocol this port
does not implement; generate completions from your shell against `--help` instead
[exit=1, output bytes=0]

$ $ISO ./target/debug/opencode-rust completion zsh
`completion` is not available: upstream's completion script is a yargs shell
function that calls back into `--get-yargs-completions`, a protocol this port
does not implement; generate completions from your shell against `--help` instead
[exit=1]

$ $ISO ./target/debug/opencode-rust completion fish
`completion` is not available: upstream's completion script is a yargs shell
function that calls back into `--get-yargs-completions`, a protocol this port
does not implement; generate completions from your shell against `--help` instead
[exit=1]

$ $ISO ./target/debug/opencode-rust --get-yargs-completions
error: unexpected argument '--get-yargs-completions' found
[exit=2]
```

Judgement: **BLOCKER**. A user following both top-level help and command help to generate shell completions cannot proceed: every plausible shell invocation produces zero bytes and exits 1, while the callback protocol named by the error is rejected. This is precisely the remaining “advertised as implemented but a stub” condition the round required me to find. The command must either generate completions or be honestly classified/described as unavailable rather than promising output.

### 13. Extra plausible user workflow — PASS

Beyond the assigned fixes, I exercised a normal operator sequence: start the top-level server, authenticate against `/api/health`, inspect database size/table/session stats, and create/delete a PTY. These all produced real state or real API responses, not 2xx-empty silent success. The obsolete pre-`/api` health route returned a structured `unimplemented_v1_route` response instead of being mistaken for the supported route; `/api/health` was the actual successful endpoint.

## Findings summary

| Severity | Finding |
|---|---|
| BLOCKER | `completion` is advertised twice as “Generate shell completion output”, but `completion`, `completion bash`, `completion zsh`, and `completion fish` all exit 1 and emit zero completion bytes; the callback protocol it cites is also rejected. |
| DEFECT | An unset `${VAR}` in `provider.*.options.baseURL` is silently reduced to an invalid relative URL; the eventual diagnostic does not name the missing variable. |

The two prior blockers are independently fixed: released TypeScript 1.18.12 reads and lists a Rust-written session, and Rust export is canonical-JSON identical to the released export. Import round-trip, config-only provider use, 401 secret handling, both SSE operations, TUI, foreground PTY input, session listing, preview prune, memory off/on, and the `OPENCODE_DB` escape hatch passed.

## Honest test gaps

- I did not run the prohibited ~100-minute memory gate or two-hour soak.
- I did not access or copy `/config/.local/share/opencode/*.db`; every database was created under the isolated QA root.
- I did not execute destructive `session prune --archive`, `--delete`, or `session delete`; the assignment allowed preview only.
- I did not invoke every one of the 58 server operations. I exercised auth, diagnostics, `/api/health`, both SSE routes, PTY create/list/delete, and the non-loopback gate.
- I did not test provider login/logout, MCP add/auth/logout, or agent creation because those intentionally mutate credentials/configuration; list/read paths were exercised.
- I did not run a browser client; this product sweep covered CLI, TUI, HTTP, SSE, and PTY paths.

## Cleanup

Only exact PIDs and exact tmux names created by this QA were stopped. Observed cleanup output:

```text
exact-pids=2950751:gone 2950752:gone 3138956:gone 3373721:gone
           3625341:gone 3659654:gone 3748009:gone
f3w2-tmux=absent
isolation-root=removed
State Recv-Q Send-Q Local Address:Port Peer Address:Port Process
```

The final `ss` output contained no listener on ports 40831–40840. The memory artifact lived inside the removed isolation root, never in the worktree. No source, test, or documentation file was modified.

## Final verification

```console
$ git status --short
?? F3-REPORT.md

$ git diff --stat
[no output]

$ git check-ignore -v target/debug/opencode-rust target/debug/oc-server
.gitignore:1:/target target/debug/opencode-rust
.gitignore:1:/target target/debug/oc-server
```

Judgement: the only worktree artifact is this required untracked report; build outputs are ignored and no tracked file changed. `lsp_diagnostics` was attempted on `F3-REPORT.md`, but the tool rejected the path because its fixed request cwd is the parent `/config/workspace/ProdDir/AI/opencode-rust`, outside the mandated `tF3` worktree. The deliverable is Markdown and no source file was changed, so there is no applicable source-language LSP result to report. I did not work around that guard by touching the forbidden parent worktree.

## Verdict

**REJECT** — both originally reported blockers are fixed, but the required advertised-command sweep found one remaining runtime blocker.

Exact blocker reproduction:

```sh
ROOT=/tmp/f3-completion-repro
mkdir -p "$ROOT"/{home,config,data,cache,state,tmp}
env -i \
  HOME="$ROOT/home" \
  XDG_CONFIG_HOME="$ROOT/config" \
  XDG_DATA_HOME="$ROOT/data" \
  XDG_CACHE_HOME="$ROOT/cache" \
  XDG_STATE_HOME="$ROOT/state" \
  TMPDIR="$ROOT/tmp" \
  PATH=/usr/bin:/bin TERM=xterm-256color \
  /config/workspace/ProdDir/AI/oc-wt/tF3/target/debug/opencode-rust \
  completion bash > "$ROOT/completion.bash"
rc=$?
wc -c "$ROOT/completion.bash"
printf 'exit=%s\n' "$rc"
```

Observed:

```text
`completion` is not available: upstream's completion script is a yargs shell
function that calls back into `--get-yargs-completions`, a protocol this port
does not implement; generate completions from your shell against `--help` instead
0 /tmp/f3-completion-repro/completion.bash
exit=1
```

F3 VERDICT: REJECT
