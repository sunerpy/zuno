# F3 Real Manual QA Report — Final Verification Wave 4

Date: 2026-08-10  
Worktree: `/config/workspace/ProdDir/AI/oc-wt/tF3`  
Branch / HEAD: `task-F3` / `55612823e3da7f025e174ccb28fa3c8a86c17fb1`  
Role: F3, Real Manual QA

## Bottom line

**REJECT.** All four previously reported blockers are fixed in real use: released TypeScript 1.18.12 reads a Rust-written session, export/import round-trips, `completion` is honestly presented in all four forms, and an HTTP turn is now readable through the pre-opened session SSE stream, `/message`, and `/history`.

The new permission broker works for its main path: a real `bash` tool call stops, appears in `GET /api/permission/request` and session SSE, accepts `reply: "once"`, executes, and resumes the same turn. However, the required fail-closed disconnect behavior does not work. After the sole session SSE client disconnected, the privileged request remained pending for at least **424 seconds**, and `/wait` did not return. The tool was not allowed—which is safer than fail-open—but the request was not rejected; I had to reconnect and manually submit `reply: "reject"` to unblock the turn. This violates the explicitly required disconnect boundary and strands unattended HTTP rounds.

The previously reported unset-variable diagnostic also remains a recoverable defect: `${F3_WAVE4_MISSING_BASE_URL}` still becomes an empty base URL and the error does not name the variable.

## Execution journal and isolation

- Runtime: `rustc 1.96.0 (ac68faa20 2026-05-25)`, `cargo 1.96.0 (30a34c682 2026-05-25)`.
- Binaries used: `target/debug/opencode-rust` and released `/config/.local/share/mise/installs/opencode/1.18.12/opencode`.
- Isolation root: `/tmp/opencode/f3-wave4-20260810-5561282`.
- Reserved ports: `42831` local provider, `42832` 401 provider, `42833` product server; cleanup checked `42831`–`42850`.
- Every product invocation used `env -i` with isolated `HOME`, `XDG_CONFIG_HOME`, `XDG_DATA_HOME`, `XDG_CACHE_HOME`, `XDG_STATE_HOME`, and `TMPDIR`.
- Every database was created under the isolation root. I did not read, copy, or mutate `/config/.local/share/opencode/*.db`.
- Only exact recorded PIDs and exact tmux session names were stopped. No broad `pkill` was used.
- References read before QA: Rust runtime, setup/journal, real manual QA, and cleanup/final-verification references from `@sunerpy/oh-my-openagent` 4.21.0.

The local OpenAI-compatible fixture deliberately distinguished title requests (no tools, response `F3_W4_TITLE`) from chat requests (tools present). This avoided the known false-negative where a title request consumes the chat response.

## 1. Offline build — PASS

Command:

```sh
cargo build --workspace --offline
```

Observed tail:

```text
Compiling oc-server v0.1.0 (.../tF3/crates/oc-server)
Compiling oc-tui v0.1.0 (.../tF3/crates/oc-tui)
Compiling oc-cli v0.1.0 (.../tF3/crates/oc-cli)
Finished `dev` profile [unoptimized + debuginfo] target(s) in 52.68s
```

Judgement: **PASS**. The real binaries used below were built from this HEAD offline.

## 2. Prior blocker: Rust write, released TypeScript read — PASS

Both turns used the same isolated explicit database and working local provider:

```sh
$ISO OPENCODE_DB="$DB" opencode run --model localqa/f3-model \
  --format json 'Reply with exactly SIDE_W4'
$ISO OPENCODE_DB="$DB" opencode-rust run --model localqa/f3-model \
  --format json 'Reply with exactly MIXED_W4'
$ISO OPENCODE_DB="$DB" opencode session list --format json
```

Observed:

```text
ts_run_exit=0 stdout_bytes=1027 stderr_bytes=0
rust_run_exit=0
rust_session_id=ses_2681c1a6d3e4405f942ab17e1f34f541
ts_list_sessions=2
ids=ses_2681c1a6d3e4405f942ab17e1f34f541,ses_015c2fbacffehtX2vNAWqj7vH2
titles=F3_W4_TITLE,F3_W4_TITLE
ts_list_exit=0 stderr_bytes=0
```

Judgement: **PASS / blocker remains fixed**. Released TypeScript 1.18.12 reads and lists the Rust-written session.

## 3. Prior blocker: export/import — PASS

Commands:

