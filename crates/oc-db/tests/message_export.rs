//! Differential: the real TypeScript binary must be able to read a conversation
//! this crate wrote.
//!
//! Todo 20 proved the *schema* a Rust-created database presents is byte-compatible
//! with the one the real binary builds. That says nothing about the two `data`
//! columns, which SQLite does not police. `opencode export <sessionID>` decodes
//! both blobs through the TypeScript schema before printing them, so it fails on
//! a payload this crate got wrong - a missing field, a renamed key, a variant tag
//! the union does not carry. That makes it the sharpest available check on the one
//! thing this module is responsible for.
//!
//! Skipped when the real binary is not installed.

use oc_db::message::{MessageRecord, MessageStore, PartKind, PartRecord};
use oc_db::{migration, open};
use oc_testkit::pinned_oracle_or_skip;
use serde_json::{Value, json};
use std::path::Path;
use std::process::{Command, Output};

const SESSION_ID: &str = "ses_5jXqK8mNpQrStUvWxYz0123456789ab";
const MESSAGE_ID: &str = "msg_5jXqK8mNpQrStUvWxYz0123456789ab";

/// Run the real binary against an isolated data home, so the user's own
/// `opencode.db` is never opened, let alone written.
fn run_oracle(binary: &Path, root: &Path, args: &[&str]) -> Output {
    let home = root.join("home");
    std::fs::create_dir_all(&home).expect("create isolated oracle home");
    Command::new(binary)
        .args(args)
        .current_dir(root)
        .env("HOME", home)
        .env("XDG_DATA_HOME", root.join("data"))
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("XDG_CACHE_HOME", root.join("cache"))
        .env("XDG_STATE_HOME", root.join("state"))
        .env_remove("OPENCODE_DB")
        .output()
        .expect("run the real opencode binary")
}

/// A part payload per variant, trimmed to the fields the TypeScript schema
/// declares required. `export` decodes strictly, so a payload missing a required
/// field is rejected there - which is the point of running it.
fn part_payload(kind: PartKind, part_id: &str, message_id: &str) -> Value {
    let mut value = match kind {
        PartKind::Text => json!({ "type": "text", "text": "hello from rust" }),
        PartKind::Reasoning => json!({
            "type": "reasoning",
            "text": "weighing the index order",
            "time": { "start": 1_780_034_795_239_i64, "end": 1_780_034_795_999_i64 },
        }),
        PartKind::Tool => json!({
            "type": "tool",
            "callID": "toolu_01RUST",
            "tool": "read",
            "state": {
                "status": "completed",
                "input": { "filePath": "/workspace/src/lib.rs" },
                "output": "pub mod message;",
                "title": "src/lib.rs",
                "metadata": { "lines": 1 },
                "time": { "start": 1_780_034_795_239_i64, "end": 1_780_034_795_512_i64 },
            },
        }),
        PartKind::StepStart => json!({ "type": "step-start" }),
        PartKind::StepFinish => json!({
            "type": "step-finish",
            "reason": "stop",
            "cost": 0.001_25,
            "tokens": {
                "input": 100.0,
                "output": 20.0,
                "reasoning": 0.0,
                "cache": { "read": 0.0, "write": 0.0 },
            },
        }),
        PartKind::Patch => json!({
            "type": "patch",
            "hash": "c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00",
            "files": ["crates/oc-db/src/message.rs"],
        }),
        PartKind::File => json!({
            "type": "file",
            "mime": "text/plain",
            "filename": "note.txt",
            "url": "data:text/plain;base64,aGVsbG8=",
        }),
        PartKind::Compaction => json!({ "type": "compaction", "auto": false }),
        PartKind::Subtask => json!({
            "type": "subtask",
            "prompt": "audit the parser",
            "description": "parser audit",
            "agent": "explore",
        }),
        PartKind::Snapshot => json!({
            "type": "snapshot",
            "snapshot": "9f2c1ab0d3e4f5061728394a5b6c7d8e9f001122",
        }),
        PartKind::Agent => json!({ "type": "agent", "name": "explore" }),
        PartKind::Retry => json!({
            "type": "retry",
            "attempt": 1,
            "error": {
                "name": "APIError",
                "data": { "message": "overloaded", "isRetryable": true },
            },
            "time": { "created": 1_780_034_795_239_i64 },
        }),
    };
    let object = value.as_object_mut().expect("payload is an object");
    object.insert("id".to_owned(), json!(part_id));
    object.insert("sessionID".to_owned(), json!(SESSION_ID));
    object.insert("messageID".to_owned(), json!(message_id));
    value
}

