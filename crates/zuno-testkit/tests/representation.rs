//! D0 — the three data-representation baselines P1 owes D1-D4.
//!
//! §5.2 of the perf plan makes D0 a **measurement**
//! and forbids D1-D4 from starting before it produces numbers. Nothing here
//! changes a representation; every test either reads a compile-time layout fact
//! or reads the pinned W-real snapshot read-only and counts bytes.
//!
//! # The three baselines
//!
//! 1. **Large-payload sharing** (`d0_large_payload_sharing_baseline`) — where the
//!    105 MB of `part.data` actually sits, so `Arc<str>` (D1) and `CompactString`
//!    (D4) are decided from the byte distribution rather than from the reference
//!    implementation's field list.
//! 2. **Enum boxing** (`d0_enum_boxing_baseline`) — the inline stride of the two
//!    hot event enums, the payload of every variant, and the **maximum number
//!    live at once**, because boxing saves `population x stride delta` and a
//!    stride delta multiplied by a small population is not a saving.
//! 3. **Derived copies** (`d0_derived_copy_baseline`) — how many bytes of the same
//!    payload are simultaneously live while a request is built, measured on both
//!    of the two projections this workspace already has.
//!
//! # Why the fixture tests skip rather than fail without the snapshot
//!
//! The pinned snapshot is a 2.6 GB file on the measuring machine, not a committed
//! fixture. A machine without it cannot produce a comparable number, so the two
//! fixture tests print what is missing and return. The layout half needs no
//! fixture and therefore always runs, which is what keeps the enum measurement
//! from silently disappearing on CI.
//!
//! # Reproduce
//!
//! ```text
//! cargo test -p zuno-testkit --test representation -- --nocapture --test-threads=1
//! ```

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

use serde_json::Value;
use zuno_db::message::{MessageRecord, MessageRole, MessageWithParts, PartKind, PartRecord};
use zuno_engine::interrupt::HardInterruptRequest;
use zuno_engine::r#loop::{
    NoticeSeverity, ProjectedMessage, TURN_EVENT_CHANNEL_CAPACITY, ToolBlockKind, ToolDiff,
    ToolInterruption, TurnEvent, project_history, project_history_owned,
};
use zuno_engine::session_command::SessionCommand;
use zuno_llm::event::{
    ConnectionPhase, FinishReason, Message, RequestContentBlock, StreamEvent, ThoughtSignature,
};
use zuno_testkit::perf::{W_REAL_SUBJECT, verify_pinned_database};
use zuno_tool::{ToolResultPresentation, ToolUiIntent};

/// Repetitions every measured quantity is reported over.
///
/// Five, the same count G1/G2 use, so the spread here is comparable with the
/// allocator table in `docs/perf-methodology.md`. A single sample is not a
/// measurement.
const RUNS: usize = 5;

/// Byte length at or above which a string leaf is a D1 candidate.
///
/// `Arc<str>` trades a heap allocation and a refcount for a free clone, so it
/// only pays on leaves large enough that copying them is the dominant cost. 1 KiB
/// is the threshold this baseline reports at; the full histogram below lets a
/// later reader pick a different one without re-measuring.
const LARGE_LEAF_BYTES: usize = 1024;

/// Byte length at or below which a string leaf is a D4 candidate.
///
/// 24 bytes is `CompactString`'s inline capacity on a 64-bit target, so a leaf at
/// or under it would cost zero heap allocations there.
const INLINE_LEAF_BYTES: usize = 24;

// ---------------------------------------------------------------------------
// Baseline 2: enum boxing. No fixture needed.
// ---------------------------------------------------------------------------

/// One variant's inline payload, measured as the tuple of its field types.
#[derive(Debug, Clone, Copy)]
struct VariantPayload {
    name: &'static str,
    bytes: usize,
}

const fn payload(name: &'static str, bytes: usize) -> VariantPayload {
    VariantPayload { name, bytes }
}

/// Every [`StreamEvent`] variant's payload size.
///
/// Kept honest two ways. `stream_event_variant_name` below is an exhaustive
/// `match`, so adding a variant upstream stops this file compiling; and
/// `assert_table_explains_stride` fails if a variant larger than everything here
/// appears, which is exactly the case that would invalidate the projection.
fn stream_event_payloads() -> Vec<VariantPayload> {
    vec![
        payload("TextDelta", size_of::<(String,)>()),
        payload("ToolUseStart", size_of::<(String, String)>()),
        payload("ToolInputDelta", size_of::<(String, String)>()),
        payload("ToolUseEnd", size_of::<(String,)>()),
        payload("ToolUseSignature", size_of::<(String, ThoughtSignature)>()),
        payload("ToolResult", size_of::<(String, String, bool)>()),
        payload(
            "GeneratedImage",
            size_of::<(String, String, Option<String>, String, Option<String>)>(),
        ),
        payload("ReasoningStart", size_of::<()>()),
        payload("ReasoningDelta", size_of::<(String,)>()),
        payload("ReasoningSignatureDelta", size_of::<(String,)>()),
        payload(
            "ProviderReasoningItem",
            size_of::<(String, Vec<String>, Option<String>, Option<String>)>(),
        ),
        payload("ReasoningEnd", size_of::<()>()),
        payload("ReasoningDone", size_of::<(f64,)>()),
        payload("MessageEnd", size_of::<(Option<FinishReason>,)>()),
        payload("RetryRollback", size_of::<(u32, u32)>()),
        payload(
            "TokenUsage",
            size_of::<(Option<u64>, Option<u64>, Option<u64>, Option<u64>)>(),
        ),
        payload("ConnectionType", size_of::<(String,)>()),
        payload("ConnectionPhase", size_of::<(ConnectionPhase,)>()),
        payload("StatusDetail", size_of::<(String,)>()),
        payload("Error", size_of::<(String, Option<Duration>)>()),
        payload("SessionId", size_of::<(String,)>()),
        payload(
            "Compaction",
            size_of::<(String, Option<u64>, Option<String>)>(),
        ),
        payload("UpstreamProvider", size_of::<(String,)>()),
        payload("NativeToolCall", size_of::<(String, String, Value)>()),
    ]
}

