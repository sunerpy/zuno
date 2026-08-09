# F3 Real Manual QA Report — Final Verification Wave 3

Date: 2026-08-09  
Worktree: `/config/workspace/ProdDir/AI/oc-wt/tF3`  
Branch / HEAD: `task-F3` / `8628937ab3ee79b8208a6b5610837cc26ac93ce2`  
Role: F3, Real Manual QA

## Bottom line

**REJECT.** The two original blockers and the wave-2 `completion` presentation issue are fixed. The new HTTP prompt path does execute the real provider turn and writes `HTTP_ASSISTANT_OK` into the canonical database, but the assistant output is invisible through every exercised HTTP read/stream path: live session SSE emits zero bytes, live global SSE emits only `server.connected`, and post-turn `/message` and `/history` return empty arrays. An HTTP client receives only prompt admission plus `204` from `/wait`, so it cannot obtain the answer it requested.

Additional recoverable defects remain: direct `oc-server serve` still exposes `/prompt` as `503` while `opencode-rust serve` implements it; a new HTTP session selected `localqa/f3-alt` despite config declaring `localqa/f3-model`; HTTP `/api/model` and `/api/provider` returned empty catalogs for a config-only provider that the CLI listed; unset `${VAR}` errors still do not name the variable; and no-model TUI startup reports failure but exits 0.

## Execution journal and isolation

- Runtime: `rustc 1.96.0 (ac68faa20 2026-05-25)`, `cargo 1.96.0 (30a34c682 2026-05-25)`.
- Binaries exercised: `target/debug/opencode-rust`, `target/debug/oc-server`, and released TypeScript `/config/.local/share/mise/installs/opencode/1.18.12/opencode`.
- Main isolation root: `/tmp/opencode/f3-wave3-20260809-kiro-8628937`; supplemental catalog root: `/tmp/opencode/f3-wave3-catalog-8628937`.
- Reserved ports: `41831` local provider, `41832` 401 provider, `41833` product server, `41835` catalog sweep.
- Every product invocation used `env -i` with isolated `HOME`, `XDG_CONFIG_HOME`, `XDG_DATA_HOME`, `XDG_CACHE_HOME`, `XDG_STATE_HOME`, and `TMPDIR`.
- Every database was created under an isolation root or copied from another isolated database. I did not read, copy, or mutate `/config/.local/share/opencode/*.db`.
- References read before QA: Rust runtime, setup/journal, real manual QA, and cleanup/final-verification references from `@sunerpy/oh-my-openagent` 4.21.0.

## 1. Build — PASS

Command:

```sh
cargo build --workspace --offline
```

Observed tail:

```text
Compiling oc-server v0.1.0 (.../tF3/crates/oc-server)
Compiling oc-tui v0.1.0 (.../tF3/crates/oc-tui)
Compiling oc-cli v0.1.0 (.../tF3/crates/oc-cli)
Finished `dev` profile [unoptimized + debuginfo] target(s) in 31.91s
```

Judgement: **PASS**. The real dev binaries used below were built from this HEAD, offline.

## 2. First contact and TUI — one DEFECT

Representative isolation envelope:

```sh
env -i \
  HOME=/tmp/opencode/f3-wave3-20260809-kiro-8628937/first/home \
  XDG_CONFIG_HOME=/tmp/opencode/f3-wave3-20260809-kiro-8628937/first/config \
  XDG_DATA_HOME=/tmp/opencode/f3-wave3-20260809-kiro-8628937/first/data \
  XDG_CACHE_HOME=/tmp/opencode/f3-wave3-20260809-kiro-8628937/first/cache \
  XDG_STATE_HOME=/tmp/opencode/f3-wave3-20260809-kiro-8628937/first/state \
  TMPDIR=/tmp/opencode/f3-wave3-20260809-kiro-8628937/first/tmp \
  PATH=/usr/bin:/bin TERM=xterm-256color \
  ./target/debug/opencode-rust ...
```

Observed:

```console
$ opencode-rust --version
1.18.13
[exit=0]

$ opencode-rust --version --long
opencode-rust 0.1.0 (Rust package 0.1.0; plugin compatibility 1.18.13)
[exit=0]

$ opencode-rust definitely-not-a-command
error: unrecognized subcommand 'definitely-not-a-command'
Usage: opencode-rust [OPTIONS] [COMMAND]
[exit=2]

$ opencode-rust --definitely-not-a-flag
error: unexpected argument '--definitely-not-a-flag' found
[exit=2]

$ opencode-rust --log-level NOPE
error: invalid value 'NOPE' for '--log-level <LOG_LEVEL>'
  [possible values: DEBUG, INFO, WARN, ERROR]
[exit=2]
```

`--help` exited 0, listed the implemented/rejected command surface, and retained the explicit danger explanation for `--auto`.

### Configured real TUI turn — PASS

I launched the configured TUI in tmux at 120x35, typed `HTTP_ROUNDTRIP`, and pressed Enter. The actual frame became:

```text
> You
  HTTP_ROUNDTRIP

* Assistant
  HTTP_ASSISTANT_OK

 idle
▏
```

After `Ctrl-C`:

```text
pane_dead=1 pane_dead_status=0
```

Judgement: **PASS**. The TUI used the real local provider, rendered the answer, and restored the terminal with exit 0.

### Empty first launch — DEFECT

Exact empty-config TUI result:

```text
no available model; configure a provider credential or provider block
F3_TUI_EXIT=0
```

Judgement: **DEFECT**. The message is actionable, but startup failed to enter the TUI and still returned success. The same scenario returned exit 1 in the earlier F3 round. Scripts and launchers cannot distinguish this failure from a clean interactive exit.

## 3. Original blocker: Rust write then released TypeScript read — PASS

Using one isolated explicit `OPENCODE_DB`, released TypeScript first wrote a real session, then Rust wrote a real session to that same database:

```sh
OPENCODE_DB="$DB" opencode run --model localqa/f3-model --format json \
  'Reply with exactly SIDE_OK'
OPENCODE_DB="$DB" opencode-rust run --model localqa/f3-model --format json \
  'Reply with exactly MIXED_OK'
OPENCODE_DB="$DB" opencode session list --format json
```

Observed final command:

```json
[
  {
    "id": "ses_35c21bc3e3964084860f630eb729ade5",
    "title": "F3_W3_TITLE",
    "directory": "/config/workspace/ProdDir/AI/oc-wt/tF3"
  },
  {
    "id": "ses_0186902deffeEywAJ6tAFjx1QZ",
    "title": "F3_W3_TITLE",
    "directory": "/config/workspace/ProdDir/AI/oc-wt/tF3"
  }
]
[exit=0]
```

Judgement: **PASS / prior blocker remains fixed**. Released TypeScript 1.18.12 reads and lists the Rust-written session.

## 4. Original blocker: export/import round trip — PASS

Commands:

```sh
OPENCODE_DB="$SOURCE_DB" opencode-rust export \
  ses_35c21bc3e3964084860f630eb729ade5 > rust-export.json
OPENCODE_DB="$EMPTY_DB" opencode-rust import rust-export.json
OPENCODE_DB="$EMPTY_DB" opencode-rust session list --all-projects --format json
OPENCODE_DB="$EMPTY_DB" opencode-rust export \
  ses_35c21bc3e3964084860f630eb729ade5 > reexport.json
```

Observed:

```text
Exporting session: ses_35c21bc3e3964084860f630eb729ade5
2578 rust-export.json
[exit=0]
Imported session: ses_35c21bc3e3964084860f630eb729ade5
[exit=0]
canonical_equal=True
messages=2
reexport_exit=0 byte_equal=yes
```

Judgement: **PASS / prior blocker remains fixed**. A real export imported into a new database, listed correctly, and re-exported byte-for-byte identically.

## 5. `completion` after todo 125 — PASS

Top-level help now says:

```text
completion  Explain why shell completion output is unavailable, and what to use instead
```

`completion --help` says it cannot emit a working yargs callback script, explains `--get-yargs-completions`, and directs users to command help. I ran all four requested forms:

```console
$ opencode-rust completion
`completion` is not available: upstream's completion script is a yargs shell
function that asks the binary back for candidates over `--get-yargs-completions`,
a protocol this port does not serve ...
stdout_bytes=0 exit=1

$ opencode-rust completion bash
stdout_bytes=0 exit=1

$ opencode-rust completion zsh
stdout_bytes=0 exit=1

$ opencode-rust completion fish
stdout_bytes=0 exit=1
```

All four printed the same explicit reason on stderr.

Judgement: **PASS / wave-2 blocker resolved by honest presentation**. The behavior is deliberately unavailable, but neither top-level nor command help now promises generated output.

## 6. HTTP prompt round — BLOCKER

### Launcher discrepancy — DEFECT

With identical isolated configuration, authentication, database, and working directory, direct server launch:

```sh
target/debug/oc-server serve --hostname 127.0.0.1 --port 41833
```

accepted session creation but returned:

```http
POST /api/session/ses_0d753c1f5b5e48e784498ce99e34a721/prompt
HTTP/1.1 503 Service Unavailable

{"error":{"code":"backend_unavailable","message":"backend unavailable for POST /api/session/{sessionID}/prompt"}}
```

Restarting through the top-level launcher:

```sh
target/debug/opencode-rust serve --hostname 127.0.0.1 --port 41833
```

made the same route operational. Judgement: **DEFECT**. The direct binary still advertises the route but lacks the mutation backend; using `opencode-rust serve` is the workaround.

### Real provider execution and admission — PASS

I created a session and submitted the exact payload shape accepted by the route:

```sh
curl -u f3user:f3-http-password \
  -H 'content-type: application/json' \
  -X POST "$BASE/api/session" --data '{}'

curl -u f3user:f3-http-password \
  -H 'content-type: application/json' \
  -X POST "$BASE/api/session/$ID/prompt" \
  --data '{"prompt":{"text":"HTTP_ROUNDTRIP"}}'

curl -u f3user:f3-http-password \
  -H 'content-type: application/json' \
  -X POST "$BASE/api/session/$ID/wait" --data '{}'
```

Observed:

```http
HTTP/1.1 200 OK

{"data":{"admittedSeq":0,"id":"msg_3cd07520ec6741189b7510a5f61a3201",
"sessionID":"ses_b0d04729d5f8418d88ee79628368d376",
"prompt":{"text":"HTTP_ROUNDTRIP","files":[],"agents":[]},
"delivery":"steer",...}}

HTTP/1.1 204 No Content
```

The local OpenAI-compatible fixture recorded the actual main request:

```json
{"messages":[{"content":"","role":"system"},{"content":"HTTP_ROUNDTRIP","role":"user"}],
 "model":"f3-alt","path":"/v1/chat/completions","stream":true,...}
```

The fixture streamed `HTTP_ASSISTANT_OK`. Direct read-back from the isolated canonical database showed:

```json
{"role":"assistant","finish":"stop",...}
{"text":"HTTP_ASSISTANT_OK","type":"text"}
```

Judgement: **PASS for execution and canonical persistence**. This is not a stub or a zero-work `200`: the provider ran and the answer was stored.

### The answer is inaccessible over HTTP — BLOCKER

I tested both read-after-completion and streams opened before submission.

Read-after-completion:

```console
$ curl -u f3user:f3-http-password "$BASE/api/session/$ID/message"
{"data":[],"cursor":{}}

$ curl -u f3user:f3-http-password "$BASE/api/session/$ID/history"
{"data":[],"hasMore":false}

$ curl --max-time 1 -u f3user:f3-http-password \
    "$BASE/api/session/$ID/event?after=0"
HTTP/1.1 200 OK
content-type: text/event-stream
curl: (28) Operation timed out ... with 0 bytes received
```

Live session stream opened before the prompt:

```sh
curl --max-time 5 -u f3user:f3-http-password \
  "$BASE/api/session/$ID/event?after=0" > live-sse.txt &
# POST prompt, then POST wait
```

Observed:

