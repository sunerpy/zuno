# F3 Real Manual QA Report — Final Verification Wave 6

Date: 2026-08-10  
Worktree: `/config/workspace/ProdDir/AI/oc-wt/tF3`  
Branch / HEAD: `task-F3` / `b753fb9`  
Role: F3, Real Manual QA

## Bottom line

**APPROVE.** All four prioritized real-user checks passed: the user's configured `kiro-auth` provider appears in `models`; HTTP answers are visible through pre-opened session SSE, `/message`, and `/history`; disconnecting the only session SSE observer rejects a pending permission immediately without running the tool; and `/api/fs/*` blocks outward file and directory symlinks without disclosing either outside secret.

Additional exercised surfaces also passed: bidirectional released-TypeScript/Rust session use, export parity, preview-only prune semantics, all ten declared 503 gaps, all three `diagnostics-name-their-cause` surfaces, and honest `completion` presentation. I found no BLOCKER, DEFECT, or OBSERVATION requiring release rejection in the scenarios actually run.

## Execution journal and isolation

- Initial `git status --short`: clean.
- The requested debug binary was initially absent in this worktree. I built it from this HEAD with `cargo build -p oc-cli --bin opencode-rust`.
- Isolation root: `/tmp/opencode/f3-wave6-20260810-b753fb9`.
- Loopback ports: `43831` for the local provider and `43833` for `opencode-rust serve`.
- Every product run except the explicitly requested read-only real-config `models` probe uses `env -i` with temporary `HOME`, `XDG_CONFIG_HOME`, `XDG_DATA_HOME`, `XDG_CACHE_HOME`, `XDG_STATE_HOME`, and `TMPDIR`.
- I did not read, copy, or mutate `/config/.local/share/opencode/*.db`, and did not write `/config/.config/opencode/`.
- Cleanup inventory, recorded before creation: the isolation root, provider/server PIDs, provider/server logs, bounded SSE captures, and exact tmux sessions `f3w6-provider` and `f3w6-server`. Only recorded PIDs and those exact session names were stopped.
- References read before QA: Rust runtime, setup/journal, real manual QA, and cleanup/final-verification references from `@sunerpy/oh-my-openagent` 4.21.0.

## 0. Subject build — PASS

Command:

```sh
cargo build -p oc-cli --bin opencode-rust
```

Observed tail:

```text
Compiling oc-server v0.1.0 (.../tF3/crates/oc-server)
Compiling oc-tui v0.1.0 (.../tF3/crates/oc-tui)
Compiling oc-cli v0.1.0 (.../tF3/crates/oc-cli)
Finished `dev` profile [unoptimized + debuginfo] target(s) in 26.50s
```

Judgement: **PASS**. Subsequent manual QA uses the freshly built binary from this exact worktree and commit.

## Results

Results below are populated only from commands actually run in this wave.

## 1. Real-config provider discovery (`kiro-auth`) — PASS

This was the assignment's explicit read-only exception to temporary `HOME`/XDG isolation. I used the user's real configuration only as input and redirected diagnostics away exactly as requested:

```sh
set -o pipefail
env -i PATH=/usr/bin:/bin HOME=/config \
  XDG_CACHE_HOME=/config/.cache \
  XDG_DATA_HOME=/config/.local/share \
  XDG_CONFIG_HOME=/config/.config \
  timeout 200 ./target/debug/opencode-rust models 2>/dev/null \
  | cut -d/ -f1 | sort -u
```

Observed output and pipeline status:

```text
amazon-bedrock
awsopenai
google
kiro-auth
myopenai
nwcdai
openai
zhipuai
zhipuai-coding-plan
PIPELINE_EXIT=0
```

Judgement: **PASS**. Nine provider prefixes were discovered and `kiro-auth` is present in the real `models` output.

## 2. HTTP turn readback through `/message`, `/history`, and pre-opened SSE — PASS

I launched the real entry point, not standalone `oc-server`, under the isolated XDG tree and with Basic auth:

```sh
env -i PATH=/usr/bin:/bin \
  HOME=/tmp/opencode/f3-wave6-20260810-b753fb9/product/home \
  XDG_CONFIG_HOME=/tmp/opencode/f3-wave6-20260810-b753fb9/product/config \
  XDG_DATA_HOME=/tmp/opencode/f3-wave6-20260810-b753fb9/product/data \
  XDG_CACHE_HOME=/tmp/opencode/f3-wave6-20260810-b753fb9/product/cache \
  XDG_STATE_HOME=/tmp/opencode/f3-wave6-20260810-b753fb9/product/state \
  TMPDIR=/tmp/opencode/f3-wave6-20260810-b753fb9/product/tmp \
  OPENCODE_DISABLE_MODELS_FETCH=1 \
  OPENCODE_SERVER_USERNAME=f3user \
  OPENCODE_SERVER_PASSWORD=f3-wave6-password \
  /config/workspace/ProdDir/AI/oc-wt/tF3/target/debug/opencode-rust \
  serve --hostname 127.0.0.1 --port 43833
```

The fixture treated the title request and chat request separately. Its observed request modes were:

```text
{"message_roles":["system","user","user"],"mode":"title","stream":true}
{"message_roles":["system","user"],"mode":"http-answer","stream":true}
```

I created `ses_7c21a1ebc2bc4d9cb636caa2cd2d6d7e`, opened its SSE stream before submission, then exercised the production HTTP turn:

```sh
curl -sS --max-time 8 -u "$AUTH" \
  "$BASE/api/session/$ID/event?after=0" > http-live-sse.txt &
curl -sS -u "$AUTH" -H 'content-type: application/json' \
  -X POST "$BASE/api/session/$ID/prompt" \
  --data '{"prompt":{"text":"HTTP_ROUNDTRIP_W6"}}'
curl -sS --max-time 30 -u "$AUTH" -H 'content-type: application/json' \
  -X POST "$BASE/api/session/$ID/wait" --data '{}'
curl -sS -u "$AUTH" "$BASE/api/session/$ID/message"
curl -sS -u "$AUTH" "$BASE/api/session/$ID/history"
```

Observed:

```text
CREATE_HTTP=200 SESSION_ID=ses_7c21a1ebc2bc4d9cb636caa2cd2d6d7e
PROMPT_HTTP=200 WAIT_HTTP=204 MESSAGE_HTTP=200 HISTORY_HTTP=200 SSE_EXIT=28
MESSAGE_ANSWER_COUNT=1
HISTORY_ANSWER_COUNT=1
SSE_BYTES=2835 SSE_ANSWER_COUNT=1
MESSAGE_DATA_COUNT=2
HISTORY_DATA_COUNT=12
SSE_EVENT_TYPES=provider,turn.started,agent.resolved,model.resolved,
assistant.message.created,tool.snapshot.locked,provider.request.started,
provider,provider,assistant.checkpointed,step.completed,turn.completed
```

The bounded SSE `curl` exit 28 is expected because the endpoint remains live. The exact assistant text `HTTP_W6_ASSISTANT_OK` appeared once in every requested client-visible path.

Judgement: **PASS**. The HTTP round executes and its answer is visible live as well as through both post-completion read APIs.

## 3. Last session-SSE observer disconnect rejects pending permission — PASS

The server retained `"permission":{"bash":"ask"}`. The fixture requested one real `bash` call with command `printf PERMISSION_TOOL_MUST_NOT_RUN_W6`, then produced a final answer only after receiving the denied tool result.

I created `ses_905c5e54d321423db1cfe39d5b438e88`, opened its only session SSE connection, submitted the prompt, and polled until the request was demonstrably pending while that exact `curl` process was still alive:

```sh
curl -sS --max-time 30 -u "$AUTH" \
  "$BASE/api/session/$ID/event?after=0" > permission-sse.txt &
SSE_PID=$!

curl -sS -u "$AUTH" -H 'content-type: application/json' \
  -X POST "$BASE/api/session/$ID/prompt" \
  --data '{"prompt":{"text":"DISCONNECT_PERMISSION_W6"}}'

# Poll GET /api/permission/request until this session is listed,
# verify kill -0 "$SSE_PID", then terminate only that curl PID.
kill "$SSE_PID"

curl -sS --max-time 10 -u "$AUTH" -H 'content-type: application/json' \
  -X POST "$BASE/api/session/$ID/wait" --data '{}'
```