/// The compile-time coupling for [`stream_event_payloads`].
///
/// Exhaustive by design: a new provider capability cannot be added without this
/// file failing to compile, which is what stops the table above from silently
/// describing an enum that has moved on.
fn stream_event_variant_name(event: &StreamEvent) -> &'static str {
    match event {
        StreamEvent::TextDelta(_) => "TextDelta",
        StreamEvent::ToolUseStart { .. } => "ToolUseStart",
        StreamEvent::ToolInputDelta { .. } => "ToolInputDelta",
        StreamEvent::ToolUseEnd { .. } => "ToolUseEnd",
        StreamEvent::ToolUseSignature { .. } => "ToolUseSignature",
        StreamEvent::ToolResult { .. } => "ToolResult",
        StreamEvent::GeneratedImage { .. } => "GeneratedImage",
        StreamEvent::ReasoningStart => "ReasoningStart",
        StreamEvent::ReasoningDelta(_) => "ReasoningDelta",
        StreamEvent::ReasoningSignatureDelta(_) => "ReasoningSignatureDelta",
        StreamEvent::ProviderReasoningItem { .. } => "ProviderReasoningItem",
        StreamEvent::ReasoningEnd => "ReasoningEnd",
        StreamEvent::ReasoningDone { .. } => "ReasoningDone",
        StreamEvent::MessageEnd { .. } => "MessageEnd",
        StreamEvent::RetryRollback { .. } => "RetryRollback",
        StreamEvent::TokenUsage { .. } => "TokenUsage",
        StreamEvent::ConnectionType { .. } => "ConnectionType",
        StreamEvent::ConnectionPhase { .. } => "ConnectionPhase",
        StreamEvent::StatusDetail { .. } => "StatusDetail",
        StreamEvent::Error { .. } => "Error",
        StreamEvent::SessionId(_) => "SessionId",
        StreamEvent::Compaction { .. } => "Compaction",
        StreamEvent::UpstreamProvider { .. } => "UpstreamProvider",
        StreamEvent::NativeToolCall { .. } => "NativeToolCall",
    }
}

/// Every [`TurnEvent`] variant's payload size. See [`stream_event_payloads`].
fn turn_event_payloads() -> Vec<VariantPayload> {
    vec![
        payload("SessionMaterialized", size_of::<(String, String)>()),
        payload("SessionTitleUpdated", size_of::<(String,)>()),
        payload("SessionCommandStarted", size_of::<(SessionCommand,)>()),
        payload(
            "SessionCommandOutput",
            size_of::<(SessionCommand, String)>(),
        ),
        payload("SessionCommandCompleted", size_of::<(SessionCommand,)>()),
        payload(
            "SessionCommandFailed",
            size_of::<(SessionCommand, String)>(),
        ),
        payload("SkillLoaded", size_of::<(String, String)>()),
        payload("Notice", size_of::<(NoticeSeverity, String, String)>()),
        payload("TurnStarted", size_of::<(String,)>()),
        payload("HistoryRepaired", size_of::<(usize,)>()),
        payload("AgentResolved", size_of::<(u32, String)>()),
        payload("ModelResolved", size_of::<(u32, String, String)>()),
        payload("AssistantMessageCreated", size_of::<(u32, String)>()),
        payload("ToolSnapshotLocked", size_of::<(u32, Vec<String>, bool)>()),
        payload("ProviderRequestStarted", size_of::<(u32, usize, u64)>()),
        payload("Provider", size_of::<(u32, StreamEvent)>()),
        payload(
            "ToolCallStarted",
            size_of::<(u32, String, String, String, ToolUiIntent)>(),
        ),
        payload("AssistantCheckpointed", size_of::<(u32, String, bool)>()),
        payload(
            "ToolDispatchStarted",
            size_of::<(u32, String, String, String, ToolUiIntent)>(),
        ),
        payload(
            "ToolDispatchBlocked",
            size_of::<(u32, String, ToolBlockKind)>(),
        ),
        payload(
            "ToolDispatchInterrupted",
            size_of::<(
                u32,
                String,
                String,
                String,
                String,
                String,
                ToolInterruption,
                bool,
            )>(),
        ),
        payload(
            "ToolResultPresented",
            size_of::<(u32, String, ToolResultPresentation)>(),
        ),
        payload(
            "ToolDispatchCompleted",
            size_of::<(
                u32,
                String,
                String,
                String,
                String,
                String,
                Option<ToolDiff>,
                Vec<String>,
                bool,
            )>(),
        ),
        payload("ToolResultAppended", size_of::<(u32, String, bool)>()),
        payload("StepCompleted", size_of::<(u32, Option<FinishReason>)>()),
        payload("TurnCompleted", size_of::<(String, u32)>()),
        payload(
            "TurnInterrupted",
            size_of::<(Option<String>, u32, Option<HardInterruptRequest>)>(),
        ),
        payload("TurnFailed", size_of::<(Option<String>, u32, String)>()),
        payload("TurnWaitingForHuman", size_of::<(String, u32, String)>()),
    ]
}