```sh
$ISO OPENCODE_DB="$DB" opencode-rust export "$RUST_SESSION" > rust-export.json
$ISO OPENCODE_DB="$DB" opencode export "$RUST_SESSION" > ts-export.json
$ISO OPENCODE_DB="$EMPTY_DB" opencode-rust import rust-export.json
$ISO OPENCODE_DB="$EMPTY_DB" opencode-rust export "$RUST_SESSION" > reexport.json
```

Observed:

```text
rust_export_bytes=2588 ts_export_bytes=2588 reexport_bytes=2588
rust_vs_released_canonical=True
roundtrip_canonical=True
roundtrip_messages=2
export_rust_exit=0 export_ts_exit=0 import_exit=0 reexport_exit=0
```

Judgement: **PASS / blocker remains fixed**. Rust and released exports are canonical-JSON identical, and import/re-export preserves the transcript.

## 4. Prior blocker: `completion` presentation — PASS

Top-level help now says:

```text
completion  Explain why shell completion output is unavailable, and what to use instead
```

I ran all four forms:

```sh
opencode-rust completion
opencode-rust completion bash
opencode-rust completion zsh
opencode-rust completion fish
```

Observed for every form:

```text
exit=1 stdout_bytes=0
`completion` is not available: upstream's completion script is a yargs shell function
that asks the binary back for candidates over `--get-yargs-completions`, a protocol
this port does not serve ... run `--help` ... instead
```

Judgement: **PASS / blocker remains resolved by honest presentation**. The command does not promise generated output and gives the same explicit reason and alternative for all requested forms.

## 5. Prior blocker: HTTP answer visibility — PASS

The real entry point was used, not standalone `oc-server`:

```sh
opencode-rust serve --hostname 127.0.0.1 --port 42833
```

After creating `ses_2df3346096fd41d699dcd15d6b207538`, I opened the session stream before submission:

```sh
curl -sS --max-time 8 -u "$AUTH" \
  "$BASE/api/session/$ID/event?after=0" > http-live-sse.txt &
curl -sS -u "$AUTH" -H 'content-type: application/json' \
  -X POST "$BASE/api/session/$ID/prompt" \
  --data '{"prompt":{"text":"HTTP_ROUNDTRIP"}}'
curl -sS -u "$AUTH" -H 'content-type: application/json' \
  -X POST "$BASE/api/session/$ID/wait" --data '{}'
curl -sS -u "$AUTH" "$BASE/api/session/$ID/message"
curl -sS -u "$AUTH" "$BASE/api/session/$ID/history"
```

Observed:

```text
prompt_http=200 wait_http=204
sse_exit=28 (intentional live-stream timeout) sse_bytes=2844
sse_contains_answer=True
sse_event_types=provider,turn.started,agent.resolved,model.resolved,
assistant.message.created,tool.snapshot.locked,provider.request.started,
provider,provider,assistant.checkpointed,step.completed,turn.completed
http-messages.json: bytes=1514 contains_answer=True
http-history.json: bytes=2786 contains_answer=True
message_data_count=2
history_data_count=12
```

The exact assistant text was `HTTP_W4_ASSISTANT_OK` on all three surfaces.

Judgement: **PASS / wave-3 blocker fixed**. The client can read the answer live and after completion.

## 6. Permission broker — main path PASS, disconnect BLOCKER

The isolated configuration set:

```json
"permission": { "bash": "ask" }
```

The provider requested a real tool call with:

```json
{"command":"printf PERMISSION_TOOL_EXECUTED_W4","intent":"F3 permission broker QA"}
```

### 6a. Stop, observe, reply, resume — PASS

After `POST /prompt`, the global pending endpoint returned:

```json
{
  "id": "per_14de2fd545de4b3b9671fbda55318b5d",
  "sessionID": "ses_6c5c71305501447090e82356672dcc92",
  "action": "bash",
  "resources": ["printf PERMISSION_TOOL_EXECUTED_W4"]
}
```

The stream opened before the prompt emitted:

```text
permission.v2.asked
tool.dispatch.started
```

Reply and wait:

```sh
curl -u "$AUTH" -H 'content-type: application/json' -X POST \
  "$BASE/api/session/$ID/permission/$REQUEST/reply" \
  --data '{"reply":"once"}'
curl -u "$AUTH" -H 'content-type: application/json' -X POST \
  "$BASE/api/session/$ID/wait" --data '{}'
```

Observed:

```text
reply_http=204 wait_http=204
permission.v2.replied reply=once
tool_status=completed
tool_output=PERMISSION_TOOL_EXECUTED_W4
message_has_answer=True (PERMISSION_RESUMED_OK)
pending_after=0
turn.completed
```

Judgement: **PASS**. This is a real paused and resumed tool round, not a stub or synthetic list item.