```text
prompt_http=200
wait_http=204
sse_exit=28
live-sse.txt: 0 bytes
post-turn /message: {"data":[],"cursor":{}}
post-turn /history: {"data":[],"hasMore":false}
```

I repeated with global `/api/event` opened before the prompt. It emitted only:

```text
data: {"data":{},"id":"evt_019fe7b8262f7203b0c7d252a497fd97","type":"server.connected"}
```

No prompt, text, completion, or error event followed.

Judgement: **BLOCKER**. The HTTP client only receives admission and idle completion. It cannot read the assistant answer live or afterward, even though the answer exists in the database. This is a silent successful-looking failure at the user boundary and prevents an HTTP-driven conversation from continuing.

### Configured default model ignored — DEFECT

The isolated config declared:

```json
"model": "localqa/f3-model"
```

The first HTTP prompt's actual provider request used:

```json
"model": "f3-alt"
```

Judgement: **DEFECT**. The alternate model sorted into the same configured provider was selected instead of the declared default. Calling `/api/session/{id}/model` before prompting is a workaround.

## 7. HTTP session mutations — PASS for exercised paths

### Agent/model switch and context marker

Commands:

```sh
curl -u f3user:f3-http-password -H 'content-type: application/json' \
  -X POST "$BASE/api/session/$ID/agent" --data '{"agent":"explore"}'
curl -u f3user:f3-http-password -H 'content-type: application/json' \
  -X POST "$BASE/api/session/$ID/model" \
  --data '{"model":{"id":"f3-model","providerID":"localqa"}}'
curl -u f3user:f3-http-password "$BASE/api/session/$ID/context"
```

Observed:

```text
agent switch: HTTP/1.1 204 No Content
model switch: HTTP/1.1 204 No Content
```

```json
{"data":[
  {"agent":"explore",...,"type":"agent-switched"},
  {"model":{"id":"f3-model","providerID":"localqa"},...,"type":"model-switched"}
]}
```

Judgement: **PASS**. The task-129 markers are visible in `/context`, and the next actual provider request used `f3-model` with the `explore` system prompt.

### Interrupt and wait

I submitted a fixture response that emitted `SLOW_STARTED`, slept eight seconds, then would emit `SLOW_FINISHED`. One second after admission I called `/interrupt`, then `/wait`.

```text
prompt: HTTP 200
interrupt: HTTP 204
wait: HTTP 204
```

Database read-back of the actual assistant message:

```json
{"error":{"data":{"message":"The operation was aborted."},"name":"AbortError"},...}
{"text":"SLOW_STARTED","type":"text"}
```

`SLOW_FINISHED` was absent. Judgement: **PASS**. Interrupt cancelled the active turn and wait returned only after the run became idle.

### Compact and revert

Calling compact with only two turns returned a specific non-compactable-history error:

```http
HTTP/1.1 500 Internal Server Error
{"error":{"code":"mutation_failed","message":"manual compaction failed: Reason(\"NoCompactableHistory: session has no compactable history before the preserved tail\")"}}
```

After six more real prompt/wait pairs, the same route returned `204 No Content`. `revert/stage`, `revert/clear`, a second stage, and `revert/commit` returned `200`, `204`, `200`, and `204`; stage returned the exact requested message ID.

Judgement: **PASS** for the successful compactable case, explicit too-short rejection, and the exercised no-file revert lifecycle.

## 8. Filesystem API security boundary — PASS

The server root contained `inside.txt`. A distinctive secret lived outside it, with both a file symlink and a directory symlink inside the root pointing outward.

```console
$ GET /api/fs/read/inside.txt
HTTP/1.1 200 OK
SAFE_INSIDE_F3

$ GET --path-as-is /api/fs/read/../outside-secret.txt
HTTP/1.1 403 Forbidden
{"error":{"code":"path_escaped_root","message":"the requested path leaves the session directory"}}

$ GET --path-as-is /api/fs/read/%2e%2e%2foutside-secret.txt
HTTP/1.1 403 Forbidden

$ GET --path-as-is /api/fs/read//etc/hostname
HTTP/1.1 403 Forbidden

$ GET /api/fs/read/link-secret
HTTP/1.1 403 Forbidden

$ GET /api/fs/read/link-dir/nested-secret.txt
HTTP/1.1 403 Forbidden
```