/// The compile-time coupling for [`turn_event_payloads`].
fn turn_event_variant_name(event: &TurnEvent) -> &'static str {
    match event {
        TurnEvent::SessionMaterialized { .. } => "SessionMaterialized",
        TurnEvent::SessionTitleUpdated { .. } => "SessionTitleUpdated",
        TurnEvent::SessionCommandStarted { .. } => "SessionCommandStarted",
        TurnEvent::SessionCommandOutput { .. } => "SessionCommandOutput",
        TurnEvent::SessionCommandCompleted { .. } => "SessionCommandCompleted",
        TurnEvent::SessionCommandFailed { .. } => "SessionCommandFailed",
        TurnEvent::SkillLoaded { .. } => "SkillLoaded",
        TurnEvent::Notice { .. } => "Notice",
        TurnEvent::TurnStarted { .. } => "TurnStarted",
        TurnEvent::HistoryRepaired { .. } => "HistoryRepaired",
        TurnEvent::AgentResolved { .. } => "AgentResolved",
        TurnEvent::ModelResolved { .. } => "ModelResolved",
        TurnEvent::AssistantMessageCreated { .. } => "AssistantMessageCreated",
        TurnEvent::ToolSnapshotLocked { .. } => "ToolSnapshotLocked",
        TurnEvent::ProviderRequestStarted { .. } => "ProviderRequestStarted",
        TurnEvent::Provider { .. } => "Provider",
        TurnEvent::ToolCallStarted { .. } => "ToolCallStarted",
        TurnEvent::AssistantCheckpointed { .. } => "AssistantCheckpointed",
        TurnEvent::ToolDispatchStarted { .. } => "ToolDispatchStarted",
        TurnEvent::ToolDispatchBlocked { .. } => "ToolDispatchBlocked",
        TurnEvent::ToolDispatchInterrupted { .. } => "ToolDispatchInterrupted",
        TurnEvent::ToolResultPresented { .. } => "ToolResultPresented",
        TurnEvent::ToolDispatchCompleted { .. } => "ToolDispatchCompleted",
        TurnEvent::ToolResultAppended { .. } => "ToolResultAppended",
        TurnEvent::StepCompleted { .. } => "StepCompleted",
        TurnEvent::TurnCompleted { .. } => "TurnCompleted",
        TurnEvent::TurnInterrupted { .. } => "TurnInterrupted",
        TurnEvent::TurnFailed { .. } => "TurnFailed",
        TurnEvent::TurnWaitingForHuman { .. } => "TurnWaitingForHuman",
    }
}

/// The two payloads that decide whether boxing the largest variant helps.
#[derive(Debug, Clone, Copy)]
struct BoxingProjection {
    stride: usize,
    largest: VariantPayload,
    runner_up: VariantPayload,
    /// Stride the enum could not drop below once the largest variant is a `Box`.
    projected_floor: usize,
}

impl BoxingProjection {
    fn saving_per_element(self) -> usize {
        self.stride.saturating_sub(self.projected_floor)
    }
}

/// Project what boxing the largest variant could do, and validate the table.
///
/// The projection is stated as a **floor**, not an exact new size: replacing the
/// largest payload with a pointer leaves the enum no smaller than its runner-up
/// plus a discriminant, and Rust's niche optimisation may keep it larger. A floor
/// is enough to decide D2, and it cannot overstate the saving.
fn project_boxing(label: &str, stride: usize, table: &[VariantPayload]) -> BoxingProjection {
    let mut sorted: Vec<VariantPayload> = table.to_vec();
    sorted.sort_by_key(|entry| std::cmp::Reverse(entry.bytes));
    let largest = sorted[0];
    let runner_up = sorted[1];

    // The table is only evidence while it still explains the real stride. A
    // variant bigger than everything listed here would make the projection
    // describe an enum that no longer exists.
    assert!(
        stride >= largest.bytes,
        "{label}: the measured stride {stride} is under the largest payload in the \
         table ({} = {}), so the table is describing a different enum",
        largest.name,
        largest.bytes
    );
    assert!(
        stride <= largest.bytes + size_of::<u64>(),
        "{label}: the measured stride {stride} exceeds the largest tabulated payload \
         ({} = {}) by more than a discriminant word, so a variant is missing from \
         the table and the boxing projection below is stale",
        largest.name,
        largest.bytes
    );

    let projected_floor = runner_up.bytes.max(size_of::<Box<u8>>()) + size_of::<u64>();
    BoxingProjection {
        stride,
        largest,
        runner_up,
        projected_floor,
    }
}