### 6b. Cross-session and malformed replies — PASS for rejection

Cross-session reply:

```http
HTTP 404
{"error":{"code":"not_found","message":"permission request `per_36d03159d8d94b59ace30d7117117159` is not pending for session `ses_6e7c4c5b2b564c118fd92b3c1c4e84d1`"}}
```

Malformed JSON and an invalid enum both returned:

```text
HTTP 400
{"error":{"code":"invalid_request","message":"reply body is invalid"}}
```

Judgement: **PASS** for scoped rejection and body validation. A separate clean request was used for the successful `once` path above.

### 6c. SSE client disconnect does not fail closed — BLOCKER

Exact exercised sequence:

```sh
ID=$(curl -sS -u "$AUTH" -H 'content-type: application/json' \
  -X POST "$BASE/api/session" --data '{}' | jq -r .data.id)

# This is the only live client observing the session.
curl -sS --max-time 2 -u "$AUTH" \
  "$BASE/api/session/$ID/event?after=0" > disconnect-sse.txt &
SSE_PID=$!

curl -sS -u "$AUTH" -H 'content-type: application/json' \
  -X POST "$BASE/api/session/$ID/prompt" \
  --data '{"prompt":{"text":"DISCONNECT_PERMISSION_W4"}}'

# Poll until permission.v2.asked appears, then let curl hit its timeout.
curl -sS -u "$AUTH" "$BASE/api/permission/request"
wait "$SSE_PID"  # curl exit 28; connection is gone

# Expected: no pending request and wait completes after automatic rejection.
# Actual: this wait never returned during the 120-second command budget.
curl -sS -u "$AUTH" -H 'content-type: application/json' \
  -X POST "$BASE/api/session/$ID/wait" --data '{}'
```

Observed connection result and stream tail:

```text
curl: (28) Operation timed out after 2003 milliseconds with 3675 bytes received
permission.v2.asked id=per_cc677b689b54447dbbf59a1d713ce7a6
tool.dispatch.started callID=call_f3_permission_wave4
```

After the command itself timed out, I checked again **424 seconds after session creation**:

```json
{
  "data": [{
    "id": "per_cc677b689b54447dbbf59a1d713ce7a6",
    "sessionID": "ses_7dcfc6665b874f7b8c162e79b993ed7e",
    "action": "bash",
    "resources": ["printf PERMISSION_TOOL_EXECUTED_W4"]
  }]
}
```

I then cleaned up the stranded round explicitly:

```text
POST reply {"reply":"reject"}: HTTP 204
POST wait: HTTP 204
tool_status=error
tool_error=tool bash was denied by the permission layer
tool_output_present=False
```

Judgement: **BLOCKER**. No privileged command ran, but disconnect did not reject the request as required. The round and `/wait` remain blocked indefinitely unless another client discovers and manually replies to the stale request.

## 7. Filesystem API boundary, including symlinks — PASS

The root contained `inside.txt`. Two distinctive outside secrets were reachable only through an outward file symlink and an outward directory symlink.

```text
GET /api/fs/read/inside.txt                         -> 200 SAFE_INSIDE_W4
GET --path-as-is /api/fs/read/../outside-secret.txt -> 403 path_escaped_root
GET --path-as-is /api/fs/read/%2e%2e%2foutside...   -> 403 path_escaped_root
GET --path-as-is /api/fs/read//etc/hostname         -> 403 path_escaped_root
GET /api/fs/read/link-secret                        -> 403 path_escaped_root
GET /api/fs/read/link-dir/nested-secret.txt         -> 403 path_escaped_root
GET /api/fs/list?path=.                             -> 200, only inside.txt
GET /api/fs/list?path=link-dir                      -> 403 path_escaped_root
GET /api/fs/find?query=inside                       -> 200, inside.txt
GET /api/fs/find?query=<outside-secret>             -> 200, empty data
outside secrets in captured responses               -> 0
```

Judgement: **PASS**. Traversal, absolute paths, and both requested symlink escape forms are blocked without disclosure.

## 8. PTY ticket and attach — PASS

I created a real PTY that printed `PTY_W4_OUTPUT_OK` and remained alive. Token minting without `x-opencode-ticket: 1` returned 403. With the header:

```text
create_http=200
ticket_http=200
ticket_len=36
expires_in=60
```

Fresh real WebSocket upgrade:

```text
HTTP/1.1 101 Switching Protocols
frame_opcode=2 payload=PTY_W4_OUTPUT_OK
```

The same ticket replay and a request without a ticket both returned:

```text
HTTP/1.1 403 Forbidden
{"error":{"code":"forbidden","message":"request is not authorized"}}
```

