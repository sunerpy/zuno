# Operational logging

Zuno separates two kinds of durable data:

- Session events contain the exact model-visible prompts, external inputs, tool
  results, retries, and subagent reports needed to reconstruct a request.
- Operational logs contain bounded diagnostic metadata: process lifecycle,
  session/turn correlation, provider attempts, tool lifecycle, timing, typed
  outcomes, and resource incidents.

Operational logs must not become a second transcript. Every text sink and the
SQLite sink pass field values through the same name-based redaction predicate
before the record is written, so a field whose name spells `authorization`,
`api_key`, `access_token`, `refresh_token`, `private_key`, `password`,
`passphrase`, `secret`, `credential`, `cookie`, `token`, `bearer`, `signature`,
`prompt`, `content`, `body`, `command`, `input`, `output`, `report`, `stdin`,
`stdout`, or `stderr` is written as `[redacted]`. Separators and case are
ignored, so `api_key`, `apiKey`, `api-key`, and `API.KEY` are one name; regular
plurals are matched too (`commands`, `cookies`, `access_tokens`). A field name
Zuno cannot classify, one containing a non-ASCII byte or with no recognizable
component, fails closed and is redacted. Redaction replaces one field's value:
the record, its event text, and its other fields are still written, so a
redacted payload never costs you the diagnostic.

Two exceptions are deliberate, and both are limits to design around:

- **A name that spells only a bound stays readable**, so there is a safe way to
  report size: `command_bytes`, `prompt_tokens`, `output_tokens`,
  `input_tokens`, `content_tokens`, `command_tokens`, `secret_bytes`,
  `cookie_count`. Only those five token-accounting classes license the plural
  `tokens` reading; `secret_tokens`, `cookie_tokens`, `auth_tokens`,
  `total_tokens`, and `max_tokens` are redacted. Never put a value in a field
  named for its bound: `secret_bytes` must hold a byte count.
- **The field named `message` is never redacted.** `message` is the name
  `tracing` gives an event's own text, and the formatter prints it with no
  `name=` prefix, so whatever a callsite puts there is written verbatim to the
  plaintext log, to `--print-logs` stderr, and to the `message` column of
  `logs.sqlite`, where it reads as the event message. Some Zuno callsites still
  route external text through `message` (an MCP server's stderr line is the one
  to know about), so the plaintext log can contain raw child-process output.
  Give a payload its own field name, which the predicate can classify, or log
  only a bound (`bytes`, `limit`, `truncated`).

A component that needs model-visible payloads must use the session event log.

## Default store

Every command that initializes the runtime writes at `INFO` to:

```text
$XDG_DATA_HOME/zuno/log/logs.sqlite
```

The database uses SQLite WAL and a five-second busy timeout, so several TUI,
headless, ACP, and server processes can write concurrently. Initial WAL/schema
lock contention is retried with bounded backoff. Every record carries
`process_uuid` and `pid`; records emitted under runtime spans also carry
`session_id`, `turn_id`, `tool_call_id`, provider, model, attempt, and operation.

Retention is enforced by the writer:

- newest 50,000 records;
- approximately 32 MiB of record payload;
- no records older than 10 days.

The queue is bounded and lossy rather than allowed to stall an agent turn.
Shutdown flushes queued records and reports dropped records or write failures to
stderr.

Example inspection:

```sh
sqlite3 "$XDG_DATA_HOME/zuno/log/logs.sqlite" \
  "select datetime(timestamp_ms / 1000, 'unixepoch'), level, target, message
   from log_record order by id desc limit 50;"
```

## Levels and filters

`INFO` is the default. The simple CLI/environment controls are:

```sh
zuno --log-level DEBUG
ZUNO_LOG_LEVEL=TRACE zuno
ZUNO_PRINT_LOGS=1 zuno
```

`TRACE`, `DEBUG`, `INFO`, `WARN`, and `ERROR` are accepted. `--print-logs` and
`ZUNO_PRINT_LOGS=1` add a stderr sink; stdout is never a log destination because
ACP and other stdio protocols frame data there.

Use standard target-aware Rust filtering when no explicit Zuno level is set:

```sh
RUST_LOG='zuno_engine=trace,zuno_tools=debug,zuno_db=warn' zuno
```

An explicit `--log-level` or `ZUNO_LOG_LEVEL` is the process-wide override and
takes precedence over `RUST_LOG`.

## Optional plaintext

Plaintext is disabled by default. Enable it only for a bounded debugging session:

```sh
ZUNO_PLAINTEXT_LOGS=1 zuno
```

Each process creates its own file:

```text
zuno.<pid>.<process_uuid>.log
```

On Unix, the log directory is `0700` and both `logs.sqlite` and plaintext files
are `0600`. Process-specific filenames avoid interleaved writes and
cross-process rotation races. The structured store remains authoritative for
bounded operational history.

## Runtime instrumentation

The real runtime, not only test fixtures, opens:

- one `turn` span per `RunTurnRequest`;
- one `provider_request` span per provider attempt, including title, summary,
  compaction, and ordinary turn operations;
- one `tool_call` span per prepared dispatch, with pending, running, completed,
  blocked, error, or abandoned lifecycle records.

Provider diagnostics record typed outcome/status metadata, not request or
response bodies. The destructive-command risk gate records verdict, shell
syntax, and command byte length, never the command itself. Its verdicts are
`run`, `confirm`, and `deny`; `confirm` means the existing `shell` permission
request was upgraded to a fresh attached-user decision, not that the model
should retry with different arguments. A TUI interruption request records the
session id and whether it fired a live turn; it never records prompt, model
output, or tool arguments. Live steering similarly records only session and
admission identifiers plus whether the active turn was woken or the durable
input remained pending.