Additional probes:

```text
GET /api/fs/list?path=.          -> 200, only inside.txt
GET /api/fs/list?path=link-dir   -> 403 path_escaped_root
GET /api/fs/find?query=inside    -> 200, inside.txt
GET /api/fs/find?query=<secret>  -> 200, empty data
captured-response secret scan    -> secret_leak=no
```

Judgement: **PASS**. Both symlink escape forms are blocked, as are traversal and absolute paths; the outside secret did not appear in any response.

## 9. PTY connect ticket — PASS

I created a real PTY that printed `PTY_OUTPUT_OK` and remained alive. Calling `connect-token` without the explicit ticket intent header returned `403`. With `x-opencode-ticket: 1`:

```json
{"location":{"directory":".../http/project",...},
 "data":{"ticket":"<36-character ticket>","expires_in":60}}
```

I performed a real WebSocket upgrade with Basic auth and the ticket:

```text
HTTP/1.1 101 Switching Protocols
frame_opcode=2 payload=PTY_OUTPUT_OKn
```

The same ticket was immediately replayed:

```text
HTTP/1.1 403 Forbidden
```

A second ticket was left unused for `60 + 2` seconds:

```text
HTTP/1.1 403 Forbidden
{"error":{"code":"forbidden","message":"request is not authorized"}}
```

No-ticket connect also returned the same 403. A recursive scan of the whole isolation root for `F3_PTY_SECRET_N0T_LEAK_8KX2` found zero files. The PTY was deleted with HTTP 204.

Judgement: **PASS**. Tickets are required, single-use, expire at the advertised TTL, and neither ticket failures nor artifacts exposed the distinctive environment secret.

## 10. Provider diagnostics and credential secrecy — one DEFECT

All cases used isolated configs and real `run --format json` invocations.

### Dead port — PASS

```text
transient provider failure (status=None): error sending request for url
(http://127.0.0.1:41999/v1/chat/completions): client error (Connect):
tcp connect error: Connection refused (os error 111)
[exit=1]
```

### Misspelled hostname — PASS

```text
transient provider failure (status=None): error sending request for url
(http://f3-wave3-does-not-exist.invalid/v1/chat/completions): client error
(Connect): dns error: failed to lookup address information: Name or service not known
[exit=1]
```

### Unset `${VAR}` — DEFECT

Config used `"baseURL":"${F3_WAVE3_MISSING_BASE_URL}"`, and that variable was absent from `env -i`:

```text
transient provider failure (status=None): builder error: relative URL without a base
[exit=1]
```

Judgement: **DEFECT**. The earlier defect is unchanged: the diagnostic does not name `F3_WAVE3_MISSING_BASE_URL`.

### HTTP 401 — PASS

The real key existed only in the environment as `F3_WAVE3_401_KEY=sk-F3-WAVE3-UNIQUE-SECRET-9xQ7`:

```text
authentication rejected by provider failureqa: provider `failureqa` returned HTTP 401:
{"error": {"message": "unauthorized by F3 wave-3 fixture", "type": "authentication_error"}};
set `provider.failureqa.options.apiKey`, or run `opencode auth login failureqa`
[exit=1]
```

The complete isolated auth tree contained zero occurrences of the distinctive key. Judgement: **PASS**. Both valid key-placement actions are named and the key is never echoed or persisted.

## 11. Catalogs and request-state reads — one DEFECT

With external model fetch disabled and a provider/model defined only in isolated config:

```console
$ opencode-rust models
localqa/f3-alt
localqa/f3-model
[exit=0]
```

A separate no-network HTTP catalog sweep observed:

```text
agent                  http=200 body=list[7]
model                  http=200 body=list[0]
command                http=200 body=list[2]
skill                  http=200 body=list[1]
reference              http=200 body=list[0]
provider               http=200 body=list[0]
integration            http=200 body=list[2]
permission/request     http=200 body=list[0]
permission/saved       http=200 body=list[0]
question/request       http=200 body=list[0]
```