#[test]
fn d0_enum_boxing_baseline() {
    let stream = project_boxing(
        "StreamEvent",
        size_of::<StreamEvent>(),
        &stream_event_payloads(),
    );
    let turn = project_boxing("TurnEvent", size_of::<TurnEvent>(), &turn_event_payloads());

    // The exhaustive matches are what couple the tables to the enums. Calling
    // them keeps them live code rather than dead helpers a later cleanup deletes.
    assert_eq!(
        stream_event_variant_name(&StreamEvent::ToolUseEnd { id: String::new() }),
        "ToolUseEnd"
    );
    assert_eq!(
        turn_event_variant_name(&TurnEvent::HistoryRepaired {
            repaired_tool_results: 0
        }),
        "HistoryRepaired"
    );

    println!("\nD0-b ENUM BOXING BASELINE  (runs={RUNS}, spread 1.0000x: layout is a");
    println!("compile-time constant, so every run is byte-identical by construction)\n");
    for (label, projection, population, population_source) in [
        (
            "StreamEvent",
            stream,
            TURN_EVENT_CHANNEL_CAPACITY,
            "reaches a consumer only inside TurnEvent::Provider, so its population \
             is the same bounded channel",
        ),
        (
            "TurnEvent",
            turn,
            TURN_EVENT_CHANNEL_CAPACITY,
            "TURN_EVENT_CHANNEL_CAPACITY, the only container that holds more than one",
        ),
    ] {
        let saving = projection.saving_per_element();
        println!("  {label}");
        println!("    inline stride                : {} B", projection.stride);
        println!(
            "    largest variant              : {} = {} B",
            projection.largest.name, projection.largest.bytes
        );
        println!(
            "    runner-up variant            : {} = {} B",
            projection.runner_up.name, projection.runner_up.bytes
        );
        println!(
            "    stride floor if boxed        : {} B",
            projection.projected_floor
        );
        println!("    saving per element (upper)   : {saving} B");
        println!("    max live at once             : {population} ({population_source})");
        println!(
            "    total saving (upper bound)   : {} B\n",
            saving * population
        );
    }

    let total_upper_bound =
        (stream.saving_per_element() + turn.saving_per_element()) * TURN_EVENT_CHANNEL_CAPACITY;
    println!(
        "  VERDICT: the whole D2 opportunity on these two enums is bounded by \
         {total_upper_bound} B,"
    );
    println!(
        "  because neither is ever collected into a length-unbounded container - the \
         only"
    );
    println!(
        "  multi-element home either has is the {TURN_EVENT_CHANNEL_CAPACITY}-slot \
         bounded turn-event channel."
    );
    println!("  See the D0 section of docs/perf-methodology.md for the recorded figures.\n");

    // The finding this test exists to pin: the population is bounded, so the
    // whole saving is bounded too. If a later change collects either enum into an
    // unbounded Vec this stops being true, and the number above stops being the
    // answer to D2.
    assert!(
        total_upper_bound < 16 * 1024,
        "the bounded-population argument that closes D2 no longer holds: the upper \
         bound is now {total_upper_bound} B. Re-run D0 and re-open D2 rather than \
         raising this number."
    );
}

#[test]
fn d0_hot_type_footprints() {
    let types: Vec<(&str, usize, usize)> = vec![
        (
            "MessageRecord",
            size_of::<MessageRecord>(),
            align_of::<MessageRecord>(),
        ),
        (
            "PartRecord",
            size_of::<PartRecord>(),
            align_of::<PartRecord>(),
        ),
        (
            "MessageWithParts",
            size_of::<MessageWithParts>(),
            align_of::<MessageWithParts>(),
        ),
        ("PartKind", size_of::<PartKind>(), align_of::<PartKind>()),
        (
            "MessageRole",
            size_of::<MessageRole>(),
            align_of::<MessageRole>(),
        ),
        (
            "StreamEvent",
            size_of::<StreamEvent>(),
            align_of::<StreamEvent>(),
        ),
        ("TurnEvent", size_of::<TurnEvent>(), align_of::<TurnEvent>()),
        (
            "ProjectedMessage",
            size_of::<ProjectedMessage>(),
            align_of::<ProjectedMessage>(),
        ),
        ("Message", size_of::<Message>(), align_of::<Message>()),
        (
            "RequestContentBlock",
            size_of::<RequestContentBlock>(),
            align_of::<RequestContentBlock>(),
        ),
        ("String", size_of::<String>(), align_of::<String>()),
        ("serde_json::Value", size_of::<Value>(), align_of::<Value>()),
    ];

    println!("\nD0 HOT TYPE FOOTPRINTS  (compile-time; identical on every run)\n");
    for (name, size, align) in &types {
        println!("  {name:<22} size={size:>4} B  align={align:>2} B");
    }
    println!();

    for (name, size, align) in &types {
        assert!(*size > 0, "{name} measured as zero-sized");
        assert!(
            align.is_power_of_two(),
            "{name} has a non-power-of-two align"
        );
    }
}