A separately minted ticket was left unused for `60 + 2` seconds and then returned the same 403. The distinctive `F3_PTY_SECRET_W4_N0T_LEAK_9XQ7` occurred zero times in the entire isolation tree. The PTYs were deleted with HTTP 204.

Judgement: **PASS**. Minting intent is explicit; fresh attach works; tickets are scoped, required, single-use, and expiring; the environment secret did not leak.

## 9. First contact, config-only provider, and session listing — PASS

Observed:

```text
opencode-rust --version             -> 1.18.13, exit 0
opencode-rust --help                -> 69 lines, exit 0
opencode-rust definitely-not-a-command
  -> error: unrecognized subcommand 'definitely-not-a-command', exit 2
opencode-rust --log-level NOPE
  -> invalid value; lists DEBUG/INFO/WARN/ERROR, exit 2
```

With an empty cache, no model-path injection, external model fetch disabled, and only the isolated config block:

```text
opencode-rust models -> localqa/f3-model, exit 0
```

The real local-provider turns throughout this report exited 0 and used that model. `session list --all-projects --format json` against the isolated mixed database returned two sessions and both versions `1.18.12,1.18.13`.

Judgement: **PASS**.

## 10. Provider errors and credential secrecy — one DEFECT

All cases were real `run --format json` invocations.

Dead port:

```text
transient provider failure (status=None): error sending request for url
(http://127.0.0.1:42999/v1/chat/completions): client error (Connect):
tcp connect error: Connection refused (os error 111)
[exit=1]
```

Misspelled host:

```text
transient provider failure (status=None): error sending request for url
(http://f3-wave4-does-not-exist.invalid/v1/chat/completions): client error
(Connect): dns error: failed to lookup address information: Name or service not known
[exit=1]
```

Unset variable, with `F3_WAVE4_MISSING_BASE_URL` absent from `env -i`:

```text
transient provider failure (status=None): builder error: relative URL without a base
[exit=1]
missing_names_variable=no
```

Judgement: **DEFECT**. The message still does not identify `${F3_WAVE4_MISSING_BASE_URL}`. The user can inspect and repair config, so this is recoverable.

HTTP 401, with the real key present only as environment variable `F3_WAVE4_401_KEY`:

```text
authentication rejected by provider failureqa: provider `failureqa` returned HTTP 401:
{"error": {"message": "unauthorized by F3 wave-4 fixture", "type": "authentication_error"}};
set `provider.failureqa.options.apiKey`, or run `opencode auth login failureqa`
[exit=1]
```

The distinctive key `sk-F3-WAVE4-UNIQUE-SECRET-9xQ7` occurred zero times in the complete isolation tree. Judgement: **PASS** for actionable placement guidance and secrecy.

## 11. The ten declared HTTP gaps — PASS as honest gaps

I invoked all ten current explicit gaps. Every one returned `503` and named its exact method/template:

```text
DELETE /api/credential/{credentialID}
PATCH  /api/credential/{credentialID}
DELETE /api/integration/attempt/{attemptID}
GET    /api/integration/attempt/{attemptID}
POST   /api/integration/attempt/{attemptID}/complete
POST   /api/integration/{integrationID}/connect/key
POST   /api/integration/{integrationID}/connect/oauth
GET    /api/session/{sessionID}/message/{messageID}
POST   /api/session/{sessionID}/permission
GET    /api/session/{sessionID}/permission/{requestID}
```

Representative result:

```json
{"error":{"code":"backend_unavailable","message":"backend unavailable for GET /api/session/{sessionID}/message/{messageID}"}}
```

Judgement: **PASS as declared compatibility gaps**, not new findings.

## 12. TUI, prune preview, memory switch, and parallel release use — PASS

### Real TUI turn

Launched in tmux at 120x35 with isolated XDG paths. I typed `TUI_W4` and pressed Enter:

```text
> You
  TUI_W4

* Assistant
  TUI_W4_OK

 idle
▏
```

After `Ctrl-C`:

```text
pane_dead=1 pane_dead_status=0
```

### Prune preview only

```sh
opencode-rust session prune --older-than 0 --all-projects \
  --include-recent --format json
```

```text
action=preview selected=2 changed_sessions=0 warnings=[]
before=722aa0e2b8479647aa06e16b60a4aa3791a04feadaf76a18061fbaa99258c988
after =722aa0e2b8479647aa06e16b60a4aa3791a04feadaf76a18061fbaa99258c988
byte_identical=yes
```

### Memory switch, actual provider request