Pending request before disconnect:

```json
{"id":"per_2abee8f4066f4ba3842fd11853a43e9e",
 "sessionID":"ses_905c5e54d321423db1cfe39d5b438e88",
 "action":"bash",
 "resources":["printf PERMISSION_TOOL_MUST_NOT_RUN_W6"]}
```

Observed lifecycle:

```text
CREATE_HTTP=200 SESSION_ID=ses_905c5e54d321423db1cfe39d5b438e88
PENDING_OBSERVED_ATTEMPT=2 REQUEST_ID=per_2abee8f4066f4ba3842fd11853a43e9e
PROMPT_HTTP=200 SSE_PID=1429177 SSE_ALIVE_BEFORE_DISCONNECT=yes
SSE_EXIT=143 WAIT_CURL_EXIT=0 WAIT_HTTP=204 DISCONNECT_TO_WAIT_MS=15 MESSAGE_HTTP=200
PENDING_BEFORE_COUNT=1
PENDING_AFTER_COUNT=0
RESUMED_ANSWER_COUNT=1
```

The resulting tool part was:

```json
{"status":"error",
 "error":"tool bash was denied by the permission layer",
 "output":null}
```

The provider then received `system,user,assistant,tool` and returned `PERMISSION_W6_RESUMED_AFTER_REJECTION`, which was readable through `/message`.

Judgement: **PASS / prior blocker fixed**. Disconnecting the only session observer removed the pending request, rejected rather than authorized the tool, produced no tool output, resumed the round, and allowed `/wait` to return 204 in 15 ms—well before the five-minute watchdog.

## 4. Filesystem API root and symlink boundary — PASS

The server's isolated working root contained `inside.txt` with `SAFE_INSIDE_W6`. Outside that root I created two distinctive secrets, then linked to them from inside the root:

```text
workspace/link-secret -> ../outside-secret.txt
workspace/link-dir    -> ../outside-dir
outside-secret.txt              = F3_W6_OUTSIDE_FILE_SECRET_7Q9
outside-dir/nested-secret.txt   = F3_W6_OUTSIDE_DIR_SECRET_8R4
```

I exercised a legitimate read, raw and encoded traversal, an absolute path, both requested outward symlink forms, list, and find. Exact observed responses (small JSON bodies shown in full):

```text
inside HTTP=200 BODY=SAFE_INSIDE_W6\n
traversal-raw HTTP=403 BODY={"error":{"code":"path_escaped_root","message":"the requested path leaves the session directory"}}
traversal-encoded HTTP=403 BODY={"error":{"code":"path_escaped_root","message":"the requested path leaves the session directory"}}
absolute HTTP=403 BODY={"error":{"code":"path_escaped_root","message":"the requested path leaves the session directory"}}
symlink-file HTTP=403 BODY={"error":{"code":"path_escaped_root","message":"the requested path leaves the session directory"}}
symlink-dir-read HTTP=403 BODY={"error":{"code":"path_escaped_root","message":"the requested path leaves the session directory"}}
list-root HTTP=200 BODY={..."data":[{"path":"inside.txt","type":"file"}]}
list-symlink-dir HTTP=403 BODY={"error":{"code":"path_escaped_root","message":"the requested path leaves the session directory"}}
find-inside HTTP=200 BODY={..."data":[{"path":"inside.txt","type":"file"}]}
find-outside HTTP=200 BODY={..."data":[]}
CAPTURE_COUNT=10 OUTSIDE_FILE_SECRET_COUNT=0 OUTSIDE_DIR_SECRET_COUNT=0
```

The traversal probes used `curl --path-as-is`, including:

```sh
curl --path-as-is "$BASE/api/fs/read/../outside-secret.txt"
curl --path-as-is "$BASE/api/fs/read/%2e%2e%2foutside-secret.txt"
curl --path-as-is "$BASE/api/fs/read//etc/hostname"
```