// ---------------------------------------------------------------------------
// Byte accounting shared by baselines 1 and 3.
// ---------------------------------------------------------------------------

/// Every string leaf's byte length inside a JSON value, appended to `out`.
///
/// Only string leaves are counted. Numbers and booleans carry no heap payload, so
/// including them would inflate the total with bytes no representation change can
/// move.
fn collect_string_leaves(value: &Value, out: &mut Vec<usize>) {
    collect_leaves(value, out, &mut Vec::new());
}

/// Split string leaves into values and `serde_json::Map` keys.
///
/// The split changes which optimisation the histogram argues for. A key is a
/// `String` in the map and so a real allocation, but it is one of a handful of
/// repeating names — interning or `Box<str>` territory. A short *value* is what
/// `CompactString` (D4) would inline. Counting them together made the first
/// version of this census attribute half the payload to D4 when most of it was
/// repeated key text.
fn collect_leaves(value: &Value, values: &mut Vec<usize>, keys: &mut Vec<usize>) {
    match value {
        Value::String(text) => values.push(text.len()),
        Value::Array(items) => {
            for item in items {
                collect_leaves(item, values, keys);
            }
        }
        Value::Object(entries) => {
            for (key, item) in entries {
                keys.push(key.len());
                collect_leaves(item, values, keys);
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
}

fn json_payload_bytes(value: &Value) -> usize {
    let mut leaves = Vec::new();
    collect_string_leaves(value, &mut leaves);
    leaves.iter().sum()
}

/// Heap string bytes a hydrated transcript owns.
fn hydrated_payload_bytes(history: &[MessageWithParts]) -> usize {
    history
        .iter()
        .map(|entry| {
            let info = entry.info.id.len()
                + entry.info.session_id.len()
                + entry
                    .info
                    .data
                    .iter()
                    .map(|(key, value)| key.len() + json_payload_bytes(value))
                    .sum::<usize>();
            let parts: usize = entry
                .parts
                .iter()
                .map(|part| {
                    part.id.len()
                        + part.message_id.len()
                        + part.session_id.len()
                        + part
                            .data
                            .iter()
                            .map(|(key, value)| key.len() + json_payload_bytes(value))
                            .sum::<usize>()
                })
                .sum();
            info + parts
        })
        .sum()
}

/// Heap string bytes one provider-bound message owns.
///
/// The match is exhaustive so a new request block cannot be added without this
/// accounting being updated to say what it costs.
fn message_payload_bytes(message: &Message) -> usize {
    message
        .content
        .iter()
        .map(|block| match block {
            RequestContentBlock::Text { text } => text.len(),
            RequestContentBlock::ResourceLink {
                name,
                uri,
                title,
                description,
                media_type,
                size: _,
            } => {
                name.len()
                    + uri.len()
                    + title.as_ref().map_or(0, String::len)
                    + description.as_ref().map_or(0, String::len)
                    + media_type.as_ref().map_or(0, String::len)
            }
            RequestContentBlock::SignedThinking {
                thinking,
                signature,
            } => thinking.len() + signature.len(),
            RequestContentBlock::ProviderEncryptedReasoning {
                id,
                summary,
                encrypted_content,
                status,
            } => {
                id.len()
                    + summary.iter().map(String::len).sum::<usize>()
                    + encrypted_content.as_ref().map_or(0, String::len)
                    + status.as_ref().map_or(0, String::len)
            }
            RequestContentBlock::ToolUse {
                id,
                name,
                input,
                raw_arguments,
                thought_signature,
            } => {
                id.len()
                    + name.len()
                    + json_payload_bytes(input)
                    + raw_arguments.as_ref().map_or(0, String::len)
                    + thought_signature
                        .as_ref()
                        .map_or(0, |signature| signature.as_str().len())
            }
            RequestContentBlock::ToolResult {
                tool_use_id,
                content,
                is_error: _,
            } => tool_use_id.len() + content.len(),
            RequestContentBlock::Image {
                media_type, data, ..
            } => media_type.len() + data.len(),
            RequestContentBlock::ImageAttachment { reference } => {
                reference.id.to_string().len()
                    + reference.filename.as_ref().map_or(0, String::len)
                    + reference.media_type.len()
            }
        })
        .sum()
}

/// Content bytes of a provenance-carrying projection.
///
/// The retained `message_id` is counted separately by [`provenance_bytes`]: it is
/// what the cloning projection carries *in addition* to the content, and mixing it
/// into the content total made the first version of this measurement compare the
/// two projections on different quantities.
fn projected_content_bytes(projected: &[ProjectedMessage]) -> usize {
    projected
        .iter()
        .map(|entry| message_payload_bytes(&entry.message))
        .sum()
}

fn provenance_bytes(projected: &[ProjectedMessage]) -> usize {
    projected
        .iter()
        .map(|entry| entry.message_id.as_ref().map_or(0, String::len))
        .sum()
}

// ---------------------------------------------------------------------------
// The pinned snapshot, read read-only.
// ---------------------------------------------------------------------------

/// The pinned snapshot's rows, or `None` when this machine does not hold it.
struct PinnedRows {
    messages: Vec<MessageRecord>,
    parts: Vec<PartRecord>,
}

/// Locate and identity-check the pinned snapshot, or explain the skip.
fn pinned_snapshot() -> Option<PathBuf> {
    let path = PathBuf::from(W_REAL_SUBJECT.database_path);
    match verify_pinned_database(&path, &W_REAL_SUBJECT) {
        Ok(()) => Some(path),
        Err(error) => {
            eprintln!(
                "SKIP D0 fixture measurement: the pinned W-real snapshot is not usable \
                 on this machine.\n  {error}"
            );
            None
        }
    }
}

/// Read the pinned session's rows with `sqlite3 -readonly`.
///
/// The same mechanism `crates/zuno-testkit/src/perf/database.rs` uses, and for
/// the same reason: the methodology says the source database is never opened
/// writable, and every Rust open path in this workspace applies pragmas. This
/// also avoids the 2.6 GB `.backup` a full W-real capture pays for, which D0 does
/// not need — it reads one session and writes nothing.
fn read_pinned_rows(database: &PathBuf) -> PinnedRows {
    let sqlite = which::which("sqlite3")
        .expect("D0 reads the pinned snapshot with sqlite3 -readonly; install sqlite3");
    let query = |sql: String| -> Vec<Value> {
        let output = Command::new(&sqlite)
            .arg("-readonly")
            .arg("-json")
            .arg("-cmd")
            .arg(".timeout 30000")
            .arg(database)
            .arg(sql)
            .output()
            .expect("run sqlite3 against the pinned snapshot");
        assert!(
            output.status.success(),
            "sqlite3 failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).expect("sqlite3 -json emits a JSON array")
    };

    let session = W_REAL_SUBJECT.session_id.replace('\'', "''");
    let message_rows = query(format!(
        "SELECT id, session_id, time_created, time_updated, data FROM message \
         WHERE session_id = '{session}' ORDER BY time_created ASC, id ASC;"
    ));
    let part_rows = query(format!(
        "SELECT id, message_id, session_id, time_created, time_updated, data FROM part \
         WHERE session_id = '{session}' ORDER BY time_created ASC, id ASC;"
    ));

    let messages = message_rows
        .into_iter()
        .map(|row| {
            let mut data: Value = serde_json::from_str(row["data"].as_str().expect("message data"))
                .expect("message.data is JSON");
            let object = data.as_object_mut().expect("message.data is an object");
            object.insert("id".to_owned(), row["id"].clone());
            object.insert("sessionID".to_owned(), row["session_id"].clone());
            MessageRecord::from_json(data).expect("a stored message splits back into a record")
        })
        .collect();
    let parts = part_rows
        .into_iter()
        .map(|row| {
            let mut data: Value = serde_json::from_str(row["data"].as_str().expect("part data"))
                .expect("part.data is JSON");
            let object = data.as_object_mut().expect("part.data is an object");
            object.insert("id".to_owned(), row["id"].clone());
            object.insert("sessionID".to_owned(), row["session_id"].clone());
            object.insert("messageID".to_owned(), row["message_id"].clone());
            let created = row["time_created"].as_i64().unwrap_or_default();
            PartRecord::from_json(data, created).expect("a stored part splits back into a record")
        })
        .collect();
    PinnedRows { messages, parts }
}

/// Attach parts to messages the way `MessageStore::hydrate` does.
fn hydrate(rows: &PinnedRows) -> Vec<MessageWithParts> {
    let mut grouped: BTreeMap<&str, Vec<PartRecord>> = BTreeMap::new();
    for part in &rows.parts {
        grouped
            .entry(part.message_id.as_str())
            .or_default()
            .push(part.clone());
    }
    rows.messages
        .iter()
        .map(|info| MessageWithParts {
            info: info.clone(),
            parts: grouped.remove(info.id.as_str()).unwrap_or_default(),
        })
        .collect()
}

/// min / median / max plus max/min, the same shape the allocator table reports.
fn spread(mut samples: Vec<Duration>) -> (Duration, Duration, Duration, f64) {
    samples.sort_unstable();
    let min = samples[0];
    let median = samples[samples.len() / 2];
    let max = samples[samples.len() - 1];
    let ratio = max.as_secs_f64() / min.as_secs_f64().max(f64::MIN_POSITIVE);
    (min, median, max, ratio)
}

// ---------------------------------------------------------------------------
// Baseline 1: large-payload sharing.
// ---------------------------------------------------------------------------

#[test]
fn d0_large_payload_sharing_baseline() {
    let Some(database) = pinned_snapshot() else {
        return;
    };
    let rows = read_pinned_rows(&database);
    assert_eq!(rows.parts.len() as u64, W_REAL_SUBJECT.part_count);
    // The pin's `message_count` is `COUNT(DISTINCT part.message_id)`, not the
    // session's `message` row count. Measured here: 933 message rows against 931
    // distinct parents, so two stored messages carry no parts. Asserting the row
    // count against the pin fails for that reason and not because the subject
    // drifted, which is what the first version of this test did.
    let parents: std::collections::BTreeSet<&str> = rows
        .parts
        .iter()
        .map(|part| part.message_id.as_str())
        .collect();
    assert_eq!(parents.len() as u64, W_REAL_SUBJECT.message_count);
    let childless = rows.messages.len() - parents.len();

    let mut leaves = Vec::new();
    let mut keys = Vec::new();
    for part in &rows.parts {
        for (key, value) in &part.data {
            keys.push(key.len());
            collect_leaves(value, &mut leaves, &mut keys);
        }
    }
    let value_bytes: usize = leaves.iter().sum();
    let key_bytes: usize = keys.iter().sum();
    let total = value_bytes + key_bytes;
    let large: Vec<usize> = leaves
        .iter()
        .copied()
        .filter(|bytes| *bytes >= LARGE_LEAF_BYTES)
        .collect();
    let large_bytes: usize = large.iter().sum();
    let inline: Vec<usize> = leaves
        .iter()
        .copied()
        .filter(|bytes| *bytes <= INLINE_LEAF_BYTES)
        .collect();
    let inline_bytes: usize = inline.iter().sum();

    const BUCKETS: &[usize] = &[24, 64, 256, 1024, 16_384, 262_144, usize::MAX];
    println!("\nD0-a LARGE-PAYLOAD SHARING BASELINE");
    println!(
        "  subject: {} ({} part parents / {} message rows, {childless} of them \
         part-less / {} parts / {} part.data bytes)",
        W_REAL_SUBJECT.session_id,
        W_REAL_SUBJECT.message_count,
        rows.messages.len(),
        W_REAL_SUBJECT.part_count,
        W_REAL_SUBJECT.part_data_bytes
    );
    println!("  string bytes     : {total}");
    println!(
        "    value leaves   : {:>9} leaves  {value_bytes:>11} B  ({:>6.2}%)",
        leaves.len(),
        value_bytes as f64 * 100.0 / total as f64
    );
    println!(
        "    map keys       : {:>9} leaves  {key_bytes:>11} B  ({:>6.2}%)",
        keys.len(),
        key_bytes as f64 * 100.0 / total as f64
    );
    println!("\n  value-leaf size histogram (upper bound inclusive)");
    let mut previous = 0usize;
    for bound in BUCKETS {
        let bucket: Vec<usize> = leaves
            .iter()
            .copied()
            .filter(|bytes| *bytes > previous && *bytes <= *bound)
            .collect();
        let bytes: usize = bucket.iter().sum();
        let label = if *bound == usize::MAX {
            format!("   >{previous}")
        } else {
            format!("<={bound}")
        };
        println!(
            "    {label:>12}  leaves={:>7}  bytes={:>11}  ({:>6.2}% of bytes)",
            bucket.len(),
            bytes,
            bytes as f64 * 100.0 / value_bytes as f64
        );
        previous = *bound;
    }

    println!(
        "\n  D1 candidates (value leaf >= {LARGE_LEAF_BYTES} B): {} leaves, {large_bytes} B \
         ({:.2}% of all string bytes)",
        large.len(),
        large_bytes as f64 * 100.0 / total as f64
    );
    println!(
        "  D4 candidates (value leaf <= {INLINE_LEAF_BYTES} B): {} leaves, {inline_bytes} B \
         ({:.2}% of all string bytes)",
        inline.len(),
        inline_bytes as f64 * 100.0 / total as f64
    );
    println!(
        "  largest single value leaf: {} B",
        leaves.iter().copied().max().unwrap_or_default()
    );

    // The measurement is deterministic given a pinned snapshot, so run-to-run
    // stability is asserted rather than sampled: five recomputations must agree
    // exactly. A number that moved between runs would not be a baseline.
    for run in 1..=RUNS {
        let mut again_values = Vec::new();
        let mut again_keys = Vec::new();
        for part in &rows.parts {
            for (key, value) in &part.data {
                again_keys.push(key.len());
                collect_leaves(value, &mut again_values, &mut again_keys);
            }
        }
        assert_eq!(
            again_values.iter().sum::<usize>() + again_keys.iter().sum::<usize>(),
            total,
            "run {run} of {RUNS} disagreed with run 1; the census is not deterministic"
        );
    }
    println!("  runs={RUNS}, spread 1.0000x (all five byte-identical)\n");

    assert!(total > 0);
}

// ---------------------------------------------------------------------------
// Baseline 3: derived copies.
// ---------------------------------------------------------------------------

#[test]
fn d0_derived_copy_baseline() {
    let Some(database) = pinned_snapshot() else {
        return;
    };
    let rows = read_pinned_rows(&database);
    let hydrated = hydrate(&rows);
    let stored = hydrated_payload_bytes(&hydrated);

    let reference = project_history("", &hydrated);
    let content_bytes = projected_content_bytes(&reference);
    let provenance = provenance_bytes(&reference);
    let projected_messages = reference.len();
    drop(reference);
    let owned_reference = project_history_owned("", hydrated.clone());
    let owned_content_bytes: usize = owned_reference.iter().map(message_payload_bytes).sum();
    drop(owned_reference);

    let mut borrowed_times = Vec::with_capacity(RUNS);
    let mut owned_times = Vec::with_capacity(RUNS);
    let mut teardown_times = Vec::with_capacity(RUNS);

    for _ in 0..RUNS {
        // Both paths are given their own owned copy of the stored transcript and
        // both timed regions end with that copy destroyed. Without the symmetry
        // the moving path alone pays for freeing 70 MB of JSON, which the first
        // version of this measurement misread as the move being 264x slower.
        let feed = hydrated.clone();
        let started = Instant::now();
        let projected = project_history("", &feed);
        drop(projected);
        drop(feed);
        borrowed_times.push(started.elapsed());

        let feed = hydrated.clone();
        let started = Instant::now();
        let projected = project_history_owned("", feed);
        drop(projected);
        owned_times.push(started.elapsed());

        let feed = hydrated.clone();
        let started = Instant::now();
        drop(feed);
        teardown_times.push(started.elapsed());
    }

    let (b_min, b_median, b_max, b_ratio) = spread(borrowed_times);
    let (o_min, o_median, o_max, o_ratio) = spread(owned_times);
    let (t_min, t_median, t_max, t_ratio) = spread(teardown_times);

    println!("\nD0-c DERIVED COPY BASELINE  (runs={RUNS})");
    println!("  hydrated transcript string payload : {stored} B");
    println!("  projected provider messages        : {projected_messages}");
    println!("  request content reaching a provider: {content_bytes} B");
    println!(
        "    that is {:.2}% of the stored payload; the projection drops reasoning \
         traces, snapshots,",
        content_bytes as f64 * 100.0 / stored as f64
    );
    println!("    patches and every other non-request part kind");
    println!("  provenance ids the cloning path also carries: {provenance} B");
    println!("\n  peak simultaneously-live payload");
    println!(
        "    project_history      (clones): {} B = {:.4}x stored",
        stored + content_bytes + provenance,
        (stored + content_bytes + provenance) as f64 / stored as f64
    );
    println!(
        "    project_history_owned (moves): {} B = 1.0000x stored (payload moves out \
         of the parts)",
        stored.max(owned_content_bytes)
    );
    println!(
        "    difference                   : {} B",
        content_bytes + provenance
    );
    println!("\n  wall time, projection plus symmetric teardown");
    println!("    clones : {b_min:?} / {b_median:?} / {b_max:?}  (max/min {b_ratio:.4}x)");
    println!("    moves  : {o_min:?} / {o_median:?} / {o_max:?}  (max/min {o_ratio:.4}x)");
    println!(
        "    teardown alone (both pay it): {t_min:?} / {t_median:?} / {t_max:?}  \
         (max/min {t_ratio:.4}x)"
    );
    println!(
        "\n  VERDICT: the runtime request path already uses the moving projection \
         (prelude.rs:535),"
    );
    println!(
        "  so the second resident transcript copy D1/D5 exist to remove is not present \
         on it."
    );
    println!(
        "  And the duplicate the cloning projection does create is {} B against a \
         {stored} B",
        content_bytes + provenance
    );
    println!(
        "  transcript, because 95% of stored payload never reaches a provider request \
         at all."
    );
    println!("  Recorded in the D0 section of docs/perf-methodology.md.\n");

    // The claim the verdict rests on: both projections carry the same request
    // content, so the moving path is not cheaper by dropping payload. If they
    // diverge, "already solved" is wrong and D1/D5 have to be reconsidered.
    assert_eq!(
        content_bytes, owned_content_bytes,
        "the two projections carry different request content, so the moving path is \
         not a drop-in for the cloning one and D1/D5's premise changes"
    );
    // D1 is closed by this measured ratio, not by jcode's "tens of MiB per active
    // session" claim. A session shape whose duplicate exceeds a fifth of the
    // transcript reopens it.
    let duplicate_fraction = (content_bytes + provenance) as f64 / stored as f64;
    assert!(
        duplicate_fraction < 0.20,
        "the cloning projection now duplicates {:.2}% of the stored transcript; D1's \
         null result was measured at 5% and has to be re-taken, not re-asserted",
        duplicate_fraction * 100.0
    );
    assert!(
        stored > 0 && content_bytes > 0,
        "a zero-byte measurement would make every ratio above meaningless"
    );
}