```text
memory:false tools=invalid,bash,read,glob,grep,edit,write,webfetch,todowrite
memory:true  tools=invalid,bash,read,glob,grep,edit,write,webfetch,todowrite,memory
both turns exit=0
```

### Rust and released TypeScript in parallel

Both processes wrote real local-provider turns concurrently to a throwaway copy selected by `OPENCODE_DB`:

```text
rust_pid=1585069 ts_pid=1585070
rust_exit=0 rust_stderr_bytes=0
ts_exit=0 ts_stderr_bytes=0
released_list_sessions=4
released_list_exit=0 list_stderr_bytes=0
```

Judgement: **PASS**. TUI rendered a real answer and exited cleanly, preview was byte-stable, the memory tool boundary switched correctly, and the explicit database escape hatch survived overlapping Rust/released use.

## Findings summary

| Severity | Finding |
|---|---|
| **BLOCKER** | Disconnecting the only live session SSE client while a permission request is pending does not fail closed. The request remained pending for at least 424 seconds and `/wait` remained blocked until a second client manually sent `reply: "reject"`. |
| **DEFECT** | An unset `${VAR}` in `provider.*.options.baseURL` still yields only `relative URL without a base`; it does not name the missing variable. |

No filesystem or PTY secret leak was found. Cross-session permission replies, malformed reply bodies, the ten declared gaps, first contact, config-only provider use, 401 guidance/redaction, TUI, prune preview, memory switching, and parallel released use behaved as documented in the exercised cases.

## Honest test gaps

- I did not run the prohibited ~100-minute memory gate or two-hour soak.
- I did not access, copy, or mutate the 62 GB user database or its pinned performance backup.
- I did not execute destructive prune/archive/delete; prune was preview-only.
- I exercised the permission broker end-to-end but did not separately generate a `question` tool call and drive question reply/reject.
- I tested PTY server-to-client output frames, ticket replay/expiry/no-ticket denial, and deletion, but not a client-to-server PTY input frame in this wave.
- I did not test every cursor, pagination, workspace, or malformed permutation of the implemented HTTP operations.
- I did not repeat all prior wave-3 catalog and no-model-TUI probes; this wave concentrated on the specified regressions and new permission/security surfaces.
- I did not rerun the full 3,319-test suite supplied as the main baseline; I performed the required real manual QA and an offline workspace build.

## Cleanup and worktree scope

Only exact recorded resources were stopped:

```text
permission-server.pid=980596:gone
provider.pid=1031541:gone
provider-401.pid=1527475:gone
product-server.pid=881532:gone
tmux_f3w4_tui=absent
tmux_f3w4_pty=absent
ports 42831-42850: no listeners
isolation_root=removed
```

Final scope check before report verification:

```text
git status --short: ?? F3-REPORT.md
git diff --stat: no tracked diff
target/debug/opencode-rust: ignored by .gitignore:1:/target
target/debug/oc-server: ignored by .gitignore:1:/target
```

No source, test, documentation, user configuration, or user database was modified.

## Verdict and exact blocker reproduction

**REJECT** — the four historical blockers are fixed, but the new broker does not perform the required fail-closed rejection when its client disconnects.

Use an isolated config with a working model and `"permission":{"bash":"ask"}`. Make the model call any valid permission-gated tool, then run:

```sh
BASE=http://127.0.0.1:42833
AUTH=f3user:f3-wave4-password

ID=$(curl -sS -u "$AUTH" -H 'content-type: application/json' \
  -X POST "$BASE/api/session" --data '{}' | jq -r .data.id)

curl -sS --max-time 2 -u "$AUTH" \
  "$BASE/api/session/$ID/event?after=0" > session-sse.txt &
SSE_PID=$!

curl -sS -u "$AUTH" -H 'content-type: application/json' \
  -X POST "$BASE/api/session/$ID/prompt" \
  --data '{"prompt":{"text":"Call the permission-gated tool now"}}'

# Wait until GET /api/permission/request lists the request, then disconnect.
wait "$SSE_PID" || true

# Actual after disconnect: the request is still listed indefinitely.
curl -sS -u "$AUTH" "$BASE/api/permission/request"

# Actual: hangs instead of completing after automatic rejection.
curl -sS --max-time 10 -u "$AUTH" -H 'content-type: application/json' \
  -X POST "$BASE/api/session/$ID/wait" --data '{}'
```

Observed here: SSE exit 28 after 2.003 seconds, the same request remained pending after 424 seconds, and `/wait` exceeded the 120-second command budget. Manual `reply: "reject"` returned 204 and immediately allowed `/wait` to return 204.

F3 VERDICT: REJECT