Judgement: **PASS**. The API reads an in-root file, but blocks traversal, absolute paths, outward file symlinks, outward directory symlinks, and symlink directory listing. Neither outside secret appeared in any captured response.

## 5. Bidirectional released-TypeScript/Rust session lifecycle — PASS

Both binaries used the same explicit isolated database and the local working provider. First released TypeScript 1.18.12 wrote a real turn, then Rust listed it; Rust wrote a second real turn, then released TypeScript listed both:

```sh
env -i ... OPENCODE_DB="$DB" OPENCODE_DISABLE_MODELS_FETCH=1 \
  /config/.local/share/mise/installs/opencode/1.18.12/opencode \
  run --model localqa/f3-model --format json 'TS_WRITES_RUST_READS_W6'

env -i ... OPENCODE_DB="$DB" OPENCODE_DISABLE_MODELS_FETCH=1 \
  ./target/debug/opencode-rust session list --all-projects --format json

env -i ... OPENCODE_DB="$DB" OPENCODE_DISABLE_MODELS_FETCH=1 \
  ./target/debug/opencode-rust run --model localqa/f3-model \
  --format json 'RUST_WRITES_TS_READS_W6'

env -i ... OPENCODE_DB="$DB" OPENCODE_DISABLE_MODELS_FETCH=1 \
  /config/.local/share/mise/installs/opencode/1.18.12/opencode \
  session list --format json
```

Observed:

```text
TS_RUN_EXIT=0 RUST_READ_EXIT=0 RUST_RUN_EXIT=0 TS_READ_EXIT=0
RUST_LIST_AFTER_TS_COUNT=1
RUST_LIST_VERSIONS=1.18.12
TS_LIST_AFTER_RUST_COUNT=2
TS_FINAL_IDS=ses_0134bec6effeWiBoPXy4CXrCZ5,ses_ec30909ae9c94a3fab5d4005a5f6dfbc
STDERR_BYTES ts_run=0 rust_read=0 rust_run=0 ts_read=0
```

The released turn emitted non-zero usage (`input=9`, `output=3`, `total=12`) and text `HTTP_W6_ASSISTANT_OK`. Rust emitted the same non-empty answer and `turn_completed`. Released TypeScript's final list named both session IDs and both `F3_W6_TITLE` titles.

Judgement: **PASS**. Each implementation can consume a session written by the other in the same explicit database.

## 6. `session prune` preview does not change logical database content — PASS, physical-WAL note

Against the two-session mixed database:

```sh
env -i ... OPENCODE_DB="$DB" OPENCODE_DISABLE_MODELS_FETCH=1 \
  ./target/debug/opencode-rust session prune --older-than 0 \
  --all-projects --include-recent --format json
```

Observed semantic result:

```text
PRUNE_EXIT=0 ACTION=preview
SELECTED_COUNT=2
CHANGED_SESSIONS=0
WARNINGS_COUNT=0
STDERR_BYTES=0
```

The first raw base-file-only SHA comparison changed:

```text
BEFORE_SHA256=afd6a3d29f11cf420f376f354767c4ccbc40cc8184c02f71cd2261abc06db615
AFTER_SHA256=d1201ec803313b5abc39a5d4558d67c949d2a9b49025c06efb96b002aa5d3ab1
```

I did not treat that alone as content mutation because this cross-binary SQLite run had a live `mixed.db-wal` state that a base `.db` hash excludes. I repeated the exact TypeScript-write → Rust-write → TypeScript-read sequence and captured SQLite state before and after preview:

```text
TS_RUN_EXIT=0 RUST_RUN_EXIT=0 TS_LIST_EXIT=0 PRUNE_EXIT=0
BYTE_IDENTICAL=no
LOGICAL_DUMP_IDENTICAL=yes
PRAGMAS_IDENTICAL=yes
ACTION=preview
```

The comparison used `sqlite3 -readonly "$DB" '.dump'` and read-only `journal_mode`, `user_version`, `schema_version`, and `freelist_count` probes. A second preview after the state settled was also physically stable:

```text
PRUNE_EXIT=0
BYTE_IDENTICAL=yes
LOGICAL_DUMP_IDENTICAL=yes
ACTION=preview
```

Judgement: **PASS with an operational note**. Preview selected two real sessions but changed no logical row/schema content. A raw hash of only the main SQLite file is not a valid byte-stability check while a `-wal` sidecar participates in the database state.

## 7. Ten declared HTTP compatibility gaps — PASS as honest gaps

I invoked every currently declared missing operation against the authenticated real server. All ten returned HTTP 503, `error.code=backend_unavailable`, and named the exact missing method/template:

```text
credential-delete HTTP=503 MESSAGE=backend unavailable for DELETE /api/credential/{credentialID}
credential-patch HTTP=503 MESSAGE=backend unavailable for PATCH /api/credential/{credentialID}
attempt-delete HTTP=503 MESSAGE=backend unavailable for DELETE /api/integration/attempt/{attemptID}
attempt-get HTTP=503 MESSAGE=backend unavailable for GET /api/integration/attempt/{attemptID}
attempt-complete HTTP=503 MESSAGE=backend unavailable for POST /api/integration/attempt/{attemptID}/complete
connect-key HTTP=503 MESSAGE=backend unavailable for POST /api/integration/{integrationID}/connect/key
connect-oauth HTTP=503 MESSAGE=backend unavailable for POST /api/integration/{integrationID}/connect/oauth
message-get HTTP=503 MESSAGE=backend unavailable for GET /api/session/{sessionID}/message/{messageID}
permission-create HTTP=503 MESSAGE=backend unavailable for POST /api/session/{sessionID}/permission
permission-get HTTP=503 MESSAGE=backend unavailable for GET /api/session/{sessionID}/permission/{requestID}
GAP_COUNT=10 BACKEND_UNAVAILABLE_COUNT=10
```

Judgement: **PASS as declared gaps**, not new findings. None masqueraded as implemented, returned 501, or omitted its operation identity.

## 8. Three `diagnostics-name-their-cause` divergence surfaces — PASS

I exercised all three named diagnostic surfaces against released upstream 1.18.15 and this port under the same isolated environment. Both sides refused every invalid operation with exit 1, while this port retained its documented cause-specific form.

Busy port `43833`:

```text
oracle exit=1 stderr:
Error: Unexpected error

ServeError

subject exit=1 stderr:
could not bind HTTP server to 127.0.0.1:43833: Address already in use (os error 98)
```

Missing run message:

```text
oracle exit=1 stderr: Error: You must provide a message or a command
subject exit=1 stderr: a message is required
```

Unavailable model with fetch disabled:

```text
oracle exit=1 stderr:
{"name":"UnknownError","data":{"message":"Unexpected server error. Check server logs for details.","ref":"err_e2795afb"}}

subject exit=1 stderr:
model `bogus/model` is not available: no `provider` block in your configuration defines it,
OPENCODE_DISABLE_MODELS_FETCH is set so no fetch from `https://models.opencode.ai` was attempted,
and no cached catalog exists at `<ISOLATED_CACHE>/opencode/models.json` ...
```

Judgement: **PASS**. The three intentional diagnostics differences are live in both directions: upstream keeps the recorded opaque forms, and this port continues to name the address, missing argument, or catalog/config cause.

## 9. Export parity and honest completion presentation — PASS

I exported the Rust-written session `ses_ec30909ae9c94a3fab5d4005a5f6dfbc` from the same database with Rust and released TypeScript 1.18.12, then canonicalized both documents with `jq -S`:

```text
RUST_EXPORT_EXIT=0 TS_EXPORT_EXIT=0 CANONICAL_EQUAL=yes
RUST_EXPORT_BYTES=2586 TS_EXPORT_BYTES=2586
RUST_STDERR: Exporting session: ses_ec30909ae9c94a3fab5d4005a5f6dfbc
TS_STDERR:   Exporting session: ses_ec30909ae9c94a3fab5d4005a5f6dfbc
```

I also invoked all four supported `completion` argument forms:

```text
FORM=none EXIT=1 STDOUT_BYTES=0 STDERR_BYTES=361 SAYS_NOT_AVAILABLE=yes
FORM=bash EXIT=1 STDOUT_BYTES=0 STDERR_BYTES=361 SAYS_NOT_AVAILABLE=yes
FORM=zsh  EXIT=1 STDOUT_BYTES=0 STDERR_BYTES=361 SAYS_NOT_AVAILABLE=yes
FORM=fish EXIT=1 STDOUT_BYTES=0 STDERR_BYTES=361 SAYS_NOT_AVAILABLE=yes
HELP_COMPLETION_LINE=completion  Explain why shell completion output is unavailable, and what to use instead
```

Representative diagnostic:

```text
`completion` is not available: upstream's completion script is a yargs shell function
that asks the binary back for candidates over `--get-yargs-completions`, a protocol
this port does not serve ... run `--help` ... instead
```

Judgement: **PASS**. Export is operational and byte-size/canonical-content equivalent to released TypeScript for the exercised session. Completion does not emit an empty script or claim implementation; every form gives the same reason and alternative.

## Findings summary

| Severity | Result |
|---|---|
| BLOCKER | None found in the exercised scenarios. |
| DEFECT | None found in the exercised scenarios. |
| OBSERVATION | A main SQLite `.db` file hash can change while a live `-wal` participates in database state; read-only logical dumps and pragma state remained identical across `session prune` preview. This was not logical data mutation. |

The previous permission-disconnect blocker is fixed in real use: the request disappeared, the tool result was a denial with no output, the provider resumed, and `/wait` returned 204 only 15 ms after the sole observer was disconnected.

## Honest test gaps

- I did not run the prohibited ~100-minute memory gate or two-hour soak.
- I did not open, copy, or mutate the 62 GB user database or its pinned performance backup.
- I did not run the full 3,360-test suite or Clippy; those were supplied as the `b753fb9` baseline. I built the exact `opencode-rust` subject used for this report.
- I did not run a real TUI turn, PTY ticket replay/expiry/no-ticket probes, credential-redaction probes, or the memory switch in this wave; those passed in wave 4 but are not claimed as rerun here.
- I did not generate a real `question` tool call or exercise its three fail-closed paths.
- I did not perform destructive prune/archive/delete; only preview was exercised.
- I did not repeat all twelve command-parity rows or all seventeen declared divergences. I exercised export, completion, session lifecycle/listing/prune, serve, run, models, and the three diagnostics divergence surfaces documented above.
- I did not test every malformed body, cursor, pagination, workspace, or concurrency permutation of implemented HTTP operations.
- The named `assistant-turn-step-parts` gap was not treated as a defect; this wave observed the known released `step_start`/`step_finish` versus Rust event-shape difference during real runs but did not independently inspect database part rows.

## Cleanup and worktree scope

I stopped only the resources created by this QA run. No broad `pkill` was used:

```text
tmux f3w6-provider: killed
tmux f3w6-server: killed
PID_1262294=gone
PID_1176060=gone
PID_1104035=gone
PID_1130401=gone
PID_1123788=gone
ports 43831 and 43833: no listeners
ISOLATION_ROOT=removed
```

Final pre-verdict scope check:

```text
git status --short: ?? F3-REPORT.md
git diff --stat: no tracked diff
git diff --check: clean
required final line: F3 VERDICT: APPROVE
```

`lsp_diagnostics` was attempted on the changed report, but the tool rejected the path before starting a client because its request CWD is the forbidden main worktree rather than `tF3`: `LSP file path must be inside request cwd`. `lsp_status` also reports no Markdown language server. Therefore no Markdown LSP diagnostic could be run; `git diff --check` and the exact final-line assertion are the available file validations.

The ignored `target/` contains only the requested local build output. No source, test, existing documentation, user configuration, or user database was modified.

## Verdict

**APPROVE.** The prioritized regression and security paths passed through real CLI/HTTP entry points, and no release-blocking behavior was found in the additional exercised coverage.

F3 VERDICT: APPROVE