Judgement: **DEFECT** for `/api/model` and `/api/provider`. The config-only provider/model is usable and visible through the CLI but missing from the implemented HTTP catalogs. Empty request-state lists and empty references are valid for this clean isolation.

## 12. Session list, prune preview, memory switch, and parallel release use — PASS

### List and preview prune

`session list --all-projects --format json` returned both isolated 1.18.12 and 1.18.13 sessions. Preview command:

```sh
OPENCODE_DB="$DB" opencode-rust session prune \
  --older-than 0 --all-projects --include-recent --format json
```

Observed:

```json
{"action":"preview",
 "selected_session_ids":["ses_0186902deffeEywAJ6tAFjx1QZ","ses_35c21bc3e3964084860f630eb729ade5"],
 "changed_sessions":0,"warnings":[]}
```

```text
before_sha256=9f23ce0f3b1c858bcfe4dff7f324cd62ebfa1ceb7741b9b6290976160e7eb4be
after_sha256=9f23ce0f3b1c858bcfe4dff7f324cd62ebfa1ceb7741b9b6290976160e7eb4be
byte_identical=yes
```

### Memory switch both ways

Actual turn snapshots:

```text
memory: false -> toolIDs=[invalid,bash,read,glob,grep,edit,write,webfetch,todowrite]
                 text=MEMORY_OFF_OK, exit=0
memory: true  -> toolIDs=[invalid,bash,read,glob,grep,edit,write,webfetch,todowrite,memory]
                 text=LOCAL_OK, exit=0
```

Judgement: **PASS for the switch boundary exercised here**. The tool is absent when disabled and present when enabled. This round did not repeat the prior round's actual memory-write/recall sequence.

### Rust and released TypeScript in parallel with `OPENCODE_DB`

I copied the isolated mixed database, then launched one real Rust turn and one real released-TypeScript turn concurrently against the same explicit file:

```text
rust_pid=1941714 ts_pid=1941715
rust_exit=0 ts_exit=0
Rust stderr: empty
TS stderr: empty
```

Released TypeScript then listed four sessions, including both concurrent writes, and exited 0. Rust read the same explicit file:

```text
sessions=4
versions=1.18.12,1.18.13
```

Judgement: **PASS**. The explicit `OPENCODE_DB` escape hatch worked under actual overlapping use; no lock error or unreadable record appeared.

## 13. Known 14 explicit gaps — PASS as declared gaps

I invoked all 14 operations marked as explicit gaps in the generated compatibility matrix. Every one returned operation-specific `503 backend_unavailable` and named its exact method/path:

```text
DELETE/PATCH /api/credential/{credentialID}
DELETE/GET /api/integration/attempt/{attemptID}
POST /api/integration/attempt/{attemptID}/complete
POST /api/integration/{integrationID}/connect/key
POST /api/integration/{integrationID}/connect/oauth
GET /api/session/{sessionID}/message/{messageID}
GET/POST /api/session/{sessionID}/permission
GET /api/session/{sessionID}/permission/{requestID}
POST /api/session/{sessionID}/permission/{requestID}/reply
POST /api/session/{sessionID}/question/{requestID}/reply
POST /api/session/{sessionID}/question/{requestID}/reject
```

Representative body:

```json
{"error":{"code":"backend_unavailable",
"message":"backend unavailable for GET /api/session/{sessionID}/message/{messageID}"}}
```

Judgement: **PASS as an honest compatibility gap**, not a new finding.

## Findings summary

| Severity | Finding |
|---|---|
| BLOCKER | A real HTTP `/prompt` turn executes and persists `HTTP_ASSISTANT_OK`, but session SSE, global SSE, `/message`, and `/history` expose none of it. The client only receives admission and `wait` 204, so an HTTP-driven conversation cannot obtain its assistant response. |
| DEFECT | Direct `oc-server serve` leaves `/api/session/{id}/prompt` at operation-specific 503; only `opencode-rust serve` wires the real turn backend. |
| DEFECT | A new HTTP session used `localqa/f3-alt` although config declared `localqa/f3-model`; `/model` switching is a workaround. |
| DEFECT | HTTP `/api/model` and `/api/provider` return empty arrays for a config-only provider/model that the CLI lists and can use. |
| DEFECT | Unset `${VAR}` provider URLs still fail as a generic relative URL and do not name the missing variable. |
| DEFECT | Empty-config TUI startup prints an actionable no-model failure but exits 0. |