/// Write a project, a session, a user message, an assistant message, and one part
/// of every variant - entirely through this crate's own writers.
fn write_conversation(path: &Path) -> Vec<String> {
    std::fs::create_dir_all(path.parent().expect("database has a parent"))
        .expect("create the data directory");
    let mut connection = open::open_at(path).expect("open the Rust database");
    migration::apply(&mut connection).expect("apply the schema");
    connection
        .execute_batch(&format!(
            "INSERT INTO project (id, worktree, time_created, time_updated, sandboxes) \
             VALUES ('prj_rust', '/workspace', 1780034795000, 1780034795000, '[]');
             INSERT INTO session \
               (id, project_id, slug, directory, title, version, time_created, time_updated) \
             VALUES ('{SESSION_ID}', 'prj_rust', 'rust-written', '/workspace', \
                     'written by oc-db', '1.18.13', 1780034795000, 1780034795000);"
        ))
        .expect("seed a project and a session");

    let store = MessageStore::new(&connection);
    let user = MessageRecord::from_json(json!({
        "id": MESSAGE_ID,
        "sessionID": SESSION_ID,
        "role": "user",
        "time": { "created": 1_780_034_795_100_i64 },
        "agent": "build",
        "model": { "providerID": "anthropic", "modelID": "claude-sonnet-4-5" },
    }))
    .expect("split the user message");
    store
        .put_message_at(&user, 1_780_034_795_100)
        .expect("write the user message");

    let assistant_id = "msg_5jXqK8mNpQrStUvWxYz0123456789cd";
    let assistant = MessageRecord::from_json(json!({
        "id": assistant_id,
        "sessionID": SESSION_ID,
        "role": "assistant",
        "time": { "created": 1_780_034_795_200_i64, "completed": 1_780_034_796_000_i64 },
        "parentID": MESSAGE_ID,
        "modelID": "claude-sonnet-4-5",
        "providerID": "anthropic",
        "mode": "build",
        "agent": "build",
        "path": { "cwd": "/workspace", "root": "/workspace" },
        "cost": 0.004_25,
        "tokens": {
            "input": 1_024.0,
            "output": 256.0,
            "reasoning": 0.0,
            "cache": { "read": 0.0, "write": 0.0 },
        },
    }))
    .expect("split the assistant message");
    store
        .put_message_at(&assistant, 1_780_034_795_200)
        .expect("write the assistant message");

    let mut part_ids = Vec::new();
    for (index, kind) in PartKind::ALL.into_iter().enumerate() {
        let part_id = format!("prt_5jXqK8mNpQrStUvWxYz01234567{index:02}");
        let payload = part_payload(kind, &part_id, assistant_id);
        let record = PartRecord::from_json(
            payload,
            1_780_034_795_300 + i64::from(u8::try_from(index).expect("index fits")),
        )
        .unwrap_or_else(|error| panic!("{kind}: split: {error}"));
        store
            .put_part_at(&record, record.time_created)
            .unwrap_or_else(|error| panic!("{kind}: write: {error}"));
        part_ids.push(part_id);
    }
    part_ids
}

#[test]
fn message_a_rust_written_session_is_readable_by_the_real_binary() {
    let Some(binary) = pinned_oracle_or_skip(
        "message_a_rust_written_session_is_readable_by_the_real_binary",
        "no conversation this crate wrote was decoded by a real release",
    ) else {
        return;
    };
    let root = tempfile::tempdir().expect("create a temporary root");
    let path = root
        .path()
        .join("data")
        .join("opencode")
        .join("opencode.db");
    let part_ids = write_conversation(&path);

    let output = run_oracle(binary, root.path(), &["export", SESSION_ID, "--pure"]);
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    assert!(
        output.status.success(),
        "opencode export exited with {}\nstdout:\n{stdout}\nstderr:\n{stderr}",
        output.status,
    );

    let exported: Value = serde_json::from_str(stdout.trim())
        .unwrap_or_else(|error| panic!("export did not print JSON ({error}):\n{stdout}"));

    let mut seen_variants = Vec::new();
    let mut seen_part_ids = Vec::new();
    collect_parts(&exported, &mut seen_variants, &mut seen_part_ids);

    for kind in PartKind::ALL {
        assert!(
            seen_variants.iter().any(|tag| tag == kind.as_str()),
            "the real binary did not surface the `{kind}` part it was given\n\
             variants it did surface: {seen_variants:?}\nexport:\n{stdout}"
        );
    }
    for part_id in &part_ids {
        assert!(
            seen_part_ids.contains(part_id),
            "the real binary dropped part {part_id}\nexport:\n{stdout}"
        );
    }
}

/// Walk the exported document collecting every `{ id, type }` pair that looks like
/// a part, without assuming where `export` nests them.
fn collect_parts(value: &Value, variants: &mut Vec<String>, ids: &mut Vec<String>) {
    match value {
        Value::Object(object) => {
            let id = object.get("id").and_then(Value::as_str);
            let tag = object.get("type").and_then(Value::as_str);
            if let (Some(id), Some(tag)) = (id, tag)
                && id.starts_with("prt_")
                && PartKind::from_tag(tag).is_some()
            {
                ids.push(id.to_owned());
                variants.push(tag.to_owned());
            }
            for nested in object.values() {
                collect_parts(nested, variants, ids);
            }
        }
        Value::Array(items) => {
            for nested in items {
                collect_parts(nested, variants, ids);
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {}
    }
}