## Honest test gaps

- I did not run the prohibited ~100-minute memory gate or two-hour soak.
- I did not access, copy, or mutate the 62 GB user database or its pinned performance backup.
- I did not run destructive prune, archive, delete, or session deletion; prune was preview-only.
- I tested memory tool exposure in both states, but did not repeat the prior round's real memory-write and cross-session recall sequence.
- I did not test a real external provider or external network. All model turns used a local OpenAI-compatible streaming fixture, with model fetch explicitly disabled.
- I did not test PTY client-to-server input frames; I tested ticket minting, real upgrade/output replay, immediate ticket replay, expiry, no-ticket denial, deletion, and secret leakage.
- I exercised all advertised 14 gaps, all requested catalogs/request-state lists, the requested session reads/mutations, filesystem boundaries, PTY attach, CLI/TUI, and compatibility flows, but not every query/cursor/error permutation of the 44 implemented HTTP operations.

## Cleanup

Only exact recorded PIDs and exact tmux names were stopped:

```text
pid=1742798 gone  # local provider
pid=1923673 gone  # 401 provider
pid=1675975 gone  # product server
pid=1977679 gone  # supplemental catalog server
f3w3-tui=absent
f3w3-pty=absent
ports 41831-41840: no listeners
isolation_root=removed
catalog_root=removed
```

No source, test, documentation, user configuration, or user database was modified. The only intended worktree artifact is this uncommitted report.

Final worktree check:

```text
git status --short: ?? F3-REPORT.md
git diff --stat: no tracked diff
ports 41831-41840: no listeners
tmux sessions matching f3w3-*: absent
both QA temp roots: absent
```

`lsp_diagnostics` was attempted on the only changed file, the Markdown report, but the tool rejected the absolute `tF3` path as outside its fixed request cwd. No Rust or other source-language file changed, so there is no applicable source LSP result; I did not touch or symlink through the forbidden parent worktree to bypass the guard. A structural report check confirmed 725 lines before this final note and the required verdict as the final line.

## Verdict and exact blocker reproduction

**REJECT** — the HTTP prompt is not consumable by an HTTP client even though the underlying turn runs.

With `opencode-rust serve` configured to any working provider, the exact API sequence used was:

```sh
BASE=http://127.0.0.1:41833
AUTH=f3user:f3-http-password

ID=$(curl -sS -u "$AUTH" -H 'content-type: application/json' \
  -X POST "$BASE/api/session" --data '{}' |
  python3 -c 'import json,sys; print(json.load(sys.stdin)["data"]["id"])')

# Subscribe before submission to rule out a replay-only problem.
curl -sS --max-time 5 -u "$AUTH" \
  "$BASE/api/session/$ID/event?after=0" > live-sse.txt &
SSE_PID=$!

curl -sS -u "$AUTH" -H 'content-type: application/json' \
  -X POST "$BASE/api/session/$ID/prompt" \
  --data '{"prompt":{"text":"HTTP_ROUNDTRIP"}}'
curl -sS -u "$AUTH" -H 'content-type: application/json' \
  -X POST "$BASE/api/session/$ID/wait" --data '{}'
wait "$SSE_PID" || true

wc -c live-sse.txt
curl -sS -u "$AUTH" "$BASE/api/session/$ID/message"
curl -sS -u "$AUTH" "$BASE/api/session/$ID/history"
```

Observed while the provider returned and the database stored `HTTP_ASSISTANT_OK`:

```text
prompt: HTTP 200 with admitted message
wait: HTTP 204
0 live-sse.txt
{"data":[],"cursor":{}}
{"data":[],"hasMore":false}
```

F3 VERDICT: REJECT
